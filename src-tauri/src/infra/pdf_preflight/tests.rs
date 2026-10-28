//! Tests for `pdf_preflight`, kept out of the production file.
use std::fmt::Write as _;
use std::io::Write;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use lopdf::{Document, Object, dictionary};

use super::{
    MAX_XREF_ENTRIES, PdfPreflightError, RawLexer, RawToken, reject_unsafe_object_dictionary,
    reset_header_parse_offsets, take_header_parse_offsets, validate_pdf_input_structure_with_limit,
    validate_pdf_structure_with_limit,
};

fn validate_pdf_structure(bytes: &[u8]) -> Result<(), PdfPreflightError> {
    validate_pdf_structure_with_limit(bytes, MAX_XREF_ENTRIES)
}

fn validate_pdf_input_structure(bytes: &[u8]) -> Result<(), PdfPreflightError> {
    validate_pdf_input_structure_with_limit(bytes, MAX_XREF_ENTRIES)
}

#[test]
fn accepts_uncompressed_xref_stream_without_compressed_entries() {
    validate_pdf_structure(&lopdf_xref_stream_pdf()).unwrap();
}

#[test]
fn accepts_nested_noncritical_xref_stream_dictionary_values() {
    let bytes = handcrafted_xref_stream_pdf(
        1,
        b"/Meta << /Type /ObjStm /Filter /FlateDecode /Length 1 /Size 1 /W [8 8 8] /Index [0 1] /Prev 0 >> ",
    );

    validate_pdf_structure(&bytes).unwrap();
}

#[test]
fn rejects_filtered_xref_stream() {
    let bytes = handcrafted_xref_stream_pdf(1, b"/Filter /FlateDecode ");

    let error = validate_pdf_structure(&bytes).unwrap_err();
    assert!(error.to_string().contains("压缩对象"));
}

#[test]
fn rejects_xref_stream_with_compressed_object_entry() {
    let bytes = handcrafted_xref_stream_pdf(2, b"");

    let error = validate_pdf_structure(&bytes).unwrap_err();
    assert!(error.to_string().contains("压缩对象"));
}

#[test]
fn rejects_classic_xref_entries_whose_object_header_does_not_match() {
    for bytes in [
        classic_pdf_with_entry_override(2, 0, 1),
        classic_pdf_with_entry_override(1, 1, 1),
    ] {
        assert_eq!(
            validate_pdf_structure(&bytes),
            Err(PdfPreflightError::Invalid)
        );
    }
}

#[test]
fn rejects_xref_stream_entries_whose_object_header_does_not_match() {
    for bytes in [
        handcrafted_xref_stream_pdf_with_page_entry(1, 1, 0, b""),
        handcrafted_xref_stream_pdf_with_page_entry(1, 3, 1, b""),
    ] {
        assert_eq!(
            validate_pdf_structure(&bytes),
            Err(PdfPreflightError::Invalid)
        );
    }
}

#[test]
fn free_xref_entries_do_not_consume_the_active_entry_budget() {
    let bytes = classic_pdf_with_free_xref_entries(20_001);

    validate_pdf_structure_with_limit(&bytes, 3).unwrap();
}

#[test]
fn repeated_entries_parse_each_unique_header_offset_once() {
    let (bytes, shared_offset) = classic_pdf_with_repeated_commented_entry(2_000, 64 * 1024);
    reset_header_parse_offsets();

    validate_pdf_structure_with_limit(&bytes, MAX_XREF_ENTRIES).unwrap();

    let parsed_offsets = take_header_parse_offsets();
    assert_eq!(
        parsed_offsets
            .iter()
            .filter(|offset| **offset == shared_offset)
            .count(),
        1
    );
}

#[test]
fn newest_normal_entry_shadows_older_active_entry_for_the_budget() {
    let bytes = incremental_pdf_with_latest_entry(b"n", true);

    validate_pdf_structure_with_limit(&bytes, 3).unwrap();
}

#[test]
fn newest_free_entry_does_not_hide_older_normal_from_loader_budget() {
    let bytes = incremental_pdf_with_latest_entry(b"f", false);
    let loaded = Document::load_mem(&bytes).unwrap();

    assert!(loaded.objects.contains_key(&(1, 0)));

    assert_eq!(
        validate_pdf_structure_with_limit(&bytes, 2),
        Err(PdfPreflightError::ResourceLimit)
    );
}

#[test]
fn historical_unique_objects_still_consume_the_active_entry_budget() {
    let bytes = incremental_pdf_with_unique_object();

    assert_eq!(
        validate_pdf_structure_with_limit(&bytes, 3),
        Err(PdfPreflightError::ResourceLimit)
    );
}

#[test]
fn accepts_dangerous_names_inside_strings_and_stream_content() {
    let bytes = classic_pdf(
        b"<< /Length 8 /Note (/Obj#53tm) >>\nstream\n/ObjStm\nendstream",
        b"",
    );

    validate_pdf_structure(&bytes).unwrap();
}

#[test]
fn rejects_escaped_object_and_hybrid_xref_names() {
    let object_dictionary = b"<< /Type /Obj#53tm /N 0 /First 0 /Length 0 >>\nstream\n\nendstream";
    let mut name_lexer = RawLexer::new(b"/Obj#53tm");
    assert!(matches!(
        name_lexer.next_token().unwrap(),
        RawToken::Name(name) if name == b"ObjStm"
    ));
    assert!(reject_unsafe_object_dictionary(object_dictionary).is_err());
    let object_stream = classic_pdf(object_dictionary, b"");
    let hybrid = classic_pdf(b"null", b"/XRef#53tm 42");

    assert!(validate_pdf_structure(&object_stream).is_err());
    assert!(validate_pdf_structure(&hybrid).is_err());
}

#[test]
fn rejects_startxref_that_points_to_an_indirect_object() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let object_offset = bytes.len();
    bytes.extend_from_slice(b"1 0 obj\n<< /Type /XRef >>\nendobj\n");
    write!(&mut bytes, "startxref\n{object_offset}\n%%EOF\n").unwrap();

    assert!(validate_pdf_structure(&bytes).is_err());
}

#[test]
fn rejects_trailing_startxref_that_lopdf_does_not_accept() {
    let bytes = pdf_with_malicious_terminal_graph_and_benign_trailing_graph();
    assert!(Document::load_mem(&bytes).is_err());

    assert!(validate_pdf_structure(&bytes).is_err());
}

#[test]
fn rejects_nested_prev_that_hides_lopdf_malicious_previous_graph() {
    let bytes = pdf_with_top_level_malicious_prev_and_nested_benign_prev();
    let loaded = Document::load_mem(&bytes).unwrap();
    assert!(loaded.objects.values().any(|object| {
        object
            .as_stream()
            .is_ok_and(|stream| stream.dict.has_type(b"ObjStm"))
    }));

    assert!(validate_pdf_structure(&bytes).is_err());
}

#[test]
fn accepts_multiple_classic_xref_revisions() {
    let bytes = incremental_classic_pdf();
    let loaded = Document::load_mem(&bytes).unwrap();
    assert_eq!(loaded.objects.len(), 5);

    validate_pdf_structure(&bytes).unwrap();
}

#[test]
fn rejects_non_direct_or_ambiguous_classic_prev_values() {
    let base = classic_pdf(b"null", b"");
    let xref_start = base
        .windows(b"xref".len())
        .position(|window| window == b"xref")
        .unwrap();
    let cycle = format!("/Prev {xref_start}");
    for bytes in [
        classic_pdf(b"null", b"/Prev 1 /Prev 2"),
        classic_pdf(b"null", b"/Prev 1 0 R"),
        classic_pdf(b"null", b"/Prev /Other"),
        classic_pdf(b"null", b"/Prev 999999"),
        classic_pdf(b"null", cycle.as_bytes()),
    ] {
        assert!(validate_pdf_structure(&bytes).is_err());
    }
}

#[test]
fn terminal_tail_allows_only_pdf_whitespace_after_eof() {
    let mut whitespace = classic_pdf(b"null", b"");
    whitespace.extend_from_slice(b"\0\t\n\x0c\r ");
    validate_pdf_structure(&whitespace).unwrap();

    let mut comment = classic_pdf(b"null", b"");
    comment.extend_from_slice(b"% trailing comment\n");
    assert!(validate_pdf_structure(&comment).is_err());
}

fn classic_pdf(extra_object: &[u8], trailer_extra: &[u8]) -> Vec<u8> {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".as_slice(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 150] >>".as_slice(),
        extra_object,
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
    bytes.extend_from_slice(b"trailer\n<< /Size 5 /Root 1 0 R ");
    bytes.extend_from_slice(trailer_extra);
    write!(&mut bytes, ">>\nstartxref\n{xref_offset}\n%%EOF\n").unwrap();
    bytes
}

fn classic_pdf_with_entry_override(
    target_object: usize,
    generation: usize,
    offset_object: usize,
) -> Vec<u8> {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".as_slice(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 150] >>".as_slice(),
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
    bytes.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
    for object_number in 1..=objects.len() {
        let offset = if object_number == target_object {
            offsets[offset_object - 1]
        } else {
            offsets[object_number - 1]
        };
        let generation = if object_number == target_object {
            generation
        } else {
            0
        };
        writeln!(&mut bytes, "{offset:010} {generation:05} n ").unwrap();
    }
    write!(
        &mut bytes,
        "trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
    )
    .unwrap();
    bytes
}

fn classic_pdf_with_free_xref_entries(free_count: usize) -> Vec<u8> {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".as_slice(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 150] >>".as_slice(),
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
    bytes.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
    for offset in offsets {
        writeln!(&mut bytes, "{offset:010} 00000 n ").unwrap();
    }
    if free_count != 0 {
        writeln!(&mut bytes, "4 {free_count}").unwrap();
        for _ in 0..free_count {
            bytes.extend_from_slice(b"0000000000 65535 f \n");
        }
    }
    write!(
        &mut bytes,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
        free_count + 4,
    )
    .unwrap();
    bytes
}

fn classic_pdf_with_repeated_commented_entry(
    repeated_entries: usize,
    comment_bytes: usize,
) -> (Vec<u8>, usize) {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let shared_offset = bytes.len();
    bytes.push(b'%');
    bytes.extend(std::iter::repeat_n(b'x', comment_bytes));
    bytes.push(b'\n');
    bytes.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_offset = bytes.len();
    bytes.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_offset = bytes.len();
    bytes.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 150] >>\nendobj\n",
    );

    let xref_offset = bytes.len();
    bytes.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
    for offset in [shared_offset, pages_offset, page_offset] {
        writeln!(&mut bytes, "{offset:010} 00000 n ").unwrap();
    }
    for _ in 0..repeated_entries {
        writeln!(&mut bytes, "1 1\n{shared_offset:010} 00000 n ").unwrap();
    }
    write!(
        &mut bytes,
        "trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
    )
    .unwrap();
    (bytes, shared_offset)
}

fn incremental_pdf_with_latest_entry(status: &[u8], write_updated_object: bool) -> Vec<u8> {
    let mut bytes = classic_pdf_with_free_xref_entries(0);
    let previous_xref = bytes
        .windows(b"xref".len())
        .position(|window| window == b"xref")
        .unwrap();
    let entry_offset = if write_updated_object {
        let offset = bytes.len();
        bytes.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        offset
    } else {
        0
    };
    let latest_xref = bytes.len();
    bytes.extend_from_slice(b"xref\n1 1\n");
    writeln!(
        &mut bytes,
        "{entry_offset:010} 00000 {} ",
        std::str::from_utf8(status).unwrap()
    )
    .unwrap();
    write!(
        &mut bytes,
        "trailer\n<< /Size 4 /Root 1 0 R /Prev {previous_xref} >>\nstartxref\n{latest_xref}\n%%EOF\n"
    )
    .unwrap();
    bytes
}

fn incremental_pdf_with_unique_object() -> Vec<u8> {
    let mut bytes = classic_pdf_with_free_xref_entries(0);
    let previous_xref = bytes
        .windows(b"xref".len())
        .position(|window| window == b"xref")
        .unwrap();
    let object_offset = bytes.len();
    bytes.extend_from_slice(b"4 0 obj\nnull\nendobj\n");
    let latest_xref = bytes.len();
    write!(
        &mut bytes,
        "xref\n4 1\n{object_offset:010} 00000 n \ntrailer\n<< /Size 5 /Root 1 0 R /Prev {previous_xref} >>\nstartxref\n{latest_xref}\n%%EOF\n"
    )
    .unwrap();
    bytes
}

fn pdf_with_malicious_terminal_graph_and_benign_trailing_graph() -> Vec<u8> {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".as_slice(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 150] >>".as_slice(),
        b"<< /Type /ObjStm /N 0 /First 0 /Length 0 >>\nstream\n\nendstream".as_slice(),
    ];
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        writeln!(&mut bytes, "{} 0 obj", index + 1).unwrap();
        bytes.extend_from_slice(object);
        bytes.extend_from_slice(b"\nendobj\n");
    }

    let malicious_xref = bytes.len();
    bytes.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in &offsets {
        writeln!(&mut bytes, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        &mut bytes,
        "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{malicious_xref}\n%%EOF\n"
    )
    .unwrap();

    let benign_xref = bytes.len();
    bytes.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
    for offset in &offsets[..3] {
        writeln!(&mut bytes, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        &mut bytes,
        "trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{benign_xref}\n"
    )
    .unwrap();
    bytes
}

fn pdf_with_top_level_malicious_prev_and_nested_benign_prev() -> Vec<u8> {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".as_slice(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 150] >>".as_slice(),
        b"<< /Type /ObjStm /N 0 /First 0 /Length 0 >>\nstream\n\nendstream".as_slice(),
    ];
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        writeln!(&mut bytes, "{} 0 obj", index + 1).unwrap();
        bytes.extend_from_slice(object);
        bytes.extend_from_slice(b"\nendobj\n");
    }

    let malicious_xref = bytes.len();
    bytes.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in &offsets {
        writeln!(&mut bytes, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        &mut bytes,
        "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{malicious_xref}\n%%EOF\n"
    )
    .unwrap();

    let benign_xref = bytes.len();
    bytes.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
    for offset in &offsets[..3] {
        writeln!(&mut bytes, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        &mut bytes,
        "trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{benign_xref}\n%%EOF\n"
    )
    .unwrap();

    let latest_xref = bytes.len();
    bytes.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
    for offset in &offsets[..3] {
        writeln!(&mut bytes, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        &mut bytes,
        "trailer\n<< /Size 4 /Root 1 0 R /Prev {malicious_xref} /Meta << /Prev {benign_xref} >> >>\nstartxref\n{latest_xref}\n%%EOF\n"
    )
    .unwrap();
    bytes
}

fn incremental_classic_pdf() -> Vec<u8> {
    let mut bytes = classic_pdf(b"null", b"");
    let previous_xref = bytes
        .windows(b"xref".len())
        .position(|window| window == b"xref")
        .unwrap();
    let update_object = bytes.len();
    bytes.extend_from_slice(b"5 0 obj\n<< /Producer (incremental update) >>\nendobj\n");
    let latest_xref = bytes.len();
    write!(
        &mut bytes,
        "xref\n5 1\n{update_object:010} 00000 n \ntrailer\n<< /Size 6 /Root 1 0 R /Prev {previous_xref} >>\nstartxref\n{latest_xref}\n%%EOF\n"
    )
    .unwrap();
    bytes
}

fn lopdf_xref_stream_pdf() -> Vec<u8> {
    let mut document = Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 100.into(), 150.into()],
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn handcrafted_xref_stream_pdf(page_entry_type: u8, dictionary_extra: &[u8]) -> Vec<u8> {
    handcrafted_xref_stream_pdf_with_page_entry(page_entry_type, 3, 0, dictionary_extra)
}

fn handcrafted_xref_stream_pdf_with_page_entry(
    page_entry_type: u8,
    page_offset_object: usize,
    page_generation: u16,
    dictionary_extra: &[u8],
) -> Vec<u8> {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".as_slice(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 150] >>".as_slice(),
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
    let mut entries = Vec::new();
    write_xref_stream_entry(&mut entries, 0, 0, u16::MAX);
    write_xref_stream_entry(&mut entries, 1, offsets[0], 0);
    write_xref_stream_entry(&mut entries, 1, offsets[1], 0);
    write_xref_stream_entry(
        &mut entries,
        page_entry_type,
        offsets[page_offset_object - 1],
        page_generation,
    );
    write_xref_stream_entry(&mut entries, 1, xref_offset, 0);
    write!(
        &mut bytes,
        "4 0 obj\n<< /Type /XRef /Size 5 /Root 1 0 R /W [1 4 2] /Index [0 5] /Length {} ",
        entries.len()
    )
    .unwrap();
    bytes.extend_from_slice(dictionary_extra);
    bytes.extend_from_slice(b">>\nstream\n");
    bytes.extend_from_slice(&entries);
    write!(
        &mut bytes,
        "\nendstream\nendobj\nstartxref\n{xref_offset}\n%%EOF\n"
    )
    .unwrap();
    bytes
}

fn write_xref_stream_entry(entries: &mut Vec<u8>, entry_type: u8, offset: usize, generation: u16) {
    entries.push(entry_type);
    entries.extend_from_slice(&(offset as u32).to_be_bytes());
    entries.extend_from_slice(&generation.to_be_bytes());
}

// ---------------------------------------------------------------------------
// Input preflight: standard object streams and cross-reference streams
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct ObjectStreamPdfConfig {
    xref_compressed: bool,
    png_predictor: bool,
    unsupported_filter: bool,
    corrupt_container: bool,
    corrupt_index: bool,
    omit_contained_object: bool,
    bad_normal_offset: bool,
    omit_self_entry: bool,
    bad_self_offset: bool,
    corrupt_xref_content: bool,
}

impl Default for ObjectStreamPdfConfig {
    fn default() -> Self {
        Self {
            xref_compressed: true,
            png_predictor: true,
            unsupported_filter: false,
            corrupt_container: false,
            corrupt_index: false,
            omit_contained_object: false,
            bad_normal_offset: false,
            omit_self_entry: false,
            bad_self_offset: false,
            corrupt_xref_content: false,
        }
    }
}

#[test]
fn accepts_compressed_xref_stream_with_object_stream() {
    let bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig::default());

    validate_pdf_input_structure(&bytes).unwrap();

    let loaded = Document::load_mem(&bytes).unwrap();
    assert_eq!(loaded.get_pages().len(), 1);
    assert!(loaded.objects.values().any(|object| {
        object
            .as_stream()
            .is_ok_and(|stream| stream.dict.has_type(b"ObjStm"))
    }));
}

#[test]
fn accepts_uncompressed_xref_stream_with_object_stream() {
    let bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig {
        xref_compressed: false,
        png_predictor: false,
        ..ObjectStreamPdfConfig::default()
    });

    validate_pdf_input_structure(&bytes).unwrap();
}

#[test]
fn input_preflight_still_enforces_the_active_object_budget() {
    let bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig::default());

    assert_eq!(
        validate_pdf_input_structure_with_limit(&bytes, 2),
        Err(PdfPreflightError::ResourceLimit)
    );
}

#[test]
fn rejects_compressed_object_entry_with_out_of_range_container() {
    let bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig {
        corrupt_container: true,
        ..ObjectStreamPdfConfig::default()
    });

    assert_eq!(
        validate_pdf_input_structure(&bytes),
        Err(PdfPreflightError::Invalid)
    );
}

#[test]
fn rejects_object_stream_index_that_disagrees_with_the_xref() {
    let bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig {
        corrupt_index: true,
        ..ObjectStreamPdfConfig::default()
    });

    assert_eq!(
        validate_pdf_input_structure(&bytes),
        Err(PdfPreflightError::Invalid)
    );
}

#[test]
fn rejects_object_stream_objects_the_xref_does_not_account_for() {
    let bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig {
        omit_contained_object: true,
        ..ObjectStreamPdfConfig::default()
    });

    assert_eq!(
        validate_pdf_input_structure(&bytes),
        Err(PdfPreflightError::Invalid)
    );
}

#[test]
fn rejects_normal_xref_entries_with_out_of_range_offsets() {
    let bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig {
        bad_normal_offset: true,
        ..ObjectStreamPdfConfig::default()
    });

    assert_eq!(
        validate_pdf_input_structure(&bytes),
        Err(PdfPreflightError::Invalid)
    );
}

#[test]
fn accepts_xref_stream_without_its_own_entry() {
    // Real generators routinely leave the cross-reference stream out of the
    // table (or mark it free); the loader never needs that entry.
    let bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig {
        omit_self_entry: true,
        ..ObjectStreamPdfConfig::default()
    });

    validate_pdf_input_structure(&bytes).unwrap();
}

#[test]
fn rejects_xref_stream_self_entry_that_points_somewhere_else() {
    let bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig {
        bad_self_offset: true,
        ..ObjectStreamPdfConfig::default()
    });

    assert_eq!(
        validate_pdf_input_structure(&bytes),
        Err(PdfPreflightError::Invalid)
    );
}

#[test]
fn rejects_unsupported_xref_stream_filters_as_unsupported() {
    let bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig {
        unsupported_filter: true,
        ..ObjectStreamPdfConfig::default()
    });

    assert_eq!(
        validate_pdf_input_structure(&bytes),
        Err(PdfPreflightError::Unsupported)
    );
}

#[test]
fn rejects_corrupt_compressed_xref_stream() {
    let bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig {
        corrupt_xref_content: true,
        ..ObjectStreamPdfConfig::default()
    });

    assert_eq!(
        validate_pdf_input_structure(&bytes),
        Err(PdfPreflightError::Invalid)
    );
}

#[test]
fn classifies_encrypted_trailers_as_encrypted() {
    let bytes = classic_pdf(b"null", b"/Encrypt 4 0 R");

    assert_eq!(
        validate_pdf_input_structure(&bytes),
        Err(PdfPreflightError::Encrypted)
    );
}

#[test]
fn input_preflight_still_requires_a_terminal_eof_marker() {
    let mut bytes = synthetic_object_stream_pdf(ObjectStreamPdfConfig::default());
    let eof = bytes
        .windows(b"%%EOF".len())
        .position(|window| window == b"%%EOF")
        .unwrap();
    bytes.truncate(eof);

    assert_eq!(
        validate_pdf_input_structure(&bytes),
        Err(PdfPreflightError::Invalid)
    );
}

fn synthetic_object_stream_pdf(config: ObjectStreamPdfConfig) -> Vec<u8> {
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
        assert_eq!(object_stream_contents.last(), Some(&b'>'));
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
    let table = synthetic_object_stream_xref_table(&offsets, config);
    let (encoded_table, decode_parms) = if config.png_predictor {
        (
            encode_png_up_rows(&table, 7),
            b"/DecodeParms << /Columns 7 /Predictor 12 >>".as_slice(),
        )
    } else {
        (table, b"".as_slice())
    };
    let stored_table = if config.corrupt_xref_content {
        b"not a zlib stream".to_vec()
    } else if config.xref_compressed {
        zlib(&encoded_table)
    } else {
        encoded_table
    };

    bytes.extend_from_slice(b"6 0 obj\n<< /Type /XRef /Size 7 /Root 1 0 R /W [1 4 2] /Index [0 7]");
    if config.xref_compressed {
        bytes.extend_from_slice(b" /Filter ");
        if config.unsupported_filter {
            bytes.extend_from_slice(b"/LZWDecode");
        } else {
            bytes.extend_from_slice(b"/FlateDecode");
        }
        bytes.extend_from_slice(decode_parms);
    }
    bytes.extend_from_slice(b" /Length ");
    write!(&mut bytes, "{}", stored_table.len()).unwrap();
    bytes.extend_from_slice(b" >>\nstream\n");
    bytes.extend_from_slice(&stored_table);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    write!(&mut bytes, "startxref\n{}\n%%EOF\n", offsets[6]).unwrap();
    bytes
}

fn synthetic_object_stream_xref_table(
    offsets: &[usize; 7],
    config: ObjectStreamPdfConfig,
) -> Vec<u8> {
    let mut table = Vec::new();
    let mut push = |entry_type: u8, field_two: u32, field_three: u16| {
        table.push(entry_type);
        table.extend_from_slice(&field_two.to_be_bytes());
        table.extend_from_slice(&field_three.to_be_bytes());
    };
    push(0, 0, 65535);
    let container = if config.corrupt_container { 99 } else { 5 };
    push(2, container, 0);
    push(2, 5, if config.corrupt_index { 0 } else { 1 });
    if config.omit_contained_object {
        push(0, 0, 0);
    } else {
        push(2, 5, 2);
    }
    push(
        1,
        if config.bad_normal_offset {
            u32::MAX
        } else {
            offsets[4] as u32
        },
        0,
    );
    push(1, offsets[5] as u32, 0);
    if config.omit_self_entry {
        push(0, 0, 0);
    } else if config.bad_self_offset {
        push(1, offsets[5] as u32, 0);
    } else {
        push(1, offsets[6] as u32, 0);
    }
    table
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
