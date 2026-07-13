use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::infra::extraction::{
    DocumentExtractor, LocalExtractor, OcrGateway, ProcessOcrGateway,
};
use printpdf::{BuiltinFont, Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, Pt, TextItem};

#[derive(Default)]
struct FakeOcr {
    calls: AtomicUsize,
    text: String,
}

impl FakeOcr {
    fn returning(text: impl Into<String>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            text: text.into(),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl OcrGateway for FakeOcr {
    fn recognize(&self, _path: &Path) -> Result<String, AppError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.text.clone())
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn text_pdf_is_extracted_without_calling_ocr() {
    let ocr = Arc::new(FakeOcr::returning("should not be used"));
    let extractor = LocalExtractor::new(ocr.clone());
    let path = fixture("text-invoice.pdf");

    let extracted = extractor.extract(&path).expect("text PDF should extract");

    assert!(extracted.text.contains("开票日期：2026年06月18日"));
    assert!(extracted.text.contains("价税合计（小写）¥128.50"));
    assert_eq!(extracted.normalized_pdf, Some(std::fs::read(path).unwrap()));
    assert!(extracted.warnings.is_empty());
    assert_eq!(ocr.call_count(), 0);
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
    let ocr = Arc::new(FakeOcr::returning("unused"));
    let extractor = LocalExtractor::new(ocr.clone());

    let extracted = extractor.extract(&path).unwrap();

    assert!(extracted.text.contains("12345678901234567890"));
    assert_eq!(ocr.call_count(), 0);
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
    assert_eq!(ocr.call_count(), 0);
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
    assert_eq!(ocr.call_count(), 0);
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

#[test]
fn process_ocr_sends_a_json_line_and_parses_success() {
    let gateway = ProcessOcrGateway::new(ocr_helper());
    let path = Path::new("success invoice.pdf");

    let text = gateway.recognize(path).expect("sidecar should succeed");

    assert_eq!(text, "北京 出租车 价税合计 ¥128.50");
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
