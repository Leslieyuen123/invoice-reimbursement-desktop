use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};
use uuid::Uuid;

use crate::db::items::{EmailFileReplacement, InvoiceItem, ItemRepository, NewItemRecord};
use crate::domain::error::AppError;
use crate::domain::model::{ConfirmationStatus, DedupeStatus, RecognitionStatus, SourceType};
use crate::infra::files::AppPaths;
use crate::infra::imap::RejectedMessage;

pub const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;
const ALLOWED_EXTENSIONS: [&str; 9] = [
    "pdf", "jpg", "jpeg", "png", "doc", "docx", "xls", "xlsx", "zip",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailImportSource {
    pub account_id: Uuid,
    pub mailbox: String,
    pub uid_validity: u32,
    pub uid: u32,
    pub message_id: Option<String>,
    pub part_id: String,
    pub received_at: DateTime<Utc>,
    pub source_received_date: NaiveDate,
    pub rescan: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    New(InvoiceItem),
    Existing(InvoiceItem),
}

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
        self.import_manual_after_open(source, || Ok(())).await
    }

    pub async fn import_email_bytes(
        &self,
        file_name: &str,
        bytes: &[u8],
        source: EmailImportSource,
    ) -> Result<ImportOutcome, AppError> {
        let original_name = sanitize_mail_filename(file_name)?;
        let extension = supported_extension(Path::new(&original_name))?;
        let is_empty = bytes.is_empty();
        self.import_email_payload(EmailPayload {
            original_name,
            extension,
            bytes,
            mime_type: None,
            recognition_status: if is_empty {
                RecognitionStatus::Failed
            } else {
                RecognitionStatus::Pending
            },
            note: is_empty.then(|| "email attachment was empty".to_owned()),
            source,
        })
        .await
    }

    pub(crate) async fn find_email_part(
        &self,
        source: &EmailImportSource,
    ) -> Result<Option<InvoiceItem>, AppError> {
        validate_email_source(source)?;
        self.items
            .find_email_part(
                source.account_id,
                &source.mailbox,
                source.uid_validity,
                source.uid,
                &source.part_id,
            )
            .await
    }

    #[cfg(test)]
    pub(crate) async fn import_email_link(
        &self,
        url: &str,
        source: EmailImportSource,
    ) -> Result<ImportOutcome, AppError> {
        self.import_email_link_with_status(
            url,
            source,
            RecognitionStatus::Succeeded,
            Some(url.to_owned()),
        )
        .await
    }

    pub(crate) async fn import_failed_email_link(
        &self,
        url: &str,
        source: EmailImportSource,
    ) -> Result<ImportOutcome, AppError> {
        let outcome = self
            .import_email_link_with_status(
                url,
                source,
                RecognitionStatus::Failed,
                Some("invoice link download failed".to_owned()),
            )
            .await?;
        let ImportOutcome::Existing(existing) = outcome else {
            return Ok(outcome);
        };
        let updated = self
            .items
            .mark_email_link_download_failed(existing.id)
            .await?;
        Ok(ImportOutcome::Existing(updated))
    }

    async fn import_email_link_with_status(
        &self,
        url: &str,
        source: EmailImportSource,
        recognition_status: RecognitionStatus,
        note: Option<String>,
    ) -> Result<ImportOutcome, AppError> {
        let original_name = format!(
            "download-{}-{}.url",
            source.uid,
            source.part_id.replace('.', "-")
        );
        let payload = format!("{url}\r\n");
        self.import_email_payload(EmailPayload {
            original_name,
            extension: "url".to_owned(),
            bytes: payload.as_bytes(),
            mime_type: Some("text/uri-list".to_owned()),
            recognition_status,
            note,
            source,
        })
        .await
    }

    pub(crate) async fn import_downloaded_email_link(
        &self,
        file_name: &str,
        bytes: &[u8],
        source: EmailImportSource,
    ) -> Result<ImportOutcome, AppError> {
        validate_email_source(&source)?;
        let Some(existing) = self
            .items
            .find_email_part(
                source.account_id,
                &source.mailbox,
                source.uid_validity,
                source.uid,
                &source.part_id,
            )
            .await?
        else {
            return self.import_email_bytes(file_name, bytes, source).await;
        };
        if existing.mime_type != "text/uri-list"
            || !existing
                .original_name
                .rsplit_once('.')
                .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("url"))
            || existing.confirmation_status != ConfirmationStatus::Pending
            || existing.batch_id.is_some()
        {
            return Ok(ImportOutcome::Existing(existing));
        }

        let original_name = sanitize_mail_filename(file_name)?;
        let extension = supported_extension(Path::new(&original_name))?;
        let candidate_id = Uuid::new_v4();
        let (staged, writer) = self.paths.begin_staged_original(candidate_id)?;
        let mut reader = bytes;
        let staged = copy_source_to_staging(&mut reader, staged, writer, false).await?;
        let sha256 = match sha256_file(staged.path()).await {
            Ok(value) => value,
            Err(error) => return Err(staged.cleanup_after(error)),
        };
        let mime_type = match detect_mime(staged.path(), &extension).await {
            Ok(value) => value,
            Err(error) => return Err(staged.cleanup_after(error)),
        };
        let promoted = staged.promote(source.source_received_date, candidate_id, &extension)?;
        let replacement = EmailFileReplacement {
            original_name,
            original_path: promoted.path().to_string_lossy().into_owned(),
            sha256,
            mime_type,
        };
        match self
            .items
            .replace_email_link_placeholder(existing.id, replacement)
            .await
        {
            Ok(Some(replaced)) => {
                promoted.commit();
                if self.paths.delete_original(&existing.original_path).is_err() {
                    tracing::warn!(
                        item_id = %existing.id,
                        "replaced email link left an unreferenced placeholder for startup recovery"
                    );
                }
                Ok(ImportOutcome::Existing(replaced))
            }
            Ok(None) => {
                promoted.rollback()?;
                let current = self
                    .items
                    .find_email_part(
                        source.account_id,
                        &source.mailbox,
                        source.uid_validity,
                        source.uid,
                        &source.part_id,
                    )
                    .await?
                    .ok_or_else(|| AppError::Conflict {
                        message: "email link changed while its download was being imported"
                            .to_owned(),
                    })?;
                Ok(ImportOutcome::Existing(current))
            }
            Err(database_error) => match promoted.rollback() {
                Ok(()) => Err(database_error),
                Err(cleanup_error) => Err(AppError::External {
                    service: "filesystem_sync".to_owned(),
                    retryable: false,
                    message: format!(
                        "email link replacement failed and downloaded file cleanup was incomplete; \
                         manual recovery is required: database error: {database_error}; cleanup \
                         error: {cleanup_error}"
                    ),
                }),
            },
        }
    }

    pub(crate) async fn discard_email_link_placeholder(
        &self,
        source: &EmailImportSource,
    ) -> Result<bool, AppError> {
        validate_email_source(source)?;
        let Some(discarded) = self
            .items
            .delete_email_link_placeholder(
                source.account_id,
                &source.mailbox,
                source.uid_validity,
                source.uid,
                &source.part_id,
            )
            .await?
        else {
            return Ok(false);
        };
        if self
            .paths
            .delete_original(&discarded.original_path)
            .is_err()
        {
            tracing::warn!(
                item_id = %discarded.id,
                "discarded email link left an unreferenced placeholder for startup recovery"
            );
        }
        Ok(true)
    }

    pub(crate) async fn import_email_rejection(
        &self,
        account_id: Uuid,
        uid_validity: u32,
        rejected: &RejectedMessage,
        rescan: bool,
    ) -> Result<ImportOutcome, AppError> {
        let reason = rejected.reason.code();
        let original_name = format!("message-{}-rejected.txt", rejected.uid);
        let placeholder = format!(
            "Mailbox message UID {} was rejected.\nReason: {reason}\n",
            rejected.uid
        );
        self.import_email_payload(EmailPayload {
            original_name,
            extension: "txt".to_owned(),
            bytes: placeholder.as_bytes(),
            mime_type: Some("text/plain".to_owned()),
            recognition_status: RecognitionStatus::Failed,
            note: Some(format!("mailbox message rejected: {reason}")),
            source: EmailImportSource {
                account_id,
                mailbox: rejected.mailbox.clone(),
                uid_validity,
                uid: rejected.uid,
                message_id: None,
                part_id: "message.rejected".to_owned(),
                received_at: rejected.received_at,
                source_received_date: rejected.source_received_date,
                rescan,
            },
        })
        .await
    }

    async fn import_email_payload(
        &self,
        payload: EmailPayload<'_>,
    ) -> Result<ImportOutcome, AppError> {
        validate_email_source(&payload.source)?;
        if let Some(existing) = self
            .items
            .find_email_part(
                payload.source.account_id,
                &payload.source.mailbox,
                payload.source.uid_validity,
                payload.source.uid,
                &payload.source.part_id,
            )
            .await?
        {
            if payload.source.rescan
                && existing.recognition_status == RecognitionStatus::Failed
                && existing.confirmation_status == ConfirmationStatus::Pending
                && existing.batch_id.is_none()
                && existing.mime_type != "text/uri-list"
                && !payload.bytes.is_empty()
            {
                return self
                    .replace_failed_email_attachment(payload, existing)
                    .await;
            }
            return Ok(ImportOutcome::Existing(existing));
        }
        let id = Uuid::new_v4();
        let (staged, writer) = self.paths.begin_staged_original(id)?;
        let mut reader = payload.bytes;
        let staged = copy_source_to_staging(&mut reader, staged, writer, true).await?;
        let sha256 = match sha256_file(staged.path()).await {
            Ok(value) => value,
            Err(error) => return Err(staged.cleanup_after(error)),
        };
        let legacy = match self
            .items
            .find_legacy_email_part(
                payload.source.account_id,
                &payload.source.mailbox,
                payload.source.message_id.as_deref(),
                &payload.source.part_id,
                &sha256,
            )
            .await
        {
            Ok(legacy) => legacy,
            Err(error) => return Err(staged.cleanup_after(error)),
        };
        if let Some(legacy) = legacy {
            staged.discard()?;
            return Ok(ImportOutcome::Existing(legacy));
        }
        if payload.source.rescan {
            let existing = match self
                .items
                .find_rescanned_email_part(
                    payload.source.account_id,
                    &payload.source.mailbox,
                    payload.source.uid_validity,
                    payload.source.message_id.as_deref(),
                    &payload.source.part_id,
                    &sha256,
                )
                .await
            {
                Ok(existing) => existing,
                Err(error) => return Err(staged.cleanup_after(error)),
            };
            if let Some(existing) = existing {
                staged.discard()?;
                return Ok(ImportOutcome::Existing(existing));
            }
        }
        let mime_type = match payload.mime_type {
            Some(mime_type) => mime_type,
            None => match detect_mime(staged.path(), &payload.extension).await {
                Ok(mime_type) => mime_type,
                Err(error) => return Err(staged.cleanup_after(error)),
            },
        };
        let promoted =
            staged.promote(payload.source.source_received_date, id, &payload.extension)?;
        let original_path = promoted.path().to_path_buf();
        let now = Utc::now();
        let item = NewItemRecord {
            id,
            original_name: payload.original_name,
            original_path: original_path.to_string_lossy().into_owned(),
            normalized_pdf_path: None,
            sha256,
            mime_type,
            source_type: SourceType::Email,
            source_account_id: Some(payload.source.account_id),
            source_mailbox: Some(payload.source.mailbox.clone()),
            source_uid_validity: Some(i64::from(payload.source.uid_validity)),
            source_uid: Some(i64::from(payload.source.uid)),
            source_message_id: payload.source.message_id.clone(),
            source_part_id: Some(payload.source.part_id.clone()),
            fetched_at: payload.source.received_at,
            source_received_date: Some(payload.source.source_received_date),
            invoice_date: None,
            suggested_period: None,
            batch_id: None,
            suggested_category: None,
            final_category: None,
            amount_cents: None,
            currency: "CNY".to_owned(),
            city: None,
            company: None,
            recognition_status: payload.recognition_status,
            confirmation_status: ConfirmationStatus::Pending,
            dedupe_status: DedupeStatus::Unique,
            duplicate_of_id: None,
            note: payload.note,
            event_tag: None,
            project_tag: None,
            created_at: now,
            updated_at: now,
        };
        match self.items.insert_deduplicated(item).await {
            Ok(item) => {
                promoted.commit();
                Ok(ImportOutcome::New(item))
            }
            Err(database_error) => {
                promoted.rollback()?;
                if matches!(database_error, AppError::Conflict { .. })
                    && let Some(existing) = self
                        .items
                        .find_email_part(
                            payload.source.account_id,
                            &payload.source.mailbox,
                            payload.source.uid_validity,
                            payload.source.uid,
                            &payload.source.part_id,
                        )
                        .await?
                {
                    return Ok(ImportOutcome::Existing(existing));
                }
                Err(database_error)
            }
        }
    }

    async fn replace_failed_email_attachment(
        &self,
        payload: EmailPayload<'_>,
        existing: InvoiceItem,
    ) -> Result<ImportOutcome, AppError> {
        let candidate_id = Uuid::new_v4();
        let (staged, writer) = self.paths.begin_staged_original(candidate_id)?;
        let mut reader = payload.bytes;
        let staged = copy_source_to_staging(&mut reader, staged, writer, false).await?;
        let sha256 = match sha256_file(staged.path()).await {
            Ok(value) => value,
            Err(error) => return Err(staged.cleanup_after(error)),
        };
        if sha256 == existing.sha256 {
            staged.discard()?;
            return Ok(ImportOutcome::Existing(existing));
        }
        let mime_type = match payload.mime_type {
            Some(mime_type) => mime_type,
            None => match detect_mime(staged.path(), &payload.extension).await {
                Ok(mime_type) => mime_type,
                Err(error) => return Err(staged.cleanup_after(error)),
            },
        };
        let promoted = staged.promote(
            payload.source.source_received_date,
            candidate_id,
            &payload.extension,
        )?;
        let replacement = EmailFileReplacement {
            original_name: payload.original_name,
            original_path: promoted.path().to_string_lossy().into_owned(),
            sha256,
            mime_type,
        };
        match self
            .items
            .replace_failed_email_attachment(existing.id, replacement)
            .await
        {
            Ok(Some(replaced)) => {
                promoted.commit();
                if let Some(normalized) = existing.normalized_pdf_path.as_deref()
                    && self
                        .paths
                        .delete_normalized_pdf(Path::new(normalized))
                        .is_err()
                {
                    tracing::warn!(
                        item_id = %existing.id,
                        "repaired email attachment left an unreferenced normalized PDF"
                    );
                }
                if self.paths.delete_original(&existing.original_path).is_err() {
                    tracing::warn!(
                        item_id = %existing.id,
                        "repaired email attachment left an unreferenced original"
                    );
                }
                Ok(ImportOutcome::Existing(replaced))
            }
            Ok(None) => {
                promoted.rollback()?;
                Ok(ImportOutcome::Existing(
                    self.items.get_by_id(existing.id).await?,
                ))
            }
            Err(database_error) => match promoted.rollback() {
                Ok(()) => Err(database_error),
                Err(cleanup_error) => Err(AppError::External {
                    service: "filesystem_sync".to_owned(),
                    retryable: false,
                    message: format!(
                        "email attachment repair failed and corrected file cleanup was incomplete; \
                         manual recovery is required: database error: {database_error}; cleanup \
                         error: {cleanup_error}"
                    ),
                }),
            },
        }
    }

    async fn import_manual_after_open<F>(
        &self,
        source: &Path,
        after_open: F,
    ) -> Result<InvoiceItem, AppError>
    where
        F: FnOnce() -> Result<(), AppError>,
    {
        let mut source_file = match tokio::fs::File::open(source).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(AppError::validation("file", "file does not exist"));
            }
            Err(error) => return Err(internal_error("failed to open import source", error)),
        };
        after_open()?;
        let metadata = source_file
            .metadata()
            .await
            .map_err(|error| internal_error("failed to inspect import source", error))?;
        validate_source_metadata(&metadata)?;
        let original_name = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| AppError::validation("file", "file name is required"))?;
        let extension = supported_extension(source)?;
        let id = Uuid::new_v4();
        let (staged, writer) = self.paths.begin_staged_original(id)?;
        let staged = copy_source_to_staging(&mut source_file, staged, writer, false).await?;

        let sha256 = match sha256_file(staged.path()).await {
            Ok(sha256) => sha256,
            Err(error) => return Err(staged.cleanup_after(error)),
        };
        let mime_type = match detect_mime(staged.path(), &extension).await {
            Ok(mime_type) => mime_type,
            Err(error) => return Err(staged.cleanup_after(error)),
        };
        let now = Utc::now();
        let promoted = staged.promote(now.date_naive(), id, &extension)?;
        let original_path = promoted.path().to_path_buf();
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
            source_uid_validity: None,
            source_uid: None,
            source_message_id: None,
            source_part_id: None,
            fetched_at: now,
            source_received_date: None,
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
            dedupe_status: DedupeStatus::Unique,
            duplicate_of_id: None,
            note: None,
            event_tag: None,
            project_tag: None,
            created_at: now,
            updated_at: now,
        };

        match self.items.insert_deduplicated(item).await {
            Ok(item) => {
                promoted.commit();
                Ok(item)
            }
            Err(database_error) => match promoted.rollback() {
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

struct EmailPayload<'a> {
    original_name: String,
    extension: String,
    bytes: &'a [u8],
    mime_type: Option<String>,
    recognition_status: RecognitionStatus,
    note: Option<String>,
    source: EmailImportSource,
}

fn validate_email_source(source: &EmailImportSource) -> Result<(), AppError> {
    if source.account_id.is_nil()
        || source.mailbox.trim().is_empty()
        || source.uid_validity == 0
        || source.uid == 0
        || source.part_id.trim().is_empty()
    {
        return Err(AppError::validation(
            "source",
            "email source requires an account, mailbox, positive UIDVALIDITY and UID, and part ID",
        ));
    }
    Ok(())
}

fn sanitize_mail_filename(file_name: &str) -> Result<String, AppError> {
    let basename = file_name
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::validation("file", "email file name is required"))?;
    let sanitized = sanitize_filename::sanitize(basename);
    if sanitized.trim().is_empty() || sanitized == "." || sanitized == ".." {
        return Err(AppError::validation(
            "file",
            "email file name is not usable",
        ));
    }
    Ok(sanitized)
}

fn validate_source_metadata(metadata: &std::fs::Metadata) -> Result<(), AppError> {
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

async fn copy_source_to_staging<R>(
    source: &mut R,
    staged: crate::infra::files::StagedOriginal,
    writer: std::fs::File,
    allow_empty: bool,
) -> Result<crate::infra::files::StagedOriginal, AppError>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut writer = tokio::fs::File::from_std(writer);
    let mut capped_source = source.take(MAX_FILE_SIZE + 1);
    let copy_result = tokio::io::copy(&mut capped_source, &mut writer).await;
    let result = match copy_result {
        Ok(0) if !allow_empty => Err(AppError::validation("file", "file must not be empty")),
        Ok(bytes_copied) if bytes_copied > MAX_FILE_SIZE => Err(AppError::validation(
            "file",
            format!("file must not exceed {MAX_FILE_SIZE} bytes"),
        )),
        Ok(_) => writer
            .sync_all()
            .await
            .map_err(|error| internal_error("failed to sync staged import", error)),
        Err(error) => Err(internal_error(
            "failed to copy import source to staging",
            error,
        )),
    };
    drop(writer);

    match result {
        Ok(()) => Ok(staged),
        Err(error) => Err(staged.cleanup_after(error)),
    }
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
    use std::fs;
    use std::path::Path;

    use chrono::{TimeZone, Utc};
    use tokio::io::{AsyncRead, AsyncReadExt};
    use uuid::Uuid;

    use super::{
        EmailImportSource, ImportOutcome, ImportService, copy_source_to_staging,
        signature_matches_extension,
    };
    use crate::db;
    use crate::db::items::{ItemPatch, ItemRepository};
    use crate::domain::error::AppError;
    use crate::domain::model::{ConfirmationStatus, DedupeStatus, RecognitionStatus};
    use crate::infra::files::AppPaths;

    fn email_source(uid: u32, part_id: &str) -> EmailImportSource {
        EmailImportSource {
            account_id: Uuid::new_v4(),
            mailbox: "INBOX".to_owned(),
            uid_validity: 71,
            uid,
            message_id: Some(format!("invoice-{uid}@example.com")),
            part_id: part_id.to_owned(),
            received_at: Utc.with_ymd_and_hms(2026, 7, 23, 10, 0, 0).unwrap(),
            source_received_date: chrono::NaiveDate::from_ymd_opt(2026, 7, 23).unwrap(),
            rescan: false,
        }
    }

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

    #[tokio::test]
    async fn email_import_preserves_imap_calendar_date_across_a_utc_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let items = ItemRepository::new(pool);
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let service = ImportService::new(items, paths);
        let local_received_at = chrono::FixedOffset::east_opt(8 * 60 * 60)
            .unwrap()
            .with_ymd_and_hms(2026, 8, 1, 0, 30, 0)
            .single()
            .unwrap();
        let source = EmailImportSource {
            received_at: local_received_at.with_timezone(&Utc),
            source_received_date: local_received_at.date_naive(),
            ..email_source(801, "1")
        };

        let ImportOutcome::New(imported) = service
            .import_email_bytes("august-boundary.pdf", b"%PDF-1.7\n%%EOF\n", source)
            .await
            .unwrap()
        else {
            panic!("boundary email fixture should be newly imported")
        };

        assert_eq!(
            imported.fetched_at,
            Utc.with_ymd_and_hms(2026, 7, 31, 16, 30, 0)
                .single()
                .unwrap()
        );
        assert_eq!(
            imported.source_received_date,
            chrono::NaiveDate::from_ymd_opt(2026, 8, 1)
        );
        assert_eq!(
            imported.batch_membership_date(),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 1)
        );
    }

    #[tokio::test]
    async fn capped_copy_rejects_zero_and_oversize_content_without_staging_residue() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let paths = AppPaths::create(directory.path().join("storage"))
            .expect("application paths should create");

        async fn assert_rejected<R>(paths: &AppPaths, reader: &mut R)
        where
            R: AsyncRead + Unpin,
        {
            let (staged, writer) = paths
                .begin_staged_original(Uuid::new_v4())
                .expect("staging should begin");
            let error = copy_source_to_staging(reader, staged, writer, false)
                .await
                .expect_err("invalid actual byte count should fail");

            assert!(matches!(error, AppError::Validation { ref field, .. } if field == "file"));
            assert!(
                fs::read_dir(&paths.staging)
                    .expect("staging should read")
                    .next()
                    .is_none()
            );
        }

        assert_rejected(&paths, &mut tokio::io::empty()).await;
        assert_rejected(
            &paths,
            &mut tokio::io::repeat(0x5a).take(super::MAX_FILE_SIZE + 1),
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacing_source_path_after_open_imports_the_pinned_inode() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let source = directory.path().join("pinned.pdf");
        let moved = directory.path().join("opened.pdf");
        let original = b"%PDF-1.7\nopened inode\n";
        let replacement = b"%PDF-1.7\nreplacement path\n";
        fs::write(&source, original).expect("original source should write");
        let pool = db::connect("sqlite::memory:")
            .await
            .expect("in-memory database should connect");
        let paths = AppPaths::create(directory.path().join("storage"))
            .expect("application paths should create");
        let service = ImportService::new(ItemRepository::new(pool), paths);
        let source_for_hook = source.clone();

        let imported = service
            .import_manual_after_open(&source, move || {
                fs::rename(&source_for_hook, &moved)
                    .map_err(|error| super::internal_error("failed to replace source", error))?;
                fs::write(&source_for_hook, replacement)
                    .map_err(|error| super::internal_error("failed to write replacement", error))
            })
            .await
            .expect("pinned source should import");

        assert_eq!(
            fs::read(imported.original_path).expect("imported original should read"),
            original
        );
        assert_eq!(
            fs::read(source).expect("replacement should read"),
            replacement
        );
    }

    #[tokio::test]
    async fn rescan_replaces_failed_unreviewed_email_attachment() {
        let directory = tempfile::tempdir().unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let items = ItemRepository::new(pool);
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let service = ImportService::new(items.clone(), paths.clone());
        let mut source = email_source(5454, "2");
        let corrupted = b"%PDF-1.7\n%\xef\xbf\xbd\xef\xbf\xbd\n%%EOF\n";
        let corrected = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n%%EOF\n";
        let ImportOutcome::New(imported) = service
            .import_email_bytes("invoice.pdf", corrupted, source.clone())
            .await
            .unwrap()
        else {
            panic!("first attachment should be new")
        };
        items
            .update_fields(
                imported.id,
                ItemPatch {
                    recognition_status: Some(RecognitionStatus::Failed),
                    ..ItemPatch::default()
                },
            )
            .await
            .unwrap();
        let old_path = imported.original_path.clone();
        source.rescan = true;

        let ImportOutcome::Existing(repaired) = service
            .import_email_bytes("invoice.pdf", corrected, source)
            .await
            .unwrap()
        else {
            panic!("rescan should preserve the item identity")
        };

        assert_eq!(repaired.id, imported.id);
        assert_eq!(repaired.recognition_status, RecognitionStatus::Pending);
        assert_eq!(fs::read(&repaired.original_path).unwrap(), corrected);
        assert_ne!(repaired.original_path, old_path);
        assert!(!Path::new(&old_path).exists());
        assert_eq!(count_files(&paths.originals), 1);
        assert!(fs::read_dir(&paths.staging).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn downloaded_link_replaces_the_legacy_placeholder_and_recalculates_dedupe() {
        let directory = tempfile::tempdir().unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let items = ItemRepository::new(pool);
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let service = ImportService::new(items.clone(), paths.clone());
        let source = email_source(103, "1.link.0");
        let ImportOutcome::New(legacy) = service
            .import_email_link("https://invoice.example/103.pdf", source.clone())
            .await
            .unwrap()
        else {
            panic!("legacy link should be new")
        };
        let legacy_path = legacy.original_path.clone();
        let pdf = b"%PDF-1.7\ninvoice contents\n%%EOF\n";
        let canonical_source = directory.path().join("canonical.pdf");
        fs::write(&canonical_source, pdf).unwrap();
        let canonical = service.import_manual(&canonical_source).await.unwrap();

        let ImportOutcome::Existing(replaced) = service
            .import_downloaded_email_link("invoice-103.pdf", pdf, source.clone())
            .await
            .unwrap()
        else {
            panic!("placeholder replacement should retain its item identity")
        };

        assert_eq!(replaced.id, legacy.id);
        assert_eq!(replaced.original_name, "invoice-103.pdf");
        assert_eq!(replaced.mime_type, "application/pdf");
        assert_eq!(replaced.recognition_status, RecognitionStatus::Pending);
        assert_eq!(replaced.confirmation_status, ConfirmationStatus::Pending);
        assert_eq!(replaced.dedupe_status, DedupeStatus::SuspectedDuplicate);
        assert_eq!(replaced.duplicate_of_id, Some(canonical.id));
        assert_eq!(replaced.source_account_id, Some(source.account_id));
        assert_eq!(replaced.source_uid, Some(i64::from(source.uid)));
        assert_eq!(replaced.source_part_id.as_deref(), Some("1.link.0"));
        assert_eq!(fs::read(&replaced.original_path).unwrap(), pdf);
        assert!(!std::path::Path::new(&legacy_path).exists());
        assert!(fs::read_dir(&paths.staging).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn stale_download_failure_cannot_mark_a_replaced_pdf_failed() {
        let directory = tempfile::tempdir().unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let items = ItemRepository::new(pool);
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let service = ImportService::new(items.clone(), paths);
        let source = email_source(104, "1.link.0");
        let ImportOutcome::New(placeholder) = service
            .import_email_link(
                "https://invoice.example/104.pdf?signature=private",
                source.clone(),
            )
            .await
            .unwrap()
        else {
            panic!("legacy link should be new")
        };
        let pdf = b"%PDF-1.7\nwinning invoice\n%%EOF\n";
        let ImportOutcome::Existing(replaced) = service
            .import_downloaded_email_link("invoice-104.pdf", pdf, source)
            .await
            .unwrap()
        else {
            panic!("download should replace the placeholder")
        };

        let after_stale_failure = items
            .mark_email_link_download_failed(placeholder.id)
            .await
            .unwrap();

        assert_eq!(after_stale_failure.id, placeholder.id);
        assert_eq!(after_stale_failure.original_name, "invoice-104.pdf");
        assert_eq!(after_stale_failure.mime_type, "application/pdf");
        assert_eq!(
            after_stale_failure.recognition_status,
            RecognitionStatus::Pending
        );
        assert_eq!(after_stale_failure.note, None);
        assert_eq!(after_stale_failure.original_path, replaced.original_path);
        assert_eq!(fs::read(after_stale_failure.original_path).unwrap(), pdf);
    }

    #[tokio::test]
    async fn concurrent_download_failure_and_success_leave_the_real_pdf_pending() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("failure-success.sqlite3");
        let database_url = format!("sqlite://{}", database_path.display());
        let pool = db::connect(&database_url).await.unwrap();
        let items = ItemRepository::new(pool);
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let service = ImportService::new(items.clone(), paths.clone());
        let source = email_source(106, "1.link.0");
        let ImportOutcome::New(placeholder) = service
            .import_email_link(
                "https://invoice.example/106.pdf?signature=private",
                source.clone(),
            )
            .await
            .unwrap()
        else {
            panic!("legacy link should be new")
        };
        let mut pdf = b"%PDF-1.7\n".to_vec();
        pdf.resize(4 * 1024 * 1024, b'y');
        pdf.extend_from_slice(b"\n%%EOF\n");
        let barrier = tokio::sync::Barrier::new(2);

        let (failure, success) = tokio::join!(
            async {
                barrier.wait().await;
                service
                    .import_failed_email_link(
                        "https://invoice.example/106.pdf?signature=private",
                        source.clone(),
                    )
                    .await
            },
            async {
                barrier.wait().await;
                service
                    .import_downloaded_email_link("invoice-106.pdf", &pdf, source.clone())
                    .await
            }
        );
        failure.unwrap();
        success.unwrap();

        let current = items.get_by_id(placeholder.id).await.unwrap();
        assert_eq!(current.original_name, "invoice-106.pdf");
        assert_eq!(current.mime_type, "application/pdf");
        assert_eq!(current.recognition_status, RecognitionStatus::Pending);
        assert_eq!(current.note, None);
        assert_eq!(fs::read(current.original_path).unwrap(), pdf);
        assert_eq!(count_files(&paths.originals), 1);
        assert!(fs::read_dir(&paths.staging).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn failure_guard_preserves_confirmed_and_batch_assigned_placeholders() {
        let directory = tempfile::tempdir().unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let items = ItemRepository::new(pool.clone());
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let service = ImportService::new(items.clone(), paths);
        let confirmed_source = email_source(107, "1.link.0");
        let ImportOutcome::New(confirmed) = service
            .import_email_link("https://invoice.example/107.pdf", confirmed_source.clone())
            .await
            .unwrap()
        else {
            panic!("confirmed fixture should be new")
        };
        items
            .update_fields(
                confirmed.id,
                ItemPatch {
                    confirmation_status: Some(ConfirmationStatus::Confirmed),
                    ..ItemPatch::default()
                },
            )
            .await
            .unwrap();
        let confirmed_before = items.get_by_id(confirmed.id).await.unwrap();

        let batched_source = email_source(108, "1.link.0");
        let ImportOutcome::New(batched) = service
            .import_email_link("https://invoice.example/108.pdf", batched_source)
            .await
            .unwrap()
        else {
            panic!("batched fixture should be new")
        };
        let batch_id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO batches (id, name, start_date, end_date, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(batch_id.to_string())
        .bind("Protected placeholders")
        .bind("2026-07-01")
        .bind("2026-07-31")
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE items SET batch_id = ? WHERE id = ?")
            .bind(batch_id.to_string())
            .bind(batched.id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let batched_before = items.get_by_id(batched.id).await.unwrap();

        let confirmed_after = items
            .mark_email_link_download_failed(confirmed.id)
            .await
            .unwrap();
        let batched_after = items
            .mark_email_link_download_failed(batched.id)
            .await
            .unwrap();

        assert_eq!(confirmed_after, confirmed_before);
        assert_eq!(batched_after, batched_before);
        assert_eq!(
            confirmed_after.recognition_status,
            RecognitionStatus::Succeeded
        );
        assert_eq!(
            batched_after.recognition_status,
            RecognitionStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn concurrent_successful_link_retries_share_one_replaced_original() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("concurrent-link.sqlite3");
        let database_url = format!("sqlite://{}", database_path.display());
        let pool = db::connect(&database_url).await.unwrap();
        let items = ItemRepository::new(pool);
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let service = ImportService::new(items.clone(), paths.clone());
        let source = email_source(105, "1.link.0");
        let ImportOutcome::New(placeholder) = service
            .import_email_link("https://invoice.example/105.pdf", source.clone())
            .await
            .unwrap()
        else {
            panic!("legacy link should be new")
        };
        let mut pdf = b"%PDF-1.7\n".to_vec();
        pdf.resize(8 * 1024 * 1024, b'x');
        pdf.extend_from_slice(b"\n%%EOF\n");
        let barrier = tokio::sync::Barrier::new(2);

        let (first, second) = tokio::join!(
            async {
                barrier.wait().await;
                service
                    .import_downloaded_email_link("invoice-105.pdf", &pdf, source.clone())
                    .await
            },
            async {
                barrier.wait().await;
                service
                    .import_downloaded_email_link("invoice-105.pdf", &pdf, source.clone())
                    .await
            }
        );
        let ImportOutcome::Existing(first) = first.unwrap() else {
            panic!("first retry should retain the placeholder identity")
        };
        let ImportOutcome::Existing(second) = second.unwrap() else {
            panic!("second retry should retain the placeholder identity")
        };

        assert_eq!(first.id, placeholder.id);
        assert_eq!(second.id, placeholder.id);
        assert_eq!(first.original_path, second.original_path);
        assert_eq!(first.original_name, "invoice-105.pdf");
        assert_eq!(first.mime_type, "application/pdf");
        assert_eq!(first.recognition_status, RecognitionStatus::Pending);
        assert_eq!(items.get_by_id(placeholder.id).await.unwrap(), first);
        assert_eq!(count_files(&paths.originals), 1);
        assert!(fs::read_dir(&paths.staging).unwrap().next().is_none());
        assert_eq!(fs::read(first.original_path).unwrap(), pdf);
    }

    #[tokio::test]
    async fn ignored_link_discards_only_an_unassigned_pending_placeholder() {
        let directory = tempfile::tempdir().unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let items = ItemRepository::new(pool);
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let service = ImportService::new(items.clone(), paths);
        let disposable_source = email_source(201, "1.link.0");
        let ImportOutcome::New(disposable) = service
            .import_email_link(
                "https://invoice.example/invoice.xml",
                disposable_source.clone(),
            )
            .await
            .unwrap()
        else {
            panic!("disposable link should be new")
        };
        let protected_source = email_source(202, "1.link.0");
        let ImportOutcome::New(protected) = service
            .import_email_link("https://fp.nuonuo.com/#/", protected_source.clone())
            .await
            .unwrap()
        else {
            panic!("protected link should be new")
        };
        items
            .update_fields(
                protected.id,
                ItemPatch {
                    confirmation_status: Some(ConfirmationStatus::Confirmed),
                    ..ItemPatch::default()
                },
            )
            .await
            .unwrap();

        assert!(
            service
                .discard_email_link_placeholder(&disposable_source)
                .await
                .unwrap()
        );
        assert!(
            !service
                .discard_email_link_placeholder(&protected_source)
                .await
                .unwrap()
        );

        assert!(items.get_by_id(disposable.id).await.is_err());
        assert!(!std::path::Path::new(&disposable.original_path).exists());
        assert_eq!(
            items.get_by_id(protected.id).await.unwrap().id,
            protected.id
        );
        assert!(std::path::Path::new(&protected.original_path).exists());
    }

    fn count_files(path: &Path) -> usize {
        fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                if path.is_dir() { count_files(&path) } else { 1 }
            })
            .sum()
    }
}
