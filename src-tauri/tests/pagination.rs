use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use invoice_reimbursement::commands::batches::BatchCandidateDisabledReason;
use invoice_reimbursement::commands::{CursorDto, PageRequestDto, batches, dashboard, items};
use invoice_reimbursement::db;
use invoice_reimbursement::db::batches::BatchRepository;
use invoice_reimbursement::db::items::{ItemFilter, ItemRepository};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::infra::credentials::MemoryCredentialStore;
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::services::batches::BatchService;
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
async fn batch_candidates_use_exact_dates_and_report_assignment_eligibility() {
    let fixture = Fixture::new().await;
    let batch_id = Uuid::parse_str(&fixture.expected_batch_ids[0]).unwrap();
    for (id, invoice_date, recognition, dedupe) in [
        (1_u128, Some("2026-01-01"), "failed", "unique"),
        (2, Some("2026-12-31"), "succeeded", "suspected_duplicate"),
        (3, Some("2025-12-31"), "succeeded", "unique"),
        (4, None, "succeeded", "unique"),
        (5, Some("2026-07-17"), "succeeded", "unique"),
    ] {
        sqlx::query(
            "UPDATE items SET invoice_date = ?, recognition_status = ?, dedupe_status = ? \
             WHERE id = ?",
        )
        .bind(invoice_date)
        .bind(recognition)
        .bind(dedupe)
        .bind(Uuid::from_u128(id).to_string())
        .execute(fixture.state.pool())
        .await
        .unwrap();
    }

    let page = batches::list_candidates(&fixture.state, batch_id, None, None)
        .await
        .unwrap();

    assert_eq!(
        page.items
            .iter()
            .map(|candidate| candidate.item.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            Uuid::from_u128(5).to_string(),
            Uuid::from_u128(2).to_string(),
            Uuid::from_u128(1).to_string(),
        ]
    );
    assert!(
        page.items
            .iter()
            .all(|candidate| !candidate.outside_batch_range)
    );
    assert!(page.items[0].eligible);
    assert_eq!(page.items[0].disabled_reason, None);
    assert!(!page.items[1].eligible);
    assert_eq!(
        page.items[1].disabled_reason,
        Some(BatchCandidateDisabledReason::SuspectedDuplicate)
    );
    assert!(!page.items[2].eligible);
    assert_eq!(
        page.items[2].disabled_reason,
        Some(BatchCandidateDisabledReason::RecognitionFailed)
    );
}

#[tokio::test]
async fn batch_candidate_search_marks_outside_dates_and_excludes_assigned_items() {
    let fixture = Fixture::new().await;
    let batch_id = Uuid::parse_str(&fixture.expected_batch_ids[0]).unwrap();
    for (id, invoice_date, name, assigned) in [
        (10_u128, Some("2025-12-31"), "差旅检索-范围外.pdf", false),
        (11, None, "差旅检索-无日期.pdf", false),
        (12, Some("2026-07-17"), "差旅检索-范围内.pdf", false),
        (13, Some("2026-07-17"), "差旅检索-已归属.pdf", true),
    ] {
        sqlx::query(
            "UPDATE items SET invoice_date = ?, original_name = ?, batch_id = ? WHERE id = ?",
        )
        .bind(invoice_date)
        .bind(name)
        .bind(assigned.then(|| batch_id.to_string()))
        .bind(Uuid::from_u128(id).to_string())
        .execute(fixture.state.pool())
        .await
        .unwrap();
    }

    let page = batches::list_candidates(
        &fixture.state,
        batch_id,
        Some("  差旅检索  ".to_owned()),
        None,
    )
    .await
    .unwrap();

    assert_eq!(page.items.len(), 3);
    assert_eq!(
        page.items
            .iter()
            .map(|candidate| (candidate.item.id.clone(), candidate.outside_batch_range))
            .collect::<Vec<_>>(),
        vec![
            (Uuid::from_u128(12).to_string(), false),
            (Uuid::from_u128(11).to_string(), true),
            (Uuid::from_u128(10).to_string(), true),
        ]
    );
}

#[tokio::test]
async fn batch_candidate_pages_seek_stably_and_validate_requests() {
    let fixture = Fixture::new().await;
    let batch_id = Uuid::parse_str(&fixture.expected_batch_ids[0]).unwrap();
    let expected = fixture.expected_item_ids[2..].to_vec();
    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let page = batches::list_candidates(
            &fixture.state,
            batch_id,
            Some(".pdf".to_owned()),
            Some(PageRequestDto {
                cursor,
                page_size: Some(37),
            }),
        )
        .await
        .unwrap();
        ids.extend(page.items.into_iter().map(|candidate| candidate.item.id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids, expected);
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), expected.len());

    for page_size in [0, 201] {
        let error = batches::list_candidates(
            &fixture.state,
            batch_id,
            None,
            Some(PageRequestDto {
                cursor: None,
                page_size: Some(page_size),
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, AppError::Validation { ref field, .. } if field == "pageSize"));
    }

    let cursor_error = batches::list_candidates(
        &fixture.state,
        batch_id,
        Some("pdf".to_owned()),
        Some(PageRequestDto {
            cursor: Some(CursorDto {
                sort_value: "not-a-date".to_owned(),
                id: Uuid::new_v4().to_string(),
            }),
            page_size: Some(5),
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(cursor_error, AppError::Validation { ref field, .. } if field == "cursor"));

    let query_error =
        batches::list_candidates(&fixture.state, batch_id, Some("x".repeat(201)), None)
            .await
            .unwrap_err();
    assert!(matches!(query_error, AppError::Validation { ref field, .. } if field == "query"));

    let missing_error = batches::list_candidates(&fixture.state, Uuid::new_v4(), None, None)
        .await
        .unwrap_err();
    assert!(matches!(missing_error, AppError::NotFound { ref entity, .. } if entity == "batch"));
}

#[tokio::test]
async fn page_boundaries_are_validated_below_the_command_layer() {
    let fixture = Fixture::new().await;
    let items = ItemRepository::new(fixture.state.pool().clone());
    let batches = BatchRepository::new(fixture.state.pool().clone());

    for page_size in [0, 201] {
        let item_error = items
            .list_page(ItemFilter::default(), None, page_size)
            .await
            .unwrap_err();
        assert!(
            matches!(item_error, AppError::Validation { ref field, .. } if field == "pageSize")
        );

        let item_service_error = fixture
            .state
            .item_service()
            .list_page(ItemFilter::default(), None, page_size)
            .await
            .unwrap_err();
        assert!(
            matches!(item_service_error, AppError::Validation { ref field, .. } if field == "pageSize")
        );

        let batch_error = batches.list_page(None, page_size).await.unwrap_err();
        assert!(
            matches!(batch_error, AppError::Validation { ref field, .. } if field == "pageSize")
        );

        let batch_service_error = BatchService::new(fixture.state.pool().clone())
            .list_page(None, page_size)
            .await
            .unwrap_err();
        assert!(
            matches!(batch_service_error, AppError::Validation { ref field, .. } if field == "pageSize")
        );
    }
}

#[tokio::test]
async fn invalid_cursors_are_rejected_and_valid_nonexistent_cursors_are_seek_anchors() {
    let fixture = Fixture::new().await;
    let invalid_item = items::list_page(
        &fixture.state,
        Default::default(),
        Some(PageRequestDto {
            cursor: Some(CursorDto {
                sort_value: "not-a-date".to_owned(),
                id: Uuid::new_v4().to_string(),
            }),
            page_size: Some(5),
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(invalid_item, AppError::Validation { ref field, .. } if field == "cursor"));

    let invalid_batch = batches::list_page(
        &fixture.state,
        Some(PageRequestDto {
            cursor: Some(CursorDto {
                sort_value: "9999-12-31T23:59:59Z".to_owned(),
                id: "not-a-uuid".to_owned(),
            }),
            page_size: Some(5),
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(invalid_batch, AppError::Validation { ref field, .. } if field == "cursor"));

    let nonexistent = CursorDto {
        sort_value: "9999-12-31T23:59:59Z".to_owned(),
        id: Uuid::new_v4().to_string(),
    };
    let item_page = items::list_page(
        &fixture.state,
        Default::default(),
        Some(PageRequestDto {
            cursor: Some(nonexistent.clone()),
            page_size: Some(5),
        }),
    )
    .await
    .unwrap();
    assert_eq!(item_page.items.len(), 5);
    let batch_page = batches::list_page(
        &fixture.state,
        Some(PageRequestDto {
            cursor: Some(nonexistent),
            page_size: Some(5),
        }),
    )
    .await
    .unwrap();
    assert_eq!(batch_page.items.len(), 5);
}

#[tokio::test]
async fn corrupt_item_in_the_extra_row_does_not_break_the_current_page() {
    let fixture = Fixture::new().await;
    let corrupt_id = &fixture.expected_item_ids[5];
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(fixture.state.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE items SET source_type = 'corrupt_source' WHERE id = ?")
        .bind(corrupt_id)
        .execute(fixture.state.pool())
        .await
        .unwrap();

    assert!(
        ItemRepository::new(fixture.state.pool().clone())
            .get_by_id(Uuid::parse_str(corrupt_id).unwrap())
            .await
            .is_err()
    );
    let page = items::list_page(
        &fixture.state,
        Default::default(),
        Some(PageRequestDto {
            cursor: None,
            page_size: Some(5),
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        page.items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        fixture.expected_item_ids[..5]
    );
    assert!(page.next_cursor.is_some());
    let containing_page = items::list_page(
        &fixture.state,
        Default::default(),
        Some(PageRequestDto {
            cursor: page.next_cursor,
            page_size: Some(5),
        }),
    )
    .await
    .expect_err("a page that actually includes the corrupt item must still fail");
    assert!(matches!(containing_page, AppError::Internal { .. }));
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
async fn corrupt_batch_in_the_extra_row_does_not_break_page_or_dashboard() {
    let fixture = Fixture::new().await;
    let sixth_batch_id = &fixture.expected_batch_ids[5];
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(fixture.state.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE batches SET status = 'corrupt_status' WHERE id = ?")
        .bind(sixth_batch_id)
        .execute(fixture.state.pool())
        .await
        .unwrap();

    let direct_error = BatchRepository::new(fixture.state.pool().clone())
        .get(Uuid::parse_str(sixth_batch_id).unwrap())
        .await
        .expect_err("the corrupt batch must fail when selected directly");
    assert!(matches!(direct_error, AppError::Internal { .. }));

    let page = batches::list_page(
        &fixture.state,
        Some(PageRequestDto {
            cursor: None,
            page_size: Some(5),
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        page.items
            .iter()
            .map(|batch| batch.id.as_str())
            .collect::<Vec<_>>(),
        fixture.expected_batch_ids[..5]
    );
    assert!(page.next_cursor.is_some());
    let containing_page = batches::list_page(
        &fixture.state,
        Some(PageRequestDto {
            cursor: page.next_cursor,
            page_size: Some(5),
        }),
    )
    .await
    .expect_err("a page that actually includes the corrupt batch must still fail");
    assert!(matches!(containing_page, AppError::Internal { .. }));

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

#[tokio::test]
async fn pagination_queries_use_bounded_candidate_plans_and_composite_indexes() {
    let fixture = Fixture::new().await;
    let indexes = sqlx::query_as::<_, (String,)>(
        "SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name",
    )
    .fetch_all(fixture.state.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| row.0)
    .collect::<HashSet<_>>();
    assert!(indexes.contains("idx_items_created_id"));
    assert!(indexes.contains("idx_batches_updated_id"));

    let item_plan = explain_details(
        fixture.state.pool(),
        "EXPLAIN QUERY PLAN
         SELECT id FROM items
         WHERE (created_at < ? OR (created_at = ? AND id < ?))
         ORDER BY created_at DESC, id DESC LIMIT ?",
        &[
            "9999-12-31T23:59:59Z",
            "9999-12-31T23:59:59Z",
            &Uuid::new_v4().to_string(),
            "51",
        ],
    )
    .await;
    assert!(
        plan_contains(&item_plan, "idx_items_created_id"),
        "{item_plan:?}"
    );

    let batch_plan = explain_details(
        fixture.state.pool(),
        "EXPLAIN QUERY PLAN
         WITH candidate_batches AS MATERIALIZED (
             SELECT id, name, start_date, end_date, status, note, created_at, updated_at,
                    last_exported_at
             FROM batches
             WHERE (updated_at < ? OR (updated_at = ? AND id < ?))
             ORDER BY updated_at DESC, id DESC LIMIT ?
         )
         SELECT b.id, COUNT(i.id)
         FROM candidate_batches b
         LEFT JOIN items i ON i.batch_id = b.id
         GROUP BY b.id
         ORDER BY b.updated_at DESC, b.id DESC",
        &[
            "9999-12-31T23:59:59Z",
            "9999-12-31T23:59:59Z",
            &Uuid::new_v4().to_string(),
            "51",
        ],
    )
    .await;
    assert!(
        plan_contains(&batch_plan, "MATERIALIZE candidate_batches"),
        "{batch_plan:?}"
    );
    assert!(
        plan_contains(&batch_plan, "idx_batches_updated_id"),
        "{batch_plan:?}"
    );
    assert!(
        plan_contains(&batch_plan, "idx_items_batch"),
        "{batch_plan:?}"
    );

    let dashboard_plan = explain_details(
        fixture.state.pool(),
        "EXPLAIN QUERY PLAN
         WITH candidate_batches AS MATERIALIZED (
             SELECT id, name, start_date, end_date, status, note, created_at, updated_at,
                    last_exported_at
             FROM batches
             ORDER BY updated_at DESC, id DESC LIMIT 5
         )
         SELECT b.id, COUNT(i.id)
         FROM candidate_batches b
         LEFT JOIN items i ON i.batch_id = b.id
         GROUP BY b.id
         ORDER BY b.updated_at DESC, b.id DESC",
        &[],
    )
    .await;
    assert!(
        plan_contains(&dashboard_plan, "MATERIALIZE candidate_batches"),
        "{dashboard_plan:?}"
    );
    assert!(
        plan_contains(&dashboard_plan, "idx_batches_updated_id"),
        "{dashboard_plan:?}"
    );
    assert!(
        plan_contains(&dashboard_plan, "idx_items_batch"),
        "{dashboard_plan:?}"
    );
}

async fn explain_details(pool: &sqlx::SqlitePool, sql: &str, bindings: &[&str]) -> Vec<String> {
    let mut query = sqlx::query_as::<_, (i64, i64, i64, String)>(sql);
    for binding in bindings {
        query = query.bind(*binding);
    }
    query
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.3)
        .collect()
}

fn plan_contains(plan: &[String], needle: &str) -> bool {
    plan.iter().any(|detail| detail.contains(needle))
}
