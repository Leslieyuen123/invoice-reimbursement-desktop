//! The "其他" category required widening a SQLite CHECK constraint, which means
//! rebuilding the whole `items` table. These tests prove the rebuild keeps every
//! row, column, duplicate link and semantic identity intact, and that it works
//! both on a populated database and on a brand-new one.

use std::borrow::Cow;

use invoice_reimbursement::db;
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use uuid::Uuid;

const ITEM_FINGERPRINT: &str = "SELECT COALESCE(id, '') || '~' || COALESCE(original_name, '') \
    || '~' || COALESCE(original_path, '') || '~' || COALESCE(normalized_pdf_path, '') \
    || '~' || COALESCE(sha256, '') || '~' || COALESCE(mime_type, '') \
    || '~' || COALESCE(source_type, '') || '~' || COALESCE(source_account_id, '') \
    || '~' || COALESCE(source_mailbox, '') || '~' || COALESCE(source_uid, '') \
    || '~' || COALESCE(source_message_id, '') || '~' || COALESCE(source_part_id, '') \
    || '~' || COALESCE(fetched_at, '') || '~' || COALESCE(invoice_date, '') \
    || '~' || COALESCE(suggested_period, '') || '~' || COALESCE(batch_id, '') \
    || '~' || COALESCE(suggested_category, '') || '~' || COALESCE(final_category, '') \
    || '~' || COALESCE(amount_cents, '') || '~' || COALESCE(currency, '') \
    || '~' || COALESCE(city, '') || '~' || COALESCE(company, '') \
    || '~' || COALESCE(recognition_status, '') || '~' || COALESCE(confirmation_status, '') \
    || '~' || COALESCE(dedupe_status, '') || '~' || COALESCE(duplicate_of_id, '') \
    || '~' || COALESCE(note, '') || '~' || COALESCE(event_tag, '') \
    || '~' || COALESCE(project_tag, '') || '~' || COALESCE(created_at, '') \
    || '~' || COALESCE(updated_at, '') || '~' || COALESCE(source_uid_validity, '') \
    || '~' || COALESCE(source_received_date, '')";

async fn item_fingerprint(pool: &SqlitePool) -> String {
    sqlx::query_scalar::<_, Option<String>>(&format!(
        "SELECT group_concat(entry, char(10)) FROM ({ITEM_FINGERPRINT} AS entry FROM items ORDER BY id)"
    ))
    .fetch_one(pool)
    .await
    .unwrap()
    .unwrap_or_default()
}

async fn semantic_identity_fingerprint(pool: &SqlitePool) -> String {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT group_concat(entry, char(10)) FROM (\
            SELECT item_id || '~' || fingerprint || '~' || created_at || '~' || updated_at \
            AS entry FROM item_semantic_identities ORDER BY item_id)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
    .unwrap_or_default()
}

async fn item_columns(pool: &SqlitePool) -> Vec<(String, String, Option<String>, i64, i64)> {
    sqlx::query_as(
        "SELECT name, type, dflt_value, \"notnull\", pk \
         FROM pragma_table_info('items') ORDER BY cid",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn item_indexes(pool: &SqlitePool) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT name, sql FROM sqlite_master \
         WHERE type = 'index' AND tbl_name = 'items' ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn item_foreign_keys(
    pool: &SqlitePool,
) -> Vec<(
    i64,
    i64,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
)> {
    sqlx::query_as(
        "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete, match \
         FROM pragma_foreign_key_list('items') ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn seed_items(pool: &SqlitePool) {
    let batch_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO batches (id, name, start_date, end_date, status, created_at, updated_at) \
         VALUES (?, '迁移测试批次', '2026-05-01', '2026-05-31', 'draft', \
            '2026-05-01T00:00:00+00:00', '2026-05-01T00:00:00+00:00')",
    )
    .bind(batch_id.to_string())
    .execute(pool)
    .await
    .unwrap();

    struct SeedItem<'a> {
        id: Uuid,
        suggested_category: Option<&'a str>,
        final_category: Option<&'a str>,
        batch_id: Option<Uuid>,
        duplicate_of_id: Option<Uuid>,
        original_name: &'a str,
    }

    let target = Uuid::new_v4();
    let duplicate = Uuid::new_v4();
    let rows = [
        SeedItem {
            id: target,
            suggested_category: Some("transport"),
            final_category: Some("dining"),
            batch_id: Some(batch_id),
            duplicate_of_id: None,
            original_name: "目标票据",
        },
        SeedItem {
            id: duplicate,
            suggested_category: Some("hospitality"),
            final_category: None,
            batch_id: None,
            duplicate_of_id: Some(target),
            original_name: "重复票据",
        },
        SeedItem {
            id: Uuid::new_v4(),
            suggested_category: None,
            final_category: None,
            batch_id: None,
            duplicate_of_id: None,
            original_name: "未分类票据",
        },
        SeedItem {
            id: Uuid::new_v4(),
            suggested_category: Some("accommodation"),
            final_category: Some("accommodation"),
            batch_id: None,
            duplicate_of_id: None,
            original_name: "住宿票据",
        },
        SeedItem {
            id: Uuid::new_v4(),
            suggested_category: None,
            final_category: Some("transport"),
            batch_id: None,
            duplicate_of_id: None,
            original_name: "仅有最终分类",
        },
    ];
    for row in rows {
        sqlx::query(
            "INSERT INTO items (\
                id, original_name, original_path, normalized_pdf_path, sha256, mime_type, \
                source_type, fetched_at, invoice_date, suggested_period, batch_id, \
                suggested_category, final_category, amount_cents, currency, city, company, \
                recognition_status, confirmation_status, dedupe_status, duplicate_of_id, note, \
                event_tag, project_tag, created_at, updated_at, source_uid_validity, \
                source_received_date\
             ) VALUES (?, ?, ?, NULL, ?, 'application/pdf', 'manual_upload', \
                '2026-05-10T08:00:00+00:00', '2026-05-10', '2026-05', ?, ?, ?, 12850, 'CNY', \
                '上海', '测试公司', 'succeeded', 'confirmed', 'unique', ?, NULL, NULL, NULL, \
                '2026-05-10T08:00:00+00:00', '2026-05-10T08:00:00+00:00', NULL, NULL)",
        )
        .bind(row.id.to_string())
        .bind(row.original_name)
        .bind(format!("/tmp/{}.pdf", row.id))
        .bind(format!("sha256-{}", row.id))
        .bind(row.batch_id.map(|value| value.to_string()))
        .bind(row.suggested_category)
        .bind(row.final_category)
        .bind(row.duplicate_of_id.map(|value| value.to_string()))
        .execute(pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO item_semantic_identities (item_id, fingerprint, created_at, updated_at) \
         VALUES (?, 'fingerprint-target', '2026-05-10T08:00:00+00:00', \
            '2026-05-10T08:00:00+00:00')",
    )
    .bind(target.to_string())
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn migration_0016_rebuilds_items_without_losing_rows_links_or_indices() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let current = sqlx::migrate!("./migrations");
    let through_0014 = sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            current
                .iter()
                .take_while(|migration| migration.version <= 14)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    through_0014.run(&pool).await.unwrap();
    seed_items(&pool).await;

    // The pre-0016 CHECK really does reject "其他"; otherwise this test would
    // prove nothing about the rebuild.
    let rejected_before =
        sqlx::query("UPDATE items SET final_category = 'other' WHERE final_category IS NULL")
            .execute(&pool)
            .await;
    assert!(
        rejected_before.is_err(),
        "the old CHECK constraint must reject 'other' before migration 0016"
    );

    let before_items = item_fingerprint(&pool).await;
    let before_semantics = semantic_identity_fingerprint(&pool).await;
    let before_columns = item_columns(&pool).await;
    let before_indexes = item_indexes(&pool).await;
    let before_foreign_keys = item_foreign_keys(&pool).await;

    current
        .run(&pool)
        .await
        .expect("migration 0016 must apply to a populated database");

    assert_eq!(item_fingerprint(&pool).await, before_items);
    assert_eq!(semantic_identity_fingerprint(&pool).await, before_semantics);
    assert_eq!(item_columns(&pool).await, before_columns);
    assert_eq!(item_indexes(&pool).await, before_indexes);
    assert_eq!(item_foreign_keys(&pool).await, before_foreign_keys);

    // The duplicate link and the batch membership survived the two-table move.
    let duplicate_links: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM items WHERE duplicate_of_id IS NOT NULL AND batch_id IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(duplicate_links, 1);
    let batch_members: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE batch_id IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(batch_members, 1);
    let semantic_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM item_semantic_identities")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(semantic_rows, 1);

    // The widened CHECK accepts "其他" while still rejecting unknown values and
    // keeping NULL (not classified yet) legal.
    sqlx::query("UPDATE items SET final_category = 'other' WHERE final_category IS NULL")
        .execute(&pool)
        .await
        .expect("'other' must be accepted after migration 0016");
    sqlx::query("UPDATE items SET suggested_category = 'other' WHERE suggested_category IS NULL")
        .execute(&pool)
        .await
        .expect("'other' must be accepted in suggested_category too");
    assert!(
        sqlx::query("UPDATE items SET final_category = 'misc'")
            .execute(&pool)
            .await
            .unwrap_err()
            .to_string()
            .contains("CHECK"),
        "unknown categories must still violate the CHECK constraint"
    );
    sqlx::query("UPDATE items SET final_category = NULL WHERE final_category = 'other'")
        .execute(&pool)
        .await
        .expect("NULL category stays legal");

    let applied: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 16 AND success = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(applied, 1, "migration 16 must be recorded as applied");
    let item_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(item_count, 5);
}

#[tokio::test]
async fn fresh_database_accepts_every_category_including_other() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO items (\
            id, original_name, original_path, sha256, mime_type, source_type, fetched_at, \
            currency, recognition_status, confirmation_status, dedupe_status, created_at, \
            updated_at, suggested_category, final_category\
         ) VALUES (?, 'fresh.pdf', ?, 'sha256-fresh', 'application/pdf', 'manual_upload', \
            '2026-05-10T08:00:00+00:00', 'CNY', 'succeeded', 'confirmed', 'unique', \
            '2026-05-10T08:00:00+00:00', '2026-05-10T08:00:00+00:00', 'other', 'other')",
    )
    .bind(id.to_string())
    .bind(format!("/tmp/{id}.pdf"))
    .execute(&pool)
    .await
    .expect("a brand-new database must accept the 'other' category");

    let (suggested, final_category): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT suggested_category, final_category FROM items WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(suggested.as_deref(), Some("other"));
    assert_eq!(final_category.as_deref(), Some("other"));
}

/// Verification against a copy of the user's real invoice library.
///
/// Run manually (never point this at the live data directory):
/// ```text
/// INVOICE_DESK_REAL_DB="/tmp/verify/invoice.sqlite3" \
///   cargo +1.98.0 test --test category_migration -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires INVOICE_DESK_REAL_DB to point at a copy of the real invoice library"]
async fn real_library_copy_survives_migration_0016() {
    let source = std::env::var("INVOICE_DESK_REAL_DB")
        .expect("set INVOICE_DESK_REAL_DB to a copy of the real invoice library");
    let directory = tempfile::tempdir().unwrap();
    let copied = directory.path().join("invoice.sqlite3");
    std::fs::copy(&source, &copied).unwrap();
    for suffix in ["-wal", "-shm"] {
        let sidecar = format!("{source}{suffix}");
        if std::path::Path::new(&sidecar).exists() {
            std::fs::copy(
                &sidecar,
                copied.with_file_name(format!("invoice.sqlite3{suffix}")),
            )
            .unwrap();
        }
    }
    let url = format!("sqlite://{}", copied.display());

    // Capture the pre-migration state through a plain connection (this does not
    // run migrations), then let the application's own migration runner upgrade
    // the copy and compare.
    let before_pool = SqlitePool::connect(&url).await.unwrap();
    let before_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&before_pool)
        .await
        .unwrap();
    let before_items = item_fingerprint(&before_pool).await;
    let before_semantics = semantic_identity_fingerprint(&before_pool).await;
    let before_columns = item_columns(&before_pool).await;
    let before_indexes = item_indexes(&before_pool).await;
    let before_foreign_keys = item_foreign_keys(&before_pool).await;
    before_pool.close().await;

    let pool = db::connect(&url).await.unwrap();

    assert!(before_count > 0, "the real copy should contain invoices");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items")
            .fetch_one(&pool)
            .await
            .unwrap(),
        before_count
    );
    assert_eq!(item_fingerprint(&pool).await, before_items);
    assert_eq!(semantic_identity_fingerprint(&pool).await, before_semantics);
    assert_eq!(item_columns(&pool).await, before_columns);
    assert_eq!(item_indexes(&pool).await, before_indexes);
    assert_eq!(item_foreign_keys(&pool).await, before_foreign_keys);
    let applied: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 16 AND success = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(applied, 1);

    sqlx::query("UPDATE items SET final_category = 'other' WHERE final_category IS NULL")
        .execute(&pool)
        .await
        .expect("the real copy must accept 'other' after migration 0016");
    println!("real copy migrated: {before_count} items, every value preserved");
}
