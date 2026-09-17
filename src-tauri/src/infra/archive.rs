//! Archive expansion that decides by content, not by file name.
//!
//! Real mailboxes contain password-protected ZIPs (Chinese invoice portals
//! encrypt the archive and mail the password separately), archives whose
//! extension lies, and archives that simply hold no invoice at all. Each case
//! has to end in a distinct, user-visible result instead of a silent omission.

use std::io::{Cursor, Read};

use crate::infra::file_signature::{self, FileKind};

pub const MAX_ARCHIVE_ENTRIES: usize = 64;
pub const MAX_ARCHIVE_ENTRY_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_ARCHIVE_TOTAL_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveOutcome {
    /// Invoice documents found inside the archive.
    Extracted(Vec<ArchiveEntry>),
    /// The archive opened but holds nothing that can be an invoice.
    NoInvoiceEntries {
        kind: FileKind,
        entries: usize,
        kinds: Vec<FileKind>,
    },
    /// Password protected, and none of the candidate passwords worked.
    Encrypted { kind: FileKind },
    /// A format this build cannot read.
    Unsupported { kind: FileKind },
    /// Archive limits were exceeded.
    Refused,
}

/// Shared budget so one mail cannot spend unbounded work on archives.
#[derive(Debug, Clone, Copy)]
pub struct ArchiveBudget {
    pub remaining_entries: usize,
    pub remaining_bytes: u64,
}

impl Default for ArchiveBudget {
    fn default() -> Self {
        Self {
            remaining_entries: MAX_ARCHIVE_ENTRIES,
            remaining_bytes: MAX_ARCHIVE_TOTAL_BYTES,
        }
    }
}

/// Extracts the invoice documents from an archive.
///
/// `passwords` are tried in order on encrypted entries; the caller derives them
/// from the mail body, so nothing is guessed.
pub fn extract_invoices(
    bytes: &[u8],
    budget: &mut ArchiveBudget,
    passwords: &[String],
) -> ArchiveOutcome {
    extract_invoices_at_depth(bytes, budget, passwords, 0)
}

/// One nested archive is unwrapped; deeper nesting is refused on purpose.
const MAX_ARCHIVE_DEPTH: usize = 1;

fn extract_invoices_at_depth(
    bytes: &[u8],
    budget: &mut ArchiveBudget,
    passwords: &[String],
    depth: usize,
) -> ArchiveOutcome {
    match file_signature::detect(bytes) {
        FileKind::Zip => extract_from_zip(bytes, budget, passwords, depth),
        FileKind::SevenZip => extract_from_seven_zip(bytes, budget, passwords, depth),
        // RAR has no pure-Rust decompressor here; it is reported as unsupported
        // so the mail stays visible instead of quietly disappearing.
        kind => ArchiveOutcome::Unsupported { kind },
    }
}

/// Validates the classic end-of-central-directory record before any parser runs.
///
/// Rejects ZIP64 metadata, ambiguous or duplicated EOCD records, multi-disk
/// archives and inconsistent central-directory sizes, which is what keeps a
/// hostile attachment from steering the decompressor.
pub(crate) fn preflight_zip_entries(bytes: &[u8]) -> Result<usize, ()> {
    const EOCD_BYTES: usize = 22;
    const MAX_COMMENT_BYTES: usize = u16::MAX as usize;

    let search_start = bytes
        .len()
        .saturating_sub(EOCD_BYTES.saturating_add(MAX_COMMENT_BYTES));
    let mut eocd_offset = None;
    for (offset, signature) in bytes.windows(4).enumerate() {
        if signature == b"PK\x06\x06" || signature == b"PK\x06\x07" {
            return Err(());
        }
        if signature == b"PK\x05\x06" && eocd_offset.replace(offset).is_some() {
            return Err(());
        }
    }
    let eocd_offset = eocd_offset
        .filter(|offset| *offset >= search_start)
        .ok_or(())?;
    let record = bytes
        .get(eocd_offset..eocd_offset.checked_add(EOCD_BYTES).ok_or(())?)
        .ok_or(())?;
    let comment_bytes = u16::from_le_bytes([record[20], record[21]]) as usize;
    if eocd_offset
        .checked_add(EOCD_BYTES)
        .and_then(|end| end.checked_add(comment_bytes))
        != Some(bytes.len())
    {
        return Err(());
    }
    let disk = u16::from_le_bytes([record[4], record[5]]);
    let central_directory_disk = u16::from_le_bytes([record[6], record[7]]);
    let entries_on_disk = u16::from_le_bytes([record[8], record[9]]);
    let total_entries = u16::from_le_bytes([record[10], record[11]]);
    let central_directory_bytes =
        u32::from_le_bytes([record[12], record[13], record[14], record[15]]);
    let central_directory_offset =
        u32::from_le_bytes([record[16], record[17], record[18], record[19]]);
    if disk != 0
        || central_directory_disk != 0
        || entries_on_disk != total_entries
        || entries_on_disk == u16::MAX
        || total_entries == u16::MAX
        || central_directory_bytes == u32::MAX
        || central_directory_offset == u32::MAX
    {
        return Err(());
    }
    let central_directory_end = usize::try_from(central_directory_offset)
        .map_err(|_| ())?
        .checked_add(usize::try_from(central_directory_bytes).map_err(|_| ())?)
        .ok_or(())?;
    if central_directory_end > eocd_offset {
        return Err(());
    }
    Ok(usize::from(total_entries))
}

fn extract_from_zip(
    bytes: &[u8],
    budget: &mut ArchiveBudget,
    passwords: &[String],
    depth: usize,
) -> ArchiveOutcome {
    let Ok(declared_entries) = preflight_zip_entries(bytes) else {
        return ArchiveOutcome::Unsupported {
            kind: FileKind::Zip,
        };
    };
    if declared_entries > MAX_ARCHIVE_ENTRIES || budget.remaining_entries < declared_entries {
        // A refused archive still spends its declared entries, so a hostile or
        // broken attachment cannot be retried for free.
        budget.remaining_entries = 0;
        return ArchiveOutcome::Refused;
    }
    // Entries are reserved before any parser sees the archive.
    budget.remaining_entries = budget.remaining_entries.saturating_sub(declared_entries);
    let mut archive = match zip::ZipArchive::new(Cursor::new(bytes)) {
        Ok(archive) => archive,
        Err(_) => {
            return ArchiveOutcome::Unsupported {
                kind: FileKind::Zip,
            };
        }
    };
    let entry_count = archive.len();
    if entry_count != declared_entries {
        return ArchiveOutcome::Unsupported {
            kind: FileKind::Zip,
        };
    }

    let mut entries = Vec::new();
    let mut kinds = Vec::new();
    let mut encrypted_seen = false;
    for index in 0..entry_count {
        let Ok(raw) = archive.by_index_raw(index) else {
            continue;
        };
        let encrypted = raw.encrypted();
        let size = raw.size();
        let name = raw
            .enclosed_name()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| format!("entry-{index}"));
        let is_directory = name.ends_with('/') || raw.is_dir();
        drop(raw);
        if is_directory || size > MAX_ARCHIVE_ENTRY_BYTES {
            continue;
        }
        if encrypted {
            encrypted_seen = true;
        }
        let Some(contents) = read_zip_entry(&mut archive, index, encrypted, passwords) else {
            continue;
        };
        if contents.is_empty() {
            continue;
        }
        if contents.len() as u64 > budget.remaining_bytes {
            return ArchiveOutcome::Refused;
        }
        budget.remaining_bytes = budget.remaining_bytes.saturating_sub(contents.len() as u64);
        let kind = file_signature::detect(&contents);
        if kind.is_invoice_document() {
            entries.push(ArchiveEntry {
                name,
                bytes: contents,
            });
            continue;
        }
        if kind.is_archive() && depth < MAX_ARCHIVE_DEPTH {
            match extract_invoices_at_depth(&contents, budget, passwords, depth + 1) {
                ArchiveOutcome::Extracted(nested) => entries.extend(nested),
                ArchiveOutcome::Encrypted { .. } => encrypted_seen = true,
                ArchiveOutcome::Refused => return ArchiveOutcome::Refused,
                _ => {}
            }
            continue;
        }
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }

    if !entries.is_empty() {
        return ArchiveOutcome::Extracted(entries);
    }
    if encrypted_seen {
        return ArchiveOutcome::Encrypted {
            kind: FileKind::Zip,
        };
    }
    ArchiveOutcome::NoInvoiceEntries {
        kind: FileKind::Zip,
        entries: entry_count,
        kinds,
    }
}

/// Extracts invoices from a 7z archive, trying the mail's passwords in order.
fn extract_from_seven_zip(
    bytes: &[u8],
    budget: &mut ArchiveBudget,
    passwords: &[String],
    depth: usize,
) -> ArchiveOutcome {
    use sevenz_rust2::{ArchiveReader, Password};

    let mut candidates: Vec<(Password, bool)> = vec![(Password::empty(), false)];
    candidates.extend(
        passwords
            .iter()
            .map(|password| (Password::from(password.as_str()), true)),
    );
    let mut encrypted_only = false;
    for (password, is_password) in candidates {
        let Ok(mut reader) = ArchiveReader::new(Cursor::new(bytes), password) else {
            continue;
        };
        let mut entries = Vec::new();
        let mut kinds = Vec::new();
        let mut refused = false;
        let result = reader.for_each_entries(|entry, contents| {
            if entry.is_directory() {
                return Ok(true);
            }
            let size = entry.size();
            if size > MAX_ARCHIVE_ENTRY_BYTES || size > budget.remaining_bytes {
                refused = true;
                return Ok(false);
            }
            let mut buffer = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
            contents.read_to_end(&mut buffer)?;
            budget.remaining_bytes = budget.remaining_bytes.saturating_sub(buffer.len() as u64);
            let kind = file_signature::detect(&buffer);
            if kind.is_invoice_document() {
                entries.push(ArchiveEntry {
                    name: entry.name().to_owned(),
                    bytes: buffer,
                });
                return Ok(true);
            }
            if kind.is_archive() && depth < MAX_ARCHIVE_DEPTH {
                match extract_invoices_at_depth(&buffer, budget, passwords, depth + 1) {
                    ArchiveOutcome::Extracted(nested) => entries.extend(nested),
                    ArchiveOutcome::Refused => {
                        refused = true;
                        return Ok(false);
                    }
                    _ => {}
                }
                return Ok(true);
            }
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
            Ok(true)
        });
        if refused {
            return ArchiveOutcome::Refused;
        }
        if !entries.is_empty() {
            return ArchiveOutcome::Extracted(entries);
        }
        match result {
            Ok(()) => {
                return ArchiveOutcome::NoInvoiceEntries {
                    kind: FileKind::SevenZip,
                    entries: kinds.len(),
                    kinds,
                };
            }
            Err(_) if is_password => {
                // A wrong password and a damaged archive look the same here; the
                // next candidate gets its chance.
                encrypted_only = true;
                continue;
            }
            Err(_) => {
                encrypted_only = true;
                continue;
            }
        }
    }
    if encrypted_only {
        ArchiveOutcome::Encrypted {
            kind: FileKind::SevenZip,
        }
    } else {
        ArchiveOutcome::Unsupported {
            kind: FileKind::SevenZip,
        }
    }
}

/// Reads one ZIP entry, trying each candidate password for encrypted entries.
fn read_zip_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    index: usize,
    encrypted: bool,
    passwords: &[String],
) -> Option<Vec<u8>> {
    let mut candidates: Vec<Option<&[u8]>> = if encrypted {
        passwords
            .iter()
            .map(|password| Some(password.as_bytes()))
            .collect()
    } else {
        vec![None]
    };
    if candidates.is_empty() {
        // Encrypted, and the mail never mentioned a password.
        return None;
    }
    while let Some(password) = candidates.pop() {
        let attempt = match password {
            Some(password) => archive.by_index_decrypt(index, password),
            None => archive.by_index(index),
        };
        let Ok(mut entry) = attempt else {
            continue;
        };
        let mut contents = Vec::new();
        if entry.read_to_end(&mut contents).is_err() {
            continue;
        }
        if contents.len() as u64 != entry.size() {
            continue;
        }
        // A wrong password can still pass the header check, so the content has
        // to look like something we understand before it is trusted.
        if encrypted && file_signature::detect(&contents) == FileKind::Unknown {
            continue;
        }
        return Some(contents);
    }
    None
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use super::{ArchiveBudget, ArchiveOutcome, extract_invoices};
    use crate::infra::file_signature;

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut buffer));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            for (name, contents) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(contents).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer
    }

    const PDF: &[u8] = b"%PDF-1.7\n%%EOF\n";

    #[test]
    fn extracts_invoice_documents_from_a_zip() {
        let archive = zip_with(&[
            ("invoice.pdf", PDF),
            ("readme.txt", b"hello"),
            (
                "scan.png",
                &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
            ),
        ]);

        let mut budget = ArchiveBudget::default();
        let outcome = extract_invoices(&archive, &mut budget, &[]);

        let ArchiveOutcome::Extracted(entries) = outcome else {
            panic!("expected extracted entries, got {outcome:?}");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "invoice.pdf");
        assert_eq!(entries[0].bytes, PDF);
        assert_eq!(entries[1].name, "scan.png");
    }

    #[test]
    fn reports_an_archive_that_holds_no_invoice() {
        // The real mailbox case: a ZIP whose only entry is a Word document.
        let archive = zip_with(&[(
            "notes.doc",
            &[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1],
        )]);

        let mut budget = ArchiveBudget::default();
        let outcome = extract_invoices(&archive, &mut budget, &[]);

        let ArchiveOutcome::NoInvoiceEntries { entries, kinds, .. } = outcome else {
            panic!("expected a no-invoice result, got {outcome:?}");
        };
        assert_eq!(entries, 1);
        assert_eq!(kinds, vec![crate::infra::file_signature::FileKind::Ole]);
    }

    #[test]
    fn a_video_that_pretends_to_be_a_zip_is_unsupported() {
        let mut movie = vec![0, 0, 0, 0x18];
        movie.extend_from_slice(b"ftypqt  ");
        movie.extend_from_slice(&[0; 16]);
        assert!(!file_signature::detect(&movie).is_archive());

        let mut budget = ArchiveBudget::default();
        let outcome = extract_invoices(&movie, &mut budget, &[]);
        assert!(matches!(outcome, ArchiveOutcome::Unsupported { .. }));
    }

    #[test]
    fn expands_one_nested_archive_level() {
        let inner = zip_with(&[("invoice.pdf", PDF)]);
        let outer = zip_with(&[("bundle.zip", &inner)]);

        let mut budget = ArchiveBudget::default();
        let outcome = extract_invoices(&outer, &mut budget, &[]);

        let ArchiveOutcome::Extracted(entries) = outcome else {
            panic!("expected nested extraction, got {outcome:?}");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].bytes, PDF);
    }

    /// The real mailbox case: an invoice portal encrypts the ZIP and mails the
    /// password in the body.
    fn encrypted_zip_with(entry: &str, contents: &[u8], password: &str) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut buffer));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .with_aes_encryption(zip::AesMode::Aes256, password);
            writer.start_file(entry, options).unwrap();
            writer.write_all(contents).unwrap();
            writer.finish().unwrap();
        }
        buffer
    }

    #[test]
    fn an_encrypted_zip_is_reported_and_opens_with_the_mailed_password() {
        let archive = encrypted_zip_with("invoice.pdf", PDF, "9527");

        let mut blocked = ArchiveBudget::default();
        assert_eq!(
            extract_invoices(&archive, &mut blocked, &[]),
            ArchiveOutcome::Encrypted {
                kind: crate::infra::file_signature::FileKind::Zip
            }
        );

        let mut budget = ArchiveBudget::default();
        let outcome = extract_invoices(&archive, &mut budget, &["9527".to_owned()]);
        let ArchiveOutcome::Extracted(entries) = outcome else {
            panic!("the mailed password should open the archive, got {outcome:?}");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].bytes, PDF);
    }

    #[test]
    fn a_wrong_password_does_not_yield_garbage_entries() {
        let archive = encrypted_zip_with("invoice.pdf", PDF, "right-password");

        let mut budget = ArchiveBudget::default();
        let outcome = extract_invoices(&archive, &mut budget, &["wrong-password".to_owned()]);
        assert_eq!(
            outcome,
            ArchiveOutcome::Encrypted {
                kind: crate::infra::file_signature::FileKind::Zip
            }
        );
    }

    #[test]
    fn refuses_an_archive_over_the_entry_limit() {
        let mut buffer = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut buffer));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            for index in 0..super::MAX_ARCHIVE_ENTRIES + 1 {
                writer.start_file(format!("{index}.txt"), options).unwrap();
                writer.write_all(b"x").unwrap();
            }
            writer.finish().unwrap();
        }

        let mut budget = ArchiveBudget::default();
        assert_eq!(
            extract_invoices(&buffer, &mut budget, &[]),
            ArchiveOutcome::Refused
        );
    }
}
