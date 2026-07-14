use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::accounts::{
    MailboxAccountRepository, MailboxProvider, NewMailboxAccount, SyncCursor, SyncRun,
};
use invoice_reimbursement::db::items::{ItemFilter, ItemRepository};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{ConfirmationStatus, RecognitionStatus};
use invoice_reimbursement::infra::credentials::{CredentialStore, MemoryCredentialStore};
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::infra::imap::{
    ImapAccountConfig, ImapGateway, MailboxDelta, NativeTlsImapGateway, RawMessage,
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
        uid_validity: 10,
        highest_uid: 102,
        messages: vec![
            raw_message(101, include_bytes!("fixtures/mail/attachment.eml")),
            raw_message(102, include_bytes!("fixtures/mail/inline-image.eml")),
        ],
    };
    let second_delta = MailboxDelta {
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
            uid_validity: 10,
            highest_uid: 101,
            messages: vec![message.clone()],
        }),
        Ok(MailboxDelta {
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
    let first = first.unwrap();
    let second = second.unwrap();

    assert_eq!(first.imported_count + second.imported_count, 1);
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
                uid_validity: 70,
                highest_uid: 1,
                messages: vec![raw_message(1, b"")],
            }),
            Ok(MailboxDelta {
                uid_validity: 70,
                highest_uid: 1_001,
                messages: too_many,
            }),
            Ok(MailboxDelta {
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
                uid: 1,
                message_id: Some("filename@example.com".to_owned()),
                part_id: "1".to_owned(),
                received_at: Utc.with_ymd_and_hms(2026, 7, 14, 10, 0, 0).unwrap(),
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
