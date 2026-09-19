use chrono::{DateTime, Utc};
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool};
use uuid::Uuid;

use crate::domain::error::AppError;
use crate::domain::model::MailOutcome;

const LEDGER_COLUMNS: &str = "account_id, mailbox, uid_validity, uid, message_id, subject, \
    sender, received_at, processed_at, candidate_count, imported_count, existing_count, \
    failed_count, outcome, reason, marked_seen";
const MAX_PAGE_SIZE: usize = 200;

/// What one scanned mail produced, written after the sync handled it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailLedgerRecord {
    pub account_id: Uuid,
    pub mailbox: String,
    pub uid_validity: u32,
    pub uid: u32,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub received_at: DateTime<Utc>,
    pub candidate_count: u32,
    pub imported_count: u32,
    pub existing_count: u32,
    pub failed_count: u32,
    pub reason: Option<String>,
}

impl MailLedgerRecord {
    /// Derives the outcome from the per-mail counters.
    ///
    /// A mail with no invoice clues at all is `Ignored` rather than `Failed`, so
    /// ordinary correspondence does not drown the queue the user must review.
    pub fn outcome(&self) -> MailOutcome {
        if self.failed_count == 0 && self.candidate_count == 0 && self.imported_count == 0 {
            return MailOutcome::Ignored;
        }
        if self.failed_count == 0 {
            return MailOutcome::Imported;
        }
        if self.imported_count + self.existing_count == 0 {
            MailOutcome::Failed
        } else {
            MailOutcome::Partial
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailLedgerEntry {
    pub account_id: Uuid,
    pub mailbox: String,
    pub uid_validity: u32,
    pub uid: u32,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub received_at: DateTime<Utc>,
    pub processed_at: DateTime<Utc>,
    pub candidate_count: u32,
    pub imported_count: u32,
    pub existing_count: u32,
    pub failed_count: u32,
    pub outcome: MailOutcome,
    pub reason: Option<String>,
    pub marked_seen: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailLedgerFilter {
    pub outcome: Option<MailOutcome>,
    pub needs_attention: bool,
    pub account_id: Option<Uuid>,
    pub query: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailLedgerCursor {
    pub received_at: DateTime<Utc>,
    pub uid: u32,
    pub account_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailLedgerPage {
    pub entries: Vec<MailLedgerEntry>,
    pub next_cursor: Option<MailLedgerCursor>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailLedgerCounts {
    pub imported: u64,
    pub partial: u64,
    pub failed: u64,
    pub ignored: u64,
}

impl MailLedgerCounts {
    /// Mails the user still has to deal with.
    pub fn needs_attention(&self) -> u64 {
        self.partial.saturating_add(self.failed)
    }
}

#[derive(Clone)]
pub struct MailLedgerRepository {
    pool: SqlitePool,
}

impl MailLedgerRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Upserts the ledger row for one scanned mail.
    ///
    /// `marked_seen` is preserved: it records that the mailbox accepted the flag
    /// at some point, which the UI compares against the ledger outcome.
    pub async fn record(&self, record: &MailLedgerRecord) -> Result<(), AppError> {
        let outcome = record.outcome();
        sqlx::query(
            "INSERT INTO mail_ledger (
                account_id, mailbox, uid_validity, uid, message_id, subject, sender,
                received_at, processed_at, candidate_count, imported_count,
                existing_count, failed_count, outcome, reason, marked_seen
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)
             ON CONFLICT(account_id, mailbox, uid_validity, uid) DO UPDATE SET
                message_id = excluded.message_id,
                subject = excluded.subject,
                sender = excluded.sender,
                received_at = excluded.received_at,
                processed_at = excluded.processed_at,
                candidate_count = excluded.candidate_count,
                imported_count = excluded.imported_count,
                existing_count = excluded.existing_count,
                failed_count = excluded.failed_count,
                outcome = excluded.outcome,
                reason = excluded.reason",
        )
        .bind(record.account_id.to_string())
        .bind(&record.mailbox)
        .bind(i64::from(record.uid_validity))
        .bind(i64::from(record.uid))
        .bind(&record.message_id)
        .bind(&record.subject)
        .bind(&record.sender)
        .bind(record.received_at.to_rfc3339())
        .bind(Utc::now().to_rfc3339())
        .bind(i64::from(record.candidate_count))
        .bind(i64::from(record.imported_count))
        .bind(i64::from(record.existing_count))
        .bind(i64::from(record.failed_count))
        .bind(outcome.as_str())
        .bind(&record.reason)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|error| database_error("failed to record mail ledger entry", error))
    }

    /// Records that the mailbox accepted the `\Seen` flag for this mail.
    pub async fn mark_seen(
        &self,
        account_id: Uuid,
        mailbox: &str,
        uid_validity: u32,
        uid: u32,
    ) -> Result<(), AppError> {
        sqlx::query(
            "UPDATE mail_ledger SET marked_seen = 1 \
             WHERE account_id = ? AND mailbox = ? AND uid_validity = ? AND uid = ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .bind(i64::from(uid_validity))
        .bind(i64::from(uid))
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|error| database_error("failed to mark mail ledger entry seen", error))
    }

    pub async fn list_page(
        &self,
        filter: &MailLedgerFilter,
        cursor: Option<MailLedgerCursor>,
        page_size: usize,
    ) -> Result<MailLedgerPage, AppError> {
        validate_page_size(page_size)?;
        let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
        query
            .push(LEDGER_COLUMNS)
            .push(" FROM mail_ledger WHERE 1 = 1");
        if let Some(outcome) = filter.outcome {
            query
                .push(" AND outcome = ")
                .push_bind(outcome.as_str().to_owned());
        }
        if filter.needs_attention {
            query.push(" AND outcome IN ('partial', 'failed')");
        }
        if let Some(account_id) = filter.account_id {
            query
                .push(" AND account_id = ")
                .push_bind(account_id.to_string());
        }
        if let Some(search) = normalized_search(filter.query.as_deref()) {
            let pattern = format!("%{search}%");
            query
                .push(" AND (LOWER(COALESCE(subject, '')) LIKE LOWER(")
                .push_bind(pattern.clone())
                .push(") OR LOWER(COALESCE(sender, '')) LIKE LOWER(")
                .push_bind(pattern)
                .push("))");
        }
        if let Some(cursor) = cursor {
            query
                .push(" AND (received_at < ")
                .push_bind(cursor.received_at.to_rfc3339())
                .push(" OR (received_at = ")
                .push_bind(cursor.received_at.to_rfc3339())
                .push(" AND (uid < ")
                .push_bind(i64::from(cursor.uid))
                .push(" OR (uid = ")
                .push_bind(i64::from(cursor.uid))
                .push(" AND account_id < ")
                .push_bind(cursor.account_id.to_string())
                .push("))))");
        }
        let fetch_limit = page_size.checked_add(1).ok_or_else(|| AppError::Internal {
            message: "mail ledger page size overflow".to_owned(),
        })?;
        query
            .push(" ORDER BY received_at DESC, uid DESC, account_id DESC LIMIT ")
            .push_bind(
                i64::try_from(fetch_limit)
                    .map_err(|_| AppError::validation("pageSize", "page size is too large"))?,
            );
        let rows = query
            .build_query_as::<DbMailLedgerRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(|error| database_error("failed to list mail ledger entries", error))?;
        let has_more = rows.len() > page_size;
        let mut rows = rows;
        rows.truncate(page_size);
        let entries = rows
            .into_iter()
            .map(MailLedgerEntry::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = has_more.then(|| {
            let last = entries
                .last()
                .expect("nonempty mail ledger page must have a cursor row");
            MailLedgerCursor {
                received_at: last.received_at,
                uid: last.uid,
                account_id: last.account_id,
            }
        });
        Ok(MailLedgerPage {
            entries,
            next_cursor,
        })
    }

    pub async fn counts(&self) -> Result<MailLedgerCounts, AppError> {
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT outcome, COUNT(*) FROM mail_ledger GROUP BY outcome",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|error| database_error("failed to count mail ledger outcomes", error))?;
        let mut counts = MailLedgerCounts::default();
        for (outcome, count) in rows {
            let count = u64::try_from(count).unwrap_or(0);
            match outcome.as_str() {
                "imported" => counts.imported = count,
                "partial" => counts.partial = count,
                "failed" => counts.failed = count,
                "ignored" => counts.ignored = count,
                _ => {}
            }
        }
        Ok(counts)
    }
}

fn normalized_search(query: Option<&str>) -> Option<String> {
    let query = query?.trim();
    (!query.is_empty()).then(|| query.to_lowercase())
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

#[derive(FromRow)]
struct DbMailLedgerRow {
    account_id: String,
    mailbox: String,
    uid_validity: i64,
    uid: i64,
    message_id: Option<String>,
    subject: Option<String>,
    sender: Option<String>,
    received_at: String,
    processed_at: String,
    candidate_count: i64,
    imported_count: i64,
    existing_count: i64,
    failed_count: i64,
    outcome: String,
    reason: Option<String>,
    marked_seen: i64,
}

impl TryFrom<DbMailLedgerRow> for MailLedgerEntry {
    type Error = AppError;

    fn try_from(row: DbMailLedgerRow) -> Result<Self, Self::Error> {
        Ok(Self {
            account_id: Uuid::parse_str(&row.account_id).map_err(|_| invalid_row("account_id"))?,
            mailbox: row.mailbox,
            uid_validity: u32::try_from(row.uid_validity)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| invalid_row("uid_validity"))?,
            uid: u32::try_from(row.uid)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| invalid_row("uid"))?,
            message_id: row.message_id,
            subject: row.subject,
            sender: row.sender,
            received_at: parse_time(&row.received_at, "received_at")?,
            processed_at: parse_time(&row.processed_at, "processed_at")?,
            candidate_count: count(row.candidate_count, "candidate_count")?,
            imported_count: count(row.imported_count, "imported_count")?,
            existing_count: count(row.existing_count, "existing_count")?,
            failed_count: count(row.failed_count, "failed_count")?,
            outcome: parse_outcome(&row.outcome)?,
            reason: row.reason,
            marked_seen: row.marked_seen != 0,
        })
    }
}

fn count(value: i64, field: &str) -> Result<u32, AppError> {
    u32::try_from(value).map_err(|_| invalid_row(field))
}

fn parse_time(value: &str, field: &str) -> Result<DateTime<Utc>, AppError> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| invalid_row(field))
}

fn parse_outcome(value: &str) -> Result<MailOutcome, AppError> {
    match value {
        "imported" => Ok(MailOutcome::Imported),
        "partial" => Ok(MailOutcome::Partial),
        "failed" => Ok(MailOutcome::Failed),
        "ignored" => Ok(MailOutcome::Ignored),
        _ => Err(invalid_row("outcome")),
    }
}

fn invalid_row(field: &str) -> AppError {
    AppError::Internal {
        message: format!("mail ledger row has an invalid {field}"),
    }
}

fn database_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Internal {
        message: format!("{context}: {error}"),
    }
}
