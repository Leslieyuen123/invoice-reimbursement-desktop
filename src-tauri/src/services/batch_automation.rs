use std::collections::HashSet;
use std::sync::Arc;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::db::accounts::MailboxAccountRepository;
use crate::db::items::{InvoiceItem, ItemRepository};
use crate::domain::error::{AppError, sanitize_message};
use crate::domain::model::{DedupeStatus, ItemStatus};
use crate::services::batches::BatchService;
use crate::services::export::{ExportResult, ExportService};
use crate::services::operations::AccountOperationCoordinator;
use crate::services::sync::SyncService;

const CANDIDATE_PAGE_SIZE: usize = 200;
const MAX_BATCH_RANGE_DAYS: i64 = 366;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountAutomationFailure {
    pub account_id: Uuid,
    pub email: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchAutomationResult {
    pub scanned_account_count: u32,
    pub failed_accounts: Vec<AccountAutomationFailure>,
    pub imported_count: u32,
    pub assigned_count: u32,
    pub exception_count: u32,
    pub export: Option<ExportResult>,
}

#[derive(Clone, Default)]
pub struct BatchAutomationCoordinator {
    active: Arc<DashMap<Uuid, ()>>,
}

impl BatchAutomationCoordinator {
    fn acquire(&self, batch_id: Uuid) -> Result<ActiveBatchAutomation, AppError> {
        match self.active.entry(batch_id) {
            Entry::Vacant(entry) => {
                entry.insert(());
                Ok(ActiveBatchAutomation {
                    active: self.active.clone(),
                    batch_id,
                })
            }
            Entry::Occupied(_) => Err(AppError::Conflict {
                message: "batch automation is already running".to_owned(),
            }),
        }
    }
}

struct ActiveBatchAutomation {
    active: Arc<DashMap<Uuid, ()>>,
    batch_id: Uuid,
}

impl Drop for ActiveBatchAutomation {
    fn drop(&mut self) {
        self.active.remove(&self.batch_id);
    }
}

#[derive(Clone)]
pub struct BatchAutomationService {
    pool: SqlitePool,
    sync: SyncService,
    batches: BatchService,
    exports: ExportService,
    account_operations: AccountOperationCoordinator,
    coordinator: BatchAutomationCoordinator,
}

impl BatchAutomationService {
    pub(crate) fn new(
        pool: SqlitePool,
        sync: SyncService,
        batches: BatchService,
        exports: ExportService,
        account_operations: AccountOperationCoordinator,
        coordinator: BatchAutomationCoordinator,
    ) -> Self {
        Self {
            pool,
            sync,
            batches,
            exports,
            account_operations,
            coordinator,
        }
    }

    pub async fn run(&self, batch_id: Uuid) -> Result<BatchAutomationResult, AppError> {
        let _active = self.coordinator.acquire(batch_id)?;
        let detail = self.batches.get(batch_id).await?;
        let batch = detail.batch;
        let inclusive_days = batch
            .end_date
            .signed_duration_since(batch.start_date)
            .num_days()
            .checked_add(1)
            .ok_or_else(count_overflow)?;
        if inclusive_days > MAX_BATCH_RANGE_DAYS {
            return Err(AppError::validation(
                "dateRange",
                "batch automation date range must not exceed 366 days",
            ));
        }

        let accounts = MailboxAccountRepository::new(self.pool.clone())
            .list()
            .await?
            .into_iter()
            .filter(|account| account.enabled)
            .collect::<Vec<_>>();
        if accounts.is_empty() {
            return Err(AppError::Conflict {
                message: "no enabled mailbox accounts are configured".to_owned(),
            });
        }
        let scanned_account_count = u32::try_from(accounts.len()).map_err(|_| count_overflow())?;
        let account_ids = accounts
            .iter()
            .map(|account| account.id)
            .collect::<Vec<_>>();
        let mut failed_accounts = Vec::new();
        let mut imported_count = 0_u32;
        for account in accounts {
            let result = match self.account_operations.try_lock(account.id) {
                Ok(_operation) => {
                    self.sync
                        .run_range(account.id, batch.start_date, batch.end_date)
                        .await
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(result) => {
                    imported_count = imported_count
                        .checked_add(result.imported_count)
                        .ok_or_else(count_overflow)?;
                }
                Err(error) => failed_accounts.push(AccountAutomationFailure {
                    account_id: account.id,
                    email: account.email,
                    message: sanitize_message(&error.to_string(), &[]),
                }),
            }
        }

        let mut cursor = None;
        let mut safe_ids = Vec::new();
        let mut safe_id_set = HashSet::new();
        let mut exception_ids = HashSet::new();
        loop {
            let page = self
                .batches
                .list_candidates(batch_id, None, cursor, CANDIDATE_PAGE_SIZE)
                .await?;
            for item in page.items {
                if is_safe_candidate(&item, batch.start_date, batch.end_date) {
                    if safe_id_set.insert(item.id) {
                        safe_ids.push(item.id);
                    }
                } else {
                    exception_ids.insert(item.id);
                }
            }
            let Some(next_cursor) = page.next_cursor else {
                break;
            };
            cursor = Some(next_cursor);
        }

        for id in ItemRepository::new(self.pool.clone())
            .list_unassigned_email_ids_fetched_between(
                &account_ids,
                batch.start_date,
                batch.end_date,
            )
            .await?
        {
            if !safe_id_set.contains(&id) {
                exception_ids.insert(id);
            }
        }

        let assigned_count = u32::try_from(safe_ids.len()).map_err(|_| count_overflow())?;
        if !safe_ids.is_empty() {
            self.batches.assign_items(batch_id, &safe_ids).await?;
        }
        let exception_count = u32::try_from(exception_ids.len()).map_err(|_| count_overflow())?;
        let resulting_batch = self.batches.get(batch_id).await?;
        let export = if resulting_batch.summary.item_count == 0 {
            None
        } else {
            Some(self.exports.export(batch_id).await?)
        };

        Ok(BatchAutomationResult {
            scanned_account_count,
            failed_accounts,
            imported_count,
            assigned_count,
            exception_count,
            export,
        })
    }
}

fn is_safe_candidate(
    item: &InvoiceItem,
    start_date: chrono::NaiveDate,
    end_date: chrono::NaiveDate,
) -> bool {
    item.status() == ItemStatus::Ready
        && item
            .invoice_date
            .is_some_and(|date| date >= start_date && date <= end_date)
        && item.dedupe_status != DedupeStatus::SuspectedDuplicate
        && item.final_category.is_some()
        && item.amount_cents.is_some()
        && item.suggested_period.is_some()
        && item.normalized_pdf_path.is_some()
}

fn count_overflow() -> AppError {
    AppError::Internal {
        message: "batch automation count overflow".to_owned(),
    }
}
