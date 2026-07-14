use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::domain::error::AppError;
use image::{ImageReader, Limits};
use printpdf::{
    Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, RawImage, RawImageData, RawImageFormat,
    XObjectTransform,
};
use serde::{Deserialize, Serialize};

const NORMALIZED_IMAGE_DPI: f32 = 96.0;
// Keep these document budgets aligned with sidecars/ocr/main.py.
const MAX_PDF_PAGES: usize = 100;
const MAX_IMAGE_DIMENSION: u32 = 20_000;
const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const MAX_NORMALIZED_PAGE_POINTS: f32 = 14_400.0;
const MAX_IMAGE_DECODE_BYTES: u64 = MAX_IMAGE_PIXELS * 4;
const OCR_COLD_START_ALLOWANCE_SECS: u64 = 45;
const OCR_MAX_PAGE_WORK_SECS: u64 = 10;
const OCR_MAX_PIXEL_WORK_SECS: u64 = 25;
const DEFAULT_OCR_TIMEOUT: Duration = Duration::from_secs(
    OCR_COLD_START_ALLOWANCE_SECS + OCR_MAX_PAGE_WORK_SECS + OCR_MAX_PIXEL_WORK_SECS,
);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
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
    fn extract_pdf_text(&self, path: &Path) -> Result<OcrResult, AppError>;
}

pub struct ProcessOcrGateway {
    executable: PathBuf,
    timeout: Duration,
    session: Mutex<Option<SidecarSession>>,
}

impl ProcessOcrGateway {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self::with_timeout(executable, DEFAULT_OCR_TIMEOUT)
    }

    pub fn with_timeout(executable: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            executable: executable.into(),
            timeout,
            session: Mutex::new(None),
        }
    }

    pub fn default_timeout() -> Duration {
        DEFAULT_OCR_TIMEOUT
    }
}

impl OcrGateway for ProcessOcrGateway {
    fn recognize(&self, path: &Path) -> Result<OcrResult, AppError> {
        self.run(path, SidecarOperation::Recognize)
    }

    fn extract_pdf_text(&self, path: &Path) -> Result<OcrResult, AppError> {
        self.run(path, SidecarOperation::ExtractPdfText)
            .map_err(|error| {
                if error == resource_limit_error() {
                    error
                } else {
                    document_error()
                }
            })
    }
}

#[derive(Clone, Copy)]
enum SidecarOperation {
    Recognize,
    ExtractPdfText,
}

impl SidecarOperation {
    fn protocol_name(self) -> &'static str {
        match self {
            Self::Recognize => "ocr",
            Self::ExtractPdfText => "extract_pdf_text",
        }
    }
}

impl ProcessOcrGateway {
    fn run(&self, path: &Path, operation: SidecarOperation) -> Result<OcrResult, AppError> {
        let mut session = self.session.lock().map_err(|_| ocr_error())?;
        if session
            .as_mut()
            .is_some_and(|current| !current.is_running())
        {
            session.take();
        }
        if session.is_none() {
            *session = Some(SidecarSession::spawn(&self.executable)?);
        }

        let result = session
            .as_mut()
            .ok_or_else(ocr_error)?
            .request(path, operation, self.timeout);
        match result {
            Ok(result) => Ok(result),
            Err(SidecarRequestError::Operation(error)) => Err(error),
            Err(SidecarRequestError::SessionFatal(error)) => {
                session.take();
                Err(error)
            }
        }
    }
}

enum SidecarRequestError {
    SessionFatal(AppError),
    Operation(AppError),
}

impl SidecarRequestError {
    fn session_fatal() -> Self {
        Self::SessionFatal(ocr_error())
    }
}

struct SidecarSession {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<ReaderOutput>,
    process_tree: ProcessTree,
    active: bool,
}

impl SidecarSession {
    fn spawn(executable: &Path) -> Result<Self, AppError> {
        let mut command = Command::new(executable);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_tree(&mut command);
        let mut child = command.spawn().map_err(|_| ocr_error())?;
        let mut process_tree = ProcessTree::attach(&child).map_err(|_| {
            let _ = child.kill();
            let _ = child.wait();
            ocr_error()
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            process_tree.terminate(&mut child);
            return Err(ocr_error());
        };
        let responses = match read_bounded_lines(stdout, MAX_OCR_RESPONSE_BYTES) {
            Ok(reader) => reader,
            Err(error) => {
                process_tree.terminate(&mut child);
                return Err(error);
            }
        };
        if let Err(error) = drain_stderr(stderr) {
            process_tree.terminate(&mut child);
            return Err(error);
        }

        Ok(Self {
            child,
            stdin,
            responses,
            process_tree,
            active: true,
        })
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().is_ok_and(|status| status.is_none())
    }

    fn request(
        &mut self,
        path: &Path,
        operation: SidecarOperation,
        timeout: Duration,
    ) -> Result<OcrResult, SidecarRequestError> {
        let deadline = Instant::now() + timeout;
        write_ocr_request(&mut self.stdin, path, operation)
            .map_err(|_| SidecarRequestError::session_fatal())?;
        let (stdout, stdout_too_large) = self.receive_response(deadline)?;

        if stdout_too_large || !self.is_running() {
            return Err(SidecarRequestError::session_fatal());
        }

        let response: OcrResponse =
            serde_json::from_slice(&stdout).map_err(|_| SidecarRequestError::session_fatal())?;
        if response.ok {
            Ok(OcrResult {
                text: response
                    .text
                    .ok_or_else(SidecarRequestError::session_fatal)?,
                warnings: sanitize_warnings(response.warnings),
            })
        } else if response.error.as_deref() == Some("document exceeds OCR resource limits") {
            Err(SidecarRequestError::Operation(resource_limit_error()))
        } else if response.error.is_some() {
            Err(SidecarRequestError::Operation(ocr_error()))
        } else {
            Err(SidecarRequestError::session_fatal())
        }
    }

    fn receive_response(
        &mut self,
        deadline: Instant,
    ) -> Result<(Vec<u8>, bool), SidecarRequestError> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SidecarRequestError::session_fatal());
            }
            match self
                .responses
                .recv_timeout(remaining.min(PROCESS_POLL_INTERVAL))
            {
                Ok(result) => {
                    return result.map_err(|_| SidecarRequestError::session_fatal());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(SidecarRequestError::session_fatal());
                }
                Err(mpsc::RecvTimeoutError::Timeout) if !self.is_running() => {
                    return Err(SidecarRequestError::session_fatal());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn terminate(&mut self) {
        if self.active {
            self.process_tree.terminate(&mut self.child);
            self.active = false;
        }
    }
}

impl Drop for SidecarSession {
    fn drop(&mut self) {
        self.terminate();
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
    operation: &'static str,
}

#[derive(Deserialize)]
struct OcrResponse {
    ok: bool,
    text: Option<String>,
    #[serde(default)]
    warnings: Vec<String>,
    error: Option<String>,
}

fn write_ocr_request(
    stdin: &mut ChildStdin,
    path: &Path,
    operation: SidecarOperation,
) -> Result<(), ()> {
    let path = path.to_string_lossy();
    let request = OcrRequest {
        path: &path,
        operation: operation.protocol_name(),
    };
    serde_json::to_writer(&mut *stdin, &request).map_err(|_| ())?;
    stdin.write_all(b"\n").map_err(|_| ())?;
    stdin.flush().map_err(|_| ())
}

type ReaderOutput = std::io::Result<(Vec<u8>, bool)>;

fn read_bounded_lines<R>(mut reader: R, limit: usize) -> Result<Receiver<ReaderOutput>, AppError>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("ocr-protocol-reader".to_owned())
        .spawn(move || {
            let mut line = Vec::new();
            let mut too_large = false;
            let mut buffer = [0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        if !line.is_empty() || too_large {
                            let _ = sender.send(Ok((line, too_large)));
                        }
                        return;
                    }
                    Ok(count) => {
                        for byte in &buffer[..count] {
                            if *byte == b'\n' {
                                let completed = std::mem::take(&mut line);
                                let completed_too_large = std::mem::take(&mut too_large);
                                if sender.send(Ok((completed, completed_too_large))).is_err() {
                                    return;
                                }
                            } else if line.len() < limit {
                                line.push(*byte);
                            } else {
                                too_large = true;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        return;
                    }
                }
            }
        })
        .map_err(|_| ocr_error())?;
    Ok(receiver)
}

fn drain_stderr<R>(mut reader: R) -> Result<(), AppError>
where
    R: Read + Send + 'static,
{
    std::thread::Builder::new()
        .name("ocr-stderr-drain".to_owned())
        .spawn(move || {
            let mut buffer = [0_u8; MAX_OCR_STDERR_BYTES];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        })
        .map(|_| ())
        .map_err(|_| ocr_error())
}

#[cfg(unix)]
fn configure_process_tree(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_tree(_command: &mut Command) {}

struct ProcessTree {
    #[cfg(unix)]
    process_group: i32,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl ProcessTree {
    fn attach(child: &Child) -> Result<Self, ()> {
        #[cfg(unix)]
        {
            return Ok(Self {
                process_group: i32::try_from(child.id()).map_err(|_| ())?,
            });
        }
        #[cfg(windows)]
        {
            return windows_process_tree(child);
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    fn terminate(&mut self, child: &mut Child) {
        #[cfg(unix)]
        {
            // A negative PID targets the group even after its original leader has exited.
            let _ = unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;

            let _ = unsafe { TerminateJobObject(self.job, 1) };
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        let _ = unsafe { CloseHandle(self.job) };
    }
}

#[cfg(windows)]
fn windows_process_tree(child: &Child) -> Result<ProcessTree, ()> {
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(());
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    let assigned = if configured != 0 {
        unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) }
    } else {
        0
    };
    if assigned == 0 {
        let _ = unsafe { CloseHandle(job) };
        return Err(());
    }
    Ok(ProcessTree { job })
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
        let extracted_text = self.ocr.extract_pdf_text(path)?;
        let (text, warnings) = if useful_character_count(&extracted_text.text) >= 20 {
            (extracted_text.text, extracted_text.warnings)
        } else {
            let result = self.ocr.recognize(path)?;
            let mut warnings = extracted_text.warnings;
            warnings.extend(result.warnings);
            (result.text, warnings)
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
