use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use invoice_reimbursement::commands::{
    COMMAND_NAMES, batches, dashboard, export, items, settings, sync as sync_commands,
};
use invoice_reimbursement::db;
use invoice_reimbursement::db::accounts::MailboxAccountRepository;
use invoice_reimbursement::db::accounts::{MailboxProvider, SyncCursor};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{Category, ItemStatus};
use invoice_reimbursement::infra::credentials::{CredentialStore, MemoryCredentialStore};
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::infra::imap::{
    ImapAccountConfig, ImapDateRange, ImapGateway, MailboxDelta,
};
use invoice_reimbursement::services::batch_automation::{
    AccountAutomationFailure, BatchAutomationResult,
};
use invoice_reimbursement::services::export::ExportResult;
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

    async fn fetch_range(
        &self,
        config: &ImapAccountConfig,
        secret: &str,
        cursor: Option<SyncCursor>,
        _range: ImapDateRange,
    ) -> Result<MailboxDelta, AppError> {
        self.fetch_since(config, secret, cursor).await
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

    async fn fetch_range(
        &self,
        config: &ImapAccountConfig,
        secret: &str,
        cursor: Option<SyncCursor>,
        _range: ImapDateRange,
    ) -> Result<MailboxDelta, AppError> {
        self.fetch_since(config, secret, cursor).await
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
async fn every_dashboard_response_publishes_its_pending_count() {
    let app = TestApp::with_dashboard_fixture().await;
    let published = std::sync::Mutex::new(Vec::new());

    let dto = dashboard::load_and_publish(&app.state, |count| {
        published.lock().unwrap().push(count);
    })
    .await
    .unwrap();

    assert_eq!(dto.pending_confirmation_count, 2);
    assert_eq!(*published.lock().unwrap(), vec![2]);
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
            "open_item_original",
            "import_manual_files",
            "review_item",
            "resolve_duplicate",
            "retry_recognition",
            "list_batches",
            "get_batch",
            "list_batch_candidates",
            "create_month_batch",
            "create_custom_batch",
            "assign_items_to_batch",
            "remove_item_from_batch",
            "export_batch",
            "run_batch_automation",
            "list_mailbox_accounts",
            "save_mailbox_account",
            "test_mailbox_account",
            "delete_mailbox_account",
            "get_preferences",
            "save_preferences",
            "get_storage_status",
            "retry_export_recovery",
            "sync_account_now",
        ]
    );
}

#[test]
fn batch_automation_dto_maps_the_backend_result_without_rewriting_failure_messages() {
    let account_id = Uuid::new_v4();
    let result = BatchAutomationResult {
        scanned_account_count: 1,
        failed_accounts: vec![AccountAutomationFailure {
            account_id,
            email: "account@example.invalid".to_owned(),
            message: "backend-safe failure".to_owned(),
        }],
        imported_count: 2,
        assigned_count: 3,
        exception_count: 4,
        repaired_count: 6,
        export: Some(ExportResult {
            directory: "test-export".into(),
            item_count: 5,
            total_amount_cents: 12_850,
        }),
    };

    let dto = batches::BatchAutomationResultDto::try_from(result).unwrap();

    assert_eq!(dto.scanned_account_count, 1);
    assert_eq!(dto.failed_accounts.len(), 1);
    assert_eq!(dto.failed_accounts[0].account_id, account_id.to_string());
    assert_eq!(dto.failed_accounts[0].email, "account@example.invalid");
    assert_eq!(dto.failed_accounts[0].message, "backend-safe failure");
    assert_eq!(dto.imported_count, 2);
    assert_eq!(dto.assigned_count, 3);
    assert_eq!(dto.exception_count, 4);
    assert_eq!(dto.repaired_count, 6);
    assert_eq!(dto.export.unwrap().item_count, 5);
}

#[test]
fn batch_automation_dto_serializes_only_safe_camel_case_fields() {
    let dto = batches::BatchAutomationResultDto {
        scanned_account_count: 1,
        failed_accounts: vec![batches::AccountAutomationFailureDto {
            account_id: "account-id".to_owned(),
            email: "account@example.invalid".to_owned(),
            message: "backend-safe failure".to_owned(),
        }],
        imported_count: 2,
        assigned_count: 3,
        exception_count: 4,
        repaired_count: 0,
        export: None,
    };

    assert_eq!(
        serde_json::to_value(dto).unwrap(),
        serde_json::json!({
            "scannedAccountCount": 1,
            "failedAccounts": [{
                "accountId": "account-id",
                "email": "account@example.invalid",
                "message": "backend-safe failure",
            }],
            "importedCount": 2,
            "assignedCount": 3,
            "exceptionCount": 4,
            "repairedCount": 0,
            "export": null,
        })
    );
}

#[tokio::test]
async fn batch_automation_adapter_delegates_to_the_state_service() {
    let app = TestApp::with_dashboard_fixture().await;
    sqlx::query("DELETE FROM items")
        .execute(app.state.pool())
        .await
        .unwrap();
    let account_id = sqlx::query_scalar::<_, String>("SELECT id FROM mailbox_accounts LIMIT 1")
        .fetch_one(app.state.pool())
        .await
        .unwrap();
    app.state
        .credentials()
        .set(&account_id, "test-value")
        .unwrap();
    let batch_id = sqlx::query_scalar::<_, String>("SELECT id FROM batches LIMIT 1")
        .fetch_one(app.state.pool())
        .await
        .unwrap();

    let dto = batches::run_automation(&app.state, Uuid::parse_str(&batch_id).unwrap())
        .await
        .unwrap();

    assert_eq!(dto.scanned_account_count, 1);
    assert!(dto.failed_accounts.is_empty());
    assert_eq!(dto.imported_count, 0);
    assert_eq!(dto.assigned_count, 0);
    assert_eq!(dto.exception_count, 0);
    assert!(dto.export.is_none());
}

#[tokio::test]
async fn batch_automation_adapter_is_rejected_after_tracked_operations_close() {
    let app = TestApp::with_dashboard_fixture().await;
    let shutdown = app.state.begin_application_shutdown(None);

    let error = batches::run_automation(&app.state, Uuid::new_v4())
        .await
        .expect_err("batch automation must enter the tracked-operation gate");

    assert!(
        matches!(error, AppError::Conflict { ref message } if message.contains("shutting down"))
    );
    let report = shutdown.wait(std::time::Duration::from_secs(1)).await;
    assert!(!report.timed_out);
}

#[tokio::test]
async fn mailbox_dtos_expose_durable_network_failure_kind_and_time() {
    let app = TestApp::with_dashboard_fixture().await;
    let account_id = sqlx::query_scalar::<_, String>("SELECT id FROM mailbox_accounts LIMIT 1")
        .fetch_one(app.state.pool())
        .await
        .unwrap();
    let account_id = Uuid::parse_str(&account_id).unwrap();
    let failed_at = chrono::DateTime::parse_from_rfc3339("2026-07-17T09:40:00+08:00")
        .unwrap()
        .with_timezone(&Utc);
    MailboxAccountRepository::new(app.state.pool().clone())
        .record_retryable_failure(account_id, failed_at, "network timeout")
        .await
        .unwrap();

    let account = settings::list_accounts(&app.state)
        .await
        .unwrap()
        .into_iter()
        .find(|account| account.id == account_id.to_string())
        .unwrap();

    assert_eq!(
        account.last_error_kind,
        Some(settings::MailboxErrorKind::Network)
    );
    assert_eq!(
        account.last_error_at.as_deref(),
        Some("2026-07-17T01:40:00+00:00")
    );
}

#[tokio::test]
async fn mailbox_dtos_expose_durable_authentication_suspension() {
    let app = TestApp::with_dashboard_fixture().await;
    let account_id = sqlx::query_scalar::<_, String>("SELECT id FROM mailbox_accounts LIMIT 1")
        .fetch_one(app.state.pool())
        .await
        .unwrap();
    let account_id = Uuid::parse_str(&account_id).unwrap();
    let failed_at = chrono::DateTime::parse_from_rfc3339("2026-07-17T09:45:00+08:00")
        .unwrap()
        .with_timezone(&Utc);
    MailboxAccountRepository::new(app.state.pool().clone())
        .suspend_retry(account_id, failed_at, "authentication failed")
        .await
        .unwrap();

    let account = settings::list_accounts(&app.state)
        .await
        .unwrap()
        .into_iter()
        .find(|account| account.id == account_id.to_string())
        .unwrap();

    assert_eq!(
        account.last_error_kind,
        Some(settings::MailboxErrorKind::Authentication)
    );
    assert_eq!(
        account.last_error_at.as_deref(),
        Some("2026-07-17T01:45:00+00:00")
    );
}

#[tokio::test]
async fn storage_status_reports_actual_and_effective_directories() {
    let app = TestApp::with_dashboard_fixture().await;

    let status = settings::storage_status(&app.state).await.unwrap();

    assert_eq!(
        std::path::Path::new(&status.local_data_directory),
        app.state.paths().root.parent().unwrap()
    );
    assert_eq!(
        std::path::Path::new(&status.export_directory),
        app.state.paths().exports
    );
    assert!(status.available_bytes.is_some_and(|bytes| bytes > 0));
    assert_eq!(status.recovery_error, None);
}

#[tokio::test]
async fn missing_saved_export_root_is_reported_without_hiding_storage_settings_and_can_retry() {
    let app = TestApp::with_dashboard_fixture().await;
    let missing_root = app
        ._directory
        .path()
        .join("temporarily-unavailable-exports");
    let preferences = serde_json::json!({
        "backgroundSyncEnabled": true,
        "exportDirectory": missing_root.to_string_lossy(),
        "batchDirectoryPattern": "{batchName}-{timestamp}",
    });
    sqlx::query("INSERT INTO settings (key, value_json, updated_at) VALUES ('preferences', ?, ?)")
        .bind(preferences.to_string())
        .bind(Utc::now().to_rfc3339())
        .execute(app.state.pool())
        .await
        .unwrap();

    app.state
        .reconcile_exports()
        .await
        .expect_err("the unavailable saved root should defer export recovery");

    let unavailable = settings::storage_status(&app.state).await.unwrap();
    assert_eq!(
        std::path::Path::new(&unavailable.export_directory),
        missing_root
    );
    assert_eq!(unavailable.available_bytes, None);
    assert!(
        unavailable
            .recovery_error
            .as_deref()
            .is_some_and(|message| !message.is_empty())
    );

    std::fs::create_dir(&missing_root).unwrap();
    settings::retry_export_recovery(&app.state).await.unwrap();

    let recovered = settings::storage_status(&app.state).await.unwrap();
    assert!(recovered.available_bytes.is_some_and(|bytes| bytes > 0));
    assert_eq!(recovered.recovery_error, None);
}

#[tokio::test]
async fn export_recovery_error_is_scoped_to_the_root_that_failed() {
    let app = TestApp::with_dashboard_fixture().await;
    let missing_root = app._directory.path().join("old-unavailable-exports");
    let replacement_root = app._directory.path().join("replacement-exports");
    let preferences = serde_json::json!({
        "backgroundSyncEnabled": true,
        "exportDirectory": missing_root.to_string_lossy(),
        "batchDirectoryPattern": "{batchName}-{timestamp}",
    });
    sqlx::query("INSERT INTO settings (key, value_json, updated_at) VALUES ('preferences', ?, ?)")
        .bind(preferences.to_string())
        .bind(Utc::now().to_rfc3339())
        .execute(app.state.pool())
        .await
        .unwrap();
    app.state.reconcile_exports().await.unwrap_err();

    std::fs::create_dir(&replacement_root).unwrap();
    settings::save_preferences(
        &app.state,
        settings::PreferencesInputDto {
            background_sync_enabled: true,
            export_directory: replacement_root.to_string_lossy().into_owned(),
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
            mark_processed_mail_seen: true,
        },
    )
    .await
    .unwrap();

    let status = settings::storage_status(&app.state).await.unwrap();
    assert_eq!(status.recovery_error, None);
    assert_eq!(
        std::path::Path::new(&status.export_directory),
        replacement_root.canonicalize().unwrap()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn saving_unrelated_preferences_does_not_hide_a_recovery_failure() {
    let app = TestApp::with_dashboard_fixture().await;
    let recovery_directory = app
        .state
        .paths()
        .exports
        .join("malformed-save-preservation");
    let recovery_marker = recovery_directory.join(".invoice-export-recovery.json");
    std::fs::create_dir(&recovery_directory).unwrap();
    std::fs::write(&recovery_marker, b"{not-json").unwrap();

    app.state
        .reconcile_exports()
        .await
        .expect_err("the malformed marker must keep recovery pending");
    let failed = settings::storage_status(&app.state).await.unwrap();
    assert!(failed.recovery_error.is_some());

    let preferences = settings::get_preferences(&app.state).await.unwrap();
    settings::save_preferences(
        &app.state,
        settings::PreferencesInputDto {
            background_sync_enabled: !preferences.background_sync_enabled,
            export_directory: preferences.export_directory,
            batch_directory_pattern: preferences.batch_directory_pattern,
            mark_processed_mail_seen: true,
        },
    )
    .await
    .unwrap();

    let after_save = settings::storage_status(&app.state).await.unwrap();
    assert_eq!(after_save.recovery_error, failed.recovery_error);
    assert!(recovery_marker.is_file());

    std::fs::remove_file(&recovery_marker).unwrap();
    app.state.reconcile_exports().await.unwrap();
    assert_eq!(
        settings::storage_status(&app.state)
            .await
            .unwrap()
            .recovery_error,
        None
    );
}

#[tokio::test]
async fn successful_recovery_clears_only_the_repaired_export_root_failure() {
    let app = TestApp::with_dashboard_fixture().await;
    let root_a = app._directory.path().join("root-a");
    let root_b = app._directory.path().join("root-b");
    let preferences = serde_json::json!({
        "backgroundSyncEnabled": true,
        "exportDirectory": root_a.to_string_lossy(),
        "batchDirectoryPattern": "{batchName}-{timestamp}",
    });
    sqlx::query("INSERT INTO settings (key, value_json, updated_at) VALUES ('preferences', ?, ?)")
        .bind(preferences.to_string())
        .bind(Utc::now().to_rfc3339())
        .execute(app.state.pool())
        .await
        .unwrap();
    app.state.reconcile_exports().await.unwrap_err();

    std::fs::create_dir(&root_b).unwrap();
    settings::save_preferences(
        &app.state,
        settings::PreferencesInputDto {
            background_sync_enabled: true,
            export_directory: root_b.to_string_lossy().into_owned(),
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
            mark_processed_mail_seen: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        settings::storage_status(&app.state)
            .await
            .unwrap()
            .recovery_error,
        None
    );

    std::fs::create_dir(&root_a).unwrap();
    settings::save_preferences(
        &app.state,
        settings::PreferencesInputDto {
            background_sync_enabled: true,
            export_directory: root_a.to_string_lossy().into_owned(),
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
            mark_processed_mail_seen: true,
        },
    )
    .await
    .unwrap();

    assert!(
        settings::storage_status(&app.state)
            .await
            .unwrap()
            .recovery_error
            .is_some()
    );
    settings::retry_export_recovery(&app.state).await.unwrap();

    let repaired = settings::storage_status(&app.state).await.unwrap();
    assert_eq!(repaired.recovery_error, None);
    assert_eq!(
        std::path::Path::new(&repaired.export_directory),
        root_a.canonicalize().unwrap()
    );
}

#[tokio::test]
async fn manual_export_recovery_is_rejected_after_tracked_operations_close() {
    let app = TestApp::with_dashboard_fixture().await;
    let shutdown = app.state.begin_application_shutdown(None);

    let error = settings::retry_export_recovery(&app.state)
        .await
        .expect_err("manual recovery must enter the tracked-operation gate");

    assert!(
        matches!(error, AppError::Conflict { ref message } if message.contains("shutting down"))
    );
    let report = shutdown.wait(std::time::Duration::from_secs(1)).await;
    assert!(!report.timed_out);
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
async fn batch_candidate_date_hint_uses_received_date_only_for_email_items() {
    let app = TestApp::with_dashboard_fixture().await;
    let batch = batches::create_month(&app.state, 2026, 8).await.unwrap();
    let batch_id = Uuid::parse_str(&batch.id).unwrap();
    let account_id = sqlx::query_scalar::<_, String>("SELECT id FROM mailbox_accounts LIMIT 1")
        .fetch_one(app.state.pool())
        .await
        .unwrap();
    let email_item_id = Uuid::new_v4();
    let manual_item_id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO items (
            id, original_name, original_path, sha256, mime_type, source_type,
            source_account_id, source_mailbox, source_uid_validity, source_uid,
            source_message_id, source_part_id, fetched_at, source_received_date,
            invoice_date, company,
            currency, recognition_status, confirmation_status, dedupe_status,
            created_at, updated_at
         ) VALUES (?, 'email-membership-hint.pdf', ?, ?, 'application/pdf', 'email',
                   ?, 'INBOX', 1, 1, '<membership-hint@example.com>', '1',
                   '2026-08-15T08:00:00+00:00', '2026-08-15', '2026-07-31',
                   'membership-hint',
                   'CNY', 'succeeded', 'pending', 'unique',
                   '2026-08-15T08:00:00+00:00', '2026-08-15T08:00:00+00:00')",
    )
    .bind(email_item_id.to_string())
    .bind(
        app.state
            .paths()
            .originals
            .join(format!("{email_item_id}.pdf"))
            .to_string_lossy(),
    )
    .bind(format!("sha-{email_item_id}"))
    .bind(account_id)
    .execute(app.state.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items (
            id, original_name, original_path, sha256, mime_type, source_type,
            fetched_at, invoice_date, company, currency, recognition_status,
            confirmation_status, dedupe_status, created_at, updated_at
         ) VALUES (?, 'manual-membership-hint.pdf', ?, ?, 'application/pdf', 'manual_upload',
                   '2026-08-15T08:00:00+00:00', '2026-07-31', 'membership-hint',
                   'CNY', 'succeeded', 'pending', 'unique',
                   '2026-08-15T08:00:01+00:00', '2026-08-15T08:00:01+00:00')",
    )
    .bind(manual_item_id.to_string())
    .bind(
        app.state
            .paths()
            .originals
            .join(format!("{manual_item_id}.pdf"))
            .to_string_lossy(),
    )
    .bind(format!("sha-{manual_item_id}"))
    .execute(app.state.pool())
    .await
    .unwrap();

    let candidates = batches::list_candidates(
        &app.state,
        batch_id,
        Some("membership-hint".to_owned()),
        None,
    )
    .await
    .unwrap()
    .items;
    let email = candidates
        .iter()
        .find(|candidate| candidate.item.id == email_item_id.to_string())
        .unwrap();
    let manual = candidates
        .iter()
        .find(|candidate| candidate.item.id == manual_item_id.to_string())
        .unwrap();

    assert!(!email.outside_batch_range);
    assert!(manual.outside_batch_range);
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

    // An unconfirmed invoice would make the whole batch unexportable.
    let refused = batches::assign(&app.state, batch_id, vec![pending_item_id])
        .await
        .expect_err("unconfirmed invoices must not join a batch");
    assert!(matches!(refused, AppError::Conflict { .. }));

    let ready_item_id = Uuid::new_v4();
    let now = Utc::now().to_rfc3339();
    std::fs::write(
        app.state.paths().originals.join("ready.pdf"),
        b"%PDF-1.4 original",
    )
    .unwrap();
    std::fs::write(
        app.state.paths().normalized.join("ready.pdf"),
        b"%PDF-1.4 normalized",
    )
    .unwrap();
    sqlx::query(
        "INSERT INTO items (
            id, original_name, original_path, normalized_pdf_path, sha256, mime_type,
            source_type, fetched_at, invoice_date, suggested_period, final_category,
            amount_cents, currency, recognition_status, confirmation_status,
            dedupe_status, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, 'application/pdf', 'manual_upload', ?, '2026-08-10',
            '2026-08', 'dining', 12850, 'CNY', 'succeeded', 'confirmed', 'unique', ?, ?)",
    )
    .bind(ready_item_id.to_string())
    .bind("ready.pdf")
    .bind(
        app.state
            .paths()
            .originals
            .join("ready.pdf")
            .to_string_lossy(),
    )
    .bind(
        app.state
            .paths()
            .normalized
            .join("ready.pdf")
            .to_string_lossy(),
    )
    .bind("sha-ready")
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .execute(app.state.pool())
    .await
    .expect("ready item should insert");

    let assigned = batches::assign(&app.state, batch_id, vec![ready_item_id])
        .await
        .unwrap();
    assert_eq!(assigned.batch.item_count, 1);
    assert_eq!(assigned.batch.unconfirmed_count, 0);
    assert_eq!(assigned.items[0].id, ready_item_id.to_string());
    assert!(assigned.issues.is_empty());

    let fetched = batches::get(&app.state, batch_id).await.unwrap();
    assert_eq!(fetched.batch, assigned.batch);

    let removed = batches::remove(&app.state, batch_id, ready_item_id)
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
    let chosen_exports = app._directory.path().join("approved-exports");
    std::fs::create_dir(&chosen_exports).unwrap();
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
            export_directory: chosen_exports.to_string_lossy().into_owned(),
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
            mark_processed_mail_seen: true,
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
    assert_eq!(response.status(), tauri::http::StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["content-type"], "application/pdf");
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["accept-ranges"], "bytes");
    assert_eq!(response.headers()["content-length"], "1");
    assert_eq!(
        response.headers()["content-range"],
        format!("bytes 0-0/{}", b"normalized-bytes".len()).as_str()
    );
    assert_eq!(response.body(), b"n");

    let full_response = items::preview_response(
        &state,
        &preview_request(
            tauri::http::Method::GET,
            &format!("invoice-file://item/{item_id}?variant=normalized"),
        ),
    )
    .await;
    assert_eq!(full_response.status(), tauri::http::StatusCode::OK);
    assert_eq!(full_response.headers()["content-type"], "application/pdf");
    assert_eq!(full_response.body(), b"normalized-bytes");

    for unsupported_range in ["bytes=0-1", "bytes=0-0,2-2", "items=0-0"] {
        let request = tauri::http::Request::builder()
            .method(tauri::http::Method::GET)
            .uri(format!("invoice-file://item/{item_id}?variant=normalized"))
            .header(tauri::http::header::RANGE, unsupported_range)
            .body(Vec::new())
            .unwrap();
        let response = items::preview_response(&state, &request).await;
        assert_eq!(response.status(), tauri::http::StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        assert_eq!(response.body(), b"Preview unavailable");
    }
}

#[tokio::test]
async fn original_open_target_resolves_local_files_and_https_links_without_exposing_paths() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let archive_id = Uuid::new_v4();
    let archive = paths.originals.join(format!("{archive_id}.zip"));
    std::fs::write(&archive, b"PK\x03\x04archive").unwrap();
    insert_preview_item(&pool, archive_id, &archive, None, "application/zip").await;
    let link_id = Uuid::new_v4();
    let link = paths.originals.join(format!("{link_id}.url"));
    std::fs::write(&link, b"https://invoice.example/download\n").unwrap();
    insert_preview_item(&pool, link_id, &link, None, "text/uri-list").await;
    let state = AppState::with_gateway(
        pool,
        paths,
        Arc::new(MemoryCredentialStore::default()),
        Arc::new(SuccessfulGateway),
    );

    assert_eq!(
        items::resolve_original_open_target(&state, archive_id)
            .await
            .unwrap(),
        items::OriginalOpenTarget::LocalPath(archive)
    );
    assert_eq!(
        items::resolve_original_open_target(&state, link_id)
            .await
            .unwrap(),
        items::OriginalOpenTarget::ExternalUrl("https://invoice.example/download".to_owned())
    );
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
        serde_json::json!([
            "core:default",
            "dialog:allow-open",
            "opener:allow-reveal-item-in-dir",
            "opener:allow-open-url",
            "opener:allow-default-urls"
        ])
    );
}
