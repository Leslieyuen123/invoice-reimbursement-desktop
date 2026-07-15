use std::sync::Arc;

use sqlx::SqlitePool;

use crate::infra::credentials::CredentialStore;
use crate::infra::files::AppPaths;
use crate::infra::imap::{ImapGateway, NativeTlsImapGateway};
use crate::services::operations::AccountOperationCoordinator;
use crate::services::scheduler::{Scheduler, SyncRunner};
use crate::services::settings::{BackgroundSyncGate, SettingsService};

#[derive(Clone)]
pub struct AppState {
    pool: SqlitePool,
    paths: AppPaths,
    credentials: Arc<dyn CredentialStore>,
    gateway: Arc<dyn ImapGateway>,
    account_operations: AccountOperationCoordinator,
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
        Self {
            pool,
            paths,
            credentials,
            gateway,
            account_operations: AccountOperationCoordinator::default(),
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
        )
    }

    pub fn scheduler(&self, runner: Arc<dyn SyncRunner>) -> Scheduler {
        Scheduler::with_operations_and_gate(
            self.pool.clone(),
            runner,
            self.account_operations.clone(),
            self.background_sync_gate.clone(),
        )
    }
}
