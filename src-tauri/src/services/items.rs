use std::ffi::{OsStr, OsString};
use std::fs;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use chrono::NaiveDate;
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::db::items::{
    InvoiceItem, ItemFilter, ItemPage, ItemPageCursor, ItemRepository, ReviewedItemFields,
};
use crate::domain::amount::validate_amount_cents;
use crate::domain::error::AppError;
use crate::domain::model::Category;
use crate::infra::files::AppPaths;
#[cfg(not(unix))]
use crate::infra::files::{remove_file_durably, rename_staged_original, sync_directory};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemReview {
    pub id: Uuid,
    pub invoice_date: Option<String>,
    pub suggested_period: String,
    pub final_category: Category,
    pub amount_cents: i64,
    pub city: Option<String>,
    pub company: Option<String>,
    pub note: Option<String>,
    pub event_tag: Option<String>,
    pub project_tag: Option<String>,
}

#[derive(Clone)]
pub struct ItemService {
    items: ItemRepository,
    files: Arc<dyn FileLifecycle>,
    recovery: Arc<OnceCell<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemFiles {
    pub original: PathBuf,
    pub normalized: Option<PathBuf>,
}

#[derive(Debug)]
pub struct IsolatedItemFiles {
    entries: Vec<IsolatedFile>,
}

impl IsolatedItemFiles {
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

pub trait FileLifecycle: Send + Sync {
    fn recover(&self, _referenced_paths: &HashSet<PathBuf>) -> Result<(), AppError> {
        Ok(())
    }

    fn isolate(
        &self,
        files: &ItemFiles,
        protected_paths: &[PathBuf],
    ) -> Result<IsolatedItemFiles, AppError>;

    fn restore(&self, isolated: &IsolatedItemFiles) -> Result<(), AppError>;

    fn purge(&self, isolated: &IsolatedItemFiles) -> Result<(), AppError>;
}

#[derive(Debug)]
pub struct StorageFileLifecycle {
    paths: AppPaths,
}

#[derive(Debug)]
struct IsolatedFile {
    #[cfg(not(unix))]
    original: PathBuf,
    #[cfg(not(unix))]
    isolated: Option<PathBuf>,
    #[cfg(unix)]
    anchor: Option<AnchoredFile>,
}

#[cfg(unix)]
#[derive(Debug)]
struct AnchoredFile {
    parent: Arc<File>,
    original_name: OsString,
    isolated_name: OsString,
}

impl ItemService {
    pub fn new(items: ItemRepository, paths: AppPaths) -> Self {
        Self {
            items,
            files: Arc::new(StorageFileLifecycle::new(paths)),
            recovery: Arc::new(OnceCell::new()),
        }
    }

    pub fn with_file_lifecycle(items: ItemRepository, files: Arc<dyn FileLifecycle>) -> Self {
        Self {
            items,
            files,
            recovery: Arc::new(OnceCell::new()),
        }
    }

    pub async fn list_page(
        &self,
        filter: ItemFilter,
        cursor: Option<ItemPageCursor>,
        page_size: usize,
    ) -> Result<ItemPage, AppError> {
        self.items.list_page(filter, cursor, page_size).await
    }

    pub async fn get(&self, id: Uuid) -> Result<InvoiceItem, AppError> {
        self.items.get_by_id(id).await
    }

    pub async fn review(&self, review: ItemReview) -> Result<InvoiceItem, AppError> {
        self.ensure_recovered().await?;
        let invoice_date = review
            .invoice_date
            .as_deref()
            .map(parse_invoice_date)
            .transpose()?;
        validate_period(&review.suggested_period)?;
        validate_amount_cents(review.amount_cents, "amount_cents")?;

        self.items
            .review_item(
                review.id,
                ReviewedItemFields {
                    invoice_date,
                    suggested_period: review.suggested_period,
                    final_category: review.final_category,
                    amount_cents: review.amount_cents,
                    city: normalize_optional_text(review.city),
                    company: normalize_optional_text(review.company),
                    note: normalize_optional_text(review.note),
                    event_tag: normalize_optional_text(review.event_tag),
                    project_tag: normalize_optional_text(review.project_tag),
                },
            )
            .await
    }

    pub async fn resolve_duplicate(
        &self,
        id: Uuid,
        keep: bool,
    ) -> Result<Option<InvoiceItem>, AppError> {
        if keep {
            self.ensure_recovered().await?;
            return self.items.keep_suspected_duplicate(id).await.map(Some);
        }

        let service = self.clone();
        tokio::spawn(async move { service.discard_duplicate(id).await })
            .await
            .map_err(|error| filesystem_task_error("duplicate resolution task failed", error))?
    }

    async fn discard_duplicate(&self, id: Uuid) -> Result<Option<InvoiceItem>, AppError> {
        self.ensure_recovered().await?;
        let claim = self.items.claim_duplicate_discard(id).await?;
        let item = claim.item().clone();
        let protected_paths = claim
            .protected_paths()
            .iter()
            .cloned()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let owned_files = ItemFiles {
            original: PathBuf::from(&item.original_path),
            normalized: item.normalized_pdf_path.as_ref().map(PathBuf::from),
        };
        let file_lifecycle = self.files.clone();
        let isolation = match tokio::task::spawn_blocking(move || {
            file_lifecycle.isolate(&owned_files, &protected_paths)
        })
        .await
        {
            Ok(isolation) => isolation,
            Err(task_error) => {
                let error = filesystem_task_error("file isolation task failed", task_error);
                return match claim.rollback().await {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(claim_rollback_error(error, rollback_error)),
                };
            }
        };
        let isolated = match isolation {
            Ok(isolated) => isolated,
            Err(error @ AppError::Validation { .. }) => {
                return match claim.rollback().await {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(claim_rollback_error(error, rollback_error)),
                };
            }
            Err(error) => {
                let error = AppError::External {
                    service: "filesystem_sync".to_owned(),
                    retryable: false,
                    message: format!("failed to isolate duplicate item files: {error}"),
                };
                return match claim.rollback().await {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(claim_rollback_error(error, rollback_error)),
                };
            }
        };

        if let Err(database_error) = claim.delete().await {
            let file_lifecycle = self.files.clone();
            let restore = tokio::task::spawn_blocking(move || file_lifecycle.restore(&isolated))
                .await
                .map_err(|error| filesystem_task_error("file restore task failed", error))?;
            return match restore {
                Ok(()) => Err(database_error),
                Err(restore_error) => Err(AppError::External {
                    service: "filesystem_sync".to_owned(),
                    retryable: false,
                    message: format!(
                        "duplicate deletion failed and isolated files could not be restored; \
                         manual recovery is required: database error: {database_error}; \
                         restore error: {restore_error}"
                    ),
                }),
            };
        }

        let file_lifecycle = self.files.clone();
        let purge = tokio::task::spawn_blocking(move || file_lifecycle.purge(&isolated))
            .await
            .map_err(|error| filesystem_task_error("file purge task failed", error))?;
        if let Err(purge_error) = purge {
            return Err(AppError::External {
                service: "filesystem_sync".to_owned(),
                retryable: false,
                message: format!(
                    "duplicate row was deleted but isolated file cleanup was incomplete; manual \
                     recovery is required: {purge_error}"
                ),
            });
        }
        Ok(None)
    }

    async fn ensure_recovered(&self) -> Result<(), AppError> {
        self.recovery
            .get_or_try_init(|| async {
                let claim = self.items.claim_file_recovery().await?;
                let referenced_paths = claim
                    .referenced_paths()
                    .iter()
                    .map(PathBuf::from)
                    .collect::<HashSet<_>>();
                let file_lifecycle = self.files.clone();
                let result = match tokio::task::spawn_blocking(move || {
                    file_lifecycle.recover(&referenced_paths)
                })
                .await
                {
                    Ok(result) => result,
                    Err(task_error) => {
                        let error =
                            filesystem_task_error("file recovery task failed", task_error);
                        return match claim.rollback().await {
                            Ok(()) => Err(error),
                            Err(rollback_error) => Err(AppError::External {
                                service: "filesystem_sync".to_owned(),
                                retryable: false,
                                message: format!(
                                    "file recovery task failed and its database claim could not be \
                                     released; manual recovery is required: task error: {error}; \
                                     rollback error: {rollback_error}"
                                ),
                            }),
                        };
                    }
                };
                match result {
                    Ok(()) => claim.commit().await,
                    Err(error) => match claim.rollback().await {
                        Ok(()) => Err(error),
                        Err(rollback_error) => Err(AppError::External {
                            service: "filesystem_sync".to_owned(),
                            retryable: false,
                            message: format!(
                                "file recovery failed and its database claim could not be released; \
                                 manual recovery is required: recovery error: {error}; rollback \
                                 error: {rollback_error}"
                            ),
                        }),
                    },
                }
            })
            .await
            .map(|_| ())
    }
}

impl StorageFileLifecycle {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }
}

fn claim_rollback_error(original: AppError, rollback: AppError) -> AppError {
    AppError::External {
        service: "filesystem_sync".to_owned(),
        retryable: false,
        message: format!(
            "duplicate file handling failed and its database claim could not be released; manual \
             recovery is required: original error: {original}; rollback error: {rollback}"
        ),
    }
}

fn filesystem_task_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::External {
        service: "filesystem_sync".to_owned(),
        retryable: false,
        message: format!("{context}: {error}"),
    }
}

impl FileLifecycle for StorageFileLifecycle {
    fn recover(&self, referenced_paths: &HashSet<PathBuf>) -> Result<(), AppError> {
        recover_storage(&self.paths, referenced_paths)
    }

    fn isolate(
        &self,
        files: &ItemFiles,
        protected_paths: &[PathBuf],
    ) -> Result<IsolatedItemFiles, AppError> {
        let mut entries = Vec::with_capacity(1 + usize::from(files.normalized.is_some()));
        isolate_owned_file(
            &mut entries,
            "original_path",
            &files.original,
            &self.paths.originals,
            protected_paths,
        )?;
        if let Some(normalized) = &files.normalized {
            isolate_owned_file(
                &mut entries,
                "normalized_pdf_path",
                normalized,
                &self.paths.normalized,
                protected_paths,
            )?;
        }

        Ok(IsolatedItemFiles { entries })
    }

    fn restore(&self, isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        restore_isolated_files(&isolated.entries)
    }

    fn purge(&self, isolated: &IsolatedItemFiles) -> Result<(), AppError> {
        let mut errors = Vec::new();
        for entry in &isolated.entries {
            if let Err(error) = purge_isolated_file(entry) {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(AppError::External {
                service: "filesystem_sync".to_owned(),
                retryable: false,
                message: format!(
                    "duplicate row was deleted but isolated file cleanup was incomplete; manual \
                     recovery is required: {}",
                    errors.join("; ")
                ),
            })
        }
    }
}

fn parse_recovery_token(file_name: &OsStr) -> Option<OsString> {
    let file_name = file_name.to_str()?;
    let encoded = file_name.strip_prefix('.')?;
    let (original_name, token_id) = encoded.rsplit_once(".delete-")?;
    if original_name.is_empty() || Uuid::parse_str(token_id).is_err() {
        return None;
    }
    Some(OsString::from(original_name))
}

#[cfg(unix)]
fn recover_storage(paths: &AppPaths, referenced_paths: &HashSet<PathBuf>) -> Result<(), AppError> {
    recover_root_anchored(&paths.originals, referenced_paths)?;
    recover_root_anchored(&paths.normalized, referenced_paths)
}

#[cfg(unix)]
fn recover_root_anchored(root: &Path, referenced_paths: &HashSet<PathBuf>) -> Result<(), AppError> {
    use rustix::fs::{Mode, OFlags};

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let directory = rustix::fs::open(root, flags, Mode::empty())
        .map(File::from)
        .map_err(|error| filesystem_error("failed to anchor recovery root", error))?;
    recover_directory_anchored(&directory, root, referenced_paths)
}

#[cfg(unix)]
fn recover_directory_anchored(
    directory: &File,
    directory_path: &Path,
    referenced_paths: &HashSet<PathBuf>,
) -> Result<(), AppError> {
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, statat};

    let mut entries = Dir::read_from(directory)
        .map_err(|error| filesystem_error("failed to read recovery directory", error))?;
    while let Some(entry) = entries.read() {
        let entry =
            entry.map_err(|error| filesystem_error("failed to read recovery entry", error))?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == OsStr::new(".") || name == OsStr::new("..") {
            continue;
        }
        let metadata = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| filesystem_error("failed to inspect recovery entry", error))?;
        let file_type = FileType::from_raw_mode(metadata.st_mode);
        if let Some(original_name) = parse_recovery_token(name) {
            if file_type != FileType::RegularFile {
                return Err(recovery_manual_error(
                    "recovery token is not a regular file",
                    "unsupported file type",
                ));
            }
            recover_file_anchored(
                directory,
                directory_path,
                name,
                &original_name,
                referenced_paths,
            )?;
            continue;
        }
        match file_type {
            FileType::Directory => {
                let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
                let child = rustix::fs::openat(directory, name, flags, Mode::empty())
                    .map(File::from)
                    .map_err(|error| {
                        filesystem_error("failed to anchor recovery subdirectory", error)
                    })?;
                recover_directory_anchored(&child, &directory_path.join(name), referenced_paths)?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(unix)]
fn recover_file_anchored(
    directory: &File,
    directory_path: &Path,
    token_name: &OsStr,
    original_name: &OsStr,
    referenced_paths: &HashSet<PathBuf>,
) -> Result<(), AppError> {
    use rustix::fs::{AtFlags, statat};
    use rustix::io::Errno;

    let original_path = directory_path.join(original_name);
    if referenced_paths.contains(&original_path) {
        match statat(directory, original_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => {
                return Err(recovery_manual_error(
                    "recovery target already exists",
                    "refusing to overwrite an existing file",
                ));
            }
            Err(error) if error == Errno::NOENT => {}
            Err(error) => {
                return Err(filesystem_error("failed to inspect recovery target", error));
            }
        }
        move_file_anchored(directory, token_name, original_name)
            .map_err(|error| recovery_manual_error("failed to restore recovery token", error))
    } else {
        purge_name_anchored(directory, token_name)
    }
}

#[cfg(not(unix))]
fn recover_storage(paths: &AppPaths, referenced_paths: &HashSet<PathBuf>) -> Result<(), AppError> {
    recover_directory_portable(&paths.originals, referenced_paths)?;
    recover_directory_portable(&paths.normalized, referenced_paths)
}

#[cfg(not(unix))]
fn recover_directory_portable(
    directory: &Path,
    referenced_paths: &HashSet<PathBuf>,
) -> Result<(), AppError> {
    for entry in fs::read_dir(directory)
        .map_err(|error| filesystem_error("failed to read recovery directory", error))?
    {
        let entry =
            entry.map_err(|error| filesystem_error("failed to read recovery entry", error))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| filesystem_error("failed to inspect recovery entry", error))?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            recover_directory_portable(&entry.path(), referenced_paths)?;
            continue;
        }
        let Some(original_name) = parse_recovery_token(&entry.file_name()) else {
            continue;
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(recovery_manual_error(
                "recovery token is not a regular file",
                "unsupported file type",
            ));
        }
        let original_path = directory.join(original_name);
        if referenced_paths.contains(&original_path) {
            if original_path.exists() {
                return Err(recovery_manual_error(
                    "recovery target already exists",
                    "refusing to overwrite an existing file",
                ));
            }
            move_file_durably(&entry.path(), &original_path).map_err(|error| {
                recovery_manual_error("failed to restore recovery token", error)
            })?;
        } else {
            remove_file_durably(&entry.path())?;
        }
    }
    Ok(())
}

fn recovery_manual_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::External {
        service: "filesystem_sync".to_owned(),
        retryable: false,
        message: format!("{context}; manual recovery is required: {error}"),
    }
}

fn isolate_owned_file(
    entries: &mut Vec<IsolatedFile>,
    field: &str,
    path: &Path,
    root: &Path,
    protected_paths: &[PathBuf],
) -> Result<(), AppError> {
    if path_is_protected(path, protected_paths) {
        return Ok(());
    }
    match isolate_file(field, path, root) {
        Ok(entry) => {
            entries.push(entry);
            Ok(())
        }
        Err(error) => Err(restore_after_isolation_error(
            std::mem::take(entries),
            error,
        )),
    }
}

fn path_is_protected(path: &Path, protected_paths: &[PathBuf]) -> bool {
    if protected_paths.iter().any(|protected| protected == path) {
        return true;
    }
    fs::canonicalize(path).ok().is_some_and(|canonical_path| {
        protected_paths
            .iter()
            .filter_map(|protected| fs::canonicalize(protected).ok())
            .any(|protected| protected == canonical_path)
    })
}

fn validate_relative_path<'a>(
    field: &str,
    path: &'a Path,
    root: &Path,
) -> Result<&'a Path, AppError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        AppError::validation(
            field,
            "stored file path is not exclusively owned by this item",
        )
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(AppError::validation(
            field,
            "stored file path is not exclusively owned by this item",
        ));
    }
    Ok(relative)
}

#[cfg(not(unix))]
fn validate_owned_file(field: &str, path: &Path, root: &Path) -> Result<(), AppError> {
    validate_relative_path(field, path, root)?;
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| filesystem_error("failed to resolve storage root", error))?;

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            AppError::validation(field, "stored file path must reference a regular file"),
        ),
        Ok(_) => {
            let parent = path.parent().ok_or_else(|| {
                AppError::validation(field, "stored file path has no parent directory")
            })?;
            let canonical_parent = fs::canonicalize(parent)
                .map_err(|error| filesystem_error("failed to resolve stored file parent", error))?;
            let canonical_path = fs::canonicalize(path)
                .map_err(|error| filesystem_error("failed to resolve stored file", error))?;
            if !canonical_parent.starts_with(&canonical_root)
                || !canonical_path.starts_with(&canonical_root)
            {
                return Err(AppError::validation(
                    field,
                    "stored file path is not exclusively owned by this item",
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            validate_missing_file_ancestor(field, path, &canonical_root)
        }
        Err(error) => Err(filesystem_error("failed to inspect stored file", error)),
    }
}

#[cfg(not(unix))]
fn validate_missing_file_ancestor(
    field: &str,
    path: &Path,
    canonical_root: &Path,
) -> Result<(), AppError> {
    let mut ancestor = path.parent();
    while let Some(candidate) = ancestor {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(AppError::validation(
                    field,
                    "stored file path escapes application storage",
                ));
            }
            Ok(_) => {
                let canonical_ancestor = fs::canonicalize(candidate).map_err(|error| {
                    filesystem_error("failed to resolve stored file ancestor", error)
                })?;
                return if canonical_ancestor.starts_with(canonical_root) {
                    Ok(())
                } else {
                    Err(AppError::validation(
                        field,
                        "stored file path escapes application storage",
                    ))
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = candidate.parent();
            }
            Err(error) => {
                return Err(filesystem_error(
                    "failed to inspect stored file ancestor",
                    error,
                ));
            }
        }
    }
    Err(AppError::validation(
        field,
        "stored file path has no storage ancestor",
    ))
}

#[cfg(not(unix))]
fn isolate_file(field: &str, path: &Path, root: &Path) -> Result<IsolatedFile, AppError> {
    validate_owned_file(field, path, root)?;
    if !path.exists() {
        return Ok(IsolatedFile {
            original: path.to_path_buf(),
            isolated: None,
        });
    }
    let file_name = path.file_name().ok_or_else(|| AppError::Internal {
        message: "stored file path has no file name".to_owned(),
    })?;
    let isolated = path.with_file_name(format!(
        ".{}.delete-{}",
        file_name.to_string_lossy(),
        Uuid::new_v4()
    ));
    move_file_durably(path, &isolated)?;
    Ok(IsolatedFile {
        original: path.to_path_buf(),
        isolated: Some(isolated),
    })
}

#[cfg(unix)]
fn isolate_file(field: &str, path: &Path, root: &Path) -> Result<IsolatedFile, AppError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, statat};
    use rustix::io::Errno;

    let relative = validate_relative_path(field, path, root)?;
    let file_name = relative
        .file_name()
        .ok_or_else(|| AppError::validation(field, "stored file path has no file name"))?;
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut parent = rustix::fs::open(root, directory_flags, Mode::empty())
        .map(File::from)
        .map_err(|error| filesystem_error("failed to anchor storage root", error))?;
    if let Some(relative_parent) = relative.parent() {
        for component in relative_parent.components() {
            let Component::Normal(component) = component else {
                return Err(AppError::validation(
                    field,
                    "stored file path is not exclusively owned by this item",
                ));
            };
            parent = match rustix::fs::openat(&parent, component, directory_flags, Mode::empty()) {
                Ok(directory) => File::from(directory),
                Err(error) if error == Errno::NOENT => {
                    return Ok(missing_isolated_file());
                }
                Err(error) if error == Errno::LOOP || error == Errno::NOTDIR => {
                    return Err(AppError::validation(
                        field,
                        "stored file path escapes application storage",
                    ));
                }
                Err(error) => {
                    return Err(filesystem_error(
                        "failed to anchor stored file parent",
                        error,
                    ));
                }
            };
        }
    }

    let metadata = match statat(&parent, file_name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => metadata,
        Err(error) if error == Errno::NOENT => return Ok(missing_isolated_file()),
        Err(error) => return Err(filesystem_error("failed to inspect stored file", error)),
    };
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        return Err(AppError::validation(
            field,
            "stored file path must reference a regular file",
        ));
    }

    let isolated_name = OsString::from(format!(
        ".{}.delete-{}",
        file_name.to_string_lossy(),
        Uuid::new_v4()
    ));
    move_file_anchored(&parent, file_name, &isolated_name)?;
    Ok(IsolatedFile {
        anchor: Some(AnchoredFile {
            parent: Arc::new(parent),
            original_name: file_name.to_os_string(),
            isolated_name,
        }),
    })
}

#[cfg(unix)]
fn missing_isolated_file() -> IsolatedFile {
    IsolatedFile { anchor: None }
}

#[cfg(unix)]
fn move_file_anchored(parent: &File, source: &OsStr, destination: &OsStr) -> Result<(), AppError> {
    use rustix::fs::{RenameFlags, fsync, renameat_with};
    use rustix::io::Errno;

    match renameat_with(parent, source, parent, destination, RenameFlags::NOREPLACE) {
        Ok(()) => {}
        Err(error) if error == Errno::EXIST => {
            return Err(AppError::Conflict {
                message: "file isolation destination already exists".to_owned(),
            });
        }
        Err(error) => return Err(filesystem_error("failed to isolate stored file", error)),
    }
    if let Err(sync_error) = fsync(parent) {
        return match renameat_with(parent, destination, parent, source, RenameFlags::NOREPLACE) {
            Ok(()) => match fsync(parent) {
                Ok(()) => Err(filesystem_error(
                    "failed to sync file move; move was rolled back",
                    sync_error,
                )),
                Err(rollback_sync_error) => Err(AppError::External {
                    service: "filesystem_sync".to_owned(),
                    retryable: false,
                    message: format!(
                        "file move sync failed and rollback durability is incomplete; manual \
                         recovery is required: move sync error: {sync_error}; rollback sync error: \
                         {rollback_sync_error}"
                    ),
                }),
            },
            Err(rollback_error) => Err(AppError::External {
                service: "filesystem_sync".to_owned(),
                retryable: false,
                message: format!(
                    "file move sync failed and rollback rename failed; manual recovery is required: \
                     move sync error: {sync_error}; rollback error: {rollback_error}"
                ),
            }),
        };
    }
    Ok(())
}

#[cfg(not(unix))]
fn move_file_durably(source: &Path, destination: &Path) -> Result<(), AppError> {
    rename_staged_original(source, destination)?;
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    if let Err(sync_error) = sync_directory(parent) {
        return match rename_staged_original(destination, source) {
            Ok(()) => match sync_directory(parent) {
                Ok(()) => Err(filesystem_error(
                    "failed to sync file isolation; isolation was rolled back",
                    sync_error,
                )),
                Err(rollback_sync_error) => Err(AppError::External {
                    service: "filesystem_sync".to_owned(),
                    retryable: false,
                    message: format!(
                        "file isolation sync failed and rollback durability is incomplete; manual \
                         recovery is required: isolation sync error: {sync_error}; rollback sync \
                         error: {rollback_sync_error}"
                    ),
                }),
            },
            Err(rollback_error) => Err(AppError::External {
                service: "filesystem_sync".to_owned(),
                retryable: false,
                message: format!(
                    "file isolation sync failed and rollback rename failed; manual recovery is \
                     required: isolation sync error: {sync_error}; rollback error: {rollback_error}"
                ),
            }),
        };
    }
    Ok(())
}

fn restore_isolated_files(entries: &[IsolatedFile]) -> Result<(), AppError> {
    let mut errors = Vec::new();
    for entry in entries.iter().rev() {
        if let Err(error) = restore_isolated_file(entry) {
            errors.push(error.to_string());
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::External {
            service: "filesystem_sync".to_owned(),
            retryable: false,
            message: format!(
                "failed to restore isolated item files; manual recovery is required: {}",
                errors.join("; ")
            ),
        })
    }
}

#[cfg(unix)]
fn restore_isolated_file(entry: &IsolatedFile) -> Result<(), AppError> {
    match &entry.anchor {
        Some(anchor) => {
            move_file_anchored(&anchor.parent, &anchor.isolated_name, &anchor.original_name)
        }
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn restore_isolated_file(entry: &IsolatedFile) -> Result<(), AppError> {
    match &entry.isolated {
        Some(isolated) => move_file_durably(isolated, &entry.original),
        None => Ok(()),
    }
}

#[cfg(unix)]
fn purge_isolated_file(entry: &IsolatedFile) -> Result<(), AppError> {
    let Some(anchor) = &entry.anchor else {
        return Ok(());
    };
    purge_name_anchored(&anchor.parent, &anchor.isolated_name)
}

#[cfg(unix)]
fn purge_name_anchored(directory: &File, file_name: &OsStr) -> Result<(), AppError> {
    use rustix::fs::{AtFlags, fsync, unlinkat};
    use rustix::io::Errno;

    match unlinkat(directory, file_name, AtFlags::empty()) {
        Ok(()) => fsync(directory)
            .map_err(|error| filesystem_error("failed to sync purged file parent", error)),
        Err(error) if error == Errno::NOENT => Ok(()),
        Err(error) => Err(filesystem_error("failed to purge isolated file", error)),
    }
}

#[cfg(not(unix))]
fn purge_isolated_file(entry: &IsolatedFile) -> Result<(), AppError> {
    match &entry.isolated {
        Some(path) => remove_file_durably(path),
        None => Ok(()),
    }
}

fn restore_after_isolation_error(entries: Vec<IsolatedFile>, error: AppError) -> AppError {
    match restore_isolated_files(&entries) {
        Ok(()) => error,
        Err(restore_error) => AppError::External {
            service: "filesystem_sync".to_owned(),
            retryable: false,
            message: format!(
                "file isolation failed and prior files could not be restored; manual recovery is \
                 required: isolation error: {error}; restore error: {restore_error}"
            ),
        },
    }
}

fn filesystem_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::External {
        service: "filesystem_sync".to_owned(),
        retryable: false,
        message: format!("{context}: {error}"),
    }
}

fn parse_invoice_date(value: &str) -> Result<NaiveDate, AppError> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .filter(|date| date.format("%Y-%m-%d").to_string() == value)
        .ok_or_else(|| AppError::validation("invoice_date", "must be YYYY-MM-DD"))
}

fn validate_period(value: &str) -> Result<(), AppError> {
    NaiveDate::parse_from_str(&format!("{value}-01"), "%Y-%m-%d")
        .ok()
        .filter(|date| date.format("%Y-%m").to_string() == value)
        .map(|_| ())
        .ok_or_else(|| AppError::validation("suggested_period", "must be YYYY-MM"))
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}
use std::collections::HashSet;
