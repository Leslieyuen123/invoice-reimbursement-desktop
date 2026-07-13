use invoice_reimbursement::db;
use sqlx::{SqlitePool, sqlite::SqliteQueryResult};

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
             (id, provider, email, host, port, sync_interval_minutes, created_at, updated_at) \
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
