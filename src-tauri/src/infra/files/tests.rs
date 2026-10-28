//! Tests for `files`, kept out of the production file.
use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, Write};
use std::sync::{Arc, Barrier};

#[cfg(unix)]
use super::open_staging_file;
use super::{
    classify_completed_rollback, classify_staging_permission_failure,
    open_staging_file_with_permissions, promote_staged_original,
    promote_staged_original_with_rollback_sync, promote_staged_original_with_sync,
    remove_file_durably_with_sync,
};
use crate::domain::error::AppError;

#[cfg(unix)]
#[test]
fn staged_original_files_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary directory should create");
    let staging = directory.path().join("original.part");

    let file = open_staging_file(&staging).expect("staging file should create");

    assert_eq!(
        file.metadata()
            .expect("staging metadata should read")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn staging_permission_failure_durably_removes_the_created_file() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let staging = directory.path().join("original.part");

    let error = open_staging_file_with_permissions(&staging, |_| {
        Err(io::Error::other("injected staging permission failure"))
    })
    .expect_err("permission failure should reject the staged file");

    assert!(matches!(
        error,
        AppError::Internal { ref message }
            if message.contains("injected staging permission failure")
    ));
    assert!(!staging.exists());
}

#[test]
fn staging_permission_cleanup_failure_requires_manual_recovery() {
    let error = classify_staging_permission_failure(
        io::Error::other("injected staging permission failure"),
        Err(AppError::Internal {
            message: "injected staging cleanup sync failure".to_owned(),
        }),
    );

    match error {
        AppError::External {
            service,
            retryable,
            message,
        } => {
            assert_eq!(service, "filesystem_sync");
            assert!(!retryable);
            assert!(message.contains("manual recovery is required"));
            assert!(message.contains("[staging]"));
            assert!(message.contains("injected staging permission failure"));
            assert!(message.contains("injected staging cleanup sync failure"));
        }
        other => panic!("unexpected staging cleanup error: {other:?}"),
    }
}

#[test]
fn durable_file_removal_syncs_the_parent_after_unlink() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let staging = directory.path().join("original.part");
    fs::write(&staging, b"staged original").expect("staging fixture should write");
    let parent_was_synced = Cell::new(false);

    remove_file_durably_with_sync(&staging, |parent| {
        assert_eq!(parent, directory.path());
        parent_was_synced.set(true);
        Ok(())
    })
    .expect("owned staging file should be removed durably");

    assert!(!staging.exists());
    assert!(parent_was_synced.get());
}

#[test]
fn durable_file_removal_ignores_a_missing_file_without_syncing() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let missing = directory.path().join("missing.part");

    remove_file_durably_with_sync(&missing, |_| {
        panic!("an unchanged directory must not be synced")
    })
    .expect("missing staging file cleanup should be idempotent");
}

#[test]
fn promotion_sync_failure_rolls_back_and_removes_the_owned_file() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let staging_directory = directory.path().join("staging");
    let destination_directory = directory.path().join("originals");
    fs::create_dir(&staging_directory).expect("staging directory should create");
    fs::create_dir(&destination_directory).expect("destination directory should create");
    let staging = staging_directory.join("original.part");
    let destination = destination_directory.join("original.pdf");
    fs::write(&staging, b"staged original").expect("staging fixture should write");

    let error = promote_staged_original_with_sync(&staging, &destination, |_| {
        Err(io::Error::other("injected directory sync failure"))
    })
    .expect_err("directory sync failure should roll promotion back");

    assert!(matches!(error, AppError::Internal { .. }));
    assert!(!destination.exists());
    assert!(!staging.exists());
}

#[test]
fn rollback_directory_sync_failure_requires_manual_recovery() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let staging_directory = directory.path().join("staging");
    let destination_directory = directory.path().join("originals");
    fs::create_dir(&staging_directory).expect("staging directory should create");
    fs::create_dir(&destination_directory).expect("destination directory should create");
    let staging = staging_directory.join("original.part");
    let destination = destination_directory.join("original.pdf");
    fs::write(&staging, b"staged original").expect("staging fixture should write");

    let error = promote_staged_original_with_rollback_sync(
        &staging,
        &destination,
        |_| Err(io::Error::other("injected promotion sync failure")),
        |_| Err(io::Error::other("injected rollback sync failure")),
    )
    .expect_err("rollback directory sync failure should require manual recovery");

    assert_filesystem_sync_recovery_error(
        error,
        &[
            "injected promotion sync failure",
            "injected rollback sync failure",
        ],
    );
    assert!(!destination.exists());
    assert!(!staging.exists());
}

#[test]
fn rollback_cleanup_sync_failure_requires_manual_recovery() {
    let error = classify_completed_rollback(
        io::Error::other("injected promotion sync failure"),
        Ok(()),
        Err(AppError::Internal {
            message: "injected cleanup sync failure".to_owned(),
        }),
    );

    assert_filesystem_sync_recovery_error(
        error,
        &[
            "injected promotion sync failure",
            "injected cleanup sync failure",
        ],
    );
}

#[test]
fn promotion_rollback_failure_reports_explicit_manual_recovery() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let staging_directory = directory.path().join("staging");
    let destination_directory = directory.path().join("originals");
    fs::create_dir(&staging_directory).expect("staging directory should create");
    fs::create_dir(&destination_directory).expect("destination directory should create");
    let staging = staging_directory.join("original.part");
    let destination = destination_directory.join("original.pdf");
    fs::write(&staging, b"staged original").expect("staging fixture should write");

    let error = promote_staged_original_with_sync(&staging, &destination, |_| {
        fs::write(&staging, b"new staging owner")
            .expect("replacement staging fixture should write");
        Err(io::Error::other("injected directory sync failure"))
    })
    .expect_err("blocked rollback should require manual recovery");

    match error {
        AppError::External {
            service,
            retryable,
            message,
        } => {
            assert_eq!(service, "filesystem_sync");
            assert!(!retryable);
            assert!(message.contains("manual recovery is required"));
        }
        other => panic!("unexpected rollback error: {other:?}"),
    }
    assert_eq!(
        fs::read(&destination).expect("promoted destination should remain"),
        b"staged original"
    );
    assert_eq!(
        fs::read(&staging).expect("new staging owner should remain"),
        b"new staging owner"
    );
}

#[test]
fn concurrent_promotions_never_clobber_the_destination() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let destination = directory.path().join("original.pdf");
    let staging_files = [
        (directory.path().join("first.part"), b"first".as_slice()),
        (directory.path().join("second.part"), b"second".as_slice()),
    ];

    for (path, content) in &staging_files {
        let mut file = File::create(path).expect("staging file should create");
        file.write_all(content)
            .expect("staging content should write");
        file.sync_all().expect("staging file should sync");
    }

    let barrier = Arc::new(Barrier::new(staging_files.len()));
    let workers = staging_files
        .iter()
        .map(|(staging, content)| {
            let staging = staging.clone();
            let destination = destination.clone();
            let content = content.to_vec();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                (promote_staged_original(&staging, &destination), content)
            })
        })
        .collect::<Vec<_>>();

    let mut winner = None;
    let mut conflicts = 0;
    for worker in workers {
        let (result, content) = worker.join().expect("promotion worker should finish");
        match result {
            Ok(()) => {
                assert!(winner.replace(content).is_none(), "only one worker may win");
            }
            Err(AppError::Conflict { .. }) => conflicts += 1,
            Err(error) => panic!("unexpected promotion error: {error:?}"),
        }
    }

    let winner = winner.expect("one promotion should succeed");
    assert_eq!(conflicts, 1);
    assert_eq!(
        fs::read(&destination).expect("destination should read"),
        winner
    );
    for (path, _) in staging_files {
        assert!(!path.exists(), "staging file leaked: {}", path.display());
    }
}

fn assert_filesystem_sync_recovery_error(error: AppError, expected_details: &[&str]) {
    match error {
        AppError::External {
            service,
            retryable,
            message,
        } => {
            assert_eq!(service, "filesystem_sync");
            assert!(!retryable);
            assert!(message.contains("manual recovery is required"));
            assert!(message.contains("[destination]"));
            assert!(message.contains("[staging]"));
            for detail in expected_details {
                assert!(message.contains(detail), "missing error detail: {detail}");
            }
        }
        other => panic!("unexpected filesystem recovery error: {other:?}"),
    }
}
