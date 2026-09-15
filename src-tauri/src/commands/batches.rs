use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::commands::export::ExportResultDto;
use crate::commands::{CursorDto, PageDto, PageRequestDto, validated_page_size};
use crate::db::batches::{Batch, BatchPageCursor, BatchSummary};
use crate::domain::amount::validate_amount_cents;
use crate::domain::error::AppError;
use crate::domain::model::{BatchStatus, Category};
use crate::services::batch_automation::{AccountAutomationFailure, BatchAutomationResult};
use crate::services::batch_eligibility::{BatchBlocker, BatchIssue, batch_issues, export_blocker};
use crate::services::batch_repair::repair_missing_normalized_pdfs;
use crate::services::batches::{
    BatchDetail, BatchDetailSummary, BatchService, CategorySummary, NewBatchInput, SettleBatchInput,
};
use crate::services::recognition::original_can_be_normalized;
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
        let total_amount_cents =
            validate_amount_cents(summary.total_amount_cents, "totalAmountCents")?;
        Ok(Self {
            id: batch.id.to_string(),
            name: batch.name,
            start_date: batch.start_date.to_string(),
            end_date: batch.end_date.to_string(),
            status: batch.status,
            item_count: summary.item_count,
            total_amount_cents,
            unconfirmed_count: summary.unconfirmed_count,
            note: batch.note,
            created_at: batch.created_at.to_rfc3339(),
            updated_at: batch.updated_at.to_rfc3339(),
            last_exported_at: batch.last_exported_at.map(|value| value.to_rfc3339()),
        })
    }
}

impl TryFrom<BatchSummary> for BatchDto {
    type Error = AppError;

    fn try_from(batch: BatchSummary) -> Result<Self, Self::Error> {
        let total_amount_cents =
            validate_amount_cents(batch.total_amount_cents, "totalAmountCents")?;
        Ok(Self {
            id: batch.id.to_string(),
            name: batch.name,
            start_date: batch.start_date.to_string(),
            end_date: batch.end_date.to_string(),
            status: batch.status,
            item_count: u64::try_from(batch.item_count).map_err(|_| invalid_batch_summary())?,
            total_amount_cents,
            unconfirmed_count: u64::try_from(batch.unconfirmed_count)
                .map_err(|_| invalid_batch_summary())?,
            note: batch.note,
            created_at: batch.created_at.to_rfc3339(),
            updated_at: batch.updated_at.to_rfc3339(),
            last_exported_at: batch.last_exported_at.map(|value| value.to_rfc3339()),
        })
    }
}

fn invalid_batch_summary() -> AppError {
    AppError::Internal {
        message: "batch summary count was invalid".to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CategorySummaryDto {
    pub item_count: u64,
    pub amount_cents: i64,
}

impl TryFrom<CategorySummary> for CategorySummaryDto {
    type Error = AppError;

    fn try_from(summary: CategorySummary) -> Result<Self, Self::Error> {
        Ok(Self {
            item_count: summary.item_count,
            amount_cents: validate_amount_cents(summary.amount_cents, "amountCents")?,
        })
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

impl TryFrom<BatchDetailSummary> for BatchDetailSummaryDto {
    type Error = AppError;

    fn try_from(summary: BatchDetailSummary) -> Result<Self, Self::Error> {
        Ok(Self {
            item_count: summary.item_count,
            total_amount_cents: validate_amount_cents(
                summary.total_amount_cents,
                "totalAmountCents",
            )?,
            transport: summary.transport.try_into()?,
            dining: summary.dining.try_into()?,
            accommodation: summary.accommodation.try_into()?,
            hospitality: summary.hospitality.try_into()?,
            unconfirmed_count: summary.unconfirmed_count,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchDetailDto {
    pub batch: BatchDto,
    pub items: Vec<crate::commands::items::InvoiceItemDto>,
    pub summary: BatchDetailSummaryDto,
    pub warnings: Vec<String>,
    /// Batch members that currently block the export, with a repair hint.
    pub issues: Vec<BatchIssueDto>,
}

/// One batch member that blocks the export of the whole batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchIssueDto {
    pub item_id: String,
    pub file_name: String,
    pub code: String,
    pub message: String,
    pub repairable: bool,
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
            items: items
                .into_iter()
                .map(crate::commands::items::InvoiceItemDto::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            summary: summary.try_into()?,
            warnings,
            issues: Vec::new(),
        })
    }
}

/// Builds the batch detail DTO including the export blockers of its members.
fn detail_dto(state: &AppState, detail: BatchDetail) -> Result<BatchDetailDto, AppError> {
    let issues = batch_issue_dtos(batch_issues(state.paths(), &detail.items));
    let mut dto = BatchDetailDto::try_from(detail)?;
    dto.issues = issues;
    Ok(dto)
}

fn batch_issue_dtos(issues: Vec<BatchIssue>) -> Vec<BatchIssueDto> {
    issues
        .into_iter()
        .map(|issue| {
            let repairable =
                issue.blocker.is_repairable() && original_can_be_normalized(&issue.file_name);
            BatchIssueDto {
                item_id: issue.item_id.to_string(),
                file_name: issue.file_name,
                code: issue.blocker.code().to_owned(),
                message: issue.blocker.message().to_owned(),
                repairable,
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewBatchInputDto {
    pub name: String,
    pub start_date: String,
    pub end_date: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchCandidateDisabledReason {
    RecognitionFailed,
    SuspectedDuplicate,
    NotRecognized,
    NotConfirmed,
    MissingNormalizedPdf,
    MissingOriginalFile,
    IncompleteDetails,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchCandidateDto {
    pub item: crate::commands::items::InvoiceItemDto,
    pub outside_batch_range: bool,
    pub eligible: bool,
    pub disabled_reason: Option<BatchCandidateDisabledReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountAutomationFailureDto {
    pub account_id: String,
    pub email: String,
    pub message: String,
}

impl From<AccountAutomationFailure> for AccountAutomationFailureDto {
    fn from(failure: AccountAutomationFailure) -> Self {
        Self {
            account_id: failure.account_id.to_string(),
            email: failure.email,
            message: failure.message,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchAutomationResultDto {
    pub scanned_account_count: u32,
    pub failed_accounts: Vec<AccountAutomationFailureDto>,
    pub imported_count: u32,
    pub assigned_count: u32,
    pub exception_count: u32,
    pub repaired_count: u32,
    pub export: Option<ExportResultDto>,
}

impl TryFrom<BatchAutomationResult> for BatchAutomationResultDto {
    type Error = AppError;

    fn try_from(result: BatchAutomationResult) -> Result<Self, Self::Error> {
        Ok(Self {
            scanned_account_count: result.scanned_account_count,
            failed_accounts: result
                .failed_accounts
                .into_iter()
                .map(AccountAutomationFailureDto::from)
                .collect(),
            imported_count: result.imported_count,
            assigned_count: result.assigned_count,
            exception_count: result.exception_count,
            repaired_count: result.repaired_count,
            export: result.export.map(ExportResultDto::try_from).transpose()?,
        })
    }
}

pub async fn list(state: &AppState) -> Result<Vec<BatchDto>, AppError> {
    Ok(list_page(state, None).await?.items)
}

pub async fn list_page(
    state: &AppState,
    page: Option<PageRequestDto>,
) -> Result<PageDto<BatchDto>, AppError> {
    let page_size = validated_page_size(page.as_ref())?;
    let cursor = page
        .as_ref()
        .and_then(|page| page.cursor.as_ref())
        .map(parse_batch_cursor)
        .transpose()?;
    let page = BatchService::new(state.pool().clone())
        .list_page(cursor, page_size)
        .await?;
    Ok(PageDto {
        items: page
            .batches
            .into_iter()
            .map(BatchDto::try_from)
            .collect::<Result<Vec<_>, _>>()?,
        next_cursor: page.next_cursor.map(|cursor| CursorDto {
            sort_value: cursor.updated_at.to_rfc3339(),
            id: cursor.id.to_string(),
        }),
    })
}

pub async fn list_candidates(
    state: &AppState,
    batch_id: Uuid,
    query: Option<String>,
    page: Option<PageRequestDto>,
) -> Result<PageDto<BatchCandidateDto>, AppError> {
    let page_size = validated_page_size(page.as_ref())?;
    let cursor = page
        .as_ref()
        .and_then(|page| page.cursor.as_ref())
        .map(crate::commands::items::parse_item_cursor)
        .transpose()?;
    let page = BatchService::new(state.pool().clone())
        .list_candidates(batch_id, query, cursor, page_size)
        .await?;
    let batch = page.batch;
    let items = page
        .items
        .into_iter()
        .map(|item| {
            let outside_batch_range = item
                .batch_membership_date()
                .is_none_or(|date| date < batch.start_date || date > batch.end_date);
            // Candidate eligibility must match what the export accepts, so a
            // batch can never be made unexportable from the batch detail page.
            let disabled_reason =
                export_blocker(state.paths(), &item).map(|blocker| match blocker {
                    BatchBlocker::SuspectedDuplicate => {
                        BatchCandidateDisabledReason::SuspectedDuplicate
                    }
                    BatchBlocker::RecognitionFailed => {
                        BatchCandidateDisabledReason::RecognitionFailed
                    }
                    BatchBlocker::NotRecognized => BatchCandidateDisabledReason::NotRecognized,
                    BatchBlocker::NotConfirmed => BatchCandidateDisabledReason::NotConfirmed,
                    BatchBlocker::MissingNormalizedPdf => {
                        BatchCandidateDisabledReason::MissingNormalizedPdf
                    }
                    BatchBlocker::MissingOriginalFile => {
                        BatchCandidateDisabledReason::MissingOriginalFile
                    }
                    BatchBlocker::MissingCategory
                    | BatchBlocker::MissingPeriod
                    | BatchBlocker::MissingAmount
                    | BatchBlocker::UnsupportedCurrency => {
                        BatchCandidateDisabledReason::IncompleteDetails
                    }
                });
            Ok(BatchCandidateDto {
                item: crate::commands::items::InvoiceItemDto::try_from(item)?,
                outside_batch_range,
                eligible: disabled_reason.is_none(),
                disabled_reason,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    Ok(PageDto {
        items,
        next_cursor: page.next_cursor.map(|cursor| CursorDto {
            sort_value: cursor.created_at.to_rfc3339(),
            id: cursor.id.to_string(),
        }),
    })
}

fn parse_batch_cursor(cursor: &CursorDto) -> Result<BatchPageCursor, AppError> {
    Ok(BatchPageCursor {
        updated_at: DateTime::parse_from_rfc3339(&cursor.sort_value)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|_| AppError::validation("cursor", "invalid batch cursor"))?,
        id: Uuid::parse_str(&cursor.id)
            .map_err(|_| AppError::validation("cursor", "invalid batch cursor"))?,
    })
}

pub async fn get(state: &AppState, id: Uuid) -> Result<BatchDetailDto, AppError> {
    let detail = BatchService::new(state.pool().clone()).get(id).await?;
    detail_dto(state, detail)
}

/// Regenerates missing normalized PDFs for the batch members that can still be
/// normalized, so the export is not blocked by an invoice nothing could repair.
pub async fn repair_normalized_pdfs(
    state: &AppState,
    batch_id: Uuid,
) -> Result<BatchRepairDto, AppError> {
    let detail = BatchService::new(state.pool().clone())
        .get(batch_id)
        .await?;
    let repaired_count =
        repair_missing_normalized_pdfs(state.paths(), &state.recognition_service(), &detail.items)
            .await;
    let refreshed = BatchService::new(state.pool().clone())
        .get(batch_id)
        .await?;
    Ok(BatchRepairDto {
        repaired_count,
        issues: batch_issue_dtos(batch_issues(state.paths(), &refreshed.items)),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRepairDto {
    pub repaired_count: u32,
    pub issues: Vec<BatchIssueDto>,
}

/// Options for the bulk confirm action on the batch detail page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettleBatchInputDto {
    #[serde(default)]
    pub fill_invoice_date_from_received: bool,
    #[serde(default)]
    pub apply_suggested_category: bool,
    #[serde(default)]
    pub default_category: Option<Category>,
}

/// One invoice the bulk confirm could not settle, with a stable reason code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedBatchItemDto {
    pub item_id: String,
    pub file_name: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettleBatchOutcomeDto {
    pub confirmed_count: u32,
    pub filled_invoice_date_count: u32,
    pub applied_category_count: u32,
    pub repaired_count: u32,
    pub skipped: Vec<SkippedBatchItemDto>,
    pub issues: Vec<BatchIssueDto>,
}

fn skipped_message(code: &str) -> &'static str {
    match code {
        "suspected_duplicate" => "疑似重复，需先处理重复",
        "recognition_failed" => "识别失败，需重新识别或更换原件",
        "not_recognized" => "尚未完成识别",
        "missing_amount" => "缺少金额，必须对照原件人工填写",
        "missing_invoice_date" => "缺少开票日期，且无法从邮件日期推导",
        "missing_category" => "缺少分类，且没有可采用的建议分类",
        "unsupported_format" => "原件格式不支持生成归一化 PDF，请改用 PDF/JPG/PNG 或移出批次",
        "outside_date_range" => "票据日期不在批次范围内",
        _ => "未满足批量确认条件",
    }
}

/// Confirms every batch member whose values can be completed from its own data,
/// then repairs the normalized PDFs that were still missing.
pub async fn settle_batch_items(
    state: &AppState,
    batch_id: Uuid,
    input: SettleBatchInputDto,
) -> Result<SettleBatchOutcomeDto, AppError> {
    let service = BatchService::new(state.pool().clone());
    let outcome = service
        .settle_items(
            batch_id,
            SettleBatchInput {
                fill_invoice_date_from_received: input.fill_invoice_date_from_received,
                apply_suggested_category: input.apply_suggested_category,
                default_category: input.default_category,
            },
        )
        .await?;
    let detail = service.get(batch_id).await?;
    let repaired_count =
        repair_missing_normalized_pdfs(state.paths(), &state.recognition_service(), &detail.items)
            .await;
    let refreshed = if repaired_count == 0 {
        detail
    } else {
        service.get(batch_id).await?
    };
    Ok(SettleBatchOutcomeDto {
        confirmed_count: outcome.confirmed_count,
        filled_invoice_date_count: outcome.filled_invoice_date_count,
        applied_category_count: outcome.applied_category_count,
        repaired_count,
        skipped: outcome
            .skipped
            .into_iter()
            .map(|skipped| SkippedBatchItemDto {
                item_id: skipped.item_id.to_string(),
                file_name: skipped.file_name,
                message: skipped_message(&skipped.code).to_owned(),
                code: skipped.code,
            })
            .collect(),
        issues: batch_issue_dtos(batch_issues(state.paths(), &refreshed.items)),
    })
}

/// Removes several invoices from a batch; ids outside the batch are ignored.
pub async fn remove_batch_items(
    state: &AppState,
    batch_id: Uuid,
    item_ids: Vec<Uuid>,
) -> Result<BatchDetailDto, AppError> {
    let (detail, _removed) = BatchService::new(state.pool().clone())
        .remove_items(batch_id, &item_ids)
        .await?;
    detail_dto(state, detail)
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
    let detail = BatchService::new(state.pool().clone())
        .assign_items(batch_id, &item_ids)
        .await?;
    detail_dto(state, detail)
}

pub async fn remove(
    state: &AppState,
    batch_id: Uuid,
    item_id: Uuid,
) -> Result<BatchDetailDto, AppError> {
    let detail = BatchService::new(state.pool().clone())
        .remove_item(batch_id, item_id)
        .await?;
    detail_dto(state, detail)
}

pub async fn run_automation(
    state: &AppState,
    batch_id: Uuid,
) -> Result<BatchAutomationResultDto, AppError> {
    let service = state.batch_automation_service();
    state
        .run_tracked_operation(async move { service.run(batch_id).await })
        .await?
        .try_into()
}

pub(crate) mod ipc {
    use tauri::State;
    use uuid::Uuid;

    use super::{
        BatchAutomationResultDto, BatchCandidateDto, BatchDetailDto, BatchDto, BatchRepairDto,
        NewBatchInputDto, SettleBatchInputDto, SettleBatchOutcomeDto,
    };
    use crate::commands::{PageDto, PageRequestDto};
    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command]
    pub async fn list_batches(
        state: State<'_, AppState>,
        page: Option<PageRequestDto>,
    ) -> Result<PageDto<BatchDto>, AppError> {
        super::list_page(&state, page).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn get_batch(
        state: State<'_, AppState>,
        batch_id: Uuid,
    ) -> Result<BatchDetailDto, AppError> {
        super::get(&state, batch_id).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn settle_batch_items(
        state: State<'_, AppState>,
        batch_id: Uuid,
        input: SettleBatchInputDto,
    ) -> Result<SettleBatchOutcomeDto, AppError> {
        super::settle_batch_items(&state, batch_id, input).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn remove_batch_items(
        state: State<'_, AppState>,
        batch_id: Uuid,
        item_ids: Vec<Uuid>,
    ) -> Result<BatchDetailDto, AppError> {
        super::remove_batch_items(&state, batch_id, item_ids).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn repair_batch_normalized_pdfs(
        state: State<'_, AppState>,
        batch_id: Uuid,
    ) -> Result<BatchRepairDto, AppError> {
        super::repair_normalized_pdfs(&state, batch_id).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn list_batch_candidates(
        state: State<'_, AppState>,
        batch_id: Uuid,
        query: Option<String>,
        page: Option<PageRequestDto>,
    ) -> Result<PageDto<BatchCandidateDto>, AppError> {
        super::list_candidates(&state, batch_id, query, page).await
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

    #[tauri::command(rename_all = "camelCase")]
    pub async fn run_batch_automation(
        state: State<'_, AppState>,
        batch_id: Uuid,
    ) -> Result<BatchAutomationResultDto, AppError> {
        super::run_automation(&state, batch_id).await
    }
}
