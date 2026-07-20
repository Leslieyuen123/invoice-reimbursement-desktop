use std::collections::HashSet;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;

use kuchiki::traits::TendrilSink;
use mail_parser::{MessageParser, MimeHeaders};
use uuid::Uuid;

use crate::db::accounts::{MailboxAccountRepository, SyncCursor};
use crate::domain::error::{AppError, sanitize_message};
use crate::domain::model::{ConfirmationStatus, RecognitionStatus};
use crate::infra::credentials::{CredentialStore, get_credential};
use crate::infra::imap::{ImapAccountConfig, ImapGateway, MailboxDelta, RawMessage};
use crate::services::import::{EmailImportSource, ImportOutcome, ImportService};
use crate::services::recognition::RecognitionService;

const MAX_MESSAGES_PER_SYNC: usize = 1_000;
const MAX_RAW_MESSAGE_BYTES: usize = 50 * 1024 * 1024;
const MAX_TOTAL_RAW_BYTES: usize = 200 * 1024 * 1024;
const MAX_PARTS_PER_MESSAGE: usize = 256;
const MAX_DOWNLOAD_LINKS_PER_MESSAGE: usize = 32;
const MAX_DOWNLOAD_URL_LENGTH: usize = 2_048;
const MAX_ZIP_ENTRIES: usize = 64;
const MAX_ZIP_ENTRY_BYTES: u64 = 50 * 1024 * 1024;
const MAX_ZIP_TOTAL_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncResult {
    pub imported_count: u32,
}

#[derive(Clone)]
pub struct SyncService {
    gateway: Arc<dyn ImapGateway>,
    credentials: Arc<dyn CredentialStore>,
    accounts: MailboxAccountRepository,
    import: ImportService,
    recognition: RecognitionService,
}

impl SyncService {
    pub fn new(
        gateway: Arc<dyn ImapGateway>,
        credentials: Arc<dyn CredentialStore>,
        accounts: MailboxAccountRepository,
        import: ImportService,
        recognition: RecognitionService,
    ) -> Self {
        Self {
            gateway,
            credentials,
            accounts,
            import,
            recognition,
        }
    }

    pub async fn run(&self, account_id: Uuid) -> Result<SyncResult, AppError> {
        let run = self.accounts.begin_sync_run(account_id).await?;
        let account = match self.accounts.get(account_id).await {
            Ok(account) => account,
            Err(error) => {
                let sanitized = sanitize_error(&error.to_string(), "");
                self.accounts.finish_sync_failure(&run, &sanitized).await?;
                return Err(sanitized_external_error(&error, sanitized));
            }
        };
        let secret = match get_credential(self.credentials.clone(), account_id.to_string()).await {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                let error = authentication_error("mailbox credential is unavailable");
                self.accounts
                    .finish_sync_failure(&run, &error.to_string())
                    .await?;
                return Err(error);
            }
            Err(error) => {
                self.accounts
                    .finish_sync_failure(&run, &error.to_string())
                    .await?;
                return Err(error);
            }
        };
        let config = ImapAccountConfig::from_account(&account);
        let cursor = match self.accounts.get_cursor(account_id, &config.mailbox).await {
            Ok(cursor) => cursor,
            Err(error) => {
                let sanitized = sanitize_error(&error.to_string(), &secret);
                self.accounts.finish_sync_failure(&run, &sanitized).await?;
                return Err(sanitized_external_error(&error, sanitized));
            }
        };
        let result = self.run_delta(account_id, &config, &secret, cursor).await;
        match result {
            Ok((result, next_cursor)) => {
                if let Err(error) = self
                    .accounts
                    .finish_sync_success(
                        &run,
                        &config.mailbox,
                        cursor,
                        next_cursor,
                        result.imported_count,
                    )
                    .await
                {
                    let sanitized = sanitize_error(&error.to_string(), &secret);
                    self.accounts.finish_sync_failure(&run, &sanitized).await?;
                    return Err(sanitized_external_error(&error, sanitized));
                }
                Ok(result)
            }
            Err(error) => {
                let sanitized = sanitize_error(&error.to_string(), &secret);
                self.accounts.finish_sync_failure(&run, &sanitized).await?;
                Err(sanitized_external_error(&error, sanitized))
            }
        }
    }

    async fn run_delta(
        &self,
        account_id: Uuid,
        config: &ImapAccountConfig,
        secret: &str,
        cursor: Option<SyncCursor>,
    ) -> Result<(SyncResult, SyncCursor), AppError> {
        let delta = self.gateway.fetch_since(config, secret, cursor).await?;
        validate_delta(&delta, &config.mailbox)?;
        let rescan = cursor.is_some_and(|cursor| cursor.uid_validity != delta.uid_validity);
        let mut imported_count = 0_u32;
        for rejected in &delta.rejected_messages {
            let outcome = self
                .import
                .import_email_rejection(account_id, delta.uid_validity, rejected, rescan)
                .await?;
            if matches!(outcome, ImportOutcome::New(_)) {
                imported_count =
                    imported_count
                        .checked_add(1)
                        .ok_or_else(|| AppError::Internal {
                            message: "sync import count overflow".to_owned(),
                        })?;
            }
        }
        for raw_message in &delta.messages {
            let parsed = parse_invoice_parts(raw_message)?;
            for part in parsed.files {
                let outcome = self
                    .import
                    .import_email_bytes(
                        &part.file_name,
                        &part.bytes,
                        EmailImportSource {
                            account_id,
                            mailbox: raw_message.mailbox.clone(),
                            uid_validity: delta.uid_validity,
                            uid: raw_message.uid,
                            message_id: part.message_id.clone(),
                            part_id: part.part_id,
                            received_at: raw_message.received_at,
                            rescan,
                        },
                    )
                    .await?;
                let item = match outcome {
                    ImportOutcome::New(item) => {
                        imported_count =
                            imported_count
                                .checked_add(1)
                                .ok_or_else(|| AppError::Internal {
                                    message: "sync import count overflow".to_owned(),
                                })?;
                        item
                    }
                    ImportOutcome::Existing(item) => item,
                };
                if item.recognition_status == RecognitionStatus::Pending
                    && item.confirmation_status == ConfirmationStatus::Pending
                    && let Err(error) = self.recognition.recognize_item(item.id).await
                    && !is_document_recognition_failure(&error)
                {
                    return Err(error);
                }
            }
            for link in parsed.links {
                let outcome = self
                    .import
                    .import_email_link(
                        &link.url,
                        EmailImportSource {
                            account_id,
                            mailbox: raw_message.mailbox.clone(),
                            uid_validity: delta.uid_validity,
                            uid: raw_message.uid,
                            message_id: link.message_id,
                            part_id: link.part_id,
                            received_at: raw_message.received_at,
                            rescan,
                        },
                    )
                    .await?;
                if matches!(outcome, ImportOutcome::New(_)) {
                    imported_count =
                        imported_count
                            .checked_add(1)
                            .ok_or_else(|| AppError::Internal {
                                message: "sync import count overflow".to_owned(),
                            })?;
                }
            }
        }
        Ok((
            SyncResult { imported_count },
            SyncCursor {
                uid_validity: delta.uid_validity,
                last_uid: delta.highest_uid,
            },
        ))
    }
}

struct InvoicePart {
    part_id: String,
    file_name: String,
    bytes: Vec<u8>,
    message_id: Option<String>,
}

struct DownloadLink {
    part_id: String,
    url: String,
    message_id: Option<String>,
}

struct ParsedInvoiceParts {
    files: Vec<InvoicePart>,
    links: Vec<DownloadLink>,
}

fn validate_delta(delta: &MailboxDelta, expected_mailbox: &str) -> Result<(), AppError> {
    let message_count = delta
        .messages
        .len()
        .checked_add(delta.rejected_messages.len())
        .ok_or_else(|| resource_limit_error("IMAP message count overflow"))?;
    if message_count > MAX_MESSAGES_PER_SYNC {
        return Err(resource_limit_error("too many messages in IMAP delta"));
    }
    let mut total_bytes = 0_usize;
    let mut seen_uids = HashSet::new();
    for message in &delta.messages {
        validate_message_identity(
            message.uid,
            &message.mailbox,
            delta.highest_uid,
            expected_mailbox,
            &mut seen_uids,
        )?;
        if message.raw.len() > MAX_RAW_MESSAGE_BYTES {
            return Err(resource_limit_error("IMAP message exceeds size limit"));
        }
        total_bytes = total_bytes
            .checked_add(message.raw.len())
            .ok_or_else(|| resource_limit_error("IMAP delta size overflow"))?;
        if total_bytes > MAX_TOTAL_RAW_BYTES {
            return Err(resource_limit_error("IMAP delta exceeds total size limit"));
        }
    }
    for message in &delta.rejected_messages {
        validate_message_identity(
            message.uid,
            &message.mailbox,
            delta.highest_uid,
            expected_mailbox,
            &mut seen_uids,
        )?;
    }
    Ok(())
}

fn validate_message_identity(
    uid: u32,
    mailbox: &str,
    highest_uid: u32,
    expected_mailbox: &str,
    seen_uids: &mut HashSet<u32>,
) -> Result<(), AppError> {
    if mailbox != expected_mailbox {
        return Err(external_error("IMAP delta contained an unexpected mailbox"));
    }
    if uid == 0 || uid > highest_uid || !seen_uids.insert(uid) {
        return Err(external_error(
            "IMAP delta contained an invalid or duplicate UID",
        ));
    }
    Ok(())
}

fn parse_invoice_parts(raw: &RawMessage) -> Result<ParsedInvoiceParts, AppError> {
    let message = MessageParser::default()
        .parse(&raw.raw)
        .ok_or_else(|| external_error("mailbox message could not be parsed"))?;
    if message.parts.len() > MAX_PARTS_PER_MESSAGE {
        return Err(resource_limit_error(
            "mailbox message has too many MIME parts",
        ));
    }
    let message_id = message.message_id().map(str::to_owned);
    let mut files = Vec::new();
    let mut links = Vec::new();
    let mut seen_links = HashSet::new();
    for (index, part) in message.parts.iter().enumerate() {
        let disposition = part.content_disposition();
        let is_attachment = disposition.is_some_and(|value| value.is_attachment());
        let is_inline_image = (part.is_content_type("image", "jpeg")
            || part.is_content_type("image", "png"))
            && (part.attachment_name().is_some() || part.content_id().is_some());
        if is_attachment || is_inline_image {
            let Some(file_name) = part
                .attachment_name()
                .map(str::to_owned)
                .or_else(|| inline_file_name(part, index))
            else {
                continue;
            };
            if !is_supported_file_name(&file_name) {
                continue;
            }
            let file = InvoicePart {
                part_id: index.to_string(),
                file_name,
                bytes: part.contents().to_vec(),
                message_id: message_id.clone(),
            };
            if file_name_has_extension(&file.file_name, "zip") {
                let expanded = expand_zip_part(&file);
                files.push(file);
                if let Ok(expanded) = expanded {
                    files.extend(expanded);
                }
            } else {
                files.push(file);
            }
        }
    }
    for part_id in &message.html_body {
        if links.len() >= MAX_DOWNLOAD_LINKS_PER_MESSAGE {
            break;
        }
        let Some(html) = message.part(*part_id).and_then(|part| part.text_contents()) else {
            continue;
        };
        collect_https_links(
            html,
            *part_id as usize,
            &message_id,
            &mut seen_links,
            &mut links,
        )?;
    }
    Ok(ParsedInvoiceParts { files, links })
}

fn expand_zip_part(part: &InvoicePart) -> Result<Vec<InvoicePart>, ()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(&part.bytes)).map_err(|_| ())?;
    if archive.len() > MAX_ZIP_ENTRIES {
        return Err(());
    }
    let mut expanded = Vec::new();
    let mut total_bytes = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|_| ())?;
        if entry.is_dir() || entry.size() > MAX_ZIP_ENTRY_BYTES {
            continue;
        }
        let Some(enclosed) = entry.enclosed_name() else {
            continue;
        };
        let Some(file_name) = enclosed.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_recognizable_file_name(file_name) {
            continue;
        }
        total_bytes = total_bytes.checked_add(entry.size()).ok_or(())?;
        if total_bytes > MAX_ZIP_TOTAL_BYTES {
            return Err(());
        }
        let capacity = usize::try_from(entry.size()).map_err(|_| ())?;
        let mut bytes = Vec::with_capacity(capacity);
        entry
            .by_ref()
            .take(MAX_ZIP_ENTRY_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        if bytes.len() as u64 > MAX_ZIP_ENTRY_BYTES {
            return Err(());
        }
        expanded.push(InvoicePart {
            part_id: format!("{}.zip.{index}", part.part_id),
            file_name: file_name.to_owned(),
            bytes,
            message_id: part.message_id.clone(),
        });
    }
    Ok(expanded)
}

fn collect_https_links(
    html: &str,
    mime_index: usize,
    message_id: &Option<String>,
    seen: &mut HashSet<String>,
    links: &mut Vec<DownloadLink>,
) -> Result<(), AppError> {
    let document = kuchiki::parse_html().one(html);
    let anchors = document.select("a[href]").map_err(|_| AppError::Internal {
        message: "failed to prepare HTML link selector".to_owned(),
    })?;
    for anchor in anchors {
        if links.len() >= MAX_DOWNLOAD_LINKS_PER_MESSAGE {
            break;
        }
        let visible_text = anchor.text_contents();
        let attributes = anchor.attributes.borrow();
        let Some(href) = attributes.get("href") else {
            continue;
        };
        let has_download_attribute = attributes.get("download").is_some();
        if href.len() > MAX_DOWNLOAD_URL_LENGTH {
            continue;
        }
        let Ok(url) = url::Url::parse(href) else {
            continue;
        };
        if url.scheme() != "https" || url.host_str().is_none() {
            continue;
        }
        if !is_supported_file_name(url.path())
            && !has_download_attribute
            && !has_download_text(&visible_text)
        {
            continue;
        }
        let normalized = url.to_string();
        if !seen.insert(normalized.clone()) {
            continue;
        }
        links.push(DownloadLink {
            part_id: format!("{mime_index}.link.{}", links.len()),
            url: normalized,
            message_id: message_id.clone(),
        });
    }
    Ok(())
}

fn has_download_text(text: &str) -> bool {
    let normalized = text.to_lowercase();
    ["download", "invoice", "receipt", "下载", "发票", "票据"]
        .iter()
        .any(|keyword| normalized.contains(keyword))
}

fn inline_file_name(part: &mail_parser::MessagePart<'_>, index: usize) -> Option<String> {
    if part.is_content_type("image", "jpeg") {
        Some(format!("inline-{index}.jpg"))
    } else if part.is_content_type("image", "png") {
        Some(format!("inline-{index}.png"))
    } else {
        None
    }
}

fn is_supported_file_name(file_name: &str) -> bool {
    Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "pdf" | "jpg" | "jpeg" | "png" | "doc" | "docx" | "xls" | "xlsx" | "zip"
            )
        })
}

fn is_recognizable_file_name(file_name: &str) -> bool {
    Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "pdf" | "jpg" | "jpeg" | "png"
            )
        })
}

fn file_name_has_extension(file_name: &str, expected: &str) -> bool {
    Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn sanitize_error(message: &str, secret: &str) -> String {
    sanitize_message(message, &[secret])
}

fn is_document_recognition_failure(error: &AppError) -> bool {
    matches!(
        error,
        AppError::External { service, .. }
            if matches!(service.as_str(), "document_extractor" | "ocr_sidecar")
    )
}

fn sanitized_external_error(error: &AppError, message: String) -> AppError {
    match error {
        AppError::External {
            service, retryable, ..
        } => AppError::External {
            service: service.clone(),
            retryable: *retryable,
            message,
        },
        AppError::Conflict { .. } => AppError::Conflict { message },
        _ => AppError::Internal { message },
    }
}

fn external_error(message: &str) -> AppError {
    AppError::External {
        service: "imap".to_owned(),
        retryable: true,
        message: message.to_owned(),
    }
}

fn authentication_error(message: &str) -> AppError {
    AppError::External {
        service: "mailbox_credential".to_owned(),
        retryable: false,
        message: message.to_owned(),
    }
}

fn resource_limit_error(message: &str) -> AppError {
    AppError::External {
        service: "imap".to_owned(),
        retryable: false,
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use chrono::Utc;
    use zip::write::SimpleFileOptions;

    use super::{
        InvoicePart, MAX_MESSAGES_PER_SYNC, expand_zip_part, parse_invoice_parts, validate_delta,
    };
    use crate::infra::imap::{MailboxDelta, MessageRejectionReason, RawMessage, RejectedMessage};

    fn rejected_message(uid: u32) -> RejectedMessage {
        RejectedMessage {
            uid,
            mailbox: "INBOX".to_owned(),
            received_at: Utc::now(),
            reason: MessageRejectionReason::MessageTooLarge,
        }
    }

    #[test]
    fn rejected_messages_count_toward_the_delta_limit() {
        let rejected_messages = (1..=MAX_MESSAGES_PER_SYNC as u32)
            .map(rejected_message)
            .collect();
        let delta = MailboxDelta {
            uid_validity: 1,
            messages: vec![RawMessage {
                uid: MAX_MESSAGES_PER_SYNC as u32 + 1,
                mailbox: "INBOX".to_owned(),
                raw: Vec::new(),
                received_at: Utc::now(),
            }],
            rejected_messages,
            highest_uid: MAX_MESSAGES_PER_SYNC as u32 + 1,
        };

        let error = validate_delta(&delta, "INBOX").unwrap_err();

        assert!(error.to_string().contains("too many messages"));
    }

    #[test]
    fn zip_attachments_expand_supported_invoice_files() {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file("folder/invoice.pdf", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"%PDF-invoice").unwrap();
        archive
            .start_file("notes/readme.txt", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"not an invoice").unwrap();
        let bytes = archive.finish().unwrap().into_inner();
        let part = InvoicePart {
            part_id: "4".to_owned(),
            file_name: "invoices.zip".to_owned(),
            bytes,
            message_id: Some("zip@example.com".to_owned()),
        };

        let expanded = expand_zip_part(&part).unwrap();

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].part_id, "4.zip.0");
        assert_eq!(expanded[0].file_name, "invoice.pdf");
        assert_eq!(expanded[0].bytes, b"%PDF-invoice");
        assert_eq!(expanded[0].message_id, part.message_id);
    }

    #[test]
    fn parsed_zip_attachment_preserves_original_and_appends_expanded_files() {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file("folder/invoice.pdf", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"%PDF-invoice").unwrap();
        let zip_bytes = archive.finish().unwrap().into_inner();
        let boundary = "invoice-parts-boundary";
        let mut message = format!(
            "From: billing@example.com\r\n\
             To: finance@example.com\r\n\
             Message-ID: <zip@example.com>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\
             \r\n\
             --{boundary}\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Invoice attached.\r\n\
             --{boundary}\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <p>Invoice attached.</p>\r\n\
             --{boundary}\r\n\
             Content-Type: application/octet-stream\r\n\
             \r\n\
             metadata\r\n\
             --{boundary}\r\n\
             Content-Type: application/zip; name=\"invoices.zip\"\r\n\
             Content-Disposition: attachment; filename=\"invoices.zip\"\r\n\
             Content-Transfer-Encoding: binary\r\n\
             \r\n"
        )
        .into_bytes();
        message.extend_from_slice(&zip_bytes);
        message.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let raw = RawMessage {
            uid: 4,
            mailbox: "INBOX".to_owned(),
            raw: message,
            received_at: Utc::now(),
        };

        let parsed = parse_invoice_parts(&raw).unwrap();

        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].part_id, "4");
        assert_eq!(parsed.files[0].file_name, "invoices.zip");
        assert_eq!(parsed.files[0].bytes, zip_bytes);
        assert_eq!(
            parsed.files[0].message_id.as_deref(),
            Some("zip@example.com")
        );
        assert_eq!(parsed.files[1].part_id, "4.zip.0");
        assert_eq!(parsed.files[1].file_name, "invoice.pdf");
        assert_eq!(parsed.files[1].bytes, b"%PDF-invoice");
        assert_eq!(
            parsed.files[1].message_id.as_deref(),
            Some("zip@example.com")
        );
    }

    #[test]
    fn rejected_message_uids_must_be_unique_across_the_delta() {
        let delta = MailboxDelta {
            uid_validity: 1,
            messages: vec![RawMessage {
                uid: 7,
                mailbox: "INBOX".to_owned(),
                raw: Vec::new(),
                received_at: Utc::now(),
            }],
            rejected_messages: vec![rejected_message(7)],
            highest_uid: 7,
        };

        let error = validate_delta(&delta, "INBOX").unwrap_err();

        assert!(error.to_string().contains("invalid or duplicate UID"));
    }

    #[test]
    fn rejected_messages_must_match_the_mailbox_and_high_water() {
        let mut wrong_mailbox = rejected_message(7);
        wrong_mailbox.mailbox = "Archive".to_owned();
        let mailbox_delta = MailboxDelta {
            uid_validity: 1,
            messages: vec![],
            rejected_messages: vec![wrong_mailbox],
            highest_uid: 7,
        };
        let uid_delta = MailboxDelta {
            uid_validity: 1,
            messages: vec![],
            rejected_messages: vec![rejected_message(8)],
            highest_uid: 7,
        };

        let mailbox_error = validate_delta(&mailbox_delta, "INBOX").unwrap_err();
        let uid_error = validate_delta(&uid_delta, "INBOX").unwrap_err();

        assert!(mailbox_error.to_string().contains("mailbox"));
        assert!(uid_error.to_string().contains("invalid or duplicate UID"));
    }
}
