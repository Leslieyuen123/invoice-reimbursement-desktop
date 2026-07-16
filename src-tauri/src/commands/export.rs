use serde::Serialize;
use uuid::Uuid;

use crate::domain::amount::validate_amount_cents;
use crate::domain::error::AppError;
use crate::services::export::ExportResult;
use crate::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResultDto {
    pub directory: String,
    pub item_count: usize,
    pub total_amount_cents: i64,
}

impl TryFrom<ExportResult> for ExportResultDto {
    type Error = AppError;

    fn try_from(result: ExportResult) -> Result<Self, Self::Error> {
        Ok(Self {
            directory: result.directory.to_string_lossy().into_owned(),
            item_count: result.item_count,
            total_amount_cents: validate_amount_cents(
                result.total_amount_cents,
                "totalAmountCents",
            )?,
        })
    }
}

pub async fn run(state: &AppState, batch_id: Uuid) -> Result<ExportResultDto, AppError> {
    let service = state.export_service();
    state
        .run_tracked_operation(async move { service.export(batch_id).await })
        .await?
        .try_into()
}

pub(crate) mod ipc {
    use tauri::State;
    use uuid::Uuid;

    use super::ExportResultDto;
    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command(rename_all = "camelCase")]
    pub async fn export_batch(
        state: State<'_, AppState>,
        batch_id: Uuid,
    ) -> Result<ExportResultDto, AppError> {
        super::run(&state, batch_id).await
    }
}
