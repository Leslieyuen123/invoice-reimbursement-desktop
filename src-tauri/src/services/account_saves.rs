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

pub use registry::AccountSagaRegistry;
use repository::{PendingAccountSave, PendingAccountSaveRepository, PendingSavePhase};

const PENDING_KEY_PREFIX: &str = "pending-save:";

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
        registry: AccountSagaRegistry,
    ) -> Self {
        Self {
            worker: AccountSaveWorker::new(pool, credentials),
            operations,
            registry,
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

    pub(crate) async fn reconcile_all(&self) -> Result<(), AppError> {
        for pending in self.worker.pending.list().await? {
            let _guard = self.operations.try_lock(pending.account_id)?;
            if let Some(current) = self.worker.pending.get(pending.operation_id).await? {
                self.worker.reconcile(current).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn reconcile_account(&self, account_id: Uuid) -> Result<(), AppError> {
        if self
            .worker
            .pending
            .get_for_account(account_id)
            .await?
            .is_none()
        {
            return Ok(());
        }
        let _guard = self.operations.try_lock(account_id)?;
        if let Some(pending) = self.worker.pending.get_for_account(account_id).await? {
            self.worker.reconcile(pending).await?;
        }
        Ok(())
    }

    pub(crate) async fn ensure_no_pending(&self, account_id: Uuid) -> Result<(), AppError> {
        if self
            .worker
            .pending
            .get_for_account(account_id)
            .await?
            .is_some()
        {
            return Err(AppError::Conflict {
                message: "mailbox account save recovery is still pending".to_owned(),
            });
        }
        Ok(())
    }

    pub(crate) fn registry(&self) -> &AccountSagaRegistry {
        &self.registry
    }
}

#[derive(Clone)]
struct AccountSaveWorker {
    pool: SqlitePool,
    credentials: Arc<dyn CredentialStore>,
    accounts: MailboxAccountRepository,
    pending: PendingAccountSaveRepository,
}

impl AccountSaveWorker {
    fn new(pool: SqlitePool, credentials: Arc<dyn CredentialStore>) -> Self {
        Self {
            accounts: MailboxAccountRepository::new(pool.clone()),
            pending: PendingAccountSaveRepository::new(pool.clone()),
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
        if let Err(error) = set_credential(
            self.credentials.clone(),
            pending_save_key(operation_id),
            secret,
        )
        .await
        {
            return Err(error);
        }
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
            let account = self.accounts.get(pending.account_id).await?;
            delete_credential(self.credentials.clone(), temp_key).await?;
            self.pending.delete(pending.operation_id).await?;
            return Ok(Some(account));
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
        self.pending.delete(pending.operation_id).await?;
        Ok(Some(account))
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
            self.pending
                .set_phase_in_transaction(
                    &mut transaction,
                    pending.operation_id,
                    PendingSavePhase::Committed,
                )
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

fn database_error(error: sqlx::Error) -> AppError {
    map_database_error("failed to persist pending mailbox account save", error)
}
