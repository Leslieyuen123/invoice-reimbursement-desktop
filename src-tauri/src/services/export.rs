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

        let exported_at = Utc::now();
        let operation_id = Uuid::new_v4();
        let staging_component = format!("export-{operation_id}");
        let final_component = export_directory_name(&batch.name, &exported_at);
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

fn export_directory_name(batch_name: &str, exported_at: &DateTime<Utc>) -> String {
    let sanitized = sanitize_filename::sanitize(batch_name);
    let sanitized = if sanitized.trim().is_empty() {
        "batch"
    } else {
        sanitized.as_str()
    };
    let suffix = format!("-{}", exported_at.format("%Y%m%d-%H%M%S"));
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
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use chrono::{DateTime, NaiveDate, TimeZone, Utc};
    use lopdf::{Document, Object, dictionary};
    use tokio::sync::{Notify, oneshot};
    use uuid::Uuid;

    use super::{
        DirectoryPublisher, ExportCommitter, ExportCoordinator, ExportOutcomeClassifier,
        ExportService, GenerationHook, OwnedExportDirectory, sha256_hex,
    };
    use crate::db;
    use crate::db::batches::{BatchExportClaim, BatchRepository};
    use crate::db::items::{ItemRepository, NewItemRecord};
    use crate::domain::error::AppError;
    use crate::domain::model::{
        BatchStatus, Category, ConfirmationStatus, DedupeStatus, NewBatch, RecognitionStatus,
        SourceType,
    };
    use crate::infra::files::{AppPaths, read_contained_regular_file_with_hooks};

    struct GatedRealPublisher {
        published: Mutex<Option<oneshot::Sender<PathBuf>>>,
        release: Arc<Notify>,
    }

    struct GatedRealCommitter {
        committed: Mutex<Option<oneshot::Sender<()>>>,
        release: Arc<Notify>,
    }

    struct PreCommitGatedRealCommitter {
        started: Mutex<Option<oneshot::Sender<()>>>,
        release: Arc<Notify>,
    }

    struct GatedFailingCommitter {
        started: Mutex<Option<oneshot::Sender<()>>>,
        release: Arc<Notify>,
    }

    struct CommitThenFailCommitter;

    struct IndeterminateOutcomeClassifier;

    struct DeleteJournalAfterClassifyingCommit {
        pool: sqlx::SqlitePool,
    }

    struct MarkerTamperingPublisher;

    #[cfg(unix)]
    struct SymlinkReplacingFailingPublisher {
        external: PathBuf,
    }

    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CaptureWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct HeartbeatGenerationHook {
        requested: Arc<AtomicBool>,
        heartbeat: Arc<AtomicBool>,
    }

    struct BlockingGenerationHook {
        started: Mutex<Option<oneshot::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl GenerationHook for BlockingGenerationHook {
        fn before_generation(&self) -> Result<(), AppError> {
            self.started
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .map_err(|_| AppError::Internal {
                    message: "generation start observer closed".to_owned(),
                })?;
            self.release
                .lock()
                .unwrap()
                .recv()
                .map_err(|_| AppError::Internal {
                    message: "generation release observer closed".to_owned(),
                })
        }
    }

    struct RecordingGenerationHook {
        started: Arc<AtomicBool>,
    }

    impl GenerationHook for RecordingGenerationHook {
        fn before_generation(&self) -> Result<(), AppError> {
            self.started.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    impl GenerationHook for HeartbeatGenerationHook {
        fn before_generation(&self) -> Result<(), AppError> {
            self.requested.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(1);
            while !self.heartbeat.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    return Err(AppError::Internal {
                        message: "async runtime heartbeat was blocked".to_owned(),
                    });
                }
                std::thread::yield_now();
            }
            Ok(())
        }
    }

    #[async_trait]
    impl ExportCommitter for GatedRealCommitter {
        async fn commit(
            &self,
            claim: BatchExportClaim,
            operation_id: Uuid,
            exported_at: DateTime<Utc>,
        ) -> Result<(), AppError> {
            let result = claim.commit(operation_id, exported_at).await.map(|_| ());
            if result.is_ok() {
                self.committed
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
            }
            self.release.notified().await;
            result
        }
    }

    #[async_trait]
    impl ExportCommitter for PreCommitGatedRealCommitter {
        async fn commit(
            &self,
            claim: BatchExportClaim,
            operation_id: Uuid,
            exported_at: DateTime<Utc>,
        ) -> Result<(), AppError> {
            self.started
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            self.release.notified().await;
            claim.commit(operation_id, exported_at).await.map(|_| ())
        }
    }

    #[async_trait]
    impl ExportCommitter for GatedFailingCommitter {
        async fn commit(
            &self,
            _claim: BatchExportClaim,
            _operation_id: Uuid,
            _exported_at: DateTime<Utc>,
        ) -> Result<(), AppError> {
            self.started
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            self.release.notified().await;
            Err(AppError::Internal {
                message: "injected detached commit failure".to_owned(),
            })
        }
    }

    #[async_trait]
    impl ExportCommitter for CommitThenFailCommitter {
        async fn commit(
            &self,
            claim: BatchExportClaim,
            operation_id: Uuid,
            exported_at: DateTime<Utc>,
        ) -> Result<(), AppError> {
            claim.commit(operation_id, exported_at).await?;
            Err(AppError::External {
                service: "sqlite".to_owned(),
                retryable: true,
                message: "injected ambiguous commit outcome".to_owned(),
            })
        }
    }

    #[async_trait]
    impl ExportOutcomeClassifier for IndeterminateOutcomeClassifier {
        async fn classify(
            &self,
            _expected: &crate::services::export_recovery::PendingExportIdentity,
        ) -> crate::services::export_recovery::ExportCommitOutcome {
            crate::services::export_recovery::ExportCommitOutcome::Indeterminate(
                AppError::Internal {
                    message: "injected outcome read failure at /private/export.db".to_owned(),
                },
            )
        }
    }

    #[async_trait]
    impl ExportOutcomeClassifier for DeleteJournalAfterClassifyingCommit {
        async fn classify(
            &self,
            expected: &crate::services::export_recovery::PendingExportIdentity,
        ) -> crate::services::export_recovery::ExportCommitOutcome {
            let classified =
                crate::services::export_recovery::ExportJournal::new(self.pool.clone())
                    .classify_commit_outcome(expected)
                    .await;
            assert!(matches!(
                classified,
                crate::services::export_recovery::ExportCommitOutcome::ConfirmedCommitted
            ));
            let deleted = sqlx::query("DELETE FROM pending_exports WHERE operation_id = ?")
                .bind(expected.operation_id.to_string())
                .execute(&self.pool)
                .await
                .unwrap();
            assert_eq!(deleted.rows_affected(), 1);
            crate::services::export_recovery::ExportCommitOutcome::Indeterminate(
                AppError::Internal {
                    message: "injected outcome race at /private/recovery.db".to_owned(),
                },
            )
        }
    }

    #[async_trait]
    impl DirectoryPublisher for GatedRealPublisher {
        async fn publish(
            &self,
            staging: OwnedExportDirectory,
            destination: PathBuf,
        ) -> Result<OwnedExportDirectory, AppError> {
            let published = tokio::task::spawn_blocking(move || staging.publish(destination))
                .await
                .map_err(|_| AppError::Internal {
                    message: "test publisher task failed".to_owned(),
                })??;
            self.published
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(published.path().to_path_buf())
                .unwrap();
            self.release.notified().await;
            Ok(published)
        }
    }

    #[async_trait]
    impl DirectoryPublisher for MarkerTamperingPublisher {
        async fn publish(
            &self,
            staging: OwnedExportDirectory,
            destination: PathBuf,
        ) -> Result<OwnedExportDirectory, AppError> {
            let published = tokio::task::spawn_blocking(move || staging.publish(destination))
                .await
                .map_err(|_| AppError::Internal {
                    message: "test publisher task failed".to_owned(),
                })??;
            let marker_path = published.path().join(".invoice-export-recovery.json");
            let mut marker: serde_json::Value =
                serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
            marker["final_component"] = serde_json::json!("tampered-final-component");
            fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
            Ok(published)
        }
    }

    #[cfg(unix)]
    #[async_trait]
    impl DirectoryPublisher for SymlinkReplacingFailingPublisher {
        async fn publish(
            &self,
            staging: OwnedExportDirectory,
            _destination: PathBuf,
        ) -> Result<OwnedExportDirectory, AppError> {
            use std::os::unix::fs::symlink;

            let path = staging.path().to_path_buf();
            std::mem::forget(staging);
            fs::remove_dir_all(&path).unwrap();
            symlink(&self.external, path).unwrap();
            Err(AppError::Internal {
                message: "injected publisher failure".to_owned(),
            })
        }
    }

    struct ExportFixture {
        _directory: tempfile::TempDir,
        paths: AppPaths,
        pool: sqlx::SqlitePool,
        batch_id: Uuid,
    }

    async fn export_fixture(name: &str) -> ExportFixture {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let batch = BatchRepository::new(pool.clone())
            .create(NewBatch::try_new(name, "2026-07-01", "2026-07-31", None).unwrap())
            .await
            .unwrap();
        let item_id = Uuid::new_v4();
        let original = paths.originals.join(format!("{item_id}.pdf"));
        let normalized = paths.normalized.join(format!("{item_id}.pdf"));
        fs::write(&original, b"original").unwrap();
        fs::write(&normalized, one_page_pdf()).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 13, 2, 0, 0).unwrap();
        ItemRepository::new(pool.clone())
            .insert(&NewItemRecord {
                id: item_id,
                original_name: "invoice.pdf".to_owned(),
                original_path: original.to_string_lossy().into_owned(),
                normalized_pdf_path: Some(normalized.to_string_lossy().into_owned()),
                sha256: sha256_hex(b"original"),
                mime_type: "application/pdf".to_owned(),
                source_type: SourceType::ManualUpload,
                source_account_id: None,
                source_mailbox: None,
                source_uid_validity: None,
                source_uid: None,
                source_message_id: None,
                source_part_id: None,
                fetched_at: now,
                invoice_date: Some(NaiveDate::from_ymd_opt(2026, 7, 12).unwrap()),
                suggested_period: Some("2026-07".to_owned()),
                batch_id: Some(batch.id),
                suggested_category: Some(Category::Transport),
                final_category: Some(Category::Transport),
                amount_cents: Some(12_345),
                currency: "CNY".to_owned(),
                city: None,
                company: None,
                recognition_status: RecognitionStatus::Succeeded,
                confirmation_status: ConfirmationStatus::Confirmed,
                dedupe_status: DedupeStatus::Unique,
                duplicate_of_id: None,
                note: None,
                event_tag: None,
                project_tag: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        ExportFixture {
            _directory: directory,
            paths,
            pool,
            batch_id: batch.id,
        }
    }

    #[tokio::test]
    async fn cancellation_after_real_publication_removes_final_and_keeps_batch_draft() {
        let fixture = export_fixture("取消测试").await;
        let (published_tx, published_rx) = oneshot::channel();
        let release = Arc::new(Notify::new());
        let service = ExportService::with_publisher(
            fixture.pool.clone(),
            fixture.paths.clone(),
            Arc::new(GatedRealPublisher {
                published: Mutex::new(Some(published_tx)),
                release: release.clone(),
            }),
        );
        let batch_id = fixture.batch_id;
        let task_service = service.clone();
        let task = tokio::spawn(async move { task_service.export(batch_id).await });

        let final_directory = tokio::time::timeout(std::time::Duration::from_secs(5), published_rx)
            .await
            .expect("publisher should report final rename")
            .expect("publisher signal should remain open");
        assert!(final_directory.is_dir());
        assert_eq!(fs::read_dir(&fixture.paths.staging).unwrap().count(), 0);

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        release.notify_waiters();
        tokio::time::timeout(Duration::from_secs(5), async {
            while service.coordinator.active_exports.contains_key(&batch_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("internal finalizer should finish rollback before assertions");

        assert!(!final_directory.exists());
        assert_eq!(fs::read_dir(&fixture.paths.staging).unwrap().count(), 0);
        let batch = BatchRepository::new(fixture.pool)
            .get(batch_id)
            .await
            .unwrap();
        assert_eq!(batch.status, BatchStatus::Draft);
        assert_eq!(batch.last_exported_at, None);
    }

    #[tokio::test]
    async fn cancellation_after_database_commit_keeps_final_and_exported_batch() {
        let fixture = export_fixture("提交取消测试").await;
        let (committed_tx, committed_rx) = oneshot::channel();
        let release = Arc::new(Notify::new());
        let service = ExportService::with_committer(
            fixture.pool.clone(),
            fixture.paths.clone(),
            Arc::new(GatedRealCommitter {
                committed: Mutex::new(Some(committed_tx)),
                release: release.clone(),
            }),
        );
        let task_service = service.clone();
        let batch_id = fixture.batch_id;
        let task = tokio::spawn(async move { task_service.export(batch_id).await });

        tokio::time::timeout(std::time::Duration::from_secs(5), committed_rx)
            .await
            .expect("committer should report the durable database commit")
            .expect("committer signal should remain open");
        let final_directory = fs::read_dir(&fixture.paths.exports)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(final_directory.is_dir());
        assert!(service.coordinator.active_exports.contains_key(&batch_id));

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        assert!(final_directory.is_dir());
        assert!(service.coordinator.active_exports.contains_key(&batch_id));
        release.notify_waiters();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while service.coordinator.active_exports.contains_key(&batch_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("internal finalizer should release the batch guard");

        assert!(final_directory.is_dir());
        let persisted = BatchRepository::new(fixture.pool)
            .get(batch_id)
            .await
            .unwrap();
        assert_eq!(persisted.status, BatchStatus::Exported);
        assert!(persisted.last_exported_at.is_some());
    }

    #[tokio::test]
    async fn commit_error_after_durable_commit_preserves_package_and_returns_success() {
        let fixture = export_fixture("提交结果歧义测试").await;
        let service = ExportService::with_committer(
            fixture.pool.clone(),
            fixture.paths.clone(),
            Arc::new(CommitThenFailCommitter),
        );

        let result = service.export(fixture.batch_id).await.unwrap();

        assert!(result.directory.is_dir());
        assert!(
            !result
                .directory
                .join(".invoice-export-recovery.json")
                .exists()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            0
        );
        let batch = BatchRepository::new(fixture.pool)
            .get(fixture.batch_id)
            .await
            .unwrap();
        assert_eq!(batch.status, BatchStatus::Exported);
        assert!(batch.last_exported_at.is_some());
    }

    #[tokio::test]
    async fn ambiguous_reexport_commit_is_not_confused_with_previous_export() {
        let fixture = export_fixture("首次成功导出").await;
        let initial = ExportService::new(
            fixture.pool.clone(),
            fixture.paths.clone(),
            ExportCoordinator::default(),
        )
        .export(fixture.batch_id)
        .await
        .unwrap();
        let previous_exported_at = BatchRepository::new(fixture.pool.clone())
            .get(fixture.batch_id)
            .await
            .unwrap()
            .last_exported_at
            .unwrap();
        BatchRepository::new(fixture.pool.clone())
            .update(
                fixture.batch_id,
                NewBatch::try_new("再次导出", "2026-07-01", "2026-07-31", None).unwrap(),
            )
            .await
            .unwrap();
        let service = ExportService::with_committer(
            fixture.pool.clone(),
            fixture.paths.clone(),
            Arc::new(CommitThenFailCommitter),
        );

        let result = service.export(fixture.batch_id).await.unwrap();

        assert!(initial.directory.is_dir());
        assert!(result.directory.is_dir());
        assert_ne!(initial.directory, result.directory);
        let batch = BatchRepository::new(fixture.pool)
            .get(fixture.batch_id)
            .await
            .unwrap();
        assert_eq!(batch.status, BatchStatus::Exported);
        assert!(batch.last_exported_at.unwrap() > previous_exported_at);
    }

    #[tokio::test]
    async fn indeterminate_commit_outcome_preserves_durable_recovery_state_until_restart() {
        let fixture = export_fixture("不可判定提交测试").await;
        let service = ExportService {
            pool: fixture.pool.clone(),
            paths: fixture.paths.clone(),
            coordinator: ExportCoordinator::default(),
            publisher: Arc::new(super::AtomicDirectoryPublisher),
            committer: Arc::new(CommitThenFailCommitter),
            outcome_classifier: Arc::new(IndeterminateOutcomeClassifier),
            generation_hook: Arc::new(super::NoopGenerationHook),
            preference_gate: Default::default(),
        };

        let error = service.export(fixture.batch_id).await.unwrap_err();

        assert!(matches!(
            &error,
            AppError::External {
                service,
                retryable: true,
                ..
            } if service == "export_recovery"
        ));
        assert!(!error.to_string().contains("/private/export.db"));
        let final_directory = fs::read_dir(&fixture.paths.exports)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(final_directory.is_dir());
        assert!(
            final_directory
                .join(".invoice-export-recovery.json")
                .is_file()
        );
        let journal = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT state, last_error FROM pending_exports WHERE batch_id = ?",
        )
        .bind(fixture.batch_id.to_string())
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
        assert_eq!(journal.0, "published");
        assert_eq!(
            journal.1.as_deref(),
            Some("export_commit_outcome_indeterminate")
        );
        assert!(!journal.1.unwrap().contains("/private/export.db"));

        let restarted = ExportService::new(
            fixture.pool.clone(),
            fixture.paths.clone(),
            ExportCoordinator::default(),
        );
        let report = restarted.reconcile_pending().await.unwrap();

        assert_eq!(report.preserved, 1);
        assert!(final_directory.is_dir());
        assert!(
            !final_directory
                .join(".invoice-export-recovery.json")
                .exists()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn indeterminate_commit_rebuilds_a_deleted_journal_from_the_final_marker() {
        let fixture = export_fixture("不可判定提交删索引测试").await;
        let service = ExportService {
            pool: fixture.pool.clone(),
            paths: fixture.paths.clone(),
            coordinator: ExportCoordinator::default(),
            publisher: Arc::new(super::AtomicDirectoryPublisher),
            committer: Arc::new(CommitThenFailCommitter),
            outcome_classifier: Arc::new(DeleteJournalAfterClassifyingCommit {
                pool: fixture.pool.clone(),
            }),
            generation_hook: Arc::new(super::NoopGenerationHook),
            preference_gate: Default::default(),
        };

        let error = service.export(fixture.batch_id).await.unwrap_err();

        assert!(matches!(
            &error,
            AppError::External {
                service,
                retryable: true,
                ..
            } if service == "export_recovery"
        ));
        let final_directory = fs::read_dir(&fixture.paths.exports)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(
            final_directory
                .join(".invoice-export-recovery.json")
                .is_file()
        );
        let journal = sqlx::query_as::<_, (String, i64, Option<String>)>(
            "SELECT state, interrupted, last_error FROM pending_exports WHERE batch_id = ?",
        )
        .bind(fixture.batch_id.to_string())
        .fetch_one(&fixture.pool)
        .await
        .expect("the final marker must reconstruct the missing recovery index");
        assert_eq!(journal.0, "published");
        assert_eq!(journal.1, 1);
        assert_eq!(
            journal.2.as_deref(),
            Some("export_commit_outcome_indeterminate")
        );
        assert!(!journal.2.unwrap().contains("/private/recovery.db"));

        let restarted = ExportService::new(
            fixture.pool.clone(),
            fixture.paths.clone(),
            ExportCoordinator::default(),
        );
        let report = restarted.reconcile_pending().await.unwrap();

        assert_eq!(report.preserved, 1);
        assert!(final_directory.is_dir());
        assert!(
            !final_directory
                .join(".invoice-export-recovery.json")
                .exists()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn restart_rebuilds_a_missing_journal_from_a_valid_final_marker() {
        let fixture = export_fixture("final marker启动发现测试").await;
        let operation_id = Uuid::new_v4();
        let exported_at = Utc::now();
        let staging_component = format!("export-{operation_id}");
        let final_component = "final-marker-discovery";
        let final_directory = fixture.paths.exports.join(final_component);
        fs::create_dir(&final_directory).unwrap();
        fs::write(final_directory.join("artifact"), b"durable").unwrap();
        crate::services::export_recovery::write_generation_marker(
            &final_directory,
            operation_id,
            fixture.batch_id,
            &staging_component,
            final_component,
            exported_at,
        )
        .unwrap();
        sqlx::query(
            "UPDATE batches SET status = 'exported', last_exported_at = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(exported_at.to_rfc3339())
        .bind(exported_at.to_rfc3339())
        .bind(fixture.batch_id.to_string())
        .execute(&fixture.pool)
        .await
        .unwrap();

        let report = ExportService::new(
            fixture.pool.clone(),
            fixture.paths,
            ExportCoordinator::default(),
        )
        .reconcile_pending()
        .await
        .unwrap();

        assert_eq!(report.preserved, 1);
        assert!(final_directory.join("artifact").is_file());
        assert!(
            !final_directory
                .join(".invoice-export-recovery.json")
                .exists()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn restart_fails_closed_on_a_final_marker_for_another_directory() {
        let fixture = export_fixture("final marker目录错配测试").await;
        let operation_id = Uuid::new_v4();
        let exported_at = Utc::now();
        let staging_component = format!("export-{operation_id}");
        let final_component = "actual-final-directory";
        let final_directory = fixture.paths.exports.join(final_component);
        fs::create_dir(&final_directory).unwrap();
        fs::write(final_directory.join("artifact"), b"keep").unwrap();
        crate::services::export_recovery::write_generation_marker(
            &final_directory,
            operation_id,
            fixture.batch_id,
            &staging_component,
            "different-final-directory",
            exported_at,
        )
        .unwrap();

        let error = ExportService::new(
            fixture.pool.clone(),
            fixture.paths,
            ExportCoordinator::default(),
        )
        .reconcile_pending()
        .await
        .unwrap_err();

        assert!(error.to_string().contains("inconsistent"));
        assert!(final_directory.join("artifact").is_file());
        assert!(
            final_directory
                .join(".invoice-export-recovery.json")
                .is_file()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn restart_does_not_confuse_a_rebuilt_marker_with_a_prior_export() {
        let fixture = export_fixture("final marker未提交测试").await;
        let operation_id = Uuid::new_v4();
        let exported_at = Utc::now();
        let previous_exported_at = exported_at - chrono::Duration::seconds(1);
        let staging_component = format!("export-{operation_id}");
        let final_component = "uncommitted-final-marker";
        let historical_directory = fixture.paths.exports.join("prior-export");
        fs::create_dir(&historical_directory).unwrap();
        fs::write(historical_directory.join("artifact"), b"committed").unwrap();
        let final_directory = fixture.paths.exports.join(final_component);
        fs::create_dir(&final_directory).unwrap();
        fs::write(final_directory.join("artifact"), b"uncommitted").unwrap();
        crate::services::export_recovery::write_generation_marker(
            &final_directory,
            operation_id,
            fixture.batch_id,
            &staging_component,
            final_component,
            exported_at,
        )
        .unwrap();
        sqlx::query(
            "UPDATE batches SET status = 'exported', last_exported_at = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(previous_exported_at.to_rfc3339())
        .bind(previous_exported_at.to_rfc3339())
        .bind(fixture.batch_id.to_string())
        .execute(&fixture.pool)
        .await
        .unwrap();

        let report = ExportService::new(
            fixture.pool.clone(),
            fixture.paths,
            ExportCoordinator::default(),
        )
        .reconcile_pending()
        .await
        .unwrap();

        assert_eq!(report.rolled_back, 1);
        assert!(!final_directory.exists());
        assert!(historical_directory.join("artifact").is_file());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn restart_ignores_an_ordinary_historical_export_without_a_marker() {
        let fixture = export_fixture("普通历史包测试").await;
        let historical = fixture.paths.exports.join("historical-export");
        fs::create_dir(&historical).unwrap();
        fs::write(historical.join("manifest.json"), b"{}").unwrap();

        let report = ExportService::new(fixture.pool, fixture.paths, ExportCoordinator::default())
            .reconcile_pending()
            .await
            .unwrap();

        assert_eq!(
            report,
            crate::services::export::ExportRecoveryReport::default()
        );
        assert!(historical.join("manifest.json").is_file());
    }

    #[tokio::test]
    async fn restart_fails_closed_on_a_malformed_final_marker() {
        let fixture = export_fixture("畸形final marker测试").await;
        let final_directory = fixture.paths.exports.join("malformed-final-marker");
        fs::create_dir(&final_directory).unwrap();
        fs::write(final_directory.join("artifact"), b"keep").unwrap();
        fs::write(
            final_directory.join(".invoice-export-recovery.json"),
            b"{not-json",
        )
        .unwrap();

        let error = ExportService::new(
            fixture.pool.clone(),
            fixture.paths,
            ExportCoordinator::default(),
        )
        .reconcile_pending()
        .await
        .unwrap_err();

        assert!(error.to_string().contains("marker is invalid"));
        assert!(final_directory.join("artifact").is_file());
        assert!(
            final_directory
                .join(".invoice-export-recovery.json")
                .is_file()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_never_follows_a_final_recovery_marker_symlink() {
        use std::os::unix::fs::symlink;

        let fixture = export_fixture("final marker符号链接测试").await;
        let external = tempfile::tempdir().unwrap();
        let external_marker = external.path().join("external-marker");
        fs::write(&external_marker, b"outside").unwrap();
        let final_directory = fixture.paths.exports.join("symlink-final-marker");
        fs::create_dir(&final_directory).unwrap();
        fs::write(final_directory.join("artifact"), b"keep").unwrap();
        symlink(
            &external_marker,
            final_directory.join(".invoice-export-recovery.json"),
        )
        .unwrap();

        let error = ExportService::new(
            fixture.pool.clone(),
            fixture.paths,
            ExportCoordinator::default(),
        )
        .reconcile_pending()
        .await
        .unwrap_err();

        assert!(error.to_string().contains("not a safe file"));
        assert_eq!(fs::read(&external_marker).unwrap(), b"outside");
        assert!(final_directory.join("artifact").is_file());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn marker_rebuild_never_overwrites_a_conflicting_operation_index() {
        let fixture = export_fixture("marker索引冲突测试").await;
        let marker_operation_id = Uuid::new_v4();
        let indexed_operation_id = Uuid::new_v4();
        let exported_at = Utc::now();
        let marker_staging_component = format!("export-{marker_operation_id}");
        let indexed_staging_component = format!("export-{indexed_operation_id}");
        let final_component = "conflicting-final-component";
        let final_directory = fixture.paths.exports.join(final_component);
        fs::create_dir(&final_directory).unwrap();
        fs::write(final_directory.join("artifact"), b"keep").unwrap();
        crate::services::export_recovery::write_generation_marker(
            &final_directory,
            marker_operation_id,
            fixture.batch_id,
            &marker_staging_component,
            final_component,
            exported_at,
        )
        .unwrap();
        crate::services::export_recovery::ExportJournal::new(fixture.pool.clone())
            .create(
                indexed_operation_id,
                fixture.batch_id,
                &indexed_staging_component,
                final_component,
                exported_at,
            )
            .await
            .unwrap();

        let error = ExportService::new(
            fixture.pool.clone(),
            fixture.paths,
            ExportCoordinator::default(),
        )
        .reconcile_pending()
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("conflicts with the durable index")
        );
        assert!(final_directory.join("artifact").is_file());
        let indexed = sqlx::query_as::<_, (String, String, String)>(
            "SELECT operation_id, staging_component, state FROM pending_exports",
        )
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
        assert_eq!(indexed.0, indexed_operation_id.to_string());
        assert_eq!(indexed.1, indexed_staging_component);
        assert_eq!(indexed.2, "generating");
    }

    #[tokio::test]
    async fn committed_marker_must_match_every_durable_operation_field() {
        let fixture = export_fixture("marker完整性测试").await;
        let service = ExportService::with_publisher(
            fixture.pool.clone(),
            fixture.paths.clone(),
            Arc::new(MarkerTamperingPublisher),
        );

        let error = service.export(fixture.batch_id).await.unwrap_err();

        assert!(matches!(
            &error,
            AppError::External {
                service,
                retryable: true,
                ..
            } if service == "export_recovery"
        ));
        let final_directory = fs::read_dir(&fixture.paths.exports)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(final_directory.is_dir());
        assert!(
            final_directory
                .join(".invoice-export-recovery.json")
                .is_file()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            1
        );
        let batch = BatchRepository::new(fixture.pool)
            .get(fixture.batch_id)
            .await
            .unwrap();
        assert_eq!(batch.status, BatchStatus::Exported);
        assert!(batch.last_exported_at.is_some());
    }

    #[tokio::test]
    async fn uncommitted_rollback_preflights_all_markers_before_removing_any_directory() {
        let fixture = export_fixture("回滚marker预检测试").await;
        let operation_id = Uuid::new_v4();
        let exported_at = Utc::now();
        let staging_component = format!("export-{operation_id}");
        let final_component = "rollback-marker-preflight";
        let staging = fixture.paths.staging.join(&staging_component);
        let final_directory = fixture.paths.exports.join(final_component);
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&final_directory).unwrap();
        crate::services::export_recovery::write_generation_marker(
            &staging,
            operation_id,
            fixture.batch_id,
            &staging_component,
            final_component,
            exported_at,
        )
        .unwrap();
        crate::services::export_recovery::write_generation_marker(
            &final_directory,
            operation_id,
            fixture.batch_id,
            &staging_component,
            "tampered-final-component",
            exported_at,
        )
        .unwrap();
        let journal = crate::services::export_recovery::ExportJournal::new(fixture.pool.clone());
        journal
            .create(
                operation_id,
                fixture.batch_id,
                &staging_component,
                final_component,
                exported_at,
            )
            .await
            .unwrap();
        let expected = crate::services::export_recovery::PendingExportIdentity::new(
            operation_id,
            fixture.batch_id,
            staging_component,
            final_component.to_owned(),
            exported_at,
        );

        let error = journal
            .rollback_uncommitted(&fixture.paths, &expected)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("mismatched recovery marker"));
        assert!(staging.is_dir());
        assert!(final_directory.is_dir());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            1
        );
    }

    #[test]
    fn detached_finalizer_logs_commit_failure_after_caller_cancellation() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || CaptureWriter(writer_output.clone()))
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let fixture = export_fixture("游离终结失败测试").await;
                    let (started_tx, started_rx) = oneshot::channel();
                    let release = Arc::new(Notify::new());
                    let service = ExportService::with_committer(
                        fixture.pool,
                        fixture.paths.clone(),
                        Arc::new(GatedFailingCommitter {
                            started: Mutex::new(Some(started_tx)),
                            release: release.clone(),
                        }),
                    );
                    let task_service = service.clone();
                    let batch_id = fixture.batch_id;
                    let task = tokio::spawn(async move { task_service.export(batch_id).await });

                    tokio::time::timeout(Duration::from_secs(5), started_rx)
                        .await
                        .unwrap()
                        .unwrap();
                    task.abort();
                    assert!(task.await.unwrap_err().is_cancelled());
                    release.notify_waiters();
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while service.coordinator.active_exports.contains_key(&batch_id) {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    assert_eq!(fs::read_dir(&fixture.paths.exports).unwrap().count(), 0);
                });
        });

        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("export finalization failed"));
        assert!(logs.contains("injected detached commit failure"));
    }

    #[cfg(unix)]
    #[test]
    fn source_replaced_by_external_symlink_after_parent_anchor_is_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("originals");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let source = nested.join("invoice.pdf");
        fs::write(&source, b"trusted invoice").unwrap();
        let external = directory.path().join("external-secret.pdf");
        fs::write(&external, b"external secret").unwrap();
        let (anchored_tx, anchored_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let reader_source = source.clone();
        let reader_root = root.clone();
        let reader = std::thread::spawn(move || {
            read_contained_regular_file_with_hooks(
                &reader_source,
                &reader_root,
                "original",
                u64::MAX,
                || {
                    anchored_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
                || {},
            )
        });

        anchored_rx.recv().unwrap();
        fs::remove_file(&source).unwrap();
        symlink(&external, &source).unwrap();
        release_tx.send(()).unwrap();

        let error = reader.join().unwrap().unwrap_err();
        assert!(matches!(error, AppError::Validation { field, .. } if field == "original"));
    }

    #[test]
    fn non_unix_export_source_open_fails_closed_by_construction() {
        let source = include_str!("../infra/files.rs");
        let start = source
            .find("#[cfg(not(unix))]\nfn open_contained_regular_file_with_hook")
            .unwrap();
        let remainder = &source[start..];
        let end = remainder.find("\n#[cfg(test)]").unwrap();
        let implementation = &remainder[..end];

        assert!(!implementation.contains("canonicalize"));
        assert!(!implementation.contains("File::open"));
        assert!(implementation.contains("unsupported_contained_file_platform(field)"));
    }

    #[test]
    fn oversized_source_is_rejected_before_read_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("originals");
        fs::create_dir(&root).unwrap();
        let source = root.join("invoice.pdf");
        let file = fs::File::create(&source).unwrap();
        file.set_len(1_024).unwrap();
        drop(file);
        let read_started = AtomicBool::new(false);

        let result = read_contained_regular_file_with_hooks(
            &source,
            &root,
            "original",
            16,
            || {},
            || read_started.store(true, Ordering::SeqCst),
        );

        assert!(matches!(result, Err(AppError::Validation { field, .. }) if field == "original"));
        assert!(!read_started.load(Ordering::SeqCst));
    }

    #[test]
    fn aggregate_source_budget_is_enforced_during_preflight() {
        let error =
            super::validate_source_lengths(&[(8, "original"), (9, "normalizedPdf")], 10, 16)
                .unwrap_err();

        assert!(matches!(error, AppError::Validation { field, .. } if field == "export"));
    }

    #[test]
    fn batch_directory_name_reserves_suffix_at_utf8_boundary() {
        let exported_at = Utc.with_ymd_and_hms(2026, 7, 15, 12, 34, 56).unwrap();

        let component = super::export_directory_name(&"报".repeat(100), &exported_at);

        assert!(component.len() <= 255);
        assert!(component.ends_with("-20260715-123456"));
        assert!(component.starts_with('报'));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_generation_does_not_stop_the_async_runtime_heartbeat() {
        let fixture = export_fixture("异步心跳测试").await;
        let heartbeat = Arc::new(AtomicBool::new(false));
        let requested = Arc::new(AtomicBool::new(false));
        let service = ExportService::with_generation_hook(
            fixture.pool,
            fixture.paths,
            Arc::new(HeartbeatGenerationHook {
                requested: requested.clone(),
                heartbeat: heartbeat.clone(),
            }),
        );
        let heartbeat_task = tokio::spawn(async move {
            while !requested.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            tokio::task::yield_now().await;
            heartbeat.store(true, Ordering::SeqCst);
        });

        let result = service.export(fixture.batch_id).await;

        heartbeat_task.await.unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn shared_coordinator_rejects_second_service_before_generation() {
        let fixture = export_fixture("共享协调测试").await;
        let coordinator = ExportCoordinator::default();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first = ExportService::with_coordinator_and_generation_hook(
            fixture.pool.clone(),
            fixture.paths.clone(),
            coordinator.clone(),
            Arc::new(BlockingGenerationHook {
                started: Mutex::new(Some(started_tx)),
                release: Mutex::new(release_rx),
            }),
        );
        let second_started = Arc::new(AtomicBool::new(false));
        let second = ExportService::with_coordinator_and_generation_hook(
            fixture.pool,
            fixture.paths,
            coordinator,
            Arc::new(RecordingGenerationHook {
                started: second_started.clone(),
            }),
        );
        let batch_id = fixture.batch_id;
        let first_task = tokio::spawn(async move { first.export(batch_id).await });
        started_rx.await.unwrap();

        let second_result = second.export(batch_id).await;

        assert!(matches!(
            second_result,
            Err(AppError::Conflict { message }) if message.contains("正在导出")
        ));
        assert!(!second_started.load(Ordering::SeqCst));
        release_tx.send(()).unwrap();
        first_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn export_shutdown_waits_for_the_real_blocking_generation_job() {
        let fixture = export_fixture("generation shutdown test").await;
        let coordinator = ExportCoordinator::default();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let service = ExportService::with_coordinator_and_generation_hook(
            fixture.pool,
            fixture.paths,
            coordinator.clone(),
            Arc::new(BlockingGenerationHook {
                started: Mutex::new(Some(started_tx)),
                release: Mutex::new(release_rx),
            }),
        );
        let batch_id = fixture.batch_id;
        let export = tokio::spawn(async move { service.export(batch_id).await });
        started_rx.await.unwrap();

        let shutdown = coordinator.begin_shutdown();
        let mut wait = Box::pin(shutdown.wait());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut wait)
                .await
                .is_err(),
            "shutdown must not finish while spawn_blocking generation can still mutate staging"
        );

        release_tx.send(()).unwrap();
        export.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("shutdown should finish after generation and finalization")
            .unwrap();
    }

    #[tokio::test]
    async fn export_shutdown_waits_for_a_committed_but_blocked_finalizer() {
        let fixture = export_fixture("committer shutdown test").await;
        let coordinator = ExportCoordinator::default();
        let (committed_tx, committed_rx) = oneshot::channel();
        let release = Arc::new(Notify::new());
        let outcome_classifier = super::default_outcome_classifier(&fixture.pool);
        let service = ExportService {
            pool: fixture.pool,
            paths: fixture.paths,
            coordinator: coordinator.clone(),
            publisher: Arc::new(super::AtomicDirectoryPublisher),
            committer: Arc::new(GatedRealCommitter {
                committed: Mutex::new(Some(committed_tx)),
                release: release.clone(),
            }),
            outcome_classifier,
            generation_hook: Arc::new(super::NoopGenerationHook),
            preference_gate: Default::default(),
        };
        let batch_id = fixture.batch_id;
        let export = tokio::spawn(async move { service.export(batch_id).await });
        committed_rx.await.unwrap();

        let shutdown = coordinator.begin_shutdown();
        let mut wait = Box::pin(shutdown.wait());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut wait)
                .await
                .is_err(),
            "shutdown must not finish before a post-commit finalizer preserves its package"
        );

        release.notify_waiters();
        export.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("shutdown should finish after finalization")
            .unwrap();
    }

    #[tokio::test]
    async fn recovery_gate_enters_only_after_the_live_export_lease_is_released() {
        let coordinator = ExportCoordinator::default();
        let active = coordinator.acquire_export(Uuid::new_v4()).await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (attempted_tx, attempted_rx) = oneshot::channel();
        let (acquired_tx, acquired_rx) = oneshot::channel();
        let recovery_coordinator = coordinator.clone();
        let recovery_order = order.clone();
        let recovery = tokio::spawn(async move {
            recovery_order.lock().unwrap().push("attempted");
            attempted_tx.send(()).unwrap();
            let _guard = recovery_coordinator.acquire_recovery().await;
            recovery_order.lock().unwrap().push("acquired");
            acquired_tx.send(()).unwrap();
        });

        attempted_rx.await.unwrap();
        order.lock().unwrap().push("released");
        drop(active);
        acquired_rx.await.unwrap();
        recovery.await.unwrap();

        assert_eq!(
            *order.lock().unwrap(),
            ["attempted", "released", "acquired"]
        );
    }

    #[tokio::test]
    async fn recovery_waits_for_live_export_finalization_and_preserves_its_package() {
        let fixture = export_fixture("live recovery exclusion").await;
        let coordinator = ExportCoordinator::default();
        let (commit_started_tx, commit_started_rx) = oneshot::channel();
        let commit_release = Arc::new(Notify::new());
        let service = ExportService {
            pool: fixture.pool.clone(),
            paths: fixture.paths.clone(),
            coordinator,
            publisher: Arc::new(super::AtomicDirectoryPublisher),
            committer: Arc::new(PreCommitGatedRealCommitter {
                started: Mutex::new(Some(commit_started_tx)),
                release: commit_release.clone(),
            }),
            outcome_classifier: super::default_outcome_classifier(&fixture.pool),
            generation_hook: Arc::new(super::NoopGenerationHook),
            preference_gate: Default::default(),
        };
        let batch_id = fixture.batch_id;
        let export_service = service.clone();
        let export = tokio::spawn(async move { export_service.export(batch_id).await });
        commit_started_rx.await.unwrap();
        let final_directory = fs::read_dir(&fixture.paths.exports)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .expect("publisher must create the final package before commit");
        assert!(final_directory.is_dir());

        let (recovery_attempted_tx, recovery_attempted_rx) = oneshot::channel();
        let recovery_service = service.clone();
        let recovery = tokio::spawn(async move {
            recovery_attempted_tx.send(()).unwrap();
            recovery_service.reconcile_pending().await
        });
        recovery_attempted_rx.await.unwrap();
        commit_release.notify_one();

        let result = export.await.unwrap().unwrap();
        recovery.await.unwrap().unwrap();
        let batch = BatchRepository::new(fixture.pool.clone())
            .get(batch_id)
            .await
            .unwrap();
        assert_eq!(batch.status, BatchStatus::Exported);
        assert_eq!(result.directory, final_directory);
        assert!(final_directory.is_dir());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn restart_recovery_rolls_back_uncommitted_and_preserves_committed_exports() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let database_url = format!(
            "sqlite://{}",
            directory.path().join("recovery.db").display()
        );
        let pool = db::connect(&database_url).await.unwrap();
        let batches = BatchRepository::new(pool.clone());
        let draft = batches
            .create(NewBatch::try_new("draft recovery", "2026-07-01", "2026-07-31", None).unwrap())
            .await
            .unwrap();
        let committed = batches
            .create(
                NewBatch::try_new("committed recovery", "2026-07-01", "2026-07-31", None).unwrap(),
            )
            .await
            .unwrap();
        let exported_at = Utc.with_ymd_and_hms(2026, 7, 16, 6, 0, 0).unwrap();
        sqlx::query("UPDATE batches SET status = 'exported', last_exported_at = ? WHERE id = ?")
            .bind(exported_at.to_rfc3339())
            .bind(committed.id.to_string())
            .execute(&pool)
            .await
            .unwrap();

        let draft_staging = paths.staging.join("export-draft-recovery");
        let draft_final = paths.exports.join("draft-recovery");
        let committed_staging = paths.staging.join("export-committed-recovery");
        let committed_final = paths.exports.join("committed-recovery");
        for path in [
            &draft_staging,
            &draft_final,
            &committed_staging,
            &committed_final,
        ] {
            fs::create_dir(path).unwrap();
            fs::write(path.join("marker"), b"durable").unwrap();
        }
        let now = Utc::now().to_rfc3339();
        for (operation_id, batch_id, staging, final_path, operation_exported_at, state) in [
            (
                Uuid::new_v4(),
                draft.id,
                &draft_staging,
                &draft_final,
                Utc.with_ymd_and_hms(2026, 7, 16, 5, 0, 0).unwrap(),
                "generating",
            ),
            (
                Uuid::new_v4(),
                committed.id,
                &committed_staging,
                &committed_final,
                exported_at,
                "published",
            ),
        ] {
            for directory in [staging, final_path] {
                crate::services::export_recovery::write_generation_marker(
                    directory,
                    operation_id,
                    batch_id,
                    staging.file_name().unwrap().to_str().unwrap(),
                    final_path.file_name().unwrap().to_str().unwrap(),
                    operation_exported_at,
                )
                .unwrap();
            }
            sqlx::query(
                "INSERT INTO pending_exports (
                    operation_id, batch_id, staging_component, final_component, exported_at, state,
                    interrupted, last_error, created_at, updated_at
                 ) VALUES (?, ?, ?, ?, ?, ?, 1, 'application_shutdown_interrupted', ?, ?)",
            )
            .bind(operation_id.to_string())
            .bind(batch_id.to_string())
            .bind(staging.file_name().unwrap().to_string_lossy())
            .bind(final_path.file_name().unwrap().to_string_lossy())
            .bind(operation_exported_at.to_rfc3339())
            .bind(state)
            .bind(&now)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();
        }

        let restarted = ExportService::new(pool.clone(), paths, ExportCoordinator::default());
        let report = restarted.reconcile_pending().await.unwrap();

        assert_eq!(report.rolled_back, 1);
        assert_eq!(report.preserved, 1);
        assert!(!draft_staging.exists());
        assert!(!draft_final.exists());
        assert!(!committed_staging.exists());
        assert!(committed_final.join("marker").is_file());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn restart_recovery_retains_staging_when_marker_does_not_own_the_journal_operation() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let batch = BatchRepository::new(pool.clone())
            .create(NewBatch::try_new("marker mismatch", "2026-07-01", "2026-07-31", None).unwrap())
            .await
            .unwrap();
        let journal_operation_id = Uuid::new_v4();
        let marker_operation_id = Uuid::new_v4();
        let staging_component = format!("export-{journal_operation_id}");
        let final_component = "marker-mismatch-final";
        let exported_at = Utc::now();
        let staging = paths.staging.join(&staging_component);
        fs::create_dir(&staging).unwrap();
        crate::services::export_recovery::write_generation_marker(
            &staging,
            marker_operation_id,
            batch.id,
            &staging_component,
            final_component,
            exported_at,
        )
        .unwrap();
        crate::services::export_recovery::ExportJournal::new(pool.clone())
            .create(
                journal_operation_id,
                batch.id,
                &staging_component,
                final_component,
                exported_at,
            )
            .await
            .unwrap();

        let error = ExportService::new(pool.clone(), paths, ExportCoordinator::default())
            .reconcile_pending()
            .await
            .unwrap_err();

        assert!(error.to_string().contains("mismatched recovery marker"));
        assert!(staging.is_dir());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM pending_exports WHERE operation_id = ?",
            )
            .bind(journal_operation_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn restart_recovery_removes_generation_staging_when_database_journal_was_locked() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let operation_id = Uuid::new_v4();
        let batch_id = Uuid::new_v4();
        let staging_component = format!("export-{operation_id}");
        let final_component = "orphan-final";
        let staging = paths.staging.join(&staging_component);
        fs::create_dir(&staging).unwrap();
        crate::services::export_recovery::write_generation_marker(
            &staging,
            operation_id,
            batch_id,
            &staging_component,
            final_component,
            Utc::now(),
        )
        .unwrap();
        fs::write(staging.join("partial-artifact"), b"partial").unwrap();

        let report = ExportService::new(pool, paths, ExportCoordinator::default())
            .reconcile_pending()
            .await
            .unwrap();

        assert_eq!(report.rolled_back, 1);
        assert_eq!(report.preserved, 0);
        assert!(!staging.exists());
    }

    #[tokio::test]
    async fn shutdown_timeout_marks_a_blocked_generation_for_durable_recovery() {
        let fixture = export_fixture("timeout marker test").await;
        let coordinator = ExportCoordinator::default();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let service = ExportService::with_coordinator_and_generation_hook(
            fixture.pool.clone(),
            fixture.paths,
            coordinator.clone(),
            Arc::new(BlockingGenerationHook {
                started: Mutex::new(Some(started_tx)),
                release: Mutex::new(release_rx),
            }),
        );
        let batch_id = fixture.batch_id;
        let export = tokio::spawn(async move { service.export(batch_id).await });
        started_rx.await.unwrap();

        let shutdown = coordinator.begin_shutdown();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), shutdown.wait())
                .await
                .is_err()
        );
        assert_eq!(
            crate::services::export_recovery::mark_pending_exports_interrupted(&fixture.pool)
                .await
                .unwrap(),
            1
        );
        let marker = sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT interrupted, last_error FROM pending_exports WHERE batch_id = ?",
        )
        .bind(batch_id.to_string())
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
        assert_eq!(marker.0, 1);
        assert_eq!(
            marker.1.as_deref(),
            Some("application_shutdown_interrupted")
        );

        release_tx.send(()).unwrap();
        export.await.unwrap().unwrap();
        shutdown.wait().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_cleanup_never_follows_a_journal_symlink_or_discards_recovery_state() {
        let fixture = export_fixture("unsafe cleanup test").await;
        let external = tempfile::tempdir().unwrap();
        fs::write(external.path().join("keep"), b"external").unwrap();
        let service = ExportService::with_publisher(
            fixture.pool.clone(),
            fixture.paths,
            Arc::new(SymlinkReplacingFailingPublisher {
                external: external.path().to_path_buf(),
            }),
        );

        let error = service.export(fixture.batch_id).await.unwrap_err();

        assert!(external.path().join("keep").is_file());
        assert!(
            !error
                .to_string()
                .contains(&external.path().display().to_string())
        );
        let journal = sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT interrupted, last_error FROM pending_exports WHERE batch_id = ?",
        )
        .bind(fixture.batch_id.to_string())
        .fetch_one(&fixture.pool)
        .await
        .expect("unsafe cleanup must retain a durable recovery record");
        assert_eq!(journal.0, 1);
        assert!(journal.1.is_some());
    }

    fn one_page_pdf() -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 100.into(), 150.into()],
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }
}
