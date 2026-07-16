use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::filesystem::{FileSystem, FileSystemError};

#[derive(Default)]
pub(crate) struct TestFileSystem {
    files: RefCell<HashMap<PathBuf, Vec<u8>>>,
    directories: RefCell<HashSet<PathBuf>>,
    move_calls: Cell<usize>,
    failed_move_calls: RefCell<HashSet<usize>>,
    fail_writes: Cell<bool>,
}

impl TestFileSystem {
    pub(crate) fn put(&self, path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) {
        self.files.borrow_mut().insert(path.into(), bytes.into());
    }

    pub(crate) fn get(&self, path: impl AsRef<Path>) -> Option<Vec<u8>> {
        self.files.borrow().get(path.as_ref()).cloned()
    }

    pub(crate) fn contains(&self, path: impl AsRef<Path>) -> bool {
        self.files.borrow().contains_key(path.as_ref())
    }

    pub(crate) fn fail_move_calls(&self, calls: impl IntoIterator<Item = usize>) {
        self.failed_move_calls.borrow_mut().extend(calls);
    }

    pub(crate) fn fail_writes(&self, fail: bool) {
        self.fail_writes.set(fail);
    }
}

impl FileSystem for TestFileSystem {
    fn read_bounded(&self, path: &Path, limit: usize) -> Result<Option<Vec<u8>>, FileSystemError> {
        let Some(bytes) = self.files.borrow().get(path).cloned() else {
            return Ok(None);
        };
        if bytes.len() > limit {
            return Err(FileSystemError::LimitExceeded);
        }
        Ok(Some(bytes))
    }

    fn file_len(&self, path: &Path) -> Result<Option<u64>, FileSystemError> {
        self.files
            .borrow()
            .get(path)
            .map(|bytes| {
                u64::try_from(bytes.len()).map_err(|_error| FileSystemError::OperationFailed)
            })
            .transpose()
    }

    fn write_atomic(&self, path: &Path, contents: &[u8]) -> Result<(), FileSystemError> {
        if self.fail_writes.get() {
            return Err(FileSystemError::OperationFailed);
        }
        self.files
            .borrow_mut()
            .insert(path.to_path_buf(), contents.to_vec());
        Ok(())
    }

    fn move_noreplace(&self, from: &Path, to: &Path) -> Result<(), FileSystemError> {
        let call = self.move_calls.get() + 1;
        self.move_calls.set(call);
        if self.failed_move_calls.borrow().contains(&call) {
            return Err(FileSystemError::OperationFailed);
        }

        let mut files = self.files.borrow_mut();
        if files.contains_key(to) {
            return Err(FileSystemError::AlreadyExists);
        }
        let Some(bytes) = files.remove(from) else {
            return Err(FileSystemError::NotFound);
        };
        files.insert(to.to_path_buf(), bytes);
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> Result<(), FileSystemError> {
        self.files.borrow_mut().remove(path);
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), FileSystemError> {
        self.directories.borrow_mut().insert(path.to_path_buf());
        Ok(())
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), FileSystemError> {
        self.files
            .borrow_mut()
            .retain(|entry, _bytes| !entry.starts_with(path));
        self.directories
            .borrow_mut()
            .retain(|entry| !entry.starts_with(path));
        Ok(())
    }
}
