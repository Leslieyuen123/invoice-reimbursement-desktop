use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

use crate::domain::error::AppError;

#[derive(Clone)]
pub(super) struct AccountSagaRegistry {
    inner: Arc<AccountSagaRegistryInner>,
}

#[derive(Clone)]
pub struct AccountSagaShutdown {
    registry: AccountSagaRegistry,
}

impl Default for AccountSagaRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(AccountSagaRegistryInner::default()),
        }
    }
}

#[derive(Default)]
struct AccountSagaRegistryInner {
    state: StdMutex<AccountSagaRegistryState>,
    changed: Notify,
}

#[derive(Default)]
struct AccountSagaRegistryState {
    closed: bool,
    next_task_id: u64,
    tasks: HashMap<u64, JoinHandle<()>>,
    first_error: Option<AppError>,
}

impl AccountSagaRegistry {
    pub(super) fn spawn<T, F>(&self, future: F) -> oneshot::Receiver<Result<T, AppError>>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, AppError>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let mut state = self.inner.state.lock().expect("account saga lock poisoned");
        if state.closed {
            let _ = sender.send(Err(registry_closed_error()));
            return receiver;
        }
        let task_id = state.next_task_id;
        state.next_task_id = state.next_task_id.wrapping_add(1);
        let inner = self.inner.clone();
        let task = tokio::spawn(async move {
            let result = match AbortOnDropJoinHandle::new(tokio::spawn(future))
                .join()
                .await
            {
                Ok(result) => result,
                Err(_) => Err(child_task_error()),
            };
            let observed = result.as_ref().map(|_| ()).map_err(Clone::clone);
            if sender.send(result).is_err() && observed.is_err() {
                tracing::warn!("mailbox account save failed after its caller stopped waiting");
            }
            let completed_handle = {
                let mut state = inner.state.lock().expect("account saga lock poisoned");
                if let Err(error) = observed
                    && state.first_error.is_none()
                {
                    state.first_error = Some(error);
                }
                state.tasks.remove(&task_id)
            };
            drop(completed_handle);
            inner.changed.notify_waiters();
        });
        state.tasks.insert(task_id, task);
        receiver
    }

    pub(super) fn ensure_open(&self) -> Result<(), AppError> {
        if self.is_closing() {
            Err(registry_closed_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn is_closing(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("account saga lock poisoned")
            .closed
    }

    pub(super) fn begin_shutdown(&self) -> AccountSagaShutdown {
        self.inner
            .state
            .lock()
            .expect("account saga lock poisoned")
            .closed = true;
        self.inner.changed.notify_waiters();
        AccountSagaShutdown {
            registry: self.clone(),
        }
    }

    async fn wait_for_completion(&self) -> Result<(), AppError> {
        loop {
            let changed = self.inner.changed.notified();
            let completed = {
                let state = self.inner.state.lock().expect("account saga lock poisoned");
                state.tasks.is_empty().then(|| state.first_error.clone())
            };
            if let Some(first_error) = completed {
                return first_error.map_or(Ok(()), Err);
            }
            changed.await;
        }
    }

    #[cfg(test)]
    fn active_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .expect("account saga lock poisoned")
            .tasks
            .len()
    }
}

impl AccountSagaShutdown {
    pub async fn wait(&self) -> Result<(), AppError> {
        self.registry.wait_for_completion().await
    }

    pub async fn abort_and_wait(&self) -> Result<(), AppError> {
        let (tasks, first_error) = {
            let mut state = self
                .registry
                .inner
                .state
                .lock()
                .expect("account saga lock poisoned");
            (std::mem::take(&mut state.tasks), state.first_error.clone())
        };
        for task in tasks.values() {
            task.abort();
        }
        for (_, task) in tasks {
            let _ = task.await;
        }
        self.registry.inner.changed.notify_waiters();
        first_error.map_or(Ok(()), Err)
    }
}

struct AbortOnDropJoinHandle<T> {
    task: Option<JoinHandle<T>>,
}

impl<T> AbortOnDropJoinHandle<T> {
    fn new(task: JoinHandle<T>) -> Self {
        Self { task: Some(task) }
    }

    async fn join(mut self) -> Result<T, tokio::task::JoinError> {
        let result = self
            .task
            .as_mut()
            .expect("owned account saga task must be present")
            .await;
        self.task.take();
        result
    }
}

impl<T> Drop for AbortOnDropJoinHandle<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn registry_closed_error() -> AppError {
    AppError::Conflict {
        message: "mailbox account save registry is shutting down".to_owned(),
    }
}

fn child_task_error() -> AppError {
    AppError::Internal {
        message: "mailbox account save child task failed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use crate::infra::credentials::{CredentialStore, set_credential};

    use super::*;

    struct LeakingCredentialStore;

    impl CredentialStore for LeakingCredentialStore {
        fn get(&self, _account_id: &str) -> Result<Option<String>, AppError> {
            Ok(None)
        }

        fn set(&self, _account_id: &str, secret: &str) -> Result<(), AppError> {
            Err(AppError::External {
                service: "keyring".to_owned(),
                retryable: false,
                message: format!("credential backend leaked {secret}\n\0"),
            })
        }

        fn delete(&self, _account_id: &str) -> Result<(), AppError> {
            Ok(())
        }
    }

    struct CaptureWriter(Arc<StdMutex<Vec<u8>>>);

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
    fn dropped_receiver_retains_sanitized_error_and_logs_no_secret() {
        let output = Arc::new(StdMutex::new(Vec::new()));
        let writer_output = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || CaptureWriter(writer_output.clone()))
            .finish();
        let secret = "registry-secret-must-not-leak";
        let error = tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let registry = AccountSagaRegistry::default();
                    let receiver = registry.spawn(set_credential(
                        Arc::new(LeakingCredentialStore),
                        "pending-save:test".to_owned(),
                        secret.to_owned(),
                    ));
                    drop(receiver);
                    registry.begin_shutdown().wait().await.unwrap_err()
                })
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();

        assert!(!error.to_string().contains(secret));
        assert!(!error.to_string().chars().any(char::is_control));
        assert!(logs.contains("mailbox account save failed after its caller stopped waiting"));
        assert!(!logs.contains(secret));
    }

    #[tokio::test]
    async fn completed_tasks_are_reaped_while_first_error_is_retained_for_shutdown() {
        let registry = AccountSagaRegistry::default();
        let receiver = registry.spawn(async {
            Err::<(), _>(AppError::Internal {
                message: "completed account saga failure".to_owned(),
            })
        });

        assert!(receiver.await.unwrap().is_err());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while registry.active_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let shutdown = registry.begin_shutdown();

        assert_eq!(registry.active_count(), 0);
        assert!(
            shutdown
                .wait()
                .await
                .unwrap_err()
                .to_string()
                .contains("completed account saga failure")
        );
    }

    #[tokio::test]
    async fn forced_shutdown_aborts_pending_child_and_retains_an_earlier_error() {
        struct DropSignal(Arc<Notify>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.notify_one();
            }
        }

        let registry = AccountSagaRegistry::default();
        let failed = registry.spawn(async {
            Err::<(), _>(AppError::Internal {
                message: "account saga failed before shutdown".to_owned(),
            })
        });
        assert!(failed.await.unwrap().is_err());
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let child_started = started.clone();
        let child_dropped = dropped.clone();
        let pending = registry.spawn(async move {
            let _drop_signal = DropSignal(child_dropped);
            child_started.notify_one();
            std::future::pending::<Result<(), AppError>>().await
        });
        started.notified().await;

        let error = registry
            .begin_shutdown()
            .abort_and_wait()
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("account saga failed before shutdown")
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), dropped.notified())
            .await
            .expect("forced shutdown must abort the real saga child");
        assert!(pending.await.is_err());
    }
}
