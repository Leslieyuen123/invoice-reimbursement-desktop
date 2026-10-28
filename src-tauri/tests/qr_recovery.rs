//! QR rescue of already-imported image invoices, and dismissal of files that
//! can never be invoices.
//!
//! The QR flow is exercised end to end with the committed QR PNG fixture and a
//! fake downloader: no test here touches the network.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{Duration, NaiveDate, Utc};
use invoice_reimbursement::commands::dashboard;
use invoice_reimbursement::commands::items::{self, ItemFilterDto};
use invoice_reimbursement::db;
use invoice_reimbursement::db::items::ItemRepository;
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{
    Category, ConfirmationStatus, ItemStatus, RecognitionStatus,
};
use invoice_reimbursement::infra::credentials::MemoryCredentialStore;
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::infra::imap::NativeTlsImapGateway;
use invoice_reimbursement::infra::invoice_download::{
    DownloadedInvoice, InvoiceLinkDownload, InvoiceLinkDownloader,
};
use invoice_reimbursement::state::AppState;
use sqlx::SqlitePool;
use uuid::Uuid;

/// A real QR image carrying an HTTPS invoice link.
const QR_FIXTURE: &[u8] = include_bytes!("fixtures/qr-invoice.png");
/// A real invoice photo without any QR code.
const PLAIN_FIXTURE: &[u8] = include_bytes!("fixtures/image-invoice.png");
/// The invoice document the fake portal returns.
const DOWNLOADED_PDF: &[u8] = b"%PDF-1.4 downloaded invoice\n%%EOF";

#[derive(Clone)]
enum FakeOutcome {
    Invoice(DownloadedInvoice),
    Ignored,
    Failure,
}

struct FakeDownloader {
    outcome: FakeOutcome,
    calls: Mutex<Vec<String>>,
}

impl FakeDownloader {
    fn new(outcome: FakeOutcome) -> Arc<Self> {
        Arc::new(Self {
            outcome,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn invoice() -> Arc<Self> {
        Self::new(FakeOutcome::Invoice(DownloadedInvoice {
            file_name: "portal-invoice.pdf".to_owned(),
            bytes: DOWNLOADED_PDF.to_vec(),
            mime_type: "application/pdf".to_owned(),
        }))
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl InvoiceLinkDownloader for FakeDownloader {
    async fn download(&self, source_url: &str) -> Result<InvoiceLinkDownload, AppError> {
        self.calls.lock().unwrap().push(source_url.to_owned());
        match &self.outcome {
            FakeOutcome::Invoice(invoice) => Ok(InvoiceLinkDownload::Downloaded(invoice.clone())),
            FakeOutcome::Ignored => Ok(InvoiceLinkDownload::Ignored),
            FakeOutcome::Failure => Err(AppError::External {
                service: "invoice_download".to_owned(),
                retryable: true,
                message: "portal unavailable".to_owned(),
            }),
        }
    }
}

/// Recognizes only the downloaded PDF, so the assertions prove recognition ran
/// on the replacement rather than on the original image.
struct PdfOnlyExtractor {
    extracted: Mutex<Vec<PathBuf>>,
}

impl PdfOnlyExtractor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            extracted: Mutex::new(Vec::new()),
        })
    }

    fn extracted_paths(&self) -> Vec<PathBuf> {
        self.extracted.lock().unwrap().clone()
    }
}

impl DocumentExtractor for PdfOnlyExtractor {
    fn extract(&self, path: &Path) -> Result<ExtractedDocument, AppError> {
        self.extracted.lock().unwrap().push(path.to_path_buf());
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        {
            Ok(ExtractedDocument {
                text: "开票日期：2026年06月18日 餐饮 食品 价税合计（小写）￥128.50".to_owned(),
                normalized_pdf: Some(b"%PDF-1.4 normalized invoice".to_vec()),
                warnings: Vec::new(),
            })
        } else {
            Err(AppError::External {
                service: "document_extractor".to_owned(),
                retryable: false,
                message: "unsupported image".to_owned(),
            })
        }
    }
}

fn state_with(
    pool: &SqlitePool,
    paths: &AppPaths,
    extractor: Arc<PdfOnlyExtractor>,
    downloader: Arc<FakeDownloader>,
) -> AppState {
    AppState::with_gateway_and_extractor(
        pool.clone(),
        paths.clone(),
        Arc::new(MemoryCredentialStore::default()),
        Arc::new(NativeTlsImapGateway::default()),
        extractor,
    )
    .with_link_downloader(downloader)
}

/// Stores `bytes` as an item original the way the import pipeline does.
fn persist_fixture(paths: &AppPaths, id: Uuid, file_name: &str, bytes: &[u8]) -> String {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join(file_name);
    std::fs::write(&source, bytes).unwrap();
    let extension = Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap()
        .to_ascii_lowercase();
    paths
        .persist_original(
            &source,
            NaiveDate::from_ymd_opt(2026, 6, 18).unwrap(),
            id,
            &extension,
        )
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

struct ItemSeed<'a> {
    id: Uuid,
    original_name: &'a str,
    original_path: String,
    recognition_status: RecognitionStatus,
    confirmation_status: ConfirmationStatus,
    amount_cents: Option<i64>,
    created_at: chrono::DateTime<Utc>,
}

async fn insert_item(pool: &SqlitePool, seed: ItemSeed<'_>) {
    sqlx::query(
        "INSERT INTO items (
            id, original_name, original_path, sha256, mime_type, source_type,
            fetched_at, currency, recognition_status, confirmation_status,
            dedupe_status, amount_cents, suggested_category, note, created_at, updated_at
         ) VALUES (?, ?, ?, ?, 'image/png', 'manual_upload', ?, 'CNY', ?, ?, 'unique', ?, ?, ?, ?, ?)",
    )
    .bind(seed.id.to_string())
    .bind(seed.original_name)
    .bind(&seed.original_path)
    .bind(format!("sha-{}", seed.id))
    .bind(seed.created_at.to_rfc3339())
    .bind(match seed.recognition_status {
        RecognitionStatus::Pending => "pending",
        RecognitionStatus::Succeeded => "succeeded",
        RecognitionStatus::Failed => "failed",
    })
    .bind(match seed.confirmation_status {
        ConfirmationStatus::Pending => "pending",
        ConfirmationStatus::Confirmed => "confirmed",
    })
    .bind(seed.amount_cents)
    .bind(Option::<String>::None)
    .bind("识别失败：图片中没有金额")
    .bind(seed.created_at.to_rfc3339())
    .bind(seed.created_at.to_rfc3339())
    .execute(pool)
    .await
    .expect("fixture item should insert");
}

fn failed_item(
    id: Uuid,
    original_name: &str,
    original_path: String,
    created_at: chrono::DateTime<Utc>,
) -> ItemSeed<'_> {
    ItemSeed {
        id,
        original_name,
        original_path,
        recognition_status: RecognitionStatus::Failed,
        confirmation_status: ConfirmationStatus::Pending,
        amount_cents: None,
        created_at,
    }
}

async fn insert_image_item(
    pool: &SqlitePool,
    paths: &AppPaths,
    file_name: &str,
    bytes: &[u8],
) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let original_path = persist_fixture(paths, id, file_name, bytes);
    insert_item(
        pool,
        failed_item(id, file_name, original_path.clone(), Utc::now()),
    )
    .await;
    (id, original_path)
}

#[tokio::test]
async fn a_qr_image_is_replaced_by_the_downloaded_invoice_and_recognized() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let (id, old_original) = insert_image_item(&pool, &paths, "qr-invoice.png", QR_FIXTURE).await;
    let extractor = PdfOnlyExtractor::new();
    let downloader = FakeDownloader::invoice();
    let state = state_with(&pool, &paths, extractor.clone(), downloader.clone());

    let report = items::recover_qr_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1, "the stored image must be examined");
    assert_eq!(report.decoded, 1, "the QR code must be decoded");
    assert_eq!(report.downloaded, 1, "the invoice must be downloaded");
    assert_eq!(
        report.recognized, 1,
        "recognition must run on the replacement"
    );
    assert_eq!(report.skipped, 0);
    assert!(report.skipped_breakdown.is_empty());

    let calls = downloader.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "exactly one download must be attempted");
    assert!(
        calls[0].starts_with("https://invoice.example/download/"),
        "unexpected download URL: {}",
        calls[0]
    );

    let repository = ItemRepository::new(pool.clone());
    let repaired = repository.get_by_id(id).await.unwrap();
    assert_eq!(repaired.original_name, "portal-invoice.pdf");
    assert!(repaired.original_path.ends_with(".pdf"));
    assert_eq!(
        std::fs::read(&repaired.original_path).unwrap(),
        DOWNLOADED_PDF,
        "the downloaded invoice must become the stored original"
    );
    assert!(
        std::fs::read(&old_original).is_err(),
        "the replaced image must not be left behind"
    );
    assert_eq!(repaired.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(repaired.amount_cents, Some(12_850));
    assert_eq!(repaired.final_category, Some(Category::Dining));
    assert!(
        repaired.normalized_pdf_path.is_some(),
        "recognition must persist the normalized PDF"
    );
    assert_eq!(
        extractor.extracted_paths(),
        vec![PathBuf::from(&repaired.original_path)],
        "recognition must run on the downloaded PDF, never on the image"
    );

    // Idempotent: the repaired invoice is no longer an image candidate.
    let second = items::recover_qr_invoices(&state, None).await.unwrap();
    assert_eq!(second.examined, 0);
    assert_eq!(second.downloaded, 0);
    assert_eq!(
        downloader.call_count(),
        1,
        "a repeated run must not download again"
    );
    let item_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(item_count, 1, "the rescue must never create a second item");
}

#[tokio::test]
async fn an_image_without_a_qr_code_is_skipped_and_left_untouched() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let (id, original_path) = insert_image_item(&pool, &paths, "plain.png", PLAIN_FIXTURE).await;
    let before = ItemRepository::new(pool.clone())
        .get_by_id(id)
        .await
        .unwrap();
    let extractor = PdfOnlyExtractor::new();
    let downloader = FakeDownloader::invoice();
    let state = state_with(&pool, &paths, extractor.clone(), downloader.clone());

    let report = items::recover_qr_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.decoded, 0);
    assert_eq!(report.downloaded, 0);
    assert_eq!(report.recognized, 0);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.skipped_breakdown.len(), 1);
    assert_eq!(report.skipped_breakdown[0].code, "no_qr_code");
    assert_eq!(report.skipped_breakdown[0].label, "没有二维码");
    assert_eq!(report.skipped_breakdown[0].count, 1);
    assert_eq!(
        downloader.call_count(),
        0,
        "an image without a code must never be downloaded"
    );

    let after = ItemRepository::new(pool.clone())
        .get_by_id(id)
        .await
        .unwrap();
    assert_eq!(after, before, "a skipped invoice must not change");
    assert_eq!(std::fs::read(&original_path).unwrap(), PLAIN_FIXTURE);
    assert!(extractor.extracted_paths().is_empty());
}

#[tokio::test]
async fn a_link_that_is_not_an_invoice_is_skipped_and_left_untouched() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let (id, _) = insert_image_item(&pool, &paths, "qr-invoice.png", QR_FIXTURE).await;
    let before = ItemRepository::new(pool.clone())
        .get_by_id(id)
        .await
        .unwrap();
    let extractor = PdfOnlyExtractor::new();
    let downloader = FakeDownloader::new(FakeOutcome::Ignored);
    let state = state_with(&pool, &paths, extractor, downloader.clone());

    let report = items::recover_qr_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.decoded, 1);
    assert_eq!(report.downloaded, 0);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.skipped_breakdown[0].code, "not_invoice_link");
    assert_eq!(report.skipped_breakdown[0].label, "不是发票链接");
    assert_eq!(downloader.call_count(), 1);
    assert_eq!(
        ItemRepository::new(pool.clone())
            .get_by_id(id)
            .await
            .unwrap(),
        before,
        "an unusable link must not change the invoice"
    );
}

#[tokio::test]
async fn a_failed_download_leaves_the_original_in_place() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let (id, original_path) = insert_image_item(&pool, &paths, "qr-invoice.png", QR_FIXTURE).await;
    let before = ItemRepository::new(pool.clone())
        .get_by_id(id)
        .await
        .unwrap();
    let extractor = PdfOnlyExtractor::new();
    let downloader = FakeDownloader::new(FakeOutcome::Failure);
    let state = state_with(&pool, &paths, extractor, downloader.clone());

    let report = items::recover_qr_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.downloaded, 0);
    assert_eq!(report.recognized, 0);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.skipped_breakdown[0].code, "download_failed");
    assert_eq!(report.skipped_breakdown[0].label, "下载失败");
    let after = ItemRepository::new(pool.clone())
        .get_by_id(id)
        .await
        .unwrap();
    assert_eq!(
        after, before,
        "a download failure must not touch the invoice"
    );
    assert_eq!(
        std::fs::read(&original_path).unwrap(),
        QR_FIXTURE,
        "the original image must survive a failed download"
    );
}

#[tokio::test]
async fn a_single_invoice_entry_rescues_only_the_requested_invoice() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let (id, _) = insert_image_item(&pool, &paths, "qr-invoice.png", QR_FIXTURE).await;
    let extractor = PdfOnlyExtractor::new();
    let downloader = FakeDownloader::invoice();
    let state = state_with(&pool, &paths, extractor, downloader.clone());

    let report = items::recover_qr_invoice(&state, id).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.recognized, 1);
    assert_eq!(downloader.call_count(), 1);

    // A PDF original is no longer a QR rescue target and the user-facing error
    // says so instead of silently reporting an empty report.
    let error = items::recover_qr_invoice(&state, id)
        .await
        .expect_err("a repaired invoice must be rejected by the single-ticket entry");
    assert!(matches!(error, AppError::Validation { ref field, .. } if field == "item"));
}

#[tokio::test]
async fn dismissing_non_invoice_documents_hides_them_from_queue_counts_and_settle_stats() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let now = Utc::now();
    let mut office_ids = Vec::new();
    for (index, file_name) in [
        "note.docx",
        "old.doc",
        "table.xls",
        "sheet.xlsx",
        "bundle.zip",
    ]
    .into_iter()
    .enumerate()
    {
        let id = Uuid::new_v4();
        insert_item(
            &pool,
            failed_item(
                id,
                file_name,
                paths
                    .originals
                    .join(format!("{id}"))
                    .to_string_lossy()
                    .into_owned(),
                now - Duration::minutes(index as i64),
            ),
        )
        .await;
        office_ids.push(id);
    }
    // A real image invoice that must stay in the queue, and a PDF failure that
    // is not an office document either.
    let (image_id, _) = insert_image_item(&pool, &paths, "plain.png", PLAIN_FIXTURE).await;
    let pdf_id = Uuid::new_v4();
    insert_item(
        &pool,
        failed_item(
            pdf_id,
            "scan.pdf",
            paths
                .originals
                .join(format!("{pdf_id}"))
                .to_string_lossy()
                .into_owned(),
            now - Duration::hours(1),
        ),
    )
    .await;
    // Recognized but amount-less office document: it is part of the bulk-confirm
    // skip statistics until it is dismissed.
    let recognized_docx = Uuid::new_v4();
    insert_item(
        &pool,
        ItemSeed {
            id: recognized_docx,
            original_name: "reimbursement-form.docx",
            original_path: paths
                .originals
                .join(format!("{recognized_docx}"))
                .to_string_lossy()
                .into_owned(),
            recognition_status: RecognitionStatus::Succeeded,
            confirmation_status: ConfirmationStatus::Pending,
            amount_cents: None,
            created_at: now - Duration::hours(2),
        },
    )
    .await;

    let extractor = PdfOnlyExtractor::new();
    let downloader = FakeDownloader::invoice();
    let state = state_with(&pool, &paths, extractor, downloader);

    let summary = items::non_invoice_summary(&state).await.unwrap();
    assert_eq!(summary.markable_count, 6);
    assert_eq!(summary.marked_count, 0);

    let settle_before = items::settle_recoverable_items(&state).await.unwrap();
    assert_eq!(
        settle_before.skipped_count, 1,
        "the amount-less office document is counted before it is dismissed"
    );

    let outcome = items::archive_non_invoice_items(&state).await.unwrap();
    assert_eq!(outcome.archived_count, 6);

    let summary = items::non_invoice_summary(&state).await.unwrap();
    assert_eq!(summary.markable_count, 0);
    assert_eq!(summary.marked_count, 6);

    let dashboard = dashboard::load(&state).await.unwrap();
    assert_eq!(
        dashboard.recognition_failed_count, 2,
        "only the image invoice and the PDF failure may remain in the failed count"
    );
    assert_eq!(
        dashboard.pending_confirmation_count, 0,
        "a dismissed non-invoice file must not count as pending confirmation"
    );

    let repository = ItemRepository::new(pool.clone());
    let dismissed = repository.get_by_id(office_ids[0]).await.unwrap();
    assert_eq!(
        dismissed.non_invoice_reason.as_deref(),
        Some("document_not_invoice")
    );
    assert_eq!(
        repository
            .get_by_id(recognized_docx)
            .await
            .unwrap()
            .non_invoice_reason
            .as_deref(),
        Some("document_not_invoice")
    );
    assert_eq!(
        repository
            .get_by_id(image_id)
            .await
            .unwrap()
            .non_invoice_reason,
        None
    );
    assert_eq!(
        repository
            .get_by_id(pdf_id)
            .await
            .unwrap()
            .non_invoice_reason,
        None
    );

    // Queue views hide them; the "全部" view (no status filter) still lists them.
    let queue = items::list(
        &state,
        ItemFilterDto {
            status: Some(ItemStatus::RecognitionFailed),
            ..ItemFilterDto::default()
        },
    )
    .await
    .unwrap();
    let mut queue_ids = queue.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
    queue_ids.sort();
    assert_eq!(queue_ids.len(), 2);
    assert!(queue_ids.contains(&image_id.to_string()));
    assert!(queue_ids.contains(&pdf_id.to_string()));

    let all = items::list(&state, ItemFilterDto::default()).await.unwrap();
    assert_eq!(all.len(), 8, "dismissed files stay visible in 全部");
    assert!(all.iter().any(|item| item.id == office_ids[0].to_string()));

    // The bulk-confirm statistics no longer skip the dismissed document.
    let settle_after = items::settle_recoverable_items(&state).await.unwrap();
    assert_eq!(settle_after.skipped_count, 0);

    // A second dismissal is a no-op.
    let again = items::archive_non_invoice_items(&state).await.unwrap();
    assert_eq!(again.archived_count, 0);
}
