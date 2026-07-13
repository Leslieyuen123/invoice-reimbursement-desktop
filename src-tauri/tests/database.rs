use chrono::{TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::items::{ItemFilter, ItemPatch, ItemRepository, NewItemRecord};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, ItemStatus, RecognitionStatus, SourceType,
};
use sqlx::{SqlitePool, sqlite::SqliteQueryResult};
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
    first.source_uid = Some(42);
    first.source_message_id = Some("message-first".to_owned());
    first.source_part_id = Some("2".to_owned());
    let mut duplicate = sample_item("email-duplicate", "sha256-email-duplicate");
    duplicate.source_type = SourceType::Email;
    duplicate.source_account_id = Some(account_id);
    duplicate.source_mailbox = Some("INBOX".to_owned());
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
            .list(ItemFilter {
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
    let repository = ItemRepository::new(pool);
    let mut target = sample_item("Quarterly-Invoice", "sha256-filter-target");
    target.id = Uuid::from_u128(3);
    target.source_type = SourceType::Email;
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
        .list(ItemFilter::default())
        .await
        .expect("unfiltered list should succeed");
    assert_eq!(
        all.iter().map(|item| item.id).collect::<Vec<_>>(),
        vec![target.id, same_time_lower_id.id, older.id]
    );

    for query in ["quarterly", "ACME", "shenzhen", "board meeting"] {
        let matches = repository
            .list(ItemFilter {
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
            .list(filter)
            .await
            .expect("typed filter should succeed");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, target.id);
    }

    let category_override = repository
        .list(ItemFilter {
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
         (id, provider, email, host, port, sync_interval_minutes, created_at, updated_at) \
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
