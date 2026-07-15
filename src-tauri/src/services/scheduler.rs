use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::db::accounts::MailboxAccountRepository;
pub use crate::db::accounts::SyncRetryState as RetryState;
use crate::domain::error::AppError;
use crate::services::account_saves::AccountSaveCoordinator;
use crate::services::operations::AccountOperationCoordinator;
use crate::services::settings::{BackgroundSyncGate, load_preferences};
use crate::services::sync::SyncService;

#[async_trait]
pub trait SyncRunner: Send + Sync {
    async fn run(&self, account_id: Uuid) -> Result<(), AppError>;
}

#[async_trait]
pub trait SyncStartBarrier: Send + Sync {
    async fn before_start(&self);
}

struct ImmediateStart;

#[async_trait]
impl SyncStartBarrier for ImmediateStart {
    async fn before_start(&self) {}
}

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default)]
pub struct UtcClock;

impl Clock for UtcClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Clone, Debug)]
pub struct ManualClock {
    now: Arc<RwLock<DateTime<Utc>>>,
}

impl ManualClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            now: Arc::new(RwLock::new(now)),
        }
    }

    pub fn set(&self, now: DateTime<Utc>) {
        *self.now.write().expect("manual clock lock poisoned") = now;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.read().expect("manual clock lock poisoned")
    }
}

#[async_trait]
impl SyncRunner for SyncService {
    async fn run(&self, account_id: Uuid) -> Result<(), AppError> {
        SyncService::run(self, account_id).await.map(|_| ())
    }
}

#[derive(Clone)]
pub struct Scheduler {
    pool: SqlitePool,
    accounts: MailboxAccountRepository,
    runner: Arc<dyn SyncRunner>,
    operations: AccountOperationCoordinator,
    account_saves: AccountSaveCoordinator,
    clock: Arc<dyn Clock>,
    background_gate: BackgroundSyncGate,
    start_barrier: Arc<dyn SyncStartBarrier>,
}

impl Scheduler {
    pub(crate) fn with_operations_and_gate(
        pool: SqlitePool,
        runner: Arc<dyn SyncRunner>,
        operations: AccountOperationCoordinator,
        account_saves: AccountSaveCoordinator,
        background_gate: BackgroundSyncGate,
    ) -> Self {
        Self::build(
            pool,
            runner,
            operations,
            account_saves,
            Arc::new(UtcClock),
            background_gate,
            Arc::new(ImmediateStart),
        )
    }

    pub(crate) fn with_operations_clock_and_gate(
        pool: SqlitePool,
        runner: Arc<dyn SyncRunner>,
        operations: AccountOperationCoordinator,
        account_saves: AccountSaveCoordinator,
        clock: Arc<dyn Clock>,
        background_gate: BackgroundSyncGate,
    ) -> Self {
        Self::build(
            pool,
            runner,
            operations,
            account_saves,
            clock,
            background_gate,
            Arc::new(ImmediateStart),
        )
    }

    pub(crate) fn with_operations_runtime(
        pool: SqlitePool,
        runner: Arc<dyn SyncRunner>,
        operations: AccountOperationCoordinator,
        account_saves: AccountSaveCoordinator,
        clock: Arc<dyn Clock>,
        background_gate: BackgroundSyncGate,
        start_barrier: Arc<dyn SyncStartBarrier>,
    ) -> Self {
        Self::build(
            pool,
            runner,
            operations,
            account_saves,
            clock,
            background_gate,
            start_barrier,
        )
    }

    fn build(
        pool: SqlitePool,
        runner: Arc<dyn SyncRunner>,
        operations: AccountOperationCoordinator,
        account_saves: AccountSaveCoordinator,
        clock: Arc<dyn Clock>,
        background_gate: BackgroundSyncGate,
        start_barrier: Arc<dyn SyncStartBarrier>,
    ) -> Self {
        Self {
            accounts: MailboxAccountRepository::new(pool.clone()),
            pool,
            runner,
            operations,
            account_saves,
            clock,
            background_gate,
            start_barrier,
        }
    }

    pub async fn tick(&self, now: DateTime<Utc>) -> Result<(), AppError> {
        let registry = TaskRegistry::default();
        let receivers = self.dispatch_due(now, &registry, None).await?;
        let mut first_error = None;
        for receiver in receivers {
            match receiver.await {
                Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
                Ok(_) => {}
                Err(_) if first_error.is_none() => {
                    first_error = Some(AppError::Internal {
                        message: "scheduled mailbox synchronization result was lost".to_owned(),
                    });
                }
                Err(_) => {}
            }
        }
        registry.wait_all().await?;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn dispatch_due(
        &self,
        now: DateTime<Utc>,
        registry: &TaskRegistry,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Vec<oneshot::Receiver<Result<(), AppError>>>, AppError> {
        if self.account_saves.is_closing() {
            return Ok(Vec::new());
        }
        let reconciliation = self.account_saves.reconcile_all().await?;
        if reconciliation.failure_count() != 0 || reconciliation.skipped_count() != 0 {
            tracing::debug!(
                failed_recoveries = reconciliation.failure_count(),
                skipped_accounts = reconciliation.skipped_count(),
                "background mailbox reconciliation completed with pending work"
            );
        }
        if !load_preferences(&self.pool).await?.background_sync_enabled {
            return Ok(Vec::new());
        }
        let mut receivers = Vec::new();
        for account in self.accounts.list().await? {
            if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::SeqCst)) {
                break;
            }
            if self.account_saves.has_blocking_pending(account.id).await? {
                continue;
            }
            let retry = self.accounts.get_retry_state(account.id).await?;
            let due = match retry {
                Some(retry) => {
                    !retry.suspended && retry.next_retry_at.is_none_or(|next| next <= now)
                }
                None => account.last_synced_at.is_none_or(|last_synced_at| {
                    last_synced_at + Duration::minutes(account.sync_interval_minutes) <= now
                }),
            };
            if account.enabled && due {
                match self
                    .spawn_scheduled_sync(account.id, registry, cancelled)
                    .await?
                {
                    Some(receiver) => receivers.push(receiver),
                    None => break,
                }
            }
        }
        Ok(receivers)
    }

    pub async fn sync_now(&self, account_id: Uuid) -> Result<(), AppError> {
        self.sync_now_with_started(account_id, None)
            .await
            .map_err(SyncExecutionError::into_error)
    }

    async fn sync_now_with_started(
        &self,
        account_id: Uuid,
        started: Option<oneshot::Sender<()>>,
    ) -> Result<(), SyncExecutionError> {
        self.account_saves
            .ensure_open()
            .map_err(SyncExecutionError::Infrastructure)?;
        let _ = self
            .account_saves
            .reconcile_account(account_id)
            .await
            .map_err(SyncExecutionError::Infrastructure)?;
        self.accounts
            .get(account_id)
            .await
            .map_err(SyncExecutionError::Infrastructure)?;
        let _guard = self
            .operations
            .try_lock(account_id)
            .map_err(SyncExecutionError::Infrastructure)?;
        self.account_saves
            .ensure_no_blocking_pending(account_id)
            .await
            .map_err(SyncExecutionError::Infrastructure)?;
        if let Some(started) = started {
            let _ = started.send(());
        }

        match self.runner.run(account_id).await {
            Ok(()) => {
                self.accounts
                    .clear_retry_state(account_id)
                    .await
                    .map_err(SyncExecutionError::Infrastructure)?;
                Ok(())
            }
            Err(error) => {
                match failure_policy(&error) {
                    FailurePolicy::Retry => self
                        .accounts
                        .record_retryable_failure(account_id, self.clock.now(), &error.to_string())
                        .await
                        .map_err(SyncExecutionError::Infrastructure)?,
                    FailurePolicy::Suspend => self
                        .accounts
                        .suspend_retry(account_id, self.clock.now(), &error.to_string())
                        .await
                        .map_err(SyncExecutionError::Infrastructure)?,
                    FailurePolicy::Observe => {
                        return Err(SyncExecutionError::Infrastructure(error));
                    }
                };
                Err(SyncExecutionError::Handled(error))
            }
        }
    }

    async fn spawn_scheduled_sync(
        &self,
        account_id: Uuid,
        registry: &TaskRegistry,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Option<oneshot::Receiver<Result<(), AppError>>>, AppError> {
        let start_guard = self.background_gate.read().await;
        if !load_preferences(&self.pool).await?.background_sync_enabled {
            return Ok(None);
        }
        self.start_barrier.before_start().await;
        if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::SeqCst)) {
            return Ok(None);
        }
        let scheduler = self.clone();
        let (started, started_rx) = oneshot::channel();
        let (result_sender, result_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = match scheduler
                .sync_now_with_started(account_id, Some(started))
                .await
            {
                Ok(()) | Err(SyncExecutionError::Handled(_)) => Ok(()),
                Err(SyncExecutionError::Infrastructure(error)) => {
                    tracing::warn!(
                        account_id = %account_id,
                        "background mailbox synchronization infrastructure failure"
                    );
                    Err(error)
                }
            };
            let _ = result_sender.send(result);
        });
        registry.push(task);
        let _ = started_rx.await;
        drop(start_guard);
        Ok(Some(result_receiver))
    }

    pub async fn retry_state(&self, account_id: Uuid) -> Result<Option<RetryState>, AppError> {
        self.accounts.get_retry_state(account_id).await
    }

    pub fn start(&self) -> SchedulerHandle {
        let scheduler = self.clone();
        let (cancel, mut cancelled) = oneshot::channel();
        let cancellation_flag = Arc::new(AtomicBool::new(false));
        let loop_cancellation = cancellation_flag.clone();
        let registry = TaskRegistry::default();
        let loop_registry = registry.clone();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut cancelled => {
                        loop_cancellation.store(true, Ordering::SeqCst);
                        break;
                    }
                    _ = interval.tick() => {
                        if loop_cancellation.load(Ordering::SeqCst) {
                            break;
                        }
                        if scheduler
                            .dispatch_due(
                                scheduler.clock.now(),
                                &loop_registry,
                                Some(loop_cancellation.as_ref()),
                            )
                            .await
                            .is_err()
                        {
                            tracing::warn!("background mailbox scheduler tick failed");
                        }
                        if loop_registry.reap_finished().await.is_err() {
                            tracing::warn!("background mailbox scheduler task failed");
                        }
                    }
                }
            }
        });
        SchedulerHandle {
            cancel: Some(cancel),
            task: Some(task),
            cancellation_flag,
            registry,
        }
    }
}

enum SyncExecutionError {
    Handled(AppError),
    Infrastructure(AppError),
}

impl SyncExecutionError {
    fn into_error(self) -> AppError {
        match self {
            Self::Handled(error) | Self::Infrastructure(error) => error,
        }
    }
}

enum FailurePolicy {
    Retry,
    Suspend,
    Observe,
}

fn failure_policy(error: &AppError) -> FailurePolicy {
    match error {
        AppError::External {
            retryable: true, ..
        } => FailurePolicy::Retry,
        AppError::External {
            service,
            retryable: false,
            ..
        } if matches!(
            service.as_str(),
            "imap_authentication" | "mailbox_credential"
        ) =>
        {
            FailurePolicy::Suspend
        }
        _ => FailurePolicy::Observe,
    }
}

pub struct SchedulerHandle {
    cancel: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    cancellation_flag: Arc<AtomicBool>,
    registry: TaskRegistry,
}

impl SchedulerHandle {
    pub async fn stop(mut self) -> Result<(), AppError> {
        self.cancellation_flag.store(true, Ordering::SeqCst);
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(task) = self.task.take() {
            AbortOnDropJoinHandle::new(task)
                .join()
                .await
                .map_err(|_| AppError::Internal {
                    message: "background mailbox scheduler task failed".to_owned(),
                })?;
        }
        self.registry.wait_all().await
    }
}

impl Drop for SchedulerHandle {
    fn drop(&mut self) {
        self.cancellation_flag.store(true, Ordering::SeqCst);
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.registry.abort_all();
    }
}

#[derive(Clone, Default)]
struct TaskRegistry {
    state: Arc<StdMutex<TaskRegistryState>>,
}

#[derive(Default)]
struct TaskRegistryState {
    closed: bool,
    tasks: Vec<JoinHandle<()>>,
    child_failed: bool,
}

impl TaskRegistry {
    fn push(&self, task: JoinHandle<()>) {
        let mut state = self.state.lock().expect("scheduler task lock poisoned");
        if state.closed {
            task.abort();
        } else {
            state.tasks.push(task);
        }
    }

    async fn reap_finished(&self) -> Result<(), AppError> {
        let finished = {
            let mut state = self.state.lock().expect("scheduler task lock poisoned");
            let mut finished = Vec::new();
            let mut index = 0;
            while index < state.tasks.len() {
                if state.tasks[index].is_finished() {
                    finished.push(state.tasks.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            finished
        };
        let result = await_tasks(finished).await;
        if result.is_err() {
            self.state
                .lock()
                .expect("scheduler task lock poisoned")
                .child_failed = true;
        }
        result
    }

    async fn wait_all(&self) -> Result<(), AppError> {
        let (tasks, previous_failure) = {
            let mut state = self.state.lock().expect("scheduler task lock poisoned");
            state.closed = true;
            let previous_failure = std::mem::take(&mut state.child_failed);
            (std::mem::take(&mut state.tasks), previous_failure)
        };
        let result = await_tasks(tasks).await;
        if previous_failure || result.is_err() {
            Err(child_task_error())
        } else {
            Ok(())
        }
    }

    fn abort_all(&self) {
        let tasks = {
            let mut state = self.state.lock().expect("scheduler task lock poisoned");
            state.closed = true;
            std::mem::take(&mut state.tasks)
        };
        for task in tasks {
            task.abort();
        }
    }
}

async fn await_tasks(tasks: Vec<JoinHandle<()>>) -> Result<(), AppError> {
    let mut tasks = AbortOnDropTaskSet::new(tasks);
    let mut first_error = None;
    while let Some(task) = tasks.pop_front() {
        if AbortOnDropJoinHandle::new(task).join().await.is_err() && first_error.is_none() {
            first_error = Some(child_task_error());
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn child_task_error() -> AppError {
    AppError::Internal {
        message: "background mailbox scheduler child task failed".to_owned(),
    }
}

struct AbortOnDropJoinHandle {
    task: Option<JoinHandle<()>>,
}

impl AbortOnDropJoinHandle {
    fn new(task: JoinHandle<()>) -> Self {
        Self { task: Some(task) }
    }

    async fn join(mut self) -> Result<(), tokio::task::JoinError> {
        let result = self
            .task
            .as_mut()
            .expect("owned task must be present")
            .await;
        self.task.take();
        result
    }
}

impl Drop for AbortOnDropJoinHandle {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct AbortOnDropTaskSet {
    tasks: VecDeque<JoinHandle<()>>,
}

impl AbortOnDropTaskSet {
    fn new(tasks: Vec<JoinHandle<()>>) -> Self {
        Self {
            tasks: tasks.into(),
        }
    }

    fn pop_front(&mut self) -> Option<JoinHandle<()>> {
        self.tasks.pop_front()
    }
}

impl Drop for AbortOnDropTaskSet {
    fn drop(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use crate::db::accounts::{MailboxProvider, NewMailboxAccount};
    use crate::infra::credentials::MemoryCredentialStore;
    use crate::services::account_saves::AccountSaveCoordinator;

    use super::*;

    struct ImmediateInfrastructureRunner {
        completed: tokio::sync::Notify,
    }

    #[async_trait]
    impl SyncRunner for ImmediateInfrastructureRunner {
        async fn run(&self, _account_id: Uuid) -> Result<(), AppError> {
            self.completed.notify_one();
            Err(AppError::External {
                service: "imap".to_owned(),
                retryable: false,
                message: "sensitive infrastructure detail".to_owned(),
            })
        }
    }

    struct CaptureWriter(Arc<StdMutex<Vec<u8>>>);

    struct DropNotifier {
        dropped: Arc<AtomicBool>,
        signal: Arc<tokio::sync::Notify>,
    }

    impl Drop for DropNotifier {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
            self.signal.notify_one();
        }
    }

    impl Write for CaptureWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("capture lock poisoned")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn fast_background_infrastructure_failure_is_logged_without_detail() {
        let output = Arc::new(StdMutex::new(Vec::new()));
        let writer_output = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || CaptureWriter(writer_output.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let pool = crate::db::connect("sqlite::memory:").await.unwrap();
                    MailboxAccountRepository::new(pool.clone())
                        .insert(NewMailboxAccount {
                            provider: MailboxProvider::Gmail,
                            email: "fast-failure@example.com".to_owned(),
                            imap_host: "imap.gmail.com".to_owned(),
                            imap_port: 993,
                            enabled: true,
                            sync_interval_minutes: 15,
                        })
                        .await
                        .unwrap();
                    let runner = Arc::new(ImmediateInfrastructureRunner {
                        completed: tokio::sync::Notify::new(),
                    });
                    let operations = AccountOperationCoordinator::default();
                    let account_saves = AccountSaveCoordinator::new(
                        pool.clone(),
                        Arc::new(MemoryCredentialStore::default()),
                        operations.clone(),
                    );
                    let handle = Scheduler::with_operations_and_gate(
                        pool,
                        runner.clone(),
                        operations,
                        account_saves,
                        BackgroundSyncGate::default(),
                    )
                    .start();
                    runner.completed.notified().await;
                    handle.stop().await.unwrap();
                });
        });
        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();

        assert!(output.contains("background mailbox synchronization infrastructure failure"));
        assert!(!output.contains("sensitive infrastructure detail"));
    }

    #[tokio::test]
    async fn closed_task_registry_aborts_late_child() {
        let registry = TaskRegistry::default();
        registry.abort_all();
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(AtomicBool::new(false));
        let completed_signal = Arc::new(tokio::sync::Notify::new());
        let child_release = release.clone();
        let child_completed = completed.clone();
        let child_completed_signal = completed_signal.clone();
        let task = tokio::spawn(async move {
            child_release.notified().await;
            child_completed.store(true, Ordering::SeqCst);
            child_completed_signal.notify_one();
        });

        registry.push(task);
        release.notify_one();
        let late_completion = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            completed_signal.notified(),
        )
        .await;

        assert!(late_completion.is_err());
        assert!(!completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn reaped_child_failure_is_retained_for_final_wait() {
        let registry = TaskRegistry::default();
        registry.push(tokio::spawn(async {
            panic!("injected reaped child panic");
        }));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if registry
                    .state
                    .lock()
                    .expect("scheduler task lock poisoned")
                    .tasks
                    .iter()
                    .all(JoinHandle::is_finished)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert!(registry.reap_finished().await.is_err());
        assert!(registry.wait_all().await.is_err());
    }

    #[tokio::test]
    async fn aborting_stop_future_aborts_child_already_taken_for_waiting() {
        let registry = TaskRegistry::default();
        let child_started = Arc::new(tokio::sync::Notify::new());
        let child_dropped = Arc::new(AtomicBool::new(false));
        let child_dropped_signal = Arc::new(tokio::sync::Notify::new());
        let cleanup = Arc::new(tokio::sync::Notify::new());
        let started = child_started.clone();
        let dropped = child_dropped.clone();
        let dropped_signal = child_dropped_signal.clone();
        let child_cleanup = cleanup.clone();
        let child = tokio::spawn(async move {
            let _notifier = DropNotifier {
                dropped,
                signal: dropped_signal,
            };
            started.notify_one();
            child_cleanup.notified().await;
        });
        child_started.notified().await;
        registry.push(child);
        let handle = SchedulerHandle {
            cancel: None,
            task: Some(tokio::spawn(async {})),
            cancellation_flag: Arc::new(AtomicBool::new(false)),
            registry: registry.clone(),
        };

        let stop = tokio::spawn(async move { handle.stop().await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if registry
                    .state
                    .lock()
                    .expect("scheduler task lock poisoned")
                    .tasks
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        stop.abort();
        assert!(stop.await.unwrap_err().is_cancelled());
        let aborted = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            child_dropped_signal.notified(),
        )
        .await;
        cleanup.notify_one();

        assert!(aborted.is_ok());
        assert!(child_dropped.load(Ordering::SeqCst));
    }
}
