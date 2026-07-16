use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{FromRow, QueryBuilder, Sqlite, SqliteConnection, SqlitePool, Transaction};
use uuid::Uuid;

use crate::db::items::{InvoiceItem, ItemRepository};
use crate::domain::amount::{
    amount_total_out_of_range, checked_add_amount_cents, validate_amount_cents,
};
use crate::domain::error::AppError;
use crate::domain::model::{BatchStatus, NewBatch};

const BATCH_COLUMNS: &str =
    "id, name, start_date, end_date, status, note, created_at, updated_at, last_exported_at";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub id: Uuid,
    pub name: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub status: BatchStatus,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_exported_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSummary {
    pub id: Uuid,
    pub name: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub status: BatchStatus,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_exported_at: Option<DateTime<Utc>>,
    pub item_count: i64,
    pub total_amount_cents: i64,
    pub unconfirmed_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchPageCursor {
    pub updated_at: DateTime<Utc>,
    pub id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchPage {
    pub batches: Vec<BatchSummary>,
    pub next_cursor: Option<BatchPageCursor>,
}

#[derive(Clone)]
pub struct BatchRepository {
    pool: SqlitePool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BatchExportSnapshot {
    pub batch: Batch,
    pub items: Vec<InvoiceItem>,
}

pub(crate) struct BatchExportClaim {
    transaction: Transaction<'static, Sqlite>,
    batch_id: Uuid,
}

impl BatchRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, batch: NewBatch) -> Result<Batch, AppError> {
        let id = Uuid::new_v4();
        let now = Utc::now();

        sqlx::query(
            "INSERT INTO batches (\
                id, name, start_date, end_date, status, note, created_at, updated_at\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(batch.name())
        .bind(batch.start_date().to_string())
        .bind(batch.end_date().to_string())
        .bind(batch_status_str(BatchStatus::Draft))
        .bind(batch.note())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(map_create_error)?;

        self.get(id).await
    }

    pub async fn get(&self, id: Uuid) -> Result<Batch, AppError> {
        let query = format!("SELECT {BATCH_COLUMNS} FROM batches WHERE id = ?");
        let row = sqlx::query_as::<_, DbBatchRow>(&query)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| internal_error("failed to get batch", error))?
            .ok_or_else(|| batch_not_found(id))?;

        Batch::try_from(row)
    }

    pub async fn update(&self, id: Uuid, batch: NewBatch) -> Result<Batch, AppError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|_| stable_internal_error("failed to begin batch update"))?;
        let result =
            async { Self::update_with_connection(&mut transaction, id, &batch).await }.await;

        finish_batch_transaction(transaction, result).await
    }

    pub(crate) async fn update_with_connection(
        connection: &mut SqliteConnection,
        id: Uuid,
        batch: &NewBatch,
    ) -> Result<Batch, AppError> {
        let current = Self::get_with_connection(connection, id).await?;
        let export_fields_changed = current.name != batch.name()
            || current.start_date != batch.start_date()
            || current.end_date != batch.end_date();
        let any_changed = export_fields_changed || current.note.as_deref() != batch.note();
        if !any_changed {
            return Ok(current);
        }
        let status = if export_fields_changed {
            BatchStatus::Draft
        } else {
            current.status
        };
        sqlx::query(
            "UPDATE batches SET name = ?, start_date = ?, end_date = ?, note = ?, \
                status = ?, updated_at = ? WHERE id = ?",
        )
        .bind(batch.name())
        .bind(batch.start_date().to_string())
        .bind(batch.end_date().to_string())
        .bind(batch.note())
        .bind(batch_status_str(status))
        .bind(Utc::now().to_rfc3339())
        .bind(id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(|_| stable_internal_error("failed to update batch"))?;
        Self::get_with_connection(connection, id).await
    }

    pub(crate) async fn get_with_connection(
        connection: &mut SqliteConnection,
        id: Uuid,
    ) -> Result<Batch, AppError> {
        let query = format!("SELECT {BATCH_COLUMNS} FROM batches WHERE id = ?");
        let row = sqlx::query_as::<_, DbBatchRow>(&query)
            .bind(id.to_string())
            .fetch_optional(&mut *connection)
            .await
            .map_err(|_| stable_internal_error("failed to get batch in transaction"))?
            .ok_or_else(|| batch_not_found(id))?;
        Batch::try_from(row)
    }

    pub async fn list(&self) -> Result<Vec<BatchSummary>, AppError> {
        let rows = sqlx::query_as::<_, DbBatchListRow>(
            "SELECT \
                b.id, b.name, b.start_date, b.end_date, b.status, b.note, \
                b.created_at, b.updated_at, b.last_exported_at, \
                i.id AS item_id, i.amount_cents AS item_amount_cents, \
                i.confirmation_status AS item_confirmation_status \
             FROM batches b \
             LEFT JOIN items i ON i.batch_id = b.id \
             ORDER BY b.updated_at DESC, b.id DESC, i.id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|error| internal_error("failed to list batches", error))?;

        let mut summaries = Vec::<BatchSummary>::new();
        for row in rows {
            let DbBatchListRow {
                id,
                name,
                start_date,
                end_date,
                status,
                note,
                created_at,
                updated_at,
                last_exported_at,
                item_id,
                item_amount_cents,
                item_confirmation_status,
            } = row;
            let batch = Batch::try_from(DbBatchRow {
                id,
                name,
                start_date,
                end_date,
                status,
                note,
                created_at,
                updated_at,
                last_exported_at,
            })?;
            if summaries
                .last()
                .is_none_or(|summary| summary.id != batch.id)
            {
                summaries.push(BatchSummary {
                    id: batch.id,
                    name: batch.name,
                    start_date: batch.start_date,
                    end_date: batch.end_date,
                    status: batch.status,
                    note: batch.note,
                    created_at: batch.created_at,
                    updated_at: batch.updated_at,
                    last_exported_at: batch.last_exported_at,
                    item_count: 0,
                    total_amount_cents: 0,
                    unconfirmed_count: 0,
                });
            }
            if item_id.is_some() {
                let summary = summaries
                    .last_mut()
                    .ok_or_else(|| stable_internal_error("failed to build batch summary"))?;
                summary.item_count = summary
                    .item_count
                    .checked_add(1)
                    .ok_or_else(|| stable_internal_error("batch summary item count overflow"))?;
                summary.total_amount_cents = checked_add_amount_cents(
                    summary.total_amount_cents,
                    item_amount_cents.unwrap_or(0),
                    "totalAmountCents",
                )?;
                if item_confirmation_status.as_deref() != Some("confirmed") {
                    summary.unconfirmed_count = summary
                        .unconfirmed_count
                        .checked_add(1)
                        .ok_or_else(|| stable_internal_error("batch unconfirmed count overflow"))?;
                }
            }
        }
        Ok(summaries)
    }

    pub async fn list_page(
        &self,
        cursor: Option<BatchPageCursor>,
        page_size: usize,
    ) -> Result<BatchPage, AppError> {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT b.id, b.name, b.start_date, b.end_date, b.status, b.note,
                    b.created_at, b.updated_at, b.last_exported_at,
                    COUNT(i.id) AS item_count,
                    COALESCE(SUM(i.amount_cents), 0) AS total_amount_cents,
                    COALESCE(SUM(CASE
                        WHEN i.id IS NOT NULL AND i.confirmation_status != 'confirmed'
                        THEN 1 ELSE 0 END), 0) AS unconfirmed_count
             FROM batches b
             LEFT JOIN items i ON i.batch_id = b.id",
        );
        if let Some(cursor) = cursor {
            let updated_at = cursor.updated_at.to_rfc3339();
            query
                .push(" WHERE (b.updated_at < ")
                .push_bind(updated_at.clone())
                .push(" OR (b.updated_at = ")
                .push_bind(updated_at)
                .push(" AND b.id < ")
                .push_bind(cursor.id.to_string())
                .push("))");
        }
        let fetch_limit = page_size.checked_add(1).ok_or_else(|| AppError::Internal {
            message: "batch page size overflow".to_owned(),
        })?;
        query
            .push(
                " GROUP BY b.id, b.name, b.start_date, b.end_date, b.status, b.note,
                           b.created_at, b.updated_at, b.last_exported_at
                  ORDER BY b.updated_at DESC, b.id DESC LIMIT ",
            )
            .push_bind(
                i64::try_from(fetch_limit)
                    .map_err(|_| AppError::validation("pageSize", "page size is too large"))?,
            );
        let rows = query
            .build_query_as::<DbBatchSummaryRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(map_batch_summary_query_error)?;
        let mut batches = rows
            .into_iter()
            .map(BatchSummary::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let has_next = batches.len() > page_size;
        batches.truncate(page_size);
        let next_cursor = has_next.then(|| {
            let last = batches
                .last()
                .expect("nonempty page must have a cursor row");
            BatchPageCursor {
                updated_at: last.updated_at,
                id: last.id,
            }
        });
        Ok(BatchPage {
            batches,
            next_cursor,
        })
    }

    pub async fn delete(&self, id: Uuid) -> Result<(), AppError> {
        let result = sqlx::query("DELETE FROM batches WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|error| internal_error("failed to delete batch", error))?;

        if result.rows_affected() == 0 {
            return Err(batch_not_found(id));
        }

        Ok(())
    }

    pub(crate) async fn export_snapshot(&self, id: Uuid) -> Result<BatchExportSnapshot, AppError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| stable_internal_error("failed to begin batch export snapshot"))?;
        let result = Self::export_snapshot_with_connection(&mut transaction, id).await;
        match result {
            Ok(snapshot) => transaction
                .commit()
                .await
                .map(|()| snapshot)
                .map_err(|_| stable_internal_error("failed to finish batch export snapshot")),
            Err(error) => match transaction.rollback().await {
                Ok(()) => Err(error),
                Err(_) => Err(stable_internal_error(
                    "batch export snapshot failed and rollback also failed",
                )),
            },
        }
    }

    pub(crate) async fn claim_export(
        &self,
        expected: &BatchExportSnapshot,
    ) -> Result<BatchExportClaim, AppError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|_| stable_internal_error("failed to claim batch export"))?;
        let current =
            Self::export_snapshot_with_connection(&mut transaction, expected.batch.id).await;
        match current {
            Ok(current) if current == *expected => Ok(BatchExportClaim {
                transaction,
                batch_id: expected.batch.id,
            }),
            Ok(_) => {
                transaction
                    .rollback()
                    .await
                    .map_err(|_| stable_internal_error("stale batch export rollback failed"))?;
                Err(AppError::Conflict {
                    message: "报销批次内容已更改，请重新导出".to_owned(),
                })
            }
            Err(error) => match transaction.rollback().await {
                Ok(()) => Err(error),
                Err(_) => Err(stable_internal_error(
                    "batch export claim failed and rollback also failed",
                )),
            },
        }
    }

    async fn export_snapshot_with_connection(
        connection: &mut SqliteConnection,
        id: Uuid,
    ) -> Result<BatchExportSnapshot, AppError> {
        Ok(BatchExportSnapshot {
            batch: Self::get_with_connection(connection, id).await?,
            items: ItemRepository::list_by_batch_with_connection(connection, id).await?,
        })
    }
}

impl BatchExportClaim {
    pub(crate) async fn commit(mut self, exported_at: DateTime<Utc>) -> Result<Batch, AppError> {
        let result = async {
            let rows = sqlx::query(
                "UPDATE batches SET status = 'exported', last_exported_at = ?, updated_at = ? \
                 WHERE id = ?",
            )
            .bind(exported_at.to_rfc3339())
            .bind(exported_at.to_rfc3339())
            .bind(self.batch_id.to_string())
            .execute(&mut *self.transaction)
            .await
            .map_err(|_| stable_internal_error("failed to mark batch exported"))?
            .rows_affected();
            if rows == 0 {
                return Err(batch_not_found(self.batch_id));
            }
            BatchRepository::get_with_connection(&mut self.transaction, self.batch_id).await
        }
        .await;

        finish_batch_transaction(self.transaction, result).await
    }
}

async fn finish_batch_transaction(
    transaction: Transaction<'_, Sqlite>,
    result: Result<Batch, AppError>,
) -> Result<Batch, AppError> {
    match result {
        Ok(batch) => transaction
            .commit()
            .await
            .map(|()| batch)
            .map_err(|_| stable_internal_error("failed to commit batch update")),
        Err(error) => match transaction.rollback().await {
            Ok(()) => Err(error),
            Err(_) => Err(stable_internal_error(
                "batch update failed and rollback also failed",
            )),
        },
    }
}

#[derive(FromRow)]
struct DbBatchRow {
    id: String,
    name: String,
    start_date: String,
    end_date: String,
    status: String,
    note: Option<String>,
    created_at: String,
    updated_at: String,
    last_exported_at: Option<String>,
}

#[derive(FromRow)]
struct DbBatchListRow {
    id: String,
    name: String,
    start_date: String,
    end_date: String,
    status: String,
    note: Option<String>,
    created_at: String,
    updated_at: String,
    last_exported_at: Option<String>,
    item_id: Option<String>,
    item_amount_cents: Option<i64>,
    item_confirmation_status: Option<String>,
}

#[derive(FromRow)]
struct DbBatchSummaryRow {
    id: String,
    name: String,
    start_date: String,
    end_date: String,
    status: String,
    note: Option<String>,
    created_at: String,
    updated_at: String,
    last_exported_at: Option<String>,
    item_count: i64,
    total_amount_cents: i64,
    unconfirmed_count: i64,
}

impl TryFrom<DbBatchSummaryRow> for BatchSummary {
    type Error = AppError;

    fn try_from(row: DbBatchSummaryRow) -> Result<Self, Self::Error> {
        let batch = Batch::try_from(DbBatchRow {
            id: row.id,
            name: row.name,
            start_date: row.start_date,
            end_date: row.end_date,
            status: row.status,
            note: row.note,
            created_at: row.created_at,
            updated_at: row.updated_at,
            last_exported_at: row.last_exported_at,
        })?;
        Ok(Self {
            id: batch.id,
            name: batch.name,
            start_date: batch.start_date,
            end_date: batch.end_date,
            status: batch.status,
            note: batch.note,
            created_at: batch.created_at,
            updated_at: batch.updated_at,
            last_exported_at: batch.last_exported_at,
            item_count: row.item_count,
            total_amount_cents: validate_amount_cents(row.total_amount_cents, "totalAmountCents")?,
            unconfirmed_count: row.unconfirmed_count,
        })
    }
}

impl TryFrom<DbBatchRow> for Batch {
    type Error = AppError;

    fn try_from(row: DbBatchRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: parse_uuid(&row.id, "id")?,
            name: row.name,
            start_date: parse_date(&row.start_date, "start_date")?,
            end_date: parse_date(&row.end_date, "end_date")?,
            status: parse_batch_status(&row.status)?,
            note: row.note,
            created_at: parse_datetime(&row.created_at, "created_at")?,
            updated_at: parse_datetime(&row.updated_at, "updated_at")?,
            last_exported_at: parse_optional_datetime(row.last_exported_at, "last_exported_at")?,
        })
    }
}

fn map_batch_summary_query_error(error: sqlx::Error) -> AppError {
    if error
        .as_database_error()
        .is_some_and(|database| database.message().contains("integer overflow"))
    {
        amount_total_out_of_range("totalAmountCents")
    } else {
        internal_error("failed to list batch page", error)
    }
}

fn batch_status_str(value: BatchStatus) -> &'static str {
    match value {
        BatchStatus::Draft => "draft",
        BatchStatus::Exported => "exported",
    }
}

fn parse_batch_status(value: &str) -> Result<BatchStatus, AppError> {
    match value {
        "draft" => Ok(BatchStatus::Draft),
        "exported" => Ok(BatchStatus::Exported),
        _ => Err(internal_error("invalid batch status", value)),
    }
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(value).map_err(|error| internal_error(&format!("invalid {field}"), error))
}

fn parse_date(value: &str, field: &str) -> Result<NaiveDate, AppError> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|error| internal_error(&format!("invalid {field}"), error))
}

fn parse_datetime(value: &str, field: &str) -> Result<DateTime<Utc>, AppError> {
    DateTime::parse_from_rfc3339(value)
        .map(|datetime| datetime.with_timezone(&Utc))
        .map_err(|error| internal_error(&format!("invalid {field}"), error))
}

fn parse_optional_datetime(
    value: Option<String>,
    field: &str,
) -> Result<Option<DateTime<Utc>>, AppError> {
    value.map(|value| parse_datetime(&value, field)).transpose()
}

fn batch_not_found(id: Uuid) -> AppError {
    AppError::NotFound {
        entity: "batch".to_owned(),
        message: format!("batch {id} was not found"),
    }
}

fn map_create_error(error: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(database_error) = &error
        && (database_error.is_unique_violation() || database_error.is_check_violation())
    {
        return AppError::Conflict {
            message: "this batch conflicts with an existing record".to_owned(),
        };
    }

    internal_error("failed to create batch", error)
}

fn internal_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Internal {
        message: format!("{context}: {error}"),
    }
}

fn stable_internal_error(message: &str) -> AppError {
    AppError::Internal {
        message: message.to_owned(),
    }
}
