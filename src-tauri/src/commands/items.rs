use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;
use uuid::Uuid;

use crate::commands::{CursorDto, PageDto, PageRequestDto, validated_page_size};
use crate::db::items::{InvoiceItem, ItemFilter, ItemPageCursor};
use crate::domain::amount::{validate_amount_cents, validate_optional_amount_cents};
use crate::domain::error::AppError;
use crate::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, ItemStatus, RecognitionStatus, SourceType,
};
use crate::infra::files::open_contained_regular_file;
use crate::services::items::ItemReview;
use crate::state::AppState;

pub use crate::services::preview::{PreviewPayload, PreviewRangePayload, PreviewVariant};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ItemFilterDto {
    pub status: Option<ItemStatus>,
    /// Source mailbox identity, used by the mail ledger to list one mail's
    /// invoices.
    pub source_account_id: Option<String>,
    pub source_uid: Option<i64>,
    #[serde(default)]
    pub recent: bool,
    pub suggested_period: Option<String>,
    pub category: Option<Category>,
    pub source_type: Option<SourceType>,
    pub batch_id: Option<String>,
    pub query: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvoiceItemDto {
    pub id: String,
    pub original_name: String,
    pub preview_url: String,
    pub source_type: SourceType,
    pub source_account_id: Option<String>,
    pub fetched_at: String,
    /// Local date the mail was received; the date batches are keyed on.
    pub source_received_date: Option<String>,
    pub invoice_date: Option<String>,
    pub suggested_period: Option<String>,
    pub batch_id: Option<String>,
    pub suggested_category: Option<Category>,
    pub final_category: Option<Category>,
    pub amount_cents: Option<i64>,
    pub currency: String,
    pub city: Option<String>,
    pub company: Option<String>,
    pub status: ItemStatus,
    pub recognition_status: RecognitionStatus,
    pub confirmation_status: ConfirmationStatus,
    pub dedupe_status: DedupeStatus,
    /// Whether a normalized PDF is recorded for this invoice.
    ///
    /// The export rejects the whole batch when any member lacks one, so the UI
    /// must be able to show that before the user assigns or exports.
    pub has_normalized_pdf: bool,
    pub note: Option<String>,
    pub event_tag: Option<String>,
    pub project_tag: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl TryFrom<InvoiceItem> for InvoiceItemDto {
    type Error = AppError;

    fn try_from(item: InvoiceItem) -> Result<Self, Self::Error> {
        let status = item.status();
        let variant = if item.normalized_pdf_path.is_some() {
            "normalized"
        } else {
            "original"
        };
        let amount_cents = validate_optional_amount_cents(item.amount_cents, "amountCents")?;
        Ok(Self {
            id: item.id.to_string(),
            original_name: item.original_name,
            preview_url: format!("invoice-file://item/{}?variant={variant}", item.id),
            source_type: item.source_type,
            source_account_id: item.source_account_id.map(|id| id.to_string()),
            fetched_at: item.fetched_at.to_rfc3339(),
            source_received_date: item.source_received_date.map(|date| date.to_string()),
            invoice_date: item.invoice_date.map(|date| date.to_string()),
            suggested_period: item.suggested_period,
            batch_id: item.batch_id.map(|id| id.to_string()),
            suggested_category: item.suggested_category,
            final_category: item.final_category,
            amount_cents,
            currency: item.currency,
            city: item.city,
            company: item.company,
            status,
            recognition_status: item.recognition_status,
            confirmation_status: item.confirmation_status,
            dedupe_status: item.dedupe_status,
            has_normalized_pdf: variant == "normalized",
            note: item.note,
            event_tag: item.event_tag,
            project_tag: item.project_tag,
            created_at: item.created_at.to_rfc3339(),
            updated_at: item.updated_at.to_rfc3339(),
        })
    }
}

pub async fn list(
    state: &AppState,
    filter: ItemFilterDto,
) -> Result<Vec<InvoiceItemDto>, AppError> {
    Ok(list_page(state, filter, None).await?.items)
}

pub async fn list_page(
    state: &AppState,
    filter: ItemFilterDto,
    page: Option<PageRequestDto>,
) -> Result<PageDto<InvoiceItemDto>, AppError> {
    list_page_at(state, filter, page, Utc::now()).await
}

#[doc(hidden)]
pub async fn list_page_at(
    state: &AppState,
    filter: ItemFilterDto,
    page: Option<PageRequestDto>,
    now: DateTime<Utc>,
) -> Result<PageDto<InvoiceItemDto>, AppError> {
    let page_size = validated_page_size(page.as_ref())?;
    let cursor = page
        .as_ref()
        .and_then(|page| page.cursor.as_ref())
        .map(parse_item_cursor)
        .transpose()?;
    let page = state
        .item_service()
        .list_page(ItemFilter::try_from((filter, now))?, cursor, page_size)
        .await?;
    Ok(PageDto {
        items: page
            .items
            .into_iter()
            .map(InvoiceItemDto::try_from)
            .collect::<Result<Vec<_>, _>>()?,
        next_cursor: page.next_cursor.map(|cursor| CursorDto {
            sort_value: cursor.created_at.to_rfc3339(),
            id: cursor.id.to_string(),
        }),
    })
}

pub(crate) fn parse_item_cursor(cursor: &CursorDto) -> Result<ItemPageCursor, AppError> {
    Ok(ItemPageCursor {
        created_at: DateTime::parse_from_rfc3339(&cursor.sort_value)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|_| AppError::validation("cursor", "invalid item cursor"))?,
        id: Uuid::parse_str(&cursor.id)
            .map_err(|_| AppError::validation("cursor", "invalid item cursor"))?,
    })
}

pub async fn get(state: &AppState, id: Uuid) -> Result<InvoiceItemDto, AppError> {
    state.item_service().get(id).await?.try_into()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewItemInputDto {
    pub id: Uuid,
    pub invoice_date: Option<String>,
    pub suggested_period: String,
    pub final_category: Category,
    pub amount_cents: i64,
    pub city: Option<String>,
    pub company: Option<String>,
    pub note: Option<String>,
    pub event_tag: Option<String>,
    pub project_tag: Option<String>,
}

pub async fn import_manual(
    state: &AppState,
    paths: Vec<String>,
) -> Result<Vec<InvoiceItemDto>, AppError> {
    let service = state.import_service();
    let mut items = Vec::with_capacity(paths.len());
    for path in paths {
        items.push(
            service
                .import_manual(&PathBuf::from(path))
                .await?
                .try_into()?,
        );
    }
    Ok(items)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ManualImportOutcomeDto {
    Imported {
        path: String,
        item: Box<InvoiceItemDto>,
    },
    Failed {
        path: String,
        error: AppError,
    },
}

pub async fn import_manual_outcomes(
    state: &AppState,
    paths: Vec<String>,
) -> Vec<ManualImportOutcomeDto> {
    let service = state.import_service();
    let mut outcomes = Vec::with_capacity(paths.len());
    for path in paths {
        let result = service
            .import_manual(&PathBuf::from(&path))
            .await
            .and_then(InvoiceItemDto::try_from);
        outcomes.push(match result {
            Ok(item) => ManualImportOutcomeDto::Imported {
                path,
                item: Box::new(item),
            },
            Err(error) => ManualImportOutcomeDto::Failed { path, error },
        });
    }
    outcomes
}

pub async fn review(
    state: &AppState,
    input: ReviewItemInputDto,
) -> Result<InvoiceItemDto, AppError> {
    validate_amount_cents(input.amount_cents, "amountCents")?;
    let reviewed = state
        .item_service()
        .review(ItemReview {
            id: input.id,
            invoice_date: input.invoice_date,
            suggested_period: input.suggested_period,
            final_category: input.final_category,
            amount_cents: input.amount_cents,
            city: input.city,
            company: input.company,
            note: input.note,
            event_tag: input.event_tag,
            project_tag: input.project_tag,
        })
        .await?;
    // Confirming an invoice whose recognition failed used to make it "ready for
    // a batch" while it had no normalized PDF, which later blocked the export
    // of the whole batch. Backfill it now; a review must never fail because of
    // this, the batch detail reports whatever is still missing.
    let reviewed = match state
        .recognition_service()
        .backfill_normalized_pdf(reviewed.id)
        .await
    {
        Ok(item) => item,
        Err(error) => {
            tracing::warn!(
                item_id = %reviewed.id,
                error = %error,
                "normalized PDF backfill after review was not possible"
            );
            reviewed
        }
    };
    reviewed.try_into()
}

pub async fn resolve_duplicate(
    state: &AppState,
    id: Uuid,
    keep: bool,
) -> Result<Option<InvoiceItemDto>, AppError> {
    state
        .item_service()
        .resolve_duplicate(id, keep)
        .await?
        .map(InvoiceItemDto::try_from)
        .transpose()
}

pub async fn retry_recognition(state: &AppState, id: Uuid) -> Result<InvoiceItemDto, AppError> {
    state.recognition_service().retry(id).await?.try_into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginalOpenTarget {
    LocalPath(PathBuf),
    ExternalUrl(String),
}

pub async fn resolve_original_open_target(
    state: &AppState,
    id: Uuid,
) -> Result<OriginalOpenTarget, AppError> {
    const MAX_URL_FILE_BYTES: u64 = 2_048;
    let item = state.item_service().get(id).await?;
    let path = PathBuf::from(&item.original_path);
    let mut opened = open_contained_regular_file(&path, &state.paths().originals, "originalPath")?;
    if item.mime_type != "text/uri-list" {
        return Ok(OriginalOpenTarget::LocalPath(path));
    }
    if opened.length == 0 || opened.length > MAX_URL_FILE_BYTES {
        return Err(AppError::validation("originalUrl", "发票链接无法打开"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(opened.length).unwrap_or_default());
    opened
        .file
        .by_ref()
        .take(MAX_URL_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| AppError::validation("originalUrl", "发票链接无法读取"))?;
    let value = std::str::from_utf8(&bytes)
        .map(str::trim)
        .map_err(|_| AppError::validation("originalUrl", "发票链接无效"))?;
    let url =
        url::Url::parse(value).map_err(|_| AppError::validation("originalUrl", "发票链接无效"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AppError::validation("originalUrl", "发票链接无效"));
    }
    Ok(OriginalOpenTarget::ExternalUrl(url.to_string()))
}

pub async fn open_original(state: &AppState, id: Uuid) -> Result<(), AppError> {
    let target = resolve_original_open_target(state, id).await?;
    let result = match target {
        OriginalOpenTarget::LocalPath(path) => tauri_plugin_opener::open_path(path, None::<&str>),
        OriginalOpenTarget::ExternalUrl(url) => tauri_plugin_opener::open_url(url, None::<&str>),
    };
    result.map_err(|_| AppError::External {
        service: "system_opener".to_owned(),
        retryable: false,
        message: "无法使用系统应用打开原件".to_owned(),
    })
}

pub fn parse_preview_uri(uri: &str) -> Result<(Uuid, PreviewVariant), AppError> {
    let url = url::Url::parse(uri).map_err(|_| invalid_preview_request())?;
    if url.scheme() != "invoice-file"
        || url.host_str() != Some("item")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_preview_request());
    }
    let segments = url
        .path_segments()
        .ok_or_else(invalid_preview_request)?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() != 1 {
        return Err(invalid_preview_request());
    }
    let id = Uuid::parse_str(segments[0]).map_err(|_| invalid_preview_request())?;
    let query = url.query_pairs().collect::<Vec<_>>();
    let variant = match query.as_slice() {
        [(key, value)] if key == "variant" && value == "original" => PreviewVariant::Original,
        [(key, value)] if key == "variant" && value == "normalized" => PreviewVariant::Normalized,
        _ => return Err(invalid_preview_request()),
    };
    Ok((id, variant))
}

pub async fn open_preview(
    state: &AppState,
    id: Uuid,
    variant: PreviewVariant,
) -> Result<PreviewPayload, AppError> {
    state.preview_service().open(id, variant).await
}

pub async fn open_preview_first_byte(
    state: &AppState,
    id: Uuid,
    variant: PreviewVariant,
) -> Result<PreviewRangePayload, AppError> {
    state.preview_service().open_first_byte(id, variant).await
}

pub async fn preview_response(
    state: &AppState,
    request: &tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    if request.method() != tauri::http::Method::GET {
        return preview_http_error(tauri::http::StatusCode::METHOD_NOT_ALLOWED);
    }
    let (id, variant) = match parse_preview_uri(&request.uri().to_string()) {
        Ok(parsed) => parsed,
        Err(error) => return preview_http_error(preview_error_status(&error)),
    };
    match request.headers().get(tauri::http::header::RANGE) {
        None => match open_preview(state, id, variant).await {
            Ok(payload) => preview_response_builder(tauri::http::StatusCode::OK)
                .header(tauri::http::header::CONTENT_TYPE, payload.mime_type)
                .header(tauri::http::header::CONTENT_LENGTH, payload.bytes.len())
                .header("Content-Disposition", "inline")
                .header("Content-Security-Policy", "default-src 'none'; sandbox")
                .body(payload.bytes)
                .unwrap_or_else(|_| {
                    preview_http_error(tauri::http::StatusCode::INTERNAL_SERVER_ERROR)
                }),
            Err(error) => preview_http_error(preview_error_status(&error)),
        },
        Some(range) if range.as_bytes() == b"bytes=0-0" => {
            match open_preview_first_byte(state, id, variant).await {
                Ok(payload) => preview_response_builder(tauri::http::StatusCode::PARTIAL_CONTENT)
                    .header(tauri::http::header::CONTENT_TYPE, payload.mime_type)
                    .header(tauri::http::header::CONTENT_LENGTH, 1)
                    .header(tauri::http::header::ACCEPT_RANGES, "bytes")
                    .header(
                        tauri::http::header::CONTENT_RANGE,
                        format!("bytes 0-0/{}", payload.total_length),
                    )
                    .header("Content-Disposition", "inline")
                    .header("Content-Security-Policy", "default-src 'none'; sandbox")
                    .body(vec![payload.first_byte])
                    .unwrap_or_else(|_| {
                        preview_http_error(tauri::http::StatusCode::INTERNAL_SERVER_ERROR)
                    }),
                Err(error) => preview_http_error(preview_error_status(&error)),
            }
        }
        Some(_) => preview_http_error(tauri::http::StatusCode::BAD_REQUEST),
    }
}

fn preview_response_builder(status: tauri::http::StatusCode) -> tauri::http::response::Builder {
    // The protocol is read-only and resolves only database-owned, contained preview files.
    tauri::http::Response::builder()
        .status(status)
        .header(tauri::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(tauri::http::header::CACHE_CONTROL, "no-store")
        .header("X-Content-Type-Options", "nosniff")
}

fn preview_error_status(error: &AppError) -> tauri::http::StatusCode {
    match error {
        AppError::Validation { .. } => tauri::http::StatusCode::BAD_REQUEST,
        AppError::NotFound { .. } => tauri::http::StatusCode::NOT_FOUND,
        AppError::Conflict { .. } => tauri::http::StatusCode::CONFLICT,
        AppError::External { service, .. } if service == "preview_capacity" => {
            tauri::http::StatusCode::SERVICE_UNAVAILABLE
        }
        AppError::External { .. } | AppError::Internal { .. } => {
            tauri::http::StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

fn preview_http_error(status: tauri::http::StatusCode) -> tauri::http::Response<Vec<u8>> {
    preview_response_builder(status)
        .header(
            tauri::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(b"Preview unavailable".to_vec())
        .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
}

fn invalid_preview_request() -> AppError {
    AppError::validation("previewUrl", "invalid item preview URL")
}

impl TryFrom<(ItemFilterDto, DateTime<Utc>)> for ItemFilter {
    type Error = AppError;

    fn try_from((filter, now): (ItemFilterDto, DateTime<Utc>)) -> Result<Self, Self::Error> {
        Ok(Self {
            status: filter.status,
            source_account_id: filter
                .source_account_id
                .map(|id| {
                    Uuid::parse_str(&id)
                        .map_err(|_| AppError::validation("sourceAccountId", "invalid account ID"))
                })
                .transpose()?,
            source_uid: filter.source_uid,
            created_after: filter
                .recent
                .then(|| crate::services::dashboard::recent_item_cutoff(now)),
            suggested_period: filter.suggested_period,
            category: filter.category,
            source_type: filter.source_type,
            batch_id: filter
                .batch_id
                .map(|id| {
                    Uuid::parse_str(&id)
                        .map_err(|_| AppError::validation("batchId", "invalid batch ID"))
                })
                .transpose()?,
            query: filter.query,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::error::AppError;

    #[test]
    fn preview_capacity_errors_map_to_service_unavailable() {
        let status = super::preview_error_status(&AppError::External {
            service: "preview_capacity".to_owned(),
            retryable: true,
            message: "preview capacity is exhausted".to_owned(),
        });

        assert_eq!(status, tauri::http::StatusCode::SERVICE_UNAVAILABLE);
    }
}

pub(crate) mod ipc {
    use tauri::State;
    use uuid::Uuid;

    use super::{InvoiceItemDto, ItemFilterDto, ManualImportOutcomeDto, ReviewItemInputDto};
    use crate::commands::{PageDto, PageRequestDto};
    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command(rename_all = "camelCase")]
    pub async fn list_items(
        state: State<'_, AppState>,
        filter: ItemFilterDto,
        page: Option<PageRequestDto>,
    ) -> Result<PageDto<InvoiceItemDto>, AppError> {
        super::list_page(&state, filter, page).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn get_item(
        state: State<'_, AppState>,
        item_id: Uuid,
    ) -> Result<InvoiceItemDto, AppError> {
        super::get(&state, item_id).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn open_item_original(
        state: State<'_, AppState>,
        item_id: Uuid,
    ) -> Result<(), AppError> {
        super::open_original(&state, item_id).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn import_manual_files(
        state: State<'_, AppState>,
        paths: Vec<String>,
    ) -> Result<Vec<ManualImportOutcomeDto>, AppError> {
        Ok(super::import_manual_outcomes(&state, paths).await)
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn review_item(
        state: State<'_, AppState>,
        input: ReviewItemInputDto,
    ) -> Result<InvoiceItemDto, AppError> {
        super::review(&state, input).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn resolve_duplicate(
        state: State<'_, AppState>,
        item_id: Uuid,
        keep: bool,
    ) -> Result<Option<InvoiceItemDto>, AppError> {
        super::resolve_duplicate(&state, item_id, keep).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn retry_recognition(
        state: State<'_, AppState>,
        item_id: Uuid,
    ) -> Result<InvoiceItemDto, AppError> {
        super::retry_recognition(&state, item_id).await
    }
}
