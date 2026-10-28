//! Link-placeholder rescue of already-imported invoices.
//!
//! Every test drives the whole flow with a fake downloader and a local PDF
//! fixture: no test here touches the network.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use invoice_reimbursement::commands::items;
use invoice_reimbursement::db;
use invoice_reimbursement::db::items::{ItemRepository, LINK_NOT_INVOICE_REASON};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{Category, RecognitionStatus};
use invoice_reimbursement::infra::credentials::MemoryCredentialStore;
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::infra::imap::NativeTlsImapGateway;
use invoice_reimbursement::infra::invoice_download::{
    DownloadFailureKind, DownloadedInvoice, InvoiceLinkDownload, InvoiceLinkDownloader,
    download_failure_error,
};
use invoice_reimbursement::state::AppState;
use sqlx::SqlitePool;
use uuid::Uuid;

/// The invoice document the fake portal returns.
const DOWNLOADED_PDF: &[u8] = b"%PDF-1.4 downloaded invoice\n%%EOF";
/// A portal page instead of a file: the downloader rejects it before returning.
const PORTAL_HTML: &[u8] = b"<!doctype html><title>portal</title>";

#[derive(Clone)]
enum FakeOutcome {
    Invoice(DownloadedInvoice),
    DownloadedBytes(Vec<u8>, &'static str),
    Ignored,
    Failure(DownloadFailureKind),
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

    fn failure(kind: DownloadFailureKind) -> Arc<Self> {
        Self::new(FakeOutcome::Failure(kind))
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl InvoiceLinkDownloader for FakeDownloader {
    async fn download(&self, source_url: &str) -> Result<InvoiceLinkDownload, AppError> {
        self.calls.lock().unwrap().push(source_url.to_owned());
        match &self.outcome {
            FakeOutcome::Invoice(invoice) => Ok(InvoiceLinkDownload::Downloaded(invoice.clone())),
            FakeOutcome::DownloadedBytes(bytes, mime) => {
                Ok(InvoiceLinkDownload::Downloaded(DownloadedInvoice {
                    file_name: "portal-page.html".to_owned(),
                    bytes: bytes.clone(),
                    mime_type: (*mime).to_owned(),
                }))
            }
            FakeOutcome::Ignored => Ok(InvoiceLinkDownload::Ignored),
            FakeOutcome::Failure(kind) => Err(download_failure_error(*kind)),
        }
    }
}

/// Recognizes only the downloaded PDF, so the assertions prove recognition ran
/// on the replacement rather than on the placeholder.
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

/// Stores `content` as a `.url` placeholder the way the import pipeline does.
async fn insert_placeholder(
    pool: &SqlitePool,
    paths: &AppPaths,
    content: &[u8],
    created_at: chrono::DateTime<Utc>,
) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let path = paths.originals.join(format!("{id}.url"));
    std::fs::write(&path, content).expect("placeholder fixture should write");
    sqlx::query(
        "INSERT INTO items (
            id, original_name, original_path, sha256, mime_type, source_type,
            source_account_id, source_mailbox, source_uid_validity, source_uid,
            source_part_id, fetched_at, currency, recognition_status,
            confirmation_status, dedupe_status, amount_cents, note, created_at,
            updated_at
         ) VALUES (?, ?, ?, ?, 'text/uri-list', 'email', ?, 'INBOX', 1, 1, '2',
                   ?, 'CNY', 'failed', 'pending', 'unique', NULL,
                   'invoice link download failed', ?, ?)",
    )
    .bind(id.to_string())
    .bind("download-1-2.url")
    .bind(path.to_string_lossy().into_owned())
    .bind(format!("sha-{id}"))
    .bind(Uuid::new_v4().to_string())
    .bind(created_at.to_rfc3339())
    .bind(created_at.to_rfc3339())
    .bind(created_at.to_rfc3339())
    .execute(pool)
    .await
    .expect("placeholder item should insert");
    (id, path.to_string_lossy().into_owned())
}

/// A fresh storage tree plus an empty in-memory database.
///
/// The `TempDir` is returned so the caller keeps the storage alive for the
/// duration of the test.
async fn empty_storage() -> (tempfile::TempDir, AppPaths, SqlitePool) {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    (directory, paths, pool)
}

fn skip_code<'a>(
    report: &'a items::LinkRecoveryReportDto,
    code: &str,
) -> Option<&'a items::LinkRecoverySkipDto> {
    report
        .skipped_breakdown
        .iter()
        .find(|entry| entry.code == code)
}

#[tokio::test]
async fn a_link_placeholder_is_replaced_by_the_downloaded_invoice_and_recognized() {
    let (_directory, paths, pool) = empty_storage().await;
    let (id, old_placeholder) = insert_placeholder(
        &pool,
        &paths,
        b"https://nnfp.jss.com.cn/5TQY1_XFJO-11SlG\r\n",
        Utc::now(),
    )
    .await;
    let extractor = PdfOnlyExtractor::new();
    let downloader = FakeDownloader::invoice();
    let state = state_with(&pool, &paths, extractor.clone(), downloader.clone());

    let report = items::recover_link_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1, "the placeholder must be examined");
    assert_eq!(report.downloaded, 1, "the invoice must be downloaded");
    assert_eq!(
        report.recognized, 1,
        "recognition must run on the replacement"
    );
    assert_eq!(report.skipped, 0);
    assert_eq!(report.archived, 0);
    assert_eq!(report.not_invoice_portal, 0);
    assert!(report.skipped_breakdown.is_empty());
    assert_eq!(
        downloader.calls(),
        vec!["https://nnfp.jss.com.cn/5TQY1_XFJO-11SlG".to_owned()]
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
        std::fs::read(&old_placeholder).is_err(),
        "the replaced placeholder must not be left behind"
    );
    assert_eq!(repaired.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(repaired.amount_cents, Some(12_850));
    assert_eq!(repaired.final_category, Some(Category::Dining));
    assert!(
        repaired.normalized_pdf_path.is_some(),
        "recognition must persist the normalized PDF"
    );
    assert_eq!(
        repaired.note, None,
        "the stale 'invoice link download failed' note must be cleared"
    );
    assert_eq!(repaired.non_invoice_reason, None);
    assert_eq!(
        extractor.extracted_paths(),
        vec![PathBuf::from(&repaired.original_path)],
        "recognition must run on the downloaded PDF, never on the placeholder"
    );

    // Idempotent: the repaired invoice is no longer a `.url` candidate.
    let second = items::recover_link_invoices(&state, None).await.unwrap();
    assert_eq!(second.examined, 0);
    assert_eq!(second.downloaded, 0);
    assert_eq!(second.recognized, 0);
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
async fn placeholder_content_that_is_not_a_downloadable_url_is_skipped_untouched() {
    let (_directory, paths, pool) = empty_storage().await;
    let (not_a_link, first_path) =
        insert_placeholder(&pool, &paths, b"not a link at all\r\n", Utc::now()).await;
    let (insecure, second_path) = insert_placeholder(
        &pool,
        &paths,
        b"http://nnfp.jss.com.cn/5TQY1_XFJO-11SlG\r\n",
        Utc::now() - Duration::minutes(1),
    )
    .await;
    let repository = ItemRepository::new(pool.clone());
    let before_first = repository.get_by_id(not_a_link).await.unwrap();
    let before_second = repository.get_by_id(insecure).await.unwrap();
    let downloader = FakeDownloader::invoice();
    let state = state_with(&pool, &paths, PdfOnlyExtractor::new(), downloader.clone());

    let report = items::recover_link_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 2);
    assert_eq!(report.downloaded, 0);
    assert_eq!(report.recognized, 0);
    assert_eq!(report.skipped, 2);
    assert_eq!(report.archived, 0, "a malformed link is not a dismissal");
    assert_eq!(
        skip_code(&report, "invalid_url").map(|entry| entry.count),
        Some(2)
    );
    assert_eq!(
        skip_code(&report, "invalid_url").map(|entry| entry.label.as_str()),
        Some("链接不合法")
    );
    assert_eq!(
        downloader.call_count(),
        0,
        "an invalid link must never be requested"
    );
    assert_eq!(
        repository.get_by_id(not_a_link).await.unwrap(),
        before_first,
        "a skipped placeholder must not change"
    );
    assert_eq!(
        repository.get_by_id(insecure).await.unwrap(),
        before_second,
        "a skipped placeholder must not change"
    );
    assert_eq!(
        std::fs::read(&first_path).unwrap(),
        b"not a link at all\r\n"
    );
    assert_eq!(
        std::fs::read(&second_path).unwrap(),
        b"http://nnfp.jss.com.cn/5TQY1_XFJO-11SlG\r\n"
    );
}

#[tokio::test]
async fn an_empty_placeholder_reports_a_missing_link() {
    let (_directory, paths, pool) = empty_storage().await;
    let (id, path) = insert_placeholder(&pool, &paths, b"  \r\n\r\n", Utc::now()).await;
    let downloader = FakeDownloader::invoice();
    let state = state_with(&pool, &paths, PdfOnlyExtractor::new(), downloader.clone());

    let report = items::recover_link_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.archived, 0);
    assert_eq!(
        skip_code(&report, "missing_url").map(|entry| entry.label.as_str()),
        Some("占位里没有链接")
    );
    assert_eq!(downloader.call_count(), 0);
    assert_eq!(
        ItemRepository::new(pool.clone())
            .get_by_id(id)
            .await
            .unwrap()
            .non_invoice_reason,
        None
    );
    assert!(std::fs::read(&path).is_ok());
}

#[tokio::test]
async fn a_marketing_link_is_not_requested_and_is_archived() {
    let (_directory, paths, pool) = empty_storage().await;
    let (id, placeholder) = insert_placeholder(
        &pool,
        &paths,
        b"https://shop.10086.cn/marketing/abc\r\n",
        Utc::now(),
    )
    .await;
    let repository = ItemRepository::new(pool.clone());
    let before = repository.get_by_id(id).await.unwrap();
    let extractor = PdfOnlyExtractor::new();
    let downloader = FakeDownloader::invoice();
    let state = state_with(&pool, &paths, extractor.clone(), downloader.clone());

    let report = items::recover_link_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.downloaded, 0);
    assert_eq!(report.recognized, 0);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.archived, 1);
    assert_eq!(report.not_invoice_portal, 1);
    assert_eq!(
        skip_code(&report, "not_invoice_portal").map(|entry| entry.label.as_str()),
        Some("非发票网站")
    );
    assert_eq!(
        downloader.call_count(),
        0,
        "a marketing host must never receive a request"
    );
    assert!(extractor.extracted_paths().is_empty());

    let after = repository.get_by_id(id).await.unwrap();
    assert_eq!(
        after.non_invoice_reason.as_deref(),
        Some(LINK_NOT_INVOICE_REASON),
        "the marketing placeholder must leave the work queue"
    );
    assert_eq!(after.original_name, before.original_name);
    assert_eq!(after.original_path, before.original_path);
    assert_eq!(after.sha256, before.sha256);
    assert_eq!(after.recognition_status, before.recognition_status);
    assert_eq!(after.note, before.note);
    assert_eq!(
        std::fs::read(&placeholder).unwrap(),
        b"https://shop.10086.cn/marketing/abc\r\n",
        "the placeholder file stays on disk for the user"
    );

    // Already dismissed placeholders are no longer candidates.
    let second = items::recover_link_invoices(&state, None).await.unwrap();
    assert_eq!(second.examined, 0);
}

#[tokio::test]
async fn a_login_wall_is_reported_and_archived_without_touching_the_row() {
    let (_directory, paths, pool) = empty_storage().await;
    let (id, placeholder) = insert_placeholder(
        &pool,
        &paths,
        b"https://pis.baiwang.com/invoice/abc\r\n",
        Utc::now(),
    )
    .await;
    let repository = ItemRepository::new(pool.clone());
    let before = repository.get_by_id(id).await.unwrap();
    let downloader = FakeDownloader::failure(DownloadFailureKind::RequiresLogin);
    let state = state_with(&pool, &paths, PdfOnlyExtractor::new(), downloader.clone());

    let report = items::recover_link_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.downloaded, 0);
    assert_eq!(report.recognized, 0);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.archived, 1);
    assert_eq!(report.not_invoice_portal, 0);
    assert_eq!(
        skip_code(&report, "requires_login").map(|entry| entry.label.as_str()),
        Some("门户需要登录")
    );
    assert_eq!(downloader.call_count(), 1);

    let after = repository.get_by_id(id).await.unwrap();
    assert_eq!(
        after.non_invoice_reason.as_deref(),
        Some(LINK_NOT_INVOICE_REASON),
        "a login wall can never be repaired by retrying"
    );
    assert_eq!(after.original_name, before.original_name);
    assert_eq!(after.original_path, before.original_path);
    assert_eq!(after.sha256, before.sha256);
    assert_eq!(after.recognition_status, before.recognition_status);
    assert_eq!(after.amount_cents, None);
    assert_eq!(after.note, before.note);
    assert_eq!(
        std::fs::read(&placeholder).unwrap(),
        b"https://pis.baiwang.com/invoice/abc\r\n"
    );
}

#[tokio::test]
async fn a_page_that_is_not_an_invoice_is_archived() {
    let (_directory, paths, pool) = empty_storage().await;
    let (rejected, _) = insert_placeholder(
        &pool,
        &paths,
        b"https://fpkj.vpiaotong.com/invoice/one\r\n",
        Utc::now(),
    )
    .await;
    let (rejected_page, _) = insert_placeholder(
        &pool,
        &paths,
        b"https://fpkj.vpiaotong.com/invoice/two\r\n",
        Utc::now() - Duration::minutes(1),
    )
    .await;
    let repository = ItemRepository::new(pool.clone());
    let downloader = FakeDownloader::failure(DownloadFailureKind::NotAnInvoice);
    // The second run returns an HTML body; the rescue must still refuse it.
    let state = state_with(&pool, &paths, PdfOnlyExtractor::new(), downloader.clone());

    let report = items::recover_link_invoices(&state, None).await.unwrap();
    assert_eq!(report.examined, 2);
    assert_eq!(report.downloaded, 0);
    assert_eq!(report.skipped, 2);
    assert_eq!(report.archived, 2);
    assert_eq!(
        skip_code(&report, "not_invoice").map(|entry| entry.label.as_str()),
        Some("下载到的不是发票")
    );
    assert_eq!(downloader.call_count(), 2);
    for id in [rejected, rejected_page] {
        assert_eq!(
            repository
                .get_by_id(id)
                .await
                .unwrap()
                .non_invoice_reason
                .as_deref(),
            Some(LINK_NOT_INVOICE_REASON)
        );
    }
}

#[tokio::test]
async fn an_ignored_portal_page_is_archived_as_a_non_invoice_file() {
    let (_directory, paths, pool) = empty_storage().await;
    let (id, _) =
        insert_placeholder(&pool, &paths, b"https://fp.nuonuo.com/#/\r\n", Utc::now()).await;
    let downloader = FakeDownloader::new(FakeOutcome::Ignored);
    let state = state_with(&pool, &paths, PdfOnlyExtractor::new(), downloader.clone());

    let report = items::recover_link_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.downloaded, 0);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.archived, 1);
    assert_eq!(
        skip_code(&report, "not_invoice").map(|entry| entry.count),
        Some(1)
    );
    assert_eq!(
        ItemRepository::new(pool.clone())
            .get_by_id(id)
            .await
            .unwrap()
            .non_invoice_reason
            .as_deref(),
        Some(LINK_NOT_INVOICE_REASON)
    );
}

#[tokio::test]
async fn a_downloaded_html_page_is_never_accepted_as_an_invoice() {
    let (_directory, paths, pool) = empty_storage().await;
    let (id, placeholder) = insert_placeholder(
        &pool,
        &paths,
        b"https://nnfp.jss.com.cn/5TQY1_XFJO-11SlG\r\n",
        Utc::now(),
    )
    .await;
    let downloader = FakeDownloader::new(FakeOutcome::DownloadedBytes(
        PORTAL_HTML.to_vec(),
        "text/html",
    ));
    let state = state_with(&pool, &paths, PdfOnlyExtractor::new(), downloader.clone());

    let report = items::recover_link_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.downloaded, 0);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.archived, 1);
    assert_eq!(
        skip_code(&report, "not_invoice").map(|entry| entry.count),
        Some(1)
    );
    assert_eq!(
        ItemRepository::new(pool.clone())
            .get_by_id(id)
            .await
            .unwrap()
            .non_invoice_reason
            .as_deref(),
        Some(LINK_NOT_INVOICE_REASON)
    );
    assert_eq!(
        std::fs::read(&placeholder).unwrap(),
        b"https://nnfp.jss.com.cn/5TQY1_XFJO-11SlG\r\n"
    );
}

#[tokio::test]
async fn a_network_failure_is_retryable_and_changes_nothing() {
    let (_directory, paths, pool) = empty_storage().await;
    let (id, placeholder) = insert_placeholder(
        &pool,
        &paths,
        b"https://nnfp.jss.com.cn/5TQY1_XFJO-11SlG\r\n",
        Utc::now(),
    )
    .await;
    let repository = ItemRepository::new(pool.clone());
    let before = repository.get_by_id(id).await.unwrap();
    let downloader = FakeDownloader::failure(DownloadFailureKind::Connection);
    let state = state_with(&pool, &paths, PdfOnlyExtractor::new(), downloader.clone());

    let report = items::recover_link_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.archived, 0, "a network failure must stay retryable");
    assert_eq!(report.not_invoice_portal, 0);
    assert_eq!(
        skip_code(&report, "download_failed").map(|entry| entry.label.as_str()),
        Some("下载失败")
    );
    assert_eq!(downloader.call_count(), 1);
    assert_eq!(
        repository.get_by_id(id).await.unwrap(),
        before,
        "a temporary failure must not touch the invoice"
    );
    assert_eq!(
        std::fs::read(&placeholder).unwrap(),
        b"https://nnfp.jss.com.cn/5TQY1_XFJO-11SlG\r\n"
    );

    // Still a candidate: the next run tries the link again.
    let second = items::recover_link_invoices(&state, None).await.unwrap();
    assert_eq!(second.examined, 1);
    assert_eq!(downloader.call_count(), 2);
}

#[tokio::test]
async fn an_expired_link_is_reported_but_not_archived_as_a_non_invoice_file() {
    let (_directory, paths, pool) = empty_storage().await;
    let (id, _) = insert_placeholder(
        &pool,
        &paths,
        b"https://fpkj.vpiaotong.com/invoice/expired\r\n",
        Utc::now(),
    )
    .await;
    let downloader = FakeDownloader::failure(DownloadFailureKind::Expired);
    let state = state_with(&pool, &paths, PdfOnlyExtractor::new(), downloader.clone());

    let report = items::recover_link_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.archived, 0);
    assert_eq!(
        skip_code(&report, "expired").map(|entry| entry.label.as_str()),
        Some("链接已失效")
    );
    assert_eq!(
        ItemRepository::new(pool.clone())
            .get_by_id(id)
            .await
            .unwrap()
            .non_invoice_reason,
        None,
        "an expired real invoice link stays in the queue for manual recovery"
    );
}

#[tokio::test]
async fn the_report_accounts_for_every_examined_item() {
    let (_directory, paths, pool) = empty_storage().await;
    // One success, one marketing link, one login wall, one invalid link.
    insert_placeholder(
        &pool,
        &paths,
        b"https://nnfp.jss.com.cn/success\r\n",
        Utc::now(),
    )
    .await;
    insert_placeholder(
        &pool,
        &paths,
        b"https://mx.tumblr.com/post\r\n",
        Utc::now() - Duration::minutes(1),
    )
    .await;
    insert_placeholder(
        &pool,
        &paths,
        b"https://pis.baiwang.com/login\r\n",
        Utc::now() - Duration::minutes(2),
    )
    .await;
    insert_placeholder(
        &pool,
        &paths,
        b"broken\r\n",
        Utc::now() - Duration::minutes(3),
    )
    .await;

    // A single fake outcome cannot mix, so this run uses the login failure and
    // the invariant is what the test is about.
    let downloader = FakeDownloader::failure(DownloadFailureKind::RequiresLogin);
    let state = state_with(&pool, &paths, PdfOnlyExtractor::new(), downloader);

    let report = items::recover_link_invoices(&state, None).await.unwrap();

    assert_eq!(report.examined, 4);
    assert_eq!(
        report.examined,
        report.recognized + report.skipped,
        "every examined item must be either rescued or skipped"
    );
    assert_eq!(report.skipped, 4);
    assert_eq!(
        report.archived, 3,
        "the marketing link, the login walls and the allowlisted login wall are all dismissed"
    );
    assert_eq!(report.not_invoice_portal, 1);
}

#[tokio::test]
async fn a_limit_caps_one_run_without_losing_the_rest() {
    let (_directory, paths, pool) = empty_storage().await;
    for index in 0..3 {
        insert_placeholder(
            &pool,
            &paths,
            b"https://nnfp.jss.com.cn/5TQY1_XFJO-11SlG\r\n",
            Utc::now() - Duration::minutes(index),
        )
        .await;
    }
    let downloader = FakeDownloader::invoice();
    let state = state_with(&pool, &paths, PdfOnlyExtractor::new(), downloader.clone());

    let first = items::recover_link_invoices(&state, Some(2)).await.unwrap();
    assert_eq!(first.examined, 2);
    assert_eq!(first.downloaded, 2);

    let second = items::recover_link_invoices(&state, Some(2)).await.unwrap();
    assert_eq!(
        second.examined, 1,
        "the third placeholder is still eligible"
    );
    assert_eq!(second.downloaded, 1);

    let third = items::recover_link_invoices(&state, Some(2)).await.unwrap();
    assert_eq!(third.examined, 0);
    assert_eq!(downloader.call_count(), 3);
}

#[tokio::test]
async fn a_single_placeholder_rescue_only_touches_the_requested_invoice() {
    let (_directory, paths, pool) = empty_storage().await;
    let (id, _) = insert_placeholder(
        &pool,
        &paths,
        b"https://nnfp.jss.com.cn/5TQY1_XFJO-11SlG\r\n",
        Utc::now(),
    )
    .await;
    let other = insert_placeholder(
        &pool,
        &paths,
        b"https://nnfp.jss.com.cn/other\r\n",
        Utc::now() - Duration::minutes(1),
    )
    .await
    .0;
    let downloader = FakeDownloader::invoice();
    let state = state_with(&pool, &paths, PdfOnlyExtractor::new(), downloader.clone());

    let report = items::recover_link_invoice(&state, id).await.unwrap();

    assert_eq!(report.examined, 1);
    assert_eq!(report.recognized, 1);
    assert_eq!(downloader.call_count(), 1);
    let repository = ItemRepository::new(pool.clone());
    assert_eq!(
        repository.get_by_id(other).await.unwrap().original_name,
        "download-1-2.url",
        "the untargeted placeholder must stay untouched"
    );

    // A repaired invoice is no longer a link target and the user-facing error
    // says so instead of silently reporting an empty report.
    let error = items::recover_link_invoice(&state, id)
        .await
        .expect_err("a repaired invoice must be rejected by the single-ticket entry");
    assert!(matches!(error, AppError::Validation { ref field, .. } if field == "item"));
}
