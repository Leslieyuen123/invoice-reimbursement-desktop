use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use image::GenericImageView;
use printpdf::{
    Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, RawImage, RawImageData, RawImageFormat,
    XObjectTransform,
};
use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

use crate::domain::error::AppError;

const NORMALIZED_IMAGE_DPI: f32 = 96.0;
const DEFAULT_OCR_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OCR_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_OCR_STDERR_BYTES: usize = 8 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct ExtractedDocument {
    pub text: String,
    pub normalized_pdf: Option<Vec<u8>>,
    pub warnings: Vec<String>,
}

pub trait DocumentExtractor: Send + Sync {
    fn extract(&self, path: &Path) -> Result<ExtractedDocument, AppError>;
}

pub trait OcrGateway: Send + Sync {
    fn recognize(&self, path: &Path) -> Result<String, AppError>;
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
    fn recognize(&self, path: &Path) -> Result<String, AppError> {
        let mut child = Command::new(&self.executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| ocr_error())?;
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
        let _ = response.warnings;
        if response.ok {
            response.text.ok_or_else(ocr_error)
        } else {
            let _ = response.error;
            Err(ocr_error())
        }
    }
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
        let bytes = fs::read(path).map_err(|_| document_error(path))?;
        let text = pdf_extract::extract_text_from_mem(&bytes).map_err(|_| document_error(path))?;
        let text = if useful_character_count(&text) >= 20 {
            text
        } else {
            self.ocr.recognize(path)?
        };

        Ok(ExtractedDocument {
            text,
            normalized_pdf: Some(bytes),
            warnings: Vec::new(),
        })
    }

    fn extract_image(&self, path: &Path) -> Result<ExtractedDocument, AppError> {
        let image = image::open(path).map_err(|_| document_error(path))?;
        let (width, height) = image.dimensions();
        let normalized_pdf = normalize_image(image, width, height);
        let text = self.ocr.recognize(path)?;

        Ok(ExtractedDocument {
            text,
            normalized_pdf: Some(normalized_pdf),
            warnings: Vec::new(),
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
    let width_mm = Mm(width as f32 * 25.4 / NORMALIZED_IMAGE_DPI);
    let height_mm = Mm(height as f32 * 25.4 / NORMALIZED_IMAGE_DPI);
    let page = PdfPage::new(
        width_mm,
        height_mm,
        vec![Op::UseXobject {
            id: image_id,
            transform: XObjectTransform {
                dpi: Some(NORMALIZED_IMAGE_DPI),
                ..XObjectTransform::default()
            },
        }],
    );

    document
        .with_pages(vec![page])
        .save(&PdfSaveOptions::default(), &mut Vec::new())
}

fn useful_character_count(text: &str) -> usize {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .count()
}

fn document_error(path: &Path) -> AppError {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document");
    AppError::External {
        service: "document_extractor".to_owned(),
        retryable: false,
        message: format!("Unable to read document {file_name:?}."),
    }
}
