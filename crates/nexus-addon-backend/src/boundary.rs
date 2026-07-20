use core::ffi::{c_char, c_void};
use core::fmt;
use core::num::NonZeroUsize;
use std::sync::Arc;

use nexus_addon_ffi::AddonCallerResolver;
use nexus_core::{CallbackGate, OwnerToken};
use nexus_native_memory::{
    NativeMemoryReader, OwnedNativeBytes, OwnedNativeString, snapshot_u8, write_u8, write_usize,
};
use thiserror::Error;

use crate::{BackendFailure, BackendFailures};

/// Fail-closed classes returned by the native argument boundary.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CallBoundaryError {
    /// No current add-on generation could be attributed to the call.
    #[error("native caller attribution failed")]
    CallerAttribution,
    /// A native address could not be copied into owned Rust memory.
    #[error("native memory snapshot failed")]
    NativeMemory,
    /// A copied native string was not valid UTF-8.
    #[error("native text was not valid UTF-8")]
    InvalidText,
}

/// An owned, NUL-free, UTF-8 native string.
///
/// Construction is private so every instance has been copied from native
/// memory and validated exactly once before a domain service can observe it.
#[derive(Clone, Eq, PartialEq)]
pub struct NativeText {
    value: String,
}

impl NativeText {
    fn from_owned(value: OwnedNativeString) -> Result<Self, CallBoundaryError> {
        let bytes = value.into_c_string().into_bytes();
        String::from_utf8(bytes)
            .map(|value| Self { value })
            .map_err(|_| CallBoundaryError::InvalidText)
    }

    /// Returns the validated text without re-reading native memory.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Returns the UTF-8 byte length without a trailing NUL.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.value.len()
    }

    /// Returns whether the copied string is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// Consumes the snapshot and returns the owned Rust string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.value
    }
}

impl fmt::Debug for NativeText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeText")
            .field("byte_len", &self.byte_len())
            .finish_non_exhaustive()
    }
}

/// Shared production gate for caller attribution and native memory snapshots.
///
/// All legacy API shims must pass through this type before invoking a typed
/// Rust service. Failure counters retain only a closed class, never arguments,
/// addresses, or copied bytes.
pub struct NativeCallBoundary {
    callers: Arc<AddonCallerResolver>,
    memory: NativeMemoryReader,
    failures: Arc<BackendFailures>,
}

impl NativeCallBoundary {
    /// Creates a boundary around the authoritative caller and memory readers.
    #[must_use]
    pub fn new(
        callers: Arc<AddonCallerResolver>,
        memory: NativeMemoryReader,
        failures: Arc<BackendFailures>,
    ) -> Self {
        Self {
            callers,
            memory,
            failures,
        }
    }

    /// Returns the redacted counters shared with this boundary.
    #[must_use]
    pub fn failures(&self) -> &BackendFailures {
        &self.failures
    }

    /// Resolves and authenticates the exact live addon generation for a call.
    ///
    /// With no address, attribution comes only from the explicit owner scope
    /// or native stack. When an address is supplied, it must additionally map
    /// to that independently resolved current generation.
    pub fn resolve_owner(
        &self,
        registered_address: Option<NonZeroUsize>,
    ) -> Result<OwnerToken, CallBoundaryError> {
        let owner = match registered_address {
            Some(address) => self.callers.resolve_registered_address(address),
            None => self.callers.resolve_actual(),
        };
        owner.ok_or_else(|| {
            self.failures.record(BackendFailure::CallerAttribution);
            CallBoundaryError::CallerAttribution
        })
    }

    /// Authenticates a caller-controlled registered function address.
    ///
    /// This compatibility name now has strict semantics: the actual caller is
    /// resolved independently and the non-null address must belong to that
    /// same live owner generation.
    pub fn resolve_owner_for_address(
        &self,
        function_address: *const c_void,
    ) -> Result<OwnerToken, CallBoundaryError> {
        self.resolve_owner_for_registered_address(function_address)
    }

    /// Authenticates a registered function address against the actual caller.
    ///
    /// The address is treated only as an opaque ownership key. It is never
    /// dereferenced, retained, formatted, or used as the attribution source.
    pub fn resolve_owner_for_registered_address(
        &self,
        function_address: *const c_void,
    ) -> Result<OwnerToken, CallBoundaryError> {
        let Some(address) = NonZeroUsize::new(function_address as usize) else {
            self.failures.record(BackendFailure::CallerAttribution);
            return Err(CallBoundaryError::CallerAttribution);
        };

        self.callers
            .resolve_registered_address(address)
            .ok_or_else(|| {
                self.failures.record(BackendFailure::CallerAttribution);
                CallBoundaryError::CallerAttribution
            })
    }

    /// Validates that an opaque retained address belongs to one exact current
    /// addon generation.
    ///
    /// This proves mapped-image provenance and current-generation admission;
    /// it does not prove allocation lifetime, aliasing, or synchronization.
    /// Callers retaining the address must uphold those separate invariants.
    pub fn validate_owned_address(
        &self,
        owner: OwnerToken,
        address: NonZeroUsize,
    ) -> Result<(), CallBoundaryError> {
        if self.callers.address_belongs_to_owner(address, owner) {
            return Ok(());
        }
        self.failures.record(BackendFailure::CallerAttribution);
        Err(CallBoundaryError::CallerAttribution)
    }

    /// Validates a retained legacy data address for one exact current owner.
    ///
    /// Known addon-image ranges must belong to `owner`. Unmapped addresses are
    /// admitted for heap and TLS compatibility under the caller's separate
    /// unsafe allocation-lifetime and synchronization contract. This method
    /// proves neither of those invariants; resolver failures fail closed.
    pub fn validate_retained_address(
        &self,
        owner: OwnerToken,
        address: NonZeroUsize,
    ) -> Result<(), CallBoundaryError> {
        if self
            .callers
            .retained_address_allowed_for_owner(address, owner)
        {
            return Ok(());
        }
        self.failures.record(BackendFailure::CallerAttribution);
        Err(CallBoundaryError::CallerAttribution)
    }

    /// Revalidates that an exact owner generation still accepts API calls.
    ///
    /// Registration paths use this after publishing a rollback-capable entry.
    /// If cleanup closed admission between initial attribution and insertion,
    /// the caller must remove that exact entry before returning failure.
    pub fn validate_current_owner(&self, owner: OwnerToken) -> Result<(), CallBoundaryError> {
        if self.callers.is_current_owner(owner) {
            return Ok(());
        }
        self.failures.record(BackendFailure::CallerAttribution);
        Err(CallBoundaryError::CallerAttribution)
    }

    /// Clones the exact callback gate from this boundary's attribution authority.
    pub fn callback_gate_for_current(
        &self,
        owner: OwnerToken,
    ) -> Result<Arc<CallbackGate>, CallBoundaryError> {
        self.callers
            .callback_gate_for_current(owner)
            .ok_or_else(|| {
                self.failures.record(BackendFailure::CallerAttribution);
                CallBoundaryError::CallerAttribution
            })
    }

    /// Copies and validates an identifier before it reaches a service.
    pub fn snapshot_identifier(
        &self,
        value: *const c_char,
    ) -> Result<NativeText, CallBoundaryError> {
        self.snapshot_text(self.memory.snapshot_identifier(value))
    }

    /// Copies and validates a filesystem path before it reaches a service.
    pub fn snapshot_path(&self, value: *const c_char) -> Result<NativeText, CallBoundaryError> {
        self.snapshot_text(self.memory.snapshot_path(value))
    }

    /// Copies and validates a URL before it reaches a service.
    pub fn snapshot_url(&self, value: *const c_char) -> Result<NativeText, CallBoundaryError> {
        self.snapshot_text(self.memory.snapshot_url(value))
    }

    /// Copies and validates a user-visible or diagnostic message.
    pub fn snapshot_message(&self, value: *const c_char) -> Result<NativeText, CallBoundaryError> {
        self.snapshot_text(self.memory.snapshot_message(value))
    }

    /// Copies a length-delimited native payload before it reaches a service.
    pub fn snapshot_buffer(
        &self,
        value: *const c_void,
        length: usize,
    ) -> Result<OwnedNativeBytes, CallBoundaryError> {
        self.memory.snapshot_buffer(value, length).map_err(|_| {
            self.failures.record(BackendFailure::NativeMemory);
            CallBoundaryError::NativeMemory
        })
    }

    /// Reads a retained one-byte compatibility cell without direct dereference.
    pub fn snapshot_u8(&self, value: *const u8) -> Result<u8, CallBoundaryError> {
        snapshot_u8(value).map_err(|_| {
            self.failures.record(BackendFailure::NativeMemory);
            CallBoundaryError::NativeMemory
        })
    }

    /// Writes a retained one-byte compatibility cell through checked native I/O.
    ///
    /// # Safety
    ///
    /// The caller must ensure `value` still denotes the live byte object that
    /// the attributed add-on registered, and that no Rust reference aliases it
    /// for the duration of this write. Page-level validation cannot prove
    /// allocation lifetime, object provenance, or Rust aliasing rules.
    pub unsafe fn write_u8(
        &self,
        value: *mut u8,
        replacement: u8,
    ) -> Result<(), CallBoundaryError> {
        // SAFETY: the exact retained-object invariant is delegated to this
        // method's caller and documented above; `write_u8` validates the page.
        unsafe { write_u8(value, replacement) }.map_err(|_| {
            self.failures.record(BackendFailure::NativeMemory);
            CallBoundaryError::NativeMemory
        })
    }

    /// Writes a retained pointer-width output through checked native I/O.
    ///
    /// # Safety
    ///
    /// The caller must ensure `destination` denotes a live, initialized, and
    /// properly aligned `usize` object owned by the attributed add-on. It must
    /// have exclusive access for the write and prevent concurrent lifetime or
    /// protection changes. Page validation cannot prove those invariants.
    pub unsafe fn write_usize(
        &self,
        destination: *mut usize,
        replacement: usize,
    ) -> Result<(), CallBoundaryError> {
        // SAFETY: the precise retained-object and exclusivity invariant is
        // delegated to this method's caller and documented above.
        unsafe { write_usize(destination, replacement) }.map_err(|_| {
            self.failures.record(BackendFailure::NativeMemory);
            CallBoundaryError::NativeMemory
        })
    }

    fn snapshot_text(
        &self,
        snapshot: Result<OwnedNativeString, nexus_native_memory::NativeMemoryError>,
    ) -> Result<NativeText, CallBoundaryError> {
        let value = snapshot.map_err(|_| {
            self.failures.record(BackendFailure::NativeMemory);
            CallBoundaryError::NativeMemory
        })?;

        NativeText::from_owned(value).inspect_err(|_error| {
            self.failures.record(BackendFailure::InvalidText);
        })
    }
}

impl fmt::Debug for NativeCallBoundary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeCallBoundary")
            .field("memory", &self.memory)
            .field("failures", &self.failures.snapshot())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;
    use std::ffi::CString;
    use std::sync::Arc;

    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::{CallbackGate, OwnerToken};
    use nexus_native_memory::NativeMemoryReader;

    use super::{CallBoundaryError, NativeCallBoundary, NativeText};
    use crate::BackendFailures;

    const OWNER: OwnerToken = OwnerToken {
        signature: 0xA11D_0001,
        generation: 7,
    };

    struct FixedOwner(Option<OwnerToken>);

    impl AddressOwnerResolver for FixedOwner {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            self.0
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            self.0 == Some(owner)
        }
    }

    struct UnmappedCurrentOwner;

    impl AddressOwnerResolver for UnmappedCurrentOwner {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            None
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            owner == OWNER
        }
    }

    struct GatedOwner {
        gate: Arc<CallbackGate>,
    }

    impl AddressOwnerResolver for GatedOwner {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            Some(OWNER)
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            owner == OWNER
        }

        fn callback_gate_for_current(&self, owner: OwnerToken) -> Option<Arc<CallbackGate>> {
            (owner == OWNER).then(|| Arc::clone(&self.gate))
        }
    }

    fn boundary_with_callers(
        owner: Option<OwnerToken>,
    ) -> (NativeCallBoundary, Arc<AddonCallerResolver>) {
        let callers = Arc::new(AddonCallerResolver::new(Arc::new(FixedOwner(owner))));
        let boundary = NativeCallBoundary::new(
            Arc::clone(&callers),
            NativeMemoryReader::default(),
            Arc::new(BackendFailures::new()),
        );
        (boundary, callers)
    }

    fn boundary(owner: Option<OwnerToken>) -> NativeCallBoundary {
        boundary_with_callers(owner).0
    }

    #[test]
    fn caller_resolution_is_generation_exact_and_counts_closed_failures() {
        let (present, callers) = boundary_with_callers(Some(OWNER));
        let _scope = callers
            .enter_owner_scope(OWNER)
            .expect("current test owner should enter an explicit scope");
        let hint = NonZeroUsize::new(1).expect("non-zero hint");
        assert_eq!(present.resolve_owner(Some(hint)), Ok(OWNER));

        let missing = boundary(None);
        assert_eq!(
            missing.resolve_owner(Some(hint)),
            Err(CallBoundaryError::CallerAttribution)
        );
        assert_eq!(missing.failures().snapshot().caller_attribution, 1);
    }

    #[test]
    fn callback_gate_comes_from_the_attribution_authority_and_fails_closed() {
        let gate = Arc::new(CallbackGate::open());
        let callers = Arc::new(AddonCallerResolver::new(Arc::new(GatedOwner {
            gate: Arc::clone(&gate),
        })));
        let present = NativeCallBoundary::new(
            callers,
            NativeMemoryReader::default(),
            Arc::new(BackendFailures::new()),
        );

        let resolved = present
            .callback_gate_for_current(OWNER)
            .expect("current owner should expose its callback gate");
        assert!(Arc::ptr_eq(&resolved, &gate));

        let missing = boundary(Some(OWNER));
        assert!(matches!(
            missing.callback_gate_for_current(OWNER),
            Err(CallBoundaryError::CallerAttribution)
        ));
        assert_eq!(missing.failures().snapshot().caller_attribution, 1);
    }

    #[test]
    fn retained_validation_distinguishes_unmapped_legacy_storage_from_strict_code_ownership() {
        let callers = Arc::new(AddonCallerResolver::new(Arc::new(UnmappedCurrentOwner)));
        let boundary = NativeCallBoundary::new(
            callers,
            NativeMemoryReader::default(),
            Arc::new(BackendFailures::new()),
        );
        let address = NonZeroUsize::new(0x10).expect("non-zero test address");

        assert_eq!(boundary.validate_retained_address(OWNER, address), Ok(()));
        assert_eq!(
            boundary.validate_owned_address(OWNER, address),
            Err(CallBoundaryError::CallerAttribution)
        );
    }

    #[test]
    fn native_text_validates_once_and_debug_is_content_free() {
        let value = CString::new("do-not-print-this").expect("test CString");
        let text = NativeText {
            value: value.into_string().expect("valid UTF-8"),
        };

        assert_eq!(text.as_str(), "do-not-print-this");
        assert_eq!(text.byte_len(), 17);
        let debug = format!("{text:?}");
        assert!(debug.contains("byte_len"));
        assert!(!debug.contains("do-not-print-this"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn invalid_native_utf8_is_copied_then_rejected_without_retention() {
        let boundary = boundary(Some(OWNER));
        let source = CString::from_vec_with_nul(vec![0xFF, 0]).expect("test CString");

        assert_eq!(
            boundary.snapshot_identifier(source.as_ptr()),
            Err(CallBoundaryError::InvalidText)
        );
        assert_eq!(boundary.failures().snapshot().invalid_text, 1);
    }
}
