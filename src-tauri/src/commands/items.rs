use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;
use uuid::Uuid;

use crate::db::items::{InvoiceItem, ItemFilter};
use crate::domain::error::AppError;
use crate::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, ItemStatus, RecognitionStatus, SourceType,
};
use crate::services::items::ItemReview;
use crate::state::AppState;

const MAX_PREVIEW_BYTES: u64 = crate::services::import::MAX_FILE_SIZE;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ItemFilterDto {
    pub status: Option<ItemStatus>,
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
    pub note: Option<String>,
    pub event_tag: Option<String>,
    pub project_tag: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<InvoiceItem> for InvoiceItemDto {
    fn from(item: InvoiceItem) -> Self {
        let status = item.status();
        let variant = if item.normalized_pdf_path.is_some() {
            "normalized"
        } else {
            "original"
        };
        Self {
            id: item.id.to_string(),
            original_name: item.original_name,
            preview_url: format!("invoice-file://item/{}?variant={variant}", item.id),
            source_type: item.source_type,
            source_account_id: item.source_account_id.map(|id| id.to_string()),
            fetched_at: item.fetched_at.to_rfc3339(),
            invoice_date: item.invoice_date.map(|date| date.to_string()),
            suggested_period: item.suggested_period,
            batch_id: item.batch_id.map(|id| id.to_string()),
            suggested_category: item.suggested_category,
            final_category: item.final_category,
            amount_cents: item.amount_cents,
            currency: item.currency,
            city: item.city,
            company: item.company,
            status,
            recognition_status: item.recognition_status,
            confirmation_status: item.confirmation_status,
            dedupe_status: item.dedupe_status,
            note: item.note,
            event_tag: item.event_tag,
            project_tag: item.project_tag,
            created_at: item.created_at.to_rfc3339(),
            updated_at: item.updated_at.to_rfc3339(),
        }
    }
}

pub async fn list(
    state: &AppState,
    filter: ItemFilterDto,
) -> Result<Vec<InvoiceItemDto>, AppError> {
    let items = state
        .item_service()
        .list(ItemFilter::try_from(filter)?)
        .await?;
    Ok(items.into_iter().map(InvoiceItemDto::from).collect())
}

pub async fn get(state: &AppState, id: Uuid) -> Result<InvoiceItemDto, AppError> {
    state.item_service().get(id).await.map(InvoiceItemDto::from)
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
                .await
                .map(InvoiceItemDto::from)?,
        );
    }
    Ok(items)
}

pub async fn review(
    state: &AppState,
    input: ReviewItemInputDto,
) -> Result<InvoiceItemDto, AppError> {
    state
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
        .await
        .map(InvoiceItemDto::from)
}

pub async fn resolve_duplicate(
    state: &AppState,
    id: Uuid,
    keep: bool,
) -> Result<Option<InvoiceItemDto>, AppError> {
    state
        .item_service()
        .resolve_duplicate(id, keep)
        .await
        .map(|item| item.map(InvoiceItemDto::from))
}

pub async fn retry_recognition(state: &AppState, id: Uuid) -> Result<InvoiceItemDto, AppError> {
    state
        .recognition_service()
        .retry(id)
        .await
        .map(InvoiceItemDto::from)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewVariant {
    Original,
    Normalized,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewPayload {
    pub bytes: Vec<u8>,
    pub mime_type: &'static str,
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
    let item = state.item_service().get(id).await?;
    let (path, root, mime_type) = match variant {
        PreviewVariant::Original => (
            PathBuf::from(item.original_path),
            state.paths().originals.clone(),
            preview_mime_type(&item.mime_type)?,
        ),
        PreviewVariant::Normalized => (
            PathBuf::from(item.normalized_pdf_path.ok_or_else(|| AppError::NotFound {
                entity: "item_preview".to_owned(),
                message: "item preview is unavailable".to_owned(),
            })?),
            state.paths().normalized.clone(),
            "application/pdf",
        ),
    };

    tokio::task::spawn_blocking(move || read_preview(path, root, mime_type))
        .await
        .map_err(|_| preview_unavailable())?
}

pub async fn preview_response(
    state: &AppState,
    method: &str,
    uri: &str,
) -> tauri::http::Response<Vec<u8>> {
    if method != "GET" {
        return preview_http_error(tauri::http::StatusCode::METHOD_NOT_ALLOWED);
    }
    let result = match parse_preview_uri(uri) {
        Ok((id, variant)) => open_preview(state, id, variant).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(payload) => tauri::http::Response::builder()
            .status(tauri::http::StatusCode::OK)
            .header(tauri::http::header::CONTENT_TYPE, payload.mime_type)
            .header(tauri::http::header::CONTENT_LENGTH, payload.bytes.len())
            .header(tauri::http::header::CACHE_CONTROL, "no-store")
            .header("X-Content-Type-Options", "nosniff")
            .header("Content-Disposition", "inline")
            .header("Content-Security-Policy", "default-src 'none'; sandbox")
            .body(payload.bytes)
            .unwrap_or_else(|_| preview_http_error(tauri::http::StatusCode::INTERNAL_SERVER_ERROR)),
        Err(AppError::Validation { .. }) => {
            preview_http_error(tauri::http::StatusCode::BAD_REQUEST)
        }
        Err(AppError::NotFound { .. }) => preview_http_error(tauri::http::StatusCode::NOT_FOUND),
        Err(AppError::Conflict { .. }) => preview_http_error(tauri::http::StatusCode::CONFLICT),
        Err(AppError::External { .. } | AppError::Internal { .. }) => {
            preview_http_error(tauri::http::StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

fn preview_http_error(status: tauri::http::StatusCode) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(status)
        .header(
            tauri::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .header(tauri::http::header::CACHE_CONTROL, "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .body(b"Preview unavailable".to_vec())
        .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
}

fn read_preview(
    path: PathBuf,
    root: PathBuf,
    mime_type: &'static str,
) -> Result<PreviewPayload, AppError> {
    let canonical_root = std::fs::canonicalize(&root).map_err(|_| preview_unavailable())?;
    let canonical_path = std::fs::canonicalize(&path).map_err(|_| preview_unavailable())?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(preview_unavailable());
    }

    let mut opened = crate::services::export::open_contained_regular_file(&path, &root, "preview")
        .map_err(|_| preview_unavailable())?;
    if opened.length > MAX_PREVIEW_BYTES {
        return Err(preview_unavailable());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(opened.length).unwrap_or(0));
    opened
        .file
        .by_ref()
        .take(MAX_PREVIEW_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| preview_unavailable())?;
    if bytes.len() as u64 > MAX_PREVIEW_BYTES {
        return Err(preview_unavailable());
    }
    Ok(PreviewPayload { bytes, mime_type })
}

fn preview_mime_type(mime_type: &str) -> Result<&'static str, AppError> {
    match mime_type {
        "application/pdf" => Ok("application/pdf"),
        "image/png" => Ok("image/png"),
        "image/jpeg" => Ok("image/jpeg"),
        _ => Err(AppError::validation(
            "preview",
            "item type cannot be previewed",
        )),
    }
}

fn invalid_preview_request() -> AppError {
    AppError::validation("previewUrl", "invalid item preview URL")
}

fn preview_unavailable() -> AppError {
    AppError::NotFound {
        entity: "item_preview".to_owned(),
        message: "item preview is unavailable".to_owned(),
    }
}

impl TryFrom<ItemFilterDto> for ItemFilter {
    type Error = AppError;

    fn try_from(filter: ItemFilterDto) -> Result<Self, Self::Error> {
        Ok(Self {
            status: filter.status,
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

pub(crate) mod ipc {
    use tauri::State;
    use uuid::Uuid;

    use super::{InvoiceItemDto, ItemFilterDto, ReviewItemInputDto};
    use crate::domain::error::AppError;
    use crate::state::AppState;

    #[tauri::command(rename_all = "camelCase")]
    pub async fn list_items(
        state: State<'_, AppState>,
        filter: ItemFilterDto,
    ) -> Result<Vec<InvoiceItemDto>, AppError> {
        super::list(&state, filter).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn get_item(
        state: State<'_, AppState>,
        item_id: Uuid,
    ) -> Result<InvoiceItemDto, AppError> {
        super::get(&state, item_id).await
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn import_manual_files(
        state: State<'_, AppState>,
        paths: Vec<String>,
    ) -> Result<Vec<InvoiceItemDto>, AppError> {
        super::import_manual(&state, paths).await
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
