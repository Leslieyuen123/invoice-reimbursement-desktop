use serde::Serialize;

use crate::commands::batches::BatchDto;
use crate::commands::settings::MailboxAccountDto;
use crate::domain::error::AppError;
use crate::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardDto {
    pub mailbox_accounts: Vec<MailboxAccountDto>,
    pub recently_added_count: u64,
    pub pending_confirmation_count: u64,
    pub recognition_failed_count: u64,
    pub suspected_duplicate_count: u64,
    pub recent_batches: Vec<BatchDto>,
}

pub async fn load(state: &AppState) -> Result<DashboardDto, AppError> {
    let snapshot = state.dashboard_service().load().await?;
    let mailbox_accounts = snapshot
        .mailbox_accounts
        .into_iter()
        .map(MailboxAccountDto::from)
        .collect();
    let recent_batches = snapshot
        .recent_batches
        .into_iter()
        .map(BatchDto::try_from)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(DashboardDto {
        mailbox_accounts,
        recently_added_count: snapshot.counts.recently_added,
        pending_confirmation_count: snapshot.counts.pending_confirmation,
        recognition_failed_count: snapshot.counts.recognition_failed,
        suspected_duplicate_count: snapshot.counts.suspected_duplicate,
        recent_batches,
    })
}

pub(crate) mod ipc {
    use tauri::State;

    use super::{DashboardDto, load};
    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command]
    pub async fn get_dashboard(state: State<'_, AppState>) -> Result<DashboardDto, AppError> {
        load(&state).await
    }
}
