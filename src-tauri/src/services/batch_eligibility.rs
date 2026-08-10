use std::path::Path;

use chrono::NaiveDate;

use crate::db::items::InvoiceItem;
use crate::domain::amount::validate_amount_cents;
use crate::domain::model::{DedupeStatus, ItemStatus};
use crate::infra::files::{AppPaths, open_contained_regular_file};

pub(crate) fn is_safe_batch_candidate(
    paths: &AppPaths,
    item: &InvoiceItem,
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> bool {
    item.status() == ItemStatus::Ready
        && item
            .batch_membership_date()
            .is_some_and(|date| date >= start_date && date <= end_date)
        && item.dedupe_status != DedupeStatus::SuspectedDuplicate
        && item.final_category.is_some()
        && item
            .amount_cents
            .is_some_and(|amount| validate_amount_cents(amount, "amountCents").is_ok())
        && item.currency == "CNY"
        && item.suggested_period.is_some()
        && invoice_files_are_safe(paths, item)
}

fn invoice_files_are_safe(paths: &AppPaths, item: &InvoiceItem) -> bool {
    open_contained_regular_file(Path::new(&item.original_path), &paths.originals, "original")
        .is_ok()
        && item.normalized_pdf_path.as_deref().is_some_and(|path| {
            open_contained_regular_file(Path::new(path), &paths.normalized, "normalizedPdf").is_ok()
        })
}
