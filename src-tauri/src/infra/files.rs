use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use uuid::Uuid;

use crate::domain::error::AppError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    pub root: PathBuf,
    pub originals: PathBuf,
    pub normalized: PathBuf,
    pub exports: PathBuf,
    pub staging: PathBuf,
}

impl AppPaths {
    pub fn create(root: impl AsRef<Path>) -> Result<Self, AppError> {
        let root = root.as_ref().to_path_buf();
        let canonical_root = ensure_application_root(&root)?;
        let paths = Self {
            originals: root.join("originals"),
            normalized: root.join("normalized"),
            exports: root.join("exports"),
            staging: root.join("staging"),
            root,
        };

        for path in [
            &paths.originals,
            &paths.normalized,
            &paths.exports,
            &paths.staging,
        ] {
            ensure_storage_directory(&canonical_root, path)?;
        }
        sync_directory(&paths.root)
            .map_err(|error| internal_error("failed to sync application root", error))?;

        Ok(paths)
    }

    pub fn persist_original(
        &self,
        source: impl AsRef<Path>,
        date: NaiveDate,
        id: Uuid,
        extension: &str,
    ) -> Result<PathBuf, AppError> {
        validate_extension(extension)?;

        let staging_path = self.staging.join(format!("{id}.part"));
        let year_directory = self.originals.join(date.format("%Y").to_string());
        let destination_directory = year_directory.join(date.format("%m").to_string());
        let destination = destination_directory.join(format!("{id}.{extension}"));
        let mut created_staging_file = false;

        let result = (|| {
            let mut source_file = File::open(source.as_ref())
                .map_err(|error| internal_error("failed to open original source", error))?;
            let mut staging_file = open_staging_file(&staging_path).map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    AppError::Conflict {
                        message: "original staging file already exists".to_owned(),
                    }
                } else {
                    internal_error("failed to create staged original", error)
                }
            })?;
            created_staging_file = true;

            io::copy(&mut source_file, &mut staging_file)
                .map_err(|error| internal_error("failed to copy original to staging", error))?;
            staging_file
                .sync_all()
                .map_err(|error| internal_error("failed to sync staged original", error))?;
            drop(staging_file);

            let canonical_originals = fs::canonicalize(&self.originals)
                .map_err(|error| internal_error("failed to resolve original storage", error))?;
            ensure_storage_directory(&canonical_originals, &year_directory)?;
            let canonical_year = fs::canonicalize(&year_directory)
                .map_err(|error| internal_error("failed to resolve original year", error))?;
            ensure_storage_directory(&canonical_year, &destination_directory)?;
            created_staging_file = false;
            promote_staged_original(&staging_path, &destination)?;

            Ok(destination)
        })();

        if result.is_err() && created_staging_file {
            remove_file_durably(&staging_path)?;
        }

        result
    }
}

fn open_staging_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let file = options.open(path)?;
    set_private_file_permissions(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn set_private_file_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn set_private_file_permissions(_file: &File) -> io::Result<()> {
    // Windows relies on the user-profile/AppData ACL. Arbitrary shared roots need ACL hardening.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn set_private_file_permissions(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn ensure_application_root(root: &Path) -> Result<PathBuf, AppError> {
    let mut missing_components = Vec::new();
    let mut existing_ancestor = root.to_path_buf();

    loop {
        match fs::metadata(&existing_ancestor) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => {
                return Err(AppError::Internal {
                    message: "application root path is not a directory".to_owned(),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing_components.push(existing_ancestor.clone());
                existing_ancestor = existing_ancestor
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
            }
            Err(error) => {
                return Err(internal_error("failed to inspect application root", error));
            }
        }
    }

    for path in missing_components.iter().rev() {
        fs::create_dir(path)
            .map_err(|error| internal_error("failed to create application root", error))?;
        set_private_directory_permissions(path)
            .map_err(|error| internal_error("failed to secure application root", error))?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        sync_directory(parent)
            .map_err(|error| internal_error("failed to sync application root parent", error))?;
    }

    set_private_directory_permissions(root)
        .map_err(|error| internal_error("failed to secure application root", error))?;
    fs::canonicalize(root)
        .map_err(|error| internal_error("failed to resolve application root", error))
}

fn ensure_storage_directory(canonical_root: &Path, path: &Path) -> Result<(), AppError> {
    let created = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(AppError::Internal {
                message: "application storage directory must not be a symlink".to_owned(),
            });
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(AppError::Internal {
                message: "application storage path is not a directory".to_owned(),
            });
        }
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .map_err(|error| internal_error("failed to create application storage", error))?;
            true
        }
        Err(error) => {
            return Err(internal_error(
                "failed to inspect application storage",
                error,
            ));
        }
    };

    set_private_directory_permissions(path)
        .map_err(|error| internal_error("failed to secure application storage", error))?;

    let canonical_path = fs::canonicalize(path)
        .map_err(|error| internal_error("failed to resolve application storage", error))?;
    if !canonical_path.starts_with(canonical_root) {
        return Err(AppError::Internal {
            message: "application storage directory escapes its root".to_owned(),
        });
    }
    if created {
        sync_directory(canonical_root)
            .map_err(|error| internal_error("failed to sync storage parent", error))?;
    }

    Ok(())
}

fn promote_staged_original(staging: &Path, destination: &Path) -> Result<(), AppError> {
    promote_staged_original_with_sync(staging, destination, sync_directory)
}

fn promote_staged_original_with_sync(
    staging: &Path,
    destination: &Path,
    mut sync_promoted_directory: impl FnMut(&Path) -> io::Result<()>,
) -> Result<(), AppError> {
    if let Err(error) = rename_staged_original(staging, destination) {
        remove_file_durably(staging)?;
        return Err(error);
    }

    let destination_parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging_parent = staging
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let sync_result = sync_promoted_directory(destination_parent)
        .and_then(|()| sync_promoted_directory(staging_parent));

    if let Err(sync_error) = sync_result {
        return match rename_staged_original(destination, staging) {
            Ok(()) => {
                let destination_sync_result = sync_directory(destination_parent);
                remove_file_durably(staging)?;
                destination_sync_result.map_err(|error| {
                    internal_error("failed to sync original directory after rollback", error)
                })?;

                Err(internal_error(
                    "failed to sync promoted original; promotion was rolled back",
                    sync_error,
                ))
            }
            Err(rollback_error) => Err(AppError::External {
                service: "filesystem_sync".to_owned(),
                retryable: false,
                message: format!(
                    "filesystem sync failed after promoting {}; rollback to {} also failed; manual recovery is required: sync error: {sync_error}; rollback error: {rollback_error}",
                    destination.display(),
                    staging.display(),
                ),
            }),
        };
    }

    Ok(())
}

#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn rename_staged_original(staging: &Path, destination: &Path) -> Result<(), AppError> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    use rustix::io::Errno;

    match renameat_with(CWD, staging, CWD, destination, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(error) if error == Errno::EXIST => Err(AppError::Conflict {
            message: "original destination already exists".to_owned(),
        }),
        Err(error) if error == Errno::NOSYS || error == Errno::INVAL => Err(AppError::Internal {
            message: format!("atomic no-replace promotion is unsupported: {error}"),
        }),
        Err(error) => Err(internal_error("failed to promote staged original", error)),
    }
}

#[cfg(windows)]
fn rename_staged_original(staging: &Path, destination: &Path) -> Result<(), AppError> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let staging_wide = staging
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();

    // Flags zero preserves MoveFileExW's strict no-replace behavior.
    let moved = unsafe { MoveFileExW(staging_wide.as_ptr(), destination_wide.as_ptr(), 0) };
    if moved != 0 {
        return Ok(());
    }

    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == ERROR_ALREADY_EXISTS as i32 || code == ERROR_FILE_EXISTS as i32 => {
            Err(AppError::Conflict {
                message: "original destination already exists".to_owned(),
            })
        }
        _ => Err(internal_error("failed to promote staged original", error)),
    }
}

#[cfg(not(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    windows,
)))]
fn rename_staged_original(_staging: &Path, _destination: &Path) -> Result<(), AppError> {
    Err(AppError::Internal {
        message: "atomic no-replace promotion is unsupported on this platform".to_owned(),
    })
}

fn remove_file_durably(path: &Path) -> Result<(), AppError> {
    remove_file_durably_with_sync(path, sync_directory)
}

fn remove_file_durably_with_sync(
    path: &Path,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), AppError> {
    match fs::remove_file(path) {
        Ok(()) => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            sync_parent(parent)
                .map_err(|error| internal_error("failed to sync cleaned file parent", error))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(internal_error("failed to clean staged original", error)),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // std cannot portably open/sync Windows directories; this is the best available no-op.
    Ok(())
}

fn validate_extension(extension: &str) -> Result<(), AppError> {
    if extension.is_empty()
        || !extension
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Err(AppError::validation(
            "extension",
            "extension must contain only ASCII letters and digits",
        ));
    }

    Ok(())
}

fn internal_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Internal {
        message: format!("{context}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs::{self, File};
    use std::io::{self, Write};
    use std::sync::{Arc, Barrier};

    #[cfg(unix)]
    use super::open_staging_file;
    use super::{
        promote_staged_original, promote_staged_original_with_sync, remove_file_durably_with_sync,
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
}
