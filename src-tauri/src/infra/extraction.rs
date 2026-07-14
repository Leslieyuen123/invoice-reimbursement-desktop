use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use image::{ImageReader, Limits};
use printpdf::{
    Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, RawImage, RawImageData, RawImageFormat,
    XObjectTransform,
};
use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

use crate::domain::error::AppError;

const NORMALIZED_IMAGE_DPI: f32 = 96.0;
// Keep these document budgets aligned with sidecars/ocr/main.py.
const MAX_PDF_PAGES: usize = 100;
const MAX_IMAGE_DIMENSION: u32 = 20_000;
const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const MAX_NORMALIZED_PAGE_POINTS: f32 = 14_400.0;
const MAX_IMAGE_DECODE_BYTES: u64 = MAX_IMAGE_PIXELS * 4;
const DEFAULT_OCR_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OCR_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_OCR_STDERR_BYTES: usize = 8 * 1024;
const MAX_OCR_WARNINGS: usize = 16;
const MAX_OCR_WARNING_CHARS: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub struct ExtractedDocument {
    pub text: String,
    pub normalized_pdf: Option<Vec<u8>>,
    pub warnings: Vec<String>,
}

pub trait DocumentExtractor: Send + Sync {
    fn extract(&self, path: &Path) -> Result<ExtractedDocument, AppError>;
}

#[derive(Debug, PartialEq, Eq)]
pub struct OcrResult {
    pub text: String,
    pub warnings: Vec<String>,
}

pub trait OcrGateway: Send + Sync {
    fn recognize(&self, path: &Path) -> Result<OcrResult, AppError>;
}

pub struct ProcessOcrGateway {
    executable: PathBuf,
    timeout: Duration,
}

impl ProcessOcrGateway {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self::with_timeout(executable, DEFAULT_OCR_TIMEOUT)
    }

    pub fn with_timeout(executable: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            executable: executable.into(),
            timeout,
        }
    }
}

impl OcrGateway for ProcessOcrGateway {
    fn recognize(&self, path: &Path) -> Result<OcrResult, AppError> {
        let mut command = Command::new(&self.executable);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_tree(&mut command);
        let mut child = command.spawn().map_err(|_| ocr_error())?;
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                terminate(&mut child);
                return Err(ocr_error());
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                terminate(&mut child);
                return Err(ocr_error());
            }
        };
        let stdout_reader = read_bounded(stdout, MAX_OCR_RESPONSE_BYTES);
        let stderr_reader = read_bounded(stderr, MAX_OCR_STDERR_BYTES);

        if write_ocr_request(&mut child, path).is_err() {
            terminate(&mut child);
            let _ = join_reader(stdout_reader);
            let _ = join_reader(stderr_reader);
            return Err(ocr_error());
        }

        let status = match child.wait_timeout(self.timeout) {
            Ok(Some(status)) => status,
            Ok(None) | Err(_) => {
                terminate(&mut child);
                let _ = join_reader(stdout_reader);
                let _ = join_reader(stderr_reader);
                return Err(ocr_error());
            }
        };
        let (stdout, stdout_too_large) = join_reader(stdout_reader)?;
        let _ = join_reader(stderr_reader)?;

        if !status.success() || stdout_too_large {
            return Err(ocr_error());
        }

        let response: OcrResponse = serde_json::from_slice(&stdout).map_err(|_| ocr_error())?;
        if response.ok {
            Ok(OcrResult {
                text: response.text.ok_or_else(ocr_error)?,
                warnings: sanitize_warnings(response.warnings),
            })
        } else {
            let _ = response.error;
            Err(ocr_error())
        }
    }
}

fn sanitize_warnings(warnings: Vec<String>) -> Vec<String> {
    warnings
        .into_iter()
        .take(MAX_OCR_WARNINGS)
        .filter_map(|warning| {
            let warning = warning.trim();
            if warning.is_empty() {
                return None;
            }
            if !warning.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '_' || character == '-'
            }) {
                return Some("ocr_warning".to_owned());
            }
            Some(warning.chars().take(MAX_OCR_WARNING_CHARS).collect())
        })
        .collect()
}

#[derive(Serialize)]
struct OcrRequest<'a> {
    path: &'a str,
}

#[derive(Deserialize)]
struct OcrResponse {
    ok: bool,
    text: Option<String>,
    #[serde(default)]
    warnings: Vec<String>,
    error: Option<String>,
}

fn write_ocr_request(child: &mut Child, path: &Path) -> Result<(), ()> {
    let mut stdin = child.stdin.take().ok_or(())?;
    let path = path.to_string_lossy();
    let request = OcrRequest { path: &path };
    serde_json::to_writer(&mut stdin, &request).map_err(|_| ())?;
    stdin.write_all(b"\n").map_err(|_| ())?;
    stdin.flush().map_err(|_| ())
}

fn read_bounded<R>(reader: R, limit: usize) -> JoinHandle<std::io::Result<(Vec<u8>, bool)>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        let too_large = bytes.len() > limit;
        bytes.truncate(limit);
        Ok((bytes, too_large))
    })
}

fn join_reader(
    reader: JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
) -> Result<(Vec<u8>, bool), AppError> {
    reader
        .join()
        .map_err(|_| ocr_error())?
        .map_err(|_| ocr_error())
}

#[cfg(unix)]
fn configure_process_tree(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_tree(_command: &mut Command) {}

#[cfg(unix)]
fn terminate(child: &mut Child) {
    if let Ok(process_group) = i32::try_from(child.id()) {
        // The child is its process-group leader, so a negative PID targets its descendants too.
        let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
fn terminate(child: &mut Child) {
    // Exercise taskkill tree semantics on Windows CI; no command shell is involved.
    let pid = child.id().to_string();
    let _ = Command::new("taskkill")
        .args(["/PID", &pid, "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(any(unix, windows)))]
fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn ocr_error() -> AppError {
    AppError::External {
        service: "ocr_sidecar".to_owned(),
        retryable: false,
        message: "OCR processing failed.".to_owned(),
    }
}

pub struct LocalExtractor {
    ocr: Arc<dyn OcrGateway>,
}

impl LocalExtractor {
    pub fn new(ocr: Arc<dyn OcrGateway>) -> Self {
        Self { ocr }
    }
}

impl DocumentExtractor for LocalExtractor {
    fn extract(&self, path: &Path) -> Result<ExtractedDocument, AppError> {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .ok_or_else(|| AppError::validation("file", "The file type is unsupported."))?;

        match extension.as_str() {
            "pdf" => self.extract_pdf(path),
            "jpg" | "jpeg" | "png" => self.extract_image(path),
            "doc" | "docx" | "xls" | "xlsx" | "zip" => Ok(ExtractedDocument {
                text: String::new(),
                normalized_pdf: None,
                warnings: vec!["unsupported_for_recognition".to_owned()],
            }),
            _ => Err(AppError::validation(
                "file",
                "The file type is unsupported.",
            )),
        }
    }
}

impl LocalExtractor {
    fn extract_pdf(&self, path: &Path) -> Result<ExtractedDocument, AppError> {
        let bytes = fs::read(path).map_err(|_| document_error())?;
        let document = lopdf::Document::load_mem(&bytes).map_err(|_| document_error())?;
        if document.get_pages().len() > MAX_PDF_PAGES {
            return Err(resource_limit_error());
        }
        drop(document);
        let text = pdf_extract::extract_text_from_mem(&bytes).map_err(|_| document_error())?;
        let (text, warnings) = if useful_character_count(&text) >= 20 {
            (text, Vec::new())
        } else {
            let result = self.ocr.recognize(path)?;
            (result.text, result.warnings)
        };

        Ok(ExtractedDocument {
            text,
            normalized_pdf: Some(bytes),
            warnings,
        })
    }

    fn extract_image(&self, path: &Path) -> Result<ExtractedDocument, AppError> {
        let (width, height) = ImageReader::open(path)
            .map_err(|_| document_error())?
            .into_dimensions()
            .map_err(|_| document_error())?;
        validate_image_dimensions(width, height)?;

        let mut reader = ImageReader::open(path).map_err(|_| document_error())?;
        reader.limits(image_decode_limits());
        let image = reader.decode().map_err(|_| document_error())?;
        let normalized_pdf = normalize_image(image, width, height);
        let result = self.ocr.recognize(path)?;

        Ok(ExtractedDocument {
            text: result.text,
            normalized_pdf: Some(normalized_pdf),
            warnings: result.warnings,
        })
    }
}

fn normalize_image(image: image::DynamicImage, width: u32, height: u32) -> Vec<u8> {
    let rgba = image.into_rgba8();
    let raw_image = RawImage {
        pixels: RawImageData::U8(rgba.into_raw()),
        width: width as usize,
        height: height as usize,
        data_format: RawImageFormat::RGBA8,
        tag: Vec::new(),
    };
    let mut document = PdfDocument::new("Normalized invoice image");
    let image_id = document.add_image(&raw_image);
    let width_points = width as f32 * 72.0 / NORMALIZED_IMAGE_DPI;
    let height_points = height as f32 * 72.0 / NORMALIZED_IMAGE_DPI;
    let page_scale = (MAX_NORMALIZED_PAGE_POINTS / width_points.max(height_points)).min(1.0);
    let width_mm = Mm(width_points * page_scale * 25.4 / 72.0);
    let height_mm = Mm(height_points * page_scale * 25.4 / 72.0);
    let page = PdfPage::new(
        width_mm,
        height_mm,
        vec![Op::UseXobject {
            id: image_id,
            transform: XObjectTransform {
                dpi: Some(NORMALIZED_IMAGE_DPI),
                scale_x: Some(page_scale),
                scale_y: Some(page_scale),
                ..XObjectTransform::default()
            },
        }],
    );

    document
        .with_pages(vec![page])
        .save(&PdfSaveOptions::default(), &mut Vec::new())
}

fn validate_image_dimensions(width: u32, height: u32) -> Result<(), AppError> {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION || pixels > MAX_IMAGE_PIXELS {
        return Err(resource_limit_error());
    }
    Ok(())
}

fn image_decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES);
    limits
}

fn useful_character_count(text: &str) -> usize {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .count()
}

fn document_error() -> AppError {
    AppError::External {
        service: "document_extractor".to_owned(),
        retryable: false,
        message: "Unable to read document.".to_owned(),
    }
}

fn resource_limit_error() -> AppError {
    AppError::External {
        service: "document_extractor".to_owned(),
        retryable: false,
        message: "Document exceeds extraction resource limits.".to_owned(),
    }
}
