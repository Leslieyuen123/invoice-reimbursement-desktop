use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::batches::Batch;
use crate::domain::error::AppError;
use crate::domain::model::BatchStatus;
use crate::services::batches::{
    BatchDetail, BatchDetailSummary, BatchService, CategorySummary, NewBatchInput,
};
use crate::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchDto {
    pub id: String,
    pub name: String,
    pub start_date: String,
    pub end_date: String,
    pub status: BatchStatus,
    pub item_count: u64,
    pub total_amount_cents: i64,
    pub unconfirmed_count: u64,
    pub note: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_exported_at: Option<String>,
}

impl TryFrom<(Batch, BatchDetailSummary)> for BatchDto {
    type Error = AppError;

    fn try_from((batch, summary): (Batch, BatchDetailSummary)) -> Result<Self, Self::Error> {
        Ok(Self {
            id: batch.id.to_string(),
            name: batch.name,
            start_date: batch.start_date.to_string(),
            end_date: batch.end_date.to_string(),
            status: batch.status,
            item_count: summary.item_count,
            total_amount_cents: summary.total_amount_cents,
            unconfirmed_count: summary.unconfirmed_count,
            note: batch.note,
            created_at: batch.created_at.to_rfc3339(),
            updated_at: batch.updated_at.to_rfc3339(),
            last_exported_at: batch.last_exported_at.map(|value| value.to_rfc3339()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CategorySummaryDto {
    pub item_count: u64,
    pub amount_cents: i64,
}

impl From<CategorySummary> for CategorySummaryDto {
    fn from(summary: CategorySummary) -> Self {
        Self {
            item_count: summary.item_count,
            amount_cents: summary.amount_cents,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchDetailSummaryDto {
    pub item_count: u64,
    pub total_amount_cents: i64,
    pub transport: CategorySummaryDto,
    pub dining: CategorySummaryDto,
    pub accommodation: CategorySummaryDto,
    pub hospitality: CategorySummaryDto,
    pub unconfirmed_count: u64,
}

impl From<BatchDetailSummary> for BatchDetailSummaryDto {
    fn from(summary: BatchDetailSummary) -> Self {
        Self {
            item_count: summary.item_count,
            total_amount_cents: summary.total_amount_cents,
            transport: summary.transport.into(),
            dining: summary.dining.into(),
            accommodation: summary.accommodation.into(),
            hospitality: summary.hospitality.into(),
            unconfirmed_count: summary.unconfirmed_count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchDetailDto {
    pub batch: BatchDto,
    pub items: Vec<crate::commands::items::InvoiceItemDto>,
    pub summary: BatchDetailSummaryDto,
    pub warnings: Vec<String>,
}

impl TryFrom<BatchDetail> for BatchDetailDto {
    type Error = AppError;

    fn try_from(detail: BatchDetail) -> Result<Self, Self::Error> {
        let BatchDetail {
            batch,
            items,
            summary,
            warnings,
        } = detail;
        Ok(Self {
            batch: BatchDto::try_from((batch, summary.clone()))?,
            items: items.into_iter().map(Into::into).collect(),
            summary: summary.into(),
            warnings,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewBatchInputDto {
    pub name: String,
    pub start_date: String,
    pub end_date: String,
    pub note: Option<String>,
}

pub async fn list(state: &AppState) -> Result<Vec<BatchDto>, AppError> {
    BatchService::new(state.pool().clone())
        .list()
        .await?
        .into_iter()
        .map(|detail| BatchDto::try_from((detail.batch, detail.summary)))
        .collect()
}

pub async fn get(state: &AppState, id: Uuid) -> Result<BatchDetailDto, AppError> {
    BatchService::new(state.pool().clone())
        .get(id)
        .await?
        .try_into()
}

pub async fn create_month(state: &AppState, year: i32, month: u32) -> Result<BatchDto, AppError> {
    let service = BatchService::new(state.pool().clone());
    let batch = service.create_month(year, month).await?;
    let detail = service.get(batch.id).await?;
    BatchDto::try_from((detail.batch, detail.summary))
}

pub async fn create_custom(
    state: &AppState,
    input: NewBatchInputDto,
) -> Result<BatchDto, AppError> {
    let service = BatchService::new(state.pool().clone());
    let batch = service
        .create(NewBatchInput {
            name: input.name,
            start_date: input.start_date,
            end_date: input.end_date,
            note: input.note,
        })
        .await?;
    let detail = service.get(batch.id).await?;
    BatchDto::try_from((detail.batch, detail.summary))
}

pub async fn assign(
    state: &AppState,
    batch_id: Uuid,
    item_ids: Vec<Uuid>,
) -> Result<BatchDetailDto, AppError> {
    BatchService::new(state.pool().clone())
        .assign_items(batch_id, &item_ids)
        .await?
        .try_into()
}

pub async fn remove(
    state: &AppState,
    batch_id: Uuid,
    item_id: Uuid,
) -> Result<BatchDetailDto, AppError> {
    BatchService::new(state.pool().clone())
        .remove_item(batch_id, item_id)
        .await?
        .try_into()
}

pub(crate) mod ipc {
    use tauri::State;
    use uuid::Uuid;

    use super::{BatchDetailDto, BatchDto, NewBatchInputDto};
    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command]
    pub async fn list_batches(state: State<'_, AppState>) -> Result<Vec<BatchDto>, AppError> {
        super::list(&state).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn get_batch(
        state: State<'_, AppState>,
        batch_id: Uuid,
    ) -> Result<BatchDetailDto, AppError> {
        super::get(&state, batch_id).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn create_month_batch(
        state: State<'_, AppState>,
        year: i32,
        month: u32,
    ) -> Result<BatchDto, AppError> {
        super::create_month(&state, year, month).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn create_custom_batch(
        state: State<'_, AppState>,
        input: NewBatchInputDto,
    ) -> Result<BatchDto, AppError> {
        super::create_custom(&state, input).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn assign_items_to_batch(
        state: State<'_, AppState>,
        batch_id: Uuid,
        item_ids: Vec<Uuid>,
    ) -> Result<BatchDetailDto, AppError> {
        super::assign(&state, batch_id, item_ids).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn remove_item_from_batch(
        state: State<'_, AppState>,
        batch_id: Uuid,
        item_id: Uuid,
    ) -> Result<BatchDetailDto, AppError> {
        super::remove(&state, batch_id, item_id).await
    }
}
