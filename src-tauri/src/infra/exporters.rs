use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use flate2::read::ZlibDecoder;
use lopdf::{Document, Object, ObjectId, Stream, dictionary};
use rust_xlsxwriter::{Format, Workbook};
use serde::Serialize;
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::db::batches::Batch;
use crate::db::items::InvoiceItem;
use crate::domain::error::AppError;
use crate::domain::model::{Category, SourceType};
use crate::infra::files::sync_directory;
use crate::infra::pdf_preflight::validate_pdf_structure;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ExportLimits {
    pub max_items: usize,
    pub max_source_file_bytes: u64,
    pub max_aggregate_source_bytes: u64,
    pub max_pdf_pages_per_item: usize,
    pub max_pdf_pages_total: usize,
    pub max_pdf_objects_per_item: usize,
    pub max_pdf_objects_total: usize,
    pub max_pdf_decoded_stream_bytes_per_item: u64,
    pub max_pdf_decoded_stream_bytes_total: u64,
    pub max_xlsx_field_units: usize,
    pub max_xlsx_total_units: usize,
}

pub(crate) const DEFAULT_EXPORT_LIMITS: ExportLimits = ExportLimits {
    max_items: 100,
    max_source_file_bytes: 50 * 1024 * 1024,
    max_aggregate_source_bytes: 256 * 1024 * 1024,
    max_pdf_pages_per_item: 100,
    max_pdf_pages_total: 500,
    max_pdf_objects_per_item: 20_000,
    max_pdf_objects_total: 100_000,
    max_pdf_decoded_stream_bytes_per_item: 100 * 1024 * 1024,
    max_pdf_decoded_stream_bytes_total: 256 * 1024 * 1024,
    max_xlsx_field_units: 32_767,
    max_xlsx_total_units: 1_000_000,
};

#[derive(Debug, Clone, Copy)]
struct PdfResourceLimits {
    max_pages_per_item: usize,
    max_pages_total: usize,
    max_objects_per_item: usize,
    max_objects_total: usize,
    max_decoded_stream_bytes_per_item: u64,
    max_decoded_stream_bytes_total: u64,
}

#[derive(Debug, Default)]
struct PdfResourceUsage {
    pages: usize,
    objects: usize,
    decoded_stream_bytes: u64,
}

#[derive(Debug, Default)]
struct SourceReadUsage {
    bytes: u64,
}

impl SourceReadUsage {
    fn record(&mut self, bytes: u64, limits: ExportLimits, field: &str) -> Result<(), AppError> {
        if bytes > limits.max_source_file_bytes {
            return Err(validation_error(field, "票据文件超过导出大小限制"));
        }
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| validation_error("export", "导出源文件总量超过限制"))?;
        if self.bytes > limits.max_aggregate_source_bytes {
            return Err(validation_error("export", "导出源文件总量超过限制"));
        }
        Ok(())
    }
}

fn validate_xlsx_text(
    field: &str,
    value: &str,
    total_units: &mut usize,
    max_field_units: usize,
    max_total_units: usize,
) -> Result<(), AppError> {
    let units = value.encode_utf16().count();
    if units > max_field_units {
        return Err(validation_error(field, "导出文本超过 Excel 单元格限制"));
    }
    let next_total = total_units
        .checked_add(units)
        .ok_or_else(|| validation_error("export", "导出文本总量超过限制"))?;
    if next_total > max_total_units {
        return Err(validation_error("export", "导出文本总量超过限制"));
    }
    *total_units = next_total;
    Ok(())
}

pub(crate) const MERGED_PDF: &str = "merged.pdf";
pub(crate) const REIMBURSEMENT_XLSX: &str = "reimbursement.xlsx";
pub(crate) const ORIGINALS_ZIP: &str = "originals.zip";
pub(crate) const MANIFEST_JSON: &str = "manifest.json";

#[derive(Debug)]
pub(crate) struct ExportItem {
    pub item: InvoiceItem,
    pub original_file: File,
    pub normalized_pdf_file: File,
    pub verified_sha256: String,
    pub archive_name: String,
}

pub(crate) struct PreparedMergedPdf {
    bytes: Vec<u8>,
    normalized_source_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest<'a> {
    batch_id: String,
    exported_at: DateTime<Utc>,
    app_version: &'static str,
    items: Vec<ManifestItem<'a>>,
    artifacts: BTreeMap<&'static str, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestItem<'a> {
    item_id: String,
    sha256: &'a str,
    file_name: &'a str,
}

pub(crate) fn generate_artifacts(
    directory: &Path,
    batch: &Batch,
    items: &mut [ExportItem],
    exported_at: DateTime<Utc>,
    limits: ExportLimits,
    merged_pdf: PreparedMergedPdf,
) -> Result<(), AppError> {
    let mut source_usage = SourceReadUsage {
        bytes: merged_pdf.normalized_source_bytes,
    };
    let merged_pdf_hash = sha256_hex(&merged_pdf.bytes);
    write_artifact(directory, MERGED_PDF, &merged_pdf.bytes)?;
    drop(merged_pdf.bytes);

    let workbook = create_workbook(batch, items, limits)?;
    let workbook_hash = sha256_hex(&workbook);
    write_artifact(directory, REIMBURSEMENT_XLSX, &workbook)?;
    drop(workbook);

    let originals_hash = write_originals_zip(directory, items, limits, &mut source_usage)?;

    let artifacts = BTreeMap::from([
        (MERGED_PDF, merged_pdf_hash),
        (REIMBURSEMENT_XLSX, workbook_hash),
        (ORIGINALS_ZIP, originals_hash),
    ]);
    let manifest = Manifest {
        batch_id: batch.id.to_string(),
        exported_at,
        app_version: env!("CARGO_PKG_VERSION"),
        items: items
            .iter()
            .map(|item| ManifestItem {
                item_id: item.item.id.to_string(),
                sha256: &item.verified_sha256,
                file_name: &item.archive_name,
            })
            .collect(),
        artifacts,
    };
    let manifest = serde_json::to_vec_pretty(&manifest)
        .map_err(|_| internal_error("failed to serialize export manifest"))?;
    write_artifact(directory, MANIFEST_JSON, &manifest)?;
    sync_directory(directory).map_err(|_| internal_error("failed to sync staged export"))
}

#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
pub(crate) fn publish_directory(staging: &Path, destination: &Path) -> Result<(), AppError> {
    publish_directory_with_sync(staging, destination, sync_publication_parents)
}

#[cfg(unix)]
fn publish_directory_with_sync(
    staging: &Path,
    destination: &Path,
    sync_parents: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), AppError> {
    rename_directory_noreplace(staging, destination)?;
    if sync_parents(staging, destination).is_ok() {
        return Ok(());
    }

    if rename_directory_noreplace(destination, staging).is_err() {
        return Err(AppError::External {
            service: "filesystem_sync".to_owned(),
            retryable: false,
            message: "export publication sync failed and rollback also failed; manual recovery is required"
                .to_owned(),
        });
    }
    sync_publication_parents(staging, destination).map_err(|_| AppError::External {
        service: "filesystem_sync".to_owned(),
        retryable: false,
        message: "export publication was rolled back but rollback durability is incomplete"
            .to_owned(),
    })?;
    Err(internal_error(
        "failed to sync export publication; publication was rolled back",
    ))
}

#[cfg(unix)]
fn rename_directory_noreplace(staging: &Path, destination: &Path) -> Result<(), AppError> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    use rustix::io::Errno;

    match renameat_with(CWD, staging, CWD, destination, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(error) if error == Errno::EXIST => Err(AppError::Conflict {
            message: "同名报销包已存在".to_owned(),
        }),
        Err(error) if error == Errno::NOSYS || error == Errno::INVAL => Err(AppError::Internal {
            message: "atomic no-replace export publication is unsupported".to_owned(),
        }),
        Err(_) => Err(internal_error("failed to publish export package")),
    }
}

#[cfg(windows)]
pub(crate) fn publish_directory(staging: &Path, destination: &Path) -> Result<(), AppError> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let staging = staging
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe { MoveFileExW(staging.as_ptr(), destination.as_ptr(), 0) } != 0 {
        return Ok(());
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(code) if code == ERROR_ALREADY_EXISTS as i32 || code == ERROR_FILE_EXISTS as i32 => {
            Err(AppError::Conflict {
                message: "同名报销包已存在".to_owned(),
            })
        }
        _ => Err(internal_error("failed to publish export package")),
    }
}

#[cfg(not(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    windows,
)))]
pub(crate) fn publish_directory(_staging: &Path, _destination: &Path) -> Result<(), AppError> {
    Err(AppError::Internal {
        message: "atomic no-replace export publication is unsupported".to_owned(),
    })
}

#[cfg(unix)]
fn sync_publication_parents(staging: &Path, destination: &Path) -> std::io::Result<()> {
    let staging_parent = staging
        .parent()
        .ok_or_else(|| std::io::Error::other("export staging directory has no parent"))?;
    let destination_parent = destination
        .parent()
        .ok_or_else(|| std::io::Error::other("export destination has no parent"))?;
    sync_directory(staging_parent).and_then(|()| sync_directory(destination_parent))
}

pub(crate) fn prepare_merged_pdf(
    items: &mut [ExportItem],
    limits: ExportLimits,
) -> Result<PreparedMergedPdf, AppError> {
    let mut output = Document::with_version("1.5");
    let mut pages = Vec::<ObjectId>::new();
    let mut usage = PdfResourceUsage::default();
    let mut source_usage = SourceReadUsage::default();
    let pdf_limits = PdfResourceLimits {
        max_pages_per_item: limits.max_pdf_pages_per_item,
        max_pages_total: limits.max_pdf_pages_total,
        max_objects_per_item: limits.max_pdf_objects_per_item,
        max_objects_total: limits.max_pdf_objects_total,
        max_decoded_stream_bytes_per_item: limits.max_pdf_decoded_stream_bytes_per_item,
        max_decoded_stream_bytes_total: limits.max_pdf_decoded_stream_bytes_total,
    };

    for item in items {
        let read_limit = limits.max_source_file_bytes.saturating_add(1);
        item.normalized_pdf_file
            .seek(SeekFrom::Start(0))
            .map_err(|_| validation_error("normalizedPdf", "归一化 PDF 无法读取"))?;
        let mut reader = Read::by_ref(&mut item.normalized_pdf_file).take(read_limit);
        let mut raw_pdf = Vec::new();
        reader
            .read_to_end(&mut raw_pdf)
            .map_err(|_| validation_error("normalizedPdf", "归一化 PDF 无法读取"))?;
        source_usage.record(raw_pdf.len() as u64, limits, "normalizedPdf")?;
        validate_pdf_structure(&raw_pdf)?;
        let mut document = Document::load_mem(&raw_pdf)
            .map_err(|_| validation_error("normalizedPdf", "归一化 PDF 无法读取"))?;
        drop(raw_pdf);
        if document.is_encrypted() || document.encryption_state.is_some() {
            return Err(validation_error("normalizedPdf", "归一化 PDF 不得加密"));
        }
        if document.get_pages().is_empty() {
            return Err(validation_error("normalizedPdf", "归一化 PDF 不包含页面"));
        }
        validate_pdf_resources(&document, &mut usage, pdf_limits)?;
        flatten_page_attributes(&mut document)?;

        document.renumber_objects_with(output.max_id + 1);
        let document_pages = document.get_pages().into_values().collect::<Vec<_>>();
        output.max_id = document.max_id;
        for (id, object) in document.objects {
            if !matches!(
                object.type_name(),
                Ok(b"Catalog" | b"Pages" | b"Outlines" | b"Outline")
            ) {
                output.objects.insert(id, object);
            }
        }
        pages.extend(document_pages);
    }

    let pages_id = output.add_object(dictionary! {
        "Type" => "Pages",
        "Kids" => pages.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
        "Count" => i64::try_from(pages.len())
            .map_err(|_| internal_error("merged PDF page count overflow"))?,
    });
    for page_id in &pages {
        output
            .get_object_mut(*page_id)
            .and_then(Object::as_dict_mut)
            .map_err(|_| validation_error("normalizedPdf", "归一化 PDF 页面结构无效"))?
            .set("Parent", pages_id);
    }
    let catalog_id = output.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    output.trailer.set("Root", catalog_id);
    output.renumber_objects();

    let mut bytes = Vec::new();
    output
        .save_to(&mut bytes)
        .map_err(|_| internal_error("failed to write merged PDF"))?;
    Document::load_mem(&bytes).map_err(|_| internal_error("failed to validate merged PDF"))?;
    Ok(PreparedMergedPdf {
        bytes,
        normalized_source_bytes: source_usage.bytes,
    })
}

fn validate_pdf_resources(
    document: &Document,
    usage: &mut PdfResourceUsage,
    limits: PdfResourceLimits,
) -> Result<(), AppError> {
    let pages = document.get_pages().len();
    if pages > limits.max_pages_per_item {
        return Err(validation_error(
            "normalizedPdf",
            "单张票据 PDF 页数超过导出限制",
        ));
    }
    let total_pages = usage
        .pages
        .checked_add(pages)
        .ok_or_else(|| validation_error("normalizedPdf", "PDF 页数超过导出限制"))?;
    if total_pages > limits.max_pages_total {
        return Err(validation_error("normalizedPdf", "PDF 总页数超过导出限制"));
    }

    let objects = document.objects.len();
    if objects > limits.max_objects_per_item {
        return Err(validation_error(
            "normalizedPdf",
            "单张票据 PDF 对象数超过导出限制",
        ));
    }
    let total_objects = usage
        .objects
        .checked_add(objects)
        .ok_or_else(|| validation_error("normalizedPdf", "PDF 对象数超过导出限制"))?;
    if total_objects > limits.max_objects_total {
        return Err(validation_error(
            "normalizedPdf",
            "PDF 总对象数超过导出限制",
        ));
    }

    let mut decoded_stream_bytes = 0_u64;
    for object in document.objects.values() {
        let Object::Stream(stream) = object else {
            continue;
        };
        let item_remaining = limits
            .max_decoded_stream_bytes_per_item
            .saturating_sub(decoded_stream_bytes);
        let package_remaining = remaining_decoded_stream_budget(
            limits.max_decoded_stream_bytes_total,
            usage.decoded_stream_bytes,
            decoded_stream_bytes,
        )?;
        let decoded_len = bounded_stream_size(stream, item_remaining.min(package_remaining))?;
        decoded_stream_bytes = decoded_stream_bytes
            .checked_add(decoded_len)
            .ok_or_else(|| validation_error("normalizedPdf", "PDF 数据流超过导出限制"))?;
        if decoded_stream_bytes > limits.max_decoded_stream_bytes_per_item {
            return Err(validation_error(
                "normalizedPdf",
                "单张票据 PDF 解码数据超过导出限制",
            ));
        }
    }
    let total_decoded_stream_bytes = usage
        .decoded_stream_bytes
        .checked_add(decoded_stream_bytes)
        .ok_or_else(|| validation_error("normalizedPdf", "PDF 数据流超过导出限制"))?;
    if total_decoded_stream_bytes > limits.max_decoded_stream_bytes_total {
        return Err(validation_error(
            "normalizedPdf",
            "PDF 解码数据总量超过导出限制",
        ));
    }

    usage.pages = total_pages;
    usage.objects = total_objects;
    usage.decoded_stream_bytes = total_decoded_stream_bytes;
    Ok(())
}

fn remaining_decoded_stream_budget(
    limit: u64,
    committed: u64,
    current: u64,
) -> Result<u64, AppError> {
    let used = committed
        .checked_add(current)
        .ok_or_else(|| validation_error("normalizedPdf", "PDF 数据流超过导出限制"))?;
    Ok(limit.saturating_sub(used))
}

fn bounded_stream_size(stream: &Stream, limit: u64) -> Result<u64, AppError> {
    let filters = match stream.filters() {
        Ok(filters) => filters,
        Err(_) if stream.dict.get(b"Filter").is_err() => {
            return bounded_opaque_size(stream.content.len(), limit);
        }
        Err(_) => return Err(unsupported_stream_filter()),
    };
    if filters.len() != 1 {
        return Err(unsupported_stream_filter());
    }
    match filters[0] {
        b"FlateDecode" | b"Fl" => bounded_flate_size(&stream.content, limit),
        // These codecs remain opaque in lopdf's load/renumber/save path. The exporter never calls
        // lopdf's content decoders for them, so their stored bytes are the allocation bound.
        b"DCTDecode" | b"DCT" | b"JPXDecode" | b"CCITTFaxDecode" | b"CCF" | b"JBIG2Decode"
        | b"Crypt" => bounded_opaque_size(stream.content.len(), limit),
        // lopdf's LZW and ASCII85 helpers allocate complete output buffers. Fail closed until a
        // bounded streaming implementation is available for these filters.
        _ => Err(unsupported_stream_filter()),
    }
}

fn bounded_flate_size(content: &[u8], limit: u64) -> Result<u64, AppError> {
    let decoder = ZlibDecoder::new(content);
    let mut bounded = decoder.take(limit.saturating_add(1));
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = bounded
            .read(&mut buffer)
            .map_err(|_| validation_error("normalizedPdf", "PDF 数据流无法解码"))?;
        if read == 0 {
            return Ok(total);
        }
        total = total.saturating_add(read as u64);
        if total > limit {
            return Err(validation_error(
                "normalizedPdf",
                "单张票据 PDF 解码数据超过导出限制",
            ));
        }
    }
}

fn bounded_opaque_size(size: usize, limit: u64) -> Result<u64, AppError> {
    let size = size as u64;
    if size > limit {
        return Err(validation_error(
            "normalizedPdf",
            "单张票据 PDF 解码数据超过导出限制",
        ));
    }
    Ok(size)
}

fn unsupported_stream_filter() -> AppError {
    validation_error("normalizedPdf", "PDF 数据流过滤器不受安全导出支持")
}

fn flatten_page_attributes(document: &mut Document) -> Result<(), AppError> {
    let page_ids = document.get_pages().into_values().collect::<Vec<_>>();
    for page_id in page_ids {
        for key in [b"MediaBox".as_slice(), b"CropBox", b"Resources", b"Rotate"] {
            let value = inherited_page_attribute(document, page_id, key);
            if let Some(value) = value {
                document
                    .get_object_mut(page_id)
                    .and_then(Object::as_dict_mut)
                    .map_err(|_| validation_error("normalizedPdf", "归一化 PDF 页面结构无效"))?
                    .set(key, value);
            }
        }
        let has_media_box = document
            .get_object(page_id)
            .and_then(Object::as_dict)
            .is_ok_and(|page| page.get(b"MediaBox").is_ok());
        if !has_media_box {
            return Err(validation_error("normalizedPdf", "归一化 PDF 页面尺寸无效"));
        }
    }
    Ok(())
}

fn inherited_page_attribute(document: &Document, page_id: ObjectId, key: &[u8]) -> Option<Object> {
    let mut current_id = page_id;
    for _ in 0..128 {
        let dictionary = document.get_object(current_id).ok()?.as_dict().ok()?;
        if let Ok(value) = dictionary.get(key) {
            return Some(value.clone());
        }
        current_id = dictionary.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

fn create_workbook(
    batch: &Batch,
    items: &[ExportItem],
    limits: ExportLimits,
) -> Result<Vec<u8>, AppError> {
    const HEADERS: [&str; 11] = [
        "开票日期",
        "建议归属时间",
        "最终归属批次",
        "分类",
        "金额",
        "城市",
        "公司主体",
        "来源",
        "备注",
        "事项标签",
        "项目标签",
    ];
    let mut workbook = Workbook::new();
    let currency = Format::new().set_num_format("¥#,##0.00");
    let worksheet = workbook.add_worksheet();
    let mut text_units = 0_usize;
    for (column, header) in HEADERS.iter().enumerate() {
        worksheet
            .write_string(0, column as u16, *header)
            .map_err(|_| internal_error("failed to write reimbursement header"))?;
    }

    for (index, export_item) in items.iter().enumerate() {
        let item = &export_item.item;
        let row = u32::try_from(index + 1)
            .map_err(|_| internal_error("reimbursement row count overflow"))?;
        let amount_cents = item
            .amount_cents
            .ok_or_else(|| validation_error("amountCents", "票据金额不能为空"))?;
        let amount = exact_xlsx_amount(amount_cents)?;
        write_string(
            worksheet,
            row,
            0,
            item.invoice_date.map(|date| date.to_string()),
            "invoiceDate",
            &mut text_units,
            limits,
        )?;
        write_string(
            worksheet,
            row,
            1,
            item.suggested_period.clone(),
            "suggestedPeriod",
            &mut text_units,
            limits,
        )?;
        write_string(
            worksheet,
            row,
            2,
            Some(batch.name.clone()),
            "batchName",
            &mut text_units,
            limits,
        )?;
        write_string(
            worksheet,
            row,
            3,
            item.final_category.map(category_label).map(str::to_owned),
            "finalCategory",
            &mut text_units,
            limits,
        )?;
        worksheet
            .write_number_with_format(row, 4, amount, &currency)
            .map_err(|_| internal_error("failed to write reimbursement amount"))?;
        write_string(
            worksheet,
            row,
            5,
            item.city.clone(),
            "city",
            &mut text_units,
            limits,
        )?;
        write_string(
            worksheet,
            row,
            6,
            item.company.clone(),
            "company",
            &mut text_units,
            limits,
        )?;
        write_string(
            worksheet,
            row,
            7,
            Some(source_label(item.source_type).to_owned()),
            "sourceType",
            &mut text_units,
            limits,
        )?;
        write_string(
            worksheet,
            row,
            8,
            item.note.clone(),
            "note",
            &mut text_units,
            limits,
        )?;
        write_string(
            worksheet,
            row,
            9,
            item.event_tag.clone(),
            "eventTag",
            &mut text_units,
            limits,
        )?;
        write_string(
            worksheet,
            row,
            10,
            item.project_tag.clone(),
            "projectTag",
            &mut text_units,
            limits,
        )?;
    }

    workbook
        .save_to_buffer()
        .map_err(|_| internal_error("failed to create reimbursement workbook"))
}

fn exact_xlsx_amount(cents: i64) -> Result<f64, AppError> {
    let value = cents as f64 / 100.0;
    let recovered = (value * 100.0).round();
    const I64_MAX_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
    if !value.is_finite()
        || !recovered.is_finite()
        || recovered < i64::MIN as f64
        || recovered >= I64_MAX_EXCLUSIVE
        || recovered as i64 != cents
    {
        return Err(validation_error("amountCents", "票据金额无法精确导出"));
    }
    Ok(value)
}

fn write_string(
    worksheet: &mut rust_xlsxwriter::Worksheet,
    row: u32,
    column: u16,
    value: Option<String>,
    field: &str,
    total_units: &mut usize,
    limits: ExportLimits,
) -> Result<(), AppError> {
    if let Some(value) = value {
        validate_xlsx_text(
            field,
            &value,
            total_units,
            limits.max_xlsx_field_units,
            limits.max_xlsx_total_units,
        )?;
        worksheet
            .write_string(row, column, value)
            .map_err(|_| internal_error("failed to write reimbursement value"))?;
    }
    Ok(())
}

fn write_originals_zip(
    directory: &Path,
    items: &mut [ExportItem],
    limits: ExportLimits,
    source_usage: &mut SourceReadUsage,
) -> Result<String, AppError> {
    let temporary = directory.join(format!(".{ORIGINALS_ZIP}.part"));
    let destination = directory.join(ORIGINALS_ZIP);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| internal_error("failed to create originals archive"))?;
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o600);
    for item in items {
        archive
            .start_file(&item.archive_name, options)
            .map_err(|_| internal_error("failed to create originals archive entry"))?;
        item.original_file
            .seek(SeekFrom::Start(0))
            .map_err(|_| validation_error("original", "票据文件无法读取"))?;
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let remaining = limits
                .max_source_file_bytes
                .saturating_add(1)
                .saturating_sub(total);
            if remaining == 0 {
                return Err(validation_error("original", "票据文件超过导出大小限制"));
            }
            let requested = buffer.len().min(remaining as usize);
            let read = item
                .original_file
                .read(&mut buffer[..requested])
                .map_err(|_| validation_error("original", "票据文件无法读取"))?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            if total > limits.max_source_file_bytes {
                return Err(validation_error("original", "票据文件超过导出大小限制"));
            }
            source_usage.record(read as u64, limits, "original")?;
            hasher.update(&buffer[..read]);
            archive
                .write_all(&buffer[..read])
                .map_err(|_| internal_error("failed to write originals archive entry"))?;
        }
        let verified_sha256 = format!("{:x}", hasher.finalize());
        if verified_sha256 != item.item.sha256 {
            return Err(validation_error("original", "票据原件完整性校验失败"));
        }
        item.verified_sha256 = verified_sha256;
    }
    let file = archive
        .finish()
        .map_err(|_| internal_error("failed to finish originals archive"))?;
    file.sync_all()
        .map_err(|_| internal_error("failed to sync originals archive"))?;
    drop(file);
    fs::rename(&temporary, &destination)
        .map_err(|_| internal_error("failed to finalize originals archive"))?;
    File::open(&destination)
        .and_then(|file| file.sync_all())
        .map_err(|_| internal_error("failed to sync originals archive"))?;
    sha256_file(&destination)
}

fn sha256_file(path: &Path) -> Result<String, AppError> {
    let mut file =
        File::open(path).map_err(|_| internal_error("failed to hash export artifact"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| internal_error("failed to hash export artifact"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn write_artifact(directory: &Path, name: &str, bytes: &[u8]) -> Result<(), AppError> {
    let temporary = directory.join(format!(".{name}.part"));
    let destination = directory.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| internal_error("failed to create staged export artifact"))?;
    file.write_all(bytes)
        .map_err(|_| internal_error("failed to write staged export artifact"))?;
    file.sync_all()
        .map_err(|_| internal_error("failed to sync staged export artifact"))?;
    drop(file);
    fs::rename(&temporary, &destination)
        .map_err(|_| internal_error("failed to finalize staged export artifact"))?;
    File::open(&destination)
        .and_then(|file| file.sync_all())
        .map_err(|_| internal_error("failed to sync export artifact"))
}

fn category_label(category: Category) -> &'static str {
    match category {
        Category::Transport => "交通",
        Category::Dining => "餐饮",
        Category::Accommodation => "住宿",
        Category::Hospitality => "招待",
    }
}

fn source_label(source: SourceType) -> &'static str {
    match source {
        SourceType::Email => "邮件",
        SourceType::ManualUpload => "手动上传",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validation_error(field: &str, message: &str) -> AppError {
    AppError::validation(field, message)
}

fn internal_error(message: &str) -> AppError {
    AppError::Internal {
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::io;

    use lopdf::{Document, Object, Stream, dictionary};

    #[cfg(unix)]
    use super::publish_directory_with_sync;
    use super::{
        PdfResourceLimits, PdfResourceUsage, exact_xlsx_amount, remaining_decoded_stream_budget,
        validate_pdf_resources, validate_xlsx_text,
    };

    #[test]
    fn xlsx_amount_round_trips_regular_cent_values() {
        for cents in [0_i64, 1, 99, 100, 12_345, 4_485_000_001] {
            let value = exact_xlsx_amount(cents).unwrap();
            assert_eq!((value * 100.0).round() as i64, cents);
        }
    }

    #[test]
    fn pdf_page_budget_is_enforced() {
        let document = pdf_with_pages(2);
        let mut usage = PdfResourceUsage::default();

        let error = validate_pdf_resources(&document, &mut usage, pdf_limits(1)).unwrap_err();

        assert!(
            matches!(error, crate::domain::error::AppError::Validation { field, .. } if field == "normalizedPdf")
        );
    }

    #[test]
    fn pdf_object_budget_is_enforced() {
        let mut document = pdf_with_pages(1);
        document.add_object(Object::Null);
        let mut usage = PdfResourceUsage::default();
        let mut limits = pdf_limits(10);
        limits.max_objects_per_item = document.objects.len() - 1;

        let error = validate_pdf_resources(&document, &mut usage, limits).unwrap_err();

        assert!(
            matches!(error, crate::domain::error::AppError::Validation { field, .. } if field == "normalizedPdf")
        );
    }

    #[test]
    fn pdf_decoded_stream_budget_is_enforced() {
        let mut document = pdf_with_pages(1);
        document.add_object(Stream::new(dictionary! {}, vec![0_u8; 17]));
        let mut usage = PdfResourceUsage::default();
        let mut limits = pdf_limits(10);
        limits.max_decoded_stream_bytes_per_item = 16;

        let error = validate_pdf_resources(&document, &mut usage, limits).unwrap_err();

        assert!(
            matches!(error, crate::domain::error::AppError::Validation { field, .. } if field == "normalizedPdf")
        );
    }

    #[test]
    fn pdf_decoded_stream_budget_overflow_is_rejected() {
        let error = remaining_decoded_stream_budget(u64::MAX, u64::MAX, 1).unwrap_err();

        assert!(
            matches!(error, crate::domain::error::AppError::Validation { field, .. } if field == "normalizedPdf")
        );
    }

    #[test]
    fn xlsx_text_field_and_aggregate_budgets_are_enforced() {
        let mut total = 0;
        let field_error = validate_xlsx_text("note", "four", &mut total, 3, 10).unwrap_err();
        assert!(
            matches!(field_error, crate::domain::error::AppError::Validation { field, .. } if field == "note")
        );
        assert_eq!(total, 0);

        validate_xlsx_text("note", "four", &mut total, 4, 7).unwrap();
        let aggregate_error = validate_xlsx_text("company", "four", &mut total, 4, 7).unwrap_err();
        assert!(
            matches!(aggregate_error, crate::domain::error::AppError::Validation { field, .. } if field == "export")
        );
        assert_eq!(total, 4);
    }

    fn pdf_limits(max_pages_per_item: usize) -> PdfResourceLimits {
        PdfResourceLimits {
            max_pages_per_item,
            max_pages_total: usize::MAX,
            max_objects_per_item: usize::MAX,
            max_objects_total: usize::MAX,
            max_decoded_stream_bytes_per_item: u64::MAX,
            max_decoded_stream_bytes_total: u64::MAX,
        }
    }

    fn pdf_with_pages(page_count: usize) -> Document {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_ids = (0..page_count)
            .map(|_| {
                document.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), 100.into(), 150.into()],
                })
            })
            .collect::<Vec<_>>();
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
                "Count" => page_count as i64,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        document
    }

    #[test]
    fn xlsx_amount_rejects_collapsed_values_near_f64_integer_limit() {
        for cents in [
            9_007_199_254_740_984_i64,
            9_007_199_254_740_986,
            9_007_199_254_740_988,
            9_007_199_254_740_989,
            9_007_199_254_740_991,
        ] {
            assert!(exact_xlsx_amount(cents).is_ok(), "should preserve {cents}");
        }
        for cents in [
            9_007_199_254_740_985_i64,
            9_007_199_254_740_987,
            9_007_199_254_740_990,
            i64::MAX,
        ] {
            assert!(exact_xlsx_amount(cents).is_err(), "must reject {cents}");
        }

        let collapsed_low = 9_007_199_254_740_990_i64 as f64 / 100.0;
        let adjacent_high = 9_007_199_254_740_991_i64 as f64 / 100.0;
        assert_eq!(collapsed_low, adjacent_high);
    }

    #[cfg(unix)]
    #[test]
    fn publication_sync_failure_rolls_directory_back_to_staging() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let destination = directory.path().join("destination");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("artifact"), b"content").unwrap();

        let error = publish_directory_with_sync(&staging, &destination, |_, _| {
            Err(io::Error::other("injected publication sync failure"))
        })
        .unwrap_err();

        assert!(matches!(
            error,
            crate::domain::error::AppError::Internal { .. }
        ));
        assert!(staging.join("artifact").is_file());
        assert!(!destination.exists());
    }
}
