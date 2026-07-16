use core::ffi::c_void;
use core::mem::{MaybeUninit, offset_of, size_of};
use core::num::NonZeroUsize;
use core::ptr;
use core::sync::atomic::{Ordering, compiler_fence};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use nexus_abi::MumbleData;
use thiserror::Error;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};

use crate::{MumbleSource, SnapshotError};

const MAX_NAME_UNITS: usize = 32_767;
const SNAPSHOT_ATTEMPTS: usize = 4;

/// Whether opening a named mapping reused an object or created a zeroed one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingDisposition {
    /// An existing named mapping was opened without modifying its contents.
    OpenedExisting,
    /// A new page-file-backed mapping was created; Windows supplies zeroed pages.
    CreatedNew,
}

/// Redaction-safe failures while opening a named MumbleLink mapping.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MappingOpenError {
    /// The caller-supplied name contains an interior nul.
    #[error("the shared-memory name contains an embedded nul")]
    EmbeddedNul,
    /// The name exceeds the Win32 object-manager limit used by this runtime.
    #[error("the shared-memory name is too long")]
    NameTooLong,
    /// Win32 could neither open nor create the mapping.
    #[error("Win32 could not open or create the MumbleLink mapping (error {code})")]
    OpenOrCreate {
        /// Win32 error code.
        code: u32,
    },
    /// Win32 could not map a view of the complete ABI object.
    #[error("Win32 could not map the MumbleLink view (error {code})")]
    MapView {
        /// Win32 error code.
        code: u32,
    },
}

/// Owned lifetime for a writable named MumbleLink mapping.
///
/// The mapping is represented by integer handle/address tokens so this owner
/// can move between threads. Win32 file-mapping handles and views are process
/// resources and may be closed/unmapped from a different thread.
pub struct MappedMumbleLink {
    handle: NonZeroUsize,
    address: NonZeroUsize,
    disposition: MappingDisposition,
}

impl MappedMumbleLink {
    /// Opens an existing mapping or creates a new zero-initialized mapping.
    ///
    /// Existing contents are never cleared. This deliberately fixes the
    /// legacy initialization race that zeroed Guild Wars 2 telemetry after a
    /// successful `OpenFileMapping`.
    pub fn open_or_create(name: &OsStr) -> Result<Self, MappingOpenError> {
        let name = wide_name(name)?;
        // SAFETY: `name` is nul-terminated and remains alive for this call.
        let mut handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, name.as_ptr()) };
        let mut disposition = MappingDisposition::OpenedExisting;
        if handle.is_null() {
            let mapping_size = u32::try_from(size_of::<MumbleData>())
                .map_err(|_| MappingOpenError::OpenOrCreate { code: 0 })?;
            // SAFETY: the invalid-file sentinel requests a page-file-backed mapping;
            // the security pointer is null; `name` is a valid nul-terminated string.
            handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    ptr::null(),
                    PAGE_READWRITE,
                    0,
                    mapping_size,
                    name.as_ptr(),
                )
            };
            if handle.is_null() {
                // SAFETY: `GetLastError` has no preconditions.
                let code = unsafe { GetLastError() };
                return Err(MappingOpenError::OpenOrCreate { code });
            }
            // SAFETY: the successful creation call sets this thread's last error.
            let creation_status = unsafe { GetLastError() };
            disposition = if creation_status == ERROR_ALREADY_EXISTS {
                MappingDisposition::OpenedExisting
            } else {
                MappingDisposition::CreatedNew
            };
        }

        // SAFETY: `handle` is a live file-mapping handle. Mapping the exact ABI
        // size either succeeds or returns a null view.
        let view =
            unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size_of::<MumbleData>()) };
        let Some(address) = NonZeroUsize::new(view.Value as usize) else {
            // Capture the mapping failure before cleanup can change last-error state.
            // SAFETY: `GetLastError` has no preconditions.
            let code = unsafe { GetLastError() };
            // SAFETY: `handle` is live and owned by this function.
            let _ = unsafe { CloseHandle(handle) };
            return Err(MappingOpenError::MapView { code });
        };
        let Some(handle) = NonZeroUsize::new(handle as usize) else {
            // A non-null handle was checked above; this branch is defensive.
            // SAFETY: `view` is a mapped view returned by Win32.
            let _ = unsafe { UnmapViewOfFile(view) };
            return Err(MappingOpenError::OpenOrCreate { code: 0 });
        };

        Ok(Self {
            handle,
            address,
            disposition,
        })
    }

    /// Reports whether this call created the mapping.
    #[must_use]
    pub const fn disposition(&self) -> MappingDisposition {
        self.disposition
    }

    /// Returns the shared ABI pointer for compatibility publication.
    ///
    /// The pointer becomes invalid when this owner is dropped. Dereferencing
    /// it is unsafe because Guild Wars 2 may write the mapping concurrently.
    #[must_use]
    pub fn as_ptr(&self) -> *mut MumbleData {
        self.address.get() as *mut MumbleData
    }
}

impl MumbleSource for MappedMumbleLink {
    fn snapshot(&self) -> Result<MumbleData, SnapshotError> {
        for _ in 0..SNAPSHOT_ATTEMPTS {
            let before = self.read_tick();
            let snapshot = self.copy_volatile();
            let after = self.read_tick();
            if before == after && snapshot.ui_tick == before {
                return Ok(snapshot);
            }
        }
        Err(SnapshotError::Unstable)
    }
}

impl MappedMumbleLink {
    fn read_tick(&self) -> u32 {
        let address = self.address.get() + offset_of!(MumbleData, ui_tick);
        // SAFETY: the owned view covers a complete `MumbleData`; volatile access
        // is required because an external process writes this shared memory.
        unsafe { ptr::read_volatile(address as *const u32) }
    }

    fn copy_volatile(&self) -> MumbleData {
        let source = self.address.get() as *const u8;
        let mut snapshot = MaybeUninit::<MumbleData>::uninit();
        let destination = snapshot.as_mut_ptr().cast::<u8>();
        compiler_fence(Ordering::SeqCst);
        for offset in 0..size_of::<MumbleData>() {
            // SAFETY: both source and destination cover `size_of::<MumbleData>()`
            // bytes. Every bit pattern in the ABI type is valid because open
            // enums and C booleans are represented by integers.
            unsafe {
                destination
                    .add(offset)
                    .write(ptr::read_volatile(source.add(offset)));
            }
        }
        compiler_fence(Ordering::SeqCst);
        // SAFETY: the loop initialized every byte and every bit pattern is valid.
        unsafe { snapshot.assume_init() }
    }
}

impl Drop for MappedMumbleLink {
    fn drop(&mut self) {
        let view = MEMORY_MAPPED_VIEW_ADDRESS {
            Value: self.address.get() as *mut c_void,
        };
        // SAFETY: both tokens were returned by successful Win32 calls and are
        // released exactly once here.
        let _ = unsafe { UnmapViewOfFile(view) };
        // SAFETY: see above; Win32 `HANDLE` is pointer-sized on this target.
        let _ = unsafe { CloseHandle(self.handle.get() as *mut c_void) };
    }
}

fn wide_name(name: &OsStr) -> Result<Vec<u16>, MappingOpenError> {
    let mut units = Vec::new();
    for unit in name.encode_wide() {
        if unit == 0 {
            return Err(MappingOpenError::EmbeddedNul);
        }
        if units.len() >= MAX_NAME_UNITS - 1 {
            return Err(MappingOpenError::NameTooLong);
        }
        units.push(unit);
    }
    units.push(0);
    Ok(units)
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::sync::atomic::{AtomicU64, Ordering};

    use nexus_abi::{MumbleData, MumbleVector3};

    use super::{MappedMumbleLink, MappingDisposition, MappingOpenError};
    use crate::MumbleSource;

    static NEXT_MAPPING: AtomicU64 = AtomicU64::new(1);

    fn unique_name() -> OsString {
        let sequence = NEXT_MAPPING.fetch_add(1, Ordering::Relaxed);
        OsString::from(format!(
            "Local\\NexusRustMumbleTest_{}_{}",
            std::process::id(),
            sequence
        ))
    }

    #[test]
    fn new_mapping_is_zeroed_and_reopen_does_not_clear_it() {
        let name = unique_name();
        let first = MappedMumbleLink::open_or_create(&name);
        assert!(first.is_ok());
        let first = first.unwrap_or_else(|error| panic!("mapping failed: {error}"));
        assert_eq!(first.disposition(), MappingDisposition::CreatedNew);
        let initial = first.snapshot();
        assert!(initial.is_ok());
        assert_eq!(initial.unwrap_or_default(), MumbleData::default());

        // SAFETY: the test owns a writable mapping and no other thread accesses it.
        unsafe {
            (*first.as_ptr()).ui_tick = 7;
            (*first.as_ptr()).avatar_position = MumbleVector3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            };
        }
        let second = MappedMumbleLink::open_or_create(&name);
        assert!(second.is_ok());
        let second = second.unwrap_or_else(|error| panic!("mapping failed: {error}"));
        assert_eq!(second.disposition(), MappingDisposition::OpenedExisting);
        let snapshot = second.snapshot();
        assert!(snapshot.is_ok());
        let snapshot = snapshot.unwrap_or_default();
        assert_eq!(snapshot.ui_tick, 7);
        assert_eq!(snapshot.avatar_position.x, 1.0);
    }

    #[test]
    fn mapping_name_errors_are_closed_and_redaction_safe() {
        let invalid = OsStr::new("invalid\0name");
        assert_eq!(
            MappedMumbleLink::open_or_create(invalid).err(),
            Some(MappingOpenError::EmbeddedNul)
        );
    }
}
