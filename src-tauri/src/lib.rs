use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::menu::MenuBuilder;
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, Runtime, WindowEvent};

pub mod commands;
pub mod db;
pub mod domain;
pub mod infra;
pub mod services;
pub mod state;

use infra::credentials::KeyringCredentialStore;
use infra::extraction::{LocalExtractor, ProcessOcrGateway};
use infra::files::AppPaths;
use infra::imap::NativeTlsImapGateway;
use services::scheduler::SchedulerHandle;
use state::AppState;

struct RuntimeTasks {
    scheduler: Mutex<Option<SchedulerHandle>>,
}

const TRAY_ID: &str = "invoice-reimbursement-tray";
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const TRAY_MENU_ITEMS: [(&str, &str); 3] = [
    ("show", "显示发票报销"),
    ("sync-all", "立即同步全部"),
    ("exit", "退出"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayAction {
    Show,
    SyncAll,
    Exit,
}

impl TrayAction {
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "show" => Some(Self::Show),
            "sync-all" => Some(Self::SyncAll),
            "exit" => Some(Self::Exit),
            _ => None,
        }
    }
}

fn tray_tooltip(pending_confirmation_count: u64) -> String {
    format!("发票报销，{pending_confirmation_count} 张待确认")
}

fn apply_tray_tooltip<E>(
    pending_confirmation_count: u64,
    set_tooltip: impl FnOnce(String) -> Result<(), E>,
) -> Result<(), E> {
    set_tooltip(tray_tooltip(pending_confirmation_count))
}

fn handle_main_window_close_request<E>(
    window_label: &str,
    prevent_close: impl FnOnce(),
    hide: impl FnOnce() -> Result<(), E>,
) -> Result<bool, E> {
    if window_label != "main" {
        return Ok(false);
    }
    prevent_close();
    hide()?;
    Ok(true)
}

fn dispatch_tray_action(
    action: TrayAction,
    show: impl FnOnce(),
    sync_all: impl FnOnce(),
    exit: impl FnOnce(i32),
) {
    match action {
        TrayAction::Show => show(),
        TrayAction::SyncAll => sync_all(),
        TrayAction::Exit => exit(0),
    }
}

fn dispatch_run_event(event: &tauri::RunEvent, shutdown: impl FnOnce()) {
    if matches!(event, tauri::RunEvent::Exit) {
        shutdown();
    }
}

pub(crate) fn refresh_tray_tooltip<R: Runtime>(
    app: &tauri::AppHandle<R>,
    pending_confirmation_count: u64,
) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    if apply_tray_tooltip(pending_confirmation_count, |tooltip| {
        tray.set_tooltip(Some(tooltip))
    })
    .is_err()
    {
        tracing::warn!("failed to refresh tray pending-confirmation tooltip");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(commands::invoke_handler())
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && handle_main_window_close_request(
                    window.label(),
                    || api.prevent_close(),
                    || window.hide(),
                )
                .is_err()
            {
                tracing::warn!("failed to hide the main window after close request");
            }
        })
        .register_asynchronous_uri_scheme_protocol("invoice-file", |context, request, responder| {
            let state = context.app_handle().state::<AppState>().inner().clone();
            tauri::async_runtime::spawn(async move {
                responder.respond(commands::items::preview_response(&state, &request).await);
            });
        })
        .setup(|app| {
            let state = initialize_state(app.handle())?;
            let pending_confirmation_count =
                tauri::async_runtime::block_on(state.dashboard_service().load())?
                    .counts
                    .pending_confirmation;
            let scheduler = start_application_scheduler(&state);
            app.manage(state);
            app.manage(RuntimeTasks {
                scheduler: Mutex::new(Some(scheduler)),
            });
            setup_tray(app, pending_confirmation_count)?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app, event| {
        dispatch_run_event(&event, || shutdown_runtime(app));
    });
}

fn setup_tray<R: Runtime>(
    app: &tauri::App<R>,
    pending_confirmation_count: u64,
) -> tauri::Result<()> {
    let menu = MenuBuilder::new(app)
        .text(TRAY_MENU_ITEMS[0].0, TRAY_MENU_ITEMS[0].1)
        .text(TRAY_MENU_ITEMS[1].0, TRAY_MENU_ITEMS[1].1)
        .text(TRAY_MENU_ITEMS[2].0, TRAY_MENU_ITEMS[2].1)
        .build()?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip(tray_tooltip(pending_confirmation_count))
        .on_menu_event(|app, event| {
            let Some(action) = TrayAction::from_id(event.id().as_ref()) else {
                return;
            };
            dispatch_tray_action(
                action,
                || restore_main_window(app),
                || sync_all_enabled_accounts(app.clone()),
                |code| app.exit(code),
            );
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

fn restore_main_window<R: Runtime>(app: &tauri::AppHandle<R>) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!("main window is unavailable for tray restore");
        return;
    };
    if window.show().is_err() || window.unminimize().is_err() || window.set_focus().is_err() {
        tracing::warn!("failed to restore and focus the main window from tray");
    }
}

fn sync_all_enabled_accounts<R: Runtime>(app: tauri::AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>().inner().clone();
        let accounts = match state.settings_service().list_accounts().await {
            Ok(accounts) => accounts,
            Err(_) => {
                tracing::warn!("tray sync all could not list mailbox accounts");
                return;
            }
        };
        for account in accounts.into_iter().filter(|account| account.enabled) {
            if commands::sync::now(&state, account.id).await.is_err() {
                tracing::warn!(
                    account_id = %account.id,
                    "tray sync all failed for mailbox account"
                );
            }
        }
        match state.dashboard_service().load().await {
            Ok(snapshot) => refresh_tray_tooltip(&app, snapshot.counts.pending_confirmation),
            Err(_) => tracing::warn!("failed to refresh tray status after sync all"),
        }
    });
}

fn initialize_state<R: Runtime>(
    app: &tauri::AppHandle<R>,
) -> Result<AppState, Box<dyn std::error::Error>> {
    let app_data = app.path().app_data_dir()?;
    let paths = AppPaths::create(app_data.join("storage"))?;
    let database_url = format!("sqlite://{}", app_data.join("invoice.sqlite3").display());
    let pool = tauri::async_runtime::block_on(db::connect(&database_url))?;
    let ocr = Arc::new(ProcessOcrGateway::new(sidecar_executable()?));
    let extractor = Arc::new(LocalExtractor::new(ocr));
    let state = AppState::with_gateway_and_extractor(
        pool,
        paths,
        Arc::new(KeyringCredentialStore::new()),
        Arc::new(NativeTlsImapGateway::default()),
        extractor,
    );
    tauri::async_runtime::block_on(state.reconcile_account_saves())?;
    tauri::async_runtime::block_on(reconcile_exports_on_startup(&state));
    Ok(state)
}

fn start_application_scheduler(state: &AppState) -> SchedulerHandle {
    let scheduler = state.application_scheduler();
    tauri::async_runtime::block_on(async move { scheduler.start() })
}

async fn reconcile_exports_on_startup(state: &AppState) {
    if state.reconcile_exports().await.is_err() {
        tracing::warn!("export recovery was deferred until storage becomes available");
    }
}

fn sidecar_executable() -> Result<PathBuf, std::io::Error> {
    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "application executable has no parent directory",
        )
    })?;
    Ok(directory.join(if cfg!(windows) {
        "invoice-ocr.exe"
    } else {
        "invoice-ocr"
    }))
}

fn shutdown_runtime<R: Runtime>(app: &tauri::AppHandle<R>) {
    let scheduler = app
        .state::<RuntimeTasks>()
        .scheduler
        .lock()
        .ok()
        .and_then(|mut scheduler| scheduler.take());
    let report = tauri::async_runtime::block_on(
        app.state::<AppState>()
            .begin_application_shutdown(scheduler)
            .wait(SHUTDOWN_TIMEOUT),
    );
    if report.timed_out {
        tracing::error!(
            interrupted_sync_runs = report.interrupted_sync_runs,
            interrupted_exports = report.interrupted_exports,
            "application shutdown exceeded the graceful deadline"
        );
    }
    for error in report.errors {
        tracing::error!(%error, "application shutdown component failed");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    #[test]
    fn exposes_the_desktop_entrypoint() {
        assert_eq!(env!("CARGO_PKG_NAME"), "invoice-reimbursement");

        let _entrypoint: fn() = super::run;
    }

    #[test]
    fn tray_contract_has_exact_labels_actions_and_pending_tooltip() {
        assert_eq!(
            super::TRAY_MENU_ITEMS,
            [
                ("show", "显示发票报销"),
                ("sync-all", "立即同步全部"),
                ("exit", "退出"),
            ]
        );
        assert_eq!(
            super::TrayAction::from_id("show"),
            Some(super::TrayAction::Show)
        );
        assert_eq!(
            super::TrayAction::from_id("sync-all"),
            Some(super::TrayAction::SyncAll)
        );
        assert_eq!(
            super::TrayAction::from_id("exit"),
            Some(super::TrayAction::Exit)
        );
        assert_eq!(super::tray_tooltip(4), "发票报销，4 张待确认");
    }

    #[test]
    fn tray_refresh_applies_the_current_pending_count_to_the_runtime_setter() {
        let mut applied = None;

        super::apply_tray_tooltip(7, |tooltip| {
            applied = Some(tooltip);
            Ok::<_, ()>(())
        })
        .unwrap();

        assert_eq!(applied.as_deref(), Some("发票报销，7 张待确认"));
    }

    #[test]
    fn main_window_close_request_prevents_close_and_hides_the_window() {
        let prevented = Cell::new(false);
        let hidden = Cell::new(false);

        let handled = super::handle_main_window_close_request(
            "main",
            || prevented.set(true),
            || {
                hidden.set(true);
                Ok::<_, ()>(())
            },
        )
        .unwrap();

        assert!(handled);
        assert!(prevented.get());
        assert!(hidden.get());
    }

    #[test]
    fn non_main_window_close_request_is_left_to_the_runtime() {
        let prevented = Cell::new(false);
        let hidden = Cell::new(false);

        let handled = super::handle_main_window_close_request(
            "preview",
            || prevented.set(true),
            || {
                hidden.set(true);
                Ok::<_, ()>(())
            },
        )
        .unwrap();

        assert!(!handled);
        assert!(!prevented.get());
        assert!(!hidden.get());
    }

    #[test]
    fn tray_exit_action_requests_application_exit_with_success_code() {
        let exit_code = Cell::new(None);

        super::dispatch_tray_action(
            super::TrayAction::Exit,
            || panic!("exit must not restore the window"),
            || panic!("exit must not start synchronization"),
            |code| exit_code.set(Some(code)),
        );

        assert_eq!(exit_code.get(), Some(0));
    }

    #[test]
    fn only_runtime_exit_dispatches_graceful_shutdown() {
        let shutdowns = Cell::new(0);
        super::dispatch_run_event(&tauri::RunEvent::Ready, || {
            shutdowns.set(shutdowns.get() + 1)
        });
        super::dispatch_run_event(&tauri::RunEvent::Exit, || {
            shutdowns.set(shutdowns.get() + 1)
        });

        assert_eq!(shutdowns.get(), 1);
    }

    #[test]
    fn graceful_shutdown_deadline_is_exactly_ten_seconds() {
        assert_eq!(super::SHUTDOWN_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn application_scheduler_starts_from_the_synchronous_tauri_setup_context() {
        use std::sync::Arc;

        use crate::db;
        use crate::infra::credentials::MemoryCredentialStore;
        use crate::infra::files::AppPaths;
        use crate::state::AppState;

        let directory = tempfile::tempdir().unwrap();
        let pool = tauri::async_runtime::block_on(db::connect("sqlite::memory:")).unwrap();
        let state = AppState::new(
            pool,
            AppPaths::create(directory.path().join("storage")).unwrap(),
            Arc::new(MemoryCredentialStore::default()),
        );

        let scheduler = super::start_application_scheduler(&state);

        tauri::async_runtime::block_on(scheduler.stop()).unwrap();
    }

    #[tokio::test]
    async fn startup_export_recovery_failure_is_recorded_without_aborting_startup() {
        use std::sync::Arc;

        use crate::db;
        use crate::infra::credentials::MemoryCredentialStore;
        use crate::infra::files::AppPaths;
        use crate::infra::imap::NativeTlsImapGateway;
        use crate::state::AppState;

        let directory = tempfile::tempdir().unwrap();
        let missing_root = directory.path().join("unmounted-exports");
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let preferences = serde_json::json!({
            "backgroundSyncEnabled": true,
            "exportDirectory": missing_root.to_string_lossy(),
            "batchDirectoryPattern": "{batchName}-{timestamp}",
        });
        sqlx::query(
            "INSERT INTO settings (key, value_json, updated_at) VALUES ('preferences', ?, ?)",
        )
        .bind(preferences.to_string())
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();
        let state = AppState::with_gateway(
            pool,
            AppPaths::create(directory.path().join("storage")).unwrap(),
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(NativeTlsImapGateway::default()),
        );

        super::reconcile_exports_on_startup(&state).await;

        let status = crate::commands::settings::storage_status(&state)
            .await
            .unwrap();
        assert!(status.recovery_error.is_some());
    }
}
