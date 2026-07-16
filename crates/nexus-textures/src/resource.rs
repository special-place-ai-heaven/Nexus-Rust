use core::ffi::c_void;
use core::fmt;
use core::num::NonZeroUsize;

use crate::{BackendFailure, ResourceProvider};

/// A borrowed Windows `HMODULE` address used only for synchronous resource copying.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ModuleHandle(NonZeroUsize);

impl ModuleHandle {
    /// Construct a handle from an addon ABI `HMODULE` value.
    ///
    /// # Safety
    ///
    /// `raw` must identify a module which is loaded for the duration of the
    /// resource request. The Windows provider obtains its own temporary module
    /// reference before reading and copies the bytes before returning.
    #[allow(unsafe_code)]
    pub unsafe fn from_hmodule(raw: *mut c_void) -> Option<Self> {
        NonZeroUsize::new(raw.addr()).map(Self)
    }

    pub(crate) const fn address(self) -> usize {
        self.0.get()
    }
}

impl fmt::Debug for ModuleHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModuleHandle(<redacted>)")
    }
}

/// Resource provider which rejects every resource request.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoResources;

impl ResourceProvider for NoResources {
    fn load_png(
        &self,
        _module: ModuleHandle,
        _resource_id: u32,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, BackendFailure> {
        Err(BackendFailure::Unavailable)
    }
}

#[cfg(windows)]
pub use crate::windows_resource::WindowsResourceProvider;

/// Windows resource provider is unavailable on non-Windows targets.
#[cfg(not(windows))]
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsResourceProvider;

#[cfg(not(windows))]
impl ResourceProvider for WindowsResourceProvider {
    fn load_png(
        &self,
        _module: ModuleHandle,
        _resource_id: u32,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, BackendFailure> {
        Err(BackendFailure::Unavailable)
    }
}
