use core::ffi::c_void;
use core::num::NonZeroUsize;
use core::ptr;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};

use crate::mapping::{MappingBackend, MappingDisposition, MappingFailure, MappingView};

const MAX_NAME_UNITS: usize = 260;

/// Win32 page-file-backed named-mapping backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsMappingBackend;

struct WindowsMappingView {
    handle: NonZeroUsize,
    address: NonZeroUsize,
    len: usize,
    disposition: MappingDisposition,
}

// SAFETY: successful Win32 calls return a stable writable view of `len` bytes.
// Integer tokens retain the mapping handle/address until `Drop` releases each
// process resource exactly once.
unsafe impl MappingView for WindowsMappingView {
    fn address(&self) -> NonZeroUsize {
        self.address
    }

    fn len(&self) -> usize {
        self.len
    }

    fn disposition(&self) -> MappingDisposition {
        self.disposition
    }
}

impl MappingBackend for WindowsMappingBackend {
    fn open_or_create(
        &self,
        name: &str,
        size: NonZeroUsize,
    ) -> Result<Arc<dyn MappingView>, MappingFailure> {
        let wide_name = wide_name(OsStr::new(name))?;
        let mapping_size =
            u32::try_from(size.get()).map_err(|_| MappingFailure::SizeUnsupported)?;

        // SAFETY: `wide_name` is nul-terminated and remains live for the call.
        let mut handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide_name.as_ptr()) };
        let mut disposition = MappingDisposition::OpenedExisting;
        if handle.is_null() {
            // SAFETY: the invalid-file sentinel requests a page-file-backed
            // mapping; security is default; the name is live and terminated.
            handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    ptr::null(),
                    PAGE_READWRITE,
                    0,
                    mapping_size,
                    wide_name.as_ptr(),
                )
            };
            if handle.is_null() {
                // SAFETY: `GetLastError` has no preconditions.
                let code = unsafe { GetLastError() };
                return Err(MappingFailure::OpenOrCreate { code });
            }
            // SAFETY: successful creation sets this thread's last-error value.
            let status = unsafe { GetLastError() };
            disposition = if status == ERROR_ALREADY_EXISTS {
                MappingDisposition::OpenedExisting
            } else {
                MappingDisposition::CreatedNew
            };
        }

        // SAFETY: `handle` is live and the requested non-zero exact view size
        // fits the mapping creation contract.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size.get()) };
        let Some(address) = NonZeroUsize::new(view.Value as usize) else {
            // Capture the mapping error before cleanup changes last-error state.
            // SAFETY: `GetLastError` has no preconditions.
            let code = unsafe { GetLastError() };
            // SAFETY: `handle` is live and owned by this function.
            let _ = unsafe { CloseHandle(handle) };
            return Err(MappingFailure::MapView { code });
        };
        let Some(handle) = NonZeroUsize::new(handle as usize) else {
            // Defensive only: the handle was checked non-null above.
            // SAFETY: `view` was returned by successful `MapViewOfFile`.
            let _ = unsafe { UnmapViewOfFile(view) };
            return Err(MappingFailure::OpenOrCreate { code: 0 });
        };

        Ok(Arc::new(WindowsMappingView {
            handle,
            address,
            len: size.get(),
            disposition,
        }))
    }
}

impl Drop for WindowsMappingView {
    fn drop(&mut self) {
        let view = MEMORY_MAPPED_VIEW_ADDRESS {
            Value: self.address.get() as *mut c_void,
        };
        // SAFETY: both tokens came from successful Win32 calls and this owner
        // releases each exactly once.
        let _ = unsafe { UnmapViewOfFile(view) };
        // SAFETY: Win32 handles are pointer-sized on the supported target.
        let _ = unsafe { CloseHandle(self.handle.get() as *mut c_void) };
    }
}

fn wide_name(name: &OsStr) -> Result<Vec<u16>, MappingFailure> {
    let mut units = Vec::new();
    for unit in name.encode_wide() {
        if unit == 0 {
            return Err(MappingFailure::EmbeddedNul);
        }
        if units.len() >= MAX_NAME_UNITS - 1 {
            return Err(MappingFailure::NameTooLong);
        }
        units.push(unit);
    }
    units.push(0);
    Ok(units)
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use crate::data_link::DataLinkService;
    use crate::mapping::{MappingBackend, MappingDisposition};

    use super::WindowsMappingBackend;

    static NEXT_MAPPING: AtomicU64 = AtomicU64::new(1);

    fn unique_name() -> String {
        format!(
            "Local\\NexusRustDataServices_{}_{}",
            std::process::id(),
            NEXT_MAPPING.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn real_mapping_reopen_preserves_existing_bytes() {
        let backend: Arc<dyn MappingBackend> = Arc::new(WindowsMappingBackend);
        let name = unique_name();
        let first_service = DataLinkService::new(Arc::clone(&backend));
        let first = first_service
            .share_public("DL_WINDOWS_FIRST", 64, Some(&name))
            .expect("the first Win32 mapping should open");
        assert_eq!(
            first.mapping_disposition(),
            Some(MappingDisposition::CreatedNew)
        );
        // SAFETY: the retained mapping exposes at least one writable byte.
        unsafe { first.as_mut_ptr().cast::<u8>().write(0x5A) };

        let second_service = DataLinkService::new(backend);
        let second = second_service
            .share_public("DL_WINDOWS_SECOND", 64, Some(&name))
            .expect("the existing Win32 mapping should reopen");
        assert_eq!(
            second.mapping_disposition(),
            Some(MappingDisposition::OpenedExisting)
        );
        // SAFETY: the retained mapping exposes at least one readable byte.
        assert_eq!(unsafe { second.as_mut_ptr().cast::<u8>().read() }, 0x5A);
    }
}
