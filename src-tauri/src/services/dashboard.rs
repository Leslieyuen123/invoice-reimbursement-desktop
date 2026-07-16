use chrono::{Duration, Utc};
use sqlx::SqlitePool;

use crate::db::accounts::{MailboxAccount, MailboxAccountRepository};
use crate::db::batches::BatchSummary;
use crate::domain::error::AppError;
use crate::services::batches::BatchService;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardCounts {
    pub recently_added: u64,
    pub pending_confirmation: u64,
    pub recognition_failed: u64,
    pub suspected_duplicate: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardSnapshot {
    pub mailbox_accounts: Vec<MailboxAccount>,
    pub counts: DashboardCounts,
    pub recent_batches: Vec<BatchSummary>,
}

#[derive(Clone)]
pub struct DashboardService {
    pool: SqlitePool,
}

impl DashboardService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn load(&self) -> Result<DashboardSnapshot, AppError> {
        let recent_cutoff = (Utc::now() - Duration::days(7)).to_rfc3339();
        let row = sqlx::query_as::<_, (i64, i64, i64, i64)>(
            "SELECT
                SUM(CASE WHEN created_at >= ? THEN 1 ELSE 0 END),
                SUM(CASE
                    WHEN recognition_status = 'succeeded'
                     AND confirmation_status = 'pending'
                     AND dedupe_status != 'suspected_duplicate'
                    THEN 1 ELSE 0 END),
                SUM(CASE
                    WHEN recognition_status = 'failed'
                     AND dedupe_status != 'suspected_duplicate'
                    THEN 1 ELSE 0 END),
                SUM(CASE WHEN dedupe_status = 'suspected_duplicate' THEN 1 ELSE 0 END)
             FROM items",
        )
        .bind(recent_cutoff)
        .fetch_one(&self.pool)
        .await
        .map_err(|_| AppError::Internal {
            message: "failed to load dashboard counts".to_owned(),
        })?;
        let recent_batches = BatchService::new(self.pool.clone())
            .list_page(None, 5)
            .await?
            .batches;

        Ok(DashboardSnapshot {
            mailbox_accounts: MailboxAccountRepository::new(self.pool.clone())
                .list()
                .await?,
            counts: DashboardCounts {
                recently_added: count(row.0)?,
                pending_confirmation: count(row.1)?,
                recognition_failed: count(row.2)?,
                suspected_duplicate: count(row.3)?,
            },
            recent_batches,
        })
    }
}

fn count(value: i64) -> Result<u64, AppError> {
    u64::try_from(value).map_err(|_| AppError::Internal {
        message: "dashboard count was invalid".to_owned(),
    })
}
