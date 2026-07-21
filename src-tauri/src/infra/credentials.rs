use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::domain::error::{AppError, sanitize_app_error};

#[cfg(not(test))]
const CREDENTIAL_READ_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(test)]
const CREDENTIAL_READ_TIMEOUT: Duration = Duration::from_millis(20);

pub trait CredentialStore: Send + Sync {
    fn get(&self, account_id: &str) -> Result<Option<String>, AppError>;
    fn set(&self, account_id: &str, secret: &str) -> Result<(), AppError>;
    fn delete(&self, account_id: &str) -> Result<(), AppError>;
}

pub async fn get_credential(
    store: Arc<dyn CredentialStore>,
    account_id: String,
) -> Result<Option<String>, AppError> {
    get_credential_with_timeout(store, account_id, CREDENTIAL_READ_TIMEOUT).await
}

async fn get_credential_with_timeout(
    store: Arc<dyn CredentialStore>,
    account_id: String,
    timeout: Duration,
) -> Result<Option<String>, AppError> {
    let task = tokio::task::spawn_blocking(move || store.get(&account_id));
    let result = tokio::time::timeout(timeout, task)
        .await
        .map_err(|_| credential_read_timeout_error())?
        .map_err(|_| credential_task_error("read"))?;
    result.map_err(|error| sanitize_app_error(error, &[]))
}

pub async fn set_credential(
    store: Arc<dyn CredentialStore>,
    account_id: String,
    secret: String,
) -> Result<(), AppError> {
    let error_secret = secret.clone();
    let result = tokio::task::spawn_blocking(move || store.set(&account_id, &secret))
        .await
        .map_err(|_| credential_task_error("write"))?;
    result.map_err(|error| sanitize_app_error(error, &[&error_secret]))
}

pub async fn delete_credential(
    store: Arc<dyn CredentialStore>,
    account_id: String,
) -> Result<(), AppError> {
    let result = tokio::task::spawn_blocking(move || store.delete(&account_id))
        .await
        .map_err(|_| credential_task_error("delete"))?;
    result.map_err(|error| sanitize_app_error(error, &[]))
}

const KEYRING_SERVICE: &str = "com.invoice-desk.credentials";

#[derive(Clone, Copy, Debug, Default)]
pub struct KeyringCredentialStore;

impl KeyringCredentialStore {
    pub fn new() -> Self {
        Self
    }

    fn entry(account_id: &str) -> Result<keyring::Entry, AppError> {
        validate_account_id(account_id)?;
        keyring::Entry::new(KEYRING_SERVICE, account_id)
            .map_err(|_| keyring_error("failed to access credential store"))
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, account_id: &str) -> Result<Option<String>, AppError> {
        match Self::entry(account_id)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(keyring_error("failed to read credential")),
        }
    }

    fn set(&self, account_id: &str, secret: &str) -> Result<(), AppError> {
        validate_secret(secret)?;
        Self::entry(account_id)?
            .set_password(secret)
            .map_err(|_| keyring_error("failed to store credential"))
    }

    fn delete(&self, account_id: &str) -> Result<(), AppError> {
        match Self::entry(account_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(keyring_error("failed to delete credential")),
        }
    }
}

#[derive(Clone, Default)]
pub struct MemoryCredentialStore {
    secrets: Arc<RwLock<HashMap<String, String>>>,
}

impl fmt::Debug for MemoryCredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MemoryCredentialStore")
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn get(&self, account_id: &str) -> Result<Option<String>, AppError> {
        validate_account_id(account_id)?;
        self.secrets
            .read()
            .map(|secrets| secrets.get(account_id).cloned())
            .map_err(|_| lock_error())
    }

    fn set(&self, account_id: &str, secret: &str) -> Result<(), AppError> {
        validate_account_id(account_id)?;
        validate_secret(secret)?;
        self.secrets
            .write()
            .map_err(|_| lock_error())?
            .insert(account_id.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, account_id: &str) -> Result<(), AppError> {
        validate_account_id(account_id)?;
        self.secrets
            .write()
            .map_err(|_| lock_error())?
            .remove(account_id);
        Ok(())
    }
}

fn validate_account_id(account_id: &str) -> Result<(), AppError> {
    if account_id.trim().is_empty() {
        return Err(AppError::validation(
            "account_id",
            "account ID must not be blank",
        ));
    }

    Ok(())
}

fn validate_secret(secret: &str) -> Result<(), AppError> {
    if secret.trim().is_empty() {
        return Err(AppError::validation(
            "secret",
            "credential secret must not be blank",
        ));
    }

    Ok(())
}

fn keyring_error(message: &str) -> AppError {
    AppError::External {
        service: "keyring".to_owned(),
        retryable: false,
        message: message.to_owned(),
    }
}

fn lock_error() -> AppError {
    AppError::Internal {
        message: "credential storage lock is unavailable".to_owned(),
    }
}

fn credential_task_error(operation: &str) -> AppError {
    AppError::Internal {
        message: format!("credential {operation} task failed"),
    }
}

fn credential_read_timeout_error() -> AppError {
    AppError::External {
        service: "mailbox_credential".to_owned(),
        retryable: false,
        message: "读取邮箱凭据超时，请解锁 Mac 后重试".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{AppError, CredentialStore, get_credential};

    struct SlowCredentialStore;

    impl CredentialStore for SlowCredentialStore {
        fn get(&self, _account_id: &str) -> Result<Option<String>, AppError> {
            std::thread::sleep(Duration::from_millis(100));
            Ok(Some("test-only-secret".to_owned()))
        }

        fn set(&self, _account_id: &str, _secret: &str) -> Result<(), AppError> {
            Ok(())
        }

        fn delete(&self, _account_id: &str) -> Result<(), AppError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn credential_read_timeout_returns_unlock_guidance() {
        let error = get_credential(Arc::new(SlowCredentialStore), "account-id".to_owned())
            .await
            .unwrap_err();

        assert_eq!(
            error,
            AppError::External {
                service: "mailbox_credential".to_owned(),
                retryable: false,
                message: "读取邮箱凭据超时，请解锁 Mac 后重试".to_owned(),
            }
        );
    }
}
