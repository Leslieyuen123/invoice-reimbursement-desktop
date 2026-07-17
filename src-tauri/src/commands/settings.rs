use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::accounts::{
    MailboxAccount, MailboxAccountRepository, MailboxProvider, SyncRetryState,
};
use crate::domain::error::AppError;
use crate::services::settings::{
    Preferences, PreferencesInput, SaveAccountInput, TestAccountInput,
};
use crate::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxAccountDto {
    pub id: String,
    pub provider: MailboxProvider,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub enabled: bool,
    pub sync_interval_minutes: i64,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub last_error_kind: Option<MailboxErrorKind>,
    pub last_error_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxErrorKind {
    Authentication,
    Network,
    Unknown,
}

impl MailboxAccountDto {
    fn from_account(account: MailboxAccount, retry: Option<SyncRetryState>) -> Self {
        let last_error_kind = account.last_error.as_ref().map(|_| match retry {
            Some(retry) if retry.suspended => MailboxErrorKind::Authentication,
            Some(_) => MailboxErrorKind::Network,
            None => MailboxErrorKind::Unknown,
        });
        let last_error_at = account
            .last_error
            .as_ref()
            .map(|_| account.updated_at.to_rfc3339());
        Self {
            id: account.id.to_string(),
            provider: account.provider,
            email: account.email,
            imap_host: account.imap_host,
            imap_port: account.imap_port,
            enabled: account.enabled,
            sync_interval_minutes: account.sync_interval_minutes,
            last_synced_at: account.last_synced_at.map(|value| value.to_rfc3339()),
            last_error: account.last_error,
            last_error_kind,
            last_error_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatusDto {
    pub local_data_directory: String,
    pub export_directory: String,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SaveMailboxAccountInput {
    pub id: Option<Uuid>,
    pub provider: MailboxProvider,
    pub email: String,
    pub secret: String,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub enabled: bool,
    pub sync_interval_minutes: i64,
}

impl From<SaveMailboxAccountInput> for SaveAccountInput {
    fn from(input: SaveMailboxAccountInput) -> Self {
        Self {
            id: input.id,
            provider: input.provider,
            email: input.email,
            secret: input.secret,
            imap_host: input.imap_host,
            imap_port: input.imap_port,
            enabled: input.enabled,
            sync_interval_minutes: input.sync_interval_minutes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestMailboxAccountInput {
    pub id: Option<Uuid>,
    pub provider: MailboxProvider,
    pub email: String,
    pub secret: String,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
}

impl From<SaveMailboxAccountInput> for TestMailboxAccountInput {
    fn from(input: SaveMailboxAccountInput) -> Self {
        Self {
            id: input.id,
            provider: input.provider,
            email: input.email,
            secret: input.secret,
            imap_host: input.imap_host,
            imap_port: input.imap_port,
        }
    }
}

impl From<TestMailboxAccountInput> for TestAccountInput {
    fn from(input: TestMailboxAccountInput) -> Self {
        Self {
            id: input.id,
            provider: input.provider,
            email: input.email,
            secret: input.secret,
            imap_host: input.imap_host,
            imap_port: input.imap_port,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreferencesInputDto {
    pub background_sync_enabled: bool,
    pub export_directory: String,
    pub batch_directory_pattern: String,
}

pub async fn list_accounts(state: &AppState) -> Result<Vec<MailboxAccountDto>, AppError> {
    account_dtos(state, state.settings_service().list_accounts().await?).await
}

pub async fn account_dtos(
    state: &AppState,
    accounts: Vec<MailboxAccount>,
) -> Result<Vec<MailboxAccountDto>, AppError> {
    let repository = MailboxAccountRepository::new(state.pool().clone());
    let mut dtos = Vec::with_capacity(accounts.len());
    for account in accounts {
        let retry = repository.get_retry_state(account.id).await?;
        dtos.push(MailboxAccountDto::from_account(account, retry));
    }
    Ok(dtos)
}

pub async fn save_account(
    state: &AppState,
    input: SaveMailboxAccountInput,
) -> Result<MailboxAccountDto, AppError> {
    let account = state.settings_service().save_account(input.into()).await?;
    let retry = MailboxAccountRepository::new(state.pool().clone())
        .get_retry_state(account.id)
        .await?;
    Ok(MailboxAccountDto::from_account(account, retry))
}

pub async fn test_account(
    state: &AppState,
    input: TestMailboxAccountInput,
) -> Result<(), AppError> {
    state.settings_service().test_account(input.into()).await
}

pub async fn delete_account(state: &AppState, id: Uuid) -> Result<(), AppError> {
    state.settings_service().delete_account(id).await
}

pub async fn get_preferences(state: &AppState) -> Result<Preferences, AppError> {
    state.settings_service().preferences().await
}

pub async fn save_preferences(
    state: &AppState,
    input: PreferencesInputDto,
) -> Result<Preferences, AppError> {
    state
        .paths()
        .for_export_directory(input.export_directory.trim())?;
    state
        .settings_service()
        .save_preferences(PreferencesInput {
            background_sync_enabled: input.background_sync_enabled,
            export_directory: input.export_directory,
            batch_directory_pattern: input.batch_directory_pattern,
        })
        .await
}

pub async fn storage_status(state: &AppState) -> Result<StorageStatusDto, AppError> {
    let preferences = state.settings_service().preferences().await?;
    let effective = state
        .paths()
        .for_export_directory(&preferences.export_directory)?;
    let local_data_directory = state
        .paths()
        .root
        .parent()
        .unwrap_or(&state.paths().root)
        .to_string_lossy()
        .into_owned();
    let available_bytes = available_bytes(&effective.exports)?;
    Ok(StorageStatusDto {
        local_data_directory,
        export_directory: effective.exports.to_string_lossy().into_owned(),
        available_bytes,
    })
}

#[cfg(unix)]
fn available_bytes(path: &std::path::Path) -> Result<u64, AppError> {
    rustix::fs::statvfs(path)
        .map(|status| status.f_bavail.saturating_mul(status.f_frsize))
        .map_err(|_| AppError::Internal {
            message: "failed to inspect available export storage".to_owned(),
        })
}

#[cfg(not(unix))]
fn available_bytes(_path: &std::path::Path) -> Result<u64, AppError> {
    Ok(0)
}

pub(crate) mod ipc {
    use tauri::State;
    use uuid::Uuid;

    use super::{
        MailboxAccountDto, Preferences, PreferencesInputDto, SaveMailboxAccountInput,
        StorageStatusDto, TestMailboxAccountInput,
    };
    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command]
    pub async fn list_mailbox_accounts(
        state: State<'_, AppState>,
    ) -> Result<Vec<MailboxAccountDto>, AppError> {
        super::list_accounts(&state).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn save_mailbox_account(
        state: State<'_, AppState>,
        input: SaveMailboxAccountInput,
    ) -> Result<MailboxAccountDto, AppError> {
        super::save_account(&state, input).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn test_mailbox_account(
        state: State<'_, AppState>,
        input: TestMailboxAccountInput,
    ) -> Result<(), AppError> {
        super::test_account(&state, input).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn delete_mailbox_account(
        state: State<'_, AppState>,
        account_id: Uuid,
    ) -> Result<(), AppError> {
        super::delete_account(&state, account_id).await
    }

    #[tauri::command]
    pub async fn get_preferences(state: State<'_, AppState>) -> Result<Preferences, AppError> {
        super::get_preferences(&state).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn save_preferences(
        state: State<'_, AppState>,
        input: PreferencesInputDto,
    ) -> Result<Preferences, AppError> {
        super::save_preferences(&state, input).await
    }

    #[tauri::command]
    pub async fn get_storage_status(
        state: State<'_, AppState>,
    ) -> Result<StorageStatusDto, AppError> {
        super::storage_status(&state).await
    }
}
