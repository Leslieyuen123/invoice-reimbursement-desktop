use std::collections::VecDeque;
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use uuid::Uuid;
use zip::write::SimpleFileOptions;

use super::{
    InvoicePart, MAX_MESSAGES_PER_SYNC, SyncProgress, TouchedItemBudget, ZipExpansionBudget,
    expand_zip_part, parse_invoice_parts, parse_invoice_parts_with_zip_budget,
    preflight_zip_entries, validate_delta,
};
use crate::db;
use crate::db::accounts::{
    MailboxAccount, MailboxAccountRepository, MailboxProvider, NewMailboxAccount, SyncCursor,
};
use crate::db::items::{ItemFilter, ItemRepository};
use crate::domain::error::AppError;
use crate::infra::credentials::MemoryCredentialStore;
use crate::infra::extraction::{DocumentExtractor, ExtractedDocument};
use crate::infra::files::AppPaths;
use crate::infra::imap::{
    ImapAccountConfig, ImapDateRange, ImapGateway, MailboxDelta, MessageRejectionReason,
    RawMessage, RejectedMessage,
};
use crate::services::import::ImportService;
use crate::services::recognition::RecognitionService;

struct BudgetGateway {
    deltas: Mutex<VecDeque<Result<MailboxDelta, AppError>>>,
    range_calls: AtomicUsize,
}

impl BudgetGateway {
    fn new(deltas: Vec<Result<MailboxDelta, AppError>>) -> Self {
        Self {
            deltas: Mutex::new(deltas.into()),
            range_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ImapGateway for BudgetGateway {
    async fn test_connection(
        &self,
        _config: &ImapAccountConfig,
        _secret: &str,
    ) -> Result<(), AppError> {
        Ok(())
    }

    async fn fetch_since(
        &self,
        _config: &ImapAccountConfig,
        _secret: &str,
        _cursor: Option<SyncCursor>,
    ) -> Result<MailboxDelta, AppError> {
        panic!("budget test must use range fetches")
    }

    async fn fetch_range(
        &self,
        _config: &ImapAccountConfig,
        _secret: &str,
        _cursor: Option<SyncCursor>,
        _range: ImapDateRange,
    ) -> Result<MailboxDelta, AppError> {
        self.range_calls.fetch_add(1, Ordering::SeqCst);
        self.deltas
            .lock()
            .unwrap()
            .pop_front()
            .expect("budget range response should be queued")
    }
}

struct BudgetExtractor;

impl DocumentExtractor for BudgetExtractor {
    fn extract(&self, _path: &Path) -> Result<ExtractedDocument, AppError> {
        Ok(ExtractedDocument {
            text: "electronic invoice".to_owned(),
            normalized_pdf: None,
            warnings: Vec::new(),
        })
    }
}

struct BudgetContext {
    _directory: tempfile::TempDir,
    service: super::SyncService,
    items: ItemRepository,
    account: MailboxAccount,
}

impl BudgetContext {
    async fn new(gateway: Arc<dyn ImapGateway>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let accounts = MailboxAccountRepository::new(pool.clone());
        let account = accounts
            .insert(NewMailboxAccount {
                provider: MailboxProvider::Gmail,
                email: "budget@example.com".to_owned(),
                imap_host: "imap.gmail.com".to_owned(),
                imap_port: 993,
                enabled: true,
                sync_interval_minutes: 15,
            })
            .await
            .unwrap();
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let items = ItemRepository::new(pool);
        let service = super::SyncService::new(
            gateway,
            Arc::new(MemoryCredentialStore::default()),
            accounts,
            ImportService::new(items.clone(), paths),
            RecognitionService::new(items.clone(), Arc::new(BudgetExtractor)),
        );
        Self {
            _directory: directory,
            service,
            items,
            account,
        }
    }
}

fn rejected_message(uid: u32) -> RejectedMessage {
    RejectedMessage {
        uid,
        mailbox: "INBOX".to_owned(),
        received_at: Utc::now(),
        source_received_date: Utc::now().date_naive(),
        reason: MessageRejectionReason::MessageTooLarge,
    }
}

fn rejection_delta(uid: u32) -> MailboxDelta {
    MailboxDelta {
        uid_validity: 1,
        messages: Vec::new(),
        rejected_messages: vec![rejected_message(uid)],
        highest_uid: uid,
    }
}

fn zip_with_pdf_entries(prefix: &str, count: usize, contents: &[u8]) -> Vec<u8> {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for index in 0..count {
        archive
            .start_file(
                format!("{prefix}-{index}.pdf"),
                SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(contents).unwrap();
    }
    archive.finish().unwrap().into_inner()
}

fn raw_message_with_zip_attachments(attachments: &[(&str, &[u8])]) -> RawMessage {
    let boundary = "aggregate-zip-budget-boundary";
    let mut message = format!(
        "From: billing@example.com\r\n\
             To: finance@example.com\r\n\
             Message-ID: <aggregate-zip@example.com>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\
             \r\n"
    )
    .into_bytes();
    for (file_name, bytes) in attachments {
        message.extend_from_slice(
            format!(
                "--{boundary}\r\n\
                     Content-Type: application/zip; name=\"{file_name}\"\r\n\
                     Content-Disposition: attachment; filename=\"{file_name}\"\r\n\
                     Content-Transfer-Encoding: binary\r\n\
                     \r\n"
            )
            .as_bytes(),
        );
        message.extend_from_slice(bytes);
        message.extend_from_slice(b"\r\n");
    }
    message.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    RawMessage {
        uid: 9,
        mailbox: "INBOX".to_owned(),
        raw: message,
        received_at: Utc::now(),
        source_received_date: Utc::now().date_naive(),
    }
}

fn eocd_offset(bytes: &[u8]) -> usize {
    bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .unwrap()
}

fn set_eocd_entry_count(bytes: &mut [u8], count: u16) {
    let eocd = eocd_offset(bytes);
    bytes[eocd + 8..eocd + 10].copy_from_slice(&count.to_le_bytes());
    bytes[eocd + 10..eocd + 12].copy_from_slice(&count.to_le_bytes());
}

fn append_fake_eocd(bytes: &mut Vec<u8>, count: u16) {
    let mut eocd = [0_u8; 22];
    eocd[..4].copy_from_slice(b"PK\x05\x06");
    eocd[8..10].copy_from_slice(&count.to_le_bytes());
    eocd[10..12].copy_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&eocd);
}

#[test]
fn mislabeled_text_pdf_preserves_binary_bytes() {
    let expected = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n1 0 obj\n<<>>\nendobj\n%%EOF\n";
    let raw = RawMessage {
        uid: 5454,
        mailbox: "INBOX".to_owned(),
        raw: b"From: billing@example.com\r\n\
            To: finance@example.com\r\n\
            Message-ID: <mislabeled-pdf@example.com>\r\n\
            MIME-Version: 1.0\r\n\
            Content-Type: multipart/mixed; boundary=invoice-boundary\r\n\
            \r\n\
            --invoice-boundary\r\n\
            Content-Type: text/plain\r\n\
            Content-Disposition: attachment; filename=invoice.pdf\r\n\
            Content-Transfer-Encoding: base64\r\n\
            \r\n\
            JVBERi0xLjcKJeLjz9MKMSAwIG9iago8PD4+CmVuZG9iagolJUVPRgo=\r\n\
            --invoice-boundary--\r\n"
            .to_vec(),
        received_at: Utc::now(),
        source_received_date: Utc::now().date_naive(),
    };

    let parsed = parse_invoice_parts(&raw).unwrap();

    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.files[0].file_name, "invoice.pdf");
    assert_eq!(parsed.files[0].bytes, expected);
}

#[test]
fn rejected_messages_count_toward_the_delta_limit() {
    let rejected_messages = (1..=MAX_MESSAGES_PER_SYNC as u32)
        .map(rejected_message)
        .collect();
    let delta = MailboxDelta {
        uid_validity: 1,
        messages: vec![RawMessage {
            uid: MAX_MESSAGES_PER_SYNC as u32 + 1,
            mailbox: "INBOX".to_owned(),
            raw: Vec::new(),
            received_at: Utc::now(),
            source_received_date: Utc::now().date_naive(),
        }],
        rejected_messages,
        highest_uid: MAX_MESSAGES_PER_SYNC as u32 + 1,
    };

    let error = validate_delta(&delta, "INBOX").unwrap_err();

    assert!(error.to_string().contains("too many messages"));
}

#[test]
fn touched_item_limit_preserves_progress_completed_before_overflow() {
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();
    let mut completed = SyncProgress {
        imported_count: 1,
        touched_item_ids: [first_id].into_iter().collect(),
    };
    let before_overflow = completed.clone();
    let next = SyncProgress {
        imported_count: 1,
        touched_item_ids: [second_id].into_iter().collect(),
    };

    let error = completed.merge_with_limit(next, 1).unwrap_err();

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: true,
            ref message,
        } if service == "imap" && message.contains("narrower date range")
    ));
    assert_eq!(completed, before_overflow);
}

#[test]
fn touched_item_limit_preserves_page_progress_before_record_overflow() {
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();
    let mut completed = SyncProgress {
        imported_count: 1,
        touched_item_ids: [first_id].into_iter().collect(),
    };
    let before_overflow = completed.clone();

    let error = completed
        .record_item_with_limit(second_id, true, 1)
        .unwrap_err();

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: true,
            ref message,
        } if service == "imap" && message.contains("narrower date range")
    ));
    assert_eq!(completed, before_overflow);
}

#[tokio::test]
async fn touched_item_budget_stops_same_page_import_before_second_side_effect() {
    let context = BudgetContext::new(Arc::new(BudgetGateway::new(Vec::new()))).await;
    let mut delta = rejection_delta(1);
    delta.rejected_messages.push(rejected_message(2));
    delta.highest_uid = 2;
    let mut budget = TouchedItemBudget::new(1);

    let failure = context
        .service
        .process_delta(context.account.id, &delta, true, &mut budget)
        .await
        .unwrap_err();

    assert!(matches!(
        failure.error,
        AppError::External {
            ref service,
            retryable: true,
            ref message,
        } if service == "imap" && message.contains("narrower date range")
    ));
    assert_eq!(failure.completed.imported_count, 1);
    assert_eq!(failure.completed.touched_item_ids.len(), 1);
    assert_eq!(
        context
            .items
            .list_bounded_for_tests(ItemFilter::default())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn touched_item_budget_carries_remaining_capacity_across_pages() {
    let gateway = Arc::new(BudgetGateway::new(vec![
        Ok(rejection_delta(1)),
        Ok(rejection_delta(2)),
    ]));
    let context = BudgetContext::new(gateway.clone()).await;
    let config = ImapAccountConfig::from_account(&context.account);
    let range = ImapDateRange::new(
        NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(),
        NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
    )
    .unwrap();
    let mut budget = TouchedItemBudget::new(1);

    let failure = context
        .service
        .run_range_deltas(context.account.id, &config, "secret", range, &mut budget)
        .await
        .unwrap_err();

    assert!(matches!(
        failure.error,
        AppError::External {
            retryable: true,
            ref message,
            ..
        } if message.contains("narrower date range")
    ));
    assert_eq!(failure.completed.imported_count, 1);
    assert_eq!(failure.completed.touched_item_ids.len(), 1);
    assert_eq!(gateway.range_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        context
            .items
            .list_bounded_for_tests(ItemFilter::default())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn zip_attachments_expand_supported_invoice_files() {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    archive
        .start_file("folder/invoice.pdf", SimpleFileOptions::default())
        .unwrap();
    archive.write_all(b"%PDF-invoice").unwrap();
    archive
        .start_file("notes/readme.txt", SimpleFileOptions::default())
        .unwrap();
    archive.write_all(b"not an invoice").unwrap();
    let bytes = archive.finish().unwrap().into_inner();
    let part = InvoicePart {
        part_id: "4".to_owned(),
        file_name: "invoices.zip".to_owned(),
        bytes,
        message_id: Some("zip@example.com".to_owned()),
    };

    let mut budget = ZipExpansionBudget::default();
    let expanded = expand_zip_part(&part, &mut budget).unwrap();

    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0].part_id, "4.zip.0");
    assert_eq!(expanded[0].file_name, "invoice.pdf");
    assert_eq!(expanded[0].bytes, b"%PDF-invoice");
    assert_eq!(expanded[0].message_id, part.message_id);
}

#[test]
fn parsed_zip_attachment_keeps_only_successfully_expanded_invoice_files() {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    archive
        .start_file("folder/invoice.pdf", SimpleFileOptions::default())
        .unwrap();
    archive.write_all(b"%PDF-invoice").unwrap();
    let zip_bytes = archive.finish().unwrap().into_inner();
    let boundary = "invoice-parts-boundary";
    let mut message = format!(
        "From: billing@example.com\r\n\
             To: finance@example.com\r\n\
             Message-ID: <zip@example.com>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\
             \r\n\
             --{boundary}\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Invoice attached.\r\n\
             --{boundary}\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <p>Invoice attached.</p>\r\n\
             --{boundary}\r\n\
             Content-Type: application/octet-stream\r\n\
             \r\n\
             metadata\r\n\
             --{boundary}\r\n\
             Content-Type: application/zip; name=\"invoices.zip\"\r\n\
             Content-Disposition: attachment; filename=\"invoices.zip\"\r\n\
             Content-Transfer-Encoding: binary\r\n\
             \r\n"
    )
    .into_bytes();
    message.extend_from_slice(&zip_bytes);
    message.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let raw = RawMessage {
        uid: 4,
        mailbox: "INBOX".to_owned(),
        raw: message,
        received_at: Utc::now(),
        source_received_date: Utc::now().date_naive(),
    };

    let parsed = parse_invoice_parts(&raw).unwrap();

    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.files[0].part_id, "4.zip.0");
    assert_eq!(parsed.files[0].file_name, "invoice.pdf");
    assert_eq!(parsed.files[0].bytes, b"%PDF-invoice");
    assert_eq!(
        parsed.files[0].message_id.as_deref(),
        Some("zip@example.com")
    );
}

#[test]
fn zip_entry_budget_is_shared_across_message_attachments() {
    let first_zip = zip_with_pdf_entries("first", 33, b"a");
    let second_zip = zip_with_pdf_entries("second", 33, b"b");
    let raw =
        raw_message_with_zip_attachments(&[("first.zip", &first_zip), ("second.zip", &second_zip)]);

    let parsed = parse_invoice_parts(&raw).unwrap();
    let originals = parsed
        .files
        .iter()
        .filter(|file| file.file_name.ends_with(".zip"))
        .map(|file| file.file_name.as_str())
        .collect::<Vec<_>>();
    let expanded = parsed
        .files
        .iter()
        .filter(|file| file.file_name.ends_with(".pdf"))
        .collect::<Vec<_>>();

    assert_eq!(originals, vec!["second.zip"]);
    assert_eq!(expanded.len(), 33);
    assert!(
        expanded
            .iter()
            .all(|file| file.file_name.starts_with("first-"))
    );
}

#[test]
fn zip_byte_budget_is_shared_across_message_attachments() {
    let first_zip = zip_with_pdf_entries("first", 1, b"aa");
    let second_zip = zip_with_pdf_entries("second", 1, b"bb");
    let raw =
        raw_message_with_zip_attachments(&[("first.zip", &first_zip), ("second.zip", &second_zip)]);

    let parsed = parse_invoice_parts_with_zip_budget(
        &raw,
        ZipExpansionBudget {
            remaining_entries: 2,
            remaining_bytes: 3,
        },
    )
    .unwrap();
    let originals = parsed
        .files
        .iter()
        .filter(|file| file.file_name.ends_with(".zip"))
        .map(|file| file.file_name.as_str())
        .collect::<Vec<_>>();
    let expanded = parsed
        .files
        .iter()
        .filter(|file| file.file_name.ends_with(".pdf"))
        .collect::<Vec<_>>();

    assert_eq!(originals, vec!["second.zip"]);
    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0].file_name, "first-0.pdf");
}

#[test]
fn zip64_entry_metadata_is_rejected_while_originals_are_preserved() {
    let mut sentinel_zip = zip_with_pdf_entries("sentinel", 1, b"pdf");
    set_eocd_entry_count(&mut sentinel_zip, u16::MAX);
    let mut locator_zip = zip_with_pdf_entries("locator", 1, b"pdf");
    let eocd = eocd_offset(&locator_zip);
    let mut locator = [0_u8; 20];
    locator[..4].copy_from_slice(b"PK\x06\x07");
    locator_zip.splice(eocd..eocd, locator);

    assert!(preflight_zip_entries(&sentinel_zip).is_err());
    assert!(preflight_zip_entries(&locator_zip).is_err());
    let raw = raw_message_with_zip_attachments(&[
        ("sentinel.zip", &sentinel_zip),
        ("locator.zip", &locator_zip),
    ]);

    let parsed = parse_invoice_parts(&raw).unwrap();

    assert_eq!(parsed.files.len(), 2);
    assert_eq!(parsed.files[0].file_name, "sentinel.zip");
    assert_eq!(parsed.files[0].bytes, sentinel_zip);
    assert_eq!(parsed.files[1].file_name, "locator.zip");
    assert_eq!(parsed.files[1].bytes, locator_zip);
}

#[test]
fn zip_entry_budget_is_reserved_before_archive_parser_runs() {
    let first_zip = zip_with_pdf_entries("first", 1, b"pdf");
    let mut second_zip = zip_with_pdf_entries("second", 1, b"pdf");
    set_eocd_entry_count(&mut second_zip, 2);
    let central_directory = second_zip
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .unwrap();
    second_zip[central_directory] = b'X';
    let first = InvoicePart {
        part_id: "1".to_owned(),
        file_name: "first.zip".to_owned(),
        bytes: first_zip,
        message_id: None,
    };
    let second = InvoicePart {
        part_id: "2".to_owned(),
        file_name: "second.zip".to_owned(),
        bytes: second_zip,
        message_id: None,
    };
    let mut budget = ZipExpansionBudget {
        remaining_entries: 2,
        remaining_bytes: 10,
    };

    assert!(expand_zip_part(&first, &mut budget).is_ok());
    assert_eq!(budget.remaining_entries, 1);
    assert!(expand_zip_part(&second, &mut budget).is_err());
    assert_eq!(budget.remaining_entries, 0);
}

#[test]
fn ambiguous_classic_eocd_is_rejected_before_budget_reservation() {
    let mut ambiguous_zip = zip_with_pdf_entries("invoice", 1, b"pdf");
    append_fake_eocd(&mut ambiguous_zip, 1);

    assert!(preflight_zip_entries(&ambiguous_zip).is_err());
    let part = InvoicePart {
        part_id: "1".to_owned(),
        file_name: "ambiguous.zip".to_owned(),
        bytes: ambiguous_zip.clone(),
        message_id: None,
    };
    let mut budget = ZipExpansionBudget::default();
    assert!(expand_zip_part(&part, &mut budget).is_err());
    assert_eq!(budget.remaining_entries, 64);
    let raw = raw_message_with_zip_attachments(&[("ambiguous.zip", &ambiguous_zip)]);

    let parsed = parse_invoice_parts(&raw).unwrap();

    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.files[0].file_name, "ambiguous.zip");
    assert_eq!(parsed.files[0].bytes, ambiguous_zip);
}

#[test]
fn zip64_signatures_are_rejected_at_any_archive_offset() {
    let zip64_eocd = zip_with_pdf_entries("beforePK\x06\x06after", 1, b"pdf");
    let zip64_locator = zip_with_pdf_entries("beforePK\x06\x07after", 1, b"pdf");
    assert!(zip64_eocd.windows(4).any(|window| window == b"PK\x06\x06"));
    assert!(
        zip64_locator
            .windows(4)
            .any(|window| window == b"PK\x06\x07")
    );

    assert!(preflight_zip_entries(&zip64_eocd).is_err());
    assert!(preflight_zip_entries(&zip64_locator).is_err());
}

#[test]
fn rejected_message_uids_must_be_unique_across_the_delta() {
    let delta = MailboxDelta {
        uid_validity: 1,
        messages: vec![RawMessage {
            uid: 7,
            mailbox: "INBOX".to_owned(),
            raw: Vec::new(),
            received_at: Utc::now(),
            source_received_date: Utc::now().date_naive(),
        }],
        rejected_messages: vec![rejected_message(7)],
        highest_uid: 7,
    };

    let error = validate_delta(&delta, "INBOX").unwrap_err();

    assert!(error.to_string().contains("invalid or duplicate UID"));
}

#[test]
fn rejected_messages_must_match_the_mailbox_and_high_water() {
    let mut wrong_mailbox = rejected_message(7);
    wrong_mailbox.mailbox = "Archive".to_owned();
    let mailbox_delta = MailboxDelta {
        uid_validity: 1,
        messages: vec![],
        rejected_messages: vec![wrong_mailbox],
        highest_uid: 7,
    };
    let uid_delta = MailboxDelta {
        uid_validity: 1,
        messages: vec![],
        rejected_messages: vec![rejected_message(8)],
        highest_uid: 7,
    };

    let mailbox_error = validate_delta(&mailbox_delta, "INBOX").unwrap_err();
    let uid_error = validate_delta(&uid_delta, "INBOX").unwrap_err();

    assert!(mailbox_error.to_string().contains("mailbox"));
    assert!(uid_error.to_string().contains("invalid or duplicate UID"));
}
