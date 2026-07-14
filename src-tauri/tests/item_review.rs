use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::items::{ItemPatch, ItemRepository, NewItemRecord};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, ItemStatus, RecognitionStatus, SourceType,
};
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::services::items::{
    FileLifecycle, IsolatedItemFiles, ItemFiles, ItemReview, ItemService, StorageFileLifecycle,
};
use invoice_reimbursement::services::recognition::RecognitionService;
use uuid::Uuid;

#[tokio::test]
async fn review_confirms_a_complete_item_and_preserves_provenance() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "core-review");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let service = ItemService::new(repository, paths);

    let reviewed = service
        .review(ItemReview {
            id: record.id,
            invoice_date: Some("2026-07-11".to_owned()),
            suggested_period: "2026-07".to_owned(),
            final_category: Category::Hospitality,
            amount_cents: 45_600,
            city: Some(" Beijing ".to_owned()),
            company: Some(" Example Ltd ".to_owned()),
            note: Some(" client dinner ".to_owned()),
            event_tag: Some(" summit ".to_owned()),
            project_tag: Some(" P-2026 ".to_owned()),
        })
        .await
        .expect("complete manual review should succeed");

    assert_eq!(reviewed.invoice_date.unwrap().to_string(), "2026-07-11");
    assert_eq!(reviewed.suggested_period.as_deref(), Some("2026-07"));
    assert_eq!(reviewed.final_category, Some(Category::Hospitality));
    assert_eq!(reviewed.amount_cents, Some(45_600));
    assert_eq!(reviewed.city.as_deref(), Some("Beijing"));
    assert_eq!(reviewed.company.as_deref(), Some("Example Ltd"));
    assert_eq!(reviewed.note.as_deref(), Some("client dinner"));
    assert_eq!(reviewed.event_tag.as_deref(), Some("summit"));
    assert_eq!(reviewed.project_tag.as_deref(), Some("P-2026"));
    assert_eq!(reviewed.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(reviewed.confirmation_status, ConfirmationStatus::Confirmed);
    assert_eq!(reviewed.status(), ItemStatus::Ready);

    assert_eq!(reviewed.original_name, record.original_name);
    assert_eq!(reviewed.original_path, record.original_path);
    assert_eq!(reviewed.normalized_pdf_path, record.normalized_pdf_path);
    assert_eq!(reviewed.sha256, record.sha256);
    assert_eq!(reviewed.source_type, record.source_type);
    assert_eq!(reviewed.batch_id, record.batch_id);
    assert_eq!(reviewed.suggested_category, record.suggested_category);
    assert_eq!(reviewed.dedupe_status, record.dedupe_status);
    assert_eq!(reviewed.duplicate_of_id, record.duplicate_of_id);
    assert_eq!(reviewed.created_at, record.created_at);
}

#[tokio::test]
async fn completed_review_is_not_overwritten_by_inflight_recognition() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "review-recognition-race");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let (started_tx, started_rx) = sync_channel(1);
    let (proceed_tx, proceed_rx) = sync_channel(1);
    let extractor = Arc::new(ReviewGatedExtractor {
        started: started_tx,
        proceed: Mutex::new(proceed_rx),
    });
    let recognition = RecognitionService::new(repository.clone(), extractor);
    let item_id = record.id;
    let recognition_task = tokio::spawn(async move { recognition.recognize_item(item_id).await });
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .expect("extractor start should be observed");
    let review_service = ItemService::new(repository.clone(), paths);

    let reviewed = review_service
        .review(valid_review(record.id))
        .await
        .expect("manual review should succeed");
    proceed_tx.send(()).expect("recognition should resume");
    let recognition_result = recognition_task
        .await
        .expect("recognition task should join")
        .expect("late recognition should return the reviewed item");

    assert_eq!(recognition_result, reviewed);
    assert_eq!(reviewed.status(), ItemStatus::Ready);
    assert_eq!(reviewed.invoice_date.unwrap().to_string(), "2026-07-11");
    assert_eq!(reviewed.suggested_period.as_deref(), Some("2026-07"));
    assert_eq!(reviewed.final_category, Some(Category::Hospitality));
    assert_eq!(reviewed.amount_cents, Some(45_600));
    assert_eq!(reviewed.city.as_deref(), Some("Beijing"));
    assert_eq!(reviewed.company.as_deref(), Some("Example Ltd"));
}

#[tokio::test]
async fn completed_review_is_not_overwritten_by_inflight_recognition_failure() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "review-recognition-failure-race");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let (started_tx, started_rx) = sync_channel(1);
    let (proceed_tx, proceed_rx) = sync_channel(1);
    let extractor = Arc::new(ReviewFailingGatedExtractor {
        started: started_tx,
        proceed: Mutex::new(proceed_rx),
    });
    let recognition = RecognitionService::new(repository.clone(), extractor);
    let item_id = record.id;
    let recognition_task = tokio::spawn(async move { recognition.recognize_item(item_id).await });
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .expect("extractor start should be observed");
    let review_service = ItemService::new(repository.clone(), paths);

    let reviewed = review_service
        .review(valid_review(record.id))
        .await
        .expect("manual review should succeed");
    proceed_tx.send(()).expect("recognition should resume");
    let error = recognition_task
        .await
        .expect("recognition task should join")
        .expect_err("extraction failure should still be returned");

    assert!(matches!(
        error,
        AppError::External { ref service, .. } if service == "document_extraction"
    ));
    assert_eq!(
        repository
            .get_by_id(record.id)
            .await
            .expect("reviewed item should remain"),
        reviewed
    );
}

#[tokio::test]
async fn completed_review_is_not_overwritten_by_recognition_started_later() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "review-before-recognition");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let review_service = ItemService::new(repository.clone(), paths);
    let reviewed = review_service
        .review(valid_review(record.id))
        .await
        .expect("manual review should succeed");
    let recognition = RecognitionService::new(repository, Arc::new(ReviewImmediateExtractor));

    let recognized = recognition
        .recognize_item(record.id)
        .await
        .expect("background recognition should preserve reviewed item");

    assert_eq!(recognized, reviewed);
}

#[tokio::test]
async fn completed_review_is_not_overwritten_by_recognition_failure_started_later() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "review-before-recognition-failure");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let review_service = ItemService::new(repository.clone(), paths);
    let reviewed = review_service
        .review(valid_review(record.id))
        .await
        .expect("manual review should succeed");
    let recognition = RecognitionService::new(
        repository.clone(),
        Arc::new(ReviewImmediateFailingExtractor),
    );

    let error = recognition
        .recognize_item(record.id)
        .await
        .expect_err("extraction failure should still be returned");

    assert!(matches!(
        error,
        AppError::External { ref service, .. } if service == "document_extraction"
    ));
    assert_eq!(
        repository
            .get_by_id(record.id)
            .await
            .expect("reviewed item should remain"),
        reviewed
    );
}

#[tokio::test]
async fn explicit_retry_can_refresh_automatic_fields_after_review() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "review-explicit-retry");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let review_service = ItemService::new(repository.clone(), paths);
    review_service
        .review(valid_review(record.id))
        .await
        .expect("manual review should succeed");
    let recognition = RecognitionService::new(repository, Arc::new(ReviewImmediateExtractor));

    let retried = recognition
        .retry(record.id)
        .await
        .expect("explicit retry should force automatic refresh");

    assert_eq!(retried.invoice_date.unwrap().to_string(), "2025-01-02");
    assert_eq!(retried.suggested_period.as_deref(), Some("2025-01"));
    assert_eq!(retried.amount_cents, Some(100));
    assert_eq!(retried.city, None);
    assert_eq!(retried.company.as_deref(), Some("Late Recognition Co"));
    assert_eq!(retried.final_category, Some(Category::Hospitality));
    assert_eq!(retried.status(), ItemStatus::Ready);
}

#[tokio::test]
async fn first_service_use_restores_crash_tokens_still_referenced_by_a_row() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "recovery-restore");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let original = PathBuf::from(&record.original_path);
    let normalized = PathBuf::from(record.normalized_pdf_path.as_deref().unwrap());
    let original_token = crash_token_path(&original);
    let normalized_token = crash_token_path(&normalized);
    fs::rename(&original, &original_token).expect("original should isolate");
    fs::rename(&normalized, &normalized_token).expect("normalized should isolate");
    let service = ItemService::new(repository, paths);

    service
        .review(valid_review(record.id))
        .await
        .expect("first use should recover before review");

    assert_eq!(fs::read(&original).unwrap(), b"original");
    assert_eq!(fs::read(&normalized).unwrap(), b"normalized");
    assert!(!original_token.exists());
    assert!(!normalized_token.exists());
}

#[tokio::test]
async fn first_service_use_recovers_a_partial_two_file_isolation() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "recovery-partial");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let original = PathBuf::from(&record.original_path);
    let normalized = PathBuf::from(record.normalized_pdf_path.as_deref().unwrap());
    let original_token = crash_token_path(&original);
    fs::rename(&original, &original_token).expect("original should isolate");
    let service = ItemService::new(repository, paths);

    service
        .review(valid_review(record.id))
        .await
        .expect("partial isolation should recover before review");

    assert_eq!(fs::read(&original).unwrap(), b"original");
    assert_eq!(fs::read(&normalized).unwrap(), b"normalized");
    assert!(!original_token.exists());
}

#[tokio::test]
async fn first_service_use_purges_crash_tokens_without_database_references() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "recovery-purge-trigger");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let orphan_target = paths.originals.join("orphan.pdf");
    fs::write(&orphan_target, b"orphan").expect("orphan should write");
    let orphan_token = crash_token_path(&orphan_target);
    fs::rename(&orphan_target, &orphan_token).expect("orphan should isolate");
    let service = ItemService::new(repository, paths);

    service
        .review(valid_review(record.id))
        .await
        .expect("first use should purge before review");

    assert!(!orphan_target.exists());
    assert!(!orphan_token.exists());
}

#[tokio::test]
async fn recovery_collision_does_not_clobber_the_recreated_target() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "recovery-collision");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let target = PathBuf::from(&record.original_path);
    let token = crash_token_path(&target);
    fs::write(&token, b"isolated original").expect("token should write");
    fs::write(&target, b"replacement").expect("replacement should write");
    let before = repository
        .get_by_id(record.id)
        .await
        .expect("item snapshot should load");
    let service = ItemService::new(repository.clone(), paths);

    let error = service
        .review(valid_review(record.id))
        .await
        .expect_err("collision should block service use");

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: false,
            ..
        } if service == "filesystem_sync"
    ));
    assert_eq!(fs::read(&target).unwrap(), b"replacement");
    assert_eq!(fs::read(&token).unwrap(), b"isolated original");
    assert_eq!(repository.get_by_id(record.id).await.unwrap(), before);
}

#[tokio::test]
async fn recovery_leaves_malformed_tokens_untouched() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "recovery-malformed");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let malformed = paths.originals.join(".invoice.pdf.delete-not-a-uuid");
    fs::write(&malformed, b"unknown").expect("malformed token should write");
    let service = ItemService::new(repository, paths);

    service
        .review(valid_review(record.id))
        .await
        .expect("malformed token should not block review");

    assert_eq!(fs::read(&malformed).unwrap(), b"unknown");
}

#[tokio::test]
async fn review_persists_long_optional_text_after_trimming() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "long-optional-text");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let service = ItemService::new(repository, paths);
    let long_note = "private".repeat(200);
    let long_event_tag = "tag".repeat(100);

    let reviewed = service
        .review(ItemReview {
            note: Some(format!("  {long_note}  ")),
            event_tag: Some(format!("\n{long_event_tag}\t")),
            ..valid_review(record.id)
        })
        .await
        .expect("long optional text should be accepted");

    assert!(long_note.chars().count() > 1_000);
    assert!(long_event_tag.chars().count() > 200);
    assert_eq!(reviewed.note.as_deref(), Some(long_note.as_str()));
    assert_eq!(reviewed.event_tag.as_deref(), Some(long_event_tag.as_str()));
}

#[tokio::test]
async fn keeping_a_duplicate_clears_its_link_and_derives_each_workflow_status() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "keep-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let service = ItemService::new(repository.clone(), paths.clone());
    let cases = [
        (
            RecognitionStatus::Succeeded,
            ConfirmationStatus::Confirmed,
            ItemStatus::Ready,
        ),
        (
            RecognitionStatus::Succeeded,
            ConfirmationStatus::Pending,
            ItemStatus::PendingConfirmation,
        ),
        (
            RecognitionStatus::Failed,
            ConfirmationStatus::Pending,
            ItemStatus::RecognitionFailed,
        ),
    ];

    for (index, (recognition, confirmation, expected_status)) in cases.into_iter().enumerate() {
        let mut duplicate = sample_item(&paths, &format!("keep-duplicate-{index}"));
        duplicate.recognition_status = recognition;
        duplicate.confirmation_status = confirmation;
        duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
        duplicate.duplicate_of_id = Some(canonical.id);
        repository
            .insert(&duplicate)
            .await
            .expect("duplicate should insert");

        let kept = service
            .resolve_duplicate(duplicate.id, true)
            .await
            .expect("keeping duplicate should succeed")
            .expect("keeping duplicate should return the retained item");

        assert_eq!(kept.dedupe_status, DedupeStatus::Resolved);
        assert_eq!(kept.duplicate_of_id, None);
        assert_eq!(kept.recognition_status, recognition);
        assert_eq!(kept.confirmation_status, confirmation);
        assert_eq!(kept.status(), expected_status);
    }
}

#[tokio::test]
async fn discarding_a_duplicate_removes_only_its_row_and_owned_files() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "discard-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "discard-duplicate");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    let service = ItemService::new(repository.clone(), paths);

    let result = service
        .resolve_duplicate(duplicate.id, false)
        .await
        .expect("discarding duplicate should succeed");

    assert_eq!(result, None);
    assert!(matches!(
        repository.get_by_id(duplicate.id).await,
        Err(AppError::NotFound { .. })
    ));
    assert!(repository.get_by_id(canonical.id).await.is_ok());
    assert!(!std::path::Path::new(&duplicate.original_path).exists());
    assert!(!std::path::Path::new(duplicate.normalized_pdf_path.as_deref().unwrap()).exists());
    assert!(std::path::Path::new(&canonical.original_path).is_file());
    assert!(std::path::Path::new(canonical.normalized_pdf_path.as_deref().unwrap()).is_file());
}

#[tokio::test]
async fn purge_failure_after_row_deletion_is_reported_as_filesystem_sync() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "purge-failure-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "purge-failure-duplicate");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    let service = ItemService::with_file_lifecycle(repository.clone(), Arc::new(PurgeFails));

    let error = service
        .resolve_duplicate(duplicate.id, false)
        .await
        .expect_err("purge failure should be reported");

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: false,
            ..
        } if service == "filesystem_sync"
    ));
    assert!(matches!(
        repository.get_by_id(duplicate.id).await,
        Err(AppError::NotFound { .. })
    ));
}

#[tokio::test]
async fn isolation_failure_is_filesystem_sync_and_keeps_the_row() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "isolation-failure-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "isolation-failure-duplicate");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    let service = ItemService::with_file_lifecycle(repository.clone(), Arc::new(IsolationFails));

    let error = service
        .resolve_duplicate(duplicate.id, false)
        .await
        .expect_err("isolation failure should be reported");

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: false,
            ..
        } if service == "filesystem_sync"
    ));
    assert!(repository.get_by_id(duplicate.id).await.is_ok());
}

#[cfg(unix)]
#[tokio::test]
async fn real_isolation_permission_failure_keeps_the_row_and_files() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "real-isolation-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "real-isolation-duplicate");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    fs::set_permissions(&paths.originals, fs::Permissions::from_mode(0o500))
        .expect("original permissions should change");
    let service = ItemService::new(repository.clone(), paths.clone());

    let result = service.resolve_duplicate(duplicate.id, false).await;
    fs::set_permissions(&paths.originals, fs::Permissions::from_mode(0o700))
        .expect("original permissions should restore");
    let error = result.expect_err("isolation without directory write access should fail");

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: false,
            ..
        } if service == "filesystem_sync"
    ));
    assert!(repository.get_by_id(duplicate.id).await.is_ok());
    assert!(std::path::Path::new(&duplicate.original_path).is_file());
    assert!(std::path::Path::new(duplicate.normalized_pdf_path.as_deref().unwrap()).is_file());
    assert_no_isolated_files(&paths.originals);
    assert_no_isolated_files(&paths.normalized);
}

#[tokio::test]
async fn discard_claim_blocks_file_path_changes_during_isolation() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let replacement = paths.normalized.join("replacement.pdf");
    fs::write(&replacement, b"replacement").expect("replacement fixture should write");
    let database_url = format!("sqlite://{}", directory.path().join("race.db").display());
    let pool = db::connect(&database_url)
        .await
        .expect("temporary database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "path-race-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "path-race-duplicate");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    let service = ItemService::with_file_lifecycle(
        repository.clone(),
        Arc::new(PathChangesDuringIsolation {
            repository: repository.clone(),
            id: duplicate.id,
            replacement: replacement.clone(),
        }),
    );

    let result = service
        .resolve_duplicate(duplicate.id, false)
        .await
        .expect("claimed discard should complete");

    assert_eq!(result, None);
    assert!(matches!(
        repository.get_by_id(duplicate.id).await,
        Err(AppError::NotFound { .. })
    ));
    assert!(replacement.is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_discards_claim_the_item_before_any_file_isolation() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("concurrent.db").display()
    );
    let pool = db::connect(&database_url)
        .await
        .expect("temporary database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "concurrent-discard-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "concurrent-discard-duplicate");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    let lifecycle = Arc::new(SlowCountingLifecycle::default());
    let first = ItemService::with_file_lifecycle(repository.clone(), lifecycle.clone());
    let second = ItemService::with_file_lifecycle(repository.clone(), lifecycle.clone());

    let (first_result, second_result) = tokio::join!(
        first.resolve_duplicate(duplicate.id, false),
        second.resolve_duplicate(duplicate.id, false)
    );

    let results = [first_result, second_result];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(AppError::NotFound { .. })))
            .count(),
        1
    );
    assert_eq!(
        lifecycle.isolate_calls.load(Ordering::SeqCst),
        1,
        "the losing discard must not touch files"
    );
}

#[tokio::test]
async fn review_rejects_invalid_period_date_and_amount_without_writing() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "invalid-review");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let before = repository
        .get_by_id(record.id)
        .await
        .expect("item snapshot should load");
    let service = ItemService::new(repository.clone(), paths);
    let cases = [
        (
            ItemReview {
                suggested_period: "2026-7".to_owned(),
                ..valid_review(record.id)
            },
            "suggested_period",
        ),
        (
            ItemReview {
                suggested_period: "2026-13".to_owned(),
                ..valid_review(record.id)
            },
            "suggested_period",
        ),
        (
            ItemReview {
                invoice_date: Some("2026-7-01".to_owned()),
                ..valid_review(record.id)
            },
            "invoice_date",
        ),
        (
            ItemReview {
                invoice_date: Some("2026-02-30".to_owned()),
                ..valid_review(record.id)
            },
            "invoice_date",
        ),
        (
            ItemReview {
                amount_cents: -1,
                ..valid_review(record.id)
            },
            "amount_cents",
        ),
    ];

    for (review, expected_field) in cases {
        let error = service
            .review(review)
            .await
            .expect_err("invalid review should be rejected");
        assert!(matches!(
            error,
            AppError::Validation { ref field, .. } if field == expected_field
        ));
        assert_eq!(
            repository
                .get_by_id(record.id)
                .await
                .expect("item should remain unchanged"),
            before
        );
    }
    let unchanged = repository
        .get_by_id(record.id)
        .await
        .expect("item should remain");
    assert_eq!(unchanged.invoice_date, record.invoice_date);
    assert_eq!(unchanged.suggested_period, record.suggested_period);
    assert_eq!(unchanged.final_category, record.final_category);
    assert_eq!(unchanged.amount_cents, record.amount_cents);
    assert_eq!(unchanged.recognition_status, record.recognition_status);
    assert_eq!(unchanged.confirmation_status, record.confirmation_status);
}

#[tokio::test]
async fn review_trims_optional_text_and_converts_whitespace_to_null() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "empty-text");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let service = ItemService::new(repository, paths);

    let reviewed = service
        .review(ItemReview {
            invoice_date: None,
            city: Some("  ".to_owned()),
            company: Some("\n\t".to_owned()),
            note: Some("\u{3000}".to_owned()),
            event_tag: Some(String::new()),
            project_tag: Some("  P-2026  ".to_owned()),
            ..valid_review(record.id)
        })
        .await
        .expect("review should normalize text");

    assert_eq!(reviewed.invoice_date, None);
    assert_eq!(reviewed.city, None);
    assert_eq!(reviewed.company, None);
    assert_eq!(reviewed.note, None);
    assert_eq!(reviewed.event_tag, None);
    assert_eq!(reviewed.project_tag.as_deref(), Some("P-2026"));
}

#[tokio::test]
async fn review_blocks_suspected_duplicates_without_partial_updates() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "review-block-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "review-block-duplicate");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    let service = ItemService::new(repository.clone(), paths);

    let error = service
        .review(valid_review(duplicate.id))
        .await
        .expect_err("suspected duplicate review should be blocked");

    assert!(matches!(error, AppError::Conflict { .. }));
    let unchanged = repository
        .get_by_id(duplicate.id)
        .await
        .expect("duplicate should remain");
    assert_eq!(unchanged.invoice_date, duplicate.invoice_date);
    assert_eq!(unchanged.final_category, duplicate.final_category);
    assert_eq!(unchanged.amount_cents, duplicate.amount_cents);
    assert_eq!(unchanged.dedupe_status, DedupeStatus::SuspectedDuplicate);
    assert_eq!(unchanged.duplicate_of_id, Some(canonical.id));
}

#[tokio::test]
async fn missing_items_and_non_duplicates_have_stable_resolution_errors() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let record = sample_item(&paths, "not-a-duplicate");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    let service = ItemService::new(repository, paths);
    let missing_id = Uuid::new_v4();

    assert!(matches!(
        service.review(valid_review(missing_id)).await,
        Err(AppError::NotFound { ref entity, .. }) if entity == "item"
    ));
    for keep in [true, false] {
        assert!(matches!(
            service.resolve_duplicate(missing_id, keep).await,
            Err(AppError::NotFound { ref entity, .. }) if entity == "item"
        ));
        assert!(matches!(
            service.resolve_duplicate(record.id, keep).await,
            Err(AppError::Conflict { .. })
        ));
    }
}

#[tokio::test]
async fn review_database_failure_rolls_back_every_field() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool.clone());
    let record = sample_item(&paths, "review-db-failure");
    repository
        .insert(&record)
        .await
        .expect("item should insert");
    sqlx::query(&format!(
        "CREATE TRIGGER fail_review BEFORE UPDATE ON items \
         WHEN OLD.id = '{}' AND NEW.confirmation_status = 'confirmed' \
         BEGIN SELECT RAISE(ABORT, 'injected review failure'); END",
        record.id
    ))
    .execute(&pool)
    .await
    .expect("failure trigger should create");
    let service = ItemService::new(repository.clone(), paths);

    let error = service
        .review(valid_review(record.id))
        .await
        .expect_err("database failure should reject review");

    assert!(matches!(error, AppError::Internal { .. }));
    let unchanged = repository
        .get_by_id(record.id)
        .await
        .expect("item should remain");
    assert_eq!(unchanged.invoice_date, record.invoice_date);
    assert_eq!(unchanged.suggested_period, record.suggested_period);
    assert_eq!(unchanged.final_category, record.final_category);
    assert_eq!(unchanged.amount_cents, record.amount_cents);
    assert_eq!(unchanged.city, record.city);
    assert_eq!(unchanged.recognition_status, record.recognition_status);
    assert_eq!(unchanged.confirmation_status, record.confirmation_status);
}

#[tokio::test]
async fn delete_database_failure_restores_isolated_files_and_keeps_the_row() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool.clone());
    let canonical = sample_item(&paths, "delete-db-failure-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "delete-db-failure-duplicate");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    sqlx::query(&format!(
        "CREATE TRIGGER fail_duplicate_delete BEFORE DELETE ON items \
         WHEN OLD.id = '{}' BEGIN SELECT RAISE(ABORT, 'injected delete failure'); END",
        duplicate.id
    ))
    .execute(&pool)
    .await
    .expect("failure trigger should create");
    let service = ItemService::new(repository.clone(), paths.clone());

    let error = service
        .resolve_duplicate(duplicate.id, false)
        .await
        .expect_err("database failure should reject deletion");

    assert!(matches!(error, AppError::Internal { .. }));
    assert!(repository.get_by_id(duplicate.id).await.is_ok());
    assert_eq!(fs::read(&duplicate.original_path).unwrap(), b"original");
    assert_eq!(
        fs::read(duplicate.normalized_pdf_path.as_deref().unwrap()).unwrap(),
        b"normalized"
    );
    assert_no_isolated_files(&paths.originals);
    assert_no_isolated_files(&paths.normalized);
}

#[cfg(unix)]
#[tokio::test]
async fn real_restore_never_clobbers_a_recreated_original_after_database_failure() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool.clone());
    let canonical = sample_item(&paths, "restore-conflict-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "restore-conflict-duplicate");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    sqlx::query(&format!(
        "CREATE TRIGGER fail_restore_conflict_delete BEFORE DELETE ON items \
         WHEN OLD.id = '{}' BEGIN SELECT RAISE(ABORT, 'injected delete failure'); END",
        duplicate.id
    ))
    .execute(&pool)
    .await
    .expect("failure trigger should create");
    let lifecycle = RestoreConflictLifecycle {
        inner: StorageFileLifecycle::new(paths.clone()),
    };
    let service = ItemService::with_file_lifecycle(repository.clone(), Arc::new(lifecycle));

    let error = service
        .resolve_duplicate(duplicate.id, false)
        .await
        .expect_err("restore conflict should require manual recovery");

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: false,
            ..
        } if service == "filesystem_sync"
    ));
    assert!(repository.get_by_id(duplicate.id).await.is_ok());
    assert_eq!(fs::read(&duplicate.original_path).unwrap(), b"replacement");
    assert!(contains_isolated_file(&paths.originals));
    assert!(std::path::Path::new(canonical.original_path.as_str()).is_file());
}

#[cfg(unix)]
#[tokio::test]
async fn real_purge_permission_failure_leaves_recoverable_isolated_files() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "real-purge-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "real-purge-duplicate");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    let lifecycle = PurgePermissionLifecycle {
        inner: StorageFileLifecycle::new(paths.clone()),
        directories: vec![paths.originals.clone(), paths.normalized.clone()],
    };
    let service = ItemService::with_file_lifecycle(repository.clone(), Arc::new(lifecycle));

    let error = service
        .resolve_duplicate(duplicate.id, false)
        .await
        .expect_err("purge without directory write access should fail");

    assert!(matches!(
        error,
        AppError::External {
            ref service,
            retryable: false,
            ..
        } if service == "filesystem_sync"
    ));
    assert!(matches!(
        repository.get_by_id(duplicate.id).await,
        Err(AppError::NotFound { .. })
    ));
    assert!(contains_isolated_file(&paths.originals));
    assert!(contains_isolated_file(&paths.normalized));
    assert!(std::path::Path::new(&canonical.original_path).is_file());
}

#[tokio::test]
async fn discarding_tolerates_already_missing_owned_files() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "missing-files-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "missing-files-duplicate");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    fs::remove_file(&duplicate.original_path).expect("original fixture should remove");
    fs::remove_file(duplicate.normalized_pdf_path.as_deref().unwrap())
        .expect("normalized fixture should remove");
    let service = ItemService::new(repository.clone(), paths);

    assert_eq!(
        service
            .resolve_duplicate(duplicate.id, false)
            .await
            .expect("missing files should be idempotent"),
        None
    );
    assert!(matches!(
        repository.get_by_id(duplicate.id).await,
        Err(AppError::NotFound { .. })
    ));
    assert!(std::path::Path::new(&canonical.original_path).is_file());
}

#[tokio::test]
async fn discarding_tolerates_missing_files_and_their_nested_directories() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "missing-directories-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "missing-directories-duplicate");
    let original_parent = paths.originals.join("2026").join("07");
    let normalized_parent = paths.normalized.join("2026").join("07");
    fs::create_dir_all(&original_parent).expect("nested original directory should create");
    fs::create_dir_all(&normalized_parent).expect("nested normalized directory should create");
    let original = original_parent.join(format!("{}.pdf", duplicate.id));
    let normalized = normalized_parent.join(format!("{}.pdf", duplicate.id));
    fs::write(&original, b"original").expect("nested original should write");
    fs::write(&normalized, b"normalized").expect("nested normalized should write");
    duplicate.original_path = original.to_string_lossy().into_owned();
    duplicate.normalized_pdf_path = Some(normalized.to_string_lossy().into_owned());
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    fs::remove_dir_all(paths.originals.join("2026")).expect("nested original tree should remove");
    fs::remove_dir_all(paths.normalized.join("2026"))
        .expect("nested normalized tree should remove");
    let service = ItemService::new(repository.clone(), paths);

    assert_eq!(
        service
            .resolve_duplicate(duplicate.id, false)
            .await
            .expect("missing nested paths should be idempotent"),
        None
    );
    assert!(matches!(
        repository.get_by_id(duplicate.id).await,
        Err(AppError::NotFound { .. })
    ));
}

#[tokio::test]
async fn discard_rejects_an_owned_path_outside_application_storage() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let outside = directory.path().join("outside.pdf");
    fs::write(&outside, b"outside").expect("outside fixture should write");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "outside-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "outside-duplicate");
    duplicate.original_path = outside.to_string_lossy().into_owned();
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    let service = ItemService::new(repository.clone(), paths);

    let error = service
        .resolve_duplicate(duplicate.id, false)
        .await
        .expect_err("outside path should be rejected");

    assert!(matches!(
        error,
        AppError::Validation { ref field, .. } if field == "original_path"
    ));
    assert!(repository.get_by_id(duplicate.id).await.is_ok());
    assert_eq!(fs::read(&outside).unwrap(), b"outside");
    assert!(std::path::Path::new(&canonical.original_path).is_file());
}

#[tokio::test]
async fn discard_preserves_a_path_also_owned_by_the_canonical_item() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "shared-path-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "shared-path-duplicate");
    duplicate.normalized_pdf_path = canonical.normalized_pdf_path.clone();
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    let service = ItemService::new(repository.clone(), paths.clone());

    let result = service
        .resolve_duplicate(duplicate.id, false)
        .await
        .expect("shared canonical path should be preserved");

    assert_eq!(result, None);
    assert!(matches!(
        repository.get_by_id(duplicate.id).await,
        Err(AppError::NotFound { .. })
    ));
    assert!(!std::path::Path::new(&duplicate.original_path).exists());
    assert!(std::path::Path::new(canonical.normalized_pdf_path.as_deref().unwrap()).is_file());
    assert_no_isolated_files(&paths.originals);
}

#[tokio::test]
async fn discard_preserves_a_path_referenced_by_any_other_item() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "other-reference-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let other = sample_item(&paths, "other-reference-owner");
    repository
        .insert(&other)
        .await
        .expect("other item should insert");
    let mut duplicate = sample_item(&paths, "other-reference-duplicate");
    duplicate.normalized_pdf_path = other.normalized_pdf_path.clone();
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    let service = ItemService::new(repository.clone(), paths.clone());

    let result = service
        .resolve_duplicate(duplicate.id, false)
        .await
        .expect("path referenced by another row should be preserved");

    assert_eq!(result, None);
    assert!(matches!(
        repository.get_by_id(duplicate.id).await,
        Err(AppError::NotFound { .. })
    ));
    assert!(repository.get_by_id(other.id).await.is_ok());
    assert!(std::path::Path::new(other.normalized_pdf_path.as_deref().unwrap()).is_file());
    assert!(!std::path::Path::new(&duplicate.original_path).exists());
    assert_no_isolated_files(&paths.originals);
}

#[cfg(unix)]
#[tokio::test]
async fn discard_rejects_a_symlink_that_escapes_storage() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let outside = directory.path().join("outside-normalized.pdf");
    fs::write(&outside, b"outside").expect("outside fixture should write");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let repository = ItemRepository::new(pool);
    let canonical = sample_item(&paths, "symlink-canonical");
    repository
        .insert(&canonical)
        .await
        .expect("canonical item should insert");
    let mut duplicate = sample_item(&paths, "symlink-duplicate");
    let normalized = PathBuf::from(duplicate.normalized_pdf_path.as_deref().unwrap());
    fs::remove_file(&normalized).expect("normalized fixture should remove");
    symlink(&outside, &normalized).expect("escaping symlink should create");
    duplicate.dedupe_status = DedupeStatus::SuspectedDuplicate;
    duplicate.duplicate_of_id = Some(canonical.id);
    repository
        .insert(&duplicate)
        .await
        .expect("duplicate should insert");
    let service = ItemService::new(repository.clone(), paths.clone());

    let error = service
        .resolve_duplicate(duplicate.id, false)
        .await
        .expect_err("escaping symlink should be rejected");

    assert!(matches!(
        error,
        AppError::Validation { ref field, .. } if field == "normalized_pdf_path"
    ));
    assert!(repository.get_by_id(duplicate.id).await.is_ok());
    assert_eq!(fs::read(&outside).unwrap(), b"outside");
    assert!(std::path::Path::new(&duplicate.original_path).is_file());
    assert_no_isolated_files(&paths.originals);
}

struct PurgeFails;

struct ReviewGatedExtractor {
    started: SyncSender<()>,
    proceed: Mutex<Receiver<()>>,
}

struct ReviewFailingGatedExtractor {
    started: SyncSender<()>,
    proceed: Mutex<Receiver<()>>,
}

impl DocumentExtractor for ReviewFailingGatedExtractor {
    fn extract(&self, _path: &std::path::Path) -> Result<ExtractedDocument, AppError> {
        self.started
            .send(())
            .expect("extractor should announce start");
        self.proceed
            .lock()
            .expect("extractor gate should lock")
            .recv()
            .expect("extractor should be released");
        Err(AppError::External {
            service: "document_extraction".to_owned(),
            retryable: false,
            message: "injected extraction failure".to_owned(),
        })
    }
}

struct ReviewImmediateExtractor;

impl DocumentExtractor for ReviewImmediateExtractor {
    fn extract(&self, _path: &std::path::Path) -> Result<ExtractedDocument, AppError> {
        Ok(ExtractedDocument {
            text: "开票日期：2025-01-02 餐饮 食品 价税合计 ￥1.00 销售方名称：Late Recognition Co"
                .to_owned(),
            normalized_pdf: None,
            warnings: Vec::new(),
        })
    }
}

struct ReviewImmediateFailingExtractor;

impl DocumentExtractor for ReviewImmediateFailingExtractor {
    fn extract(&self, _path: &std::path::Path) -> Result<ExtractedDocument, AppError> {
        Err(AppError::External {
            service: "document_extraction".to_owned(),
            retryable: false,
            message: "injected extraction failure".to_owned(),
        })
    }
}

impl DocumentExtractor for ReviewGatedExtractor {
    fn extract(&self, _path: &std::path::Path) -> Result<ExtractedDocument, AppError> {
        self.started
            .send(())
            .expect("extractor should announce start");
        self.proceed
            .lock()
            .expect("extractor gate should lock")
            .recv()
            .expect("extractor should be released");
        Ok(ExtractedDocument {
            text: "开票日期：2025-01-02 餐饮 食品 价税合计 ￥1.00 销售方名称：Late Recognition Co"
                .to_owned(),
            normalized_pdf: None,
            warnings: Vec::new(),
        })
    }
}

struct IsolationFails;

#[cfg(unix)]
struct RestoreConflictLifecycle {
    inner: StorageFileLifecycle,
}

#[cfg(unix)]
impl FileLifecycle for RestoreConflictLifecycle {
    fn isolate(
        &self,
        files: &ItemFiles,
        protected_paths: &[PathBuf],
    ) -> Result<IsolatedItemFiles, AppError> {
        let isolated = self.inner.isolate(files, protected_paths)?;
        fs::write(&files.original, b"replacement").expect("replacement file should write");
        Ok(isolated)
    }

    fn restore(&self, isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        self.inner.restore(isolated)
    }

    fn purge(&self, isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        self.inner.purge(isolated)
    }
}

#[cfg(unix)]
struct PurgePermissionLifecycle {
    inner: StorageFileLifecycle,
    directories: Vec<PathBuf>,
}

#[cfg(unix)]
impl FileLifecycle for PurgePermissionLifecycle {
    fn isolate(
        &self,
        files: &ItemFiles,
        protected_paths: &[PathBuf],
    ) -> Result<IsolatedItemFiles, AppError> {
        self.inner.isolate(files, protected_paths)
    }

    fn restore(&self, isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        self.inner.restore(isolated)
    }

    fn purge(&self, isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        use std::os::unix::fs::PermissionsExt;

        for directory in &self.directories {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o500))
                .expect("storage permissions should change");
        }
        let result = self.inner.purge(isolated);
        for directory in &self.directories {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .expect("storage permissions should restore");
        }
        result
    }
}

struct PathChangesDuringIsolation {
    repository: ItemRepository,
    id: Uuid,
    replacement: PathBuf,
}

#[derive(Default)]
struct SlowCountingLifecycle {
    isolate_calls: AtomicUsize,
}

impl FileLifecycle for SlowCountingLifecycle {
    fn isolate(
        &self,
        _files: &ItemFiles,
        _protected_paths: &[PathBuf],
    ) -> Result<IsolatedItemFiles, AppError> {
        self.isolate_calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(150));
        Ok(IsolatedItemFiles::empty())
    }

    fn restore(&self, _isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        Ok(())
    }

    fn purge(&self, _isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        Ok(())
    }
}

impl FileLifecycle for PathChangesDuringIsolation {
    fn isolate(
        &self,
        _files: &ItemFiles,
        _protected_paths: &[PathBuf],
    ) -> Result<IsolatedItemFiles, AppError> {
        let repository = self.repository.clone();
        let id = self.id;
        let replacement = self.replacement.to_string_lossy().into_owned();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("mutation runtime should build")
                .block_on(repository.update_fields(
                    id,
                    ItemPatch {
                        normalized_pdf_path: Some(Some(replacement)),
                        ..ItemPatch::default()
                    },
                ))
                .expect_err("duplicate deletion claim should block path mutation");
        })
        .join()
        .expect("path mutation thread should finish");
        Ok(IsolatedItemFiles::empty())
    }

    fn restore(&self, _isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        Ok(())
    }

    fn purge(&self, _isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        Ok(())
    }
}

impl FileLifecycle for IsolationFails {
    fn isolate(
        &self,
        _files: &ItemFiles,
        _protected_paths: &[PathBuf],
    ) -> Result<IsolatedItemFiles, AppError> {
        Err(AppError::Internal {
            message: "injected isolation failure".to_owned(),
        })
    }

    fn restore(&self, _isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        Ok(())
    }

    fn purge(&self, _isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        Ok(())
    }
}

impl FileLifecycle for PurgeFails {
    fn isolate(
        &self,
        _files: &ItemFiles,
        _protected_paths: &[PathBuf],
    ) -> Result<IsolatedItemFiles, AppError> {
        Ok(IsolatedItemFiles::empty())
    }

    fn restore(&self, _isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        Ok(())
    }

    fn purge(&self, _isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        Err(AppError::Internal {
            message: "injected purge failure".to_owned(),
        })
    }
}

fn assert_no_isolated_files(directory: &std::path::Path) {
    assert!(
        !contains_isolated_file(directory),
        "{} contains isolated deletion files",
        directory.display()
    );
}

fn contains_isolated_file(directory: &std::path::Path) -> bool {
    fs::read_dir(directory)
        .expect("storage directory should read")
        .any(|entry| {
            let entry = entry.expect("storage entry should read");
            entry.file_name().to_string_lossy().contains(".delete-")
                || (entry
                    .file_type()
                    .expect("storage entry type should read")
                    .is_dir()
                    && contains_isolated_file(&entry.path()))
        })
}

fn valid_review(id: Uuid) -> ItemReview {
    ItemReview {
        id,
        invoice_date: Some("2026-07-11".to_owned()),
        suggested_period: "2026-07".to_owned(),
        final_category: Category::Hospitality,
        amount_cents: 45_600,
        city: Some("Beijing".to_owned()),
        company: Some("Example Ltd".to_owned()),
        note: Some("client dinner".to_owned()),
        event_tag: Some("summit".to_owned()),
        project_tag: Some("P-2026".to_owned()),
    }
}

fn crash_token_path(target: &std::path::Path) -> PathBuf {
    target.with_file_name(format!(
        ".{}.delete-{}",
        target.file_name().unwrap().to_string_lossy(),
        Uuid::new_v4()
    ))
}

fn sample_item(paths: &AppPaths, suffix: &str) -> NewItemRecord {
    let id = Uuid::new_v4();
    let original_path = paths.originals.join(format!("{id}.pdf"));
    let normalized_path = paths.normalized.join(format!("{id}.pdf"));
    fs::write(&original_path, b"original").expect("original fixture should write");
    fs::write(&normalized_path, b"normalized").expect("normalized fixture should write");
    let fetched_at = Utc
        .with_ymd_and_hms(2026, 7, 13, 2, 0, 0)
        .single()
        .expect("valid fetched time");

    NewItemRecord {
        id,
        original_name: format!("invoice-{suffix}.pdf"),
        original_path: original_path.to_string_lossy().into_owned(),
        normalized_pdf_path: Some(normalized_path.to_string_lossy().into_owned()),
        sha256: format!("sha256-{suffix}"),
        mime_type: "application/pdf".to_owned(),
        source_type: SourceType::ManualUpload,
        source_account_id: None,
        source_mailbox: None,
        source_uid: None,
        source_message_id: Some(format!("message-{suffix}")),
        source_part_id: None,
        fetched_at,
        invoice_date: None,
        suggested_period: Some("2026-06".to_owned()),
        batch_id: None,
        suggested_category: Some(Category::Transport),
        final_category: None,
        amount_cents: None,
        currency: "CNY".to_owned(),
        city: None,
        company: None,
        recognition_status: RecognitionStatus::Failed,
        confirmation_status: ConfirmationStatus::Pending,
        dedupe_status: DedupeStatus::Unique,
        duplicate_of_id: None,
        note: None,
        event_tag: None,
        project_tag: None,
        created_at: fetched_at,
        updated_at: fetched_at,
    }
}
