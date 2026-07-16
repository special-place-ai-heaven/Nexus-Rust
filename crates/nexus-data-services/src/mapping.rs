use core::num::NonZeroUsize;
use std::sync::Arc;

use thiserror::Error;

/// How a named mapping was acquired by one open operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingDisposition {
    /// This operation created a new page-file-backed mapping.
    CreatedNew,
    /// This operation opened a mapping that already existed.
    OpenedExisting,
}

/// Redaction-safe failures produced by a named-mapping backend.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MappingFailure {
    /// The platform mapping name contained a nul code unit.
    #[error("the shared-memory name contains an embedded nul")]
    EmbeddedNul,
    /// The platform mapping name exceeded the supported object-name bound.
    #[error("the shared-memory name is too long")]
    NameTooLong,
    /// The requested size cannot be represented by the platform backend.
    #[error("the shared-memory size is unsupported")]
    SizeUnsupported,
    /// The platform could neither open nor create the mapping.
    #[error("the platform could not open or create the shared-memory mapping (error {code})")]
    OpenOrCreate {
        /// Platform error code, which never contains the mapping name.
        code: u32,
    },
    /// The platform could not map a view of the complete resource.
    #[error("the platform could not map the shared-memory view (error {code})")]
    MapView {
        /// Platform error code, which never contains the mapping name.
        code: u32,
    },
    /// An injected backend returned a view with a different length.
    #[error("the mapping backend returned length {actual}, expected {expected}")]
    ContractLength {
        /// Required resource length.
        expected: usize,
        /// Length reported by the backend.
        actual: usize,
    },
    /// No public-mapping implementation is available on this platform.
    #[error("public named mappings are unavailable on this platform")]
    UnsupportedPlatform,
}

/// Owned view of a stable shared-memory mapping.
///
/// # Safety
///
/// Implementors must keep `address()` valid and writable for exactly `len()`
/// bytes until the final owner is dropped. The address must not change when
/// the implementing value moves. Concurrent native access follows the same
/// synchronization contract as a Win32 shared-memory mapping.
pub unsafe trait MappingView: Send + Sync {
    /// Stable non-null base address of the mapped view.
    fn address(&self) -> NonZeroUsize;

    /// Exact mapped view length.
    fn len(&self) -> usize;

    /// Reports whether this open operation created the underlying mapping.
    fn disposition(&self) -> MappingDisposition;

    /// Returns whether the mapped view is empty.
    #[must_use]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Injectable factory for writable named shared-memory mappings.
pub trait MappingBackend: Send + Sync {
    /// Opens or creates one exact-length named mapping.
    ///
    /// Existing mappings must be returned without clearing or initializing
    /// their contents.
    fn open_or_create(
        &self,
        name: &str,
        size: NonZeroUsize,
    ) -> Result<Arc<dyn MappingView>, MappingFailure>;
}
