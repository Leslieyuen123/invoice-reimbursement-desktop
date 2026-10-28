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
        let existing = self
            .items
            .find_email_part(
                source.account_id,
                &source.mailbox,
                source.uid_validity,
                source.uid,
                &source.part_id,
            )
            .await?;
        match existing {
            Some(item) => self
                .refresh_email_received_metadata(item, source)
                .await
                .map(Some),
            None => Ok(None),
        }
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
        let Some(existing) = self.find_email_part(&source).await? else {
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
                let current =
                    self.find_email_part(&source)
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
        if let Some(existing) = self.find_email_part(&payload.source).await? {
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
            let legacy = self
                .refresh_email_received_metadata(legacy, &payload.source)
                .await?;
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
                let existing = self
                    .refresh_email_received_metadata(existing, &payload.source)
                    .await?;
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
                    && let Some(existing) = self.find_email_part(&payload.source).await?
                {
                    return Ok(ImportOutcome::Existing(existing));
                }
                Err(database_error)
            }
        }
    }

    async fn refresh_email_received_metadata(
        &self,
        existing: InvoiceItem,
        source: &EmailImportSource,
    ) -> Result<InvoiceItem, AppError> {
        self.items
            .update_email_received_metadata(
                existing.id,
                source.received_at,
                source.source_received_date,
            )
            .await
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
mod tests;
