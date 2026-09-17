//! QR payload extraction from invoice images.
//!
//! Chinese e-invoices frequently carry a QR code that links to the real file
//! instead of attaching it, so a mail whose only "invoice" is an image with a
//! code used to look like it contained nothing usable. Decoding happens locally
//! and only the URL is used, through the same link pipeline as any other link.

/// Largest image decoded, as a pixel count, so a hostile attachment cannot turn
/// QR scanning into unbounded work.
const MAX_QR_PIXELS: u64 = 16_000_000;
/// QR codes read from a single image.
const MAX_QR_CODES_PER_IMAGE: usize = 4;

/// HTTPS URLs found in the QR codes of an image, in scan order.
///
/// A wrong or unreadable image yields an empty list: this is a best-effort step
/// whose failures must never break ingestion.
pub fn decode_qr_urls(bytes: &[u8], limit: usize) -> Vec<String> {
    if limit == 0 || bytes.is_empty() {
        return Vec::new();
    }
    let Ok(image) = image::load_from_memory(bytes) else {
        return Vec::new();
    };
    let (width, height) = (u64::from(image.width()), u64::from(image.height()));
    if width == 0 || height == 0 || width.saturating_mul(height) > MAX_QR_PIXELS {
        return Vec::new();
    }
    let luma = image.to_luma8();
    let mut prepared = rqrr::PreparedImage::prepare(luma);
    let grids = prepared.detect_grids();
    let mut urls = Vec::new();
    for grid in grids.into_iter().take(MAX_QR_CODES_PER_IMAGE) {
        let Ok((_metadata, content)) = grid.decode() else {
            continue;
        };
        for candidate in qr_payload_urls(&content) {
            if !urls.contains(&candidate) {
                urls.push(candidate);
            }
            if urls.len() >= limit {
                return urls;
            }
        }
    }
    urls
}

/// Pulls HTTPS URLs out of one decoded payload.
///
/// Some codes wrap the URL in a wrapper such as `https://example/?url=…`, so the
/// payload is scanned rather than parsed as a single URL.
fn qr_payload_urls(content: &str) -> Vec<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut urls = Vec::new();
    for candidate in trimmed.split_whitespace() {
        let candidate = candidate.trim_matches(|character: char| {
            matches!(
                character,
                '"' | '\'' | '，' | ',' | '。' | '；' | ';' | ')' | '）'
            )
        });
        if candidate.starts_with("https://") && candidate.len() > "https://".len() {
            urls.push(candidate.to_owned());
        }
    }
    urls
}

#[cfg(test)]
mod tests {
    use super::{decode_qr_urls, qr_payload_urls};

    /// A real QR image (generated once, committed as a fixture) carrying an
    /// HTTPS invoice link.
    const QR_FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/qr-invoice.png");

    #[test]
    fn decodes_an_invoice_link_from_an_image() {
        let urls = decode_qr_urls(QR_FIXTURE, 2);
        assert_eq!(urls.len(), 1, "expected one link, got {urls:?}");
        assert!(
            urls[0].starts_with("https://invoice.example/download/"),
            "unexpected payload: {}",
            urls[0]
        );
    }

    #[test]
    fn an_image_without_a_code_yields_nothing() {
        let plain = include_bytes!("../../tests/fixtures/image-invoice.png");
        assert!(decode_qr_urls(plain, 4).is_empty());
        assert!(decode_qr_urls(&[0xff, 0xd8, 0xff, 0x00], 4).is_empty());
    }

    #[test]
    fn only_https_payloads_are_used() {
        assert_eq!(
            qr_payload_urls("http://invoice.example/1"),
            Vec::<String>::new()
        );
        assert_eq!(
            qr_payload_urls("  https://invoice.example/1 。"),
            vec!["https://invoice.example/1".to_owned()]
        );
        assert!(qr_payload_urls("   ").is_empty());
    }
}
