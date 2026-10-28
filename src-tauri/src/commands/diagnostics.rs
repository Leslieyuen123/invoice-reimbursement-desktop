use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::error::AppError;
use crate::services::diagnostics::write_bundle;
use crate::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportDiagnosticsInput {
    /// Directory the archive is written to; the export directory by default.
    pub destination: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsBundleDto {
    pub path: String,
    pub directory: String,
    pub bytes: u64,
}

pub async fn export(
    state: &AppState,
    input: Option<ExportDiagnosticsInput>,
) -> Result<DiagnosticsBundleDto, AppError> {
    let destination = input
        .and_then(|input| input.destination)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    if let Some(destination) = destination.as_ref()
        && !destination.is_absolute()
    {
        return Err(AppError::validation(
            "destination",
            "诊断包目录必须是绝对路径",
        ));
    }
    let outcome = write_bundle(state.pool(), state.paths(), destination).await?;
    let directory = outcome
        .path
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(DiagnosticsBundleDto {
        path: outcome.path.to_string_lossy().into_owned(),
        directory,
        bytes: outcome.bytes,
    })
}

pub(crate) mod ipc {
    use tauri::State;

    use super::{DiagnosticsBundleDto, ExportDiagnosticsInput};
    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command(rename_all = "camelCase")]
    pub async fn export_diagnostics(
        state: State<'_, AppState>,
        input: Option<ExportDiagnosticsInput>,
    ) -> Result<DiagnosticsBundleDto, AppError> {
        super::export(&state, input).await
    }
}
