use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use invoice_reimbursement::db;
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::services::preview::{
    PreviewCoordinator, PreviewFileReader, PreviewPayload, PreviewService, PreviewVariant,
};
use uuid::Uuid;

#[derive(Default)]
struct BlockingReader {
    started: AtomicUsize,
    allocations: AtomicUsize,
    releases: Mutex<usize>,
    changed: Condvar,
}

impl BlockingReader {
    fn wait_until_started(&self, expected: usize) {
        let mut releases = self.releases.lock().unwrap();
        while self.started.load(Ordering::SeqCst) < expected {
            releases = self.changed.wait(releases).unwrap();
        }
    }

    fn release(&self, count: usize) {
        let mut releases = self.releases.lock().unwrap();
        *releases += count;
        self.changed.notify_all();
    }
}

impl PreviewFileReader for BlockingReader {
    fn read(
        &self,
        _path: PathBuf,
        _root: PathBuf,
        mime_type: &'static str,
        _max_bytes: u64,
    ) -> Result<PreviewPayload, AppError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_all();

        let mut releases = self.releases.lock().unwrap();
        while *releases == 0 {
            releases = self.changed.wait(releases).unwrap();
        }
        *releases -= 1;
        drop(releases);

        self.allocations.fetch_add(1, Ordering::SeqCst);
        Ok(PreviewPayload {
            bytes: b"preview".to_vec(),
            mime_type,
        })
    }
}

struct PanicOnceReader {
    calls: AtomicUsize,
}

impl PreviewFileReader for PanicOnceReader {
    fn read(
        &self,
        _path: PathBuf,
        _root: PathBuf,
        mime_type: &'static str,
        _max_bytes: u64,
    ) -> Result<PreviewPayload, AppError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("injected preview reader panic");
        }
        Ok(PreviewPayload {
            bytes: b"recovered".to_vec(),
            mime_type,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn third_preview_fails_before_reader_allocation_and_capacity_recovers() {
    let (pool, paths, item_ids) = preview_fixture(3).await;
    let coordinator = PreviewCoordinator::default();
    let reader = Arc::new(BlockingReader::default());
    let service = PreviewService::with_reader(pool, paths, coordinator.clone(), reader.clone());
    let first_id = item_ids[0];
    let second_id = item_ids[1];
    let third_id = item_ids[2];

    let first_service = service.clone();
    let first =
        tokio::spawn(async move { first_service.open(first_id, PreviewVariant::Original).await });
    let second_service = service.clone();
    let second = tokio::spawn(async move {
        second_service
            .open(second_id, PreviewVariant::Original)
            .await
    });
    tokio::task::block_in_place(|| reader.wait_until_started(2));

    let error = service
        .open(third_id, PreviewVariant::Original)
        .await
        .expect_err("the third concurrent preview must fail fast");
    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: true,
            ..
        } if service == "preview_capacity"
    ));
    assert_eq!(reader.started.load(Ordering::SeqCst), 2);
    assert_eq!(reader.allocations.load(Ordering::SeqCst), 0);

    reader.release(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while coordinator.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let third_service = service.clone();
    let third =
        tokio::spawn(async move { third_service.open(third_id, PreviewVariant::Original).await });
    tokio::task::block_in_place(|| reader.wait_until_started(3));
    reader.release(2);

    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    third.await.unwrap().unwrap();
    assert_eq!(coordinator.available_permits(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_preview_keeps_its_permit_until_the_blocking_reader_exits() {
    let (pool, paths, item_ids) = preview_fixture(1).await;
    let coordinator = PreviewCoordinator::with_limit(1);
    let reader = Arc::new(BlockingReader::default());
    let service = PreviewService::with_reader(pool, paths, coordinator.clone(), reader.clone());
    let item_id = item_ids[0];

    let task_service = service.clone();
    let task =
        tokio::spawn(async move { task_service.open(item_id, PreviewVariant::Original).await });
    tokio::task::block_in_place(|| reader.wait_until_started(1));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    let error = service
        .open(item_id, PreviewVariant::Original)
        .await
        .expect_err("the live blocking reader must retain the only permit");
    assert!(matches!(
        error,
        AppError::External {
            ref service,
            ..
        } if service == "preview_capacity"
    ));

    reader.release(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while coordinator.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    reader.release(1);
    service
        .open(item_id, PreviewVariant::Original)
        .await
        .unwrap();
    assert_eq!(coordinator.available_permits(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_preview_reader_releases_the_permit() {
    let (pool, paths, item_ids) = preview_fixture(1).await;
    let coordinator = PreviewCoordinator::with_limit(1);
    let service = PreviewService::with_reader(
        pool,
        paths,
        coordinator.clone(),
        Arc::new(PanicOnceReader {
            calls: AtomicUsize::new(0),
        }),
    );

    let error = service
        .open(item_ids[0], PreviewVariant::Original)
        .await
        .expect_err("reader panic must become an internal preview error");
    assert!(matches!(error, AppError::Internal { .. }));
    assert_eq!(coordinator.available_permits(), 1);

    let recovered = service
        .open(item_ids[0], PreviewVariant::Original)
        .await
        .unwrap();
    assert_eq!(recovered.bytes, b"recovered");
    assert_eq!(coordinator.available_permits(), 1);
}

async fn preview_fixture(count: usize) -> (sqlx::SqlitePool, AppPaths, Vec<Uuid>) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.keep();
    let paths = AppPaths::create(root.join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let mut item_ids = Vec::with_capacity(count);
    for index in 0..count {
        let item_id = Uuid::new_v4();
        insert_preview_item(&pool, item_id, paths.originals.join(format!("{index}.pdf"))).await;
        item_ids.push(item_id);
    }
    (pool, paths, item_ids)
}

async fn insert_preview_item(pool: &sqlx::SqlitePool, id: Uuid, original: PathBuf) {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO items (
            id, original_name, original_path, sha256, mime_type,
            source_type, fetched_at, currency, recognition_status, confirmation_status,
            dedupe_status, created_at, updated_at
         ) VALUES (?, 'invoice.pdf', ?, ?, 'application/pdf', 'manual_upload', ?, 'CNY',
                   'succeeded', 'pending', 'unique', ?, ?)",
    )
    .bind(id.to_string())
    .bind(original.to_string_lossy())
    .bind(format!("preview-sha-{id}"))
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
}
