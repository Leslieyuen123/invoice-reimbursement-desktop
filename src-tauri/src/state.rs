use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::db::accounts::MailboxAccountRepository;
use crate::db::items::ItemRepository;
use crate::infra::credentials::CredentialStore;
use crate::infra::extraction::{DocumentExtractor, ExtractedDocument};
use crate::infra::files::AppPaths;
use crate::infra::imap::{ImapGateway, NativeTlsImapGateway};
use crate::services::account_saves::{
    AccountSagaShutdown, AccountSaveCoordinator, AccountSaveReconciliationReport,
};
use crate::services::dashboard::DashboardService;
use crate::services::export::{ExportCoordinator, ExportService};
use crate::services::import::ImportService;
use crate::services::items::ItemService;
use crate::services::operations::AccountOperationCoordinator;
use crate::services::preview::{PreviewCoordinator, PreviewService};
use crate::services::recognition::RecognitionService;
use crate::services::scheduler::{Clock, Scheduler, SyncRunner, SyncStartBarrier};
use crate::services::settings::{BackgroundSyncGate, ExportPreferenceGate, SettingsService};
use crate::services::shutdown::{ApplicationShutdown, RuntimeOperationCoordinator};
use crate::services::sync::SyncService;

#[derive(Clone)]
pub struct AppState {
    pool: SqlitePool,
    paths: AppPaths,
    credentials: Arc<dyn CredentialStore>,
    gateway: Arc<dyn ImapGateway>,
    import_service: ImportService,
    item_service: ItemService,
    recognition_service: RecognitionService,
    account_operations: AccountOperationCoordinator,
    account_saves: AccountSaveCoordinator,
    export_coordinator: ExportCoordinator,
    preview_coordinator: PreviewCoordinator,
    background_sync_gate: BackgroundSyncGate,
    export_preference_gate: ExportPreferenceGate,
    application_scheduler: Scheduler,
    runtime_operations: RuntimeOperationCoordinator,
    export_recovery_failures: Arc<RwLock<HashMap<String, String>>>,
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
        Self::with_gateway_and_extractor(
            pool,
            paths,
            credentials,
            gateway,
            Arc::new(UnavailableExtractor),
        )
    }

    pub fn with_gateway_and_extractor(
        pool: SqlitePool,
        paths: AppPaths,
        credentials: Arc<dyn CredentialStore>,
        gateway: Arc<dyn ImapGateway>,
        extractor: Arc<dyn DocumentExtractor>,
    ) -> Self {
        let account_operations = AccountOperationCoordinator::default();
        let account_saves = AccountSaveCoordinator::new(
            pool.clone(),
            credentials.clone(),
            account_operations.clone(),
        );
        let background_sync_gate = BackgroundSyncGate::default();
        let export_preference_gate = ExportPreferenceGate::default();
        let items = ItemRepository::new(pool.clone());
        let import_service = ImportService::new(items.clone(), paths.clone());
        let item_service = ItemService::new(items.clone(), paths.clone());
        let recognition_service = RecognitionService::new(items, extractor);
        let sync_service = SyncService::new(
            gateway.clone(),
            credentials.clone(),
            MailboxAccountRepository::new(pool.clone()),
            import_service.clone(),
            recognition_service.clone(),
        );
        let application_scheduler = Scheduler::with_operations_and_gate(
            pool.clone(),
            Arc::new(sync_service),
            account_operations.clone(),
            account_saves.clone(),
            background_sync_gate.clone(),
        );
        Self {
            pool,
            paths,
            credentials,
            gateway,
            import_service,
            item_service,
            recognition_service,
            account_operations,
            account_saves,
            export_coordinator: ExportCoordinator::default(),
            preview_coordinator: PreviewCoordinator::default(),
            background_sync_gate,
            export_preference_gate,
            application_scheduler,
            runtime_operations: RuntimeOperationCoordinator::default(),
            export_recovery_failures: Arc::new(RwLock::new(HashMap::new())),
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
            self.export_preference_gate.clone(),
            self.account_operations.clone(),
            self.account_saves.clone(),
        )
    }

    pub fn export_service(&self) -> ExportService {
        ExportService::with_preference_gate(
            self.pool.clone(),
            self.paths.clone(),
            self.export_coordinator.clone(),
            self.export_preference_gate.clone(),
        )
    }

    pub fn preview_service(&self) -> PreviewService {
        PreviewService::new(
            self.pool.clone(),
            self.paths.clone(),
            self.preview_coordinator.clone(),
        )
    }

    pub fn import_service(&self) -> ImportService {
        self.import_service.clone()
    }

    pub fn item_service(&self) -> ItemService {
        self.item_service.clone()
    }

    pub fn recognition_service(&self) -> RecognitionService {
        self.recognition_service.clone()
    }

    pub fn dashboard_service(&self) -> DashboardService {
        DashboardService::new(self.pool.clone())
    }

    pub fn application_scheduler(&self) -> Scheduler {
        self.application_scheduler.clone()
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

    pub async fn reconcile_account_saves(
        &self,
    ) -> Result<AccountSaveReconciliationReport, crate::domain::error::AppError> {
        self.account_saves.reconcile_all().await
    }

    pub async fn reconcile_exports(
        &self,
    ) -> Result<crate::services::export::ExportRecoveryReport, crate::domain::error::AppError> {
        let (export_directory, result) = self.export_service().reconcile_pending_with_root().await;
        let mut failures = self.export_recovery_failures.write().await;
        match result {
            Ok(report) => {
                if let Some(export_directory) = export_directory {
                    failures.remove(&export_directory);
                }
                Ok(report)
            }
            Err(error) => {
                if let Some(export_directory) = export_directory {
                    failures.insert(export_directory, error.to_string());
                }
                Err(error)
            }
        }
    }

    pub async fn export_recovery_error(&self, export_directory: &str) -> Option<String> {
        self.export_recovery_failures
            .read()
            .await
            .get(export_directory)
            .cloned()
    }

    pub fn begin_account_saga_shutdown(&self) -> AccountSagaShutdown {
        self.account_saves.begin_shutdown()
    }

    pub async fn run_tracked_operation<T, F>(
        &self,
        future: F,
    ) -> Result<T, crate::domain::error::AppError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, crate::domain::error::AppError>> + Send + 'static,
    {
        self.runtime_operations.run(future).await
    }

    pub fn begin_application_shutdown(
        &self,
        scheduler: Option<crate::services::scheduler::SchedulerHandle>,
    ) -> ApplicationShutdown {
        ApplicationShutdown::new(
            scheduler,
            self.runtime_operations.begin_shutdown(),
            self.account_saves.begin_shutdown(),
            self.export_coordinator.begin_shutdown(),
            self.pool.clone(),
        )
    }
}

struct UnavailableExtractor;

impl DocumentExtractor for UnavailableExtractor {
    fn extract(
        &self,
        _path: &std::path::Path,
    ) -> Result<ExtractedDocument, crate::domain::error::AppError> {
        Err(crate::domain::error::AppError::External {
            service: "ocr_sidecar".to_owned(),
            retryable: false,
            message: "OCR runtime is unavailable".to_owned(),
        })
    }
}
