use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::db::batches::BatchRepository;
use crate::domain::error::AppError;
use crate::domain::model::{ItemStatus, RecognitionStatus};
use crate::infra::exporters::{ExportItem, generate_artifacts, publish_directory};
use crate::infra::files::{AppPaths, sync_directory};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportResult {
    pub directory: PathBuf,
    pub item_count: usize,
    pub total_amount_cents: i64,
}

#[derive(Clone)]
pub struct ExportService {
    pool: SqlitePool,
    paths: AppPaths,
    active_exports: Arc<DashMap<Uuid, ()>>,
    publisher: Arc<dyn DirectoryPublisher>,
}

impl ExportService {
    pub fn new(pool: SqlitePool, paths: AppPaths) -> Self {
        Self {
            pool,
            paths,
            active_exports: Arc::new(DashMap::new()),
            publisher: Arc::new(AtomicDirectoryPublisher),
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
            active_exports: Arc::new(DashMap::new()),
            publisher,
        }
    }

    pub async fn export(&self, batch_id: Uuid) -> Result<ExportResult, AppError> {
        let _active_export = ActiveExport::acquire(self.active_exports.clone(), batch_id)?;
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
        let export_items = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                total_amount_cents =
                    total_amount_cents
                        .checked_add(item.amount_cents.ok_or_else(|| {
                            AppError::validation("amountCents", "票据金额不能为空")
                        })?)
                        .ok_or_else(|| AppError::Internal {
                            message: "batch export amount overflow".to_owned(),
                        })?;
                prepare_export_item(&self.paths, index + 1, item)
            })
            .collect::<Result<Vec<_>, AppError>>()?;

        let exported_at = Utc::now();
        let staging = self
            .paths
            .staging
            .join(format!("export-{}", Uuid::new_v4()));
        let staging = OwnedExportDirectory::create(staging)?;
        generate_artifacts(staging.path(), &batch, &export_items, exported_at)?;

        let sanitized_name = sanitize_filename::sanitize(&batch.name);
        let batch_name = if sanitized_name.trim().is_empty() {
            "batch"
        } else {
            sanitized_name.as_str()
        };
        let directory = self.paths.exports.join(format!(
            "{batch_name}-{}",
            exported_at.format("%Y%m%d-%H%M%S")
        ));
        let claim = batches.claim_export(&snapshot).await?;
        let published = self.publisher.publish(staging, directory).await?;
        if let Err(error) = claim.commit(exported_at).await {
            return Err(published.cleanup_after(error));
        }
        let directory = published.commit();

        Ok(ExportResult {
            directory,
            item_count: items.len(),
            total_amount_cents,
        })
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

struct ActiveExport {
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
            active_exports,
            batch_id,
        })
    }
}

impl Drop for ActiveExport {
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
            Err(()) => AppError::External {
                service: "filesystem_sync".to_owned(),
                retryable: false,
                message: format!(
                    "export failed and package cleanup was incomplete; manual recovery is required: {error}"
                ),
            },
        }
    }
}

impl Drop for OwnedExportDirectory {
    fn drop(&mut self) {
        if self.owned {
            let _ = remove_export_directory(&self.path);
        }
    }
}

fn remove_export_directory(path: &Path) -> Result<(), ()> {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(()),
    }
    path.parent()
        .ok_or(())
        .and_then(|parent| sync_directory(parent).map_err(|_| ()))
}

fn prepare_export_item(
    paths: &AppPaths,
    sequence: usize,
    item: &crate::db::items::InvoiceItem,
) -> Result<ExportItem, AppError> {
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

    let original_bytes =
        read_contained_regular_file(Path::new(&item.original_path), &paths.originals, "original")?;
    let normalized_path = item
        .normalized_pdf_path
        .as_deref()
        .ok_or_else(|| AppError::validation("normalizedPdf", "票据缺少归一化 PDF"))?;
    let normalized_pdf_bytes = read_contained_regular_file(
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

    Ok(ExportItem {
        item: item.clone(),
        original_bytes,
        normalized_pdf_bytes,
        archive_name: format!("{sequence}-{id_prefix}-{sanitized_name}"),
    })
}

fn read_contained_regular_file(path: &Path, root: &Path, field: &str) -> Result<Vec<u8>, AppError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| AppError::validation(field, "票据文件不存在"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::validation(field, "票据路径必须是普通文件"));
    }
    let canonical_root = fs::canonicalize(root).map_err(|_| AppError::Internal {
        message: "failed to resolve application file storage".to_owned(),
    })?;
    let canonical_path =
        fs::canonicalize(path).map_err(|_| AppError::validation(field, "票据文件不存在"))?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(AppError::validation(field, "票据文件不在应用存储目录内"));
    }
    fs::read(canonical_path).map_err(|_| AppError::validation(field, "票据文件无法读取"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lopdf::{Document, Object, dictionary};
    use tokio::sync::{Notify, oneshot};
    use uuid::Uuid;

    use super::{DirectoryPublisher, ExportService, OwnedExportDirectory};
    use crate::db;
    use crate::db::batches::BatchRepository;
    use crate::db::items::{ItemRepository, NewItemRecord};
    use crate::domain::error::AppError;
    use crate::domain::model::{
        BatchStatus, Category, ConfirmationStatus, DedupeStatus, NewBatch, RecognitionStatus,
        SourceType,
    };
    use crate::infra::files::AppPaths;

    struct GatedRealPublisher {
        published: Mutex<Option<oneshot::Sender<PathBuf>>>,
        release: Arc<Notify>,
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

    #[tokio::test]
    async fn cancellation_after_real_publication_removes_final_and_keeps_batch_draft() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let batch = BatchRepository::new(pool.clone())
            .create(NewBatch::try_new("取消测试", "2026-07-01", "2026-07-31", None).unwrap())
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
                sha256: "fixture-sha256".to_owned(),
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
        let (published_tx, published_rx) = oneshot::channel();
        let release = Arc::new(Notify::new());
        let service = ExportService::with_publisher(
            pool.clone(),
            paths.clone(),
            Arc::new(GatedRealPublisher {
                published: Mutex::new(Some(published_tx)),
                release: release.clone(),
            }),
        );
        let task = tokio::spawn(async move { service.export(batch.id).await });

        let final_directory = tokio::time::timeout(std::time::Duration::from_secs(5), published_rx)
            .await
            .expect("publisher should report final rename")
            .expect("publisher signal should remain open");
        assert!(final_directory.is_dir());
        assert_eq!(fs::read_dir(&paths.staging).unwrap().count(), 0);

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        release.notify_waiters();

        assert!(!final_directory.exists());
        assert_eq!(fs::read_dir(&paths.staging).unwrap().count(), 0);
        let batch = BatchRepository::new(pool).get(batch.id).await.unwrap();
        assert_eq!(batch.status, BatchStatus::Draft);
        assert_eq!(batch.last_exported_at, None);
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
