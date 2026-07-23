use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::LOCATION;
use reqwest::redirect::Policy;
use serde::Deserialize;
use tokio::net::lookup_host;
use url::{Host, Url};

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

pub const MAX_DOWNLOAD_BYTES: u64 = 50 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "invoice-reimbursement-desktop/0.2";
const NUONUO_HOST: &str = "nnfp.jss.com.cn";
const NUONUO_LANDING_PATH: &str = "/scan-invoice/printQrcode";
const NUONUO_DETAIL_PATH: &str = "/sapi/scan2/getIvcDetailShow.do";
const NUONUO_FORM_FIELDS: [&str; 5] = [
    "paramList",
    "code",
    "aliView",
    "invoiceDetailMiddleUri",
    "shortLinkSource",
];

#[derive(Clone, Default)]
pub struct SecureInvoiceLinkDownloader;

#[async_trait]
impl InvoiceLinkDownloader for SecureInvoiceLinkDownloader {
    async fn download(&self, source_url: &str) -> Result<InvoiceLinkDownload, AppError> {
        let source = Url::parse(source_url).map_err(|_| invalid_download_url())?;
        validate_source_url(&source)?;
        if is_ignored_source(&source) {
            return Ok(InvoiceLinkDownload::Ignored);
        }

        let fetched = fetch_get(&source, MAX_DOWNLOAD_BYTES).await?;
        if let Ok(invoice) = classify_download(&fetched.final_url, &fetched.bytes) {
            return Ok(InvoiceLinkDownload::Downloaded(invoice));
        }
        if !is_nuonuo_landing(&fetched.final_url) {
            return Err(unsupported_download_body());
        }

        let pdf_url = resolve_nuonuo_pdf_url(&fetched.final_url).await?;
        let fetched = fetch_get(&pdf_url, MAX_DOWNLOAD_BYTES).await?;
        classify_download(&fetched.final_url, &fetched.bytes).map(InvoiceLinkDownload::Downloaded)
    }
}

struct FetchedBody {
    final_url: Url,
    bytes: Vec<u8>,
}

async fn fetch_get(source: &Url, max_bytes: u64) -> Result<FetchedBody, AppError> {
    let mut current = source.clone();
    for redirect_count in 0..=MAX_REDIRECTS {
        validate_source_url(&current)?;
        let client = client_for_url(&current).await?;
        let response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|_| request_failure(true))?;
        if response.status().is_redirection() {
            if redirect_count == MAX_REDIRECTS {
                return Err(download_error(
                    false,
                    "invoice link exceeded the redirect limit",
                ));
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| download_error(false, "invoice link redirect was invalid"))?;
            current = current
                .join(location)
                .map_err(|_| download_error(false, "invoice link redirect was invalid"))?;
            continue;
        }
        validate_success_status(response.status())?;
        let bytes = read_response_body(response, max_bytes).await?;
        return Ok(FetchedBody {
            final_url: current,
            bytes,
        });
    }
    Err(download_error(
        false,
        "invoice link exceeded the redirect limit",
    ))
}

async fn client_for_url(url: &Url) -> Result<reqwest::Client, AppError> {
    let (domain, addresses) = resolve_public_target(url).await?;
    let mut builder = reqwest::Client::builder()
        .redirect(Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT);
    if let Some(domain) = domain {
        builder = builder.resolve_to_addrs(&domain, &addresses);
    }
    builder.build().map_err(|_| request_failure(false))
}

async fn resolve_public_target(url: &Url) -> Result<(Option<String>, Vec<SocketAddr>), AppError> {
    let port = url
        .port_or_known_default()
        .ok_or_else(invalid_download_url)?;
    let (domain, addresses) = match url.host().ok_or_else(invalid_download_url)? {
        Host::Ipv4(ip) => (None, vec![SocketAddr::new(IpAddr::V4(ip), port)]),
        Host::Ipv6(ip) => (None, vec![SocketAddr::new(IpAddr::V6(ip), port)]),
        Host::Domain(domain) => {
            let mut addresses = lookup_host((domain, port))
                .await
                .map_err(|_| request_failure(true))?
                .collect::<Vec<_>>();
            addresses.sort_unstable();
            addresses.dedup();
            (Some(domain.to_owned()), addresses)
        }
    };
    if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(address.ip())) {
        return Err(download_error(
            false,
            "invoice link target is not a public network address",
        ));
    }
    Ok((domain, addresses))
}

async fn read_response_body(
    mut response: reqwest::Response,
    max_bytes: u64,
) -> Result<Vec<u8>, AppError> {
    validate_content_length_with_limit(response.content_length(), max_bytes)?;
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await.map_err(|_| request_failure(true))? {
        append_bounded_chunk(&mut bytes, &chunk, max_bytes)?;
    }
    Ok(bytes)
}

#[cfg(test)]
fn validate_content_length(content_length: Option<u64>) -> Result<(), AppError> {
    validate_content_length_with_limit(content_length, MAX_DOWNLOAD_BYTES)
}

fn validate_content_length_with_limit(
    content_length: Option<u64>,
    max_bytes: u64,
) -> Result<(), AppError> {
    if content_length.is_some_and(|length| length > max_bytes) {
        return Err(download_error(
            false,
            "invoice link response exceeded the size limit",
        ));
    }
    Ok(())
}

fn append_bounded_chunk(
    destination: &mut Vec<u8>,
    chunk: &[u8],
    max_bytes: u64,
) -> Result<(), AppError> {
    let next_length = destination
        .len()
        .checked_add(chunk.len())
        .and_then(|length| u64::try_from(length).ok())
        .ok_or_else(|| download_error(false, "invoice link response exceeded the size limit"))?;
    if next_length > max_bytes {
        return Err(download_error(
            false,
            "invoice link response exceeded the size limit",
        ));
    }
    destination.extend_from_slice(chunk);
    Ok(())
}

fn is_nuonuo_landing(url: &Url) -> bool {
    url.host_str() == Some(NUONUO_HOST) && url.path() == NUONUO_LANDING_PATH
}

fn nuonuo_form_fields(url: &Url) -> Result<Vec<(String, String)>, AppError> {
    if !is_nuonuo_landing(url) {
        return Err(nuonuo_failure());
    }
    let pairs = url.query_pairs().collect::<Vec<_>>();
    NUONUO_FORM_FIELDS
        .into_iter()
        .map(|name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| (name.to_owned(), value.to_string()))
                .filter(|(_, value)| !value.trim().is_empty())
                .ok_or_else(nuonuo_failure)
        })
        .collect()
}

async fn resolve_nuonuo_pdf_url(landing: &Url) -> Result<Url, AppError> {
    let mut form = nuonuo_form_fields(landing)?;
    form.push((
        "_timestamp".to_owned(),
        chrono::Utc::now().timestamp_millis().to_string(),
    ));
    let endpoint = Url::parse(&format!("https://{NUONUO_HOST}{NUONUO_DETAIL_PATH}"))
        .map_err(|_| nuonuo_failure())?;
    let client = client_for_url(&endpoint).await?;
    let response = client
        .post(endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|_| request_failure(true))?;
    validate_success_status(response.status())?;
    let body = read_response_body(response, MAX_METADATA_BYTES).await?;
    parse_nuonuo_pdf_url(&body)
}

#[derive(Deserialize)]
struct NuonuoResponse {
    status: String,
    data: Option<NuonuoData>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NuonuoData {
    invoice_simple_vo: Option<NuonuoInvoice>,
}

#[derive(Deserialize)]
struct NuonuoInvoice {
    url: Option<String>,
}

fn parse_nuonuo_pdf_url(body: &[u8]) -> Result<Url, AppError> {
    let response = serde_json::from_slice::<NuonuoResponse>(body).map_err(|_| nuonuo_failure())?;
    if response.status != "0000" {
        return Err(nuonuo_failure());
    }
    let url = response
        .data
        .and_then(|data| data.invoice_simple_vo)
        .and_then(|invoice| invoice.url)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(nuonuo_failure)
        .and_then(|value| Url::parse(&value).map_err(|_| nuonuo_failure()))?;
    validate_source_url(&url).map_err(|_| nuonuo_failure())?;
    Ok(url)
}

fn validate_success_status(status: reqwest::StatusCode) -> Result<(), AppError> {
    if status.is_success() {
        return Ok(());
    }
    Err(download_error(
        status.is_server_error() || status.as_u16() == 429,
        "invoice link returned an unsuccessful response",
    ))
}

fn invalid_download_url() -> AppError {
    AppError::validation("invoiceLink", "invoice download URL is invalid")
}

fn request_failure(retryable: bool) -> AppError {
    download_error(retryable, "invoice link request failed")
}

fn unsupported_download_body() -> AppError {
    download_error(
        false,
        "invoice link did not return a supported PDF, image, or ZIP file",
    )
}

fn nuonuo_failure() -> AppError {
    download_error(false, "Nuonuo invoice link could not be resolved")
}

fn download_error(retryable: bool, message: &str) -> AppError {
    AppError::External {
        service: "invoice_download".to_owned(),
        retryable,
        message: message.to_owned(),
    }
}

fn validate_source_url(url: &Url) -> Result<(), AppError> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
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
        return Err(unsupported_download_body());
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
mod tests;
