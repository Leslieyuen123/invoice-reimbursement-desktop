use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{AssignmentPause, BatchAutomationService};
use crate::db;
use crate::db::accounts::{
    MailboxAccountRepository, MailboxProvider, NewMailboxAccount, SyncCursor,
};
use crate::db::items::{ItemFilter, ItemRepository, NewItemRecord};
use crate::domain::amount::MAX_SAFE_AMOUNT_CENTS;
use crate::domain::error::AppError;
use crate::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, RecognitionStatus, SourceType,
};
use crate::infra::credentials::{CredentialStore, MemoryCredentialStore};
use crate::infra::extraction::{DocumentExtractor, ExtractedDocument};
use crate::infra::files::AppPaths;
use crate::infra::imap::{
    ImapAccountConfig, ImapDateRange, ImapGateway, MailboxDelta, MessageRejectionReason,
    RejectedMessage,
};
use crate::services::batches::BatchService;
use crate::state::AppState;

struct AccountBudgetGateway {
    responses: Mutex<HashMap<String, VecDeque<Result<MailboxDelta, AppError>>>>,
    requests: Mutex<Vec<String>>,
}

impl AccountBudgetGateway {
    fn new(emails: &[&str]) -> Self {
        let responses = emails
            .iter()
            .map(|email| ((*email).to_owned(), completed_rejection_scan().into()))
            .collect();
        Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn empty(email: &str) -> Self {
        let empty = MailboxDelta {
            uid_validity: 1,
            messages: Vec::new(),
            rejected_messages: Vec::new(),
            highest_uid: 0,
        };
        Self {
            responses: Mutex::new(HashMap::from([(
                email.to_owned(),
                VecDeque::from([Ok(empty.clone()), Ok(empty)]),
            )])),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ImapGateway for AccountBudgetGateway {
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
        panic!("automation budget test must use range fetches")
    }

    async fn fetch_range(
        &self,
        config: &ImapAccountConfig,
        _secret: &str,
        _cursor: Option<SyncCursor>,
        _range: ImapDateRange,
    ) -> Result<MailboxDelta, AppError> {
        self.requests.lock().unwrap().push(config.email.clone());
        self.responses
            .lock()
            .unwrap()
            .get_mut(&config.email)
            .and_then(VecDeque::pop_front)
            .expect("account budget response should be queued")
    }
}

struct AccountBudgetExtractor;

impl DocumentExtractor for AccountBudgetExtractor {
    fn extract(&self, _path: &Path) -> Result<ExtractedDocument, AppError> {
        Ok(ExtractedDocument {
            text: "electronic invoice".to_owned(),
            normalized_pdf: None,
            warnings: Vec::new(),
        })
    }
}

struct NormalizingExtractor;

impl DocumentExtractor for NormalizingExtractor {
    fn extract(&self, _path: &Path) -> Result<ExtractedDocument, AppError> {
        Ok(ExtractedDocument {
            text: "electronic invoice".to_owned(),
            normalized_pdf: Some(
                include_bytes!("../../../tests/fixtures/text-invoice.pdf").to_vec(),
            ),
            warnings: Vec::new(),
        })
    }
}

fn completed_rejection_scan() -> Vec<Result<MailboxDelta, AppError>> {
    let rejected = RejectedMessage {
        uid: 1,
        mailbox: "INBOX".to_owned(),
        received_at: Utc::now(),
        source_received_date: Utc::now().date_naive(),
        reason: MessageRejectionReason::MessageTooLarge,
    };
    vec![
        Ok(MailboxDelta {
            uid_validity: 1,
            messages: Vec::new(),
            rejected_messages: vec![rejected],
            highest_uid: 1,
        }),
        Ok(MailboxDelta {
            uid_validity: 1,
            messages: Vec::new(),
            rejected_messages: Vec::new(),
            highest_uid: 1,
        }),
    ]
}

fn safe_item(paths: &AppPaths, id: Uuid) -> NewItemRecord {
    let original_bytes = format!("managed original {id}");
    let original_path = paths.originals.join(format!("{id}.pdf"));
    let normalized_path = paths.normalized.join(format!("{id}.pdf"));
    fs::write(&original_path, original_bytes.as_bytes()).unwrap();
    fs::write(
        &normalized_path,
        include_bytes!("../../../tests/fixtures/text-invoice.pdf"),
    )
    .unwrap();
    let now = Utc::now();
    NewItemRecord {
        id,
        original_name: format!("{id}.pdf"),
        original_path: original_path.to_string_lossy().into_owned(),
        normalized_pdf_path: Some(normalized_path.to_string_lossy().into_owned()),
        sha256: format!("{:x}", Sha256::digest(original_bytes.as_bytes())),
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

#[tokio::test]
async fn exhausted_touched_budget_stops_before_starting_the_next_account() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let gateway = Arc::new(AccountBudgetGateway::new(&[
        "first@example.com",
        "second@example.com",
    ]));
    let credentials = Arc::new(MemoryCredentialStore::default());
    let state = AppState::with_gateway_and_extractor(
        pool.clone(),
        paths,
        credentials.clone(),
        gateway.clone(),
        Arc::new(AccountBudgetExtractor),
    );
    let accounts = MailboxAccountRepository::new(pool.clone());
    for email in ["first@example.com", "second@example.com"] {
        let account = accounts
            .insert(NewMailboxAccount {
                provider: MailboxProvider::Gmail,
                email: email.to_owned(),
                imap_host: "imap.gmail.com".to_owned(),
                imap_port: 993,
                enabled: true,
                sync_interval_minutes: 15,
            })
            .await
            .unwrap();
        credentials.set(&account.id.to_string(), "secret").unwrap();
    }
    let batch = BatchService::new(pool.clone())
        .create_month(2026, 5)
        .await
        .unwrap();
    let service: BatchAutomationService = state.batch_automation_service();

    let error = service
        .run_with_touched_item_limit(batch.id, 1)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        AppError::External {
            retryable: true,
            ref message,
            ..
        } if message.contains("narrower date range")
    ));
    {
        let requests = gateway.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|email| email == &requests[0]));
    }
    assert_eq!(
        ItemRepository::new(pool)
            .list_bounded_for_tests(ItemFilter::default())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn stale_safe_candidates_are_revalidated_and_in_range_owners_are_reclaimed() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let email = "race@example.com";
    let gateway = Arc::new(AccountBudgetGateway::empty(email));
    let credentials = Arc::new(MemoryCredentialStore::default());
    let state = AppState::with_gateway_and_extractor(
        pool.clone(),
        paths.clone(),
        credentials.clone(),
        gateway,
        Arc::new(AccountBudgetExtractor),
    );
    let account = MailboxAccountRepository::new(pool.clone())
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: email.to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    credentials.set(&account.id.to_string(), "secret").unwrap();
    let batch = BatchService::new(pool.clone())
        .create_month(2026, 5)
        .await
        .unwrap();
    let manual_batch = BatchService::new(pool.clone())
        .create_month(2026, 6)
        .await
        .unwrap();
    let date_id = Uuid::new_v4();
    let currency_id = Uuid::new_v4();
    let amount_id = Uuid::new_v4();
    let owner_id = Uuid::new_v4();
    let items = ItemRepository::new(pool.clone());
    for id in [date_id, currency_id, amount_id, owner_id] {
        items.insert(&safe_item(&paths, id)).await.unwrap();
    }
    let pause = Arc::new(AssignmentPause::default());
    let mut service = state.batch_automation_service();
    service.assignment_pause = Some(pause.clone());
    let batch_id = batch.id;
    let automation = tokio::spawn(async move { service.run(batch_id).await });
    pause.reached.notified().await;

    sqlx::query("UPDATE items SET invoice_date = '2026-06-01' WHERE id = ?")
        .bind(date_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE items SET currency = 'USD' WHERE id = ?")
        .bind(currency_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE items SET amount_cents = ? WHERE id = ?")
        .bind(MAX_SAFE_AMOUNT_CENTS + 1)
        .bind(amount_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    BatchService::new(pool.clone())
        .assign_items(manual_batch.id, &[owner_id])
        .await
        .unwrap();
    pause.release.notify_one();

    let result = automation.await.unwrap().unwrap();
    // The three invoices that became unsafe after the scan are exceptions.
    assert_eq!(result.assigned_count, 1);
    assert_eq!(result.moved_from_other_batches_count, 1);
    assert_eq!(result.exception_count, 3);
    assert_eq!(
        result.export.as_ref().map(|export| export.item_count),
        Some(1)
    );
    for id in [date_id, currency_id, amount_id] {
        assert!(items.get_by_id(id).await.unwrap().batch_id.is_none());
    }
    // The manual owner is still re-anchored: the batch owns the range it covers.
    assert_eq!(
        items.get_by_id(owner_id).await.unwrap().batch_id,
        Some(batch.id)
    );
    assert!(
        BatchService::new(pool.clone())
            .get(manual_batch.id)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn current_batch_range_is_revalidated_after_candidate_scanning() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("batch-range-race.sqlite3");
    let pool = db::connect(&format!("sqlite://{}", database_path.display()))
        .await
        .unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let email = "batch-range-race@example.com";
    let gateway = Arc::new(AccountBudgetGateway::empty(email));
    let credentials = Arc::new(MemoryCredentialStore::default());
    let state = AppState::with_gateway_and_extractor(
        pool.clone(),
        paths.clone(),
        credentials.clone(),
        gateway,
        Arc::new(AccountBudgetExtractor),
    );
    let account = MailboxAccountRepository::new(pool.clone())
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: email.to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    credentials.set(&account.id.to_string(), "secret").unwrap();
    let batch = BatchService::new(pool.clone())
        .create_month(2026, 5)
        .await
        .unwrap();
    let item_id = Uuid::new_v4();
    let items = ItemRepository::new(pool.clone());
    items.insert(&safe_item(&paths, item_id)).await.unwrap();
    let pause = Arc::new(AssignmentPause::default());
    let mut service = state.batch_automation_service();
    service.assignment_pause = Some(pause.clone());
    let batch_id = batch.id;
    let automation = tokio::spawn(async move { service.run(batch_id).await });
    pause.reached.notified().await;

    sqlx::query(
        "UPDATE batches SET start_date = '2026-06-01', end_date = '2026-06-30' WHERE id = ?",
    )
    .bind(batch_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    pause.release.notify_one();

    let result = automation.await.unwrap().unwrap();
    assert_eq!(result.assigned_count, 0);
    assert_eq!(result.exception_count, 1);
    assert!(result.export.is_none());
    assert!(items.get_by_id(item_id).await.unwrap().batch_id.is_none());
}

#[tokio::test]
async fn current_managed_paths_are_revalidated_after_candidate_scanning() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("managed-path-race.sqlite3");
    let pool = db::connect(&format!("sqlite://{}", database_path.display()))
        .await
        .unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let email = "managed-path-race@example.com";
    let gateway = Arc::new(AccountBudgetGateway::empty(email));
    let credentials = Arc::new(MemoryCredentialStore::default());
    let state = AppState::with_gateway_and_extractor(
        pool.clone(),
        paths.clone(),
        credentials.clone(),
        gateway,
        Arc::new(AccountBudgetExtractor),
    );
    let account = MailboxAccountRepository::new(pool.clone())
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: email.to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    credentials.set(&account.id.to_string(), "secret").unwrap();
    let batch = BatchService::new(pool.clone())
        .create_month(2026, 5)
        .await
        .unwrap();
    let stale_id = Uuid::new_v4();
    let current_id = Uuid::new_v4();
    let items = ItemRepository::new(pool.clone());
    for id in [stale_id, current_id] {
        items.insert(&safe_item(&paths, id)).await.unwrap();
    }
    let pause = Arc::new(AssignmentPause::default());
    let mut service = state.batch_automation_service();
    service.assignment_pause = Some(pause.clone());
    let batch_id = batch.id;
    let automation = tokio::spawn(async move { service.run(batch_id).await });
    pause.reached.notified().await;

    let outside_original = directory.path().join("outside-original.pdf");
    let outside_normalized = directory.path().join("outside-normalized.pdf");
    fs::write(&outside_original, b"outside original").unwrap();
    fs::write(
        &outside_normalized,
        include_bytes!("../../../tests/fixtures/text-invoice.pdf"),
    )
    .unwrap();
    sqlx::query("UPDATE items SET original_path = ?, normalized_pdf_path = ? WHERE id = ?")
        .bind(outside_original.to_string_lossy().into_owned())
        .bind(outside_normalized.to_string_lossy().into_owned())
        .bind(stale_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pause.release.notify_one();

    let result = automation.await.unwrap().unwrap();
    assert_eq!(result.assigned_count, 1);
    assert_eq!(result.exception_count, 1);
    assert_eq!(result.export.unwrap().item_count, 1);
    assert!(items.get_by_id(stale_id).await.unwrap().batch_id.is_none());
    assert_eq!(
        items.get_by_id(current_id).await.unwrap().batch_id,
        Some(batch_id)
    );
}

#[tokio::test]
async fn automation_repairs_a_batch_member_that_lost_its_normalized_pdf() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let email = "repair@example.com";
    let credentials = Arc::new(MemoryCredentialStore::default());
    let state = AppState::with_gateway_and_extractor(
        pool.clone(),
        paths.clone(),
        credentials.clone(),
        Arc::new(AccountBudgetGateway::empty(email)),
        Arc::new(NormalizingExtractor),
    );
    let account = MailboxAccountRepository::new(pool.clone())
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: email.to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    credentials.set(&account.id.to_string(), "secret").unwrap();
    let batch = BatchService::new(pool.clone())
        .create_month(2026, 5)
        .await
        .unwrap();
    let item_id = Uuid::new_v4();
    let mut record = safe_item(&paths, item_id);
    record.batch_id = Some(batch.id);
    // An invoice that was confirmed while it had no normalized PDF: the batch
    // used to be unexportable forever and "retry automation" could not help.
    fs::remove_file(record.normalized_pdf_path.clone().unwrap()).unwrap();
    record.normalized_pdf_path = None;
    ItemRepository::new(pool.clone())
        .insert(&record)
        .await
        .unwrap();

    let result = state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap();

    assert_eq!(result.repaired_count, 1);
    let repaired = ItemRepository::new(pool.clone())
        .get_by_id(item_id)
        .await
        .unwrap();
    let repaired_path = repaired
        .normalized_pdf_path
        .expect("repair should record a normalized PDF");
    assert!(Path::new(&repaired_path).is_file());
    assert_eq!(repaired.final_category, Some(Category::Dining));
    assert_eq!(repaired.amount_cents, Some(12_850));
    assert_eq!(repaired.confirmation_status, ConfirmationStatus::Confirmed);
    assert!(result.export.is_some());
}

#[tokio::test]
async fn automation_files_otherwise_ready_candidates_without_a_category_under_other() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let email = "other-category@example.com";
    let credentials = Arc::new(MemoryCredentialStore::default());
    let state = AppState::with_gateway_and_extractor(
        pool.clone(),
        paths.clone(),
        credentials.clone(),
        Arc::new(AccountBudgetGateway::empty(email)),
        Arc::new(AccountBudgetExtractor),
    );
    let account = MailboxAccountRepository::new(pool.clone())
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: email.to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    credentials.set(&account.id.to_string(), "secret").unwrap();
    let batch = BatchService::new(pool.clone())
        .create_month(2026, 5)
        .await
        .unwrap();
    let items = ItemRepository::new(pool.clone());
    let ready_id = Uuid::new_v4();
    items.insert(&safe_item(&paths, ready_id)).await.unwrap();
    // The real library state: recognized, dated, priced and normalized, but no
    // category was ever assigned, so it never got confirmed.
    sqlx::query(
        "UPDATE items SET suggested_category = NULL, final_category = NULL, \
            confirmation_status = 'pending' WHERE id = ?",
    )
    .bind(ready_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let missing_amount_id = Uuid::new_v4();
    let mut incomplete = safe_item(&paths, missing_amount_id);
    incomplete.amount_cents = None;
    incomplete.suggested_category = None;
    incomplete.final_category = None;
    incomplete.confirmation_status = ConfirmationStatus::Pending;
    items.insert(&incomplete).await.unwrap();

    let service = state.batch_automation_service();
    let first = service.run(batch.id).await.unwrap();

    assert_eq!(first.assigned_count, 1);
    assert_eq!(first.categorized_as_other_count, 1);
    assert_eq!(first.exception_count, 1);
    assert_eq!(
        first.export.as_ref().map(|export| export.item_count),
        Some(1)
    );
    let stored = items.get_by_id(ready_id).await.unwrap();
    assert_eq!(stored.final_category, Some(Category::Other));
    assert_eq!(stored.confirmation_status, ConfirmationStatus::Confirmed);
    assert_eq!(stored.batch_id, Some(batch.id));
    // An invoice missing its amount is not "ready except for the category", so
    // automation must not invent a category (or confirmation) for it.
    let untouched = items.get_by_id(missing_amount_id).await.unwrap();
    assert_eq!(untouched.final_category, None);
    assert_eq!(untouched.confirmation_status, ConfirmationStatus::Pending);
    assert_eq!(untouched.batch_id, None);

    // Re-running the automation is idempotent: nothing is re-categorized. A
    // fresh service instance shares the database, so this is the same pass with
    // a fresh mailbox scan budget.
    let second_state = AppState::with_gateway_and_extractor(
        pool.clone(),
        paths.clone(),
        credentials.clone(),
        Arc::new(AccountBudgetGateway::empty(email)),
        Arc::new(AccountBudgetExtractor),
    );
    let second = second_state
        .batch_automation_service()
        .run(batch.id)
        .await
        .unwrap();
    assert_eq!(second.assigned_count, 0);
    assert_eq!(second.categorized_as_other_count, 0);
}
