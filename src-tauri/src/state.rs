use std::sync::Arc;

use sqlx::SqlitePool;

use crate::infra::credentials::CredentialStore;
use crate::infra::files::AppPaths;
use crate::infra::imap::{ImapGateway, NativeTlsImapGateway};
use crate::services::scheduler::{Scheduler, SchedulerCoordinator, SyncRunner};
use crate::services::settings::{BackgroundSyncGate, SettingsService};

#[derive(Clone)]
pub struct AppState {
    pool: SqlitePool,
    paths: AppPaths,
    credentials: Arc<dyn CredentialStore>,
    gateway: Arc<dyn ImapGateway>,
    scheduler_coordinator: SchedulerCoordinator,
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
            scheduler_coordinator: SchedulerCoordinator::default(),
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
        SettingsService::with_background_gate(
            self.pool.clone(),
            self.gateway.clone(),
            self.credentials.clone(),
            self.background_sync_gate.clone(),
        )
    }

    pub fn scheduler(&self, runner: Arc<dyn SyncRunner>) -> Scheduler {
        Scheduler::with_coordinator_and_gate(
            self.pool.clone(),
            runner,
            self.scheduler_coordinator.clone(),
            self.background_sync_gate.clone(),
        )
    }
}
