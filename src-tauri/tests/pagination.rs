use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use invoice_reimbursement::commands::{PageRequestDto, batches, dashboard, items};
use invoice_reimbursement::db;
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::infra::credentials::MemoryCredentialStore;
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::state::AppState;
use uuid::Uuid;

struct Fixture {
    _directory: tempfile::TempDir,
    state: AppState,
    expected_item_ids: Vec<String>,
    expected_batch_ids: Vec<String>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let pool = db::connect("sqlite::memory:").await.unwrap();
        let timestamp = Utc::now().to_rfc3339();
        let mut expected_batch_ids = Vec::new();
        for index in 1..=211_u128 {
            let id = Uuid::from_u128(10_000 + index);
            expected_batch_ids.push(id.to_string());
            sqlx::query(
                "INSERT INTO batches (
                    id, name, start_date, end_date, status, note, created_at, updated_at
                 ) VALUES (?, ?, '2026-01-01', '2026-12-31', 'draft', NULL, ?, ?)",
            )
            .bind(id.to_string())
            .bind(format!("Batch {index}"))
            .bind(&timestamp)
            .bind(&timestamp)
            .execute(&pool)
            .await
            .unwrap();
        }
        expected_batch_ids.reverse();

        let aggregate_batch_id = Uuid::parse_str(&expected_batch_ids[0]).unwrap();
        let mut expected_item_ids = Vec::new();
        for index in 1..=251_u128 {
            let id = Uuid::from_u128(index);
            expected_item_ids.push(id.to_string());
            let (batch_id, amount, confirmation) = match index {
                251 => (Some(aggregate_batch_id), Some(300_i64), "pending"),
                250 => (Some(aggregate_batch_id), Some(700_i64), "confirmed"),
                _ => (None, Some(100_i64), "confirmed"),
            };
            sqlx::query(
                "INSERT INTO items (
                    id, original_name, original_path, sha256, mime_type, source_type,
                    fetched_at, batch_id, amount_cents, currency, recognition_status,
                    confirmation_status, dedupe_status, created_at, updated_at
                 ) VALUES (?, ?, ?, ?, 'application/pdf', 'manual_upload', ?, ?, ?, 'CNY',
                           'succeeded', ?, 'unique', ?, ?)",
            )
            .bind(id.to_string())
            .bind(format!("{id}.pdf"))
            .bind(paths.originals.join(format!("{id}.pdf")).to_string_lossy())
            .bind(format!("hash-{id}"))
            .bind(&timestamp)
            .bind(batch_id.map(|id| id.to_string()))
            .bind(amount)
            .bind(confirmation)
            .bind(&timestamp)
            .bind(&timestamp)
            .execute(&pool)
            .await
            .unwrap();
        }
        expected_item_ids.reverse();

        Self {
            state: AppState::new(pool, paths, Arc::new(MemoryCredentialStore::default())),
            _directory: directory,
            expected_item_ids,
            expected_batch_ids,
        }
    }
}

#[tokio::test]
async fn item_pages_are_bounded_and_seek_without_duplicates_or_omissions() {
    let fixture = Fixture::new().await;
    let default_page = items::list_page(&fixture.state, Default::default(), None)
        .await
        .unwrap();
    assert_eq!(default_page.items.len(), 50);
    assert!(default_page.next_cursor.is_some());

    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let page = items::list_page(
            &fixture.state,
            Default::default(),
            Some(PageRequestDto {
                cursor,
                page_size: Some(37),
            }),
        )
        .await
        .unwrap();
        ids.extend(page.items.into_iter().map(|item| item.id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(ids, fixture.expected_item_ids);
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 251);
    let error = items::list_page(
        &fixture.state,
        Default::default(),
        Some(PageRequestDto {
            cursor: None,
            page_size: Some(201),
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, AppError::Validation { ref field, .. } if field == "pageSize"));
}

#[tokio::test]
async fn batch_pages_use_bounded_sql_aggregation_and_dashboard_limits_to_five() {
    let fixture = Fixture::new().await;
    let default_page = batches::list_page(&fixture.state, None).await.unwrap();
    assert_eq!(default_page.items.len(), 50);
    assert_eq!(default_page.items[0].id, fixture.expected_batch_ids[0]);
    assert_eq!(default_page.items[0].item_count, 2);
    assert_eq!(default_page.items[0].total_amount_cents, 1_000);
    assert_eq!(default_page.items[0].unconfirmed_count, 1);

    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let page = batches::list_page(
            &fixture.state,
            Some(PageRequestDto {
                cursor,
                page_size: Some(43),
            }),
        )
        .await
        .unwrap();
        ids.extend(page.items.into_iter().map(|batch| batch.id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids, fixture.expected_batch_ids);
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 211);

    let dashboard = dashboard::load(&fixture.state).await.unwrap();
    assert_eq!(dashboard.recent_batches.len(), 5);
    assert_eq!(
        dashboard.recent_batches[0].id,
        fixture.expected_batch_ids[0]
    );
}

#[tokio::test]
async fn dashboard_does_not_decode_items_from_the_sixth_batch() {
    let fixture = Fixture::new().await;
    let sixth_batch_id = &fixture.expected_batch_ids[5];
    let corrupt_item_id = Uuid::new_v4();
    let timestamp = Utc::now().to_rfc3339();
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(fixture.state.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO items (
            id, original_name, original_path, sha256, mime_type, source_type,
            fetched_at, batch_id, amount_cents, currency, recognition_status,
            confirmation_status, dedupe_status, created_at, updated_at
         ) VALUES (?, 'corrupt.pdf', ?, ?, 'application/pdf', 'corrupt_source', ?, ?, 1,
                   'CNY', 'succeeded', 'confirmed', 'unique', ?, ?)",
    )
    .bind(corrupt_item_id.to_string())
    .bind(
        fixture
            .state
            .paths()
            .originals
            .join("corrupt.pdf")
            .to_string_lossy(),
    )
    .bind(format!("corrupt-{corrupt_item_id}"))
    .bind(&timestamp)
    .bind(sixth_batch_id)
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(fixture.state.pool())
    .await
    .unwrap();

    let detail_error = batches::get(&fixture.state, Uuid::parse_str(sixth_batch_id).unwrap())
        .await
        .expect_err("the sixth batch fixture must fail item decoding when opened directly");
    assert!(matches!(detail_error, AppError::Internal { .. }));

    let dashboard = dashboard::load(&fixture.state).await.unwrap();
    assert_eq!(dashboard.recent_batches.len(), 5);
    assert_eq!(
        dashboard
            .recent_batches
            .iter()
            .map(|batch| batch.id.as_str())
            .collect::<Vec<_>>(),
        fixture.expected_batch_ids[..5]
    );
}
