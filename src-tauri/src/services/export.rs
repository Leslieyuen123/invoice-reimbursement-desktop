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
}

impl ExportService {
    pub fn new(pool: SqlitePool, paths: AppPaths) -> Self {
        Self {
            pool,
            paths,
            active_exports: Arc::new(DashMap::new()),
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
        let published = staging.publish(directory)?;
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
