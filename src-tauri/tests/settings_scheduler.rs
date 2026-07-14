use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use chrono::{Duration, TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::accounts::{
    MailboxAccountRepository, MailboxProvider, NewMailboxAccount, SyncCursor,
};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::infra::credentials::{CredentialStore, MemoryCredentialStore};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::infra::imap::{ImapAccountConfig, ImapGateway, MailboxDelta};
use invoice_reimbursement::services::scheduler::{Scheduler, SyncRunner};
use invoice_reimbursement::services::settings::{
    Preferences, PreferencesInput, SaveAccountInput, SettingsService,
};
use invoice_reimbursement::state::AppState;
use uuid::Uuid;

#[derive(Default)]
struct PassingGateway;

#[async_trait]
impl ImapGateway for PassingGateway {
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
        unreachable!("settings tests do not fetch mail")
    }
}

struct FailingConnectionGateway;

#[async_trait]
impl ImapGateway for FailingConnectionGateway {
    async fn test_connection(
        &self,
        _config: &ImapAccountConfig,
        secret: &str,
    ) -> Result<(), AppError> {
        Err(AppError::External {
            service: "imap".to_owned(),
            retryable: false,
            message: format!("authentication failed for {secret}"),
        })
    }

    async fn fetch_since(
        &self,
        _config: &ImapAccountConfig,
        _secret: &str,
        _cursor: Option<SyncCursor>,
    ) -> Result<MailboxDelta, AppError> {
        unreachable!("settings tests do not fetch mail")
    }
}

#[derive(Default)]
struct ControlledCredentialStore {
    secrets: RwLock<HashMap<String, String>>,
    fail_set: AtomicBool,
    fail_delete: AtomicBool,
}

impl ControlledCredentialStore {
    fn set_fails(&self, value: bool) {
        self.fail_set.store(value, Ordering::SeqCst);
    }

    fn delete_fails(&self, value: bool) {
        self.fail_delete.store(value, Ordering::SeqCst);
    }
}

impl CredentialStore for ControlledCredentialStore {
    fn get(&self, account_id: &str) -> Result<Option<String>, AppError> {
        Ok(self.secrets.read().unwrap().get(account_id).cloned())
    }

    fn set(&self, account_id: &str, secret: &str) -> Result<(), AppError> {
        if self.fail_set.load(Ordering::SeqCst) {
            return Err(AppError::External {
                service: "keyring".to_owned(),
                retryable: false,
                message: "credential write failed".to_owned(),
            });
        }
        self.secrets
            .write()
            .unwrap()
            .insert(account_id.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, account_id: &str) -> Result<(), AppError> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(AppError::External {
                service: "keyring".to_owned(),
                retryable: false,
                message: "credential delete failed".to_owned(),
            });
        }
        self.secrets.write().unwrap().remove(account_id);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingRunner {
    account_ids: Mutex<Vec<Uuid>>,
}

struct SequenceRunner {
    results: Mutex<VecDeque<Result<(), AppError>>>,
    calls: AtomicUsize,
}

impl SequenceRunner {
    fn new(results: Vec<Result<(), AppError>>) -> Self {
        Self {
            results: Mutex::new(results.into()),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SyncRunner for SequenceRunner {
    async fn run(&self, _account_id: Uuid) -> Result<(), AppError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.results.lock().unwrap().pop_front().unwrap_or(Ok(()))
    }
}

struct BlockingRunner {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct FirstAccountBlockingRunner {
    first_account_id: Uuid,
    account_ids: Mutex<Vec<Uuid>>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl SyncRunner for FirstAccountBlockingRunner {
    async fn run(&self, account_id: Uuid) -> Result<(), AppError> {
        self.account_ids.lock().unwrap().push(account_id);
        if account_id == self.first_account_id {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}

#[async_trait]
impl SyncRunner for BlockingRunner {
    async fn run(&self, _account_id: Uuid) -> Result<(), AppError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[async_trait]
impl SyncRunner for RecordingRunner {
    async fn run(&self, account_id: Uuid) -> Result<(), AppError> {
        self.account_ids.lock().unwrap().push(account_id);
        Ok(())
    }
}

#[tokio::test]
async fn saves_account_metadata_but_secret_only_in_credential_store() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let service = SettingsService::new(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let secret = "gmail-app-secret";

    let account = service
        .save_account(SaveAccountInput {
            id: None,
            provider: MailboxProvider::Gmail,
            email: "invoices@example.com".to_owned(),
            secret: secret.to_owned(),
            imap_host: None,
            imap_port: None,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();

    assert_eq!(
        credentials.get(&account.id.to_string()).unwrap(),
        Some(secret.to_owned())
    );
    let persisted = sqlx::query_scalar::<_, String>(
        "SELECT group_concat(CAST(value AS TEXT), '|') FROM mailbox_accounts, json_each(json_object(\
            'id', mailbox_accounts.id, 'provider', provider, 'email', email, 'imap_host', imap_host, \
            'imap_port', imap_port, 'enabled', enabled, 'sync_interval_minutes', sync_interval_minutes, \
            'last_synced_at', last_synced_at, 'last_error', last_error, 'created_at', created_at, \
            'updated_at', updated_at)) WHERE mailbox_accounts.id = ?",
    )
    .bind(account.id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!persisted.contains(secret));
}

#[tokio::test]
async fn connection_failure_is_sanitized_and_does_not_save_account() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let service = SettingsService::new(
        pool.clone(),
        Arc::new(FailingConnectionGateway),
        credentials,
    );
    let secret = "must-not-leak";

    let error = service
        .save_account(save_input(None, "failed@example.com", secret))
        .await
        .unwrap_err();

    assert!(!error.to_string().contains(secret));
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mailbox_accounts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn invalid_imap_host_override_is_rejected_before_persistence() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let service = settings_service(pool.clone());
    let mut input = save_input(None, "invalid-host@example.com", "secret");
    input.imap_host = Some("imaps://imap.example.com/path".to_owned());

    let error = service.save_account(input).await.unwrap_err();

    assert!(matches!(error, AppError::Validation { ref field, .. } if field == "imap_host"));
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mailbox_accounts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn credential_set_failure_rolls_back_new_account_metadata() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(ControlledCredentialStore::default());
    credentials.set_fails(true);
    let service = SettingsService::new(pool.clone(), Arc::new(PassingGateway), credentials);

    service
        .save_account(save_input(None, "rollback@example.com", "new-secret"))
        .await
        .unwrap_err();

    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mailbox_accounts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn credential_set_failure_preserves_edited_account_and_old_secret() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(ControlledCredentialStore::default());
    let service = SettingsService::new(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let original = service
        .save_account(save_input(None, "original@example.com", "old-secret"))
        .await
        .unwrap();
    credentials.set_fails(true);
    let mut edit = save_input(Some(original.id), "changed@example.com", "new-secret");
    edit.enabled = false;

    service.save_account(edit).await.unwrap_err();

    let persisted = MailboxAccountRepository::new(pool)
        .get(original.id)
        .await
        .unwrap();
    assert_eq!(persisted.email, "original@example.com");
    assert!(persisted.enabled);
    assert_eq!(
        credentials.get(&original.id.to_string()).unwrap(),
        Some("old-secret".to_owned())
    );
}

#[tokio::test]
async fn credential_delete_failure_leaves_account_disabled_and_recoverable() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(ControlledCredentialStore::default());
    let service = SettingsService::new(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let account = service
        .save_account(save_input(None, "delete-fails@example.com", "old-secret"))
        .await
        .unwrap();
    credentials.delete_fails(true);

    service.delete_account(account.id).await.unwrap_err();

    let persisted = MailboxAccountRepository::new(pool)
        .get(account.id)
        .await
        .unwrap();
    assert!(!persisted.enabled);
    assert_eq!(
        credentials.get(&account.id.to_string()).unwrap(),
        Some("old-secret".to_owned())
    );
}

#[tokio::test]
async fn delete_account_removes_credential_and_metadata() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(ControlledCredentialStore::default());
    let service = SettingsService::new(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let account = service
        .save_account(save_input(None, "delete@example.com", "old-secret"))
        .await
        .unwrap();

    service.delete_account(account.id).await.unwrap();

    assert!(
        MailboxAccountRepository::new(pool)
            .get(account.id)
            .await
            .is_err()
    );
    assert_eq!(credentials.get(&account.id.to_string()).unwrap(), None);
}

#[tokio::test]
async fn preferences_have_stable_defaults_and_roundtrip_as_json() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let service = settings_service(pool.clone());

    assert_eq!(service.preferences().await.unwrap(), Preferences::default());
    let expected = Preferences {
        background_sync_enabled: false,
        export_directory: "/tmp/invoice-exports".to_owned(),
        batch_directory_pattern: "{start_date}_{end_date}".to_owned(),
    };
    let saved = service
        .save_preferences(PreferencesInput {
            background_sync_enabled: expected.background_sync_enabled,
            export_directory: expected.export_directory.clone(),
            batch_directory_pattern: expected.batch_directory_pattern.clone(),
        })
        .await
        .unwrap();

    assert_eq!(saved, expected);
    assert_eq!(
        settings_service(pool.clone()).preferences().await.unwrap(),
        expected
    );
    let raw = sqlx::query_scalar::<_, String>(
        "SELECT value_json FROM settings WHERE key = 'preferences'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(serde_json::from_str::<Preferences>(&raw).unwrap(), expected);
}

#[tokio::test]
async fn invalid_preferences_are_rejected_without_overwriting_saved_values() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let service = settings_service(pool.clone());
    let original = service
        .save_preferences(PreferencesInput {
            background_sync_enabled: true,
            export_directory: "/tmp/exports".to_owned(),
            batch_directory_pattern: "{start_date}-{end_date}".to_owned(),
        })
        .await
        .unwrap();

    let blank_error = service
        .save_preferences(PreferencesInput {
            background_sync_enabled: false,
            export_directory: "  ".to_owned(),
            batch_directory_pattern: "{start_date}-{end_date}".to_owned(),
        })
        .await
        .unwrap_err();
    let pattern_error = service
        .save_preferences(PreferencesInput {
            background_sync_enabled: false,
            export_directory: "/tmp/new".to_owned(),
            batch_directory_pattern: "../escape".to_owned(),
        })
        .await
        .unwrap_err();

    assert!(
        matches!(blank_error, AppError::Validation { ref field, .. } if field == "export_directory")
    );
    assert!(
        matches!(pattern_error, AppError::Validation { ref field, .. } if field == "batch_directory_pattern")
    );
    assert_eq!(service.preferences().await.unwrap(), original);
}

#[tokio::test]
async fn background_sync_disabled_prevents_new_scheduled_runs() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    insert_account(&accounts, "background-off@example.com", true).await;
    settings_service(pool.clone())
        .save_preferences(PreferencesInput {
            background_sync_enabled: false,
            export_directory: "exports".to_owned(),
            batch_directory_pattern: "{start_date}_{end_date}".to_owned(),
        })
        .await
        .unwrap();
    let runner = Arc::new(RecordingRunner::default());
    let scheduler = Scheduler::new(pool, runner.clone());

    scheduler.tick(Utc::now()).await.unwrap();

    assert!(runner.account_ids.lock().unwrap().is_empty());
}

#[tokio::test]
async fn disabling_background_sync_during_tick_allows_current_run_but_starts_no_next_run() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let first = insert_account(&accounts, "a-blocking@example.com", true).await;
    insert_account(&accounts, "z-next@example.com", true).await;
    let runner = Arc::new(FirstAccountBlockingRunner {
        first_account_id: first,
        account_ids: Mutex::new(Vec::new()),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let scheduler = Scheduler::new(pool.clone(), runner.clone());
    let running_scheduler = scheduler.clone();
    let tick = tokio::spawn(async move { running_scheduler.tick(Utc::now()).await });
    runner.started.notified().await;

    settings_service(pool)
        .save_preferences(PreferencesInput {
            background_sync_enabled: false,
            export_directory: "exports".to_owned(),
            batch_directory_pattern: "{start_date}_{end_date}".to_owned(),
        })
        .await
        .unwrap();
    runner.release.notify_one();
    tick.await.unwrap().unwrap();

    assert_eq!(*runner.account_ids.lock().unwrap(), vec![first]);
}

#[tokio::test]
async fn concurrent_sync_for_same_account_returns_conflict_without_waiting() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let account_id = insert_account(
        &MailboxAccountRepository::new(pool.clone()),
        "concurrent@example.com",
        true,
    )
    .await;
    let runner = Arc::new(BlockingRunner {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let scheduler = Scheduler::new(pool, runner.clone());
    let first_scheduler = scheduler.clone();
    let first = tokio::spawn(async move { first_scheduler.sync_now(account_id, Utc::now()).await });
    runner.started.notified().await;

    let second = scheduler.sync_now(account_id, Utc::now()).await;

    assert!(matches!(second, Err(AppError::Conflict { .. })));
    assert!(!first.is_finished());
    runner.release.notify_one();
    first.await.unwrap().unwrap();
}

#[tokio::test]
async fn retryable_failures_back_off_for_one_five_then_fifteen_minutes() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    insert_account(
        &MailboxAccountRepository::new(pool.clone()),
        "retry@example.com",
        true,
    )
    .await;
    let retryable = || AppError::External {
        service: "imap".to_owned(),
        retryable: true,
        message: "network unavailable".to_owned(),
    };
    let runner = Arc::new(SequenceRunner::new(vec![
        Err(retryable()),
        Err(retryable()),
        Err(retryable()),
        Err(retryable()),
    ]));
    let scheduler = Scheduler::new(pool, runner.clone());
    let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();

    scheduler.tick(start).await.unwrap();
    scheduler.tick(start + Duration::seconds(59)).await.unwrap();
    scheduler.tick(start + Duration::minutes(1)).await.unwrap();
    scheduler
        .tick(start + Duration::minutes(5) + Duration::seconds(59))
        .await
        .unwrap();
    scheduler.tick(start + Duration::minutes(6)).await.unwrap();
    scheduler
        .tick(start + Duration::minutes(20) + Duration::seconds(59))
        .await
        .unwrap();
    scheduler.tick(start + Duration::minutes(21)).await.unwrap();
    scheduler.tick(start + Duration::minutes(35)).await.unwrap();
    scheduler.tick(start + Duration::minutes(36)).await.unwrap();

    assert_eq!(runner.calls.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn authentication_failure_is_not_retried_and_preserves_last_error() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "auth@example.com", true).await;
    let runner = Arc::new(SequenceRunner::new(vec![Err(AppError::External {
        service: "imap".to_owned(),
        retryable: false,
        message: "authentication failed".to_owned(),
    })]));
    let scheduler = Scheduler::new(pool, runner.clone());
    let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();

    scheduler.tick(start).await.unwrap();
    scheduler.tick(start + Duration::days(1)).await.unwrap();

    assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        accounts
            .get(account_id)
            .await
            .unwrap()
            .last_error
            .as_deref(),
        Some("authentication failed")
    );
}

#[tokio::test]
async fn successful_manual_sync_clears_retry_state_and_last_error() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "recovered@example.com", true).await;
    let runner = Arc::new(SequenceRunner::new(vec![
        Err(AppError::External {
            service: "imap".to_owned(),
            retryable: true,
            message: "temporary failure".to_owned(),
        }),
        Ok(()),
    ]));
    let scheduler = Scheduler::new(pool, runner);
    let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    scheduler.tick(start).await.unwrap();
    assert!(scheduler.retry_state(account_id).is_some());
    assert!(accounts.get(account_id).await.unwrap().last_error.is_some());

    scheduler
        .sync_now(account_id, start + Duration::seconds(10))
        .await
        .unwrap();

    assert_eq!(scheduler.retry_state(account_id), None);
    assert_eq!(accounts.get(account_id).await.unwrap().last_error, None);
}

#[tokio::test]
async fn scheduler_loop_can_be_stopped_after_its_initial_tick() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    insert_account(
        &MailboxAccountRepository::new(pool.clone()),
        "loop@example.com",
        true,
    )
    .await;
    let runner = Arc::new(RecordingRunner::default());
    let scheduler = Scheduler::new(pool, runner.clone());
    let handle = scheduler.start();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if !runner.account_ids.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    handle.stop().await.unwrap();

    assert_eq!(runner.account_ids.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn app_state_builds_testable_settings_and_scheduler_services() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path()).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let state = AppState::with_gateway(pool.clone(), paths, credentials, Arc::new(PassingGateway));
    let account = state
        .settings_service()
        .save_account(save_input(None, "state@example.com", "state-secret"))
        .await
        .unwrap();
    let runner = Arc::new(RecordingRunner::default());

    state
        .scheduler(runner.clone())
        .tick(Utc::now())
        .await
        .unwrap();

    assert_eq!(*runner.account_ids.lock().unwrap(), vec![account.id]);
}

#[tokio::test]
async fn schedulers_from_same_app_state_share_the_account_guard() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path()).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let state = AppState::with_gateway(
        pool.clone(),
        paths,
        Arc::new(MemoryCredentialStore::default()),
        Arc::new(PassingGateway),
    );
    let account_id = insert_account(
        &MailboxAccountRepository::new(pool),
        "shared-guard@example.com",
        true,
    )
    .await;
    let runner = Arc::new(BlockingRunner {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let scheduled = state.scheduler(runner.clone());
    let manual = state.scheduler(runner.clone());
    let first = tokio::spawn(async move { scheduled.sync_now(account_id, Utc::now()).await });
    runner.started.notified().await;

    let second = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        manual.sync_now(account_id, Utc::now()),
    )
    .await;
    runner.release.notify_waiters();
    first.await.unwrap().unwrap();

    assert!(matches!(second, Ok(Err(AppError::Conflict { .. }))));
}

#[tokio::test]
async fn scheduler_only_runs_enabled_due_accounts() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let due = insert_account(&accounts, "due@example.com", true).await;
    let disabled = insert_account(&accounts, "disabled@example.com", false).await;
    let not_due = insert_account(&accounts, "fresh@example.com", true).await;
    sqlx::query("UPDATE mailbox_accounts SET last_synced_at = ? WHERE id = ?")
        .bind((now - Duration::minutes(5)).to_rfc3339())
        .bind(not_due.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let runner = Arc::new(RecordingRunner::default());
    let scheduler = Scheduler::new(pool, runner.clone());

    scheduler.tick(now).await.unwrap();

    assert_eq!(*runner.account_ids.lock().unwrap(), vec![due]);
    assert!(!runner.account_ids.lock().unwrap().contains(&disabled));
}

async fn insert_account(accounts: &MailboxAccountRepository, email: &str, enabled: bool) -> Uuid {
    accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: email.to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap()
        .id
}

fn save_input(id: Option<Uuid>, email: &str, secret: &str) -> SaveAccountInput {
    SaveAccountInput {
        id,
        provider: MailboxProvider::Gmail,
        email: email.to_owned(),
        secret: secret.to_owned(),
        imap_host: None,
        imap_port: None,
        enabled: true,
        sync_interval_minutes: 15,
    }
}

fn settings_service(pool: sqlx::SqlitePool) -> SettingsService {
    SettingsService::new(
        pool,
        Arc::new(PassingGateway),
        Arc::new(MemoryCredentialStore::default()),
    )
}
