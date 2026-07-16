use core::ffi::{c_char, c_void};
use core::fmt;
use core::num::NonZeroUsize;
use std::sync::Arc;

use nexus_addon_ffi::AddonCallerResolver;
use nexus_core::OwnerToken;
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

    /// Resolves the exact live add-on generation responsible for one call.
    pub fn resolve_owner(
        &self,
        function_hint: Option<NonZeroUsize>,
    ) -> Result<OwnerToken, CallBoundaryError> {
        self.callers.resolve(function_hint).ok_or_else(|| {
            self.failures.record(BackendFailure::CallerAttribution);
            CallBoundaryError::CallerAttribution
        })
    }

    /// Resolves a caller using a nullable native function-address hint.
    pub fn resolve_owner_for_address(
        &self,
        function_hint: *const c_void,
    ) -> Result<OwnerToken, CallBoundaryError> {
        self.resolve_owner(NonZeroUsize::new(function_hint as usize))
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
    use nexus_core::OwnerToken;
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

    fn boundary(owner: Option<OwnerToken>) -> NativeCallBoundary {
        NativeCallBoundary::new(
            Arc::new(AddonCallerResolver::new(Arc::new(FixedOwner(owner)))),
            NativeMemoryReader::default(),
            Arc::new(BackendFailures::new()),
        )
    }

    #[test]
    fn caller_resolution_is_generation_exact_and_counts_closed_failures() {
        let present = boundary(Some(OWNER));
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
