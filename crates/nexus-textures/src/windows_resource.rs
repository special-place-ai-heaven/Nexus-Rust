use core::ptr;

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    FindResourceW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GetModuleHandleExW, LoadResource,
    LockResource, SizeofResource,
};

use crate::{BackendFailure, ModuleHandle, ResourceProvider};

const PNG_RESOURCE_TYPE: [u16; 4] = [b'P' as u16, b'N' as u16, b'G' as u16, 0];

/// Copies PNG resources while holding a temporary reference to the source module.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsResourceProvider;

struct PinnedModule(HMODULE);

impl Drop for PinnedModule {
    fn drop(&mut self) {
        // SAFETY: `PinnedModule` is created only from a successful
        // `GetModuleHandleExW` call which grants exactly one reference.
        unsafe {
            FreeLibrary(self.0);
        }
    }
}

impl ResourceProvider for WindowsResourceProvider {
    fn load_png(
        &self,
        module: ModuleHandle,
        resource_id: u32,
        max_bytes: usize,
    ) -> Result<Vec<u8>, BackendFailure> {
        if resource_id > u16::MAX.into() {
            return Err(BackendFailure::Rejected);
        }
        let mut pinned = ptr::null_mut();
        // SAFETY: `module` was constructed under the `ModuleHandle` validity
        // contract. FROM_ADDRESS treats the base address as an address within
        // the module and increments its loader reference count on success.
        let pinned_ok = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                module.address() as *const u16,
                &mut pinned,
            )
        };
        if pinned_ok == 0 || pinned.is_null() {
            return Err(BackendFailure::Unavailable);
        }
        let pinned = PinnedModule(pinned);
        let resource_name = resource_id as usize as *const u16;

        // SAFETY: `pinned` holds a live module reference, the integer resource
        // identifier follows MAKEINTRESOURCEW encoding, and the type is NUL terminated.
        let resource =
            unsafe { FindResourceW(pinned.0, resource_name, PNG_RESOURCE_TYPE.as_ptr()) };
        if resource.is_null() {
            return Err(BackendFailure::Unavailable);
        }

        // SAFETY: both handles originate from the successful resource lookup above.
        let size = unsafe { SizeofResource(pinned.0, resource) };
        let size = usize::try_from(size).map_err(|_| BackendFailure::Rejected)?;
        if size == 0 || size > max_bytes {
            return Err(BackendFailure::Rejected);
        }

        // SAFETY: both handles originate from the successful resource lookup above.
        let loaded = unsafe { LoadResource(pinned.0, resource) };
        if loaded.is_null() {
            return Err(BackendFailure::Unavailable);
        }
        // SAFETY: `loaded` is a valid resource handle and remains valid while
        // the pinned module reference is held.
        let bytes = unsafe { LockResource(loaded) };
        if bytes.is_null() {
            return Err(BackendFailure::Unavailable);
        }

        // SAFETY: Windows guarantees `LockResource` exposes at least
        // `SizeofResource` bytes for the lifetime of the loaded module. The
        // slice is copied before `pinned` is dropped.
        Ok(unsafe { std::slice::from_raw_parts(bytes.cast::<u8>(), size) }.to_vec())
    }
}
