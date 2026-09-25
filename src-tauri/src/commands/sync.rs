use uuid::Uuid;

use crate::domain::error::AppError;
use crate::state::AppState;

pub async fn now(state: &AppState, account_id: Uuid) -> Result<(), AppError> {
    let scheduler = state.application_scheduler();
    state
        .run_tracked_operation(async move { scheduler.sync_now(account_id).await })
        .await
}

/// Progress of every sync running right now.
pub fn progress() -> Vec<crate::services::sync_progress::SyncProgressSnapshot> {
    crate::services::sync_progress::monitor().snapshot()
}

/// Ask a running sync to stop; returns whether one was running.
pub fn cancel(account_id: Uuid) -> bool {
    crate::services::sync_progress::monitor().cancel(account_id)
}

pub(crate) mod ipc {
    use tauri::State;
    use uuid::Uuid;

    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command(rename_all = "camelCase")]
    pub async fn sync_account_now(
        state: State<'_, AppState>,
        account_id: Uuid,
    ) -> Result<(), AppError> {
        super::now(&state, account_id).await
    }
    #[tauri::command]
    pub async fn get_sync_progress()
    -> Result<Vec<crate::services::sync_progress::SyncProgressSnapshot>, AppError> {
        Ok(super::progress())
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn cancel_sync(account_id: Uuid) -> Result<bool, AppError> {
        Ok(super::cancel(account_id))
    }
}
