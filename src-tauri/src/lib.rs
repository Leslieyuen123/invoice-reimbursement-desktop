use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{Manager, Runtime};

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(commands::invoke_handler())
        .register_asynchronous_uri_scheme_protocol("invoice-file", |context, request, responder| {
            let state = context.app_handle().state::<AppState>().inner().clone();
            tauri::async_runtime::spawn(async move {
                responder.respond(commands::items::preview_response(&state, &request).await);
            });
        })
        .setup(|app| {
            let state = initialize_state(app.handle())?;
            let scheduler = state.application_scheduler().start();
            app.manage(state);
            app.manage(RuntimeTasks {
                scheduler: Mutex::new(Some(scheduler)),
            });
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
}
