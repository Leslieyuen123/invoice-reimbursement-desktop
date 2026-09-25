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
mod tests;
