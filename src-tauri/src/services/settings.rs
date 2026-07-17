use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use uuid::Uuid;

use crate::db::accounts::{
    MailboxAccount, MailboxAccountRepository, MailboxProvider, NewMailboxAccount,
};
use crate::domain::error::{AppError, sanitize_app_error};
use crate::infra::credentials::{CredentialStore, delete_credential, get_credential};
use crate::infra::imap::{ImapAccountConfig, ImapGateway};
use crate::services::account_saves::AccountSaveCoordinator;
use crate::services::operations::AccountOperationCoordinator;

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
    pub id: Option<Uuid>,
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

#[derive(Clone, Default)]
pub struct ExportPreferenceGate {
    lock: Arc<RwLock<()>>,
}

impl ExportPreferenceGate {
    pub(crate) async fn read(&self) -> RwLockReadGuard<'_, ()> {
        self.lock.read().await
    }

    async fn write(&self) -> RwLockWriteGuard<'_, ()> {
        self.lock.write().await
    }
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
    export_preference_gate: ExportPreferenceGate,
    operations: AccountOperationCoordinator,
    account_saves: AccountSaveCoordinator,
}

impl SettingsService {
    pub(crate) fn with_runtime(
        pool: SqlitePool,
        gateway: Arc<dyn ImapGateway>,
        credentials: Arc<dyn CredentialStore>,
        background_gate: BackgroundSyncGate,
        export_preference_gate: ExportPreferenceGate,
        operations: AccountOperationCoordinator,
        account_saves: AccountSaveCoordinator,
    ) -> Self {
        Self {
            accounts: MailboxAccountRepository::new(pool.clone()),
            pool,
            gateway,
            credentials,
            background_gate,
            export_preference_gate,
            operations,
            account_saves,
        }
    }

    pub async fn list_accounts(&self) -> Result<Vec<MailboxAccount>, AppError> {
        self.accounts.list().await
    }

    pub async fn save_account(&self, input: SaveAccountInput) -> Result<MailboxAccount, AppError> {
        self.account_saves.ensure_open()?;
        let is_update = input.id.is_some();
        let account_id = input.id.unwrap_or_else(Uuid::new_v4);
        if is_update {
            let _ = self.account_saves.reconcile_account(account_id).await?;
        }
        let operation_guard = self.operations.try_lock(account_id)?;
        if is_update {
            self.account_saves
                .ensure_no_blocking_pending(account_id)
                .await?;
        }
        let config = account_config(
            input.provider,
            &input.email,
            input.imap_host.as_deref(),
            input.imap_port,
        )?;
        let secret = self.resolve_secret(input.id, input.secret).await?;
        if let Err(error) = self.gateway.test_connection(&config, &secret).await {
            return Err(sanitize_error(error, &secret));
        }

        let metadata = NewMailboxAccount {
            provider: input.provider,
            email: input.email,
            imap_host: config.host,
            imap_port: i64::from(config.port),
            enabled: input.enabled,
            sync_interval_minutes: input.sync_interval_minutes,
        };
        self.account_saves
            .start_save(operation_guard, account_id, is_update, metadata, secret)
            .await
            .map_err(|_| AppError::Internal {
                message: "mailbox account save result was lost".to_owned(),
            })?
    }

    pub async fn test_account(&self, input: TestAccountInput) -> Result<(), AppError> {
        self.account_saves.ensure_open()?;
        if let Some(id) = input.id {
            let _ = self.account_saves.reconcile_account(id).await?;
        }
        let _operation_guard = input
            .id
            .map(|id| self.operations.try_lock(id))
            .transpose()?;
        if let Some(id) = input.id {
            self.account_saves.ensure_no_blocking_pending(id).await?;
        }
        let config = account_config(
            input.provider,
            &input.email,
            input.imap_host.as_deref(),
            input.imap_port,
        )?;
        let secret = self.resolve_secret(input.id, input.secret).await?;
        self.gateway
            .test_connection(&config, &secret)
            .await
            .map_err(|error| sanitize_error(error, &secret))
    }

    pub async fn delete_account(&self, id: Uuid) -> Result<(), AppError> {
        self.account_saves.ensure_open()?;
        let _ = self.account_saves.reconcile_account(id).await?;
        let _operation_guard = self.operations.try_lock(id)?;
        self.account_saves.ensure_no_blocking_pending(id).await?;
        self.accounts.get(id).await?;
        self.accounts.set_enabled(id, false).await?;
        delete_credential(self.credentials.clone(), id.to_string()).await?;
        self.accounts.delete(id).await
    }

    pub async fn preferences(&self) -> Result<Preferences, AppError> {
        load_preferences(&self.pool).await
    }

    pub async fn save_preferences(&self, input: PreferencesInput) -> Result<Preferences, AppError> {
        let _export_preference_guard = self.export_preference_gate.write().await;
        let preferences = Preferences {
            background_sync_enabled: input.background_sync_enabled,
            export_directory: input.export_directory.trim().to_owned(),
            batch_directory_pattern: input.batch_directory_pattern.trim().to_owned(),
        };
        validate_preferences(&preferences)?;
        let current = load_preferences(&self.pool).await?;
        if current.export_directory != preferences.export_directory {
            let pending_exports =
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
                    .fetch_one(&self.pool)
                    .await
                    .map_err(database_error)?;
            if pending_exports != 0 {
                return Err(AppError::Conflict {
                    message: "导出恢复完成前不能更改导出目录".to_owned(),
                });
            }
        }
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

    async fn resolve_secret(&self, id: Option<Uuid>, secret: String) -> Result<String, AppError> {
        if !secret.trim().is_empty() {
            return Ok(secret);
        }
        let Some(id) = id else {
            return Err(AppError::validation(
                "secret",
                "credential secret must not be blank for a new account",
            ));
        };
        get_credential(self.credentials.clone(), id.to_string())
            .await?
            .ok_or_else(missing_credential_error)
    }
}

fn missing_credential_error() -> AppError {
    AppError::External {
        service: "mailbox_credential".to_owned(),
        retryable: false,
        message: "mailbox credential is unavailable".to_owned(),
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
    if preferences.export_directory != "exports"
        && !std::path::Path::new(&preferences.export_directory).is_absolute()
    {
        return Err(AppError::validation(
            "export_directory",
            "export directory must be an absolute path or exports",
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
    sanitize_app_error(error, &[secret])
}

fn database_error(error: sqlx::Error) -> AppError {
    crate::db::accounts::map_database_error("failed to save mailbox settings", error)
}
