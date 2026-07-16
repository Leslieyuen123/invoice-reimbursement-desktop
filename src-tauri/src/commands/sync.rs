use uuid::Uuid;

use crate::domain::error::AppError;
use crate::state::AppState;

pub async fn now(state: &AppState, account_id: Uuid) -> Result<(), AppError> {
    let scheduler = state.application_scheduler();
    state
        .run_tracked_operation(async move { scheduler.sync_now(account_id).await })
        .await
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
}
