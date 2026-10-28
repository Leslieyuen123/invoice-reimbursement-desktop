use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::mail_ledger::{
    MailLedgerCursor, MailLedgerEntry, MailLedgerFilter, MailLedgerRepository,
};
use crate::domain::error::AppError;
use crate::domain::model::MailOutcome;
use crate::state::AppState;

const MAX_PAGE_SIZE: usize = 200;
const DEFAULT_PAGE_SIZE: usize = 50;

/// One scanned mail and what it produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailLedgerEntryDto {
    pub account_id: String,
    pub mailbox: String,
    pub uid: u32,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub received_at: String,
    pub processed_at: String,
    pub candidate_count: u32,
    pub imported_count: u32,
    pub existing_count: u32,
    pub failed_count: u32,
    pub outcome: MailOutcome,
    pub reason: Option<String>,
    pub marked_seen: bool,
    /// The invoices are in the library but the mailbox still shows the mail
    /// unread, so the `\Seen` update either failed or the mail was moved.
    pub seen_mismatch: bool,
}

impl From<MailLedgerEntry> for MailLedgerEntryDto {
    fn from(entry: MailLedgerEntry) -> Self {
        let settled = entry.failed_count == 0
            && entry.imported_count.saturating_add(entry.existing_count) > 0;
        Self {
            account_id: entry.account_id.to_string(),
            mailbox: entry.mailbox,
            uid: entry.uid,
            subject: entry.subject,
            sender: entry.sender,
            received_at: entry.received_at.to_rfc3339(),
            processed_at: entry.processed_at.to_rfc3339(),
            candidate_count: entry.candidate_count,
            imported_count: entry.imported_count,
            existing_count: entry.existing_count,
            failed_count: entry.failed_count,
            outcome: entry.outcome,
            reason: entry.reason,
            marked_seen: entry.marked_seen,
            seen_mismatch: settled && !entry.marked_seen,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailLedgerCountsDto {
    pub imported: u64,
    pub partial: u64,
    pub failed: u64,
    pub ignored: u64,
    pub needs_attention: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailLedgerCursorDto {
    pub received_at: String,
    pub uid: u32,
    pub account_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailLedgerPageDto {
    pub items: Vec<MailLedgerEntryDto>,
    pub next_cursor: Option<MailLedgerCursorDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailLedgerFilterDto {
    pub outcome: Option<MailOutcome>,
    #[serde(default)]
    pub needs_attention: bool,
    pub account_id: Option<String>,
    pub query: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailLedgerPageRequestDto {
    pub cursor: Option<MailLedgerCursorDto>,
    pub page_size: Option<u32>,
}

pub async fn list(
    state: &AppState,
    filter: MailLedgerFilterDto,
    page: Option<MailLedgerPageRequestDto>,
) -> Result<MailLedgerPageDto, AppError> {
    let page_size = validated_page_size(page.as_ref())?;
    let cursor = page
        .as_ref()
        .and_then(|page| page.cursor.as_ref())
        .map(parse_cursor)
        .transpose()?;
    let parsed_filter = MailLedgerFilter {
        outcome: filter.outcome,
        needs_attention: filter.needs_attention,
        account_id: filter
            .account_id
            .as_deref()
            .map(|value| {
                Uuid::parse_str(value)
                    .map_err(|_| AppError::validation("accountId", "invalid account id"))
            })
            .transpose()?,
        query: filter.query,
    };
    let page = MailLedgerRepository::new(state.pool().clone())
        .list_page(&parsed_filter, cursor, page_size)
        .await?;
    Ok(MailLedgerPageDto {
        items: page
            .entries
            .into_iter()
            .map(MailLedgerEntryDto::from)
            .collect(),
        next_cursor: page.next_cursor.map(|cursor| MailLedgerCursorDto {
            received_at: cursor.received_at.to_rfc3339(),
            uid: cursor.uid,
            account_id: cursor.account_id.to_string(),
        }),
    })
}

pub async fn counts(state: &AppState) -> Result<MailLedgerCountsDto, AppError> {
    let counts = MailLedgerRepository::new(state.pool().clone())
        .counts()
        .await?;
    Ok(MailLedgerCountsDto {
        imported: counts.imported,
        partial: counts.partial,
        failed: counts.failed,
        ignored: counts.ignored,
        needs_attention: counts.needs_attention(),
    })
}

fn validated_page_size(page: Option<&MailLedgerPageRequestDto>) -> Result<usize, AppError> {
    let Some(requested) = page.and_then(|page| page.page_size) else {
        return Ok(DEFAULT_PAGE_SIZE);
    };
    let requested = usize::try_from(requested)
        .map_err(|_| AppError::validation("pageSize", "page size is too large"))?;
    if requested == 0 || requested > MAX_PAGE_SIZE {
        return Err(AppError::validation(
            "pageSize",
            format!("page size must be between 1 and {MAX_PAGE_SIZE}"),
        ));
    }
    Ok(requested)
}

fn parse_cursor(cursor: &MailLedgerCursorDto) -> Result<MailLedgerCursor, AppError> {
    Ok(MailLedgerCursor {
        received_at: chrono::DateTime::parse_from_rfc3339(&cursor.received_at)
            .map(|value| value.with_timezone(&chrono::Utc))
            .map_err(|_| AppError::validation("cursor", "invalid mail ledger cursor"))?,
        uid: cursor.uid,
        account_id: Uuid::parse_str(&cursor.account_id)
            .map_err(|_| AppError::validation("cursor", "invalid mail ledger cursor"))?,
    })
}

pub(crate) mod ipc {
    use tauri::State;

    use super::{
        MailLedgerCountsDto, MailLedgerFilterDto, MailLedgerPageDto, MailLedgerPageRequestDto,
    };
    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command(rename_all = "camelCase")]
    pub async fn list_mail_ledger(
        state: State<'_, AppState>,
        filter: MailLedgerFilterDto,
        page: Option<MailLedgerPageRequestDto>,
    ) -> Result<MailLedgerPageDto, AppError> {
        super::list(&state, filter, page).await
    }

    #[tauri::command]
    pub async fn get_mail_ledger_counts(
        state: State<'_, AppState>,
    ) -> Result<MailLedgerCountsDto, AppError> {
        super::counts(&state).await
    }
}
