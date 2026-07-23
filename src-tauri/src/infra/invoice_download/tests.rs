use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::Url;

use super::{
    DownloadedInvoice, MAX_DOWNLOAD_BYTES, append_bounded_chunk, classify_download,
    is_ignored_source, is_public_ip, nuonuo_form_fields, parse_nuonuo_pdf_url,
    validate_content_length, validate_source_url,
};

#[test]
fn source_url_policy_requires_https_and_a_host() {
    for value in [
        "http://invoice.example/invoice.pdf",
        "file:///tmp/invoice.pdf",
        "https://user:password@invoice.example/invoice.pdf",
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

#[test]
fn response_size_is_checked_from_headers_and_streamed_chunks() {
    assert!(validate_content_length(None).is_ok());
    assert!(validate_content_length(Some(MAX_DOWNLOAD_BYTES)).is_ok());
    assert!(validate_content_length(Some(MAX_DOWNLOAD_BYTES + 1)).is_err());

    let mut bytes = b"1234".to_vec();
    append_bounded_chunk(&mut bytes, b"56", 6).unwrap();
    assert_eq!(bytes, b"123456");
    let error = append_bounded_chunk(&mut bytes, b"7", 6).unwrap_err();
    assert_eq!(bytes, b"123456");
    assert!(error.to_string().contains("size limit"));
}

#[test]
fn nuonuo_form_uses_only_whitelisted_print_qrcode_fields() {
    let url = Url::parse(
        "https://nnfp.jss.com.cn/scan-invoice/printQrcode?\
         paramList=invoice-data&code=short-code&aliView=true&\
         invoiceDetailMiddleUri=printQrcode%3FparamList%3Dinvoice-data&\
         shortLinkSource=1&wxApplet=0&unexpected=secret",
    )
    .unwrap();

    assert_eq!(
        nuonuo_form_fields(&url).unwrap(),
        vec![
            ("paramList".to_owned(), "invoice-data".to_owned()),
            ("code".to_owned(), "short-code".to_owned()),
            ("aliView".to_owned(), "true".to_owned()),
            (
                "invoiceDetailMiddleUri".to_owned(),
                "printQrcode?paramList=invoice-data".to_owned(),
            ),
            ("shortLinkSource".to_owned(), "1".to_owned()),
        ]
    );
}

#[test]
fn nuonuo_parser_returns_only_a_valid_https_pdf_url() {
    let response = br#"{
        "status": "0000",
        "data": {
            "invoiceSimpleVo": {
                "url": "https://files.example/invoice.pdf?signature=secret",
                "imgUrl": "https://files.example/invoice.jpg"
            }
        }
    }"#;

    assert_eq!(
        parse_nuonuo_pdf_url(response).unwrap().as_str(),
        "https://files.example/invoice.pdf?signature=secret"
    );

    for invalid in [
        br#"{"status":"9999","msg":"provider secret diagnostic"}"#.as_slice(),
        br#"{"status":"0000","data":{"invoiceSimpleVo":{"url":"http://127.0.0.1/invoice.pdf?token=secret"}}}"#.as_slice(),
        br#"{"status":"0000","data":{"invoiceSimpleVo":{}}}"#.as_slice(),
        b"not-json".as_slice(),
    ] {
        let error = parse_nuonuo_pdf_url(invalid).unwrap_err();
        let message = error.to_string();
        assert!(!message.contains("provider secret diagnostic"));
        assert!(!message.contains("token=secret"));
        assert!(message.contains("Nuonuo"));
    }
}
