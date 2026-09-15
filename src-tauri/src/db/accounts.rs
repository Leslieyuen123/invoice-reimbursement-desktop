use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqliteConnection, SqlitePool, Transaction, Type};
use uuid::Uuid;

use crate::domain::error::{AppError, sanitize_message};

const ACCOUNT_COLUMNS: &str = "id, provider, email, imap_host, imap_port, enabled, \
    sync_interval_minutes, last_synced_at, last_error, last_error_at, created_at, updated_at";

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
    pub last_error_at: Option<DateTime<Utc>>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncCursor {
    pub uid_validity: u32,
    pub last_uid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRun {
    pub id: Uuid,
    pub account_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRetryState {
    pub failures: u32,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub suspended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxAccountWithRetryState {
    pub account: MailboxAccount,
    pub retry_state: Option<SyncRetryState>,
}

#[derive(Clone)]
pub struct MailboxAccountRepository {
    pool: SqlitePool,
}

impl MailboxAccountRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// The shared application pool, so services built on this repository can
    /// reach their own tables without another constructor argument.
    pub(crate) fn pool(&self) -> SqlitePool {
        self.pool.clone()
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

    pub async fn save_metadata(
        &self,
        connection: &mut SqliteConnection,
        id: Uuid,
        is_update: bool,
        account: NewMailboxAccount,
    ) -> Result<MailboxAccount, AppError> {
        validate_account(&account)?;
        let now = Utc::now().to_rfc3339();
        if id.is_nil() {
            return Err(AppError::validation("id", "account ID must not be nil"));
        }

        let exists =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mailbox_accounts WHERE id = ?")
                .bind(id.to_string())
                .fetch_one(&mut *connection)
                .await
                .map_err(|error| map_database_error("failed to find mailbox account", error))?
                != 0;

        if is_update && !exists {
            return Err(account_not_found(id));
        }
        if is_update {
            sqlx::query(
                "UPDATE mailbox_accounts SET provider = ?, email = ?, imap_host = ?, \
                    imap_port = ?, enabled = ?, sync_interval_minutes = ?, updated_at = ? \
                 WHERE id = ?",
            )
            .bind(provider_str(account.provider))
            .bind(&account.email)
            .bind(&account.imap_host)
            .bind(account.imap_port)
            .bind(account.enabled)
            .bind(account.sync_interval_minutes)
            .bind(&now)
            .bind(id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(map_insert_error)?;
        } else {
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
            .bind(&now)
            .bind(&now)
            .execute(&mut *connection)
            .await
            .map_err(map_insert_error)?;
        }

        let query = format!("SELECT {ACCOUNT_COLUMNS} FROM mailbox_accounts WHERE id = ?");
        let row = sqlx::query_as::<_, DbMailboxAccountRow>(&query)
            .bind(id.to_string())
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| map_database_error("failed to read saved mailbox account", error))?;
        MailboxAccount::try_from(row)
    }

    pub async fn get(&self, id: Uuid) -> Result<MailboxAccount, AppError> {
        let query = format!("SELECT {ACCOUNT_COLUMNS} FROM mailbox_accounts WHERE id = ?");
        let row = sqlx::query_as::<_, DbMailboxAccountRow>(&query)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| map_database_error("failed to get mailbox account", error))?
            .ok_or_else(|| account_not_found(id))?;

        MailboxAccount::try_from(row)
    }

    pub async fn list(&self) -> Result<Vec<MailboxAccount>, AppError> {
        let query = format!("SELECT {ACCOUNT_COLUMNS} FROM mailbox_accounts ORDER BY email ASC");
        let rows = sqlx::query_as::<_, DbMailboxAccountRow>(&query)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| map_database_error("failed to list mailbox accounts", error))?;

        rows.into_iter().map(MailboxAccount::try_from).collect()
    }

    pub async fn list_with_retry_states(
        &self,
    ) -> Result<Vec<MailboxAccountWithRetryState>, AppError> {
        let rows = sqlx::query_as::<_, DbMailboxAccountWithRetryStateRow>(
            "SELECT accounts.id, accounts.provider, accounts.email, accounts.imap_host, \
                accounts.imap_port, accounts.enabled, accounts.sync_interval_minutes, \
                accounts.last_synced_at, accounts.last_error, accounts.last_error_at, \
                accounts.created_at, accounts.updated_at, retry.failures AS retry_failures, \
                retry.next_retry_at AS retry_next_retry_at, retry.suspended AS retry_suspended \
             FROM mailbox_accounts AS accounts \
             LEFT JOIN sync_retry_states AS retry ON retry.account_id = accounts.id \
             ORDER BY accounts.email ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|error| map_database_error("failed to list mailbox account statuses", error))?;

        rows.into_iter()
            .map(MailboxAccountWithRetryState::try_from)
            .collect()
    }

    pub async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<(), AppError> {
        let result =
            sqlx::query("UPDATE mailbox_accounts SET enabled = ?, updated_at = ? WHERE id = ?")
                .bind(enabled)
                .bind(Utc::now().to_rfc3339())
                .bind(id.to_string())
                .execute(&self.pool)
                .await
                .map_err(|error| map_database_error("failed to update mailbox account", error))?;
        if result.rows_affected() != 1 {
            return Err(account_not_found(id));
        }
        Ok(())
    }

    pub async fn delete(&self, id: Uuid) -> Result<(), AppError> {
        let result = sqlx::query("DELETE FROM mailbox_accounts WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|error| map_database_error("failed to delete mailbox account", error))?;
        if result.rows_affected() != 1 {
            return Err(account_not_found(id));
        }
        Ok(())
    }

    pub async fn get_retry_state(
        &self,
        account_id: Uuid,
    ) -> Result<Option<SyncRetryState>, AppError> {
        let row = sqlx::query_as::<_, DbSyncRetryStateRow>(
            "SELECT failures, next_retry_at, suspended FROM sync_retry_states \
             WHERE account_id = ?",
        )
        .bind(account_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| map_database_error("failed to get mailbox retry state", error))?;
        row.map(SyncRetryState::try_from).transpose()
    }

    pub async fn record_retryable_failure(
        &self,
        account_id: Uuid,
        failed_at: DateTime<Utc>,
        message: &str,
    ) -> Result<SyncRetryState, AppError> {
        let next_one = (failed_at + chrono::Duration::minutes(1)).to_rfc3339();
        let next_five = (failed_at + chrono::Duration::minutes(5)).to_rfc3339();
        let next_fifteen = (failed_at + chrono::Duration::minutes(15)).to_rfc3339();
        let updated_at = failed_at.to_rfc3339();
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin mailbox retry update", error))?;
        let result = async {
            sqlx::query(
                "INSERT INTO sync_retry_states (\
                    account_id, failures, next_retry_at, suspended, updated_at\
                 ) VALUES (?, 1, ?, 0, ?) \
                 ON CONFLICT(account_id) DO UPDATE SET \
                    failures = CASE \
                        WHEN sync_retry_states.failures < 2147483647 \
                        THEN sync_retry_states.failures + 1 \
                        ELSE sync_retry_states.failures \
                    END, \
                    next_retry_at = CASE sync_retry_states.failures \
                        WHEN 0 THEN ? WHEN 1 THEN ? ELSE ? END, \
                    suspended = 0, updated_at = excluded.updated_at",
            )
            .bind(account_id.to_string())
            .bind(&next_one)
            .bind(&updated_at)
            .bind(&next_one)
            .bind(&next_five)
            .bind(&next_fifteen)
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to save mailbox retry state", error))?;
            update_last_error_in_transaction(
                &mut transaction,
                account_id,
                Some(message),
                &updated_at,
            )
            .await?;
            let row = sqlx::query_as::<_, DbSyncRetryStateRow>(
                "SELECT failures, next_retry_at, suspended FROM sync_retry_states \
                 WHERE account_id = ?",
            )
            .bind(account_id.to_string())
            .fetch_one(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to read mailbox retry state", error))?;
            SyncRetryState::try_from(row)
        }
        .await;
        finish_retry_transaction(transaction, result).await
    }

    pub async fn suspend_retry(
        &self,
        account_id: Uuid,
        failed_at: DateTime<Utc>,
        message: &str,
    ) -> Result<SyncRetryState, AppError> {
        let updated_at = failed_at.to_rfc3339();
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| {
                map_database_error("failed to begin mailbox suspension update", error)
            })?;
        let result = async {
            sqlx::query(
                "INSERT INTO sync_retry_states (\
                    account_id, failures, next_retry_at, suspended, updated_at\
                 ) VALUES (?, 0, NULL, 1, ?) \
                 ON CONFLICT(account_id) DO UPDATE SET failures = 0, \
                    next_retry_at = NULL, suspended = 1, updated_at = excluded.updated_at",
            )
            .bind(account_id.to_string())
            .bind(&updated_at)
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to suspend mailbox retry", error))?;
            update_last_error_in_transaction(
                &mut transaction,
                account_id,
                Some(message),
                &updated_at,
            )
            .await?;
            Ok(SyncRetryState {
                failures: 0,
                next_retry_at: None,
                suspended: true,
            })
        }
        .await;
        finish_retry_transaction(transaction, result).await
    }

    pub async fn clear_retry_state(&self, account_id: Uuid) -> Result<(), AppError> {
        let updated_at = Utc::now().to_rfc3339();
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin mailbox retry cleanup", error))?;
        let result = async {
            sqlx::query("DELETE FROM sync_retry_states WHERE account_id = ?")
                .bind(account_id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(|error| {
                    map_database_error("failed to clear mailbox retry state", error)
                })?;
            update_last_error_in_transaction(&mut transaction, account_id, None, &updated_at).await
        }
        .await;
        finish_retry_transaction(transaction, result).await
    }

    pub async fn clear_retry_state_in_transaction(
        &self,
        connection: &mut SqliteConnection,
        account_id: Uuid,
    ) -> Result<MailboxAccount, AppError> {
        sqlx::query("DELETE FROM sync_retry_states WHERE account_id = ?")
            .bind(account_id.to_string())
            .execute(&mut *connection)
            .await
            .map_err(|error| map_database_error("failed to clear mailbox retry state", error))?;
        let result = sqlx::query(
            "UPDATE mailbox_accounts SET last_error = NULL, last_error_at = NULL, \
                updated_at = ? WHERE id = ?",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(account_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(|error| map_database_error("failed to clear mailbox error", error))?;
        if result.rows_affected() != 1 {
            return Err(account_not_found(account_id));
        }
        let query = format!("SELECT {ACCOUNT_COLUMNS} FROM mailbox_accounts WHERE id = ?");
        let row = sqlx::query_as::<_, DbMailboxAccountRow>(&query)
            .bind(account_id.to_string())
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| map_database_error("failed to read saved mailbox account", error))?;
        MailboxAccount::try_from(row)
    }

    pub async fn get_cursor(
        &self,
        account_id: Uuid,
        mailbox: &str,
    ) -> Result<Option<SyncCursor>, AppError> {
        let row = sqlx::query_as::<_, (i64, i64)>(
            "SELECT uid_validity, last_uid FROM sync_cursors \
             WHERE account_id = ? AND mailbox = ?",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| map_database_error("failed to get sync cursor", error))?;

        row.map(|(uid_validity, last_uid)| {
            Ok(SyncCursor {
                uid_validity: u32::try_from(uid_validity)
                    .map_err(|error| internal_error("invalid cursor UIDVALIDITY", error))?,
                last_uid: u32::try_from(last_uid)
                    .map_err(|error| internal_error("invalid cursor UID", error))?,
            })
        })
        .transpose()
    }

    pub async fn upsert_cursor(
        &self,
        account_id: Uuid,
        mailbox: &str,
        cursor: SyncCursor,
    ) -> Result<(), AppError> {
        if mailbox.trim().is_empty() {
            return Err(AppError::validation("mailbox", "mailbox must not be blank"));
        }
        self.get(account_id).await?;
        sqlx::query(
            "INSERT INTO sync_cursors (account_id, mailbox, uid_validity, last_uid) \
             VALUES (?, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET \
                uid_validity = excluded.uid_validity, last_uid = excluded.last_uid",
        )
        .bind(account_id.to_string())
        .bind(mailbox)
        .bind(i64::from(cursor.uid_validity))
        .bind(i64::from(cursor.last_uid))
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|error| map_database_error("failed to upsert sync cursor", error))
    }

    pub async fn begin_sync_run(&self, account_id: Uuid) -> Result<SyncRun, AppError> {
        self.get(account_id).await?;
        let run = SyncRun {
            id: Uuid::new_v4(),
            account_id,
        };
        sqlx::query(
            "INSERT INTO sync_runs (id, account_id, started_at, status) \
             VALUES (?, ?, ?, 'running')",
        )
        .bind(run.id.to_string())
        .bind(account_id.to_string())
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|error| map_database_error("failed to begin sync run", error))?;
        Ok(run)
    }

    pub async fn finish_sync_success(
        &self,
        run: &SyncRun,
        mailbox: &str,
        expected_cursor: Option<SyncCursor>,
        cursor: SyncCursor,
        imported_count: u32,
    ) -> Result<(), AppError> {
        let now = Utc::now().to_rfc3339();
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin sync completion", error))?;
        let result = async {
            let current_cursor = sqlx::query_as::<_, (i64, i64)>(
                "SELECT uid_validity, last_uid FROM sync_cursors \
                 WHERE account_id = ? AND mailbox = ?",
            )
            .bind(run.account_id.to_string())
            .bind(mailbox)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to validate sync cursor", error))?
            .map(|(uid_validity, last_uid)| {
                Ok(SyncCursor {
                    uid_validity: u32::try_from(uid_validity)
                        .map_err(|error| internal_error("invalid cursor UIDVALIDITY", error))?,
                    last_uid: u32::try_from(last_uid)
                        .map_err(|error| internal_error("invalid cursor UID", error))?,
                })
            })
            .transpose()?;
            if current_cursor != expected_cursor {
                return Err(AppError::Conflict {
                    message: "sync cursor changed while this run was in progress".to_owned(),
                });
            }
            let run_update = sqlx::query(
                "UPDATE sync_runs SET finished_at = ?, status = 'succeeded', imported_count = ?, \
                    error_message = NULL WHERE id = ? AND account_id = ? AND status = 'running'",
            )
            .bind(&now)
            .bind(i64::from(imported_count))
            .bind(run.id.to_string())
            .bind(run.account_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to finish sync run", error))?;
            if run_update.rows_affected() != 1 {
                return Err(AppError::Conflict {
                    message: "sync run is missing or is no longer running".to_owned(),
                });
            }
            sqlx::query(
                "INSERT INTO sync_cursors (account_id, mailbox, uid_validity, last_uid) \
                 VALUES (?, ?, ?, ?) ON CONFLICT(account_id, mailbox) DO UPDATE SET \
                    uid_validity = excluded.uid_validity, last_uid = excluded.last_uid",
            )
            .bind(run.account_id.to_string())
            .bind(mailbox)
            .bind(i64::from(cursor.uid_validity))
            .bind(i64::from(cursor.last_uid))
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to update sync cursor", error))?;
            let account_update = sqlx::query(
                "UPDATE mailbox_accounts SET last_synced_at = ?, last_error = NULL, \
                    last_error_at = NULL, updated_at = ? WHERE id = ?",
            )
            .bind(&now)
            .bind(&now)
            .bind(run.account_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to update synced account", error))?;
            if account_update.rows_affected() != 1 {
                return Err(account_not_found(run.account_id));
            }
            Ok::<_, AppError>(())
        }
        .await;

        match result {
            Ok(()) => transaction
                .commit()
                .await
                .map_err(|error| map_database_error("failed to commit sync completion", error)),
            Err(error) => {
                transaction.rollback().await.map_err(|rollback| {
                    map_database_error("failed to roll back sync completion", rollback)
                })?;
                Err(error)
            }
        }
    }

    pub async fn finish_range_sync_success(
        &self,
        run: &SyncRun,
        imported_count: u32,
    ) -> Result<(), AppError> {
        let now = Utc::now().to_rfc3339();
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_database_error("failed to begin sync completion", error))?;
        let result = async {
            let run_update = sqlx::query(
                "UPDATE sync_runs SET finished_at = ?, status = 'succeeded', imported_count = ?, \
                    error_message = NULL WHERE id = ? AND account_id = ? AND status = 'running'",
            )
            .bind(&now)
            .bind(i64::from(imported_count))
            .bind(run.id.to_string())
            .bind(run.account_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to finish sync run", error))?;
            if run_update.rows_affected() != 1 {
                return Err(AppError::Conflict {
                    message: "sync run is missing or is no longer running".to_owned(),
                });
            }
            let account_update = sqlx::query(
                "UPDATE mailbox_accounts SET last_synced_at = ?, last_error = NULL, \
                    last_error_at = NULL, updated_at = ? WHERE id = ?",
            )
            .bind(&now)
            .bind(&now)
            .bind(run.account_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to update synced account", error))?;
            if account_update.rows_affected() != 1 {
                return Err(account_not_found(run.account_id));
            }
            Ok::<_, AppError>(())
        }
        .await;

        match result {
            Ok(()) => transaction
                .commit()
                .await
                .map_err(|error| map_database_error("failed to commit sync completion", error)),
            Err(error) => {
                transaction.rollback().await.map_err(|rollback| {
                    map_database_error("failed to roll back sync completion", rollback)
                })?;
                Err(error)
            }
        }
    }

    pub async fn finish_sync_failure(&self, run: &SyncRun, message: &str) -> Result<(), AppError> {
        let now = Utc::now().to_rfc3339();
        let message = sanitize_error_message(message);
        let mut transaction =
            self.pool.begin().await.map_err(|error| {
                map_database_error("failed to begin sync failure update", error)
            })?;
        let result = async {
            let run_update = sqlx::query(
                "UPDATE sync_runs SET finished_at = ?, status = 'failed', error_message = ? \
                 WHERE id = ? AND account_id = ? AND status = 'running'",
            )
            .bind(&now)
            .bind(&message)
            .bind(run.id.to_string())
            .bind(run.account_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to mark sync run failed", error))?;
            if run_update.rows_affected() != 1 {
                return Err(AppError::Conflict {
                    message: "sync run is missing or is no longer running".to_owned(),
                });
            }
            let account_update = sqlx::query(
                "UPDATE mailbox_accounts SET last_error = ?, last_error_at = ?, \
                    updated_at = ? WHERE id = ?",
            )
            .bind(&message)
            .bind(&now)
            .bind(&now)
            .bind(run.account_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_database_error("failed to update account sync error", error))?;
            if account_update.rows_affected() != 1 {
                return Err(account_not_found(run.account_id));
            }
            Ok::<_, AppError>(())
        }
        .await;
        match result {
            Ok(()) => transaction
                .commit()
                .await
                .map_err(|error| map_database_error("failed to commit sync failure", error)),
            Err(error) => {
                transaction.rollback().await.map_err(|rollback| {
                    map_database_error("failed to roll back sync failure", rollback)
                })?;
                Err(error)
            }
        }
    }
}

async fn update_last_error_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    account_id: Uuid,
    message: Option<&str>,
    updated_at: &str,
) -> Result<(), AppError> {
    let message = message.map(sanitize_error_message);
    let last_error_at = message.as_ref().map(|_| updated_at);
    let result = sqlx::query(
        "UPDATE mailbox_accounts SET last_error = ?, last_error_at = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(message)
    .bind(last_error_at)
    .bind(updated_at)
    .bind(account_id.to_string())
    .execute(&mut **transaction)
    .await
    .map_err(|error| map_database_error("failed to update mailbox error", error))?;
    if result.rows_affected() != 1 {
        return Err(account_not_found(account_id));
    }
    Ok(())
}

async fn finish_retry_transaction<T>(
    transaction: Transaction<'_, Sqlite>,
    result: Result<T, AppError>,
) -> Result<T, AppError> {
    match result {
        Ok(value) => {
            transaction.commit().await.map_err(|error| {
                map_database_error("failed to commit mailbox retry update", error)
            })?;
            Ok(value)
        }
        Err(error) => {
            transaction.rollback().await.map_err(|rollback| {
                map_database_error("failed to roll back mailbox retry update", rollback)
            })?;
            Err(error)
        }
    }
}

fn sanitize_error_message(message: &str) -> String {
    sanitize_message(message, &[])
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
    last_error_at: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(FromRow)]
struct DbSyncRetryStateRow {
    failures: i64,
    next_retry_at: Option<String>,
    suspended: i64,
}

#[derive(FromRow)]
struct DbMailboxAccountWithRetryStateRow {
    id: String,
    provider: String,
    email: String,
    imap_host: String,
    imap_port: i64,
    enabled: i64,
    sync_interval_minutes: i64,
    last_synced_at: Option<String>,
    last_error: Option<String>,
    last_error_at: Option<String>,
    created_at: String,
    updated_at: String,
    retry_failures: Option<i64>,
    retry_next_retry_at: Option<String>,
    retry_suspended: Option<i64>,
}

impl TryFrom<DbMailboxAccountWithRetryStateRow> for MailboxAccountWithRetryState {
    type Error = AppError;

    fn try_from(row: DbMailboxAccountWithRetryStateRow) -> Result<Self, Self::Error> {
        let retry_state = row
            .retry_failures
            .map(|failures| {
                SyncRetryState::try_from(DbSyncRetryStateRow {
                    failures,
                    next_retry_at: row.retry_next_retry_at,
                    suspended: row.retry_suspended.ok_or_else(|| {
                        internal_error("missing joined retry suspension state", "NULL")
                    })?,
                })
            })
            .transpose()?;
        let account = MailboxAccount::try_from(DbMailboxAccountRow {
            id: row.id,
            provider: row.provider,
            email: row.email,
            imap_host: row.imap_host,
            imap_port: row.imap_port,
            enabled: row.enabled,
            sync_interval_minutes: row.sync_interval_minutes,
            last_synced_at: row.last_synced_at,
            last_error: row.last_error,
            last_error_at: row.last_error_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })?;
        Ok(Self {
            account,
            retry_state,
        })
    }
}

impl TryFrom<DbSyncRetryStateRow> for SyncRetryState {
    type Error = AppError;

    fn try_from(row: DbSyncRetryStateRow) -> Result<Self, Self::Error> {
        Ok(Self {
            failures: u32::try_from(row.failures)
                .map_err(|error| internal_error("invalid retry failure count", error))?,
            next_retry_at: parse_optional_datetime(row.next_retry_at, "next_retry_at")?,
            suspended: parse_enabled(row.suspended)?,
        })
    }
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
            last_error_at: parse_optional_datetime(row.last_error_at, "last_error_at")?,
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

    map_database_error("failed to insert mailbox account", error)
}

pub(crate) fn map_database_error(context: &str, error: sqlx::Error) -> AppError {
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

fn internal_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Internal {
        message: format!("{context}: {error}"),
    }
}
