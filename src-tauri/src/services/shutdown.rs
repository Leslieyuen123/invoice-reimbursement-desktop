use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::domain::error::AppError;
use crate::services::account_saves::AccountSagaShutdown;
use crate::services::scheduler::SchedulerHandle;

const INTERRUPTED_MESSAGE: &str = "application_shutdown_interrupted";

#[derive(Clone, Default)]
pub(crate) struct RuntimeOperationCoordinator {
    inner: Arc<RuntimeOperationInner>,
}

#[derive(Default)]
struct RuntimeOperationInner {
    state: Mutex<RuntimeOperationState>,
    changed: Notify,
}

#[derive(Default)]
struct RuntimeOperationState {
    closed: bool,
    next_id: u64,
    tasks: HashMap<u64, JoinHandle<()>>,
    first_error: Option<AppError>,
}

pub(crate) struct RuntimeOperationShutdown {
    coordinator: RuntimeOperationCoordinator,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    pub timed_out: bool,
    pub errors: Vec<String>,
    pub interrupted_sync_runs: u64,
}

pub struct ApplicationShutdown {
    scheduler: Option<SchedulerHandle>,
    operations: RuntimeOperationShutdown,
    account_saves: AccountSagaShutdown,
    pool: SqlitePool,
}

impl RuntimeOperationCoordinator {
    pub(crate) async fn run<T, F>(&self, future: F) -> Result<T, AppError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, AppError>> + Send + 'static,
    {
        let receiver = self.spawn(future)?;
        receiver.await.map_err(|_| AppError::Internal {
            message: "tracked runtime operation was interrupted".to_owned(),
        })?
    }

    fn spawn<T, F>(&self, future: F) -> Result<oneshot::Receiver<Result<T, AppError>>, AppError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, AppError>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let mut state = self
            .inner
            .state
            .lock()
            .expect("runtime operation lock poisoned");
        if state.closed {
            return Err(AppError::Conflict {
                message: "application is shutting down".to_owned(),
            });
        }
        let task_id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        let inner = self.inner.clone();
        let task = tokio::spawn(async move {
            let result = future.await;
            let observed = result.as_ref().map(|_| ()).map_err(Clone::clone);
            let _ = sender.send(result);
            {
                let mut state = inner.state.lock().expect("runtime operation lock poisoned");
                if let Err(error) = observed
                    && state.first_error.is_none()
                {
                    state.first_error = Some(error);
                }
                state.tasks.remove(&task_id);
            }
            inner.changed.notify_waiters();
        });
        state.tasks.insert(task_id, task);
        Ok(receiver)
    }

    pub(crate) fn begin_shutdown(&self) -> RuntimeOperationShutdown {
        self.inner
            .state
            .lock()
            .expect("runtime operation lock poisoned")
            .closed = true;
        self.inner.changed.notify_waiters();
        RuntimeOperationShutdown {
            coordinator: self.clone(),
        }
    }
}

impl RuntimeOperationShutdown {
    async fn wait(&self) -> Result<(), AppError> {
        loop {
            let changed = self.coordinator.inner.changed.notified();
            let completed = {
                let state = self
                    .coordinator
                    .inner
                    .state
                    .lock()
                    .expect("runtime operation lock poisoned");
                state.tasks.is_empty().then(|| state.first_error.clone())
            };
            if let Some(first_error) = completed {
                return first_error.map_or(Ok(()), Err);
            }
            changed.await;
        }
    }

    async fn abort_and_wait(&self) -> Result<(), AppError> {
        let (tasks, first_error) = {
            let mut state = self
                .coordinator
                .inner
                .state
                .lock()
                .expect("runtime operation lock poisoned");
            (std::mem::take(&mut state.tasks), state.first_error.clone())
        };
        for task in tasks.values() {
            task.abort();
        }
        for (_, task) in tasks {
            let _ = task.await;
        }
        self.coordinator.inner.changed.notify_waiters();
        first_error.map_or(Ok(()), Err)
    }
}

impl ApplicationShutdown {
    pub(crate) fn new(
        scheduler: Option<SchedulerHandle>,
        operations: RuntimeOperationShutdown,
        account_saves: AccountSagaShutdown,
        pool: SqlitePool,
    ) -> Self {
        Self {
            scheduler,
            operations,
            account_saves,
            pool,
        }
    }

    pub async fn wait(mut self, timeout: Duration) -> ShutdownReport {
        let mut report = ShutdownReport::default();
        let deadline = Instant::now() + timeout;
        let cleanup_reserve = timeout.div_f32(4.0).min(Duration::from_secs(1));
        let graceful_budget = timeout.saturating_sub(cleanup_reserve);
        let mut scheduler = self.scheduler.take();
        let graceful = tokio::time::timeout(graceful_budget, async {
            let scheduler_wait = async {
                match scheduler.take() {
                    Some(scheduler) => scheduler.stop().await,
                    None => Ok(()),
                }
            };
            tokio::join!(
                scheduler_wait,
                self.operations.wait(),
                self.account_saves.wait()
            )
        })
        .await;

        match graceful {
            Ok((scheduler_result, operation_result, saga_result)) => {
                collect_error(&mut report, "scheduler", scheduler_result);
                collect_error(&mut report, "runtime operation", operation_result);
                collect_error(&mut report, "account save", saga_result);
            }
            Err(_) => {
                report.timed_out = true;
                report
                    .errors
                    .push("application graceful shutdown timed out".to_owned());
                let cleanup = async {
                    let (operation_result, saga_result) = tokio::join!(
                        self.operations.abort_and_wait(),
                        self.account_saves.abort_and_wait()
                    );
                    collect_error(&mut report, "runtime operation abort", operation_result);
                    collect_error(&mut report, "account save abort", saga_result);
                    match mark_running_syncs_interrupted(&self.pool).await {
                        Ok(count) => report.interrupted_sync_runs = count,
                        Err(error) => collect_error(&mut report, "sync interruption", Err(error)),
                    }
                };
                if tokio::time::timeout_at(deadline, cleanup).await.is_err() {
                    report
                        .errors
                        .push("forced shutdown cleanup exceeded its deadline".to_owned());
                }
            }
        }
        report
    }
}

fn collect_error(report: &mut ShutdownReport, component: &str, result: Result<(), AppError>) {
    if let Err(error) = result {
        report.errors.push(format!("{component}: {error}"));
    }
}

async fn mark_running_syncs_interrupted(pool: &SqlitePool) -> Result<u64, AppError> {
    let now = chrono::Utc::now().to_rfc3339();
    let result = sqlx::query(
        "UPDATE sync_runs SET finished_at = ?, status = 'failed', error_message = ?
         WHERE status = 'running'",
    )
    .bind(now)
    .bind(INTERRUPTED_MESSAGE)
    .execute(pool)
    .await
    .map_err(|_| AppError::Internal {
        message: "failed to persist interrupted synchronization state".to_owned(),
    })?;
    Ok(result.rows_affected())
}
