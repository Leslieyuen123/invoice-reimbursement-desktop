use std::path::Path;

use chrono::NaiveDate;
use uuid::Uuid;

use crate::db::items::InvoiceItem;
use crate::domain::amount::validate_amount_cents;
use crate::domain::model::ItemStatus;
use crate::infra::files::{AppPaths, open_contained_regular_file};

/// The single authoritative reason an invoice cannot be exported inside a batch.
///
/// Every layer that decides whether an invoice may be put into (or stay in) a
/// reimbursement batch must agree on this predicate: batch automation, batch
/// candidates, manual assignment, and the export preflight. When they disagree,
/// a batch can be exported once and then fail later with a missing normalized
/// PDF that no user-facing screen ever warned about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BatchBlocker {
    SuspectedDuplicate,
    RecognitionFailed,
    NotRecognized,
    NotConfirmed,
    MissingNormalizedPdf,
    MissingOriginalFile,
    MissingCategory,
    MissingPeriod,
    MissingAmount,
    UnsupportedCurrency,
}

impl BatchBlocker {
    /// Stable machine-readable code shared with the frontend.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::SuspectedDuplicate => "suspected_duplicate",
            Self::RecognitionFailed => "recognition_failed",
            Self::NotRecognized => "not_recognized",
            Self::NotConfirmed => "not_confirmed",
            Self::MissingNormalizedPdf => "missing_normalized_pdf",
            Self::MissingOriginalFile => "missing_original_file",
            Self::MissingCategory => "missing_category",
            Self::MissingPeriod => "missing_period",
            Self::MissingAmount => "missing_amount",
            Self::UnsupportedCurrency => "unsupported_currency",
        }
    }

    /// Whether re-running document recognition could resolve the blocker.
    ///
    /// Only a missing normalized PDF is repairable, and only when the original
    /// is a format the extractor can normalize (see
    /// [`crate::services::recognition::original_can_be_normalized`]).
    pub(crate) fn is_repairable(self) -> bool {
        matches!(self, Self::MissingNormalizedPdf)
    }

    /// Error field used when a single invoice blocks an export.
    ///
    /// Keeps the historical validation taxonomy (`normalizedPdf`, `original`,
    /// …) so field-scoped error handling and existing callers keep working.
    pub(crate) fn field(self) -> &'static str {
        match self {
            Self::MissingNormalizedPdf => "normalizedPdf",
            Self::MissingOriginalFile => "original",
            Self::MissingCategory => "finalCategory",
            Self::MissingPeriod => "suggestedPeriod",
            Self::MissingAmount => "amountCents",
            Self::UnsupportedCurrency => "currency",
            Self::SuspectedDuplicate | Self::RecognitionFailed => "items",
            Self::NotRecognized => "recognitionStatus",
            Self::NotConfirmed => "confirmationStatus",
        }
    }

    /// Chinese user-facing explanation used by export and batch diagnostics.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::SuspectedDuplicate => "票据疑似重复，请先在待处理池处理",
            Self::RecognitionFailed => "票据识别失败，请先重新识别或更换原件",
            Self::NotRecognized => "票据尚未完成识别",
            Self::NotConfirmed => "票据尚未确认",
            Self::MissingNormalizedPdf => "票据缺少归一化 PDF",
            Self::MissingOriginalFile => "票据原件缺失或无法读取",
            Self::MissingCategory => "票据分类不能为空",
            Self::MissingPeriod => "建议归属时间不能为空",
            Self::MissingAmount => "票据金额不能为空",
            Self::UnsupportedCurrency => "仅支持人民币票据",
        }
    }
}

/// Every export prerequisite that can be decided from the database alone.
///
/// This is the layer `BatchService::assign_items` enforces inside its
/// transaction: an invoice whose recorded state cannot be exported must never
/// become a batch member, because that would make the whole batch unexportable.
pub(crate) fn db_export_blocker(item: &InvoiceItem) -> Option<BatchBlocker> {
    match item.status() {
        ItemStatus::SuspectedDuplicate => return Some(BatchBlocker::SuspectedDuplicate),
        ItemStatus::RecognitionFailed => return Some(BatchBlocker::RecognitionFailed),
        ItemStatus::PendingRecognition => return Some(BatchBlocker::NotRecognized),
        ItemStatus::PendingConfirmation => return Some(BatchBlocker::NotConfirmed),
        ItemStatus::Ready => {}
    }

    if item.final_category.is_none() {
        return Some(BatchBlocker::MissingCategory);
    }
    if item.suggested_period.is_none() {
        return Some(BatchBlocker::MissingPeriod);
    }
    if item.amount_cents.is_none() {
        return Some(BatchBlocker::MissingAmount);
    }
    if item.currency != "CNY" {
        return Some(BatchBlocker::UnsupportedCurrency);
    }
    if item.normalized_pdf_path.is_none() {
        return Some(BatchBlocker::MissingNormalizedPdf);
    }

    None
}

/// Every export prerequisite of a single invoice, ignoring the batch date range.
pub(crate) fn export_blocker(paths: &AppPaths, item: &InvoiceItem) -> Option<BatchBlocker> {
    if let Some(blocker) = db_export_blocker(item) {
        return Some(blocker);
    }
    // Amount range and overflow stay owned by the export amount contract, which
    // reports them with their own field names.
    if !item
        .amount_cents
        .is_some_and(|amount| validate_amount_cents(amount, "amountCents").is_ok())
    {
        return Some(BatchBlocker::MissingAmount);
    }

    if open_contained_regular_file(Path::new(&item.original_path), &paths.originals, "original")
        .is_err()
    {
        return Some(BatchBlocker::MissingOriginalFile);
    }
    let Some(normalized) = item.normalized_pdf_path.as_deref() else {
        return Some(BatchBlocker::MissingNormalizedPdf);
    };
    if open_contained_regular_file(Path::new(normalized), &paths.normalized, "normalizedPdf")
        .is_err()
    {
        return Some(BatchBlocker::MissingNormalizedPdf);
    }

    None
}

/// Whether this invoice can be exported inside a batch, ignoring the range.
pub(crate) fn is_export_ready(paths: &AppPaths, item: &InvoiceItem) -> bool {
    export_blocker(paths, item).is_none()
}

pub(crate) fn is_in_batch_range(
    item: &InvoiceItem,
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> bool {
    item.batch_membership_date()
        .is_some_and(|date| date >= start_date && date <= end_date)
}

/// The strict predicate batch automation uses before claiming an invoice.
pub(crate) fn is_safe_batch_candidate(
    paths: &AppPaths,
    item: &InvoiceItem,
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> bool {
    is_export_ready(paths, item) && is_in_batch_range(item, start_date, end_date)
}

/// One batch member that currently blocks the whole export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BatchIssue {
    pub item_id: Uuid,
    pub file_name: String,
    pub blocker: BatchBlocker,
}

/// Every batch member that would make `export` fail, in batch order.
pub(crate) fn batch_issues(paths: &AppPaths, items: &[InvoiceItem]) -> Vec<BatchIssue> {
    items
        .iter()
        .filter_map(|item| {
            let blocker = export_blocker(paths, item)?;
            Some(BatchIssue {
                item_id: item.id,
                file_name: item.original_name.clone(),
                blocker,
            })
        })
        .collect()
}

/// Names the blocking invoices, capped so the message stays readable.
pub(crate) fn describe_issues(issues: &[BatchIssue]) -> String {
    const MAX_NAMED: usize = 3;
    let named = issues
        .iter()
        .take(MAX_NAMED)
        .map(|issue| format!("{}（{}）", issue.file_name, issue.blocker.message()))
        .collect::<Vec<_>>()
        .join("；");
    if issues.len() > MAX_NAMED {
        format!("{named}；另有 {} 张同类问题票据", issues.len() - MAX_NAMED)
    } else {
        named
    }
}

#[cfg(test)]
mod tests {
    use super::{BatchBlocker, BatchIssue, describe_issues};
    use uuid::Uuid;

    fn issue(id: u128, file_name: &str, blocker: BatchBlocker) -> BatchIssue {
        BatchIssue {
            item_id: Uuid::from_u128(id),
            file_name: file_name.to_owned(),
            blocker,
        }
    }

    #[test]
    fn names_the_blocking_invoices_and_caps_the_message() {
        let single = describe_issues(&[issue(
            1,
            "invoice-1.pdf",
            BatchBlocker::MissingNormalizedPdf,
        )]);
        assert_eq!(single, "invoice-1.pdf（票据缺少归一化 PDF）");

        let many = describe_issues(&[
            issue(1, "a.pdf", BatchBlocker::MissingNormalizedPdf),
            issue(2, "b.pdf", BatchBlocker::MissingOriginalFile),
            issue(3, "c.zip", BatchBlocker::MissingNormalizedPdf),
            issue(4, "d.zip", BatchBlocker::MissingNormalizedPdf),
        ]);
        assert!(many.contains("a.pdf"));
        assert!(many.contains("c.zip"));
        assert!(!many.contains("d.zip"));
        assert!(many.ends_with("另有 1 张同类问题票据"));
    }

    #[test]
    fn maps_every_blocker_to_a_stable_code_field_and_message() {
        for (blocker, code, field) in [
            (
                BatchBlocker::SuspectedDuplicate,
                "suspected_duplicate",
                "items",
            ),
            (
                BatchBlocker::RecognitionFailed,
                "recognition_failed",
                "items",
            ),
            (
                BatchBlocker::NotRecognized,
                "not_recognized",
                "recognitionStatus",
            ),
            (
                BatchBlocker::NotConfirmed,
                "not_confirmed",
                "confirmationStatus",
            ),
            (
                BatchBlocker::MissingNormalizedPdf,
                "missing_normalized_pdf",
                "normalizedPdf",
            ),
            (
                BatchBlocker::MissingOriginalFile,
                "missing_original_file",
                "original",
            ),
            (
                BatchBlocker::MissingCategory,
                "missing_category",
                "finalCategory",
            ),
            (
                BatchBlocker::MissingPeriod,
                "missing_period",
                "suggestedPeriod",
            ),
            (BatchBlocker::MissingAmount, "missing_amount", "amountCents"),
            (
                BatchBlocker::UnsupportedCurrency,
                "unsupported_currency",
                "currency",
            ),
        ] {
            assert_eq!(blocker.code(), code);
            assert_eq!(blocker.field(), field);
            assert!(!blocker.message().is_empty());
        }
        assert!(BatchBlocker::MissingNormalizedPdf.is_repairable());
        assert!(!BatchBlocker::MissingOriginalFile.is_repairable());
        assert!(!BatchBlocker::NotConfirmed.is_repairable());
    }
}
