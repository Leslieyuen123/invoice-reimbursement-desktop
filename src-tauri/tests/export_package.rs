use std::fs::{self, File};
use std::io::{Read, Write};

use chrono::{NaiveDate, TimeZone, Utc};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use invoice_reimbursement::db;
use invoice_reimbursement::db::batches::BatchRepository;
use invoice_reimbursement::db::items::{ItemRepository, NewItemRecord};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{
    BatchStatus, Category, ConfirmationStatus, DedupeStatus, ItemStatus, NewBatch,
    RecognitionStatus, SourceType,
};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::services::export::{ExportCoordinator, ExportService};
use lopdf::{Document, Object, Stream, dictionary};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

struct TestApp {
    _directory: tempfile::TempDir,
    exports: ExportService,
    pool: SqlitePool,
    paths: AppPaths,
    batch_id: Uuid,
    item_ids: Vec<Uuid>,
}

impl TestApp {
    async fn with_batch_item(status: ItemStatus) -> Self {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let paths = AppPaths::create(directory.path().join("storage"))
            .expect("application paths should create");
        let pool = db::connect("sqlite::memory:")
            .await
            .expect("in-memory database should connect");
        let batch = BatchRepository::new(pool.clone())
            .create(
                NewBatch::try_new("2026 年 7 月报销", "2026-07-01", "2026-07-31", None)
                    .expect("batch input should be valid"),
            )
            .await
            .expect("batch should create");
        let insert_status = match status {
            ItemStatus::RecognitionFailed | ItemStatus::SuspectedDuplicate => ItemStatus::Ready,
            status => status,
        };
        let inserted = ItemRepository::new(pool.clone())
            .insert(&sample_item(&paths, batch.id, insert_status))
            .await
            .expect("item should insert");
        match status {
            ItemStatus::RecognitionFailed => {
                sqlx::query("UPDATE items SET recognition_status = 'failed' WHERE id = ?")
                    .bind(inserted.id.to_string())
                    .execute(&pool)
                    .await
                    .expect("fixture state should update");
            }
            ItemStatus::SuspectedDuplicate => {
                sqlx::query("UPDATE items SET dedupe_status = 'suspected_duplicate' WHERE id = ?")
                    .bind(inserted.id.to_string())
                    .execute(&pool)
                    .await
                    .expect("fixture state should update");
            }
            _ => {}
        }

        Self {
            _directory: directory,
            exports: ExportService::new(pool.clone(), paths.clone(), ExportCoordinator::default()),
            pool,
            paths,
            batch_id: batch.id,
            item_ids: vec![inserted.id],
        }
    }

    async fn with_exportable_batch() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let paths = AppPaths::create(directory.path().join("storage"))
            .expect("application paths should create");
        let database_url = format!("sqlite://{}", directory.path().join("app.db").display());
        let pool = db::connect(&database_url)
            .await
            .expect("in-memory database should connect");
        let batch = BatchRepository::new(pool.clone())
            .create(
                NewBatch::try_new("2026 年 7 月报销", "2026-07-01", "2026-07-31", None)
                    .expect("batch input should be valid"),
            )
            .await
            .expect("batch should create");
        let repository = ItemRepository::new(pool.clone());
        let mut item_ids = Vec::new();
        for (sequence, amount_cents) in [(1_u32, 12_300), (2, 32_550)] {
            let mut item = sample_item(&paths, batch.id, ItemStatus::Ready);
            item.id = Uuid::from_u128(u128::from(sequence));
            item.original_name = if sequence == 1 {
                "../../invoice-1.pdf".to_owned()
            } else {
                format!("invoice-{sequence}.pdf")
            };
            item.amount_cents = Some(amount_cents);
            let original_bytes = format!("original-{sequence}");
            item.sha256 = sha256_hex(original_bytes.as_bytes());
            fs::write(&item.original_path, original_bytes).expect("original fixture should write");
            fs::write(
                item.normalized_pdf_path.as_deref().unwrap(),
                inherited_media_box_pdf(100.0 * f64::from(sequence), 150.0 * f64::from(sequence)),
            )
            .expect("normalized PDF fixture should write");
            repository.insert(&item).await.expect("item should insert");
            item_ids.push(item.id);
        }

        Self {
            _directory: directory,
            exports: ExportService::new(pool.clone(), paths.clone(), ExportCoordinator::default()),
            pool,
            paths,
            batch_id: batch.id,
            item_ids,
        }
    }
}

#[tokio::test]
async fn refuses_export_when_batch_contains_unconfirmed_item() {
    let app = TestApp::with_batch_item(ItemStatus::PendingConfirmation).await;
    let error = app.exports.export(app.batch_id).await.unwrap_err();
    assert!(matches!(error, AppError::Conflict { message } if message.contains("1 张票据待确认")));
}

#[tokio::test]
async fn reports_the_number_of_items_still_awaiting_recognition() {
    let app = TestApp::with_batch_item(ItemStatus::PendingRecognition).await;
    let error = app.exports.export(app.batch_id).await.unwrap_err();
    assert!(matches!(error, AppError::Conflict { message } if message.contains("1 张票据待识别")));
}

#[tokio::test]
async fn reports_the_number_of_suspected_duplicates() {
    let app = TestApp::with_batch_item(ItemStatus::SuspectedDuplicate).await;
    let error = app.exports.export(app.batch_id).await.unwrap_err();
    assert!(
        matches!(error, AppError::Conflict { message } if message.contains("1 张票据疑似重复"))
    );
}

#[tokio::test]
async fn exports_pdf_xlsx_originals_and_manifest() {
    let app = TestApp::with_exportable_batch().await;
    let result = app.exports.export(app.batch_id).await.unwrap();
    for name in [
        "merged.pdf",
        "reimbursement.xlsx",
        "originals.zip",
        "manifest.json",
    ] {
        assert!(result.directory.join(name).is_file(), "missing {name}");
    }
    assert!(
        !result
            .directory
            .join(".invoice-export-recovery.json")
            .exists(),
        "successful packages must not expose internal recovery metadata"
    );
    assert_eq!(result.item_count, 2);
    assert_eq!(result.total_amount_cents, 44_850);
    let batch = BatchRepository::new(app.pool)
        .get(app.batch_id)
        .await
        .expect("exported batch should reload");
    assert_eq!(batch.status, BatchStatus::Exported);
    assert!(batch.last_exported_at.is_some());
}

#[tokio::test]
async fn next_export_uses_the_saved_custom_root_without_moving_history() {
    let app = TestApp::with_exportable_batch().await;
    let custom_root = app._directory.path().join("chosen-exports");
    fs::create_dir(&custom_root).unwrap();
    sqlx::query("INSERT INTO settings (key, value_json, updated_at) VALUES ('preferences', ?, ?)")
        .bind(
            serde_json::json!({
                "backgroundSyncEnabled": true,
                "exportDirectory": custom_root.to_string_lossy(),
                "batchDirectoryPattern": "{batchName}-{timestamp}",
            })
            .to_string(),
        )
        .bind(Utc::now().to_rfc3339())
        .execute(&app.pool)
        .await
        .unwrap();

    let historical = app.paths.exports.join("historical-package");
    fs::create_dir(&historical).unwrap();
    let result = app.exports.export(app.batch_id).await.unwrap();

    assert_eq!(
        result.directory.parent(),
        Some(custom_root.canonicalize().unwrap().as_path())
    );
    assert!(result.directory.join("manifest.json").is_file());
    assert!(historical.is_dir(), "historical packages must not move");
}

#[tokio::test]
async fn restart_recovery_uses_the_durable_custom_root() {
    let app = TestApp::with_exportable_batch().await;
    let custom_root = app._directory.path().join("chosen-recovery-root");
    let staging_root = custom_root.join(".invoice-reimbursement-staging");
    fs::create_dir(&custom_root).unwrap();
    fs::create_dir(&staging_root).unwrap();
    sqlx::query("INSERT INTO settings (key, value_json, updated_at) VALUES ('preferences', ?, ?)")
        .bind(
            serde_json::json!({
                "backgroundSyncEnabled": true,
                "exportDirectory": custom_root.to_string_lossy(),
                "batchDirectoryPattern": "{batchName}-{timestamp}",
            })
            .to_string(),
        )
        .bind(Utc::now().to_rfc3339())
        .execute(&app.pool)
        .await
        .unwrap();

    let operation_id = Uuid::new_v4();
    let exported_at = Utc.with_ymd_and_hms(2026, 7, 17, 1, 40, 0).unwrap();
    let staging_component = format!("export-{operation_id}");
    let final_component = "interrupted-package";
    let staging = staging_root.join(&staging_component);
    let final_directory = custom_root.join(final_component);
    for directory in [&staging, &final_directory] {
        fs::create_dir(directory).unwrap();
        fs::write(
            directory.join(".invoice-export-recovery.json"),
            serde_json::json!({
                "operation_id": operation_id.to_string(),
                "batch_id": app.batch_id.to_string(),
                "staging_component": staging_component,
                "final_component": final_component,
                "exported_at": exported_at.to_rfc3339(),
                "state": "generating",
            })
            .to_string(),
        )
        .unwrap();
    }
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO pending_exports (
            operation_id, batch_id, staging_component, final_component, exported_at, state,
            interrupted, last_error, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, 'generating', 1, 'application_shutdown_interrupted', ?, ?)",
    )
    .bind(operation_id.to_string())
    .bind(app.batch_id.to_string())
    .bind(&staging_component)
    .bind(final_component)
    .bind(exported_at.to_rfc3339())
    .bind(&now)
    .bind(&now)
    .execute(&app.pool)
    .await
    .unwrap();

    drop(app.exports);
    let restarted = ExportService::new(
        app.pool.clone(),
        app.paths.clone(),
        ExportCoordinator::default(),
    );
    let report = restarted.reconcile_pending().await.unwrap();

    assert_eq!(report.rolled_back, 1);
    assert!(!staging.exists());
    assert!(!final_directory.exists());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pending_exports")
            .fetch_one(&app.pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn zip_names_use_sequence_id_prefix_and_sanitized_basename() {
    let app = TestApp::with_exportable_batch().await;
    let result = app.exports.export(app.batch_id).await.unwrap();
    let mut archive = zip::ZipArchive::new(
        File::open(result.directory.join("originals.zip")).expect("ZIP should open"),
    )
    .expect("ZIP should parse");
    let mut names = (0..archive.len())
        .map(|index| archive.by_index(index).unwrap().name().to_owned())
        .collect::<Vec<_>>();
    let mut sorted_ids = app.item_ids.clone();
    sorted_ids.sort_unstable();

    assert_eq!(names.len(), 2);
    for (index, id) in sorted_ids.iter().enumerate() {
        let prefix = id.to_string().chars().take(8).collect::<String>();
        assert!(
            names[index].starts_with(&format!("{}-{prefix}-", index + 1)),
            "unexpected archive name: {}",
            names[index]
        );
        assert!(!names[index].contains('/'));
        assert!(!names[index].contains('\\'));
        assert!(!names[index].contains(".."));
    }
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 2, "ZIP entries must never collide");
}

#[tokio::test]
async fn merged_pdf_preserves_inherited_page_dimensions() {
    let app = TestApp::with_exportable_batch().await;
    let result = app.exports.export(app.batch_id).await.unwrap();
    let document =
        Document::load(result.directory.join("merged.pdf")).expect("merged PDF should parse");
    let dimensions = document
        .get_pages()
        .into_values()
        .map(|page_id| {
            let page = document.get_object(page_id).unwrap().as_dict().unwrap();
            let media_box = page.get(b"MediaBox").unwrap().as_array().unwrap();
            (
                media_box[2].as_float().unwrap(),
                media_box[3].as_float().unwrap(),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(dimensions, vec![(100.0, 150.0), (200.0, 300.0)]);
    for page_id in document.get_pages().into_values() {
        let page = document.get_object(page_id).unwrap().as_dict().unwrap();
        let resources_id = page.get(b"Resources").unwrap().as_reference().unwrap();
        let resources = document
            .get_object(resources_id)
            .unwrap()
            .as_dict()
            .unwrap();
        assert!(resources.get(b"ProcSet").is_ok());
        let contents_id = page.get(b"Contents").unwrap().as_reference().unwrap();
        let contents = document
            .get_object(contents_id)
            .unwrap()
            .as_stream()
            .unwrap();
        assert_eq!(contents.content, b"q Q");
    }
}

#[tokio::test]
async fn merged_pdf_renumbers_colliding_indirect_font_and_xobject_resources() {
    let app = TestApp::with_exportable_batch().await;
    let result = app.exports.export(app.batch_id).await.unwrap();
    let document = Document::load(result.directory.join("merged.pdf")).unwrap();

    for (page_id, marker) in document.get_pages().into_values().zip([100, 200]) {
        let page = document.get_object(page_id).unwrap().as_dict().unwrap();
        let resources_id = page.get(b"Resources").unwrap().as_reference().unwrap();
        let resources = document
            .get_object(resources_id)
            .unwrap()
            .as_dict()
            .unwrap();
        let fonts = resources.get(b"Font").unwrap().as_dict().unwrap();
        let font_id = fonts.get(b"F1").unwrap().as_reference().unwrap();
        let font = document.get_object(font_id).unwrap().as_dict().unwrap();
        assert_eq!(
            font.get(b"BaseFont").unwrap().as_name().unwrap(),
            format!("FixtureFont{marker}").as_bytes()
        );

        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        let xobject_id = xobjects.get(b"XO1").unwrap().as_reference().unwrap();
        let xobject = document
            .get_object(xobject_id)
            .unwrap()
            .as_stream()
            .unwrap();
        assert_eq!(
            xobject.content,
            format!("fixture-xobject-{marker}").as_bytes()
        );
    }
}

#[tokio::test]
async fn pdf_and_rows_sort_by_invoice_date_created_at_then_item_id() {
    let mut app = TestApp::with_exportable_batch().await;
    let repository = ItemRepository::new(app.pool.clone());
    for (id, date, created_at, dimension) in [
        (
            Uuid::from_u128(1),
            NaiveDate::from_ymd_opt(2026, 7, 10).unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 13, 2, 0, 0).unwrap(),
            100.0,
        ),
        (
            Uuid::from_u128(2),
            NaiveDate::from_ymd_opt(2026, 7, 10).unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 13, 2, 0, 0).unwrap(),
            200.0,
        ),
    ] {
        sqlx::query("UPDATE items SET invoice_date = ?, created_at = ? WHERE id = ?")
            .bind(date.to_string())
            .bind(created_at.to_rfc3339())
            .bind(id.to_string())
            .execute(&app.pool)
            .await
            .unwrap();
        let path = normalized_path(&app.pool, id).await;
        fs::write(path, inherited_media_box_pdf(dimension, dimension + 50.0)).unwrap();
    }
    for (id_number, date, hour, dimension) in [
        (3_u128, (2026, 7, 10), 3, 300.0),
        (4, (2026, 7, 20), 1, 400.0),
    ] {
        let mut item = sample_item(&app.paths, app.batch_id, ItemStatus::Ready);
        item.id = Uuid::from_u128(id_number);
        item.original_name = format!("invoice-{id_number}.pdf");
        item.invoice_date = Some(NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap());
        item.created_at = Utc.with_ymd_and_hms(2026, 7, 13, hour, 0, 0).unwrap();
        item.updated_at = item.created_at;
        let original_bytes = format!("original-{id_number}");
        item.sha256 = sha256_hex(original_bytes.as_bytes());
        fs::write(&item.original_path, original_bytes).expect("original should write");
        fs::write(
            item.normalized_pdf_path.as_deref().unwrap(),
            inherited_media_box_pdf(dimension, dimension + 50.0),
        )
        .expect("PDF should write");
        repository.insert(&item).await.expect("item should insert");
        app.item_ids.push(item.id);
    }

    let result = app.exports.export(app.batch_id).await.unwrap();
    let document = Document::load(result.directory.join("merged.pdf")).unwrap();
    let widths = document
        .get_pages()
        .into_values()
        .map(|page_id| {
            document
                .get_object(page_id)
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"MediaBox")
                .unwrap()
                .as_array()
                .unwrap()[2]
                .as_float()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(widths, [100.0, 200.0, 300.0, 400.0]);
}

#[tokio::test]
async fn timestamp_collision_never_overwrites_an_existing_directory() {
    let app = TestApp::with_exportable_batch().await;
    let now = Utc::now();
    let mut collision_directories = Vec::new();
    for offset in -60..=60 {
        let timestamp = now + chrono::Duration::seconds(offset);
        let directory = app.paths.exports.join(format!(
            "2026 年 7 月报销-{}",
            timestamp.format("%Y%m%d-%H%M%S")
        ));
        fs::create_dir(&directory).expect("collision fixture should create");
        collision_directories.push(directory);
    }

    let error = app.exports.export(app.batch_id).await.unwrap_err();

    assert!(matches!(error, AppError::Conflict { message } if message.contains("已存在")));
    for directory in collision_directories {
        assert!(directory.is_dir(), "pre-existing directory was removed");
        assert_eq!(
            fs::read_dir(directory).unwrap().count(),
            0,
            "pre-existing directory was overwritten"
        );
    }
    assert_eq!(fs::read_dir(&app.paths.staging).unwrap().count(), 0);
}

#[tokio::test]
async fn cancellation_during_publication_claim_removes_staging_and_keeps_batch_draft() {
    let app = TestApp::with_exportable_batch().await;
    let blocker = app
        .pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("writer blocker should begin");
    let exports = app.exports.clone();
    let batch_id = app.batch_id;
    let task = tokio::spawn(async move { exports.export(batch_id).await });

    let generated = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let staged = fs::read_dir(&app.paths.staging).unwrap().next().is_some();
            if staged {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        generated.is_ok(),
        "package generation should finish before cancellation"
    );
    assert_eq!(fs::read_dir(&app.paths.exports).unwrap().count(), 0);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    blocker
        .rollback()
        .await
        .expect("writer blocker should rollback");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fs::read_dir(&app.paths.staging).unwrap().next().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled blocking generation should release its staging directory");
    assert_eq!(fs::read_dir(&app.paths.exports).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&app.paths.staging).unwrap().count(), 0);
    let batch = BatchRepository::new(app.pool)
        .get(app.batch_id)
        .await
        .expect("batch should remain");
    assert_eq!(batch.status, BatchStatus::Draft);
    assert_eq!(batch.last_exported_at, None);
}

#[tokio::test]
async fn concurrent_export_of_same_batch_is_rejected_before_second_publication() {
    let app = TestApp::with_exportable_batch().await;
    let blocker = app
        .pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("writer blocker should begin");
    let first_service = app.exports.clone();
    let batch_id = app.batch_id;
    let first = tokio::spawn(async move { first_service.export(batch_id).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let staged = fs::read_dir(&app.paths.staging).unwrap().next().is_some();
            let published = fs::read_dir(&app.paths.exports).unwrap().next().is_some();
            if staged || published {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first package should publish");
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;

    let second_service = app.exports.clone();
    let mut second = tokio::spawn(async move { second_service.export(batch_id).await });
    let second_result =
        tokio::time::timeout(std::time::Duration::from_millis(500), &mut second).await;
    assert!(matches!(
        second_result,
        Ok(Ok(Err(AppError::Conflict { message }))) if message.contains("正在导出")
    ));

    blocker
        .rollback()
        .await
        .expect("writer blocker should rollback");
    first
        .await
        .expect("first task should join")
        .expect("first export should succeed");
    assert_eq!(fs::read_dir(&app.paths.exports).unwrap().count(), 1);
}

#[tokio::test]
async fn changed_batch_snapshot_is_rejected_before_publication() {
    let app = TestApp::with_exportable_batch().await;
    let mut writer = app
        .pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("writer should begin");
    sqlx::query("UPDATE items SET amount_cents = 99999, updated_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(app.item_ids[0].to_string())
        .execute(&mut *writer)
        .await
        .expect("concurrent item update should stage");

    let exports = app.exports.clone();
    let batch_id = app.batch_id;
    let task = tokio::spawn(async move { exports.export(batch_id).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let staging_started = fs::read_dir(&app.paths.staging).unwrap().next().is_some();
            let published = fs::read_dir(&app.paths.exports).unwrap().next().is_some();
            if staging_started || published {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("export generation should finish while writer is held");
    writer
        .commit()
        .await
        .expect("concurrent item update should commit");

    let error = task
        .await
        .expect("export task should join")
        .expect_err("stale export should fail");
    assert!(matches!(error, AppError::Conflict { message } if message.contains("内容已更改")));
    assert_eq!(fs::read_dir(&app.paths.exports).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&app.paths.staging).unwrap().count(), 0);
    let batch = BatchRepository::new(app.pool)
        .get(app.batch_id)
        .await
        .expect("batch should remain");
    assert_eq!(batch.status, BatchStatus::Draft);
    assert_eq!(batch.last_exported_at, None);
}

#[tokio::test]
async fn artifacts_contain_exact_workbook_zip_and_manifest_data() {
    let app = TestApp::with_exportable_batch().await;
    let result = app.exports.export(app.batch_id).await.unwrap();

    let shared_strings = read_zip_entry(
        &result.directory.join("reimbursement.xlsx"),
        "xl/sharedStrings.xml",
    );
    let mut header_offset = 0;
    for header in [
        "开票日期",
        "建议归属时间",
        "最终归属批次",
        "分类",
        "金额",
        "城市",
        "公司主体",
        "来源",
        "备注",
        "事项标签",
        "项目标签",
    ] {
        let relative = shared_strings[header_offset..]
            .find(header)
            .unwrap_or_else(|| panic!("missing or out-of-order XLSX header {header}"));
        header_offset += relative + header.len();
    }
    let sheet = read_zip_entry(
        &result.directory.join("reimbursement.xlsx"),
        "xl/worksheets/sheet1.xml",
    );
    assert_numeric_cell(&sheet, "E2", "123");
    assert_numeric_cell(&sheet, "E3", "325.5");
    let styles = read_zip_entry(
        &result.directory.join("reimbursement.xlsx"),
        "xl/styles.xml",
    );
    assert!(styles.contains("¥#,##0.00"));

    let originals_path = result.directory.join("originals.zip");
    let mut originals = zip::ZipArchive::new(File::open(&originals_path).unwrap()).unwrap();
    let mut archived_bytes = Vec::new();
    for index in 0..originals.len() {
        let mut entry = originals.by_index(index).unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        archived_bytes.push(bytes);
    }
    assert_eq!(archived_bytes, [b"original-1", b"original-2"]);

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(result.directory.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["batchId"], app.batch_id.to_string());
    assert_eq!(manifest["appVersion"], env!("CARGO_PKG_VERSION"));
    assert!(manifest["exportedAt"].as_str().is_some());
    assert_eq!(manifest["items"].as_array().unwrap().len(), 2);
    for (item, original_bytes) in manifest["items"]
        .as_array()
        .unwrap()
        .iter()
        .zip([b"original-1".as_slice(), b"original-2".as_slice()])
    {
        assert_eq!(item["sha256"], sha256_hex(original_bytes));
        let file_name = item["fileName"].as_str().unwrap();
        assert!(file_name.contains(item["itemId"].as_str().unwrap().get(..8).unwrap()));
    }
    for name in ["merged.pdf", "reimbursement.xlsx", "originals.zip"] {
        assert_eq!(
            manifest["artifacts"][name],
            sha256_hex(&fs::read(result.directory.join(name)).unwrap())
        );
    }
    assert!(manifest["artifacts"].get("manifest.json").is_none());
}

#[tokio::test]
async fn xlsx_rejects_cents_that_cannot_round_trip_through_numeric_cell() {
    let app = TestApp::with_exportable_batch().await;
    sqlx::query("UPDATE items SET amount_cents = ? WHERE id = ?")
        .bind(9_007_199_254_740_990_i64)
        .bind(app.item_ids[0].to_string())
        .execute(&app.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE items SET amount_cents = 0 WHERE id = ?")
        .bind(app.item_ids[1].to_string())
        .execute(&app.pool)
        .await
        .unwrap();

    let error = app.exports.export(app.batch_id).await.unwrap_err();

    assert!(matches!(
        error,
        AppError::Validation { field, .. } if field == "amountCents"
    ));
    assert_storage_empty(&app.paths);
    let batch = BatchRepository::new(app.pool)
        .get(app.batch_id)
        .await
        .unwrap();
    assert_eq!(batch.status, BatchStatus::Draft);
    assert_eq!(batch.last_exported_at, None);
}

#[tokio::test]
async fn missing_batch_is_reported_without_creating_files() {
    let app = TestApp::with_exportable_batch().await;
    let error = app.exports.export(Uuid::new_v4()).await.unwrap_err();
    assert!(matches!(error, AppError::NotFound { entity, .. } if entity == "batch"));
    assert_storage_empty(&app.paths);
}

#[tokio::test]
async fn empty_batch_is_blocked_without_creating_files() {
    let app = TestApp::with_exportable_batch().await;
    sqlx::query("DELETE FROM items WHERE batch_id = ?")
        .bind(app.batch_id.to_string())
        .execute(&app.pool)
        .await
        .unwrap();
    let error = app.exports.export(app.batch_id).await.unwrap_err();
    assert!(matches!(error, AppError::Conflict { message } if message.contains("没有票据")));
    assert_storage_empty(&app.paths);
}

#[tokio::test]
async fn reports_the_number_of_recognition_failures() {
    let app = TestApp::with_batch_item(ItemStatus::RecognitionFailed).await;
    let error = app.exports.export(app.batch_id).await.unwrap_err();
    assert!(
        matches!(error, AppError::Conflict { message } if message.contains("1 张票据识别失败"))
    );
    assert_storage_empty(&app.paths);
}

#[tokio::test]
async fn missing_original_and_normalized_pdf_are_rejected_before_staging() {
    let missing_original = TestApp::with_exportable_batch().await;
    let original_path = sqlx::query_scalar::<_, String>("SELECT original_path FROM items LIMIT 1")
        .fetch_one(&missing_original.pool)
        .await
        .unwrap();
    fs::remove_file(original_path).unwrap();
    let error = missing_original
        .exports
        .export(missing_original.batch_id)
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Validation { field, .. } if field == "original"));
    assert_storage_empty(&missing_original.paths);

    let missing_normalized = TestApp::with_exportable_batch().await;
    sqlx::query("UPDATE items SET normalized_pdf_path = NULL WHERE id = ?")
        .bind(missing_normalized.item_ids[0].to_string())
        .execute(&missing_normalized.pool)
        .await
        .unwrap();
    let error = missing_normalized
        .exports
        .export(missing_normalized.batch_id)
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Validation { field, .. } if field == "normalizedPdf"));
    assert_storage_empty(&missing_normalized.paths);
}

#[tokio::test]
async fn original_changed_after_import_is_rejected_without_residue() {
    let app = TestApp::with_exportable_batch().await;
    let original_path =
        sqlx::query_scalar::<_, String>("SELECT original_path FROM items WHERE id = ?")
            .bind(app.item_ids[0].to_string())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    fs::write(original_path, b"corrupted after import").unwrap();

    let error = app.exports.export(app.batch_id).await.unwrap_err();

    assert!(matches!(error, AppError::Validation { field, .. } if field == "original"));
    assert_storage_empty(&app.paths);
    let batch = BatchRepository::new(app.pool)
        .get(app.batch_id)
        .await
        .unwrap();
    assert_eq!(batch.status, BatchStatus::Draft);
    assert_eq!(batch.last_exported_at, None);
}

#[tokio::test]
async fn malformed_and_encrypted_pdfs_fail_with_no_staging_residue() {
    let malformed = TestApp::with_exportable_batch().await;
    let malformed_path = normalized_path(&malformed.pool, malformed.item_ids[0]).await;
    fs::write(malformed_path, b"not a PDF").unwrap();
    let error = malformed
        .exports
        .export(malformed.batch_id)
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Validation { field, .. } if field == "normalizedPdf"));
    assert_storage_empty(&malformed.paths);
    assert_eq!(
        BatchRepository::new(malformed.pool)
            .get(malformed.batch_id)
            .await
            .unwrap()
            .status,
        BatchStatus::Draft
    );

    let encrypted = TestApp::with_exportable_batch().await;
    let encrypted_path = normalized_path(&encrypted.pool, encrypted.item_ids[0]).await;
    fs::write(encrypted_path, encrypted_pdf()).unwrap();
    let error = encrypted
        .exports
        .export(encrypted.batch_id)
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Validation { field, .. } if field == "normalizedPdf"));
    assert_storage_empty(&encrypted.paths);
}

#[tokio::test]
async fn escaped_object_stream_is_rejected_before_staging() {
    let app = TestApp::with_exportable_batch().await;
    let normalized = normalized_path(&app.pool, app.item_ids[0]).await;
    fs::write(normalized, classic_pdf_with_escaped_object_stream()).unwrap();

    let error = app.exports.export(app.batch_id).await.unwrap_err();

    assert!(matches!(
        error,
        AppError::Validation { field, message }
            if field == "normalizedPdf" && message.contains("压缩对象")
    ));
    assert_storage_empty(&app.paths);
    let batch = BatchRepository::new(app.pool)
        .get(app.batch_id)
        .await
        .unwrap();
    assert_eq!(batch.status, BatchStatus::Draft);
    assert_eq!(batch.last_exported_at, None);
}

#[tokio::test]
async fn highly_compressed_stream_is_bounded_before_staging() {
    let app = TestApp::with_exportable_batch().await;
    let normalized = normalized_path(&app.pool, app.item_ids[0]).await;
    fs::write(normalized, high_ratio_flate_pdf(100 * 1024 * 1024 + 1)).unwrap();

    let error = app.exports.export(app.batch_id).await.unwrap_err();

    assert!(matches!(
        error,
        AppError::Validation { field, message }
            if field == "normalizedPdf" && message.contains("解码数据")
    ));
    assert_storage_empty(&app.paths);
    let batch = BatchRepository::new(app.pool)
        .get(app.batch_id)
        .await
        .unwrap();
    assert_eq!(batch.status, BatchStatus::Draft);
    assert_eq!(batch.last_exported_at, None);
}

#[tokio::test]
async fn opaque_dct_stream_exports_without_in_process_decode() {
    let app = TestApp::with_exportable_batch().await;
    let normalized = normalized_path(&app.pool, app.item_ids[0]).await;
    fs::write(
        normalized,
        pdf_with_filtered_stream("DCTDecode", b"opaque-jpeg-payload"),
    )
    .unwrap();

    let result = app.exports.export(app.batch_id).await.unwrap();

    assert!(result.directory.join("merged.pdf").is_file());
}

#[tokio::test]
async fn allocative_stream_filters_are_rejected_before_staging() {
    for filter in ["LZWDecode", "ASCII85Decode"] {
        let app = TestApp::with_exportable_batch().await;
        let normalized = normalized_path(&app.pool, app.item_ids[0]).await;
        fs::write(
            normalized,
            pdf_with_filtered_stream(filter, b"encoded-payload"),
        )
        .unwrap();

        let error = app.exports.export(app.batch_id).await.unwrap_err();

        assert!(matches!(
            error,
            AppError::Validation { field, message }
                if field == "normalizedPdf" && message.contains("过滤器")
        ));
        assert_storage_empty(&app.paths);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_and_out_of_root_originals_are_rejected_without_path_disclosure() {
    use std::os::unix::fs::symlink;

    let symlinked = TestApp::with_exportable_batch().await;
    let original_path = sqlx::query_scalar::<_, String>("SELECT original_path FROM items LIMIT 1")
        .fetch_one(&symlinked.pool)
        .await
        .unwrap();
    let external = symlinked._directory.path().join("secret.txt");
    fs::write(&external, b"private contents").unwrap();
    fs::remove_file(&original_path).unwrap();
    symlink(&external, &original_path).unwrap();
    let error = symlinked
        .exports
        .export(symlinked.batch_id)
        .await
        .unwrap_err();
    assert!(matches!(&error, AppError::Validation { field, .. } if field == "original"));
    assert!(
        !error
            .to_string()
            .contains(external.to_string_lossy().as_ref())
    );
    assert!(!error.to_string().contains("private contents"));
    assert_storage_empty(&symlinked.paths);

    let escaped = TestApp::with_exportable_batch().await;
    let external = escaped._directory.path().join("outside.pdf");
    fs::write(&external, b"outside").unwrap();
    sqlx::query("UPDATE items SET original_path = ? WHERE id = ?")
        .bind(external.to_string_lossy().as_ref())
        .bind(escaped.item_ids[0].to_string())
        .execute(&escaped.pool)
        .await
        .unwrap();
    let error = escaped.exports.export(escaped.batch_id).await.unwrap_err();
    assert!(matches!(error, AppError::Validation { field, .. } if field == "original"));
    assert_storage_empty(&escaped.paths);
}

#[tokio::test]
async fn database_status_failure_removes_final_directory_and_keeps_draft() {
    let app = TestApp::with_exportable_batch().await;
    sqlx::query(
        "CREATE TRIGGER reject_export_status \
         BEFORE UPDATE OF status ON batches \
         WHEN NEW.status = 'exported' \
         BEGIN SELECT RAISE(ABORT, 'injected export failure'); END",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    let error = app.exports.export(app.batch_id).await.unwrap_err();
    assert!(matches!(error, AppError::Internal { .. }));
    assert_storage_empty(&app.paths);
    let batch = BatchRepository::new(app.pool)
        .get(app.batch_id)
        .await
        .unwrap();
    assert_eq!(batch.status, BatchStatus::Draft);
    assert_eq!(batch.last_exported_at, None);
}

fn sample_item(paths: &AppPaths, batch_id: Uuid, status: ItemStatus) -> NewItemRecord {
    let id = Uuid::new_v4();
    let now = Utc
        .with_ymd_and_hms(2026, 7, 13, 2, 0, 0)
        .single()
        .expect("test time should be valid");
    let (recognition_status, confirmation_status, dedupe_status) = match status {
        ItemStatus::PendingRecognition => (
            RecognitionStatus::Pending,
            ConfirmationStatus::Pending,
            DedupeStatus::Unique,
        ),
        ItemStatus::PendingConfirmation => (
            RecognitionStatus::Succeeded,
            ConfirmationStatus::Pending,
            DedupeStatus::Unique,
        ),
        ItemStatus::RecognitionFailed => (
            RecognitionStatus::Failed,
            ConfirmationStatus::Pending,
            DedupeStatus::Unique,
        ),
        ItemStatus::SuspectedDuplicate => (
            RecognitionStatus::Succeeded,
            ConfirmationStatus::Confirmed,
            DedupeStatus::SuspectedDuplicate,
        ),
        ItemStatus::Ready => (
            RecognitionStatus::Succeeded,
            ConfirmationStatus::Confirmed,
            DedupeStatus::Unique,
        ),
    };

    NewItemRecord {
        id,
        original_name: "invoice.pdf".to_owned(),
        original_path: paths
            .originals
            .join(format!("{id}.pdf"))
            .to_string_lossy()
            .into_owned(),
        normalized_pdf_path: Some(
            paths
                .normalized
                .join(format!("{id}.pdf"))
                .to_string_lossy()
                .into_owned(),
        ),
        sha256: "fixture-sha256".to_owned(),
        mime_type: "application/pdf".to_owned(),
        source_type: SourceType::ManualUpload,
        source_account_id: None,
        source_mailbox: None,
        source_uid_validity: None,
        source_uid: None,
        source_message_id: None,
        source_part_id: None,
        fetched_at: now,
        invoice_date: Some(
            NaiveDate::from_ymd_opt(2026, 7, 12).expect("fixture date should be valid"),
        ),
        suggested_period: Some("2026-07".to_owned()),
        batch_id: Some(batch_id),
        suggested_category: Some(Category::Transport),
        final_category: Some(Category::Transport),
        amount_cents: Some(12_300),
        currency: "CNY".to_owned(),
        city: Some("上海".to_owned()),
        company: Some("示例公司".to_owned()),
        recognition_status,
        confirmation_status,
        dedupe_status,
        duplicate_of_id: None,
        note: None,
        event_tag: None,
        project_tag: None,
        created_at: now,
        updated_at: now,
    }
}

fn inherited_media_box_pdf(width: f64, height: f64) -> Vec<u8> {
    let mut document = Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let marker = width.round() as i64;
    let font_id = document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => Object::Name(format!("FixtureFont{marker}").into_bytes()),
    });
    let xobject_id = document.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
        },
        format!("fixture-xobject-{marker}").into_bytes(),
    ));
    let resources_id = document.add_object(dictionary! {
        "ProcSet" => vec![Object::Name(b"PDF".to_vec())],
        "Font" => dictionary! {
            "F1" => font_id,
        },
        "XObject" => dictionary! {
            "XO1" => xobject_id,
        },
    });
    let contents_id = document.add_object(Stream::new(dictionary! {}, b"q Q".to_vec()));
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => contents_id,
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
            "MediaBox" => vec![0.into(), 0.into(), width.into(), height.into()],
            "Resources" => resources_id,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn classic_pdf_with_escaped_object_stream() -> Vec<u8> {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".as_slice(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 150] >>".as_slice(),
        b"<< /Type /Obj#53tm /N 0 /First 0 /Length 0 >>\nstream\n\nendstream".as_slice(),
    ];
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        writeln!(&mut bytes, "{} 0 obj", index + 1).unwrap();
        bytes.extend_from_slice(object);
        bytes.extend_from_slice(b"\nendobj\n");
    }
    let xref_offset = bytes.len();
    bytes.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets {
        writeln!(&mut bytes, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        &mut bytes,
        "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
    )
    .unwrap();
    bytes
}

fn high_ratio_flate_pdf(decoded_size: usize) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    let zeros = [0_u8; 64 * 1024];
    let mut remaining = decoded_size;
    while remaining != 0 {
        let chunk = remaining.min(zeros.len());
        encoder.write_all(&zeros[..chunk]).unwrap();
        remaining -= chunk;
    }
    let compressed = encoder.finish().unwrap();

    let mut document = Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let contents_id = document.add_object(Stream::new(
        dictionary! { "Filter" => "FlateDecode" },
        compressed,
    ));
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 100.into(), 150.into()],
        "Contents" => contents_id,
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn pdf_with_filtered_stream(filter: &str, content: &[u8]) -> Vec<u8> {
    let mut document = Document::with_version("1.5");
    let pages_id = document.new_object_id();
    document.add_object(Stream::new(
        dictionary! { "Filter" => Object::Name(filter.as_bytes().to_vec()) },
        content.to_vec(),
    ));
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 100.into(), 150.into()],
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn read_zip_entry(path: &std::path::Path, name: &str) -> String {
    let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
    let mut value = String::new();
    archive
        .by_name(name)
        .unwrap_or_else(|_| panic!("missing ZIP entry {name}"))
        .read_to_string(&mut value)
        .unwrap();
    value
}

fn assert_numeric_cell(sheet: &str, reference: &str, expected: &str) {
    let cell_start = sheet
        .find(&format!("r=\"{reference}\""))
        .unwrap_or_else(|| panic!("missing cell {reference}"));
    let cell = &sheet[cell_start..sheet[cell_start..].find("</c>").unwrap() + cell_start];
    assert!(
        !cell.contains("t=\""),
        "{reference} must be numeric: {cell}"
    );
    assert!(
        cell.contains(&format!("<v>{expected}</v>")),
        "unexpected value for {reference}: {cell}"
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn normalized_path(pool: &SqlitePool, item_id: Uuid) -> String {
    sqlx::query_scalar::<_, String>("SELECT normalized_pdf_path FROM items WHERE id = ?")
        .bind(item_id.to_string())
        .fetch_one(pool)
        .await
        .unwrap()
}

fn encrypted_pdf() -> Vec<u8> {
    let bytes = inherited_media_box_pdf(100.0, 150.0);
    let mut document = Document::load_mem(&bytes).unwrap();
    document.trailer.set(
        "ID",
        vec![
            Object::string_literal("fixture-file-id"),
            Object::string_literal("fixture-file-id"),
        ],
    );
    let state = lopdf::EncryptionState::try_from(lopdf::EncryptionVersion::V2 {
        document: &document,
        owner_password: "owner-password",
        user_password: "user-password",
        key_length: 128,
        permissions: lopdf::Permissions::default(),
    })
    .unwrap();
    document.encrypt(&state).unwrap();
    let mut encrypted = Vec::new();
    document.save_to(&mut encrypted).unwrap();
    encrypted
}

fn assert_storage_empty(paths: &AppPaths) {
    assert_eq!(fs::read_dir(&paths.staging).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&paths.exports).unwrap().count(), 0);
}
