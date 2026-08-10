use std::collections::HashSet;

use chrono::{NaiveDate, Utc};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::db::batches::{Batch, BatchPage, BatchPageCursor, BatchRepository, BatchSummary};
use crate::db::items::{InvoiceItem, ItemPageCursor, ItemRepository};
use crate::domain::amount::checked_add_amount_cents;
use crate::domain::error::AppError;
use crate::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, NewBatch, RecognitionStatus,
};
use crate::infra::files::AppPaths;
use crate::services::batch_eligibility::is_safe_batch_candidate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBatchInput {
    pub name: String,
    pub start_date: String,
    pub end_date: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CategorySummary {
    pub item_count: u64,
    pub amount_cents: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchDetailSummary {
    pub item_count: u64,
    pub total_amount_cents: i64,
    pub transport: CategorySummary,
    pub dining: CategorySummary,
    pub accommodation: CategorySummary,
    pub hospitality: CategorySummary,
    pub unconfirmed_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchDetail {
    pub batch: Batch,
    pub items: Vec<InvoiceItem>,
    pub summary: BatchDetailSummary,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchCandidatePage {
    pub batch: Batch,
    pub items: Vec<InvoiceItem>,
    pub next_cursor: Option<ItemPageCursor>,
}

#[derive(Clone)]
pub struct BatchService {
    pool: SqlitePool,
}

impl BatchService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list_page(
        &self,
        cursor: Option<BatchPageCursor>,
        page_size: usize,
    ) -> Result<BatchPage, AppError> {
        BatchRepository::new(self.pool.clone())
            .list_page(cursor, page_size)
            .await
    }

    pub async fn recent_for_dashboard(&self) -> Result<Vec<BatchSummary>, AppError> {
        BatchRepository::new(self.pool.clone())
            .recent_for_dashboard()
            .await
    }

    pub async fn create_month(&self, year: i32, month: u32) -> Result<Batch, AppError> {
        if !(1..=9999).contains(&year) {
            return Err(AppError::validation(
                "year",
                "year must be between 1 and 9999",
            ));
        }
        if !(1..=12).contains(&month) {
            return Err(AppError::validation(
                "month",
                "month must be between 1 and 12",
            ));
        }

        let start = NaiveDate::from_ymd_opt(year, month, 1)
            .ok_or_else(|| AppError::validation("year", "year must be between 1 and 9999"))?;
        let end = if month == 12 {
            NaiveDate::from_ymd_opt(year, 12, 31)
        } else {
            NaiveDate::from_ymd_opt(year, month + 1, 1).and_then(|date| date.pred_opt())
        }
        .ok_or_else(|| AppError::validation("month", "month does not form a valid date"))?;

        self.create(NewBatchInput {
            name: format!("{year} 年 {month} 月报销"),
            start_date: start.to_string(),
            end_date: end.to_string(),
            note: None,
        })
        .await
    }

    pub async fn create(&self, input: NewBatchInput) -> Result<Batch, AppError> {
        let batch = NewBatch::try_new(
            input.name,
            input.start_date,
            input.end_date,
            normalize_optional_text(input.note),
        )?;
        BatchRepository::new(self.pool.clone()).create(batch).await
    }

    pub async fn update(
        &self,
        batch_id: Uuid,
        input: NewBatchInput,
    ) -> Result<BatchDetail, AppError> {
        let batch = NewBatch::try_new(
            input.name,
            input.start_date,
            input.end_date,
            normalize_optional_text(input.note),
        )?;
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| database_error("failed to begin batch update", error))?;
        let result = async {
            BatchRepository::update_with_connection(&mut transaction, batch_id, &batch).await?;
            Self::detail_with_connection(&mut transaction, batch_id).await
        }
        .await;
        finish_detail_transaction(transaction, result).await
    }

    pub async fn recommend(
        &self,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<InvoiceItem>, AppError> {
        let start_date = parse_date(start_date, "startDate")?;
        let end_date = parse_date(end_date, "endDate")?;
        if start_date > end_date {
            return Err(AppError::validation(
                "dateRange",
                "start date must not be after end date",
            ));
        }

        ItemRepository::new(self.pool.clone())
            .recommend_unassigned(
                &start_date.format("%Y-%m").to_string(),
                &end_date.format("%Y-%m").to_string(),
            )
            .await
    }

    pub async fn list_candidates(
        &self,
        batch_id: Uuid,
        query: Option<String>,
        cursor: Option<ItemPageCursor>,
        page_size: usize,
    ) -> Result<BatchCandidatePage, AppError> {
        let batch = BatchRepository::new(self.pool.clone())
            .get(batch_id)
            .await?;
        let query = normalize_candidate_query(query)?;
        let page = ItemRepository::new(self.pool.clone())
            .list_batch_candidates(batch.start_date, batch.end_date, query, cursor, page_size)
            .await?;
        Ok(BatchCandidatePage {
            batch,
            items: page.items,
            next_cursor: page.next_cursor,
        })
    }

    pub async fn assign_items(
        &self,
        batch_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<BatchDetail, AppError> {
        let mut seen = HashSet::with_capacity(item_ids.len());
        let item_ids = item_ids
            .iter()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect::<Vec<_>>();
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| database_error("failed to begin batch assignment", error))?;

        let result = async {
            let batch_exists =
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM batches WHERE id = ?")
                    .bind(batch_id.to_string())
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(|error| {
                        database_error("failed to validate assignment batch", error)
                    })?
                    != 0;
            if !batch_exists {
                return Err(batch_not_found(batch_id));
            }

            let mut assignments = Vec::new();
            for item_id in item_ids {
                let state = sqlx::query_as::<_, (String, String, Option<String>)>(
                    "SELECT recognition_status, dedupe_status, batch_id FROM items WHERE id = ?",
                )
                .bind(item_id.to_string())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|error| database_error("failed to validate assignment item", error))?
                .ok_or_else(|| item_not_found(item_id))?;
                let (recognition_status, dedupe_status, current_batch_id) = state;
                if dedupe_status == dedupe_status_str(DedupeStatus::SuspectedDuplicate) {
                    return Err(AppError::Conflict {
                        message: format!(
                            "item {item_id} is a suspected duplicate and cannot be assigned"
                        ),
                    });
                }
                if recognition_status == recognition_status_str(RecognitionStatus::Failed) {
                    return Err(AppError::Conflict {
                        message: format!(
                            "item {item_id} has failed recognition and cannot be assigned"
                        ),
                    });
                }
                match current_batch_id {
                    None => assignments.push((item_id, None)),
                    Some(current) if current == batch_id.to_string() => {}
                    Some(current) => assignments.push((
                        item_id,
                        Some(Uuid::parse_str(&current).map_err(|_| AppError::Internal {
                            message: "invalid item batch reference".to_owned(),
                        })?),
                    )),
                }
            }

            let updated_at = Utc::now();
            for (item_id, _) in &assignments {
                sqlx::query("UPDATE items SET batch_id = ?, updated_at = ? WHERE id = ?")
                    .bind(batch_id.to_string())
                    .bind(updated_at.to_rfc3339())
                    .bind(item_id.to_string())
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| database_error("failed to assign item", error))?;
            }
            if !assignments.is_empty() {
                let mut affected_batches = assignments
                    .iter()
                    .filter_map(|(_, old_batch_id)| *old_batch_id)
                    .chain(std::iter::once(batch_id))
                    .collect::<Vec<_>>();
                affected_batches.sort_unstable();
                affected_batches.dedup();
                for affected_batch_id in affected_batches {
                    sqlx::query("UPDATE batches SET status = 'draft', updated_at = ? WHERE id = ?")
                        .bind(updated_at.to_rfc3339())
                        .bind(affected_batch_id.to_string())
                        .execute(&mut *transaction)
                        .await
                        .map_err(|error| {
                            database_error("failed to refresh assigned batch", error)
                        })?;
                    validate_summary_total(&mut transaction, affected_batch_id).await?;
                }
            }
            Ok(())
        }
        .await;

        finish_unit_transaction(transaction, result).await?;
        self.get(batch_id).await
    }

    pub(crate) async fn assign_unassigned_items(
        &self,
        batch_id: Uuid,
        paths: &AppPaths,
        item_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, AppError> {
        let mut seen = HashSet::with_capacity(item_ids.len());
        let item_ids = item_ids
            .iter()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect::<Vec<_>>();
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| database_error("failed to begin automation assignment", error))?;

        let result = async {
            let batch = BatchRepository::get_with_connection(&mut transaction, batch_id).await?;

            let updated_at = Utc::now().to_rfc3339();
            let mut assigned_ids = Vec::new();
            for item_id in item_ids {
                let Some(item) =
                    ItemRepository::find_by_id_with_connection(&mut transaction, item_id).await?
                else {
                    continue;
                };
                if item.batch_id.is_some()
                    || !is_safe_batch_candidate(paths, &item, batch.start_date, batch.end_date)
                {
                    continue;
                }
                let result = sqlx::query(
                    "UPDATE items SET batch_id = ?, updated_at = ? \
                     WHERE id = ? AND batch_id IS NULL",
                )
                .bind(batch_id.to_string())
                .bind(&updated_at)
                .bind(item_id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(|error| database_error("failed to assign automation item", error))?;
                if result.rows_affected() == 1 {
                    assigned_ids.push(item_id);
                }
            }
            if !assigned_ids.is_empty() {
                sqlx::query("UPDATE batches SET status = 'draft', updated_at = ? WHERE id = ?")
                    .bind(&updated_at)
                    .bind(batch_id.to_string())
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| database_error("failed to refresh automation batch", error))?;
                validate_summary_total(&mut transaction, batch_id).await?;
            }
            Ok(assigned_ids)
        }
        .await;

        finish_automation_assignment(transaction, result).await
    }

    pub async fn remove_item(
        &self,
        batch_id: Uuid,
        item_id: Uuid,
    ) -> Result<BatchDetail, AppError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| database_error("failed to begin item removal", error))?;
        let result = async {
            let batch_exists =
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM batches WHERE id = ?")
                    .bind(batch_id.to_string())
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(|error| database_error("failed to validate removal batch", error))?
                    != 0;
            if !batch_exists {
                return Err(batch_not_found(batch_id));
            }

            let current_batch_id =
                sqlx::query_as::<_, (Option<String>,)>("SELECT batch_id FROM items WHERE id = ?")
                    .bind(item_id.to_string())
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(|error| database_error("failed to validate removal item", error))?
                    .ok_or_else(|| item_not_found(item_id))?
                    .0;
            if current_batch_id.as_deref() != Some(batch_id.to_string().as_str()) {
                return Err(AppError::Conflict {
                    message: format!("item {item_id} does not belong to batch {batch_id}"),
                });
            }

            sqlx::query("UPDATE items SET batch_id = NULL, updated_at = ? WHERE id = ?")
                .bind(Utc::now().to_rfc3339())
                .bind(item_id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(|error| database_error("failed to remove item from batch", error))?;
            sqlx::query("UPDATE batches SET status = 'draft', updated_at = ? WHERE id = ?")
                .bind(Utc::now().to_rfc3339())
                .bind(batch_id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(|error| database_error("failed to refresh batch after removal", error))?;
            validate_summary_total(&mut transaction, batch_id).await?;
            Ok(())
        }
        .await;

        finish_unit_transaction(transaction, result).await?;
        self.get(batch_id).await
    }

    pub async fn get(&self, batch_id: Uuid) -> Result<BatchDetail, AppError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| database_error("failed to begin batch detail", error))?;
        let result = Self::detail_with_connection(&mut transaction, batch_id).await;
        finish_detail_transaction(transaction, result).await
    }

    async fn detail_with_connection(
        connection: &mut SqliteConnection,
        batch_id: Uuid,
    ) -> Result<BatchDetail, AppError> {
        let batch = BatchRepository::get_with_connection(connection, batch_id).await?;
        let items = ItemRepository::list_by_batch_with_connection(connection, batch_id).await?;
        let warnings = items
            .iter()
            .filter(|item| {
                item.batch_membership_date()
                    .is_some_and(|date| date < batch.start_date || date > batch.end_date)
            })
            .map(|item| format!("outside_date_range:{}", item.id))
            .collect();
        let summary = build_summary(&items)?;

        Ok(BatchDetail {
            batch,
            items,
            summary,
            warnings,
        })
    }
}

fn normalize_candidate_query(query: Option<String>) -> Result<Option<String>, AppError> {
    let query = query.map(|value| value.trim().to_owned());
    let Some(query) = query.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if query.chars().count() > 200 || query.chars().any(char::is_control) {
        return Err(AppError::validation(
            "query",
            "query must contain at most 200 printable characters",
        ));
    }
    Ok(Some(query))
}

fn parse_date(value: &str, field: &str) -> Result<NaiveDate, AppError> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .filter(|date| date.format("%Y-%m-%d").to_string() == value)
        .ok_or_else(|| AppError::validation(field, "must be YYYY-MM-DD"))
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn build_summary(items: &[InvoiceItem]) -> Result<BatchDetailSummary, AppError> {
    let mut summary = BatchDetailSummary {
        item_count: u64::try_from(items.len()).map_err(|_| summary_count_overflow())?,
        ..BatchDetailSummary::default()
    };
    for item in items {
        let amount = item.amount_cents.unwrap_or(0);
        summary.total_amount_cents =
            checked_add_amount_cents(summary.total_amount_cents, amount, "totalAmountCents")?;
        if item.confirmation_status != ConfirmationStatus::Confirmed {
            summary.unconfirmed_count = summary
                .unconfirmed_count
                .checked_add(1)
                .ok_or_else(summary_count_overflow)?;
        }
        if let Some(category) = item.final_category {
            let category_summary = match category {
                Category::Transport => &mut summary.transport,
                Category::Dining => &mut summary.dining,
                Category::Accommodation => &mut summary.accommodation,
                Category::Hospitality => &mut summary.hospitality,
            };
            category_summary.item_count = category_summary
                .item_count
                .checked_add(1)
                .ok_or_else(summary_count_overflow)?;
            category_summary.amount_cents =
                checked_add_amount_cents(category_summary.amount_cents, amount, "amountCents")?;
        }
    }
    Ok(summary)
}

fn summary_count_overflow() -> AppError {
    AppError::Internal {
        message: "batch summary count overflow".to_owned(),
    }
}

async fn validate_summary_total(
    connection: &mut SqliteConnection,
    batch_id: Uuid,
) -> Result<(), AppError> {
    let amounts =
        sqlx::query_scalar::<_, Option<i64>>("SELECT amount_cents FROM items WHERE batch_id = ?")
            .bind(batch_id.to_string())
            .fetch_all(&mut *connection)
            .await
            .map_err(|error| database_error("failed to validate batch summary", error))?;
    amounts.into_iter().try_fold(0_i64, |total, amount| {
        checked_add_amount_cents(total, amount.unwrap_or(0), "totalAmountCents")
    })?;
    Ok(())
}

async fn finish_unit_transaction(
    transaction: sqlx::Transaction<'_, sqlx::Sqlite>,
    result: Result<(), AppError>,
) -> Result<(), AppError> {
    match result {
        Ok(()) => transaction
            .commit()
            .await
            .map_err(|error| database_error("failed to commit batch transaction", error)),
        Err(error) => match transaction.rollback().await {
            Ok(()) => Err(error),
            Err(_) => Err(AppError::Internal {
                message: "batch transaction failed and rollback also failed".to_owned(),
            }),
        },
    }
}

async fn finish_automation_assignment(
    transaction: sqlx::Transaction<'_, sqlx::Sqlite>,
    result: Result<Vec<Uuid>, AppError>,
) -> Result<Vec<Uuid>, AppError> {
    match result {
        Ok(assigned_ids) => transaction
            .commit()
            .await
            .map(|()| assigned_ids)
            .map_err(|error| database_error("failed to commit automation assignment", error)),
        Err(error) => match transaction.rollback().await {
            Ok(()) => Err(error),
            Err(_) => Err(AppError::Internal {
                message: "automation assignment failed and rollback also failed".to_owned(),
            }),
        },
    }
}

async fn finish_detail_transaction(
    transaction: sqlx::Transaction<'_, sqlx::Sqlite>,
    result: Result<BatchDetail, AppError>,
) -> Result<BatchDetail, AppError> {
    match result {
        Ok(detail) => transaction
            .commit()
            .await
            .map(|()| detail)
            .map_err(|error| database_error("failed to commit batch detail transaction", error)),
        Err(error) => match transaction.rollback().await {
            Ok(()) => Err(error),
            Err(_) => Err(AppError::Internal {
                message: "batch detail transaction failed and rollback also failed".to_owned(),
            }),
        },
    }
}

fn batch_not_found(id: Uuid) -> AppError {
    AppError::NotFound {
        entity: "batch".to_owned(),
        message: format!("batch {id} was not found"),
    }
}

fn item_not_found(id: Uuid) -> AppError {
    AppError::NotFound {
        entity: "item".to_owned(),
        message: format!("item {id} was not found"),
    }
}

fn recognition_status_str(value: RecognitionStatus) -> &'static str {
    match value {
        RecognitionStatus::Pending => "pending",
        RecognitionStatus::Succeeded => "succeeded",
        RecognitionStatus::Failed => "failed",
    }
}

fn dedupe_status_str(value: DedupeStatus) -> &'static str {
    match value {
        DedupeStatus::Unique => "unique",
        DedupeStatus::SuspectedDuplicate => "suspected_duplicate",
        DedupeStatus::Resolved => "resolved",
    }
}

fn database_error(context: &str, error: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(database_error) = &error
        && database_error
            .code()
            .and_then(|code| code.parse::<i32>().ok())
            .is_some_and(|code| matches!(code & 0xff, 5 | 6))
    {
        return AppError::External {
            service: "database".to_owned(),
            retryable: true,
            message: context.to_owned(),
        };
    }
    AppError::Internal {
        message: context.to_owned(),
    }
}
