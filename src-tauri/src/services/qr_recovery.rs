//! Repairs invoices that were stored as an image carrying a QR code.
//!
//! Chinese e-invoices are frequently delivered as an image whose QR code links
//! to the real file instead of attaching it. The sync pipeline already follows
//! such links for *new* mail attachments, but invoices that were imported before
//! that path existed sit in the library with no amount and cannot be rescued by
//! re-running recognition: the image itself simply has no amount.
//!
//! This service gives those stored invoices the same treatment after the fact:
//! decode the QR code of the stored image, follow the link through the existing
//! [`InvoiceLinkDownloader`] (same HTTPS/domain/signature rules), replace the
//! original with the downloaded invoice, and re-run recognition on it.
//!
//! The action is intentionally conservative:
//!
//! * only JPG/PNG originals without an amount, outside a batch, and not marked
//!   as a duplicate or non-invoice file are examined;
//! * a failed download never touches the stored invoice;
//! * the replacement reuses the import-side staging mechanism, so the original
//!   file swap is durable and the database row is updated transactionally;
//! * once the original is a PDF the invoice stops being a candidate, which makes
//!   running the action again a no-op instead of a repeated download.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::db::items::{
    EmailFileReplacement, InvoiceItem, ItemRepository, RecoveryOriginalKind, is_image_original_name,
};
use crate::domain::error::AppError;
use crate::domain::model::{DedupeStatus, RecognitionStatus};
use crate::infra::file_signature;
use crate::infra::files::{AppPaths, StagedOriginal, open_contained_regular_file};
use crate::infra::invoice_download::{
    DownloadedInvoice, InvoiceLinkDownload, InvoiceLinkDownloader,
};
use crate::infra::qr;
use crate::services::import::MAX_FILE_SIZE;
use crate::services::recognition::RecognitionService;

/// QR links followed for one stored image.
const MAX_QR_LINKS_PER_ITEM: usize = 2;
/// Candidates read per page while collecting the work list.
const CANDIDATE_PAGE_SIZE: usize = 200;

/// Stable skip codes reported to the UI, in this order.
pub const QR_SKIP_NO_CODE: &str = "no_qr_code";
pub const QR_SKIP_NOT_INVOICE_LINK: &str = "not_invoice_link";
pub const QR_SKIP_DOWNLOAD_FAILED: &str = "download_failed";
pub const QR_SKIP_UNREADABLE_ORIGINAL: &str = "unreadable_original";
pub const QR_SKIP_REPLACEMENT_FAILED: &str = "replacement_failed";
pub const QR_SKIP_ITEM_CHANGED: &str = "item_changed";
pub const QR_SKIP_RECOGNITION_FAILED: &str = "recognition_failed";

/// What one QR rescue run did.
///
/// `examined` is always the sum of `recognized` and `skipped`: every examined
/// invoice either ended fully rescued or was skipped for a reported reason.
/// `decoded` and `downloaded` are sub-counts of the rescue stages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QrRecoveryReport {
    /// Image invoices the run looked at.
    pub examined: u32,
    /// Invoices whose QR code carried at least one HTTPS link.
    pub decoded: u32,
    /// Invoices whose original was replaced by a downloaded invoice file.
    pub downloaded: u32,
    /// Invoices that ended recognized after the replacement.
    pub recognized: u32,
    /// Invoices that could not be rescued; see [`Self::skip_reasons`].
    pub skipped: u32,
    /// Skip counts by stable code, sorted by code.
    pub skip_reasons: Vec<QrRecoverySkip>,
}

/// One skip-reason bucket of a QR rescue run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrRecoverySkip {
    pub code: String,
    pub count: u32,
}

impl QrRecoveryReport {
    fn record_skip(&mut self, code: &'static str) {
        self.skipped = self.skipped.saturating_add(1);
        let mut counts: BTreeMap<String, u32> = self
            .skip_reasons
            .iter()
            .map(|entry| (entry.code.clone(), entry.count))
            .collect();
        let count = counts.entry(code.to_owned()).or_insert(0);
        *count = count.saturating_add(1);
        self.skip_reasons = counts
            .into_iter()
            .map(|(code, count)| QrRecoverySkip { code, count })
            .collect();
    }
}

/// The QR rescue for already-imported invoices.
#[derive(Clone)]
pub struct QrRecoveryService {
    items: ItemRepository,
    recognition: RecognitionService,
    paths: AppPaths,
    downloader: Arc<dyn InvoiceLinkDownloader>,
}

impl QrRecoveryService {
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

    /// Rescues every eligible image invoice, optionally capped by `limit`.
    ///
    /// Invoices left out by an explicit `limit` are simply not examined, so the
    /// report always accounts for what it says it examined.
    pub async fn recover_all(&self, limit: Option<u32>) -> Result<QrRecoveryReport, AppError> {
        if limit == Some(0) {
            return Err(AppError::validation("limit", "limit must be at least 1"));
        }
        let mut remaining_budget = limit.map_or(usize::MAX, |limit| limit as usize);
        let mut candidates: Vec<InvoiceItem> = Vec::new();
        let mut cursor = None;
        while remaining_budget > 0 {
            let page = self
                .items
                .list_qr_recovery_candidates(cursor, CANDIDATE_PAGE_SIZE)
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

    /// Rescues one explicitly chosen invoice.
    ///
    /// Unlike the bulk action this reports why a specific invoice could not even
    /// be examined, because the user asked about that exact invoice.
    pub async fn recover_item(&self, id: Uuid) -> Result<QrRecoveryReport, AppError> {
        let item = self.items.get_by_id(id).await?;
        if !is_candidate(&item) {
            return Err(AppError::validation(
                "item",
                "该票据不是可补救的二维码图片票据（需要 JPG/PNG 原件、尚未填写金额、不在批次中）",
            ));
        }
        self.run(vec![item]).await
    }

    async fn run(&self, candidates: Vec<InvoiceItem>) -> Result<QrRecoveryReport, AppError> {
        let mut report = QrRecoveryReport::default();
        for item in candidates {
            report.examined = report.examined.saturating_add(1);
            match self.rescue(&item, &mut report).await {
                None => report.recognized = report.recognized.saturating_add(1),
                Some(code) => {
                    tracing::info!(
                        item_id = %item.id,
                        reason = code,
                        "QR rescue skipped an invoice"
                    );
                    report.record_skip(code);
                }
            }
        }
        Ok(report)
    }

    /// Runs the rescue stages for one invoice, returning the skip code on failure.
    async fn rescue(
        &self,
        item: &InvoiceItem,
        report: &mut QrRecoveryReport,
    ) -> Option<&'static str> {
        let paths = self.paths.clone();
        let source = PathBuf::from(&item.original_path);
        let decoded = tokio::task::spawn_blocking(move || read_and_decode(&paths, &source))
            .await
            .map_err(|error| {
                tracing::warn!(
                    item_id = %item.id,
                    error = %error,
                    "QR rescue decoding task failed"
                );
            })
            .ok()?;
        let urls = match decoded {
            Ok(urls) => urls,
            Err(error) => {
                tracing::warn!(
                    item_id = %item.id,
                    error = %error,
                    "QR rescue could not read the stored original"
                );
                return Some(QR_SKIP_UNREADABLE_ORIGINAL);
            }
        };
        if urls.is_empty() {
            return Some(QR_SKIP_NO_CODE);
        }
        report.decoded = report.decoded.saturating_add(1);

        let downloaded = match self.download_invoice(item, urls).await {
            Ok(downloaded) => downloaded,
            Err(code) => return Some(code),
        };
        match replace_original_with_download(
            &self.items,
            &self.paths,
            item,
            &downloaded,
            RecoveryOriginalKind::QrImage,
        )
        .await
        {
            Ok(()) => report.downloaded = report.downloaded.saturating_add(1),
            Err(error) => {
                tracing::warn!(
                    item_id = %item.id,
                    error = %error,
                    "QR rescue could not replace the stored original"
                );
                return Some(if matches!(error, AppError::Conflict { .. }) {
                    QR_SKIP_ITEM_CHANGED
                } else {
                    QR_SKIP_REPLACEMENT_FAILED
                });
            }
        }

        match self.recognition.recognize_item(item.id).await {
            Ok(recognized) if recognized.recognition_status == RecognitionStatus::Succeeded => None,
            Ok(_) => Some(QR_SKIP_RECOGNITION_FAILED),
            Err(error) => {
                tracing::warn!(
                    item_id = %item.id,
                    error = %error,
                    "QR rescue replaced the original but recognition failed"
                );
                Some(QR_SKIP_RECOGNITION_FAILED)
            }
        }
    }

    /// Follows the decoded links until one yields a usable invoice document.
    ///
    /// The `Err` value is the skip code: a link the downloader rejected or did
    /// not recognize as an invoice is "not an invoice link", while a request
    /// failure is reported as a download failure so the user knows retrying
    /// later may work.
    async fn download_invoice(
        &self,
        item: &InvoiceItem,
        urls: Vec<String>,
    ) -> Result<DownloadedInvoice, &'static str> {
        let mut saw_download_error = false;
        for url in urls {
            match self.downloader.download(&url).await {
                Ok(InvoiceLinkDownload::Downloaded(downloaded))
                    if file_signature::detect(&downloaded.bytes).is_invoice_document() =>
                {
                    return Ok(downloaded);
                }
                Ok(InvoiceLinkDownload::Downloaded(_)) => {
                    // A ZIP or an image without a code is not an invoice file the
                    // extractor could read, so the next link is tried instead.
                }
                Ok(InvoiceLinkDownload::Ignored) => {}
                Err(error) => {
                    saw_download_error = true;
                    tracing::warn!(
                        item_id = %item.id,
                        error = %error,
                        "QR rescue download failed"
                    );
                }
            }
        }
        if saw_download_error {
            Err(QR_SKIP_DOWNLOAD_FAILED)
        } else {
            Err(QR_SKIP_NOT_INVOICE_LINK)
        }
    }
}

/// Replaces a stored original with a downloaded invoice file.
///
/// Shared by the QR rescue and the link rescue. The bytes are written through
/// the same staging directory and promotion used by the import pipeline, and
/// the database swap mirrors the import side: the new file is promoted first,
/// the row is updated transactionally, and a failed update rolls the promoted
/// file back. The previous original and normalized PDF are deleted only after
/// the row no longer references them, and a failed cleanup is left to the
/// startup recovery instead of failing the already-successful repair.
pub(crate) async fn replace_original_with_download(
    items: &ItemRepository,
    paths: &AppPaths,
    item: &InvoiceItem,
    downloaded: &DownloadedInvoice,
    kind: RecoveryOriginalKind,
) -> Result<(), AppError> {
    let file_kind = file_signature::detect(&downloaded.bytes);
    let Some((extension, mime_type)) = file_kind.invoice_identity() else {
        return Err(AppError::validation(
            "downloadedInvoice",
            "下载到的文件不是可识别的发票文件",
        ));
    };
    let sha256 = format!("{:x}", Sha256::digest(&downloaded.bytes));
    let candidate_id = Uuid::new_v4();
    let paths_for_staging = paths.clone();
    let bytes = downloaded.bytes.clone();
    let staged = tokio::task::spawn_blocking(move || {
        stage_original(&paths_for_staging, candidate_id, &bytes)
    })
    .await
    .map_err(|error| AppError::Internal {
        message: format!("recovery replacement staging task failed: {error}"),
    })??;
    let date = item
        .batch_membership_date()
        .unwrap_or_else(|| item.fetched_at.date_naive());
    let promoted = staged.promote(date, item.id, extension)?;
    let replacement = EmailFileReplacement {
        original_name: downloaded.file_name.clone(),
        original_path: promoted.path().to_string_lossy().into_owned(),
        sha256,
        mime_type: mime_type.to_owned(),
    };
    match items
        .replace_original_for_recovery(item.id, replacement, kind)
        .await
    {
        Ok(Some(_replaced)) => {
            promoted.commit();
            if paths.delete_original(&item.original_path).is_err() {
                tracing::warn!(
                    item_id = %item.id,
                    "recovery left an unreferenced original for startup recovery"
                );
            }
            if let Some(normalized) = item.normalized_pdf_path.as_deref()
                && paths.delete_normalized_pdf(Path::new(normalized)).is_err()
            {
                tracing::warn!(
                    item_id = %item.id,
                    "recovery left an unreferenced normalized PDF for startup recovery"
                );
            }
            Ok(())
        }
        Ok(None) => {
            promoted.rollback()?;
            Err(AppError::Conflict {
                message: "票据在补救过程中被修改，已放弃本次替换".to_owned(),
            })
        }
        Err(database_error) => match promoted.rollback() {
            Ok(()) => Err(database_error),
            Err(cleanup_error) => Err(AppError::External {
                service: "filesystem_sync".to_owned(),
                retryable: false,
                message: format!(
                    "recovery replacement failed and downloaded file cleanup was incomplete; \
                     manual recovery is required: database error: {database_error}; cleanup \
                     error: {cleanup_error}"
                ),
            }),
        },
    }
}

/// Whether this invoice is in scope for the QR rescue.
fn is_candidate(item: &InvoiceItem) -> bool {
    item.amount_cents.is_none()
        && item.batch_id.is_none()
        && item.dedupe_status != DedupeStatus::SuspectedDuplicate
        && item.non_invoice_reason.is_none()
        && is_image_original_name(&item.original_name)
}

fn read_and_decode(paths: &AppPaths, source: &Path) -> Result<Vec<String>, AppError> {
    let mut opened = open_contained_regular_file(source, &paths.originals, "originalPath")?;
    if opened.length == 0 {
        return Err(AppError::validation("originalPath", "票据原件为空"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(opened.length).unwrap_or(0));
    std::io::Read::take(&mut opened.file, MAX_FILE_SIZE.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::Internal {
            message: format!("failed to read the stored original: {error}"),
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_SIZE {
        return Err(AppError::validation(
            "originalPath",
            "票据原件超过可处理的大小上限",
        ));
    }
    Ok(qr::decode_qr_urls(&bytes, MAX_QR_LINKS_PER_ITEM))
}

/// Writes the downloaded bytes into the staging area, exactly like an import.
///
/// Shared by every recovery service so a repaired original goes through the
/// same durable staging/promotion path as a fresh import.
pub(crate) fn stage_original(
    paths: &AppPaths,
    id: Uuid,
    bytes: &[u8],
) -> Result<StagedOriginal, AppError> {
    let (staged, mut file) = paths.begin_staged_original(id)?;
    let result = (|| {
        file.write_all(bytes).map_err(|error| AppError::Internal {
            message: format!("failed to write the downloaded invoice: {error}"),
        })?;
        file.sync_all().map_err(|error| AppError::Internal {
            message: format!("failed to sync the downloaded invoice: {error}"),
        })
    })();
    drop(file);
    match result {
        Ok(()) => Ok(staged),
        Err(error) => Err(staged.cleanup_after(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::QrRecoveryReport;

    #[test]
    fn skip_counts_are_grouped_and_stable() {
        let mut report = QrRecoveryReport::default();
        report.record_skip(super::QR_SKIP_NO_CODE);
        report.record_skip(super::QR_SKIP_DOWNLOAD_FAILED);
        report.record_skip(super::QR_SKIP_NO_CODE);

        assert_eq!(report.skipped, 3);
        assert_eq!(
            report.skip_reasons,
            vec![
                super::QrRecoverySkip {
                    code: super::QR_SKIP_DOWNLOAD_FAILED.to_owned(),
                    count: 1,
                },
                super::QrRecoverySkip {
                    code: super::QR_SKIP_NO_CODE.to_owned(),
                    count: 2,
                },
            ]
        );
    }
}
