use std::path::Path;

use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use crate::db::items::{InvoiceItem, ItemRepository, NewItemRecord};
use crate::domain::error::AppError;
use crate::domain::model::{ConfirmationStatus, DedupeStatus, RecognitionStatus, SourceType};
use crate::infra::files::AppPaths;

pub const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;
const ALLOWED_EXTENSIONS: [&str; 9] = [
    "pdf", "jpg", "jpeg", "png", "doc", "docx", "xls", "xlsx", "zip",
];

#[derive(Clone)]
pub struct ImportService {
    items: ItemRepository,
    paths: AppPaths,
}

impl ImportService {
    pub fn new(items: ItemRepository, paths: AppPaths) -> Self {
        Self { items, paths }
    }

    pub async fn import_manual(&self, source: &Path) -> Result<InvoiceItem, AppError> {
        validate_source(source).await?;
        let original_name = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| AppError::validation("file", "file name is required"))?;
        let extension = supported_extension(source)?;
        let id = Uuid::new_v4();
        let mut source_file = tokio::fs::File::open(source)
            .await
            .map_err(|error| internal_error("failed to open import source", error))?;
        let mut staged = self.paths.stage_original(id)?;
        let mut staged_file = tokio::fs::File::from_std(staged.take_file()?);
        let copy_result = tokio::io::copy(&mut source_file, &mut staged_file).await;
        let copy_result = match copy_result {
            Ok(_) => staged_file
                .sync_all()
                .await
                .map_err(|error| internal_error("failed to sync staged import", error)),
            Err(error) => Err(internal_error(
                "failed to copy import source to staging",
                error,
            )),
        };
        drop(staged_file);
        if let Err(error) = copy_result {
            return Err(staged.cleanup_after(error));
        }

        let sha256 = match sha256_file(staged.path()).await {
            Ok(sha256) => sha256,
            Err(error) => return Err(staged.cleanup_after(error)),
        };
        let mime_type = match detect_mime(staged.path(), &extension).await {
            Ok(mime_type) => mime_type,
            Err(error) => return Err(staged.cleanup_after(error)),
        };
        let duplicate = match self.items.find_by_hash(&sha256).await {
            Ok(duplicate) => duplicate,
            Err(error) => return Err(staged.cleanup_after(error)),
        };
        let now = Utc::now();
        let original_path = staged.promote(now.date_naive(), id, &extension)?;
        let item = NewItemRecord {
            id,
            original_name,
            original_path: original_path.to_string_lossy().into_owned(),
            normalized_pdf_path: None,
            sha256,
            mime_type,
            source_type: SourceType::ManualUpload,
            source_account_id: None,
            source_mailbox: None,
            source_uid: None,
            source_message_id: None,
            source_part_id: None,
            fetched_at: now,
            invoice_date: None,
            suggested_period: None,
            batch_id: None,
            suggested_category: None,
            final_category: None,
            amount_cents: None,
            currency: "CNY".to_owned(),
            city: None,
            company: None,
            recognition_status: RecognitionStatus::Pending,
            confirmation_status: ConfirmationStatus::Pending,
            dedupe_status: if duplicate.is_some() {
                DedupeStatus::SuspectedDuplicate
            } else {
                DedupeStatus::Unique
            },
            duplicate_of_id: duplicate.map(|item| item.id),
            note: None,
            event_tag: None,
            project_tag: None,
            created_at: now,
            updated_at: now,
        };

        match self.items.insert(&item).await {
            Ok(item) => Ok(item),
            Err(database_error) => match self.paths.delete_original(&original_path) {
                Ok(()) => Err(database_error),
                Err(cleanup_error) => Err(AppError::External {
                    service: "filesystem_sync".to_owned(),
                    retryable: false,
                    message: format!(
                        "database insert failed after original promotion and durable cleanup was incomplete; manual recovery is required for {}: database error: {database_error}; cleanup error: {cleanup_error}",
                        original_path.display()
                    ),
                }),
            },
        }
    }
}

async fn validate_source(source: &Path) -> Result<(), AppError> {
    let metadata = match tokio::fs::metadata(source).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AppError::validation("file", "file does not exist"));
        }
        Err(error) => return Err(internal_error("failed to inspect import source", error)),
    };
    if !metadata.is_file() {
        return Err(AppError::validation("file", "path must reference a file"));
    }
    if metadata.len() == 0 {
        return Err(AppError::validation("file", "file must not be empty"));
    }
    if metadata.len() > MAX_FILE_SIZE {
        return Err(AppError::validation(
            "file",
            format!("file must not exceed {MAX_FILE_SIZE} bytes"),
        ));
    }

    Ok(())
}

fn supported_extension(source: &Path) -> Result<String, AppError> {
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|extension| ALLOWED_EXTENSIONS.contains(&extension.as_str()))
        .ok_or_else(|| AppError::validation("file", "file extension is not supported"))?;
    Ok(extension)
}

async fn sha256_file(path: &Path) -> Result<String, AppError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| internal_error("failed to open staged import for hashing", error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let bytes_read = file
            .read(&mut buffer)
            .await
            .map_err(|error| internal_error("failed to hash staged import", error))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

async fn detect_mime(path: &Path, extension: &str) -> Result<String, AppError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| internal_error("failed to inspect staged import", error))?;
    let mut buffer = [0_u8; 8 * 1024];
    let bytes_read = file
        .read(&mut buffer)
        .await
        .map_err(|error| internal_error("failed to inspect staged import", error))?;

    let bytes = &buffer[..bytes_read];
    let mime_type = infer::get(bytes)
        .map(|kind| kind.mime_type())
        .unwrap_or("application/octet-stream")
        .to_owned();
    if !signature_matches_extension(extension, bytes) {
        tracing::debug!(
            extension,
            mime_type,
            "import signature does not match extension"
        );
    }

    Ok(mime_type)
}

fn signature_matches_extension(extension: &str, bytes: &[u8]) -> bool {
    const PDF: &[u8] = b"%PDF-";
    const JPEG: &[u8] = &[0xff, 0xd8, 0xff];
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    const OLE: &[u8] = &[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
    const ZIP_LOCAL: &[u8] = b"PK\x03\x04";
    const ZIP_EMPTY: &[u8] = b"PK\x05\x06";
    const ZIP_SPANNED: &[u8] = b"PK\x07\x08";

    match extension.to_ascii_lowercase().as_str() {
        "pdf" => bytes.starts_with(PDF),
        "jpg" | "jpeg" => bytes.starts_with(JPEG),
        "png" => bytes.starts_with(PNG),
        "doc" | "xls" => bytes.starts_with(OLE),
        "docx" | "xlsx" | "zip" => [ZIP_LOCAL, ZIP_EMPTY, ZIP_SPANNED]
            .iter()
            .any(|signature| bytes.starts_with(signature)),
        _ => false,
    }
}

fn internal_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Internal {
        message: format!("{context}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::signature_matches_extension;

    #[test]
    fn signature_compatibility_uses_exact_families_for_supported_extensions() {
        let pdf = b"%PDF-1.7";
        let jpeg = [0xff, 0xd8, 0xff, 0xe0];
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let ole = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
        let zip = *b"PK\x03\x04";

        for (extension, bytes) in [
            ("pdf", pdf.as_slice()),
            ("jpg", jpeg.as_slice()),
            ("jpeg", jpeg.as_slice()),
            ("png", png.as_slice()),
            ("doc", ole.as_slice()),
            ("xls", ole.as_slice()),
            ("docx", zip.as_slice()),
            ("xlsx", zip.as_slice()),
            ("zip", zip.as_slice()),
        ] {
            assert!(
                signature_matches_extension(extension, bytes),
                "{extension} should accept its signature"
            );
        }

        assert!(!signature_matches_extension("pdf", &jpeg));
        assert!(!signature_matches_extension("doc", &zip));
        assert!(!signature_matches_extension("docx", &ole));
        assert!(!signature_matches_extension("pdf", b"unknown"));
    }
}
