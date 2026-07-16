use std::{
    cell::Cell,
    env, io,
    mem::{size_of, transmute_copy},
    num::NonZeroUsize,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use thiserror::Error;
use windows_sys::Win32::{
    Foundation::HMODULE,
    System::{
        LibraryLoader::{GetProcAddress, LoadLibraryW},
        SystemInformation::GetSystemDirectoryW,
    },
};

/// System graphics module represented by one group of proxy exports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModuleKind {
    D3d9,
    D3d11,
    Dxgi,
}

impl ModuleKind {
    pub(crate) const fn file_name(self) -> &'static str {
        match self {
            Self::D3d9 => "d3d9.dll",
            Self::D3d11 => "d3d11.dll",
            Self::Dxgi => "dxgi.dll",
        }
    }

    const fn chainload_file_name(self) -> &'static str {
        match self {
            Self::D3d9 => "d3d9_chainload.dll",
            Self::D3d11 => "d3d11_chainload.dll",
            Self::Dxgi => "dxgi_chainload.dll",
        }
    }
}

/// Failure to initialize a proxy module or resolve an export.
#[derive(Debug, Error)]
pub(crate) enum ProxyError {
    #[error("failed to query the Windows system directory: {0}")]
    SystemDirectory(#[source] io::Error),
    #[error("the Windows system directory exceeded the supported buffer")]
    SystemDirectoryTooLong,
    #[error("failed to locate the process executable: {0}")]
    ExecutablePath(#[source] io::Error),
    #[error("the process executable has no parent directory")]
    ExecutableHasNoParent,
    #[error("failed to load system module {module} from {path}: {source}")]
    LoadSystemModule {
        module: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("export {export} was not found in {module}")]
    MissingExport {
        module: &'static str,
        export: &'static str,
    },
    #[error("resolved export {export} had an unexpected function-pointer size")]
    UnexpectedFunctionPointerSize { export: &'static str },
}

/// Loaded system module plus an optional add-on chainload module.
pub(crate) struct ProxyModule {
    kind: ModuleKind,
    system: NonZeroUsize,
    chainload: Option<NonZeroUsize>,
}

static D3D9_MODULE: OnceLock<Result<ProxyModule, ProxyError>> = OnceLock::new();
static D3D11_MODULE: OnceLock<Result<ProxyModule, ProxyError>> = OnceLock::new();
static DXGI_MODULE: OnceLock<Result<ProxyModule, ProxyError>> = OnceLock::new();

impl ProxyModule {
    pub(crate) fn get(kind: ModuleKind) -> Result<&'static Self, &'static ProxyError> {
        let cell = match kind {
            ModuleKind::D3d9 => &D3D9_MODULE,
            ModuleKind::D3d11 => &D3D11_MODULE,
            ModuleKind::Dxgi => &DXGI_MODULE,
        };

        cell.get_or_init(|| Self::load(kind)).as_ref()
    }

    fn load(kind: ModuleKind) -> Result<Self, ProxyError> {
        let system_path = system_directory()?.join(kind.file_name());
        let system = load_library(&system_path).map_err(|source| ProxyError::LoadSystemModule {
            module: kind.file_name(),
            path: system_path,
            source,
        })?;

        let executable = env::current_exe().map_err(ProxyError::ExecutablePath)?;
        let game_directory = executable
            .parent()
            .ok_or(ProxyError::ExecutableHasNoParent)?;
        let chainload_path = game_directory.join(kind.chainload_file_name());
        let chainload = chainload_path
            .is_file()
            .then(|| load_library(&chainload_path).ok())
            .flatten();

        Ok(Self {
            kind,
            system,
            chainload,
        })
    }

    /// Resolves an export to the caller-supplied Windows function type.
    ///
    /// # Safety
    ///
    /// `Function` must exactly match the ABI of `export` in the selected
    /// system DLL. Callers use aliases copied from the Windows SDK headers.
    pub(crate) unsafe fn resolve<Function: Copy>(
        &self,
        export: &'static str,
        export_nul: &'static [u8],
        system_only: bool,
    ) -> Result<Function, ProxyError> {
        if size_of::<Function>() != size_of::<usize>() {
            return Err(ProxyError::UnexpectedFunctionPointerSize { export });
        }

        let preferred = if system_only {
            self.system
        } else {
            self.chainload.unwrap_or(self.system)
        };
        let address = resolve_address(preferred, export_nul).or_else(|| {
            (preferred != self.system)
                .then(|| resolve_address(self.system, export_nul))
                .flatten()
        });
        let address = address.ok_or(ProxyError::MissingExport {
            module: self.kind.file_name(),
            export,
        })?;

        // SAFETY: the size check above proves the destination is pointer-sized;
        // the function's ABI/signature obligation is documented on this method.
        Ok(unsafe { transmute_copy::<usize, Function>(&address) })
    }
}

fn system_directory() -> Result<PathBuf, ProxyError> {
    let mut buffer = vec![0_u16; 32_768];

    // SAFETY: `buffer` is writable for the length passed to the API.
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 {
        return Err(ProxyError::SystemDirectory(io::Error::last_os_error()));
    }
    let length = length as usize;
    if length >= buffer.len() {
        return Err(ProxyError::SystemDirectoryTooLong);
    }

    buffer.truncate(length);
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
}

fn load_library(path: &Path) -> Result<NonZeroUsize, io::Error> {
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(core::iter::once(0))
        .collect::<Vec<_>>();

    // SAFETY: `wide` is NUL-terminated and remains alive during the call.
    let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
    NonZeroUsize::new(handle as usize).ok_or_else(io::Error::last_os_error)
}

fn resolve_address(module: NonZeroUsize, export_nul: &'static [u8]) -> Option<usize> {
    debug_assert_eq!(export_nul.last(), Some(&0));

    // SAFETY: `module` came from a successful `LoadLibraryW` call and
    // `export_nul` is a static NUL-terminated ASCII export name.
    let proc = unsafe { GetProcAddress(module.get() as HMODULE, export_nul.as_ptr()) }?;
    Some(proc as usize)
}

thread_local! {
    static IN_PROXY_CALL: Cell<bool> = const { Cell::new(false) };
}

/// Restores recursion state even if a chainloaded export panics internally.
pub(crate) struct RecursionGuard {
    previous: bool,
}

impl RecursionGuard {
    pub(crate) fn is_active() -> bool {
        IN_PROXY_CALL.get()
    }

    pub(crate) fn enter() -> Self {
        let previous = IN_PROXY_CALL.replace(true);
        Self { previous }
    }
}

impl Drop for RecursionGuard {
    fn drop(&mut self) {
        IN_PROXY_CALL.set(self.previous);
    }
}

#[cfg(test)]
mod tests {
    use super::{ModuleKind, RecursionGuard};

    #[test]
    fn module_names_match_legacy_proxy_contract() {
        assert_eq!(ModuleKind::D3d9.file_name(), "d3d9.dll");
        assert_eq!(ModuleKind::D3d11.file_name(), "d3d11.dll");
        assert_eq!(ModuleKind::Dxgi.file_name(), "dxgi.dll");
    }

    #[test]
    fn recursion_guard_restores_nested_state() {
        assert!(!RecursionGuard::is_active());
        {
            let _outer = RecursionGuard::enter();
            assert!(RecursionGuard::is_active());
            {
                let _inner = RecursionGuard::enter();
                assert!(RecursionGuard::is_active());
            }
            assert!(RecursionGuard::is_active());
        }
        assert!(!RecursionGuard::is_active());
    }
}
