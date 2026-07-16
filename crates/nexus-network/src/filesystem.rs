//! Redaction-safe filesystem operations used by cache and update transactions.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A path-free filesystem failure suitable for logs and API boundaries.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FileSystemError {
    /// A requested source entry did not exist.
    #[error("filesystem entry was not found")]
    NotFound,
    /// A no-clobber destination already existed.
    #[error("filesystem destination already exists")]
    AlreadyExists,
    /// The process lacked permission for the operation.
    #[error("filesystem permission was denied")]
    PermissionDenied,
    /// The backing device could not accept more data.
    #[error("filesystem capacity was exhausted")]
    CapacityExhausted,
    /// A read exceeded its caller-provided bound.
    #[error("filesystem data exceeded the configured bound")]
    LimitExceeded,
    /// The operation failed for another reason.
    #[error("filesystem operation failed")]
    OperationFailed,
}

/// Filesystem boundary required by persistent cache and update transactions.
///
/// Implementations must keep [`FileSystem::write_atomic`] atomic with respect to
/// readers and must not overwrite the destination of
/// [`FileSystem::move_noreplace`].
pub trait FileSystem {
    /// Reads at most `limit` bytes, returning `None` when the path is absent.
    fn read_bounded(&self, path: &Path, limit: usize) -> Result<Option<Vec<u8>>, FileSystemError>;

    /// Returns a regular file's length, or `None` when it is absent.
    fn file_len(&self, path: &Path) -> Result<Option<u64>, FileSystemError>;

    /// Atomically replaces a file with `contents` using a same-directory stage.
    fn write_atomic(&self, path: &Path, contents: &[u8]) -> Result<(), FileSystemError>;

    /// Moves a file while refusing to overwrite an existing destination.
    fn move_noreplace(&self, from: &Path, to: &Path) -> Result<(), FileSystemError>;

    /// Removes a file; an absent file is treated as success.
    fn remove_file(&self, path: &Path) -> Result<(), FileSystemError>;

    /// Creates a directory and all missing parents.
    fn create_dir_all(&self, path: &Path) -> Result<(), FileSystemError>;

    /// Removes a directory tree; an absent directory is treated as success.
    fn remove_dir_all(&self, path: &Path) -> Result<(), FileSystemError>;
}

/// Production filesystem implementation using the Rust standard library.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdFileSystem;

impl FileSystem for StdFileSystem {
    fn read_bounded(&self, path: &Path, limit: usize) -> Result<Option<Vec<u8>>, FileSystemError> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(map_io_error(&error)),
        };

        if let Ok(metadata) = file.metadata()
            && metadata.len() > usize_to_u64(limit)
        {
            return Err(FileSystemError::LimitExceeded);
        }

        let read_limit = usize_to_u64(limit).saturating_add(1);
        let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
        Read::by_ref(&mut file)
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|error| map_io_error(&error))?;

        if bytes.len() > limit {
            return Err(FileSystemError::LimitExceeded);
        }

        Ok(Some(bytes))
    }

    fn file_len(&self, path: &Path) -> Result<Option<u64>, FileSystemError> {
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
            Ok(_) => Err(FileSystemError::OperationFailed),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(map_io_error(&error)),
        }
    }

    fn write_atomic(&self, path: &Path, contents: &[u8]) -> Result<(), FileSystemError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|error| map_io_error(&error))?;

        let (temporary_path, mut temporary_file) = create_temporary_file(path)?;
        let write_result = (|| -> Result<(), FileSystemError> {
            temporary_file
                .write_all(contents)
                .map_err(|error| map_io_error(&error))?;
            temporary_file
                .sync_all()
                .map_err(|error| map_io_error(&error))?;
            drop(temporary_file);
            replace_file(&temporary_path, path).map_err(|error| map_io_error(&error))?;
            Ok(())
        })();

        if write_result.is_err() {
            let _ignored = fs::remove_file(&temporary_path);
        }

        write_result
    }

    fn move_noreplace(&self, from: &Path, to: &Path) -> Result<(), FileSystemError> {
        // Creating a hard link gives us an atomic no-clobber operation on the
        // same volume. Removing the source then completes the logical move.
        fs::hard_link(from, to).map_err(|error| map_io_error(&error))?;

        if let Err(error) = fs::remove_file(from) {
            let _ignored = fs::remove_file(to);
            return Err(map_io_error(&error));
        }

        Ok(())
    }

    fn remove_file(&self, path: &Path) -> Result<(), FileSystemError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(map_io_error(&error)),
        }
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), FileSystemError> {
        fs::create_dir_all(path).map_err(|error| map_io_error(&error))
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), FileSystemError> {
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(map_io_error(&error)),
        }
    }
}

fn create_temporary_file(target: &Path) -> Result<(PathBuf, File), FileSystemError> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let stem = target
        .file_name()
        .map_or_else(|| OsString::from("entry"), OsString::from);

    for _attempt in 0..128 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut name = stem.clone();
        name.push(format!(".nexus-{sequence:016x}.tmp"));
        let path = parent.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(map_io_error(&error)),
        }
    }

    Err(FileSystemError::AlreadyExists)
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();

    // SAFETY: both buffers are valid, NUL-terminated UTF-16 paths and remain
    // alive for the duration of the call. The flags request an atomic replace
    // on the same volume and synchronous metadata completion.
    let succeeded = unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };

    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn map_io_error(error: &io::Error) -> FileSystemError {
    match error.kind() {
        io::ErrorKind::NotFound => FileSystemError::NotFound,
        io::ErrorKind::AlreadyExists => FileSystemError::AlreadyExists,
        io::ErrorKind::PermissionDenied => FileSystemError::PermissionDenied,
        io::ErrorKind::StorageFull | io::ErrorKind::FileTooLarge => {
            FileSystemError::CapacityExhausted
        }
        _ => FileSystemError::OperationFailed,
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    use super::{FileSystem, FileSystemError, StdFileSystem, TEMP_FILE_SEQUENCE};

    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    fn temporary_directory() -> (PathBuf, Cleanup) {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nexus-network-test-{}-{sequence:016x}",
            std::process::id()
        ));
        (path.clone(), Cleanup(path))
    }

    #[test]
    fn standard_filesystem_atomically_replaces_and_bounds_reads() {
        let (directory, _cleanup) = temporary_directory();
        let filesystem = StdFileSystem;
        assert!(filesystem.create_dir_all(&directory).is_ok());
        let target = directory.join("cache.json");
        assert!(filesystem.write_atomic(&target, b"old").is_ok());
        assert!(filesystem.write_atomic(&target, b"new body").is_ok());
        assert_eq!(
            filesystem.read_bounded(&target, 8),
            Ok(Some(b"new body".to_vec()))
        );
        assert_eq!(
            filesystem.read_bounded(&target, 7),
            Err(FileSystemError::LimitExceeded)
        );

        let read_directory = fs::read_dir(&directory);
        let Ok(read_directory) = read_directory else {
            panic!("test directory should remain readable");
        };
        let names: Vec<_> = read_directory
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        assert_eq!(names, vec!["cache.json"]);
    }

    #[test]
    fn standard_filesystem_no_clobber_move_preserves_both_sources_on_conflict() {
        let (directory, _cleanup) = temporary_directory();
        let filesystem = StdFileSystem;
        assert!(filesystem.create_dir_all(&directory).is_ok());
        let source = directory.join("source.dll");
        let destination = directory.join("destination.dll");
        assert!(filesystem.write_atomic(&source, b"source").is_ok());
        assert!(
            filesystem
                .write_atomic(&destination, b"destination")
                .is_ok()
        );

        assert_eq!(
            filesystem.move_noreplace(&source, &destination),
            Err(FileSystemError::AlreadyExists)
        );
        assert_eq!(
            filesystem.read_bounded(&source, 32),
            Ok(Some(b"source".to_vec()))
        );
        assert_eq!(
            filesystem.read_bounded(&destination, 32),
            Ok(Some(b"destination".to_vec()))
        );
    }

    #[test]
    fn standard_filesystem_removals_are_idempotent() {
        let (directory, _cleanup) = temporary_directory();
        let filesystem = StdFileSystem;
        assert!(
            filesystem
                .remove_file(Path::new("definitely-absent-file"))
                .is_ok()
        );
        assert!(filesystem.remove_dir_all(&directory).is_ok());
        assert!(filesystem.remove_dir_all(&directory).is_ok());
    }
}
