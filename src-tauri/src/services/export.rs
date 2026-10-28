use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
#[cfg(test)]
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::sync::{
    Notify, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, Semaphore, oneshot,
};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::db::batches::{BatchExportClaim, BatchRepository};
use crate::domain::amount::{checked_add_amount_cents, validate_amount_cents};
use crate::domain::error::AppError;
use crate::domain::model::{ItemStatus, RecognitionStatus};
use crate::infra::exporters::{
    DEFAULT_EXPORT_LIMITS, ExportItem, ExportLimits, generate_artifacts, prepare_merged_pdf,
    publish_directory,
};
use crate::infra::files::{
    AppPaths, OpenedContainedFile, open_contained_regular_file, sync_directory,
};
use crate::services::batch_eligibility::{batch_issues, describe_issues};
pub use crate::services::export_recovery::ExportRecoveryReport;
use crate::services::export_recovery::{
    ExportCommitOutcome, ExportJournal, PendingExportIdentity, write_generation_marker,
};
use crate::services::settings::{ExportPreferenceGate, load_preferences};

const MAX_FILENAME_COMPONENT_BYTES: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportResult {
    pub directory: PathBuf,
    pub item_count: usize,
    pub total_amount_cents: i64,
}

#[derive(Clone)]
pub struct ExportCoordinator {
    active_exports: Arc<DashMap<Uuid, ()>>,
    generation_slots: Arc<Semaphore>,
    lifecycle: Arc<ExportLifecycleInner>,
    operation_gate: Arc<RwLock<()>>,
}

#[derive(Default)]
struct ExportLifecycleInner {
    state: Mutex<ExportLifecycleState>,
    changed: Notify,
}

#[derive(Default)]
struct ExportLifecycleState {
    closed: bool,
    next_id: u64,
    tasks: HashMap<u64, JoinHandle<()>>,
    first_error: Option<AppError>,
}

pub(crate) struct ExportShutdown {
    coordinator: ExportCoordinator,
}

impl Default for ExportCoordinator {
    fn default() -> Self {
        Self {
            active_exports: Arc::new(DashMap::new()),
            generation_slots: Arc::new(Semaphore::new(2)),
            lifecycle: Arc::new(ExportLifecycleInner::default()),
            operation_gate: Arc::new(RwLock::new(())),
        }
    }
}

impl ExportCoordinator {
    async fn acquire_export(&self, batch_id: Uuid) -> Result<ActiveExport, AppError> {
        let operation_guard = self.operation_gate.clone().read_owned().await;
        let state = self
            .lifecycle
            .state
            .lock()
            .expect("export lifecycle lock poisoned");
        if state.closed {
            return Err(AppError::Conflict {
                message: "application is shutting down".to_owned(),
            });
        }
        ActiveExport::acquire(
            self.active_exports.clone(),
            self.lifecycle.clone(),
            operation_guard,
            batch_id,
        )
    }

    async fn acquire_recovery(&self) -> OwnedRwLockWriteGuard<()> {
        self.operation_gate.clone().write_owned().await
    }

    fn spawn_internal<T, F>(&self, future: F) -> oneshot::Receiver<Result<T, AppError>>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, AppError>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let mut state = self
            .lifecycle
            .state
            .lock()
            .expect("export lifecycle lock poisoned");
        let task_id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        let lifecycle = self.lifecycle.clone();
        let task = tokio::spawn(async move {
            let child = tokio::spawn(future);
            let result = child.await.map_err(|_| AppError::Internal {
                message: "export internal task was interrupted".to_owned(),
            });
            let result = result.and_then(|result| result);
            let detached_error = match sender.send(result) {
                Err(Err(error)) => Some(error),
                Err(Ok(_)) | Ok(()) => None,
            };
            {
                let mut state = lifecycle
                    .state
                    .lock()
                    .expect("export lifecycle lock poisoned");
                if let Some(error) = detached_error {
                    tracing::error!(%error, "detached export internal task failed");
                    if state.first_error.is_none() {
                        state.first_error = Some(error);
                    }
                }
                state.tasks.remove(&task_id);
            }
            lifecycle.changed.notify_waiters();
        });
        state.tasks.insert(task_id, task);
        receiver
    }

    pub(crate) fn begin_shutdown(&self) -> ExportShutdown {
        self.lifecycle
            .state
            .lock()
            .expect("export lifecycle lock poisoned")
            .closed = true;
        self.lifecycle.changed.notify_waiters();
        ExportShutdown {
            coordinator: self.clone(),
        }
    }
}

impl ExportShutdown {
    pub(crate) async fn wait(&self) -> Result<(), AppError> {
        loop {
            let changed = self.coordinator.lifecycle.changed.notified();
            let completed = {
                let state = self
                    .coordinator
                    .lifecycle
                    .state
                    .lock()
                    .expect("export lifecycle lock poisoned");
                (state.tasks.is_empty() && self.coordinator.active_exports.is_empty())
                    .then(|| state.first_error.clone())
            };
            if let Some(error) = completed {
                return error.map_or(Ok(()), Err);
            }
            changed.await;
        }
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.coordinator
            .lifecycle
            .state
            .lock()
            .expect("export lifecycle lock poisoned")
            .tasks
            .len()
    }
}

#[derive(Clone)]
pub struct ExportService {
    pool: SqlitePool,
    paths: AppPaths,
    coordinator: ExportCoordinator,
    publisher: Arc<dyn DirectoryPublisher>,
    committer: Arc<dyn ExportCommitter>,
    outcome_classifier: Arc<dyn ExportOutcomeClassifier>,
    generation_hook: Arc<dyn GenerationHook>,
    preference_gate: ExportPreferenceGate,
}

impl ExportService {
    pub fn new(pool: SqlitePool, paths: AppPaths, coordinator: ExportCoordinator) -> Self {
        Self {
            outcome_classifier: default_outcome_classifier(&pool),
            pool,
            paths,
            coordinator,
            publisher: Arc::new(AtomicDirectoryPublisher),
            committer: Arc::new(SqliteExportCommitter),
            generation_hook: Arc::new(NoopGenerationHook),
            preference_gate: ExportPreferenceGate::default(),
        }
    }

    pub(crate) fn with_preference_gate(
        pool: SqlitePool,
        paths: AppPaths,
        coordinator: ExportCoordinator,
        preference_gate: ExportPreferenceGate,
    ) -> Self {
        Self {
            outcome_classifier: default_outcome_classifier(&pool),
            pool,
            paths,
            coordinator,
            publisher: Arc::new(AtomicDirectoryPublisher),
            committer: Arc::new(SqliteExportCommitter),
            generation_hook: Arc::new(NoopGenerationHook),
            preference_gate,
        }
    }

    #[cfg(test)]
    fn with_publisher(
        pool: SqlitePool,
        paths: AppPaths,
        publisher: Arc<dyn DirectoryPublisher>,
    ) -> Self {
        Self {
            outcome_classifier: default_outcome_classifier(&pool),
            pool,
            paths,
            coordinator: ExportCoordinator::default(),
            publisher,
            committer: Arc::new(SqliteExportCommitter),
            generation_hook: Arc::new(NoopGenerationHook),
            preference_gate: ExportPreferenceGate::default(),
        }
    }

    #[cfg(test)]
    fn with_committer(
        pool: SqlitePool,
        paths: AppPaths,
        committer: Arc<dyn ExportCommitter>,
    ) -> Self {
        Self {
            outcome_classifier: default_outcome_classifier(&pool),
            pool,
            paths,
            coordinator: ExportCoordinator::default(),
            publisher: Arc::new(AtomicDirectoryPublisher),
            committer,
            generation_hook: Arc::new(NoopGenerationHook),
            preference_gate: ExportPreferenceGate::default(),
        }
    }

    #[cfg(test)]
    fn with_generation_hook(
        pool: SqlitePool,
        paths: AppPaths,
        generation_hook: Arc<dyn GenerationHook>,
    ) -> Self {
        Self {
            outcome_classifier: default_outcome_classifier(&pool),
            pool,
            paths,
            coordinator: ExportCoordinator::default(),
            publisher: Arc::new(AtomicDirectoryPublisher),
            committer: Arc::new(SqliteExportCommitter),
            generation_hook,
            preference_gate: ExportPreferenceGate::default(),
        }
    }

    #[cfg(test)]
    fn with_coordinator_and_generation_hook(
        pool: SqlitePool,
        paths: AppPaths,
        coordinator: ExportCoordinator,
        generation_hook: Arc<dyn GenerationHook>,
    ) -> Self {
        Self {
            outcome_classifier: default_outcome_classifier(&pool),
            pool,
            paths,
            coordinator,
            publisher: Arc::new(AtomicDirectoryPublisher),
            committer: Arc::new(SqliteExportCommitter),
            generation_hook,
            preference_gate: ExportPreferenceGate::default(),
        }
    }

    pub async fn export(&self, batch_id: Uuid) -> Result<ExportResult, AppError> {
        let active_export = self.coordinator.acquire_export(batch_id).await?;
        let preference_guard = self.preference_gate.read().await;
        let paths = self.effective_paths().await?;
        let batches = BatchRepository::new(self.pool.clone());
        let snapshot = batches.export_snapshot(batch_id).await?;
        let batch = snapshot.batch.clone();
        let items = snapshot.items.clone();
        let suspected_duplicate_count = items
            .iter()
            .filter(|item| item.status() == ItemStatus::SuspectedDuplicate)
            .count();
        if suspected_duplicate_count != 0 {
            return Err(AppError::Conflict {
                message: format!("{suspected_duplicate_count} 张票据疑似重复"),
            });
        }
        let pending_recognition_count = items
            .iter()
            .filter(|item| item.status() == ItemStatus::PendingRecognition)
            .count();
        if pending_recognition_count != 0 {
            return Err(AppError::Conflict {
                message: format!("{pending_recognition_count} 张票据待识别"),
            });
        }
        let unconfirmed_count = items
            .iter()
            .filter(|item| item.status() == ItemStatus::PendingConfirmation)
            .count();
        if unconfirmed_count != 0 {
            return Err(AppError::Conflict {
                message: format!("{unconfirmed_count} 张票据待确认"),
            });
        }

        let failed_count = items
            .iter()
            .filter(|item| item.recognition_status == RecognitionStatus::Failed)
            .count();
        if failed_count != 0 {
            return Err(AppError::Conflict {
                message: format!("{failed_count} 张票据识别失败"),
            });
        }
        if items.is_empty() {
            return Err(AppError::Conflict {
                message: "报销批次没有票据".to_owned(),
            });
        }
        if items.len() > DEFAULT_EXPORT_LIMITS.max_items {
            return Err(AppError::validation("export", "导出票据数量超过限制"));
        }

        let mut items = items;
        items.sort_by_key(|item| {
            (
                item.invoice_date.is_none(),
                item.invoice_date,
                item.created_at,
                item.id,
            )
        });
        let mut total_amount_cents = 0_i64;
        for item in &items {
            let amount_cents = validate_amount_cents(
                item.amount_cents
                    .ok_or_else(|| AppError::validation("amountCents", "票据金额不能为空"))?,
                "amountCents",
            )?;
            total_amount_cents =
                checked_add_amount_cents(total_amount_cents, amount_cents, "totalAmountCents")?;
        }

        // Report every invoice that cannot be exported before any staging
        // directory exists. The bare "票据缺少归一化 PDF" error used to abort a
        // whole batch without naming the offending invoice, so retrying the
        // automation could never succeed and the user had no way to find it.
        let issues = batch_issues(&paths, &items);
        if let Some(first) = issues.first() {
            let detail = describe_issues(&issues);
            if issues.len() == 1 {
                return Err(AppError::validation(first.blocker.field(), detail));
            }
            return Err(AppError::Conflict {
                message: format!("{} 张票据无法导出：{}", issues.len(), detail),
            });
        }

        let exported_at = Utc::now();
        let operation_id = Uuid::new_v4();
        let staging_component = format!("export-{operation_id}");
        let final_component = export_directory_name(&batch.name, &exported_at, operation_id);
        let staging_path = paths.staging.join(&staging_component);
        let directory = paths.exports.join(&final_component);
        let journal = ExportJournal::new(self.pool.clone());
        let generation_slot = self
            .coordinator
            .generation_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AppError::Internal {
                message: "export generation coordinator closed".to_owned(),
            })?;
        let generation_paths = paths.clone();
        let generation_batch = batch.clone();
        let generation_items = items.clone();
        let generation_active_export = active_export.clone();
        let generation_hook = self.generation_hook.clone();
        let generation_staging_component = staging_component.clone();
        let generation_final_component = final_component.clone();
        let generation = self.coordinator.spawn_internal(async move {
            tokio::task::spawn_blocking(move || {
                let _generation_slot = generation_slot;
                let _generation_active_export = generation_active_export;
                let staging = OwnedExportDirectory::create(staging_path)?;
                write_generation_marker(
                    staging.path(),
                    operation_id,
                    batch_id,
                    &generation_staging_component,
                    &generation_final_component,
                    exported_at,
                )?;
                generation_hook.before_generation()?;
                let mut export_items = prepare_export_items(
                    &generation_paths,
                    &generation_items,
                    DEFAULT_EXPORT_LIMITS,
                )?;
                let merged_pdf = prepare_merged_pdf(&mut export_items, DEFAULT_EXPORT_LIMITS)?;
                generate_artifacts(
                    staging.path(),
                    &generation_batch,
                    &mut export_items,
                    exported_at,
                    DEFAULT_EXPORT_LIMITS,
                    merged_pdf,
                )?;
                Ok::<_, AppError>(staging)
            })
            .await
            .map_err(|_| AppError::Internal {
                message: "export generation task failed".to_owned(),
            })?
        });
        let (generation_result, journal_result) = tokio::join!(
            await_internal(generation, "export generation task was interrupted"),
            journal.create(
                operation_id,
                batch_id,
                &staging_component,
                &final_component,
                exported_at,
            )
        );
        let staging = match (generation_result, journal_result) {
            (Ok(staging), Ok(())) => staging,
            (Ok(staging), Err(error)) => {
                drop(staging);
                return Err(error);
            }
            (Err(error), Ok(())) => {
                return Err(finish_failed_export(&journal, &paths, operation_id, error).await);
            }
            (Err(generation_error), Err(journal_error)) => {
                return Err(AppError::External {
                    service: "export_recovery".to_owned(),
                    retryable: false,
                    message: format!(
                        "export generation and recovery journal both failed: generation error: \
                         {generation_error}; journal error: {journal_error}"
                    ),
                });
            }
        };
        drop(preference_guard);

        let claim = match batches.claim_export(&snapshot).await {
            Ok(claim) => claim,
            Err(error) => {
                drop(staging);
                return Err(finish_failed_export(&journal, &paths, operation_id, error).await);
            }
        };
        let committer = self.committer.clone();
        let outcome_classifier = self.outcome_classifier.clone();
        let publisher = self.publisher.clone();
        let publication_active_export = active_export.clone();
        let publication = self.coordinator.spawn_internal(async move {
            let _active_export = publication_active_export;
            publisher.publish(staging, directory).await
        });
        let published =
            match await_internal(publication, "export publisher task was interrupted").await {
                Ok(published) => published,
                Err(error) => {
                    drop(claim);
                    return Err(finish_failed_export(&journal, &paths, operation_id, error).await);
                }
            };
        let finalizer_journal = journal.clone();
        let finalizer_paths = paths;
        let finalizer = self.coordinator.spawn_internal(async move {
            let _active_export = active_export;
            finalize_export(
                committer,
                outcome_classifier,
                finalizer_journal,
                finalizer_paths,
                staging_component,
                final_component,
                operation_id,
                batch_id,
                exported_at,
                published,
                claim,
            )
            .await
        });
        let directory = await_internal(finalizer, "export finalizer task was interrupted").await?;

        Ok(ExportResult {
            directory,
            item_count: items.len(),
            total_amount_cents,
        })
    }

    pub async fn reconcile_pending(&self) -> Result<ExportRecoveryReport, AppError> {
        self.reconcile_pending_with_root().await.1
    }

    pub(crate) async fn reconcile_pending_with_root(
        &self,
    ) -> (Option<String>, Result<ExportRecoveryReport, AppError>) {
        let _recovery_guard = self.coordinator.acquire_recovery().await;
        let _preference_guard = self.preference_gate.read().await;
        let preferences = match load_preferences(&self.pool).await {
            Ok(preferences) => preferences,
            Err(error) => return (None, Err(error)),
        };
        let export_directory = preferences.export_directory.clone();
        let result = match self.paths.for_export_directory(&export_directory) {
            Ok(paths) => {
                ExportJournal::new(self.pool.clone())
                    .reconcile(&paths)
                    .await
            }
            Err(error) => Err(error),
        };
        (Some(export_directory), result)
    }

    async fn effective_paths(&self) -> Result<AppPaths, AppError> {
        let preferences = load_preferences(&self.pool).await?;
        self.paths
            .for_export_directory(&preferences.export_directory)
    }
}

async fn await_internal<T>(
    receiver: oneshot::Receiver<Result<T, AppError>>,
    interrupted_message: &str,
) -> Result<T, AppError> {
    receiver.await.map_err(|_| AppError::Internal {
        message: interrupted_message.to_owned(),
    })?
}

async fn finish_failed_export(
    journal: &ExportJournal,
    paths: &AppPaths,
    operation_id: Uuid,
    error: AppError,
) -> AppError {
    match journal.reconcile_operation(paths, operation_id).await {
        Ok(()) => error,
        Err(journal_error) => AppError::External {
            service: "export_recovery".to_owned(),
            retryable: false,
            message: format!(
                "export failed and its recovery journal could not be cleared: original error: \
                 {error}; journal error: {journal_error}"
            ),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn finalize_export(
    committer: Arc<dyn ExportCommitter>,
    outcome_classifier: Arc<dyn ExportOutcomeClassifier>,
    journal: ExportJournal,
    paths: AppPaths,
    staging_component: String,
    final_component: String,
    operation_id: Uuid,
    batch_id: Uuid,
    exported_at: DateTime<Utc>,
    published: OwnedExportDirectory,
    claim: BatchExportClaim,
) -> Result<PathBuf, AppError> {
    let directory = published.commit();
    let expected = PendingExportIdentity::new(
        operation_id,
        batch_id,
        staging_component,
        final_component,
        exported_at,
    );
    match committer.commit(claim, operation_id, exported_at).await {
        Ok(()) => finish_confirmed_export(&journal, &paths, &expected, directory).await,
        Err(commit_error) => match outcome_classifier.classify(&expected).await {
            ExportCommitOutcome::ConfirmedCommitted => {
                finish_confirmed_export(&journal, &paths, &expected, directory).await
            }
            ExportCommitOutcome::ConfirmedUncommitted => {
                tracing::error!(
                    %batch_id,
                    error = %commit_error,
                    "export finalization failed"
                );
                match journal.rollback_uncommitted(&paths, &expected).await {
                    Ok(()) => Err(commit_error),
                    Err(recovery_error) => {
                        tracing::error!(
                            %batch_id,
                            error = %recovery_error,
                            "uncommitted export rollback failed"
                        );
                        Err(recoverable_finalization_error(
                            "uncommitted export rollback is pending recovery",
                        ))
                    }
                }
            }
            ExportCommitOutcome::Indeterminate(outcome_error) => {
                tracing::error!(
                    %batch_id,
                    commit_error = %commit_error,
                    outcome_error = %outcome_error,
                    "export commit outcome is indeterminate"
                );
                if let Err(record_error) = journal.record_indeterminate(&paths, &expected).await {
                    tracing::error!(
                        %batch_id,
                        error = %record_error,
                        "failed to persist indeterminate export outcome"
                    );
                }
                Err(recoverable_finalization_error(
                    "export commit outcome is indeterminate; restart recovery is required",
                ))
            }
        },
    }
}

async fn finish_confirmed_export(
    journal: &ExportJournal,
    paths: &AppPaths,
    expected: &PendingExportIdentity,
    directory: PathBuf,
) -> Result<PathBuf, AppError> {
    if let Err(cleanup_error) = journal.advance(expected.operation_id, "committed").await {
        tracing::error!(
            batch_id = %expected.batch_id,
            error = %cleanup_error,
            "committed export journal transition failed"
        );
        return Err(recoverable_finalization_error(
            "committed export cleanup is pending recovery",
        ));
    }
    if let Err(cleanup_error) = journal.preserve_committed(paths, expected).await {
        tracing::error!(
            batch_id = %expected.batch_id,
            error = %cleanup_error,
            "committed export recovery cleanup failed"
        );
        return Err(recoverable_finalization_error(
            "committed export cleanup is pending recovery",
        ));
    }
    Ok(directory)
}

fn recoverable_finalization_error(message: &str) -> AppError {
    AppError::External {
        service: "export_recovery".to_owned(),
        retryable: true,
        message: message.to_owned(),
    }
}

fn export_directory_name(
    batch_name: &str,
    exported_at: &DateTime<Utc>,
    operation_id: Uuid,
) -> String {
    let sanitized = sanitize_filename::sanitize(batch_name);
    let sanitized = if sanitized.trim().is_empty() {
        "batch"
    } else {
        sanitized.as_str()
    };
    let suffix = format!("-{}-{operation_id}", exported_at.format("%Y%m%d-%H%M%S"));
    let batch_budget = MAX_FILENAME_COMPONENT_BYTES.saturating_sub(suffix.len());
    let mut end = sanitized.len().min(batch_budget);
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &sanitized[..end])
}

trait GenerationHook: Send + Sync {
    fn before_generation(&self) -> Result<(), AppError>;
}

struct NoopGenerationHook;

impl GenerationHook for NoopGenerationHook {
    fn before_generation(&self) -> Result<(), AppError> {
        Ok(())
    }
}

#[async_trait::async_trait]
trait ExportCommitter: Send + Sync {
    async fn commit(
        &self,
        claim: BatchExportClaim,
        operation_id: Uuid,
        exported_at: DateTime<Utc>,
    ) -> Result<(), AppError>;
}

#[async_trait::async_trait]
trait ExportOutcomeClassifier: Send + Sync {
    async fn classify(&self, expected: &PendingExportIdentity) -> ExportCommitOutcome;
}

struct SqliteExportOutcomeClassifier {
    journal: ExportJournal,
}

#[async_trait::async_trait]
impl ExportOutcomeClassifier for SqliteExportOutcomeClassifier {
    async fn classify(&self, expected: &PendingExportIdentity) -> ExportCommitOutcome {
        self.journal.classify_commit_outcome(expected).await
    }
}

fn default_outcome_classifier(pool: &SqlitePool) -> Arc<dyn ExportOutcomeClassifier> {
    Arc::new(SqliteExportOutcomeClassifier {
        journal: ExportJournal::new(pool.clone()),
    })
}

struct SqliteExportCommitter;

#[async_trait::async_trait]
impl ExportCommitter for SqliteExportCommitter {
    async fn commit(
        &self,
        claim: BatchExportClaim,
        operation_id: Uuid,
        exported_at: DateTime<Utc>,
    ) -> Result<(), AppError> {
        claim.commit(operation_id, exported_at).await.map(|_| ())
    }
}

#[async_trait::async_trait]
trait DirectoryPublisher: Send + Sync {
    async fn publish(
        &self,
        staging: OwnedExportDirectory,
        destination: PathBuf,
    ) -> Result<OwnedExportDirectory, AppError>;
}

struct AtomicDirectoryPublisher;

#[async_trait::async_trait]
impl DirectoryPublisher for AtomicDirectoryPublisher {
    async fn publish(
        &self,
        staging: OwnedExportDirectory,
        destination: PathBuf,
    ) -> Result<OwnedExportDirectory, AppError> {
        tokio::task::spawn_blocking(move || staging.publish(destination))
            .await
            .map_err(|_| AppError::Internal {
                message: "atomic export publisher task failed".to_owned(),
            })?
    }
}

#[derive(Clone)]
struct ActiveExport {
    _lease: Arc<ActiveExportLease>,
}

struct ActiveExportLease {
    active_exports: Arc<DashMap<Uuid, ()>>,
    lifecycle: Arc<ExportLifecycleInner>,
    _operation_guard: OwnedRwLockReadGuard<()>,
    batch_id: Uuid,
}

impl ActiveExport {
    fn acquire(
        active_exports: Arc<DashMap<Uuid, ()>>,
        lifecycle: Arc<ExportLifecycleInner>,
        operation_guard: OwnedRwLockReadGuard<()>,
        batch_id: Uuid,
    ) -> Result<Self, AppError> {
        match active_exports.entry(batch_id) {
            Entry::Vacant(entry) => {
                entry.insert(());
            }
            Entry::Occupied(_) => {
                return Err(AppError::Conflict {
                    message: "该报销批次正在导出".to_owned(),
                });
            }
        }
        Ok(Self {
            _lease: Arc::new(ActiveExportLease {
                active_exports,
                lifecycle,
                _operation_guard: operation_guard,
                batch_id,
            }),
        })
    }
}

impl Drop for ActiveExportLease {
    fn drop(&mut self) {
        self.active_exports.remove(&self.batch_id);
        self.lifecycle.changed.notify_waiters();
    }
}

struct OwnedExportDirectory {
    path: PathBuf,
    owned: bool,
}

impl OwnedExportDirectory {
    fn create(path: PathBuf) -> Result<Self, AppError> {
        fs::create_dir(&path).map_err(|_| AppError::Internal {
            message: "failed to create export staging directory".to_owned(),
        })?;
        Ok(Self { path, owned: true })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn publish(mut self, destination: PathBuf) -> Result<Self, AppError> {
        publish_directory(&self.path, &destination)?;
        self.owned = false;
        Ok(Self {
            path: destination,
            owned: true,
        })
    }

    fn commit(mut self) -> PathBuf {
        self.owned = false;
        self.path.clone()
    }
}

impl Drop for OwnedExportDirectory {
    fn drop(&mut self) {
        if self.owned
            && let Err(error) = remove_export_directory(&self.path)
        {
            tracing::error!(
                path = %self.path.display(),
                %error,
                "export directory cleanup failed; manual recovery may be required"
            );
        }
    }
}

fn remove_export_directory(path: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    path.parent()
        .ok_or_else(|| std::io::Error::other("export directory has no parent"))
        .and_then(sync_directory)
}

struct PendingExportItem {
    item: crate::db::items::InvoiceItem,
    original: OpenedSourceFile,
    normalized_pdf: OpenedSourceFile,
    archive_name: String,
}

type OpenedSourceFile = OpenedContainedFile;

fn prepare_export_items(
    paths: &AppPaths,
    items: &[crate::db::items::InvoiceItem],
    limits: ExportLimits,
) -> Result<Vec<ExportItem>, AppError> {
    if items.len() > limits.max_items {
        return Err(AppError::validation("export", "导出票据数量超过限制"));
    }
    let pending = items
        .iter()
        .enumerate()
        .map(|(index, item)| open_export_item(paths, index + 1, item))
        .collect::<Result<Vec<_>, AppError>>()?;
    let lengths = pending
        .iter()
        .flat_map(|item| {
            [
                (item.original.length, "original"),
                (item.normalized_pdf.length, "normalizedPdf"),
            ]
        })
        .collect::<Vec<_>>();
    validate_source_lengths(
        &lengths,
        limits.max_source_file_bytes,
        limits.max_aggregate_source_bytes,
    )?;

    pending
        .into_iter()
        .map(|pending| {
            Ok(ExportItem {
                item: pending.item,
                original_file: pending.original.file,
                normalized_pdf_file: pending.normalized_pdf.file,
                verified_sha256: String::new(),
                archive_name: pending.archive_name,
            })
        })
        .collect()
}

fn open_export_item(
    paths: &AppPaths,
    sequence: usize,
    item: &crate::db::items::InvoiceItem,
) -> Result<PendingExportItem, AppError> {
    if item.status() != ItemStatus::Ready {
        return Err(AppError::Conflict {
            message: "批次包含尚未就绪的票据".to_owned(),
        });
    }
    if item.final_category.is_none() {
        return Err(AppError::validation("finalCategory", "票据分类不能为空"));
    }
    if item.suggested_period.is_none() {
        return Err(AppError::validation(
            "suggestedPeriod",
            "建议归属时间不能为空",
        ));
    }
    if item.currency != "CNY" {
        return Err(AppError::validation("currency", "仅支持人民币票据"));
    }

    let original =
        open_contained_regular_file(Path::new(&item.original_path), &paths.originals, "original")?;
    let normalized_path = item
        .normalized_pdf_path
        .as_deref()
        .ok_or_else(|| AppError::validation("normalizedPdf", "票据缺少归一化 PDF"))?;
    let normalized_pdf = open_contained_regular_file(
        Path::new(normalized_path),
        &paths.normalized,
        "normalizedPdf",
    )?;
    let basename = item
        .original_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("original");
    let sanitized_name = sanitize_filename::sanitize(basename);
    let sanitized_name = if sanitized_name.trim().is_empty() {
        "original"
    } else {
        sanitized_name.as_str()
    };
    let id_prefix = item.id.to_string().chars().take(8).collect::<String>();

    Ok(PendingExportItem {
        item: item.clone(),
        original,
        normalized_pdf,
        archive_name: format!("{sequence}-{id_prefix}-{sanitized_name}"),
    })
}

fn validate_source_lengths(
    lengths: &[(u64, &str)],
    max_file_bytes: u64,
    max_total_bytes: u64,
) -> Result<(), AppError> {
    let mut total = 0_u64;
    for (length, field) in lengths {
        if *length > max_file_bytes {
            return Err(AppError::validation(*field, "票据文件超过导出大小限制"));
        }
        total = total
            .checked_add(*length)
            .ok_or_else(|| AppError::validation("export", "导出源文件总量超过限制"))?;
        if total > max_total_bytes {
            return Err(AppError::validation("export", "导出源文件总量超过限制"));
        }
    }
    Ok(())
}

#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests;
