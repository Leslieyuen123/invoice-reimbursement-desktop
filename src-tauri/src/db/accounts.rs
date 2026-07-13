use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool, Type};
use uuid::Uuid;

use crate::domain::error::AppError;

const ACCOUNT_COLUMNS: &str = "id, provider, email, imap_host, imap_port, enabled, \
    sync_interval_minutes, last_synced_at, last_error, created_at, updated_at";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum MailboxProvider {
    Gmail,
    #[serde(rename = "qq")]
    QQ,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxAccount {
    pub id: Uuid,
    pub provider: MailboxProvider,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub enabled: bool,
    pub sync_interval_minutes: i64,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMailboxAccount {
    pub provider: MailboxProvider,
    pub email: String,
    pub imap_host: String,
    pub imap_port: i64,
    pub enabled: bool,
    pub sync_interval_minutes: i64,
}

#[derive(Clone)]
pub struct MailboxAccountRepository {
    pool: SqlitePool,
}

impl MailboxAccountRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, account: NewMailboxAccount) -> Result<MailboxAccount, AppError> {
        validate_account(&account)?;
        let id = Uuid::new_v4();
        let now = Utc::now();

        sqlx::query(
            "INSERT INTO mailbox_accounts (\
                id, provider, email, imap_host, imap_port, enabled, sync_interval_minutes, \
                created_at, updated_at\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(provider_str(account.provider))
        .bind(&account.email)
        .bind(&account.imap_host)
        .bind(account.imap_port)
        .bind(account.enabled)
        .bind(account.sync_interval_minutes)
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(map_insert_error)?;

        self.get(id).await
    }

    pub async fn get(&self, id: Uuid) -> Result<MailboxAccount, AppError> {
        let query = format!("SELECT {ACCOUNT_COLUMNS} FROM mailbox_accounts WHERE id = ?");
        let row = sqlx::query_as::<_, DbMailboxAccountRow>(&query)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| internal_error("failed to get mailbox account", error))?
            .ok_or_else(|| account_not_found(id))?;

        MailboxAccount::try_from(row)
    }

    pub async fn list(&self) -> Result<Vec<MailboxAccount>, AppError> {
        let query = format!("SELECT {ACCOUNT_COLUMNS} FROM mailbox_accounts ORDER BY email ASC");
        let rows = sqlx::query_as::<_, DbMailboxAccountRow>(&query)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| internal_error("failed to list mailbox accounts", error))?;

        rows.into_iter().map(MailboxAccount::try_from).collect()
    }
}

#[derive(FromRow)]
struct DbMailboxAccountRow {
    id: String,
    provider: String,
    email: String,
    imap_host: String,
    imap_port: i64,
    enabled: i64,
    sync_interval_minutes: i64,
    last_synced_at: Option<String>,
    last_error: Option<String>,
    created_at: String,
    updated_at: String,
}

impl TryFrom<DbMailboxAccountRow> for MailboxAccount {
    type Error = AppError;

    fn try_from(row: DbMailboxAccountRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: parse_uuid(&row.id, "id")?,
            provider: parse_provider(&row.provider)?,
            email: row.email,
            imap_host: row.imap_host,
            imap_port: u16::try_from(row.imap_port)
                .map_err(|error| internal_error("invalid IMAP port", error))?,
            enabled: parse_enabled(row.enabled)?,
            sync_interval_minutes: row.sync_interval_minutes,
            last_synced_at: parse_optional_datetime(row.last_synced_at, "last_synced_at")?,
            last_error: row.last_error,
            created_at: parse_datetime(&row.created_at, "created_at")?,
            updated_at: parse_datetime(&row.updated_at, "updated_at")?,
        })
    }
}

fn validate_account(account: &NewMailboxAccount) -> Result<(), AppError> {
    if account.email.trim().is_empty() {
        return Err(AppError::validation("email", "email must not be blank"));
    }
    if account.imap_host.trim().is_empty() {
        return Err(AppError::validation(
            "imap_host",
            "IMAP host must not be blank",
        ));
    }
    if !(1..=65_535).contains(&account.imap_port) {
        return Err(AppError::validation(
            "imap_port",
            "port must be between 1 and 65535",
        ));
    }
    if !(5..=1_440).contains(&account.sync_interval_minutes) {
        return Err(AppError::validation(
            "sync_interval_minutes",
            "sync interval must be between 5 and 1440 minutes",
        ));
    }

    Ok(())
}

fn provider_str(value: MailboxProvider) -> &'static str {
    match value {
        MailboxProvider::Gmail => "gmail",
        MailboxProvider::QQ => "qq",
    }
}

fn parse_provider(value: &str) -> Result<MailboxProvider, AppError> {
    match value {
        "gmail" => Ok(MailboxProvider::Gmail),
        "qq" => Ok(MailboxProvider::QQ),
        _ => Err(internal_error("invalid provider", value)),
    }
}

fn parse_enabled(value: i64) -> Result<bool, AppError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(internal_error("invalid enabled flag", value)),
    }
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(value).map_err(|error| internal_error(&format!("invalid {field}"), error))
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

fn account_not_found(id: Uuid) -> AppError {
    AppError::NotFound {
        entity: "mailbox_account".to_owned(),
        message: format!("mailbox account {id} was not found"),
    }
}

fn map_insert_error(error: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(database_error) = &error
        && database_error.is_unique_violation()
    {
        return AppError::Conflict {
            message: "a mailbox account with this email already exists".to_owned(),
        };
    }

    internal_error("failed to insert mailbox account", error)
}

fn internal_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Internal {
        message: format!("{context}: {error}"),
    }
}
