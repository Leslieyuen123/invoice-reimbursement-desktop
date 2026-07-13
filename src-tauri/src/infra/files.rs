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
        fs::create_dir_all(&root)
            .map_err(|error| internal_error("failed to create application root", error))?;
        set_private_directory_permissions(&root)
            .map_err(|error| internal_error("failed to secure application root", error))?;
        let canonical_root = fs::canonicalize(&root)
            .map_err(|error| internal_error("failed to resolve application root", error))?;
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
            promote_staged_original(&staging_path, &destination)?;
            sync_directory(&destination_directory)
                .map_err(|error| internal_error("failed to sync original directory", error))?;
            sync_directory(&self.staging)
                .map_err(|error| internal_error("failed to sync staging directory", error))?;

            Ok(destination)
        })();

        if result.is_err() && created_staging_file {
            match fs::remove_file(&staging_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(internal_error("failed to clean staged original", error));
                }
            }
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
    if let Err(error) = rename_staged_original(staging, destination) {
        remove_staging_file(staging)?;
        return Err(error);
    }

    match staging.try_exists() {
        Ok(false) => Ok(()),
        Ok(true) => Err(AppError::Internal {
            message: "staged original remained after promotion".to_owned(),
        }),
        Err(error) => Err(internal_error(
            "failed to verify staged original promotion",
            error,
        )),
    }
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
    match fs::rename(staging, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(AppError::Conflict {
            message: "original destination already exists".to_owned(),
        }),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            match destination.try_exists() {
                Ok(true) => Err(AppError::Conflict {
                    message: "original destination already exists".to_owned(),
                }),
                Ok(false) => Err(internal_error("failed to promote staged original", error)),
                Err(inspect_error) => Err(internal_error(
                    "failed to inspect original destination",
                    inspect_error,
                )),
            }
        }
        Err(error) => Err(internal_error("failed to promote staged original", error)),
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

fn remove_staging_file(path: &Path) -> Result<(), AppError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
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
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::{Arc, Barrier};

    #[cfg(unix)]
    use super::open_staging_file;
    use super::promote_staged_original;
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
