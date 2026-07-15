use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::accounts::{MailboxAccount, MailboxProvider};
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
}

impl From<MailboxAccount> for MailboxAccountDto {
    fn from(account: MailboxAccount) -> Self {
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
        }
    }
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
    Ok(state
        .settings_service()
        .list_accounts()
        .await?
        .into_iter()
        .map(Into::into)
        .collect())
}

pub async fn save_account(
    state: &AppState,
    input: SaveMailboxAccountInput,
) -> Result<MailboxAccountDto, AppError> {
    state
        .settings_service()
        .save_account(input.into())
        .await
        .map(Into::into)
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
        .settings_service()
        .save_preferences(PreferencesInput {
            background_sync_enabled: input.background_sync_enabled,
            export_directory: input.export_directory,
            batch_directory_pattern: input.batch_directory_pattern,
        })
        .await
}

pub(crate) mod ipc {
    use tauri::State;
    use uuid::Uuid;

    use super::{
        MailboxAccountDto, Preferences, PreferencesInputDto, SaveMailboxAccountInput,
        TestMailboxAccountInput,
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
}
