use std::collections::HashSet;
use std::sync::Arc;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::db::accounts::MailboxAccountRepository;
use crate::db::items::ItemRepository;
use crate::domain::error::{AppError, sanitize_message};
use crate::infra::files::AppPaths;
use crate::services::batch_eligibility::{
    can_fill_other_category, is_in_batch_range, is_safe_batch_candidate,
};
use crate::services::batch_repair::repair_missing_normalized_pdfs;
use crate::services::batches::{AutomationAssignmentOutcome, BatchService};
use crate::services::export::{ExportResult, ExportService};
use crate::services::operations::AccountOperationCoordinator;
use crate::services::recognition::RecognitionService;
use crate::services::sync::{SyncProgress, SyncService, TouchedItemBudget};

const CANDIDATE_PAGE_SIZE: usize = 200;
const MAX_BATCH_RANGE_DAYS: i64 = 366;
const ALL_MAILBOXES_FAILED_MESSAGE: &str = "所有已启用邮箱同步失败，请检查网络和邮箱授权后重试";

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
    /// Batch members this run took over from another batch (range re-anchor).
    pub moved_from_other_batches_count: u32,
    /// Invoices this run admitted by filling their missing category as "其他".
    pub categorized_as_other_count: u32,
    pub exception_count: u32,
    /// Normalized PDFs regenerated for batch members that were missing one.
    pub repaired_count: u32,
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
    recognition: RecognitionService,
    account_operations: AccountOperationCoordinator,
    coordinator: BatchAutomationCoordinator,
    paths: AppPaths,
    #[cfg(test)]
    assignment_pause: Option<Arc<AssignmentPause>>,
}

#[cfg(test)]
#[derive(Default)]
struct AssignmentPause {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl BatchAutomationService {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        pool: SqlitePool,
        sync: SyncService,
        batches: BatchService,
        exports: ExportService,
        recognition: RecognitionService,
        account_operations: AccountOperationCoordinator,
        coordinator: BatchAutomationCoordinator,
        paths: AppPaths,
    ) -> Self {
        Self {
            pool,
            sync,
            batches,
            exports,
            recognition,
            account_operations,
            coordinator,
            paths,
            #[cfg(test)]
            assignment_pause: None,
        }
    }

    pub async fn run(&self, batch_id: Uuid) -> Result<BatchAutomationResult, AppError> {
        let mut budget = TouchedItemBudget::default();
        self.run_with_touched_item_budget(batch_id, &mut budget)
            .await
    }

    #[cfg(test)]
    async fn run_with_touched_item_limit(
        &self,
        batch_id: Uuid,
        limit: usize,
    ) -> Result<BatchAutomationResult, AppError> {
        let mut budget = TouchedItemBudget::new(limit);
        self.run_with_touched_item_budget(batch_id, &mut budget)
            .await
    }

    async fn run_with_touched_item_budget(
        &self,
        batch_id: Uuid,
        budget: &mut TouchedItemBudget,
    ) -> Result<BatchAutomationResult, AppError> {
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
        let account_count = accounts.len();
        let scanned_account_count = u32::try_from(accounts.len()).map_err(|_| count_overflow())?;
        let mut failed_accounts = Vec::new();
        let mut completed = SyncProgress::default();
        for account in accounts {
            budget.ensure_available()?;
            let (result, error) = match self.account_operations.try_lock(account.id) {
                Ok(_operation) => match self
                    .sync
                    .run_range_with_progress_and_budget(
                        account.id,
                        batch.start_date,
                        batch.end_date,
                        budget,
                    )
                    .await
                {
                    Ok(result) => (Some(result), None),
                    Err(failure) => (Some(failure.completed), Some(failure.error)),
                },
                Err(error) => (None, Some(error)),
            };
            if let Some(result) = result {
                completed.merge(result)?;
            }
            if let Some(error) = error {
                failed_accounts.push(AccountAutomationFailure {
                    account_id: account.id,
                    email: account.email,
                    message: sanitize_message(&error.to_string(), &[]),
                });
            }
        }
        if failed_accounts.len() == account_count {
            return Err(AppError::External {
                service: "mailbox".to_owned(),
                retryable: true,
                message: ALL_MAILBOXES_FAILED_MESSAGE.to_owned(),
            });
        }

        let mut cursor = None;
        let mut safe_ids = Vec::new();
        let mut safe_id_set = HashSet::new();
        // Candidates that are fully exportable except for the missing category:
        // the run may decide "其他" for them while claiming them (see the
        // user's "找不到分类就统一设置为其他" rule).
        let mut other_category_ids = Vec::new();
        let mut other_category_id_set = HashSet::new();
        let mut exception_ids = HashSet::new();
        loop {
            let page = self
                .batches
                .list_candidates(batch_id, None, cursor, CANDIDATE_PAGE_SIZE)
                .await?;
            for item in page.items {
                if is_safe_batch_candidate(&self.paths, &item, batch.start_date, batch.end_date) {
                    if safe_id_set.insert(item.id) {
                        safe_ids.push(item.id);
                    }
                } else if is_in_batch_range(&item, batch.start_date, batch.end_date)
                    && can_fill_other_category(&self.paths, &item)
                {
                    if other_category_id_set.insert(item.id) {
                        other_category_ids.push(item.id);
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

        let imported_count = completed.imported_count;
        let touched_item_ids = completed.touched_item_ids.into_iter().collect::<Vec<_>>();
        for item in ItemRepository::new(self.pool.clone())
            .list_by_ids(&touched_item_ids)
            .await?
        {
            if item.batch_id != Some(batch_id)
                && !safe_id_set.contains(&item.id)
                && !other_category_id_set.contains(&item.id)
            {
                exception_ids.insert(item.id);
            }
        }

        #[cfg(test)]
        if let Some(pause) = &self.assignment_pause {
            pause.reached.notify_one();
            pause.release.notified().await;
        }

        let mut assignment_ids = safe_ids.clone();
        for id in &other_category_ids {
            if !assignment_ids.contains(id) {
                assignment_ids.push(*id);
            }
        }
        let assignment = if assignment_ids.is_empty() {
            AutomationAssignmentOutcome::default()
        } else {
            self.batches
                .assign_unassigned_items(
                    batch_id,
                    &self.paths,
                    &assignment_ids,
                    &other_category_ids,
                )
                .await?
        };
        let assigned_ids = assignment.assigned_ids;
        let moved_from_other_batches_count = assignment.moved_from_other_batches_count;
        let categorized_as_other_count = assignment.categorized_as_other_count;
        let assigned_id_set = assigned_ids.iter().copied().collect::<HashSet<_>>();
        let mut requested_ids = safe_ids;
        for id in other_category_ids {
            if !requested_ids.contains(&id) {
                requested_ids.push(id);
            }
        }
        let skipped_safe_ids = requested_ids
            .iter()
            .copied()
            .filter(|id| !assigned_id_set.contains(id))
            .collect::<Vec<_>>();
        for item in ItemRepository::new(self.pool.clone())
            .list_by_ids(&skipped_safe_ids)
            .await?
        {
            if item.batch_id != Some(batch_id) {
                exception_ids.insert(item.id);
            }
        }
        let assigned_count = u32::try_from(assigned_ids.len()).map_err(|_| count_overflow())?;
        let exception_count = u32::try_from(exception_ids.len()).map_err(|_| count_overflow())?;
        let mut resulting_batch = self.batches.get(batch_id).await?;
        // A batch member may be missing its normalized PDF (manually confirmed
        // invoice, replaced attachment, restored backup). Regenerate it before
        // exporting so retrying the automation can actually fix the batch.
        let repaired_count =
            repair_missing_normalized_pdfs(&self.paths, &self.recognition, &resulting_batch.items)
                .await;
        if repaired_count != 0 {
            resulting_batch = self.batches.get(batch_id).await?;
        }
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
            moved_from_other_batches_count,
            categorized_as_other_count,
            exception_count,
            repaired_count,
            export,
        })
    }
}

fn count_overflow() -> AppError {
    AppError::Internal {
        message: "batch automation count overflow".to_owned(),
    }
}

#[cfg(test)]
mod automation_tests;
