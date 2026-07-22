use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{NaiveDate, TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::accounts::{
    MailboxAccount, MailboxAccountRepository, MailboxProvider, NewMailboxAccount, SyncCursor,
};
use invoice_reimbursement::db::items::{ItemFilter, ItemRepository};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::infra::credentials::{CredentialStore, MemoryCredentialStore};
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::infra::imap::{
    ImapAccountConfig, ImapDateRange, ImapGateway, MailboxDelta, RawMessage,
};
use invoice_reimbursement::services::batches::{BatchService, NewBatchInput};
use invoice_reimbursement::state::AppState;
use tokio::sync::Notify;

#[derive(Default)]
struct FakeRangeGateway {
    responses: Mutex<HashMap<String, VecDeque<Result<MailboxDelta, AppError>>>>,
    requests: Mutex<Vec<(String, Option<SyncCursor>, ImapDateRange)>>,
}

impl FakeRangeGateway {
    fn queue(&self, email: &str, responses: Vec<Result<MailboxDelta, AppError>>) {
        self.responses
            .lock()
            .unwrap()
            .insert(email.to_owned(), responses.into());
    }
}

#[async_trait]
impl ImapGateway for FakeRangeGateway {
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
        panic!("batch automation must not run an incremental mailbox fetch")
    }

    async fn fetch_range(
        &self,
        config: &ImapAccountConfig,
        _secret: &str,
        cursor: Option<SyncCursor>,
        range: ImapDateRange,
    ) -> Result<MailboxDelta, AppError> {
        self.requests
            .lock()
            .unwrap()
            .push((config.email.clone(), cursor, range));
        self.responses
            .lock()
            .unwrap()
            .get_mut(&config.email)
            .and_then(VecDeque::pop_front)
            .expect("fake range response should be queued")
    }
}

struct BlockingRangeGateway {
    calls: AtomicUsize,
    started: Notify,
    release: Notify,
}

impl Default for BlockingRangeGateway {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
        }
    }
}

#[async_trait]
impl ImapGateway for BlockingRangeGateway {
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
        panic!("batch automation must not run an incremental mailbox fetch")
    }

    async fn fetch_range(
        &self,
        _config: &ImapAccountConfig,
        _secret: &str,
        _cursor: Option<SyncCursor>,
        _range: ImapDateRange,
    ) -> Result<MailboxDelta, AppError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(MailboxDelta {
            uid_validity: 1,
            messages: Vec::new(),
            rejected_messages: Vec::new(),
            highest_uid: 0,
        })
    }
}

struct SequenceExtractor {
    documents: Mutex<VecDeque<ExtractedDocument>>,
}

impl SequenceExtractor {
    fn new(texts: &[&str]) -> Self {
        Self {
            documents: Mutex::new(
                texts
                    .iter()
                    .map(|text| ExtractedDocument {
                        text: (*text).to_owned(),
                        normalized_pdf: Some(include_bytes!("fixtures/text-invoice.pdf").to_vec()),
                        warnings: Vec::new(),
                    })
                    .collect(),
            ),
        }
    }
}

impl DocumentExtractor for SequenceExtractor {
    fn extract(&self, _path: &Path) -> Result<ExtractedDocument, AppError> {
        Ok(self
            .documents
            .lock()
            .unwrap()
            .pop_front()
            .expect("deterministic extraction should be queued"))
    }
}

struct Harness {
    _directory: tempfile::TempDir,
    pool: sqlx::SqlitePool,
    state: AppState,
    accounts: MailboxAccountRepository,
    credentials: Arc<MemoryCredentialStore>,
}

impl Harness {
    async fn new(gateway: Arc<dyn ImapGateway>, extractor: Arc<dyn DocumentExtractor>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let credentials = Arc::new(MemoryCredentialStore::default());
        let state = AppState::with_gateway_and_extractor(
            pool.clone(),
            paths,
            credentials.clone(),
            gateway,
            extractor,
        );
        Self {
            _directory: directory,
            accounts: MailboxAccountRepository::new(pool.clone()),
            pool,
            state,
            credentials,
        }
    }

    async fn add_account(&self, email: &str, enabled: bool) -> MailboxAccount {
        let account = self
            .accounts
            .insert(NewMailboxAccount {
                provider: MailboxProvider::Gmail,
                email: email.to_owned(),
                imap_host: "imap.gmail.com".to_owned(),
                imap_port: 993,
                enabled,
                sync_interval_minutes: 15,
            })
            .await
            .unwrap();
        self.credentials
            .set(&account.id.to_string(), &format!("secret-{email}"))
            .unwrap();
        account
    }

    async fn may_batch(&self) -> invoice_reimbursement::db::batches::Batch {
        BatchService::new(self.pool.clone())
            .create_month(2026, 5)
            .await
            .unwrap()
    }
}

const SAFE_TEXT: &str = "开票日期：2026年05月10日 餐饮服务 价税合计 ¥128.50";
const INCOMPLETE_TEXT: &str = "电子票据";
const OUTSIDE_TEXT: &str = "开票日期：2026年04月30日 餐饮服务 价税合计 ¥50.00";

#[tokio::test]
async fn imports_assigns_exceptions_and_exports_across_enabled_accounts() {
    let gateway = Arc::new(FakeRangeGateway::default());
    gateway.queue(
        "a@example.com",
        completed_scan(vec![
            raw_message(1, include_bytes!("fixtures/mail/attachment.eml")),
            raw_message(2, include_bytes!("fixtures/mail/inline-image.eml")),
        ]),
    );
    gateway.queue(
        "b@example.com",
        completed_scan(vec![raw_message(
            1,
            include_bytes!("fixtures/mail/attachment.eml"),
        )]),
    );
    let harness = Harness::new(
        gateway,
        Arc::new(SequenceExtractor::new(&[
            SAFE_TEXT,
            INCOMPLETE_TEXT,
            SAFE_TEXT,
        ])),
    )
    .await;
    harness.add_account("a@example.com", true).await;
    harness.add_account("b@example.com", true).await;
    let batch = harness.may_batch().await;

    let result = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap();

    assert_eq!(result.scanned_account_count, 2);
    assert_eq!(result.failed_accounts, vec![]);
    assert_eq!(result.imported_count, 3);
    assert_eq!(result.assigned_count, 1);
    assert_eq!(result.exception_count, 2);
    let export = result.export.expect("one safe invoice must be exported");
    assert_eq!(export.item_count, 1);
    for name in [
        "merged.pdf",
        "reimbursement.xlsx",
        "originals.zip",
        "manifest.json",
    ] {
        assert!(export.directory.join(name).is_file(), "missing {name}");
    }
}

#[tokio::test]
async fn one_account_failure_does_not_block_successful_accounts_or_export() {
    let gateway = Arc::new(FakeRangeGateway::default());
    gateway.queue(
        "a@example.com",
        vec![Err(AppError::External {
            service: "imap".to_owned(),
            retryable: true,
            message: "failed with secret-a@example.com\nand control".to_owned(),
        })],
    );
    gateway.queue(
        "b@example.com",
        completed_scan(vec![raw_message(
            1,
            include_bytes!("fixtures/mail/attachment.eml"),
        )]),
    );
    let harness = Harness::new(gateway, Arc::new(SequenceExtractor::new(&[SAFE_TEXT]))).await;
    let failed = harness.add_account("a@example.com", true).await;
    harness.add_account("b@example.com", true).await;
    let batch = harness.may_batch().await;

    let result = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap();

    assert_eq!(result.scanned_account_count, 2);
    assert_eq!(result.failed_accounts.len(), 1);
    assert_eq!(result.failed_accounts[0].account_id, failed.id);
    assert_eq!(result.failed_accounts[0].email, failed.email);
    assert!(
        !result.failed_accounts[0]
            .message
            .contains("secret-a@example.com")
    );
    assert!(!result.failed_accounts[0].message.contains('\n'));
    assert_eq!(result.imported_count, 1);
    assert_eq!(result.assigned_count, 1);
    assert_eq!(result.exception_count, 0);
    assert!(result.export.is_some());
}

#[tokio::test]
async fn no_enabled_mailbox_account_is_a_stable_configuration_conflict() {
    let harness = Harness::new(
        Arc::new(FakeRangeGateway::default()),
        Arc::new(SequenceExtractor::new(&[])),
    )
    .await;
    harness.add_account("disabled@example.com", false).await;
    let batch = harness.may_batch().await;

    let error = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap_err();

    assert_eq!(
        error,
        AppError::Conflict {
            message: "no enabled mailbox accounts are configured".to_owned(),
        }
    );
}

#[tokio::test]
async fn concurrent_runs_for_the_same_batch_are_rejected_before_account_work() {
    let gateway = Arc::new(BlockingRangeGateway::default());
    let harness = Harness::new(gateway.clone(), Arc::new(SequenceExtractor::new(&[]))).await;
    harness.add_account("a@example.com", true).await;
    let batch = harness.may_batch().await;
    let first_service = harness.state.batch_automation_service();
    let first = tokio::spawn(async move { first_service.run(batch.id).await });
    gateway.started.notified().await;

    let error = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::Conflict { .. }));
    assert_eq!(gateway.calls.load(Ordering::SeqCst), 1);
    gateway.release.notify_one();
    let first_result = first.await.unwrap().unwrap();
    assert_eq!(first_result.imported_count, 0);
    assert_eq!(first_result.assigned_count, 0);
    assert!(first_result.export.is_none());
}

#[tokio::test]
async fn rerun_does_not_duplicate_rows_or_assignments() {
    let gateway = Arc::new(FakeRangeGateway::default());
    let mut responses = completed_scan(vec![raw_message(
        1,
        include_bytes!("fixtures/mail/attachment.eml"),
    )]);
    responses.extend(completed_scan(vec![raw_message(
        1,
        include_bytes!("fixtures/mail/attachment.eml"),
    )]));
    gateway.queue("a@example.com", responses);
    let harness = Harness::new(gateway, Arc::new(SequenceExtractor::new(&[SAFE_TEXT]))).await;
    harness.add_account("a@example.com", true).await;
    let batch = harness.may_batch().await;
    let service = harness.state.batch_automation_service();

    let first = service.run(batch.id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let second = service.run(batch.id).await.unwrap();

    assert_eq!(first.imported_count, 1);
    assert_eq!(first.assigned_count, 1);
    assert_eq!(second.imported_count, 0);
    assert_eq!(second.assigned_count, 0);
    assert_eq!(second.exception_count, 0);
    assert!(second.export.is_some());
    assert_eq!(
        ItemRepository::new(harness.pool)
            .list_bounded_for_tests(ItemFilter::default())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn inclusive_range_longer_than_366_days_is_rejected() {
    let harness = Harness::new(
        Arc::new(FakeRangeGateway::default()),
        Arc::new(SequenceExtractor::new(&[])),
    )
    .await;
    let batch = BatchService::new(harness.pool.clone())
        .create(NewBatchInput {
            name: "oversized".to_owned(),
            start_date: "2025-01-01".to_owned(),
            end_date: "2026-01-02".to_owned(),
            note: None,
        })
        .await
        .unwrap();

    let error = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::Validation { ref field, .. } if field == "dateRange"));
}

#[tokio::test]
async fn all_failed_accounts_with_no_items_return_failures_without_export() {
    let gateway = Arc::new(FakeRangeGateway::default());
    gateway.queue(
        "a@example.com",
        vec![Err(AppError::External {
            service: "imap".to_owned(),
            retryable: true,
            message: "range scan failed".to_owned(),
        })],
    );
    let harness = Harness::new(gateway, Arc::new(SequenceExtractor::new(&[]))).await;
    let account = harness.add_account("a@example.com", true).await;
    let batch = BatchService::new(harness.pool.clone())
        .create(NewBatchInput {
            name: "maximum date".to_owned(),
            start_date: "9999-12-31".to_owned(),
            end_date: "9999-12-31".to_owned(),
            note: None,
        })
        .await
        .unwrap();

    let result = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap();

    assert_eq!(result.scanned_account_count, 1);
    assert_eq!(result.failed_accounts.len(), 1);
    assert_eq!(result.failed_accounts[0].account_id, account.id);
    assert_eq!(result.imported_count, 0);
    assert_eq!(result.assigned_count, 0);
    assert_eq!(result.exception_count, 0);
    assert!(result.export.is_none());
}

#[tokio::test]
async fn missing_and_outside_invoice_dates_are_exceptions_but_are_not_assigned() {
    let gateway = Arc::new(FakeRangeGateway::default());
    gateway.queue(
        "a@example.com",
        completed_scan(vec![
            raw_message(1, include_bytes!("fixtures/mail/attachment.eml")),
            raw_message(2, include_bytes!("fixtures/mail/inline-image.eml")),
        ]),
    );
    let harness = Harness::new(
        gateway,
        Arc::new(SequenceExtractor::new(&[INCOMPLETE_TEXT, OUTSIDE_TEXT])),
    )
    .await;
    harness.add_account("a@example.com", true).await;
    let batch = harness.may_batch().await;

    let result = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap();

    assert_eq!(result.imported_count, 2);
    assert_eq!(result.assigned_count, 0);
    assert_eq!(result.exception_count, 2);
    assert!(result.export.is_none());
    let items = ItemRepository::new(harness.pool)
        .list_bounded_for_tests(ItemFilter::default())
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|item| item.batch_id.is_none()));
    assert!(items.iter().any(|item| item.invoice_date.is_none()));
    assert!(
        items.iter().any(|item| {
            item.invoice_date == Some(NaiveDate::from_ymd_opt(2026, 4, 30).unwrap())
        })
    );
}

fn completed_scan(messages: Vec<RawMessage>) -> Vec<Result<MailboxDelta, AppError>> {
    let highest_uid = messages
        .iter()
        .map(|message| message.uid)
        .max()
        .unwrap_or(0);
    vec![
        Ok(MailboxDelta {
            uid_validity: 1,
            messages,
            rejected_messages: Vec::new(),
            highest_uid,
        }),
        Ok(MailboxDelta {
            uid_validity: 1,
            messages: Vec::new(),
            rejected_messages: Vec::new(),
            highest_uid,
        }),
    ]
}

fn raw_message(uid: u32, raw: &[u8]) -> RawMessage {
    RawMessage {
        uid,
        mailbox: "INBOX".to_owned(),
        raw: raw.to_vec(),
        received_at: Utc.with_ymd_and_hms(2026, 5, 15, 8, 0, 0).single().unwrap(),
    }
}
