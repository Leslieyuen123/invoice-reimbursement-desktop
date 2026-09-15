use crate::db::items::InvoiceItem;
use crate::infra::files::AppPaths;
use crate::services::batch_eligibility::{BatchBlocker, export_blocker};
use crate::services::recognition::{RecognitionService, original_can_be_normalized};

/// Regenerates the normalized PDF of every batch member that is missing one.
///
/// Recognition only writes the normalized PDF while it runs for a pending
/// invoice, so an invoice confirmed by hand (or one whose normalized file went
/// missing) stayed in the batch as "ready" while every export failed with
/// 票据缺少归一化 PDF. Repairing before the export makes "重试自动处理" able to
/// heal such a batch instead of failing on the same invoice forever.
///
/// Returns how many invoices were repaired. Failures are logged and left for
/// the export preflight to report, so a single broken original never hides the
/// ones that could be fixed.
pub(crate) async fn repair_missing_normalized_pdfs(
    paths: &AppPaths,
    recognition: &RecognitionService,
    items: &[InvoiceItem],
) -> u32 {
    let mut repaired_count = 0_u32;
    for item in items {
        if export_blocker(paths, item) != Some(BatchBlocker::MissingNormalizedPdf)
            || !original_can_be_normalized(&item.original_name)
        {
            continue;
        }
        match recognition.backfill_normalized_pdf(item.id).await {
            Ok(_) => repaired_count = repaired_count.saturating_add(1),
            Err(error) => tracing::warn!(
                item_id = %item.id,
                error = %error,
                "normalized PDF repair failed; the export preflight will report it"
            ),
        }
    }
    repaired_count
}
