use chrono::Utc;
use sqlx::{FromRow, SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::db::accounts::{MailboxProvider, NewMailboxAccount, map_database_error};
use crate::domain::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingSavePhase {
    Prepared,
    CredentialStaged,
    Committed,
}

impl PendingSavePhase {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::CredentialStaged => "credential_staged",
            Self::Committed => "committed",
        }
    }

    fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "credential_staged" => Ok(Self::CredentialStaged),
            "committed" => Ok(Self::Committed),
            _ => Err(AppError::Internal {
                message: "pending mailbox account save has an invalid phase".to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct PendingAccountSave {
    pub(super) operation_id: Uuid,
    pub(super) account_id: Uuid,
    pub(super) phase: PendingSavePhase,
    pub(super) is_update: bool,
    pub(super) metadata: NewMailboxAccount,
}

#[derive(Clone)]
pub(super) struct PendingAccountSaveRepository {
    pool: SqlitePool,
}

impl PendingAccountSaveRepository {
    pub(super) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub(super) async fn insert(
        &self,
        operation_id: Uuid,
        account_id: Uuid,
        is_update: bool,
        metadata: NewMailboxAccount,
    ) -> Result<(), AppError> {
        let now = Utc::now().to_rfc3339();
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(database_error)?;
        let result = async {
            let live_email_conflict = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM mailbox_accounts WHERE email = ? AND id != ?",
            )
            .bind(&metadata.email)
            .bind(account_id.to_string())
            .fetch_one(&mut *transaction)
            .await
            .map_err(database_error)?;
            if live_email_conflict != 0 {
                return Err(email_conflict_error());
            }
            sqlx::query(
                "INSERT INTO pending_account_saves (\
                    operation_id, account_id, phase, is_update, provider, email, imap_host, \
                    imap_port, enabled, sync_interval_minutes, created_at, updated_at\
                 ) VALUES (?, ?, 'prepared', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(operation_id.to_string())
            .bind(account_id.to_string())
            .bind(is_update)
            .bind(provider_str(metadata.provider))
            .bind(metadata.email)
            .bind(metadata.imap_host)
            .bind(metadata.imap_port)
            .bind(metadata.enabled)
            .bind(metadata.sync_interval_minutes)
            .bind(&now)
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .map_err(marker_insert_error)?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => transaction.commit().await.map_err(database_error),
            Err(error) => {
                transaction.rollback().await.map_err(database_error)?;
                Err(error)
            }
        }
    }

    pub(super) async fn list(&self) -> Result<Vec<PendingAccountSave>, AppError> {
        let rows = sqlx::query_as::<_, PendingAccountSaveRow>(
            "SELECT operation_id, account_id, phase, is_update, provider, email, imap_host, \
                imap_port, enabled, sync_interval_minutes \
             FROM pending_account_saves ORDER BY created_at, operation_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        rows.into_iter().map(PendingAccountSave::try_from).collect()
    }

    pub(super) async fn get(
        &self,
        operation_id: Uuid,
    ) -> Result<Option<PendingAccountSave>, AppError> {
        let row = sqlx::query_as::<_, PendingAccountSaveRow>(
            "SELECT operation_id, account_id, phase, is_update, provider, email, imap_host, \
                imap_port, enabled, sync_interval_minutes \
             FROM pending_account_saves WHERE operation_id = ?",
        )
        .bind(operation_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?;
        row.map(PendingAccountSave::try_from).transpose()
    }

    pub(super) async fn get_for_account(
        &self,
        account_id: Uuid,
    ) -> Result<Option<PendingAccountSave>, AppError> {
        let row = sqlx::query_as::<_, PendingAccountSaveRow>(
            "SELECT operation_id, account_id, phase, is_update, provider, email, imap_host, \
                imap_port, enabled, sync_interval_minutes \
             FROM pending_account_saves WHERE account_id = ?",
        )
        .bind(account_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?;
        row.map(PendingAccountSave::try_from).transpose()
    }

    pub(super) async fn has_blocking_for_account(
        &self,
        account_id: Uuid,
    ) -> Result<bool, AppError> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pending_account_saves \
             WHERE account_id = ? AND phase IN ('prepared', 'credential_staged')",
        )
        .bind(account_id.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(database_error)?;
        Ok(count != 0)
    }

    pub(super) async fn set_phase(
        &self,
        operation_id: Uuid,
        phase: PendingSavePhase,
    ) -> Result<(), AppError> {
        let result = sqlx::query(
            "UPDATE pending_account_saves SET phase = ?, updated_at = ? WHERE operation_id = ?",
        )
        .bind(phase.as_str())
        .bind(Utc::now().to_rfc3339())
        .bind(operation_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        require_updated_marker(result.rows_affected())
    }

    pub(super) async fn set_phase_in_transaction(
        &self,
        connection: &mut SqliteConnection,
        operation_id: Uuid,
        phase: PendingSavePhase,
    ) -> Result<(), AppError> {
        let result = sqlx::query(
            "UPDATE pending_account_saves SET phase = ?, updated_at = ? WHERE operation_id = ?",
        )
        .bind(phase.as_str())
        .bind(Utc::now().to_rfc3339())
        .bind(operation_id.to_string())
        .execute(connection)
        .await
        .map_err(database_error)?;
        require_updated_marker(result.rows_affected())
    }

    pub(super) async fn delete(&self, operation_id: Uuid) -> Result<(), AppError> {
        sqlx::query("DELETE FROM pending_account_saves WHERE operation_id = ?")
            .bind(operation_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(database_error)?;
        Ok(())
    }
}

#[derive(FromRow)]
struct PendingAccountSaveRow {
    operation_id: String,
    account_id: String,
    phase: String,
    is_update: i64,
    provider: String,
    email: String,
    imap_host: String,
    imap_port: i64,
    enabled: i64,
    sync_interval_minutes: i64,
}

impl TryFrom<PendingAccountSaveRow> for PendingAccountSave {
    type Error = AppError;

    fn try_from(row: PendingAccountSaveRow) -> Result<Self, Self::Error> {
        Ok(Self {
            operation_id: parse_uuid(&row.operation_id)?,
            account_id: parse_uuid(&row.account_id)?,
            phase: PendingSavePhase::parse(&row.phase)?,
            is_update: parse_boolean(row.is_update)?,
            metadata: NewMailboxAccount {
                provider: parse_provider(&row.provider)?,
                email: row.email,
                imap_host: row.imap_host,
                imap_port: row.imap_port,
                enabled: parse_boolean(row.enabled)?,
                sync_interval_minutes: row.sync_interval_minutes,
            },
        })
    }
}

fn parse_uuid(value: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(value).map_err(|_| AppError::Internal {
        message: "pending mailbox account save has an invalid ID".to_owned(),
    })
}

fn parse_boolean(value: i64) -> Result<bool, AppError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(AppError::Internal {
            message: "pending mailbox account save has an invalid boolean".to_owned(),
        }),
    }
}

fn provider_str(provider: MailboxProvider) -> &'static str {
    match provider {
        MailboxProvider::Gmail => "gmail",
        MailboxProvider::QQ => "qq",
    }
}

fn parse_provider(value: &str) -> Result<MailboxProvider, AppError> {
    match value {
        "gmail" => Ok(MailboxProvider::Gmail),
        "qq" => Ok(MailboxProvider::QQ),
        _ => Err(AppError::Internal {
            message: "pending mailbox account save has an invalid provider".to_owned(),
        }),
    }
}

fn require_updated_marker(rows_affected: u64) -> Result<(), AppError> {
    if rows_affected == 1 {
        Ok(())
    } else {
        Err(AppError::Internal {
            message: "pending mailbox account save disappeared".to_owned(),
        })
    }
}

fn database_error(error: sqlx::Error) -> AppError {
    map_database_error("failed to persist pending mailbox account save", error)
}

fn marker_insert_error(error: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(database_error) = &error
        && database_error.is_unique_violation()
    {
        return email_conflict_error();
    }
    database_error(error)
}

fn email_conflict_error() -> AppError {
    AppError::Conflict {
        message: "a mailbox account with this email already exists or is pending".to_owned(),
    }
}
