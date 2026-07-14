use chrono::{NaiveDate, TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::batches::BatchRepository;
use invoice_reimbursement::db::items::{ItemPatch, ItemRepository, NewItemRecord};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{
    BatchStatus, Category, ConfirmationStatus, DedupeStatus, RecognitionStatus, SourceType,
};
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::services::batches::{BatchService, NewBatchInput};
use invoice_reimbursement::services::items::{ItemReview, ItemService};
use invoice_reimbursement::services::recognition::RecognitionService;
use sqlx::SqlitePool;
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
        normalized_pdf_path: None,
        sha256: format!("sha-{id}"),
        mime_type: "application/pdf".to_owned(),
        source_type: SourceType::ManualUpload,
        source_account_id: None,
        source_mailbox: None,
        source_uid: None,
        source_message_id: None,
        source_part_id: None,
        fetched_at: now,
        invoice_date: Some(date(2026, 2, 15)),
        suggested_period: period.map(str::to_owned),
        batch_id: None,
        suggested_category: Some(Category::Transport),
        final_category: None,
        amount_cents: Some(100),
        currency: "CNY".to_owned(),
        city: None,
        company: None,
        recognition_status: RecognitionStatus::Succeeded,
        confirmation_status: ConfirmationStatus::Pending,
        dedupe_status: DedupeStatus::Unique,
        duplicate_of_id: None,
        note: None,
        event_tag: None,
        project_tag: None,
        created_at: now,
        updated_at: now,
    }
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

    let detail = service
        .create_month(2026, 7)
        .await
        .expect("monthly batch should create");

    assert_eq!(detail.batch.name, "2026 年 7 月报销");
    assert_eq!(detail.batch.start_date, date(2026, 7, 1));
    assert_eq!(detail.batch.end_date, date(2026, 7, 31));
    assert!(detail.items.is_empty());
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

    assert_eq!(leap.batch.end_date, date(2024, 2, 29));
    assert_eq!(december.batch.end_date, date(2026, 12, 31));
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

    let detail = service
        .create(NewBatchInput {
            name: "  补开跨月报销  ".to_owned(),
            start_date: "2026-01-31".to_owned(),
            end_date: "2026-03-01".to_owned(),
            note: Some("  客户项目  ".to_owned()),
        })
        .await
        .expect("custom batch should create");

    assert_eq!(detail.batch.name, "补开跨月报销");
    assert_eq!(detail.batch.start_date, date(2026, 1, 31));
    assert_eq!(detail.batch.end_date, date(2026, 3, 1));
    assert_eq!(detail.batch.note.as_deref(), Some("客户项目"));

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
async fn recommend_includes_boundary_months_and_filters_unassignable_items() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool);

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
    assigned.batch_id = Some(batch.batch.id);
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
        vec![Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)]
    );
    assert!(recommended.iter().all(|item| {
        item.confirmation_status == ConfirmationStatus::Pending && item.batch_id.is_none()
    }));
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
async fn assign_is_idempotent_deduplicates_input_and_warns_only_for_outside_dates() {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let service = BatchService::new(pool.clone());
    let items = ItemRepository::new(pool);
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
        .assign_items(batch.batch.id, &[outside.id, outside.id, no_date.id])
        .await
        .expect("valid items should assign");

    assert_eq!(detail.items.len(), 2);
    assert_eq!(
        detail.warnings,
        vec![format!("outside_date_range:{}", outside.id)]
    );
    let again = service
        .assign_items(batch.batch.id, &[outside.id])
        .await
        .expect("same-batch assignment should be idempotent");
    assert_eq!(again.items.len(), 2);
    let empty = service
        .assign_items(batch.batch.id, &[])
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
    elsewhere.batch_id = Some(other.batch.id);
    for item in [&valid, &duplicate, &failed, &elsewhere] {
        items.insert(item).await.expect("item should insert");
    }

    for invalid_id in [duplicate.id, failed.id, elsewhere.id, Uuid::from_u128(99)] {
        let error = service
            .assign_items(target.batch.id, &[valid.id, invalid_id])
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
        Some(other.batch.id)
    );
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
    elsewhere.batch_id = Some(other.batch.id);
    for item in [&owned, &unassigned, &elsewhere] {
        items.insert(item).await.expect("item should insert");
    }
    service
        .assign_items(owner.batch.id, &[owned.id])
        .await
        .expect("owned item should assign");

    let detail = service
        .remove_item(owner.batch.id, owned.id)
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
        (owner.batch.id, Uuid::from_u128(405), Some("item")),
        (owner.batch.id, unassigned.id, None),
        (owner.batch.id, elsewhere.id, None),
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
        Some(other.batch.id)
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
        ids.push(item.id);
        items
            .insert(&item)
            .await
            .expect("summary item should insert");
    }

    let detail = service
        .assign_items(batch.batch.id, &ids)
        .await
        .expect("summary items should assign");

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
    let items = ItemRepository::new(pool);
    let batch = service
        .create_month(2026, 2)
        .await
        .expect("batch should create");
    let mut maximum = sample_item(66, Some("2026-02"));
    maximum.amount_cents = Some(i64::MAX);
    let mut one = sample_item(67, Some("2026-02"));
    one.amount_cents = Some(1);
    items
        .insert(&maximum)
        .await
        .expect("maximum item should insert");
    items
        .insert(&one)
        .await
        .expect("one-cent item should insert");

    let error = service
        .assign_items(batch.batch.id, &[maximum.id, one.id])
        .await
        .expect_err("overflowing summary should return an error");

    assert_eq!(
        error,
        AppError::Internal {
            message: "batch summary amount overflow".to_owned(),
        }
    );
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
    mark_exported(&pool, batch.batch.id).await;

    let unchanged = service
        .update(
            batch.batch.id,
            NewBatchInput {
                name: batch.batch.name.clone(),
                start_date: batch.batch.start_date.to_string(),
                end_date: batch.batch.end_date.to_string(),
                note: None,
            },
        )
        .await
        .expect("unchanged update should succeed");
    assert_eq!(unchanged.batch.status, BatchStatus::Exported);

    let renamed = service
        .update(
            batch.batch.id,
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

    mark_exported(&pool, batch.batch.id).await;
    let redated = service
        .update(
            batch.batch.id,
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
    mark_exported(&pool, batch.batch.id).await;

    let assigned = service
        .assign_items(batch.batch.id, &[item.id])
        .await
        .expect("assignment should succeed");
    assert_eq!(assigned.batch.status, BatchStatus::Draft);
    assert!(assigned.batch.last_exported_at.is_some());

    mark_exported(&pool, batch.batch.id).await;
    assert_eq!(
        service
            .assign_items(batch.batch.id, &[item.id])
            .await
            .expect("idempotent assignment should succeed")
            .batch
            .status,
        BatchStatus::Exported
    );
    assert_eq!(
        service
            .assign_items(batch.batch.id, &[])
            .await
            .expect("empty assignment should succeed")
            .batch
            .status,
        BatchStatus::Exported
    );

    let removed = service
        .remove_item(batch.batch.id, item.id)
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
        .assign_items(batch.batch.id, &[item.id])
        .await
        .expect("item should assign");
    mark_exported(&pool, batch.batch.id).await;

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
        .get(batch.batch.id)
        .await
        .expect("batch should reload");
    assert_eq!(reviewed_batch.status, BatchStatus::Draft);
    assert!(reviewed_batch.last_exported_at.is_some());

    mark_exported(&pool, batch.batch.id).await;
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
        .get(batch.batch.id)
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
    items.insert(&item).await.expect("item should insert");
    batches
        .assign_items(batch.batch.id, &[item.id])
        .await
        .expect("item should assign");
    mark_exported(&pool, batch.batch.id).await;

    RecognitionService::new(items, Arc::new(SuccessfulExtractor))
        .recognize_item(item.id)
        .await
        .expect("recognition should succeed");

    let persisted = BatchRepository::new(pool)
        .get(batch.batch.id)
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
    mark_exported(&pool, batch.batch.id).await;
    sqlx::query(&format!(
        "CREATE TRIGGER fail_batch_refresh BEFORE UPDATE ON batches \
         WHEN OLD.id = '{}' AND NEW.status = 'draft' \
         BEGIN SELECT RAISE(ABORT, 'forced batch refresh failure'); END",
        batch.batch.id
    ))
    .execute(&pool)
    .await
    .expect("failure trigger should create");

    let error = service
        .assign_items(batch.batch.id, &[first.id, second.id])
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
        .get(batch.batch.id)
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
        .assign_items(batch.batch.id, &[item.id])
        .await
        .expect("item should assign");
    mark_exported(&pool, batch.batch.id).await;
    sqlx::query(&format!(
        "CREATE TRIGGER fail_review_fallback BEFORE UPDATE ON batches \
         WHEN OLD.id = '{}' AND NEW.status = 'draft' \
         BEGIN SELECT RAISE(ABORT, 'forced review fallback failure'); END",
        batch.batch.id
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
    assert_eq!(
        persisted_item.confirmation_status,
        ConfirmationStatus::Pending
    );
    let persisted_batch = BatchRepository::new(pool)
        .get(batch.batch.id)
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
    let barrier = Arc::new(Barrier::new(2));
    let first_service = service.clone();
    let first_barrier = barrier.clone();
    let first_id = first_batch.batch.id;
    let item_id = item.id;
    let first = tokio::spawn(async move {
        first_barrier.wait().await;
        first_service.assign_items(first_id, &[item_id]).await
    });
    let second_service = service.clone();
    let second_id = second_batch.batch.id;
    let second = tokio::spawn(async move {
        barrier.wait().await;
        second_service.assign_items(second_id, &[item_id]).await
    });

    let first_result = first.await.expect("first assignment should join");
    let second_result = second.await.expect("second assignment should join");
    let successful_batch = match (&first_result, &second_result) {
        (Ok(detail), Err(AppError::Conflict { .. })) => detail.batch.id,
        (Err(AppError::Conflict { .. }), Ok(detail)) => detail.batch.id,
        outcomes => panic!("expected one success and one conflict, got {outcomes:?}"),
    };
    let persisted = ItemRepository::new(pool)
        .get_by_id(item.id)
        .await
        .expect("item should reload");
    assert_eq!(persisted.batch_id, Some(successful_batch));
}
