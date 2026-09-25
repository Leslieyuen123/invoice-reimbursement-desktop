use crate::domain::error::AppError;
use crate::services::consistency::{ConsistencyReport, audit};
use crate::state::AppState;

pub async fn get_report(state: &AppState) -> Result<ConsistencyReport, AppError> {
    audit(state.pool(), state.paths()).await
}

pub(crate) mod ipc {
    use tauri::State;

    use super::ConsistencyReport;
    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command]
    pub async fn get_consistency_report(
        state: State<'_, AppState>,
    ) -> Result<ConsistencyReport, AppError> {
        super::get_report(&state).await
    }
}
