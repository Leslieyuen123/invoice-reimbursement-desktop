use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

use crate::db::batches::BatchRepository;
use crate::domain::error::AppError;
use crate::infra::files::{AppPaths, sync_directory};

const INTERRUPTED_MESSAGE: &str = "application_shutdown_interrupted";
const RECOVERY_MARKER: &str = ".invoice-export-recovery.json";
const MAX_MARKER_BYTES: usize = 4 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryMarker {
    operation_id: String,
    batch_id: String,
    staging_component: String,
    final_component: String,
    exported_at: String,
    state: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportRecoveryReport {
    pub rolled_back: u64,
    pub preserved: u64,
}

#[derive(Clone)]
pub(crate) struct ExportJournal {
    pool: SqlitePool,
}

#[derive(FromRow)]
struct PendingExportRow {
    operation_id: String,
    batch_id: String,
    staging_component: String,
    final_component: String,
    exported_at: String,
    state: String,
}

impl ExportJournal {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub(crate) async fn create(
        &self,
        operation_id: Uuid,
        batch_id: Uuid,
        staging_component: &str,
        final_component: &str,
        exported_at: DateTime<Utc>,
    ) -> Result<(), AppError> {
        validate_component(staging_component, Some("export-"))?;
        validate_component(final_component, None)?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO pending_exports (
                operation_id, batch_id, staging_component, final_component, exported_at, state,
                interrupted, last_error, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, 'generating', 0, NULL, ?, ?)",
        )
        .bind(operation_id.to_string())
        .bind(batch_id.to_string())
        .bind(staging_component)
        .bind(final_component)
        .bind(exported_at.to_rfc3339())
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|_| internal_error("failed to persist export recovery journal"))
    }

    pub(crate) async fn advance(
        &self,
        operation_id: Uuid,
        state: &'static str,
    ) -> Result<(), AppError> {
        if !matches!(state, "published" | "committed") {
            return Err(internal_error("invalid export recovery transition"));
        }
        let rows = sqlx::query(
            "UPDATE pending_exports
             SET state = ?, last_error = NULL, updated_at = ?
             WHERE operation_id = ?",
        )
        .bind(state)
        .bind(Utc::now().to_rfc3339())
        .bind(operation_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(|_| internal_error("failed to advance export recovery journal"))?
        .rows_affected();
        if rows == 0 {
            return Err(internal_error("export recovery journal was missing"));
        }
        Ok(())
    }

    pub(crate) async fn finish(&self, operation_id: Uuid) -> Result<(), AppError> {
        sqlx::query("DELETE FROM pending_exports WHERE operation_id = ?")
            .bind(operation_id.to_string())
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|_| internal_error("failed to clear export recovery journal"))
    }

    pub(crate) async fn reconcile(
        &self,
        paths: &AppPaths,
    ) -> Result<ExportRecoveryReport, AppError> {
        let pending = sqlx::query_as::<_, PendingExportRow>(
            "SELECT operation_id, batch_id, staging_component, final_component, exported_at, state
             FROM pending_exports ORDER BY created_at ASC, operation_id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| internal_error("failed to load export recovery journal"))?;
        let mut report = ExportRecoveryReport::default();
        let mut first_error = None;
        for row in pending {
            let operation_id = match Uuid::parse_str(&row.operation_id) {
                Ok(value) => value,
                Err(_) => {
                    let error = recovery_error("journal contains an invalid operation identity");
                    self.record_error(&row.operation_id, &error).await?;
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            match self.reconcile_one(paths, operation_id, &row).await {
                Ok(RecoveryDisposition::RolledBack) => report.rolled_back += 1,
                Ok(RecoveryDisposition::Preserved) => report.preserved += 1,
                Err(error) => {
                    self.record_error(&row.operation_id, &error).await?;
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Err(error) = self.reconcile_orphan_staging(paths, &mut report).await {
            tracing::error!(%error, "orphan export staging recovery failed");
            first_error.get_or_insert(error);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(report),
        }
    }

    async fn reconcile_orphan_staging(
        &self,
        paths: &AppPaths,
        report: &mut ExportRecoveryReport,
    ) -> Result<(), AppError> {
        let entries = std::fs::read_dir(&paths.staging)
            .map_err(|_| recovery_error("failed to inspect export staging recovery storage"))?;
        for entry in entries {
            let entry = entry
                .map_err(|_| recovery_error("failed to inspect export staging recovery entry"))?;
            let Some(component) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !component.starts_with("export-") {
                continue;
            }
            let marker = read_recovery_marker(&paths.staging, &component)?
                .ok_or_else(|| recovery_error("orphan export staging has no recovery marker"))?;
            validate_marker(&marker, &component)?;
            let operation_id = Uuid::parse_str(&marker.operation_id)
                .map_err(|_| recovery_error("export recovery marker has an invalid identity"))?;
            let journal_exists = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM pending_exports
                 WHERE operation_id = ? OR staging_component = ?",
            )
            .bind(operation_id.to_string())
            .bind(&component)
            .fetch_one(&self.pool)
            .await
            .map_err(|_| internal_error("failed to inspect export recovery journal"))?
                != 0;
            if journal_exists {
                continue;
            }
            remove_recovery_directory(&paths.staging, &component, operation_id, "orphan-staging")?;
            report.rolled_back += 1;
        }
        Ok(())
    }

    pub(crate) async fn reconcile_operation(
        &self,
        paths: &AppPaths,
        operation_id: Uuid,
    ) -> Result<(), AppError> {
        let row = sqlx::query_as::<_, PendingExportRow>(
            "SELECT operation_id, batch_id, staging_component, final_component, exported_at, state
             FROM pending_exports WHERE operation_id = ?",
        )
        .bind(operation_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| internal_error("failed to load export recovery journal"))?;
        let Some(row) = row else {
            return Ok(());
        };
        match self.reconcile_one(paths, operation_id, &row).await {
            Ok(_) => Ok(()),
            Err(error) => {
                self.record_error(&row.operation_id, &error).await?;
                Err(error)
            }
        }
    }

    async fn reconcile_one(
        &self,
        paths: &AppPaths,
        operation_id: Uuid,
        row: &PendingExportRow,
    ) -> Result<RecoveryDisposition, AppError> {
        let batch_id = Uuid::parse_str(&row.batch_id)
            .map_err(|_| recovery_error("journal contains an invalid batch identity"))?;
        let exported_at = DateTime::parse_from_rfc3339(&row.exported_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|_| recovery_error("journal contains an invalid export timestamp"))?;
        if !matches!(row.state.as_str(), "generating" | "published" | "committed") {
            return Err(recovery_error("journal contains an invalid export state"));
        }
        validate_component(&row.staging_component, Some("export-"))?;
        validate_component(&row.final_component, None)?;

        let committed = match BatchRepository::new(self.pool.clone()).get(batch_id).await {
            Ok(batch) => batch
                .last_exported_at
                .is_some_and(|value| value == exported_at),
            Err(AppError::NotFound { .. }) => false,
            Err(error) => return Err(error),
        };

        if committed {
            verify_recovery_directory(&paths.exports, &row.final_component)?;
            if let Some(marker) = read_recovery_marker(&paths.exports, &row.final_component)? {
                if !marker_matches_pending(&marker, row, operation_id)? {
                    return Err(recovery_error(
                        "committed export package has a mismatched recovery marker",
                    ));
                }
                clear_recovery_marker(&paths.exports, &row.final_component)?;
            }
            remove_owned_staging_directory(paths, row, operation_id)?;
            self.finish(operation_id).await?;
            return Ok(RecoveryDisposition::Preserved);
        }

        remove_owned_staging_directory(paths, row, operation_id)?;
        let owns_final = read_recovery_marker(&paths.exports, &row.final_component)?
            .map(|marker| marker_matches_pending(&marker, row, operation_id))
            .transpose()?
            .unwrap_or(false);
        if owns_final {
            remove_recovery_directory(&paths.exports, &row.final_component, operation_id, "final")?;
        }
        self.finish(operation_id).await?;
        Ok(RecoveryDisposition::RolledBack)
    }

    async fn record_error(&self, operation_id: &str, error: &AppError) -> Result<(), AppError> {
        let message = error.to_string().chars().take(512).collect::<String>();
        sqlx::query(
            "UPDATE pending_exports SET interrupted = 1, last_error = ?, updated_at = ?
             WHERE operation_id = ?",
        )
        .bind(message)
        .bind(Utc::now().to_rfc3339())
        .bind(operation_id)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|_| internal_error("failed to record export recovery error"))
    }
}

pub(crate) fn write_generation_marker(
    directory: &Path,
    operation_id: Uuid,
    batch_id: Uuid,
    staging_component: &str,
    final_component: &str,
    exported_at: DateTime<Utc>,
) -> Result<(), AppError> {
    validate_component(staging_component, Some("export-"))?;
    validate_component(final_component, None)?;
    let bytes = serde_json::to_vec(&RecoveryMarker {
        operation_id: operation_id.to_string(),
        batch_id: batch_id.to_string(),
        staging_component: staging_component.to_owned(),
        final_component: final_component.to_owned(),
        exported_at: exported_at.to_rfc3339(),
        state: "generating".to_owned(),
    })
    .map_err(|_| internal_error("failed to encode export recovery marker"))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(directory.join(RECOVERY_MARKER))
        .map_err(|_| internal_error("failed to create export recovery marker"))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| internal_error("failed to persist export recovery marker"))?;
    sync_directory(directory)
        .map_err(|_| internal_error("failed to sync export recovery marker directory"))
}

pub(crate) fn clear_committed_marker(
    root: &Path,
    final_component: &str,
    operation_id: Uuid,
) -> Result<(), AppError> {
    match read_recovery_marker(root, final_component)? {
        Some(marker) if marker.operation_id == operation_id.to_string() => {
            clear_recovery_marker(root, final_component)
        }
        Some(_) => Err(recovery_error(
            "committed export package has a mismatched recovery marker",
        )),
        None => Ok(()),
    }
}

fn validate_marker(marker: &RecoveryMarker, staging_component: &str) -> Result<(), AppError> {
    Uuid::parse_str(&marker.operation_id)
        .map_err(|_| recovery_error("export recovery marker has an invalid identity"))?;
    Uuid::parse_str(&marker.batch_id)
        .map_err(|_| recovery_error("export recovery marker has an invalid batch identity"))?;
    DateTime::parse_from_rfc3339(&marker.exported_at)
        .map_err(|_| recovery_error("export recovery marker has an invalid timestamp"))?;
    validate_component(&marker.staging_component, Some("export-"))?;
    validate_component(&marker.final_component, None)?;
    if marker.state != "generating" || marker.staging_component != staging_component {
        return Err(recovery_error("export recovery marker is inconsistent"));
    }
    Ok(())
}

fn marker_matches_pending(
    marker: &RecoveryMarker,
    row: &PendingExportRow,
    operation_id: Uuid,
) -> Result<bool, AppError> {
    validate_marker(marker, &row.staging_component)?;
    Ok(marker.operation_id == operation_id.to_string()
        && marker.batch_id == row.batch_id
        && marker.staging_component == row.staging_component
        && marker.final_component == row.final_component
        && marker.exported_at == row.exported_at)
}

fn remove_owned_staging_directory(
    paths: &AppPaths,
    row: &PendingExportRow,
    operation_id: Uuid,
) -> Result<(), AppError> {
    let marker = match read_recovery_marker(&paths.staging, &row.staging_component)? {
        Some(marker) => marker,
        None if recovery_directory_exists(&paths.staging, &row.staging_component)? => {
            return Err(recovery_error(
                "export staging directory has no recovery marker",
            ));
        }
        None => return Ok(()),
    };
    if !marker_matches_pending(&marker, row, operation_id)? {
        return Err(recovery_error(
            "export staging directory has a mismatched recovery marker",
        ));
    }
    remove_recovery_directory(
        &paths.staging,
        &row.staging_component,
        operation_id,
        "staging",
    )
}

#[cfg(unix)]
fn recovery_directory_exists(root: &Path, component: &str) -> Result<bool, AppError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, statat};
    use rustix::io::Errno;

    let component = validate_component(component, None)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root = rustix::fs::open(root, flags, Mode::empty())
        .map(File::from)
        .map_err(|_| recovery_error("failed to anchor export recovery storage"))?;
    match statat(&root, &component, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory => {
            Ok(true)
        }
        Ok(_) => Err(recovery_error(
            "export recovery target is not a safe directory",
        )),
        Err(error) if error == Errno::NOENT => Ok(false),
        Err(_) => Err(recovery_error(
            "failed to inspect export recovery directory",
        )),
    }
}

#[cfg(not(unix))]
fn recovery_directory_exists(_root: &Path, _component: &str) -> Result<bool, AppError> {
    Err(recovery_error(
        "safe export recovery is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn read_recovery_marker(root: &Path, component: &str) -> Result<Option<RecoveryMarker>, AppError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, statat};
    use rustix::io::Errno;

    let component = validate_component(component, None)?;
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root = rustix::fs::open(root, directory_flags, Mode::empty())
        .map(File::from)
        .map_err(|_| recovery_error("failed to anchor export recovery storage"))?;
    let directory = match openat(&root, &component, directory_flags, Mode::empty()) {
        Ok(directory) => File::from(directory),
        Err(error) if error == Errno::NOENT => return Ok(None),
        Err(_) => return Err(recovery_error("export recovery directory is not safe")),
    };
    let metadata = match statat(&directory, RECOVERY_MARKER, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => metadata,
        Err(error) if error == Errno::NOENT => return Ok(None),
        Err(_) => return Err(recovery_error("failed to inspect export recovery marker")),
    };
    let marker_length = usize::try_from(metadata.st_size)
        .map_err(|_| recovery_error("export recovery marker has an invalid size"))?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
        || marker_length > MAX_MARKER_BYTES
    {
        return Err(recovery_error("export recovery marker is not a safe file"));
    }
    let file_flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let mut file = openat(&directory, RECOVERY_MARKER, file_flags, Mode::empty())
        .map(File::from)
        .map_err(|_| recovery_error("failed to open export recovery marker"))?;
    let mut bytes = Vec::with_capacity(marker_length);
    file.read_to_end(&mut bytes)
        .map_err(|_| recovery_error("failed to read export recovery marker"))?;
    if bytes.len() > MAX_MARKER_BYTES {
        return Err(recovery_error("export recovery marker is too large"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| recovery_error("export recovery marker is invalid"))
}

#[cfg(not(unix))]
fn read_recovery_marker(
    _root: &Path,
    _component: &str,
) -> Result<Option<RecoveryMarker>, AppError> {
    Err(recovery_error(
        "safe export recovery is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn clear_recovery_marker(root: &Path, component: &str) -> Result<(), AppError> {
    use rustix::fs::{AtFlags, Mode, OFlags, fsync, openat, unlinkat};
    use rustix::io::Errno;

    let component = validate_component(component, None)?;
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root = rustix::fs::open(root, directory_flags, Mode::empty())
        .map(File::from)
        .map_err(|_| recovery_error("failed to anchor export recovery storage"))?;
    let directory = openat(&root, &component, directory_flags, Mode::empty())
        .map(File::from)
        .map_err(|_| recovery_error("failed to anchor committed export package"))?;
    match unlinkat(&directory, RECOVERY_MARKER, AtFlags::empty()) {
        Ok(()) => {
            fsync(&directory).map_err(|_| recovery_error("failed to sync committed export package"))
        }
        Err(error) if error == Errno::NOENT => Ok(()),
        Err(_) => Err(recovery_error(
            "failed to clear committed export recovery marker",
        )),
    }
}

#[cfg(not(unix))]
fn clear_recovery_marker(_root: &Path, _component: &str) -> Result<(), AppError> {
    Err(recovery_error(
        "safe export recovery is unavailable on this platform",
    ))
}

enum RecoveryDisposition {
    RolledBack,
    Preserved,
}

pub(crate) async fn mark_pending_exports_interrupted(pool: &SqlitePool) -> Result<u64, AppError> {
    sqlx::query(
        "UPDATE pending_exports
         SET interrupted = 1, last_error = ?, updated_at = ?
         WHERE interrupted = 0",
    )
    .bind(INTERRUPTED_MESSAGE)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
    .map_err(|_| internal_error("failed to mark pending exports interrupted"))
}

fn validate_component(value: &str, required_prefix: Option<&str>) -> Result<OsString, AppError> {
    if required_prefix.is_some_and(|prefix| !value.starts_with(prefix)) {
        return Err(recovery_error(
            "journal contains an invalid storage component",
        ));
    }
    let mut components = Path::new(value).components();
    let Some(Component::Normal(component)) = components.next() else {
        return Err(recovery_error(
            "journal contains an invalid storage component",
        ));
    };
    if components.next().is_some() || component.is_empty() {
        return Err(recovery_error(
            "journal contains an invalid storage component",
        ));
    }
    Ok(component.to_os_string())
}

fn verify_recovery_directory(root: &Path, component: &str) -> Result<(), AppError> {
    let component = validate_component(component, None)?;
    let root = root
        .canonicalize()
        .map_err(|_| recovery_error("failed to anchor export recovery storage"))?;
    let candidate = root.join(component);
    let metadata = candidate
        .symlink_metadata()
        .map_err(|_| recovery_error("committed export package is missing"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(recovery_error(
            "committed export package is not a safe directory",
        ));
    }
    let canonical = candidate
        .canonicalize()
        .map_err(|_| recovery_error("failed to verify committed export package"))?;
    if canonical.parent() != Some(root.as_path()) {
        return Err(recovery_error(
            "committed export package escaped application storage",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn remove_recovery_directory(
    root: &Path,
    component: &str,
    operation_id: Uuid,
    kind: &str,
) -> Result<(), AppError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, fsync, renameat, statat};
    use rustix::io::Errno;

    let component = validate_component(component, None)?;
    let quarantine = OsString::from(format!(".export-recovery-{operation_id}-{kind}"));
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root_fd = rustix::fs::open(root, flags, Mode::empty())
        .map(File::from)
        .map_err(|_| recovery_error("failed to anchor export recovery storage"))?;

    purge_quarantine(root, &root_fd, &quarantine)?;
    let metadata = match statat(&root_fd, &component, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => metadata,
        Err(error) if error == Errno::NOENT => return Ok(()),
        Err(_) => {
            return Err(recovery_error(
                "failed to inspect export recovery directory",
            ));
        }
    };
    if FileType::from_raw_mode(metadata.st_mode) != FileType::Directory {
        return Err(recovery_error(
            "export recovery target is not a safe directory",
        ));
    }
    renameat(&root_fd, &component, &root_fd, &quarantine)
        .map_err(|_| recovery_error("failed to isolate export recovery directory"))?;
    fsync(&root_fd).map_err(|_| recovery_error("failed to sync export recovery storage"))?;
    purge_quarantine(root, &root_fd, &quarantine)
}

#[cfg(unix)]
fn purge_quarantine(root: &Path, root_fd: &File, quarantine: &OsStr) -> Result<(), AppError> {
    use rustix::fs::{AtFlags, FileType, fsync, statat};
    use rustix::io::Errno;

    let metadata = match statat(root_fd, quarantine, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => metadata,
        Err(error) if error == Errno::NOENT => return Ok(()),
        Err(_) => {
            return Err(recovery_error(
                "failed to inspect isolated export directory",
            ));
        }
    };
    if FileType::from_raw_mode(metadata.st_mode) != FileType::Directory {
        return Err(recovery_error(
            "isolated export target is not a safe directory",
        ));
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|_| recovery_error("failed to resolve export recovery storage"))?;
    let quarantine_path = canonical_root.join(quarantine);
    let canonical_quarantine = quarantine_path
        .canonicalize()
        .map_err(|_| recovery_error("failed to resolve isolated export directory"))?;
    if canonical_quarantine.parent() != Some(canonical_root.as_path()) {
        return Err(recovery_error(
            "isolated export directory escaped application storage",
        ));
    }
    std::fs::remove_dir_all(&canonical_quarantine)
        .map_err(|_| recovery_error("failed to remove isolated export directory"))?;
    fsync(root_fd).map_err(|_| recovery_error("failed to sync export recovery storage"))
}

#[cfg(not(unix))]
fn remove_recovery_directory(
    _root: &Path,
    _component: &str,
    _operation_id: Uuid,
    _kind: &str,
) -> Result<(), AppError> {
    Err(recovery_error(
        "safe export recovery is unavailable on this platform",
    ))
}

fn recovery_error(message: &str) -> AppError {
    AppError::External {
        service: "export_recovery".to_owned(),
        retryable: false,
        message: message.to_owned(),
    }
}

fn internal_error(message: &str) -> AppError {
    AppError::Internal {
        message: message.to_owned(),
    }
}
