use std::fs;
use std::sync::Arc;

use chrono::NaiveDate;
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::infra::credentials::{CredentialStore, MemoryCredentialStore};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::state::AppState;
use sqlx::SqlitePool;
use uuid::Uuid;

#[test]
fn app_paths_create_the_stable_storage_layout() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let root = directory.path().join("invoice-desk");

    let paths = AppPaths::create(&root).expect("application paths should create");

    assert_eq!(paths.root, root);
    assert_eq!(paths.originals, root.join("originals"));
    assert_eq!(paths.normalized, root.join("normalized"));
    assert_eq!(paths.exports, root.join("exports"));
    assert_eq!(paths.staging, root.join("staging"));
    for path in [
        &paths.originals,
        &paths.normalized,
        &paths.exports,
        &paths.staging,
    ] {
        assert!(
            path.is_dir(),
            "missing storage directory: {}",
            path.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn app_paths_secure_every_new_component_of_a_nested_root() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary directory should create");
    let first_component = directory.path().join("level-one");
    let second_component = first_component.join("level-two");
    let root = second_component.join("invoice-desk");

    AppPaths::create(&root).expect("nested application root should create");

    for path in [&first_component, &second_component, &root] {
        let mode = fs::metadata(path)
            .expect("nested root metadata should read")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode,
            0o700,
            "nested root component is not private: {}",
            path.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn app_paths_reject_a_storage_subdirectory_symlink_escape() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("temporary directory should create");
    let root = directory.path().join("invoice-desk");
    let outside = directory.path().join("outside");
    fs::create_dir(&root).expect("application root fixture should create");
    fs::create_dir(&outside).expect("outside fixture should create");
    symlink(&outside, root.join("originals")).expect("symlink fixture should create");

    let error = AppPaths::create(&root).expect_err("storage symlink should be rejected");

    assert!(matches!(error, AppError::Internal { .. }));
}

#[cfg(unix)]
#[test]
fn storage_directories_and_original_files_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary directory should create");
    let root = directory.path().join("invoice-desk");
    let paths = AppPaths::create(&root).expect("application paths should create");
    let source = directory.path().join("upload.pdf");
    fs::write(&source, b"private invoice").expect("source fixture should write");
    let id = Uuid::new_v4();
    let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("fixture date should be valid");

    let destination = paths
        .persist_original(&source, date, id, "pdf")
        .expect("original should persist");

    for path in [
        &paths.root,
        &paths.originals,
        &paths.normalized,
        &paths.exports,
        &paths.staging,
        &paths.originals.join("2026"),
        &paths.originals.join("2026").join("07"),
    ] {
        let mode = fs::metadata(path)
            .expect("directory metadata should read")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "directory is not private: {}", path.display());
    }
    assert_eq!(
        fs::metadata(destination)
            .expect("original metadata should read")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn memory_credentials_are_scoped_by_account() {
    let credentials = MemoryCredentialStore::default();

    credentials
        .set("account-a", "secret-a")
        .expect("first credential should store");
    credentials
        .set("account-b", "secret-b")
        .expect("second credential should store");

    assert_eq!(
        credentials
            .get("account-a")
            .expect("credential should read"),
        Some("secret-a".to_owned())
    );
    assert_eq!(
        credentials
            .get("account-b")
            .expect("credential should read"),
        Some("secret-b".to_owned())
    );
    assert_eq!(
        credentials
            .get("unknown-account")
            .expect("missing credential should read"),
        None
    );
}

#[test]
fn persist_original_streams_content_to_the_dated_destination() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path()).expect("application paths should create");
    let source = directory.path().join("upload.pdf");
    let content = vec![0x5a; 128 * 1024];
    fs::write(&source, &content).expect("source fixture should write");
    let id =
        Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("fixture UUID should parse");
    let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("fixture date should be valid");

    let destination = paths
        .persist_original(&source, date, id, "pdf")
        .expect("original should persist");

    assert_eq!(
        destination,
        paths
            .originals
            .join("2026")
            .join("07")
            .join(format!("{id}.pdf"))
    );
    assert_eq!(
        fs::read(destination).expect("persisted original should read"),
        content
    );
    assert!(!paths.staging.join(format!("{id}.part")).exists());
}

#[test]
fn persist_original_rejects_unsafe_extensions() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path()).expect("application paths should create");
    let source = directory.path().join("upload");
    fs::write(&source, b"invoice").expect("source fixture should write");
    let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("fixture date should be valid");

    for extension in ["../pdf", r"..\pdf", "pdf/other", r"pdf\other", ".."] {
        let id = Uuid::new_v4();
        let error = paths
            .persist_original(&source, date, id, extension)
            .expect_err("unsafe extension should be rejected");

        assert!(
            matches!(error, AppError::Validation { ref field, .. } if field == "extension"),
            "unexpected error for {extension:?}: {error:?}"
        );
        assert!(!paths.staging.join(format!("{id}.part")).exists());
    }
}

#[test]
fn persist_original_returns_conflict_without_overwriting() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path()).expect("application paths should create");
    let source = directory.path().join("upload.pdf");
    fs::write(&source, b"replacement").expect("source fixture should write");
    let id = Uuid::new_v4();
    let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("fixture date should be valid");
    let destination_directory = paths.originals.join("2026").join("07");
    fs::create_dir_all(&destination_directory).expect("destination directory should create");
    let destination = destination_directory.join(format!("{id}.pdf"));
    fs::write(&destination, b"existing").expect("existing original should write");

    let error = paths
        .persist_original(&source, date, id, "pdf")
        .expect_err("existing destination should conflict");

    assert!(matches!(error, AppError::Conflict { .. }));
    assert_eq!(
        fs::read(destination).expect("existing original should remain readable"),
        b"existing"
    );
    assert!(!paths.staging.join(format!("{id}.part")).exists());
}

#[test]
fn persist_original_cleans_staging_when_the_source_cannot_be_read() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path()).expect("application paths should create");
    let id = Uuid::new_v4();
    let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("fixture date should be valid");

    let error = paths
        .persist_original(directory.path().join("missing.pdf"), date, id, "pdf")
        .expect_err("missing source should fail");

    assert!(matches!(error, AppError::Internal { .. }));
    assert!(!paths.staging.join(format!("{id}.part")).exists());
}

#[test]
fn persist_original_maps_an_existing_staging_file_to_conflict() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path()).expect("application paths should create");
    let source = directory.path().join("upload.pdf");
    fs::write(&source, b"new upload").expect("source fixture should write");
    let id = Uuid::new_v4();
    let staging = paths.staging.join(format!("{id}.part"));
    fs::write(&staging, b"in progress").expect("stale staging fixture should write");
    let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("fixture date should be valid");

    let error = paths
        .persist_original(&source, date, id, "pdf")
        .expect_err("existing staging file should conflict");

    assert!(matches!(error, AppError::Conflict { .. }));
    assert_eq!(
        fs::read(staging).expect("pre-existing staging file should remain"),
        b"in progress"
    );
}

#[test]
fn memory_credential_delete_is_idempotent_and_account_scoped() {
    let credentials = MemoryCredentialStore::default();
    credentials
        .set("account-a", "secret-a")
        .expect("first credential should store");
    credentials
        .set("account-b", "secret-b")
        .expect("second credential should store");

    credentials
        .delete("account-a")
        .expect("credential should delete");
    credentials
        .delete("account-a")
        .expect("deleting a missing credential should succeed");

    assert_eq!(
        credentials
            .get("account-a")
            .expect("credential should read"),
        None
    );
    assert_eq!(
        credentials
            .get("account-b")
            .expect("credential should read"),
        Some("secret-b".to_owned())
    );
}

#[test]
fn credentials_reject_blank_account_ids_and_secrets() {
    let credentials = MemoryCredentialStore::default();

    for account_id in ["", " ", "\t\n"] {
        let get_error = credentials
            .get(account_id)
            .expect_err("blank account ID should be rejected by get");
        assert!(
            matches!(get_error, AppError::Validation { ref field, .. } if field == "account_id")
        );

        let set_error = credentials
            .set(account_id, "secret")
            .expect_err("blank account ID should be rejected by set");
        assert!(
            matches!(set_error, AppError::Validation { ref field, .. } if field == "account_id")
        );

        let delete_error = credentials
            .delete(account_id)
            .expect_err("blank account ID should be rejected by delete");
        assert!(
            matches!(delete_error, AppError::Validation { ref field, .. } if field == "account_id")
        );
    }

    for secret in ["", " ", "\t\n"] {
        let error = credentials
            .set("account-a", secret)
            .expect_err("blank secret should be rejected");
        assert!(matches!(
            error,
            AppError::Validation { ref field, .. } if field == "secret"
        ));
    }
}

#[test]
fn memory_credentials_support_concurrent_account_scoped_access() {
    let credentials = MemoryCredentialStore::default();
    let threads = (0..16)
        .map(|index| {
            let credentials = credentials.clone();
            std::thread::spawn(move || {
                let account_id = format!("account-{index}");
                let secret = format!("secret-{index}");
                credentials
                    .set(&account_id, &secret)
                    .expect("credential should store from a worker");
                assert_eq!(
                    credentials
                        .get(&account_id)
                        .expect("credential should read from a worker"),
                    Some(secret)
                );
            })
        })
        .collect::<Vec<_>>();

    for thread in threads {
        thread.join().expect("credential worker should finish");
    }

    for index in 0..16 {
        assert_eq!(
            credentials
                .get(&format!("account-{index}"))
                .expect("credential should remain readable"),
            Some(format!("secret-{index}"))
        );
    }
}

#[tokio::test]
async fn app_state_accepts_a_memory_credential_store_trait_object() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let paths = AppPaths::create(directory.path()).expect("application paths should create");
    let pool = SqlitePool::connect_lazy("sqlite::memory:")
        .expect("lazy in-memory database pool should create");
    let credentials: Arc<dyn CredentialStore> = Arc::new(MemoryCredentialStore::default());

    let state = AppState::new(pool.clone(), paths.clone(), credentials);
    state
        .credentials()
        .set("account-a", "secret-a")
        .expect("state credential should store");

    assert_eq!(state.paths(), &paths);
    assert_eq!(
        state
            .credentials()
            .get("account-a")
            .expect("state credential should read"),
        Some("secret-a".to_owned())
    );
    assert_eq!(
        state.pool().options().get_max_connections(),
        pool.options().get_max_connections()
    );
}
