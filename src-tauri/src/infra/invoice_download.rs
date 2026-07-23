use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use async_trait::async_trait;
use url::Url;

use crate::domain::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedInvoice {
    pub file_name: String,
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvoiceLinkDownload {
    Downloaded(DownloadedInvoice),
    Ignored,
}

#[async_trait]
pub trait InvoiceLinkDownloader: Send + Sync {
    async fn download(&self, source_url: &str) -> Result<InvoiceLinkDownload, AppError>;
}

fn validate_source_url(url: &Url) -> Result<(), AppError> {
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(AppError::validation(
            "invoiceLink",
            "invoice download URL must use HTTPS and include a host",
        ));
    }
    Ok(())
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return is_public_ipv4(ipv4);
    }
    let segments = ip.segments();
    let is_unique_local = (segments[0] & 0xfe00) == 0xfc00;
    let is_link_local = (segments[0] & 0xffc0) == 0xfe80;
    let is_documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
    let is_discard_only = segments[..4] == [0x0100, 0, 0, 0];
    let is_ipv4_compatible = segments[..6] == [0, 0, 0, 0, 0, 0];
    if is_ipv4_compatible {
        return is_public_ipv4(Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        ));
    }
    !(is_unique_local || is_link_local || is_documentation || is_discard_only)
}

fn is_ignored_source(url: &Url) -> bool {
    let is_xml = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|name| std::path::Path::new(name).extension())
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("xml"));
    let is_nuonuo_home =
        url.host_str() == Some("fp.nuonuo.com") && url.path() == "/" && url.query().is_none();
    is_xml || is_nuonuo_home
}

fn classify_download(url: &Url, bytes: &[u8]) -> Result<DownloadedInvoice, AppError> {
    let (extension, mime_type) = if bytes.starts_with(b"%PDF-") {
        ("pdf", "application/pdf")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        ("jpg", "image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        ("png", "image/png")
    } else if [b"PK\x03\x04".as_slice(), b"PK\x05\x06", b"PK\x07\x08"]
        .iter()
        .any(|signature| bytes.starts_with(signature))
    {
        ("zip", "application/zip")
    } else {
        return Err(AppError::External {
            service: "invoice_download".to_owned(),
            retryable: false,
            message: "invoice link did not return a supported PDF, image, or ZIP file".to_owned(),
        });
    };
    let file_name = download_file_name(url, extension);
    Ok(DownloadedInvoice {
        file_name,
        bytes: bytes.to_vec(),
        mime_type: mime_type.to_owned(),
    })
}

fn download_file_name(url: &Url, extension: &str) -> String {
    let candidate = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|name| !name.is_empty())
        .map(sanitize_filename::sanitize)
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case(extension))
        });
    candidate.unwrap_or_else(|| format!("downloaded-invoice.{extension}"))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use url::Url;

    use super::{
        DownloadedInvoice, classify_download, is_ignored_source, is_public_ip, validate_source_url,
    };

    #[test]
    fn source_url_policy_requires_https_and_a_host() {
        for value in [
            "http://invoice.example/invoice.pdf",
            "file:///tmp/invoice.pdf",
        ] {
            let parsed = Url::parse(value).expect("fixture URL should parse");
            assert!(validate_source_url(&parsed).is_err(), "{value} must fail");
        }

        let accepted = Url::parse("https://invoice.example/invoice.pdf").unwrap();
        assert!(validate_source_url(&accepted).is_ok());
    }

    #[test]
    fn public_ip_policy_rejects_local_private_and_reserved_ranges() {
        for ip in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)),
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)),
            IpAddr::V4(Ipv4Addr::BROADCAST),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6("fc00::1".parse().unwrap()),
            IpAddr::V6("fe80::1".parse().unwrap()),
            IpAddr::V6("ff02::1".parse().unwrap()),
            IpAddr::V6("2001:db8::1".parse().unwrap()),
            IpAddr::V6("::ffff:127.0.0.1".parse().unwrap()),
        ] {
            assert!(!is_public_ip(ip), "{ip} must be rejected");
        }

        for ip in [
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V6("2606:4700:4700::1111".parse().unwrap()),
        ] {
            assert!(is_public_ip(ip), "{ip} must be accepted");
        }
    }

    #[test]
    fn classifies_supported_downloads_by_magic_bytes() {
        let cases: [(&str, &[u8], &str, &str); 4] = [
            (
                "https://invoice.example/opaque?id=1",
                b"%PDF-1.7\n%%EOF\n",
                "downloaded-invoice.pdf",
                "application/pdf",
            ),
            (
                "https://invoice.example/photo",
                &[0xff, 0xd8, 0xff, 0xe0],
                "downloaded-invoice.jpg",
                "image/jpeg",
            ),
            (
                "https://invoice.example/invoice.png?signature=secret",
                &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
                "invoice.png",
                "image/png",
            ),
            (
                "https://invoice.example/archive.zip",
                b"PK\x05\x06payload",
                "archive.zip",
                "application/zip",
            ),
        ];

        for (url, bytes, expected_name, expected_mime) in cases {
            assert_eq!(
                classify_download(&Url::parse(url).unwrap(), bytes).unwrap(),
                DownloadedInvoice {
                    file_name: expected_name.to_owned(),
                    bytes: bytes.to_vec(),
                    mime_type: expected_mime.to_owned(),
                }
            );
        }
    }

    #[test]
    fn rejects_html_xml_and_unknown_download_bodies() {
        let url = Url::parse("https://invoice.example/invoice.pdf").unwrap();

        for bytes in [
            b"<!doctype html><title>portal</title>".as_slice(),
            b"<?xml version=\"1.0\"?><invoice/>".as_slice(),
            b"unknown".as_slice(),
        ] {
            assert!(classify_download(&url, bytes).is_err());
        }
    }

    #[test]
    fn ignores_xml_and_the_parameterless_nuonuo_home_page() {
        for value in [
            "https://invoice.example/invoice.xml?signature=secret",
            "https://fp.nuonuo.com/#/",
        ] {
            assert!(is_ignored_source(&Url::parse(value).unwrap()));
        }

        for value in [
            "https://fp.nuonuo.com/invoice/123",
            "https://nnfp.jss.com.cn/6zs=8fpWZO-1bZaS",
            "https://invoice.example/invoice.pdf",
        ] {
            assert!(!is_ignored_source(&Url::parse(value).unwrap()));
        }
    }
}
