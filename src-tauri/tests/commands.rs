use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use invoice_reimbursement::commands::{
    COMMAND_NAMES, batches, dashboard, export, items, settings, sync as sync_commands,
};
use invoice_reimbursement::db;
use invoice_reimbursement::db::accounts::{MailboxProvider, SyncCursor};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{Category, ItemStatus};
use invoice_reimbursement::infra::credentials::{CredentialStore, MemoryCredentialStore};
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::infra::imap::{ImapAccountConfig, ImapGateway, MailboxDelta};
use invoice_reimbursement::state::AppState;
use tokio::sync::Notify;
use uuid::Uuid;

struct TestApp {
    _directory: tempfile::TempDir,
    state: AppState,
}

impl TestApp {
    async fn with_dashboard_fixture() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let paths = AppPaths::create(directory.path().join("storage"))
            .expect("application paths should create");
        let pool = db::connect("sqlite::memory:")
            .await
            .expect("in-memory database should connect");
        let now = Utc::now();
        for (recognition, confirmation, dedupe, created_at) in [
            ("succeeded", "pending", "unique", now),
            ("succeeded", "pending", "unique", now - Duration::hours(1)),
            ("failed", "pending", "unique", now - Duration::days(1)),
            (
                "succeeded",
                "confirmed",
                "suspected_duplicate",
                now - Duration::days(8),
            ),
        ] {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO items (
                    id, original_name, original_path, sha256, mime_type, source_type,
                    fetched_at, currency, recognition_status, confirmation_status,
                    dedupe_status, created_at, updated_at
                 ) VALUES (?, ?, ?, ?, 'application/pdf', 'manual_upload', ?, 'CNY', ?, ?, ?, ?, ?)",
            )
            .bind(id.to_string())
            .bind(format!("{id}.pdf"))
            .bind(paths.originals.join(format!("{id}.pdf")).to_string_lossy())
            .bind(format!("sha-{id}"))
            .bind(created_at.to_rfc3339())
            .bind(recognition)
            .bind(confirmation)
            .bind(dedupe)
            .bind(created_at.to_rfc3339())
            .bind(created_at.to_rfc3339())
            .execute(&pool)
            .await
            .expect("dashboard item should insert");
        }
        sqlx::query(
            "INSERT INTO mailbox_accounts (
                id, provider, email, imap_host, imap_port, enabled,
                sync_interval_minutes, created_at, updated_at
             ) VALUES (?, 'gmail', 'owner@example.com', 'imap.example.com', 993, 1, 15, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(&pool)
        .await
        .expect("dashboard account should insert");
        sqlx::query(
            "INSERT INTO batches (
                id, name, start_date, end_date, status, note, created_at, updated_at
             ) VALUES (?, 'July', '2026-07-01', '2026-07-31', 'draft', NULL, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(&pool)
        .await
        .expect("dashboard batch should insert");

        Self {
            state: AppState::with_gateway(
                pool,
                paths,
                Arc::new(MemoryCredentialStore::default()),
                Arc::new(SuccessfulGateway),
            ),
            _directory: directory,
        }
    }
}

struct SuccessfulGateway;

struct SuccessfulExtractor;

impl DocumentExtractor for SuccessfulExtractor {
    fn extract(&self, _path: &std::path::Path) -> Result<ExtractedDocument, AppError> {
        Ok(ExtractedDocument {
            text: "开票日期：2026年07月10日 餐饮服务 价税合计 ¥128.50".to_owned(),
            normalized_pdf: None,
            warnings: Vec::new(),
        })
    }
}

#[async_trait]
impl ImapGateway for SuccessfulGateway {
    async fn test_connection(
        &self,
        _config: &ImapAccountConfig,
        _secret: &str,
    ) -> Result<(), AppError> {
        Ok(())
    }

    async fn fetch_since(
        &self,
        _config: &ImapAccountConfig,
        _secret: &str,
        _cursor: Option<SyncCursor>,
    ) -> Result<MailboxDelta, AppError> {
        Ok(MailboxDelta {
            uid_validity: 1,
            messages: Vec::new(),
            rejected_messages: Vec::new(),
            highest_uid: 0,
        })
    }
}

#[derive(Default)]
struct BlockingGateway {
    started: Notify,
    release: Notify,
}

#[async_trait]
impl ImapGateway for BlockingGateway {
    async fn test_connection(
        &self,
        _config: &ImapAccountConfig,
        _secret: &str,
    ) -> Result<(), AppError> {
        Ok(())
    }

    async fn fetch_since(
        &self,
        _config: &ImapAccountConfig,
        _secret: &str,
        _cursor: Option<SyncCursor>,
    ) -> Result<MailboxDelta, AppError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(MailboxDelta {
            uid_validity: 1,
            messages: Vec::new(),
            rejected_messages: Vec::new(),
            highest_uid: 0,
        })
    }
}

#[tokio::test]
async fn dashboard_counts_are_derived_from_persisted_state() {
    let app = TestApp::with_dashboard_fixture().await;

    let dto = dashboard::load(&app.state).await.unwrap();

    assert_eq!(dto.pending_confirmation_count, 2);
    assert_eq!(dto.recognition_failed_count, 1);
    assert_eq!(dto.suspected_duplicate_count, 1);
    assert_eq!(dto.recently_added_count, 3);
    assert_eq!(dto.mailbox_accounts.len(), 1);
    assert_eq!(dto.mailbox_accounts[0].email, "owner@example.com");
    assert_eq!(dto.recent_batches.len(), 1);
    assert_eq!(dto.recent_batches[0].name, "July");

    let json = serde_json::to_value(dto).unwrap();
    assert_eq!(json["pendingConfirmationCount"], 2);
    assert!(json.get("pending_confirmation_count").is_none());
}

#[tokio::test]
async fn dashboard_is_zeroed_for_a_new_installation() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let state = AppState::with_gateway(
        pool,
        paths,
        Arc::new(MemoryCredentialStore::default()),
        Arc::new(SuccessfulGateway),
    );

    let dto = dashboard::load(&state).await.unwrap();

    assert_eq!(dto.recently_added_count, 0);
    assert_eq!(dto.pending_confirmation_count, 0);
    assert_eq!(dto.recognition_failed_count, 0);
    assert_eq!(dto.suspected_duplicate_count, 0);
    assert!(dto.mailbox_accounts.is_empty());
    assert!(dto.recent_batches.is_empty());
}

#[test]
fn desktop_api_exposes_only_the_planned_command_names() {
    assert_eq!(
        COMMAND_NAMES,
        &[
            "get_dashboard",
            "list_items",
            "get_item",
            "import_manual_files",
            "review_item",
            "resolve_duplicate",
            "retry_recognition",
            "list_batches",
            "get_batch",
            "create_month_batch",
            "create_custom_batch",
            "assign_items_to_batch",
            "remove_item_from_batch",
            "export_batch",
            "list_mailbox_accounts",
            "save_mailbox_account",
            "test_mailbox_account",
            "delete_mailbox_account",
            "get_preferences",
            "save_preferences",
            "sync_account_now",
        ]
    );
}

#[test]
fn planned_commands_build_a_tauri_invoke_handler() {
    let _handler = invoice_reimbursement::commands::invoke_handler::<tauri::Wry>();
}

#[tokio::test]
async fn item_queries_return_safe_dtos_and_structured_errors() {
    let app = TestApp::with_dashboard_fixture().await;

    let failed = items::list(
        &app.state,
        items::ItemFilterDto {
            status: Some(ItemStatus::RecognitionFailed),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(failed.len(), 1);
    assert!(failed[0].preview_url.starts_with("invoice-file://item/"));
    assert!(failed[0].preview_url.ends_with("?variant=original"));
    let json = serde_json::to_value(&failed[0]).unwrap();
    assert!(json.get("originalPath").is_none());
    assert!(json.get("normalizedPdfPath").is_none());
    assert_eq!(json["recognitionStatus"], "failed");

    let missing = items::get(&app.state, Uuid::new_v4()).await.unwrap_err();
    assert!(matches!(missing, AppError::NotFound { .. }));
    assert_eq!(serde_json::to_value(missing).unwrap()["code"], "not_found");
}

#[tokio::test]
async fn batch_adapters_use_service_summaries_for_assignment_and_removal() {
    let app = TestApp::with_dashboard_fixture().await;
    let pending_item = items::list(
        &app.state,
        items::ItemFilterDto {
            status: Some(ItemStatus::PendingConfirmation),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .pop()
    .unwrap();
    let pending_item_id = Uuid::parse_str(&pending_item.id).unwrap();

    let batch = batches::create_month(&app.state, 2026, 8).await.unwrap();
    let batch_id = Uuid::parse_str(&batch.id).unwrap();
    assert_eq!(batch.name, "2026 年 8 月报销");

    let assigned = batches::assign(&app.state, batch_id, vec![pending_item_id])
        .await
        .unwrap();
    assert_eq!(assigned.batch.item_count, 1);
    assert_eq!(assigned.batch.unconfirmed_count, 1);
    assert_eq!(assigned.items[0].id, pending_item.id);

    let fetched = batches::get(&app.state, batch_id).await.unwrap();
    assert_eq!(fetched.batch, assigned.batch);

    let removed = batches::remove(&app.state, batch_id, pending_item_id)
        .await
        .unwrap();
    assert_eq!(removed.batch.item_count, 0);
    assert_eq!(removed.batch.unconfirmed_count, 0);

    let listed = batches::list(&app.state).await.unwrap();
    assert_eq!(listed[0], removed.batch);
}

#[tokio::test]
async fn settings_adapters_preserve_account_saga_and_preferences() {
    let app = TestApp::with_dashboard_fixture().await;
    assert_eq!(settings::list_accounts(&app.state).await.unwrap().len(), 1);
    assert!(
        settings::get_preferences(&app.state)
            .await
            .unwrap()
            .background_sync_enabled
    );

    let saved_preferences = settings::save_preferences(
        &app.state,
        settings::PreferencesInputDto {
            background_sync_enabled: false,
            export_directory: "exports/approved".to_owned(),
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
        },
    )
    .await
    .unwrap();
    assert!(!saved_preferences.background_sync_enabled);

    let new_account = settings::SaveMailboxAccountInput {
        id: None,
        provider: MailboxProvider::QQ,
        email: "finance@example.com".to_owned(),
        secret: "app-password".to_owned(),
        imap_host: None,
        imap_port: None,
        enabled: true,
        sync_interval_minutes: 30,
    };
    settings::test_account(&app.state, new_account.clone().into())
        .await
        .unwrap();
    let saved = settings::save_account(&app.state, new_account)
        .await
        .unwrap();
    assert_eq!(saved.email, "finance@example.com");
    assert_eq!(settings::list_accounts(&app.state).await.unwrap().len(), 2);

    settings::delete_account(&app.state, Uuid::parse_str(&saved.id).unwrap())
        .await
        .unwrap();
    assert_eq!(settings::list_accounts(&app.state).await.unwrap().len(), 1);
}

#[tokio::test]
async fn sync_now_reuses_the_application_account_lock() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let account_id = Uuid::new_v4();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO mailbox_accounts (
            id, provider, email, imap_host, imap_port, enabled,
            sync_interval_minutes, created_at, updated_at
         ) VALUES (?, 'gmail', 'sync@example.com', 'imap.example.com', 993, 1, 15, ?, ?)",
    )
    .bind(account_id.to_string())
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    let credentials = MemoryCredentialStore::default();
    credentials
        .set(&account_id.to_string(), "app-password")
        .unwrap();
    let gateway = Arc::new(BlockingGateway::default());
    let state = AppState::with_gateway(pool, paths, Arc::new(credentials), gateway.clone());

    let first_state = state.clone();
    let first = tokio::spawn(async move { sync_commands::now(&first_state, account_id).await });
    gateway.started.notified().await;

    let error = sync_commands::now(&state, account_id).await.unwrap_err();
    assert!(matches!(error, AppError::Conflict { .. }));

    gateway.release.notify_one();
    first.await.unwrap().unwrap();
}

#[tokio::test]
async fn item_write_adapters_delegate_to_import_recognition_and_review_services() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let state = AppState::with_gateway_and_extractor(
        pool,
        paths,
        Arc::new(MemoryCredentialStore::default()),
        Arc::new(SuccessfulGateway),
        Arc::new(SuccessfulExtractor),
    );
    let first_source = directory.path().join("first.pdf");
    let second_source = directory.path().join("second.pdf");
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text-invoice.pdf");
    std::fs::copy(&fixture, &first_source).unwrap();
    std::fs::copy(&fixture, &second_source).unwrap();

    let imported = items::import_manual(&state, vec![first_source.to_string_lossy().into_owned()])
        .await
        .unwrap();
    assert_eq!(imported.len(), 1);
    let item_id = Uuid::parse_str(&imported[0].id).unwrap();

    let recognized = items::retry_recognition(&state, item_id).await.unwrap();
    assert_eq!(recognized.amount_cents, Some(12_850));

    let reviewed = items::review(
        &state,
        items::ReviewItemInputDto {
            id: item_id,
            invoice_date: Some("2026-07-10".to_owned()),
            suggested_period: "2026-07".to_owned(),
            final_category: Category::Dining,
            amount_cents: 12_850,
            city: Some("北京".to_owned()),
            company: Some("测试餐厅".to_owned()),
            note: None,
            event_tag: None,
            project_tag: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(reviewed.status, ItemStatus::Ready);

    let duplicate =
        items::import_manual(&state, vec![second_source.to_string_lossy().into_owned()])
            .await
            .unwrap()
            .pop()
            .unwrap();
    assert_eq!(duplicate.status, ItemStatus::SuspectedDuplicate);
    let kept = items::resolve_duplicate(&state, Uuid::parse_str(&duplicate.id).unwrap(), true)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(kept.status, ItemStatus::SuspectedDuplicate);
}

#[tokio::test]
async fn multi_file_import_reports_each_result_without_stopping_after_an_error() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let state = AppState::with_gateway(
        pool,
        paths,
        Arc::new(MemoryCredentialStore::default()),
        Arc::new(SuccessfulGateway),
    );
    let missing = directory.path().join("missing.pdf");
    let valid = directory.path().join("valid.pdf");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text-invoice.pdf"),
        &valid,
    )
    .unwrap();

    let outcomes = items::import_manual_outcomes(
        &state,
        vec![
            missing.to_string_lossy().into_owned(),
            valid.to_string_lossy().into_owned(),
        ],
    )
    .await;

    assert_eq!(outcomes.len(), 2);
    assert!(matches!(
        &outcomes[0],
        items::ManualImportOutcomeDto::Failed { path, error }
            if path == missing.to_string_lossy().as_ref()
                && matches!(error, AppError::Validation { .. })
    ));
    assert!(matches!(
        &outcomes[1],
        items::ManualImportOutcomeDto::Imported { path, item }
            if path == valid.to_string_lossy().as_ref()
                && item.original_name == "valid.pdf"
    ));
    let wire = serde_json::to_value(&outcomes).unwrap();
    assert_eq!(wire[0]["status"], "failed");
    assert_eq!(wire[1]["status"], "imported");
    assert_eq!(wire[1]["path"], valid.to_string_lossy().as_ref());
    assert_eq!(wire[1]["item"]["originalName"], "valid.pdf");
    assert!(wire[1]["item"].is_object());
}

#[tokio::test]
async fn export_adapter_reuses_the_state_export_coordinator() {
    let app = TestApp::with_dashboard_fixture().await;
    let batch = batches::create_month(&app.state, 2026, 9).await.unwrap();

    let error = export::run(&app.state, Uuid::parse_str(&batch.id).unwrap())
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::Conflict { .. }));
}

#[tokio::test]
async fn preview_resolver_allows_only_database_owned_item_variants() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let item_id = Uuid::new_v4();
    let original = paths.originals.join(format!("{item_id}.pdf"));
    let normalized = paths.normalized.join(format!("{item_id}.pdf"));
    std::fs::write(&original, b"original-bytes").unwrap();
    std::fs::write(&normalized, b"normalized-bytes").unwrap();
    insert_preview_item(
        &pool,
        item_id,
        &original,
        Some(&normalized),
        "application/pdf",
    )
    .await;
    let state = AppState::with_gateway(
        pool,
        paths,
        Arc::new(MemoryCredentialStore::default()),
        Arc::new(SuccessfulGateway),
    );

    let (parsed_id, variant) =
        items::parse_preview_uri(&format!("invoice-file://item/{item_id}?variant=normalized"))
            .unwrap();
    assert_eq!(parsed_id, item_id);
    assert_eq!(variant, items::PreviewVariant::Normalized);
    assert!(items::parse_preview_uri(&format!("invoice-file://item/{item_id}")).is_err());
    assert!(
        items::parse_preview_uri("invoice-file://item/../../etc/passwd?variant=original").is_err()
    );

    let original_payload = items::open_preview(&state, item_id, items::PreviewVariant::Original)
        .await
        .unwrap();
    assert_eq!(original_payload.bytes, b"original-bytes");
    assert_eq!(original_payload.mime_type, "application/pdf");
    let normalized_payload =
        items::open_preview(&state, item_id, items::PreviewVariant::Normalized)
            .await
            .unwrap();
    assert_eq!(normalized_payload.bytes, b"normalized-bytes");

    let request = tauri::http::Request::builder()
        .method(tauri::http::Method::GET)
        .uri(format!("invoice-file://item/{item_id}?variant=normalized"))
        .header(tauri::http::header::RANGE, "bytes=0-0")
        .body(Vec::new())
        .unwrap();
    let response = items::preview_response(&state, &request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/pdf");
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.body(), b"normalized-bytes");
}

#[tokio::test]
async fn preview_error_responses_are_cors_readable_without_exposing_paths() {
    let app = TestApp::with_dashboard_fixture().await;
    let missing_id = Uuid::new_v4();
    let responses = [
        (
            items::preview_response(
                &app.state,
                &preview_request(
                    tauri::http::Method::GET,
                    "invoice-file://item/not-a-uuid?variant=original",
                ),
            )
            .await,
            tauri::http::StatusCode::BAD_REQUEST,
        ),
        (
            items::preview_response(
                &app.state,
                &preview_request(
                    tauri::http::Method::GET,
                    &format!("invoice-file://item/{missing_id}?variant=original"),
                ),
            )
            .await,
            tauri::http::StatusCode::NOT_FOUND,
        ),
        (
            items::preview_response(
                &app.state,
                &preview_request(
                    tauri::http::Method::POST,
                    &format!("invoice-file://item/{missing_id}?variant=original"),
                ),
            )
            .await,
            tauri::http::StatusCode::METHOD_NOT_ALLOWED,
        ),
    ];

    for (response, expected_status) in responses {
        assert_eq!(response.status(), expected_status);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        assert_eq!(
            response.headers()["content-type"],
            "text/plain; charset=utf-8"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.body(), b"Preview unavailable");
        assert!(!String::from_utf8_lossy(response.body()).contains("storage"));
    }
}

fn preview_request(method: tauri::http::Method, uri: &str) -> tauri::http::Request<Vec<u8>> {
    tauri::http::Request::builder()
        .method(method)
        .uri(uri)
        .body(Vec::new())
        .unwrap()
}

#[tokio::test]
async fn preview_resolver_rejects_escaped_and_symlinked_database_paths_without_leaking_them() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let outside = directory.path().join("private.pdf");
    std::fs::write(&outside, b"private").unwrap();
    let escaped_id = Uuid::new_v4();
    insert_preview_item(&pool, escaped_id, &outside, None, "application/pdf").await;

    #[cfg(unix)]
    let symlink_id = {
        use std::os::unix::fs::symlink;

        let id = Uuid::new_v4();
        let link = paths.originals.join(format!("{id}.pdf"));
        symlink(&outside, &link).unwrap();
        insert_preview_item(&pool, id, &link, None, "application/pdf").await;
        id
    };
    let state = AppState::with_gateway(
        pool,
        paths,
        Arc::new(MemoryCredentialStore::default()),
        Arc::new(SuccessfulGateway),
    );

    let escaped = items::open_preview(&state, escaped_id, items::PreviewVariant::Original)
        .await
        .unwrap_err();
    assert!(
        !escaped
            .to_string()
            .contains(outside.to_string_lossy().as_ref())
    );
    #[cfg(unix)]
    assert!(
        items::open_preview(&state, symlink_id, items::PreviewVariant::Original)
            .await
            .is_err()
    );
}

async fn insert_preview_item(
    pool: &sqlx::SqlitePool,
    id: Uuid,
    original: &std::path::Path,
    normalized: Option<&std::path::Path>,
    mime_type: &str,
) {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO items (
            id, original_name, original_path, normalized_pdf_path, sha256, mime_type,
            source_type, fetched_at, currency, recognition_status, confirmation_status,
            dedupe_status, created_at, updated_at
         ) VALUES (?, 'invoice.pdf', ?, ?, 'preview-sha', ?, 'manual_upload', ?, 'CNY',
                   'succeeded', 'pending', 'unique', ?, ?)",
    )
    .bind(id.to_string())
    .bind(original.to_string_lossy())
    .bind(normalized.map(|path| path.to_string_lossy().into_owned()))
    .bind(mime_type)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
}

#[test]
fn tauri_security_configuration_is_narrow_and_blocks_remote_scripts() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest.join("tauri.conf.json")).unwrap()).unwrap();
    let csp = config["app"]["security"]["csp"].as_str().unwrap();
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("script-src 'self'"));
    assert!(csp.contains("frame-src 'self' data: invoice-file:"));
    assert!(csp.contains("connect-src 'self' ipc: http://ipc.localhost invoice-file:"));
    assert!(!csp.contains("script-src 'unsafe-inline'"));
    assert!(!csp.contains("https:"));

    let capability: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest.join("capabilities/default.json")).unwrap())
            .unwrap();
    assert_eq!(
        capability["permissions"],
        serde_json::json!(["core:default", "dialog:allow-open"])
    );
}
