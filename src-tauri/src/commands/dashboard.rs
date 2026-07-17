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
    let mailbox_accounts = crate::commands::settings::account_dtos(snapshot.mailbox_accounts);
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

pub async fn load_and_publish(
    state: &AppState,
    publish_pending_count: impl FnOnce(u64),
) -> Result<DashboardDto, AppError> {
    let dashboard = load(state).await?;
    publish_pending_count(dashboard.pending_confirmation_count);
    Ok(dashboard)
}

pub(crate) mod ipc {
    use tauri::{AppHandle, Runtime, State};

    use super::{DashboardDto, load_and_publish};
    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command]
    pub async fn get_dashboard<R: Runtime>(
        app: AppHandle<R>,
        state: State<'_, AppState>,
    ) -> Result<DashboardDto, AppError> {
        load_and_publish(&state, |pending_count| {
            crate::refresh_tray_tooltip(&app, pending_count);
        })
        .await
    }
}
