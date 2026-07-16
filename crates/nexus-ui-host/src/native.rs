#![allow(unsafe_code)]

use core::fmt;
use core::num::NonZeroUsize;

use crate::OwnerHandle;

type RenderCallback = unsafe extern "C" fn();

/// Explicit boundary wrapper for an addon-owned native render callback.
///
/// The containing [`crate::OwnerHandle`] must remain active while this is
/// invoked. Native callbacks must obey the C ABI and must not unwind across it.
#[derive(Clone)]
pub struct NativeRenderCallback {
    callback: RenderCallback,
    owner: OwnerHandle,
}

impl NativeRenderCallback {
    /// Wraps a non-null callback supplied through the Nexus ABI.
    ///
    /// # Safety
    ///
    /// The function must be valid to call with no arguments, must not unwind
    /// across the C ABI, and must remain executable until its associated owner
    /// generation has retired and reached quiescence.
    #[must_use]
    pub unsafe fn new(owner: OwnerHandle, callback: RenderCallback) -> Self {
        Self { callback, owner }
    }

    pub(crate) fn address(&self) -> usize {
        self.callback as usize
    }

    pub(crate) fn owner(&self) -> &OwnerHandle {
        &self.owner
    }

    pub(crate) fn invoke(&self) {
        // SAFETY: construction requires the ABI function pointer type; the
        // owner activity guard prevents invocation after generation cleanup.
        unsafe { (self.callback)() };
    }
}

impl fmt::Debug for NativeRenderCallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeRenderCallback(..)")
    }
}

/// Explicit boundary wrapper for the legacy `bool*` close-on-Escape API.
///
/// The pointer is deliberately not exposed by safe APIs. It is only read or
/// written while its owner's generation activity gate is held.
#[derive(Clone)]
pub struct NativeVisibilityPointer {
    address: NonZeroUsize,
    owner: OwnerHandle,
}

impl NativeVisibilityPointer {
    /// Creates a native visibility pointer from the ABI's `bool*` storage.
    ///
    /// # Safety
    ///
    /// `pointer` must be non-null, aligned, and valid for byte reads and
    /// writes until the associated owner generation has retired and reached
    /// quiescence. Concurrent access must be synchronized by the caller.
    #[must_use]
    pub unsafe fn from_ptr(owner: OwnerHandle, pointer: *mut u8) -> Option<Self> {
        NonZeroUsize::new(pointer as usize).map(|address| Self { address, owner })
    }

    pub(crate) fn address(&self) -> usize {
        self.address.get()
    }

    pub(crate) fn owner(&self) -> &OwnerHandle {
        &self.owner
    }

    pub(crate) fn is_visible(&self) -> bool {
        let pointer = self.address.get() as *const u8;
        // SAFETY: upheld by `from_ptr` and the owner activity guard.
        unsafe { pointer.read() != 0 }
    }

    pub(crate) fn close(&self) {
        let pointer = self.address.get() as *mut u8;
        // SAFETY: upheld by `from_ptr` and the owner activity guard.
        unsafe { pointer.write(0) };
    }
}

impl fmt::Debug for NativeVisibilityPointer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeVisibilityPointer(..)")
    }
}
