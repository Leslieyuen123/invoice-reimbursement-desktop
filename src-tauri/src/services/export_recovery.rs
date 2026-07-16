use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use uuid::Uuid;

use crate::domain::error::AppError;
use crate::infra::files::{AppPaths, sync_directory};

const INTERRUPTED_MESSAGE: &str = "application_shutdown_interrupted";
const RECOVERY_MARKER: &str = ".invoice-export-recovery.json";
const MAX_MARKER_BYTES: usize = 4 * 1024;
const MAX_EXPORT_SCAN_ENTRIES: usize = 8 * 1024;
const MAX_DISCOVERED_MARKERS: usize = 1024;
const REBUILT_INDEX_MESSAGE: &str = "export_recovery_index_rebuilt";
const INDETERMINATE_MESSAGE: &str = "export_commit_outcome_indeterminate";

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

#[derive(Debug, FromRow)]
struct PendingExportRow {
    operation_id: String,
    batch_id: String,
    staging_component: String,
    final_component: String,
    exported_at: String,
    state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingExportIdentity {
    pub(crate) operation_id: Uuid,
    pub(crate) batch_id: Uuid,
    pub(crate) staging_component: String,
    pub(crate) final_component: String,
    pub(crate) exported_at: DateTime<Utc>,
}

impl PendingExportIdentity {
    pub(crate) fn new(
        operation_id: Uuid,
        batch_id: Uuid,
        staging_component: String,
        final_component: String,
        exported_at: DateTime<Utc>,
    ) -> Self {
        Self {
            operation_id,
            batch_id,
            staging_component,
            final_component,
            exported_at,
        }
    }

    fn from_row(row: &PendingExportRow) -> Result<Self, AppError> {
        if !matches!(row.state.as_str(), "generating" | "published" | "committed") {
            return Err(recovery_error("journal contains an invalid export state"));
        }
        let operation_id = Uuid::parse_str(&row.operation_id)
            .map_err(|_| recovery_error("journal contains an invalid operation identity"))?;
        let batch_id = Uuid::parse_str(&row.batch_id)
            .map_err(|_| recovery_error("journal contains an invalid batch identity"))?;
        let exported_at = DateTime::parse_from_rfc3339(&row.exported_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|_| recovery_error("journal contains an invalid export timestamp"))?;
        validate_component(&row.staging_component, Some("export-"))?;
        validate_component(&row.final_component, None)?;
        Ok(Self::new(
            operation_id,
            batch_id,
            row.staging_component.clone(),
            row.final_component.clone(),
            exported_at,
        ))
    }

    fn matches_row(&self, row: &PendingExportRow) -> bool {
        row.operation_id == self.operation_id.to_string()
            && row.batch_id == self.batch_id.to_string()
            && row.staging_component == self.staging_component
            && row.final_component == self.final_component
            && row.exported_at == self.exported_at.to_rfc3339()
    }
}

#[derive(Debug)]
pub(crate) enum ExportCommitOutcome {
    ConfirmedCommitted,
    ConfirmedUncommitted,
    Indeterminate(AppError),
}

#[derive(FromRow)]
struct CommitOutcomeRow {
    operation_id: String,
    batch_id: String,
    staging_component: String,
    final_component: String,
    exported_at: String,
    state: String,
    persisted_batch_id: Option<String>,
    batch_status: Option<String>,
    last_exported_at: Option<String>,
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
        let rows = sqlx::query(
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
        .map_err(|_| internal_error("failed to persist export recovery journal"))?
        .rows_affected();
        require_single_journal_row(rows, "failed to persist export recovery journal")
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
        require_single_journal_row(rows, "export recovery journal was missing")
    }

    pub(crate) async fn finish(&self, operation_id: Uuid) -> Result<(), AppError> {
        let rows = sqlx::query("DELETE FROM pending_exports WHERE operation_id = ?")
            .bind(operation_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|_| internal_error("failed to clear export recovery journal"))?
            .rows_affected();
        require_single_journal_row(rows, "export recovery journal was missing during cleanup")
    }

    pub(crate) async fn classify_commit_outcome(
        &self,
        expected: &PendingExportIdentity,
    ) -> ExportCommitOutcome {
        let mut connection = match self.pool.acquire().await {
            Ok(connection) => connection,
            Err(_) => {
                return ExportCommitOutcome::Indeterminate(outcome_error(
                    "failed to acquire a fresh export outcome connection",
                ));
            }
        };
        let row = match sqlx::query_as::<_, CommitOutcomeRow>(
            "SELECT p.operation_id, p.batch_id, p.staging_component, p.final_component,
                    p.exported_at, p.state, b.id AS persisted_batch_id,
                    b.status AS batch_status, b.last_exported_at
             FROM pending_exports p
             LEFT JOIN batches b ON b.id = p.batch_id
             WHERE p.operation_id = ?",
        )
        .bind(expected.operation_id.to_string())
        .fetch_optional(&mut *connection)
        .await
        {
            Ok(Some(row)) => row,
            Ok(None) => {
                return ExportCommitOutcome::Indeterminate(outcome_error(
                    "export outcome journal is missing",
                ));
            }
            Err(_) => {
                return ExportCommitOutcome::Indeterminate(outcome_error(
                    "failed to read durable export outcome",
                ));
            }
        };

        let exported_at = expected.exported_at.to_rfc3339();
        if row.operation_id != expected.operation_id.to_string()
            || row.batch_id != expected.batch_id.to_string()
            || row.staging_component != expected.staging_component
            || row.final_component != expected.final_component
            || row.exported_at != exported_at
            || !matches!(row.state.as_str(), "generating" | "published" | "committed")
        {
            return ExportCommitOutcome::Indeterminate(outcome_error(
                "export outcome journal does not match the active operation",
            ));
        }

        if row.persisted_batch_id.as_deref() != Some(row.batch_id.as_str()) {
            return ExportCommitOutcome::Indeterminate(outcome_error(
                "export outcome batch is missing",
            ));
        }

        let batch_has_current_export = row.batch_status.as_deref() == Some("exported")
            && row.last_exported_at.as_deref() == Some(exported_at.as_str());
        match (row.state.as_str(), batch_has_current_export) {
            ("published" | "committed", true) => ExportCommitOutcome::ConfirmedCommitted,
            ("generating" | "published", false) => ExportCommitOutcome::ConfirmedUncommitted,
            _ => ExportCommitOutcome::Indeterminate(outcome_error(
                "export journal and batch outcome disagree",
            )),
        }
    }

    pub(crate) async fn preserve_committed(
        &self,
        paths: &AppPaths,
        expected: &PendingExportIdentity,
    ) -> Result<(), AppError> {
        verify_recovery_directory(&paths.exports, &expected.final_component)?;
        if let Some(marker) = read_recovery_marker(&paths.exports, &expected.final_component)?
            && !marker_matches_identity(&marker, expected)?
        {
            return Err(recovery_error(
                "committed export package has a mismatched recovery marker",
            ));
        }
        let owns_staging = owned_recovery_directory_exists(
            &paths.staging,
            &expected.staging_component,
            expected,
            "export staging directory has no recovery marker",
            "export staging directory has a mismatched recovery marker",
            false,
        )?;
        if owns_staging {
            remove_recovery_directory(
                &paths.staging,
                &expected.staging_component,
                expected.operation_id,
                "staging",
            )?;
        }
        clear_committed_marker(&paths.exports, &expected.final_component, expected)?;
        self.finish(expected.operation_id).await
    }

    pub(crate) async fn rollback_uncommitted(
        &self,
        paths: &AppPaths,
        expected: &PendingExportIdentity,
    ) -> Result<(), AppError> {
        self.rollback_uncommitted_with_policy(paths, expected, false)
            .await
    }

    async fn rollback_uncommitted_with_policy(
        &self,
        paths: &AppPaths,
        expected: &PendingExportIdentity,
        allow_unmarked_existing_final: bool,
    ) -> Result<(), AppError> {
        let owns_staging = owned_recovery_directory_exists(
            &paths.staging,
            &expected.staging_component,
            expected,
            "export staging directory has no recovery marker",
            "export staging directory has a mismatched recovery marker",
            false,
        )?;
        let owns_final = owned_recovery_directory_exists(
            &paths.exports,
            &expected.final_component,
            expected,
            "export package has no recovery marker",
            "export package has a mismatched recovery marker",
            allow_unmarked_existing_final,
        )?;
        if owns_staging {
            remove_recovery_directory(
                &paths.staging,
                &expected.staging_component,
                expected.operation_id,
                "staging",
            )?;
        }
        if owns_final {
            remove_recovery_directory(
                &paths.exports,
                &expected.final_component,
                expected.operation_id,
                "final",
            )?;
        }
        self.finish(expected.operation_id).await
    }

    pub(crate) async fn record_indeterminate(
        &self,
        paths: &AppPaths,
        expected: &PendingExportIdentity,
    ) -> Result<(), AppError> {
        let operation_id = expected.operation_id.to_string();
        let rows = self
            .update_error_message(&operation_id, INDETERMINATE_MESSAGE)
            .await?;
        if rows == 1 {
            return Ok(());
        }
        if rows != 0 {
            return Err(internal_error(
                "export recovery journal update affected an invalid row count",
            ));
        }

        let marker = read_final_recovery_marker(&paths.exports, &expected.final_component)?
            .ok_or_else(|| recovery_error("committed export package has no recovery marker"))?;
        if marker != *expected {
            return Err(recovery_error(
                "committed export package has a mismatched recovery marker",
            ));
        }
        self.rebuild_missing_index(&marker, INDETERMINATE_MESSAGE, true)
            .await
    }

    pub(crate) async fn reconcile(
        &self,
        paths: &AppPaths,
    ) -> Result<ExportRecoveryReport, AppError> {
        let discovered = discover_final_recovery_markers(&paths.exports)?;
        for marker in &discovered {
            self.rebuild_missing_index(marker, REBUILT_INDEX_MESSAGE, false)
                .await?;
        }
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
                    self.record_unrecoverable_row_error(&row.operation_id, &error)
                        .await?;
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            match self.reconcile_one(paths, operation_id, &row).await {
                Ok(RecoveryDisposition::RolledBack) => report.rolled_back += 1,
                Ok(RecoveryDisposition::Preserved) => report.preserved += 1,
                Err(error) => {
                    self.record_error(paths, &row, &error).await?;
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
                self.record_error(paths, &row, &error).await?;
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
        let expected = PendingExportIdentity::from_row(row)?;
        if expected.operation_id != operation_id {
            return Err(recovery_error("journal operation identity changed"));
        }
        match self.classify_commit_outcome(&expected).await {
            ExportCommitOutcome::ConfirmedCommitted => {
                self.preserve_committed(paths, &expected).await?;
                Ok(RecoveryDisposition::Preserved)
            }
            ExportCommitOutcome::ConfirmedUncommitted => {
                self.rollback_uncommitted_with_policy(paths, &expected, true)
                    .await?;
                Ok(RecoveryDisposition::RolledBack)
            }
            ExportCommitOutcome::Indeterminate(error) => Err(error),
        }
    }

    async fn record_error(
        &self,
        paths: &AppPaths,
        row: &PendingExportRow,
        error: &AppError,
    ) -> Result<(), AppError> {
        let message = error.to_string().chars().take(512).collect::<String>();
        let rows = self
            .update_error_message(&row.operation_id, &message)
            .await?;
        if rows == 1 {
            return Ok(());
        }
        if rows != 0 {
            return Err(internal_error(
                "export recovery journal update affected an invalid row count",
            ));
        }

        let expected = PendingExportIdentity::from_row(row)?;
        let marker = read_final_recovery_marker(&paths.exports, &expected.final_component)?
            .ok_or_else(|| recovery_error("export recovery journal disappeared"))?;
        if marker != expected {
            return Err(recovery_error(
                "export recovery journal disappeared behind a mismatched marker",
            ));
        }
        self.rebuild_missing_index(&marker, &message, true).await
    }

    async fn record_unrecoverable_row_error(
        &self,
        operation_id: &str,
        error: &AppError,
    ) -> Result<(), AppError> {
        let message = error.to_string().chars().take(512).collect::<String>();
        let rows = self.update_error_message(operation_id, &message).await?;
        require_single_journal_row(rows, "export recovery journal disappeared")
    }

    async fn update_error_message(
        &self,
        operation_id: &str,
        message: &str,
    ) -> Result<u64, AppError> {
        sqlx::query(
            "UPDATE pending_exports SET interrupted = 1, last_error = ?, updated_at = ?
             WHERE operation_id = ?",
        )
        .bind(message)
        .bind(Utc::now().to_rfc3339())
        .bind(operation_id)
        .execute(&self.pool)
        .await
        .map(|result| result.rows_affected())
        .map_err(|_| internal_error("failed to record export recovery error"))
    }

    async fn rebuild_missing_index(
        &self,
        expected: &PendingExportIdentity,
        message: &str,
        require_published_existing: bool,
    ) -> Result<(), AppError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|_| internal_error("failed to begin export recovery index rebuild"))?;
        let result = self
            .rebuild_missing_index_in_transaction(
                &mut transaction,
                expected,
                message,
                require_published_existing,
            )
            .await;
        finish_index_transaction(transaction, result).await
    }

    async fn rebuild_missing_index_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        expected: &PendingExportIdentity,
        message: &str,
        require_published_existing: bool,
    ) -> Result<(), AppError> {
        let conflicts = sqlx::query_as::<_, PendingExportRow>(
            "SELECT operation_id, batch_id, staging_component, final_component, exported_at, state
             FROM pending_exports
             WHERE operation_id = ? OR batch_id = ? OR staging_component = ? OR final_component = ?",
        )
        .bind(expected.operation_id.to_string())
        .bind(expected.batch_id.to_string())
        .bind(&expected.staging_component)
        .bind(&expected.final_component)
        .fetch_all(&mut **transaction)
        .await
        .map_err(|_| internal_error("failed to inspect export recovery index conflicts"))?;

        if let [existing] = conflicts.as_slice() {
            PendingExportIdentity::from_row(existing)?;
            if !expected.matches_row(existing)
                || (require_published_existing && existing.state != "published")
            {
                return Err(recovery_error(
                    "export recovery marker conflicts with the durable index",
                ));
            }
            if require_published_existing {
                let rows = sqlx::query(
                    "UPDATE pending_exports
                     SET interrupted = 1, last_error = ?, updated_at = ?
                     WHERE operation_id = ? AND state = 'published'",
                )
                .bind(message)
                .bind(Utc::now().to_rfc3339())
                .bind(expected.operation_id.to_string())
                .execute(&mut **transaction)
                .await
                .map_err(|_| internal_error("failed to restore export recovery error"))?
                .rows_affected();
                require_single_journal_row(rows, "export recovery index changed during rebuild")?;
            }
            return Ok(());
        }
        if !conflicts.is_empty() {
            return Err(recovery_error(
                "export recovery marker conflicts with the durable index",
            ));
        }

        let now = Utc::now().to_rfc3339();
        let rows = sqlx::query(
            "INSERT INTO pending_exports (
                operation_id, batch_id, staging_component, final_component, exported_at, state,
                interrupted, last_error, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, 'published', 1, ?, ?, ?)",
        )
        .bind(expected.operation_id.to_string())
        .bind(expected.batch_id.to_string())
        .bind(&expected.staging_component)
        .bind(&expected.final_component)
        .bind(expected.exported_at.to_rfc3339())
        .bind(message)
        .bind(&now)
        .bind(&now)
        .execute(&mut **transaction)
        .await
        .map_err(|_| internal_error("failed to rebuild export recovery index"))?
        .rows_affected();
        require_single_journal_row(rows, "failed to rebuild export recovery index")
    }
}

async fn finish_index_transaction(
    transaction: Transaction<'_, Sqlite>,
    result: Result<(), AppError>,
) -> Result<(), AppError> {
    match result {
        Ok(()) => transaction
            .commit()
            .await
            .map_err(|_| internal_error("failed to commit export recovery index rebuild")),
        Err(error) => match transaction.rollback().await {
            Ok(()) => Err(error),
            Err(_) => Err(internal_error(
                "export recovery index rebuild failed and rollback also failed",
            )),
        },
    }
}

fn require_single_journal_row(rows: u64, message: &'static str) -> Result<(), AppError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(internal_error(message))
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
    expected: &PendingExportIdentity,
) -> Result<(), AppError> {
    match read_recovery_marker(root, final_component)? {
        Some(marker) if marker_matches_identity(&marker, expected)? => {
            clear_recovery_marker(root, final_component)
        }
        Some(_) => Err(recovery_error(
            "committed export package has a mismatched recovery marker",
        )),
        None => Ok(()),
    }
}

fn validate_marker(marker: &RecoveryMarker, staging_component: &str) -> Result<(), AppError> {
    let identity = marker_identity(marker)?;
    if identity.staging_component != staging_component {
        return Err(recovery_error("export recovery marker is inconsistent"));
    }
    Ok(())
}

fn marker_identity(marker: &RecoveryMarker) -> Result<PendingExportIdentity, AppError> {
    let operation_id = Uuid::parse_str(&marker.operation_id)
        .map_err(|_| recovery_error("export recovery marker has an invalid identity"))?;
    let batch_id = Uuid::parse_str(&marker.batch_id)
        .map_err(|_| recovery_error("export recovery marker has an invalid batch identity"))?;
    let exported_at = DateTime::parse_from_rfc3339(&marker.exported_at)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| recovery_error("export recovery marker has an invalid timestamp"))?;
    validate_component(&marker.staging_component, Some("export-"))?;
    validate_component(&marker.final_component, None)?;
    if marker.state != "generating" {
        return Err(recovery_error("export recovery marker is inconsistent"));
    }
    Ok(PendingExportIdentity::new(
        operation_id,
        batch_id,
        marker.staging_component.clone(),
        marker.final_component.clone(),
        exported_at,
    ))
}

fn final_marker_identity(
    marker: &RecoveryMarker,
    final_component: &str,
) -> Result<PendingExportIdentity, AppError> {
    validate_component(final_component, None)?;
    let identity = marker_identity(marker)?;
    if identity.final_component != final_component {
        return Err(recovery_error("export recovery marker is inconsistent"));
    }
    Ok(identity)
}

fn marker_matches_identity(
    marker: &RecoveryMarker,
    expected: &PendingExportIdentity,
) -> Result<bool, AppError> {
    Ok(marker_identity(marker)? == *expected)
}

fn owned_recovery_directory_exists(
    root: &Path,
    component: &str,
    expected: &PendingExportIdentity,
    missing_marker_message: &'static str,
    mismatched_marker_message: &'static str,
    allow_unmarked_existing: bool,
) -> Result<bool, AppError> {
    match read_recovery_marker(root, component)? {
        Some(marker) if marker_matches_identity(&marker, expected)? => Ok(true),
        Some(_) => Err(recovery_error(mismatched_marker_message)),
        None => match recovery_directory_exists(root, component)? {
            true if allow_unmarked_existing => Ok(false),
            true => Err(recovery_error(missing_marker_message)),
            false => Ok(false),
        },
    }
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

fn read_final_recovery_marker(
    root: &Path,
    final_component: &str,
) -> Result<Option<PendingExportIdentity>, AppError> {
    read_recovery_marker(root, final_component)?
        .map(|marker| final_marker_identity(&marker, final_component))
        .transpose()
}

#[cfg(unix)]
fn discover_final_recovery_markers(root: &Path) -> Result<Vec<PendingExportIdentity>, AppError> {
    discover_final_recovery_markers_with_budget(
        root,
        MAX_EXPORT_SCAN_ENTRIES,
        MAX_DISCOVERED_MARKERS,
    )
}

#[cfg(unix)]
fn discover_final_recovery_markers_with_budget(
    root: &Path,
    max_entries: usize,
    max_markers: usize,
) -> Result<Vec<PendingExportIdentity>, AppError> {
    use std::os::unix::ffi::OsStrExt;

    use rustix::fs::{AtFlags, Dir, FileType, statat};

    let root = open_recovery_root(root)?;
    let entries = Dir::read_from(&root)
        .map_err(|_| recovery_error("failed to enumerate export recovery storage"))?;
    let mut scanned = 0_usize;
    let mut discovered = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|_| recovery_error("failed to enumerate export recovery entry"))?;
        let name = entry.file_name().to_bytes();
        if matches!(name, b"." | b"..") {
            continue;
        }
        scanned = scanned
            .checked_add(1)
            .ok_or_else(|| recovery_error("export recovery scan exceeded its entry budget"))?;
        if scanned > max_entries {
            return Err(recovery_error(
                "export recovery scan exceeded its entry budget",
            ));
        }

        let component = OsStr::from_bytes(name);
        let metadata = statat(&root, component, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| recovery_error("failed to inspect export recovery entry"))?;
        if FileType::from_raw_mode(metadata.st_mode) != FileType::Directory {
            continue;
        }
        let component = component
            .to_str()
            .ok_or_else(|| recovery_error("export recovery directory has an invalid name"))?;
        validate_component(component, None)?;
        let directory = open_recovery_directory(&root, component)?
            .ok_or_else(|| recovery_error("export recovery directory changed during discovery"))?;
        let Some(marker) = read_recovery_marker_from_directory(&directory)? else {
            continue;
        };
        let identity = final_marker_identity(&marker, component)?;
        discovered.push(identity);
        if discovered.len() > max_markers {
            return Err(recovery_error(
                "export recovery scan exceeded its marker budget",
            ));
        }
    }
    Ok(discovered)
}

#[cfg(not(unix))]
fn discover_final_recovery_markers(_root: &Path) -> Result<Vec<PendingExportIdentity>, AppError> {
    Err(recovery_error(
        "safe export recovery is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn open_recovery_root(root: &Path) -> Result<File, AppError> {
    use rustix::fs::{Mode, OFlags};

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::open(root, flags, Mode::empty())
        .map(File::from)
        .map_err(|_| recovery_error("failed to anchor export recovery storage"))
}

#[cfg(unix)]
fn open_recovery_directory(root: &File, component: &str) -> Result<Option<File>, AppError> {
    use rustix::fs::{Mode, OFlags, openat};
    use rustix::io::Errno;

    let component = validate_component(component, None)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(root, component, flags, Mode::empty()) {
        Ok(directory) => Ok(Some(File::from(directory))),
        Err(error) if error == Errno::NOENT => Ok(None),
        Err(_) => Err(recovery_error("export recovery directory is not safe")),
    }
}

#[cfg(unix)]
fn read_recovery_marker(root: &Path, component: &str) -> Result<Option<RecoveryMarker>, AppError> {
    let root = open_recovery_root(root)?;
    let Some(directory) = open_recovery_directory(&root, component)? else {
        return Ok(None);
    };
    read_recovery_marker_from_directory(&directory)
}

#[cfg(unix)]
fn read_recovery_marker_from_directory(
    directory: &File,
) -> Result<Option<RecoveryMarker>, AppError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, statat};
    use rustix::io::Errno;

    let metadata = match statat(directory, RECOVERY_MARKER, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => metadata,
        Err(error) if error == Errno::NOENT => return Ok(None),
        Err(_) => return Err(recovery_error("failed to inspect export recovery marker")),
    };
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        return Err(recovery_error("export recovery marker is not a safe file"));
    }
    let file_flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let mut file = openat(directory, RECOVERY_MARKER, file_flags, Mode::empty())
        .map(File::from)
        .map_err(|_| recovery_error("failed to open export recovery marker"))?;
    let opened_metadata = file
        .metadata()
        .map_err(|_| recovery_error("failed to inspect opened export recovery marker"))?;
    if !opened_metadata.is_file() || opened_metadata.len() > MAX_MARKER_BYTES as u64 {
        return Err(recovery_error("export recovery marker is not a safe file"));
    }
    let marker_length = usize::try_from(opened_metadata.len())
        .map_err(|_| recovery_error("export recovery marker has an invalid size"))?;
    let mut bytes = Vec::with_capacity(marker_length);
    Read::by_ref(&mut file)
        .take((MAX_MARKER_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
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

fn outcome_error(message: &str) -> AppError {
    AppError::External {
        service: "export_recovery".to_owned(),
        retryable: true,
        message: message.to_owned(),
    }
}

fn internal_error(message: &str) -> AppError {
    AppError::Internal {
        message: message.to_owned(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;

    use chrono::Utc;
    use uuid::Uuid;

    use super::{
        ExportJournal, discover_final_recovery_markers_with_budget, write_generation_marker,
    };
    use crate::db;
    use crate::infra::files::AppPaths;

    #[test]
    fn final_marker_discovery_enforces_its_entry_budget() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        fs::write(paths.exports.join("one"), b"").unwrap();
        fs::write(paths.exports.join("two"), b"").unwrap();

        let error = discover_final_recovery_markers_with_budget(&paths.exports, 1, 10).unwrap_err();

        assert!(error.to_string().contains("entry budget"));
    }

    #[test]
    fn final_marker_discovery_enforces_its_marker_budget() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::create(directory.path().join("storage")).unwrap();
        let batch_id = Uuid::new_v4();
        for final_component in ["one", "two"] {
            let operation_id = Uuid::new_v4();
            let staging_component = format!("export-{operation_id}");
            let final_directory = paths.exports.join(final_component);
            fs::create_dir(&final_directory).unwrap();
            write_generation_marker(
                &final_directory,
                operation_id,
                batch_id,
                &staging_component,
                final_component,
                Utc::now(),
            )
            .unwrap();
        }

        let error = discover_final_recovery_markers_with_budget(&paths.exports, 10, 1).unwrap_err();

        assert!(error.to_string().contains("marker budget"));
        assert!(paths.exports.join("one").is_dir());
        assert!(paths.exports.join("two").is_dir());
    }

    #[tokio::test]
    async fn advancing_a_missing_index_is_not_reported_as_success() {
        let pool = db::connect("sqlite::memory:").await.unwrap();

        let error = ExportJournal::new(pool)
            .advance(Uuid::new_v4(), "published")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("journal was missing"));
    }

    #[tokio::test]
    async fn finishing_a_missing_index_is_not_reported_as_success() {
        let pool = db::connect("sqlite::memory:").await.unwrap();

        let error = ExportJournal::new(pool)
            .finish(Uuid::new_v4())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("missing during cleanup"));
    }
}
