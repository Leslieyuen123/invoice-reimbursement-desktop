use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::net::{TcpStream, ToSocketAddrs};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRejectionReason {
    MessageTooLarge,
}

impl MessageRejectionReason {
    pub const fn code(self) -> &'static str {
        match self {
            Self::MessageTooLarge => "message_too_large",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedMessage {
    pub uid: u32,
    pub mailbox: String,
    pub received_at: DateTime<Utc>,
    pub reason: MessageRejectionReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxDelta {
    pub uid_validity: u32,
    pub messages: Vec<RawMessage>,
    pub rejected_messages: Vec<RejectedMessage>,
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
    let mut rejected_messages = Vec::new();
    let mut budget = RawBudget::new(settings.max_message_bytes, settings.max_total_bytes);
    let (uids, mut high_water) = match uid_start(cursor, uid_validity) {
        Some(start_uid) if start_uid <= highest_uid => {
            let found = session
                .uid_search(invoice_uid_search_query(start_uid))
                .map_err(|_| imap_error("IMAP UID search failed"))?;
            select_uid_batch(found, highest_uid, settings.max_messages)?
        }
        _ => (Vec::new(), highest_uid),
    };
    let mut processed_high_water = None;
    for uid in uids {
        let metadata = session
            .uid_fetch(uid.to_string(), "(UID RFC822.SIZE INTERNALDATE)")
            .map_err(|_| imap_error("IMAP message metadata fetch failed"))?;
        let Some(metadata) = metadata.iter().next() else {
            processed_high_water = Some(uid);
            continue;
        };
        let actual_uid = metadata
            .uid
            .ok_or_else(|| imap_error("IMAP response omitted UID"))?;
        let size = usize::try_from(metadata.size.unwrap_or(0))
            .map_err(|_| limit_error("IMAP message size is invalid"))?;
        let received_at = metadata
            .internal_date()
            .map(|date| date.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);
        match budget.admission(size)? {
            MessageAdmission::Admit => {}
            MessageAdmission::Reject(reason) => {
                rejected_messages.push(RejectedMessage {
                    uid: actual_uid,
                    mailbox: config.mailbox.clone(),
                    received_at,
                    reason,
                });
                processed_high_water = Some(uid);
                continue;
            }
            MessageAdmission::StopBatch => {
                let Some(processed) = processed_high_water else {
                    return Err(limit_error("IMAP delta exceeds total size limit"));
                };
                high_water = processed;
                break;
            }
        }
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
        match budget.admission(raw.len())? {
            MessageAdmission::Admit => {}
            MessageAdmission::Reject(reason) => {
                rejected_messages.push(RejectedMessage {
                    uid: actual_uid,
                    mailbox: config.mailbox.clone(),
                    received_at,
                    reason,
                });
                processed_high_water = Some(uid);
                continue;
            }
            MessageAdmission::StopBatch => {
                let Some(processed) = processed_high_water else {
                    return Err(limit_error("IMAP delta exceeds total size limit"));
                };
                high_water = processed;
                break;
            }
        }
        budget.add_actual(raw.len())?;
        messages.push(RawMessage {
            uid: actual_uid,
            mailbox: config.mailbox.clone(),
            raw: raw.to_vec(),
            received_at,
        });
        processed_high_water = Some(uid);
    }
    let _ = session.logout();
    Ok(MailboxDelta {
        uid_validity,
        messages,
        rejected_messages,
        highest_uid: high_water,
    })
}

fn invoice_uid_search_query(start_uid: u32) -> String {
    format!("UID {start_uid}:* SINCE 1-Jun-2026 BEFORE 1-Aug-2026")
}

fn uid_start(cursor: Option<SyncCursor>, uid_validity: u32) -> Option<u32> {
    match cursor {
        Some(cursor) if cursor.uid_validity == uid_validity => cursor.last_uid.checked_add(1),
        _ => Some(1),
    }
}

fn select_uid_batch(
    uids: impl IntoIterator<Item = u32>,
    mailbox_highest: u32,
    max_messages: usize,
) -> Result<(Vec<u32>, u32), AppError> {
    if max_messages == 0 {
        return Err(limit_error("IMAP message count limit is zero"));
    }
    let mut selected = uids
        .into_iter()
        .filter(|uid| *uid != 0 && *uid <= mailbox_highest)
        .collect::<Vec<_>>();
    selected.sort_unstable();
    selected.dedup();
    if selected.len() > max_messages {
        selected.truncate(max_messages);
        let high_water = *selected
            .last()
            .ok_or_else(|| limit_error("IMAP UID batch was unexpectedly empty"))?;
        Ok((selected, high_water))
    } else {
        Ok((selected, mailbox_highest))
    }
}

struct RawBudget {
    total: usize,
    max_message: usize,
    max_total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageAdmission {
    Admit,
    Reject(MessageRejectionReason),
    StopBatch,
}

impl RawBudget {
    fn new(max_message: usize, max_total: usize) -> Self {
        Self {
            total: 0,
            max_message,
            max_total,
        }
    }

    fn check_message(&self, bytes: usize) -> Result<(), AppError> {
        if bytes > self.max_message {
            return Err(limit_error("IMAP message exceeds size limit"));
        }
        Ok(())
    }

    fn admission(&self, bytes: usize) -> Result<MessageAdmission, AppError> {
        if bytes > self.max_message {
            return Ok(MessageAdmission::Reject(
                MessageRejectionReason::MessageTooLarge,
            ));
        }
        if self.can_fit(bytes)? {
            Ok(MessageAdmission::Admit)
        } else {
            Ok(MessageAdmission::StopBatch)
        }
    }

    fn can_fit(&self, bytes: usize) -> Result<bool, AppError> {
        let projected = self
            .total
            .checked_add(bytes)
            .ok_or_else(|| limit_error("IMAP delta size overflow"))?;
        Ok(projected <= self.max_total)
    }

    fn add_actual(&mut self, bytes: usize) -> Result<(), AppError> {
        self.check_message(bytes)?;
        let projected = self
            .total
            .checked_add(bytes)
            .ok_or_else(|| limit_error("IMAP delta size overflow"))?;
        if projected > self.max_total {
            return Err(limit_error("IMAP delta exceeds total size limit"));
        }
        self.total = projected;
        Ok(())
    }
}

fn connect(
    config: &ImapAccountConfig,
    secret: &str,
    settings: ImapGatewaySettings,
) -> Result<imap::Session<native_tls::TlsStream<TcpStream>>, AppError> {
    if !config.tls {
        return Err(configuration_error("unencrypted IMAP is not supported"));
    }
    if secret.trim().is_empty() {
        return Err(authentication_error("mailbox credential is unavailable"));
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
            .map_err(|_| authentication_error("IMAP authentication failed"));
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

fn authentication_error(message: &str) -> AppError {
    AppError::External {
        service: "imap_authentication".to_owned(),
        retryable: false,
        message: message.to_owned(),
    }
}

fn configuration_error(message: &str) -> AppError {
    permanent_imap_error(message)
}

fn permanent_imap_error(message: &str) -> AppError {
    AppError::External {
        service: "imap".to_owned(),
        retryable: false,
        message: message.to_owned(),
    }
}

fn limit_error(message: &str) -> AppError {
    permanent_imap_error(message)
}

#[cfg(test)]
mod tests {
    use super::{
        ImapAccountConfig, MessageAdmission, MessageRejectionReason, NativeTlsImapGateway,
        RawBudget, authentication_error, connect, imap_error, invoice_uid_search_query,
        limit_error, select_uid_batch, uid_start,
    };

    #[test]
    fn invoice_search_is_limited_to_june_and_july_2026() {
        assert_eq!(
            invoice_uid_search_query(4073),
            "UID 4073:* SINCE 1-Jun-2026 BEFORE 1-Aug-2026"
        );
    }
    use crate::db::accounts::{MailboxProvider, SyncCursor};
    use crate::domain::error::AppError;

    fn assert_external(error: AppError, expected_service: &str, expected_retryable: bool) {
        assert!(
            matches!(
                error,
                AppError::External {
                    ref service,
                    retryable,
                    ..
                } if service == expected_service && retryable == expected_retryable
            ),
            "unexpected IMAP error: {error:?}"
        );
    }

    #[test]
    fn gateway_auth_and_configuration_errors_are_not_retryable() {
        let config =
            ImapAccountConfig::provider_default(MailboxProvider::Gmail, "finance@example.com");
        let settings = NativeTlsImapGateway::default().settings();
        let blank_secret = match connect(&config, "   ", settings) {
            Ok(_) => panic!("blank secret must be rejected before connecting"),
            Err(error) => error,
        };
        assert_external(blank_secret, "imap_authentication", false);

        let mut insecure = config;
        insecure.tls = false;
        let insecure_config = match connect(&insecure, "password", settings) {
            Ok(_) => panic!("unencrypted configuration must be rejected before connecting"),
            Err(error) => error,
        };
        assert_external(insecure_config, "imap", false);

        assert_external(
            authentication_error("IMAP authentication failed"),
            "imap_authentication",
            false,
        );
        assert_external(imap_error("IMAP transport failed"), "imap", true);
        assert_external(limit_error("IMAP resource limit"), "imap", false);
    }

    #[test]
    fn sparse_uid_selection_uses_actual_messages_instead_of_numeric_span() {
        let (uids, high_water) = select_uid_batch(vec![50_000, 7], 50_000, 1_000)
            .expect("sparse batch should select and sort");

        assert_eq!(uids, [7, 50_000]);
        assert_eq!(high_water, 50_000);
    }

    #[test]
    fn empty_uid_selection_advances_to_the_mailbox_high_water() {
        let (uids, high_water) =
            select_uid_batch(Vec::<u32>::new(), 50_000, 1_000).expect("empty search should select");

        assert!(uids.is_empty());
        assert_eq!(high_water, 50_000);
    }

    #[test]
    fn uid_selection_pages_large_backlogs_without_skipping_the_next_batch() {
        let (first_uids, first_high_water) =
            select_uid_batch(1..=1_001, 1_001, 1_000).expect("first batch should select");
        let (second_uids, second_high_water) =
            select_uid_batch(vec![1_001], 1_001, 1_000).expect("second batch should select");

        assert_eq!(first_uids.len(), 1_000);
        assert_eq!(first_uids.first(), Some(&1));
        assert_eq!(first_uids.last(), Some(&1_000));
        assert_eq!(first_high_water, 1_000);
        assert_eq!(second_uids, [1_001]);
        assert_eq!(second_high_water, 1_001);
    }

    #[test]
    fn total_budget_can_stop_a_nonempty_batch_before_overflow() {
        let mut budget = RawBudget::new(10, 12);
        budget.add_actual(7).unwrap();

        assert!(!budget.can_fit(6).unwrap());
        assert_eq!(budget.total, 7);
    }

    #[test]
    fn message_admission_distinguishes_rejection_from_batch_exhaustion() {
        let mut budget = RawBudget::new(10, 12);

        assert_eq!(
            budget.admission(11).unwrap(),
            MessageAdmission::Reject(MessageRejectionReason::MessageTooLarge)
        );
        assert_eq!(budget.total, 0);

        budget.add_actual(7).unwrap();
        assert_eq!(budget.admission(6).unwrap(), MessageAdmission::StopBatch);
        assert_eq!(budget.admission(5).unwrap(), MessageAdmission::Admit);
        assert_eq!(budget.total, 7);
    }

    #[test]
    fn a_single_message_over_the_total_budget_is_rejected() {
        let mut budget = RawBudget::new(20, 12);

        let error = budget.add_actual(13).unwrap_err();

        assert!(error.to_string().contains("total size limit"));
        assert_eq!(budget.total, 0);
    }

    #[test]
    fn max_uid_cursor_has_no_incremental_window_but_uidvalidity_change_rescans() {
        assert_eq!(
            uid_start(
                Some(SyncCursor {
                    uid_validity: 10,
                    last_uid: u32::MAX,
                }),
                10,
            ),
            None
        );
        assert_eq!(
            uid_start(
                Some(SyncCursor {
                    uid_validity: 10,
                    last_uid: u32::MAX,
                }),
                11,
            ),
            Some(1)
        );
    }

    #[test]
    fn actual_raw_bytes_enforce_total_even_when_server_declares_smaller_sizes() {
        let mut budget = RawBudget::new(10, 12);
        budget.check_message(1).unwrap();
        budget.add_actual(7).unwrap();
        budget.check_message(1).unwrap();

        let error = budget.add_actual(6).unwrap_err();

        assert!(error.to_string().contains("total size limit"));
    }
}
