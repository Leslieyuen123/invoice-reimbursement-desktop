mod registry;
mod repository;

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::{OwnedMutexGuard, oneshot};
use uuid::Uuid;

use crate::db::accounts::{
    MailboxAccount, MailboxAccountRepository, NewMailboxAccount, map_database_error,
};
use crate::domain::error::AppError;
use crate::infra::credentials::{
    CredentialStore, delete_credential, get_credential, set_credential,
};
use crate::services::operations::AccountOperationCoordinator;

use registry::AccountSagaRegistry;
pub use registry::AccountSagaShutdown;
use repository::{
    PendingAccountSave, PendingAccountSaveCleanupRepository, PendingAccountSaveRepository,
    PendingSavePhase,
};

const PENDING_KEY_PREFIX: &str = "pending-save:";

#[derive(Debug, Default)]
pub struct AccountSaveReconciliationReport {
    failed_accounts: Vec<Uuid>,
    skipped_accounts: Vec<Uuid>,
    failed_cleanups: Vec<Uuid>,
}

impl AccountSaveReconciliationReport {
    pub fn failure_count(&self) -> usize {
        self.failed_accounts.len() + self.failed_cleanups.len()
    }

    pub fn skipped_count(&self) -> usize {
        self.skipped_accounts.len()
    }

    pub fn cleanup_failure_count(&self) -> usize {
        self.failed_cleanups.len()
    }

    pub fn pending_count(&self) -> usize {
        self.failure_count() + self.skipped_count()
    }
}

#[derive(Clone)]
pub(crate) struct AccountSaveCoordinator {
    worker: AccountSaveWorker,
    operations: AccountOperationCoordinator,
    registry: AccountSagaRegistry,
}

impl AccountSaveCoordinator {
    pub(crate) fn new(
        pool: SqlitePool,
        credentials: Arc<dyn CredentialStore>,
        operations: AccountOperationCoordinator,
    ) -> Self {
        Self {
            worker: AccountSaveWorker::new(pool, credentials),
            operations,
            registry: AccountSagaRegistry::default(),
        }
    }

    pub(crate) fn start_save(
        &self,
        operation_guard: OwnedMutexGuard<()>,
        account_id: Uuid,
        is_update: bool,
        metadata: NewMailboxAccount,
        secret: String,
    ) -> oneshot::Receiver<Result<MailboxAccount, AppError>> {
        let worker = self.worker.clone();
        self.registry.spawn(async move {
            let _operation_guard = operation_guard;
            worker.save(account_id, is_update, metadata, secret).await
        })
    }

    pub(crate) async fn reconcile_all(&self) -> Result<AccountSaveReconciliationReport, AppError> {
        self.registry.ensure_open()?;
        let mut report = AccountSaveReconciliationReport::default();
        for pending in self.worker.pending.list().await? {
            self.registry.ensure_open()?;
            let _guard = match self.operations.try_lock(pending.account_id) {
                Ok(guard) => guard,
                Err(AppError::Conflict { .. }) => {
                    report.skipped_accounts.push(pending.account_id);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if let Some(current) = self.worker.pending.get(pending.operation_id).await? {
                let diagnostic = current.clone();
                if self.worker.reconcile(current).await.is_err() {
                    log_reconciliation_failure(&diagnostic);
                    report.failed_accounts.push(diagnostic.account_id);
                }
            }
        }
        for operation_id in self.worker.cleanups.list().await? {
            self.registry.ensure_open()?;
            if self.worker.reconcile_cleanup(operation_id).await.is_err() {
                log_cleanup_failure(operation_id);
                report.failed_cleanups.push(operation_id);
            }
        }
        Ok(report)
    }

    pub(crate) async fn reconcile_account(
        &self,
        account_id: Uuid,
    ) -> Result<AccountSaveReconciliationReport, AppError> {
        self.registry.ensure_open()?;
        let mut report = AccountSaveReconciliationReport::default();
        if self
            .worker
            .pending
            .get_for_account(account_id)
            .await?
            .is_none()
        {
            return Ok(report);
        }
        let _guard = match self.operations.try_lock(account_id) {
            Ok(guard) => guard,
            Err(AppError::Conflict { .. }) => {
                report.skipped_accounts.push(account_id);
                return Ok(report);
            }
            Err(error) => return Err(error),
        };
        if let Some(pending) = self.worker.pending.get_for_account(account_id).await? {
            let diagnostic = pending.clone();
            if self.worker.reconcile(pending).await.is_err() {
                log_reconciliation_failure(&diagnostic);
                report.failed_accounts.push(account_id);
            }
        }
        Ok(report)
    }

    pub(crate) async fn ensure_no_blocking_pending(
        &self,
        account_id: Uuid,
    ) -> Result<(), AppError> {
        self.registry.ensure_open()?;
        if self
            .worker
            .pending
            .has_blocking_for_account(account_id)
            .await?
        {
            return Err(AppError::Conflict {
                message: "mailbox account save recovery is still pending".to_owned(),
            });
        }
        Ok(())
    }

    pub(crate) async fn has_blocking_pending(&self, account_id: Uuid) -> Result<bool, AppError> {
        self.worker
            .pending
            .has_blocking_for_account(account_id)
            .await
    }

    pub(crate) fn ensure_open(&self) -> Result<(), AppError> {
        self.registry.ensure_open()
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.registry.is_closing()
    }

    pub(crate) fn begin_shutdown(&self) -> AccountSagaShutdown {
        self.registry.begin_shutdown()
    }
}

#[derive(Clone)]
struct AccountSaveWorker {
    pool: SqlitePool,
    credentials: Arc<dyn CredentialStore>,
    accounts: MailboxAccountRepository,
    pending: PendingAccountSaveRepository,
    cleanups: PendingAccountSaveCleanupRepository,
}

impl AccountSaveWorker {
    fn new(pool: SqlitePool, credentials: Arc<dyn CredentialStore>) -> Self {
        Self {
            accounts: MailboxAccountRepository::new(pool.clone()),
            pending: PendingAccountSaveRepository::new(pool.clone()),
            cleanups: PendingAccountSaveCleanupRepository::new(pool.clone()),
            pool,
            credentials,
        }
    }

    async fn save(
        &self,
        account_id: Uuid,
        is_update: bool,
        metadata: NewMailboxAccount,
        secret: String,
    ) -> Result<MailboxAccount, AppError> {
        if is_update {
            self.accounts.get(account_id).await?;
        }
        let operation_id = Uuid::new_v4();
        self.pending
            .insert(operation_id, account_id, is_update, metadata)
            .await?;
        set_credential(
            self.credentials.clone(),
            pending_save_key(operation_id),
            secret,
        )
        .await?;
        self.pending
            .set_phase(operation_id, PendingSavePhase::CredentialStaged)
            .await?;
        let pending = self
            .pending
            .get(operation_id)
            .await?
            .ok_or_else(|| AppError::Internal {
                message: "prepared mailbox account save disappeared".to_owned(),
            })?;
        self.reconcile(pending)
            .await?
            .ok_or_else(|| AppError::Internal {
                message: "prepared mailbox account save did not publish".to_owned(),
            })
    }

    async fn reconcile(
        &self,
        mut pending: PendingAccountSave,
    ) -> Result<Option<MailboxAccount>, AppError> {
        let temp_key = pending_save_key(pending.operation_id);
        if pending.phase == PendingSavePhase::Committed {
            self.move_committed_to_cleanup(pending.operation_id).await?;
            return Ok(None);
        }

        let secret = get_credential(self.credentials.clone(), temp_key.clone()).await?;
        let Some(secret) = secret else {
            if pending.phase == PendingSavePhase::Prepared {
                self.pending.delete(pending.operation_id).await?;
                return Ok(None);
            }
            return Err(AppError::External {
                service: "mailbox_credential".to_owned(),
                retryable: false,
                message: "staged mailbox credential is unavailable".to_owned(),
            });
        };
        if pending.phase == PendingSavePhase::Prepared {
            self.pending
                .set_phase(pending.operation_id, PendingSavePhase::CredentialStaged)
                .await?;
            pending.phase = PendingSavePhase::CredentialStaged;
        }

        set_credential(
            self.credentials.clone(),
            pending.account_id.to_string(),
            secret,
        )
        .await?;
        let account = self.publish(&pending).await?;
        delete_credential(self.credentials.clone(), temp_key).await?;
        self.cleanups.delete(pending.operation_id).await?;
        Ok(Some(account))
    }

    async fn reconcile_cleanup(&self, operation_id: Uuid) -> Result<(), AppError> {
        delete_credential(self.credentials.clone(), pending_save_key(operation_id)).await?;
        self.cleanups.delete(operation_id).await
    }

    async fn move_committed_to_cleanup(&self, operation_id: Uuid) -> Result<(), AppError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let result = async {
            self.cleanups
                .insert_in_transaction(&mut transaction, operation_id)
                .await?;
            self.pending
                .delete_in_transaction(&mut transaction, operation_id)
                .await
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

    async fn publish(&self, pending: &PendingAccountSave) -> Result<MailboxAccount, AppError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let result = async {
            let account = self
                .accounts
                .save_metadata(
                    &mut transaction,
                    pending.account_id,
                    pending.is_update,
                    pending.metadata.clone(),
                )
                .await?;
            let account = self
                .accounts
                .clear_retry_state_in_transaction(&mut transaction, account.id)
                .await?;
            self.cleanups
                .insert_in_transaction(&mut transaction, pending.operation_id)
                .await?;
            self.pending
                .delete_in_transaction(&mut transaction, pending.operation_id)
                .await?;
            Ok::<_, AppError>(account)
        }
        .await;
        match result {
            Ok(account) => {
                transaction.commit().await.map_err(database_error)?;
                Ok(account)
            }
            Err(error) => {
                transaction.rollback().await.map_err(database_error)?;
                Err(error)
            }
        }
    }
}

fn pending_save_key(operation_id: Uuid) -> String {
    format!("{PENDING_KEY_PREFIX}{operation_id}")
}

fn log_reconciliation_failure(pending: &PendingAccountSave) {
    tracing::warn!(
        account_id = %pending.account_id,
        operation_id = %pending.operation_id,
        phase = pending.phase.as_str(),
        "mailbox account save reconciliation failed"
    );
}

fn log_cleanup_failure(operation_id: Uuid) {
    tracing::warn!(
        operation_id = %operation_id,
        "mailbox account save credential cleanup failed"
    );
}

fn database_error(error: sqlx::Error) -> AppError {
    map_database_error("failed to persist pending mailbox account save", error)
}
