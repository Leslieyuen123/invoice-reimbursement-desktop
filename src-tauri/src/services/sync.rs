use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use kuchiki::traits::TendrilSink;
use mail_parser::{MessageParser, MimeHeaders};
use uuid::Uuid;

use crate::db::accounts::{MailboxAccountRepository, SyncCursor};
use crate::domain::error::AppError;
use crate::infra::credentials::CredentialStore;
use crate::infra::imap::{ImapAccountConfig, ImapGateway, MailboxDelta, RawMessage};
use crate::services::import::{EmailImportSource, ImportOutcome, ImportService};
use crate::services::recognition::RecognitionService;

const MAX_MESSAGES_PER_SYNC: usize = 1_000;
const MAX_RAW_MESSAGE_BYTES: usize = 50 * 1024 * 1024;
const MAX_TOTAL_RAW_BYTES: usize = 200 * 1024 * 1024;
const MAX_PARTS_PER_MESSAGE: usize = 256;
const MAX_DOWNLOAD_LINKS_PER_MESSAGE: usize = 32;
const MAX_DOWNLOAD_URL_LENGTH: usize = 2_048;

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
        let secret = match self.credentials.get(&account_id.to_string()) {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                let error = external_error("mailbox credential is unavailable");
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
                    .finish_sync_success(&run, &config.mailbox, next_cursor, result.imported_count)
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
        let mut imported_count = 0_u32;
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
                            uid: raw_message.uid,
                            message_id: part.message_id.clone(),
                            part_id: part.part_id,
                            received_at: raw_message.received_at,
                        },
                    )
                    .await?;
                if let ImportOutcome::New(item) = outcome {
                    imported_count =
                        imported_count
                            .checked_add(1)
                            .ok_or_else(|| AppError::Internal {
                                message: "sync import count overflow".to_owned(),
                            })?;
                    if let Err(error) = self.recognition.recognize_item(item.id).await
                        && !is_document_recognition_failure(&error)
                    {
                        return Err(error);
                    }
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
                            uid: raw_message.uid,
                            message_id: link.message_id,
                            part_id: link.part_id,
                            received_at: raw_message.received_at,
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
    if delta.messages.len() > MAX_MESSAGES_PER_SYNC {
        return Err(resource_limit_error("too many messages in IMAP delta"));
    }
    let mut total_bytes = 0_usize;
    let mut seen_uids = HashSet::new();
    for message in &delta.messages {
        if message.mailbox != expected_mailbox {
            return Err(external_error("IMAP delta contained an unexpected mailbox"));
        }
        if message.uid == 0 || message.uid > delta.highest_uid || !seen_uids.insert(message.uid) {
            return Err(external_error(
                "IMAP delta contained an invalid or duplicate UID",
            ));
        }
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
        if let Some(html) = part
            .text_contents()
            .filter(|_| part.is_content_type("text", "html"))
        {
            collect_https_links(html, index, &message_id, &mut seen_links, &mut links)?;
        }
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
            files.push(InvoicePart {
                part_id: index.to_string(),
                file_name,
                bytes: part.contents().to_vec(),
                message_id: message_id.clone(),
            });
        }
    }
    Ok(ParsedInvoiceParts { files, links })
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
        let attributes = anchor.attributes.borrow();
        let Some(href) = attributes.get("href") else {
            continue;
        };
        if href.len() > MAX_DOWNLOAD_URL_LENGTH {
            continue;
        }
        let Ok(url) = url::Url::parse(href) else {
            continue;
        };
        if url.scheme() != "https" || url.host_str().is_none() {
            continue;
        }
        let normalized = url.to_string();
        if !seen.insert(normalized.clone()) {
            continue;
        }
        if links.len() >= MAX_DOWNLOAD_LINKS_PER_MESSAGE {
            return Err(resource_limit_error(
                "mailbox message has too many HTTPS download links",
            ));
        }
        links.push(DownloadLink {
            part_id: format!("{mime_index}.link.{}", links.len()),
            url: normalized,
            message_id: message_id.clone(),
        });
    }
    Ok(())
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

fn sanitize_error(message: &str, secret: &str) -> String {
    let redacted = if secret.is_empty() {
        message.to_owned()
    } else {
        message.replace(secret, "[redacted]")
    };
    redacted
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(512)
        .collect::<String>()
        .trim()
        .to_owned()
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

fn resource_limit_error(message: &str) -> AppError {
    AppError::External {
        service: "imap".to_owned(),
        retryable: false,
        message: message.to_owned(),
    }
}
