use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tempfile::Builder;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DurableFsError {
    #[error("path has no parent: {0}")]
    MissingParent(PathBuf),
    #[error("refusing to replace a symbolic link or reparse point: {0}")]
    UnsafeTarget(PathBuf),
    #[error("another Codex Switch instance is already changing files")]
    AlreadyLocked,
    #[error("file operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub struct ExclusiveLock {
    _file: File,
}

pub fn ensure_private_dir(path: &Path) -> Result<(), DurableFsError> {
    fs::create_dir_all(path).map_err(|source| io_error(path, source))?;
    set_private_dir_permissions(path)?;
    set_hidden_if_supported(path)?;
    Ok(())
}

pub fn acquire_lock(path: &Path) -> Result<ExclusiveLock, DurableFsError> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    set_private_file_permissions(path)?;
    file.try_lock().map_err(|source| match source {
        std::fs::TryLockError::WouldBlock => DurableFsError::AlreadyLocked,
        std::fs::TryLockError::Error(source) => io_error(path, source),
    })?;

    Ok(ExclusiveLock { _file: file })
}

pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, DurableFsError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error(path, source)),
    }
}

pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), DurableFsError> {
    reject_unsafe_target(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| DurableFsError::MissingParent(path.to_path_buf()))?;
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;

    let mut staged = Builder::new()
        .prefix(".codex-switch-")
        .tempfile_in(parent)
        .map_err(|source| io_error(parent, source))?;
    staged
        .write_all(contents)
        .and_then(|_| staged.flush())
        .map_err(|source| io_error(staged.path(), source))?;

    set_private_file_permissions(staged.path())?;
    staged
        .as_file()
        .sync_all()
        .map_err(|source| io_error(staged.path(), source))?;

    persist_staged(staged, path)?;
    set_private_file_permissions(path)?;
    sync_parent(parent)?;
    Ok(())
}

pub fn atomic_remove(path: &Path) -> Result<(), DurableFsError> {
    reject_unsafe_target(path)?;
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_parent(parent)?;
            }
            Ok(())
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

pub fn copy_private(source: &Path, destination: &Path) -> Result<(), DurableFsError> {
    let bytes = fs::read(source).map_err(|source_error| io_error(source, source_error))?;
    atomic_write(destination, &bytes)
}

pub fn revision(contents: Option<&[u8]>) -> String {
    let mut hasher = Sha256::new();
    match contents {
        Some(bytes) => {
            hasher.update([1]);
            hasher.update(bytes);
        }
        None => hasher.update([0]),
    }
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

pub fn sync_directory(path: &Path) -> Result<(), DurableFsError> {
    sync_parent(path)
}

fn reject_unsafe_target(path: &Path) -> Result<(), DurableFsError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(path, source)),
    };

    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(DurableFsError::UnsafeTarget(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> Result<(), DurableFsError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> Result<(), DurableFsError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), DurableFsError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), DurableFsError> {
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), DurableFsError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), DurableFsError> {
    Ok(())
}

#[cfg(not(windows))]
fn persist_staged(staged: tempfile::NamedTempFile, path: &Path) -> Result<(), DurableFsError> {
    staged
        .persist(path)
        .map(|_| ())
        .map_err(|error| io_error(path, error.error))
}

#[cfg(windows)]
fn persist_staged(staged: tempfile::NamedTempFile, path: &Path) -> Result<(), DurableFsError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    if !path.exists() {
        return staged
            .persist(path)
            .map(|_| ())
            .map_err(|error| io_error(path, error.error));
    }

    let staged_path = staged.path().to_path_buf();
    let (_file, kept_path) = staged
        .keep()
        .map_err(|error| io_error(&staged_path, error.error))?;
    drop(_file);

    let destination: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let replacement: Vec<u16> = kept_path.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        let source = io::Error::last_os_error();
        let _ = fs::remove_file(&kept_path);
        return Err(io_error(path, source));
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn set_hidden_if_supported(path: &Path) -> Result<(), DurableFsError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_HIDDEN, GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW,
    };

    let encoded: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let attributes = unsafe { GetFileAttributesW(encoded.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return Err(io_error(path, io::Error::last_os_error()));
    }
    let result =
        unsafe { SetFileAttributesW(encoded.as_ptr(), attributes | FILE_ATTRIBUTE_HIDDEN) };
    if result == 0 {
        return Err(io_error(path, io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(windows))]
fn set_hidden_if_supported(_path: &Path) -> Result<(), DurableFsError> {
    Ok(())
}

fn io_error(path: &Path, source: io::Error) -> DurableFsError {
    DurableFsError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_contents() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("state.json");
        atomic_write(&target, b"one").unwrap();
        atomic_write(&target, b"two").unwrap();
        assert_eq!(fs::read(target).unwrap(), b"two");
    }

    #[test]
    fn revisions_distinguish_missing_and_empty() {
        assert_ne!(revision(None), revision(Some(b"")));
        assert_eq!(revision(Some(b"same")), revision(Some(b"same")));
    }

    #[cfg(unix)]
    #[test]
    fn private_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("secret");
        atomic_write(&target, b"key").unwrap();
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_public_file_tightens_its_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("auth.json");
        fs::write(&target, b"old key").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();

        atomic_write(&target, b"new key").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new key");
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
