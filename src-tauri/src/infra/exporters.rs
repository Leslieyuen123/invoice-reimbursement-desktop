use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use lopdf::{Document, Object, ObjectId, dictionary};
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

pub(crate) const MERGED_PDF: &str = "merged.pdf";
pub(crate) const REIMBURSEMENT_XLSX: &str = "reimbursement.xlsx";
pub(crate) const ORIGINALS_ZIP: &str = "originals.zip";
pub(crate) const MANIFEST_JSON: &str = "manifest.json";

#[derive(Debug, Clone)]
pub(crate) struct ExportItem {
    pub item: InvoiceItem,
    pub original_bytes: Vec<u8>,
    pub normalized_pdf_bytes: Vec<u8>,
    pub archive_name: String,
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
    items: &[ExportItem],
    exported_at: DateTime<Utc>,
) -> Result<(), AppError> {
    let merged_pdf = merge_pdfs(items)?;
    let workbook = create_workbook(batch, items)?;
    let originals = create_originals_zip(items)?;

    write_artifact(directory, MERGED_PDF, &merged_pdf)?;
    write_artifact(directory, REIMBURSEMENT_XLSX, &workbook)?;
    write_artifact(directory, ORIGINALS_ZIP, &originals)?;

    let artifacts = BTreeMap::from([
        (MERGED_PDF, sha256_hex(&merged_pdf)),
        (REIMBURSEMENT_XLSX, sha256_hex(&workbook)),
        (ORIGINALS_ZIP, sha256_hex(&originals)),
    ]);
    let manifest = Manifest {
        batch_id: batch.id.to_string(),
        exported_at,
        app_version: env!("CARGO_PKG_VERSION"),
        items: items
            .iter()
            .map(|item| ManifestItem {
                item_id: item.item.id.to_string(),
                sha256: &item.item.sha256,
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

fn merge_pdfs(items: &[ExportItem]) -> Result<Vec<u8>, AppError> {
    let mut output = Document::with_version("1.5");
    let mut pages = Vec::<ObjectId>::new();

    for item in items {
        let mut document = Document::load_mem(&item.normalized_pdf_bytes)
            .map_err(|_| validation_error("normalizedPdf", "归一化 PDF 无法读取"))?;
        if document.is_encrypted() || document.encryption_state.is_some() {
            return Err(validation_error("normalizedPdf", "归一化 PDF 不得加密"));
        }
        if document.get_pages().is_empty() {
            return Err(validation_error("normalizedPdf", "归一化 PDF 不包含页面"));
        }
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
    Ok(bytes)
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

fn create_workbook(batch: &Batch, items: &[ExportItem]) -> Result<Vec<u8>, AppError> {
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
        )?;
        write_string(worksheet, row, 1, item.suggested_period.clone())?;
        write_string(worksheet, row, 2, Some(batch.name.clone()))?;
        write_string(
            worksheet,
            row,
            3,
            item.final_category.map(category_label).map(str::to_owned),
        )?;
        worksheet
            .write_number_with_format(row, 4, amount, &currency)
            .map_err(|_| internal_error("failed to write reimbursement amount"))?;
        write_string(worksheet, row, 5, item.city.clone())?;
        write_string(worksheet, row, 6, item.company.clone())?;
        write_string(
            worksheet,
            row,
            7,
            Some(source_label(item.source_type).to_owned()),
        )?;
        write_string(worksheet, row, 8, item.note.clone())?;
        write_string(worksheet, row, 9, item.event_tag.clone())?;
        write_string(worksheet, row, 10, item.project_tag.clone())?;
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
) -> Result<(), AppError> {
    if let Some(value) = value {
        worksheet
            .write_string(row, column, value)
            .map_err(|_| internal_error("failed to write reimbursement value"))?;
    }
    Ok(())
}

fn create_originals_zip(items: &[ExportItem]) -> Result<Vec<u8>, AppError> {
    let cursor = Cursor::new(Vec::new());
    let mut archive = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o600);
    for item in items {
        archive
            .start_file(&item.archive_name, options)
            .map_err(|_| internal_error("failed to create originals archive entry"))?;
        archive
            .write_all(&item.original_bytes)
            .map_err(|_| internal_error("failed to write originals archive entry"))?;
    }
    archive
        .finish()
        .map(|cursor| cursor.into_inner())
        .map_err(|_| internal_error("failed to finish originals archive"))
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

    use super::exact_xlsx_amount;
    #[cfg(unix)]
    use super::publish_directory_with_sync;

    #[test]
    fn xlsx_amount_round_trips_regular_cent_values() {
        for cents in [0_i64, 1, 99, 100, 12_345, 4_485_000_001] {
            let value = exact_xlsx_amount(cents).unwrap();
            assert_eq!((value * 100.0).round() as i64, cents);
        }
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
