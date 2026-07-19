#![allow(unsafe_code)]

use core::fmt;
use core::num::NonZeroUsize;
use std::sync::{Arc, Mutex, OnceLock};

use crate::OwnerHandle;

type RenderCallback = unsafe extern "C" fn();

const RETAINED_CELL_LOCK_STRIPES: usize = 64;

fn retained_cell_operation(address: NonZeroUsize) -> &'static Mutex<()> {
    static OPERATIONS: OnceLock<[Mutex<()>; RETAINED_CELL_LOCK_STRIPES]> = OnceLock::new();

    let operations = OPERATIONS.get_or_init(|| std::array::from_fn(|_| Mutex::new(())));
    let value = address.get();
    let stripe = (value ^ value.rotate_right(17)) % RETAINED_CELL_LOCK_STRIPES;
    &operations[stripe]
}

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

/// Checked access to one retained legacy visibility cell.
///
/// # Safety
///
/// Implementations are trusted host adapters. Before every access they must
/// enforce the host's provenance policy and reject the operation without
/// dereferencing an address that fails validation. A policy may rely on this
/// bridge's unsafe construction proof for heap or TLS storage that cannot be
/// attributed independently, but it must reject addresses known to belong to a
/// different owner generation. Implementations must also avoid reentering any
/// operation that invokes the same retained cell, synchronously
/// deregistering or cleaning up its registration, or otherwise waiting for it:
/// the bridge serializes the read/write transaction and the registry holds an
/// in-flight drain guard across these methods, so that reentrancy would
/// self-deadlock.
pub unsafe trait CheckedVisibilityAccess: Send + Sync + 'static {
    /// Reads the current visibility state after validating the native address.
    fn read_visible(&self, address: NonZeroUsize) -> Option<bool>;

    /// Writes the hidden state after validating the native address.
    fn write_hidden(&self, address: NonZeroUsize) -> bool;
}

/// Owner-scoped retained-cell bridge for the legacy `bool*` Escape API.
///
/// The raw address is deliberately not exposed by safe APIs. Access is routed
/// through [`CheckedVisibilityAccess`] while the exact owner generation's
/// activity gate is held, and registry cleanup drains in-flight operations
/// before the adapter can be dropped and its module unloaded.
pub struct NativeVisibilityPointer {
    address: NonZeroUsize,
    owner: OwnerHandle,
    access: Arc<dyn CheckedVisibilityAccess>,
}

impl NativeVisibilityPointer {
    /// Creates a checked bridge for a non-null native visibility-cell address.
    ///
    /// # Safety
    ///
    /// `address` must identify one initialized, writable byte in the allocation
    /// belonging to `owner`. That byte must remain the same live allocation and
    /// must not be repurposed until the owner-scoped deregistration containing
    /// this value returns, owner cleanup finishes draining it, or this value is
    /// dropped without being registered. Every other access to the byte must be
    /// synchronized so that it cannot conflict with adapter reads or writes.
    /// `access` must uphold [`CheckedVisibilityAccess`]'s contract for this
    /// owner and address.
    #[must_use]
    pub unsafe fn checked(
        owner: OwnerHandle,
        address: NonZeroUsize,
        access: Arc<dyn CheckedVisibilityAccess>,
    ) -> Self {
        Self {
            address,
            owner,
            access,
        }
    }

    pub(crate) fn address(&self) -> usize {
        self.address.get()
    }

    pub(crate) fn owner(&self) -> &OwnerHandle {
        &self.owner
    }

    pub(crate) fn close_if_visible(&self) -> bool {
        let Some(_activity) = self.owner.try_enter() else {
            return false;
        };
        let _operation = match retained_cell_operation(self.address).lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.access.read_visible(self.address) != Some(true) {
            return false;
        }
        self.access.write_hidden(self.address)
    }
}

impl fmt::Debug for NativeVisibilityPointer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeVisibilityPointer(..)")
    }
}
