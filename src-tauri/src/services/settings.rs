use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use uuid::Uuid;

use crate::db::accounts::{
    MailboxAccount, MailboxAccountRepository, MailboxProvider, NewMailboxAccount,
};
use crate::domain::error::AppError;
use crate::infra::credentials::CredentialStore;
use crate::infra::imap::{ImapAccountConfig, ImapGateway};

pub struct SaveAccountInput {
    pub id: Option<Uuid>,
    pub provider: MailboxProvider,
    pub email: String,
    pub secret: String,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub enabled: bool,
    pub sync_interval_minutes: i64,
}

pub struct TestAccountInput {
    pub provider: MailboxProvider,
    pub email: String,
    pub secret: String,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Preferences {
    pub background_sync_enabled: bool,
    pub export_directory: String,
    pub batch_directory_pattern: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            background_sync_enabled: true,
            export_directory: "exports".to_owned(),
            batch_directory_pattern: "{batchName}-{timestamp}".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferencesInput {
    pub background_sync_enabled: bool,
    pub export_directory: String,
    pub batch_directory_pattern: String,
}

#[derive(Clone, Default)]
pub struct BackgroundSyncGate {
    lock: Arc<RwLock<()>>,
}

impl BackgroundSyncGate {
    pub(crate) async fn read(&self) -> RwLockReadGuard<'_, ()> {
        self.lock.read().await
    }

    async fn write(&self) -> RwLockWriteGuard<'_, ()> {
        self.lock.write().await
    }
}

#[derive(Clone)]
pub struct SettingsService {
    pool: SqlitePool,
    gateway: Arc<dyn ImapGateway>,
    credentials: Arc<dyn CredentialStore>,
    accounts: MailboxAccountRepository,
    background_gate: BackgroundSyncGate,
}

impl SettingsService {
    pub fn new(
        pool: SqlitePool,
        gateway: Arc<dyn ImapGateway>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Self {
        Self::with_background_gate(pool, gateway, credentials, BackgroundSyncGate::default())
    }

    pub fn with_background_gate(
        pool: SqlitePool,
        gateway: Arc<dyn ImapGateway>,
        credentials: Arc<dyn CredentialStore>,
        background_gate: BackgroundSyncGate,
    ) -> Self {
        Self {
            accounts: MailboxAccountRepository::new(pool.clone()),
            pool,
            gateway,
            credentials,
            background_gate,
        }
    }

    pub async fn save_account(&self, input: SaveAccountInput) -> Result<MailboxAccount, AppError> {
        let config = account_config(
            input.provider,
            &input.email,
            input.imap_host.as_deref(),
            input.imap_port,
        )?;
        if let Err(error) = self.gateway.test_connection(&config, &input.secret).await {
            return Err(sanitize_error(error, &input.secret));
        }

        let previous_secret = input
            .id
            .map(|id| self.credentials.get(&id.to_string()))
            .transpose()?
            .flatten();

        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let account = self
            .accounts
            .save_metadata(
                &mut transaction,
                input.id,
                NewMailboxAccount {
                    provider: input.provider,
                    email: input.email,
                    imap_host: config.host,
                    imap_port: i64::from(config.port),
                    enabled: input.enabled,
                    sync_interval_minutes: input.sync_interval_minutes,
                },
            )
            .await?;
        if let Err(error) = self.credentials.set(&account.id.to_string(), &input.secret) {
            transaction.rollback().await.map_err(database_error)?;
            return Err(sanitize_error(error, &input.secret));
        }
        if let Err(error) = transaction.commit().await {
            let restored = match previous_secret {
                Some(previous_secret) => self
                    .credentials
                    .set(&account.id.to_string(), &previous_secret),
                None => self.credentials.delete(&account.id.to_string()),
            };
            if restored.is_err() {
                return Err(AppError::Internal {
                    message: "failed to restore credential after database commit failure"
                        .to_owned(),
                });
            }
            return Err(database_error(error));
        }
        Ok(account)
    }

    pub async fn test_account(&self, input: TestAccountInput) -> Result<(), AppError> {
        let config = account_config(
            input.provider,
            &input.email,
            input.imap_host.as_deref(),
            input.imap_port,
        )?;
        self.gateway
            .test_connection(&config, &input.secret)
            .await
            .map_err(|error| sanitize_error(error, &input.secret))
    }

    pub async fn delete_account(&self, id: Uuid) -> Result<(), AppError> {
        self.accounts.get(id).await?;
        self.accounts.set_enabled(id, false).await?;
        self.credentials.delete(&id.to_string())?;
        self.accounts.delete(id).await
    }

    pub async fn preferences(&self) -> Result<Preferences, AppError> {
        load_preferences(&self.pool).await
    }

    pub async fn save_preferences(&self, input: PreferencesInput) -> Result<Preferences, AppError> {
        let preferences = Preferences {
            background_sync_enabled: input.background_sync_enabled,
            export_directory: input.export_directory.trim().to_owned(),
            batch_directory_pattern: input.batch_directory_pattern.trim().to_owned(),
        };
        validate_preferences(&preferences)?;
        let value_json = serde_json::to_string(&preferences).map_err(|_| AppError::Internal {
            message: "failed to serialize preferences".to_owned(),
        })?;
        let _gate = self.background_gate.write().await;
        sqlx::query(
            "INSERT INTO settings (key, value_json, updated_at) VALUES ('preferences', ?, ?) \
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, \
                updated_at = excluded.updated_at",
        )
        .bind(value_json)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        Ok(preferences)
    }
}

pub(crate) async fn load_preferences(pool: &SqlitePool) -> Result<Preferences, AppError> {
    let value = sqlx::query_scalar::<_, String>(
        "SELECT value_json FROM settings WHERE key = 'preferences'",
    )
    .fetch_optional(pool)
    .await
    .map_err(database_error)?;
    let Some(value) = value else {
        return Ok(Preferences::default());
    };
    let preferences =
        serde_json::from_str::<Preferences>(&value).map_err(|_| AppError::Internal {
            message: "saved preferences contain invalid JSON".to_owned(),
        })?;
    validate_preferences(&preferences)?;
    Ok(preferences)
}

fn validate_preferences(preferences: &Preferences) -> Result<(), AppError> {
    if preferences.export_directory.is_empty() || preferences.export_directory.contains('\0') {
        return Err(AppError::validation(
            "export_directory",
            "export directory must not be blank or contain NUL",
        ));
    }
    if preferences.batch_directory_pattern != "{batchName}-{timestamp}" {
        return Err(AppError::validation(
            "batch_directory_pattern",
            "batch directory pattern must be {batchName}-{timestamp}",
        ));
    }
    Ok(())
}

fn account_config(
    provider: MailboxProvider,
    email: &str,
    host: Option<&str>,
    port: Option<u16>,
) -> Result<ImapAccountConfig, AppError> {
    if email.trim().is_empty() {
        return Err(AppError::validation("email", "email must not be blank"));
    }
    let mut config = ImapAccountConfig::provider_default(provider, email.trim());
    if let Some(host) = host {
        let host = host.trim();
        if !valid_imap_host(host) {
            return Err(AppError::validation(
                "imap_host",
                "IMAP host must be a hostname or IP address without a scheme or path",
            ));
        }
        config.host = host.to_owned();
    }
    if let Some(port) = port {
        if port == 0 {
            return Err(AppError::validation(
                "imap_port",
                "port must be between 1 and 65535",
            ));
        }
        config.port = port;
    }
    Ok(config)
}

fn valid_imap_host(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
    })
}

fn sanitize_error(error: AppError, secret: &str) -> AppError {
    let sanitize = |message: String| {
        if secret.is_empty() {
            message
        } else {
            message.replace(secret, "[redacted]")
        }
    };
    match error {
        AppError::Validation { field, message } => AppError::Validation {
            field,
            message: sanitize(message),
        },
        AppError::NotFound { entity, message } => AppError::NotFound {
            entity,
            message: sanitize(message),
        },
        AppError::Conflict { message } => AppError::Conflict {
            message: sanitize(message),
        },
        AppError::External {
            service,
            retryable,
            message,
        } => AppError::External {
            service,
            retryable,
            message: sanitize(message),
        },
        AppError::Internal { message } => AppError::Internal {
            message: sanitize(message),
        },
    }
}

fn database_error(error: sqlx::Error) -> AppError {
    AppError::Internal {
        message: format!("failed to save mailbox settings: {error}"),
    }
}
