//! Tests for `import`, kept out of the production file.
use std::fs;
use std::path::Path;

use chrono::{TimeZone, Utc};
use tokio::io::{AsyncRead, AsyncReadExt};
use uuid::Uuid;

use super::{
    EmailImportSource, ImportOutcome, ImportService, copy_source_to_staging,
    signature_matches_extension,
};
use crate::db;
use crate::db::items::{ItemPatch, ItemRepository};
use crate::domain::error::AppError;
use crate::domain::model::{ConfirmationStatus, DedupeStatus, RecognitionStatus};
use crate::infra::files::AppPaths;

fn email_source(uid: u32, part_id: &str) -> EmailImportSource {
    EmailImportSource {
        account_id: Uuid::new_v4(),
        mailbox: "INBOX".to_owned(),
        uid_validity: 71,
        uid,
        message_id: Some(format!("invoice-{uid}@example.com")),
        part_id: part_id.to_owned(),
        received_at: Utc.with_ymd_and_hms(2026, 7, 23, 10, 0, 0).unwrap(),
        source_received_date: chrono::NaiveDate::from_ymd_opt(2026, 7, 23).unwrap(),
        rescan: false,
    }
}

#[test]
fn signature_compatibility_uses_exact_families_for_supported_extensions() {
    let pdf = b"%PDF-1.7";
    let jpeg = [0xff, 0xd8, 0xff, 0xe0];
    let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let ole = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
    let zip = *b"PK\x03\x04";

    for (extension, bytes) in [
        ("pdf", pdf.as_slice()),
        ("jpg", jpeg.as_slice()),
        ("jpeg", jpeg.as_slice()),
        ("png", png.as_slice()),
        ("doc", ole.as_slice()),
        ("xls", ole.as_slice()),
        ("docx", zip.as_slice()),
        ("xlsx", zip.as_slice()),
        ("zip", zip.as_slice()),
    ] {
        assert!(
            signature_matches_extension(extension, bytes),
            "{extension} should accept its signature"
        );
    }

    assert!(!signature_matches_extension("pdf", &jpeg));
    assert!(!signature_matches_extension("doc", &zip));
    assert!(!signature_matches_extension("docx", &ole));
    assert!(!signature_matches_extension("pdf", b"unknown"));
}

#[tokio::test]
async fn email_import_preserves_imap_calendar_date_across_a_utc_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let items = ItemRepository::new(pool);
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = ImportService::new(items, paths);
    let local_received_at = chrono::FixedOffset::east_opt(8 * 60 * 60)
        .unwrap()
        .with_ymd_and_hms(2026, 8, 1, 0, 30, 0)
        .single()
        .unwrap();
    let source = EmailImportSource {
        received_at: local_received_at.with_timezone(&Utc),
        source_received_date: local_received_at.date_naive(),
        ..email_source(801, "1")
    };

    let ImportOutcome::New(imported) = service
        .import_email_bytes("august-boundary.pdf", b"%PDF-1.7\n%%EOF\n", source)
        .await
        .unwrap()
    else {
        panic!("boundary email fixture should be newly imported")
    };

    assert_eq!(
        imported.fetched_at,
        Utc.with_ymd_and_hms(2026, 7, 31, 16, 30, 0)
            .single()
            .unwrap()
    );
    assert_eq!(
        imported.source_received_date,
        chrono::NaiveDate::from_ymd_opt(2026, 8, 1)
    );
    assert_eq!(
        imported.batch_membership_date(),
        chrono::NaiveDate::from_ymd_opt(2026, 8, 1)
    );
}

#[tokio::test]
async fn rescan_repairs_received_metadata_for_every_existing_email_match_path() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let items = ItemRepository::new(pool.clone());
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = ImportService::new(items, paths);
    let pdf = b"%PDF-1.7\nreceived metadata repair\n%%EOF\n";
    let mut fixtures = Vec::new();

    for (uid, part_id) in [(811, "exact"), (812, "legacy"), (813, "rescanned")] {
        let source = email_source(uid, part_id);
        let ImportOutcome::New(item) = service
            .import_email_bytes(&format!("{part_id}.pdf"), pdf, source.clone())
            .await
            .unwrap()
        else {
            panic!("{part_id} fixture should be newly imported")
        };
        fixtures.push((item, source));
    }

    sqlx::query("UPDATE items SET source_received_date = NULL WHERE id = ?")
        .bind(fixtures[0].0.id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE items SET source_uid_validity = 0, source_received_date = NULL WHERE id = ?",
    )
    .bind(fixtures[1].0.id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE items SET source_uid_validity = 70, source_received_date = NULL WHERE id = ?",
    )
    .bind(fixtures[2].0.id.to_string())
    .execute(&pool)
    .await
    .unwrap();

    let local_received_at = chrono::FixedOffset::east_opt(8 * 60 * 60)
        .unwrap()
        .with_ymd_and_hms(2026, 8, 1, 0, 30, 0)
        .single()
        .unwrap();
    for (original, mut source) in fixtures {
        source.received_at = local_received_at.with_timezone(&Utc);
        source.source_received_date = local_received_at.date_naive();
        source.rescan = true;

        let ImportOutcome::Existing(repaired) = service
            .import_email_bytes(&original.original_name, pdf, source)
            .await
            .unwrap()
        else {
            panic!("rescan should preserve the existing item identity")
        };

        assert_eq!(repaired.id, original.id);
        assert_eq!(repaired.fetched_at, local_received_at.with_timezone(&Utc));
        assert_eq!(
            repaired.source_received_date,
            Some(local_received_at.date_naive())
        );
        assert_eq!(
            repaired.batch_membership_date(),
            Some(local_received_at.date_naive())
        );
    }
}

#[tokio::test]
async fn capped_copy_rejects_zero_and_oversize_content_without_staging_residue() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");

    async fn assert_rejected<R>(paths: &AppPaths, reader: &mut R)
    where
        R: AsyncRead + Unpin,
    {
        let (staged, writer) = paths
            .begin_staged_original(Uuid::new_v4())
            .expect("staging should begin");
        let error = copy_source_to_staging(reader, staged, writer, false)
            .await
            .expect_err("invalid actual byte count should fail");

        assert!(matches!(error, AppError::Validation { ref field, .. } if field == "file"));
        assert!(
            fs::read_dir(&paths.staging)
                .expect("staging should read")
                .next()
                .is_none()
        );
    }

    assert_rejected(&paths, &mut tokio::io::empty()).await;
    assert_rejected(
        &paths,
        &mut tokio::io::repeat(0x5a).take(super::MAX_FILE_SIZE + 1),
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn replacing_source_path_after_open_imports_the_pinned_inode() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let source = directory.path().join("pinned.pdf");
    let moved = directory.path().join("opened.pdf");
    let original = b"%PDF-1.7\nopened inode\n";
    let replacement = b"%PDF-1.7\nreplacement path\n";
    fs::write(&source, original).expect("original source should write");
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("in-memory database should connect");
    let paths = AppPaths::create(directory.path().join("storage"))
        .expect("application paths should create");
    let service = ImportService::new(ItemRepository::new(pool), paths);
    let source_for_hook = source.clone();

    let imported = service
        .import_manual_after_open(&source, move || {
            fs::rename(&source_for_hook, &moved)
                .map_err(|error| super::internal_error("failed to replace source", error))?;
            fs::write(&source_for_hook, replacement)
                .map_err(|error| super::internal_error("failed to write replacement", error))
        })
        .await
        .expect("pinned source should import");

    assert_eq!(
        fs::read(imported.original_path).expect("imported original should read"),
        original
    );
    assert_eq!(
        fs::read(source).expect("replacement should read"),
        replacement
    );
}

#[tokio::test]
async fn rescan_replaces_failed_unreviewed_email_attachment() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let items = ItemRepository::new(pool);
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = ImportService::new(items.clone(), paths.clone());
    let mut source = email_source(5454, "2");
    let corrupted = b"%PDF-1.7\n%\xef\xbf\xbd\xef\xbf\xbd\n%%EOF\n";
    let corrected = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n%%EOF\n";
    let ImportOutcome::New(imported) = service
        .import_email_bytes("invoice.pdf", corrupted, source.clone())
        .await
        .unwrap()
    else {
        panic!("first attachment should be new")
    };
    items
        .update_fields(
            imported.id,
            ItemPatch {
                recognition_status: Some(RecognitionStatus::Failed),
                ..ItemPatch::default()
            },
        )
        .await
        .unwrap();
    let old_path = imported.original_path.clone();
    source.rescan = true;

    let ImportOutcome::Existing(repaired) = service
        .import_email_bytes("invoice.pdf", corrected, source)
        .await
        .unwrap()
    else {
        panic!("rescan should preserve the item identity")
    };

    assert_eq!(repaired.id, imported.id);
    assert_eq!(repaired.recognition_status, RecognitionStatus::Pending);
    assert_eq!(fs::read(&repaired.original_path).unwrap(), corrected);
    assert_ne!(repaired.original_path, old_path);
    assert!(!Path::new(&old_path).exists());
    assert_eq!(count_files(&paths.originals), 1);
    assert!(fs::read_dir(&paths.staging).unwrap().next().is_none());
}

#[tokio::test]
async fn downloaded_link_replaces_the_legacy_placeholder_and_recalculates_dedupe() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let items = ItemRepository::new(pool);
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = ImportService::new(items.clone(), paths.clone());
    let source = email_source(103, "1.link.0");
    let ImportOutcome::New(legacy) = service
        .import_email_link("https://invoice.example/103.pdf", source.clone())
        .await
        .unwrap()
    else {
        panic!("legacy link should be new")
    };
    let legacy_path = legacy.original_path.clone();
    let pdf = b"%PDF-1.7\ninvoice contents\n%%EOF\n";
    let canonical_source = directory.path().join("canonical.pdf");
    fs::write(&canonical_source, pdf).unwrap();
    let canonical = service.import_manual(&canonical_source).await.unwrap();

    let ImportOutcome::Existing(replaced) = service
        .import_downloaded_email_link("invoice-103.pdf", pdf, source.clone())
        .await
        .unwrap()
    else {
        panic!("placeholder replacement should retain its item identity")
    };

    assert_eq!(replaced.id, legacy.id);
    assert_eq!(replaced.original_name, "invoice-103.pdf");
    assert_eq!(replaced.mime_type, "application/pdf");
    assert_eq!(replaced.recognition_status, RecognitionStatus::Pending);
    assert_eq!(replaced.confirmation_status, ConfirmationStatus::Pending);
    assert_eq!(replaced.dedupe_status, DedupeStatus::SuspectedDuplicate);
    assert_eq!(replaced.duplicate_of_id, Some(canonical.id));
    assert_eq!(replaced.source_account_id, Some(source.account_id));
    assert_eq!(replaced.source_uid, Some(i64::from(source.uid)));
    assert_eq!(replaced.source_part_id.as_deref(), Some("1.link.0"));
    assert_eq!(fs::read(&replaced.original_path).unwrap(), pdf);
    assert!(!std::path::Path::new(&legacy_path).exists());
    assert!(fs::read_dir(&paths.staging).unwrap().next().is_none());
}

#[tokio::test]
async fn stale_download_failure_cannot_mark_a_replaced_pdf_failed() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let items = ItemRepository::new(pool);
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = ImportService::new(items.clone(), paths);
    let source = email_source(104, "1.link.0");
    let ImportOutcome::New(placeholder) = service
        .import_email_link(
            "https://invoice.example/104.pdf?signature=private",
            source.clone(),
        )
        .await
        .unwrap()
    else {
        panic!("legacy link should be new")
    };
    let pdf = b"%PDF-1.7\nwinning invoice\n%%EOF\n";
    let ImportOutcome::Existing(replaced) = service
        .import_downloaded_email_link("invoice-104.pdf", pdf, source)
        .await
        .unwrap()
    else {
        panic!("download should replace the placeholder")
    };

    let after_stale_failure = items
        .mark_email_link_download_failed(placeholder.id)
        .await
        .unwrap();

    assert_eq!(after_stale_failure.id, placeholder.id);
    assert_eq!(after_stale_failure.original_name, "invoice-104.pdf");
    assert_eq!(after_stale_failure.mime_type, "application/pdf");
    assert_eq!(
        after_stale_failure.recognition_status,
        RecognitionStatus::Pending
    );
    assert_eq!(after_stale_failure.note, None);
    assert_eq!(after_stale_failure.original_path, replaced.original_path);
    assert_eq!(fs::read(after_stale_failure.original_path).unwrap(), pdf);
}

#[tokio::test]
async fn concurrent_download_failure_and_success_leave_the_real_pdf_pending() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("failure-success.sqlite3");
    let database_url = format!("sqlite://{}", database_path.display());
    let pool = db::connect(&database_url).await.unwrap();
    let items = ItemRepository::new(pool);
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = ImportService::new(items.clone(), paths.clone());
    let source = email_source(106, "1.link.0");
    let ImportOutcome::New(placeholder) = service
        .import_email_link(
            "https://invoice.example/106.pdf?signature=private",
            source.clone(),
        )
        .await
        .unwrap()
    else {
        panic!("legacy link should be new")
    };
    let mut pdf = b"%PDF-1.7\n".to_vec();
    pdf.resize(4 * 1024 * 1024, b'y');
    pdf.extend_from_slice(b"\n%%EOF\n");
    let barrier = tokio::sync::Barrier::new(2);

    let (failure, success) = tokio::join!(
        async {
            barrier.wait().await;
            service
                .import_failed_email_link(
                    "https://invoice.example/106.pdf?signature=private",
                    source.clone(),
                )
                .await
        },
        async {
            barrier.wait().await;
            service
                .import_downloaded_email_link("invoice-106.pdf", &pdf, source.clone())
                .await
        }
    );
    failure.unwrap();
    success.unwrap();

    let current = items.get_by_id(placeholder.id).await.unwrap();
    assert_eq!(current.original_name, "invoice-106.pdf");
    assert_eq!(current.mime_type, "application/pdf");
    assert_eq!(current.recognition_status, RecognitionStatus::Pending);
    assert_eq!(current.note, None);
    assert_eq!(fs::read(current.original_path).unwrap(), pdf);
    assert_eq!(count_files(&paths.originals), 1);
    assert!(fs::read_dir(&paths.staging).unwrap().next().is_none());
}

#[tokio::test]
async fn failure_guard_preserves_confirmed_and_batch_assigned_placeholders() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let items = ItemRepository::new(pool.clone());
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = ImportService::new(items.clone(), paths);
    let confirmed_source = email_source(107, "1.link.0");
    let ImportOutcome::New(confirmed) = service
        .import_email_link("https://invoice.example/107.pdf", confirmed_source.clone())
        .await
        .unwrap()
    else {
        panic!("confirmed fixture should be new")
    };
    items
        .update_fields(
            confirmed.id,
            ItemPatch {
                confirmation_status: Some(ConfirmationStatus::Confirmed),
                ..ItemPatch::default()
            },
        )
        .await
        .unwrap();
    let confirmed_before = items.get_by_id(confirmed.id).await.unwrap();

    let batched_source = email_source(108, "1.link.0");
    let ImportOutcome::New(batched) = service
        .import_email_link("https://invoice.example/108.pdf", batched_source)
        .await
        .unwrap()
    else {
        panic!("batched fixture should be new")
    };
    let batch_id = Uuid::new_v4();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO batches (id, name, start_date, end_date, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(batch_id.to_string())
    .bind("Protected placeholders")
    .bind("2026-07-01")
    .bind("2026-07-31")
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE items SET batch_id = ? WHERE id = ?")
        .bind(batch_id.to_string())
        .bind(batched.id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let batched_before = items.get_by_id(batched.id).await.unwrap();

    let confirmed_after = items
        .mark_email_link_download_failed(confirmed.id)
        .await
        .unwrap();
    let batched_after = items
        .mark_email_link_download_failed(batched.id)
        .await
        .unwrap();

    assert_eq!(confirmed_after, confirmed_before);
    assert_eq!(batched_after, batched_before);
    assert_eq!(
        confirmed_after.recognition_status,
        RecognitionStatus::Succeeded
    );
    assert_eq!(
        batched_after.recognition_status,
        RecognitionStatus::Succeeded
    );
}

#[tokio::test]
async fn concurrent_successful_link_retries_share_one_replaced_original() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("concurrent-link.sqlite3");
    let database_url = format!("sqlite://{}", database_path.display());
    let pool = db::connect(&database_url).await.unwrap();
    let items = ItemRepository::new(pool);
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = ImportService::new(items.clone(), paths.clone());
    let source = email_source(105, "1.link.0");
    let ImportOutcome::New(placeholder) = service
        .import_email_link("https://invoice.example/105.pdf", source.clone())
        .await
        .unwrap()
    else {
        panic!("legacy link should be new")
    };
    let mut pdf = b"%PDF-1.7\n".to_vec();
    pdf.resize(8 * 1024 * 1024, b'x');
    pdf.extend_from_slice(b"\n%%EOF\n");
    let barrier = tokio::sync::Barrier::new(2);

    let (first, second) = tokio::join!(
        async {
            barrier.wait().await;
            service
                .import_downloaded_email_link("invoice-105.pdf", &pdf, source.clone())
                .await
        },
        async {
            barrier.wait().await;
            service
                .import_downloaded_email_link("invoice-105.pdf", &pdf, source.clone())
                .await
        }
    );
    let ImportOutcome::Existing(first) = first.unwrap() else {
        panic!("first retry should retain the placeholder identity")
    };
    let ImportOutcome::Existing(second) = second.unwrap() else {
        panic!("second retry should retain the placeholder identity")
    };

    assert_eq!(first.id, placeholder.id);
    assert_eq!(second.id, placeholder.id);
    assert_eq!(first.original_path, second.original_path);
    assert_eq!(first.original_name, "invoice-105.pdf");
    assert_eq!(first.mime_type, "application/pdf");
    assert_eq!(first.recognition_status, RecognitionStatus::Pending);
    assert_eq!(items.get_by_id(placeholder.id).await.unwrap(), first);
    assert_eq!(count_files(&paths.originals), 1);
    assert!(fs::read_dir(&paths.staging).unwrap().next().is_none());
    assert_eq!(fs::read(first.original_path).unwrap(), pdf);
}

#[tokio::test]
async fn ignored_link_discards_only_an_unassigned_pending_placeholder() {
    let directory = tempfile::tempdir().unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let items = ItemRepository::new(pool);
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let service = ImportService::new(items.clone(), paths);
    let disposable_source = email_source(201, "1.link.0");
    let ImportOutcome::New(disposable) = service
        .import_email_link(
            "https://invoice.example/invoice.xml",
            disposable_source.clone(),
        )
        .await
        .unwrap()
    else {
        panic!("disposable link should be new")
    };
    let protected_source = email_source(202, "1.link.0");
    let ImportOutcome::New(protected) = service
        .import_email_link("https://fp.nuonuo.com/#/", protected_source.clone())
        .await
        .unwrap()
    else {
        panic!("protected link should be new")
    };
    items
        .update_fields(
            protected.id,
            ItemPatch {
                confirmation_status: Some(ConfirmationStatus::Confirmed),
                ..ItemPatch::default()
            },
        )
        .await
        .unwrap();

    assert!(
        service
            .discard_email_link_placeholder(&disposable_source)
            .await
            .unwrap()
    );
    assert!(
        !service
            .discard_email_link_placeholder(&protected_source)
            .await
            .unwrap()
    );

    assert!(items.get_by_id(disposable.id).await.is_err());
    assert!(!std::path::Path::new(&disposable.original_path).exists());
    assert_eq!(
        items.get_by_id(protected.id).await.unwrap().id,
        protected.id
    );
    assert!(std::path::Path::new(&protected.original_path).exists());
}

fn count_files(path: &Path) -> usize {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() { count_files(&path) } else { 1 }
        })
        .sum()
}
