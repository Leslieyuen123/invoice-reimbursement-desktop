use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use invoice_reimbursement::commands::items::{self, RetryFailedRecognitionsReport};
use invoice_reimbursement::db;
use invoice_reimbursement::db::items::ItemRepository;
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::RecognitionStatus;
use invoice_reimbursement::infra::credentials::MemoryCredentialStore;
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::infra::imap::NativeTlsImapGateway;
use invoice_reimbursement::state::AppState;
use sqlx::SqlitePool;
use uuid::Uuid;

/// Succeeds for originals whose name contains `recoverable`, fails otherwise so
/// one invoice in the batch genuinely cannot be recognized.
struct ScriptedExtractor {
    extracted: Mutex<Vec<PathBuf>>,
}

impl ScriptedExtractor {
    fn new() -> Self {
        Self {
            extracted: Mutex::new(Vec::new()),
        }
    }

    fn extracted_paths(&self) -> Vec<PathBuf> {
        self.extracted.lock().unwrap().clone()
    }
}

impl DocumentExtractor for ScriptedExtractor {
    fn extract(&self, path: &Path) -> Result<ExtractedDocument, AppError> {
        self.extracted.lock().unwrap().push(path.to_path_buf());
        if path.to_string_lossy().contains("recoverable") {
            Ok(ExtractedDocument {
                text: "开票日期：2026年06月18日 餐饮 食品 价税合计（小写）￥128.50".to_owned(),
                normalized_pdf: Some(b"%PDF-1.4 recovered invoice".to_vec()),
                warnings: Vec::new(),
            })
        } else {
            Err(AppError::External {
                service: "document_extractor".to_owned(),
                retryable: false,
                message: "unsupported pdf object stream".to_owned(),
            })
        }
    }
}

fn state_with(pool: &SqlitePool, paths: &AppPaths, extractor: Arc<ScriptedExtractor>) -> AppState {
    AppState::with_gateway_and_extractor(
        pool.clone(),
        paths.clone(),
        Arc::new(MemoryCredentialStore::default()),
        Arc::new(NativeTlsImapGateway::default()),
        extractor,
    )
}

async fn insert_failed_item(
    pool: &SqlitePool,
    paths: &AppPaths,
    file_name: &str,
    mime_type: &str,
    note: &str,
    created_at: DateTime<Utc>,
) -> Uuid {
    let id = Uuid::new_v4();
    let original_path = paths.originals.join(format!("{id}-{file_name}"));
    sqlx::query(
        "INSERT INTO items (
            id, original_name, original_path, sha256, mime_type, source_type,
            fetched_at, currency, recognition_status, confirmation_status,
            dedupe_status, note, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, 'manual_upload', ?, 'CNY', 'failed', 'pending',
                   'unique', ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(file_name)
    .bind(original_path.to_string_lossy().into_owned())
    .bind(format!("sha-{id}"))
    .bind(mime_type)
    .bind(created_at.to_rfc3339())
    .bind(note)
    .bind(created_at.to_rfc3339())
    .bind(created_at.to_rfc3339())
    .execute(pool)
    .await
    .expect("failed invoice should insert");
    id
}

#[tokio::test]
async fn recoverable_failures_recover_and_link_placeholders_are_skipped() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let now = Utc::now();
    let recoverable = insert_failed_item(
        &pool,
        &paths,
        "recoverable.pdf",
        "application/pdf",
        "识别失败：PDF 使用了暂不支持的结构",
        now,
    )
    .await;
    let broken = insert_failed_item(
        &pool,
        &paths,
        "broken.pdf",
        "application/pdf",
        "识别失败：文件损坏或不是有效的发票文件",
        now - Duration::hours(1),
    )
    .await;
    let placeholder = insert_failed_item(
        &pool,
        &paths,
        "invoice-link.url",
        "text/uri-list",
        "识别失败：发票链接下载失败",
        now - Duration::hours(2),
    )
    .await;
    let extractor = Arc::new(ScriptedExtractor::new());
    let state = state_with(&pool, &paths, extractor.clone());

    let report = items::retry_failed_recognitions(&state, None)
        .await
        .unwrap();

    assert_eq!(
        report,
        RetryFailedRecognitionsReport {
            attempted: 2,
            recovered: 1,
            still_failed: 1,
            skipped: 1,
        }
    );

    let repository = ItemRepository::new(pool.clone());
    let recovered = repository.get_by_id(recoverable).await.unwrap();
    assert_eq!(
        recovered.recognition_status,
        RecognitionStatus::Succeeded,
        "a readable failure must recover"
    );
    assert!(
        recovered.normalized_pdf_path.is_some(),
        "recovery must persist the normalized PDF"
    );
    assert_eq!(
        recovered.note, None,
        "a stale recognition reason must be cleared after success"
    );

    let still_failed = repository.get_by_id(broken).await.unwrap();
    assert_eq!(still_failed.recognition_status, RecognitionStatus::Failed);
    let reason = still_failed.note.unwrap_or_default();
    assert!(
        reason.starts_with("识别失败：") && reason.contains(".pdf"),
        "the failure reason should be refreshed, got {reason:?}"
    );

    let skipped = repository.get_by_id(placeholder).await.unwrap();
    assert_eq!(skipped.recognition_status, RecognitionStatus::Failed);
    assert_eq!(
        skipped.note.as_deref(),
        Some("识别失败：发票链接下载失败"),
        "link placeholders must not be touched"
    );
    assert_eq!(skipped.normalized_pdf_path, None);
    assert_eq!(
        extractor.extracted_paths().len(),
        2,
        "only the two recognizable originals may be extracted"
    );
    assert!(
        !extractor
            .extracted_paths()
            .iter()
            .any(|path| path.to_string_lossy().contains("invoice-link.url")),
        "the .url placeholder must never reach the extractor"
    );
}

#[tokio::test]
async fn a_limit_caps_one_run_and_reports_the_leftovers_as_skipped() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let now = Utc::now();
    let newest = insert_failed_item(
        &pool,
        &paths,
        "recoverable-newest.pdf",
        "application/pdf",
        "识别失败：旧原因",
        now,
    )
    .await;
    let older = insert_failed_item(
        &pool,
        &paths,
        "recoverable-older.pdf",
        "application/pdf",
        "识别失败：旧原因",
        now - Duration::hours(1),
    )
    .await;
    let extractor = Arc::new(ScriptedExtractor::new());
    let state = state_with(&pool, &paths, extractor.clone());

    let report = items::retry_failed_recognitions(&state, Some(1))
        .await
        .unwrap();

    assert_eq!(
        report,
        RetryFailedRecognitionsReport {
            attempted: 1,
            recovered: 1,
            still_failed: 0,
            skipped: 1,
        }
    );
    let repository = ItemRepository::new(pool.clone());
    assert_eq!(
        repository
            .get_by_id(newest)
            .await
            .unwrap()
            .recognition_status,
        RecognitionStatus::Succeeded
    );
    assert_eq!(
        repository
            .get_by_id(older)
            .await
            .unwrap()
            .recognition_status,
        RecognitionStatus::Failed
    );
    assert_eq!(extractor.extracted_paths().len(), 1);
}

#[tokio::test]
async fn a_zero_limit_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let extractor = Arc::new(ScriptedExtractor::new());
    let state = state_with(&pool, &paths, extractor);

    let error = items::retry_failed_recognitions(&state, Some(0))
        .await
        .expect_err("a zero budget can never be useful");

    assert!(matches!(error, AppError::Validation { ref field, .. } if field == "limit"));
}
