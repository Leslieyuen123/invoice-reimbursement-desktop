#[cfg(test)]
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use thiserror::Error;

use crate::domain::error::AppError;

const MAX_XREF_ENTRIES: usize = 100_000;
const MAX_XREF_REVISIONS: usize = 64;
const MAX_XREF_STREAM_DICTIONARY_BYTES: usize = 64 * 1024;
const MAX_OBJECT_NESTING: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum PdfPreflightError {
    #[error("归一化 PDF 结构无效")]
    Invalid,
    #[error("归一化 PDF 包含不受支持的压缩对象结构")]
    Unsupported,
    #[error("PDF 对象数超过导出限制")]
    ResourceLimit,
}

impl PdfPreflightError {
    pub(crate) fn into_validation_error(self) -> AppError {
        AppError::validation("normalizedPdf", self.to_string())
    }
}

#[derive(Debug, Clone, Copy)]
struct ActiveXrefEntry {
    object_number: usize,
    generation: usize,
    offset: usize,
}

#[derive(Debug, Clone, Copy)]
struct RevisionXrefEntry {
    object_number: usize,
    is_active: bool,
}

struct XrefCollection {
    visited_xrefs: HashSet<usize>,
    active_entries: Vec<ActiveXrefEntry>,
    total_entries: usize,
    active_entries_total: usize,
    max_active_entries: usize,
    seen_object_ids: HashSet<usize>,
}

impl XrefCollection {
    fn new(max_active_entries: usize) -> Self {
        Self {
            visited_xrefs: HashSet::new(),
            active_entries: Vec::new(),
            total_entries: 0,
            active_entries_total: 0,
            max_active_entries,
            seen_object_ids: HashSet::new(),
        }
    }
}

#[cfg(test)]
thread_local! {
    static HEADER_PARSE_OFFSETS: RefCell<Option<Vec<usize>>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn reset_header_parse_offsets() {
    HEADER_PARSE_OFFSETS.with(|offsets| *offsets.borrow_mut() = Some(Vec::new()));
}

#[cfg(test)]
fn take_header_parse_offsets() -> Vec<usize> {
    HEADER_PARSE_OFFSETS.with(|offsets| offsets.borrow_mut().take().unwrap_or_default())
}

pub(crate) fn validate_pdf_structure_with_limit(
    bytes: &[u8],
    max_active_entries: usize,
) -> Result<(), PdfPreflightError> {
    let xref_start = terminal_startxref(bytes)?;
    let mut collection = XrefCollection::new(max_active_entries);
    collect_xref_offsets(bytes, xref_start, &mut collection)?;

    let mut header_cache = HashMap::new();
    for entry in &collection.active_entries {
        let (object_number, generation) = match header_cache.get(&entry.offset).copied() {
            Some(header) => header,
            None => {
                if header_cache.len() >= MAX_XREF_ENTRIES {
                    return Err(resource_limit());
                }
                let header = parse_indirect_object_header(bytes, entry.offset)?;
                header_cache.insert(entry.offset, header);
                header
            }
        };
        if object_number != entry.object_number || generation != entry.generation {
            return Err(invalid_pdf_structure());
        }
    }

    let mut object_offsets = collection
        .active_entries
        .into_iter()
        .map(|entry| entry.offset)
        .collect::<Vec<_>>();
    object_offsets.retain(|offset| !collection.visited_xrefs.contains(offset));
    object_offsets.sort_unstable();
    object_offsets.dedup();

    for (index, offset) in object_offsets.iter().copied().enumerate() {
        let end = object_offsets
            .get(index + 1)
            .copied()
            .unwrap_or(bytes.len());
        if offset >= end || end > bytes.len() {
            return Err(invalid_pdf_structure());
        }
        reject_unsafe_object_dictionary(&bytes[offset..end])?;
    }
    Ok(())
}

fn terminal_startxref(bytes: &[u8]) -> Result<usize, PdfPreflightError> {
    let terminal_end = bytes
        .iter()
        .rposition(|byte| !is_whitespace(*byte))
        .and_then(|position| position.checked_add(1))
        .ok_or_else(invalid_pdf_structure)?;
    let eof_start = terminal_end
        .checked_sub(b"%%EOF".len())
        .filter(|start| bytes.get(*start..terminal_end) == Some(b"%%EOF"))
        .ok_or_else(invalid_pdf_structure)?;
    if eof_start <= 25 || eof_start < bytes.len().saturating_sub(512) {
        return Err(invalid_pdf_structure());
    }

    let marker = b"startxref";
    let search_start = eof_start - 25;
    let mut positions = bytes[search_start..eof_start]
        .windows(marker.len())
        .enumerate()
        .filter_map(|(position, window)| (window == marker).then_some(search_start + position));
    let position = positions.next().ok_or_else(invalid_pdf_structure)?;
    if positions.next().is_some() || !token_boundary_before(bytes, position) {
        return Err(invalid_pdf_structure());
    }

    let mut cursor = position + marker.len();
    consume_eol(bytes, &mut cursor)?;
    while bytes.get(cursor) == Some(&b' ') {
        cursor += 1;
    }
    let integer_start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if cursor == integer_start {
        return Err(invalid_pdf_structure());
    }
    let xref_start = parse_usize(&bytes[integer_start..cursor])?;
    while bytes.get(cursor) == Some(&b' ') {
        cursor += 1;
    }
    consume_eol(bytes, &mut cursor)?;
    if cursor != eof_start {
        return Err(invalid_pdf_structure());
    }
    Ok(xref_start)
}

fn consume_eol(bytes: &[u8], cursor: &mut usize) -> Result<(), PdfPreflightError> {
    match bytes.get(*cursor..) {
        Some(rest) if rest.starts_with(b"\r\n") => *cursor += 2,
        Some(rest) if rest.starts_with(b"\n") || rest.starts_with(b"\r") => *cursor += 1,
        _ => return Err(invalid_pdf_structure()),
    }
    Ok(())
}

fn collect_xref_offsets(
    bytes: &[u8],
    xref_start: usize,
    collection: &mut XrefCollection,
) -> Result<(), PdfPreflightError> {
    if xref_start >= bytes.len()
        || collection.visited_xrefs.len() >= MAX_XREF_REVISIONS
        || !collection.visited_xrefs.insert(xref_start)
    {
        return Err(invalid_pdf_structure());
    }
    let mut lexer = RawLexer::at(bytes, xref_start);
    let first = lexer.next_token()?;
    let previous_xref = match first {
        RawToken::Word(b"xref") => {
            collect_classic_xref_section(&mut lexer, xref_start, collection)?
        }
        RawToken::Word(_) => collect_uncompressed_xref_stream(bytes, xref_start, collection)?,
        _ => return Err(invalid_pdf_structure()),
    };

    if let Some(previous_xref) = previous_xref {
        collect_xref_offsets(bytes, previous_xref, collection)?;
    }
    Ok(())
}

fn collect_classic_xref_section(
    lexer: &mut RawLexer<'_>,
    xref_start: usize,
    collection: &mut XrefCollection,
) -> Result<Option<usize>, PdfPreflightError> {
    let mut revision_entries = Vec::new();
    let previous_xref = loop {
        let token = lexer.next_token()?;
        match token {
            RawToken::Word(b"trailer") => {
                break parse_classic_trailer(lexer, xref_start)?;
            }
            RawToken::Word(first) => {
                let first_object = parse_usize(first)?;
                let count = parse_usize_word(lexer.next_token()?)?;
                record_xref_entries(&mut collection.total_entries, count)?;
                for index in 0..count {
                    let object_number = first_object
                        .checked_add(index)
                        .ok_or_else(invalid_pdf_structure)?;
                    let offset = parse_usize_word(lexer.next_token()?)?;
                    let generation = parse_usize_word(lexer.next_token()?)?;
                    if generation > u16::MAX as usize {
                        return Err(invalid_pdf_structure());
                    }
                    match lexer.next_token()? {
                        RawToken::Word(b"n") => {
                            collection.active_entries.push(ActiveXrefEntry {
                                object_number,
                                generation,
                                offset,
                            });
                            revision_entries.push(RevisionXrefEntry {
                                object_number,
                                is_active: true,
                            });
                        }
                        RawToken::Word(b"f") => revision_entries.push(RevisionXrefEntry {
                            object_number,
                            is_active: false,
                        }),
                        _ => return Err(invalid_pdf_structure()),
                    }
                }
            }
            _ => return Err(invalid_pdf_structure()),
        }
    };
    record_effective_active_entries(
        revision_entries,
        &mut collection.seen_object_ids,
        &mut collection.active_entries_total,
        collection.max_active_entries,
    )?;
    Ok(previous_xref)
}

#[derive(Default)]
struct XrefStreamDictionary {
    length: Option<usize>,
    size: Option<usize>,
    widths: Option<[usize; 3]>,
    index: Option<Vec<usize>>,
    previous_xref: Option<usize>,
    is_xref: bool,
}

fn collect_uncompressed_xref_stream(
    bytes: &[u8],
    xref_start: usize,
    collection: &mut XrefCollection,
) -> Result<Option<usize>, PdfPreflightError> {
    let prefix_end = xref_start
        .checked_add(MAX_XREF_STREAM_DICTIONARY_BYTES)
        .map(|end| end.min(bytes.len()))
        .ok_or_else(invalid_pdf_structure)?;
    let mut lexer = RawLexer::at(&bytes[..prefix_end], xref_start);
    let object_number = parse_structural_usize(lexer.next_structural_token()?)?;
    let generation = parse_structural_usize(lexer.next_structural_token()?)?;
    if generation > u16::MAX as usize {
        return Err(invalid_pdf_structure());
    }
    expect_structural_word(&mut lexer, b"obj")?;
    match lexer.next_structural_token()? {
        StructuralToken::DictionaryStart => {}
        _ => return Err(invalid_pdf_structure()),
    }
    let dictionary = parse_xref_stream_dictionary(&mut lexer)?;
    expect_structural_word(&mut lexer, b"stream")?;

    let length = dictionary.length.ok_or_else(invalid_pdf_structure)?;
    let size = dictionary.size.ok_or_else(invalid_pdf_structure)?;
    let widths = dictionary.widths.ok_or_else(invalid_pdf_structure)?;
    if !dictionary.is_xref || size == 0 || size > MAX_XREF_ENTRIES || object_number >= size {
        return Err(invalid_pdf_structure());
    }
    let index = dictionary.index.unwrap_or_else(|| vec![0, size]);
    let entry_count = validate_xref_index(&index, size)?;
    record_xref_entries(&mut collection.total_entries, entry_count)?;

    let entry_width = widths
        .into_iter()
        .try_fold(0_usize, |total, width| total.checked_add(width))
        .ok_or_else(invalid_pdf_structure)?;
    if entry_width == 0 || widths.into_iter().any(|width| width > 8) {
        return Err(invalid_pdf_structure());
    }
    let expected_length = entry_count
        .checked_mul(entry_width)
        .ok_or_else(invalid_pdf_structure)?;
    if length != expected_length {
        return Err(invalid_pdf_structure());
    }
    let content_start = stream_content_start(bytes, lexer.position)?;
    let content_end = content_start
        .checked_add(length)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(invalid_pdf_structure)?;
    validate_endstream(bytes, content_end)?;

    let mut cursor = content_start;
    let mut found_self = false;
    let mut revision_entries = Vec::new();
    for range in index.as_chunks::<2>().0 {
        let first_object = range[0];
        let count = range[1];
        for object_id in first_object..first_object + count {
            let entry_end = cursor
                .checked_add(entry_width)
                .filter(|end| *end <= content_end)
                .ok_or_else(invalid_pdf_structure)?;
            let entry = &bytes[cursor..entry_end];
            cursor = entry_end;
            let type_end = widths[0];
            let offset_end = type_end + widths[1];
            let entry_type = if widths[0] == 0 {
                1
            } else {
                read_big_endian(&entry[..type_end])?
            };
            let offset = read_big_endian(&entry[type_end..offset_end])?;
            let generation = read_big_endian(&entry[offset_end..])?;
            match entry_type {
                0 => revision_entries.push(RevisionXrefEntry {
                    object_number: object_id,
                    is_active: false,
                }),
                1 => {
                    let offset = usize::try_from(offset).map_err(|_| invalid_pdf_structure())?;
                    let generation = usize::try_from(generation)
                        .ok()
                        .filter(|generation| *generation <= u16::MAX as usize)
                        .ok_or_else(invalid_pdf_structure)?;
                    if offset >= bytes.len() {
                        return Err(invalid_pdf_structure());
                    }
                    collection.active_entries.push(ActiveXrefEntry {
                        object_number: object_id,
                        generation,
                        offset,
                    });
                    revision_entries.push(RevisionXrefEntry {
                        object_number: object_id,
                        is_active: true,
                    });
                    if object_id == object_number {
                        found_self = offset == xref_start;
                    }
                }
                2 => return Err(unsupported_pdf_structure()),
                _ => return Err(invalid_pdf_structure()),
            }
        }
    }
    if cursor != content_end || !found_self {
        return Err(invalid_pdf_structure());
    }
    record_effective_active_entries(
        revision_entries,
        &mut collection.seen_object_ids,
        &mut collection.active_entries_total,
        collection.max_active_entries,
    )?;
    Ok(dictionary.previous_xref)
}

fn parse_xref_stream_dictionary(
    lexer: &mut RawLexer<'_>,
) -> Result<XrefStreamDictionary, PdfPreflightError> {
    let mut dictionary = XrefStreamDictionary::default();
    loop {
        let key = match lexer.next_structural_token()? {
            StructuralToken::DictionaryEnd => return Ok(dictionary),
            StructuralToken::Name(key) => key,
            _ => return Err(invalid_pdf_structure()),
        };
        match key.as_slice() {
            b"Type" => {
                if dictionary.is_xref {
                    return Err(invalid_pdf_structure());
                }
                dictionary.is_xref = matches!(
                    lexer.next_structural_token()?,
                    StructuralToken::Name(name) if name == b"XRef"
                );
                if !dictionary.is_xref {
                    return Err(invalid_pdf_structure());
                }
            }
            b"Length" => {
                dictionary.length = Some(parse_unique_structural_usize(lexer, dictionary.length)?);
            }
            b"Size" => {
                dictionary.size = Some(parse_unique_structural_usize(lexer, dictionary.size)?);
            }
            b"Prev" => {
                dictionary.previous_xref = Some(parse_unique_structural_usize(
                    lexer,
                    dictionary.previous_xref,
                )?);
            }
            b"W" => {
                if dictionary.widths.is_some() {
                    return Err(invalid_pdf_structure());
                }
                let values = parse_usize_array(lexer)?;
                dictionary.widths = Some(values.try_into().map_err(|_| invalid_pdf_structure())?);
            }
            b"Index" => {
                if dictionary.index.is_some() {
                    return Err(invalid_pdf_structure());
                }
                dictionary.index = Some(parse_usize_array(lexer)?);
            }
            b"Filter" | b"DecodeParms" | b"XRefStm" | b"ObjStm" => {
                return Err(unsupported_pdf_structure());
            }
            _ => consume_raw_value(lexer, 0)?,
        }
    }
}

fn parse_unique_structural_usize(
    lexer: &mut RawLexer<'_>,
    existing: Option<usize>,
) -> Result<usize, PdfPreflightError> {
    if existing.is_some() {
        return Err(invalid_pdf_structure());
    }
    parse_structural_usize(lexer.next_structural_token()?)
}

fn parse_usize_array(lexer: &mut RawLexer<'_>) -> Result<Vec<usize>, PdfPreflightError> {
    match lexer.next_structural_token()? {
        StructuralToken::ArrayStart => {}
        _ => return Err(invalid_pdf_structure()),
    }
    let mut values = Vec::new();
    loop {
        match lexer.next_structural_token()? {
            StructuralToken::ArrayEnd => return Ok(values),
            StructuralToken::Word(word) => values.push(parse_usize(word)?),
            _ => return Err(invalid_pdf_structure()),
        }
    }
}

fn validate_xref_index(index: &[usize], size: usize) -> Result<usize, PdfPreflightError> {
    if index.is_empty() || !index.len().is_multiple_of(2) {
        return Err(invalid_pdf_structure());
    }
    index
        .as_chunks::<2>()
        .0
        .iter()
        .try_fold(0_usize, |total, range| {
            range[0]
                .checked_add(range[1])
                .filter(|end| *end <= size)
                .ok_or_else(invalid_pdf_structure)?;
            total
                .checked_add(range[1])
                .filter(|count| *count <= MAX_XREF_ENTRIES)
                .ok_or_else(resource_limit)
        })
}

fn record_xref_entries(total: &mut usize, count: usize) -> Result<(), PdfPreflightError> {
    *total = total
        .checked_add(count)
        .filter(|total| *total <= MAX_XREF_ENTRIES)
        .ok_or_else(resource_limit)?;
    Ok(())
}

fn record_effective_active_entries(
    revision_entries: Vec<RevisionXrefEntry>,
    seen_object_ids: &mut HashSet<usize>,
    active_total: &mut usize,
    active_limit: usize,
) -> Result<(), PdfPreflightError> {
    // Match lopdf 0.36's loader: its xref parsers omit free entries, then its reader merges
    // normal entries newest-first without replacing an object ID that is already present.
    for entry in revision_entries.into_iter().rev() {
        if entry.is_active && seen_object_ids.insert(entry.object_number) {
            *active_total = active_total
                .checked_add(1)
                .filter(|total| *total <= active_limit)
                .ok_or_else(resource_limit)?;
        }
    }
    Ok(())
}

fn parse_indirect_object_header(
    bytes: &[u8],
    offset: usize,
) -> Result<(usize, usize), PdfPreflightError> {
    #[cfg(test)]
    HEADER_PARSE_OFFSETS.with(|offsets| {
        if let Some(offsets) = offsets.borrow_mut().as_mut() {
            offsets.push(offset);
        }
    });
    if offset >= bytes.len() {
        return Err(invalid_pdf_structure());
    }
    let mut lexer = RawLexer::at(bytes, offset);
    let object_number = parse_structural_usize(lexer.next_structural_token()?)?;
    let generation = parse_structural_usize(lexer.next_structural_token()?)?;
    expect_structural_word(&mut lexer, b"obj")?;
    Ok((object_number, generation))
}

fn stream_content_start(bytes: &[u8], position: usize) -> Result<usize, PdfPreflightError> {
    match bytes.get(position..) {
        Some(rest) if rest.starts_with(b"\r\n") => Ok(position + 2),
        Some(rest) if rest.starts_with(b"\n") || rest.starts_with(b"\r") => Ok(position + 1),
        _ => Err(invalid_pdf_structure()),
    }
}

fn validate_endstream(bytes: &[u8], content_end: usize) -> Result<(), PdfPreflightError> {
    let mut marker = content_end;
    if bytes
        .get(marker..)
        .is_some_and(|rest| rest.starts_with(b"\r\n"))
    {
        marker += 2;
    } else if bytes
        .get(marker)
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        marker += 1;
    }
    let end = marker
        .checked_add(b"endstream".len())
        .ok_or_else(invalid_pdf_structure)?;
    if bytes.get(marker..end) != Some(b"endstream") || !token_boundary_after(bytes, end) {
        return Err(invalid_pdf_structure());
    }
    Ok(())
}

fn read_big_endian(bytes: &[u8]) -> Result<u64, PdfPreflightError> {
    if bytes.len() > 8 {
        return Err(invalid_pdf_structure());
    }
    Ok(bytes
        .iter()
        .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte)))
}

fn consume_raw_value(lexer: &mut RawLexer<'_>, depth: usize) -> Result<(), PdfPreflightError> {
    consume_raw_value_rejecting(lexer, depth, None)
}

fn consume_raw_value_rejecting(
    lexer: &mut RawLexer<'_>,
    depth: usize,
    forbidden_nested_name: Option<&[u8]>,
) -> Result<(), PdfPreflightError> {
    if depth >= MAX_OBJECT_NESTING {
        return Err(resource_limit());
    }
    match lexer.next_structural_token()? {
        StructuralToken::ArrayStart => loop {
            let checkpoint = lexer.position;
            if matches!(lexer.next_structural_token()?, StructuralToken::ArrayEnd) {
                return Ok(());
            }
            lexer.position = checkpoint;
            consume_raw_value_rejecting(lexer, depth + 1, forbidden_nested_name)?;
        },
        StructuralToken::DictionaryStart => loop {
            match lexer.next_structural_token()? {
                StructuralToken::DictionaryEnd => return Ok(()),
                StructuralToken::Name(name) => {
                    if forbidden_nested_name.is_some_and(|forbidden| name == forbidden) {
                        return Err(invalid_pdf_structure());
                    }
                    consume_raw_value_rejecting(lexer, depth + 1, forbidden_nested_name)?;
                }
                _ => return Err(invalid_pdf_structure()),
            }
        },
        StructuralToken::Word(first) => {
            if first.iter().all(u8::is_ascii_digit) {
                let checkpoint = lexer.position;
                let is_reference = matches!(
                    (lexer.next_structural_token(), lexer.next_structural_token()),
                    (
                        Ok(StructuralToken::Word(generation)),
                        Ok(StructuralToken::Word(b"R"))
                    ) if generation.iter().all(u8::is_ascii_digit)
                );
                if !is_reference {
                    lexer.position = checkpoint;
                }
            }
            Ok(())
        }
        StructuralToken::Name(_) | StructuralToken::Scalar => Ok(()),
        StructuralToken::DictionaryEnd | StructuralToken::ArrayEnd => Err(invalid_pdf_structure()),
    }
}

fn parse_classic_trailer(
    lexer: &mut RawLexer<'_>,
    xref_start: usize,
) -> Result<Option<usize>, PdfPreflightError> {
    match lexer.next_structural_token()? {
        StructuralToken::DictionaryStart => {}
        _ => return Err(invalid_pdf_structure()),
    }
    let mut previous_xref = None;
    loop {
        let key = match lexer.next_structural_token()? {
            StructuralToken::DictionaryEnd => break,
            StructuralToken::Name(name) => name,
            _ => return Err(invalid_pdf_structure()),
        };
        match key.as_slice() {
            b"Prev" => {
                if previous_xref.is_some() {
                    return Err(invalid_pdf_structure());
                }
                previous_xref = Some(parse_structural_usize(lexer.next_structural_token()?)?);
            }
            b"XRefStm" | b"ObjStm" | b"XRef" => {
                return Err(unsupported_pdf_structure());
            }
            _ => consume_raw_value_rejecting(lexer, 0, Some(b"Prev"))?,
        }
    }

    expect_structural_word(lexer, b"startxref")?;
    let declared_xref = parse_structural_usize(lexer.next_structural_token()?)?;
    if declared_xref != xref_start {
        return Err(invalid_pdf_structure());
    }
    Ok(previous_xref)
}

fn reject_unsafe_object_dictionary(bytes: &[u8]) -> Result<(), PdfPreflightError> {
    let mut lexer = RawLexer::new(bytes);
    let mut previous_name_was_type = false;
    loop {
        match lexer.next_token() {
            Ok(RawToken::Word(b"stream" | b"endobj")) => return Ok(()),
            Ok(RawToken::Name(name)) => {
                if name == b"ObjStm" || name == b"XRefStm" {
                    return Err(unsupported_pdf_structure());
                }
                if previous_name_was_type && name == b"XRef" {
                    return Err(unsupported_pdf_structure());
                }
                previous_name_was_type = name == b"Type";
            }
            Ok(_) => previous_name_was_type = false,
            Err(_) => return Err(invalid_pdf_structure()),
        }
    }
}

fn parse_usize_word(token: RawToken<'_>) -> Result<usize, PdfPreflightError> {
    match token {
        RawToken::Word(word) => parse_usize(word),
        _ => Err(invalid_pdf_structure()),
    }
}

fn parse_structural_usize(token: StructuralToken<'_>) -> Result<usize, PdfPreflightError> {
    match token {
        StructuralToken::Word(word) => parse_usize(word),
        _ => Err(invalid_pdf_structure()),
    }
}

fn expect_structural_word(
    lexer: &mut RawLexer<'_>,
    expected: &[u8],
) -> Result<(), PdfPreflightError> {
    match lexer.next_structural_token()? {
        StructuralToken::Word(actual) if actual == expected => Ok(()),
        _ => Err(invalid_pdf_structure()),
    }
}

fn parse_usize(bytes: &[u8]) -> Result<usize, PdfPreflightError> {
    let value = std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(invalid_pdf_structure)?;
    Ok(value)
}

#[derive(Debug)]
enum RawToken<'a> {
    Word(&'a [u8]),
    Name(Vec<u8>),
}

#[derive(Debug)]
enum StructuralToken<'a> {
    Word(&'a [u8]),
    Name(Vec<u8>),
    DictionaryStart,
    DictionaryEnd,
    ArrayStart,
    ArrayEnd,
    Scalar,
}

struct RawLexer<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> RawLexer<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn at(bytes: &'a [u8], position: usize) -> Self {
        Self { bytes, position }
    }

    fn next_token(&mut self) -> Result<RawToken<'a>, PdfPreflightError> {
        loop {
            self.skip_whitespace_and_comments();
            let Some(&byte) = self.bytes.get(self.position) else {
                return Err(invalid_pdf_structure());
            };
            match byte {
                b'/' => return self.read_name(),
                b'(' => self.skip_literal_string()?,
                b'<' if self.bytes.get(self.position + 1) == Some(&b'<') => self.position += 2,
                b'>' if self.bytes.get(self.position + 1) == Some(&b'>') => self.position += 2,
                b'<' => self.skip_hex_string()?,
                byte if is_delimiter(byte) => self.position += 1,
                _ => return self.read_word(),
            }
        }
    }

    fn next_structural_token(&mut self) -> Result<StructuralToken<'a>, PdfPreflightError> {
        self.skip_whitespace_and_comments();
        let Some(&byte) = self.bytes.get(self.position) else {
            return Err(invalid_pdf_structure());
        };
        match byte {
            b'/' => match self.read_name()? {
                RawToken::Name(name) => Ok(StructuralToken::Name(name)),
                RawToken::Word(_) => unreachable!(),
            },
            b'(' => {
                self.skip_literal_string()?;
                Ok(StructuralToken::Scalar)
            }
            b'<' if self.bytes.get(self.position + 1) == Some(&b'<') => {
                self.position += 2;
                Ok(StructuralToken::DictionaryStart)
            }
            b'>' if self.bytes.get(self.position + 1) == Some(&b'>') => {
                self.position += 2;
                Ok(StructuralToken::DictionaryEnd)
            }
            b'<' => {
                self.skip_hex_string()?;
                Ok(StructuralToken::Scalar)
            }
            b'[' => {
                self.position += 1;
                Ok(StructuralToken::ArrayStart)
            }
            b']' => {
                self.position += 1;
                Ok(StructuralToken::ArrayEnd)
            }
            byte if is_delimiter(byte) => Err(invalid_pdf_structure()),
            _ => match self.read_word()? {
                RawToken::Word(word) => Ok(StructuralToken::Word(word)),
                RawToken::Name(_) => unreachable!(),
            },
        }
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            while self
                .bytes
                .get(self.position)
                .is_some_and(|byte| is_whitespace(*byte))
            {
                self.position += 1;
            }
            if self.bytes.get(self.position) != Some(&b'%') {
                return;
            }
            while self
                .bytes
                .get(self.position)
                .is_some_and(|byte| *byte != b'\r' && *byte != b'\n')
            {
                self.position += 1;
            }
        }
    }

    fn read_word(&mut self) -> Result<RawToken<'a>, PdfPreflightError> {
        let start = self.position;
        while self
            .bytes
            .get(self.position)
            .is_some_and(|byte| !is_whitespace(*byte) && !is_delimiter(*byte))
        {
            self.position += 1;
        }
        if self.position == start {
            return Err(invalid_pdf_structure());
        }
        Ok(RawToken::Word(&self.bytes[start..self.position]))
    }

    fn read_name(&mut self) -> Result<RawToken<'a>, PdfPreflightError> {
        self.position += 1;
        let mut decoded = Vec::new();
        while let Some(&byte) = self.bytes.get(self.position) {
            if is_whitespace(byte) || is_delimiter(byte) {
                break;
            }
            if byte == b'#' {
                let high = self
                    .bytes
                    .get(self.position + 1)
                    .and_then(|byte| hex_value(*byte))
                    .ok_or_else(invalid_pdf_structure)?;
                let low = self
                    .bytes
                    .get(self.position + 2)
                    .and_then(|byte| hex_value(*byte))
                    .ok_or_else(invalid_pdf_structure)?;
                decoded.push((high << 4) | low);
                self.position += 3;
            } else {
                decoded.push(byte);
                self.position += 1;
            }
        }
        Ok(RawToken::Name(decoded))
    }

    fn skip_literal_string(&mut self) -> Result<(), PdfPreflightError> {
        self.position += 1;
        let mut depth = 1_usize;
        while let Some(&byte) = self.bytes.get(self.position) {
            self.position += 1;
            match byte {
                b'\\' => {
                    if self.position < self.bytes.len() {
                        self.position += 1;
                    }
                }
                b'(' => depth = depth.saturating_add(1),
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        Err(invalid_pdf_structure())
    }

    fn skip_hex_string(&mut self) -> Result<(), PdfPreflightError> {
        self.position += 1;
        while let Some(&byte) = self.bytes.get(self.position) {
            self.position += 1;
            if byte == b'>' {
                return Ok(());
            }
        }
        Err(invalid_pdf_structure())
    }
}

fn token_boundary_before(bytes: &[u8], position: usize) -> bool {
    position == 0
        || bytes
            .get(position - 1)
            .is_some_and(|byte| is_whitespace(*byte) || is_delimiter(*byte))
}

fn token_boundary_after(bytes: &[u8], position: usize) -> bool {
    position == bytes.len()
        || bytes
            .get(position)
            .is_some_and(|byte| is_whitespace(*byte) || is_delimiter(*byte))
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, 0 | b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn invalid_pdf_structure() -> PdfPreflightError {
    PdfPreflightError::Invalid
}

fn unsupported_pdf_structure() -> PdfPreflightError {
    PdfPreflightError::Unsupported
}

fn resource_limit() -> PdfPreflightError {
    PdfPreflightError::ResourceLimit
}

#[cfg(test)]
mod tests {
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
        let object_dictionary =
            b"<< /Type /Obj#53tm /N 0 /First 0 /Length 0 >>\nstream\n\nendstream";
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

    fn write_xref_stream_entry(
        entries: &mut Vec<u8>,
        entry_type: u8,
        offset: usize,
        generation: u16,
    ) {
        entries.push(entry_type);
        entries.extend_from_slice(&(offset as u32).to_be_bytes());
        entries.extend_from_slice(&generation.to_be_bytes());
    }
}
