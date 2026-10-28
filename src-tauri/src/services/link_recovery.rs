//! Repairs invoices that were stored as a failed mail-link placeholder.
//!
//! When a mail body links to the invoice instead of attaching it, the sync
//! pipeline downloads the link and imports the file directly. If that download
//! fails, the pipeline stores a `.url` placeholder whose content is the link
//! itself, and the invoice sits in the library without an amount forever.
//!
//! This service gives those placeholders another chance after the fact: read
//! the URL out of the stored placeholder, download it through the existing
//! [`InvoiceLinkDownloader`] (same HTTPS/domain/signature rules), replace the
//! original through the import-side staging/promote path, and re-run
//! recognition on the downloaded file.
//!
//! Real libraries are dominated by marketing mail, so the action is
//! deliberately conservative and cheap:
//!
//! * only `.url` placeholders without an amount, outside a batch, marked
//!   unique and not already dismissed are examined;
//! * a link is only requested when its host is a known e-invoice portal — a
//!   marketing or tracking link is reported as such without a network request;
//! * placeholders that can never become invoices (non-portal host, login wall,
//!   HTML page) are marked as non-invoice files so they leave the work queues;
//! * a temporary failure (network, server) changes nothing and stays retryable;
//! * once a real invoice replaces the placeholder the row stops being a
//!   candidate, which makes running the action again a no-op.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use url::Url;
use uuid::Uuid;

use crate::db::items::{
    InvoiceItem, ItemRepository, RecoveryOriginalKind, is_link_placeholder_original_name,
};
use crate::domain::error::AppError;
use crate::domain::model::{DedupeStatus, RecognitionStatus};
use crate::infra::file_signature;
use crate::infra::files::{AppPaths, open_contained_regular_file};
use crate::infra::invoice_download::{
    DownloadFailureKind, InvoiceLinkDownload, InvoiceLinkDownloader, download_failure,
    is_downloadable_url, is_known_invoice_portal,
};
use crate::services::qr_recovery::replace_original_with_download;
use crate::services::recognition::RecognitionService;

/// The stored placeholder is a link list; a real one is a single short URL.
const MAX_PLACEHOLDER_BYTES: u64 = 2 * 1024;
/// Candidates read per page while collecting the work list.
const CANDIDATE_PAGE_SIZE: usize = 200;

/// Stable skip codes reported to the UI, in this order.
pub const LINK_SKIP_MISSING_URL: &str = "missing_url";
pub const LINK_SKIP_INVALID_URL: &str = "invalid_url";
pub const LINK_SKIP_NOT_INVOICE_PORTAL: &str = "not_invoice_portal";
pub const LINK_SKIP_REQUIRES_LOGIN: &str = "requires_login";
pub const LINK_SKIP_EXPIRED: &str = "expired";
pub const LINK_SKIP_DOWNLOAD_FAILED: &str = "download_failed";
pub const LINK_SKIP_NOT_INVOICE: &str = "not_invoice";
pub const LINK_SKIP_REPLACE_FAILED: &str = "replace_failed";
pub const LINK_SKIP_RECOGNIZED_FAILED: &str = "recognized_failed";

/// What one link rescue run did.
///
/// `examined` is always the sum of `recognized` and `skipped`: every examined
/// placeholder either ended fully rescued or was skipped for a reported reason.
/// `downloaded` counts replacements and `archived` counts placeholders that were
/// marked as non-invoice files during the run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkRecoveryReport {
    /// Placeholders the run looked at.
    pub examined: u32,
    /// Placeholders whose original was replaced by a downloaded invoice file.
    pub downloaded: u32,
    /// Placeholders that ended recognized after the replacement.
    pub recognized: u32,
    /// Placeholders that could not be rescued; see [`Self::skip_reasons`].
    pub skipped: u32,
    /// Placeholders marked as non-invoice files during the run.
    pub archived: u32,
    /// Placeholders skipped because the host is not an invoice portal.
    pub not_invoice_portal: u32,
    /// Skip counts by stable code, sorted by code.
    pub skip_reasons: Vec<LinkRecoverySkip>,
}

/// One skip-reason bucket of a link rescue run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRecoverySkip {
    pub code: String,
    pub count: u32,
}

impl LinkRecoveryReport {
    fn record_skip(&mut self, code: &'static str) {
        self.skipped = self.skipped.saturating_add(1);
        if code == LINK_SKIP_NOT_INVOICE_PORTAL {
            self.not_invoice_portal = self.not_invoice_portal.saturating_add(1);
        }
        let mut counts: BTreeMap<String, u32> = self
            .skip_reasons
            .iter()
            .map(|entry| (entry.code.clone(), entry.count))
            .collect();
        let count = counts.entry(code.to_owned()).or_insert(0);
        *count = count.saturating_add(1);
        self.skip_reasons = counts
            .into_iter()
            .map(|(code, count)| LinkRecoverySkip { code, count })
            .collect();
    }
}

/// The link rescue for already-imported link placeholders.
#[derive(Clone)]
pub struct LinkRecoveryService {
    items: ItemRepository,
    recognition: RecognitionService,
    paths: AppPaths,
    downloader: Arc<dyn InvoiceLinkDownloader>,
}

impl LinkRecoveryService {
    pub fn new(
        items: ItemRepository,
        recognition: RecognitionService,
        paths: AppPaths,
        downloader: Arc<dyn InvoiceLinkDownloader>,
    ) -> Self {
        Self {
            items,
            recognition,
            paths,
            downloader,
        }
    }

    /// Rescues every eligible link placeholder, optionally capped by `limit`.
    ///
    /// Placeholders left out by an explicit `limit` are simply not examined, so
    /// the report always accounts for what it says it examined.
    pub async fn recover_all(&self, limit: Option<u32>) -> Result<LinkRecoveryReport, AppError> {
        if limit == Some(0) {
            return Err(AppError::validation("limit", "limit must be at least 1"));
        }
        let mut remaining_budget = limit.map_or(usize::MAX, |limit| limit as usize);
        let mut candidates: Vec<InvoiceItem> = Vec::new();
        let mut cursor = None;
        while remaining_budget > 0 {
            let page = self
                .items
                .list_link_recovery_candidates(cursor, CANDIDATE_PAGE_SIZE)
                .await?;
            let take = page.items.len().min(remaining_budget);
            remaining_budget -= take;
            let exhausted = take < page.items.len() || page.next_cursor.is_none();
            candidates.extend(page.items.into_iter().take(take));
            if exhausted {
                break;
            }
            cursor = page.next_cursor;
        }
        self.run(candidates).await
    }

    /// Rescues one explicitly chosen placeholder.
    ///
    /// Unlike the bulk action this reports why a specific invoice could not even
    /// be examined, because the user asked about that exact invoice.
    pub async fn recover_item(&self, id: Uuid) -> Result<LinkRecoveryReport, AppError> {
        let item = self.items.get_by_id(id).await?;
        if !is_candidate(&item) {
            return Err(AppError::validation(
                "item",
                "该票据不是可补救的链接票据（需要 .url 占位原件、尚未填写金额、不在批次中）",
            ));
        }
        self.run(vec![item]).await
    }

    async fn run(&self, candidates: Vec<InvoiceItem>) -> Result<LinkRecoveryReport, AppError> {
        let mut report = LinkRecoveryReport::default();
        for item in candidates {
            report.examined = report.examined.saturating_add(1);
            match self.rescue(&item, &mut report).await {
                None => report.recognized = report.recognized.saturating_add(1),
                Some(code) => {
                    tracing::info!(
                        item_id = %item.id,
                        reason = code,
                        "link rescue skipped an invoice"
                    );
                    report.record_skip(code);
                }
            }
        }
        Ok(report)
    }

    /// Runs the rescue stages for one placeholder, returning the skip code.
    async fn rescue(
        &self,
        item: &InvoiceItem,
        report: &mut LinkRecoveryReport,
    ) -> Option<&'static str> {
        let paths = self.paths.clone();
        let source = PathBuf::from(&item.original_path);
        let read = tokio::task::spawn_blocking(move || read_placeholder_url(&paths, &source))
            .await
            .map_err(|error| {
                tracing::warn!(
                    item_id = %item.id,
                    error = %error,
                    "link rescue reading task failed"
                );
            })
            .ok()?;
        let url = match read {
            Ok(url) => url,
            Err(code) => {
                tracing::warn!(
                    item_id = %item.id,
                    reason = code,
                    "link rescue could not read a usable link from the placeholder"
                );
                return Some(code);
            }
        };

        // A marketing or tracking link must not be requested at all: the real
        // library has ~200 of them among 237 placeholders.
        if !is_known_invoice_portal(&url) {
            self.archive(item, report).await;
            return Some(LINK_SKIP_NOT_INVOICE_PORTAL);
        }

        let downloaded = match self.downloader.download(url.as_str()).await {
            Ok(InvoiceLinkDownload::Downloaded(downloaded))
                if file_signature::detect(&downloaded.bytes).is_invoice_document() =>
            {
                downloaded
            }
            Ok(InvoiceLinkDownload::Downloaded(_)) => {
                self.archive(item, report).await;
                return Some(LINK_SKIP_NOT_INVOICE);
            }
            Ok(InvoiceLinkDownload::Ignored) => {
                self.archive(item, report).await;
                return Some(LINK_SKIP_NOT_INVOICE);
            }
            Err(error) => {
                let kind = download_failure(&error);
                tracing::warn!(
                    item_id = %item.id,
                    error = %error,
                    failure = ?kind,
                    "link rescue download failed"
                );
                return Some(match kind {
                    DownloadFailureKind::RequiresLogin => {
                        self.archive(item, report).await;
                        LINK_SKIP_REQUIRES_LOGIN
                    }
                    DownloadFailureKind::NotAnInvoice => {
                        self.archive(item, report).await;
                        LINK_SKIP_NOT_INVOICE
                    }
                    DownloadFailureKind::Expired => LINK_SKIP_EXPIRED,
                    DownloadFailureKind::Connection => LINK_SKIP_DOWNLOAD_FAILED,
                });
            }
        };

        match replace_original_with_download(
            &self.items,
            &self.paths,
            item,
            &downloaded,
            RecoveryOriginalKind::LinkPlaceholder,
        )
        .await
        {
            Ok(()) => report.downloaded = report.downloaded.saturating_add(1),
            Err(error) => {
                tracing::warn!(
                    item_id = %item.id,
                    error = %error,
                    "link rescue could not replace the stored placeholder"
                );
                return Some(LINK_SKIP_REPLACE_FAILED);
            }
        }

        match self.recognition.recognize_item(item.id).await {
            Ok(recognized) if recognized.recognition_status == RecognitionStatus::Succeeded => None,
            Ok(_) => Some(LINK_SKIP_RECOGNIZED_FAILED),
            Err(error) => {
                tracing::warn!(
                    item_id = %item.id,
                    error = %error,
                    "link rescue replaced the placeholder but recognition failed"
                );
                Some(LINK_SKIP_RECOGNIZED_FAILED)
            }
        }
    }

    /// Marks a placeholder that can never become an invoice (best effort).
    ///
    /// The row keeps its invoice fields and its file; only `non_invoice_reason`
    /// is written, which removes it from the work queues. A failure to mark is
    /// logged and does not change the reported skip reason.
    async fn archive(&self, item: &InvoiceItem, report: &mut LinkRecoveryReport) {
        match self.items.mark_link_placeholder_not_invoice(item.id).await {
            Ok(true) => report.archived = report.archived.saturating_add(1),
            Ok(false) => {}
            Err(error) => tracing::warn!(
                item_id = %item.id,
                error = %error,
                "link rescue could not mark the placeholder as a non-invoice file"
            ),
        }
    }
}

/// Whether this invoice is in scope for the link rescue.
fn is_candidate(item: &InvoiceItem) -> bool {
    item.amount_cents.is_none()
        && item.batch_id.is_none()
        && item.dedupe_status == DedupeStatus::Unique
        && item.non_invoice_reason.is_none()
        && is_link_placeholder_original_name(&item.original_name)
}

/// Reads the URL stored in a `.url` placeholder and validates it.
fn read_placeholder_url(paths: &AppPaths, source: &Path) -> Result<Url, &'static str> {
    let mut opened = open_contained_regular_file(source, &paths.originals, "originalPath")
        .map_err(|_| LINK_SKIP_INVALID_URL)?;
    if opened.length == 0 {
        return Err(LINK_SKIP_MISSING_URL);
    }
    let mut bytes =
        Vec::with_capacity(usize::try_from(opened.length.min(MAX_PLACEHOLDER_BYTES)).unwrap_or(0));
    Read::take(&mut opened.file, MAX_PLACEHOLDER_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| LINK_SKIP_INVALID_URL)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PLACEHOLDER_BYTES {
        return Err(LINK_SKIP_INVALID_URL);
    }
    extract_placeholder_url(&bytes)
}

/// The first non-comment line of a link list, when it is a downloadable URL.
fn extract_placeholder_url(bytes: &[u8]) -> Result<Url, &'static str> {
    let text = String::from_utf8_lossy(bytes);
    let Some(raw) = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
    else {
        return Err(LINK_SKIP_MISSING_URL);
    };
    let url = Url::parse(raw).map_err(|_| LINK_SKIP_INVALID_URL)?;
    // The downloader only accepts HTTPS public hosts; rejecting the rest here
    // reports "invalid link" instead of a vague download failure.
    if !is_downloadable_url(&url) {
        return Err(LINK_SKIP_INVALID_URL);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::{
        LINK_SKIP_INVALID_URL, LINK_SKIP_MISSING_URL, LINK_SKIP_NOT_INVOICE_PORTAL,
        LinkRecoveryReport, LinkRecoverySkip, extract_placeholder_url,
    };

    #[test]
    fn skip_counts_are_grouped_and_stable() {
        let mut report = LinkRecoveryReport::default();
        report.record_skip(LINK_SKIP_INVALID_URL);
        report.record_skip(LINK_SKIP_NOT_INVOICE_PORTAL);
        report.record_skip(LINK_SKIP_INVALID_URL);

        assert_eq!(report.skipped, 3);
        assert_eq!(report.not_invoice_portal, 1);
        assert_eq!(
            report.skip_reasons,
            vec![
                LinkRecoverySkip {
                    code: LINK_SKIP_INVALID_URL.to_owned(),
                    count: 2,
                },
                LinkRecoverySkip {
                    code: LINK_SKIP_NOT_INVOICE_PORTAL.to_owned(),
                    count: 1,
                },
            ]
        );
    }

    #[test]
    fn placeholder_content_must_be_one_downloadable_https_url() {
        let url = extract_placeholder_url(b"https://nnfp.jss.com.cn/5TQY1_XFJO-11SlG\r\n").unwrap();
        assert_eq!(url.as_str(), "https://nnfp.jss.com.cn/5TQY1_XFJO-11SlG");

        // Blank content has no link at all.
        assert_eq!(
            extract_placeholder_url(b"  \r\n\r\n").unwrap_err(),
            LINK_SKIP_MISSING_URL
        );
        assert_eq!(
            extract_placeholder_url(b"# comment only\n").unwrap_err(),
            LINK_SKIP_MISSING_URL
        );

        // Non-URL content, non-HTTPS URLs and credential URLs are invalid.
        for content in [
            b"not a link\r\n".as_slice(),
            b"http://nnfp.jss.com.cn/5TQY1\r\n".as_slice(),
            b"https://user:secret@nnfp.jss.com.cn/5TQY1\r\n".as_slice(),
            b"file:///tmp/invoice.pdf\r\n".as_slice(),
        ] {
            assert_eq!(
                extract_placeholder_url(content).unwrap_err(),
                LINK_SKIP_INVALID_URL,
                "unexpected result for {:?}",
                String::from_utf8_lossy(content)
            );
        }
    }
}
