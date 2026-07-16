use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::db::items::ItemRepository;
use crate::domain::error::AppError;
use crate::infra::files::{AppPaths, open_contained_regular_file};

const MAX_PREVIEW_BYTES: u64 = crate::services::import::MAX_FILE_SIZE;
const DEFAULT_ACTIVE_PREVIEW_LIMIT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewVariant {
    Original,
    Normalized,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewPayload {
    pub bytes: Vec<u8>,
    pub mime_type: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRangePayload {
    pub first_byte: u8,
    pub total_length: u64,
    pub mime_type: &'static str,
}

pub trait PreviewFileReader: Send + Sync {
    fn read(
        &self,
        path: PathBuf,
        root: PathBuf,
        mime_type: &'static str,
        max_bytes: u64,
    ) -> Result<PreviewPayload, AppError>;

    fn read_first_byte(
        &self,
        path: PathBuf,
        root: PathBuf,
        mime_type: &'static str,
        max_bytes: u64,
    ) -> Result<PreviewRangePayload, AppError>;
}

#[derive(Clone)]
pub struct PreviewCoordinator {
    slots: Arc<Semaphore>,
}

impl PreviewCoordinator {
    pub fn with_limit(limit: usize) -> Self {
        assert!(limit > 0, "preview concurrency limit must be positive");
        Self {
            slots: Arc::new(Semaphore::new(limit)),
        }
    }

    #[doc(hidden)]
    pub fn available_permits(&self) -> usize {
        self.slots.available_permits()
    }
}

impl Default for PreviewCoordinator {
    fn default() -> Self {
        Self::with_limit(DEFAULT_ACTIVE_PREVIEW_LIMIT)
    }
}

#[derive(Clone)]
pub struct PreviewService {
    pool: SqlitePool,
    paths: AppPaths,
    coordinator: PreviewCoordinator,
    reader: Arc<dyn PreviewFileReader>,
}

impl PreviewService {
    pub fn new(pool: SqlitePool, paths: AppPaths, coordinator: PreviewCoordinator) -> Self {
        Self::with_reader(pool, paths, coordinator, Arc::new(FileSystemPreviewReader))
    }

    pub fn with_reader(
        pool: SqlitePool,
        paths: AppPaths,
        coordinator: PreviewCoordinator,
        reader: Arc<dyn PreviewFileReader>,
    ) -> Self {
        Self {
            pool,
            paths,
            coordinator,
            reader,
        }
    }

    pub async fn open(
        &self,
        id: Uuid,
        variant: PreviewVariant,
    ) -> Result<PreviewPayload, AppError> {
        let permit = self
            .coordinator
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| preview_capacity_exhausted())?;
        let item = ItemRepository::new(self.pool.clone()).get_by_id(id).await?;
        let (path, root, mime_type) = match variant {
            PreviewVariant::Original => (
                PathBuf::from(item.original_path),
                self.paths.originals.clone(),
                preview_mime_type(&item.mime_type)?,
            ),
            PreviewVariant::Normalized => (
                PathBuf::from(item.normalized_pdf_path.ok_or_else(preview_unavailable)?),
                self.paths.normalized.clone(),
                "application/pdf",
            ),
        };
        let reader = self.reader.clone();

        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            reader.read(path, root, mime_type, MAX_PREVIEW_BYTES)
        })
        .await
        .map_err(|_| AppError::Internal {
            message: "preview reader task failed".to_owned(),
        })?
    }

    pub async fn open_first_byte(
        &self,
        id: Uuid,
        variant: PreviewVariant,
    ) -> Result<PreviewRangePayload, AppError> {
        let permit = self
            .coordinator
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| preview_capacity_exhausted())?;
        let item = ItemRepository::new(self.pool.clone()).get_by_id(id).await?;
        let (path, root, mime_type) = match variant {
            PreviewVariant::Original => (
                PathBuf::from(item.original_path),
                self.paths.originals.clone(),
                preview_mime_type(&item.mime_type)?,
            ),
            PreviewVariant::Normalized => (
                PathBuf::from(item.normalized_pdf_path.ok_or_else(preview_unavailable)?),
                self.paths.normalized.clone(),
                "application/pdf",
            ),
        };
        let reader = self.reader.clone();

        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            reader.read_first_byte(path, root, mime_type, MAX_PREVIEW_BYTES)
        })
        .await
        .map_err(|_| AppError::Internal {
            message: "preview reader task failed".to_owned(),
        })?
    }
}

struct FileSystemPreviewReader;

impl PreviewFileReader for FileSystemPreviewReader {
    fn read(
        &self,
        path: PathBuf,
        root: PathBuf,
        mime_type: &'static str,
        max_bytes: u64,
    ) -> Result<PreviewPayload, AppError> {
        let mut opened = open_contained_regular_file(&path, &root, "preview")
            .map_err(|_| preview_unavailable())?;
        if opened.length > max_bytes {
            return Err(preview_unavailable());
        }
        let capacity = usize::try_from(opened.length).map_err(|_| preview_unavailable())?;
        let mut bytes = Vec::with_capacity(capacity);
        opened
            .file
            .by_ref()
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| preview_unavailable())?;
        if bytes.len() as u64 > max_bytes {
            return Err(preview_unavailable());
        }
        Ok(PreviewPayload { bytes, mime_type })
    }

    fn read_first_byte(
        &self,
        path: PathBuf,
        root: PathBuf,
        mime_type: &'static str,
        max_bytes: u64,
    ) -> Result<PreviewRangePayload, AppError> {
        let mut opened = open_contained_regular_file(&path, &root, "preview")
            .map_err(|_| preview_unavailable())?;
        if opened.length == 0 || opened.length > max_bytes {
            return Err(preview_unavailable());
        }
        let mut first_byte = [0_u8; 1];
        opened
            .file
            .read_exact(&mut first_byte)
            .map_err(|_| preview_unavailable())?;
        Ok(PreviewRangePayload {
            first_byte: first_byte[0],
            total_length: opened.length,
            mime_type,
        })
    }
}

fn preview_mime_type(mime_type: &str) -> Result<&'static str, AppError> {
    match mime_type {
        "application/pdf" => Ok("application/pdf"),
        "image/png" => Ok("image/png"),
        "image/jpeg" => Ok("image/jpeg"),
        _ => Err(AppError::validation(
            "preview",
            "item type cannot be previewed",
        )),
    }
}

fn preview_capacity_exhausted() -> AppError {
    AppError::External {
        service: "preview_capacity".to_owned(),
        retryable: true,
        message: "preview capacity is temporarily exhausted".to_owned(),
    }
}

fn preview_unavailable() -> AppError {
    AppError::NotFound {
        entity: "item_preview".to_owned(),
        message: "item preview is unavailable".to_owned(),
    }
}
