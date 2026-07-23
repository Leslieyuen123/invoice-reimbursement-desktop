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
use crate::infra::invoice_download::{
    InvoiceLinkDownload, InvoiceLinkDownloader, SecureInvoiceLinkDownloader,
};
use crate::services::import::{EmailImportSource, ImportOutcome, ImportService};
use crate::services::recognition::RecognitionService;

const MAX_MESSAGES_PER_SYNC: usize = 1_000;
const MAX_RANGE_SYNC_PAGES: usize = 64;
const MAX_TOUCHED_ITEM_IDS: usize = MAX_MESSAGES_PER_SYNC * MAX_RANGE_SYNC_PAGES;
const MAX_RAW_MESSAGE_BYTES: usize = 50 * 1024 * 1024;
const MAX_TOTAL_RAW_BYTES: usize = 200 * 1024 * 1024;
const MAX_PARTS_PER_MESSAGE: usize = 256;
const MAX_DOWNLOAD_LINKS_PER_MESSAGE: usize = 32;
const MAX_DOWNLOAD_URL_LENGTH: usize = 2_048;
const MAX_ZIP_ENTRIES: usize = 64;
const MAX_ZIP_ENTRY_BYTES: u64 = 50 * 1024 * 1024;
const MAX_ZIP_TOTAL_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct TouchedItemBudget {
    remaining: usize,
}

impl TouchedItemBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self { remaining: limit }
    }

    pub(crate) fn ensure_available(&self) -> Result<(), AppError> {
        if self.remaining == 0 {
            return Err(touched_item_limit_error());
        }
        Ok(())
    }

    fn reserve(&mut self) -> Result<(), AppError> {
        self.ensure_available()?;
        self.remaining -= 1;
        Ok(())
    }
}

impl Default for TouchedItemBudget {
    fn default() -> Self {
        Self::new(MAX_TOUCHED_ITEM_IDS)
    }
}

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SyncProgress {
    pub(crate) imported_count: u32,
    pub(crate) touched_item_ids: HashSet<Uuid>,
}

impl SyncProgress {
    fn into_result(self) -> SyncResult {
        SyncResult {
            imported_count: self.imported_count,
        }
    }

    pub(crate) fn merge(&mut self, other: Self) -> Result<(), AppError> {
        self.merge_with_limit(other, MAX_TOUCHED_ITEM_IDS)
    }

    fn merge_with_limit(&mut self, other: Self, limit: usize) -> Result<(), AppError> {
        let imported_count = self
            .imported_count
            .checked_add(other.imported_count)
            .ok_or_else(sync_import_count_overflow)?;
        if self.touched_item_ids.len() > limit {
            return Err(touched_item_limit_error());
        }
        let remaining = limit - self.touched_item_ids.len();
        let new_unique_count = other
            .touched_item_ids
            .iter()
            .filter(|id| !self.touched_item_ids.contains(id))
            .take(remaining.saturating_add(1))
            .count();
        if new_unique_count > remaining {
            return Err(touched_item_limit_error());
        }

        self.imported_count = imported_count;
        self.touched_item_ids.extend(other.touched_item_ids);
        Ok(())
    }

    fn record_item(&mut self, item_id: Uuid, imported: bool) -> Result<(), AppError> {
        self.record_item_with_limit(item_id, imported, MAX_TOUCHED_ITEM_IDS)
    }

    fn record_item_with_limit(
        &mut self,
        item_id: Uuid,
        imported: bool,
        limit: usize,
    ) -> Result<(), AppError> {
        let imported_count = if imported {
            self.imported_count
                .checked_add(1)
                .ok_or_else(sync_import_count_overflow)?
        } else {
            self.imported_count
        };
        if !self.touched_item_ids.contains(&item_id) && self.touched_item_ids.len() >= limit {
            return Err(touched_item_limit_error());
        }

        self.imported_count = imported_count;
        self.touched_item_ids.insert(item_id);
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SyncProgressFailure {
    pub(crate) error: AppError,
    pub(crate) completed: SyncProgress,
}

impl SyncProgressFailure {
    fn empty(error: AppError) -> Self {
        Self {
            error,
            completed: SyncProgress::default(),
        }
    }
}

#[derive(Clone)]
pub struct SyncService {
    gateway: Arc<dyn ImapGateway>,
    credentials: Arc<dyn CredentialStore>,
    accounts: MailboxAccountRepository,
    import: ImportService,
    recognition: RecognitionService,
    link_downloader: Arc<dyn InvoiceLinkDownloader>,
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
            link_downloader: Arc::new(SecureInvoiceLinkDownloader),
        }
    }

    pub fn with_link_downloader(mut self, downloader: Arc<dyn InvoiceLinkDownloader>) -> Self {
        self.link_downloader = downloader;
        self
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
        self.run_range_with_progress(account_id, start_date, end_date)
            .await
            .map(SyncProgress::into_result)
            .map_err(|failure| failure.error)
    }

    pub(crate) async fn run_range_with_progress(
        &self,
        account_id: Uuid,
        start_date: NaiveDate,
        end_date: NaiveDate,
    ) -> Result<SyncProgress, SyncProgressFailure> {
        let mut budget = TouchedItemBudget::default();
        self.run_range_with_progress_and_budget(account_id, start_date, end_date, &mut budget)
            .await
    }

    pub(crate) async fn run_range_with_progress_and_budget(
        &self,
        account_id: Uuid,
        start_date: NaiveDate,
        end_date: NaiveDate,
        budget: &mut TouchedItemBudget,
    ) -> Result<SyncProgress, SyncProgressFailure> {
        if start_date > end_date {
            return Err(SyncProgressFailure::empty(AppError::validation(
                "dateRange",
                "IMAP start date must not be after end date",
            )));
        }
        let end_exclusive = end_date
            .succ_opt()
            .ok_or_else(|| {
                AppError::validation("dateRange", "IMAP end date is outside the supported range")
            })
            .map_err(SyncProgressFailure::empty)?;
        let range =
            ImapDateRange::new(start_date, end_exclusive).map_err(SyncProgressFailure::empty)?;
        let run = self
            .accounts
            .begin_sync_run(account_id)
            .await
            .map_err(SyncProgressFailure::empty)?;
        let account = match self.accounts.get(account_id).await {
            Ok(account) => account,
            Err(error) => {
                let sanitized = sanitize_error(&error.to_string(), "");
                self.accounts
                    .finish_sync_failure(&run, &sanitized)
                    .await
                    .map_err(SyncProgressFailure::empty)?;
                return Err(SyncProgressFailure::empty(sanitized_external_error(
                    &error, sanitized,
                )));
            }
        };
        let secret = match get_credential(self.credentials.clone(), account_id.to_string()).await {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                let error = authentication_error("mailbox credential is unavailable");
                self.accounts
                    .finish_sync_failure(&run, &error.to_string())
                    .await
                    .map_err(SyncProgressFailure::empty)?;
                return Err(SyncProgressFailure::empty(error));
            }
            Err(error) => {
                self.accounts
                    .finish_sync_failure(&run, &error.to_string())
                    .await
                    .map_err(SyncProgressFailure::empty)?;
                return Err(SyncProgressFailure::empty(error));
            }
        };
        let config = ImapAccountConfig::from_account(&account);
        let result = self
            .run_range_deltas(account_id, &config, &secret, range, budget)
            .await;
        match result {
            Ok(result) => {
                if let Err(error) = self
                    .accounts
                    .finish_range_sync_success(&run, result.imported_count)
                    .await
                {
                    let sanitized = sanitize_error(&error.to_string(), &secret);
                    if let Err(record_error) =
                        self.accounts.finish_sync_failure(&run, &sanitized).await
                    {
                        return Err(SyncProgressFailure {
                            error: record_error,
                            completed: result,
                        });
                    }
                    return Err(SyncProgressFailure {
                        error: sanitized_external_error(&error, sanitized),
                        completed: result,
                    });
                }
                Ok(result)
            }
            Err(failure) => {
                let SyncProgressFailure { error, completed } = failure;
                let sanitized = sanitize_error(&error.to_string(), &secret);
                if let Err(record_error) = self.accounts.finish_sync_failure(&run, &sanitized).await
                {
                    return Err(SyncProgressFailure {
                        error: record_error,
                        completed,
                    });
                }
                Err(SyncProgressFailure {
                    error: sanitized_external_error(&error, sanitized),
                    completed,
                })
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
        let mut budget = TouchedItemBudget::default();
        let result = Box::pin(self.process_delta(account_id, &delta, rescan, &mut budget))
            .await
            .map_err(|failure| failure.error)?
            .into_result();
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
        budget: &mut TouchedItemBudget,
    ) -> Result<SyncProgress, SyncProgressFailure> {
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut page_count = 0_usize;
        let mut completed = SyncProgress::default();
        loop {
            if page_count >= MAX_RANGE_SYNC_PAGES {
                return Err(sync_progress_failure(
                    external_error("IMAP range scan exceeded page limit"),
                    completed,
                ));
            }
            page_count += 1;
            let delta = self
                .gateway
                .fetch_range(config, secret, cursor, range)
                .await
                .map_err(|error| sync_progress_failure(error, completed.clone()))?;
            validate_delta(&delta, &config.mailbox)
                .map_err(|error| sync_progress_failure(error, completed.clone()))?;
            let next_cursor = SyncCursor {
                uid_validity: delta.uid_validity,
                last_uid: delta.highest_uid,
            };
            let finished = cursor == Some(next_cursor);
            let cursor_key = (next_cursor.uid_validity, next_cursor.last_uid);
            if !finished
                && (seen_cursors.contains(&cursor_key)
                    || cursor.is_some_and(|cursor: SyncCursor| {
                        cursor.uid_validity == next_cursor.uid_validity
                            && next_cursor.last_uid < cursor.last_uid
                    }))
            {
                return Err(sync_progress_failure(
                    external_error("IMAP range scan did not make forward progress"),
                    completed,
                ));
            }
            match Box::pin(self.process_delta(account_id, &delta, true, budget)).await {
                Ok(page) => {
                    if let Err(error) = completed.merge(page) {
                        return Err(sync_progress_failure(error, completed));
                    }
                }
                Err(failure) => {
                    let SyncProgressFailure {
                        error,
                        completed: page,
                    } = failure;
                    if let Err(merge_error) = completed.merge(page) {
                        return Err(sync_progress_failure(merge_error, completed));
                    }
                    return Err(sync_progress_failure(error, completed));
                }
            }
            if finished {
                return Ok(completed);
            }
            seen_cursors.insert(cursor_key);
            cursor = Some(next_cursor);
        }
    }

    async fn process_delta(
        &self,
        account_id: Uuid,
        delta: &MailboxDelta,
        rescan: bool,
        budget: &mut TouchedItemBudget,
    ) -> Result<SyncProgress, SyncProgressFailure> {
        let mut completed = SyncProgress::default();
        for rejected in &delta.rejected_messages {
            budget
                .reserve()
                .map_err(|error| sync_progress_failure(error, completed.clone()))?;
            let outcome = self
                .import
                .import_email_rejection(account_id, delta.uid_validity, rejected, rescan)
                .await
                .map_err(|error| sync_progress_failure(error, completed.clone()))?;
            track_import_outcome(outcome, &mut completed)
                .map_err(|error| sync_progress_failure(error, completed.clone()))?;
        }
        for raw_message in &delta.messages {
            let parsed = parse_invoice_parts(raw_message)
                .map_err(|error| sync_progress_failure(error, completed.clone()))?;
            let ParsedInvoiceParts {
                files,
                links,
                mut zip_budget,
            } = parsed;
            for part in files {
                budget
                    .reserve()
                    .map_err(|error| sync_progress_failure(error, completed.clone()))?;
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
                    .await
                    .map_err(|error| sync_progress_failure(error, completed.clone()))?;
                self.track_and_recognize(outcome, &mut completed)
                    .await
                    .map_err(|error| sync_progress_failure(error, completed.clone()))?;
            }
            for link in links {
                Box::pin(self.process_link(
                    account_id,
                    delta.uid_validity,
                    raw_message,
                    rescan,
                    link,
                    budget,
                    &mut zip_budget,
                    &mut completed,
                ))
                .await
                .map_err(|error| sync_progress_failure(error, completed.clone()))?;
            }
        }
        Ok(completed)
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_link(
        &self,
        account_id: Uuid,
        uid_validity: u32,
        raw_message: &RawMessage,
        rescan: bool,
        link: DownloadLink,
        budget: &mut TouchedItemBudget,
        zip_budget: &mut ZipExpansionBudget,
        completed: &mut SyncProgress,
    ) -> Result<(), AppError> {
        budget.reserve()?;
        let source = EmailImportSource {
            account_id,
            mailbox: raw_message.mailbox.clone(),
            uid_validity,
            uid: raw_message.uid,
            message_id: link.message_id,
            part_id: link.part_id,
            received_at: raw_message.received_at,
            rescan,
        };
        if let Some(existing) = self.import.find_email_part(&source).await?
            && !is_email_link_placeholder(&existing)
        {
            self.track_and_recognize(ImportOutcome::Existing(existing), completed)
                .await?;
            return Ok(());
        }
        let download = match self.link_downloader.download(&link.url).await {
            Ok(download) => download,
            Err(_) => {
                let outcome = self
                    .import
                    .import_failed_email_link(&link.url, source)
                    .await?;
                track_import_outcome(outcome, completed)?;
                return Ok(());
            }
        };
        match download {
            InvoiceLinkDownload::Downloaded(downloaded) => {
                let downloaded_part = InvoicePart {
                    part_id: source.part_id.clone(),
                    file_name: downloaded.file_name,
                    bytes: downloaded.bytes,
                    message_id: source.message_id.clone(),
                };
                let expanded = if file_name_has_extension(&downloaded_part.file_name, "zip") {
                    expand_zip_part(&downloaded_part, zip_budget).unwrap_or_default()
                } else {
                    Vec::new()
                };
                let outcome = self
                    .import
                    .import_downloaded_email_link(
                        &downloaded_part.file_name,
                        &downloaded_part.bytes,
                        source,
                    )
                    .await?;
                self.track_and_recognize(outcome, completed).await?;
                for child in expanded {
                    budget.reserve()?;
                    let outcome = self
                        .import
                        .import_email_bytes(
                            &child.file_name,
                            &child.bytes,
                            EmailImportSource {
                                account_id,
                                mailbox: raw_message.mailbox.clone(),
                                uid_validity,
                                uid: raw_message.uid,
                                message_id: child.message_id,
                                part_id: child.part_id,
                                received_at: raw_message.received_at,
                                rescan,
                            },
                        )
                        .await?;
                    self.track_and_recognize(outcome, completed).await?;
                }
            }
            InvoiceLinkDownload::Ignored => {
                self.import.discard_email_link_placeholder(&source).await?;
            }
        }
        Ok(())
    }

    async fn track_and_recognize(
        &self,
        outcome: ImportOutcome,
        completed: &mut SyncProgress,
    ) -> Result<(), AppError> {
        let item = track_import_outcome(outcome, completed)?;
        if item.recognition_status == RecognitionStatus::Pending
            && item.confirmation_status == ConfirmationStatus::Pending
            && let Err(error) = self.recognition.recognize_item(item.id).await
            && !is_document_recognition_failure(&error)
        {
            return Err(error);
        }
        Ok(())
    }
}

fn sync_progress_failure(error: AppError, completed: SyncProgress) -> SyncProgressFailure {
    SyncProgressFailure { error, completed }
}

fn track_import_outcome(
    outcome: ImportOutcome,
    completed: &mut SyncProgress,
) -> Result<crate::db::items::InvoiceItem, AppError> {
    let (item, imported) = match outcome {
        ImportOutcome::New(item) => (item, true),
        ImportOutcome::Existing(item) => (item, false),
    };
    completed.record_item(item.id, imported)?;
    Ok(item)
}

fn is_email_link_placeholder(item: &crate::db::items::InvoiceItem) -> bool {
    item.mime_type == "text/uri-list"
        && item
            .original_name
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("url"))
}

fn sync_import_count_overflow() -> AppError {
    AppError::Internal {
        message: "sync import count overflow".to_owned(),
    }
}

fn touched_item_limit_error() -> AppError {
    AppError::External {
        service: "imap".to_owned(),
        retryable: true,
        message: "IMAP sync touched-item limit exceeded; retry with a narrower date range"
            .to_owned(),
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
    zip_budget: ZipExpansionBudget,
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
    Ok(ParsedInvoiceParts {
        files,
        links,
        zip_budget,
    })
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
mod tests;
