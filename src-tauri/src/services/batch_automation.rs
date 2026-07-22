use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::db::accounts::MailboxAccountRepository;
use crate::db::items::{InvoiceItem, ItemRepository};
use crate::domain::error::{AppError, sanitize_message};
use crate::domain::model::{DedupeStatus, ItemStatus};
use crate::infra::files::{AppPaths, open_contained_regular_file};
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
    paths: AppPaths,
}

impl BatchAutomationService {
    pub(crate) fn new(
        pool: SqlitePool,
        sync: SyncService,
        batches: BatchService,
        exports: ExportService,
        account_operations: AccountOperationCoordinator,
        coordinator: BatchAutomationCoordinator,
        paths: AppPaths,
    ) -> Self {
        Self {
            pool,
            sync,
            batches,
            exports,
            account_operations,
            coordinator,
            paths,
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
        let mut failed_accounts = Vec::new();
        let mut imported_count = 0_u32;
        let mut touched_item_ids = HashSet::new();
        for account in accounts {
            let (result, error) = match self.account_operations.try_lock(account.id) {
                Ok(_operation) => match self
                    .sync
                    .run_range_with_progress(account.id, batch.start_date, batch.end_date)
                    .await
                {
                    Ok(result) => (Some(result), None),
                    Err(failure) => (Some(failure.completed), Some(failure.error)),
                },
                Err(error) => (None, Some(error)),
            };
            if let Some(result) = result {
                imported_count = imported_count
                    .checked_add(result.imported_count)
                    .ok_or_else(count_overflow)?;
                touched_item_ids.extend(result.touched_item_ids);
            }
            if let Some(error) = error {
                failed_accounts.push(AccountAutomationFailure {
                    account_id: account.id,
                    email: account.email,
                    message: sanitize_message(&error.to_string(), &[]),
                });
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
                if is_safe_candidate(&self.paths, &item, batch.start_date, batch.end_date) {
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

        let touched_item_ids = touched_item_ids.into_iter().collect::<Vec<_>>();
        for item in ItemRepository::new(self.pool.clone())
            .list_by_ids(&touched_item_ids)
            .await?
        {
            if item.batch_id != Some(batch_id) && !safe_id_set.contains(&item.id) {
                exception_ids.insert(item.id);
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
    paths: &AppPaths,
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
        && invoice_files_are_safe(paths, item)
}

fn invoice_files_are_safe(paths: &AppPaths, item: &InvoiceItem) -> bool {
    open_contained_regular_file(Path::new(&item.original_path), &paths.originals, "original")
        .is_ok()
        && item.normalized_pdf_path.as_deref().is_some_and(|path| {
            open_contained_regular_file(Path::new(path), &paths.normalized, "normalizedPdf").is_ok()
        })
}

fn count_overflow() -> AppError {
    AppError::Internal {
        message: "batch automation count overflow".to_owned(),
    }
}
