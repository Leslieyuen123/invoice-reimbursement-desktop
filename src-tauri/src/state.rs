use std::sync::Arc;

use sqlx::SqlitePool;

use crate::infra::credentials::CredentialStore;
use crate::infra::files::AppPaths;
use crate::infra::imap::{ImapGateway, NativeTlsImapGateway};
use crate::services::account_saves::{AccountSagaShutdown, AccountSaveCoordinator};
use crate::services::operations::AccountOperationCoordinator;
use crate::services::scheduler::{Clock, Scheduler, SyncRunner, SyncStartBarrier};
use crate::services::settings::{BackgroundSyncGate, SettingsService};

#[derive(Clone)]
pub struct AppState {
    pool: SqlitePool,
    paths: AppPaths,
    credentials: Arc<dyn CredentialStore>,
    gateway: Arc<dyn ImapGateway>,
    account_operations: AccountOperationCoordinator,
    account_saves: AccountSaveCoordinator,
    background_sync_gate: BackgroundSyncGate,
}

impl AppState {
    pub fn new(pool: SqlitePool, paths: AppPaths, credentials: Arc<dyn CredentialStore>) -> Self {
        Self::with_gateway(
            pool,
            paths,
            credentials,
            Arc::new(NativeTlsImapGateway::default()),
        )
    }

    pub fn with_gateway(
        pool: SqlitePool,
        paths: AppPaths,
        credentials: Arc<dyn CredentialStore>,
        gateway: Arc<dyn ImapGateway>,
    ) -> Self {
        let account_operations = AccountOperationCoordinator::default();
        let account_saves = AccountSaveCoordinator::new(
            pool.clone(),
            credentials.clone(),
            account_operations.clone(),
        );
        Self {
            pool,
            paths,
            credentials,
            gateway,
            account_operations,
            account_saves,
            background_sync_gate: BackgroundSyncGate::default(),
        }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn credentials(&self) -> &dyn CredentialStore {
        self.credentials.as_ref()
    }

    pub fn settings_service(&self) -> SettingsService {
        SettingsService::with_runtime(
            self.pool.clone(),
            self.gateway.clone(),
            self.credentials.clone(),
            self.background_sync_gate.clone(),
            self.account_operations.clone(),
            self.account_saves.clone(),
        )
    }

    pub fn scheduler(&self, runner: Arc<dyn SyncRunner>) -> Scheduler {
        Scheduler::with_operations_and_gate(
            self.pool.clone(),
            runner,
            self.account_operations.clone(),
            self.account_saves.clone(),
            self.background_sync_gate.clone(),
        )
    }

    pub fn scheduler_with_clock(
        &self,
        runner: Arc<dyn SyncRunner>,
        clock: Arc<dyn Clock>,
    ) -> Scheduler {
        Scheduler::with_operations_clock_and_gate(
            self.pool.clone(),
            runner,
            self.account_operations.clone(),
            self.account_saves.clone(),
            clock,
            self.background_sync_gate.clone(),
        )
    }

    pub fn scheduler_with_runtime(
        &self,
        runner: Arc<dyn SyncRunner>,
        clock: Arc<dyn Clock>,
        start_barrier: Arc<dyn SyncStartBarrier>,
    ) -> Scheduler {
        Scheduler::with_operations_runtime(
            self.pool.clone(),
            runner,
            self.account_operations.clone(),
            self.account_saves.clone(),
            clock,
            self.background_sync_gate.clone(),
            start_barrier,
        )
    }

    pub async fn reconcile_account_saves(&self) -> Result<(), crate::domain::error::AppError> {
        self.account_saves.reconcile_all().await.map(|_| ())
    }

    pub fn begin_account_saga_shutdown(&self) -> AccountSagaShutdown {
        self.account_saves.begin_shutdown()
    }
}
