use core::ffi::c_void;
use core::fmt;
use std::sync::Arc;

use nexus_abi::MinHookStatus;
use nexus_inline_hooks::{
    InlineHookService, MH_ERROR_MEMORY_PROTECT, MH_ERROR_UNSUPPORTED_FUNCTION, MH_OK,
};

use crate::{BackendFailure, NativeCallBoundary};

/// Caller-attributed MinHook compatibility adapter.
pub struct InlineHookApi {
    boundary: Arc<NativeCallBoundary>,
    hooks: Arc<InlineHookService>,
}

impl InlineHookApi {
    /// Creates an adapter around the process inline-hook registry.
    #[must_use]
    pub fn new(boundary: Arc<NativeCallBoundary>, hooks: Arc<InlineHookService>) -> Self {
        Self { boundary, hooks }
    }

    /// Creates a disabled owner-scoped hook and copies out its trampoline.
    ///
    /// The untrusted output pointer is never passed to the hook service. The
    /// trampoline is first written into local Rust memory, then copied through
    /// the checked native-memory boundary. A failed copy rolls the hook back.
    ///
    /// # Safety
    ///
    /// `target` and `detour` must denote live functions with compatible ABIs
    /// and signatures for the full hook lifetime. A non-null `original` must
    /// denote one live, aligned, exclusively writable pointer-sized object.
    pub unsafe fn create(
        &self,
        target: *mut c_void,
        detour: *mut c_void,
        original: *mut *mut c_void,
    ) -> MinHookStatus {
        let owner = match self.boundary.resolve_owner_for_address(detour.cast_const()) {
            Ok(owner) => owner,
            Err(_) => return MH_ERROR_UNSUPPORTED_FUNCTION,
        };
        let mut trampoline = core::ptr::null_mut();
        let output = if original.is_null() {
            core::ptr::null_mut()
        } else {
            &mut trampoline
        };
        let status = unsafe {
            // SAFETY: the legacy ABI owns target/detour signature validity.
            // `output` is either null or a live local pointer under our control.
            self.hooks.create_hook(owner, target, detour, output)
        };
        if status != MH_OK {
            return self.service_status(status);
        }
        if original.is_null() {
            return MH_OK;
        }

        let copied = unsafe {
            // SAFETY: the native ABI requires `original` to identify one live,
            // aligned, exclusively writable pointer-sized output object.
            self.boundary
                .write_usize(original.cast::<usize>(), trampoline as usize)
        };
        if copied.is_ok() {
            return MH_OK;
        }

        let rollback = self.hooks.remove_hook(target);
        if rollback != MH_OK {
            self.boundary
                .failures()
                .record(BackendFailure::ServiceRejected);
        }
        MH_ERROR_MEMORY_PROTECT
    }

    /// Removes one hook after validating that the call belongs to a live add-on.
    pub fn remove(&self, target: *mut c_void) -> MinHookStatus {
        if self.boundary.resolve_owner(None).is_err() {
            return MH_ERROR_UNSUPPORTED_FUNCTION;
        }
        let status = self.hooks.remove_hook(target);
        self.service_status(status)
    }

    /// Enables one hook, or all hooks for a null target.
    pub fn enable(&self, target: *mut c_void) -> MinHookStatus {
        if self.boundary.resolve_owner(None).is_err() {
            return MH_ERROR_UNSUPPORTED_FUNCTION;
        }
        let status = self.hooks.enable_hook(target);
        self.service_status(status)
    }

    /// Disables one hook, or all hooks for a null target.
    pub fn disable(&self, target: *mut c_void) -> MinHookStatus {
        if self.boundary.resolve_owner(None).is_err() {
            return MH_ERROR_UNSUPPORTED_FUNCTION;
        }
        let status = self.hooks.disable_hook(target);
        self.service_status(status)
    }

    fn service_status(&self, status: MinHookStatus) -> MinHookStatus {
        if status != MH_OK {
            self.boundary
                .failures()
                .record(BackendFailure::ServiceRejected);
        }
        status
    }
}

impl fmt::Debug for InlineHookApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InlineHookApi")
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use core::ffi::c_void;
    use core::num::NonZeroUsize;
    use std::sync::Arc;

    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::OwnerToken;
    use nexus_inline_hooks::{InlineHookService, MH_ERROR_UNSUPPORTED_FUNCTION};
    use nexus_native_memory::NativeMemoryReader;

    use super::InlineHookApi;
    use crate::{BackendFailures, NativeCallBoundary};

    struct NoOwners;

    unsafe extern "C" fn target() {}

    unsafe extern "C" fn detour() {}

    impl AddressOwnerResolver for NoOwners {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            None
        }

        fn is_current_owner(&self, _owner: OwnerToken) -> bool {
            false
        }
    }

    #[test]
    fn unattributed_calls_fail_before_touching_the_hook_service() {
        let boundary = Arc::new(NativeCallBoundary::new(
            Arc::new(AddonCallerResolver::new(Arc::new(NoOwners))),
            NativeMemoryReader::default(),
            Arc::new(BackendFailures::new()),
        ));
        let api = InlineHookApi::new(boundary, Arc::new(InlineHookService::new()));

        let status = unsafe {
            // SAFETY: the test functions have identical ABIs and signatures,
            // remain live for the test, and the output pointer is null.
            api.create(
                target as *const () as *mut c_void,
                detour as *const () as *mut c_void,
                core::ptr::null_mut(),
            )
        };
        assert_eq!(status, MH_ERROR_UNSUPPORTED_FUNCTION);
        assert_eq!(api.boundary.failures().snapshot().caller_attribution, 1);
    }
}
