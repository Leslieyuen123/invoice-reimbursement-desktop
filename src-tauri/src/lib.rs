pub mod db;
pub mod domain;
pub mod infra;
pub mod services;
pub mod state;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    #[test]
    fn exposes_the_desktop_entrypoint() {
        assert_eq!(env!("CARGO_PKG_NAME"), "invoice-reimbursement");

        let _entrypoint: fn() = super::run;
    }
}
