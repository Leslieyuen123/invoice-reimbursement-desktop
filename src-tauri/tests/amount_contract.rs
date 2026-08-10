use std::path::PathBuf;
use std::sync::Arc;

use chrono::{NaiveDate, Utc};
use invoice_reimbursement::commands::{batches, export, items};
use invoice_reimbursement::db;
use invoice_reimbursement::db::batches::{Batch, BatchSummary};
use invoice_reimbursement::db::items::{ItemPatch, ItemRepository, NewItemRecord};
use invoice_reimbursement::domain::amount::{MAX_SAFE_AMOUNT_CENTS, checked_add_amount_cents};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{
    BatchStatus, Category, ConfirmationStatus, DedupeStatus, RecognitionStatus, SourceType,
};
use invoice_reimbursement::infra::credentials::MemoryCredentialStore;
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::services::batches::{BatchDetailSummary, BatchService, CategorySummary};
use invoice_reimbursement::services::export::ExportResult;
use invoice_reimbursement::state::AppState;
use uuid::Uuid;

#[tokio::test]
async fn maximum_safe_item_amount_round_trips_as_an_exact_json_number() {
    let fixture = AmountFixture::new().await;
    let item_id = fixture.insert_raw_item(MAX_SAFE_AMOUNT_CENTS, None).await;

    let dto = items::get(&fixture.state, item_id).await.unwrap();
    let json = serde_json::to_value(&dto).unwrap();

    assert_eq!(json["amountCents"].as_i64(), Some(MAX_SAFE_AMOUNT_CENTS));
    let decoded: serde_json::Value = serde_json::from_str(&json.to_string()).unwrap();
    assert_eq!(decoded["amountCents"].as_i64(), Some(MAX_SAFE_AMOUNT_CENTS));
}

#[tokio::test]
async fn review_command_rejects_an_amount_above_the_js_safe_limit() {
    let fixture = AmountFixture::new().await;

    let error = items::review(
        &fixture.state,
        items::ReviewItemInputDto {
            id: Uuid::new_v4(),
            invoice_date: Some("2026-07-16".to_owned()),
            suggested_period: "2026-07".to_owned(),
            final_category: Category::Dining,
            amount_cents: MAX_SAFE_AMOUNT_CENTS + 1,
            city: None,
            company: None,
            note: None,
            event_tag: None,
            project_tag: None,
        },
    )
    .await
    .unwrap_err();

    assert_validation_field(error, "amountCents");
}

#[tokio::test]
async fn raw_database_item_above_the_js_safe_limit_is_rejected_at_the_dto_boundary() {
    let fixture = AmountFixture::new().await;
    let item_id = fixture
        .insert_raw_item(MAX_SAFE_AMOUNT_CENTS + 1, None)
        .await;

    let error = items::get(&fixture.state, item_id).await.unwrap_err();

    assert_validation_field(error, "amountCents");
}

#[test]
fn batch_and_category_dtos_reject_amounts_above_the_js_safe_limit() {
    let summary_error =
        batches::BatchDto::try_from(sample_batch_summary(MAX_SAFE_AMOUNT_CENTS + 1)).unwrap_err();
    assert_validation_field(summary_error, "totalAmountCents");

    let detail_error = batches::BatchDetailSummaryDto::try_from(BatchDetailSummary {
        total_amount_cents: MAX_SAFE_AMOUNT_CENTS,
        dining: CategorySummary {
            item_count: 1,
            amount_cents: MAX_SAFE_AMOUNT_CENTS + 1,
        },
        ..BatchDetailSummary::default()
    })
    .unwrap_err();
    assert_validation_field(detail_error, "amountCents");
}

#[test]
fn export_result_dto_rejects_an_amount_above_the_js_safe_limit() {
    let error = export::ExportResultDto::try_from(ExportResult {
        directory: PathBuf::from("exports/unsafe"),
        item_count: 1,
        total_amount_cents: MAX_SAFE_AMOUNT_CENTS + 1,
    })
    .unwrap_err();

    assert_validation_field(error, "totalAmountCents");
}

#[test]
fn amount_addition_overflow_uses_the_stable_total_field() {
    let error = checked_add_amount_cents(i64::MAX, 1, "totalAmountCents").unwrap_err();

    assert_validation_field(error, "totalAmountCents");
}

#[tokio::test]
async fn repository_accepts_the_safe_max_and_rejects_larger_insert_and_update_values() {
    let fixture = AmountFixture::new().await;
    let repository = ItemRepository::new(fixture.pool.clone());
    let accepted = repository
        .insert(&fixture.sample_record(MAX_SAFE_AMOUNT_CENTS))
        .await
        .unwrap();
    assert_eq!(accepted.amount_cents, Some(MAX_SAFE_AMOUNT_CENTS));

    let insert_error = repository
        .insert(&fixture.sample_record(MAX_SAFE_AMOUNT_CENTS + 1))
        .await
        .unwrap_err();
    assert_validation_field(insert_error, "amount_cents");

    let update_error = repository
        .update_fields(
            accepted.id,
            ItemPatch {
                amount_cents: Some(Some(MAX_SAFE_AMOUNT_CENTS + 1)),
                ..ItemPatch::default()
            },
        )
        .await
        .unwrap_err();
    assert_validation_field(update_error, "amount_cents");
}

#[tokio::test]
async fn batch_detail_aggregation_rejects_safe_range_and_i64_overflow() {
    for amounts in [[MAX_SAFE_AMOUNT_CENTS, 1], [i64::MAX, 1]] {
        let fixture = AmountFixture::new().await;
        let batch_id = fixture.insert_batch().await;
        for amount in amounts {
            fixture.insert_raw_item(amount, Some(batch_id)).await;
        }

        let error = BatchService::new(fixture.pool.clone())
            .get(batch_id)
            .await
            .unwrap_err();
        assert_validation_field(error, "totalAmountCents");
    }
}

#[tokio::test]
async fn batch_sql_aggregation_maps_safe_range_and_sqlite_overflow_to_validation() {
    for amounts in [[MAX_SAFE_AMOUNT_CENTS, 1], [i64::MAX, 1]] {
        let fixture = AmountFixture::new().await;
        let batch_id = fixture.insert_batch().await;
        for amount in amounts {
            fixture.insert_raw_item(amount, Some(batch_id)).await;
        }

        let error = BatchService::new(fixture.pool.clone())
            .list_page(None, 5)
            .await
            .unwrap_err();
        assert_validation_field(error, "totalAmountCents");
    }
}

#[tokio::test]
async fn dashboard_recent_batch_aggregation_rejects_sqlite_integer_overflow_stably() {
    let fixture = AmountFixture::new().await;
    let batch_id = fixture.insert_batch().await;
    fixture.insert_raw_item(i64::MAX, Some(batch_id)).await;
    fixture.insert_raw_item(1, Some(batch_id)).await;

    let error = invoice_reimbursement::commands::dashboard::load(&fixture.state)
        .await
        .unwrap_err();

    assert_validation_field(error, "totalAmountCents");
}

#[tokio::test]
async fn export_service_rejects_safe_range_and_i64_overflow_before_file_generation() {
    for (amounts, expected_field) in [
        ([MAX_SAFE_AMOUNT_CENTS, 1], "totalAmountCents"),
        ([i64::MAX, 1], "amountCents"),
    ] {
        let fixture = AmountFixture::new().await;
        let batch_id = fixture.insert_batch().await;
        for amount in amounts {
            fixture.insert_raw_item(amount, Some(batch_id)).await;
        }

        let error = fixture
            .state
            .export_service()
            .export(batch_id)
            .await
            .unwrap_err();
        assert_validation_field(error, expected_field);
        assert_eq!(
            std::fs::read_dir(&fixture.state.paths().staging)
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            std::fs::read_dir(&fixture.state.paths().exports)
                .unwrap()
                .count(),
            0
        );
    }
}

fn sample_batch_summary(total_amount_cents: i64) -> BatchSummary {
    let now = Utc::now();
    BatchSummary {
        id: Uuid::new_v4(),
        name: "JS safe contract".to_owned(),
        start_date: NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
        end_date: NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
        status: BatchStatus::Draft,
        note: None,
        created_at: now,
        updated_at: now,
        last_exported_at: None,
        item_count: 1,
        total_amount_cents,
        unconfirmed_count: 0,
    }
}

fn assert_validation_field(error: AppError, expected: &str) {
    assert!(
        matches!(error, AppError::Validation { ref field, .. } if field == expected),
        "expected validation field {expected}, got {error:?}"
    );
}

struct AmountFixture {
    state: AppState,
    pool: sqlx::SqlitePool,
    _root: PathBuf,
}

impl AmountFixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.keep();
        let paths = AppPaths::create(root.join("storage")).unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let state = AppState::new(
            pool.clone(),
            paths,
            Arc::new(MemoryCredentialStore::default()),
        );
        Self {
            state,
            pool,
            _root: root,
        }
    }

    async fn insert_raw_item(&self, amount_cents: i64, batch_id: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO items (
                id, original_name, original_path, normalized_pdf_path, sha256, mime_type,
                source_type, fetched_at, suggested_period, batch_id, final_category, amount_cents, currency,
                recognition_status, confirmation_status, dedupe_status, created_at, updated_at
             ) VALUES (?, 'invoice.pdf', ?, ?, ?, 'application/pdf', 'manual_upload', ?, '2026-07', ?,
                       'dining', ?, 'CNY', 'succeeded', 'confirmed', 'unique', ?, ?)",
        )
        .bind(id.to_string())
        .bind(
            self.state
                .paths()
                .originals
                .join(format!("{id}.pdf"))
                .to_string_lossy(),
        )
        .bind(
            self.state
                .paths()
                .normalized
                .join(format!("{id}.pdf"))
                .to_string_lossy(),
        )
        .bind(format!("raw-amount-{id}"))
        .bind(&now)
        .bind(batch_id.map(|id| id.to_string()))
        .bind(amount_cents)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    async fn insert_batch(&self) -> Uuid {
        let batch_id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO batches (
                id, name, start_date, end_date, status, created_at, updated_at
             ) VALUES (?, 'JS safe contract', '2026-07-01', '2026-07-31', 'draft', ?, ?)",
        )
        .bind(batch_id.to_string())
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .unwrap();
        batch_id
    }

    fn sample_record(&self, amount_cents: i64) -> NewItemRecord {
        let id = Uuid::new_v4();
        let now = Utc::now();
        NewItemRecord {
            id,
            original_name: "invoice.pdf".to_owned(),
            original_path: self
                .state
                .paths()
                .originals
                .join(format!("{id}.pdf"))
                .to_string_lossy()
                .into_owned(),
            normalized_pdf_path: None,
            sha256: format!("repository-amount-{id}"),
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
            invoice_date: Some(NaiveDate::from_ymd_opt(2026, 7, 16).unwrap()),
            suggested_period: Some("2026-07".to_owned()),
            batch_id: None,
            suggested_category: Some(Category::Dining),
            final_category: Some(Category::Dining),
            amount_cents: Some(amount_cents),
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
}

#[allow(dead_code)]
fn sample_batch() -> Batch {
    let now = Utc::now();
    Batch {
        id: Uuid::new_v4(),
        name: "JS safe contract".to_owned(),
        start_date: NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
        end_date: NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
        status: BatchStatus::Draft,
        note: None,
        created_at: now,
        updated_at: now,
        last_exported_at: None,
    }
}
