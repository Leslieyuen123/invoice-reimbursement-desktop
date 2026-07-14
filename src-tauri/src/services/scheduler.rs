use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::db::accounts::MailboxAccountRepository;
use crate::domain::error::AppError;
use crate::services::settings::load_preferences;
use crate::services::sync::SyncService;

#[async_trait]
pub trait SyncRunner: Send + Sync {
    async fn run(&self, account_id: Uuid) -> Result<(), AppError>;
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
}

#[derive(Clone, Default)]
pub(crate) struct SchedulerCoordinator {
    locks: Arc<DashMap<Uuid, Arc<Mutex<()>>>>,
    retries: Arc<DashMap<Uuid, RetryState>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryState {
    pub failures: u32,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub suspended: bool,
}

impl Scheduler {
    pub fn new(pool: SqlitePool, runner: Arc<dyn SyncRunner>) -> Self {
        Self::with_coordinator(pool, runner, SchedulerCoordinator::default())
    }

    pub(crate) fn with_coordinator(
        pool: SqlitePool,
        runner: Arc<dyn SyncRunner>,
        coordinator: SchedulerCoordinator,
    ) -> Self {
        Self {
            accounts: MailboxAccountRepository::new(pool.clone()),
            pool,
            runner,
            coordinator,
        }
    }

    pub async fn tick(&self, now: DateTime<Utc>) -> Result<(), AppError> {
        if !load_preferences(&self.pool).await?.background_sync_enabled {
            return Ok(());
        }
        for account in self.accounts.list().await? {
            let due = account.last_synced_at.is_none_or(|last_synced_at| {
                last_synced_at + Duration::minutes(account.sync_interval_minutes) <= now
            });
            let retry_due = self
                .coordinator
                .retries
                .get(&account.id)
                .is_none_or(|retry| {
                    !retry.suspended && retry.next_retry_at.is_none_or(|next| next <= now)
                });
            if account.enabled && due && retry_due {
                if !load_preferences(&self.pool).await?.background_sync_enabled {
                    break;
                }
                let _ = self.sync_now(account.id, now).await;
            }
        }
        Ok(())
    }

    pub async fn sync_now(&self, account_id: Uuid, now: DateTime<Utc>) -> Result<(), AppError> {
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

        match self.runner.run(account_id).await {
            Ok(()) => {
                self.coordinator.retries.remove(&account_id);
                self.accounts.set_last_error(account_id, None).await?;
                Ok(())
            }
            Err(error) => {
                self.accounts
                    .set_last_error(account_id, Some(&error.to_string()))
                    .await?;
                self.record_failure(account_id, now, &error);
                Err(error)
            }
        }
    }

    pub fn retry_state(&self, account_id: Uuid) -> Option<RetryState> {
        self.coordinator
            .retries
            .get(&account_id)
            .map(|state| state.clone())
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
                        if scheduler.tick(Utc::now()).await.is_err() {
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

    fn record_failure(&self, account_id: Uuid, now: DateTime<Utc>, error: &AppError) {
        if matches!(
            error,
            AppError::External {
                retryable: true,
                ..
            }
        ) {
            let failures = self
                .coordinator
                .retries
                .get(&account_id)
                .map_or(1, |state| state.failures.saturating_add(1));
            let delay_minutes = match failures {
                1 => 1,
                2 => 5,
                _ => 15,
            };
            self.coordinator.retries.insert(
                account_id,
                RetryState {
                    failures,
                    next_retry_at: Some(now + Duration::minutes(delay_minutes)),
                    suspended: false,
                },
            );
        } else {
            self.coordinator.retries.insert(
                account_id,
                RetryState {
                    failures: 0,
                    next_retry_at: None,
                    suspended: true,
                },
            );
        }
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
