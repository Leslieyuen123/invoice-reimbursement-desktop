use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::accounts::{
    MailboxAccountRepository, MailboxProvider, NewMailboxAccount, SyncCursor, SyncRun,
};
use invoice_reimbursement::db::items::{ItemFilter, ItemPatch, ItemRepository};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{ConfirmationStatus, RecognitionStatus};
use invoice_reimbursement::infra::credentials::{CredentialStore, MemoryCredentialStore};
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::infra::imap::{
    ImapAccountConfig, ImapGateway, MailboxDelta, MessageRejectionReason, NativeTlsImapGateway,
    RawMessage, RejectedMessage,
};
use invoice_reimbursement::services::import::{EmailImportSource, ImportOutcome, ImportService};
use invoice_reimbursement::services::recognition::RecognitionService;
use invoice_reimbursement::services::sync::SyncService;
use uuid::Uuid;

#[derive(Default)]
struct FakeExtractor;

impl DocumentExtractor for FakeExtractor {
    fn extract(&self, _path: &Path) -> Result<ExtractedDocument, AppError> {
        Ok(ExtractedDocument {
            text: "电子票据".to_owned(),
            normalized_pdf: None,
            warnings: Vec::new(),
        })
    }
}

struct FailingExtractor {
    error: Mutex<Option<AppError>>,
}

impl FailingExtractor {
    fn once(error: AppError) -> Self {
        Self {
            error: Mutex::new(Some(error)),
        }
    }
}

impl DocumentExtractor for FailingExtractor {
    fn extract(&self, _path: &Path) -> Result<ExtractedDocument, AppError> {
        match self.error.lock().unwrap().take() {
            Some(error) => Err(error),
            None => Ok(ExtractedDocument {
                text: "电子票据".to_owned(),
                normalized_pdf: None,
                warnings: Vec::new(),
            }),
        }
    }
}

struct FakeImapGateway {
    deltas: Mutex<VecDeque<Result<MailboxDelta, AppError>>>,
    cursors: Mutex<Vec<Option<SyncCursor>>>,
}

struct ConcurrentGateway {
    delta: MailboxDelta,
    barrier: tokio::sync::Barrier,
}

struct OrderedCompletionGateway {
    stale_delta: MailboxDelta,
    newer_delta: MailboxDelta,
    calls: AtomicUsize,
    cursors: Mutex<Vec<Option<SyncCursor>>>,
    stale_started: tokio::sync::Notify,
    release_stale: tokio::sync::Notify,
}

#[async_trait]
impl ImapGateway for ConcurrentGateway {
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
        self.barrier.wait().await;
        Ok(self.delta.clone())
    }
}

#[async_trait]
impl ImapGateway for OrderedCompletionGateway {
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
        cursor: Option<SyncCursor>,
    ) -> Result<MailboxDelta, AppError> {
        self.cursors.lock().unwrap().push(cursor);
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => {
                self.stale_started.notify_one();
                self.release_stale.notified().await;
                Ok(self.stale_delta.clone())
            }
            1 => Ok(self.newer_delta.clone()),
            _ => panic!("ordered gateway received an unexpected fetch"),
        }
    }
}

impl FakeImapGateway {
    fn new(deltas: Vec<Result<MailboxDelta, AppError>>) -> Self {
        Self {
            deltas: Mutex::new(deltas.into()),
            cursors: Mutex::new(Vec::new()),
        }
    }

    fn cursors(&self) -> Vec<Option<SyncCursor>> {
        self.cursors.lock().unwrap().clone()
    }
}

#[async_trait]
impl ImapGateway for FakeImapGateway {
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
        cursor: Option<SyncCursor>,
    ) -> Result<MailboxDelta, AppError> {
        self.cursors.lock().unwrap().push(cursor);
        self.deltas
            .lock()
            .unwrap()
            .pop_front()
            .expect("fake IMAP delta")
    }
}

#[tokio::test]
async fn incremental_sync_imports_each_mail_part_once() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "finance@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "application-password")
        .unwrap();
    let first_delta = MailboxDelta {
        rejected_messages: vec![],
        uid_validity: 10,
        highest_uid: 102,
        messages: vec![
            raw_message(101, include_bytes!("fixtures/mail/attachment.eml")),
            raw_message(102, include_bytes!("fixtures/mail/inline-image.eml")),
        ],
    };
    let second_delta = MailboxDelta {
        rejected_messages: vec![],
        uid_validity: 10,
        highest_uid: 102,
        messages: vec![],
    };
    let gateway = Arc::new(FakeImapGateway::new(vec![
        Ok(first_delta),
        Ok(second_delta),
    ]));
    let import = ImportService::new(
        items.clone(),
        AppPaths::create(directory.path().join("storage")).unwrap(),
    );
    let recognition = RecognitionService::new(items.clone(), Arc::new(FakeExtractor));
    let service = SyncService::new(
        gateway.clone(),
        credentials,
        accounts.clone(),
        import,
        recognition,
    );

    let first = service.run(account.id).await.unwrap();
    let second = service.run(account.id).await.unwrap();

    assert_eq!(first.imported_count, 2);
    assert_eq!(second.imported_count, 0);
    assert_eq!(items.list(ItemFilter::default()).await.unwrap().len(), 2);
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 10,
            last_uid: 102,
        })
    );
    assert_eq!(
        gateway.cursors(),
        vec![
            None,
            Some(SyncCursor {
                uid_validity: 10,
                last_uid: 102,
            }),
        ]
    );
}

#[tokio::test]
async fn refetched_existing_pending_part_resumes_recognition_without_reimporting() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "pending-recovery@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let import = ImportService::new(items.clone(), paths);
    let staged = import
        .import_email_bytes(
            "invoice-101.pdf",
            b"%PDF-1.7\n1 0 obj\n<</Type/Catalog>>\nendobj\n%%EOF\n",
            EmailImportSource {
                account_id: account.id,
                mailbox: "INBOX".to_owned(),
                uid_validity: 61,
                uid: 101,
                message_id: Some("attachment-101@example.com".to_owned()),
                part_id: "2".to_owned(),
                received_at: Utc.with_ymd_and_hms(2026, 7, 14, 10, 0, 0).unwrap(),
                rescan: false,
            },
        )
        .await
        .unwrap();
    let ImportOutcome::New(staged) = staged else {
        panic!("crash fixture should be a newly persisted item")
    };
    assert_eq!(staged.recognition_status, RecognitionStatus::Pending);
    let confirmed = import
        .import_email_bytes(
            "invoice-102.pdf",
            b"%PDF-1.7\nconfirmed but pending\n%%EOF\n",
            EmailImportSource {
                account_id: account.id,
                mailbox: "INBOX".to_owned(),
                uid_validity: 61,
                uid: 102,
                message_id: Some("attachment-101@example.com".to_owned()),
                part_id: "2".to_owned(),
                received_at: Utc.with_ymd_and_hms(2026, 7, 14, 10, 0, 0).unwrap(),
                rescan: false,
            },
        )
        .await
        .unwrap();
    let ImportOutcome::New(confirmed) = confirmed else {
        panic!("confirmed fixture should be a newly persisted item")
    };
    let confirmed = items
        .update_fields(
            confirmed.id,
            ItemPatch {
                confirmation_status: Some(ConfirmationStatus::Confirmed),
                ..ItemPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(confirmed.recognition_status, RecognitionStatus::Pending);
    let service = SyncService::new(
        Arc::new(FakeImapGateway::new(vec![Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 61,
            highest_uid: 102,
            messages: vec![
                raw_message(101, include_bytes!("fixtures/mail/attachment.eml")),
                raw_message(102, include_bytes!("fixtures/mail/attachment.eml")),
            ],
        })])),
        credentials,
        accounts.clone(),
        import,
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    let result = service.run(account.id).await.unwrap();

    assert_eq!(result.imported_count, 0);
    let recovered = items.get_by_id(staged.id).await.unwrap();
    assert_eq!(recovered.recognition_status, RecognitionStatus::Succeeded);
    let preserved = items.get_by_id(confirmed.id).await.unwrap();
    assert_eq!(preserved.recognition_status, RecognitionStatus::Pending);
    assert_eq!(preserved.confirmation_status, ConfirmationStatus::Confirmed);
    assert_eq!(items.list(ItemFilter::default()).await.unwrap().len(), 2);
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 61,
            last_uid: 102,
        })
    );
}

#[tokio::test]
async fn first_sync_recovers_a_legacy_epoch_zero_pending_part() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "legacy-epoch@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let import = ImportService::new(items.clone(), paths.clone());
    let legacy = import
        .import_email_bytes(
            "invoice-101.pdf",
            b"%PDF-1.7\n1 0 obj\n<</Type/Catalog>>\nendobj\n%%EOF\n",
            EmailImportSource {
                account_id: account.id,
                mailbox: "INBOX".to_owned(),
                uid_validity: 10,
                uid: 101,
                message_id: Some("attachment-101@example.com".to_owned()),
                part_id: "2".to_owned(),
                received_at: Utc.with_ymd_and_hms(2026, 7, 14, 10, 0, 0).unwrap(),
                rescan: false,
            },
        )
        .await
        .unwrap();
    let ImportOutcome::New(legacy) = legacy else {
        panic!("legacy fixture should be newly persisted")
    };
    sqlx::query("UPDATE items SET source_uid_validity = 0 WHERE id = ?")
        .bind(legacy.id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        items
            .get_by_id(legacy.id)
            .await
            .unwrap()
            .source_uid_validity,
        Some(0)
    );
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        None
    );
    let service = SyncService::new(
        Arc::new(FakeImapGateway::new(vec![Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 11,
            highest_uid: 7,
            messages: vec![raw_message(
                7,
                include_bytes!("fixtures/mail/attachment.eml"),
            )],
        })])),
        credentials,
        accounts.clone(),
        import,
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    let result = service.run(account.id).await.unwrap();

    assert_eq!(result.imported_count, 0);
    let stored = items.list(ItemFilter::default()).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, legacy.id);
    assert_eq!(stored[0].source_uid_validity, Some(0));
    assert_eq!(stored[0].recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(count_files(&paths.originals), 1);
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 11,
            last_uid: 7,
        })
    );
}

#[tokio::test]
async fn cid_image_without_content_disposition_is_imported() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "cid-without-disposition@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "application-password")
        .unwrap();
    let service = SyncService::new(
        Arc::new(FakeImapGateway::new(vec![Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 11,
            highest_uid: 104,
            messages: vec![raw_message(
                104,
                include_bytes!("fixtures/mail/inline-cid-no-disposition.eml"),
            )],
        })])),
        credentials,
        accounts,
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    let result = service.run(account.id).await.unwrap();
    let stored = items.list(ItemFilter::default()).await.unwrap();

    assert_eq!(result.imported_count, 1);
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].original_name, "inline-2.png");
    assert_eq!(stored[0].source_uid, Some(104));
    assert_eq!(
        stored[0].source_message_id.as_deref(),
        Some("inline-cid-104@example.com")
    );
    assert_eq!(stored[0].source_part_id.as_deref(), Some("2"));
}

#[tokio::test]
async fn https_download_link_becomes_a_local_pending_placeholder_without_network_access() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::QQ,
            email: "links@example.com".to_owned(),
            imap_host: "imap.qq.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "auth-code")
        .unwrap();
    let gateway = Arc::new(FakeImapGateway::new(vec![Ok(MailboxDelta {
        rejected_messages: vec![],
        uid_validity: 20,
        highest_uid: 103,
        messages: vec![raw_message(
            103,
            include_bytes!("fixtures/mail/download-link.eml"),
        )],
    })]));
    let import = ImportService::new(
        items.clone(),
        AppPaths::create(directory.path().join("storage")).unwrap(),
    );
    let recognition = RecognitionService::new(items.clone(), Arc::new(FakeExtractor));
    let service = SyncService::new(gateway, credentials, accounts.clone(), import, recognition);

    let result = tokio::time::timeout(std::time::Duration::from_secs(1), service.run(account.id))
        .await
        .expect("sync must not try to fetch the unreachable URL")
        .unwrap();

    assert_eq!(result.imported_count, 1);
    let stored = items.list(ItemFilter::default()).await.unwrap();
    assert_eq!(stored.len(), 1);
    let link = &stored[0];
    assert_eq!(link.mime_type, "text/uri-list");
    assert_eq!(
        link.note.as_deref(),
        Some("https://127.0.0.1:1/invoices/103.pdf")
    );
    assert_eq!(link.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(link.confirmation_status, ConfirmationStatus::Pending);
    assert_eq!(link.source_account_id, Some(account.id));
    assert_eq!(link.source_mailbox.as_deref(), Some("INBOX"));
    assert_eq!(link.source_uid, Some(103));
    assert_eq!(
        link.source_message_id.as_deref(),
        Some("link-103@example.com")
    );
    assert_eq!(link.source_part_id.as_deref(), Some("0.link.0"));
    assert!(!link.original_path.starts_with("https://"));
    assert_eq!(
        std::fs::read_to_string(&link.original_path).unwrap(),
        "https://127.0.0.1:1/invoices/103.pdf\r\n"
    );
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 20,
            last_uid: 103,
        })
    );
}

#[tokio::test]
async fn html_links_only_import_download_candidates_from_the_message_body() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "link-policy@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let service = SyncService::new(
        Arc::new(FakeImapGateway::new(vec![Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 21,
            highest_uid: 201,
            messages: vec![raw_message(
                201,
                include_bytes!("fixtures/mail/link-policy.eml"),
            )],
        })])),
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    let result = service.run(account.id).await.unwrap();

    assert_eq!(result.imported_count, 3);
    let stored = items.list(ItemFilter::default()).await.unwrap();
    assert_eq!(stored.len(), 3);
    let mut urls = stored
        .iter()
        .map(|item| item.note.clone().unwrap())
        .collect::<Vec<_>>();
    urls.sort();
    assert_eq!(
        urls,
        [
            "https://example.com/materials/invoice-201.pdf",
            "https://example.com/secure/material?id=202",
            "https://example.com/secure/receipt?id=203",
        ]
    );
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 21,
            last_uid: 201,
        })
    );
}

#[tokio::test]
async fn excessive_download_candidates_are_truncated_and_cursor_advances() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "link-budget@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let raw = many_download_links_message(35);
    let service = SyncService::new(
        Arc::new(FakeImapGateway::new(vec![Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 22,
            highest_uid: 202,
            messages: vec![raw_message(202, raw.as_bytes())],
        })])),
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    let result = service.run(account.id).await.unwrap();

    assert_eq!(result.imported_count, 32);
    let stored = items.list(ItemFilter::default()).await.unwrap();
    assert_eq!(stored.len(), 32);
    assert!(stored.iter().any(|item| {
        item.note.as_deref() == Some("https://example.com/invoices/invoice-00.pdf")
    }));
    assert!(stored.iter().any(|item| {
        item.note.as_deref() == Some("https://example.com/invoices/invoice-31.pdf")
    }));
    assert!(!stored.iter().any(|item| {
        item.note.as_deref() == Some("https://example.com/invoices/invoice-32.pdf")
    }));
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 22,
            last_uid: 202,
        })
    );
}

#[tokio::test]
async fn authentication_failure_is_sanitized_and_does_not_advance_the_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::QQ,
            email: "auth@example.com".to_owned(),
            imap_host: "imap.qq.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    let secret = "private-auth-code";
    credentials.set(&account.id.to_string(), secret).unwrap();
    let gateway = Arc::new(FakeImapGateway::new(vec![
        Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 9,
            highest_uid: 42,
            messages: vec![],
        }),
        Err(AppError::External {
            service: "imap".to_owned(),
            retryable: false,
            message: format!("login rejected credential {secret}\nserver detail"),
        }),
    ]));
    let service = SyncService::new(
        gateway,
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items, Arc::new(FakeExtractor)),
    );
    service.run(account.id).await.unwrap();

    let error = service.run(account.id).await.unwrap_err();

    assert!(!error.to_string().contains(secret));
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 9,
            last_uid: 42,
        })
    );
    let refreshed = accounts.get(account.id).await.unwrap();
    assert!(refreshed.last_synced_at.is_some());
    assert!(
        refreshed
            .last_error
            .as_deref()
            .unwrap()
            .contains("[redacted]")
    );
    assert!(!refreshed.last_error.as_deref().unwrap().contains(secret));
    let runs = sqlx::query_as::<_, (String, i64, Option<String>)>(
        "SELECT status, imported_count, error_message FROM sync_runs ORDER BY started_at, rowid",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(runs[0], ("succeeded".to_owned(), 0, None));
    assert_eq!(runs[1].0, "failed");
    assert_eq!(runs[1].1, 0);
    assert!(runs[1].2.as_deref().unwrap().contains("[redacted]"));
    assert!(!runs[1].2.as_deref().unwrap().contains(secret));
}

#[tokio::test]
async fn missing_mailbox_credential_is_not_retryable_and_does_not_fetch() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "missing-credential@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let gateway = Arc::new(FakeImapGateway::new(vec![]));
    let service = SyncService::new(
        gateway.clone(),
        Arc::new(MemoryCredentialStore::default()),
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items, Arc::new(FakeExtractor)),
    );

    let error = service.run(account.id).await.unwrap_err();

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: false,
            ..
        } if service == "imap"
    ));
    assert!(gateway.cursors().is_empty());
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn infrastructure_failure_after_import_keeps_the_item_but_not_the_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "infra@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let delta = MailboxDelta {
        rejected_messages: vec![],
        uid_validity: 30,
        highest_uid: 101,
        messages: vec![raw_message(
            101,
            include_bytes!("fixtures/mail/attachment.eml"),
        )],
    };
    let gateway = Arc::new(FakeImapGateway::new(vec![Ok(delta.clone()), Ok(delta)]));
    let service = SyncService::new(
        gateway,
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(
            items.clone(),
            Arc::new(FailingExtractor::once(AppError::External {
                service: "filesystem".to_owned(),
                retryable: true,
                message: "storage temporarily unavailable".to_owned(),
            })),
        ),
    );

    let first_error = service.run(account.id).await.unwrap_err();

    assert!(
        first_error
            .to_string()
            .contains("storage temporarily unavailable")
    );
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        None
    );
    assert_eq!(items.list(ItemFilter::default()).await.unwrap().len(), 1);

    let rerun = service.run(account.id).await.unwrap();
    assert_eq!(rerun.imported_count, 0);
    assert_eq!(items.list(ItemFilter::default()).await.unwrap().len(), 1);
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 30,
            last_uid: 101,
        })
    );
    let statuses =
        sqlx::query_scalar::<_, String>("SELECT status FROM sync_runs ORDER BY started_at, rowid")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(statuses, ["failed", "succeeded"]);
}

#[tokio::test]
async fn cursor_database_failure_rolls_back_success_and_marks_the_run_failed() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "db-failure@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_sync_cursor BEFORE INSERT ON sync_cursors \
         BEGIN SELECT RAISE(ABORT, 'injected cursor failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let service = SyncService::new(
        Arc::new(FakeImapGateway::new(vec![Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 40,
            highest_uid: 0,
            messages: vec![],
        })])),
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items, Arc::new(FakeExtractor)),
    );

    let error = service.run(account.id).await.unwrap_err();

    assert!(error.to_string().contains("sync cursor"));
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        None
    );
    let refreshed = accounts.get(account.id).await.unwrap();
    assert_eq!(refreshed.last_synced_at, None);
    assert!(refreshed.last_error.is_some());
    let run = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT status, finished_at, error_message FROM sync_runs",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(run.0, "failed");
    assert!(run.1.is_some());
    assert!(run.2.as_deref().unwrap().contains("sync cursor"));
}

#[tokio::test]
async fn cursor_repository_upserts_u32_values_and_rejects_a_missing_account() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool);
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "cursor@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();

    accounts
        .upsert_cursor(
            account.id,
            "INBOX",
            SyncCursor {
                uid_validity: u32::MAX,
                last_uid: u32::MAX,
            },
        )
        .await
        .unwrap();
    accounts
        .upsert_cursor(
            account.id,
            "INBOX",
            SyncCursor {
                uid_validity: 11,
                last_uid: 7,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 11,
            last_uid: 7,
        })
    );
    let missing = Uuid::new_v4();
    assert!(
        accounts
            .upsert_cursor(
                missing,
                "INBOX",
                SyncCursor {
                    uid_validity: 1,
                    last_uid: 1,
                },
            )
            .await
            .is_err()
    );
    assert_eq!(accounts.get_cursor(missing, "INBOX").await.unwrap(), None);
}

#[tokio::test]
async fn damaged_document_is_marked_failed_while_later_parts_continue_with_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "damage@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let service = SyncService::new(
        Arc::new(FakeImapGateway::new(vec![Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 50,
            highest_uid: 102,
            messages: vec![
                raw_message(101, include_bytes!("fixtures/mail/attachment.eml")),
                raw_message(102, include_bytes!("fixtures/mail/inline-image.eml")),
            ],
        })])),
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(
            items.clone(),
            Arc::new(FailingExtractor::once(AppError::External {
                service: "document_extractor".to_owned(),
                retryable: false,
                message: "damaged PDF".to_owned(),
            })),
        ),
    );

    let result = service.run(account.id).await.unwrap();

    assert_eq!(result.imported_count, 2);
    let stored = items.list(ItemFilter::default()).await.unwrap();
    assert_eq!(stored.len(), 2);
    let failed = stored
        .iter()
        .find(|item| item.source_uid == Some(101))
        .unwrap();
    let succeeded = stored
        .iter()
        .find(|item| item.source_uid == Some(102))
        .unwrap();
    assert_eq!(failed.recognition_status, RecognitionStatus::Failed);
    assert_eq!(failed.confirmation_status, ConfirmationStatus::Pending);
    assert_eq!(succeeded.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(
        succeeded.invoice_date,
        chrono::NaiveDate::from_ymd_opt(2026, 7, 14)
    );
    assert_eq!(succeeded.suggested_period.as_deref(), Some("2026-07"));
    for (item, uid, message_id) in [
        (failed, 101, "attachment-101@example.com"),
        (succeeded, 102, "inline-102@example.com"),
    ] {
        assert_eq!(item.source_account_id, Some(account.id));
        assert_eq!(item.source_mailbox.as_deref(), Some("INBOX"));
        assert_eq!(item.source_uid, Some(uid));
        assert_eq!(item.source_message_id.as_deref(), Some(message_id));
        assert_eq!(item.source_part_id.as_deref(), Some("2"));
        assert_eq!(
            item.fetched_at,
            Utc.with_ymd_and_hms(2026, 7, 14, 10, 0, 0).unwrap()
        );
    }
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 50,
            last_uid: 102,
        })
    );
}

#[tokio::test]
async fn empty_email_attachment_is_retained_as_failed_while_later_mail_continues() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "empty-part@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let service = SyncService::new(
        Arc::new(FakeImapGateway::new(vec![Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 51,
            highest_uid: 102,
            messages: vec![
                raw_message(101, include_bytes!("fixtures/mail/empty-attachment.eml")),
                raw_message(102, include_bytes!("fixtures/mail/attachment.eml")),
            ],
        })])),
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    let result = service.run(account.id).await.unwrap();

    assert_eq!(result.imported_count, 2);
    let stored = items.list(ItemFilter::default()).await.unwrap();
    assert_eq!(stored.len(), 2);
    let empty = stored
        .iter()
        .find(|item| item.source_uid == Some(101))
        .unwrap();
    let valid = stored
        .iter()
        .find(|item| item.source_uid == Some(102))
        .unwrap();
    assert_eq!(empty.original_name, "empty.pdf");
    assert_eq!(std::fs::read(&empty.original_path).unwrap(), b"");
    assert_eq!(
        empty.sha256,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(empty.mime_type, "application/octet-stream");
    assert_eq!(empty.recognition_status, RecognitionStatus::Failed);
    assert_eq!(empty.confirmation_status, ConfirmationStatus::Pending);
    assert!(
        empty
            .note
            .as_deref()
            .is_some_and(|note| note.contains("empty"))
    );
    assert_eq!(valid.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 51,
            last_uid: 102,
        })
    );
}

#[tokio::test]
async fn oversized_message_creates_a_rejection_item_and_later_mail_continues_once() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "oversized-message@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let rejected = RejectedMessage {
        uid: 101,
        mailbox: "INBOX".to_owned(),
        received_at: Utc.with_ymd_and_hms(2026, 7, 14, 9, 0, 0).unwrap(),
        reason: MessageRejectionReason::MessageTooLarge,
    };
    let gateway = Arc::new(FakeImapGateway::new(vec![
        Ok(MailboxDelta {
            uid_validity: 71,
            highest_uid: 102,
            rejected_messages: vec![rejected.clone()],
            messages: vec![raw_message(
                102,
                include_bytes!("fixtures/mail/attachment.eml"),
            )],
        }),
        Ok(MailboxDelta {
            uid_validity: 71,
            highest_uid: 102,
            rejected_messages: vec![rejected],
            messages: vec![],
        }),
    ]));
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = SyncService::new(
        gateway,
        credentials,
        accounts.clone(),
        ImportService::new(items.clone(), paths.clone()),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    let first = service.run(account.id).await.unwrap();
    let second = service.run(account.id).await.unwrap();

    assert_eq!(first.imported_count, 2);
    assert_eq!(second.imported_count, 0);
    let stored = items.list(ItemFilter::default()).await.unwrap();
    assert_eq!(stored.len(), 2);
    let rejected = stored
        .iter()
        .find(|item| item.source_uid == Some(101))
        .unwrap();
    let valid = stored
        .iter()
        .find(|item| item.source_uid == Some(102))
        .unwrap();
    assert_eq!(rejected.source_part_id.as_deref(), Some("message.rejected"));
    assert_eq!(rejected.recognition_status, RecognitionStatus::Failed);
    assert_eq!(rejected.confirmation_status, ConfirmationStatus::Pending);
    assert_eq!(rejected.mime_type, "text/plain");
    assert_eq!(
        rejected.note.as_deref(),
        Some("mailbox message rejected: message_too_large")
    );
    let placeholder = std::fs::read_to_string(&rejected.original_path).unwrap();
    assert!(placeholder.contains("message_too_large"));
    assert!(!placeholder.contains("server"));
    assert_eq!(valid.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(count_files(&paths.originals), 2);
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 71,
            last_uid: 102,
        })
    );
}

#[tokio::test]
async fn uidvalidity_change_rescans_without_duplicating_a_mail_part() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "rescan@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let message = raw_message(101, include_bytes!("fixtures/mail/attachment.eml"));
    let gateway = Arc::new(FakeImapGateway::new(vec![
        Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 10,
            highest_uid: 101,
            messages: vec![message.clone()],
        }),
        Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 11,
            highest_uid: 101,
            messages: vec![message],
        }),
    ]));
    let service = SyncService::new(
        gateway.clone(),
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    assert_eq!(service.run(account.id).await.unwrap().imported_count, 1);
    assert_eq!(service.run(account.id).await.unwrap().imported_count, 0);

    assert_eq!(items.list(ItemFilter::default()).await.unwrap().len(), 1);
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 11,
            last_uid: 101,
        })
    );
    assert_eq!(
        gateway.cursors(),
        vec![
            None,
            Some(SyncCursor {
                uid_validity: 10,
                last_uid: 101,
            }),
        ]
    );
}

#[tokio::test]
async fn uidvalidity_change_reuses_the_same_message_part_at_a_new_uid() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "epoch-renumber@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let gateway = Arc::new(FakeImapGateway::new(vec![
        Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 10,
            highest_uid: 101,
            messages: vec![raw_message(
                101,
                include_bytes!("fixtures/mail/attachment.eml"),
            )],
        }),
        Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 11,
            highest_uid: 7,
            messages: vec![raw_message(
                7,
                include_bytes!("fixtures/mail/attachment.eml"),
            )],
        }),
    ]));
    let service = SyncService::new(
        gateway,
        credentials,
        accounts,
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    assert_eq!(service.run(account.id).await.unwrap().imported_count, 1);
    assert_eq!(service.run(account.id).await.unwrap().imported_count, 0);
    let stored = items.list(ItemFilter::default()).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].source_uid_validity, Some(10));
}

#[tokio::test]
async fn uidvalidity_change_keeps_reused_uid_when_message_content_changes() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "epoch-reuse@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let gateway = Arc::new(FakeImapGateway::new(vec![
        Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 10,
            highest_uid: 101,
            messages: vec![raw_message(
                101,
                include_bytes!("fixtures/mail/attachment.eml"),
            )],
        }),
        Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 11,
            highest_uid: 101,
            messages: vec![raw_message(
                101,
                include_bytes!("fixtures/mail/inline-image.eml"),
            )],
        }),
    ]));
    let service = SyncService::new(
        gateway,
        credentials,
        accounts,
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    assert_eq!(service.run(account.id).await.unwrap().imported_count, 1);
    assert_eq!(service.run(account.id).await.unwrap().imported_count, 1);
    let stored = items.list(ItemFilter::default()).await.unwrap();
    assert_eq!(stored.len(), 2);
    let mut identities = stored
        .iter()
        .map(|item| (item.source_uid_validity, item.source_uid))
        .collect::<Vec<_>>();
    identities.sort_unstable();
    assert_eq!(identities, [(Some(10), Some(101)), (Some(11), Some(101))]);
}

#[tokio::test]
async fn same_uidvalidity_same_content_at_a_new_uid_is_a_suspected_duplicate() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "same-epoch-duplicate@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let gateway = Arc::new(FakeImapGateway::new(vec![
        Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 10,
            highest_uid: 101,
            messages: vec![raw_message(
                101,
                include_bytes!("fixtures/mail/attachment.eml"),
            )],
        }),
        Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 10,
            highest_uid: 102,
            messages: vec![raw_message(
                102,
                include_bytes!("fixtures/mail/attachment.eml"),
            )],
        }),
    ]));
    let service = SyncService::new(
        gateway,
        credentials,
        accounts,
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    assert_eq!(service.run(account.id).await.unwrap().imported_count, 1);
    assert_eq!(service.run(account.id).await.unwrap().imported_count, 1);
    let stored = items.list(ItemFilter::default()).await.unwrap();
    assert_eq!(stored.len(), 2);
    let duplicate = stored
        .iter()
        .find(|item| item.source_uid == Some(102))
        .expect("new UID should be stored");
    assert_eq!(
        duplicate.dedupe_status,
        invoice_reimbursement::domain::model::DedupeStatus::SuspectedDuplicate
    );
    assert!(
        stored
            .iter()
            .all(|item| item.source_uid_validity == Some(10))
    );
}

#[tokio::test]
async fn concurrent_syncs_import_one_database_row_and_one_original() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("sync.sqlite3").display()
    );
    let pool = db::connect(&database_url).await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "concurrent@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = SyncService::new(
        Arc::new(ConcurrentGateway {
            delta: MailboxDelta {
                rejected_messages: vec![],
                uid_validity: 60,
                highest_uid: 101,
                messages: vec![raw_message(
                    101,
                    include_bytes!("fixtures/mail/attachment.eml"),
                )],
            },
            barrier: tokio::sync::Barrier::new(2),
        }),
        credentials,
        accounts.clone(),
        ImportService::new(items.clone(), paths.clone()),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    let (first, second) = tokio::join!(service.run(account.id), service.run(account.id));
    let results = [first, second];

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert_eq!(items.list(ItemFilter::default()).await.unwrap().len(), 1);
    assert_eq!(count_files(&paths.originals), 1);
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 60,
            last_uid: 101,
        })
    );
}

#[tokio::test]
async fn stale_concurrent_completion_cannot_overwrite_a_newer_cursor_epoch() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("cursor-cas.sqlite3").display()
    );
    let pool = db::connect(&database_url).await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "cursor-cas@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let expected = SyncCursor {
        uid_validity: 10,
        last_uid: 50,
    };
    accounts
        .upsert_cursor(account.id, "INBOX", expected)
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let gateway = Arc::new(OrderedCompletionGateway {
        stale_delta: MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 10,
            highest_uid: 100,
            messages: vec![],
        },
        newer_delta: MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 11,
            highest_uid: 7,
            messages: vec![],
        },
        calls: AtomicUsize::new(0),
        cursors: Mutex::new(Vec::new()),
        stale_started: tokio::sync::Notify::new(),
        release_stale: tokio::sync::Notify::new(),
    });
    let service = SyncService::new(
        gateway.clone(),
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );
    let stale_service = service.clone();
    let stale_run = tokio::spawn(async move { stale_service.run(account.id).await });
    gateway.stale_started.notified().await;

    let newer_result = service.run(account.id).await;
    gateway.release_stale.notify_one();
    let stale_result = stale_run.await.unwrap();

    assert_eq!(newer_result.unwrap().imported_count, 0);
    assert!(
        matches!(stale_result, Err(AppError::Conflict { .. })),
        "stale completion must conflict"
    );
    assert_eq!(
        gateway.cursors.lock().unwrap().as_slice(),
        [Some(expected), Some(expected)]
    );
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(SyncCursor {
            uid_validity: 11,
            last_uid: 7,
        })
    );
    assert!(items.list(ItemFilter::default()).await.unwrap().is_empty());
    let mut statuses = sqlx::query_scalar::<_, String>(
        "SELECT status FROM sync_runs WHERE account_id = ? ORDER BY status",
    )
    .bind(account.id.to_string())
    .fetch_all(&pool)
    .await
    .unwrap();
    statuses.sort();
    assert_eq!(statuses, ["failed", "succeeded"]);
}

#[tokio::test]
async fn malformed_mail_and_raw_message_budgets_fail_without_a_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::QQ,
            email: "limits@example.com".to_owned(),
            imap_host: "imap.qq.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let too_many = (1..=1_001)
        .map(|uid| raw_message(uid, b"From: sender@example.com\r\n\r\nbody"))
        .collect();
    let service = SyncService::new(
        Arc::new(FakeImapGateway::new(vec![
            Ok(MailboxDelta {
                rejected_messages: vec![],
                uid_validity: 70,
                highest_uid: 1,
                messages: vec![raw_message(1, b"")],
            }),
            Ok(MailboxDelta {
                rejected_messages: vec![],
                uid_validity: 70,
                highest_uid: 1_001,
                messages: too_many,
            }),
            Ok(MailboxDelta {
                rejected_messages: vec![],
                uid_validity: 70,
                highest_uid: 1,
                messages: vec![raw_message(1, &vec![b'x'; 50 * 1024 * 1024 + 1])],
            }),
        ])),
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items, Arc::new(FakeExtractor)),
    );

    let malformed = service.run(account.id).await.unwrap_err();
    let count = service.run(account.id).await.unwrap_err();
    let size = service.run(account.id).await.unwrap_err();

    assert!(malformed.to_string().contains("could not be parsed"));
    assert!(count.to_string().contains("too many messages"));
    assert!(size.to_string().contains("size limit"));
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        None
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sync_runs WHERE status = 'failed'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn missing_sync_run_cannot_commit_cursor_or_account_success() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool);
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "missing-run@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let missing_run = SyncRun {
        id: Uuid::new_v4(),
        account_id: account.id,
    };

    let error = accounts
        .finish_sync_success(
            &missing_run,
            "INBOX",
            None,
            SyncCursor {
                uid_validity: 80,
                last_uid: 9,
            },
            0,
        )
        .await
        .unwrap_err();

    assert!(matches!(error, AppError::Conflict { .. }));
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        None
    );
    let refreshed = accounts.get(account.id).await.unwrap();
    assert_eq!(refreshed.last_synced_at, None);
    assert_eq!(refreshed.last_error, None);
}

#[tokio::test]
async fn inconsistent_gateway_mailbox_is_rejected_before_import_or_cursor_update() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "wrong-mailbox@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let mut message = raw_message(101, include_bytes!("fixtures/mail/attachment.eml"));
    message.mailbox = "Archive".to_owned();
    let service = SyncService::new(
        Arc::new(FakeImapGateway::new(vec![Ok(MailboxDelta {
            rejected_messages: vec![],
            uid_validity: 90,
            highest_uid: 101,
            messages: vec![message],
        })])),
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items.clone(), Arc::new(FakeExtractor)),
    );

    let error = service.run(account.id).await.unwrap_err();

    assert!(error.to_string().contains("mailbox"));
    assert!(items.list(ItemFilter::default()).await.unwrap().is_empty());
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn corrupt_cursor_read_marks_the_started_run_failed_without_overwriting_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "corrupt-cursor@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sync_cursors (account_id, mailbox, uid_validity, last_uid) \
         VALUES (?, 'INBOX', -1, 7)",
    )
    .bind(account.id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let credentials = Arc::new(MemoryCredentialStore::default());
    credentials
        .set(&account.id.to_string(), "password")
        .unwrap();
    let service = SyncService::new(
        Arc::new(FakeImapGateway::new(vec![])),
        credentials,
        accounts.clone(),
        ImportService::new(
            items.clone(),
            AppPaths::create(directory.path().join("storage")).unwrap(),
        ),
        RecognitionService::new(items, Arc::new(FakeExtractor)),
    );

    let error = service.run(account.id).await.unwrap_err();

    assert!(error.to_string().contains("cursor UIDVALIDITY"));
    assert_eq!(
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT uid_validity, last_uid FROM sync_cursors WHERE account_id = ?",
        )
        .bind(account.id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap(),
        (-1, 7)
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM sync_runs")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "failed"
    );
    assert!(accounts.get(account.id).await.unwrap().last_error.is_some());
}

#[tokio::test]
async fn email_filename_is_a_sanitized_cross_platform_basename_and_url_stays_internal() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let accounts = MailboxAccountRepository::new(pool.clone());
    let items = ItemRepository::new(pool);
    let account = accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "filename@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = ImportService::new(items, paths.clone());

    let imported = service
        .import_email_bytes(
            "../../nested\\escape.pdf",
            b"%PDF-1.7\n%%EOF\n",
            EmailImportSource {
                account_id: account.id,
                mailbox: "INBOX".to_owned(),
                uid_validity: 10,
                uid: 1,
                message_id: Some("filename@example.com".to_owned()),
                part_id: "1".to_owned(),
                received_at: Utc.with_ymd_and_hms(2026, 7, 14, 10, 0, 0).unwrap(),
                rescan: false,
            },
        )
        .await
        .unwrap();
    let ImportOutcome::New(imported) = imported else {
        panic!("first import must be new")
    };

    assert_eq!(imported.original_name, "escape.pdf");
    assert!(Path::new(&imported.original_path).starts_with(&paths.originals));
    let manual_url = directory.path().join("remote.url");
    std::fs::write(&manual_url, b"https://example.com/invoice.pdf\r\n").unwrap();
    let error = service.import_manual(&manual_url).await.unwrap_err();
    assert!(matches!(error, AppError::Validation { ref field, .. } if field == "file"));
    assert_eq!(count_files(&paths.originals), 1);
}

fn raw_message(uid: u32, raw: &[u8]) -> RawMessage {
    RawMessage {
        uid,
        mailbox: "INBOX".to_owned(),
        raw: raw.to_vec(),
        received_at: Utc.with_ymd_and_hms(2026, 7, 14, 10, 0, 0).unwrap(),
    }
}

fn many_download_links_message(count: usize) -> String {
    let mut body = String::from("<html><body>\n");
    for index in 0..count {
        body.push_str(&format!(
            "<a href=\"https://example.com/invoices/invoice-{index:02}.pdf\">Invoice {index:02}</a>\n"
        ));
    }
    body.push_str("</body></html>");
    format!(
        "From: portal@example.com\r\n\
         To: finance@example.com\r\n\
         Date: Tue, 14 Jul 2026 12:00:00 +0800\r\n\
         Message-ID: <link-budget-202@example.com>\r\n\
         Subject: Invoice downloads\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         \r\n\
         {body}"
    )
}

fn count_files(path: &Path) -> usize {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() { count_files(&path) } else { 1 }
        })
        .sum()
}

#[test]
fn provider_defaults_and_native_gateway_force_verified_tls_with_budgets() {
    let gmail = ImapAccountConfig::provider_default(MailboxProvider::Gmail, "g@example.com");
    let qq = ImapAccountConfig::provider_default(MailboxProvider::QQ, "q@example.com");

    assert_eq!(
        (gmail.host.as_str(), gmail.port, gmail.tls),
        ("imap.gmail.com", 993, true)
    );
    assert_eq!(
        (qq.host.as_str(), qq.port, qq.tls),
        ("imap.qq.com", 993, true)
    );

    let settings = NativeTlsImapGateway::default().settings();
    assert!(settings.verify_certificates);
    assert!(settings.connect_timeout <= std::time::Duration::from_secs(30));
    assert!(settings.read_timeout <= std::time::Duration::from_secs(60));
    assert!(settings.write_timeout <= std::time::Duration::from_secs(60));
    assert!(settings.max_messages > 0);
    assert!(settings.max_message_bytes > 0);
    assert!(settings.max_total_bytes >= settings.max_message_bytes);
}
