pub mod batches;
pub mod dashboard;
pub mod export;
pub mod items;
pub mod mail;
pub mod settings;
pub mod sync;

use serde::{Deserialize, Serialize};

pub const DEFAULT_PAGE_SIZE: u32 = 50;
pub const MAX_PAGE_SIZE: u32 = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CursorDto {
    pub sort_value: String,
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PageRequestDto {
    pub cursor: Option<CursorDto>,
    pub page_size: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageDto<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<CursorDto>,
}

pub(crate) fn validated_page_size(
    request: Option<&PageRequestDto>,
) -> Result<usize, crate::domain::error::AppError> {
    let page_size = request
        .and_then(|request| request.page_size)
        .unwrap_or(DEFAULT_PAGE_SIZE);
    if page_size == 0 || page_size > MAX_PAGE_SIZE {
        return Err(crate::domain::error::AppError::validation(
            "pageSize",
            format!("page size must be between 1 and {MAX_PAGE_SIZE}"),
        ));
    }
    Ok(page_size as usize)
}

pub const COMMAND_NAMES: &[&str] = &[
    "get_dashboard",
    "list_items",
    "get_item",
    "open_item_original",
    "import_manual_files",
    "review_item",
    "resolve_duplicate",
    "retry_recognition",
    "list_batches",
    "get_batch",
    "list_batch_candidates",
    "create_month_batch",
    "create_custom_batch",
    "assign_items_to_batch",
    "remove_item_from_batch",
    "export_batch",
    "run_batch_automation",
    "list_mailbox_accounts",
    "save_mailbox_account",
    "test_mailbox_account",
    "delete_mailbox_account",
    "get_preferences",
    "save_preferences",
    "get_storage_status",
    "retry_export_recovery",
    "sync_account_now",
];

pub fn invoke_handler<R: tauri::Runtime>()
-> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        dashboard::ipc::get_dashboard,
        items::ipc::list_items,
        items::ipc::get_item,
        items::ipc::open_item_original,
        items::ipc::import_manual_files,
        items::ipc::review_item,
        items::ipc::resolve_duplicate,
        items::ipc::retry_recognition,
        mail::ipc::list_mail_ledger,
        mail::ipc::get_mail_ledger_counts,
        batches::ipc::list_batches,
        batches::ipc::get_batch,
        batches::ipc::repair_batch_normalized_pdfs,
        batches::ipc::update_batch_range,
        batches::ipc::settle_batch_items,
        batches::ipc::remove_batch_items,
        batches::ipc::list_batch_candidates,
        batches::ipc::create_month_batch,
        batches::ipc::create_custom_batch,
        batches::ipc::assign_items_to_batch,
        batches::ipc::remove_item_from_batch,
        export::ipc::export_batch,
        batches::ipc::run_batch_automation,
        settings::ipc::list_mailbox_accounts,
        settings::ipc::save_mailbox_account,
        settings::ipc::test_mailbox_account,
        settings::ipc::delete_mailbox_account,
        settings::ipc::get_preferences,
        settings::ipc::save_preferences,
        settings::ipc::get_storage_status,
        settings::ipc::retry_export_recovery,
        sync::ipc::sync_account_now,
    ]
}
