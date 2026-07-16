use std::future::pending;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use invoice_reimbursement::db;
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::infra::credentials::MemoryCredentialStore;
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::services::scheduler::SyncRunner;
use invoice_reimbursement::state::AppState;
use tokio::sync::Notify;
use uuid::Uuid;

struct TestState {
    _directory: tempfile::TempDir,
    state: AppState,
}

impl TestState {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        Self {
            state: AppState::new(pool, paths, Arc::new(MemoryCredentialStore::default())),
            _directory: directory,
        }
    }
}

#[tokio::test]
async fn graceful_shutdown_completes_without_errors_and_is_repeatable() {
    let app = TestState::new().await;

    let first = app
        .state
        .begin_application_shutdown(None)
        .wait(Duration::from_secs(1))
        .await;
    let second = app
        .state
        .begin_application_shutdown(None)
        .wait(Duration::from_secs(1))
        .await;

    assert!(!first.timed_out);
    assert!(first.errors.is_empty());
    assert!(!second.timed_out);
    assert!(second.errors.is_empty());
}

#[tokio::test]
async fn graceful_shutdown_reports_a_completed_tracked_operation_error() {
    let app = TestState::new().await;
    let state = app.state.clone();
    let operation = tokio::spawn(async move {
        state
            .run_tracked_operation(async {
                Err::<(), _>(AppError::Internal {
                    message: "tracked operation failed".to_owned(),
                })
            })
            .await
    });
    assert!(operation.await.unwrap().is_err());

    let report = app
        .state
        .begin_application_shutdown(None)
        .wait(Duration::from_secs(1))
        .await;

    assert!(!report.timed_out);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("tracked operation failed"))
    );
}

#[tokio::test]
async fn graceful_shutdown_does_not_repeat_completed_business_errors() {
    let app = TestState::new().await;
    let validation = app
        .state
        .run_tracked_operation(async { Err::<(), _>(AppError::validation("name", "invalid")) })
        .await;
    assert!(matches!(validation, Err(AppError::Validation { .. })));
    let conflict = app
        .state
        .run_tracked_operation(async {
            Err::<(), _>(AppError::Conflict {
                message: "already handled by caller".to_owned(),
            })
        })
        .await;
    assert!(matches!(conflict, Err(AppError::Conflict { .. })));

    let report = app
        .state
        .begin_application_shutdown(None)
        .wait(Duration::from_secs(1))
        .await;

    assert!(!report.timed_out);
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
}

#[tokio::test]
async fn beginning_application_shutdown_closes_the_export_gate() {
    let app = TestState::new().await;
    let shutdown = app.state.begin_application_shutdown(None);

    let error = app
        .state
        .export_service()
        .export(Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(
        matches!(error, AppError::Conflict { ref message } if message.contains("shutting down"))
    );

    let report = shutdown.wait(Duration::from_secs(1)).await;
    assert!(!report.timed_out);
}

#[tokio::test]
async fn shutdown_timeout_aborts_pending_operations_and_marks_running_syncs_interrupted() {
    let app = TestState::new().await;
    let account_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO mailbox_accounts (
            id, provider, email, imap_host, imap_port, enabled,
            sync_interval_minutes, created_at, updated_at
         ) VALUES (?, 'gmail', 'shutdown@example.com', 'imap.example.com', 993, 1, 15, ?, ?)",
    )
    .bind(account_id.to_string())
    .bind(&now)
    .bind(&now)
    .execute(app.state.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sync_runs (id, account_id, started_at, status)
         VALUES (?, ?, ?, 'running')",
    )
    .bind(run_id.to_string())
    .bind(account_id.to_string())
    .bind(&now)
    .execute(app.state.pool())
    .await
    .unwrap();

    let started = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let state = app.state.clone();
    let operation_started = started.clone();
    let operation_dropped = dropped.clone();
    let operation = tokio::spawn(async move {
        state
            .run_tracked_operation(async move {
                let _drop_signal = DropSignal(operation_dropped);
                operation_started.notify_one();
                pending::<Result<(), AppError>>().await
            })
            .await
    });
    started.notified().await;

    let report = app
        .state
        .begin_application_shutdown(None)
        .wait(Duration::from_millis(20))
        .await;

    assert!(report.timed_out);
    assert_eq!(report.interrupted_sync_runs, 1);
    tokio::time::timeout(Duration::from_secs(1), dropped.notified())
        .await
        .expect("aborted operation must drop before shutdown returns");
    assert!(operation.await.unwrap().is_err());
    let persisted = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, error_message FROM sync_runs WHERE id = ?",
    )
    .bind(run_id.to_string())
    .fetch_one(app.state.pool())
    .await
    .unwrap();
    assert_eq!(persisted.0, "failed");
    assert_eq!(
        persisted.1.as_deref(),
        Some("application_shutdown_interrupted")
    );
}

#[tokio::test]
async fn forced_shutdown_retains_an_earlier_tracked_operation_error() {
    let app = TestState::new().await;
    let state = app.state.clone();
    assert!(
        state
            .run_tracked_operation(async {
                Err::<(), _>(AppError::Internal {
                    message: "operation failed before timeout".to_owned(),
                })
            })
            .await
            .is_err()
    );
    let started = Arc::new(Notify::new());
    let operation_started = started.clone();
    let state = app.state.clone();
    let pending_operation = tokio::spawn(async move {
        state
            .run_tracked_operation(async move {
                operation_started.notify_one();
                pending::<Result<(), AppError>>().await
            })
            .await
    });
    started.notified().await;

    let report = app
        .state
        .begin_application_shutdown(None)
        .wait(Duration::from_millis(20))
        .await;

    assert!(report.timed_out);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("operation failed before timeout"))
    );
    assert!(pending_operation.await.unwrap().is_err());
}

struct DropSignal(Arc<Notify>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

struct PendingRunner {
    started: Arc<Notify>,
    dropped: Arc<Notify>,
}

#[async_trait]
impl SyncRunner for PendingRunner {
    async fn run(&self, _account_id: Uuid) -> Result<(), AppError> {
        let _drop_signal = DropSignal(self.dropped.clone());
        self.started.notify_one();
        pending().await
    }
}

#[tokio::test]
async fn shutdown_timeout_aborts_a_permanently_blocked_scheduler_runner() {
    let app = TestState::new().await;
    let account_id = Uuid::new_v4();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO mailbox_accounts (
            id, provider, email, imap_host, imap_port, enabled,
            sync_interval_minutes, created_at, updated_at
         ) VALUES (?, 'gmail', 'runner@example.com', 'imap.example.com', 993, 1, 15, ?, ?)",
    )
    .bind(account_id.to_string())
    .bind(&now)
    .bind(&now)
    .execute(app.state.pool())
    .await
    .unwrap();
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let handle = app
        .state
        .scheduler(Arc::new(PendingRunner {
            started: started.clone(),
            dropped: dropped.clone(),
        }))
        .start();
    started.notified().await;

    let report = app
        .state
        .begin_application_shutdown(Some(handle))
        .wait(Duration::from_millis(20))
        .await;

    assert!(report.timed_out);
    tokio::time::timeout(Duration::from_secs(1), dropped.notified())
        .await
        .expect("scheduler runner must be dropped before shutdown returns");
}
