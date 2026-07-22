use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};

use async_trait::async_trait;
use chrono::{Duration, TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::accounts::{
    MailboxAccountRepository, MailboxProvider, NewMailboxAccount, SyncCursor,
};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::infra::credentials::{CredentialStore, MemoryCredentialStore};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::infra::imap::{
    ImapAccountConfig, ImapDateRange, ImapGateway, MailboxDelta,
};
use invoice_reimbursement::services::scheduler::{
    Clock, ManualClock, Scheduler, SyncRunner, SyncStartBarrier,
};
use invoice_reimbursement::services::settings::{
    Preferences, PreferencesInput, SaveAccountInput, SettingsService, TestAccountInput,
};
use invoice_reimbursement::state::AppState;
use sqlx::sqlite::SqlitePoolOptions;
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

struct FailingConnectionGateway;

#[derive(Default)]
struct CapturingGateway {
    tested_secrets: Mutex<Vec<String>>,
}

#[async_trait]
impl ImapGateway for CapturingGateway {
    async fn test_connection(
        &self,
        _config: &ImapAccountConfig,
        secret: &str,
    ) -> Result<(), AppError> {
        self.tested_secrets.lock().unwrap().push(secret.to_owned());
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
struct ControlledCredentialStore {
    secrets: RwLock<HashMap<String, String>>,
    fail_set: AtomicBool,
    fail_delete: AtomicBool,
    set_calls: AtomicUsize,
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
        self.set_calls.fetch_add(1, Ordering::SeqCst);
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
struct NewAccountRecoveryFailureCredentialStore;

impl CredentialStore for NewAccountRecoveryFailureCredentialStore {
    fn get(&self, _account_id: &str) -> Result<Option<String>, AppError> {
        Ok(None)
    }

    fn set(&self, _account_id: &str, secret: &str) -> Result<(), AppError> {
        Err(AppError::External {
            service: "keyring".to_owned(),
            retryable: false,
            message: format!("new credential failed\n{secret}\0{}", "y".repeat(700)),
        })
    }

    fn delete(&self, _account_id: &str) -> Result<(), AppError> {
        Err(AppError::External {
            service: "keyring".to_owned(),
            retryable: false,
            message: format!("delete leaked new-secret\n\0{}", "z".repeat(700)),
        })
    }
}

#[derive(Default)]
struct BlockingCredentialStore {
    secrets: RwLock<HashMap<String, String>>,
    block_next_set: AtomicBool,
    fail_blocked_set: AtomicBool,
    set_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    released: Mutex<bool>,
    release_signal: Condvar,
}

#[derive(Default)]
struct FailNextLiveCredentialStore {
    secrets: RwLock<HashMap<String, String>>,
    fail_next_live_set: AtomicBool,
}

#[derive(Default)]
struct PermanentTempDeleteFailureStore {
    secrets: RwLock<HashMap<String, String>>,
    failed_temp_key: RwLock<Option<String>>,
    fail_all_temp_deletes: AtomicBool,
    failed_delete_attempts: AtomicUsize,
}

impl PermanentTempDeleteFailureStore {
    fn fail_delete_for(&self, key: String) {
        *self.failed_temp_key.write().unwrap() = Some(key);
    }

    fn fail_all_temp_deletes(&self, fail: bool) {
        self.fail_all_temp_deletes.store(fail, Ordering::SeqCst);
    }

    fn failed_delete_attempts(&self) -> usize {
        self.failed_delete_attempts.load(Ordering::SeqCst)
    }
}

impl CredentialStore for PermanentTempDeleteFailureStore {
    fn get(&self, account_id: &str) -> Result<Option<String>, AppError> {
        Ok(self.secrets.read().unwrap().get(account_id).cloned())
    }

    fn set(&self, account_id: &str, secret: &str) -> Result<(), AppError> {
        self.secrets
            .write()
            .unwrap()
            .insert(account_id.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, account_id: &str) -> Result<(), AppError> {
        if self.failed_temp_key.read().unwrap().as_deref() == Some(account_id)
            || (account_id.starts_with("pending-save:")
                && self.fail_all_temp_deletes.load(Ordering::SeqCst))
        {
            self.failed_delete_attempts.fetch_add(1, Ordering::SeqCst);
            return Err(AppError::External {
                service: "keyring".to_owned(),
                retryable: false,
                message: "cleanup failed for cleanup-secret-must-not-leak\n\0".to_owned(),
            });
        }
        self.secrets.write().unwrap().remove(account_id);
        Ok(())
    }
}

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl FailNextLiveCredentialStore {
    fn fail_next_live_set(&self) {
        self.fail_next_live_set.store(true, Ordering::SeqCst);
    }
}

impl CredentialStore for FailNextLiveCredentialStore {
    fn get(&self, account_id: &str) -> Result<Option<String>, AppError> {
        Ok(self.secrets.read().unwrap().get(account_id).cloned())
    }

    fn set(&self, account_id: &str, secret: &str) -> Result<(), AppError> {
        if !account_id.starts_with("pending-save:")
            && self.fail_next_live_set.swap(false, Ordering::SeqCst)
        {
            return Err(AppError::External {
                service: "keyring".to_owned(),
                retryable: false,
                message: format!("failed to publish live credential {secret}\n\0"),
            });
        }
        self.secrets
            .write()
            .unwrap()
            .insert(account_id.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, account_id: &str) -> Result<(), AppError> {
        self.secrets.write().unwrap().remove(account_id);
        Ok(())
    }
}

impl BlockingCredentialStore {
    fn block_next_set(&self) -> tokio::sync::oneshot::Receiver<()> {
        self.prepare_blocked_set(false)
    }

    fn block_next_set_with_failure(&self) -> tokio::sync::oneshot::Receiver<()> {
        self.prepare_blocked_set(true)
    }

    fn prepare_blocked_set(&self, fail: bool) -> tokio::sync::oneshot::Receiver<()> {
        let (started, receiver) = tokio::sync::oneshot::channel();
        *self.set_started.lock().unwrap() = Some(started);
        *self.released.lock().unwrap() = false;
        self.fail_blocked_set.store(fail, Ordering::SeqCst);
        self.block_next_set.store(true, Ordering::SeqCst);
        receiver
    }

    fn release_set(&self) {
        *self.released.lock().unwrap() = true;
        self.release_signal.notify_all();
    }
}

impl CredentialStore for BlockingCredentialStore {
    fn get(&self, account_id: &str) -> Result<Option<String>, AppError> {
        Ok(self.secrets.read().unwrap().get(account_id).cloned())
    }

    fn set(&self, account_id: &str, secret: &str) -> Result<(), AppError> {
        if self.block_next_set.swap(false, Ordering::SeqCst) {
            if let Some(started) = self.set_started.lock().unwrap().take() {
                let _ = started.send(());
            }
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.release_signal.wait(released).unwrap();
            }
            if self.fail_blocked_set.swap(false, Ordering::SeqCst) {
                return Err(AppError::External {
                    service: "keyring".to_owned(),
                    retryable: false,
                    message: format!("credential write failed for {secret}"),
                });
            }
        }
        self.secrets
            .write()
            .unwrap()
            .insert(account_id.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, account_id: &str) -> Result<(), AppError> {
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

struct BlockingFailureRunner {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct SlowFirstRunner {
    slow_account_id: Uuid,
    slow_started: tokio::sync::Notify,
    fast_started: tokio::sync::Notify,
    release_slow: tokio::sync::Notify,
}

#[async_trait]
impl SyncRunner for SlowFirstRunner {
    async fn run(&self, account_id: Uuid) -> Result<(), AppError> {
        if account_id == self.slow_account_id {
            self.slow_started.notify_one();
            self.release_slow.notified().await;
        } else {
            self.fast_started.notify_one();
        }
        Ok(())
    }
}

struct CompletionBlockingRunner {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    completed: AtomicBool,
    completed_signal: tokio::sync::Notify,
}

struct PanicAndBlockRunner {
    panic_account_id: Uuid,
    release_panic: tokio::sync::Notify,
    blocked_started: tokio::sync::Notify,
    release_blocked: tokio::sync::Notify,
    blocked_completed: AtomicBool,
    blocked_completed_signal: tokio::sync::Notify,
}

#[async_trait]
impl SyncRunner for PanicAndBlockRunner {
    async fn run(&self, account_id: Uuid) -> Result<(), AppError> {
        if account_id == self.panic_account_id {
            self.release_panic.notified().await;
            panic!("injected scheduler child panic");
        }
        self.blocked_started.notify_one();
        self.release_blocked.notified().await;
        self.blocked_completed.store(true, Ordering::SeqCst);
        self.blocked_completed_signal.notify_one();
        Ok(())
    }
}

#[async_trait]
impl SyncRunner for CompletionBlockingRunner {
    async fn run(&self, _account_id: Uuid) -> Result<(), AppError> {
        self.started.notify_one();
        self.release.notified().await;
        self.completed.store(true, Ordering::SeqCst);
        self.completed_signal.notify_one();
        Ok(())
    }
}

#[async_trait]
impl SyncRunner for BlockingFailureRunner {
    async fn run(&self, _account_id: Uuid) -> Result<(), AppError> {
        self.started.notify_one();
        self.release.notified().await;
        Err(AppError::External {
            service: "imap".to_owned(),
            retryable: true,
            message: "network unavailable".to_owned(),
        })
    }
}

struct FirstAccountBlockingRunner {
    first_account_id: Uuid,
    account_ids: Mutex<Vec<Uuid>>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct ControlledStartBarrier {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl SyncStartBarrier for ControlledStartBarrier {
    async fn before_start(&self) {
        self.reached.notify_one();
        self.release.notified().await;
    }
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
    let service =
        settings_service_with(pool.clone(), Arc::new(PassingGateway), credentials.clone());
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
    let service = settings_service_with(
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
    let service = settings_service_with(pool.clone(), Arc::new(PassingGateway), credentials);

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
    let service =
        settings_service_with(pool.clone(), Arc::new(PassingGateway), credentials.clone());
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
async fn duplicate_email_edit_is_rejected_before_any_credential_is_staged() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(ControlledCredentialStore::default());
    let service =
        settings_service_with(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let first = service
        .save_account(save_input(None, "first@example.com", "first-secret"))
        .await
        .unwrap();
    service
        .save_account(save_input(None, "second@example.com", "second-secret"))
        .await
        .unwrap();
    let writes_before_conflict = credentials.set_calls.load(Ordering::SeqCst);

    let error = service
        .save_account(save_input(
            Some(first.id),
            "second@example.com",
            "replacement-secret",
        ))
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::Conflict { .. }));
    assert_eq!(
        MailboxAccountRepository::new(pool.clone())
            .get(first.id)
            .await
            .unwrap()
            .email,
        "first@example.com"
    );
    assert_eq!(
        credentials.get(&first.id.to_string()).unwrap().as_deref(),
        Some("first-secret")
    );
    assert_eq!(
        credentials.set_calls.load(Ordering::SeqCst),
        writes_before_conflict
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_account_saves")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn final_credential_failure_does_not_expose_either_secret() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(FailNextLiveCredentialStore::default());
    let service =
        settings_service_with(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let account = service
        .save_account(save_input(None, "recovery@example.com", "old-secret"))
        .await
        .unwrap();
    credentials.fail_next_live_set();

    let error = service
        .save_account(save_input(
            Some(account.id),
            "changed@example.com",
            "new-secret",
        ))
        .await
        .unwrap_err();
    let message = error.to_string();

    assert!(matches!(error, AppError::External { .. }));
    assert!(!message.contains("old-secret"));
    assert!(!message.contains("new-secret"));
    assert!(!message.chars().any(char::is_control));
    assert!(message.chars().count() <= 512);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT phase FROM pending_account_saves")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "credential_staged"
    );
    assert_eq!(
        MailboxAccountRepository::new(pool.clone())
            .get(account.id)
            .await
            .unwrap()
            .email,
        "recovery@example.com"
    );
    assert_eq!(
        credentials.get(&account.id.to_string()).unwrap(),
        Some("old-secret".to_owned())
    );
}

#[tokio::test]
async fn temp_credential_failure_is_bounded_redacted_and_reconciles_marker_only() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(NewAccountRecoveryFailureCredentialStore);
    let state = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials);
    let service = state.settings_service();

    let error = service
        .save_account(save_input(
            None,
            "new-recovery-failure@example.com",
            "new-secret",
        ))
        .await
        .unwrap_err();
    let message = error.to_string();

    assert!(matches!(error, AppError::External { .. }));
    assert!(!message.contains("new-secret"));
    assert!(!message.chars().any(char::is_control));
    assert!(message.chars().count() <= 512);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mailbox_accounts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_account_saves")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );

    state.reconcile_account_saves().await.unwrap();

    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_account_saves")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn blank_secret_edit_stages_and_republishes_old_credential() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(ControlledCredentialStore::default());
    let gateway = Arc::new(CapturingGateway::default());
    let service = settings_service_with(pool.clone(), gateway.clone(), credentials.clone());
    let original = service
        .save_account(save_input(None, "blank-edit@example.com", "old-secret"))
        .await
        .unwrap();
    let mut edit = save_input(Some(original.id), "edited@example.com", "   ");
    edit.enabled = false;

    let edited = service.save_account(edit).await.unwrap();

    assert_eq!(edited.email, "edited@example.com");
    assert!(!edited.enabled);
    assert_eq!(
        credentials.get(&original.id.to_string()).unwrap(),
        Some("old-secret".to_owned())
    );
    assert_eq!(credentials.set_calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        *gateway.tested_secrets.lock().unwrap(),
        vec!["old-secret".to_owned(), "old-secret".to_owned()]
    );
}

#[tokio::test]
async fn blank_secret_connection_test_reuses_existing_credential() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let gateway = Arc::new(CapturingGateway::default());
    let service = settings_service_with(pool, gateway.clone(), credentials);
    let account = service
        .save_account(save_input(None, "blank-test@example.com", "old-secret"))
        .await
        .unwrap();

    service
        .test_account(TestAccountInput {
            id: Some(account.id),
            provider: MailboxProvider::Gmail,
            email: account.email,
            secret: "".to_owned(),
            imap_host: None,
            imap_port: None,
        })
        .await
        .unwrap();

    assert_eq!(
        gateway.tested_secrets.lock().unwrap().last().unwrap(),
        "old-secret"
    );
}

#[tokio::test]
async fn blank_secret_edit_reports_missing_stored_credential() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "missing-secret@example.com", true).await;
    let service = settings_service(pool);

    let error = service
        .save_account(save_input(
            Some(account_id),
            "missing-secret@example.com",
            "",
        ))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: false,
            ..
        } if service == "mailbox_credential"
    ));
}

#[tokio::test]
async fn blank_secret_is_rejected_for_new_account_without_persistence() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let service = settings_service(pool.clone());

    let error = service
        .save_account(save_input(None, "blank-new@example.com", "   "))
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::Validation { ref field, .. } if field == "secret"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mailbox_accounts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn credential_replacement_clears_persisted_auth_suspension() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let service =
        settings_service_with(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let account = service
        .save_account(save_input(None, "suspended@example.com", "old-secret"))
        .await
        .unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    accounts
        .suspend_retry(account.id, Utc::now(), "authentication failed")
        .await
        .unwrap();
    assert!(
        accounts
            .get_retry_state(account.id)
            .await
            .unwrap()
            .is_some()
    );

    let saved = service
        .save_account(save_input(
            Some(account.id),
            "recovered@example.com",
            "new-secret",
        ))
        .await
        .unwrap();

    assert_eq!(saved.last_error, None);
    let recovered = accounts.get(account.id).await.unwrap();
    assert_eq!(saved, recovered);
    assert_eq!(recovered.email, "recovered@example.com");
    assert_eq!(recovered.last_error, None);
    assert_eq!(accounts.get_retry_state(account.id).await.unwrap(), None);
    assert_eq!(
        credentials.get(&account.id.to_string()).unwrap(),
        Some("new-secret".to_owned())
    );
}

#[tokio::test]
async fn editing_account_conflicts_with_running_sync_without_changing_state() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path()).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let state = AppState::with_gateway(
        pool.clone(),
        paths,
        credentials.clone(),
        Arc::new(PassingGateway),
    );
    let account = state
        .settings_service()
        .save_account(save_input(None, "race-save@example.com", "old-secret"))
        .await
        .unwrap();
    let runner = Arc::new(BlockingRunner {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let scheduler = state.scheduler(runner.clone());
    let running = scheduler.clone();
    let sync = tokio::spawn(async move { running.sync_now(account.id).await });
    runner.started.notified().await;

    let error = state
        .settings_service()
        .save_account(save_input(
            Some(account.id),
            "changed@example.com",
            "new-secret",
        ))
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::Conflict { .. }));
    assert_eq!(
        MailboxAccountRepository::new(pool)
            .get(account.id)
            .await
            .unwrap()
            .email,
        "race-save@example.com"
    );
    assert_eq!(
        credentials.get(&account.id.to_string()).unwrap(),
        Some("old-secret".to_owned())
    );
    runner.release.notify_one();
    sync.await.unwrap().unwrap();
}

#[tokio::test]
async fn deleting_account_conflicts_with_running_sync_without_disabling_it() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path()).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let state = AppState::with_gateway(
        pool.clone(),
        paths,
        credentials.clone(),
        Arc::new(PassingGateway),
    );
    let account = state
        .settings_service()
        .save_account(save_input(None, "race-delete@example.com", "old-secret"))
        .await
        .unwrap();
    let runner = Arc::new(BlockingRunner {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let scheduler = state.scheduler(runner.clone());
    let running = scheduler.clone();
    let sync = tokio::spawn(async move { running.sync_now(account.id).await });
    runner.started.notified().await;

    let error = state
        .settings_service()
        .delete_account(account.id)
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::Conflict { .. }));
    assert!(
        MailboxAccountRepository::new(pool)
            .get(account.id)
            .await
            .unwrap()
            .enabled
    );
    assert_eq!(
        credentials.get(&account.id.to_string()).unwrap(),
        Some("old-secret".to_owned())
    );
    runner.release.notify_one();
    sync.await.unwrap().unwrap();
}

#[tokio::test]
async fn blocking_credential_write_does_not_hold_sqlite_writer_transaction() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}",
        directory
            .path()
            .join("credential-blocking.sqlite3")
            .display()
    );
    let pool = db::connect(&database_url).await.unwrap();
    let credentials = Arc::new(BlockingCredentialStore::default());
    let service =
        settings_service_with(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let account = service
        .save_account(save_input(None, "blocking@example.com", "old-secret"))
        .await
        .unwrap();
    let set_started = credentials.block_next_set();
    let saving = tokio::spawn(async move {
        service
            .save_account(save_input(
                Some(account.id),
                "blocking-updated@example.com",
                "new-secret",
            ))
            .await
    });
    set_started.await.unwrap();

    let probe = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        sqlx::query("INSERT INTO settings (key, value_json, updated_at) VALUES (?, ?, ?)")
            .bind("credential_probe")
            .bind("true")
            .bind(Utc::now().to_rfc3339())
            .execute(&pool),
    )
    .await;

    credentials.release_set();
    saving.await.unwrap().unwrap();
    assert!(matches!(probe, Ok(Ok(_))));
}

#[tokio::test]
async fn cancelled_new_account_save_finishes_after_keyring_succeeds() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(BlockingCredentialStore::default());
    let state = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let service = state.settings_service();
    let set_started = credentials.block_next_set();
    let saving_service = service.clone();
    let caller = tokio::spawn(async move {
        saving_service
            .save_account(save_input(None, "cancel-new@example.com", "new-secret"))
            .await
    });
    set_started.await.unwrap();

    let metadata_during_keyring =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mailbox_accounts WHERE email = ?")
            .bind("cancel-new@example.com")
            .fetch_one(&pool)
            .await
            .unwrap();
    let marker_during_keyring =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_account_saves")
            .fetch_one(&pool)
            .await
            .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    credentials.release_set();
    state.begin_account_saga_shutdown().wait().await.unwrap();
    let account_id =
        sqlx::query_scalar::<_, String>("SELECT id FROM mailbox_accounts WHERE email = ?")
            .bind("cancel-new@example.com")
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(metadata_during_keyring, 0);
    assert_eq!(marker_during_keyring, 1);
    assert_eq!(
        credentials.get(&account_id).unwrap().as_deref(),
        Some("new-secret")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_account_saves")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn cancelled_shutdown_wait_keeps_blocking_temp_save_owned_until_cleanup() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(BlockingCredentialStore::default());
    let state = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let settings = state.settings_service();
    let stable = settings
        .save_account(save_input(
            None,
            "shutdown-stable@example.com",
            "stable-secret",
        ))
        .await
        .unwrap();
    let temp_set_started = credentials.block_next_set();
    let saving_service = settings.clone();
    let caller = tokio::spawn(async move {
        saving_service
            .save_account(save_input(
                None,
                "shutdown-blocked@example.com",
                "shutdown-secret",
            ))
            .await
    });
    temp_set_started.await.unwrap();
    let (operation_id, account_id, phase) = sqlx::query_as::<_, (String, String, String)>(
        "SELECT operation_id, account_id, phase FROM pending_account_saves",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let operation_id = Uuid::parse_str(&operation_id).unwrap();
    let account_id = Uuid::parse_str(&account_id).unwrap();
    let shutdown = state.begin_account_saga_shutdown();

    let timed_out =
        tokio::time::timeout(std::time::Duration::from_millis(50), shutdown.wait()).await;
    let reconcile_while_closing = state.reconcile_account_saves().await;
    let rejected_save = settings
        .save_account(save_input(
            None,
            "shutdown-rejected@example.com",
            "rejected-secret",
        ))
        .await;
    let runner = Arc::new(RecordingRunner::default());
    let scheduler = state.scheduler(runner.clone());
    let tick_while_closing = scheduler.tick(Utc::now()).await;
    let manual_while_closing = scheduler.sync_now(stable.id).await;

    assert!(timed_out.is_err());
    assert!(matches!(
        reconcile_while_closing,
        Err(AppError::Conflict { .. })
    ));
    assert!(matches!(rejected_save, Err(AppError::Conflict { .. })));
    assert!(tick_while_closing.is_ok());
    assert!(matches!(
        manual_while_closing,
        Err(AppError::Conflict { .. })
    ));
    assert!(runner.account_ids.lock().unwrap().is_empty());
    assert_eq!(phase, "prepared");
    assert_eq!(
        credentials.get(&pending_save_key(operation_id)).unwrap(),
        None
    );

    credentials.release_set();
    let saved = caller.await.unwrap().unwrap();
    shutdown.wait().await.unwrap();

    assert_eq!(saved.id, account_id);
    assert_eq!(
        credentials.get(&account_id.to_string()).unwrap().as_deref(),
        Some("shutdown-secret")
    );
    assert_pending_save_cleaned(&pool, credentials.as_ref(), operation_id).await;
}

#[tokio::test]
async fn cancelled_new_account_temp_failure_remains_owned_and_reconciles() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(BlockingCredentialStore::default());
    let state = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let service = state.settings_service();
    let set_started = credentials.block_next_set_with_failure();
    let saving_service = service.clone();
    let caller = tokio::spawn(async move {
        saving_service
            .save_account(save_input(
                None,
                "cancel-new-failure@example.com",
                "new-secret",
            ))
            .await
    });
    set_started.await.unwrap();

    let metadata_during_keyring =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mailbox_accounts WHERE email = ?")
            .bind("cancel-new-failure@example.com")
            .fetch_one(&pool)
            .await
            .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    credentials.release_set();
    let error = state
        .begin_account_saga_shutdown()
        .wait()
        .await
        .unwrap_err();

    assert_eq!(metadata_during_keyring, 0);
    assert!(!error.to_string().contains("new-secret"));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT phase FROM pending_account_saves")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "prepared"
    );
    drop(service);
    drop(state);
    let restarted = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    restarted.reconcile_account_saves().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_account_saves")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert!(credentials.secrets.read().unwrap().is_empty());
}

#[tokio::test]
async fn cancelled_account_edit_keeps_guard_and_live_state_until_temp_failure_reconciles() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(BlockingCredentialStore::default());
    let state = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let service = state.settings_service();
    let account = service
        .save_account(save_input(None, "cancel-edit@example.com", "old-secret"))
        .await
        .unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let failed_at = Utc.with_ymd_and_hms(2026, 7, 15, 10, 30, 0).unwrap();
    accounts
        .record_retryable_failure(account.id, failed_at, "old failure")
        .await
        .unwrap();
    let original_account = accounts.get(account.id).await.unwrap();
    let original_retry = accounts.get_retry_state(account.id).await.unwrap();
    let set_started = credentials.block_next_set_with_failure();
    let saving_service = service.clone();
    let caller = tokio::spawn(async move {
        saving_service
            .save_account(save_input(
                Some(account.id),
                "cancel-edit-updated@example.com",
                "new-secret",
            ))
            .await
    });
    set_started.await.unwrap();

    let staged_account = accounts.get(account.id).await.unwrap();
    let staged_retry = accounts.get_retry_state(account.id).await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let conflict = service
        .test_account(TestAccountInput {
            id: Some(account.id),
            provider: MailboxProvider::Gmail,
            email: "cancel-edit-updated@example.com".to_owned(),
            secret: "".to_owned(),
            imap_host: None,
            imap_port: None,
        })
        .await;
    credentials.release_set();
    let error = state
        .begin_account_saga_shutdown()
        .wait()
        .await
        .unwrap_err();
    drop(service);
    drop(state);
    let restarted = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    restarted.reconcile_account_saves().await.unwrap();

    assert_eq!(staged_account, original_account);
    assert_eq!(staged_retry, original_retry);
    assert!(matches!(conflict, Err(AppError::Conflict { .. })));
    assert!(!error.to_string().contains("new-secret"));
    assert_eq!(accounts.get(account.id).await.unwrap(), original_account);
    assert_eq!(
        accounts.get_retry_state(account.id).await.unwrap(),
        original_retry
    );
    assert_eq!(
        credentials.get(&account.id.to_string()).unwrap(),
        Some("old-secret".to_owned())
    );
}

#[tokio::test]
async fn credential_delete_failure_leaves_account_disabled_and_recoverable() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(ControlledCredentialStore::default());
    let service =
        settings_service_with(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let account = service
        .save_account(save_input(None, "delete-fails@example.com", "old-secret"))
        .await
        .unwrap();
    let failed_at = Utc.with_ymd_and_hms(2026, 7, 17, 9, 45, 0).unwrap();
    MailboxAccountRepository::new(pool.clone())
        .suspend_retry(account.id, failed_at, "authentication failed")
        .await
        .unwrap();
    credentials.delete_fails(true);

    service.delete_account(account.id).await.unwrap_err();

    let persisted = MailboxAccountRepository::new(pool)
        .get(account.id)
        .await
        .unwrap();
    assert!(!persisted.enabled);
    assert_eq!(persisted.last_error_at, Some(failed_at));
    assert_eq!(
        credentials.get(&account.id.to_string()).unwrap(),
        Some("old-secret".to_owned())
    );
}

#[tokio::test]
async fn delete_account_removes_credential_and_metadata() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(ControlledCredentialStore::default());
    let service =
        settings_service_with(pool.clone(), Arc::new(PassingGateway), credentials.clone());
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

    let defaults = Preferences::default();
    assert_eq!(defaults.batch_directory_pattern, "{batchName}-{timestamp}");
    assert_eq!(service.preferences().await.unwrap(), defaults);
    let expected = Preferences {
        background_sync_enabled: false,
        export_directory: "/tmp/invoice-exports".to_owned(),
        batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
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
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&raw).unwrap(),
        serde_json::json!({
            "backgroundSyncEnabled": false,
            "exportDirectory": "/tmp/invoice-exports",
            "batchDirectoryPattern": "{batchName}-{timestamp}",
        })
    );
    assert_eq!(serde_json::from_str::<Preferences>(&raw).unwrap(), expected);
}

#[tokio::test]
async fn saving_preferences_maps_sqlite_busy_as_retryable_database_error() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("settings-busy.sqlite3").display()
    );
    let options = database_url
        .parse::<sqlx::sqlite::SqliteConnectOptions>()
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete)
        .busy_timeout(std::time::Duration::ZERO);
    let setup_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run(&setup_pool)
        .await
        .unwrap();
    setup_pool.close().await;

    let lock_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    let service_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let mut lock = lock_pool.acquire().await.unwrap();
    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let service = settings_service(service_pool);

    let error = service
        .save_preferences(PreferencesInput {
            background_sync_enabled: false,
            export_directory: "/tmp/invoice-exports".to_owned(),
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
        })
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: true,
            ..
        } if service == "database"
    ));
    sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();
}

#[tokio::test]
async fn invalid_preferences_are_rejected_without_overwriting_saved_values() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let service = settings_service(pool.clone());
    let original = service
        .save_preferences(PreferencesInput {
            background_sync_enabled: true,
            export_directory: "/tmp/exports".to_owned(),
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
        })
        .await
        .unwrap();

    let blank_error = service
        .save_preferences(PreferencesInput {
            background_sync_enabled: false,
            export_directory: "  ".to_owned(),
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
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
async fn export_directory_cannot_change_while_recovery_work_is_pending() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let service = settings_service(pool.clone());
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO pending_exports (
            operation_id, batch_id, staging_component, final_component, exported_at, state,
            created_at, updated_at
         ) VALUES (?, ?, ?, 'pending-package', ?, 'generating', ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(Uuid::new_v4().to_string())
    .bind(format!("export-{}", Uuid::new_v4()))
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();

    let error = service
        .save_preferences(PreferencesInput {
            background_sync_enabled: true,
            export_directory: "/tmp/different-root".to_owned(),
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::Conflict { .. }));
    assert_eq!(service.preferences().await.unwrap(), Preferences::default());
}

#[tokio::test]
async fn persisted_legacy_batch_pattern_is_reported_as_invalid() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO settings (key, value_json, updated_at) VALUES (?, ?, ?)")
        .bind("preferences")
        .bind(
            serde_json::json!({
                "backgroundSyncEnabled": true,
                "exportDirectory": "exports",
                "batchDirectoryPattern": "{start_date}_{end_date}",
            })
            .to_string(),
        )
        .bind(Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();

    let error = settings_service(pool).preferences().await.unwrap_err();

    assert!(
        matches!(error, AppError::Validation { ref field, .. } if field == "batch_directory_pattern")
    );
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
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
        })
        .await
        .unwrap();
    let runner = Arc::new(RecordingRunner::default());
    let scheduler = scheduler(pool, runner.clone());

    scheduler.tick(Utc::now()).await.unwrap();

    assert!(runner.account_ids.lock().unwrap().is_empty());
}

#[tokio::test]
async fn disabling_background_sync_during_tick_allows_current_run_but_starts_no_next_run() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path()).unwrap();
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
    let state = AppState::with_gateway(
        pool,
        paths,
        Arc::new(MemoryCredentialStore::default()),
        Arc::new(PassingGateway),
    );
    let scheduler = state.scheduler(runner.clone());
    let running_scheduler = scheduler.clone();
    let tick = tokio::spawn(async move { running_scheduler.tick(Utc::now()).await });
    runner.started.notified().await;

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        state.settings_service().save_preferences(PreferencesInput {
            background_sync_enabled: false,
            export_directory: "exports".to_owned(),
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
        }),
    )
    .await
    .unwrap()
    .unwrap();
    runner.release.notify_one();
    tick.await.unwrap().unwrap();

    assert_eq!(*runner.account_ids.lock().unwrap(), vec![first]);
}

#[tokio::test]
async fn disabling_background_sync_linearizes_before_runner_start() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "start-race@example.com", true).await;
    let state = mailbox_state(
        pool.clone(),
        Arc::new(PassingGateway),
        Arc::new(MemoryCredentialStore::default()),
    );
    let settings = state.settings_service();
    let runner = Arc::new(RecordingRunner::default());
    let boundary = Arc::new(ControlledStartBarrier {
        reached: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let scan_time = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let scheduler = state.scheduler_with_runtime(
        runner.clone(),
        Arc::new(ManualClock::new(scan_time)),
        boundary.clone(),
    );
    let running = scheduler.clone();
    let tick = tokio::spawn(async move { running.tick(scan_time).await });
    boundary.reached.notified().await;

    let (attempting, attempted) = tokio::sync::oneshot::channel();
    let disable = tokio::spawn(async move {
        let _ = attempting.send(());
        settings
            .save_preferences(PreferencesInput {
                background_sync_enabled: false,
                export_directory: "exports".to_owned(),
                batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
            })
            .await
    });
    attempted.await.unwrap();
    tokio::task::yield_now().await;
    assert!(!disable.is_finished());
    boundary.release.notify_one();
    disable.await.unwrap().unwrap();
    tick.await.unwrap().unwrap();

    assert_eq!(*runner.account_ids.lock().unwrap(), vec![account_id]);
    scheduler.tick(scan_time + Duration::days(1)).await.unwrap();
    assert_eq!(*runner.account_ids.lock().unwrap(), vec![account_id]);
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
    let scheduler = scheduler(pool, runner.clone());
    let first_scheduler = scheduler.clone();
    let first = tokio::spawn(async move { first_scheduler.sync_now(account_id).await });
    runner.started.notified().await;

    let second = scheduler.sync_now(account_id).await;

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
        service: "imap_authentication".to_owned(),
        retryable: true,
        message: "network unavailable".to_owned(),
    };
    let runner = Arc::new(SequenceRunner::new(vec![
        Err(retryable()),
        Err(retryable()),
        Err(retryable()),
        Err(retryable()),
    ]));
    let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let clock = Arc::new(ManualClock::new(start));
    let scheduler = scheduler_with_clock(pool, runner.clone(), clock.clone());

    scheduler.tick(start).await.unwrap();
    clock.set(start + Duration::seconds(59));
    scheduler.tick(start + Duration::seconds(59)).await.unwrap();
    clock.set(start + Duration::minutes(1));
    scheduler.tick(start + Duration::minutes(1)).await.unwrap();
    clock.set(start + Duration::minutes(5) + Duration::seconds(59));
    scheduler
        .tick(start + Duration::minutes(5) + Duration::seconds(59))
        .await
        .unwrap();
    clock.set(start + Duration::minutes(6));
    scheduler.tick(start + Duration::minutes(6)).await.unwrap();
    clock.set(start + Duration::minutes(20) + Duration::seconds(59));
    scheduler
        .tick(start + Duration::minutes(20) + Duration::seconds(59))
        .await
        .unwrap();
    clock.set(start + Duration::minutes(21));
    scheduler.tick(start + Duration::minutes(21)).await.unwrap();
    clock.set(start + Duration::minutes(35));
    scheduler.tick(start + Duration::minutes(35)).await.unwrap();
    clock.set(start + Duration::minutes(36));
    scheduler.tick(start + Duration::minutes(36)).await.unwrap();

    assert_eq!(runner.calls.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn authentication_failure_is_not_retried_and_preserves_last_error() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "auth@example.com", true).await;
    let runner = Arc::new(SequenceRunner::new(vec![Err(AppError::External {
        service: "imap_authentication".to_owned(),
        retryable: false,
        message: "authentication failed".to_owned(),
    })]));
    let scheduler = scheduler(pool, runner.clone());
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
async fn retry_policy_persistence_failure_is_returned_by_tick() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    insert_account(
        &MailboxAccountRepository::new(pool.clone()),
        "policy-failure@example.com",
        true,
    )
    .await;
    sqlx::query(
        "CREATE TRIGGER fail_retry_policy BEFORE INSERT ON sync_retry_states \
         BEGIN SELECT RAISE(ABORT, 'retry policy write failed'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let runner = Arc::new(SequenceRunner::new(vec![Err(AppError::External {
        service: "imap".to_owned(),
        retryable: true,
        message: "network unavailable".to_owned(),
    })]));
    let scheduler = scheduler(pool, runner);

    let error = scheduler.tick(Utc::now()).await.unwrap_err();

    assert!(
        matches!(error, AppError::Internal { .. })
            || matches!(
                error,
                AppError::External {
                    ref service,
                    retryable: true,
                    ..
                } if service == "database"
            )
    );
}

#[tokio::test]
async fn non_authentication_configuration_failure_is_observable_not_suspended() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let account_id = insert_account(
        &MailboxAccountRepository::new(pool.clone()),
        "config-failure@example.com",
        true,
    )
    .await;
    let runner = Arc::new(SequenceRunner::new(vec![Err(AppError::External {
        service: "imap".to_owned(),
        retryable: false,
        message: "IMAP configuration failed".to_owned(),
    })]));
    let scheduler = scheduler(pool, runner);

    let error = scheduler.tick(Utc::now()).await.unwrap_err();

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: false,
            ..
        } if service == "imap"
    ));
    assert_eq!(scheduler.retry_state(account_id).await.unwrap(), None);
}

#[tokio::test]
async fn authentication_suspension_survives_scheduler_reconstruction() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "auth-restart@example.com", true).await;
    let first_runner = Arc::new(SequenceRunner::new(vec![Err(AppError::External {
        service: "imap_authentication".to_owned(),
        retryable: false,
        message: "authentication failed".to_owned(),
    })]));
    let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    scheduler_with_clock(
        pool.clone(),
        first_runner,
        Arc::new(ManualClock::new(start)),
    )
    .tick(start)
    .await
    .unwrap();

    let second_runner = Arc::new(RecordingRunner::default());
    scheduler(pool.clone(), second_runner.clone())
        .tick(start + Duration::days(1))
        .await
        .unwrap();

    assert!(second_runner.account_ids.lock().unwrap().is_empty());
    let suspended = sqlx::query_scalar::<_, i64>(
        "SELECT suspended FROM sync_retry_states WHERE account_id = ?",
    )
    .bind(account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(suspended, 1);
}

#[tokio::test]
async fn retryable_backoff_survives_scheduler_reconstruction() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "retry-restart@example.com", true).await;
    let first_runner = Arc::new(SequenceRunner::new(vec![Err(AppError::External {
        service: "imap".to_owned(),
        retryable: true,
        message: "network unavailable".to_owned(),
    })]));
    let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    scheduler_with_clock(
        pool.clone(),
        first_runner,
        Arc::new(ManualClock::new(start)),
    )
    .tick(start)
    .await
    .unwrap();

    let second_runner = Arc::new(RecordingRunner::default());
    let reconstructed = scheduler(pool, second_runner.clone());
    reconstructed
        .tick(start + Duration::seconds(59))
        .await
        .unwrap();
    assert!(second_runner.account_ids.lock().unwrap().is_empty());
    reconstructed
        .tick(start + Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(*second_runner.account_ids.lock().unwrap(), vec![account_id]);
}

#[tokio::test]
async fn retry_due_time_overrides_normal_sync_interval() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "retry-interval@example.com", true).await;
    let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    sqlx::query("UPDATE mailbox_accounts SET last_synced_at = ? WHERE id = ?")
        .bind(start.to_rfc3339())
        .bind(account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let runner = Arc::new(SequenceRunner::new(vec![
        Err(AppError::External {
            service: "imap".to_owned(),
            retryable: true,
            message: "network unavailable".to_owned(),
        }),
        Ok(()),
    ]));
    let scheduler = scheduler_with_clock(pool, runner.clone(), Arc::new(ManualClock::new(start)));
    scheduler.sync_now(account_id).await.unwrap_err();

    scheduler.tick(start + Duration::minutes(1)).await.unwrap();

    assert_eq!(runner.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retry_backoff_starts_when_the_failed_run_finishes() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "failure-time@example.com", true).await;
    let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let clock = Arc::new(ManualClock::new(start));
    let runner = Arc::new(BlockingFailureRunner {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let scheduler = scheduler_with_clock(pool, runner.clone(), clock.clone());
    let running = scheduler.clone();
    let tick = tokio::spawn(async move { running.tick(start).await });
    runner.started.notified().await;

    clock.set(start + Duration::minutes(10));
    runner.release.notify_one();
    tick.await.unwrap().unwrap();

    let retry = scheduler.retry_state(account_id).await.unwrap().unwrap();
    assert_eq!(retry.next_retry_at, Some(start + Duration::minutes(11)));
}

#[tokio::test]
async fn deleting_account_cascades_persisted_retry_state() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "retry-delete@example.com", true).await;
    let runner = Arc::new(SequenceRunner::new(vec![Err(AppError::External {
        service: "imap".to_owned(),
        retryable: true,
        message: "network unavailable".to_owned(),
    })]));
    scheduler(pool.clone(), runner)
        .tick(Utc::now())
        .await
        .unwrap();
    let before =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sync_retry_states WHERE account_id = ?")
            .bind(account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, 1);

    accounts.delete(account_id).await.unwrap();

    let after =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sync_retry_states WHERE account_id = ?")
            .bind(account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after, 0);
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
    let start = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
    let scheduler = scheduler_with_clock(pool, runner, Arc::new(ManualClock::new(start)));
    scheduler.tick(start).await.unwrap();
    assert!(scheduler.retry_state(account_id).await.unwrap().is_some());
    assert!(accounts.get(account_id).await.unwrap().last_error.is_some());
    assert_eq!(
        accounts.get(account_id).await.unwrap().last_error_at,
        Some(start)
    );

    scheduler.sync_now(account_id).await.unwrap();

    assert_eq!(scheduler.retry_state(account_id).await.unwrap(), None);
    assert_eq!(accounts.get(account_id).await.unwrap().last_error, None);
    assert_eq!(accounts.get(account_id).await.unwrap().last_error_at, None);
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
    let scheduler = scheduler(pool, runner.clone());
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
async fn slow_first_account_does_not_block_starting_second_due_account() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let slow = insert_account(&accounts, "a-slow@example.com", true).await;
    insert_account(&accounts, "z-fast@example.com", true).await;
    let runner = Arc::new(SlowFirstRunner {
        slow_account_id: slow,
        slow_started: tokio::sync::Notify::new(),
        fast_started: tokio::sync::Notify::new(),
        release_slow: tokio::sync::Notify::new(),
    });
    let scheduler = scheduler(pool, runner.clone());
    let running = scheduler.clone();
    let tick = tokio::spawn(async move { running.tick(Utc::now()).await });
    runner.slow_started.notified().await;

    let fast_started = tokio::time::timeout(
        std::time::Duration::from_millis(150),
        runner.fast_started.notified(),
    )
    .await;
    runner.release_slow.notify_one();
    tick.await.unwrap().unwrap();

    assert!(fast_started.is_ok());
}

#[tokio::test]
async fn dropping_active_scheduler_handle_does_not_detach_child_task() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    insert_account(
        &MailboxAccountRepository::new(pool.clone()),
        "drop-active@example.com",
        true,
    )
    .await;
    let runner = Arc::new(CompletionBlockingRunner {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        completed: AtomicBool::new(false),
        completed_signal: tokio::sync::Notify::new(),
    });
    let handle = scheduler(pool, runner.clone()).start();
    runner.started.notified().await;

    drop(handle);
    runner.release.notify_one();
    let detached_completion = tokio::time::timeout(
        std::time::Duration::from_millis(150),
        runner.completed_signal.notified(),
    )
    .await;

    assert!(detached_completion.is_err());
    assert!(!runner.completed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn stopping_scheduler_waits_for_in_flight_child_task() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    insert_account(
        &MailboxAccountRepository::new(pool.clone()),
        "stop-active@example.com",
        true,
    )
    .await;
    let runner = Arc::new(CompletionBlockingRunner {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        completed: AtomicBool::new(false),
        completed_signal: tokio::sync::Notify::new(),
    });
    let handle = scheduler(pool, runner.clone()).start();
    runner.started.notified().await;

    let stop = tokio::spawn(async move { handle.stop().await });
    tokio::task::yield_now().await;
    assert!(!stop.is_finished());

    runner.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(1), stop)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert!(runner.completed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn stopping_scheduler_waits_for_blocked_child_after_another_child_panics() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let panic_account_id = insert_account(&accounts, "a-panic@example.com", true).await;
    insert_account(&accounts, "z-blocked@example.com", true).await;
    let runner = Arc::new(PanicAndBlockRunner {
        panic_account_id,
        release_panic: tokio::sync::Notify::new(),
        blocked_started: tokio::sync::Notify::new(),
        release_blocked: tokio::sync::Notify::new(),
        blocked_completed: AtomicBool::new(false),
        blocked_completed_signal: tokio::sync::Notify::new(),
    });
    let handle = scheduler(pool, runner.clone()).start();
    runner.blocked_started.notified().await;

    let mut stop = tokio::spawn(async move { handle.stop().await });
    tokio::task::yield_now().await;
    runner.release_panic.notify_one();
    let early = tokio::time::timeout(std::time::Duration::from_millis(100), &mut stop).await;
    runner.release_blocked.notify_one();
    let returned_early = match early {
        Ok(result) => {
            assert!(result.unwrap().is_err());
            true
        }
        Err(_) => {
            assert!(stop.await.unwrap().is_err());
            false
        }
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        runner.blocked_completed_signal.notified(),
    )
    .await
    .unwrap();

    assert!(!returned_early);
    assert!(runner.blocked_completed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn ready_cancellation_prevents_overdue_scheduler_start() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    insert_account(
        &MailboxAccountRepository::new(pool.clone()),
        "cancel-overdue@example.com",
        true,
    )
    .await;
    let runner = Arc::new(RecordingRunner::default());
    let scheduler = scheduler(pool, runner.clone());

    for _ in 0..64 {
        scheduler.start().stop().await.unwrap();
    }

    assert!(runner.account_ids.lock().unwrap().is_empty());
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
    let first = tokio::spawn(async move { scheduled.sync_now(account_id).await });
    runner.started.notified().await;

    let second = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        manual.sync_now(account_id),
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
    let scheduler = scheduler(pool, runner.clone());

    scheduler.tick(now).await.unwrap();

    assert_eq!(*runner.account_ids.lock().unwrap(), vec![due]);
    assert!(!runner.account_ids.lock().unwrap().contains(&disabled));
}

#[tokio::test]
async fn locked_account_save_does_not_block_other_account_scheduler_or_save() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(BlockingCredentialStore::default());
    let state = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let settings = state.settings_service();
    let locked = settings
        .save_account(save_input(None, "a-locked@example.com", "locked-secret"))
        .await
        .unwrap();
    let free = settings
        .save_account(save_input(None, "b-free@example.com", "free-secret"))
        .await
        .unwrap();
    let temp_set_started = credentials.block_next_set();
    let saving_service = settings.clone();
    let locked_save = tokio::spawn(async move {
        saving_service
            .save_account(save_input(
                Some(locked.id),
                "a-locked-updated@example.com",
                "locked-new-secret",
            ))
            .await
    });
    temp_set_started.await.unwrap();
    let runner = Arc::new(RecordingRunner::default());
    let scheduler = state.scheduler(runner.clone());

    let tick_result = scheduler.tick(Utc::now()).await;
    let saved_free = settings
        .save_account(save_input(
            Some(free.id),
            "b-free-updated@example.com",
            "free-new-secret",
        ))
        .await;

    credentials.release_set();
    locked_save.await.unwrap().unwrap();
    assert!(tick_result.is_ok());
    assert_eq!(*runner.account_ids.lock().unwrap(), vec![free.id]);
    assert_eq!(saved_free.unwrap().email, "b-free-updated@example.com");
}

#[tokio::test]
async fn staged_missing_temp_for_one_account_does_not_block_other_scheduled_sync() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let blocked = insert_account(&accounts, "a-staged-missing@example.com", true).await;
    let free = insert_account(&accounts, "b-staged-free@example.com", true).await;
    let operation_id = Uuid::new_v4();
    insert_pending_save(
        &pool,
        operation_id,
        blocked,
        "credential_staged",
        true,
        "a-staged-updated@example.com",
    )
    .await;
    let state = mailbox_state(
        pool,
        Arc::new(PassingGateway),
        Arc::new(MemoryCredentialStore::default()),
    );
    let runner = Arc::new(RecordingRunner::default());
    let scheduler = state.scheduler(runner.clone());

    scheduler.tick(Utc::now()).await.unwrap();
    let manual = scheduler.sync_now(blocked).await;

    assert_eq!(*runner.account_ids.lock().unwrap(), vec![free]);
    assert!(matches!(manual, Err(AppError::Conflict { .. })));
}

#[tokio::test]
async fn committed_cleanup_survives_live_delete_without_reserving_account_or_email() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(PermanentTempDeleteFailureStore::default());
    credentials.fail_all_temp_deletes(true);
    let state = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let settings = state.settings_service();

    let initial_error = settings
        .save_account(save_input(
            None,
            "cleanup-reservation@example.com",
            "initial-cleanup-secret",
        ))
        .await
        .unwrap_err();
    let account_id = Uuid::parse_str(
        &sqlx::query_scalar::<_, String>(
            "SELECT id FROM mailbox_accounts WHERE email = 'cleanup-reservation@example.com'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
    )
    .unwrap();

    assert!(matches!(initial_error, AppError::External { .. }));
    assert_eq!(pending_save_count(&pool).await, 0);
    assert_eq!(pending_cleanup_count(&pool).await, 1);
    assert!(!cleanup_rows(&pool).await.contains("initial-cleanup-secret"));

    let edit_error = settings
        .save_account(save_input(
            Some(account_id),
            "cleanup-reusable@example.com",
            "edited-cleanup-secret",
        ))
        .await
        .unwrap_err();

    assert!(matches!(edit_error, AppError::External { .. }));
    assert_eq!(
        MailboxAccountRepository::new(pool.clone())
            .get(account_id)
            .await
            .unwrap()
            .email,
        "cleanup-reusable@example.com"
    );
    assert_eq!(pending_save_count(&pool).await, 0);
    assert_eq!(pending_cleanup_count(&pool).await, 2);

    settings.delete_account(account_id).await.unwrap();
    let replacement_error = settings
        .save_account(save_input(
            None,
            "cleanup-reusable@example.com",
            "replacement-cleanup-secret",
        ))
        .await
        .unwrap_err();

    assert!(matches!(replacement_error, AppError::External { .. }));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM mailbox_accounts WHERE email = 'cleanup-reusable@example.com'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(pending_save_count(&pool).await, 0);
    let operation_ids = cleanup_operation_ids(&pool).await;
    assert_eq!(operation_ids.len(), 3);

    credentials.fail_all_temp_deletes(false);
    state.reconcile_account_saves().await.unwrap();

    assert_eq!(pending_cleanup_count(&pool).await, 0);
    for operation_id in operation_ids {
        assert_eq!(
            credentials.get(&pending_save_key(operation_id)).unwrap(),
            None
        );
    }
}

#[tokio::test]
async fn cleanup_failure_does_not_block_account_sync_save_or_delete() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let committed = insert_account(&accounts, "a-committed-cleanup@example.com", true).await;
    let free = insert_account(&accounts, "b-committed-free@example.com", true).await;
    let operation_id = Uuid::new_v4();
    insert_pending_cleanup(&pool, operation_id).await;
    let credentials = Arc::new(PermanentTempDeleteFailureStore::default());
    let temp_key = pending_save_key(operation_id);
    credentials.set(&temp_key, "cleanup-secret").unwrap();
    credentials
        .set(&committed.to_string(), "committed-secret")
        .unwrap();
    credentials.set(&free.to_string(), "free-secret").unwrap();
    credentials.fail_delete_for(temp_key);
    let state = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let runner = Arc::new(RecordingRunner::default());
    let scheduler = state.scheduler(runner.clone());

    scheduler.tick(Utc::now()).await.unwrap();
    let saved_free = state
        .settings_service()
        .save_account(save_input(
            Some(free),
            "b-committed-free-updated@example.com",
            "free-new-secret",
        ))
        .await
        .unwrap();
    state
        .settings_service()
        .delete_account(committed)
        .await
        .unwrap();
    let recovery = state.reconcile_account_saves().await.unwrap();

    assert_eq!(*runner.account_ids.lock().unwrap(), vec![committed, free]);
    assert_eq!(saved_free.email, "b-committed-free-updated@example.com");
    assert!(matches!(
        accounts.get(committed).await,
        Err(AppError::NotFound { .. })
    ));
    assert_eq!(pending_save_count(&pool).await, 0);
    assert_eq!(pending_cleanup_count(&pool).await, 1);
    assert_eq!(recovery.cleanup_failure_count(), 1);
    assert_eq!(recovery.pending_count(), 1);
    assert!(credentials.failed_delete_attempts() >= 2);
}

#[test]
fn best_effort_reconciliation_log_identifies_account_without_secret() {
    let output = Arc::new(Mutex::new(Vec::new()));
    let writer_output = output.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || CaptureWriter(writer_output.clone()))
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let operation_id = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let pool = db::connect("sqlite::memory:").await.unwrap();
            let operation_id = Uuid::new_v4();
            insert_pending_cleanup(&pool, operation_id).await;
            let credentials = Arc::new(PermanentTempDeleteFailureStore::default());
            let temp_key = pending_save_key(operation_id);
            credentials
                .set(&temp_key, "cleanup-secret-must-not-leak")
                .unwrap();
            credentials.fail_delete_for(temp_key);
            let state = mailbox_state(pool, Arc::new(PassingGateway), credentials);

            state.reconcile_account_saves().await.unwrap();
            operation_id
        });
    let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();

    assert!(logs.contains(&operation_id.to_string()));
    assert!(!logs.contains("cleanup-secret-must-not-leak"));
    assert!(!logs.chars().any(|character| character == '\0'));
}

#[tokio::test]
async fn prepared_marker_without_temp_is_rolled_back_after_restart() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "before-marker@example.com", true).await;
    credentials
        .set(&account_id.to_string(), "old-secret")
        .unwrap();
    let operation_id = Uuid::new_v4();
    insert_pending_save(
        &pool,
        operation_id,
        account_id,
        "prepared",
        true,
        "after-marker@example.com",
    )
    .await;
    drop(mailbox_state(
        pool.clone(),
        Arc::new(PassingGateway),
        credentials.clone(),
    ));

    let restarted = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    restarted.reconcile_account_saves().await.unwrap();

    assert_eq!(
        accounts.get(account_id).await.unwrap().email,
        "before-marker@example.com"
    );
    assert_eq!(
        credentials.get(&account_id.to_string()).unwrap().as_deref(),
        Some("old-secret")
    );
    assert_pending_save_cleaned(&pool, credentials.as_ref(), operation_id).await;
}

#[tokio::test]
async fn prepared_new_account_with_temp_rolls_forward_after_restart() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let account_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    insert_pending_save(
        &pool,
        operation_id,
        account_id,
        "prepared",
        false,
        "prepared-temp@example.com",
    )
    .await;
    credentials
        .set(&pending_save_key(operation_id), "new-secret")
        .unwrap();
    drop(mailbox_state(
        pool.clone(),
        Arc::new(PassingGateway),
        credentials.clone(),
    ));

    let restarted = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    restarted.reconcile_account_saves().await.unwrap();

    assert_eq!(
        MailboxAccountRepository::new(pool.clone())
            .get(account_id)
            .await
            .unwrap()
            .email,
        "prepared-temp@example.com"
    );
    assert_eq!(
        credentials.get(&account_id.to_string()).unwrap().as_deref(),
        Some("new-secret")
    );
    assert_pending_save_cleaned(&pool, credentials.as_ref(), operation_id).await;
}

#[tokio::test]
async fn staged_edit_with_live_credential_switched_rolls_forward_after_restart() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "before-switch@example.com", true).await;
    let operation_id = Uuid::new_v4();
    insert_pending_save(
        &pool,
        operation_id,
        account_id,
        "credential_staged",
        true,
        "after-switch@example.com",
    )
    .await;
    credentials
        .set(&pending_save_key(operation_id), "new-secret")
        .unwrap();
    credentials
        .set(&account_id.to_string(), "new-secret")
        .unwrap();
    drop(mailbox_state(
        pool.clone(),
        Arc::new(PassingGateway),
        credentials.clone(),
    ));

    let restarted = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    restarted.reconcile_account_saves().await.unwrap();

    assert_eq!(
        accounts.get(account_id).await.unwrap().email,
        "after-switch@example.com"
    );
    assert_eq!(
        credentials.get(&account_id.to_string()).unwrap().as_deref(),
        Some("new-secret")
    );
    assert_pending_save_cleaned(&pool, credentials.as_ref(), operation_id).await;
}

#[tokio::test]
async fn legacy_committed_marker_migrates_and_cleans_without_live_account_dependency() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let current = sqlx::migrate!("./migrations");
    let through_0005 = sqlx::migrate::Migrator {
        migrations: Cow::Owned(current.iter().take(5).cloned().collect()),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    through_0005.run(&pool).await.unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = Uuid::new_v4();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO mailbox_accounts (\
            id, provider, email, imap_host, imap_port, enabled, sync_interval_minutes, \
            created_at, updated_at\
         ) VALUES (?, 'gmail', 'committed@example.com', 'imap.gmail.com', 993, 1, 15, ?, ?)",
    )
    .bind(account_id.to_string())
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    let operation_id = Uuid::new_v4();
    insert_pending_save(
        &pool,
        operation_id,
        account_id,
        "committed",
        true,
        "committed@example.com",
    )
    .await;
    credentials
        .set(&pending_save_key(operation_id), "final-secret")
        .unwrap();
    credentials
        .set(&account_id.to_string(), "final-secret")
        .unwrap();
    current.run(&pool).await.unwrap();

    assert_eq!(pending_save_count(&pool).await, 0);
    assert_eq!(pending_cleanup_count(&pool).await, 1);
    sqlx::query("DELETE FROM mailbox_accounts WHERE id = ?")
        .bind(account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();

    let restarted = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    restarted.reconcile_account_saves().await.unwrap();

    assert!(matches!(
        accounts.get(account_id).await,
        Err(AppError::NotFound { .. })
    ));
    assert_eq!(
        credentials.get(&account_id.to_string()).unwrap().as_deref(),
        Some("final-secret")
    );
    assert_pending_save_cleaned(&pool, credentials.as_ref(), operation_id).await;
}

#[tokio::test]
async fn final_credential_failure_preserves_durable_state_and_restart_forwards() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(FailNextLiveCredentialStore::default());
    let state = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    credentials.fail_next_live_set();

    let error = state
        .settings_service()
        .save_account(save_input(
            None,
            "resume-final-set@example.com",
            "durable-secret",
        ))
        .await
        .unwrap_err();
    let (operation_id, account_id, phase) = sqlx::query_as::<_, (String, String, String)>(
        "SELECT operation_id, account_id, phase FROM pending_account_saves",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let operation_id = Uuid::parse_str(&operation_id).unwrap();
    let account_id = Uuid::parse_str(&account_id).unwrap();
    let persisted_marker = sqlx::query_scalar::<_, String>(
        "SELECT operation_id || account_id || phase || email || imap_host \
         FROM pending_account_saves WHERE operation_id = ?",
    )
    .bind(operation_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(phase, "credential_staged");
    assert!(!error.to_string().contains("durable-secret"));
    assert!(!error.to_string().chars().any(char::is_control));
    assert!(!persisted_marker.contains("durable-secret"));
    assert_eq!(
        credentials
            .get(&pending_save_key(operation_id))
            .unwrap()
            .as_deref(),
        Some("durable-secret")
    );
    assert!(
        MailboxAccountRepository::new(pool.clone())
            .get(account_id)
            .await
            .is_err()
    );
    drop(state);

    let restarted = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    restarted.reconcile_account_saves().await.unwrap();

    assert_eq!(
        MailboxAccountRepository::new(pool.clone())
            .get(account_id)
            .await
            .unwrap()
            .email,
        "resume-final-set@example.com"
    );
    assert_eq!(
        credentials.get(&account_id.to_string()).unwrap().as_deref(),
        Some("durable-secret")
    );
    assert_pending_save_cleaned(&pool, credentials.as_ref(), operation_id).await;
}

#[tokio::test]
async fn pending_existing_account_cannot_run_manual_or_scheduled_sync() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let credentials = Arc::new(FailNextLiveCredentialStore::default());
    let accounts = MailboxAccountRepository::new(pool.clone());
    let account_id = insert_account(&accounts, "pending-sync@example.com", true).await;
    credentials
        .set(&account_id.to_string(), "old-secret")
        .unwrap();
    let operation_id = Uuid::new_v4();
    insert_pending_save(
        &pool,
        operation_id,
        account_id,
        "credential_staged",
        true,
        "pending-sync-updated@example.com",
    )
    .await;
    credentials
        .set(&pending_save_key(operation_id), "new-secret")
        .unwrap();
    let state = mailbox_state(pool.clone(), Arc::new(PassingGateway), credentials.clone());
    let runner = Arc::new(RecordingRunner::default());
    let scheduler = state.scheduler(runner.clone());

    credentials.fail_next_live_set();
    let manual = scheduler.sync_now(account_id).await;
    credentials.fail_next_live_set();
    let scheduled = scheduler.tick(Utc::now()).await;

    assert!(manual.is_err());
    assert!(scheduled.is_ok());
    assert!(runner.account_ids.lock().unwrap().is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pending_account_saves WHERE account_id = ?",
        )
        .bind(account_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
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
    settings_service_with(
        pool,
        Arc::new(PassingGateway),
        Arc::new(MemoryCredentialStore::default()),
    )
}

fn settings_service_with(
    pool: sqlx::SqlitePool,
    gateway: Arc<dyn ImapGateway>,
    credentials: Arc<dyn CredentialStore>,
) -> SettingsService {
    mailbox_state(pool, gateway, credentials).settings_service()
}

fn scheduler(pool: sqlx::SqlitePool, runner: Arc<dyn SyncRunner>) -> Scheduler {
    mailbox_state(
        pool,
        Arc::new(PassingGateway),
        Arc::new(MemoryCredentialStore::default()),
    )
    .scheduler(runner)
}

fn scheduler_with_clock(
    pool: sqlx::SqlitePool,
    runner: Arc<dyn SyncRunner>,
    clock: Arc<dyn Clock>,
) -> Scheduler {
    mailbox_state(
        pool,
        Arc::new(PassingGateway),
        Arc::new(MemoryCredentialStore::default()),
    )
    .scheduler_with_clock(runner, clock)
}

fn mailbox_state(
    pool: sqlx::SqlitePool,
    gateway: Arc<dyn ImapGateway>,
    credentials: Arc<dyn CredentialStore>,
) -> AppState {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path()).unwrap();
    AppState::with_gateway(pool, paths, credentials, gateway)
}

async fn insert_pending_save(
    pool: &sqlx::SqlitePool,
    operation_id: Uuid,
    account_id: Uuid,
    phase: &str,
    is_update: bool,
    email: &str,
) {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO pending_account_saves (\
            operation_id, account_id, phase, is_update, provider, email, imap_host, imap_port, \
            enabled, sync_interval_minutes, created_at, updated_at\
         ) VALUES (?, ?, ?, ?, 'gmail', ?, 'imap.gmail.com', 993, 1, 15, ?, ?)",
    )
    .bind(operation_id.to_string())
    .bind(account_id.to_string())
    .bind(phase)
    .bind(is_update)
    .bind(email)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_pending_cleanup(pool: &sqlx::SqlitePool, operation_id: Uuid) {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO pending_account_save_cleanups (operation_id, created_at, updated_at) \
         VALUES (?, ?, ?)",
    )
    .bind(operation_id.to_string())
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
}

async fn pending_save_count(pool: &sqlx::SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM pending_account_saves")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn pending_cleanup_count(pool: &sqlx::SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM pending_account_save_cleanups")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn cleanup_operation_ids(pool: &sqlx::SqlitePool) -> Vec<Uuid> {
    sqlx::query_scalar::<_, String>(
        "SELECT operation_id FROM pending_account_save_cleanups ORDER BY created_at, operation_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|operation_id| Uuid::parse_str(&operation_id).unwrap())
    .collect()
}

async fn cleanup_rows(pool: &sqlx::SqlitePool) -> String {
    sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(group_concat(operation_id || created_at || updated_at, ''), '') \
         FROM pending_account_save_cleanups",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

fn pending_save_key(operation_id: Uuid) -> String {
    format!("pending-save:{operation_id}")
}

async fn assert_pending_save_cleaned(
    pool: &sqlx::SqlitePool,
    credentials: &dyn CredentialStore,
    operation_id: Uuid,
) {
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pending_account_saves WHERE operation_id = ?",
        )
        .bind(operation_id.to_string())
        .fetch_one(pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        credentials.get(&pending_save_key(operation_id)).unwrap(),
        None
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pending_account_save_cleanups WHERE operation_id = ?",
        )
        .bind(operation_id.to_string())
        .fetch_one(pool)
        .await
        .unwrap(),
        0
    );
}
