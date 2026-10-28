use std::collections::HashSet;

use chrono::{Datelike, NaiveDate, Utc};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::db::batches::{Batch, BatchPage, BatchPageCursor, BatchRepository, BatchSummary};
use crate::db::items::{InvoiceItem, ItemPageCursor, ItemRepository, category_str};
use crate::domain::amount::checked_add_amount_cents;
use crate::domain::error::AppError;
use crate::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, NewBatch, RecognitionStatus, SourceType,
};
use crate::infra::files::AppPaths;
use crate::services::batch_eligibility::{
    BatchIssue, can_fill_other_category, db_export_blocker, describe_issues, is_in_batch_range,
    is_safe_batch_candidate,
};
use crate::services::recognition::original_can_be_normalized;

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

/// Longest range a single batch may cover, matching the automation limit.
const MAX_RANGE_DAYS: u32 = 366;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchDetailSummary {
    pub item_count: u64,
    pub total_amount_cents: i64,
    pub transport: CategorySummary,
    pub dining: CategorySummary,
    pub accommodation: CategorySummary,
    pub hospitality: CategorySummary,
    pub other: CategorySummary,
    pub unconfirmed_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchDetail {
    pub batch: Batch,
    pub items: Vec<InvoiceItem>,
    pub summary: BatchDetailSummary,
    pub warnings: Vec<String>,
    /// Invoices a single operation moved into this batch from other batches.
    ///
    /// Batch membership is a single-owner relationship, so this is an operation
    /// report rather than stored state: a plain reload sees the moved invoices
    /// as ordinary members and reports zero.
    pub moved_from_other_batches_count: u32,
}

/// Result of one automation assignment pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AutomationAssignmentOutcome {
    pub assigned_ids: Vec<Uuid>,
    /// How many of `assigned_ids` were taken away from another batch.
    pub moved_from_other_batches_count: u32,
    /// How many assigned invoices had their missing category filled as "其他"
    /// by this automation pass (the user-facing "按其他分类纳入" count).
    pub categorized_as_other_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchCandidatePage {
    pub batch: Batch,
    pub items: Vec<InvoiceItem>,
    pub next_cursor: Option<ItemPageCursor>,
}

/// Options for the bulk "confirm every ready invoice" action.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettleBatchInput {
    /// Use the mail received date when an invoice date could not be read.
    pub fill_invoice_date_from_received: bool,
    /// Adopt the recognized category when the user has not chosen one.
    pub apply_suggested_category: bool,
    /// Fallback category for invoices that have no category at all.
    pub default_category: Option<Category>,
}

/// One batch member the bulk confirm had to skip, with a stable reason code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedBatchItem {
    pub item_id: Uuid,
    pub file_name: String,
    pub code: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettleBatchOutcome {
    pub confirmed_count: u32,
    pub filled_invoice_date_count: u32,
    pub applied_category_count: u32,
    pub skipped: Vec<SkippedBatchItem>,
}

/// Options for the inbox-level "confirm every derivable invoice" action.
///
/// Unlike [`SettleBatchInput`] this has no category choices: an invoice with no
/// category is filed under "其他" because that is the user's standing rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoverableSettleInput {
    /// Use the mail received date when an invoice date could not be read.
    pub fill_invoice_date_from_received: bool,
}

impl Default for RecoverableSettleInput {
    /// Fills the date by default: 334 of the user's invoices carry an amount but
    /// no readable invoice date, and the mail received date is the only signal
    /// left. The outcome reports how many dates were filled.
    fn default() -> Self {
        Self {
            fill_invoice_date_from_received: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverableSettleOutcome {
    pub confirmed_count: u32,
    pub filled_invoice_date_count: u32,
    pub applied_category_count: u32,
    /// Ids of the invoices this run confirmed, so the caller can repair the
    /// normalized PDFs they still miss.
    pub confirmed_item_ids: Vec<Uuid>,
    pub skipped: Vec<SkippedBatchItem>,
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
            .list_batch_candidates(
                batch_id,
                batch.start_date,
                batch.end_date,
                query,
                cursor,
                page_size,
            )
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
            let mut blocked: Vec<BatchIssue> = Vec::new();
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
                let previous_batch = match current_batch_id {
                    None => None,
                    Some(current) if current == batch_id.to_string() => continue,
                    Some(current) => {
                        Some(Uuid::parse_str(&current).map_err(|_| AppError::Internal {
                            message: "invalid item batch reference".to_owned(),
                        })?)
                    }
                };
                // A batch member without an exportable original or normalized
                // PDF makes the whole batch unexportable, so manual assignment
                // must refuse it here instead of failing much later at export.
                let item = ItemRepository::find_by_id_with_connection(&mut transaction, item_id)
                    .await?
                    .ok_or_else(|| item_not_found(item_id))?;
                if let Some(blocker) = db_export_blocker(&item) {
                    blocked.push(BatchIssue {
                        item_id,
                        file_name: item.original_name.clone(),
                        blocker,
                    });
                    continue;
                }
                assignments.push((item_id, previous_batch));
            }
            if !blocked.is_empty() {
                return Err(AppError::Conflict {
                    message: format!(
                        "{} 张票据无法加入批次：{}",
                        blocked.len(),
                        describe_issues(&blocked)
                    ),
                });
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
            let moved_from_other_batches_count = u32::try_from(
                assignments
                    .iter()
                    .filter(|(_, previous_batch)| previous_batch.is_some())
                    .count(),
            )
            .map_err(|_| summary_count_overflow())?;
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
            Ok(moved_from_other_batches_count)
        }
        .await;

        let moved_from_other_batches_count = finish_bulk_transaction(transaction, result).await?;
        let mut detail = self.get(batch_id).await?;
        detail.moved_from_other_batches_count = moved_from_other_batches_count;
        Ok(detail)
    }

    /// Claims every eligible invoice whose batch membership date falls inside
    /// the batch range, moving it out of any other batch.
    ///
    /// This is the range re-anchoring rule: a new 4–20 batch takes over the
    /// invoices an earlier 14–20 batch already holds instead of leaving them
    /// behind. Invoices outside the range are never touched. Every move happens
    /// inside one `BEGIN IMMEDIATE` transaction, the source batches fall back to
    /// `draft`, and the caller learns how many invoices actually changed hands.
    ///
    /// `fill_category_ids` are candidate ids the caller found fully exportable
    /// except for the missing category. Inside the same transaction they get
    /// `final_category = 'other'` (and the confirmation that category unlocks)
    /// before being claimed, so a batch is never left half-settled.
    pub(crate) async fn assign_unassigned_items(
        &self,
        batch_id: Uuid,
        paths: &AppPaths,
        item_ids: &[Uuid],
        fill_category_ids: &[Uuid],
    ) -> Result<AutomationAssignmentOutcome, AppError> {
        let mut seen = HashSet::with_capacity(item_ids.len());
        let item_ids = item_ids
            .iter()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect::<Vec<_>>();
        let fill_category_ids = fill_category_ids.iter().copied().collect::<HashSet<_>>();
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| database_error("failed to begin automation assignment", error))?;

        let result = async {
            let batch = BatchRepository::get_with_connection(&mut transaction, batch_id).await?;

            let updated_at = Utc::now().to_rfc3339();
            let mut outcome = AutomationAssignmentOutcome::default();
            let mut source_batches = Vec::new();
            let mut filled_category_ids = HashSet::new();
            for item_id in item_ids {
                let Some(mut item) =
                    ItemRepository::find_by_id_with_connection(&mut transaction, item_id).await?
                else {
                    continue;
                };
                if item.batch_id == Some(batch_id) {
                    continue;
                }
                if item.final_category.is_none() && fill_category_ids.contains(&item_id) {
                    let Some(filled) = Self::fill_other_category(
                        &mut transaction,
                        paths,
                        &batch,
                        &item,
                        &updated_at,
                    )
                    .await?
                    else {
                        continue;
                    };
                    item = filled;
                    filled_category_ids.insert(item_id);
                }
                if !is_safe_batch_candidate(paths, &item, batch.start_date, batch.end_date) {
                    continue;
                }
                let affected = sqlx::query(
                    "UPDATE items SET batch_id = ?, updated_at = ? \
                     WHERE id = ? AND (batch_id IS NULL OR batch_id <> ?)",
                )
                .bind(batch_id.to_string())
                .bind(&updated_at)
                .bind(item_id.to_string())
                .bind(batch_id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(|error| database_error("failed to assign automation item", error))?
                .rows_affected();
                if affected != 1 {
                    continue;
                }
                if let Some(previous_batch) = item.batch_id {
                    source_batches.push(previous_batch);
                    outcome.moved_from_other_batches_count = outcome
                        .moved_from_other_batches_count
                        .checked_add(1)
                        .ok_or_else(summary_count_overflow)?;
                }
                if filled_category_ids.contains(&item_id) {
                    outcome.categorized_as_other_count = outcome
                        .categorized_as_other_count
                        .checked_add(1)
                        .ok_or_else(summary_count_overflow)?;
                }
                outcome.assigned_ids.push(item_id);
            }
            source_batches.sort_unstable();
            source_batches.dedup();
            if !outcome.assigned_ids.is_empty() {
                for source_batch in source_batches {
                    sqlx::query("UPDATE batches SET status = 'draft', updated_at = ? WHERE id = ?")
                        .bind(&updated_at)
                        .bind(source_batch.to_string())
                        .execute(&mut *transaction)
                        .await
                        .map_err(|error| {
                            database_error("failed to refresh re-anchored source batch", error)
                        })?;
                    validate_summary_total(&mut transaction, source_batch).await?;
                }
                sqlx::query("UPDATE batches SET status = 'draft', updated_at = ? WHERE id = ?")
                    .bind(&updated_at)
                    .bind(batch_id.to_string())
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| database_error("failed to refresh automation batch", error))?;
                validate_summary_total(&mut transaction, batch_id).await?;
            }
            Ok(outcome)
        }
        .await;

        finish_automation_assignment(transaction, result).await
    }

    /// Fills the missing category of an otherwise-exportable invoice as "其他"
    /// and confirms it, inside the caller's automation transaction.
    ///
    /// Returns the refreshed invoice when the row was actually updated so the
    /// caller re-validates the *stored* state instead of the stale scan result.
    /// The same `BEGIN IMMEDIATE` transaction then claims the invoice, which
    /// keeps "categorized" and "assigned" from ever diverging.
    async fn fill_other_category(
        connection: &mut SqliteConnection,
        paths: &AppPaths,
        batch: &Batch,
        item: &InvoiceItem,
        updated_at: &str,
    ) -> Result<Option<InvoiceItem>, AppError> {
        if item.final_category.is_some()
            || !is_in_batch_range(item, batch.start_date, batch.end_date)
            || !can_fill_other_category(paths, item)
        {
            return Ok(None);
        }
        let Some(suggested_period) = item.suggested_period.clone().or_else(|| {
            item.invoice_date
                .map(|date| date.format("%Y-%m").to_string())
        }) else {
            return Ok(None);
        };
        let affected = sqlx::query(
            "UPDATE items SET final_category = 'other', suggested_period = ?, \
                confirmation_status = 'confirmed', updated_at = ? \
             WHERE id = ? AND final_category IS NULL",
        )
        .bind(&suggested_period)
        .bind(updated_at)
        .bind(item.id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(|error| database_error("failed to fill missing category as other", error))?
        .rows_affected();
        if affected != 1 {
            return Ok(None);
        }
        ItemRepository::find_by_id_with_connection(connection, item.id).await
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

    /// Moves a batch's date range.
    ///
    /// Batches are created around a month or a custom range, and a single
    /// invoice received one day outside it used to leave the user with no option
    /// but to build a second batch. Members are kept as they are; the batch
    /// detail already reports the ones the new range excludes.
    pub async fn update_range(
        &self,
        batch_id: Uuid,
        start_date: &str,
        end_date: &str,
    ) -> Result<BatchDetail, AppError> {
        let start_date = parse_date(start_date, "startDate")?;
        let end_date = parse_date(end_date, "endDate")?;
        if start_date > end_date {
            return Err(AppError::validation(
                "dateRange",
                "start date must not be after end date",
            ));
        }
        // The automation refuses ranges longer than 366 days, so a batch must
        // never be saved into a range its own automation cannot process.
        let inclusive_days = end_date
            .signed_duration_since(start_date)
            .num_days()
            .checked_add(1)
            .ok_or_else(summary_count_overflow)?;
        if inclusive_days > i64::from(MAX_RANGE_DAYS) {
            return Err(AppError::validation(
                "dateRange",
                format!("a batch range must not exceed {MAX_RANGE_DAYS} days"),
            ));
        }

        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| database_error("failed to begin batch range update", error))?;
        let result = async {
            let exists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM batches WHERE id = ?")
                .bind(batch_id.to_string())
                .fetch_one(&mut *transaction)
                .await
                .map_err(|error| database_error("failed to validate batch range update", error))?
                != 0;
            if !exists {
                return Err(batch_not_found(batch_id));
            }
            sqlx::query(
                "UPDATE batches SET start_date = ?, end_date = ?, status = 'draft', updated_at = ? \
                 WHERE id = ?",
            )
            .bind(start_date.to_string())
            .bind(end_date.to_string())
            .bind(Utc::now().to_rfc3339())
            .bind(batch_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| database_error("failed to update batch range", error))?;
            Ok(())
        }
        .await;
        finish_unit_transaction(transaction, result).await?;
        self.get(batch_id).await
    }

    /// Removes several invoices from a batch in one transaction.
    ///
    /// Ids that do not belong to the batch are ignored instead of failing the
    /// whole call, so the batch detail can offer "remove every blocking
    /// invoice" without racing the user's own edits.
    pub async fn remove_items(
        &self,
        batch_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<(BatchDetail, u32), AppError> {
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
            .map_err(|error| database_error("failed to begin batch removal", error))?;
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

            let updated_at = Utc::now().to_rfc3339();
            let mut removed = 0_u32;
            for item_id in item_ids {
                let affected = sqlx::query(
                    "UPDATE items SET batch_id = NULL, updated_at = ? \
                     WHERE id = ? AND batch_id = ?",
                )
                .bind(&updated_at)
                .bind(item_id.to_string())
                .bind(batch_id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(|error| database_error("failed to remove batch item", error))?
                .rows_affected();
                removed = removed.saturating_add(u32::try_from(affected).unwrap_or(0));
            }
            if removed != 0 {
                sqlx::query("UPDATE batches SET status = 'draft', updated_at = ? WHERE id = ?")
                    .bind(&updated_at)
                    .bind(batch_id.to_string())
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| {
                        database_error("failed to refresh batch after bulk removal", error)
                    })?;
                validate_summary_total(&mut transaction, batch_id).await?;
            }
            Ok(removed)
        }
        .await;

        let removed = finish_bulk_transaction(transaction, result).await?;
        Ok((self.get(batch_id).await?, removed))
    }

    /// Confirms every batch member whose reviewed values can be completed, so a
    /// batch of hundreds of invoices does not have to be confirmed one by one.
    ///
    /// "待确认" invoices are exactly the ones recognition could not finish, so
    /// this never invents missing data: it only derives what the data already
    /// implies (mail received date for a missing invoice date, suggested
    /// category, month from the date) and reports every invoice it had to skip
    /// with a reason.
    pub async fn settle_items(
        &self,
        batch_id: Uuid,
        input: SettleBatchInput,
    ) -> Result<SettleBatchOutcome, AppError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| database_error("failed to begin batch settle", error))?;
        let result = async {
            let batch = BatchRepository::get_with_connection(&mut transaction, batch_id).await?;
            let items =
                ItemRepository::list_by_batch_with_connection(&mut transaction, batch_id).await?;
            let mut outcome = SettleBatchOutcome::default();
            let mut changed = false;
            for item in items {
                if item.confirmation_status == ConfirmationStatus::Confirmed {
                    continue;
                }
                let decision = match settle_decision(
                    &item,
                    input.fill_invoice_date_from_received,
                    input.apply_suggested_category,
                    input.default_category,
                ) {
                    Ok(decision) => decision,
                    Err(code) => {
                        outcome.skipped.push(SkippedBatchItem {
                            item_id: item.id,
                            file_name: item.original_name.clone(),
                            code: code.to_owned(),
                        });
                        continue;
                    }
                };
                if !is_in_batch_range(&item, batch.start_date, batch.end_date) {
                    outcome.skipped.push(SkippedBatchItem {
                        item_id: item.id,
                        file_name: item.original_name.clone(),
                        code: "outside_date_range".to_owned(),
                    });
                    continue;
                }

                let updated_at = Utc::now();
                let affected = sqlx::query(
                    "UPDATE items SET invoice_date = ?, suggested_period = ?, final_category = ?, \
                        recognition_status = 'succeeded', confirmation_status = 'confirmed', \
                        updated_at = ? \
                     WHERE id = ? AND confirmation_status = 'pending'",
                )
                .bind(decision.invoice_date.to_string())
                .bind(&decision.suggested_period)
                .bind(category_str(decision.final_category))
                .bind(updated_at.to_rfc3339())
                .bind(item.id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(|error| database_error("failed to confirm batch item", error))?
                .rows_affected();
                if affected == 0 {
                    continue;
                }
                changed = true;
                outcome.confirmed_count = outcome.confirmed_count.saturating_add(1);
                if item.invoice_date.is_none() {
                    outcome.filled_invoice_date_count =
                        outcome.filled_invoice_date_count.saturating_add(1);
                }
                if item.final_category != Some(decision.final_category) {
                    outcome.applied_category_count =
                        outcome.applied_category_count.saturating_add(1);
                }
            }
            if changed {
                sqlx::query("UPDATE batches SET status = 'draft', updated_at = ? WHERE id = ?")
                    .bind(Utc::now().to_rfc3339())
                    .bind(batch_id.to_string())
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| {
                        database_error("failed to refresh batch after settle", error)
                    })?;
                validate_summary_total(&mut transaction, batch_id).await?;
            }
            Ok(outcome)
        }
        .await;

        finish_bulk_transaction(transaction, result).await
    }

    /// Confirms every invoice in the library that is recognized but still
    /// waiting for confirmation, as long as its missing values can be derived
    /// from what is already stored.
    ///
    /// This is the inbox-level counterpart of [`Self::settle_items`] and shares
    /// its per-item rules (and its `BEGIN IMMEDIATE` transaction). An invoice
    /// with no category is filed under the suggested category, or "其他" when
    /// there is none; a missing month is derived from the invoice date. Amount,
    /// invoice date and a normalizable original remain hard requirements, so a
    /// skipped invoice is reported with a stable reason instead of being
    /// confirmed without the data an export needs.
    pub async fn settle_recoverable_items(
        &self,
        input: RecoverableSettleInput,
    ) -> Result<RecoverableSettleOutcome, AppError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| database_error("failed to begin recoverable settle", error))?;
        let result = async {
            let items =
                ItemRepository::list_pending_recoverable_with_connection(&mut transaction).await?;
            let mut outcome = RecoverableSettleOutcome::default();
            let mut batches_to_refresh = HashSet::new();
            for item in items {
                let decision = match settle_decision(
                    &item,
                    input.fill_invoice_date_from_received,
                    true,
                    Some(Category::Other),
                ) {
                    Ok(decision) => decision,
                    Err(code) => {
                        outcome.skipped.push(SkippedBatchItem {
                            item_id: item.id,
                            file_name: item.original_name.clone(),
                            code: recoverable_skip_code(code).to_owned(),
                        });
                        continue;
                    }
                };

                let updated_at = Utc::now();
                let affected = sqlx::query(
                    "UPDATE items SET invoice_date = ?, suggested_period = ?, final_category = ?, \
                        recognition_status = 'succeeded', confirmation_status = 'confirmed', \
                        updated_at = ? \
                     WHERE id = ? AND confirmation_status = 'pending'",
                )
                .bind(decision.invoice_date.to_string())
                .bind(&decision.suggested_period)
                .bind(category_str(decision.final_category))
                .bind(updated_at.to_rfc3339())
                .bind(item.id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(|error| database_error("failed to confirm recoverable item", error))?
                .rows_affected();
                if affected == 0 {
                    continue;
                }
                outcome.confirmed_count = outcome.confirmed_count.saturating_add(1);
                outcome.confirmed_item_ids.push(item.id);
                if item.invoice_date.is_none() {
                    outcome.filled_invoice_date_count =
                        outcome.filled_invoice_date_count.saturating_add(1);
                }
                if item.final_category != Some(decision.final_category) {
                    outcome.applied_category_count =
                        outcome.applied_category_count.saturating_add(1);
                }
                if let Some(batch_id) = item.batch_id {
                    batches_to_refresh.insert(batch_id);
                }
            }
            // A confirmed invoice can belong to an already exported batch; that
            // batch has to be exported again so the export reflects the change.
            let refreshed_at = Utc::now().to_rfc3339();
            for batch_id in batches_to_refresh {
                sqlx::query("UPDATE batches SET status = 'draft', updated_at = ? WHERE id = ?")
                    .bind(&refreshed_at)
                    .bind(batch_id.to_string())
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| {
                        database_error("failed to refresh a batch after recoverable settle", error)
                    })?;
            }
            Ok(outcome)
        }
        .await;

        finish_bulk_transaction(transaction, result).await
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
            moved_from_other_batches_count: 0,
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

/// The reviewed values a bulk confirmation writes onto one invoice.
struct SettleDecision {
    invoice_date: NaiveDate,
    suggested_period: String,
    final_category: Category,
}

/// Applies the shared "can this invoice be confirmed from its own data?" rule.
///
/// Both the per-batch settle and the inbox-level recoverable settle use this so
/// they can never disagree about what counts as derivable. The `Err` value is
/// the stable skip code reported to the user; a missing category is never an
/// error when a fallback (suggested category or the caller's default) exists.
fn settle_decision(
    item: &InvoiceItem,
    fill_invoice_date_from_received: bool,
    apply_suggested_category: bool,
    default_category: Option<Category>,
) -> Result<SettleDecision, &'static str> {
    if item.dedupe_status == DedupeStatus::SuspectedDuplicate {
        return Err("suspected_duplicate");
    }
    if item.recognition_status == RecognitionStatus::Failed {
        return Err("recognition_failed");
    }
    if item.recognition_status == RecognitionStatus::Pending {
        return Err("not_recognized");
    }
    if item.amount_cents.is_none() {
        return Err("missing_amount");
    }
    let derived_date = item.invoice_date.or_else(|| {
        (fill_invoice_date_from_received && item.source_type == SourceType::Email)
            .then_some(item.source_received_date)
            .flatten()
    });
    let Some(invoice_date) = derived_date else {
        return Err("missing_invoice_date");
    };
    let suggested_period = item
        .suggested_period
        .clone()
        .unwrap_or_else(|| format!("{:04}-{:02}", invoice_date.year(), invoice_date.month()));
    let final_category = item
        .final_category
        .or_else(|| {
            apply_suggested_category
                .then_some(item.suggested_category)
                .flatten()
        })
        .or(default_category);
    let Some(final_category) = final_category else {
        return Err("missing_category");
    };
    // Confirming an invoice whose original can never be normalized would
    // recreate the very state that blocked exports.
    if item.normalized_pdf_path.is_none() && !original_can_be_normalized(&item.original_name) {
        return Err("unsupported_format");
    }
    Ok(SettleDecision {
        invoice_date,
        suggested_period,
        final_category,
    })
}

/// User-facing skip code for the inbox-level recoverable settle.
///
/// The per-item rule reports "unsupported_format"; at library scope the user
/// cares that the invoice has no normalized PDF at all.
fn recoverable_skip_code(code: &'static str) -> &'static str {
    match code {
        "unsupported_format" => "missing_normalized_pdf",
        other => other,
    }
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
                Category::Other => &mut summary.other,
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
    result: Result<AutomationAssignmentOutcome, AppError>,
) -> Result<AutomationAssignmentOutcome, AppError> {
    match result {
        Ok(outcome) => transaction
            .commit()
            .await
            .map(|()| outcome)
            .map_err(|error| database_error("failed to commit automation assignment", error)),
        Err(error) => match transaction.rollback().await {
            Ok(()) => Err(error),
            Err(_) => Err(AppError::Internal {
                message: "automation assignment failed and rollback also failed".to_owned(),
            }),
        },
    }
}

async fn finish_bulk_transaction<T>(
    transaction: sqlx::Transaction<'_, sqlx::Sqlite>,
    result: Result<T, AppError>,
) -> Result<T, AppError> {
    match result {
        Ok(value) => transaction
            .commit()
            .await
            .map(|()| value)
            .map_err(|error| database_error("failed to commit bulk batch transaction", error)),
        Err(error) => match transaction.rollback().await {
            Ok(()) => Err(error),
            Err(_) => Err(AppError::Internal {
                message: "bulk batch transaction failed and rollback also failed".to_owned(),
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
