use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{FromRow, QueryBuilder, Sqlite, SqliteConnection, SqlitePool, Transaction};
use uuid::Uuid;

use crate::domain::amount::{checked_add_amount_cents, validate_optional_amount_cents};
use crate::domain::error::AppError;
use crate::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, ItemStatus, RecognitionStatus, SourceType,
    derive_item_status,
};

const ITEM_COLUMNS: &str = "id, original_name, original_path, normalized_pdf_path, sha256, \
    mime_type, source_type, source_account_id, source_mailbox, source_uid_validity, source_uid, \
    source_message_id, source_part_id, fetched_at, source_received_date, invoice_date, \
    suggested_period, batch_id, suggested_category, final_category, amount_cents, currency, city, \
    company, recognition_status, confirmation_status, dedupe_status, duplicate_of_id, note, \
    event_tag, project_tag, created_at, updated_at";
const MAX_PAGE_SIZE: usize = 200;

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
    pub source_uid_validity: Option<i64>,
    pub source_uid: Option<i64>,
    pub source_message_id: Option<String>,
    pub source_part_id: Option<String>,
    pub fetched_at: DateTime<Utc>,
    pub source_received_date: Option<NaiveDate>,
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

    pub fn batch_membership_date(&self) -> Option<NaiveDate> {
        match self.source_type {
            SourceType::Email => self
                .source_received_date
                .or_else(|| Some(self.fetched_at.date_naive())),
            SourceType::ManualUpload => self.invoice_date,
        }
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
    pub source_uid_validity: Option<i64>,
    pub source_uid: Option<i64>,
    pub source_message_id: Option<String>,
    pub source_part_id: Option<String>,
    pub fetched_at: DateTime<Utc>,
    pub source_received_date: Option<NaiveDate>,
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
    pub created_after: Option<DateTime<Utc>>,
    pub suggested_period: Option<String>,
    pub category: Option<Category>,
    pub source_type: Option<SourceType>,
    pub batch_id: Option<Uuid>,
    pub query: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemPageCursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemPage {
    pub items: Vec<InvoiceItem>,
    pub next_cursor: Option<ItemPageCursor>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemPatch {
    pub normalized_pdf_path: Option<Option<String>>,
    pub invoice_date: Option<Option<NaiveDate>>,
    pub suggested_period: Option<Option<String>>,
    pub batch_id: Option<Option<Uuid>>,
    pub suggested_category: Option<Option<Category>>,
    pub final_category: Option<Option<Category>>,
    pub final_category_if_missing: Option<Category>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EmailFileReplacement {
    pub(crate) original_name: String,
    pub(crate) original_path: String,
    pub(crate) sha256: String,
    pub(crate) mime_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmailFileReplacementGuard {
    LinkPlaceholder,
    FailedAttachment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewedItemFields {
    pub invoice_date: Option<NaiveDate>,
    pub suggested_period: String,
    pub final_category: Category,
    pub amount_cents: i64,
    pub city: Option<String>,
    pub company: Option<String>,
    pub note: Option<String>,
    pub event_tag: Option<String>,
    pub project_tag: Option<String>,
}

#[derive(Clone)]
pub struct ItemRepository {
    pool: SqlitePool,
}

pub(crate) struct DuplicateDiscardClaim {
    transaction: Transaction<'static, Sqlite>,
    item: InvoiceItem,
    protected_paths: Vec<String>,
}

pub(crate) struct FileRecoveryClaim {
    transaction: Transaction<'static, Sqlite>,
    referenced_paths: Vec<String>,
}

impl ItemRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get_by_id(&self, id: Uuid) -> Result<InvoiceItem, AppError> {
        let query = format!("SELECT {ITEM_COLUMNS} FROM items WHERE id = ?");
        let row = sqlx::query_as::<_, DbItemRow>(&query)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| map_database_error("failed to get item", error))?
            .ok_or_else(|| AppError::NotFound {
                entity: "item".to_owned(),
                message: format!("item {id} was not found"),
            })?;
        InvoiceItem::try_from(row)
    }

    pub(crate) async fn find_by_id_with_connection(
        connection: &mut SqliteConnection,
        id: Uuid,
    ) -> Result<Option<InvoiceItem>, AppError> {
        let query = format!("SELECT {ITEM_COLUMNS} FROM items WHERE id = ?");
        sqlx::query_as::<_, DbItemRow>(&query)
            .bind(id.to_string())
            .fetch_optional(&mut *connection)
            .await
            .map_err(|error| map_database_error("failed to get item", error))?
            .map(InvoiceItem::try_from)
            .transpose()
    }

    pub async fn insert(&self, item: &NewItemRecord) -> Result<InvoiceItem, AppError> {
        validate_amount(item.amount_cents)?;
        validate_source(item)?;
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin item transaction", error))?;
        let result = async {
            validate_assigned_batch_exists(&mut transaction, item.batch_id).await?;
            validate_assigned_item_state(item)?;
            insert_with_connection(&mut transaction, item).await?;
            draft_inserted_item_batch(&mut transaction, item.batch_id, Utc::now()).await?;
            get_with_connection(&mut transaction, item.id).await
        }
        .await;
        finish_transaction(transaction, result).await
    }

    pub async fn insert_deduplicated(
        &self,
        mut item: NewItemRecord,
    ) -> Result<InvoiceItem, AppError> {
        validate_amount(item.amount_cents)?;
        validate_source(&item)?;
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin item transaction", error))?;
        let result = async {
            validate_assigned_batch_exists(&mut transaction, item.batch_id).await?;
            let canonical_id =
                find_canonical_id_with_connection(&mut transaction, &item.sha256).await?;
            match canonical_id {
                Some(canonical_id) => {
                    item.dedupe_status = DedupeStatus::SuspectedDuplicate;
                    item.duplicate_of_id = Some(canonical_id);
                }
                None => {
                    item.dedupe_status = DedupeStatus::Unique;
                    item.duplicate_of_id = None;
                }
            }
            validate_assigned_item_state(&item)?;
            insert_with_connection(&mut transaction, &item).await?;
            draft_inserted_item_batch(&mut transaction, item.batch_id, Utc::now()).await?;
            get_with_connection(&mut transaction, item.id).await
        }
        .await;
        finish_transaction(transaction, result).await
    }

    pub async fn find_by_hash(&self, sha256: &str) -> Result<Option<InvoiceItem>, AppError> {
        let query = format!(
            "SELECT {ITEM_COLUMNS} FROM items \
             WHERE sha256 = ? ORDER BY created_at ASC, id ASC LIMIT 1"
        );
        let row = sqlx::query_as::<_, DbItemRow>(&query)
            .bind(sha256)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| internal_error("failed to find item by hash", error))?;

        row.map(InvoiceItem::try_from).transpose()
    }

    pub async fn find_email_part(
        &self,
        account_id: Uuid,
        mailbox: &str,
        uid_validity: u32,
        uid: u32,
        part_id: &str,
    ) -> Result<Option<InvoiceItem>, AppError> {
        let query = format!(
            "SELECT {ITEM_COLUMNS} FROM items WHERE source_type = 'email' \
             AND source_account_id = ? AND source_mailbox = ? AND source_uid_validity = ? \
             AND source_uid = ? AND source_part_id = ? LIMIT 1"
        );
        let row = sqlx::query_as::<_, DbItemRow>(&query)
            .bind(account_id.to_string())
            .bind(mailbox)
            .bind(i64::from(uid_validity))
            .bind(i64::from(uid))
            .bind(part_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| map_database_error("failed to find email part", error))?;
        row.map(InvoiceItem::try_from).transpose()
    }

    pub(crate) async fn replace_email_link_placeholder(
        &self,
        id: Uuid,
        replacement: EmailFileReplacement,
    ) -> Result<Option<InvoiceItem>, AppError> {
        self.replace_email_file(id, replacement, EmailFileReplacementGuard::LinkPlaceholder)
            .await
    }

    pub(crate) async fn replace_failed_email_attachment(
        &self,
        id: Uuid,
        replacement: EmailFileReplacement,
    ) -> Result<Option<InvoiceItem>, AppError> {
        self.replace_email_file(id, replacement, EmailFileReplacementGuard::FailedAttachment)
            .await
    }

    async fn replace_email_file(
        &self,
        id: Uuid,
        replacement: EmailFileReplacement,
        guard: EmailFileReplacementGuard,
    ) -> Result<Option<InvoiceItem>, AppError> {
        if replacement.original_name.trim().is_empty()
            || replacement.original_path.trim().is_empty()
            || replacement.sha256.trim().is_empty()
            || replacement.mime_type.trim().is_empty()
        {
            return Err(AppError::Internal {
                message: "email file replacement metadata is incomplete".to_owned(),
            });
        }
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin email file replacement", error))?;
        let result = async {
            let item = get_with_connection(&mut transaction, id).await?;
            let replaceable = match guard {
                EmailFileReplacementGuard::LinkPlaceholder => is_replaceable_email_link(&item),
                EmailFileReplacementGuard::FailedAttachment => {
                    is_replaceable_failed_email_attachment(&item)
                }
            };
            if !replaceable {
                return Ok(None);
            }
            let canonical_id = sqlx::query_scalar::<_, String>(
                "SELECT id FROM items \
                 WHERE id != ? AND sha256 = ? AND duplicate_of_id IS NULL \
                 ORDER BY CASE WHEN dedupe_status = 'unique' THEN 0 ELSE 1 END, \
                          created_at ASC, id ASC LIMIT 1",
            )
            .bind(id.to_string())
            .bind(&replacement.sha256)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to deduplicate email replacement", error))?
            .map(|value| parse_uuid(&value, "id"))
            .transpose()?;
            let (dedupe_status, duplicate_of_id) = match canonical_id {
                Some(canonical_id) => ("suspected_duplicate", Some(canonical_id)),
                None => ("unique", None),
            };
            let updated_at = Utc::now();
            sqlx::query(
                "UPDATE items SET \
                    original_name = ?, original_path = ?, normalized_pdf_path = NULL, \
                    sha256 = ?, mime_type = ?, invoice_date = NULL, suggested_period = NULL, \
                    suggested_category = NULL, final_category = NULL, amount_cents = NULL, \
                    city = NULL, company = NULL, recognition_status = 'pending', \
                    confirmation_status = 'pending', dedupe_status = ?, duplicate_of_id = ?, \
                    note = NULL, event_tag = NULL, project_tag = NULL, updated_at = ? \
                 WHERE id = ?",
            )
            .bind(replacement.original_name)
            .bind(replacement.original_path)
            .bind(replacement.sha256)
            .bind(replacement.mime_type)
            .bind(dedupe_status)
            .bind(duplicate_of_id.map(|value| value.to_string()))
            .bind(updated_at.to_rfc3339())
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to replace email item", error))?;
            get_with_connection(&mut transaction, id).await.map(Some)
        }
        .await;
        finish_transaction(transaction, result).await
    }

    pub(crate) async fn mark_email_link_download_failed(
        &self,
        id: Uuid,
    ) -> Result<InvoiceItem, AppError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin link failure update", error))?;
        let result = async {
            let item = get_with_connection(&mut transaction, id).await?;
            if !is_replaceable_email_link(&item) {
                return Ok(item);
            }
            sqlx::query(
                "UPDATE items SET recognition_status = 'failed', note = ?, updated_at = ? \
                 WHERE id = ?",
            )
            .bind("invoice link download failed")
            .bind(Utc::now().to_rfc3339())
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                map_database_error("failed to mark email link download failed", error)
            })?;
            get_with_connection(&mut transaction, id).await
        }
        .await;
        finish_transaction(transaction, result).await
    }

    pub(crate) async fn delete_email_link_placeholder(
        &self,
        account_id: Uuid,
        mailbox: &str,
        uid_validity: u32,
        uid: u32,
        part_id: &str,
    ) -> Result<Option<InvoiceItem>, AppError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin link discard", error))?;
        let result = async {
            let query = format!(
                "SELECT {ITEM_COLUMNS} FROM items WHERE source_type = 'email' \
                 AND source_account_id = ? AND source_mailbox = ? AND source_uid_validity = ? \
                 AND source_uid = ? AND source_part_id = ? LIMIT 1"
            );
            let item = sqlx::query_as::<_, DbItemRow>(&query)
                .bind(account_id.to_string())
                .bind(mailbox)
                .bind(i64::from(uid_validity))
                .bind(i64::from(uid))
                .bind(part_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|error| map_database_error("failed to find ignored email link", error))?
                .map(InvoiceItem::try_from)
                .transpose()?;
            let Some(item) = item.filter(is_replaceable_email_link) else {
                return Ok(None);
            };
            sqlx::query("DELETE FROM items WHERE id = ?")
                .bind(item.id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(|error| {
                    map_database_error("failed to discard ignored email link", error)
                })?;
            Ok(Some(item))
        }
        .await;
        finish_transaction(transaction, result).await
    }

    pub async fn find_rescanned_email_part(
        &self,
        account_id: Uuid,
        mailbox: &str,
        current_uid_validity: u32,
        message_id: Option<&str>,
        part_id: &str,
        sha256: &str,
    ) -> Result<Option<InvoiceItem>, AppError> {
        let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
        query
            .push(ITEM_COLUMNS)
            .push(" FROM items WHERE source_type = 'email' AND source_account_id = ");
        query
            .push_bind(account_id.to_string())
            .push(" AND source_mailbox = ")
            .push_bind(mailbox)
            .push(" AND source_uid_validity > 0 AND source_uid_validity != ")
            .push_bind(i64::from(current_uid_validity))
            .push(" AND source_part_id = ")
            .push_bind(part_id)
            .push(" AND sha256 = ")
            .push_bind(sha256);
        if let Some(message_id) = message_id {
            query
                .push(" AND source_message_id = ")
                .push_bind(message_id);
        }
        query.push(" ORDER BY created_at ASC, id ASC LIMIT 1");

        let row = query
            .build_query_as::<DbItemRow>()
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| map_database_error("failed to find rescanned email part", error))?;
        row.map(InvoiceItem::try_from).transpose()
    }

    pub async fn find_legacy_email_part(
        &self,
        account_id: Uuid,
        mailbox: &str,
        message_id: Option<&str>,
        part_id: &str,
        sha256: &str,
    ) -> Result<Option<InvoiceItem>, AppError> {
        let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
        query
            .push(ITEM_COLUMNS)
            .push(" FROM items WHERE source_type = 'email' AND source_account_id = ");
        query
            .push_bind(account_id.to_string())
            .push(" AND source_mailbox = ")
            .push_bind(mailbox)
            .push(" AND source_uid_validity = 0 AND source_part_id = ")
            .push_bind(part_id)
            .push(" AND sha256 = ")
            .push_bind(sha256);
        if let Some(message_id) = message_id {
            query
                .push(" AND source_message_id = ")
                .push_bind(message_id);
        }
        query.push(" ORDER BY created_at ASC, id ASC LIMIT 1");

        let row = query
            .build_query_as::<DbItemRow>()
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| map_database_error("failed to find legacy email part", error))?;
        row.map(InvoiceItem::try_from).transpose()
    }

    #[doc(hidden)]
    pub async fn list_bounded_for_tests(
        &self,
        filter: ItemFilter,
    ) -> Result<Vec<InvoiceItem>, AppError> {
        self.list_page(filter, None, MAX_PAGE_SIZE)
            .await
            .map(|page| page.items)
    }

    pub async fn list_page(
        &self,
        filter: ItemFilter,
        cursor: Option<ItemPageCursor>,
        page_size: usize,
    ) -> Result<ItemPage, AppError> {
        validate_page_size(page_size)?;
        let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
        query.push(ITEM_COLUMNS).push(" FROM items WHERE 1 = 1");
        push_item_filters(&mut query, filter);
        if let Some(cursor) = cursor {
            let created_at = cursor.created_at.to_rfc3339();
            query
                .push(" AND (created_at < ")
                .push_bind(created_at.clone())
                .push(" OR (created_at = ")
                .push_bind(created_at)
                .push(" AND id < ")
                .push_bind(cursor.id.to_string())
                .push("))");
        }
        let fetch_limit = page_size.checked_add(1).ok_or_else(|| AppError::Internal {
            message: "item page size overflow".to_owned(),
        })?;
        query
            .push(" ORDER BY created_at DESC, id DESC LIMIT ")
            .push_bind(
                i64::try_from(fetch_limit)
                    .map_err(|_| AppError::validation("pageSize", "page size is too large"))?,
            );
        let rows = query
            .build_query_as::<DbItemRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(|error| internal_error("failed to list item page", error))?;
        let has_next = rows.len() > page_size;
        let mut rows = rows;
        rows.truncate(page_size);
        let items = rows
            .into_iter()
            .map(InvoiceItem::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = has_next.then(|| {
            let last = items.last().expect("nonempty page must have a cursor row");
            ItemPageCursor {
                created_at: last.created_at,
                id: last.id,
            }
        });
        Ok(ItemPage { items, next_cursor })
    }

    pub async fn recommend_unassigned(
        &self,
        start_period: &str,
        end_period: &str,
    ) -> Result<Vec<InvoiceItem>, AppError> {
        let query = format!(
            "SELECT {ITEM_COLUMNS} FROM items \
             WHERE batch_id IS NULL \
               AND suggested_period BETWEEN ? AND ? \
             ORDER BY suggested_period ASC, \
                      CASE WHEN invoice_date IS NULL THEN 1 ELSE 0 END ASC, \
                      invoice_date ASC, created_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, DbItemRow>(&query)
            .bind(start_period)
            .bind(end_period)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| map_database_error("failed to recommend items", error))?;

        rows.into_iter().map(InvoiceItem::try_from).collect()
    }

    pub async fn list_batch_candidates(
        &self,
        start_date: NaiveDate,
        end_date: NaiveDate,
        search: Option<String>,
        cursor: Option<ItemPageCursor>,
        page_size: usize,
    ) -> Result<ItemPage, AppError> {
        validate_page_size(page_size)?;
        let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
        query
            .push(ITEM_COLUMNS)
            .push(" FROM items WHERE batch_id IS NULL");
        if let Some(search) = search {
            push_item_search_filter(&mut query, search);
        } else {
            query
                .push(" AND ((source_type = 'email' AND COALESCE(source_received_date, substr(fetched_at, 1, 10)) >= ")
                .push_bind(start_date.to_string())
                .push(" AND COALESCE(source_received_date, substr(fetched_at, 1, 10)) <= ")
                .push_bind(end_date.to_string())
                .push(") OR (source_type = 'manual_upload' AND invoice_date >= ")
                .push_bind(start_date.to_string())
                .push(" AND invoice_date <= ")
                .push_bind(end_date.to_string())
                .push("))");
        }
        if let Some(cursor) = cursor {
            let created_at = cursor.created_at.to_rfc3339();
            query
                .push(" AND (created_at < ")
                .push_bind(created_at.clone())
                .push(" OR (created_at = ")
                .push_bind(created_at)
                .push(" AND id < ")
                .push_bind(cursor.id.to_string())
                .push("))");
        }
        let fetch_limit = page_size.checked_add(1).ok_or_else(|| AppError::Internal {
            message: "batch candidate page size overflow".to_owned(),
        })?;
        query
            .push(" ORDER BY created_at DESC, id DESC LIMIT ")
            .push_bind(
                i64::try_from(fetch_limit)
                    .map_err(|_| AppError::validation("pageSize", "page size is too large"))?,
            );
        let rows = query
            .build_query_as::<DbItemRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(|error| internal_error("failed to list batch candidates", error))?;
        let has_next = rows.len() > page_size;
        let mut rows = rows;
        rows.truncate(page_size);
        let items = rows
            .into_iter()
            .map(InvoiceItem::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = has_next.then(|| {
            let last = items
                .last()
                .expect("nonempty batch candidate page must have a cursor row");
            ItemPageCursor {
                created_at: last.created_at,
                id: last.id,
            }
        });
        Ok(ItemPage { items, next_cursor })
    }

    pub(crate) async fn list_by_ids(&self, ids: &[Uuid]) -> Result<Vec<InvoiceItem>, AppError> {
        let mut items = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(MAX_PAGE_SIZE) {
            let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
            query.push(ITEM_COLUMNS).push(" FROM items WHERE id IN (");
            {
                let mut separated = query.separated(", ");
                for id in chunk {
                    separated.push_bind(id.to_string());
                }
            }
            query.push(")");
            let rows = query
                .build_query_as::<DbItemRow>()
                .fetch_all(&self.pool)
                .await
                .map_err(|error| internal_error("failed to list items by id", error))?;
            items.extend(
                rows.into_iter()
                    .map(InvoiceItem::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        Ok(items)
    }

    pub(crate) async fn list_by_batch_with_connection(
        connection: &mut SqliteConnection,
        batch_id: Uuid,
    ) -> Result<Vec<InvoiceItem>, AppError> {
        let query = format!(
            "SELECT {ITEM_COLUMNS} FROM items WHERE batch_id = ? \
             ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, DbItemRow>(&query)
            .bind(batch_id.to_string())
            .fetch_all(&mut *connection)
            .await
            .map_err(|error| map_database_error("failed to list batch items", error))?;
        rows.into_iter().map(InvoiceItem::try_from).collect()
    }

    pub async fn update_fields(&self, id: Uuid, patch: ItemPatch) -> Result<InvoiceItem, AppError> {
        self.update_fields_internal(id, patch, false).await
    }

    pub(crate) async fn update_recognition_fields(
        &self,
        id: Uuid,
        patch: ItemPatch,
    ) -> Result<InvoiceItem, AppError> {
        self.update_fields_internal(id, patch, true).await
    }

    pub(crate) async fn record_semantic_identity(
        &self,
        id: Uuid,
        fingerprint: &str,
        expected_updated_at: DateTime<Utc>,
    ) -> Result<InvoiceItem, AppError> {
        if fingerprint.trim().is_empty() {
            return Err(AppError::Internal {
                message: "semantic invoice fingerprint must not be blank".to_owned(),
            });
        }

        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin semantic dedupe", error))?;
        let result = async {
            let item = get_with_connection(&mut transaction, id).await?;
            if item.updated_at != expected_updated_at
                || item.batch_id.is_some()
                || item.dedupe_status != DedupeStatus::Unique
            {
                return Ok(item);
            }

            let now = Utc::now();
            sqlx::query(
                "INSERT INTO item_semantic_identities (item_id, fingerprint, created_at, updated_at) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT(item_id) DO UPDATE SET \
                    fingerprint = excluded.fingerprint, \
                    created_at = CASE \
                        WHEN item_semantic_identities.fingerprint = excluded.fingerprint \
                        THEN item_semantic_identities.created_at \
                        ELSE excluded.created_at \
                    END, \
                    updated_at = excluded.updated_at",
            )
            .bind(id.to_string())
            .bind(fingerprint)
            .bind(now.to_rfc3339())
            .bind(now.to_rfc3339())
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to save semantic identity", error))?;

            let canonical_id = sqlx::query_scalar::<_, String>(
                "SELECT identity.item_id \
                 FROM item_semantic_identities identity \
                 JOIN items item ON item.id = identity.item_id \
                 WHERE identity.fingerprint = ? AND identity.item_id != ? \
                   AND item.duplicate_of_id IS NULL \
                 ORDER BY identity.created_at ASC, identity.item_id ASC \
                 LIMIT 1",
            )
            .bind(fingerprint)
            .bind(id.to_string())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to find semantic duplicate", error))?
            .map(|value| parse_uuid(&value, "item_id"))
            .transpose()?;

            let Some(canonical_id) = canonical_id else {
                return Ok(item);
            };
            sqlx::query(
                "UPDATE items SET dedupe_status = 'suspected_duplicate', duplicate_of_id = ?, \
                    updated_at = ? \
                 WHERE id = ? AND batch_id IS NULL AND dedupe_status = 'unique' AND updated_at = ?",
            )
            .bind(canonical_id.to_string())
            .bind(now.to_rfc3339())
            .bind(id.to_string())
            .bind(expected_updated_at.to_rfc3339())
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to mark semantic duplicate", error))?;

            get_with_connection(&mut transaction, id).await
        }
        .await;
        finish_transaction(transaction, result).await
    }

    async fn update_fields_internal(
        &self,
        id: Uuid,
        patch: ItemPatch,
        preserve_manual_confirmation: bool,
    ) -> Result<InvoiceItem, AppError> {
        if let Some(amount_cents) = patch.amount_cents {
            validate_amount(amount_cents)?;
        }

        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin item update", error))?;
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
        let original = item.clone();

        if preserve_manual_confirmation
            && item.confirmation_status == ConfirmationStatus::Confirmed
            && item.final_category.is_some()
        {
            if item.normalized_pdf_path.is_none()
                && let Some(normalized_pdf_path) = patch
                    .normalized_pdf_path
                    .as_ref()
                    .and_then(|value| value.as_ref())
            {
                let updated_at = Utc::now();
                let update = sqlx::query(
                    "UPDATE items SET normalized_pdf_path = ?, updated_at = ? \
                     WHERE id = ? AND normalized_pdf_path IS NULL",
                )
                .bind(normalized_pdf_path)
                .bind(updated_at.to_rfc3339())
                .bind(id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(|error| internal_error("failed to update normalized PDF path", error))?;
                if update.rows_affected() != 0 {
                    reset_affected_batches(
                        &mut transaction,
                        item.batch_id,
                        item.batch_id,
                        updated_at,
                    )
                    .await?;
                }
                let refreshed_row = sqlx::query_as::<_, DbItemRow>(&select)
                    .bind(id.to_string())
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(|error| {
                        internal_error("failed to refresh guarded item update", error)
                    })?;
                item = InvoiceItem::try_from(refreshed_row)?;
            }
            transaction
                .commit()
                .await
                .map_err(|error| internal_error("failed to finish guarded item update", error))?;
            return Ok(item);
        }

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
        } else if item.final_category.is_none()
            && let Some(value) = patch.final_category_if_missing
        {
            item.final_category = Some(value);
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
        if item == original {
            transaction.commit().await.map_err(|error| {
                map_database_error("failed to finish unchanged item update", error)
            })?;
            return Ok(item);
        }
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

        reset_affected_batches(
            &mut transaction,
            original.batch_id,
            item.batch_id,
            item.updated_at,
        )
        .await?;

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

    pub(crate) async fn review_item(
        &self,
        id: Uuid,
        fields: ReviewedItemFields,
    ) -> Result<InvoiceItem, AppError> {
        validate_amount(Some(fields.amount_cents))?;
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin item review", error))?;
        let result = async {
            let item = get_with_connection(&mut transaction, id).await?;
            if item.dedupe_status == DedupeStatus::SuspectedDuplicate {
                return Err(AppError::Conflict {
                    message: "resolve the suspected duplicate before reviewing it".to_owned(),
                });
            }

            let export_fields_changed = item.invoice_date != fields.invoice_date
                || item.suggested_period.as_deref() != Some(fields.suggested_period.as_str())
                || item.final_category != Some(fields.final_category)
                || item.amount_cents != Some(fields.amount_cents)
                || item.city != fields.city
                || item.company != fields.company
                || item.note != fields.note
                || item.event_tag != fields.event_tag
                || item.project_tag != fields.project_tag
                || item.recognition_status != RecognitionStatus::Succeeded
                || item.confirmation_status != ConfirmationStatus::Confirmed;
            let updated_at = Utc::now();

            sqlx::query(
                "UPDATE items SET \
                    invoice_date = ?, suggested_period = ?, final_category = ?, \
                    amount_cents = ?, city = ?, company = ?, note = ?, event_tag = ?, \
                    project_tag = ?, recognition_status = 'succeeded', \
                    confirmation_status = 'confirmed', updated_at = ? \
                 WHERE id = ?",
            )
            .bind(fields.invoice_date.map(|date| date.to_string()))
            .bind(fields.suggested_period)
            .bind(category_str(fields.final_category))
            .bind(fields.amount_cents)
            .bind(fields.city)
            .bind(fields.company)
            .bind(fields.note)
            .bind(fields.event_tag)
            .bind(fields.project_tag)
            .bind(updated_at.to_rfc3339())
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to review item", error))?;

            if export_fields_changed {
                reset_affected_batches(&mut transaction, item.batch_id, item.batch_id, updated_at)
                    .await?;
            }

            get_with_connection(&mut transaction, id).await
        }
        .await;

        finish_transaction(transaction, result).await
    }

    pub(crate) async fn keep_suspected_duplicate(&self, id: Uuid) -> Result<InvoiceItem, AppError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin duplicate resolution", error))?;
        let result = async {
            let item = get_with_connection(&mut transaction, id).await?;
            if item.dedupe_status != DedupeStatus::SuspectedDuplicate {
                return Err(AppError::Conflict {
                    message: "item is not awaiting duplicate resolution".to_owned(),
                });
            }

            sqlx::query(
                "UPDATE items SET dedupe_status = 'resolved', duplicate_of_id = NULL, \
                    updated_at = ? WHERE id = ?",
            )
            .bind(Utc::now().to_rfc3339())
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to keep duplicate item", error))?;
            get_with_connection(&mut transaction, id).await
        }
        .await;

        finish_transaction(transaction, result).await
    }

    pub(crate) async fn claim_duplicate_discard(
        &self,
        id: Uuid,
    ) -> Result<DuplicateDiscardClaim, AppError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to claim duplicate deletion", error))?;
        let result = async {
            let item = get_with_connection(&mut transaction, id).await?;
            if item.dedupe_status != DedupeStatus::SuspectedDuplicate {
                return Err(AppError::Conflict {
                    message: "item is not awaiting duplicate resolution".to_owned(),
                });
            }
            let rows = sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT original_path, normalized_pdf_path FROM items WHERE id != ?",
            )
            .bind(id.to_string())
            .fetch_all(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to load protected item paths", error))?;
            let protected_paths = rows
                .into_iter()
                .flat_map(|(original, normalized)| [Some(original), normalized])
                .flatten()
                .collect();
            Ok((item, protected_paths))
        }
        .await;

        match result {
            Ok((item, protected_paths)) => Ok(DuplicateDiscardClaim {
                transaction,
                item,
                protected_paths,
            }),
            Err(error) => match transaction.rollback().await {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(AppError::Internal {
                    message: format!(
                        "duplicate deletion claim failed and rollback also failed: original error: \
                         {error}; rollback error: {rollback_error}"
                    ),
                }),
            },
        }
    }

    pub(crate) async fn claim_file_recovery(&self) -> Result<FileRecoveryClaim, AppError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to claim file recovery", error))?;
        let result = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT original_path, normalized_pdf_path FROM items",
        )
        .fetch_all(&mut *transaction)
        .await
        .map(|rows| {
            rows.into_iter()
                .flat_map(|(original, normalized)| [Some(original), normalized])
                .flatten()
                .collect()
        })
        .map_err(|error| map_database_error("failed to load file recovery references", error));

        match result {
            Ok(referenced_paths) => Ok(FileRecoveryClaim {
                transaction,
                referenced_paths,
            }),
            Err(error) => match transaction.rollback().await {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(AppError::Internal {
                    message: format!(
                        "file recovery claim failed and rollback also failed: original error: \
                         {error}; rollback error: {rollback_error}"
                    ),
                }),
            },
        }
    }
}

fn is_replaceable_email_link(item: &InvoiceItem) -> bool {
    item.source_type == SourceType::Email
        && item.mime_type == "text/uri-list"
        && item
            .original_name
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("url"))
        && item.confirmation_status == ConfirmationStatus::Pending
        && item.batch_id.is_none()
}

fn is_replaceable_failed_email_attachment(item: &InvoiceItem) -> bool {
    item.source_type == SourceType::Email
        && item.mime_type != "text/uri-list"
        && item.recognition_status == RecognitionStatus::Failed
        && item.confirmation_status == ConfirmationStatus::Pending
        && item.batch_id.is_none()
}

fn validate_page_size(page_size: usize) -> Result<(), AppError> {
    if page_size == 0 || page_size > MAX_PAGE_SIZE {
        return Err(AppError::validation(
            "pageSize",
            format!("page size must be between 1 and {MAX_PAGE_SIZE}"),
        ));
    }
    Ok(())
}

fn push_item_filters(query: &mut QueryBuilder<'_, Sqlite>, filter: ItemFilter) {
    if let Some(created_after) = filter.created_after {
        query
            .push(" AND created_at >= ")
            .push_bind(created_after.to_rfc3339());
    }
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
        push_item_search_filter(query, search);
    }
}

fn push_item_search_filter(query: &mut QueryBuilder<'_, Sqlite>, search: String) {
    let mut escaped = String::with_capacity(search.len());
    for character in search.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    let pattern = format!("%{escaped}%");
    query
        .push(" AND (LOWER(original_name) LIKE LOWER(")
        .push_bind(pattern.clone())
        .push(") ESCAPE '\\' OR LOWER(company) LIKE LOWER(")
        .push_bind(pattern.clone())
        .push(") ESCAPE '\\' OR LOWER(city) LIKE LOWER(")
        .push_bind(pattern.clone())
        .push(") ESCAPE '\\' OR LOWER(note) LIKE LOWER(")
        .push_bind(pattern)
        .push(") ESCAPE '\\')");
}

fn validate_assigned_item_state(item: &NewItemRecord) -> Result<(), AppError> {
    if item.batch_id.is_none() {
        return Ok(());
    }
    if item.dedupe_status == DedupeStatus::SuspectedDuplicate {
        return Err(AppError::Conflict {
            message: format!(
                "item {} is a suspected duplicate and cannot be assigned",
                item.id
            ),
        });
    }
    if item.recognition_status == RecognitionStatus::Failed {
        return Err(AppError::Conflict {
            message: format!(
                "item {} has failed recognition and cannot be assigned",
                item.id
            ),
        });
    }
    Ok(())
}

async fn validate_assigned_batch_exists(
    connection: &mut SqliteConnection,
    batch_id: Option<Uuid>,
) -> Result<(), AppError> {
    if let Some(batch_id) = batch_id {
        let exists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM batches WHERE id = ?")
            .bind(batch_id.to_string())
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| map_database_error("failed to validate item batch", error))?
            != 0;
        if !exists {
            return Err(AppError::NotFound {
                entity: "batch".to_owned(),
                message: format!("batch {batch_id} was not found"),
            });
        }
    }
    Ok(())
}

async fn draft_inserted_item_batch(
    connection: &mut SqliteConnection,
    batch_id: Option<Uuid>,
    updated_at: DateTime<Utc>,
) -> Result<(), AppError> {
    if let Some(batch_id) = batch_id {
        validate_and_draft_batch(connection, batch_id, &updated_at).await?;
    }
    Ok(())
}

async fn reset_affected_batches(
    connection: &mut SqliteConnection,
    original_batch_id: Option<Uuid>,
    current_batch_id: Option<Uuid>,
    updated_at: DateTime<Utc>,
) -> Result<(), AppError> {
    let mut batch_ids = [original_batch_id, current_batch_id]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    batch_ids.sort_unstable();
    batch_ids.dedup();
    for batch_id in batch_ids {
        validate_and_draft_batch(connection, batch_id, &updated_at).await?;
    }
    Ok(())
}

async fn validate_and_draft_batch(
    connection: &mut SqliteConnection,
    batch_id: Uuid,
    updated_at: &DateTime<Utc>,
) -> Result<(), AppError> {
    validate_batch_summary_total(connection, batch_id).await?;
    sqlx::query("UPDATE batches SET status = 'draft', updated_at = ? WHERE id = ?")
        .bind(updated_at.to_rfc3339())
        .bind(batch_id.to_string())
        .execute(&mut *connection)
        .await
        .map(|_| ())
        .map_err(|error| map_database_error("failed to refresh item batch", error))
}

async fn validate_batch_summary_total(
    connection: &mut SqliteConnection,
    batch_id: Uuid,
) -> Result<(), AppError> {
    let amounts =
        sqlx::query_scalar::<_, Option<i64>>("SELECT amount_cents FROM items WHERE batch_id = ?")
            .bind(batch_id.to_string())
            .fetch_all(&mut *connection)
            .await
            .map_err(|error| map_database_error("failed to validate batch summary", error))?;
    amounts.into_iter().try_fold(0_i64, |total, amount| {
        checked_add_amount_cents(total, amount.unwrap_or(0), "totalAmountCents")
    })?;
    Ok(())
}

impl DuplicateDiscardClaim {
    pub fn item(&self) -> &InvoiceItem {
        &self.item
    }

    pub fn protected_paths(&self) -> &[String] {
        &self.protected_paths
    }

    pub async fn delete(mut self) -> Result<(), AppError> {
        let result = sqlx::query("DELETE FROM items WHERE id = ?")
            .bind(self.item.id.to_string())
            .execute(&mut *self.transaction)
            .await
            .map(|_| ())
            .map_err(|error| map_database_error("failed to delete duplicate item", error));
        match result {
            Ok(()) => {
                self.transaction.commit().await.map_err(|error| {
                    map_database_error("failed to commit duplicate deletion", error)
                })
            }
            Err(error) => match self.transaction.rollback().await {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(AppError::Internal {
                    message: format!(
                        "duplicate deletion failed and rollback also failed: original error: \
                         {error}; rollback error: {rollback_error}"
                    ),
                }),
            },
        }
    }

    pub async fn rollback(self) -> Result<(), AppError> {
        self.transaction.rollback().await.map_err(|error| {
            map_database_error("failed to release duplicate deletion claim", error)
        })
    }
}

impl FileRecoveryClaim {
    pub fn referenced_paths(&self) -> &[String] {
        &self.referenced_paths
    }

    pub async fn commit(self) -> Result<(), AppError> {
        self.transaction
            .commit()
            .await
            .map_err(|error| map_database_error("failed to commit file recovery", error))
    }

    pub async fn rollback(self) -> Result<(), AppError> {
        self.transaction
            .rollback()
            .await
            .map_err(|error| map_database_error("failed to rollback file recovery", error))
    }
}

async fn finish_transaction<T>(
    transaction: Transaction<'_, Sqlite>,
    result: Result<T, AppError>,
) -> Result<T, AppError> {
    match result {
        Ok(item) => transaction
            .commit()
            .await
            .map(|()| item)
            .map_err(|error| map_database_error("failed to commit item transaction", error)),
        Err(error) => match transaction.rollback().await {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(AppError::Internal {
                message: format!(
                    "item transaction failed and rollback also failed: original error: {error}; rollback error: {rollback_error}"
                ),
            }),
        },
    }
}

async fn find_canonical_id_with_connection(
    connection: &mut SqliteConnection,
    sha256: &str,
) -> Result<Option<Uuid>, AppError> {
    let id = sqlx::query_scalar::<_, String>(
        "SELECT id FROM items \
         WHERE sha256 = ? AND duplicate_of_id IS NULL \
         ORDER BY CASE WHEN dedupe_status = 'unique' THEN 0 ELSE 1 END, \
                  created_at ASC, id ASC \
         LIMIT 1",
    )
    .bind(sha256)
    .fetch_optional(&mut *connection)
    .await
    .map_err(|error| map_database_error("failed to select canonical item", error))?;
    id.map(|id| parse_uuid(&id, "id")).transpose()
}

async fn insert_with_connection(
    connection: &mut SqliteConnection,
    item: &NewItemRecord,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO items (\
            id, original_name, original_path, normalized_pdf_path, sha256, mime_type, \
            source_type, source_account_id, source_mailbox, source_uid_validity, source_uid, \
            source_message_id, source_part_id, fetched_at, source_received_date, invoice_date, \
            suggested_period, batch_id, suggested_category, final_category, amount_cents, currency, \
            city, company, recognition_status, confirmation_status, dedupe_status, duplicate_of_id, \
            note, event_tag, project_tag, created_at, updated_at\
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
            ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .bind(item.source_uid_validity)
    .bind(item.source_uid)
    .bind(&item.source_message_id)
    .bind(&item.source_part_id)
    .bind(item.fetched_at.to_rfc3339())
    .bind(item.source_received_date.map(|date| date.to_string()))
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
    .execute(&mut *connection)
    .await
    .map(|_| ())
    .map_err(map_insert_error)
}

async fn get_with_connection(
    connection: &mut SqliteConnection,
    id: Uuid,
) -> Result<InvoiceItem, AppError> {
    let query = format!("SELECT {ITEM_COLUMNS} FROM items WHERE id = ?");
    let row = sqlx::query_as::<_, DbItemRow>(&query)
        .bind(id.to_string())
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| map_database_error("failed to get item", error))?
        .ok_or_else(|| AppError::NotFound {
            entity: "item".to_owned(),
            message: format!("item {id} was not found"),
        })?;
    InvoiceItem::try_from(row)
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
    source_uid_validity: Option<i64>,
    source_uid: Option<i64>,
    source_message_id: Option<String>,
    source_part_id: Option<String>,
    fetched_at: String,
    source_received_date: Option<String>,
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
            source_uid_validity: row.source_uid_validity,
            source_uid: row.source_uid,
            source_message_id: row.source_message_id,
            source_part_id: row.source_part_id,
            fetched_at: parse_datetime(&row.fetched_at, "fetched_at")?,
            source_received_date: parse_optional_date(
                row.source_received_date,
                "source_received_date",
            )?,
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
    validate_optional_amount_cents(amount_cents, "amount_cents").map(|_| ())
}

fn validate_source(item: &NewItemRecord) -> Result<(), AppError> {
    if item.source_type == SourceType::Email
        && (item.source_account_id.is_none_or(|id| id.is_nil())
            || item
                .source_mailbox
                .as_deref()
                .is_none_or(|mailbox| mailbox.trim().is_empty())
            || item
                .source_uid_validity
                .is_none_or(|uid_validity| uid_validity <= 0)
            || item.source_uid.is_none_or(|uid| uid <= 0)
            || item
                .source_part_id
                .as_deref()
                .is_none_or(|part_id| part_id.trim().is_empty()))
    {
        return Err(AppError::validation(
            "source",
            "email source requires an account, mailbox, positive UIDVALIDITY and UID, and part ID",
        ));
    }
    if item.source_type == SourceType::ManualUpload && item.source_uid_validity.is_some() {
        return Err(AppError::validation(
            "source_uid_validity",
            "manual uploads must not have IMAP UIDVALIDITY",
        ));
    }
    if item.source_type == SourceType::Email && item.source_received_date.is_none() {
        return Err(AppError::validation(
            "source_received_date",
            "email sources must preserve the IMAP INTERNALDATE calendar date",
        ));
    }
    if item.source_type == SourceType::ManualUpload && item.source_received_date.is_some() {
        return Err(AppError::validation(
            "source_received_date",
            "manual uploads must not have an IMAP received date",
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

    if let sqlx::Error::Database(database_error) = &error
        && (database_error.is_foreign_key_violation()
            || database_error.is_check_violation()
            || matches!(
                database_error.kind(),
                sqlx::error::ErrorKind::NotNullViolation
            ))
    {
        return AppError::Conflict {
            message: format!(
                "invoice item violates a database constraint: {}",
                database_error.message()
            ),
        };
    }

    map_database_error("failed to insert item", error)
}

fn map_database_error(context: &str, error: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(database_error) = &error
        && database_error
            .code()
            .and_then(|code| code.parse::<i32>().ok())
            .is_some_and(|code| matches!(code & 0xff, 5 | 6))
    {
        return AppError::External {
            service: "database".to_owned(),
            retryable: true,
            message: format!("{context}: {error}"),
        };
    }

    internal_error(context, error)
}
