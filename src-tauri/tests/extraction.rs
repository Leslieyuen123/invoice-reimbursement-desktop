use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::infra::extraction::{
    DocumentExtractor, LocalExtractor, OcrGateway, OcrResult, ProcessOcrGateway,
};
use printpdf::{BuiltinFont, Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, Pt, TextItem};

#[derive(Default)]
struct FakeOcr {
    calls: AtomicUsize,
    pdf_text_calls: AtomicUsize,
    text: String,
    pdf_text: String,
    warnings: Vec<String>,
}

impl FakeOcr {
    fn returning(text: impl Into<String>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            pdf_text_calls: AtomicUsize::new(0),
            text: text.into(),
            pdf_text: String::new(),
            warnings: Vec::new(),
        }
    }

    fn returning_pdf_text(text: impl Into<String>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            pdf_text_calls: AtomicUsize::new(0),
            text: "must not OCR".to_owned(),
            pdf_text: text.into(),
            warnings: Vec::new(),
        }
    }

    fn returning_with_warnings(text: impl Into<String>, warnings: &[&str]) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            pdf_text_calls: AtomicUsize::new(0),
            text: text.into(),
            pdf_text: String::new(),
            warnings: warnings
                .iter()
                .map(|warning| (*warning).to_owned())
                .collect(),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn pdf_text_call_count(&self) -> usize {
        self.pdf_text_calls.load(Ordering::SeqCst)
    }
}

impl OcrGateway for FakeOcr {
    fn recognize(&self, _path: &Path) -> Result<OcrResult, AppError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(OcrResult {
            text: self.text.clone(),
            warnings: self.warnings.clone(),
        })
    }

    fn extract_pdf_text(&self, path: &Path) -> Result<OcrResult, AppError> {
        self.pdf_text_calls.fetch_add(1, Ordering::SeqCst);
        match path.file_name().and_then(|name| name.to_str()) {
            Some("malformed-content.pdf") => Err(AppError::External {
                service: "document_extractor".to_owned(),
                retryable: false,
                message: "Unable to read document.".to_owned(),
            }),
            Some("compressed-text-bomb.pdf") => Err(AppError::External {
                service: "document_extractor".to_owned(),
                retryable: false,
                message: "Document exceeds extraction resource limits.".to_owned(),
            }),
            _ => Ok(OcrResult {
                text: self.pdf_text.clone(),
                warnings: Vec::new(),
            }),
        }
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn text_pdf_is_extracted_without_calling_ocr() {
    let ocr = Arc::new(FakeOcr::returning_pdf_text(
        "开票日期：2026年06月18日\n价税合计（小写）¥128.50",
    ));
    let extractor = LocalExtractor::new(ocr.clone());
    let path = fixture("text-invoice.pdf");

    let extracted = extractor.extract(&path).expect("text PDF should extract");

    assert!(extracted.text.contains("开票日期：2026年06月18日"));
    assert!(extracted.text.contains("价税合计（小写）¥128.50"));
    assert_eq!(extracted.normalized_pdf, Some(std::fs::read(path).unwrap()));
    assert!(extracted.warnings.is_empty());
    assert_eq!(ocr.call_count(), 0);
    assert_eq!(ocr.pdf_text_call_count(), 1);
}

#[test]
fn image_is_ocrd_and_normalized_to_a_valid_one_page_pdf() {
    let ocr_text = "北京 出租车 价税合计 ¥128.50";
    let ocr = Arc::new(FakeOcr::returning(ocr_text));
    let extractor = LocalExtractor::new(ocr.clone());

    let extracted = extractor
        .extract(&fixture("image-invoice.png"))
        .expect("image should extract");

    assert_eq!(extracted.text, ocr_text);
    assert!(extracted.warnings.is_empty());
    assert_eq!(ocr.call_count(), 1);
    let normalized = extracted.normalized_pdf.expect("image should normalize");
    let pdf = lopdf::Document::load_mem(&normalized).expect("normalized PDF should reopen");
    assert_eq!(pdf.get_pages().len(), 1);
    let page_id = *pdf.get_pages().values().next().unwrap();
    let page = pdf.get_object(page_id).unwrap().as_dict().unwrap();
    let media_box = page.get(b"MediaBox").unwrap().as_array().unwrap();
    let width = media_box[2].as_float().unwrap() - media_box[0].as_float().unwrap();
    let height = media_box[3].as_float().unwrap() - media_box[1].as_float().unwrap();
    assert!((width / height - 1.5).abs() < 0.001);
}

#[test]
fn blank_pdf_calls_ocr_and_preserves_the_valid_original_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scan.PDF");
    let bytes = PdfDocument::new("blank scan")
        .with_pages(vec![PdfPage::new(Mm(210.0), Mm(297.0), Vec::new())])
        .save(&PdfSaveOptions::default(), &mut Vec::new());
    std::fs::write(&path, &bytes).unwrap();
    let ocr = Arc::new(FakeOcr::returning("scanned invoice text"));
    let extractor = LocalExtractor::new(ocr.clone());

    let extracted = extractor.extract(&path).expect("blank PDF should OCR");

    assert_eq!(extracted.text, "scanned invoice text");
    assert_eq!(extracted.normalized_pdf, Some(bytes));
    assert_eq!(ocr.call_count(), 1);
}

#[test]
fn scanned_pdf_propagates_ocr_warnings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scan.pdf");
    let bytes = PdfDocument::new("blank scan")
        .with_pages(vec![PdfPage::new(Mm(10.0), Mm(10.0), Vec::new())])
        .save(&PdfSaveOptions::default(), &mut Vec::new());
    std::fs::write(&path, bytes).unwrap();
    let extractor = LocalExtractor::new(Arc::new(FakeOcr::returning_with_warnings(
        "scan text",
        &["low_confidence"],
    )));

    let extracted = extractor.extract(&path).unwrap();

    assert_eq!(extracted.text, "scan text");
    assert_eq!(extracted.warnings, ["low_confidence"]);
}

#[test]
fn exactly_twenty_non_whitespace_pdf_characters_skip_ocr() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("threshold.pdf");
    let mut pdf = PdfDocument::new("threshold");
    let bytes = pdf
        .with_pages(vec![PdfPage::new(
            Mm(210.0),
            Mm(297.0),
            vec![
                Op::StartTextSection,
                Op::SetFontSizeBuiltinFont {
                    size: Pt(12.0),
                    font: BuiltinFont::Helvetica,
                },
                Op::WriteTextBuiltinFont {
                    items: vec![TextItem::Text("12345678901234567890".to_owned())],
                    font: BuiltinFont::Helvetica,
                },
                Op::EndTextSection,
            ],
        )])
        .save(&PdfSaveOptions::default(), &mut Vec::new());
    std::fs::write(&path, bytes).unwrap();
    let ocr = Arc::new(FakeOcr::returning_pdf_text("12345678901234567890"));
    let extractor = LocalExtractor::new(ocr.clone());

    let extracted = extractor.extract(&path).unwrap();

    assert!(extracted.text.contains("12345678901234567890"));
    assert_eq!(ocr.call_count(), 0);
    assert_eq!(ocr.pdf_text_call_count(), 1);
}

#[test]
fn rgb_rgba_and_grayscale_images_all_normalize() {
    let directory = tempfile::tempdir().unwrap();
    let cases = [
        (
            "rgb.jpg",
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                90,
                60,
                image::Rgb([10, 20, 30]),
            )),
        ),
        (
            "rgba.png",
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                90,
                60,
                image::Rgba([10, 20, 30, 100]),
            )),
        ),
        (
            "gray.png",
            image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(
                90,
                60,
                image::Luma([120]),
            )),
        ),
    ];
    let ocr = Arc::new(FakeOcr::returning("recognized"));
    let extractor = LocalExtractor::new(ocr.clone());

    for (name, image) in cases {
        let path = directory.path().join(name);
        image.save(&path).unwrap();
        let extracted = extractor.extract(&path).unwrap();
        let normalized = extracted.normalized_pdf.unwrap();
        let pdf = lopdf::Document::load_mem(&normalized).unwrap();
        assert_eq!(
            pdf.get_pages().len(),
            1,
            "invalid normalized PDF for {name}"
        );
    }
    assert_eq!(ocr.call_count(), 3);
}

#[test]
fn office_and_archive_formats_are_retained_but_not_recognized() {
    let directory = tempfile::tempdir().unwrap();
    let ocr = Arc::new(FakeOcr::returning("unused"));
    let extractor = LocalExtractor::new(ocr.clone());

    for name in [
        "legacy.DOC",
        "document.docx",
        "legacy.XlS",
        "sheet.xlsx",
        "bundle.ZIP",
    ] {
        let path = directory.path().join(name);
        std::fs::write(&path, b"original bytes stay untouched").unwrap();

        let extracted = extractor.extract(&path).unwrap();

        assert_eq!(extracted.text, "", "unexpected text for {name}");
        assert_eq!(extracted.normalized_pdf, None, "normalized {name}");
        assert_eq!(extracted.warnings, ["unsupported_for_recognition"]);
    }
    assert_eq!(ocr.call_count(), 0);
}

#[test]
fn unknown_extension_is_a_stable_file_validation_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invoice.bmp");
    std::fs::write(&path, b"unknown").unwrap();
    let extractor = LocalExtractor::new(Arc::new(FakeOcr::default()));

    let error = extractor.extract(&path).expect_err("BMP is unsupported");

    assert!(matches!(error, AppError::Validation { ref field, .. } if field == "file"));
}

#[test]
fn corrupt_pdf_is_rejected_without_ocr_or_path_disclosure() {
    let directory = tempfile::tempdir().unwrap();
    let nested = directory.path().join("private-secret");
    std::fs::create_dir(&nested).unwrap();
    let path = nested.join("broken.pdf");
    std::fs::write(&path, b"%PDF-1.7 definitely corrupt").unwrap();
    let ocr = Arc::new(FakeOcr::returning("must not run"));
    let extractor = LocalExtractor::new(ocr.clone());

    let error = extractor
        .extract(&path)
        .expect_err("corrupt PDF should fail");

    assert_external(&error, "document_extractor");
    assert!(!error.to_string().contains("private-secret"));
    assert!(!error.to_string().contains("broken.pdf"));
    assert_eq!(ocr.call_count(), 0);
}

#[test]
fn malformed_pdf_content_cannot_panic_the_tauri_process() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("malformed-content.pdf");
    std::fs::write(&path, malformed_content_pdf()).unwrap();
    let extractor = LocalExtractor::new(Arc::new(FakeOcr::returning("must not run")));

    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| extractor.extract(&path)));

    assert!(outcome.is_ok(), "untrusted PDF parser panicked in-process");
    let error = outcome.unwrap().expect_err("malformed content should fail");
    assert_external(&error, "document_extractor");
}

#[test]
fn compressed_pdf_text_bomb_is_rejected_without_expanding_in_process() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("compressed-text-bomb.pdf");
    let bytes = compressed_text_pdf(2_000_000);
    assert!(
        bytes.len() < 100_000,
        "fixture did not compress: {}",
        bytes.len()
    );
    std::fs::write(&path, bytes).unwrap();
    let extractor = LocalExtractor::new(Arc::new(FakeOcr::returning("must not run")));
    let started = Instant::now();

    let error = extractor
        .extract(&path)
        .expect_err("compressed text bomb should be rejected");

    assert_external(&error, "document_extractor");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn corrupt_image_is_rejected_without_ocr_or_path_disclosure() {
    let directory = tempfile::tempdir().unwrap();
    let nested = directory.path().join("private-secret");
    std::fs::create_dir(&nested).unwrap();
    let path = nested.join("broken.JPEG");
    std::fs::write(&path, b"not a JPEG").unwrap();
    let ocr = Arc::new(FakeOcr::returning("must not run"));
    let extractor = LocalExtractor::new(ocr.clone());

    let error = extractor
        .extract(&path)
        .expect_err("corrupt image should fail");

    assert_external(&error, "document_extractor");
    assert!(!error.to_string().contains("private-secret"));
    assert!(!error.to_string().contains("broken.JPEG"));
    assert_eq!(ocr.call_count(), 0);
}

#[test]
fn pdfs_over_one_hundred_pages_are_rejected_before_text_extraction_or_ocr() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("too-many-pages.pdf");
    let pages = (0..101)
        .map(|_| PdfPage::new(Mm(10.0), Mm(10.0), Vec::new()))
        .collect();
    let bytes = PdfDocument::new("too many pages")
        .with_pages(pages)
        .save(&PdfSaveOptions::default(), &mut Vec::new());
    std::fs::write(&path, bytes).unwrap();
    let ocr = Arc::new(FakeOcr::returning("must not run"));
    let extractor = LocalExtractor::new(ocr.clone());

    let error = extractor
        .extract(&path)
        .expect_err("page budget should reject PDF");

    assert_resource_error(&error);
    assert_eq!(ocr.call_count(), 0);
}

#[test]
fn images_over_the_dimension_budget_are_rejected_before_ocr() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("too-wide.png");
    image::RgbImage::new(20_001, 1).save(&path).unwrap();
    let ocr = Arc::new(FakeOcr::returning("must not run"));
    let extractor = LocalExtractor::new(ocr.clone());

    let error = extractor
        .extract(&path)
        .expect_err("dimension budget should reject image");

    assert_resource_error(&error);
    assert_eq!(ocr.call_count(), 0);
}

#[test]
fn image_headers_over_the_pixel_budget_are_rejected_without_decoding_pixels() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("too-many-pixels.png");
    std::fs::write(&path, png_header(10_000, 4_001)).unwrap();
    let ocr = Arc::new(FakeOcr::returning("must not run"));
    let extractor = LocalExtractor::new(ocr.clone());

    let error = extractor
        .extract(&path)
        .expect_err("pixel budget should reject image header");

    assert_resource_error(&error);
    assert_eq!(ocr.call_count(), 0);
}

#[test]
fn normalized_image_pages_fit_the_pdf_point_limit_without_changing_aspect() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("long-receipt.png");
    image::RgbImage::new(20_000, 100).save(&path).unwrap();
    let extractor = LocalExtractor::new(Arc::new(FakeOcr::returning("receipt")));

    let extracted = extractor.extract(&path).unwrap();
    let pdf = lopdf::Document::load_mem(&extracted.normalized_pdf.unwrap()).unwrap();
    let page_id = *pdf.get_pages().values().next().unwrap();
    let page = pdf.get_object(page_id).unwrap().as_dict().unwrap();
    let media_box = page.get(b"MediaBox").unwrap().as_array().unwrap();
    let width = media_box[2].as_float().unwrap() - media_box[0].as_float().unwrap();
    let height = media_box[3].as_float().unwrap() - media_box[1].as_float().unwrap();

    assert!(width <= 14_400.0, "page width was {width}");
    assert!(height <= 14_400.0, "page height was {height}");
    assert!((width / height - 200.0).abs() < 0.1);
}

fn assert_external(error: &AppError, expected_service: &str) {
    assert!(
        matches!(
            error,
            AppError::External {
                service,
                retryable: false,
                ..
            } if service == expected_service
        ),
        "unexpected error: {error:?}"
    );
}

fn assert_resource_error(error: &AppError) {
    assert_external(error, "document_extractor");
    assert_eq!(
        error.to_string(),
        "Document exceeds extraction resource limits."
    );
}

fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    append_png_chunk(&mut bytes, b"IHDR", &ihdr);
    append_png_chunk(
        &mut bytes,
        b"IDAT",
        &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    );
    append_png_chunk(&mut bytes, b"IEND", &[]);
    bytes
}

fn append_png_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(data);
    let mut crc_input = kind.to_vec();
    crc_input.extend_from_slice(data);
    bytes.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn malformed_content_pdf() -> Vec<u8> {
    let bytes = PdfDocument::new("malformed content")
        .with_pages(vec![PdfPage::new(Mm(10.0), Mm(10.0), Vec::new())])
        .save(&PdfSaveOptions::default(), &mut Vec::new());
    let mut document = lopdf::Document::load_mem(&bytes).unwrap();
    let page_id = *document.get_pages().values().next().unwrap();
    let stream_id = document.add_object(lopdf::Stream::new(
        lopdf::dictionary! {},
        b"BT [(unterminated".to_vec(),
    ));
    document
        .get_object_mut(page_id)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Contents", stream_id);
    let mut output = Vec::new();
    document.save_to(&mut output).unwrap();
    output
}

fn compressed_text_pdf(character_count: usize) -> Vec<u8> {
    let text = "A".repeat(character_count);
    let bytes = PdfDocument::new("compressed text bomb")
        .with_pages(vec![PdfPage::new(
            Mm(10.0),
            Mm(10.0),
            vec![
                Op::StartTextSection,
                Op::SetFontSizeBuiltinFont {
                    size: Pt(8.0),
                    font: BuiltinFont::Helvetica,
                },
                Op::WriteTextBuiltinFont {
                    items: vec![TextItem::Text(text)],
                    font: BuiltinFont::Helvetica,
                },
                Op::EndTextSection,
            ],
        )])
        .save(&PdfSaveOptions::default(), &mut Vec::new());
    let mut document = lopdf::Document::load_mem(&bytes).unwrap();
    let page_id = *document.get_pages().values().next().unwrap();
    for content_id in document.get_page_contents(page_id) {
        document
            .get_object_mut(content_id)
            .unwrap()
            .as_stream_mut()
            .unwrap()
            .compress()
            .unwrap();
    }
    let mut output = Vec::new();
    document.save_to(&mut output).unwrap();
    output
}

#[test]
fn process_ocr_sends_a_json_line_and_parses_success() {
    let gateway = ProcessOcrGateway::new(ocr_helper());
    let path = Path::new("success invoice.pdf");

    let result = gateway.recognize(path).expect("sidecar should succeed");

    assert_eq!(result.text, "北京 出租车 价税合计 ¥128.50");
    assert_eq!(result.warnings, ["low_confidence"]);
}

#[test]
fn process_gateway_uses_a_distinct_pdf_text_operation() {
    let gateway = ProcessOcrGateway::new(ocr_helper());

    let result = gateway
        .extract_pdf_text(&fixture("text-invoice.pdf"))
        .unwrap();

    assert!(result.text.contains("开票日期：2026年06月18日"));
    assert!(result.text.contains("价税合计（小写）¥128.50"));
    assert!(result.warnings.is_empty());
}

#[test]
fn process_gateway_reuses_one_sidecar_for_sequential_requests() {
    let gateway = ProcessOcrGateway::new(ocr_helper());
    let path = Path::new("persistent-session.pdf");

    let first_process = gateway.recognize(path).unwrap().text;
    let second_process = gateway.recognize(path).unwrap().text;

    assert_eq!(first_process, second_process);
}

#[test]
fn process_gateway_restarts_after_a_protocol_error() {
    let gateway = ProcessOcrGateway::new(ocr_helper());

    gateway
        .recognize(Path::new("malformed.pdf"))
        .expect_err("malformed response should fail");
    let recovered = gateway.recognize(Path::new("success invoice.pdf")).unwrap();

    assert_eq!(recovered.text, "北京 出租车 价税合计 ¥128.50");
}

#[test]
fn process_gateway_keeps_its_session_after_a_resource_error() {
    let gateway = ProcessOcrGateway::new(ocr_helper());
    let session_path = Path::new("persistent-session.pdf");
    let first_process = gateway.recognize(session_path).unwrap().text;

    gateway
        .extract_pdf_text(Path::new("compressed-text-bomb.pdf"))
        .expect_err("oversized PDF text should fail");
    let second_process = gateway.recognize(session_path).unwrap().text;

    assert_eq!(first_process, second_process);
}

#[test]
fn process_gateway_restarts_after_a_timeout() {
    let gateway = ProcessOcrGateway::with_timeout(ocr_helper(), Duration::from_millis(500));

    gateway
        .recognize(Path::new("timeout.pdf"))
        .expect_err("hung response should time out");
    let recovered = gateway.recognize(Path::new("success invoice.pdf")).unwrap();

    assert_eq!(recovered.text, "北京 出租车 价税合计 ¥128.50");
}

#[cfg(unix)]
#[test]
fn process_gateway_lock_wait_respects_each_call_deadline() {
    let _guard = sidecar_timing_test_guard();
    let directory = tempfile::tempdir().unwrap();
    let executable = lock_holder_ocr_helper(directory.path());
    let ready_socket = executable.with_extension("ready.sock");
    let ready_listener = std::os::unix::net::UnixListener::bind(&ready_socket).unwrap();
    let gateway = Arc::new(ProcessOcrGateway::with_timeout(
        executable,
        Duration::from_secs(1),
    ));
    gateway
        .recognize_with_timeout(Path::new("arm-lock-holder.pdf"), Duration::from_secs(5))
        .expect("helper should arm before the setup deadline");
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let ready = std::thread::spawn(move || {
        ready_tx
            .send(ready_listener.accept().map(|(connection, _)| connection))
            .unwrap();
    });
    let first_gateway = gateway.clone();
    let first =
        std::thread::spawn(move || first_gateway.recognize(Path::new("hold-session-lock.pdf")));
    let _ready_connection = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("helper should signal readiness before the setup deadline")
        .expect("helper readiness socket should accept");
    ready.join().unwrap();
    let started = Instant::now();

    let error = gateway
        .recognize(Path::new("second-request.pdf"))
        .expect_err("lock admission should respect the second request deadline");
    let elapsed = started.elapsed();
    let first_result = first.join().unwrap();

    assert_external(&error, "ocr_sidecar");
    assert!(first_result.is_err());
    assert!(
        elapsed >= Duration::from_millis(900),
        "second request failed before its admission deadline: elapsed={elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1_500),
        "second request waited beyond its own deadline: elapsed={elapsed:?}"
    );
}

#[cfg(unix)]
#[test]
fn process_ocr_write_respects_deadline_when_sidecar_does_not_read_stdin() {
    let _guard = sidecar_timing_test_guard();
    let directory = tempfile::tempdir().unwrap();
    let executable = no_stdin_ocr_helper(directory.path());
    let gateway = ProcessOcrGateway::with_timeout(executable, Duration::from_secs(1));
    let descendant_pid = gateway
        .recognize(Path::new("arm-no-stdin.pdf"))
        .expect("helper should arm before it stops reading stdin")
        .text
        .parse::<u32>()
        .expect("arm response should contain the descendant PID");
    let started = Instant::now();

    let error = gateway
        .recognize(&backpressure_path())
        .expect_err("stdin backpressure should time out");
    let elapsed = started.elapsed();

    assert_external(&error, "ocr_sidecar");
    assert!(
        elapsed < Duration::from_millis(1_500),
        "stdin write ignored the request deadline: elapsed={elapsed:?}"
    );
    let gone = (0..50).any(|_| {
        if !process_exists(descendant_pid) {
            true
        } else {
            std::thread::sleep(Duration::from_millis(20));
            false
        }
    });
    assert!(gone, "descendant {descendant_pid} survived write timeout");
}

#[test]
fn local_extractor_propagates_process_sidecar_warnings_end_to_end() {
    let gateway = Arc::new(ProcessOcrGateway::new(ocr_helper()));
    let extractor = LocalExtractor::new(gateway);

    let extracted = extractor.extract(&fixture("image-invoice.png")).unwrap();

    assert_eq!(extracted.warnings, ["low_confidence"]);
}

#[test]
fn process_ocr_bounds_and_sanitizes_warning_codes() {
    let gateway = ProcessOcrGateway::new(ocr_helper());

    let result = gateway
        .recognize(Path::new("warning-sanitize.pdf"))
        .unwrap();

    assert!(result.warnings.len() <= 16);
    assert!(
        result
            .warnings
            .iter()
            .all(|warning| warning.chars().count() <= 64)
    );
    assert!(
        result.warnings.iter().all(|warning| warning
            .chars()
            .all(|character| character.is_ascii_alphanumeric()
                || character == '_'
                || character == '-'))
    );
    assert!(!result.warnings.join(" ").contains("secret"));
}

#[test]
fn process_ocr_sanitizes_a_structured_sidecar_error() {
    let gateway = ProcessOcrGateway::new(ocr_helper());
    let path = Path::new("customer-secret-error.pdf");

    let error = gateway
        .recognize(path)
        .expect_err("sidecar error should fail");

    assert_external(&error, "ocr_sidecar");
    assert!(!error.to_string().contains("super-secret-sidecar-detail"));
    assert!(!error.to_string().contains("customer-secret"));
}

#[test]
fn process_ocr_rejects_malformed_output_without_exposing_it() {
    let gateway = ProcessOcrGateway::new(ocr_helper());

    let error = gateway
        .recognize(Path::new("malformed.pdf"))
        .expect_err("malformed response should fail");

    assert_external(&error, "ocr_sidecar");
    assert!(!error.to_string().contains("not-json"));
}

#[test]
fn process_ocr_does_not_expose_stderr_on_nonzero_exit() {
    let gateway = ProcessOcrGateway::new(ocr_helper());

    let error = gateway
        .recognize(Path::new("crash.pdf"))
        .expect_err("crashed sidecar should fail");

    assert_external(&error, "ocr_sidecar");
    assert!(!error.to_string().contains("stderr-secret"));
}

#[test]
fn process_ocr_kills_a_hung_sidecar_after_the_timeout() {
    let gateway = ProcessOcrGateway::with_timeout(ocr_helper(), Duration::from_millis(100));
    let started = Instant::now();

    let error = gateway
        .recognize(Path::new("timeout.pdf"))
        .expect_err("hung sidecar should time out");

    assert_external(&error, "ocr_sidecar");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[test]
fn process_ocr_timeout_kills_descendants_holding_output_open() {
    let _guard = sidecar_timing_test_guard();
    let gateway = ProcessOcrGateway::with_timeout(ocr_helper(), Duration::from_secs(1));
    let descendant_pid = gateway
        .recognize(Path::new("arm-descendant.pdf"))
        .expect("helper should return the descendant PID before the timeout trigger")
        .text
        .parse::<u32>()
        .expect("arm response should contain the descendant PID");
    let started = Instant::now();

    let error = gateway
        .recognize(Path::new("trigger-descendant-timeout.pdf"))
        .expect_err("sidecar process tree should time out");

    assert_external(&error, "ocr_sidecar");
    assert!(started.elapsed() < Duration::from_secs(2));
    let gone = (0..50).any(|_| {
        if !process_exists(descendant_pid) {
            true
        } else {
            std::thread::sleep(Duration::from_millis(20));
            false
        }
    });
    assert!(gone, "descendant {descendant_pid} survived timeout");
}

#[cfg(unix)]
#[test]
fn process_ocr_cleans_descendants_when_the_leader_exits_before_pipe_eof() {
    let _guard = sidecar_timing_test_guard();
    let gateway = ProcessOcrGateway::with_timeout(ocr_helper(), Duration::from_secs(1));
    let descendant_pid = gateway
        .recognize(Path::new("arm-descendant.pdf"))
        .expect("helper should return the descendant PID before the exit trigger")
        .text
        .parse::<u32>()
        .expect("arm response should contain the descendant PID");
    let started = Instant::now();

    let error = gateway
        .recognize(Path::new("trigger-orphan-pipe.pdf"))
        .expect_err("leader exit without a response should fail promptly");

    assert_external(&error, "ocr_sidecar");
    assert!(started.elapsed() < Duration::from_secs(2));
    let gone = (0..50).any(|_| {
        if !process_exists(descendant_pid) {
            true
        } else {
            std::thread::sleep(Duration::from_millis(20));
            false
        }
    });
    assert!(gone, "descendant {descendant_pid} survived leader exit");
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn process_ocr_maps_an_unstartable_sidecar_to_a_sanitized_error() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = ProcessOcrGateway::new(directory.path().join("private-missing-sidecar"));

    let error = gateway
        .recognize(Path::new("invoice.pdf"))
        .expect_err("missing sidecar should fail");

    assert_external(&error, "ocr_sidecar");
    assert!(!error.to_string().contains("private-missing-sidecar"));
}

#[test]
fn default_process_timeout_has_a_cold_start_and_work_budget() {
    let timeout = ProcessOcrGateway::default_timeout();
    let reported_cold_start = Duration::from_millis(22_230);

    assert_eq!(timeout, Duration::from_secs(80));
    assert!(timeout.saturating_sub(reported_cold_start) >= Duration::from_secs(45));
}

#[test]
#[ignore = "requires INVOICE_OCR_BIN pointing to a packaged one-file sidecar"]
fn packaged_sidecar_runs_through_process_gateway_with_cold_start_margin() {
    let executable = std::env::var_os("INVOICE_OCR_BIN").expect("INVOICE_OCR_BIN is required");
    let gateway = ProcessOcrGateway::new(executable);
    let total_started = Instant::now();
    let pdf_started = Instant::now();

    let pdf_text = gateway
        .extract_pdf_text(&fixture("text-invoice.pdf"))
        .unwrap();
    let pdf_elapsed = pdf_started.elapsed();

    assert!(pdf_text.text.contains("开票日期：2026年06月18日"));
    assert!(pdf_text.text.contains("价税合计（小写）¥128.50"));
    assert!(pdf_text.warnings.is_empty());
    assert!(
        ProcessOcrGateway::default_timeout().saturating_sub(pdf_elapsed) >= Duration::from_secs(45),
        "packaged PDF text sidecar left insufficient margin: elapsed={pdf_elapsed:?}"
    );

    let first_ocr_started = Instant::now();

    let result = gateway.recognize(&fixture("image-invoice.png")).unwrap();
    let first_ocr_elapsed = first_ocr_started.elapsed();
    let total_elapsed = total_started.elapsed();

    assert!(result.text.contains("北京"));
    assert!(result.text.contains("128.50"));
    assert!(
        first_ocr_elapsed < Duration::from_secs(45),
        "packaged first OCR exceeded its measured engine-start budget: elapsed={first_ocr_elapsed:?}"
    );
    assert!(
        ProcessOcrGateway::default_timeout().saturating_sub(first_ocr_elapsed)
            >= Duration::from_secs(35),
        "packaged first OCR left insufficient timeout margin: elapsed={first_ocr_elapsed:?}"
    );

    let steady_ocr_started = Instant::now();
    let steady_result = gateway.recognize(&fixture("image-invoice.png")).unwrap();
    let steady_ocr_elapsed = steady_ocr_started.elapsed();

    assert!(steady_result.text.contains("北京"));
    assert!(steady_result.text.contains("128.50"));
    eprintln!(
        "packaged sidecar timings: cold_pdf={pdf_elapsed:?}, first_ocr={first_ocr_elapsed:?}, first_workflow={total_elapsed:?}, steady_ocr={steady_ocr_elapsed:?}"
    );
    assert!(
        steady_ocr_elapsed < Duration::from_secs(10),
        "persistent OCR session did not reach steady-state performance: elapsed={steady_ocr_elapsed:?}"
    );
}

fn ocr_helper() -> PathBuf {
    static HELPER: OnceLock<PathBuf> = OnceLock::new();
    HELPER
        .get_or_init(|| {
            let directory = tempfile::tempdir().unwrap().keep();
            let executable = directory.join(format!(
                "ocr-sidecar-helper{}",
                std::env::consts::EXE_SUFFIX
            ));
            let source = fixture("ocr-sidecar-helper.rs");
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let status = std::process::Command::new(rustc)
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .status()
                .expect("Rust OCR helper compiler should start");
            assert!(status.success(), "OCR helper should compile");
            executable
        })
        .clone()
}

#[cfg(unix)]
fn no_stdin_ocr_helper(directory: &Path) -> PathBuf {
    let executable = directory.join("ocr-sidecar-no-stdin");
    std::fs::copy(ocr_helper(), &executable).unwrap();
    executable
}

#[cfg(unix)]
fn lock_holder_ocr_helper(directory: &Path) -> PathBuf {
    let executable = directory.join("ocr-sidecar-lock-holder");
    std::fs::copy(ocr_helper(), &executable).unwrap();
    executable
}

#[cfg(unix)]
fn backpressure_path() -> PathBuf {
    PathBuf::from("x".repeat(2 * 1024 * 1024))
}

#[cfg(unix)]
fn sidecar_timing_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
