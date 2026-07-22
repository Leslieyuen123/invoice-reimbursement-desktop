use std::collections::HashSet;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;

use chrono::NaiveDate;
use kuchiki::traits::TendrilSink;
use mail_parser::{MessageParser, MimeHeaders};
use uuid::Uuid;

use crate::db::accounts::{MailboxAccountRepository, SyncCursor};
use crate::domain::error::{AppError, sanitize_message};
use crate::domain::model::{ConfirmationStatus, RecognitionStatus};
use crate::infra::credentials::{CredentialStore, get_credential};
use crate::infra::imap::{ImapAccountConfig, ImapDateRange, ImapGateway, MailboxDelta, RawMessage};
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

#[derive(Debug, Clone, Copy)]
struct ZipExpansionBudget {
    remaining_entries: usize,
    remaining_bytes: u64,
}

impl Default for ZipExpansionBudget {
    fn default() -> Self {
        Self {
            remaining_entries: MAX_ZIP_ENTRIES,
            remaining_bytes: MAX_ZIP_TOTAL_BYTES,
        }
    }
}

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

    pub async fn run_range(
        &self,
        account_id: Uuid,
        start_date: NaiveDate,
        end_date: NaiveDate,
    ) -> Result<SyncResult, AppError> {
        if start_date > end_date {
            return Err(AppError::validation(
                "dateRange",
                "IMAP start date must not be after end date",
            ));
        }
        let end_exclusive = end_date.succ_opt().ok_or_else(|| {
            AppError::validation("dateRange", "IMAP end date is outside the supported range")
        })?;
        let range = ImapDateRange::new(start_date, end_exclusive)?;
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
        let result = self
            .run_range_deltas(account_id, &config, &secret, range)
            .await;
        match result {
            Ok(result) => {
                if let Err(error) = self
                    .accounts
                    .finish_range_sync_success(&run, result.imported_count)
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
        let result = self.process_delta(account_id, &delta, rescan).await?;
        Ok((
            result,
            SyncCursor {
                uid_validity: delta.uid_validity,
                last_uid: delta.highest_uid,
            },
        ))
    }

    async fn run_range_deltas(
        &self,
        account_id: Uuid,
        config: &ImapAccountConfig,
        secret: &str,
        range: ImapDateRange,
    ) -> Result<SyncResult, AppError> {
        let mut cursor = None;
        let mut imported_count = 0_u32;
        loop {
            let delta = self
                .gateway
                .fetch_range(config, secret, cursor, range)
                .await?;
            validate_delta(&delta, &config.mailbox)?;
            let rescan =
                cursor.is_some_and(|cursor: SyncCursor| cursor.uid_validity != delta.uid_validity);
            let result = self.process_delta(account_id, &delta, rescan).await?;
            imported_count = imported_count
                .checked_add(result.imported_count)
                .ok_or_else(sync_import_count_overflow)?;
            let next_cursor = SyncCursor {
                uid_validity: delta.uid_validity,
                last_uid: delta.highest_uid,
            };
            if cursor == Some(next_cursor) {
                return Ok(SyncResult { imported_count });
            }
            cursor = Some(next_cursor);
        }
    }

    async fn process_delta(
        &self,
        account_id: Uuid,
        delta: &MailboxDelta,
        rescan: bool,
    ) -> Result<SyncResult, AppError> {
        let mut imported_count = 0_u32;
        for rejected in &delta.rejected_messages {
            let outcome = self
                .import
                .import_email_rejection(account_id, delta.uid_validity, rejected, rescan)
                .await?;
            if matches!(outcome, ImportOutcome::New(_)) {
                imported_count = imported_count
                    .checked_add(1)
                    .ok_or_else(sync_import_count_overflow)?;
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
                        imported_count = imported_count
                            .checked_add(1)
                            .ok_or_else(sync_import_count_overflow)?;
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
                    imported_count = imported_count
                        .checked_add(1)
                        .ok_or_else(sync_import_count_overflow)?;
                }
            }
        }
        Ok(SyncResult { imported_count })
    }
}

fn sync_import_count_overflow() -> AppError {
    AppError::Internal {
        message: "sync import count overflow".to_owned(),
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
    parse_invoice_parts_with_zip_budget(raw, ZipExpansionBudget::default())
}

fn parse_invoice_parts_with_zip_budget(
    raw: &RawMessage,
    mut zip_budget: ZipExpansionBudget,
) -> Result<ParsedInvoiceParts, AppError> {
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
                let expanded = expand_zip_part(&file, &mut zip_budget);
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

fn preflight_zip_entries(bytes: &[u8]) -> Result<usize, ()> {
    const EOCD_BYTES: usize = 22;
    const MAX_COMMENT_BYTES: usize = u16::MAX as usize;

    let search_start = bytes
        .len()
        .saturating_sub(EOCD_BYTES.saturating_add(MAX_COMMENT_BYTES));
    let mut eocd_offset = None;
    for (offset, signature) in bytes.windows(4).enumerate() {
        if signature == b"PK\x06\x06" || signature == b"PK\x06\x07" {
            return Err(());
        }
        if signature == b"PK\x05\x06" && eocd_offset.replace(offset).is_some() {
            return Err(());
        }
    }
    let eocd_offset = eocd_offset
        .filter(|offset| *offset >= search_start)
        .ok_or(())?;
    let record = bytes
        .get(eocd_offset..eocd_offset.checked_add(EOCD_BYTES).ok_or(())?)
        .ok_or(())?;
    let comment_bytes = u16::from_le_bytes([record[20], record[21]]) as usize;
    if eocd_offset
        .checked_add(EOCD_BYTES)
        .and_then(|end| end.checked_add(comment_bytes))
        != Some(bytes.len())
    {
        return Err(());
    }
    let disk = u16::from_le_bytes([record[4], record[5]]);
    let central_directory_disk = u16::from_le_bytes([record[6], record[7]]);
    let entries_on_disk = u16::from_le_bytes([record[8], record[9]]);
    let total_entries = u16::from_le_bytes([record[10], record[11]]);
    let central_directory_bytes =
        u32::from_le_bytes([record[12], record[13], record[14], record[15]]);
    let central_directory_offset =
        u32::from_le_bytes([record[16], record[17], record[18], record[19]]);
    if disk != 0
        || central_directory_disk != 0
        || entries_on_disk != total_entries
        || entries_on_disk == u16::MAX
        || total_entries == u16::MAX
        || central_directory_bytes == u32::MAX
        || central_directory_offset == u32::MAX
    {
        return Err(());
    }
    let central_directory_end = usize::try_from(central_directory_offset)
        .map_err(|_| ())?
        .checked_add(usize::try_from(central_directory_bytes).map_err(|_| ())?)
        .ok_or(())?;
    if central_directory_end > eocd_offset {
        return Err(());
    }
    Ok(usize::from(total_entries))
}

fn expand_zip_part(
    part: &InvoicePart,
    budget: &mut ZipExpansionBudget,
) -> Result<Vec<InvoicePart>, ()> {
    let declared_entries = preflight_zip_entries(&part.bytes)?;
    if declared_entries > MAX_ZIP_ENTRIES {
        return Err(());
    }
    let Some(remaining_entries) = budget.remaining_entries.checked_sub(declared_entries) else {
        budget.remaining_entries = 0;
        return Err(());
    };
    budget.remaining_entries = remaining_entries;
    if declared_entries == 0 {
        return Ok(Vec::new());
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(&part.bytes)).map_err(|_| ())?;
    if archive.len() != declared_entries {
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
        let entry_size = entry.size();
        total_bytes = total_bytes.checked_add(entry_size).ok_or(())?;
        if total_bytes > MAX_ZIP_TOTAL_BYTES {
            return Err(());
        }
        let Some(remaining_bytes) = budget.remaining_bytes.checked_sub(entry_size) else {
            budget.remaining_bytes = 0;
            return Err(());
        };
        budget.remaining_bytes = remaining_bytes;
        let capacity = usize::try_from(entry_size).map_err(|_| ())?;
        let mut bytes = Vec::with_capacity(capacity);
        entry
            .by_ref()
            .take(entry_size)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        let mut extra = [0_u8; 1];
        if bytes.len() as u64 != entry_size || entry.read(&mut extra).map_err(|_| ())? != 0 {
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
        InvoicePart, MAX_MESSAGES_PER_SYNC, ZipExpansionBudget, expand_zip_part,
        parse_invoice_parts, parse_invoice_parts_with_zip_budget, preflight_zip_entries,
        validate_delta,
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

    fn zip_with_pdf_entries(prefix: &str, count: usize, contents: &[u8]) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..count {
            archive
                .start_file(
                    format!("{prefix}-{index}.pdf"),
                    SimpleFileOptions::default(),
                )
                .unwrap();
            archive.write_all(contents).unwrap();
        }
        archive.finish().unwrap().into_inner()
    }

    fn raw_message_with_zip_attachments(attachments: &[(&str, &[u8])]) -> RawMessage {
        let boundary = "aggregate-zip-budget-boundary";
        let mut message = format!(
            "From: billing@example.com\r\n\
             To: finance@example.com\r\n\
             Message-ID: <aggregate-zip@example.com>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\
             \r\n"
        )
        .into_bytes();
        for (file_name, bytes) in attachments {
            message.extend_from_slice(
                format!(
                    "--{boundary}\r\n\
                     Content-Type: application/zip; name=\"{file_name}\"\r\n\
                     Content-Disposition: attachment; filename=\"{file_name}\"\r\n\
                     Content-Transfer-Encoding: binary\r\n\
                     \r\n"
                )
                .as_bytes(),
            );
            message.extend_from_slice(bytes);
            message.extend_from_slice(b"\r\n");
        }
        message.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        RawMessage {
            uid: 9,
            mailbox: "INBOX".to_owned(),
            raw: message,
            received_at: Utc::now(),
        }
    }

    fn eocd_offset(bytes: &[u8]) -> usize {
        bytes
            .windows(4)
            .rposition(|window| window == b"PK\x05\x06")
            .unwrap()
    }

    fn set_eocd_entry_count(bytes: &mut [u8], count: u16) {
        let eocd = eocd_offset(bytes);
        bytes[eocd + 8..eocd + 10].copy_from_slice(&count.to_le_bytes());
        bytes[eocd + 10..eocd + 12].copy_from_slice(&count.to_le_bytes());
    }

    fn append_fake_eocd(bytes: &mut Vec<u8>, count: u16) {
        let mut eocd = [0_u8; 22];
        eocd[..4].copy_from_slice(b"PK\x05\x06");
        eocd[8..10].copy_from_slice(&count.to_le_bytes());
        eocd[10..12].copy_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&eocd);
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

        let mut budget = ZipExpansionBudget::default();
        let expanded = expand_zip_part(&part, &mut budget).unwrap();

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
    fn zip_entry_budget_is_shared_across_message_attachments() {
        let first_zip = zip_with_pdf_entries("first", 33, b"a");
        let second_zip = zip_with_pdf_entries("second", 33, b"b");
        let raw = raw_message_with_zip_attachments(&[
            ("first.zip", &first_zip),
            ("second.zip", &second_zip),
        ]);

        let parsed = parse_invoice_parts(&raw).unwrap();
        let originals = parsed
            .files
            .iter()
            .filter(|file| file.file_name.ends_with(".zip"))
            .map(|file| file.file_name.as_str())
            .collect::<Vec<_>>();
        let expanded = parsed
            .files
            .iter()
            .filter(|file| file.file_name.ends_with(".pdf"))
            .collect::<Vec<_>>();

        assert_eq!(originals, vec!["first.zip", "second.zip"]);
        assert_eq!(expanded.len(), 33);
        assert!(
            expanded
                .iter()
                .all(|file| file.file_name.starts_with("first-"))
        );
    }

    #[test]
    fn zip_byte_budget_is_shared_across_message_attachments() {
        let first_zip = zip_with_pdf_entries("first", 1, b"aa");
        let second_zip = zip_with_pdf_entries("second", 1, b"bb");
        let raw = raw_message_with_zip_attachments(&[
            ("first.zip", &first_zip),
            ("second.zip", &second_zip),
        ]);

        let parsed = parse_invoice_parts_with_zip_budget(
            &raw,
            ZipExpansionBudget {
                remaining_entries: 2,
                remaining_bytes: 3,
            },
        )
        .unwrap();
        let originals = parsed
            .files
            .iter()
            .filter(|file| file.file_name.ends_with(".zip"))
            .map(|file| file.file_name.as_str())
            .collect::<Vec<_>>();
        let expanded = parsed
            .files
            .iter()
            .filter(|file| file.file_name.ends_with(".pdf"))
            .collect::<Vec<_>>();

        assert_eq!(originals, vec!["first.zip", "second.zip"]);
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].file_name, "first-0.pdf");
    }

    #[test]
    fn zip64_entry_metadata_is_rejected_while_originals_are_preserved() {
        let mut sentinel_zip = zip_with_pdf_entries("sentinel", 1, b"pdf");
        set_eocd_entry_count(&mut sentinel_zip, u16::MAX);
        let mut locator_zip = zip_with_pdf_entries("locator", 1, b"pdf");
        let eocd = eocd_offset(&locator_zip);
        let mut locator = [0_u8; 20];
        locator[..4].copy_from_slice(b"PK\x06\x07");
        locator_zip.splice(eocd..eocd, locator);

        assert!(preflight_zip_entries(&sentinel_zip).is_err());
        assert!(preflight_zip_entries(&locator_zip).is_err());
        let raw = raw_message_with_zip_attachments(&[
            ("sentinel.zip", &sentinel_zip),
            ("locator.zip", &locator_zip),
        ]);

        let parsed = parse_invoice_parts(&raw).unwrap();

        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].file_name, "sentinel.zip");
        assert_eq!(parsed.files[0].bytes, sentinel_zip);
        assert_eq!(parsed.files[1].file_name, "locator.zip");
        assert_eq!(parsed.files[1].bytes, locator_zip);
    }

    #[test]
    fn zip_entry_budget_is_reserved_before_archive_parser_runs() {
        let first_zip = zip_with_pdf_entries("first", 1, b"pdf");
        let mut second_zip = zip_with_pdf_entries("second", 1, b"pdf");
        set_eocd_entry_count(&mut second_zip, 2);
        let central_directory = second_zip
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .unwrap();
        second_zip[central_directory] = b'X';
        let first = InvoicePart {
            part_id: "1".to_owned(),
            file_name: "first.zip".to_owned(),
            bytes: first_zip,
            message_id: None,
        };
        let second = InvoicePart {
            part_id: "2".to_owned(),
            file_name: "second.zip".to_owned(),
            bytes: second_zip,
            message_id: None,
        };
        let mut budget = ZipExpansionBudget {
            remaining_entries: 2,
            remaining_bytes: 10,
        };

        assert!(expand_zip_part(&first, &mut budget).is_ok());
        assert_eq!(budget.remaining_entries, 1);
        assert!(expand_zip_part(&second, &mut budget).is_err());
        assert_eq!(budget.remaining_entries, 0);
    }

    #[test]
    fn ambiguous_classic_eocd_is_rejected_before_budget_reservation() {
        let mut ambiguous_zip = zip_with_pdf_entries("invoice", 1, b"pdf");
        append_fake_eocd(&mut ambiguous_zip, 1);

        assert!(preflight_zip_entries(&ambiguous_zip).is_err());
        let part = InvoicePart {
            part_id: "1".to_owned(),
            file_name: "ambiguous.zip".to_owned(),
            bytes: ambiguous_zip.clone(),
            message_id: None,
        };
        let mut budget = ZipExpansionBudget::default();
        assert!(expand_zip_part(&part, &mut budget).is_err());
        assert_eq!(budget.remaining_entries, 64);
        let raw = raw_message_with_zip_attachments(&[("ambiguous.zip", &ambiguous_zip)]);

        let parsed = parse_invoice_parts(&raw).unwrap();

        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].file_name, "ambiguous.zip");
        assert_eq!(parsed.files[0].bytes, ambiguous_zip);
    }

    #[test]
    fn zip64_signatures_are_rejected_at_any_archive_offset() {
        let zip64_eocd = zip_with_pdf_entries("beforePK\x06\x06after", 1, b"pdf");
        let zip64_locator = zip_with_pdf_entries("beforePK\x06\x07after", 1, b"pdf");
        assert!(zip64_eocd.windows(4).any(|window| window == b"PK\x06\x06"));
        assert!(
            zip64_locator
                .windows(4)
                .any(|window| window == b"PK\x06\x07")
        );

        assert!(preflight_zip_entries(&zip64_eocd).is_err());
        assert!(preflight_zip_entries(&zip64_locator).is_err());
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
