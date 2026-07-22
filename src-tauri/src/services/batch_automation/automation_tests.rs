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

fn completed_rejection_scan() -> Vec<Result<MailboxDelta, AppError>> {
    let rejected = RejectedMessage {
        uid: 1,
        mailbox: "INBOX".to_owned(),
        received_at: Utc::now(),
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
async fn stale_safe_candidates_are_skipped_and_counted_as_exceptions() {
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
    let automation = tokio::spawn(async move { service.run(batch.id).await });
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
    assert_eq!(result.assigned_count, 0);
    assert_eq!(result.exception_count, 4);
    assert!(result.export.is_none());
    for id in [date_id, currency_id, amount_id] {
        assert!(items.get_by_id(id).await.unwrap().batch_id.is_none());
    }
    assert_eq!(
        items.get_by_id(owner_id).await.unwrap().batch_id,
        Some(manual_batch.id)
    );
}
