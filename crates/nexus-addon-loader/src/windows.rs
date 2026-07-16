use std::{
    ffi::{c_char, c_void},
    mem::{size_of, transmute, zeroed},
    num::NonZeroUsize,
    os::windows::ffi::OsStrExt,
    ptr::NonNull,
};

use nexus_abi::GetAddonDefinitionV1;
use nexus_host::{ModuleMemory, ModuleReadError};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, HMODULE},
    System::{
        LibraryLoader::{
            GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
            LoadLibraryExW,
        },
        Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE, PAGE_EXECUTE_READ,
            PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS,
            PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY, VirtualQuery,
        },
        ProcessStatus::{K32GetModuleInformation, MODULEINFO},
        Threading::GetCurrentProcess,
    },
};

use crate::platform::{
    AbsoluteDllPath, LoaderPlatform, ModuleBounds, ModuleHandle, ModuleImage, PlatformError,
};

/// Windows x64 platform adapter using constrained DLL search directories.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsPlatform;

/// Virtual-memory reader tied to one mapped Windows module image.
#[derive(Clone, Copy, Debug)]
pub struct WindowsModuleMemory {
    bounds: ModuleBounds,
}

impl ModuleMemory for WindowsModuleMemory {
    fn read_bounded(
        &self,
        address: NonNull<c_char>,
        maximum_bytes: usize,
    ) -> Result<&[u8], ModuleReadError> {
        let Some(address_value) = NonZeroUsize::new(address.as_ptr() as usize) else {
            return Err(ModuleReadError::new("module memory address was null"));
        };
        let Some(maximum_bytes) = self.bounds.cap_read(address_value, maximum_bytes) else {
            return Err(ModuleReadError::new(
                "module memory address is outside the image",
            ));
        };
        if maximum_bytes == 0 {
            return Ok(&[]);
        }

        let mut verified = 0_usize;
        while verified < maximum_bytes {
            let cursor = address_value
                .get()
                .checked_add(verified)
                .ok_or_else(|| ModuleReadError::new("module memory range overflowed"))?;
            let info = query_memory(cursor)
                .map_err(|_| ModuleReadError::new("module memory query failed"))?;
            let region_base = info.BaseAddress as usize;
            let region_end = region_base
                .checked_add(info.RegionSize)
                .ok_or_else(|| ModuleReadError::new("module memory region overflowed"))?;
            if cursor < region_base
                || cursor >= region_end
                || info.State != MEM_COMMIT
                || info.AllocationBase as usize != self.bounds.base().get()
                || !is_readable_protection(info.Protect)
            {
                break;
            }
            let available = region_end - cursor;
            let step = available.min(maximum_bytes - verified);
            if step == 0 {
                break;
            }
            verified += step;
        }
        if verified == 0 {
            return Err(ModuleReadError::new("module memory range is not readable"));
        }

        // SAFETY: every byte in the returned prefix lies inside the live module
        // bounds and was confirmed committed and readable by `VirtualQuery`.
        Ok(unsafe { std::slice::from_raw_parts(address.as_ptr().cast::<u8>(), verified) })
    }
}

// SAFETY: handles come directly from `LoadLibraryExW`; image bounds come from
// `K32GetModuleInformation`; memory and code addresses are validated with
// `VirtualQuery`; and `FreeLibrary` is the sole release operation.
unsafe impl LoaderPlatform for WindowsPlatform {
    type Memory = WindowsModuleMemory;

    unsafe fn load_library(&self, path: &AbsoluteDllPath) -> Result<ModuleHandle, PlatformError> {
        let mut wide = path.as_path().as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(PlatformError::PathEncoding);
        }
        wide.push(0);
        let flags = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32;
        // SAFETY: the validated absolute path is NUL-terminated and remains
        // alive for the call. Search flags exclude the current directory.
        let handle = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), flags) };
        NonZeroUsize::new(handle as usize)
            .map(ModuleHandle::from_non_zero)
            .ok_or(PlatformError::LoadLibrary)
    }

    unsafe fn resolve_definition_export(
        &self,
        module: ModuleHandle,
        export: &'static std::ffi::CStr,
    ) -> Result<GetAddonDefinitionV1, PlatformError> {
        // SAFETY: the module token is live by the trait contract and `export`
        // is NUL-terminated for the duration of the call.
        let procedure =
            unsafe { GetProcAddress(module.raw().get() as HMODULE, export.as_ptr().cast::<u8>()) }
                .ok_or(PlatformError::MissingDefinitionExport)?;
        // SAFETY: `GetProcAddress` returned one pointer-sized function address;
        // the loader requests only the exact `GetAddonDef` ABI through this API.
        Ok(unsafe {
            transmute::<unsafe extern "system" fn() -> isize, GetAddonDefinitionV1>(procedure)
        })
    }

    unsafe fn module_image(
        &self,
        module: ModuleHandle,
    ) -> Result<ModuleImage<Self::Memory>, PlatformError> {
        // SAFETY: zero is a valid initial bit-pattern for `MODULEINFO` output.
        let mut info = unsafe { zeroed::<MODULEINFO>() };
        // SAFETY: the process pseudo-handle and live module token are valid;
        // `info` is writable for the exact structure size supplied.
        let succeeded = unsafe {
            K32GetModuleInformation(
                GetCurrentProcess(),
                module.raw().get() as HMODULE,
                &mut info,
                u32::try_from(size_of::<MODULEINFO>()).map_err(|_| PlatformError::ModuleImage)?,
            )
        };
        if succeeded == 0 {
            return Err(PlatformError::ModuleImage);
        }
        let base =
            NonZeroUsize::new(info.lpBaseOfDll as usize).ok_or(PlatformError::ModuleImage)?;
        let bounds = ModuleBounds::new(base, info.SizeOfImage as usize)
            .map_err(|_| PlatformError::ModuleImage)?;
        Ok(ModuleImage::new(bounds, WindowsModuleMemory { bounds }))
    }

    unsafe fn is_executable_address(
        &self,
        module: ModuleHandle,
        address: NonZeroUsize,
    ) -> Result<bool, PlatformError> {
        let info = query_memory(address.get())?;
        Ok(info.State == MEM_COMMIT
            && info.AllocationBase as usize == module.raw().get()
            && is_executable_protection(info.Protect))
    }

    unsafe fn free_library(&self, module: ModuleHandle) -> Result<(), PlatformError> {
        // SAFETY: the token came from a successful `LoadLibraryExW` and the
        // lifecycle layer guarantees this release is attempted at most once.
        let succeeded = unsafe { FreeLibrary(module.raw().get() as HMODULE) };
        if succeeded == 0 {
            Err(PlatformError::FreeLibrary)
        } else {
            Ok(())
        }
    }
}

fn query_memory(address: usize) -> Result<MEMORY_BASIC_INFORMATION, PlatformError> {
    // SAFETY: zero is a valid initial bit-pattern for this output structure.
    let mut info = unsafe { zeroed::<MEMORY_BASIC_INFORMATION>() };
    // SAFETY: `info` is writable for the exact structure size; `VirtualQuery`
    // validates the numeric process address without dereferencing it here.
    let returned = unsafe {
        VirtualQuery(
            address as *const c_void,
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if returned < size_of::<MEMORY_BASIC_INFORMATION>() {
        Err(PlatformError::MemoryQuery)
    } else {
        Ok(info)
    }
}

const fn is_readable_protection(protection: u32) -> bool {
    if protection & (PAGE_GUARD | PAGE_NOACCESS) != 0 {
        return false;
    }
    matches!(
        protection & 0xff,
        PAGE_READONLY
            | PAGE_READWRITE
            | PAGE_WRITECOPY
            | PAGE_EXECUTE_READ
            | PAGE_EXECUTE_READWRITE
            | PAGE_EXECUTE_WRITECOPY
    )
}

const fn is_executable_protection(protection: u32) -> bool {
    if protection & (PAGE_GUARD | PAGE_NOACCESS) != 0 {
        return false;
    }
    matches!(
        protection & 0xff,
        PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    )
}
