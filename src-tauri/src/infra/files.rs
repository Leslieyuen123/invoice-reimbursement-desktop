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
            fs::create_dir_all(path).map_err(|error| AppError::Internal {
                message: format!("failed to create application storage: {error}"),
            })?;
        }

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
        let destination_directory = self
            .originals
            .join(date.format("%Y").to_string())
            .join(date.format("%m").to_string());
        let destination = destination_directory.join(format!("{id}.{extension}"));
        let mut created_staging_file = false;

        let result = (|| {
            let mut source_file = File::open(source.as_ref())
                .map_err(|error| internal_error("failed to open original source", error))?;
            let mut staging_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staging_path)
                .map_err(|error| internal_error("failed to create staged original", error))?;
            created_staging_file = true;

            io::copy(&mut source_file, &mut staging_file)
                .map_err(|error| internal_error("failed to copy original to staging", error))?;
            staging_file
                .sync_all()
                .map_err(|error| internal_error("failed to sync staged original", error))?;
            drop(staging_file);

            fs::create_dir_all(&destination_directory)
                .map_err(|error| internal_error("failed to create original directory", error))?;
            promote_staged_original(&staging_path, &destination)?;

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

fn promote_staged_original(staging: &Path, destination: &Path) -> Result<(), AppError> {
    let staging = tempfile::TempPath::try_from_path(staging.to_path_buf())
        .map_err(|error| internal_error("failed to manage staged original", error))?;

    match staging.persist_noclobber(destination) {
        Ok(()) => Ok(()),
        Err(error) => {
            let tempfile::PathPersistError { error, path } = error;
            drop(path);

            if error.kind() == io::ErrorKind::AlreadyExists {
                Err(AppError::Conflict {
                    message: "original destination already exists".to_owned(),
                })
            } else {
                Err(internal_error("failed to promote staged original", error))
            }
        }
    }
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

    use super::promote_staged_original;
    use crate::domain::error::AppError;

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
