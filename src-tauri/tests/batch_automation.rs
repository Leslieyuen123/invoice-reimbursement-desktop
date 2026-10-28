use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{NaiveDate, TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::accounts::{
    MailboxAccount, MailboxAccountRepository, MailboxProvider, NewMailboxAccount, SyncCursor,
};
use invoice_reimbursement::db::items::{ItemFilter, ItemRepository, NewItemRecord};
use invoice_reimbursement::domain::amount::MAX_SAFE_AMOUNT_CENTS;
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, RecognitionStatus, SourceType,
};
use invoice_reimbursement::infra::credentials::{CredentialStore, MemoryCredentialStore};
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::infra::imap::{
    ImapAccountConfig, ImapDateRange, ImapGateway, MailboxDelta, RawMessage,
};
use invoice_reimbursement::services::batches::{BatchService, NewBatchInput};
use invoice_reimbursement::state::AppState;
use sha2::{Digest, Sha256};
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
    paths: AppPaths,
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
            paths.clone(),
            credentials.clone(),
            gateway,
            extractor,
        );
        Self {
            _directory: directory,
            accounts: MailboxAccountRepository::new(pool.clone()),
            pool,
            paths,
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
async fn missing_or_unmanaged_invoice_files_remain_exceptions_without_blocking_the_batch() {
    let gateway = Arc::new(FakeRangeGateway::default());
    gateway.queue("a@example.com", completed_scan(Vec::new()));
    let harness = Harness::new(gateway, Arc::new(SequenceExtractor::new(&[]))).await;
    harness.add_account("a@example.com", true).await;
    let batch = harness.may_batch().await;
    let repository = ItemRepository::new(harness.pool.clone());
    let missing_original_id = uuid::Uuid::new_v4();
    let mut missing_original = ready_item(&harness.paths, missing_original_id);
    fs::write(
        missing_original.normalized_pdf_path.as_deref().unwrap(),
        include_bytes!("fixtures/text-invoice.pdf"),
    )
    .unwrap();
    missing_original.original_path = harness
        .paths
        .originals
        .join("missing.pdf")
        .to_string_lossy()
        .into_owned();
    repository.insert(&missing_original).await.unwrap();

    let unmanaged_pdf_id = uuid::Uuid::new_v4();
    let mut unmanaged_pdf = ready_item(&harness.paths, unmanaged_pdf_id);
    fs::write(&unmanaged_pdf.original_path, b"managed original").unwrap();
    let outside_pdf = harness._directory.path().join("outside.pdf");
    fs::write(&outside_pdf, include_bytes!("fixtures/text-invoice.pdf")).unwrap();
    unmanaged_pdf.normalized_pdf_path = Some(outside_pdf.to_string_lossy().into_owned());
    repository.insert(&unmanaged_pdf).await.unwrap();

    let result = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap();

    assert_eq!(result.assigned_count, 0);
    assert_eq!(result.exception_count, 2);
    assert!(result.export.is_none());
    let detail = BatchService::new(harness.pool).get(batch.id).await.unwrap();
    assert!(detail.items.is_empty());
}

#[tokio::test]
async fn only_cny_candidates_with_valid_amounts_are_assigned_and_exported() {
    let gateway = Arc::new(FakeRangeGateway::default());
    gateway.queue("a@example.com", completed_scan(Vec::new()));
    let harness = Harness::new(gateway, Arc::new(SequenceExtractor::new(&[]))).await;
    harness.add_account("a@example.com", true).await;
    let batch = harness.may_batch().await;
    let repository = ItemRepository::new(harness.pool.clone());
    let valid_id = uuid::Uuid::new_v4();
    let foreign_currency_id = uuid::Uuid::new_v4();
    let negative_id = uuid::Uuid::new_v4();
    let excessive_id = uuid::Uuid::new_v4();

    for (id, currency) in [
        (valid_id, "CNY"),
        (foreign_currency_id, "USD"),
        (negative_id, "CNY"),
        (excessive_id, "CNY"),
    ] {
        let mut item = ready_item(&harness.paths, id);
        item.currency = currency.to_owned();
        let original_bytes = format!("managed original {id}");
        item.sha256 = format!("{:x}", Sha256::digest(original_bytes.as_bytes()));
        fs::write(&item.original_path, original_bytes).unwrap();
        fs::write(
            item.normalized_pdf_path.as_deref().unwrap(),
            include_bytes!("fixtures/text-invoice.pdf"),
        )
        .unwrap();
        repository.insert(&item).await.unwrap();
    }

    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&harness.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE items SET amount_cents = -1 WHERE id = ?")
        .bind(negative_id.to_string())
        .execute(&harness.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE items SET amount_cents = ? WHERE id = ?")
        .bind(MAX_SAFE_AMOUNT_CENTS + 1)
        .bind(excessive_id.to_string())
        .execute(&harness.pool)
        .await
        .unwrap();
    sqlx::query("PRAGMA ignore_check_constraints = OFF")
        .execute(&harness.pool)
        .await
        .unwrap();

    let result = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap();

    assert_eq!(result.assigned_count, 1);
    assert_eq!(result.exception_count, 3);
    assert_eq!(result.export.unwrap().item_count, 1);
    let items = repository
        .list_bounded_for_tests(ItemFilter::default())
        .await
        .unwrap();
    assert_eq!(
        items
            .iter()
            .find(|item| item.id == valid_id)
            .unwrap()
            .batch_id,
        Some(batch.id)
    );
    for id in [foreign_currency_id, negative_id, excessive_id] {
        assert!(
            items
                .iter()
                .find(|item| item.id == id)
                .unwrap()
                .batch_id
                .is_none()
        );
    }
}

#[tokio::test]
async fn candidate_pagination_classifies_rows_after_the_first_two_hundred() {
    let gateway = Arc::new(FakeRangeGateway::default());
    gateway.queue("a@example.com", completed_scan(Vec::new()));
    let harness = Harness::new(gateway, Arc::new(SequenceExtractor::new(&[]))).await;
    harness.add_account("a@example.com", true).await;
    let batch = harness.may_batch().await;
    let repository = ItemRepository::new(harness.pool.clone());
    for _ in 0..201 {
        repository
            .insert(&ready_item(&harness.paths, uuid::Uuid::new_v4()))
            .await
            .unwrap();
    }

    let result = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap();

    assert_eq!(result.assigned_count, 0);
    assert_eq!(result.exception_count, 201);
    assert!(result.export.is_none());
    assert!(
        BatchService::new(harness.pool)
            .get(batch.id)
            .await
            .unwrap()
            .items
            .is_empty()
    );
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
async fn all_failed_accounts_keep_partial_progress_without_assignment_or_export() {
    let gateway = Arc::new(FakeRangeGateway::default());
    gateway.queue(
        "a@example.com",
        vec![
            Ok(MailboxDelta {
                uid_validity: 1,
                messages: vec![raw_message(
                    1,
                    include_bytes!("fixtures/mail/attachment.eml"),
                )],
                rejected_messages: Vec::new(),
                highest_uid: 1,
            }),
            Err(AppError::External {
                service: "imap".to_owned(),
                retryable: true,
                message: "second page failed with secret-a@example.com".to_owned(),
            }),
        ],
    );
    let harness = Harness::new(
        gateway,
        Arc::new(SequenceExtractor::new(&[INCOMPLETE_TEXT])),
    )
    .await;
    let failed_account = harness.add_account("a@example.com", true).await;
    let batch = harness.may_batch().await;
    let safe_id = uuid::Uuid::new_v4();
    let mut safe_item = ready_item(&harness.paths, safe_id);
    let original_bytes = b"managed original";
    safe_item.sha256 = format!("{:x}", Sha256::digest(original_bytes));
    fs::write(&safe_item.original_path, original_bytes).unwrap();
    fs::write(
        safe_item.normalized_pdf_path.as_deref().unwrap(),
        include_bytes!("fixtures/text-invoice.pdf"),
    )
    .unwrap();
    ItemRepository::new(harness.pool.clone())
        .insert(&safe_item)
        .await
        .unwrap();

    let error = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap_err();

    assert_eq!(
        error,
        AppError::External {
            service: "mailbox".to_owned(),
            retryable: true,
            message: "所有已启用邮箱同步失败，请检查网络和邮箱授权后重试".to_owned(),
        }
    );
    let sync_run = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, error_message FROM sync_runs WHERE account_id = ?",
    )
    .bind(failed_account.id.to_string())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(sync_run.0, "failed");
    assert!(!sync_run.1.unwrap().contains("secret-a@example.com"));
    let items = ItemRepository::new(harness.pool.clone())
        .list_bounded_for_tests(ItemFilter::default())
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        items
            .iter()
            .find(|item| item.id == safe_id)
            .unwrap()
            .batch_id,
        None
    );
    assert!(
        items
            .iter()
            .find(|item| item.id != safe_id)
            .unwrap()
            .batch_id
            .is_none()
    );
    assert!(
        BatchService::new(harness.pool)
            .get(batch.id)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn later_message_failure_returns_error_and_keeps_completed_progress() {
    let gateway = Arc::new(FakeRangeGateway::default());
    gateway.queue(
        "a@example.com",
        vec![Ok(MailboxDelta {
            uid_validity: 1,
            messages: vec![
                raw_message(1, include_bytes!("fixtures/mail/attachment.eml")),
                raw_message(2, b""),
            ],
            rejected_messages: Vec::new(),
            highest_uid: 2,
        })],
    );
    let harness = Harness::new(
        gateway,
        Arc::new(SequenceExtractor::new(&[INCOMPLETE_TEXT])),
    )
    .await;
    let failed_account = harness.add_account("a@example.com", true).await;
    let batch = harness.may_batch().await;

    let error = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap_err();

    assert_eq!(
        error,
        AppError::External {
            service: "mailbox".to_owned(),
            retryable: true,
            message: "所有已启用邮箱同步失败，请检查网络和邮箱授权后重试".to_owned(),
        }
    );
    let sync_run = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, error_message FROM sync_runs WHERE account_id = ?",
    )
    .bind(failed_account.id.to_string())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(sync_run.0, "failed");
    assert!(sync_run.1.unwrap().contains("could not be parsed"));
    let items = ItemRepository::new(harness.pool.clone())
        .list_bounded_for_tests(ItemFilter::default())
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].batch_id.is_none());
    assert_eq!(items[0].confirmation_status, ConfirmationStatus::Pending);
    assert!(
        BatchService::new(harness.pool)
            .get(batch.id)
            .await
            .unwrap()
            .items
            .is_empty()
    );
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
            message: "range scan failed with secret-a@example.com\n\u{0000}control".to_owned(),
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

    let error = harness
        .state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap_err();

    assert_eq!(
        error,
        AppError::External {
            service: "mailbox".to_owned(),
            retryable: true,
            message: "所有已启用邮箱同步失败，请检查网络和邮箱授权后重试".to_owned(),
        }
    );
    assert!(!error.to_string().contains("secret-a@example.com"));
    assert!(!error.to_string().chars().any(char::is_control));
    let sync_run = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, error_message FROM sync_runs WHERE account_id = ?",
    )
    .bind(account.id.to_string())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(sync_run.0, "failed");
    let recorded_message = sync_run.1.unwrap();
    assert!(!recorded_message.contains("secret-a@example.com"));
    assert!(!recorded_message.chars().any(char::is_control));
    assert!(
        BatchService::new(harness.pool)
            .get(batch.id)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn email_received_in_batch_month_uses_received_date_for_automatic_assignment() {
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
    assert_eq!(result.assigned_count, 1);
    assert_eq!(result.exception_count, 1);
    assert_eq!(result.export.unwrap().item_count, 1);
    let items = ItemRepository::new(harness.pool)
        .list_bounded_for_tests(ItemFilter::default())
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    let incomplete = items
        .iter()
        .find(|item| item.invoice_date.is_none())
        .expect("incomplete item should remain visible");
    assert!(incomplete.batch_id.is_none());
    let april_invoice = items
        .iter()
        .find(|item| item.invoice_date == Some(NaiveDate::from_ymd_opt(2026, 4, 30).unwrap()))
        .expect("April-dated invoice should remain visible");
    assert_eq!(april_invoice.batch_id, Some(batch.id));
}

#[tokio::test]
async fn range_touched_existing_item_outside_utc_date_bounds_remains_an_exception() {
    let gateway = Arc::new(FakeRangeGateway::default());
    let mut message = raw_message(1, include_bytes!("fixtures/mail/attachment.eml"));
    message.received_at = Utc
        .with_ymd_and_hms(2026, 4, 30, 16, 30, 0)
        .single()
        .unwrap();
    let mut responses = completed_scan(vec![message.clone()]);
    responses.extend(completed_scan(vec![message]));
    gateway.queue("a@example.com", responses);
    let harness = Harness::new(
        gateway,
        Arc::new(SequenceExtractor::new(&[INCOMPLETE_TEXT])),
    )
    .await;
    harness.add_account("a@example.com", true).await;
    let batch = harness.may_batch().await;
    let service = harness.state.batch_automation_service();

    let first = service.run(batch.id).await.unwrap();
    let second = service.run(batch.id).await.unwrap();

    assert_eq!(first.imported_count, 1);
    assert_eq!(first.exception_count, 1);
    assert_eq!(second.imported_count, 0);
    assert_eq!(second.exception_count, 1);
    assert!(second.export.is_none());
}

#[tokio::test]
async fn touched_item_owned_by_another_batch_is_re_anchored_into_the_target_batch() {
    let gateway = Arc::new(FakeRangeGateway::default());
    let message = raw_message(1, include_bytes!("fixtures/mail/attachment.eml"));
    let mut responses = completed_scan(vec![message.clone()]);
    responses.extend(completed_scan(vec![message]));
    gateway.queue("a@example.com", responses);
    let harness = Harness::new(gateway, Arc::new(SequenceExtractor::new(&[SAFE_TEXT]))).await;
    harness.add_account("a@example.com", true).await;
    let original_batch = harness.may_batch().await;
    let target_batch = BatchService::new(harness.pool.clone())
        .create(NewBatchInput {
            name: "May reimbursement retry".to_owned(),
            start_date: "2026-05-01".to_owned(),
            end_date: "2026-05-31".to_owned(),
            note: None,
        })
        .await
        .unwrap();
    let service = harness.state.batch_automation_service();

    let first = service.run(original_batch.id).await.unwrap();
    let item_id = BatchService::new(harness.pool.clone())
        .get(original_batch.id)
        .await
        .unwrap()
        .items[0]
        .id;
    let second = service.run(target_batch.id).await.unwrap();

    assert_eq!(first.assigned_count, 1);
    assert_eq!(second.imported_count, 0);
    // Same range: the target batch re-anchors the invoice away from the
    // original batch instead of leaving it there.
    assert_eq!(second.assigned_count, 1);
    assert_eq!(second.moved_from_other_batches_count, 1);
    assert_eq!(second.exception_count, 0);
    assert_eq!(
        second.export.as_ref().map(|export| export.item_count),
        Some(1)
    );
    assert_eq!(
        ItemRepository::new(harness.pool.clone())
            .get_by_id(item_id)
            .await
            .unwrap()
            .batch_id,
        Some(target_batch.id)
    );
    let original_detail = BatchService::new(harness.pool)
        .get(original_batch.id)
        .await
        .unwrap();
    assert!(original_detail.items.is_empty());
    assert_eq!(
        original_detail.batch.status,
        invoice_reimbursement::domain::model::BatchStatus::Draft
    );
}

#[tokio::test]
async fn authoritative_rescan_moves_a_legacy_email_out_of_the_wrong_exported_month() {
    let gateway = Arc::new(FakeRangeGateway::default());
    let message = raw_message(1, include_bytes!("fixtures/mail/attachment.eml"));
    let mut responses = completed_scan(vec![message.clone()]);
    responses.extend(completed_scan(vec![message]));
    gateway.queue("a@example.com", responses);
    let harness = Harness::new(gateway, Arc::new(SequenceExtractor::new(&[SAFE_TEXT]))).await;
    harness.add_account("a@example.com", true).await;
    let may_batch = harness.may_batch().await;
    let april_batch = BatchService::new(harness.pool.clone())
        .create_month(2026, 4)
        .await
        .unwrap();
    let service = harness.state.batch_automation_service();

    let first = service.run(may_batch.id).await.unwrap();
    assert_eq!(first.assigned_count, 1);
    let item_id = BatchService::new(harness.pool.clone())
        .get(may_batch.id)
        .await
        .unwrap()
        .items[0]
        .id;
    BatchService::new(harness.pool.clone())
        .assign_items(april_batch.id, &[item_id])
        .await
        .unwrap();
    sqlx::query(
        "UPDATE batches SET status = 'exported', last_exported_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind("2026-05-31T16:00:00Z")
    .bind("2026-05-31T16:00:00Z")
    .bind(april_batch.id.to_string())
    .execute(&harness.pool)
    .await
    .unwrap();

    let second = service.run(may_batch.id).await.unwrap();
    assert_eq!(second.imported_count, 0);
    assert_eq!(second.assigned_count, 1);
    assert_eq!(second.exception_count, 0);
    assert_eq!(
        ItemRepository::new(harness.pool.clone())
            .get_by_id(item_id)
            .await
            .unwrap()
            .batch_id,
        Some(may_batch.id)
    );
    let repaired_old_batch = BatchService::new(harness.pool)
        .get(april_batch.id)
        .await
        .unwrap()
        .batch;
    assert_eq!(
        repaired_old_batch.status,
        invoice_reimbursement::domain::model::BatchStatus::Draft
    );
    assert!(repaired_old_batch.last_exported_at.is_some());
}

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("test date should be valid")
}

async fn create_range_batch(
    harness: &Harness,
    name: &str,
    start_date: &str,
    end_date: &str,
) -> invoice_reimbursement::db::batches::Batch {
    BatchService::new(harness.pool.clone())
        .create(NewBatchInput {
            name: name.to_owned(),
            start_date: start_date.to_owned(),
            end_date: end_date.to_owned(),
            note: None,
        })
        .await
        .expect("range batch should create")
}

/// A batch-ready invoice with real managed files, so safety checks pass.
fn managed_item(
    paths: &AppPaths,
    id: uuid::Uuid,
    invoice_date: NaiveDate,
    amount_cents: i64,
) -> NewItemRecord {
    let mut item = ready_item(paths, id);
    let original_bytes = format!("managed original {id}");
    item.sha256 = format!("{:x}", Sha256::digest(original_bytes.as_bytes()));
    fs::write(&item.original_path, original_bytes.as_bytes()).unwrap();
    fs::write(
        item.normalized_pdf_path.as_deref().unwrap(),
        include_bytes!("fixtures/text-invoice.pdf"),
    )
    .unwrap();
    item.invoice_date = Some(invoice_date);
    item.suggested_period = Some(invoice_date.format("%Y-%m").to_string());
    item.amount_cents = Some(amount_cents);
    item
}

#[tokio::test]
async fn widening_the_range_re_anchors_older_batch_members_and_leaves_outside_invoices_alone() {
    let email = "reanchor@example.com";
    let gateway = Arc::new(FakeRangeGateway::default());
    gateway.queue(email, completed_scan(Vec::new()));
    let harness = Harness::new(gateway, Arc::new(SequenceExtractor::new(&[]))).await;
    let account = harness.add_account(email, true).await;
    let service = BatchService::new(harness.pool.clone());
    let narrow = create_range_batch(&harness, "5 月中段", "2026-05-14", "2026-05-20").await;
    let outside = create_range_batch(&harness, "5 月初", "2026-05-01", "2026-05-03").await;
    let wide = create_range_batch(&harness, "5 月重新锚定", "2026-05-04", "2026-05-20").await;
    let repository = ItemRepository::new(harness.pool.clone());
    let first_id = uuid::Uuid::new_v4();
    let second_id = uuid::Uuid::new_v4();
    let outside_id = uuid::Uuid::new_v4();
    for (id, day, amount) in [
        (first_id, 15_u32, 12_850_i64),
        (second_id, 18, 5_000),
        (outside_id, 2, 7_777),
    ] {
        repository
            .insert(&managed_item(
                &harness.paths,
                id,
                date(2026, 5, day),
                amount,
            ))
            .await
            .unwrap();
    }
    // A mail invoice is anchored by its received date, not its invoice date:
    // it was received inside the range although the extracted date is not.
    let email_id = uuid::Uuid::new_v4();
    let mut email_invoice = managed_item(&harness.paths, email_id, date(2026, 4, 30), 2_000);
    email_invoice.source_type = SourceType::Email;
    email_invoice.source_account_id = Some(account.id);
    email_invoice.source_mailbox = Some("INBOX".to_owned());
    email_invoice.source_uid_validity = Some(1);
    email_invoice.source_uid = Some(77);
    email_invoice.source_message_id = Some("<reanchor-email@example.com>".to_owned());
    email_invoice.source_part_id = Some("1".to_owned());
    email_invoice.source_received_date = Some(date(2026, 5, 16));
    repository.insert(&email_invoice).await.unwrap();
    service
        .assign_items(narrow.id, &[first_id, second_id])
        .await
        .unwrap();
    service
        .assign_items(outside.id, &[outside_id])
        .await
        .unwrap();
    sqlx::query("UPDATE items SET batch_id = ? WHERE id = ?")
        .bind(narrow.id.to_string())
        .bind(email_id.to_string())
        .execute(&harness.pool)
        .await
        .unwrap();

    let result = harness
        .state
        .batch_automation_service()
        .run(wide.id)
        .await
        .unwrap();

    assert_eq!(result.imported_count, 0);
    assert_eq!(result.assigned_count, 3);
    assert_eq!(result.moved_from_other_batches_count, 3);
    assert_eq!(result.exception_count, 0);
    assert_eq!(
        result.export.as_ref().map(|export| export.item_count),
        Some(3)
    );
    for id in [first_id, second_id, email_id] {
        assert_eq!(
            repository.get_by_id(id).await.unwrap().batch_id,
            Some(wide.id)
        );
    }
    let narrow_detail = service.get(narrow.id).await.unwrap();
    assert!(narrow_detail.items.is_empty());
    assert_eq!(narrow_detail.summary.total_amount_cents, 0);
    let wide_detail = service.get(wide.id).await.unwrap();
    assert_eq!(wide_detail.summary.item_count, 3);
    assert_eq!(
        wide_detail.summary.total_amount_cents,
        12_850 + 5_000 + 2_000
    );
    // The invoice outside the re-anchored range stays where it was.
    assert_eq!(
        repository.get_by_id(outside_id).await.unwrap().batch_id,
        Some(outside.id)
    );
    let outside_detail = service.get(outside.id).await.unwrap();
    assert_eq!(outside_detail.summary.item_count, 1);
    assert_eq!(outside_detail.summary.total_amount_cents, 7_777);
    // No invoice is counted by two batches.
    assert_eq!(
        narrow_detail.summary.total_amount_cents + wide_detail.summary.total_amount_cents,
        12_850 + 5_000 + 2_000
    );
}

#[tokio::test]
async fn re_anchoring_is_idempotent_across_repeated_automation_runs() {
    let email = "reanchor-idempotent@example.com";
    let gateway = Arc::new(FakeRangeGateway::default());
    let mut responses = completed_scan(Vec::new());
    responses.extend(completed_scan(Vec::new()));
    gateway.queue(email, responses);
    let harness = Harness::new(gateway, Arc::new(SequenceExtractor::new(&[]))).await;
    harness.add_account(email, true).await;
    let service = BatchService::new(harness.pool.clone());
    let narrow = create_range_batch(&harness, "5 月中段", "2026-05-14", "2026-05-20").await;
    let wide = create_range_batch(&harness, "5 月重新锚定", "2026-05-04", "2026-05-20").await;
    let repository = ItemRepository::new(harness.pool.clone());
    let ids = [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
    for (index, id) in ids.iter().enumerate() {
        let amount = 1_000_i64 * i64::try_from(index + 1).unwrap();
        repository
            .insert(&managed_item(
                &harness.paths,
                *id,
                date(2026, 5, 15 + u32::try_from(index).unwrap()),
                amount,
            ))
            .await
            .unwrap();
    }
    service.assign_items(narrow.id, &ids).await.unwrap();
    let automation = harness.state.batch_automation_service();

    let first = automation.run(wide.id).await.unwrap();
    let second = automation.run(wide.id).await.unwrap();

    assert_eq!(first.assigned_count, 2);
    assert_eq!(first.moved_from_other_batches_count, 2);
    for id in ids {
        assert_eq!(
            repository.get_by_id(id).await.unwrap().batch_id,
            Some(wide.id)
        );
    }
    // A second run finds nothing left to move and changes nothing.
    assert_eq!(second.imported_count, 0);
    assert_eq!(second.assigned_count, 0);
    assert_eq!(second.moved_from_other_batches_count, 0);
    assert_eq!(second.exception_count, 0);
    let wide_detail = service.get(wide.id).await.unwrap();
    assert_eq!(wide_detail.summary.item_count, 2);
    assert_eq!(wide_detail.summary.total_amount_cents, 3_000);
    assert!(service.get(narrow.id).await.unwrap().items.is_empty());
}

#[tokio::test]
async fn re_anchoring_still_skips_suspected_duplicates_and_unconfirmed_invoices() {
    let email = "reanchor-safety@example.com";
    let gateway = Arc::new(FakeRangeGateway::default());
    gateway.queue(email, completed_scan(Vec::new()));
    let harness = Harness::new(gateway, Arc::new(SequenceExtractor::new(&[]))).await;
    harness.add_account(email, true).await;
    let service = BatchService::new(harness.pool.clone());
    let narrow = create_range_batch(&harness, "5 月中段", "2026-05-14", "2026-05-20").await;
    let wide = create_range_batch(&harness, "5 月重新锚定", "2026-05-04", "2026-05-20").await;
    let repository = ItemRepository::new(harness.pool.clone());
    let safe_id = uuid::Uuid::new_v4();
    let duplicate_id = uuid::Uuid::new_v4();
    let unconfirmed_id = uuid::Uuid::new_v4();
    let failed_id = uuid::Uuid::new_v4();
    repository
        .insert(&managed_item(
            &harness.paths,
            safe_id,
            date(2026, 5, 16),
            12_850,
        ))
        .await
        .unwrap();
    let mut duplicate = managed_item(&harness.paths, duplicate_id, date(2026, 5, 17), 4_000);
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(safe_id);
    repository.insert(&duplicate).await.unwrap();
    let mut unconfirmed = managed_item(&harness.paths, unconfirmed_id, date(2026, 5, 18), 3_000);
    unconfirmed.confirmation_status = ConfirmationStatus::Pending;
    repository.insert(&unconfirmed).await.unwrap();
    let mut failed = managed_item(&harness.paths, failed_id, date(2026, 5, 19), 2_500);
    failed.recognition_status = RecognitionStatus::Failed;
    repository.insert(&failed).await.unwrap();
    for id in [safe_id, duplicate_id, unconfirmed_id, failed_id] {
        sqlx::query("UPDATE items SET batch_id = ? WHERE id = ?")
            .bind(narrow.id.to_string())
            .bind(id.to_string())
            .execute(&harness.pool)
            .await
            .unwrap();
    }

    let result = harness
        .state
        .batch_automation_service()
        .run(wide.id)
        .await
        .unwrap();

    assert_eq!(result.assigned_count, 1);
    assert_eq!(result.moved_from_other_batches_count, 1);
    assert_eq!(result.exception_count, 3);
    assert_eq!(
        result.export.as_ref().map(|export| export.item_count),
        Some(1)
    );
    assert_eq!(
        repository.get_by_id(safe_id).await.unwrap().batch_id,
        Some(wide.id)
    );
    for id in [duplicate_id, unconfirmed_id, failed_id] {
        assert_eq!(
            repository.get_by_id(id).await.unwrap().batch_id,
            Some(narrow.id)
        );
    }
    let wide_detail = service.get(wide.id).await.unwrap();
    assert_eq!(wide_detail.summary.item_count, 1);
    assert_eq!(wide_detail.summary.total_amount_cents, 12_850);
    assert_eq!(service.get(narrow.id).await.unwrap().summary.item_count, 3);
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
        source_received_date: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
    }
}

fn ready_item(paths: &AppPaths, id: uuid::Uuid) -> NewItemRecord {
    let now = Utc.with_ymd_and_hms(2026, 5, 15, 8, 0, 0).single().unwrap();
    NewItemRecord {
        id,
        original_name: format!("{id}.pdf"),
        original_path: paths
            .originals
            .join(format!("{id}.pdf"))
            .to_string_lossy()
            .into_owned(),
        normalized_pdf_path: Some(
            paths
                .normalized
                .join(format!("{id}.pdf"))
                .to_string_lossy()
                .into_owned(),
        ),
        sha256: format!("sha-{id}"),
        mime_type: "application/pdf".to_owned(),
        source_type: SourceType::ManualUpload,
        source_account_id: None,
        source_mailbox: None,
        source_uid_validity: None,
        source_uid: None,
        source_message_id: None,
        source_part_id: None,
        fetched_at: now,
        source_received_date: None,
        invoice_date: Some(NaiveDate::from_ymd_opt(2026, 5, 10).unwrap()),
        suggested_period: Some("2026-05".to_owned()),
        batch_id: None,
        suggested_category: Some(Category::Dining),
        final_category: Some(Category::Dining),
        amount_cents: Some(12_850),
        currency: "CNY".to_owned(),
        city: None,
        company: None,
        recognition_status: RecognitionStatus::Succeeded,
        confirmation_status: ConfirmationStatus::Confirmed,
        dedupe_status: DedupeStatus::Unique,
        duplicate_of_id: None,
        note: None,
        event_tag: None,
        project_tag: None,
        created_at: now,
        updated_at: now,
    }
}
