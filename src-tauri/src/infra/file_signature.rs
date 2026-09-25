//! File type detection from content, never from the file name.
//!
//! Mail attachments routinely lie about their type: a Chinese invoice portal
//! sends `…8.mov.zip` whose content is a QuickTime movie, an Office document
//! arrives as `application/zip` because DOCX is a ZIP container, and a portal
//! answer is an HTML page that merely links to the invoice. Every decision that
//! used to read the extension now reads these signatures instead.

/// Container formats the ingestion pipeline understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Pdf,
    Jpeg,
    Png,
    Gif,
    Zip,
    SevenZip,
    Rar,
    /// OLE2 compound file: legacy `.doc`, `.xls` and friends.
    Ole,
    /// QuickTime/MP4 container, seen when a `.zip` attachment is a video.
    QuickTime,
    Html,
    Xml,
    /// Plain text that is not markup (a rejection notice, a link list).
    Text,
    Unknown,
}

impl FileKind {
    /// Extension and MIME type for a kind that can be stored as an invoice.
    pub const fn invoice_identity(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Pdf => Some(("pdf", "application/pdf")),
            Self::Jpeg => Some(("jpg", "image/jpeg")),
            Self::Png => Some(("png", "image/png")),
            Self::Zip => Some(("zip", "application/zip")),
            _ => None,
        }
    }

    /// Whether document recognition and normalization can handle this kind.
    pub const fn is_invoice_document(self) -> bool {
        matches!(self, Self::Pdf | Self::Jpeg | Self::Png)
    }

    pub const fn is_archive(self) -> bool {
        matches!(self, Self::Zip | Self::SevenZip | Self::Rar)
    }

    pub const fn code(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Zip => "zip",
            Self::SevenZip => "7z",
            Self::Rar => "rar",
            Self::Ole => "ole",
            Self::QuickTime => "quicktime",
            Self::Html => "html",
            Self::Xml => "xml",
            Self::Text => "text",
            Self::Unknown => "unknown",
        }
    }
}

const PDF: &[u8] = b"%PDF-";
const JPEG: &[u8] = &[0xff, 0xd8, 0xff];
const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
const GIF: &[u8] = b"GIF8";
const OLE: &[u8] = &[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
const SEVEN_ZIP: &[u8] = &[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c];
const RAR4: &[u8] = &[0x52, 0x61, 0x72, 0x21, 0x1a, 0x07, 0x00];
const RAR5: &[u8] = &[0x52, 0x61, 0x72, 0x21, 0x1a, 0x07, 0x01, 0x00];

/// Detects the container kind from the leading bytes.
pub fn detect(bytes: &[u8]) -> FileKind {
    if bytes.starts_with(PDF) {
        return FileKind::Pdf;
    }
    if bytes.starts_with(JPEG) {
        return FileKind::Jpeg;
    }
    if bytes.starts_with(PNG) {
        return FileKind::Png;
    }
    if bytes.starts_with(GIF) {
        return FileKind::Gif;
    }
    if is_zip(bytes) {
        return FileKind::Zip;
    }
    if bytes.starts_with(SEVEN_ZIP) {
        return FileKind::SevenZip;
    }
    if bytes.starts_with(RAR4) || bytes.starts_with(RAR5) {
        return FileKind::Rar;
    }
    if bytes.starts_with(OLE) {
        return FileKind::Ole;
    }
    if is_quicktime(bytes) {
        return FileKind::QuickTime;
    }
    if looks_like_html(bytes) {
        return FileKind::Html;
    }
    if looks_like_xml(bytes) {
        return FileKind::Xml;
    }
    if looks_like_text(bytes) {
        return FileKind::Text;
    }
    FileKind::Unknown
}

/// ZIP local file header, empty archive, or spanned-archive marker.
pub fn is_zip(bytes: &[u8]) -> bool {
    [b"PK\x03\x04".as_slice(), b"PK\x05\x06", b"PK\x07\x08"]
        .iter()
        .any(|signature| bytes.starts_with(signature))
}

/// QuickTime/MP4 stores a box type at offset 4 (`ftyp`, `moov`, `mdat`, …).
fn is_quicktime(bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    let marker = &bytes[4..8];
    marker == b"ftyp"
        || marker == b"moov"
        || marker == b"mdat"
        || marker == b"wide"
        || marker == b"free"
        || marker == b"skip"
}

fn looks_like_html(bytes: &[u8]) -> bool {
    let head = leading_text(bytes, 512);
    let lowered = head.to_ascii_lowercase();
    lowered.contains("<!doctype html") || lowered.contains("<html")
}

fn looks_like_xml(bytes: &[u8]) -> bool {
    leading_text(bytes, 128).trim_start().starts_with("<?xml")
}

fn looks_like_text(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(512)];
    !sample.is_empty()
        && !sample.contains(&0)
        && sample
            .iter()
            .all(|byte| byte.is_ascii_graphic() || byte.is_ascii_whitespace() || *byte >= 0x80)
}

fn leading_text(bytes: &[u8], limit: usize) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(limit)]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{FileKind, detect, is_zip};

    #[test]
    fn detects_invoice_documents() {
        assert_eq!(detect(b"%PDF-1.7\n%%EOF"), FileKind::Pdf);
        assert_eq!(detect(&[0xff, 0xd8, 0xff, 0xe0]), FileKind::Jpeg);
        assert_eq!(
            detect(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            FileKind::Png
        );
        assert!(FileKind::Pdf.is_invoice_document());
        assert!(!FileKind::Zip.is_invoice_document());
    }

    #[test]
    fn detects_archives_office_and_lookalikes() {
        assert_eq!(detect(b"PK\x03\x04rest"), FileKind::Zip);
        assert_eq!(
            detect(&[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c]),
            FileKind::SevenZip
        );
        assert_eq!(detect(b"Rar!\x1a\x07\x00"), FileKind::Rar);
        assert_eq!(detect(b"Rar!\x1a\x07\x01\x00"), FileKind::Rar);
        assert_eq!(
            detect(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]),
            FileKind::Ole
        );
        assert!(!FileKind::Ole.is_archive());
        assert!(detect(b"7z\xbc\xaf\x27\x1c").is_archive());
    }

    #[test]
    fn a_video_named_zip_is_not_a_zip() {
        // `…8.mov.zip` from a real mailbox: the name promised an archive, the
        // content is a QuickTime movie.
        let mut movie = vec![0, 0, 0, 0x18];
        movie.extend_from_slice(b"ftypqt  ");
        movie.extend_from_slice(b"\x00\x00\x02\x00qt  ");
        let kind = detect(&movie);
        assert_eq!(kind, FileKind::QuickTime);
        assert!(!kind.is_archive());
        assert!(!is_zip(&movie));
    }

    #[test]
    fn detects_portal_pages_and_text() {
        assert_eq!(
            detect(b"<!DOCTYPE html><html><body>invoice</body></html>"),
            FileKind::Html
        );
        assert_eq!(detect(b"<?xml version=\"1.0\"?><Invoice/>"), FileKind::Xml);
        assert_eq!(detect(b"invoice link expired\n"), FileKind::Text);
        assert_eq!(detect(&[0x00, 0x01, 0x02, 0x03]), FileKind::Unknown);
    }
}
