use std::fs::{self, File, OpenOptions};
#[cfg(test)]
use std::io::Read;
use std::io::{self, Write};
#[cfg(unix)]
use std::path::Component;
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

#[derive(Debug)]
pub struct StagedOriginal {
    paths: AppPaths,
    path: PathBuf,
    owned: bool,
}

#[derive(Debug)]
pub struct PromotedOriginal {
    path: PathBuf,
    owned: bool,
}

pub(crate) struct OpenedContainedFile {
    pub(crate) file: File,
    pub(crate) length: u64,
}

pub(crate) fn open_contained_regular_file(
    path: &Path,
    root: &Path,
    field: &str,
) -> Result<OpenedContainedFile, AppError> {
    open_contained_regular_file_with_hook(path, root, field, || {})
}

#[cfg(unix)]
fn open_contained_regular_file_with_hook(
    path: &Path,
    root: &Path,
    field: &str,
    parent_anchored: impl FnOnce(),
) -> Result<OpenedContainedFile, AppError> {
    use rustix::fs::{Mode, OFlags};

    let relative = path
        .strip_prefix(root)
        .map_err(|_| AppError::validation(field, "票据文件不在应用存储目录内"))?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(AppError::validation(field, "票据文件不在应用存储目录内"));
    }

    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut parent = rustix::fs::open(root, directory_flags, Mode::empty())
        .map(File::from)
        .map_err(|_| AppError::Internal {
            message: "failed to open application file storage".to_owned(),
        })?;
    if let Some(relative_parent) = relative.parent() {
        for component in relative_parent.components() {
            let Component::Normal(component) = component else {
                return Err(AppError::validation(field, "票据文件不在应用存储目录内"));
            };
            parent = rustix::fs::openat(&parent, component, directory_flags, Mode::empty())
                .map(File::from)
                .map_err(|_| AppError::validation(field, "票据文件路径无法读取"))?;
        }
    }

    parent_anchored();
    let file_name = relative
        .file_name()
        .ok_or_else(|| AppError::validation(field, "票据文件路径无法读取"))?;
    let file_flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let file = rustix::fs::openat(&parent, file_name, file_flags, Mode::empty())
        .map(File::from)
        .map_err(|_| AppError::validation(field, "票据路径必须是普通文件"))?;
    let metadata = file
        .metadata()
        .map_err(|_| AppError::validation(field, "票据文件无法读取"))?;
    if !metadata.is_file() {
        return Err(AppError::validation(field, "票据路径必须是普通文件"));
    }
    Ok(OpenedContainedFile {
        file,
        length: metadata.len(),
    })
}

#[cfg(not(unix))]
fn open_contained_regular_file_with_hook(
    _path: &Path,
    _root: &Path,
    field: &str,
    _parent_anchored: impl FnOnce(),
) -> Result<OpenedContainedFile, AppError> {
    Err(unsupported_contained_file_platform(field))
}

#[cfg(not(unix))]
fn unsupported_contained_file_platform(field: &str) -> AppError {
    AppError::validation(field, "当前平台不支持安全文件读取")
}

#[cfg(test)]
pub(crate) fn read_contained_regular_file_with_hooks(
    path: &Path,
    root: &Path,
    field: &str,
    max_bytes: u64,
    parent_anchored: impl FnOnce(),
    before_read: impl FnOnce(),
) -> Result<Vec<u8>, AppError> {
    let mut opened = open_contained_regular_file_with_hook(path, root, field, parent_anchored)?;
    if opened.length > max_bytes {
        return Err(AppError::validation(field, "票据文件超过读取大小限制"));
    }
    before_read();
    let mut bytes = Vec::with_capacity(usize::try_from(opened.length).unwrap_or(0));
    Read::by_ref(&mut opened.file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| AppError::validation(field, "票据文件无法读取"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(AppError::validation(field, "票据文件超过读取大小限制"));
    }
    Ok(bytes)
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

    pub fn for_export_directory(&self, preference: &str) -> Result<Self, AppError> {
        let preference = preference.trim();
        if preference == "exports" {
            return Ok(self.clone());
        }
        let requested = Path::new(preference);
        if !requested.is_absolute() {
            return Err(AppError::validation(
                "export_directory",
                "导出目录必须是绝对路径",
            ));
        }
        let metadata = fs::symlink_metadata(requested)
            .map_err(|_| AppError::validation("export_directory", "导出目录不存在或无法访问"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::validation(
                "export_directory",
                "导出目录必须是安全的普通目录",
            ));
        }
        let exports = fs::canonicalize(requested)
            .map_err(|_| AppError::validation("export_directory", "导出目录无法安全解析"))?;
        let staging = exports.join(".invoice-reimbursement-staging");
        ensure_storage_directory(&exports, &staging)
            .map_err(|_| AppError::validation("export_directory", "导出目录不可写或不安全"))?;
        sync_directory(&exports)
            .map_err(|_| AppError::validation("export_directory", "导出目录无法同步"))?;

        let mut paths = self.clone();
        paths.exports = exports;
        paths.staging = staging;
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
        let mut source_file = File::open(source.as_ref())
            .map_err(|error| internal_error("failed to open original source", error))?;
        let (staged, mut staging_file) = self.begin_staged_original(id)?;
        let copy_result = (|| {
            io::copy(&mut source_file, &mut staging_file)
                .map_err(|error| internal_error("failed to copy original to staging", error))?;
            staging_file
                .sync_all()
                .map_err(|error| internal_error("failed to sync staged original", error))
        })();
        drop(staging_file);
        if let Err(error) = copy_result {
            return Err(staged.cleanup_after(error));
        }

        staged
            .promote(date, id, extension)
            .map(PromotedOriginal::commit)
    }

    pub fn begin_staged_original(&self, id: Uuid) -> Result<(StagedOriginal, File), AppError> {
        let path = self.staging.join(format!("{id}.part"));
        let file = open_staging_file(&path)?;
        Ok((
            StagedOriginal {
                paths: self.clone(),
                path,
                owned: true,
            },
            file,
        ))
    }

    pub(crate) fn persist_normalized_pdf(
        &self,
        id: Uuid,
        bytes: &[u8],
    ) -> Result<PathBuf, AppError> {
        let staging = self.staging.join(format!("{id}.normalized.part"));
        let destination = self.normalized.join(format!("{id}.pdf"));
        let mut file = open_staging_file(&staging)?;
        let write_result = (|| {
            file.write_all(bytes)
                .map_err(|error| internal_error("failed to write normalized PDF", error))?;
            file.sync_all()
                .map_err(|error| internal_error("failed to sync normalized PDF", error))
        })();
        drop(file);
        if let Err(error) = write_result {
            return match remove_file_durably(&staging) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(AppError::External {
                    service: "filesystem_sync".to_owned(),
                    retryable: false,
                    message: format!(
                        "normalized PDF staging cleanup was incomplete; manual recovery is required: original error: {error}; cleanup error: {cleanup_error}",
                    ),
                }),
            };
        }
        promote_staged_original(&staging, &destination)?;
        Ok(destination)
    }

    pub(crate) fn delete_normalized_pdf(&self, path: &Path) -> Result<(), AppError> {
        if !path.starts_with(&self.normalized) {
            return Err(AppError::Internal {
                message: "refusing to delete a file outside normalized storage".to_owned(),
            });
        }
        remove_file_durably(path)
    }

    pub fn delete_original(&self, path: impl AsRef<Path>) -> Result<(), AppError> {
        let path = path.as_ref();
        if !path.starts_with(&self.originals) {
            return Err(AppError::Internal {
                message: "refusing to delete a file outside original storage".to_owned(),
            });
        }
        let canonical_originals = fs::canonicalize(&self.originals)
            .map_err(|error| internal_error("failed to resolve original storage", error))?;
        let parent = path.parent().ok_or_else(|| AppError::Internal {
            message: "original path has no parent directory".to_owned(),
        })?;
        let canonical_parent = fs::canonicalize(parent)
            .map_err(|error| internal_error("failed to resolve original parent", error))?;
        if !canonical_parent.starts_with(canonical_originals) {
            return Err(AppError::Internal {
                message: "refusing to delete an original outside storage root".to_owned(),
            });
        }

        remove_file_durably(path)
    }
}

impl StagedOriginal {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn promote(
        mut self,
        date: NaiveDate,
        id: Uuid,
        extension: &str,
    ) -> Result<PromotedOriginal, AppError> {
        let destination = match self.paths.original_destination(date, id, extension) {
            Ok(destination) => destination,
            Err(error) => return Err(self.cleanup_after(error)),
        };
        self.owned = false;
        promote_staged_original(&self.path, &destination)?;
        Ok(PromotedOriginal {
            path: destination,
            owned: true,
        })
    }

    pub(crate) fn discard(mut self) -> Result<(), AppError> {
        self.owned = false;
        remove_file_durably(&self.path)
    }

    pub(crate) fn cleanup_after(mut self, error: AppError) -> AppError {
        self.owned = false;
        match remove_file_durably(&self.path) {
            Ok(()) => error,
            Err(cleanup_error) => AppError::External {
                service: "filesystem_sync".to_owned(),
                retryable: false,
                message: format!(
                    "staged original cleanup was incomplete; manual recovery is required: original error: {error}; cleanup error: {cleanup_error}",
                ),
            },
        }
    }
}

impl Drop for StagedOriginal {
    fn drop(&mut self) {
        if self.owned {
            let _ = remove_file_durably(&self.path);
        }
    }
}

impl PromotedOriginal {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn commit(mut self) -> PathBuf {
        self.owned = false;
        self.path.clone()
    }

    pub fn rollback(mut self) -> Result<(), AppError> {
        self.owned = false;
        remove_file_durably(&self.path)
    }
}

impl Drop for PromotedOriginal {
    fn drop(&mut self) {
        if self.owned {
            let _ = remove_file_durably(&self.path);
        }
    }
}

impl AppPaths {
    fn original_destination(
        &self,
        date: NaiveDate,
        id: Uuid,
        extension: &str,
    ) -> Result<PathBuf, AppError> {
        validate_extension(extension)?;
        let year_directory = self.originals.join(date.format("%Y").to_string());
        let destination_directory = year_directory.join(date.format("%m").to_string());
        let destination = destination_directory.join(format!("{id}.{extension}"));
        let canonical_originals = fs::canonicalize(&self.originals)
            .map_err(|error| internal_error("failed to resolve original storage", error))?;
        ensure_storage_directory(&canonical_originals, &year_directory)?;
        let canonical_year = fs::canonicalize(&year_directory)
            .map_err(|error| internal_error("failed to resolve original year", error))?;
        ensure_storage_directory(&canonical_year, &destination_directory)?;
        Ok(destination)
    }
}

fn open_staging_file(path: &Path) -> Result<File, AppError> {
    open_staging_file_with_permissions(path, set_private_file_permissions)
}

fn open_staging_file_with_permissions(
    path: &Path,
    set_permissions: impl FnOnce(&File) -> io::Result<()>,
) -> Result<File, AppError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let file = options.open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            AppError::Conflict {
                message: "original staging file already exists".to_owned(),
            }
        } else {
            internal_error("failed to create staged original", error)
        }
    })?;
    if let Err(permission_error) = set_permissions(&file) {
        drop(file);
        let cleanup_result = remove_file_durably(path);
        return Err(classify_staging_permission_failure(
            permission_error,
            cleanup_result,
        ));
    }

    Ok(file)
}

fn classify_staging_permission_failure(
    permission_error: io::Error,
    cleanup_result: Result<(), AppError>,
) -> AppError {
    match cleanup_result {
        Ok(()) => internal_error(
            "failed to secure staged original; created [staging] was removed",
            permission_error,
        ),
        Err(cleanup_error) => AppError::External {
            service: "filesystem_sync".to_owned(),
            retryable: false,
            message: format!(
                "failed to secure newly created [staging] and durable cleanup was incomplete; manual recovery is required: permission error: {permission_error}; cleanup error: {cleanup_error}",
            ),
        },
    }
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
    sync_promoted_directory: impl FnMut(&Path) -> io::Result<()>,
) -> Result<(), AppError> {
    promote_staged_original_with_rollback_sync(
        staging,
        destination,
        sync_promoted_directory,
        sync_directory,
    )
}

fn promote_staged_original_with_rollback_sync(
    staging: &Path,
    destination: &Path,
    mut sync_promoted_directory: impl FnMut(&Path) -> io::Result<()>,
    sync_rollback_directory: impl FnOnce(&Path) -> io::Result<()>,
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
                let destination_sync_result = sync_rollback_directory(destination_parent);
                let cleanup_result = remove_file_durably(staging);
                Err(classify_completed_rollback(
                    sync_error,
                    destination_sync_result,
                    cleanup_result,
                ))
            }
            Err(rollback_error) => Err(AppError::External {
                service: "filesystem_sync".to_owned(),
                retryable: false,
                message: format!(
                    "filesystem sync failed after promoting [destination]; rollback to [staging] also failed; manual recovery is required: sync error: {sync_error}; rollback error: {rollback_error}",
                ),
            }),
        };
    }

    Ok(())
}

fn classify_completed_rollback(
    promotion_sync_error: io::Error,
    destination_sync_result: io::Result<()>,
    cleanup_result: Result<(), AppError>,
) -> AppError {
    match (destination_sync_result, cleanup_result) {
        (Ok(()), Ok(())) => internal_error(
            "failed to sync promoted original; promotion was rolled back",
            promotion_sync_error,
        ),
        (destination_sync_result, cleanup_result) => {
            let destination_sync_detail = destination_sync_result
                .err()
                .map(|error| format!("failed: {error}"))
                .unwrap_or_else(|| "succeeded".to_owned());
            let cleanup_detail = cleanup_result
                .err()
                .map(|error| format!("failed: {error}"))
                .unwrap_or_else(|| "succeeded".to_owned());
            AppError::External {
                service: "filesystem_sync".to_owned(),
                retryable: false,
                message: format!(
                    "promotion sync failed and rollback reached [staging], but rollback durability is incomplete for [destination] and [staging]; manual recovery is required: promotion sync error: {promotion_sync_error}; destination parent sync {destination_sync_detail}; staging cleanup {cleanup_detail}",
                ),
            }
        }
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
pub(crate) fn rename_staged_original(staging: &Path, destination: &Path) -> Result<(), AppError> {
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
pub(crate) fn rename_staged_original(staging: &Path, destination: &Path) -> Result<(), AppError> {
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
pub(crate) fn rename_staged_original(_staging: &Path, _destination: &Path) -> Result<(), AppError> {
    Err(AppError::Internal {
        message: "atomic no-replace promotion is unsupported on this platform".to_owned(),
    })
}

pub(crate) fn remove_file_durably(path: &Path) -> Result<(), AppError> {
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
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> io::Result<()> {
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
mod tests;
