use chrono::{NaiveDate, TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::batches::{Batch, BatchRepository};
use invoice_reimbursement::db::items::{ItemPatch, ItemRepository, NewItemRecord};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{
    BatchStatus, Category, ConfirmationStatus, DedupeStatus, ItemStatus, RecognitionStatus,
    SourceType,
};
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::services::batches::{BatchService, NewBatchInput, SettleBatchInput};
use invoice_reimbursement::services::items::{ItemReview, ItemService};
use invoice_reimbursement::services::recognition::RecognitionService;
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Barrier;
use uuid::Uuid;

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("test date should be valid")
}

async fn service() -> BatchService {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    BatchService::new(pool)
}

fn sample_item(id: u128, period: Option<&str>) -> NewItemRecord {
    let id = Uuid::from_u128(id);
    let now = Utc
        .with_ymd_and_hms(2026, 7, 13, 2, 0, 0)
        .single()
        .expect("test time should be valid");
    NewItemRecord {
        id,
        original_name: format!("{id}.pdf"),
        original_path: format!("/tmp/{id}.pdf"),
        normalized_pdf_path: Some(format!("/tmp/normalized-{id}.pdf")),
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
        invoice_date: Some(date(2026, 2, 15)),
        suggested_period: period.map(str::to_owned),
        batch_id: None,
        suggested_category: Some(Category::Transport),
        final_category: Some(Category::Transport),
        amount_cents: Some(100),
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

fn email_item(
    id: u128,
    fetched_at: chrono::DateTime<Utc>,
    invoice_date: Option<NaiveDate>,
) -> NewItemRecord {
    let mut item = sample_item(
        id,
        invoice_date
            .map(|date| date.format("%Y-%m").to_string())
            .as_deref(),
    );
    item.source_type = SourceType::Email;
    item.source_account_id = Some(Uuid::from_u128(10_000 + id));
    item.source_mailbox = Some("INBOX".to_owned());
    item.source_uid_validity = Some(1);
    item.source_uid = Some(i64::try_from(id).expect("test id should fit in i64"));
    item.source_message_id = Some(format!("<message-{id}@example.com>"));
    item.source_part_id = Some("1".to_owned());
    item.fetched_at = fetched_at;
    item.source_received_date = Some(fetched_at.date_naive());
    item.invoice_date = invoice_date;
    item
}

async fn mark_exported(pool: &SqlitePool, batch_id: Uuid) {
    sqlx::query(
        "UPDATE batches SET status = 'exported', last_exported_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind("2026-07-14T08:00:00Z")
    .bind("2026-07-14T08:00:00Z")
    .bind(batch_id.to_string())
    .execute(pool)
    .await
    .expect("batch should mark exported");
}

struct SuccessfulExtractor;

impl DocumentExtractor for SuccessfulExtractor {
    fn extract(&self, _path: &Path) -> Result<ExtractedDocument, AppError> {
        Ok(ExtractedDocument {
            text: "开票日期：2026年02月16日 餐饮服务 价税合计 ¥12.34".to_owned(),
            normalized_pdf: None,
            warnings: Vec::new(),
        })
    }
}

#[tokio::test]
async fn create_month_uses_the_full_calendar_month_and_expected_name() {
    let service = service().await;

    let batch: Batch = service
        .create_month(2026, 7)
        .await
        .expect("monthly batch should create");

    assert_eq!(batch.name, "2026 年 7 月报销");
    assert_eq!(batch.start_date, date(2026, 7, 1));
    assert_eq!(batch.end_date, date(2026, 7, 31));
}

#[tokio::test]
async fn create_month_handles_leap_february_and_december() {
    let service = service().await;

    let leap = service
        .create_month(2024, 2)
        .await
        .expect("leap February should create");
    let december = service
        .create_month(2026, 12)
        .await
        .expect("December should create");

    assert_eq!(leap.end_date, date(2024, 2, 29));
    assert_eq!(december.end_date, date(2026, 12, 31));
}

#[tokio::test]
async fn create_month_rejects_invalid_years_and_months_stably() {
    for (year, month, field) in [
        (0, 1, "year"),
        (10_000, 1, "year"),
        (2026, 0, "month"),
        (2026, 13, "month"),
    ] {
        let error = service()
            .await
            .create_month(year, month)
            .await
            .expect_err("invalid year or month should fail");

        assert!(
            matches!(error, AppError::Validation { field: ref error_field, .. } if error_field == field),
            "unexpected error for ({year}, {month}): {error:?}"
        );
    }
}

#[tokio::test]
async fn create_custom_validates_dates_and_normalizes_text() {
    let service = service().await;

    let batch: Batch = service
        .create(NewBatchInput {
            name: "  补开跨月报销  ".to_owned(),
            start_date: "2026-01-31".to_owned(),
            end_date: "2026-03-01".to_owned(),
            note: Some("  客户项目  ".to_owned()),
        })
        .await
        .expect("custom batch should create");

    assert_eq!(batch.name, "补开跨月报销");
    assert_eq!(batch.start_date, date(2026, 1, 31));
    assert_eq!(batch.end_date, date(2026, 3, 1));
    assert_eq!(batch.note.as_deref(), Some("客户项目"));

    let invalid = service
        .create(NewBatchInput {
            name: "batch".to_owned(),
            start_date: "2026-02-30".to_owned(),
            end_date: "2026-03-01".to_owned(),
            note: None,
        })
        .await
        .expect_err("impossible date should fail");
    assert!(matches!(invalid, AppError::Validation { ref field, .. } if field == "startDate"));
}

#[tokio::test]
async fn recommend_includes_boundary_months_and_all_unassigned_item_states() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());

    let mut candidates = vec![
        sample_item(3, Some("2026-03")),
        sample_item(1, Some("2026-01")),
        sample_item(2, Some("2026-02")),
    ];
    candidates[2].invoice_date = Some(date(2026, 2, 1));
    for item in &candidates {
        items.insert(item).await.expect("candidate should insert");
    }

    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let mut assigned = sample_item(4, Some("2026-02"));
    assigned.batch_id = Some(batch.id);
    let mut duplicate = sample_item(5, Some("2026-02"));
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    let mut failed = sample_item(6, Some("2026-02"));
    failed.recognition_status = RecognitionStatus::Failed;
    let before = sample_item(7, Some("2025-12"));
    let after = sample_item(8, Some("2026-04"));
    let no_period = sample_item(9, None);
    for item in [&assigned, &duplicate, &failed, &before, &after, &no_period] {
        items
            .insert(item)
            .await
            .expect("filtered item should insert");
    }

    let recommended = service
        .recommend("2026-01-31", "2026-03-01")
        .await
        .expect("recommendation should succeed");

    assert_eq!(
        recommended.iter().map(|item| item.id).collect::<Vec<_>>(),
        vec![
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            Uuid::from_u128(5),
            Uuid::from_u128(6),
            Uuid::from_u128(3),
        ]
    );
    assert!(recommended.iter().all(|item| item.batch_id.is_none()));
    assert_eq!(
        recommended[2].dedupe_status,
        DedupeStatus::SuspectedDuplicate
    );
    assert_eq!(recommended[3].recognition_status, RecognitionStatus::Failed);
}

#[tokio::test]
async fn recommend_rejects_non_iso_impossible_and_reversed_ranges() {
    let service = service().await;

    for (start, end, field) in [
        ("2026-2-01", "2026-03-01", "startDate"),
        ("2026-02-30", "2026-03-01", "startDate"),
        ("2026-02-01", "2026-2-28", "endDate"),
        ("2026-03-01", "2026-02-28", "dateRange"),
    ] {
        let error = service
            .recommend(start, end)
            .await
            .expect_err("invalid range should fail");
        assert!(
            matches!(error, AppError::Validation { field: ref error_field, .. } if error_field == field),
            "unexpected error for {start}..={end}: {error:?}"
        );
    }
}

#[tokio::test]
async fn batch_candidates_match_email_by_received_date_and_manual_uploads_by_invoice_date() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool);
    let batch = service
        .create_month(2026, 8)
        .await
        .expect("August batch should create");
    let other_batch = service
        .create_month(2026, 9)
        .await
        .expect("other batch should create");

    let august_received = Utc.with_ymd_and_hms(2026, 8, 15, 8, 0, 0).single().unwrap();
    let july_received = Utc
        .with_ymd_and_hms(2026, 7, 31, 23, 59, 59)
        .single()
        .unwrap();
    let email_with_july_invoice = email_item(100, august_received, Some(date(2026, 7, 31)));
    let email_without_invoice_date = email_item(101, august_received, None);
    let email_received_in_july = email_item(102, july_received, Some(date(2026, 8, 1)));
    let email_received_at_month_end = email_item(
        106,
        Utc.with_ymd_and_hms(2026, 8, 31, 23, 59, 59)
            .single()
            .unwrap(),
        Some(date(2026, 9, 1)),
    );
    let email_received_in_september = email_item(
        107,
        Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).single().unwrap(),
        Some(date(2026, 8, 31)),
    );
    let mut manual_with_august_invoice = sample_item(103, Some("2026-08"));
    manual_with_august_invoice.invoice_date = Some(date(2026, 8, 31));
    let mut manual_with_july_invoice = sample_item(104, Some("2026-07"));
    manual_with_july_invoice.fetched_at = august_received;
    manual_with_july_invoice.invoice_date = Some(date(2026, 7, 31));
    let mut already_assigned = email_item(105, august_received, Some(date(2026, 8, 10)));
    already_assigned.batch_id = Some(other_batch.id);

    for item in [
        &email_with_july_invoice,
        &email_without_invoice_date,
        &email_received_in_july,
        &email_received_at_month_end,
        &email_received_in_september,
        &manual_with_august_invoice,
        &manual_with_july_invoice,
        &already_assigned,
    ] {
        items
            .insert(item)
            .await
            .expect("candidate fixture should insert");
    }

    let page = service
        .list_candidates(batch.id, None, None, 20)
        .await
        .expect("candidate list should load");
    let candidate_ids = page
        .items
        .into_iter()
        .map(|item| item.id)
        .collect::<HashSet<_>>();

    assert_eq!(
        candidate_ids,
        HashSet::from([
            email_with_july_invoice.id,
            email_without_invoice_date.id,
            email_received_at_month_end.id,
            manual_with_august_invoice.id,
        ])
    );
}

#[tokio::test]
async fn batch_candidates_keep_the_imap_internal_date_calendar_day_across_utc_boundary() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let batch = service
        .create_month(2026, 8)
        .await
        .expect("August batch should create");
    let account_id = Uuid::from_u128(20_000);
    sqlx::query(
        "INSERT INTO mailbox_accounts (
            id, provider, email, imap_host, imap_port, sync_interval_minutes, created_at, updated_at
         ) VALUES (?, 'qq', 'boundary@example.com', 'imap.qq.com', 993, 15, ?, ?)",
    )
    .bind(account_id.to_string())
    .bind("2026-08-01T00:30:00+08:00")
    .bind("2026-08-01T00:30:00+08:00")
    .execute(&pool)
    .await
    .expect("mailbox account fixture should insert");

    let local_received_at = chrono::FixedOffset::east_opt(8 * 60 * 60)
        .unwrap()
        .with_ymd_and_hms(2026, 8, 1, 0, 30, 0)
        .single()
        .unwrap();
    let fetched_at = local_received_at.with_timezone(&Utc);
    assert_eq!(fetched_at.date_naive(), date(2026, 7, 31));
    sqlx::query(
        "INSERT INTO items (
            id, original_name, original_path, sha256, mime_type, source_type,
            source_account_id, source_mailbox, source_uid_validity, source_uid,
            source_message_id, source_part_id, fetched_at, source_received_date,
            recognition_status, confirmation_status, dedupe_status, created_at, updated_at
         ) VALUES (?, ?, ?, ?, 'application/pdf', 'email', ?, 'INBOX', 1, 1, ?, '1', ?, ?,
                   'pending', 'pending', 'unique', ?, ?)",
    )
    .bind(Uuid::from_u128(20_001).to_string())
    .bind("august-boundary.pdf")
    .bind("/invoices/august-boundary.pdf")
    .bind("sha256-august-boundary")
    .bind(account_id.to_string())
    .bind("<august-boundary@example.com>")
    .bind(fetched_at.to_rfc3339())
    .bind(local_received_at.date_naive().to_string())
    .bind(fetched_at.to_rfc3339())
    .bind(fetched_at.to_rfc3339())
    .execute(&pool)
    .await
    .expect("boundary email fixture should insert");

    let page = service
        .list_candidates(batch.id, None, None, 20)
        .await
        .expect("candidate list should load");

    assert_eq!(page.items.len(), 1);
    assert_eq!(
        page.items[0].batch_membership_date(),
        Some(date(2026, 8, 1))
    );
}

#[tokio::test]
async fn batch_candidates_do_not_guess_a_legacy_email_month_from_utc_fetched_at() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let july = service
        .create_month(2026, 7)
        .await
        .expect("July batch should create");
    let august = service
        .create_month(2026, 8)
        .await
        .expect("August batch should create");
    let account_id = Uuid::from_u128(20_100);
    sqlx::query(
        "INSERT INTO mailbox_accounts (
            id, provider, email, imap_host, imap_port, sync_interval_minutes, created_at, updated_at
         ) VALUES (?, 'qq', 'legacy-boundary@example.com', 'imap.qq.com', 993, 15, ?, ?)",
    )
    .bind(account_id.to_string())
    .bind("2026-08-01T00:30:00+08:00")
    .bind("2026-08-01T00:30:00+08:00")
    .execute(&pool)
    .await
    .expect("mailbox account fixture should insert");

    let item_id = Uuid::from_u128(20_101);
    sqlx::query(
        "INSERT INTO items (
            id, original_name, original_path, sha256, mime_type, source_type,
            source_account_id, source_mailbox, source_uid_validity, source_uid,
            source_message_id, source_part_id, fetched_at, source_received_date,
            recognition_status, confirmation_status, dedupe_status, created_at, updated_at
         ) VALUES (?, ?, ?, ?, 'application/pdf', 'email', ?, 'INBOX', 1, 1, ?, '1', ?, NULL,
                   'pending', 'pending', 'unique', ?, ?)",
    )
    .bind(item_id.to_string())
    .bind("legacy-august-boundary.pdf")
    .bind("/invoices/legacy-august-boundary.pdf")
    .bind("sha256-legacy-august-boundary")
    .bind(account_id.to_string())
    .bind("<legacy-august-boundary@example.com>")
    .bind("2026-07-31T16:30:00Z")
    .bind("2026-07-31T16:30:00Z")
    .bind("2026-07-31T16:30:00Z")
    .execute(&pool)
    .await
    .expect("legacy email fixture should insert");

    for batch in [july, august] {
        let page = service
            .list_candidates(batch.id, None, None, 20)
            .await
            .expect("candidate list should load");
        assert!(
            page.items.is_empty(),
            "unknown legacy calendar dates must wait for an authoritative IMAP rescan"
        );
    }

    let item = ItemRepository::new(pool)
        .get_by_id(item_id)
        .await
        .expect("legacy item should load");
    assert_eq!(item.batch_membership_date(), None);
}

#[tokio::test]
async fn assign_refuses_members_the_export_would_reject() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let mut missing_normalized = sample_item(90, Some("2026-02"));
    missing_normalized.normalized_pdf_path = None;
    let mut unconfirmed = sample_item(91, Some("2026-02"));
    unconfirmed.confirmation_status = ConfirmationStatus::Pending;
    for item in [&missing_normalized, &unconfirmed] {
        items.insert(item).await.expect("item should insert");
    }

    let error = service
        .assign_items(batch.id, &[missing_normalized.id])
        .await
        .expect_err("an invoice without a normalized PDF must not join a batch");
    assert!(matches!(
        error,
        AppError::Conflict { ref message }
            if message.contains("归一化 PDF") && message.contains(&missing_normalized.original_name)
    ));

    let error = service
        .assign_items(batch.id, &[unconfirmed.id])
        .await
        .expect_err("an unconfirmed invoice must not join a batch");
    assert!(matches!(error, AppError::Conflict { .. }));

    let detail = service.get(batch.id).await.expect("batch should reload");
    assert!(detail.items.is_empty());
}

#[tokio::test]
async fn settle_confirms_derivable_items_and_reports_every_skip() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");

    // (a) email invoice: the missing invoice date can come from the mail date.
    let mut derivable = email_item(
        100,
        Utc.with_ymd_and_hms(2026, 2, 10, 3, 0, 0)
            .single()
            .expect("valid time"),
        None,
    );
    derivable.confirmation_status = ConfirmationStatus::Pending;
    derivable.suggested_category = Some(Category::Transport);
    derivable.final_category = None;
    derivable.suggested_period = None;
    derivable.amount_cents = Some(100);

    // (b) reviewed category missing but the default category can fill it in.
    let mut no_category = sample_item(101, Some("2026-02"));
    no_category.confirmation_status = ConfirmationStatus::Pending;
    no_category.suggested_category = None;
    no_category.final_category = None;

    // (c..h) every reason the bulk confirm must refuse to guess.
    let mut no_amount = sample_item(102, Some("2026-02"));
    no_amount.confirmation_status = ConfirmationStatus::Pending;
    no_amount.amount_cents = None;

    let mut archive = sample_item(103, Some("2026-02"));
    archive.confirmation_status = ConfirmationStatus::Pending;
    archive.original_name = "invoice-archive.zip".to_owned();
    archive.mime_type = "application/zip".to_owned();
    archive.normalized_pdf_path = None;

    let mut failed = sample_item(104, Some("2026-02"));
    failed.confirmation_status = ConfirmationStatus::Pending;
    failed.recognition_status = RecognitionStatus::Failed;

    let mut duplicate = sample_item(105, Some("2026-02"));
    duplicate.confirmation_status = ConfirmationStatus::Pending;

    let mut no_date = sample_item(106, Some("2026-02"));
    no_date.confirmation_status = ConfirmationStatus::Pending;
    no_date.source_type = SourceType::ManualUpload;
    no_date.source_received_date = None;
    no_date.invoice_date = None;

    let mut outside = sample_item(107, Some("2026-03"));
    outside.confirmation_status = ConfirmationStatus::Pending;
    outside.invoice_date = Some(date(2026, 3, 5));

    for item in [
        &derivable,
        &no_category,
        &no_amount,
        &archive,
        &failed,
        &duplicate,
        &no_date,
        &outside,
    ] {
        items
            .insert(item)
            .await
            .expect("fixture item should insert");
    }
    // Membership and duplicate state are written directly: this is the legacy
    // state a batch can already hold, which the bulk confirm has to cope with.
    for item in [
        &derivable,
        &no_category,
        &no_amount,
        &archive,
        &failed,
        &duplicate,
        &no_date,
        &outside,
    ] {
        sqlx::query("UPDATE items SET batch_id = ? WHERE id = ?")
            .bind(batch.id.to_string())
            .bind(item.id.to_string())
            .execute(&pool)
            .await
            .expect("fixture membership should update");
    }
    sqlx::query("UPDATE items SET dedupe_status = 'suspected_duplicate' WHERE id = ?")
        .bind(duplicate.id.to_string())
        .execute(&pool)
        .await
        .expect("duplicate fixture should update");

    let outcome = service
        .settle_items(
            batch.id,
            SettleBatchInput {
                fill_invoice_date_from_received: true,
                apply_suggested_category: true,
                default_category: Some(Category::Hospitality),
            },
        )
        .await
        .expect("bulk confirm should run");

    assert_eq!(outcome.confirmed_count, 2);
    assert_eq!(outcome.filled_invoice_date_count, 1);
    assert_eq!(outcome.applied_category_count, 2);
    let mut codes: Vec<&str> = outcome
        .skipped
        .iter()
        .map(|skipped| skipped.code.as_str())
        .collect();
    codes.sort_unstable();
    assert_eq!(
        codes,
        vec![
            "missing_amount",
            "missing_invoice_date",
            "outside_date_range",
            "recognition_failed",
            "suspected_duplicate",
            "unsupported_format",
        ]
    );

    let confirmed = items
        .get_by_id(derivable.id)
        .await
        .expect("item should reload");
    assert_eq!(confirmed.status(), ItemStatus::Ready);
    assert_eq!(confirmed.invoice_date, Some(date(2026, 2, 10)));
    assert_eq!(confirmed.suggested_period.as_deref(), Some("2026-02"));
    assert_eq!(confirmed.final_category, Some(Category::Transport));
    let defaulted = items
        .get_by_id(no_category.id)
        .await
        .expect("item should reload");
    assert_eq!(defaulted.final_category, Some(Category::Hospitality));
    let untouched = items
        .get_by_id(no_amount.id)
        .await
        .expect("item should reload");
    assert_eq!(untouched.confirmation_status, ConfirmationStatus::Pending);
}

#[tokio::test]
async fn settle_can_leave_invoice_dates_and_categories_alone() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let mut item = email_item(
        110,
        Utc.with_ymd_and_hms(2026, 2, 10, 3, 0, 0)
            .single()
            .expect("valid time"),
        None,
    );
    item.confirmation_status = ConfirmationStatus::Pending;
    item.suggested_category = Some(Category::Transport);
    item.final_category = None;
    items
        .insert(&item)
        .await
        .expect("fixture item should insert");
    sqlx::query("UPDATE items SET batch_id = ? WHERE id = ?")
        .bind(batch.id.to_string())
        .bind(item.id.to_string())
        .execute(&pool)
        .await
        .expect("fixture membership should update");

    let outcome = service
        .settle_items(batch.id, SettleBatchInput::default())
        .await
        .expect("bulk confirm should run");

    assert_eq!(outcome.confirmed_count, 0);
    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(outcome.skipped[0].code, "missing_invoice_date");
    assert_eq!(
        items
            .get_by_id(item.id)
            .await
            .expect("item should reload")
            .confirmation_status,
        ConfirmationStatus::Pending
    );
}

#[tokio::test]
async fn updating_a_batch_range_keeps_members_and_returns_it_to_draft() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let mut inside = sample_item(130, Some("2026-02"));
    inside.batch_id = Some(batch.id);
    items.insert(&inside).await.expect("item should insert");
    mark_exported(&pool, batch.id).await;

    // The invoice the user could not fit before: one day after the old range.
    let detail = service
        .update_range(batch.id, "2026-02-01", "2026-03-05")
        .await
        .expect("range update should succeed");

    assert_eq!(detail.batch.start_date, date(2026, 2, 1));
    assert_eq!(detail.batch.end_date, date(2026, 3, 5));
    assert_eq!(detail.batch.status, BatchStatus::Draft);
    assert_eq!(detail.items.len(), 1, "members are kept");
    let persisted = BatchRepository::new(pool)
        .get(batch.id)
        .await
        .expect("batch should reload");
    assert_eq!(persisted.end_date, date(2026, 3, 5));
}

#[tokio::test]
async fn updating_a_batch_range_rejects_unusable_ranges() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");

    let reversed = service
        .update_range(batch.id, "2026-03-05", "2026-02-01")
        .await
        .expect_err("a reversed range must be rejected");
    assert!(matches!(
        reversed,
        AppError::Validation { ref field, .. } if field == "dateRange"
    ));

    let too_long = service
        .update_range(batch.id, "2026-01-01", "2027-06-01")
        .await
        .expect_err("a range the automation cannot process must be rejected");
    assert!(matches!(
        too_long,
        AppError::Validation { ref field, .. } if field == "dateRange"
    ));

    let malformed = service
        .update_range(batch.id, "2026-2-01", "2026-03-05")
        .await
        .expect_err("a non-ISO date must be rejected");
    assert!(matches!(
        malformed,
        AppError::Validation { ref field, .. } if field == "startDate"
    ));

    let missing = service
        .update_range(Uuid::from_u128(4321), "2026-02-01", "2026-03-05")
        .await
        .expect_err("an unknown batch must be reported");
    assert!(matches!(missing, AppError::NotFound { ref entity, .. } if entity == "batch"));
}

#[tokio::test]
async fn bulk_removal_ignores_foreign_ids_and_drafts_the_batch() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let other = service
        .create_month(2026, 3)
        .await
        .expect("other batch should create");
    for id in [120, 121] {
        let mut item = sample_item(id, Some("2026-02"));
        item.batch_id = Some(batch.id);
        items.insert(&item).await.expect("item should insert");
    }
    let mut foreign = sample_item(122, Some("2026-03"));
    foreign.batch_id = Some(other.id);
    items
        .insert(&foreign)
        .await
        .expect("foreign item should insert");
    mark_exported(&pool, batch.id).await;

    let (detail, removed) = service
        .remove_items(
            batch.id,
            &[Uuid::from_u128(120), Uuid::from_u128(121), foreign.id],
        )
        .await
        .expect("bulk removal should run");

    assert_eq!(removed, 2);
    assert!(detail.items.is_empty());
    assert_eq!(detail.batch.status, BatchStatus::Draft);
    assert_eq!(
        items
            .get_by_id(foreign.id)
            .await
            .expect("foreign item should remain")
            .batch_id,
        Some(other.id)
    );
}

#[tokio::test]
async fn assign_is_idempotent_deduplicates_input_and_warns_only_for_outside_dates() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let mut outside = sample_item(20, Some("2026-01"));
    outside.invoice_date = Some(date(2026, 1, 31));
    let mut no_date = sample_item(21, Some("2026-01"));
    no_date.invoice_date = None;
    items
        .insert(&outside)
        .await
        .expect("outside item should insert");
    items
        .insert(&no_date)
        .await
        .expect("dateless item should insert");

    let detail = service
        .assign_items(batch.id, &[outside.id, outside.id, no_date.id])
        .await
        .expect("valid items should assign");

    assert_eq!(detail.items.len(), 2);
    assert_eq!(
        detail.warnings,
        vec![format!("outside_date_range:{}", outside.id)]
    );
    let again = service
        .assign_items(batch.id, &[outside.id])
        .await
        .expect("same-batch assignment should be idempotent");
    assert_eq!(again.items.len(), 2);
    let empty = service
        .assign_items(batch.id, &[])
        .await
        .expect("empty assignment should be a no-op for an existing batch");
    assert_eq!(empty.items.len(), 2);
}

#[tokio::test]
async fn assign_rejects_invalid_states_and_rolls_back_every_item() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool);
    let target = service
        .create_month(2026, 2)
        .await
        .expect("target batch should create");
    let other = service
        .create_month(2026, 3)
        .await
        .expect("other batch should create");
    let valid = sample_item(30, Some("2026-02"));
    let mut duplicate = sample_item(31, Some("2026-02"));
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    let mut failed = sample_item(32, Some("2026-02"));
    failed.recognition_status = RecognitionStatus::Failed;
    let mut elsewhere = sample_item(33, Some("2026-02"));
    elsewhere.batch_id = Some(other.id);
    for item in [&valid, &duplicate, &failed, &elsewhere] {
        items.insert(item).await.expect("item should insert");
    }

    for invalid_id in [duplicate.id, failed.id, Uuid::from_u128(99)] {
        let error = service
            .assign_items(target.id, &[valid.id, elsewhere.id, invalid_id])
            .await
            .expect_err("one invalid item should reject the whole assignment");
        assert!(matches!(
            error,
            AppError::Conflict { .. } | AppError::NotFound { .. }
        ));
        assert_eq!(
            items
                .get_by_id(valid.id)
                .await
                .expect("valid item should remain")
                .batch_id,
            None
        );
    }
    assert_eq!(
        items
            .get_by_id(elsewhere.id)
            .await
            .expect("other item should remain")
            .batch_id,
        Some(other.id)
    );
}

#[tokio::test]
async fn assign_moves_items_from_another_batch_and_falls_back_both_exported_batches() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let old_batch = service
        .create_month(2026, 1)
        .await
        .expect("old batch should create");
    let new_batch = service
        .create_month(2026, 2)
        .await
        .expect("new batch should create");
    let mut moved = sample_item(34, Some("2026-01"));
    moved.batch_id = Some(old_batch.id);
    let fresh = sample_item(35, Some("2026-02"));
    items
        .insert(&moved)
        .await
        .expect("moved item should insert");
    items
        .insert(&fresh)
        .await
        .expect("fresh item should insert");
    mark_exported(&pool, old_batch.id).await;
    mark_exported(&pool, new_batch.id).await;

    let detail = service
        .assign_items(new_batch.id, &[moved.id, fresh.id])
        .await
        .expect("cross-batch assignment should move the item");

    assert_eq!(detail.batch.id, new_batch.id);
    assert_eq!(detail.items.len(), 2);
    assert!(
        detail
            .items
            .iter()
            .all(|item| item.batch_id == Some(new_batch.id))
    );
    let repository = BatchRepository::new(pool);
    let persisted_old = repository
        .get(old_batch.id)
        .await
        .expect("old batch should reload");
    let persisted_new = repository
        .get(new_batch.id)
        .await
        .expect("new batch should reload");
    assert_eq!(persisted_old.status, BatchStatus::Draft);
    assert_eq!(persisted_new.status, BatchStatus::Draft);
    assert!(persisted_old.last_exported_at.is_some());
    assert!(persisted_new.last_exported_at.is_some());
}

#[tokio::test]
async fn assign_reports_a_missing_batch_before_changing_items() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool);
    let item = sample_item(40, Some("2026-02"));
    items.insert(&item).await.expect("item should insert");

    let error = service
        .assign_items(Uuid::from_u128(404), &[item.id])
        .await
        .expect_err("missing batch should fail");

    assert!(matches!(error, AppError::NotFound { ref entity, .. } if entity == "batch"));
    assert_eq!(
        items
            .get_by_id(item.id)
            .await
            .expect("item should remain")
            .batch_id,
        None
    );
}

#[tokio::test]
async fn remove_item_only_removes_an_item_owned_by_the_batch() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool);
    let owner = service
        .create_month(2026, 2)
        .await
        .expect("owner batch should create");
    let other = service
        .create_month(2026, 3)
        .await
        .expect("other batch should create");
    let owned = sample_item(50, Some("2026-02"));
    let unassigned = sample_item(51, Some("2026-02"));
    let mut elsewhere = sample_item(52, Some("2026-02"));
    elsewhere.batch_id = Some(other.id);
    for item in [&owned, &unassigned, &elsewhere] {
        items.insert(item).await.expect("item should insert");
    }
    service
        .assign_items(owner.id, &[owned.id])
        .await
        .expect("owned item should assign");

    let detail = service
        .remove_item(owner.id, owned.id)
        .await
        .expect("owned item should remove");

    assert!(detail.items.is_empty());
    assert_eq!(
        items
            .get_by_id(owned.id)
            .await
            .expect("removed item should remain")
            .batch_id,
        None
    );
    for (batch_id, item_id, expected_entity) in [
        (Uuid::from_u128(404), unassigned.id, Some("batch")),
        (owner.id, Uuid::from_u128(405), Some("item")),
        (owner.id, unassigned.id, None),
        (owner.id, elsewhere.id, None),
    ] {
        let error = service
            .remove_item(batch_id, item_id)
            .await
            .expect_err("invalid removal should fail");
        match expected_entity {
            Some(entity) => assert!(
                matches!(error, AppError::NotFound { entity: ref error_entity, .. } if error_entity == entity)
            ),
            None => assert!(matches!(error, AppError::Conflict { .. })),
        }
    }
    assert_eq!(
        items
            .get_by_id(elsewhere.id)
            .await
            .expect("other item should remain")
            .batch_id,
        Some(other.id)
    );
}

#[tokio::test]
async fn summary_counts_total_unconfirmed_and_only_final_categories() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool);
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let cases = [
        (
            60,
            Some(Category::Transport),
            Some(100),
            ConfirmationStatus::Confirmed,
        ),
        (
            61,
            Some(Category::Dining),
            Some(200),
            ConfirmationStatus::Confirmed,
        ),
        (
            62,
            Some(Category::Accommodation),
            None,
            ConfirmationStatus::Confirmed,
        ),
        (
            63,
            Some(Category::Hospitality),
            Some(400),
            ConfirmationStatus::Confirmed,
        ),
        (64, None, Some(500), ConfirmationStatus::Pending),
        (65, None, Some(600), ConfirmationStatus::Confirmed),
    ];
    let mut ids = Vec::new();
    for (id, final_category, amount_cents, confirmation_status) in cases {
        let mut item = sample_item(id, Some("2026-02"));
        item.final_category = final_category;
        item.amount_cents = amount_cents;
        item.confirmation_status = confirmation_status;
        // Legacy batches can hold members that today's assignment guard would
        // refuse, and the summary must still count them.
        item.batch_id = Some(batch.id);
        ids.push(item.id);
        items
            .insert(&item)
            .await
            .expect("summary item should insert");
    }

    let detail = service
        .get(batch.id)
        .await
        .expect("batch detail should load");

    assert_eq!(detail.summary.item_count, 6);
    assert_eq!(detail.summary.total_amount_cents, 1_800);
    assert_eq!(detail.summary.transport.item_count, 1);
    assert_eq!(detail.summary.transport.amount_cents, 100);
    assert_eq!(detail.summary.dining.item_count, 1);
    assert_eq!(detail.summary.dining.amount_cents, 200);
    assert_eq!(detail.summary.accommodation.item_count, 1);
    assert_eq!(detail.summary.accommodation.amount_cents, 0);
    assert_eq!(detail.summary.hospitality.item_count, 1);
    assert_eq!(detail.summary.hospitality.amount_cents, 400);
    assert_eq!(detail.summary.unconfirmed_count, 1);
}

#[tokio::test]
async fn summary_reports_amount_overflow_without_panicking() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let mut maximum = sample_item(66, Some("2026-02"));
    maximum.amount_cents = Some(1);
    let mut one = sample_item(67, Some("2026-02"));
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

    let error = service
        .assign_items(batch.id, &[maximum.id, one.id])
        .await
        .expect_err("overflowing summary should return an error");

    assert!(matches!(
        error,
        AppError::Validation { ref field, .. } if field == "totalAmountCents"
    ));
    assert_eq!(
        items
            .get_by_id(maximum.id)
            .await
            .expect("item should remain after rollback")
            .batch_id,
        None
    );
}

#[tokio::test]
async fn updating_exported_batch_name_or_dates_falls_back_and_preserves_export_time() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    mark_exported(&pool, batch.id).await;

    let unchanged = service
        .update(
            batch.id,
            NewBatchInput {
                name: batch.name.clone(),
                start_date: batch.start_date.to_string(),
                end_date: batch.end_date.to_string(),
                note: None,
            },
        )
        .await
        .expect("unchanged update should succeed");
    assert_eq!(unchanged.batch.status, BatchStatus::Exported);

    let renamed = service
        .update(
            batch.id,
            NewBatchInput {
                name: "  February amended  ".to_owned(),
                start_date: "2026-02-01".to_owned(),
                end_date: "2026-02-28".to_owned(),
                note: Some("  close  ".to_owned()),
            },
        )
        .await
        .expect("renamed batch should update");
    assert_eq!(renamed.batch.status, BatchStatus::Draft);
    assert_eq!(renamed.batch.name, "February amended");
    assert_eq!(renamed.batch.note.as_deref(), Some("close"));
    assert_eq!(
        renamed
            .batch
            .last_exported_at
            .expect("export time should remain")
            .to_rfc3339(),
        "2026-07-14T08:00:00+00:00"
    );

    mark_exported(&pool, batch.id).await;
    let redated = service
        .update(
            batch.id,
            NewBatchInput {
                name: renamed.batch.name,
                start_date: "2026-01-31".to_owned(),
                end_date: "2026-03-01".to_owned(),
                note: renamed.batch.note,
            },
        )
        .await
        .expect("redated batch should update");
    assert_eq!(redated.batch.status, BatchStatus::Draft);
    assert!(matches!(
        service
            .update(
                Uuid::from_u128(999),
                NewBatchInput {
                    name: "missing".to_owned(),
                    start_date: "2026-01-01".to_owned(),
                    end_date: "2026-01-31".to_owned(),
                    note: None,
                },
            )
            .await,
        Err(AppError::NotFound { ref entity, .. }) if entity == "batch"
    ));
}

#[tokio::test]
async fn update_rolls_back_batch_fields_when_detail_amounts_overflow() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let mut maximum = sample_item(68, Some("2026-02"));
    maximum.amount_cents = None;
    let mut one = sample_item(69, Some("2026-02"));
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
        .bind(batch.id.to_string())
        .bind(maximum.id.to_string())
        .bind(one.id.to_string())
        .execute(&pool)
        .await
        .expect("overflow fixture should assign directly");
    mark_exported(&pool, batch.id).await;

    let error = service
        .update(
            batch.id,
            NewBatchInput {
                name: "Must roll back".to_owned(),
                start_date: "2026-01-31".to_owned(),
                end_date: "2026-03-01".to_owned(),
                note: None,
            },
        )
        .await
        .expect_err("overflowing detail should fail the update");

    assert!(matches!(
        error,
        AppError::Validation { ref field, .. } if field == "totalAmountCents"
    ));
    let persisted = BatchRepository::new(pool)
        .get(batch.id)
        .await
        .expect("batch should reload");
    assert_eq!(persisted.name, batch.name);
    assert_eq!(persisted.start_date, batch.start_date);
    assert_eq!(persisted.end_date, batch.end_date);
    assert_eq!(persisted.status, BatchStatus::Exported);
    assert!(persisted.last_exported_at.is_some());
}

#[tokio::test]
async fn update_rolls_back_batch_fields_when_an_item_row_cannot_decode() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let mut item = sample_item(73, Some("2026-02"));
    item.batch_id = Some(batch.id);
    items.insert(&item).await.expect("item should insert");
    mark_exported(&pool, batch.id).await;
    sqlx::query("UPDATE items SET invoice_date = 'not-a-date' WHERE id = ?")
        .bind(item.id.to_string())
        .execute(&pool)
        .await
        .expect("bad item row should be injected");

    service
        .update(
            batch.id,
            NewBatchInput {
                name: "Must also roll back".to_owned(),
                start_date: "2026-01-31".to_owned(),
                end_date: "2026-03-01".to_owned(),
                note: None,
            },
        )
        .await
        .expect_err("undecodable detail should fail the update");

    let persisted = BatchRepository::new(pool)
        .get(batch.id)
        .await
        .expect("batch should reload");
    assert_eq!(persisted.name, batch.name);
    assert_eq!(persisted.start_date, batch.start_date);
    assert_eq!(persisted.end_date, batch.end_date);
    assert_eq!(persisted.status, BatchStatus::Exported);
    assert!(persisted.last_exported_at.is_some());
}

#[tokio::test]
async fn exported_batch_falls_back_only_for_actual_assignment_changes() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let item = sample_item(70, Some("2026-02"));
    items.insert(&item).await.expect("item should insert");
    mark_exported(&pool, batch.id).await;

    let assigned = service
        .assign_items(batch.id, &[item.id])
        .await
        .expect("assignment should succeed");
    assert_eq!(assigned.batch.status, BatchStatus::Draft);
    assert!(assigned.batch.last_exported_at.is_some());

    mark_exported(&pool, batch.id).await;
    assert_eq!(
        service
            .assign_items(batch.id, &[item.id])
            .await
            .expect("idempotent assignment should succeed")
            .batch
            .status,
        BatchStatus::Exported
    );
    assert_eq!(
        service
            .assign_items(batch.id, &[])
            .await
            .expect("empty assignment should succeed")
            .batch
            .status,
        BatchStatus::Exported
    );

    let removed = service
        .remove_item(batch.id, item.id)
        .await
        .expect("removal should succeed");
    assert_eq!(removed.batch.status, BatchStatus::Draft);
    assert!(removed.batch.last_exported_at.is_some());
}

#[tokio::test]
async fn item_review_and_repository_updates_fall_back_the_exported_batch() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let batches = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = batches
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let item = sample_item(71, Some("2026-02"));
    items.insert(&item).await.expect("item should insert");
    batches
        .assign_items(batch.id, &[item.id])
        .await
        .expect("item should assign");
    mark_exported(&pool, batch.id).await;

    ItemService::new(items.clone(), paths)
        .review(ItemReview {
            id: item.id,
            invoice_date: Some("2026-02-15".to_owned()),
            suggested_period: "2026-02".to_owned(),
            final_category: Category::Dining,
            amount_cents: 999,
            city: None,
            company: None,
            note: None,
            event_tag: None,
            project_tag: None,
        })
        .await
        .expect("review should succeed");
    let reviewed_batch = BatchRepository::new(pool.clone())
        .get(batch.id)
        .await
        .expect("batch should reload");
    assert_eq!(reviewed_batch.status, BatchStatus::Draft);
    assert!(reviewed_batch.last_exported_at.is_some());

    mark_exported(&pool, batch.id).await;
    items
        .update_fields(
            item.id,
            ItemPatch {
                amount_cents: Some(Some(1_001)),
                ..ItemPatch::default()
            },
        )
        .await
        .expect("repository update should succeed");
    let updated_batch = BatchRepository::new(pool)
        .get(batch.id)
        .await
        .expect("batch should reload");
    assert_eq!(updated_batch.status, BatchStatus::Draft);
    assert!(updated_batch.last_exported_at.is_some());
}

#[tokio::test]
async fn recognition_updates_fall_back_the_exported_batch() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let batches = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = batches
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let mut item = sample_item(72, Some("2026-02"));
    item.invoice_date = None;
    item.amount_cents = None;
    item.suggested_category = None;
    item.final_category = None;
    // An incomplete legacy member: assignment refuses it today, but an export
    // fallback must still invalidate a batch that already holds it.
    item.batch_id = Some(batch.id);
    items.insert(&item).await.expect("item should insert");
    batches
        .assign_items(batch.id, &[item.id])
        .await
        .expect("item should assign");
    mark_exported(&pool, batch.id).await;

    RecognitionService::new(items, Arc::new(SuccessfulExtractor))
        .recognize_item(item.id)
        .await
        .expect("recognition should succeed");

    let persisted = BatchRepository::new(pool)
        .get(batch.id)
        .await
        .expect("batch should reload");
    assert_eq!(persisted.status, BatchStatus::Draft);
    assert!(persisted.last_exported_at.is_some());
}

#[tokio::test]
async fn database_failure_rolls_back_assignments_and_export_status_without_sql_leaks() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let first = sample_item(80, Some("2026-02"));
    let second = sample_item(81, Some("2026-02"));
    items
        .insert(&first)
        .await
        .expect("first item should insert");
    items
        .insert(&second)
        .await
        .expect("second item should insert");
    mark_exported(&pool, batch.id).await;
    sqlx::query(&format!(
        "CREATE TRIGGER fail_batch_refresh BEFORE UPDATE ON batches \
         WHEN OLD.id = '{}' AND NEW.status = 'draft' \
         BEGIN SELECT RAISE(ABORT, 'forced batch refresh failure'); END",
        batch.id
    ))
    .execute(&pool)
    .await
    .expect("failure trigger should create");

    let error = service
        .assign_items(batch.id, &[first.id, second.id])
        .await
        .expect_err("injected database failure should fail assignment");

    assert_eq!(
        error,
        AppError::Internal {
            message: "failed to refresh assigned batch".to_owned(),
        }
    );
    assert!(!error.to_string().contains("UPDATE batches"));
    for id in [first.id, second.id] {
        assert_eq!(
            items
                .get_by_id(id)
                .await
                .expect("item should reload")
                .batch_id,
            None
        );
    }
    let persisted = BatchRepository::new(pool)
        .get(batch.id)
        .await
        .expect("batch should reload");
    assert_eq!(persisted.status, BatchStatus::Exported);
    assert!(persisted.last_exported_at.is_some());
}

#[tokio::test]
async fn item_review_rolls_back_when_export_fallback_cannot_be_persisted() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let batches = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool.clone());
    let batch = batches
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let item = sample_item(82, Some("2026-02"));
    let original_amount = item.amount_cents;
    items.insert(&item).await.expect("item should insert");
    batches
        .assign_items(batch.id, &[item.id])
        .await
        .expect("item should assign");
    mark_exported(&pool, batch.id).await;
    sqlx::query(&format!(
        "CREATE TRIGGER fail_review_fallback BEFORE UPDATE ON batches \
         WHEN OLD.id = '{}' AND NEW.status = 'draft' \
         BEGIN SELECT RAISE(ABORT, 'forced review fallback failure'); END",
        batch.id
    ))
    .execute(&pool)
    .await
    .expect("failure trigger should create");

    ItemService::new(items.clone(), paths)
        .review(ItemReview {
            id: item.id,
            invoice_date: Some("2026-02-15".to_owned()),
            suggested_period: "2026-02".to_owned(),
            final_category: Category::Dining,
            amount_cents: 9_999,
            city: None,
            company: None,
            note: None,
            event_tag: None,
            project_tag: None,
        })
        .await
        .expect_err("failed fallback should roll back review");

    let persisted_item = items.get_by_id(item.id).await.expect("item should reload");
    assert_eq!(persisted_item.amount_cents, original_amount);
    assert_eq!(persisted_item.confirmation_status, item.confirmation_status);
    let persisted_batch = BatchRepository::new(pool)
        .get(batch.id)
        .await
        .expect("batch should reload");
    assert_eq!(persisted_batch.status, BatchStatus::Exported);
    assert!(persisted_batch.last_exported_at.is_some());
}

#[tokio::test]
async fn concurrent_assignments_cannot_give_one_item_to_two_batches() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let database_path = directory.path().join("concurrent.sqlite3");
    let pool = db::connect(&format!("sqlite://{}", database_path.display()))
        .await
        .expect("file database should connect");
    let service = BatchService::new(pool.clone());
    let first_batch = service
        .create_month(2026, 2)
        .await
        .expect("first batch should create");
    let second_batch = service
        .create_month(2026, 3)
        .await
        .expect("second batch should create");
    let item = sample_item(83, Some("2026-02"));
    ItemRepository::new(pool.clone())
        .insert(&item)
        .await
        .expect("item should insert");
    mark_exported(&pool, first_batch.id).await;
    mark_exported(&pool, second_batch.id).await;
    let batch_repository = BatchRepository::new(pool.clone());
    let first_before = batch_repository
        .get(first_batch.id)
        .await
        .expect("first batch should load before assignment");
    let second_before = batch_repository
        .get(second_batch.id)
        .await
        .expect("second batch should load before assignment");
    let barrier = Arc::new(Barrier::new(2));
    let first_service = service.clone();
    let first_barrier = barrier.clone();
    let first_id = first_batch.id;
    let item_id = item.id;
    let first = tokio::spawn(async move {
        first_barrier.wait().await;
        first_service.assign_items(first_id, &[item_id]).await
    });
    let second_service = service.clone();
    let second_id = second_batch.id;
    let second = tokio::spawn(async move {
        barrier.wait().await;
        second_service.assign_items(second_id, &[item_id]).await
    });

    let first_result = first.await.expect("first assignment should join");
    let second_result = second.await.expect("second assignment should join");
    let first_detail = first_result.expect("first assignment should succeed");
    let second_detail = second_result.expect("second assignment should succeed");
    for (detail, expected_id) in [(&first_detail, first_id), (&second_detail, second_id)] {
        assert_eq!(detail.batch.id, expected_id);
        assert_eq!(detail.batch.status, BatchStatus::Draft);
        assert!(detail.batch.last_exported_at.is_some());
        assert_eq!(detail.summary.item_count, detail.items.len() as u64);
        assert!(
            detail
                .items
                .iter()
                .all(|item| item.batch_id == Some(expected_id))
        );
    }
    let persisted = ItemRepository::new(pool.clone())
        .get_by_id(item.id)
        .await
        .expect("item should reload");
    let winner = persisted.batch_id.expect("item should remain assigned");
    assert!(winner == first_id || winner == second_id);
    let first_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items WHERE batch_id = ?")
        .bind(first_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("first batch count should load");
    let second_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items WHERE batch_id = ?")
            .bind(second_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("second batch count should load");
    assert_eq!(first_count + second_count, 1);
    assert_eq!(first_count, i64::from(winner == first_id));
    assert_eq!(second_count, i64::from(winner == second_id));

    let first_after = batch_repository
        .get(first_id)
        .await
        .expect("first batch should reload");
    let second_after = batch_repository
        .get(second_id)
        .await
        .expect("second batch should reload");
    assert_eq!(first_after.status, BatchStatus::Draft);
    assert_eq!(second_after.status, BatchStatus::Draft);
    assert!(first_after.last_exported_at.is_some());
    assert!(second_after.last_exported_at.is_some());
    assert!(first_after.updated_at > first_before.updated_at);
    assert!(second_after.updated_at > second_before.updated_at);
    assert_eq!(first_after.updated_at, second_after.updated_at);
}
