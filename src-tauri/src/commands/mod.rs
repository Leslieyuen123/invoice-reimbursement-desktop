pub mod batches;
pub mod dashboard;
pub mod export;
pub mod items;
pub mod settings;
pub mod sync;

pub const COMMAND_NAMES: &[&str] = &[
    "get_dashboard",
    "list_items",
    "get_item",
    "import_manual_files",
    "review_item",
    "resolve_duplicate",
    "retry_recognition",
    "list_batches",
    "get_batch",
    "create_month_batch",
    "create_custom_batch",
    "assign_items_to_batch",
    "remove_item_from_batch",
    "export_batch",
    "list_mailbox_accounts",
    "save_mailbox_account",
    "test_mailbox_account",
    "delete_mailbox_account",
    "get_preferences",
    "save_preferences",
    "sync_account_now",
];

pub fn invoke_handler<R: tauri::Runtime>()
-> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        dashboard::ipc::get_dashboard,
        items::ipc::list_items,
        items::ipc::get_item,
        items::ipc::import_manual_files,
        items::ipc::review_item,
        items::ipc::resolve_duplicate,
        items::ipc::retry_recognition,
        batches::ipc::list_batches,
        batches::ipc::get_batch,
        batches::ipc::create_month_batch,
        batches::ipc::create_custom_batch,
        batches::ipc::assign_items_to_batch,
        batches::ipc::remove_item_from_batch,
        export::ipc::export_batch,
        settings::ipc::list_mailbox_accounts,
        settings::ipc::save_mailbox_account,
        settings::ipc::test_mailbox_account,
        settings::ipc::delete_mailbox_account,
        settings::ipc::get_preferences,
        settings::ipc::save_preferences,
        sync::ipc::sync_account_now,
    ]
}
