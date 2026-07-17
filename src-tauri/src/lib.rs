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
            if window.label() == "main"
                && let WindowEvent::CloseRequested { api, .. } = event
            {
                api.prevent_close();
                if window.hide().is_err() {
                    tracing::warn!("failed to hide the main window after close request");
                }
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
            let scheduler = state.application_scheduler().start();
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
        if matches!(event, tauri::RunEvent::Exit) {
            shutdown_runtime(app);
        }
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
            match action {
                TrayAction::Show => restore_main_window(app),
                TrayAction::SyncAll => sync_all_enabled_accounts(app.clone()),
                TrayAction::Exit => app.exit(0),
            }
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
    tauri::async_runtime::block_on(state.reconcile_exports())?;
    Ok(state)
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
            .wait(Duration::from_secs(10)),
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
}
