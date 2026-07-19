use std::{ffi::c_void, fmt};

use nexus_core::OwnerToken;

use crate::{
    CleanupError, CleanupReport, MH_ERROR_NOT_CREATED, MH_ERROR_UNSUPPORTED_FUNCTION, MinHookStatus,
};

/// A deterministic unsupported implementation for non-Windows-x64 targets.
pub struct InlineHookService;

impl InlineHookService {
    /// Creates an unsupported service value.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Reports that inline hooks are unsupported on this target.
    pub const fn initialize(&self) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Reports that inline hooks are unsupported on this target.
    pub const fn uninitialize(&self) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Reports that create preflight is unsupported on this target.
    pub const fn preflight_create(
        &self,
        _target: *mut c_void,
        _detour: *mut c_void,
    ) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Reports that remove preflight is unsupported on this target.
    pub const fn preflight_remove(&self, _target: *mut c_void) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Reports that enable/disable preflight is unsupported on this target.
    pub const fn preflight_change(&self) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Reports that inline hooks are unsupported on this target.
    ///
    /// # Safety
    ///
    /// This implementation never dereferences any supplied pointer.
    pub unsafe fn create_hook(
        &self,
        _owner: OwnerToken,
        _target: *mut c_void,
        _detour: *mut c_void,
        _original: *mut *mut c_void,
    ) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Reports that transactional hook creation is unsupported on this target.
    ///
    /// # Safety
    ///
    /// This implementation neither dereferences pointers nor invokes
    /// `publish`.
    pub unsafe fn create_hook_transaction(
        &self,
        _owner: OwnerToken,
        _target: *mut c_void,
        _detour: *mut c_void,
        _publish: impl FnOnce(*mut c_void) -> MinHookStatus,
    ) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Reports that inline hooks are unsupported on this target.
    pub const fn remove_hook(&self, _target: *mut c_void) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Rejects a null target and otherwise reports owner-scoped hooks unsupported.
    pub const fn remove_owned_hook(
        &self,
        _owner: OwnerToken,
        target: *mut c_void,
    ) -> MinHookStatus {
        if target.is_null() {
            MH_ERROR_NOT_CREATED
        } else {
            MH_ERROR_UNSUPPORTED_FUNCTION
        }
    }

    /// Reports that inline hooks are unsupported on this target.
    pub const fn enable_hook(&self, _target: *mut c_void) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Reports that inline hooks are unsupported on this target.
    pub const fn disable_hook(&self, _target: *mut c_void) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Reports that owner-scoped inline hooks are unsupported on this target.
    pub const fn enable_owned_hook(
        &self,
        _owner: OwnerToken,
        _target: *mut c_void,
    ) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Reports that owner-scoped inline hooks are unsupported on this target.
    pub const fn disable_owned_hook(
        &self,
        _owner: OwnerToken,
        _target: *mut c_void,
    ) -> MinHookStatus {
        MH_ERROR_UNSUPPORTED_FUNCTION
    }

    /// Reports deterministic unsupported cleanup without exposing addresses.
    pub const fn cleanup_owner(&self, owner: OwnerToken) -> Result<CleanupReport, CleanupError> {
        Err(CleanupError::new(
            owner,
            MH_ERROR_UNSUPPORTED_FUNCTION,
            0,
            0,
        ))
    }

    /// Returns zero because hooks cannot be registered on this target.
    #[must_use]
    pub const fn hook_count(&self) -> usize {
        0
    }

    /// Returns zero because hooks cannot be registered on this target.
    #[must_use]
    pub const fn owned_hook_count(&self, _owner: OwnerToken) -> usize {
        0
    }
}

impl Default for InlineHookService {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for InlineHookService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InlineHookService")
            .field("supported", &false)
            .finish()
    }
}
