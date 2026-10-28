//! Real invoices are PDF 1.5+ documents whose objects live in `/ObjStm`
//! object streams described by a compressed `/Type /XRef` stream. These are
//! integration tests for the input preflight and the extractor: such documents
//! must be accepted, normalized to a flat classic-xref PDF, and errors that are
//! not resource limits must not be reported as resource limits.

use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::infra::extraction::{
    DocumentExtractor, LocalExtractor, OcrGateway, OcrResult,
};

const RESOURCE_LIMIT_MESSAGE: &str = "Document exceeds extraction resource limits.";
const UNSUPPORTED_MESSAGE: &str = "PDF uses an unsupported structure.";
const ENCRYPTED_MESSAGE: &str = "Document is encrypted; unlock the PDF before importing.";
const INVOICE_TEXT: &str = "开票日期：2026年06月18日\n价税合计（小写）¥128.50";

#[derive(Default)]
struct FakeOcr {
    calls: AtomicUsize,
    pdf_text_calls: AtomicUsize,
    text: String,
    pdf_text: String,
}

impl FakeOcr {
    fn returning_pdf_text(text: impl Into<String>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            pdf_text_calls: AtomicUsize::new(0),
            text: "must not OCR".to_owned(),
            pdf_text: text.into(),
        }
    }

    fn returning(text: impl Into<String>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            pdf_text_calls: AtomicUsize::new(0),
            text: text.into(),
            pdf_text: String::new(),
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
            warnings: Vec::new(),
        })
    }

    fn extract_pdf_text(&self, _path: &Path) -> Result<OcrResult, AppError> {
        self.pdf_text_calls.fetch_add(1, Ordering::SeqCst);
        Ok(OcrResult {
            text: self.pdf_text.clone(),
            warnings: Vec::new(),
        })
    }
}

#[test]
fn object_stream_pdf_is_extracted_and_normalized_to_classic_xref() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("object-stream-invoice.pdf");
    let bytes = object_stream_pdf(true);
    assert!(bytes.windows(8).any(|window| window == b"/ObjStm "));
    assert!(
        bytes
            .windows(b"/Type /XRef".len())
            .any(|window| window == b"/Type /XRef")
    );
    std::fs::write(&path, &bytes).unwrap();
    let ocr = Arc::new(FakeOcr::returning_pdf_text(INVOICE_TEXT));
    let extractor = LocalExtractor::new(ocr.clone());

    let extracted = extractor
        .extract(&path)
        .expect("standard object stream PDF should extract");

    assert_eq!(extracted.text, INVOICE_TEXT);
    assert_eq!(ocr.pdf_text_call_count(), 1);
    assert_eq!(ocr.call_count(), 0);
    let normalized = extracted
        .normalized_pdf
        .expect("object stream PDF should normalize");
    assert!(normalized.windows(6).any(|window| window == b"\nxref\n"));
    assert!(!normalized.windows(7).any(|window| window == b"/ObjStm"));
    let document = lopdf::Document::load_mem(&normalized).expect("normalized PDF should reopen");
    assert_eq!(document.get_pages().len(), 1);
}

#[test]
fn object_stream_pdf_with_png_predictor_is_accepted() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("predictor-invoice.pdf");
    std::fs::write(&path, object_stream_pdf(false)).unwrap();
    let ocr = Arc::new(FakeOcr::returning_pdf_text(INVOICE_TEXT));
    let extractor = LocalExtractor::new(ocr.clone());

    let extracted = extractor
        .extract(&path)
        .expect("predictor-compressed xref stream should extract");

    assert_eq!(extracted.text, INVOICE_TEXT);
    assert_eq!(ocr.pdf_text_call_count(), 1);
}

#[test]
fn encrypted_pdf_is_reported_as_encrypted_without_running_ocr() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("encrypted.pdf");
    std::fs::write(&path, encrypted_pdf()).unwrap();
    let ocr = Arc::new(FakeOcr::returning("must not OCR"));
    let extractor = LocalExtractor::new(ocr.clone());

    let error = extractor
        .extract(&path)
        .expect_err("encrypted PDF should be rejected");

    assert_external(&error, "document_extractor");
    assert_eq!(error.to_string(), ENCRYPTED_MESSAGE);
    assert_ne!(error.to_string(), RESOURCE_LIMIT_MESSAGE);
    assert_eq!(ocr.pdf_text_call_count(), 0);
    assert_eq!(ocr.call_count(), 0);
}

#[test]
fn unsupported_structure_is_reported_differs_from_the_resource_limit_message() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unsupported-filter.pdf");
    std::fs::write(&path, object_stream_pdf_with_unsupported_filter()).unwrap();
    let ocr = Arc::new(FakeOcr::returning("must not OCR"));
    let extractor = LocalExtractor::new(ocr.clone());

    let error = extractor
        .extract(&path)
        .expect_err("unsupported xref filter should be rejected");

    assert_external(&error, "document_extractor");
    assert_eq!(error.to_string(), UNSUPPORTED_MESSAGE);
    assert_ne!(error.to_string(), RESOURCE_LIMIT_MESSAGE);
    assert_ne!(error.to_string(), ENCRYPTED_MESSAGE);
    assert_eq!(ocr.pdf_text_call_count(), 0);
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

/// A one-page PDF whose catalog, page tree and page dictionary live in an
/// object stream addressed by a compressed cross-reference stream. Mirrors what
/// real e-invoice exports look like (60 KB, one page, `/ObjStm` + `/Type /XRef`).
fn object_stream_pdf(png_predictor: bool) -> Vec<u8> {
    build_object_stream_pdf(png_predictor, false)
}

fn object_stream_pdf_with_unsupported_filter() -> Vec<u8> {
    build_object_stream_pdf(true, true)
}

fn build_object_stream_pdf(png_predictor: bool, unsupported_filter: bool) -> Vec<u8> {
    let contained_bodies: [&[u8]; 3] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>",
    ];
    let mut object_stream_index = String::new();
    let mut object_stream_contents = Vec::new();
    for (position, body) in contained_bodies.iter().enumerate() {
        write!(
            &mut object_stream_index,
            "{} {} ",
            position + 1,
            object_stream_contents.len()
        )
        .unwrap();
        object_stream_contents.extend_from_slice(body);
        object_stream_contents.push(b' ');
    }
    let first = object_stream_index.len();
    let mut object_stream_decoded = object_stream_index.into_bytes();
    object_stream_decoded.extend_from_slice(&object_stream_contents);
    let object_stream_compressed = zlib(&object_stream_decoded);

    let content_stream = b"BT (Invoice 128.50) Tj ET";
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let mut offsets = [0_usize; 7];

    offsets[4] = bytes.len();
    bytes.extend_from_slice(b"4 0 obj\n<< /Length ");
    write!(&mut bytes, "{}", content_stream.len()).unwrap();
    bytes.extend_from_slice(b" >>\nstream\n");
    bytes.extend_from_slice(content_stream);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    offsets[5] = bytes.len();
    bytes.extend_from_slice(b"5 0 obj\n<< /Type /ObjStm /N 3 /First ");
    write!(&mut bytes, "{first}").unwrap();
    bytes.extend_from_slice(b" /Filter /FlateDecode /Length ");
    write!(&mut bytes, "{}", object_stream_compressed.len()).unwrap();
    bytes.extend_from_slice(b" >>\nstream\n");
    bytes.extend_from_slice(&object_stream_compressed);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    offsets[6] = bytes.len();
    let mut table = Vec::new();
    let mut push_entry = |entry_type: u8, field_two: u32, field_three: u16| {
        table.push(entry_type);
        table.extend_from_slice(&field_two.to_be_bytes());
        table.extend_from_slice(&field_three.to_be_bytes());
    };
    push_entry(0, 0, 65535);
    push_entry(2, 5, 0);
    push_entry(2, 5, 1);
    push_entry(2, 5, 2);
    push_entry(1, offsets[4] as u32, 0);
    push_entry(1, offsets[5] as u32, 0);
    push_entry(1, offsets[6] as u32, 0);
    let (encoded_table, decode_parms) = if png_predictor {
        (encode_png_up_rows(&table, 7), true)
    } else {
        (table, false)
    };
    let stored_table = zlib(&encoded_table);

    bytes.extend_from_slice(
        b"6 0 obj\n<< /Type /XRef /Size 7 /Root 1 0 R /W [1 4 2] /Index [0 7] /Filter ",
    );
    if unsupported_filter {
        bytes.extend_from_slice(b"/LZWDecode");
    } else {
        bytes.extend_from_slice(b"/FlateDecode");
    }
    if decode_parms {
        bytes.extend_from_slice(b" /DecodeParms << /Columns 7 /Predictor 12 >>");
    }
    bytes.extend_from_slice(b" /Length ");
    write!(&mut bytes, "{}", stored_table.len()).unwrap();
    bytes.extend_from_slice(b" >>\nstream\n");
    bytes.extend_from_slice(&stored_table);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");
    write!(&mut bytes, "startxref\n{}\n%%EOF\n", offsets[6]).unwrap();
    bytes
}

fn encrypted_pdf() -> Vec<u8> {
    let objects: [&[u8]; 4] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>",
        b"<< /Filter /Standard /V 1 /R 2 /O (owner) /U (user) /P -4 >>",
    ];
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        writeln!(&mut bytes, "{} 0 obj", index + 1).unwrap();
        bytes.extend_from_slice(object);
        bytes.extend_from_slice(b"\nendobj\n");
    }
    let xref_offset = bytes.len();
    bytes.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets {
        writeln!(&mut bytes, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        &mut bytes,
        "trailer\n<< /Size 5 /Root 1 0 R /Encrypt 4 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
    )
    .unwrap();
    bytes
}

fn encode_png_up_rows(table: &[u8], columns: usize) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(table.len() + table.len() / columns);
    let mut previous = vec![0_u8; columns];
    for row in table.chunks_exact(columns) {
        encoded.push(2);
        for (index, byte) in row.iter().enumerate() {
            encoded.push(byte.wrapping_sub(previous[index]));
        }
        previous.copy_from_slice(row);
    }
    encoded
}

fn zlib(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}
