use std::fs;

use invoice_reimbursement::db;
use invoice_reimbursement::db::items::ItemRepository;
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{
    ConfirmationStatus, DedupeStatus, ItemStatus, SourceType,
};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::services::import::ImportService;

#[tokio::test]
async fn importing_the_same_file_twice_preserves_both_and_links_the_duplicate() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let source = directory.path().join("invoice.pdf");
    let content = b"%PDF-1.7\ninvoice contents\n%%EOF\n";
    fs::write(&source, content).expect("source fixture should write");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let service = ImportService::new(ItemRepository::new(pool), paths.clone());

    let first = service
        .import_manual(&source)
        .await
        .expect("first import should succeed");
    let second = service
        .import_manual(&source)
        .await
        .expect("second import should succeed");

    assert_eq!(first.original_name, "invoice.pdf");
    assert_eq!(first.source_type, SourceType::ManualUpload);
    assert_eq!(first.dedupe_status, DedupeStatus::Unique);
    assert_eq!(first.duplicate_of_id, None);
    assert_eq!(first.status(), ItemStatus::PendingRecognition);
    assert_eq!(second.dedupe_status, DedupeStatus::SuspectedDuplicate);
    assert_eq!(second.duplicate_of_id, Some(first.id));
    assert_eq!(second.status(), ItemStatus::SuspectedDuplicate);
    assert_eq!(second.sha256, first.sha256);
    assert_eq!(
        first.sha256,
        "7fa0ecd2db72674e3eb178994a59dcd52ec5f4518c4123559d4f76197d9e3a2d"
    );
    assert_ne!(second.id, first.id);
    assert_ne!(second.original_path, first.original_path);
    assert_eq!(
        fs::read(&first.original_path).expect("first original should read"),
        content
    );
    assert_eq!(
        fs::read(&second.original_path).expect("second original should read"),
        content
    );
    assert!(
        fs::read_dir(&paths.staging)
            .expect("staging directory should read")
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn invalid_source_paths_and_sizes_leave_no_owned_file_residue() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let missing = directory.path().join("missing.pdf");
    let not_a_file = directory.path().join("directory.pdf");
    fs::create_dir(&not_a_file).expect("directory fixture should create");
    let empty = directory.path().join("empty.pdf");
    fs::write(&empty, []).expect("empty fixture should write");
    let oversized = directory.path().join("oversized.pdf");
    let oversized_file = fs::File::create(&oversized).expect("oversized fixture should create");
    oversized_file
        .set_len(50 * 1024 * 1024 + 1)
        .expect("sparse oversized fixture should resize");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let service = ImportService::new(ItemRepository::new(pool), paths.clone());

    for source in [&missing, &not_a_file, &empty, &oversized] {
        let error = service
            .import_manual(source)
            .await
            .expect_err("invalid source should be rejected");

        assert!(
            matches!(error, AppError::Validation { ref field, .. } if field == "file"),
            "unexpected error for {}: {error:?}",
            source.display()
        );
        assert_directory_empty(&paths.originals);
        assert_directory_empty(&paths.staging);
    }
}

fn assert_directory_empty(path: &std::path::Path) {
    assert!(
        fs::read_dir(path)
            .unwrap_or_else(|error| panic!("{} should read: {error}", path.display()))
            .next()
            .is_none(),
        "{} should be empty",
        path.display()
    );
}

#[tokio::test]
async fn supported_signatures_store_the_mime_detected_from_staged_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let service = ImportService::new(ItemRepository::new(pool), paths);
    let cases: [(&str, &[u8], &str); 8] = [
        ("invoice.PDF", b"%PDF-1.7\n", "application/pdf"),
        ("photo.jpg", &[0xff, 0xd8, 0xff, 0xe0], "image/jpeg"),
        (
            "scan.png",
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
            "image/png",
        ),
        (
            "legacy.doc",
            &[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1],
            "application/x-ole-storage",
        ),
        (
            "legacy.xls",
            &[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1],
            "application/x-ole-storage",
        ),
        ("invoice.docx", b"PK\x03\x04archive", "application/zip"),
        ("ledger.xlsx", b"PK\x03\x04archive", "application/zip"),
        ("bundle.zip", b"PK\x03\x04archive", "application/zip"),
    ];

    for (name, bytes, expected_mime) in cases {
        let source = directory.path().join(name);
        fs::write(&source, bytes).expect("signature fixture should write");

        let imported = service
            .import_manual(&source)
            .await
            .unwrap_or_else(|error| panic!("{name} should import: {error}"));

        assert_eq!(imported.mime_type, expected_mime, "wrong MIME for {name}");
        assert_eq!(imported.confirmation_status, ConfirmationStatus::Pending);
    }
}

#[tokio::test]
async fn unknown_or_mismatched_signature_is_retained_without_forced_classification() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let source = directory.path().join("spoofed.pdf");
    let content = [0x01, 0x23, 0x45, 0x67, 0x89];
    fs::write(&source, content).expect("spoofed fixture should write");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let service = ImportService::new(ItemRepository::new(pool), paths);

    let imported = service
        .import_manual(&source)
        .await
        .expect("allowed extension should be retained");

    assert_eq!(imported.mime_type, "application/octet-stream");
    assert_eq!(imported.confirmation_status, ConfirmationStatus::Pending);
    assert_eq!(imported.suggested_category, None);
    assert_eq!(imported.final_category, None);
    assert!(std::path::Path::new(&imported.original_path).is_file());
}

#[tokio::test]
async fn unsupported_extension_is_rejected_before_staging() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let source = directory.path().join("program.exe");
    fs::write(&source, b"MZ executable").expect("unsupported fixture should write");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let service = ImportService::new(ItemRepository::new(pool), paths.clone());

    let error = service
        .import_manual(&source)
        .await
        .expect_err("unsupported extension should fail");

    assert!(matches!(error, AppError::Validation { ref field, .. } if field == "file"));
    assert_directory_empty(&paths.originals);
    assert_directory_empty(&paths.staging);
}

#[tokio::test]
async fn database_insert_failure_durably_removes_the_promoted_original() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let source = directory.path().join("rejected.pdf");
    fs::write(&source, b"%PDF-1.7\nrejected\n").expect("rejected import fixture should write");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    sqlx::query(
        "CREATE TRIGGER reject_import BEFORE INSERT ON items \
         BEGIN SELECT RAISE(ABORT, 'injected insert failure'); END",
    )
    .execute(&pool)
    .await
    .expect("failure trigger should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let service = ImportService::new(ItemRepository::new(pool.clone()), paths.clone());

    let error = service
        .import_manual(&source)
        .await
        .expect_err("database rejection should fail import");

    assert!(matches!(error, AppError::Internal { .. }));
    let item_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items")
        .fetch_one(&pool)
        .await
        .expect("item count should query");
    assert_eq!(item_count, 0);
    assert_eq!(count_files(&paths.originals), 0);
    assert_directory_empty(&paths.staging);
}

#[tokio::test]
async fn hash_lookup_returns_the_earliest_item_deterministically() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let source = directory.path().join("ordered.pdf");
    fs::write(&source, b"%PDF-1.7\nordered\n").expect("ordered fixture should write");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool.clone());
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let service = ImportService::new(repository.clone(), paths);
    let first = service
        .import_manual(&source)
        .await
        .expect("first ordered item should import");
    let second = service
        .import_manual(&source)
        .await
        .expect("second ordered item should import");
    let earlier = first.created_at - chrono::Duration::seconds(1);
    sqlx::query("UPDATE items SET created_at = ? WHERE id = ?")
        .bind(earlier.to_rfc3339())
        .bind(second.id.to_string())
        .execute(&pool)
        .await
        .expect("fixture timestamp should update");

    let found = repository
        .find_by_hash(&first.sha256)
        .await
        .expect("hash lookup should succeed")
        .expect("matching item should exist");

    assert_eq!(found.id, second.id);
}

#[tokio::test]
async fn concurrent_identical_imports_choose_one_canonical_root() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let source = directory.path().join("concurrent.pdf");
    let content = b"%PDF-1.7\nconcurrent invoice\n";
    fs::write(&source, content).expect("concurrent fixture should write");
    let database_path = directory.path().join("concurrent.sqlite3");
    let database_url = format!("sqlite://{}", database_path.display());
    let pool = db::connect(&database_url)
        .await
        .expect("disk database should connect");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let service = ImportService::new(ItemRepository::new(pool), paths);

    let (first, second) = tokio::join!(
        service.import_manual(&source),
        service.import_manual(&source)
    );
    let first = first.expect("first concurrent import should succeed");
    let second = second.expect("second concurrent import should succeed");
    let items = [&first, &second];
    let roots = items
        .iter()
        .filter(|item| item.dedupe_status == DedupeStatus::Unique)
        .copied()
        .collect::<Vec<_>>();
    let duplicates = items
        .iter()
        .filter(|item| item.dedupe_status == DedupeStatus::SuspectedDuplicate)
        .copied()
        .collect::<Vec<_>>();

    assert_eq!(roots.len(), 1);
    assert_eq!(duplicates.len(), 1);
    assert_eq!(duplicates[0].duplicate_of_id, Some(roots[0].id));
    assert!(std::path::Path::new(&first.original_path).is_file());
    assert!(std::path::Path::new(&second.original_path).is_file());
    assert_eq!(fs::read(&first.original_path).unwrap(), content);
    assert_eq!(fs::read(&second.original_path).unwrap(), content);
}

#[tokio::test]
async fn corrupt_after_insert_readback_rolls_back_row_and_promoted_file() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let source = directory.path().join("corrupt-readback.pdf");
    fs::write(&source, b"%PDF-1.7\ncorrupt readback\n")
        .expect("corrupt readback fixture should write");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    sqlx::query(
        "CREATE TRIGGER corrupt_readback AFTER INSERT ON items \
         BEGIN UPDATE items SET created_at = 'not-a-date' WHERE id = NEW.id; END",
    )
    .execute(&pool)
    .await
    .expect("corrupting trigger should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let service = ImportService::new(ItemRepository::new(pool.clone()), paths.clone());

    let error = service
        .import_manual(&source)
        .await
        .expect_err("typed readback corruption should fail import");

    assert!(matches!(error, AppError::Internal { .. }));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items")
            .fetch_one(&pool)
            .await
            .expect("item count should query"),
        0
    );
    assert_eq!(count_files(&paths.originals), 0);
    assert_directory_empty(&paths.staging);
}

fn count_files(path: &std::path::Path) -> usize {
    fs::read_dir(path)
        .unwrap_or_else(|error| panic!("{} should read: {error}", path.display()))
        .map(|entry| {
            let entry = entry.expect("storage entry should read");
            if entry.path().is_dir() {
                count_files(&entry.path())
            } else {
                1
            }
        })
        .sum()
}
