use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
#[cfg(test)]
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::sync::Semaphore;
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
}

impl Default for ExportCoordinator {
    fn default() -> Self {
        Self {
            active_exports: Arc::new(DashMap::new()),
            generation_slots: Arc::new(Semaphore::new(2)),
        }
    }
}

#[derive(Clone)]
pub struct ExportService {
    pool: SqlitePool,
    paths: AppPaths,
    coordinator: ExportCoordinator,
    publisher: Arc<dyn DirectoryPublisher>,
    committer: Arc<dyn ExportCommitter>,
    generation_hook: Arc<dyn GenerationHook>,
}

impl ExportService {
    pub fn new(pool: SqlitePool, paths: AppPaths, coordinator: ExportCoordinator) -> Self {
        Self {
            pool,
            paths,
            coordinator,
            publisher: Arc::new(AtomicDirectoryPublisher),
            committer: Arc::new(SqliteExportCommitter),
            generation_hook: Arc::new(NoopGenerationHook),
        }
    }

    #[cfg(test)]
    fn with_publisher(
        pool: SqlitePool,
        paths: AppPaths,
        publisher: Arc<dyn DirectoryPublisher>,
    ) -> Self {
        Self {
            pool,
            paths,
            coordinator: ExportCoordinator::default(),
            publisher,
            committer: Arc::new(SqliteExportCommitter),
            generation_hook: Arc::new(NoopGenerationHook),
        }
    }

    #[cfg(test)]
    fn with_committer(
        pool: SqlitePool,
        paths: AppPaths,
        committer: Arc<dyn ExportCommitter>,
    ) -> Self {
        Self {
            pool,
            paths,
            coordinator: ExportCoordinator::default(),
            publisher: Arc::new(AtomicDirectoryPublisher),
            committer,
            generation_hook: Arc::new(NoopGenerationHook),
        }
    }

    #[cfg(test)]
    fn with_generation_hook(
        pool: SqlitePool,
        paths: AppPaths,
        generation_hook: Arc<dyn GenerationHook>,
    ) -> Self {
        Self {
            pool,
            paths,
            coordinator: ExportCoordinator::default(),
            publisher: Arc::new(AtomicDirectoryPublisher),
            committer: Arc::new(SqliteExportCommitter),
            generation_hook,
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
            pool,
            paths,
            coordinator,
            publisher: Arc::new(AtomicDirectoryPublisher),
            committer: Arc::new(SqliteExportCommitter),
            generation_hook,
        }
    }

    pub async fn export(&self, batch_id: Uuid) -> Result<ExportResult, AppError> {
        let active_export =
            ActiveExport::acquire(self.coordinator.active_exports.clone(), batch_id)?;
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
        let staging_path = self
            .paths
            .staging
            .join(format!("export-{}", Uuid::new_v4()));
        let generation_slot = self
            .coordinator
            .generation_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AppError::Internal {
                message: "export generation coordinator closed".to_owned(),
            })?;
        let generation_paths = self.paths.clone();
        let generation_batch = batch.clone();
        let generation_items = items.clone();
        let generation_active_export = active_export.clone();
        let generation_hook = self.generation_hook.clone();
        let staging = tokio::task::spawn_blocking(move || {
            let _generation_slot = generation_slot;
            let _generation_active_export = generation_active_export;
            generation_hook.before_generation()?;
            let mut export_items =
                prepare_export_items(&generation_paths, &generation_items, DEFAULT_EXPORT_LIMITS)?;
            let merged_pdf = prepare_merged_pdf(&mut export_items, DEFAULT_EXPORT_LIMITS)?;
            let staging = OwnedExportDirectory::create(staging_path)?;
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
        })??;

        let directory = self
            .paths
            .exports
            .join(export_directory_name(&batch.name, &exported_at));
        let claim = batches.claim_export(&snapshot).await?;
        let published = self.publisher.publish(staging, directory).await?;
        let committer = self.committer.clone();
        let directory = tokio::spawn(async move {
            let _active_export = active_export;
            if let Err(error) = committer.commit(claim, exported_at).await {
                let finalization_error = published.cleanup_after(error);
                tracing::error!(
                    %batch_id,
                    error = %finalization_error,
                    "export finalization failed"
                );
                return Err(finalization_error);
            }
            Ok(published.commit())
        })
        .await
        .map_err(|_| AppError::Internal {
            message: "export finalizer task failed".to_owned(),
        })??;

        Ok(ExportResult {
            directory,
            item_count: items.len(),
            total_amount_cents,
        })
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
        exported_at: DateTime<Utc>,
    ) -> Result<(), AppError>;
}

struct SqliteExportCommitter;

#[async_trait::async_trait]
impl ExportCommitter for SqliteExportCommitter {
    async fn commit(
        &self,
        claim: BatchExportClaim,
        exported_at: DateTime<Utc>,
    ) -> Result<(), AppError> {
        claim.commit(exported_at).await.map(|_| ())
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
    batch_id: Uuid,
}

impl ActiveExport {
    fn acquire(active_exports: Arc<DashMap<Uuid, ()>>, batch_id: Uuid) -> Result<Self, AppError> {
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
                batch_id,
            }),
        })
    }
}

impl Drop for ActiveExportLease {
    fn drop(&mut self) {
        self.active_exports.remove(&self.batch_id);
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

    fn cleanup_after(mut self, error: AppError) -> AppError {
        self.owned = false;
        match remove_export_directory(&self.path) {
            Ok(()) => error,
            Err(cleanup_error) => AppError::External {
                service: "filesystem_sync".to_owned(),
                retryable: false,
                message: format!(
                    "export failed and package cleanup was incomplete; manual recovery is required: original error: {error}; cleanup error: {cleanup_error}"
                ),
            },
        }
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
        DirectoryPublisher, ExportCommitter, ExportCoordinator, ExportService, GenerationHook,
        OwnedExportDirectory, sha256_hex,
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

    struct GatedFailingCommitter {
        started: Mutex<Option<oneshot::Sender<()>>>,
        release: Arc<Notify>,
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
            exported_at: DateTime<Utc>,
        ) -> Result<(), AppError> {
            let result = claim.commit(exported_at).await.map(|_| ());
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
    impl ExportCommitter for GatedFailingCommitter {
        async fn commit(
            &self,
            _claim: BatchExportClaim,
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
        let task = tokio::spawn(async move { service.export(batch_id).await });

        let final_directory = tokio::time::timeout(std::time::Duration::from_secs(5), published_rx)
            .await
            .expect("publisher should report final rename")
            .expect("publisher signal should remain open");
        assert!(final_directory.is_dir());
        assert_eq!(fs::read_dir(&fixture.paths.staging).unwrap().count(), 0);

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        release.notify_waiters();

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
