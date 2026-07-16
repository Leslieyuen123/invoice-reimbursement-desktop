use chrono::{NaiveDate, TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::accounts::{
    MailboxAccountRepository, MailboxProvider, NewMailboxAccount, SyncCursor,
};
use invoice_reimbursement::db::batches::BatchRepository;
use invoice_reimbursement::db::items::{ItemFilter, ItemPatch, ItemRepository, NewItemRecord};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{
    BatchStatus, Category, ConfirmationStatus, DedupeStatus, ItemStatus, NewBatch,
    RecognitionStatus, SourceType,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteQueryResult};
use sqlx::{Row, SqlitePool};
use std::borrow::Cow;
use std::str::FromStr;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn connect_runs_migrations_and_creates_application_tables() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");

    let tables = sqlx::query_scalar::<_, String>(
        "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .expect("schema tables should be queryable");

    for table in [
        "batches",
        "items",
        "mailbox_accounts",
        "pending_account_save_cleanups",
        "pending_account_saves",
        "pending_exports",
        "settings",
        "sync_cursors",
        "sync_runs",
    ] {
        assert!(
            tables.iter().any(|name| name == table),
            "missing table {table}; found {tables:?}"
        );
    }
}

#[tokio::test]
async fn pending_export_journal_has_recovery_identity_paths_and_state() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let columns = sqlx::query_scalar::<_, String>(
        "SELECT name FROM pragma_table_info('pending_exports') ORDER BY cid",
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    assert_eq!(
        columns,
        vec![
            "operation_id",
            "batch_id",
            "staging_component",
            "final_component",
            "exported_at",
            "state",
            "interrupted",
            "last_error",
            "created_at",
            "updated_at",
        ]
    );
}

#[tokio::test]
async fn migration_0006_moves_legacy_committed_markers_to_independent_cleanup_rows() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let current = sqlx::migrate!("./migrations");
    let through_0005 = sqlx::migrate::Migrator {
        migrations: Cow::Owned(current.iter().take(5).cloned().collect()),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    through_0005.run(&pool).await.unwrap();
    let account_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    insert_mailbox_account(&pool, account_id).await;
    sqlx::query(
        "INSERT INTO pending_account_saves (\
            operation_id, account_id, phase, is_update, provider, email, imap_host, imap_port, \
            enabled, sync_interval_minutes, created_at, updated_at\
         ) VALUES (?, ?, 'committed', 1, 'gmail', 'legacy-cleanup@example.com', \
            'imap.gmail.com', 993, 1, 15, '2026-07-15T10:00:00Z', \
            '2026-07-15T10:01:00Z')",
    )
    .bind(operation_id.to_string())
    .bind(account_id.to_string())
    .execute(&pool)
    .await
    .unwrap();

    current.run(&pool).await.unwrap();

    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT operation_id FROM pending_account_save_cleanups",)
            .fetch_one(&pool)
            .await
            .unwrap(),
        operation_id.to_string()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_account_saves")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT name FROM pragma_table_info('pending_account_save_cleanups') ORDER BY cid",
        )
        .fetch_all(&pool)
        .await
        .unwrap(),
        vec!["operation_id", "created_at", "updated_at"]
    );
}

#[tokio::test]
async fn migrations_upgrade_database_through_0004_with_pending_account_saves() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let current = sqlx::migrate!("./migrations");
    let through_0004 = sqlx::migrate::Migrator {
        migrations: Cow::Owned(current.iter().take(4).cloned().collect()),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    through_0004.run(&pool).await.unwrap();
    let existing_account_id = Uuid::new_v4();
    insert_mailbox_account(&pool, existing_account_id).await;

    current.run(&pool).await.unwrap();

    let pending_account_columns = sqlx::query_scalar::<_, String>(
        "SELECT name FROM pragma_table_info('pending_account_saves') ORDER BY cid",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        pending_account_columns,
        vec![
            "operation_id",
            "account_id",
            "phase",
            "is_update",
            "provider",
            "email",
            "imap_host",
            "imap_port",
            "enabled",
            "sync_interval_minutes",
            "created_at",
            "updated_at",
        ]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mailbox_accounts WHERE id = ?")
            .bind(existing_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn connect_enables_foreign_key_enforcement() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");

    let foreign_keys = sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
        .fetch_one(&pool)
        .await
        .expect("foreign key pragma should be queryable");

    assert_eq!(foreign_keys, 1);
}

#[tokio::test]
async fn retry_state_failures_reject_values_above_i32_max() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let account_id = Uuid::new_v4();
    insert_mailbox_account(&pool, account_id).await;

    let error = sqlx::query(
        "INSERT INTO sync_retry_states (\
            account_id, failures, next_retry_at, suspended, updated_at\
         ) VALUES (?, ?, ?, 0, ?)",
    )
    .bind(account_id.to_string())
    .bind(2_147_483_648_i64)
    .bind("2026-07-15T12:01:00Z")
    .bind(Utc::now().to_rfc3339())
    .execute(&pool)
    .await
    .unwrap_err();

    assert!(matches!(error, sqlx::Error::Database(_)));
}

#[tokio::test]
async fn migrations_upgrade_original_retry_schema_and_preserve_state() {
    const ORIGINAL_0003: &str = r#"CREATE TABLE sync_retry_states (
    account_id TEXT PRIMARY KEY NOT NULL,
    failures INTEGER NOT NULL CHECK (failures >= 0),
    next_retry_at TEXT,
    suspended INTEGER NOT NULL CHECK (suspended IN (0, 1)),
    updated_at TEXT NOT NULL,
    FOREIGN KEY (account_id) REFERENCES mailbox_accounts(id) ON DELETE CASCADE,
    CHECK (
        (suspended = 1 AND next_retry_at IS NULL) OR
        (suspended = 0 AND next_retry_at IS NOT NULL)
    )
);
"#;
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let current = sqlx::migrate!("./migrations");
    assert_eq!(
        ORIGINAL_0003.as_bytes(),
        current.iter().nth(2).unwrap().sql.as_bytes()
    );
    let mut legacy_migrations = current.iter().take(2).cloned().collect::<Vec<_>>();
    legacy_migrations.push(sqlx::migrate::Migration::new(
        3,
        Cow::Borrowed("sync retry states"),
        sqlx::migrate::MigrationType::Simple,
        Cow::Borrowed(ORIGINAL_0003),
        false,
    ));
    let legacy = sqlx::migrate::Migrator {
        migrations: Cow::Owned(legacy_migrations),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    legacy.run(&pool).await.unwrap();
    let active_id = Uuid::new_v4();
    let suspended_id = Uuid::new_v4();
    insert_mailbox_account(&pool, active_id).await;
    insert_mailbox_account(&pool, suspended_id).await;
    sqlx::query(
        "INSERT INTO sync_retry_states \
         (account_id, failures, next_retry_at, suspended, updated_at) VALUES \
         (?, 7, '2026-07-15T10:01:00Z', 0, '2026-07-15T10:00:00Z'), \
         (?, 0, NULL, 1, '2026-07-15T11:00:00Z')",
    )
    .bind(active_id.to_string())
    .bind(suspended_id.to_string())
    .execute(&pool)
    .await
    .unwrap();

    current.run(&pool).await.unwrap();

    let active = sqlx::query_as::<_, (i64, Option<String>, i64, String)>(
        "SELECT failures, next_retry_at, suspended, updated_at \
         FROM sync_retry_states WHERE account_id = ?",
    )
    .bind(active_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let suspended = sqlx::query_as::<_, (i64, Option<String>, i64, String)>(
        "SELECT failures, next_retry_at, suspended, updated_at \
         FROM sync_retry_states WHERE account_id = ?",
    )
    .bind(suspended_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        active,
        (
            7,
            Some("2026-07-15T10:01:00Z".to_owned()),
            0,
            "2026-07-15T10:00:00Z".to_owned()
        )
    );
    assert_eq!(suspended, (0, None, 1, "2026-07-15T11:00:00Z".to_owned()));
}

#[tokio::test]
async fn mailbox_sync_repository_maps_real_sqlite_busy_errors_as_retryable() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("accounts-busy.sqlite3").display()
    );
    let options = SqliteConnectOptions::from_str(&database_url)
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Delete)
        .busy_timeout(Duration::ZERO);
    let setup_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run(&setup_pool)
        .await
        .unwrap();
    let setup_accounts = MailboxAccountRepository::new(setup_pool.clone());
    let account = setup_accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "busy@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let cursor = SyncCursor {
        uid_validity: 10,
        last_uid: 50,
    };
    setup_accounts
        .upsert_cursor(account.id, "INBOX", cursor)
        .await
        .unwrap();
    let run = setup_accounts.begin_sync_run(account.id).await.unwrap();
    drop(setup_accounts);
    setup_pool.close().await;

    let lock_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    let mut lock = lock_pool.acquire().await.unwrap();
    let repository_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let accounts = MailboxAccountRepository::new(repository_pool.clone());
    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut *lock)
        .await
        .unwrap();

    let errors = [
        accounts.get_cursor(account.id, "INBOX").await.unwrap_err(),
        accounts.begin_sync_run(account.id).await.unwrap_err(),
        accounts
            .finish_sync_success(
                &run,
                "INBOX",
                Some(cursor),
                SyncCursor {
                    uid_validity: 10,
                    last_uid: 60,
                },
                0,
            )
            .await
            .unwrap_err(),
        accounts
            .finish_sync_failure(&run, "locked")
            .await
            .unwrap_err(),
    ];

    for error in errors {
        assert!(
            matches!(
                error,
                AppError::External {
                    ref service,
                    retryable: true,
                    ..
                } if service == "database"
            ),
            "unexpected busy mapping: {error:?}"
        );
    }

    sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();
    assert_eq!(
        accounts.get_cursor(account.id, "INBOX").await.unwrap(),
        Some(cursor)
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM sync_runs WHERE id = ?")
            .bind(run.id.to_string())
            .fetch_one(&repository_pool)
            .await
            .unwrap(),
        "running"
    );
}

#[tokio::test]
async fn item_email_lookups_map_real_sqlite_busy_errors_as_retryable() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("item-lookups-busy.sqlite3").display()
    );
    let options = SqliteConnectOptions::from_str(&database_url)
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Delete)
        .busy_timeout(Duration::ZERO);
    let setup_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run(&setup_pool)
        .await
        .unwrap();
    let account_id = Uuid::new_v4();
    insert_mailbox_account(&setup_pool, account_id).await;
    let setup_items = ItemRepository::new(setup_pool.clone());
    let mut current = sample_item("busy-current", "sha256-busy-current");
    current.source_type = SourceType::Email;
    current.source_account_id = Some(account_id);
    current.source_mailbox = Some("INBOX".to_owned());
    current.source_uid_validity = Some(10);
    current.source_uid = Some(42);
    current.source_message_id = Some("busy-current@example.com".to_owned());
    current.source_part_id = Some("2".to_owned());
    setup_items.insert(&current).await.unwrap();
    let mut legacy = sample_item("busy-legacy", "sha256-busy-legacy");
    legacy.source_type = SourceType::Email;
    legacy.source_account_id = Some(account_id);
    legacy.source_mailbox = Some("INBOX".to_owned());
    legacy.source_uid_validity = Some(11);
    legacy.source_uid = Some(43);
    legacy.source_message_id = Some("busy-legacy@example.com".to_owned());
    legacy.source_part_id = Some("3".to_owned());
    setup_items.insert(&legacy).await.unwrap();
    sqlx::query("UPDATE items SET source_uid_validity = 0 WHERE id = ?")
        .bind(legacy.id.to_string())
        .execute(&setup_pool)
        .await
        .unwrap();
    drop(setup_items);
    setup_pool.close().await;

    let lock_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    let mut lock = lock_pool.acquire().await.unwrap();
    let repository_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let items = ItemRepository::new(repository_pool);
    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut *lock)
        .await
        .unwrap();

    let errors = [
        items
            .find_email_part(account_id, "INBOX", 10, 42, "2")
            .await
            .unwrap_err(),
        items
            .find_rescanned_email_part(
                account_id,
                "INBOX",
                11,
                Some("busy-current@example.com"),
                "2",
                "sha256-busy-current",
            )
            .await
            .unwrap_err(),
        items
            .find_legacy_email_part(
                account_id,
                "INBOX",
                Some("busy-legacy@example.com"),
                "3",
                "sha256-busy-legacy",
            )
            .await
            .unwrap_err(),
    ];

    for error in errors {
        assert!(
            matches!(
                error,
                AppError::External {
                    ref service,
                    retryable: true,
                    ..
                } if service == "database"
            ),
            "unexpected busy mapping: {error:?}"
        );
    }

    sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();
}

#[tokio::test]
async fn private_memory_url_aliases_keep_schema_access_on_one_connection() {
    for database_url in ["sqlite://:memory:", "sqlite://?mode=memory"] {
        let pool = db::connect(database_url)
            .await
            .unwrap_or_else(|error| panic!("{database_url} should connect: {error}"));

        assert_eq!(
            pool.options().get_max_connections(),
            1,
            "{database_url} must not create isolated private databases"
        );

        for _ in 0..3 {
            let mut connection = pool
                .acquire()
                .await
                .unwrap_or_else(|error| panic!("{database_url} should acquire: {error}"));
            let item_table_count = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'items'",
            )
            .fetch_one(&mut *connection)
            .await
            .unwrap_or_else(|error| panic!("{database_url} should retain its schema: {error}"));

            assert_eq!(item_table_count, 1);
        }
    }
}

#[tokio::test]
async fn percent_encoded_memory_mode_uses_one_migrated_connection() {
    let database_url = "sqlite://?mode=mem%6Fry&cache=private";
    let pool = db::connect(database_url)
        .await
        .expect("percent-encoded memory URL should connect");

    assert_eq!(pool.options().get_max_connections(), 1);
    for _ in 0..3 {
        let mut connection = pool
            .acquire()
            .await
            .expect("memory connection should acquire");
        let item_table_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'items'",
        )
        .fetch_one(&mut *connection)
        .await
        .expect("migrated schema should remain accessible");

        assert_eq!(item_table_count, 1);
    }
}

#[tokio::test]
async fn file_database_reopens_with_persisted_data_and_connection_pragmas() {
    let directory = tempfile::tempdir().expect("temporary database directory should create");
    let database_path = directory.path().join("invoice-reimbursement.sqlite3");
    let database_url = format!("sqlite://{}", database_path.display());

    let pool = db::connect(&database_url)
        .await
        .expect("file database should connect");
    assert_eq!(pool.options().get_max_connections(), 5);
    assert_eq!(pragma_string(&pool, "PRAGMA journal_mode").await, "wal");
    assert_eq!(pragma_i64(&pool, "PRAGMA busy_timeout").await, 5_000);
    assert_eq!(pragma_i64(&pool, "PRAGMA foreign_keys").await, 1);

    sqlx::query("INSERT INTO settings (key, value_json, updated_at) VALUES (?, ?, ?)")
        .bind("persisted")
        .bind(r#"{"enabled":true}"#)
        .bind("2026-07-13T10:00:00Z")
        .execute(&pool)
        .await
        .expect("setting should persist");
    pool.close().await;

    let reopened = db::connect(&database_url)
        .await
        .expect("file database should reopen");
    let value = sqlx::query_scalar::<_, String>("SELECT value_json FROM settings WHERE key = ?")
        .bind("persisted")
        .fetch_one(&reopened)
        .await
        .expect("persisted setting should be readable");

    assert_eq!(value, r#"{"enabled":true}"#);
    assert_eq!(pragma_i64(&reopened, "PRAGMA foreign_keys").await, 1);
}

#[tokio::test]
async fn file_name_ending_in_memory_uses_disk_pool_and_reopens() {
    let directory = tempfile::tempdir().expect("temporary database directory should create");
    let database_path = directory.path().join("invoice:memory:");
    let database_url = format!("sqlite://{}", database_path.display());

    let pool = db::connect(&database_url)
        .await
        .expect("file database ending in :memory: should connect");
    assert_eq!(pool.options().get_max_connections(), 5);
    assert_eq!(pragma_string(&pool, "PRAGMA journal_mode").await, "wal");
    sqlx::query("INSERT INTO settings (key, value_json, updated_at) VALUES (?, ?, ?)")
        .bind("disk-memory-suffix")
        .bind(r#"{"persisted":true}"#)
        .bind("2026-07-13T10:00:00Z")
        .execute(&pool)
        .await
        .expect("setting should persist to disk");
    pool.close().await;

    assert!(database_path.is_file());
    let reopened = db::connect(&database_url)
        .await
        .expect("file database ending in :memory: should reopen");
    let value = sqlx::query_scalar::<_, String>("SELECT value_json FROM settings WHERE key = ?")
        .bind("disk-memory-suffix")
        .fetch_one(&reopened)
        .await
        .expect("persisted setting should be readable");

    assert_eq!(value, r#"{"persisted":true}"#);
}

#[tokio::test]
async fn mailbox_account_schema_uses_explicit_imap_column_names() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");

    let rows = sqlx::query("PRAGMA table_info(mailbox_accounts)")
        .fetch_all(&pool)
        .await
        .expect("mailbox account schema should be queryable");
    let columns = rows
        .iter()
        .map(|row| {
            row.try_get::<String, _>("name")
                .expect("column should have a name")
        })
        .collect::<Vec<_>>();

    assert!(columns.iter().any(|column| column == "imap_host"));
    assert!(columns.iter().any(|column| column == "imap_port"));
    assert!(!columns.iter().any(|column| column == "host"));
    assert!(!columns.iter().any(|column| column == "port"));
}

#[tokio::test]
async fn sync_run_schema_uses_error_message_column_name() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");

    let rows = sqlx::query("PRAGMA table_info(sync_runs)")
        .fetch_all(&pool)
        .await
        .expect("sync run schema should be queryable");
    let columns = rows
        .iter()
        .map(|row| {
            row.try_get::<String, _>("name")
                .expect("column should have a name")
        })
        .collect::<Vec<_>>();

    assert!(columns.iter().any(|column| column == "error_message"));
    assert!(!columns.iter().any(|column| column == "error"));
}

#[tokio::test]
async fn work_queue_index_prioritizes_dedupe_before_processing_statuses() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");

    let rows = sqlx::query("PRAGMA index_info(idx_items_work_queue)")
        .fetch_all(&pool)
        .await
        .expect("work queue index should be queryable");
    let columns = rows
        .iter()
        .map(|row| {
            row.try_get::<String, _>("name")
                .expect("index column should have a name")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        columns,
        vec!["dedupe_status", "recognition_status", "confirmation_status"]
    );
}

#[tokio::test]
async fn batches_reject_names_that_are_empty_after_trimming() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");

    let result = sqlx::query(
        "INSERT INTO batches \
         (id, name, start_date, end_date, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind("batch-empty-name")
    .bind("   ")
    .bind("2026-07-01")
    .bind("2026-07-31")
    .bind("2026-07-13T10:00:00Z")
    .bind("2026-07-13T10:00:00Z")
    .execute(&pool)
    .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn items_reject_values_outside_each_status_enum() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");

    for (id, recognition, confirmation, dedupe) in [
        ("bad-recognition", "unknown", "pending", "unique"),
        ("bad-confirmation", "pending", "unknown", "unique"),
        ("bad-dedupe", "pending", "pending", "unknown"),
    ] {
        let result = insert_item(&pool, id, recognition, confirmation, dedupe, None).await;
        assert!(
            result.is_err(),
            "invalid status values were accepted for {id}"
        );
    }
}

#[tokio::test]
async fn items_reject_negative_amounts() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");

    let result = insert_item(
        &pool,
        "negative-amount",
        "pending",
        "pending",
        "unique",
        Some(-1),
    )
    .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn item_schema_rejects_email_items_without_part_provenance() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");

    let result = sqlx::query(
        "INSERT INTO items (\
            id, original_name, original_path, sha256, mime_type, source_type, fetched_at, \
            created_at, updated_at\
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("email-without-provenance")
    .bind("missing.pdf")
    .bind("/invoices/missing.pdf")
    .bind("sha256-missing-provenance")
    .bind("application/pdf")
    .bind("email")
    .bind("2026-07-13T10:00:00Z")
    .bind("2026-07-13T10:00:00Z")
    .bind("2026-07-13T10:00:00Z")
    .execute(&pool)
    .await;

    assert!(
        result.is_err(),
        "email provenance must be enforced by SQLite"
    );
}

#[tokio::test]
async fn mailbox_accounts_reject_sync_intervals_outside_allowed_range() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");

    for (id, email, interval) in [
        ("too-frequent", "frequent@example.com", 4),
        ("too-infrequent", "infrequent@example.com", 1441),
    ] {
        let result = sqlx::query(
            "INSERT INTO mailbox_accounts \
             (id, provider, email, imap_host, imap_port, sync_interval_minutes, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("gmail")
        .bind(email)
        .bind("imap.example.com")
        .bind(993)
        .bind(interval)
        .bind("2026-07-13T10:00:00Z")
        .bind("2026-07-13T10:00:00Z")
        .execute(&pool)
        .await;

        assert!(
            result.is_err(),
            "invalid sync interval {interval} was accepted"
        );
    }
}

#[tokio::test]
async fn mailbox_account_schema_rejects_blank_endpoints_and_invalid_ports() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");

    for (id, email, imap_host, imap_port) in [
        ("blank-email", "   ", "imap.example.com", 993),
        ("blank-host", "blank-host@example.com", "   ", 993),
        ("zero-port", "zero-port@example.com", "imap.example.com", 0),
        (
            "high-port",
            "high-port@example.com",
            "imap.example.com",
            65_536,
        ),
    ] {
        let result = sqlx::query(
            "INSERT INTO mailbox_accounts \
             (id, provider, email, imap_host, imap_port, sync_interval_minutes, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("gmail")
        .bind(email)
        .bind(imap_host)
        .bind(imap_port)
        .bind(15)
        .bind("2026-07-13T10:00:00Z")
        .bind("2026-07-13T10:00:00Z")
        .execute(&pool)
        .await;

        assert!(
            result.is_err(),
            "invalid endpoint was accepted: email={email:?}, host={imap_host:?}, port={imap_port}"
        );
    }
}

#[tokio::test]
async fn item_repository_inserts_and_finds_a_fully_typed_item_by_hash() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let new_item = sample_item("insert-find", "sha256-insert-find");

    let inserted = repository
        .insert(&new_item)
        .await
        .expect("item should insert");
    let found = repository
        .find_by_hash(&new_item.sha256)
        .await
        .expect("hash lookup should succeed")
        .expect("inserted item should be found");

    assert_eq!(inserted, found);
    assert_eq!(found.id, new_item.id);
    assert_eq!(found.original_name, new_item.original_name);
    assert_eq!(found.original_path, new_item.original_path);
    assert_eq!(found.normalized_pdf_path, new_item.normalized_pdf_path);
    assert_eq!(found.sha256, new_item.sha256);
    assert_eq!(found.mime_type, new_item.mime_type);
    assert_eq!(found.source_type, SourceType::ManualUpload);
    assert_eq!(found.source_account_id, new_item.source_account_id);
    assert_eq!(found.source_mailbox, new_item.source_mailbox);
    assert_eq!(found.source_uid_validity, new_item.source_uid_validity);
    assert_eq!(found.source_uid, new_item.source_uid);
    assert_eq!(found.source_message_id, new_item.source_message_id);
    assert_eq!(found.source_part_id, new_item.source_part_id);
    assert_eq!(found.fetched_at, new_item.fetched_at);
    assert_eq!(found.invoice_date, new_item.invoice_date);
    assert_eq!(found.suggested_period, new_item.suggested_period);
    assert_eq!(found.batch_id, new_item.batch_id);
    assert_eq!(found.suggested_category, Some(Category::Transport));
    assert_eq!(found.final_category, Some(Category::Dining));
    assert_eq!(found.amount_cents, Some(12_345));
    assert_eq!(found.currency, "CNY");
    assert_eq!(found.city.as_deref(), Some("Shanghai"));
    assert_eq!(found.company.as_deref(), Some("Example Co"));
    assert_eq!(found.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(found.confirmation_status, ConfirmationStatus::Confirmed);
    assert_eq!(found.dedupe_status, DedupeStatus::Resolved);
    assert_eq!(found.duplicate_of_id, new_item.duplicate_of_id);
    assert_eq!(found.note.as_deref(), Some("client visit"));
    assert_eq!(found.event_tag.as_deref(), Some("summit"));
    assert_eq!(found.project_tag.as_deref(), Some("P-2026"));
    assert_eq!(found.created_at, new_item.created_at);
    assert_eq!(found.updated_at, new_item.updated_at);
}

#[tokio::test]
async fn item_repository_rolls_back_when_typed_insert_readback_fails() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    sqlx::query(
        "CREATE TRIGGER corrupt_item_readback AFTER INSERT ON items \
         BEGIN UPDATE items SET created_at = 'not-a-date' WHERE id = NEW.id; END",
    )
    .execute(&pool)
    .await
    .expect("corrupting trigger should create");
    let repository = ItemRepository::new(pool.clone());

    let error = repository
        .insert(&sample_item("corrupt-readback", "sha256-corrupt-readback"))
        .await
        .expect_err("invalid typed readback should fail");

    assert!(matches!(error, AppError::Internal { .. }));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items")
            .fetch_one(&pool)
            .await
            .expect("item count should query"),
        0
    );
}

#[tokio::test]
async fn deduplicated_insert_prefers_unique_then_earliest_canonical_root() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let hash = "sha256-canonical-preference";
    let mut resolved_earliest = sample_item("resolved-earliest", hash);
    resolved_earliest.created_at -= chrono::Duration::hours(2);
    let mut unique_earliest = sample_item("unique-earliest", hash);
    unique_earliest.dedupe_status = DedupeStatus::Unique;
    unique_earliest.created_at -= chrono::Duration::hours(1);
    let mut unique_later = sample_item("unique-later", hash);
    unique_later.dedupe_status = DedupeStatus::Unique;
    unique_later.created_at += chrono::Duration::hours(1);
    for root in [&resolved_earliest, &unique_later, &unique_earliest] {
        repository
            .insert(root)
            .await
            .expect("canonical fixture should insert");
    }
    let incoming = sample_item("canonical-incoming", hash);

    let inserted = repository
        .insert_deduplicated(incoming)
        .await
        .expect("deduplicated item should insert");

    assert_eq!(inserted.dedupe_status, DedupeStatus::SuspectedDuplicate);
    assert_eq!(inserted.duplicate_of_id, Some(unique_earliest.id));
}

#[tokio::test]
async fn item_repository_maps_duplicate_email_parts_to_an_actionable_conflict() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let account_id = Uuid::new_v4();
    insert_mailbox_account(&pool, account_id).await;
    let repository = ItemRepository::new(pool);
    let mut first = sample_item("email-first", "sha256-email-first");
    first.source_type = SourceType::Email;
    first.source_account_id = Some(account_id);
    first.source_mailbox = Some("INBOX".to_owned());
    first.source_uid_validity = Some(10);
    first.source_uid = Some(42);
    first.source_message_id = Some("message-first".to_owned());
    first.source_part_id = Some("2".to_owned());
    let mut duplicate = sample_item("email-duplicate", "sha256-email-duplicate");
    duplicate.source_type = SourceType::Email;
    duplicate.source_account_id = Some(account_id);
    duplicate.source_mailbox = Some("INBOX".to_owned());
    duplicate.source_uid_validity = Some(10);
    duplicate.source_uid = Some(42);
    duplicate.source_message_id = Some("message-duplicate".to_owned());
    duplicate.source_part_id = Some("2".to_owned());

    repository
        .insert(&first)
        .await
        .expect("first email part should insert");
    let error = repository
        .insert(&duplicate)
        .await
        .expect_err("duplicate email part should conflict");

    assert!(
        matches!(error, AppError::Conflict { ref message } if message.contains("email attachment")),
        "unexpected error: {error:?}"
    );
}

#[tokio::test]
async fn deleting_mailbox_account_preserves_imported_email_provenance() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let account_id = Uuid::new_v4();
    insert_mailbox_account(&pool, account_id).await;
    let repository = ItemRepository::new(pool.clone());
    let mut email_item = sample_item("preserved-email", "sha256-preserved-email");
    email_item.source_type = SourceType::Email;
    email_item.source_account_id = Some(account_id);
    email_item.source_mailbox = Some("INBOX".to_owned());
    email_item.source_uid_validity = Some(10);
    email_item.source_uid = Some(42);
    email_item.source_part_id = Some("2".to_owned());
    repository
        .insert(&email_item)
        .await
        .expect("email item should insert");

    let deleted = sqlx::query("DELETE FROM mailbox_accounts WHERE id = ?")
        .bind(account_id.to_string())
        .execute(&pool)
        .await
        .expect("mailbox account should delete without rewriting provenance");
    let retained = repository
        .find_by_hash(&email_item.sha256)
        .await
        .expect("item lookup should succeed")
        .expect("imported email item should remain");

    assert_eq!(deleted.rows_affected(), 1);
    assert_eq!(retained.source_account_id, Some(account_id));
    assert_eq!(retained.source_mailbox.as_deref(), Some("INBOX"));
    assert_eq!(retained.source_uid_validity, Some(10));
    assert_eq!(retained.source_uid, Some(42));
    assert_eq!(retained.source_part_id.as_deref(), Some("2"));
}

#[tokio::test]
async fn item_repository_validates_email_provenance_before_sql() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool.clone());
    pool.close().await;

    let mut valid_email = sample_item("valid-email-source", "sha256-valid-email-source");
    valid_email.source_type = SourceType::Email;
    valid_email.source_account_id = Some(Uuid::new_v4());
    valid_email.source_mailbox = Some("INBOX".to_owned());
    valid_email.source_uid_validity = Some(10);
    valid_email.source_uid = Some(42);
    valid_email.source_part_id = Some("2".to_owned());

    let mut invalid_items = Vec::new();
    invalid_items.push({
        let mut item = valid_email.clone();
        item.source_account_id = None;
        item
    });
    invalid_items.push({
        let mut item = valid_email.clone();
        item.source_account_id = Some(Uuid::nil());
        item
    });
    invalid_items.push({
        let mut item = valid_email.clone();
        item.source_mailbox = None;
        item
    });
    invalid_items.push({
        let mut item = valid_email.clone();
        item.source_mailbox = Some("   ".to_owned());
        item
    });
    invalid_items.push({
        let mut item = valid_email.clone();
        item.source_uid_validity = None;
        item
    });
    invalid_items.push({
        let mut item = valid_email.clone();
        item.source_uid_validity = Some(0);
        item
    });
    invalid_items.push({
        let mut item = valid_email.clone();
        item.source_uid = None;
        item
    });
    invalid_items.push({
        let mut item = valid_email.clone();
        item.source_uid = Some(0);
        item
    });
    invalid_items.push({
        let mut item = valid_email.clone();
        item.source_part_id = None;
        item
    });
    invalid_items.push({
        let mut item = valid_email;
        item.source_part_id = Some("   ".to_owned());
        item
    });

    for item in invalid_items {
        let error = repository
            .insert(&item)
            .await
            .expect_err("invalid email provenance should be rejected");
        assert_eq!(
            error,
            AppError::Validation {
                field: "source".to_owned(),
                message:
                    "email source requires an account, mailbox, positive UIDVALIDITY and UID, and part ID"
                        .to_owned(),
            }
        );
    }
}

#[tokio::test]
async fn item_repository_filters_all_derived_statuses_with_dedupe_precedence() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let cases = [
        (
            "suspected",
            RecognitionStatus::Failed,
            ConfirmationStatus::Confirmed,
            DedupeStatus::SuspectedDuplicate,
            ItemStatus::SuspectedDuplicate,
        ),
        (
            "failed",
            RecognitionStatus::Failed,
            ConfirmationStatus::Confirmed,
            DedupeStatus::Resolved,
            ItemStatus::RecognitionFailed,
        ),
        (
            "recognition",
            RecognitionStatus::Pending,
            ConfirmationStatus::Confirmed,
            DedupeStatus::Unique,
            ItemStatus::PendingRecognition,
        ),
        (
            "confirmation",
            RecognitionStatus::Succeeded,
            ConfirmationStatus::Pending,
            DedupeStatus::Resolved,
            ItemStatus::PendingConfirmation,
        ),
        (
            "ready",
            RecognitionStatus::Succeeded,
            ConfirmationStatus::Confirmed,
            DedupeStatus::Unique,
            ItemStatus::Ready,
        ),
    ];

    for (suffix, recognition, confirmation, dedupe, _) in cases {
        let mut item = sample_item(suffix, &format!("sha256-{suffix}"));
        item.recognition_status = recognition;
        item.confirmation_status = confirmation;
        item.dedupe_status = dedupe;
        repository.insert(&item).await.expect("item should insert");
    }

    for (expected_suffix, _, _, _, status) in cases {
        let items = repository
            .list_bounded_for_tests(ItemFilter {
                status: Some(status),
                ..ItemFilter::default()
            })
            .await
            .expect("status filter should succeed");

        assert_eq!(items.len(), 1, "unexpected matches for {status:?}");
        assert_eq!(items[0].status(), status);
        assert!(items[0].original_name.contains(expected_suffix));
    }
}

#[tokio::test]
async fn item_repository_combines_typed_filters_text_search_and_deterministic_ordering() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let batch_id = Uuid::new_v4();
    insert_batch(&pool, batch_id).await;
    let account_id = Uuid::new_v4();
    insert_mailbox_account(&pool, account_id).await;
    let repository = ItemRepository::new(pool);
    let mut target = sample_item("Quarterly-Invoice", "sha256-filter-target");
    target.id = Uuid::from_u128(3);
    target.source_type = SourceType::Email;
    target.source_account_id = Some(account_id);
    target.source_mailbox = Some("INBOX".to_owned());
    target.source_uid_validity = Some(10);
    target.source_uid = Some(3);
    target.source_part_id = Some("1".to_owned());
    target.suggested_period = Some("2026-Q3".to_owned());
    target.batch_id = Some(batch_id);
    target.suggested_category = Some(Category::Transport);
    target.final_category = Some(Category::Dining);
    target.company = Some("Acme Holdings".to_owned());
    target.city = Some("Shenzhen".to_owned());
    target.note = Some("Board Meeting".to_owned());
    target.created_at = Utc
        .with_ymd_and_hms(2026, 7, 13, 4, 0, 0)
        .single()
        .expect("valid target time");
    target.updated_at = target.created_at;
    repository
        .insert(&target)
        .await
        .expect("target item should insert");

    let mut same_time_lower_id = sample_item("noise-same-time", "sha256-noise-same-time");
    same_time_lower_id.id = Uuid::from_u128(2);
    same_time_lower_id.final_category = Some(Category::Accommodation);
    same_time_lower_id.created_at = target.created_at;
    same_time_lower_id.updated_at = target.created_at;
    repository
        .insert(&same_time_lower_id)
        .await
        .expect("same-time noise should insert");

    let mut older = sample_item("noise-older", "sha256-noise-older");
    older.id = Uuid::from_u128(1);
    older.final_category = Some(Category::Hospitality);
    older.created_at = Utc
        .with_ymd_and_hms(2026, 7, 13, 3, 0, 0)
        .single()
        .expect("valid older time");
    older.updated_at = older.created_at;
    repository
        .insert(&older)
        .await
        .expect("older noise should insert");

    let all = repository
        .list_bounded_for_tests(ItemFilter::default())
        .await
        .expect("unfiltered list should succeed");
    assert_eq!(
        all.iter().map(|item| item.id).collect::<Vec<_>>(),
        vec![target.id, same_time_lower_id.id, older.id]
    );

    for query in ["quarterly", "ACME", "shenzhen", "board meeting"] {
        let matches = repository
            .list_bounded_for_tests(ItemFilter {
                query: Some(query.to_owned()),
                ..ItemFilter::default()
            })
            .await
            .expect("text filter should succeed");
        assert_eq!(matches.len(), 1, "unexpected matches for query {query}");
        assert_eq!(matches[0].id, target.id);
    }

    for filter in [
        ItemFilter {
            suggested_period: Some("2026-Q3".to_owned()),
            ..ItemFilter::default()
        },
        ItemFilter {
            category: Some(Category::Dining),
            ..ItemFilter::default()
        },
        ItemFilter {
            source_type: Some(SourceType::Email),
            ..ItemFilter::default()
        },
        ItemFilter {
            batch_id: Some(batch_id),
            ..ItemFilter::default()
        },
    ] {
        let matches = repository
            .list_bounded_for_tests(filter)
            .await
            .expect("typed filter should succeed");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, target.id);
    }

    let category_override = repository
        .list_bounded_for_tests(ItemFilter {
            category: Some(Category::Transport),
            ..ItemFilter::default()
        })
        .await
        .expect("category filter should succeed");
    assert!(category_override.iter().all(|item| item.id != target.id));
}

#[tokio::test]
async fn item_repository_update_fields_sets_and_clears_mutable_values() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let batch_id = Uuid::new_v4();
    insert_batch(&pool, batch_id).await;
    let repository = ItemRepository::new(pool);
    let original = sample_item("update", "sha256-update");
    let duplicate_target = sample_item("duplicate-target", "sha256-duplicate-target");
    repository
        .insert(&duplicate_target)
        .await
        .expect("duplicate target should insert");
    repository
        .insert(&original)
        .await
        .expect("original item should insert");

    let updated = repository
        .update_fields(
            original.id,
            ItemPatch {
                normalized_pdf_path: Some(Some("/normalized/replaced.pdf".to_owned())),
                invoice_date: Some(chrono::NaiveDate::from_ymd_opt(2026, 7, 11)),
                suggested_period: Some(Some("2026-Q3".to_owned())),
                batch_id: Some(Some(batch_id)),
                suggested_category: Some(Some(Category::Hospitality)),
                final_category: Some(None),
                amount_cents: Some(Some(54_321)),
                currency: Some("USD".to_owned()),
                city: Some(Some("Beijing".to_owned())),
                company: Some(Some("Updated Co".to_owned())),
                recognition_status: Some(RecognitionStatus::Failed),
                confirmation_status: Some(ConfirmationStatus::Pending),
                dedupe_status: Some(DedupeStatus::SuspectedDuplicate),
                duplicate_of_id: Some(Some(duplicate_target.id)),
                note: Some(Some("updated note".to_owned())),
                event_tag: Some(Some("updated event".to_owned())),
                project_tag: Some(Some("updated project".to_owned())),
            },
        )
        .await
        .expect("item update should succeed");

    assert_eq!(
        updated.normalized_pdf_path.as_deref(),
        Some("/normalized/replaced.pdf")
    );
    assert_eq!(
        updated.invoice_date,
        chrono::NaiveDate::from_ymd_opt(2026, 7, 11)
    );
    assert_eq!(updated.suggested_period.as_deref(), Some("2026-Q3"));
    assert_eq!(updated.batch_id, Some(batch_id));
    assert_eq!(updated.suggested_category, Some(Category::Hospitality));
    assert_eq!(updated.final_category, None);
    assert_eq!(updated.amount_cents, Some(54_321));
    assert_eq!(updated.currency, "USD");
    assert_eq!(updated.city.as_deref(), Some("Beijing"));
    assert_eq!(updated.company.as_deref(), Some("Updated Co"));
    assert_eq!(updated.recognition_status, RecognitionStatus::Failed);
    assert_eq!(updated.confirmation_status, ConfirmationStatus::Pending);
    assert_eq!(updated.dedupe_status, DedupeStatus::SuspectedDuplicate);
    assert_eq!(updated.duplicate_of_id, Some(duplicate_target.id));
    assert_eq!(updated.note.as_deref(), Some("updated note"));
    assert_eq!(updated.event_tag.as_deref(), Some("updated event"));
    assert_eq!(updated.project_tag.as_deref(), Some("updated project"));
    assert_eq!(updated.status(), ItemStatus::SuspectedDuplicate);
    assert_eq!(updated.id, original.id);
    assert_eq!(updated.original_name, original.original_name);
    assert_eq!(updated.original_path, original.original_path);
    assert_eq!(updated.sha256, original.sha256);
    assert_eq!(updated.source_type, original.source_type);
    assert_eq!(updated.source_account_id, original.source_account_id);
    assert_eq!(updated.source_mailbox, original.source_mailbox);
    assert_eq!(updated.source_uid, original.source_uid);
    assert_eq!(updated.source_message_id, original.source_message_id);
    assert_eq!(updated.source_part_id, original.source_part_id);
    assert_eq!(updated.fetched_at, original.fetched_at);
    assert_eq!(updated.created_at, original.created_at);

    let cleared = repository
        .update_fields(
            original.id,
            ItemPatch {
                normalized_pdf_path: Some(None),
                invoice_date: Some(None),
                suggested_period: Some(None),
                batch_id: Some(None),
                suggested_category: Some(None),
                final_category: Some(None),
                amount_cents: Some(None),
                city: Some(None),
                company: Some(None),
                duplicate_of_id: Some(None),
                note: Some(None),
                event_tag: Some(None),
                project_tag: Some(None),
                recognition_status: Some(RecognitionStatus::Succeeded),
                confirmation_status: Some(ConfirmationStatus::Confirmed),
                dedupe_status: Some(DedupeStatus::Resolved),
                ..ItemPatch::default()
            },
        )
        .await
        .expect("nullable values should clear");

    assert_eq!(cleared.normalized_pdf_path, None);
    assert_eq!(cleared.invoice_date, None);
    assert_eq!(cleared.suggested_period, None);
    assert_eq!(cleared.batch_id, None);
    assert_eq!(cleared.suggested_category, None);
    assert_eq!(cleared.final_category, None);
    assert_eq!(cleared.amount_cents, None);
    assert_eq!(cleared.city, None);
    assert_eq!(cleared.company, None);
    assert_eq!(cleared.duplicate_of_id, None);
    assert_eq!(cleared.note, None);
    assert_eq!(cleared.event_tag, None);
    assert_eq!(cleared.project_tag, None);
    assert_eq!(cleared.currency, "USD");
    assert_eq!(cleared.status(), ItemStatus::Ready);
}

#[tokio::test]
async fn item_repository_rejects_negative_patch_amounts_before_writing() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let item = sample_item("negative-patch", "sha256-negative-patch");
    repository.insert(&item).await.expect("item should insert");

    let error = repository
        .update_fields(
            item.id,
            ItemPatch {
                amount_cents: Some(Some(-1)),
                ..ItemPatch::default()
            },
        )
        .await
        .expect_err("negative amount should be rejected");

    assert!(matches!(error, AppError::Validation { ref field, .. } if field == "amount_cents"));
    assert_eq!(
        repository
            .find_by_hash(&item.sha256)
            .await
            .expect("hash lookup should succeed")
            .expect("item should remain")
            .amount_cents,
        item.amount_cents
    );

    let missing_error = repository
        .update_fields(
            Uuid::new_v4(),
            ItemPatch {
                amount_cents: Some(Some(-1)),
                ..ItemPatch::default()
            },
        )
        .await
        .expect_err("negative amount should be validated before item lookup");
    assert!(
        matches!(missing_error, AppError::Validation { ref field, .. } if field == "amount_cents")
    );
}

#[tokio::test]
async fn item_repository_reports_missing_updates_as_item_not_found() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);

    let error = repository
        .update_fields(Uuid::new_v4(), ItemPatch::default())
        .await
        .expect_err("missing item should not update");

    assert!(
        matches!(error, AppError::NotFound { ref entity, .. } if entity == "item"),
        "unexpected error: {error:?}"
    );
}

#[tokio::test]
async fn batch_repository_creates_gets_and_lists_normalized_batches() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = BatchRepository::new(pool);
    let new_batch = NewBatch::try_new(
        "  July expenses  ",
        "2026-07-01",
        "2026-07-31",
        Some("month close".to_owned()),
    )
    .expect("batch input should validate");

    let created = repository
        .create(new_batch)
        .await
        .expect("batch should create");
    let fetched = repository
        .get(created.id)
        .await
        .expect("batch should be retrievable");
    let summaries = repository
        .list_bounded_for_tests()
        .await
        .expect("batches should list");

    assert_eq!(created, fetched);
    assert_eq!(created.name, "July expenses");
    assert_eq!(created.start_date, date(2026, 7, 1));
    assert_eq!(created.end_date, date(2026, 7, 31));
    assert_eq!(created.status, BatchStatus::Draft);
    assert_eq!(created.note.as_deref(), Some("month close"));
    assert_eq!(created.created_at, created.updated_at);
    assert_eq!(created.last_exported_at, None);
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, created.id);
    assert_eq!(summaries[0].name, created.name);
    assert_eq!(summaries[0].status, BatchStatus::Draft);
    assert_eq!(summaries[0].item_count, 0);
    assert_eq!(summaries[0].total_amount_cents, 0);
}

#[tokio::test]
async fn batch_repository_list_aggregates_only_assigned_items() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let batches = BatchRepository::new(pool.clone());
    let items = ItemRepository::new(pool);
    let target = batches
        .create(sample_batch("Target batch"))
        .await
        .expect("target batch should create");
    let other = batches
        .create(sample_batch("Other batch"))
        .await
        .expect("other batch should create");

    let mut assigned_amount = sample_item("assigned-amount", "sha256-assigned-amount");
    assigned_amount.batch_id = Some(target.id);
    assigned_amount.amount_cents = Some(12_345);
    items
        .insert(&assigned_amount)
        .await
        .expect("assigned item should insert");
    let mut assigned_null = sample_item("assigned-null", "sha256-assigned-null");
    assigned_null.batch_id = Some(target.id);
    assigned_null.amount_cents = None;
    items
        .insert(&assigned_null)
        .await
        .expect("null amount item should insert");
    let unassigned = sample_item("unassigned", "sha256-unassigned");
    items
        .insert(&unassigned)
        .await
        .expect("unassigned item should insert");
    let mut assigned_elsewhere = sample_item("elsewhere", "sha256-elsewhere");
    assigned_elsewhere.batch_id = Some(other.id);
    assigned_elsewhere.amount_cents = Some(99_999);
    items
        .insert(&assigned_elsewhere)
        .await
        .expect("other batch item should insert");

    let summaries = batches
        .list_bounded_for_tests()
        .await
        .expect("batches should list");
    let target_summary = summaries
        .iter()
        .find(|summary| summary.id == target.id)
        .expect("target summary should exist");
    let other_summary = summaries
        .iter()
        .find(|summary| summary.id == other.id)
        .expect("other summary should exist");

    assert_eq!(target_summary.item_count, 2);
    assert_eq!(target_summary.total_amount_cents, 12_345);
    assert_eq!(other_summary.item_count, 1);
    assert_eq!(other_summary.total_amount_cents, 99_999);
}

#[tokio::test]
async fn batch_repository_list_reports_stable_rust_amount_overflow() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let batches = BatchRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    batches
        .create(sample_batch("Healthy batch"))
        .await
        .expect("healthy batch should create");
    let overflowing = batches
        .create(sample_batch("Overflowing batch"))
        .await
        .expect("overflowing batch should create");
    let mut maximum = sample_item("list-overflow-max", "sha-list-overflow-max");
    maximum.amount_cents = None;
    let mut one = sample_item("list-overflow-one", "sha-list-overflow-one");
    one.amount_cents = Some(1);
    items
        .insert(&maximum)
        .await
        .expect("maximum item should insert");
    sqlx::query("UPDATE items SET amount_cents = ? WHERE id = ?")
        .bind(i64::MAX)
        .bind(maximum.id.to_string())
        .execute(&pool)
        .await
        .expect("raw overflow fixture should bypass repository validation");
    items
        .insert(&one)
        .await
        .expect("one-cent item should insert");
    sqlx::query("UPDATE items SET batch_id = ? WHERE id IN (?, ?)")
        .bind(overflowing.id.to_string())
        .bind(maximum.id.to_string())
        .bind(one.id.to_string())
        .execute(&pool)
        .await
        .expect("overflow fixture should assign directly");

    let error = batches
        .list_bounded_for_tests()
        .await
        .expect_err("one overflowing batch should fail the full list");

    assert!(matches!(
        error,
        AppError::Validation { ref field, .. } if field == "totalAmountCents"
    ));
}

#[tokio::test]
async fn batch_repository_list_orders_by_updated_at_then_id_descending() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = BatchRepository::new(pool.clone());
    let same_time_a = repository
        .create(sample_batch("Same time A"))
        .await
        .expect("batch should create");
    let same_time_b = repository
        .create(sample_batch("Same time B"))
        .await
        .expect("batch should create");
    let newest = repository
        .create(sample_batch("Newest"))
        .await
        .expect("batch should create");

    for (id, updated_at) in [
        (same_time_a.id, "2026-07-13T10:00:00Z"),
        (same_time_b.id, "2026-07-13T10:00:00Z"),
        (newest.id, "2026-07-14T10:00:00Z"),
    ] {
        sqlx::query("UPDATE batches SET updated_at = ? WHERE id = ?")
            .bind(updated_at)
            .bind(id.to_string())
            .execute(&pool)
            .await
            .expect("batch timestamp should update");
    }

    let summaries = repository
        .list_bounded_for_tests()
        .await
        .expect("batches should list");
    let mut tied_ids = [same_time_a.id, same_time_b.id];
    tied_ids.sort_unstable_by(|left, right| right.cmp(left));

    assert_eq!(
        summaries
            .iter()
            .map(|summary| summary.id)
            .collect::<Vec<_>>(),
        vec![newest.id, tied_ids[0], tied_ids[1]]
    );
}

#[tokio::test]
async fn deleting_a_batch_keeps_assigned_items_and_clears_their_batch_id() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let batches = BatchRepository::new(pool.clone());
    let items = ItemRepository::new(pool);
    let batch = batches
        .create(sample_batch("Disposable batch"))
        .await
        .expect("batch should create");
    let mut item = sample_item("delete-batch", "sha256-delete-batch");
    item.batch_id = Some(batch.id);
    items.insert(&item).await.expect("item should insert");

    batches.delete(batch.id).await.expect("batch should delete");
    let retained = items
        .find_by_hash(&item.sha256)
        .await
        .expect("item lookup should succeed")
        .expect("item should remain");

    assert_eq!(retained.batch_id, None);
    assert!(matches!(
        batches.get(batch.id).await,
        Err(AppError::NotFound { ref entity, .. }) if entity == "batch"
    ));
}

#[tokio::test]
async fn assigned_public_inserts_fall_back_exported_batches_and_preserve_export_time() {
    for mode in [InsertMode::Plain, InsertMode::Deduplicated] {
        let pool = db::connect("sqlite::memory:")
            .await
            .expect("in-memory database should connect");
        let batches = BatchRepository::new(pool.clone());
        let items = ItemRepository::new(pool.clone());
        let batch = batches
            .create(sample_batch("Exported insert target"))
            .await
            .expect("batch should create");
        sqlx::query("UPDATE batches SET status = 'exported', last_exported_at = ? WHERE id = ?")
            .bind("2026-07-14T08:00:00Z")
            .bind(batch.id.to_string())
            .execute(&pool)
            .await
            .expect("batch should mark exported");
        let mut item = sample_item(mode.label(), &format!("sha-assigned-{}", mode.label()));
        item.batch_id = Some(batch.id);

        insert_with_mode(&items, item, mode)
            .await
            .expect("assigned item should insert");

        let persisted = batches.get(batch.id).await.expect("batch should reload");
        assert_eq!(persisted.status, BatchStatus::Draft);
        assert_eq!(
            persisted
                .last_exported_at
                .expect("export time should remain")
                .to_rfc3339(),
            "2026-07-14T08:00:00+00:00"
        );
    }
}

#[tokio::test]
async fn assigned_public_inserts_report_missing_batches_and_roll_back_rows() {
    for mode in [InsertMode::Plain, InsertMode::Deduplicated] {
        let pool = db::connect("sqlite::memory:")
            .await
            .expect("in-memory database should connect");
        let items = ItemRepository::new(pool);
        let mut item = sample_item(
            &format!("missing-{}", mode.label()),
            &format!("sha-missing-{}", mode.label()),
        );
        item.batch_id = Some(Uuid::from_u128(404));
        let item_id = item.id;

        let error = insert_with_mode(&items, item, mode)
            .await
            .expect_err("missing batch should reject assigned insert");

        assert!(matches!(
            error,
            AppError::NotFound { ref entity, .. } if entity == "batch"
        ));
        assert!(matches!(
            items.get_by_id(item_id).await,
            Err(AppError::NotFound { ref entity, .. }) if entity == "item"
        ));
    }
}

#[tokio::test]
async fn assigned_public_inserts_reject_invalid_item_states_without_partial_writes() {
    for mode in [InsertMode::Plain, InsertMode::Deduplicated] {
        for invalid_state in ["duplicate", "failed"] {
            let pool = db::connect("sqlite::memory:")
                .await
                .expect("in-memory database should connect");
            let batches = BatchRepository::new(pool.clone());
            let items = ItemRepository::new(pool.clone());
            let batch = batches
                .create(sample_batch("Invalid insert target"))
                .await
                .expect("batch should create");
            let shared_hash = format!("sha-{}-{invalid_state}", mode.label());
            if matches!(mode, InsertMode::Deduplicated) && invalid_state == "duplicate" {
                let canonical = sample_item(&format!("canonical-{}", mode.label()), &shared_hash);
                items
                    .insert(&canonical)
                    .await
                    .expect("canonical item should insert");
            }
            sqlx::query(
                "UPDATE batches SET status = 'exported', last_exported_at = ? WHERE id = ?",
            )
            .bind("2026-07-14T08:00:00Z")
            .bind(batch.id.to_string())
            .execute(&pool)
            .await
            .expect("batch should mark exported");
            let mut candidate = sample_item(
                &format!("invalid-{}-{invalid_state}", mode.label()),
                &shared_hash,
            );
            candidate.batch_id = Some(batch.id);
            match invalid_state {
                "duplicate" if matches!(mode, InsertMode::Plain) => {
                    candidate.dedupe_status = DedupeStatus::SuspectedDuplicate;
                }
                "failed" => candidate.recognition_status = RecognitionStatus::Failed,
                _ => {}
            }
            let candidate_id = candidate.id;

            let error = insert_with_mode(&items, candidate, mode)
                .await
                .expect_err("invalid assigned item should not insert");

            assert!(matches!(error, AppError::Conflict { .. }));
            assert!(matches!(
                items.get_by_id(candidate_id).await,
                Err(AppError::NotFound { ref entity, .. }) if entity == "item"
            ));
            let persisted = batches.get(batch.id).await.expect("batch should reload");
            assert_eq!(persisted.status, BatchStatus::Exported);
            assert!(persisted.last_exported_at.is_some());
        }
    }
}

#[tokio::test]
async fn assigned_public_inserts_roll_back_batch_total_overflow() {
    for mode in [InsertMode::Plain, InsertMode::Deduplicated] {
        let pool = db::connect("sqlite::memory:")
            .await
            .expect("in-memory database should connect");
        let batches = BatchRepository::new(pool.clone());
        let items = ItemRepository::new(pool.clone());
        let batch = batches
            .create(sample_batch("Overflow insert target"))
            .await
            .expect("batch should create");
        let mut maximum = sample_item(
            &format!("maximum-{}", mode.label()),
            &format!("sha-maximum-{}", mode.label()),
        );
        maximum.batch_id = Some(batch.id);
        maximum.amount_cents = None;
        items
            .insert(&maximum)
            .await
            .expect("maximum item should insert");
        sqlx::query("UPDATE items SET amount_cents = ? WHERE id = ?")
            .bind(i64::MAX)
            .bind(maximum.id.to_string())
            .execute(&pool)
            .await
            .expect("raw overflow fixture should bypass repository validation");
        sqlx::query("UPDATE batches SET status = 'exported', last_exported_at = ? WHERE id = ?")
            .bind("2026-07-14T08:00:00Z")
            .bind(batch.id.to_string())
            .execute(&pool)
            .await
            .expect("batch should mark exported");
        let mut candidate = sample_item(
            &format!("overflow-{}", mode.label()),
            &format!("sha-overflow-{}", mode.label()),
        );
        candidate.batch_id = Some(batch.id);
        candidate.amount_cents = Some(1);
        let candidate_id = candidate.id;

        let error = insert_with_mode(&items, candidate, mode)
            .await
            .expect_err("overflowing assigned item should not insert");

        assert!(matches!(
            error,
            AppError::Validation { ref field, .. } if field == "totalAmountCents"
        ));
        assert!(matches!(
            items.get_by_id(candidate_id).await,
            Err(AppError::NotFound { ref entity, .. }) if entity == "item"
        ));
        let persisted = batches.get(batch.id).await.expect("batch should reload");
        assert_eq!(persisted.status, BatchStatus::Exported);
        assert!(persisted.last_exported_at.is_some());
    }
}

#[tokio::test]
async fn mailbox_account_repository_inserts_gets_lists_and_round_trips_providers() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = MailboxAccountRepository::new(pool);
    let qq = repository
        .insert(NewMailboxAccount {
            provider: MailboxProvider::QQ,
            email: "z@example.com".to_owned(),
            imap_host: "imap.qq.com".to_owned(),
            imap_port: 993,
            enabled: false,
            sync_interval_minutes: 60,
        })
        .await
        .expect("QQ account should insert");
    let gmail = repository
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "a@example.com".to_owned(),
            imap_host: "imap.gmail.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .expect("Gmail account should insert");

    assert_eq!(repository.get(qq.id).await.expect("account should get"), qq);
    let accounts = repository.list().await.expect("accounts should list");
    assert_eq!(accounts, vec![gmail, qq]);
    assert_eq!(accounts[0].provider, MailboxProvider::Gmail);
    assert_eq!(accounts[1].provider, MailboxProvider::QQ);
    assert_eq!(
        serde_json::to_string(&MailboxProvider::Gmail).expect("Gmail provider should serialize"),
        "\"gmail\""
    );
    assert_eq!(
        serde_json::to_string(&MailboxProvider::QQ).expect("QQ provider should serialize"),
        "\"qq\""
    );
    assert!(accounts[0].enabled);
    assert!(!accounts[1].enabled);
    assert_eq!(accounts[0].last_synced_at, None);
    assert_eq!(accounts[0].last_error, None);

    let missing_error = repository
        .get(Uuid::new_v4())
        .await
        .expect_err("missing account should not be returned");
    assert!(matches!(
        missing_error,
        AppError::NotFound { ref entity, .. } if entity == "mailbox_account"
    ));
}

#[tokio::test]
async fn mailbox_account_repository_validates_before_sql_and_rejects_duplicate_email() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = MailboxAccountRepository::new(pool.clone());
    let valid = sample_account("duplicate@example.com");
    repository
        .insert(valid.clone())
        .await
        .expect("first account should insert");

    let duplicate_error = repository
        .insert(valid)
        .await
        .expect_err("duplicate email should conflict");
    assert!(matches!(duplicate_error, AppError::Conflict { .. }));

    for (field, account) in [
        (
            "imap_port",
            NewMailboxAccount {
                imap_port: 0,
                email: "bad-port@example.com".to_owned(),
                ..sample_account("unused-port@example.com")
            },
        ),
        (
            "imap_port",
            NewMailboxAccount {
                imap_port: 65_536,
                email: "bad-high-port@example.com".to_owned(),
                ..sample_account("unused-high-port@example.com")
            },
        ),
        (
            "sync_interval_minutes",
            NewMailboxAccount {
                sync_interval_minutes: 4,
                email: "bad-interval@example.com".to_owned(),
                ..sample_account("unused-interval@example.com")
            },
        ),
        (
            "sync_interval_minutes",
            NewMailboxAccount {
                sync_interval_minutes: 1_441,
                email: "bad-high-interval@example.com".to_owned(),
                ..sample_account("unused-high-interval@example.com")
            },
        ),
    ] {
        let error = repository
            .insert(account)
            .await
            .expect_err("invalid account should be rejected");
        assert!(
            matches!(error, AppError::Validation { field: ref actual, .. } if actual == field),
            "unexpected error: {error:?}"
        );
    }

    let stored_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mailbox_accounts")
        .fetch_one(&pool)
        .await
        .expect("account count should query");
    assert_eq!(stored_count, 1);
}

#[tokio::test]
async fn mailbox_account_repository_rejects_invalid_endpoints_before_sql() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = MailboxAccountRepository::new(pool.clone());
    pool.close().await;

    for (field, account) in [
        (
            "email",
            NewMailboxAccount {
                email: "   ".to_owned(),
                ..sample_account("unused-email@example.com")
            },
        ),
        (
            "imap_host",
            NewMailboxAccount {
                imap_host: "   ".to_owned(),
                ..sample_account("blank-host@example.com")
            },
        ),
        (
            "imap_port",
            NewMailboxAccount {
                imap_port: 0,
                ..sample_account("zero-port@example.com")
            },
        ),
        (
            "imap_port",
            NewMailboxAccount {
                imap_port: 65_536,
                ..sample_account("high-port@example.com")
            },
        ),
    ] {
        let error = repository
            .insert(account)
            .await
            .expect_err("invalid endpoint should be rejected");
        assert!(
            matches!(error, AppError::Validation { field: ref actual, .. } if actual == field),
            "unexpected error: {error:?}"
        );
    }
}

#[tokio::test]
async fn persisted_domain_enums_round_trip_through_repository_rows() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let batches = BatchRepository::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let accounts = MailboxAccountRepository::new(pool.clone());

    let batch = batches
        .create(sample_batch("Enum batch"))
        .await
        .expect("batch should create");
    assert_eq!(batch.status, BatchStatus::Draft);

    let account = accounts
        .insert(sample_account("enum@example.com"))
        .await
        .expect("account should insert");
    assert_eq!(account.provider, MailboxProvider::Gmail);
    accounts
        .insert(NewMailboxAccount {
            provider: MailboxProvider::QQ,
            ..sample_account("enum-qq@example.com")
        })
        .await
        .expect("QQ account should insert");

    let category_cases = [
        Category::Transport,
        Category::Dining,
        Category::Accommodation,
        Category::Hospitality,
    ];
    let source_cases = [SourceType::Email, SourceType::ManualUpload];
    let recognition_cases = [
        RecognitionStatus::Pending,
        RecognitionStatus::Succeeded,
        RecognitionStatus::Failed,
    ];
    let confirmation_cases = [ConfirmationStatus::Pending, ConfirmationStatus::Confirmed];
    let dedupe_cases = [
        DedupeStatus::Unique,
        DedupeStatus::SuspectedDuplicate,
        DedupeStatus::Resolved,
    ];

    for index in 0..12 {
        let mut item = sample_item(&format!("enum-{index}"), &format!("sha256-enum-{index}"));
        item.suggested_category = Some(category_cases[index % category_cases.len()]);
        item.final_category = Some(category_cases[(index + 1) % category_cases.len()]);
        item.source_type = source_cases[index % source_cases.len()];
        if item.source_type == SourceType::Email {
            item.source_account_id = Some(account.id);
            item.source_mailbox = Some("INBOX".to_owned());
            item.source_uid_validity = Some(10);
            item.source_uid = Some(index as i64 + 1);
            item.source_part_id = Some("1".to_owned());
        }
        item.recognition_status = recognition_cases[index % recognition_cases.len()];
        item.confirmation_status = confirmation_cases[index % confirmation_cases.len()];
        item.dedupe_status = dedupe_cases[index % dedupe_cases.len()];

        let inserted = items.insert(&item).await.expect("enum item should insert");
        assert_eq!(inserted.suggested_category, item.suggested_category);
        assert_eq!(inserted.final_category, item.final_category);
        assert_eq!(inserted.source_type, item.source_type);
        assert_eq!(inserted.recognition_status, item.recognition_status);
        assert_eq!(inserted.confirmation_status, item.confirmation_status);
        assert_eq!(inserted.dedupe_status, item.dedupe_status);
    }

    let exported_batch = batches
        .create(sample_batch("Exported enum batch"))
        .await
        .expect("second batch should create");
    sqlx::query("UPDATE batches SET status = 'exported' WHERE id = ?")
        .bind(exported_batch.id.to_string())
        .execute(&pool)
        .await
        .expect("batch status should update");

    let persisted_batch_statuses =
        sqlx::query_scalar::<_, BatchStatus>("SELECT status FROM batches ORDER BY status")
            .fetch_all(&pool)
            .await
            .expect("batch statuses should decode through SQLx");
    let persisted_providers = sqlx::query_scalar::<_, MailboxProvider>(
        "SELECT provider FROM mailbox_accounts ORDER BY provider",
    )
    .fetch_all(&pool)
    .await
    .expect("mailbox providers should decode through SQLx");
    let persisted_item_enums = sqlx::query_as::<
        _,
        (
            Category,
            Category,
            SourceType,
            RecognitionStatus,
            ConfirmationStatus,
            DedupeStatus,
        ),
    >(
        "SELECT suggested_category, final_category, source_type, recognition_status, \
            confirmation_status, dedupe_status \
         FROM items WHERE original_name LIKE 'invoice-enum-%'",
    )
    .fetch_all(&pool)
    .await
    .expect("item enums should decode through SQLx");

    assert_eq!(
        persisted_batch_statuses,
        vec![BatchStatus::Draft, BatchStatus::Exported]
    );
    assert_eq!(
        persisted_providers,
        vec![MailboxProvider::Gmail, MailboxProvider::QQ]
    );
    for expected in category_cases {
        assert!(
            persisted_item_enums
                .iter()
                .any(|(suggested, _, ..)| *suggested == expected)
        );
        assert!(
            persisted_item_enums
                .iter()
                .any(|(_, final_category, ..)| *final_category == expected)
        );
    }
    for expected in source_cases {
        assert!(
            persisted_item_enums
                .iter()
                .any(|(_, _, source, ..)| *source == expected)
        );
    }
    for expected in recognition_cases {
        assert!(
            persisted_item_enums
                .iter()
                .any(|(_, _, _, recognition, ..)| *recognition == expected)
        );
    }
    for expected in confirmation_cases {
        assert!(
            persisted_item_enums
                .iter()
                .any(|(_, _, _, _, confirmation, _)| *confirmation == expected)
        );
    }
    for expected in dedupe_cases {
        assert!(
            persisted_item_enums
                .iter()
                .any(|(_, _, _, _, _, dedupe)| *dedupe == expected)
        );
    }

    sqlx::query("UPDATE batches SET status = 'exported' WHERE id = ?")
        .bind(batch.id.to_string())
        .execute(&pool)
        .await
        .expect("batch status should update");
    assert_eq!(
        batches
            .get(batch.id)
            .await
            .expect("batch should reload")
            .status,
        BatchStatus::Exported
    );
}

fn sample_batch(name: &str) -> NewBatch {
    NewBatch::try_new(name, "2026-07-01", "2026-07-31", None).expect("sample batch should validate")
}

#[derive(Clone, Copy)]
enum InsertMode {
    Plain,
    Deduplicated,
}

impl InsertMode {
    fn label(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Deduplicated => "deduplicated",
        }
    }
}

async fn insert_with_mode(
    repository: &ItemRepository,
    item: NewItemRecord,
    mode: InsertMode,
) -> Result<invoice_reimbursement::db::items::InvoiceItem, AppError> {
    match mode {
        InsertMode::Plain => repository.insert(&item).await,
        InsertMode::Deduplicated => repository.insert_deduplicated(item).await,
    }
}

fn sample_account(email: &str) -> NewMailboxAccount {
    NewMailboxAccount {
        provider: MailboxProvider::Gmail,
        email: email.to_owned(),
        imap_host: "imap.example.com".to_owned(),
        imap_port: 993,
        enabled: true,
        sync_interval_minutes: 15,
    }
}

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("test date should be valid")
}

async fn pragma_i64(pool: &SqlitePool, query: &str) -> i64 {
    sqlx::query_scalar(query)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("{query} should be queryable: {error}"))
}

async fn pragma_string(pool: &SqlitePool, query: &str) -> String {
    sqlx::query_scalar(query)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("{query} should be queryable: {error}"))
}

async fn insert_batch(pool: &SqlitePool, id: Uuid) {
    sqlx::query(
        "INSERT INTO batches (id, name, start_date, end_date, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind("Q3 expenses")
    .bind("2026-07-01")
    .bind("2026-09-30")
    .bind("2026-07-13T10:00:00Z")
    .bind("2026-07-13T10:00:00Z")
    .execute(pool)
    .await
    .expect("batch should insert");
}

async fn insert_mailbox_account(pool: &SqlitePool, id: Uuid) {
    sqlx::query(
        "INSERT INTO mailbox_accounts \
         (id, provider, email, imap_host, imap_port, sync_interval_minutes, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind("gmail")
    .bind(format!("{id}@example.com"))
    .bind("imap.example.com")
    .bind(993)
    .bind(15)
    .bind("2026-07-13T10:00:00Z")
    .bind("2026-07-13T10:00:00Z")
    .execute(pool)
    .await
    .expect("mailbox account should insert");
}

fn sample_item(suffix: &str, sha256: &str) -> NewItemRecord {
    NewItemRecord {
        id: Uuid::new_v4(),
        original_name: format!("invoice-{suffix}.pdf"),
        original_path: format!("/invoices/invoice-{suffix}.pdf"),
        normalized_pdf_path: Some(format!("/normalized/invoice-{suffix}.pdf")),
        sha256: sha256.to_owned(),
        mime_type: "application/pdf".to_owned(),
        source_type: SourceType::ManualUpload,
        source_account_id: None,
        source_mailbox: None,
        source_uid_validity: None,
        source_uid: None,
        source_message_id: None,
        source_part_id: None,
        fetched_at: Utc
            .with_ymd_and_hms(2026, 7, 13, 2, 0, 0)
            .single()
            .expect("valid fetched time"),
        invoice_date: chrono::NaiveDate::from_ymd_opt(2026, 7, 12),
        suggested_period: Some("2026-07".to_owned()),
        batch_id: None,
        suggested_category: Some(Category::Transport),
        final_category: Some(Category::Dining),
        amount_cents: Some(12_345),
        currency: "CNY".to_owned(),
        city: Some("Shanghai".to_owned()),
        company: Some("Example Co".to_owned()),
        recognition_status: RecognitionStatus::Succeeded,
        confirmation_status: ConfirmationStatus::Confirmed,
        dedupe_status: DedupeStatus::Resolved,
        duplicate_of_id: None,
        note: Some("client visit".to_owned()),
        event_tag: Some("summit".to_owned()),
        project_tag: Some("P-2026".to_owned()),
        created_at: Utc
            .with_ymd_and_hms(2026, 7, 13, 2, 5, 0)
            .single()
            .expect("valid created time"),
        updated_at: Utc
            .with_ymd_and_hms(2026, 7, 13, 2, 10, 0)
            .single()
            .expect("valid updated time"),
    }
}

async fn insert_item(
    pool: &SqlitePool,
    id: &str,
    recognition_status: &str,
    confirmation_status: &str,
    dedupe_status: &str,
    amount_cents: Option<i64>,
) -> Result<SqliteQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO items (\
            id, original_name, original_path, sha256, mime_type, source_type, fetched_at, \
            recognition_status, confirmation_status, dedupe_status, amount_cents, \
            created_at, updated_at\
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(format!("{id}.pdf"))
    .bind(format!("/invoices/{id}.pdf"))
    .bind(format!("sha256-{id}"))
    .bind("application/pdf")
    .bind("manual_upload")
    .bind("2026-07-13T10:00:00Z")
    .bind(recognition_status)
    .bind(confirmation_status)
    .bind(dedupe_status)
    .bind(amount_cents)
    .bind("2026-07-13T10:00:00Z")
    .bind("2026-07-13T10:00:00Z")
    .execute(pool)
    .await
}
