//! Tests for `pdf_preflight`, kept out of the production file.
use std::io::Write;

use lopdf::{Document, Object, dictionary};

use super::{
    MAX_XREF_ENTRIES, PdfPreflightError, RawLexer, RawToken, reject_unsafe_object_dictionary,
    reset_header_parse_offsets, take_header_parse_offsets, validate_pdf_structure_with_limit,
};

fn validate_pdf_structure(bytes: &[u8]) -> Result<(), PdfPreflightError> {
    validate_pdf_structure_with_limit(bytes, MAX_XREF_ENTRIES)
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
