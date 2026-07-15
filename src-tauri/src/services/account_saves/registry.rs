use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::domain::error::AppError;

#[derive(Clone)]
pub struct AccountSagaRegistry {
    inner: Arc<AccountSagaRegistryInner>,
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
}

#[derive(Default)]
struct AccountSagaRegistryState {
    closed: bool,
    tasks: Vec<JoinHandle<Result<(), AppError>>>,
    first_error: Option<AppError>,
}

impl AccountSagaRegistry {
    pub fn spawn<T, F>(&self, future: F) -> oneshot::Receiver<Result<T, AppError>>
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
        let task = tokio::spawn(async move {
            let result = future.await;
            let observed = result.as_ref().map(|_| ()).map_err(Clone::clone);
            if sender.send(result).is_err() && observed.is_err() {
                tracing::warn!("mailbox account save failed after its caller stopped waiting");
            }
            observed
        });
        state.tasks.push(task);
        receiver
    }

    pub async fn wait_all(&self) -> Result<(), AppError> {
        let (tasks, previous_error) = {
            let mut state = self.inner.state.lock().expect("account saga lock poisoned");
            state.closed = true;
            (std::mem::take(&mut state.tasks), state.first_error.clone())
        };
        let result = await_tasks(tasks, previous_error).await;
        if let Err(error) = &result {
            let mut state = self.inner.state.lock().expect("account saga lock poisoned");
            if state.first_error.is_none() {
                state.first_error = Some(error.clone());
            }
        }
        result
    }

    pub fn abort_all(&self) {
        let tasks = {
            let mut state = self.inner.state.lock().expect("account saga lock poisoned");
            state.closed = true;
            std::mem::take(&mut state.tasks)
        };
        for task in tasks {
            task.abort();
        }
    }
}

impl Drop for AccountSagaRegistryInner {
    fn drop(&mut self) {
        let tasks = self
            .state
            .get_mut()
            .expect("account saga lock poisoned")
            .tasks
            .drain(..)
            .collect::<Vec<_>>();
        for task in tasks {
            task.abort();
        }
    }
}

async fn await_tasks(
    tasks: Vec<JoinHandle<Result<(), AppError>>>,
    mut first_error: Option<AppError>,
) -> Result<(), AppError> {
    let mut tasks = AbortOnDropTaskSet::new(tasks);
    while let Some(task) = tasks.pop_front() {
        match AbortOnDropJoinHandle::new(task).join().await {
            Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
            Ok(_) => {}
            Err(_) if first_error.is_none() => first_error = Some(child_task_error()),
            Err(_) => {}
        }
    }
    first_error.map_or(Ok(()), Err)
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
            .expect("owned task must be present")
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

struct AbortOnDropTaskSet<T> {
    tasks: VecDeque<JoinHandle<T>>,
}

impl<T> AbortOnDropTaskSet<T> {
    fn new(tasks: Vec<JoinHandle<T>>) -> Self {
        Self {
            tasks: tasks.into(),
        }
    }

    fn pop_front(&mut self) -> Option<JoinHandle<T>> {
        self.tasks.pop_front()
    }
}

impl<T> Drop for AbortOnDropTaskSet<T> {
    fn drop(&mut self) {
        for task in self.tasks.drain(..) {
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
                    registry.wait_all().await.unwrap_err()
                })
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();

        assert!(!error.to_string().contains(secret));
        assert!(!error.to_string().chars().any(char::is_control));
        assert!(logs.contains("mailbox account save failed after its caller stopped waiting"));
        assert!(!logs.contains(secret));
    }
}
