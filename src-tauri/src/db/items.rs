use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool};
use uuid::Uuid;

use crate::domain::error::AppError;
use crate::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, ItemStatus, RecognitionStatus, SourceType,
    derive_item_status,
};

const ITEM_COLUMNS: &str = "id, original_name, original_path, normalized_pdf_path, sha256, \
    mime_type, source_type, source_account_id, source_mailbox, source_uid, source_message_id, \
    source_part_id, fetched_at, invoice_date, suggested_period, batch_id, suggested_category, \
    final_category, amount_cents, currency, city, company, recognition_status, \
    confirmation_status, dedupe_status, duplicate_of_id, note, event_tag, project_tag, \
    created_at, updated_at";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceItem {
    pub id: Uuid,
    pub original_name: String,
    pub original_path: String,
    pub normalized_pdf_path: Option<String>,
    pub sha256: String,
    pub mime_type: String,
    pub source_type: SourceType,
    pub source_account_id: Option<Uuid>,
    pub source_mailbox: Option<String>,
    pub source_uid: Option<i64>,
    pub source_message_id: Option<String>,
    pub source_part_id: Option<String>,
    pub fetched_at: DateTime<Utc>,
    pub invoice_date: Option<NaiveDate>,
    pub suggested_period: Option<String>,
    pub batch_id: Option<Uuid>,
    pub suggested_category: Option<Category>,
    pub final_category: Option<Category>,
    pub amount_cents: Option<i64>,
    pub currency: String,
    pub city: Option<String>,
    pub company: Option<String>,
    pub recognition_status: RecognitionStatus,
    pub confirmation_status: ConfirmationStatus,
    pub dedupe_status: DedupeStatus,
    pub duplicate_of_id: Option<Uuid>,
    pub note: Option<String>,
    pub event_tag: Option<String>,
    pub project_tag: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl InvoiceItem {
    pub fn status(&self) -> ItemStatus {
        derive_item_status(
            self.recognition_status,
            self.confirmation_status,
            self.dedupe_status,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewItemRecord {
    pub id: Uuid,
    pub original_name: String,
    pub original_path: String,
    pub normalized_pdf_path: Option<String>,
    pub sha256: String,
    pub mime_type: String,
    pub source_type: SourceType,
    pub source_account_id: Option<Uuid>,
    pub source_mailbox: Option<String>,
    pub source_uid: Option<i64>,
    pub source_message_id: Option<String>,
    pub source_part_id: Option<String>,
    pub fetched_at: DateTime<Utc>,
    pub invoice_date: Option<NaiveDate>,
    pub suggested_period: Option<String>,
    pub batch_id: Option<Uuid>,
    pub suggested_category: Option<Category>,
    pub final_category: Option<Category>,
    pub amount_cents: Option<i64>,
    pub currency: String,
    pub city: Option<String>,
    pub company: Option<String>,
    pub recognition_status: RecognitionStatus,
    pub confirmation_status: ConfirmationStatus,
    pub dedupe_status: DedupeStatus,
    pub duplicate_of_id: Option<Uuid>,
    pub note: Option<String>,
    pub event_tag: Option<String>,
    pub project_tag: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemFilter {
    pub status: Option<ItemStatus>,
    pub suggested_period: Option<String>,
    pub category: Option<Category>,
    pub source_type: Option<SourceType>,
    pub batch_id: Option<Uuid>,
    pub query: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemPatch {
    pub normalized_pdf_path: Option<Option<String>>,
    pub invoice_date: Option<Option<NaiveDate>>,
    pub suggested_period: Option<Option<String>>,
    pub batch_id: Option<Option<Uuid>>,
    pub suggested_category: Option<Option<Category>>,
    pub final_category: Option<Option<Category>>,
    pub amount_cents: Option<Option<i64>>,
    pub currency: Option<String>,
    pub city: Option<Option<String>>,
    pub company: Option<Option<String>>,
    pub recognition_status: Option<RecognitionStatus>,
    pub confirmation_status: Option<ConfirmationStatus>,
    pub dedupe_status: Option<DedupeStatus>,
    pub duplicate_of_id: Option<Option<Uuid>>,
    pub note: Option<Option<String>>,
    pub event_tag: Option<Option<String>>,
    pub project_tag: Option<Option<String>>,
}

#[derive(Clone)]
pub struct ItemRepository {
    pool: SqlitePool,
}

impl ItemRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, item: &NewItemRecord) -> Result<InvoiceItem, AppError> {
        validate_amount(item.amount_cents)?;
        validate_source(item)?;

        sqlx::query(
            "INSERT INTO items (\
                id, original_name, original_path, normalized_pdf_path, sha256, mime_type, \
                source_type, source_account_id, source_mailbox, source_uid, source_message_id, \
                source_part_id, fetched_at, invoice_date, suggested_period, batch_id, \
                suggested_category, final_category, amount_cents, currency, city, company, \
                recognition_status, confirmation_status, dedupe_status, duplicate_of_id, note, \
                event_tag, project_tag, created_at, updated_at\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
                ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(item.id.to_string())
        .bind(&item.original_name)
        .bind(&item.original_path)
        .bind(&item.normalized_pdf_path)
        .bind(&item.sha256)
        .bind(&item.mime_type)
        .bind(source_type_str(item.source_type))
        .bind(item.source_account_id.map(|id| id.to_string()))
        .bind(&item.source_mailbox)
        .bind(item.source_uid)
        .bind(&item.source_message_id)
        .bind(&item.source_part_id)
        .bind(item.fetched_at.to_rfc3339())
        .bind(item.invoice_date.map(|date| date.to_string()))
        .bind(&item.suggested_period)
        .bind(item.batch_id.map(|id| id.to_string()))
        .bind(item.suggested_category.map(category_str))
        .bind(item.final_category.map(category_str))
        .bind(item.amount_cents)
        .bind(&item.currency)
        .bind(&item.city)
        .bind(&item.company)
        .bind(recognition_status_str(item.recognition_status))
        .bind(confirmation_status_str(item.confirmation_status))
        .bind(dedupe_status_str(item.dedupe_status))
        .bind(item.duplicate_of_id.map(|id| id.to_string()))
        .bind(&item.note)
        .bind(&item.event_tag)
        .bind(&item.project_tag)
        .bind(item.created_at.to_rfc3339())
        .bind(item.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(map_insert_error)?;

        self.get(item.id).await
    }

    pub async fn find_by_hash(&self, sha256: &str) -> Result<Option<InvoiceItem>, AppError> {
        let query = format!("SELECT {ITEM_COLUMNS} FROM items WHERE sha256 = ? LIMIT 1");
        let row = sqlx::query_as::<_, DbItemRow>(&query)
            .bind(sha256)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| internal_error("failed to find item by hash", error))?;

        row.map(InvoiceItem::try_from).transpose()
    }

    pub async fn list(&self, filter: ItemFilter) -> Result<Vec<InvoiceItem>, AppError> {
        let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
        query.push(ITEM_COLUMNS).push(" FROM items WHERE 1 = 1");

        if let Some(status) = filter.status {
            match status {
                ItemStatus::SuspectedDuplicate => {
                    query.push(" AND dedupe_status = 'suspected_duplicate'");
                }
                ItemStatus::RecognitionFailed => {
                    query.push(
                        " AND dedupe_status != 'suspected_duplicate' \
                         AND recognition_status = 'failed'",
                    );
                }
                ItemStatus::PendingRecognition => {
                    query.push(
                        " AND dedupe_status != 'suspected_duplicate' \
                         AND recognition_status = 'pending'",
                    );
                }
                ItemStatus::PendingConfirmation => {
                    query.push(
                        " AND dedupe_status != 'suspected_duplicate' \
                         AND recognition_status = 'succeeded' \
                         AND confirmation_status = 'pending'",
                    );
                }
                ItemStatus::Ready => {
                    query.push(
                        " AND dedupe_status != 'suspected_duplicate' \
                         AND recognition_status = 'succeeded' \
                         AND confirmation_status = 'confirmed'",
                    );
                }
            }
        }
        if let Some(period) = filter.suggested_period {
            query.push(" AND suggested_period = ").push_bind(period);
        }
        if let Some(category) = filter.category {
            query
                .push(" AND COALESCE(final_category, suggested_category) = ")
                .push_bind(category_str(category));
        }
        if let Some(source_type) = filter.source_type {
            query
                .push(" AND source_type = ")
                .push_bind(source_type_str(source_type));
        }
        if let Some(batch_id) = filter.batch_id {
            query
                .push(" AND batch_id = ")
                .push_bind(batch_id.to_string());
        }
        if let Some(search) = filter.query {
            let pattern = format!("%{search}%");
            query
                .push(" AND (LOWER(original_name) LIKE LOWER(")
                .push_bind(pattern.clone())
                .push(") OR LOWER(company) LIKE LOWER(")
                .push_bind(pattern.clone())
                .push(") OR LOWER(city) LIKE LOWER(")
                .push_bind(pattern.clone())
                .push(") OR LOWER(note) LIKE LOWER(")
                .push_bind(pattern)
                .push("))");
        }

        query.push(" ORDER BY created_at DESC, id DESC");
        let rows = query
            .build_query_as::<DbItemRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(|error| internal_error("failed to list items", error))?;

        rows.into_iter().map(InvoiceItem::try_from).collect()
    }

    pub async fn update_fields(&self, id: Uuid, patch: ItemPatch) -> Result<InvoiceItem, AppError> {
        if let Some(amount_cents) = patch.amount_cents {
            validate_amount(amount_cents)?;
        }

        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| internal_error("failed to begin item update", error))?;
        let select = format!("SELECT {ITEM_COLUMNS} FROM items WHERE id = ?");
        let row = sqlx::query_as::<_, DbItemRow>(&select)
            .bind(id.to_string())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|error| internal_error("failed to get item for update", error))?
            .ok_or_else(|| AppError::NotFound {
                entity: "item".to_owned(),
                message: format!("item {id} was not found"),
            })?;
        let mut item = InvoiceItem::try_from(row)?;

        if let Some(value) = patch.normalized_pdf_path {
            item.normalized_pdf_path = value;
        }
        if let Some(value) = patch.invoice_date {
            item.invoice_date = value;
        }
        if let Some(value) = patch.suggested_period {
            item.suggested_period = value;
        }
        if let Some(value) = patch.batch_id {
            item.batch_id = value;
        }
        if let Some(value) = patch.suggested_category {
            item.suggested_category = value;
        }
        if let Some(value) = patch.final_category {
            item.final_category = value;
        }
        if let Some(value) = patch.amount_cents {
            item.amount_cents = value;
        }
        if let Some(value) = patch.currency {
            item.currency = value;
        }
        if let Some(value) = patch.city {
            item.city = value;
        }
        if let Some(value) = patch.company {
            item.company = value;
        }
        if let Some(value) = patch.recognition_status {
            item.recognition_status = value;
        }
        if let Some(value) = patch.confirmation_status {
            item.confirmation_status = value;
        }
        if let Some(value) = patch.dedupe_status {
            item.dedupe_status = value;
        }
        if let Some(value) = patch.duplicate_of_id {
            item.duplicate_of_id = value;
        }
        if let Some(value) = patch.note {
            item.note = value;
        }
        if let Some(value) = patch.event_tag {
            item.event_tag = value;
        }
        if let Some(value) = patch.project_tag {
            item.project_tag = value;
        }
        validate_amount(item.amount_cents)?;
        item.updated_at = Utc::now();

        sqlx::query(
            "UPDATE items SET \
                normalized_pdf_path = ?, invoice_date = ?, suggested_period = ?, batch_id = ?, \
                suggested_category = ?, final_category = ?, amount_cents = ?, currency = ?, \
                city = ?, company = ?, recognition_status = ?, confirmation_status = ?, \
                dedupe_status = ?, duplicate_of_id = ?, note = ?, event_tag = ?, project_tag = ?, \
                updated_at = ? \
             WHERE id = ?",
        )
        .bind(&item.normalized_pdf_path)
        .bind(item.invoice_date.map(|date| date.to_string()))
        .bind(&item.suggested_period)
        .bind(item.batch_id.map(|value| value.to_string()))
        .bind(item.suggested_category.map(category_str))
        .bind(item.final_category.map(category_str))
        .bind(item.amount_cents)
        .bind(&item.currency)
        .bind(&item.city)
        .bind(&item.company)
        .bind(recognition_status_str(item.recognition_status))
        .bind(confirmation_status_str(item.confirmation_status))
        .bind(dedupe_status_str(item.dedupe_status))
        .bind(item.duplicate_of_id.map(|value| value.to_string()))
        .bind(&item.note)
        .bind(&item.event_tag)
        .bind(&item.project_tag)
        .bind(item.updated_at.to_rfc3339())
        .bind(id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| internal_error("failed to update item", error))?;

        let refreshed_row = sqlx::query_as::<_, DbItemRow>(&select)
            .bind(id.to_string())
            .fetch_one(&mut *transaction)
            .await
            .map_err(|error| internal_error("failed to refresh updated item", error))?;
        let refreshed = InvoiceItem::try_from(refreshed_row)?;
        transaction
            .commit()
            .await
            .map_err(|error| internal_error("failed to commit item update", error))?;

        Ok(refreshed)
    }

    async fn get(&self, id: Uuid) -> Result<InvoiceItem, AppError> {
        let query = format!("SELECT {ITEM_COLUMNS} FROM items WHERE id = ?");
        let row = sqlx::query_as::<_, DbItemRow>(&query)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| internal_error("failed to get item", error))?
            .ok_or_else(|| AppError::NotFound {
                entity: "item".to_owned(),
                message: format!("item {id} was not found"),
            })?;

        InvoiceItem::try_from(row)
    }
}

#[derive(FromRow)]
struct DbItemRow {
    id: String,
    original_name: String,
    original_path: String,
    normalized_pdf_path: Option<String>,
    sha256: String,
    mime_type: String,
    source_type: String,
    source_account_id: Option<String>,
    source_mailbox: Option<String>,
    source_uid: Option<i64>,
    source_message_id: Option<String>,
    source_part_id: Option<String>,
    fetched_at: String,
    invoice_date: Option<String>,
    suggested_period: Option<String>,
    batch_id: Option<String>,
    suggested_category: Option<String>,
    final_category: Option<String>,
    amount_cents: Option<i64>,
    currency: String,
    city: Option<String>,
    company: Option<String>,
    recognition_status: String,
    confirmation_status: String,
    dedupe_status: String,
    duplicate_of_id: Option<String>,
    note: Option<String>,
    event_tag: Option<String>,
    project_tag: Option<String>,
    created_at: String,
    updated_at: String,
}

impl TryFrom<DbItemRow> for InvoiceItem {
    type Error = AppError;

    fn try_from(row: DbItemRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: parse_uuid(&row.id, "id")?,
            original_name: row.original_name,
            original_path: row.original_path,
            normalized_pdf_path: row.normalized_pdf_path,
            sha256: row.sha256,
            mime_type: row.mime_type,
            source_type: parse_source_type(&row.source_type)?,
            source_account_id: parse_optional_uuid(row.source_account_id, "source_account_id")?,
            source_mailbox: row.source_mailbox,
            source_uid: row.source_uid,
            source_message_id: row.source_message_id,
            source_part_id: row.source_part_id,
            fetched_at: parse_datetime(&row.fetched_at, "fetched_at")?,
            invoice_date: parse_optional_date(row.invoice_date, "invoice_date")?,
            suggested_period: row.suggested_period,
            batch_id: parse_optional_uuid(row.batch_id, "batch_id")?,
            suggested_category: parse_optional_category(row.suggested_category)?,
            final_category: parse_optional_category(row.final_category)?,
            amount_cents: row.amount_cents,
            currency: row.currency,
            city: row.city,
            company: row.company,
            recognition_status: parse_recognition_status(&row.recognition_status)?,
            confirmation_status: parse_confirmation_status(&row.confirmation_status)?,
            dedupe_status: parse_dedupe_status(&row.dedupe_status)?,
            duplicate_of_id: parse_optional_uuid(row.duplicate_of_id, "duplicate_of_id")?,
            note: row.note,
            event_tag: row.event_tag,
            project_tag: row.project_tag,
            created_at: parse_datetime(&row.created_at, "created_at")?,
            updated_at: parse_datetime(&row.updated_at, "updated_at")?,
        })
    }
}

fn validate_amount(amount_cents: Option<i64>) -> Result<(), AppError> {
    if amount_cents.is_some_and(|amount| amount < 0) {
        return Err(AppError::validation(
            "amount_cents",
            "amount must be nonnegative",
        ));
    }
    Ok(())
}

fn validate_source(item: &NewItemRecord) -> Result<(), AppError> {
    if item.source_type == SourceType::Email
        && (item.source_account_id.is_none_or(|id| id.is_nil())
            || item
                .source_mailbox
                .as_deref()
                .is_none_or(|mailbox| mailbox.trim().is_empty())
            || item.source_uid.is_none_or(|uid| uid <= 0)
            || item
                .source_part_id
                .as_deref()
                .is_none_or(|part_id| part_id.trim().is_empty()))
    {
        return Err(AppError::validation(
            "source",
            "email source requires an account, mailbox, positive UID, and part ID",
        ));
    }

    Ok(())
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(value).map_err(|error| internal_error(&format!("invalid {field}"), error))
}

fn parse_optional_uuid(value: Option<String>, field: &str) -> Result<Option<Uuid>, AppError> {
    value.map(|value| parse_uuid(&value, field)).transpose()
}

fn parse_datetime(value: &str, field: &str) -> Result<DateTime<Utc>, AppError> {
    DateTime::parse_from_rfc3339(value)
        .map(|datetime| datetime.with_timezone(&Utc))
        .map_err(|error| internal_error(&format!("invalid {field}"), error))
}

fn parse_optional_date(value: Option<String>, field: &str) -> Result<Option<NaiveDate>, AppError> {
    value
        .map(|value| {
            NaiveDate::parse_from_str(&value, "%Y-%m-%d")
                .map_err(|error| internal_error(&format!("invalid {field}"), error))
        })
        .transpose()
}

fn source_type_str(value: SourceType) -> &'static str {
    match value {
        SourceType::Email => "email",
        SourceType::ManualUpload => "manual_upload",
    }
}

fn parse_source_type(value: &str) -> Result<SourceType, AppError> {
    match value {
        "email" => Ok(SourceType::Email),
        "manual_upload" => Ok(SourceType::ManualUpload),
        _ => Err(internal_error("invalid source_type", value)),
    }
}

fn category_str(value: Category) -> &'static str {
    match value {
        Category::Transport => "transport",
        Category::Dining => "dining",
        Category::Accommodation => "accommodation",
        Category::Hospitality => "hospitality",
    }
}

fn parse_optional_category(value: Option<String>) -> Result<Option<Category>, AppError> {
    value
        .map(|value| match value.as_str() {
            "transport" => Ok(Category::Transport),
            "dining" => Ok(Category::Dining),
            "accommodation" => Ok(Category::Accommodation),
            "hospitality" => Ok(Category::Hospitality),
            _ => Err(internal_error("invalid category", value)),
        })
        .transpose()
}

fn recognition_status_str(value: RecognitionStatus) -> &'static str {
    match value {
        RecognitionStatus::Pending => "pending",
        RecognitionStatus::Succeeded => "succeeded",
        RecognitionStatus::Failed => "failed",
    }
}

fn parse_recognition_status(value: &str) -> Result<RecognitionStatus, AppError> {
    match value {
        "pending" => Ok(RecognitionStatus::Pending),
        "succeeded" => Ok(RecognitionStatus::Succeeded),
        "failed" => Ok(RecognitionStatus::Failed),
        _ => Err(internal_error("invalid recognition_status", value)),
    }
}

fn confirmation_status_str(value: ConfirmationStatus) -> &'static str {
    match value {
        ConfirmationStatus::Pending => "pending",
        ConfirmationStatus::Confirmed => "confirmed",
    }
}

fn parse_confirmation_status(value: &str) -> Result<ConfirmationStatus, AppError> {
    match value {
        "pending" => Ok(ConfirmationStatus::Pending),
        "confirmed" => Ok(ConfirmationStatus::Confirmed),
        _ => Err(internal_error("invalid confirmation_status", value)),
    }
}

fn dedupe_status_str(value: DedupeStatus) -> &'static str {
    match value {
        DedupeStatus::Unique => "unique",
        DedupeStatus::SuspectedDuplicate => "suspected_duplicate",
        DedupeStatus::Resolved => "resolved",
    }
}

fn parse_dedupe_status(value: &str) -> Result<DedupeStatus, AppError> {
    match value {
        "unique" => Ok(DedupeStatus::Unique),
        "suspected_duplicate" => Ok(DedupeStatus::SuspectedDuplicate),
        "resolved" => Ok(DedupeStatus::Resolved),
        _ => Err(internal_error("invalid dedupe_status", value)),
    }
}

fn internal_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Internal {
        message: format!("{context}: {error}"),
    }
}

fn map_insert_error(error: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(database_error) = &error
        && database_error.is_unique_violation()
    {
        let details = database_error.message();
        let message = if details.contains("source_account_id")
            && details.contains("source_mailbox")
            && details.contains("source_uid")
            && details.contains("source_part_id")
        {
            "this email attachment was already imported; review the existing invoice item"
        } else if details.contains("original_path") {
            "an invoice item already uses this original path; choose a different file"
        } else if details.contains("items.id") {
            "an invoice item with this ID already exists; generate a new item ID"
        } else {
            "this invoice item conflicts with an existing record; review existing items"
        };

        return AppError::Conflict {
            message: message.to_owned(),
        };
    }

    internal_error("failed to insert item", error)
}
