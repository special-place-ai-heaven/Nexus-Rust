use core::ffi::c_char;
use core::fmt;
use std::collections::HashMap;
use std::ffi::{CString, OsString};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{self, AtomicU64},
};

use nexus_platform::{PathIndex, PathKey};
use thiserror::Error;

use crate::{BackendFailure, BackendOperationError, NativeCallBoundary};

/// Stable native path storage with a bounded set of add-on-specific paths.
pub struct StablePathStore {
    game: CString,
    addons: CString,
    common: CString,
    addons_path: PathBuf,
    /// Advisory threshold; exceeding it is recorded, never refused.
    maximum_addon_paths: usize,
    addon_paths: Mutex<HashMap<OsString, Box<CString>>>,
    over_threshold_interns: AtomicU64,
}

impl StablePathStore {
    /// Copies the three legacy roots and prepares process-lifetime C strings.
    pub fn from_index(
        paths: &PathIndex,
        maximum_addon_paths: usize,
    ) -> Result<Self, StablePathError> {
        Self::new(
            paths.get(PathKey::GameDirectory),
            paths.get(PathKey::AddonsDirectory),
            paths.get(PathKey::CommonDirectory),
            maximum_addon_paths,
        )
    }

    fn new(
        game: &Path,
        addons: &Path,
        common: &Path,
        maximum_addon_paths: usize,
    ) -> Result<Self, StablePathError> {
        if maximum_addon_paths == 0 {
            return Err(StablePathError::InvalidCapacity);
        }
        Ok(Self {
            game: path_to_c_string(game)?,
            addons: path_to_c_string(addons)?,
            common: path_to_c_string(common)?,
            addons_path: addons.to_path_buf(),
            maximum_addon_paths,
            addon_paths: Mutex::new(HashMap::new()),
            over_threshold_interns: AtomicU64::new(0),
        })
    }

    /// Returns the process-lifetime game-directory pointer.
    #[must_use]
    pub fn game_directory(&self) -> *const c_char {
        self.game.as_ptr()
    }

    /// Returns the process-lifetime common-directory pointer.
    #[must_use]
    pub fn common_directory(&self) -> *const c_char {
        self.common.as_ptr()
    }

    /// Returns a stable add-on-directory pointer, interning non-empty names.
    pub fn addon_directory(&self, name: &str) -> Result<*const c_char, StablePathError> {
        if name.is_empty() {
            return Ok(self.addons.as_ptr());
        }

        let key = OsString::from(name);
        let mut paths = lock_unpoison(&self.addon_paths);
        if let Some(path) = paths.get(&key) {
            return Ok(path.as_ptr());
        }
        // The ceiling is a diagnostic threshold, not a rejection. The reference has no
        // ceiling and no failure path here: every call returns a valid NUL-terminated
        // pointer valid for the process lifetime (`ApiBuilder.cpp:175-215`). Refusing
        // instead hands an add-on a null pointer that it concatenates onto its own path,
        // which is a crash rather than a degraded result. Interning past the threshold
        // costs one path per distinct name — exactly the reference's growth — and is
        // strictly preferable to that.
        let over_threshold = paths.len() >= self.maximum_addon_paths;
        paths
            .try_reserve(1)
            .map_err(|_error| StablePathError::AllocationFailed)?;
        let path = Box::new(path_to_c_string(&self.addons_path.join(&key))?);
        let address = path.as_ptr();
        paths.insert(key, path);
        if over_threshold {
            self.over_threshold_interns
                .fetch_add(1, atomic::Ordering::Relaxed);
        }
        Ok(address)
    }

    /// Number of names interned past the advisory ceiling. Diagnostics only.
    #[must_use]
    pub fn over_threshold_interns(&self) -> u64 {
        self.over_threshold_interns.load(atomic::Ordering::Relaxed)
    }

    /// Returns the number of interned non-empty add-on names.
    #[must_use]
    pub fn interned_addon_paths(&self) -> usize {
        lock_unpoison(&self.addon_paths).len()
    }
}

/// Caller-attributed adapter for process-lifetime native path pointers.
pub struct PathApi {
    boundary: Arc<NativeCallBoundary>,
    paths: Arc<StablePathStore>,
}

impl PathApi {
    /// Creates an adapter around stable process path storage.
    #[must_use]
    pub fn new(boundary: Arc<NativeCallBoundary>, paths: Arc<StablePathStore>) -> Self {
        Self { boundary, paths }
    }

    /// Returns the process game-directory pointer for a current add-on caller.
    pub fn game_directory(&self) -> Result<*const c_char, BackendOperationError> {
        let _owner = self.boundary.resolve_owner(None)?;
        Ok(self.paths.game_directory())
    }

    /// Returns a stable add-on-directory pointer for a current add-on caller.
    pub fn addon_directory(
        &self,
        name: *const c_char,
    ) -> Result<*const c_char, BackendOperationError> {
        let _owner = self.boundary.resolve_owner(None)?;
        if name.is_null() {
            return self.paths.addon_directory("").map_err(|_| self.rejected());
        }
        let name = self.boundary.snapshot_path(name)?;
        self.paths
            .addon_directory(name.as_str())
            .map_err(|_| self.rejected())
    }

    /// Returns the process common-directory pointer for a current add-on caller.
    pub fn common_directory(&self) -> Result<*const c_char, BackendOperationError> {
        let _owner = self.boundary.resolve_owner(None)?;
        Ok(self.paths.common_directory())
    }

    fn rejected(&self) -> BackendOperationError {
        self.boundary
            .failures()
            .record(BackendFailure::ServiceRejected);
        BackendOperationError::ServiceRejected
    }
}

impl fmt::Debug for PathApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PathApi")
            .field("boundary", &self.boundary)
            .field("interned_addon_paths", &self.paths.interned_addon_paths())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for StablePathStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StablePathStore")
            .field("maximum_addon_paths", &self.maximum_addon_paths)
            .field("interned_addon_paths", &self.interned_addon_paths())
            .finish()
    }
}

/// Redacted stable-path construction or interning failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum StablePathError {
    /// The configured interning capacity was zero.
    #[error("stable path capacity is invalid")]
    InvalidCapacity,
    /// A native path contained an interior NUL byte.
    #[error("native path cannot be represented as a C string")]
    InteriorNul,
    /// The path registry could not reserve another entry.
    #[error("stable path allocation failed")]
    AllocationFailed,
}

fn path_to_c_string(path: &Path) -> Result<CString, StablePathError> {
    CString::new(path.to_string_lossy().as_bytes()).map_err(|_error| StablePathError::InteriorNul)
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;
    use std::path::Path;

    use super::{StablePathError, StablePathStore};

    fn store(maximum: usize) -> StablePathStore {
        StablePathStore::new(
            Path::new("C:/game"),
            Path::new("C:/game/addons"),
            Path::new("C:/game/addons/common"),
            maximum,
        )
        .expect("test path store")
    }

    #[test]
    fn pointers_remain_stable_while_the_bounded_registry_grows() {
        let paths = store(128);
        let first = paths.addon_directory("first").expect("first path");
        for index in 0..100 {
            paths
                .addon_directory(&format!("addon-{index}"))
                .expect("interned path");
        }
        assert_eq!(first, paths.addon_directory("first").expect("same path"));
        // SAFETY: `first` points into boxed storage retained by `paths`.
        let value = unsafe { CStr::from_ptr(first) };
        assert_eq!(
            value.to_string_lossy(),
            Path::new("C:/game/addons").join("first").to_string_lossy()
        );
    }

    /// The reference has no ceiling and no failure path for the directory getters, and an
    /// add-on concatenates whatever it is handed. Past the advisory threshold a name must
    /// therefore still intern to a valid, stable pointer rather than being refused.
    #[test]
    fn the_capacity_threshold_records_rather_than_refuses() {
        let paths = store(1);
        assert_eq!(
            paths.addon_directory("").expect("root"),
            paths.addons.as_ptr()
        );
        let retained = paths.addon_directory("retained").expect("retained");
        assert_eq!(paths.over_threshold_interns(), 0);

        let beyond = paths
            .addon_directory("second")
            .expect("a name past the threshold must still resolve, never return null");
        assert!(!beyond.is_null());
        assert_ne!(beyond, retained);
        assert_eq!(paths.over_threshold_interns(), 1);

        // Interning past the threshold must not evict or move an earlier pointer.
        assert_eq!(
            paths.addon_directory("retained").expect("still retained"),
            retained
        );
        // SAFETY: `beyond` points into boxed storage retained by `paths`.
        let value = unsafe { CStr::from_ptr(beyond) };
        assert_eq!(
            value.to_string_lossy(),
            Path::new("C:/game/addons").join("second").to_string_lossy()
        );
    }

    #[test]
    fn debug_and_errors_never_expose_path_contents() {
        let paths = store(1);
        let debug = format!("{paths:?}");
        assert!(!debug.contains("C:/game"));
        assert!(!format!("{:?}", StablePathError::InteriorNul).contains("C:/"));
    }
}
