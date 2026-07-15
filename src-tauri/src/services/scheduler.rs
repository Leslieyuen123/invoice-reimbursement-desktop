use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::db::accounts::MailboxAccountRepository;
pub use crate::db::accounts::SyncRetryState as RetryState;
use crate::domain::error::AppError;
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
    coordinator: SchedulerCoordinator,
    clock: Arc<dyn Clock>,
    background_gate: BackgroundSyncGate,
    start_barrier: Arc<dyn SyncStartBarrier>,
}

#[derive(Clone, Default)]
pub(crate) struct SchedulerCoordinator {
    locks: Arc<DashMap<Uuid, Arc<Mutex<()>>>>,
}

impl Scheduler {
    pub fn new(pool: SqlitePool, runner: Arc<dyn SyncRunner>) -> Self {
        Self::with_coordinator_and_gate(
            pool,
            runner,
            SchedulerCoordinator::default(),
            BackgroundSyncGate::default(),
        )
    }

    pub fn with_clock(
        pool: SqlitePool,
        runner: Arc<dyn SyncRunner>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::build(
            pool,
            runner,
            SchedulerCoordinator::default(),
            clock,
            BackgroundSyncGate::default(),
            Arc::new(ImmediateStart),
        )
    }

    pub fn with_runtime(
        pool: SqlitePool,
        runner: Arc<dyn SyncRunner>,
        clock: Arc<dyn Clock>,
        background_gate: BackgroundSyncGate,
        start_barrier: Arc<dyn SyncStartBarrier>,
    ) -> Self {
        Self::build(
            pool,
            runner,
            SchedulerCoordinator::default(),
            clock,
            background_gate,
            start_barrier,
        )
    }

    pub(crate) fn with_coordinator_and_gate(
        pool: SqlitePool,
        runner: Arc<dyn SyncRunner>,
        coordinator: SchedulerCoordinator,
        background_gate: BackgroundSyncGate,
    ) -> Self {
        Self::build(
            pool,
            runner,
            coordinator,
            Arc::new(UtcClock),
            background_gate,
            Arc::new(ImmediateStart),
        )
    }

    fn build(
        pool: SqlitePool,
        runner: Arc<dyn SyncRunner>,
        coordinator: SchedulerCoordinator,
        clock: Arc<dyn Clock>,
        background_gate: BackgroundSyncGate,
        start_barrier: Arc<dyn SyncStartBarrier>,
    ) -> Self {
        Self {
            accounts: MailboxAccountRepository::new(pool.clone()),
            pool,
            runner,
            coordinator,
            clock,
            background_gate,
            start_barrier,
        }
    }

    pub async fn tick(&self, now: DateTime<Utc>) -> Result<(), AppError> {
        if !load_preferences(&self.pool).await?.background_sync_enabled {
            return Ok(());
        }
        for account in self.accounts.list().await? {
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
                let started = self.start_scheduled_sync(account.id).await?;
                if !started {
                    break;
                }
            }
        }
        Ok(())
    }

    pub async fn sync_now(&self, account_id: Uuid) -> Result<(), AppError> {
        self.sync_now_with_started(account_id, None).await
    }

    async fn sync_now_with_started(
        &self,
        account_id: Uuid,
        started: Option<oneshot::Sender<()>>,
    ) -> Result<(), AppError> {
        self.accounts.get(account_id).await?;
        let lock = self
            .coordinator
            .locks
            .entry(account_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.try_lock().map_err(|_| AppError::Conflict {
            message: "mailbox synchronization is already running".to_owned(),
        })?;
        if let Some(started) = started {
            let _ = started.send(());
        }

        match self.runner.run(account_id).await {
            Ok(()) => {
                self.accounts.clear_retry_state(account_id).await?;
                Ok(())
            }
            Err(error) => {
                self.record_failure(account_id, self.clock.now(), &error)
                    .await?;
                Err(error)
            }
        }
    }

    async fn start_scheduled_sync(&self, account_id: Uuid) -> Result<bool, AppError> {
        let start_guard = self.background_gate.read().await;
        if !load_preferences(&self.pool).await?.background_sync_enabled {
            return Ok(false);
        }
        self.start_barrier.before_start().await;
        let scheduler = self.clone();
        let (started, started_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            scheduler
                .sync_now_with_started(account_id, Some(started))
                .await
        });
        let _ = started_rx.await;
        drop(start_guard);
        match task.await {
            Ok(_) => Ok(true),
            Err(_) => Err(AppError::Internal {
                message: "scheduled mailbox synchronization task failed".to_owned(),
            }),
        }
    }

    pub async fn retry_state(&self, account_id: Uuid) -> Result<Option<RetryState>, AppError> {
        self.accounts.get_retry_state(account_id).await
    }

    pub fn start(&self) -> SchedulerHandle {
        let scheduler = self.clone();
        let (cancel, mut cancelled) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = &mut cancelled => break,
                    _ = interval.tick() => {
                        if scheduler.tick(scheduler.clock.now()).await.is_err() {
                            tracing::warn!("background mailbox scheduler tick failed");
                        }
                    }
                }
            }
        });
        SchedulerHandle {
            cancel: Some(cancel),
            task: Some(task),
        }
    }

    async fn record_failure(
        &self,
        account_id: Uuid,
        now: DateTime<Utc>,
        error: &AppError,
    ) -> Result<(), AppError> {
        if matches!(
            error,
            AppError::External {
                retryable: true,
                ..
            }
        ) {
            self.accounts
                .record_retryable_failure(account_id, now, &error.to_string())
                .await?;
        } else {
            self.accounts
                .suspend_retry(account_id, now, &error.to_string())
                .await?;
        }
        Ok(())
    }
}

pub struct SchedulerHandle {
    cancel: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl SchedulerHandle {
    pub async fn stop(mut self) -> Result<(), AppError> {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.map_err(|_| AppError::Internal {
                message: "background mailbox scheduler task failed".to_owned(),
            })?;
        }
        Ok(())
    }
}

impl Drop for SchedulerHandle {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
