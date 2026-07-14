use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::net::{TcpStream, ToSocketAddrs};
use std::ops::RangeInclusive;
use std::time::Duration;

use crate::db::accounts::{MailboxAccount, MailboxProvider, SyncCursor};
use crate::domain::error::AppError;

pub const DEFAULT_MAILBOX: &str = "INBOX";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImapAccountConfig {
    pub provider: MailboxProvider,
    pub email: String,
    pub host: String,
    pub port: u16,
    pub mailbox: String,
    pub tls: bool,
}

impl ImapAccountConfig {
    pub fn from_account(account: &MailboxAccount) -> Self {
        Self {
            provider: account.provider,
            email: account.email.clone(),
            host: account.imap_host.clone(),
            port: account.imap_port,
            mailbox: DEFAULT_MAILBOX.to_owned(),
            tls: true,
        }
    }

    pub fn provider_default(provider: MailboxProvider, email: impl Into<String>) -> Self {
        let host = match provider {
            MailboxProvider::Gmail => "imap.gmail.com",
            MailboxProvider::QQ => "imap.qq.com",
        };
        Self {
            provider,
            email: email.into(),
            host: host.to_owned(),
            port: 993,
            mailbox: DEFAULT_MAILBOX.to_owned(),
            tls: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMessage {
    pub uid: u32,
    pub mailbox: String,
    pub raw: Vec<u8>,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxDelta {
    pub uid_validity: u32,
    pub messages: Vec<RawMessage>,
    pub highest_uid: u32,
}

#[async_trait]
pub trait ImapGateway: Send + Sync {
    async fn test_connection(
        &self,
        config: &ImapAccountConfig,
        secret: &str,
    ) -> Result<(), AppError>;

    async fn fetch_since(
        &self,
        config: &ImapAccountConfig,
        secret: &str,
        cursor: Option<SyncCursor>,
    ) -> Result<MailboxDelta, AppError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImapGatewaySettings {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub max_messages: usize,
    pub max_message_bytes: usize,
    pub max_total_bytes: usize,
    pub verify_certificates: bool,
}

#[derive(Debug, Clone)]
pub struct NativeTlsImapGateway {
    settings: ImapGatewaySettings,
}

impl Default for NativeTlsImapGateway {
    fn default() -> Self {
        Self {
            settings: ImapGatewaySettings {
                connect_timeout: Duration::from_secs(15),
                read_timeout: Duration::from_secs(30),
                write_timeout: Duration::from_secs(30),
                max_messages: 1_000,
                max_message_bytes: 50 * 1024 * 1024,
                max_total_bytes: 200 * 1024 * 1024,
                verify_certificates: true,
            },
        }
    }
}

impl NativeTlsImapGateway {
    pub fn settings(&self) -> ImapGatewaySettings {
        self.settings
    }
}

#[async_trait]
impl ImapGateway for NativeTlsImapGateway {
    async fn test_connection(
        &self,
        config: &ImapAccountConfig,
        secret: &str,
    ) -> Result<(), AppError> {
        let config = config.clone();
        let secret = secret.to_owned();
        let settings = self.settings;
        tokio::task::spawn_blocking(move || {
            let mut session = connect(&config, &secret, settings)?;
            session
                .logout()
                .map_err(|_| imap_error("IMAP logout failed"))
        })
        .await
        .map_err(|_| imap_error("IMAP connection task failed"))?
    }

    async fn fetch_since(
        &self,
        config: &ImapAccountConfig,
        secret: &str,
        cursor: Option<SyncCursor>,
    ) -> Result<MailboxDelta, AppError> {
        let config = config.clone();
        let secret = secret.to_owned();
        let settings = self.settings;
        tokio::task::spawn_blocking(move || fetch_blocking(&config, &secret, cursor, settings))
            .await
            .map_err(|_| imap_error("IMAP fetch task failed"))?
    }
}

fn fetch_blocking(
    config: &ImapAccountConfig,
    secret: &str,
    cursor: Option<SyncCursor>,
    settings: ImapGatewaySettings,
) -> Result<MailboxDelta, AppError> {
    let mut session = connect(config, secret, settings)?;
    let mailbox = session
        .examine(&config.mailbox)
        .map_err(|_| imap_error("IMAP mailbox selection failed"))?;
    let uid_validity = mailbox
        .uid_validity
        .ok_or_else(|| imap_error("IMAP mailbox did not provide UIDVALIDITY"))?;
    let highest_uid = mailbox
        .uid_next
        .map(|uid| uid.saturating_sub(1))
        .unwrap_or(0);
    let mut messages = Vec::new();
    let mut budget = RawBudget::new(settings.max_message_bytes, settings.max_total_bytes);
    let window = uid_window(cursor, uid_validity, highest_uid, settings.max_messages)?;
    for uid in window.into_iter().flatten() {
        let metadata = session
            .uid_fetch(uid.to_string(), "(UID RFC822.SIZE INTERNALDATE)")
            .map_err(|_| imap_error("IMAP message metadata fetch failed"))?;
        let Some(metadata) = metadata.iter().next() else {
            continue;
        };
        let actual_uid = metadata
            .uid
            .ok_or_else(|| imap_error("IMAP response omitted UID"))?;
        let size = usize::try_from(metadata.size.unwrap_or(0))
            .map_err(|_| limit_error("IMAP message size is invalid"))?;
        budget.check_declared(size)?;
        let received_at = metadata
            .internal_date()
            .map(|date| date.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);
        let query = format!(
            "(UID BODY.PEEK[]<0.{}>)",
            settings.max_message_bytes.saturating_add(1)
        );
        let fetched = session
            .uid_fetch(actual_uid.to_string(), query)
            .map_err(|_| imap_error("IMAP message body fetch failed"))?;
        let fetched = fetched
            .iter()
            .next()
            .ok_or_else(|| imap_error("IMAP message body response was empty"))?;
        let raw = fetched
            .body()
            .ok_or_else(|| imap_error("IMAP message body was omitted"))?;
        budget.add_actual(raw.len())?;
        messages.push(RawMessage {
            uid: actual_uid,
            mailbox: config.mailbox.clone(),
            raw: raw.to_vec(),
            received_at,
        });
    }
    let _ = session.logout();
    Ok(MailboxDelta {
        uid_validity,
        messages,
        highest_uid,
    })
}

fn uid_window(
    cursor: Option<SyncCursor>,
    uid_validity: u32,
    highest_uid: u32,
    max_messages: usize,
) -> Result<Option<RangeInclusive<u32>>, AppError> {
    let start_uid = match cursor {
        Some(cursor) if cursor.uid_validity == uid_validity => {
            let Some(next) = cursor.last_uid.checked_add(1) else {
                return Ok(None);
            };
            next
        }
        _ => 1,
    };
    if start_uid > highest_uid {
        return Ok(None);
    }
    let candidate_count = u64::from(highest_uid) - u64::from(start_uid) + 1;
    if candidate_count > max_messages as u64 {
        return Err(limit_error("IMAP delta exceeds message count limit"));
    }
    Ok(Some(start_uid..=highest_uid))
}

struct RawBudget {
    total: usize,
    max_message: usize,
    max_total: usize,
}

impl RawBudget {
    fn new(max_message: usize, max_total: usize) -> Self {
        Self {
            total: 0,
            max_message,
            max_total,
        }
    }

    fn check_declared(&self, bytes: usize) -> Result<(), AppError> {
        if bytes > self.max_message {
            return Err(limit_error("IMAP message exceeds size limit"));
        }
        let projected = self
            .total
            .checked_add(bytes)
            .ok_or_else(|| limit_error("IMAP delta size overflow"))?;
        if projected > self.max_total {
            return Err(limit_error("IMAP delta exceeds total size limit"));
        }
        Ok(())
    }

    fn add_actual(&mut self, bytes: usize) -> Result<(), AppError> {
        if bytes > self.max_message {
            return Err(limit_error("IMAP message exceeds size limit"));
        }
        self.total = self
            .total
            .checked_add(bytes)
            .ok_or_else(|| limit_error("IMAP delta size overflow"))?;
        if self.total > self.max_total {
            return Err(limit_error("IMAP delta exceeds total size limit"));
        }
        Ok(())
    }
}

fn connect(
    config: &ImapAccountConfig,
    secret: &str,
    settings: ImapGatewaySettings,
) -> Result<imap::Session<native_tls::TlsStream<TcpStream>>, AppError> {
    if !config.tls {
        return Err(imap_error("unencrypted IMAP is not supported"));
    }
    if secret.trim().is_empty() {
        return Err(imap_error("mailbox credential is unavailable"));
    }
    let addresses = (config.host.as_str(), config.port)
        .to_socket_addrs()
        .map_err(|_| imap_error("IMAP host resolution failed"))?;
    let mut last_error = None;
    for address in addresses {
        let tcp = match TcpStream::connect_timeout(&address, settings.connect_timeout) {
            Ok(stream) => stream,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        tcp.set_read_timeout(Some(settings.read_timeout))
            .map_err(|_| imap_error("failed to set IMAP read timeout"))?;
        tcp.set_write_timeout(Some(settings.write_timeout))
            .map_err(|_| imap_error("failed to set IMAP write timeout"))?;
        let connector = native_tls::TlsConnector::builder()
            .build()
            .map_err(|_| imap_error("failed to initialize TLS"))?;
        let tls = connector
            .connect(&config.host, tcp)
            .map_err(|_| imap_error("verified TLS handshake failed"))?;
        let mut client = imap::Client::new(tls);
        client
            .read_greeting()
            .map_err(|_| imap_error("IMAP server greeting failed"))?;
        return client
            .login(&config.email, secret)
            .map_err(|_| imap_error("IMAP authentication failed"));
    }
    let _ = last_error;
    Err(imap_error("IMAP connection failed"))
}

fn imap_error(message: &str) -> AppError {
    AppError::External {
        service: "imap".to_owned(),
        retryable: true,
        message: message.to_owned(),
    }
}

fn limit_error(message: &str) -> AppError {
    AppError::External {
        service: "imap".to_owned(),
        retryable: false,
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{RawBudget, uid_window};
    use crate::db::accounts::SyncCursor;

    #[test]
    fn max_uid_cursor_has_no_incremental_window_but_uidvalidity_change_rescans() {
        assert_eq!(
            uid_window(
                Some(SyncCursor {
                    uid_validity: 10,
                    last_uid: u32::MAX,
                }),
                10,
                u32::MAX,
                1_000,
            )
            .unwrap(),
            None
        );
        let changed = uid_window(
            Some(SyncCursor {
                uid_validity: 10,
                last_uid: u32::MAX,
            }),
            11,
            2,
            1_000,
        )
        .unwrap()
        .unwrap();
        assert_eq!(changed.collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn actual_raw_bytes_enforce_total_even_when_server_declares_smaller_sizes() {
        let mut budget = RawBudget::new(10, 12);
        budget.check_declared(1).unwrap();
        budget.add_actual(7).unwrap();
        budget.check_declared(1).unwrap();

        let error = budget.add_actual(6).unwrap_err();

        assert!(error.to_string().contains("total size limit"));
    }
}
