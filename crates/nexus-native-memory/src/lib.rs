//! Owned, bounded snapshots of add-on supplied native memory.
//!
//! On Windows, this crate validates each committed memory region with
//! `VirtualQuery` and copies through `ReadProcessMemory(GetCurrentProcess())`.
//! Rust never directly dereferences the caller's pointer. A narrowly scoped,
//! unsafe byte write uses `WriteProcessMemory(GetCurrentProcess())` after the
//! same region checks. Other platforms return
//! [`NativeMemoryError::Unsupported`] deterministically.
//!
//! The Windows query and copy are separate operating-system calls. Native code
//! in the same process can mutate bytes or protections between those calls, or
//! while a multi-region copy is underway. A successful copy is therefore not
//! an atomic view of the source. The returned snapshot owns its bytes, so later
//! source mutations cannot change it.

use core::ffi::{c_char, c_void};
use core::fmt;
use std::error::Error;
use std::ffi::{CStr, CString};

#[cfg(not(windows))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
use unsupported as platform;
#[cfg(windows)]
use windows as platform;

/// Default maximum bytes inspected for an identifier, including its nul.
pub const DEFAULT_IDENTIFIER_MAX_BYTES: usize = 1_024;
/// Default maximum bytes inspected for a path, including its nul.
pub const DEFAULT_PATH_MAX_BYTES: usize = 32 * 1_024;
/// Default maximum bytes inspected for a URL, including its nul.
pub const DEFAULT_URL_MAX_BYTES: usize = 16 * 1_024;
/// Default maximum bytes inspected for a message, including its nul.
pub const DEFAULT_MESSAGE_MAX_BYTES: usize = 64 * 1_024;
/// Default maximum bytes copied for an arbitrary binary buffer.
pub const DEFAULT_BINARY_MAX_BYTES: usize = 16 * 1_024 * 1_024;

/// Configurable hard maxima for each native input category.
///
/// String maxima include the terminating nul byte, so a maximum of `N`
/// accepts at most `N - 1` payload bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeMemoryLimits {
    identifier: usize,
    path: usize,
    url: usize,
    message: usize,
    binary: usize,
}

impl NativeMemoryLimits {
    /// Creates a validated set of nonzero hard maxima.
    ///
    /// # Errors
    ///
    /// Returns [`NativeMemoryError::InvalidLimit`] when any maximum is zero.
    pub const fn new(
        identifier: usize,
        path: usize,
        url: usize,
        message: usize,
        binary: usize,
    ) -> Result<Self, NativeMemoryError> {
        if identifier == 0 || path == 0 || url == 0 || message == 0 || binary == 0 {
            return Err(NativeMemoryError::InvalidLimit);
        }

        Ok(Self {
            identifier,
            path,
            url,
            message,
            binary,
        })
    }

    /// Returns the identifier maximum, including the terminating nul.
    #[must_use]
    pub const fn identifier(self) -> usize {
        self.identifier
    }

    /// Returns the path maximum, including the terminating nul.
    #[must_use]
    pub const fn path(self) -> usize {
        self.path
    }

    /// Returns the URL maximum, including the terminating nul.
    #[must_use]
    pub const fn url(self) -> usize {
        self.url
    }

    /// Returns the message maximum, including the terminating nul.
    #[must_use]
    pub const fn message(self) -> usize {
        self.message
    }

    /// Returns the arbitrary binary-buffer maximum.
    #[must_use]
    pub const fn binary(self) -> usize {
        self.binary
    }
}

impl Default for NativeMemoryLimits {
    fn default() -> Self {
        Self {
            identifier: DEFAULT_IDENTIFIER_MAX_BYTES,
            path: DEFAULT_PATH_MAX_BYTES,
            url: DEFAULT_URL_MAX_BYTES,
            message: DEFAULT_MESSAGE_MAX_BYTES,
            binary: DEFAULT_BINARY_MAX_BYTES,
        }
    }
}

/// Stateless native-memory snapshot reader with category-specific limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeMemoryReader {
    limits: NativeMemoryLimits,
}

impl NativeMemoryReader {
    /// Creates a reader using the supplied validated limits.
    #[must_use]
    pub const fn new(limits: NativeMemoryLimits) -> Self {
        Self { limits }
    }

    /// Returns this reader's hard maxima.
    #[must_use]
    pub const fn limits(self) -> NativeMemoryLimits {
        self.limits
    }

    /// Copies a nul-terminated identifier into owned memory.
    ///
    /// The source may be any numeric address. On Windows, inaccessible or
    /// malformed ranges return an error rather than being dereferenced by Rust.
    /// Same-process mutation can race the operating-system copy; the returned
    /// owned snapshot is stable after a successful return.
    ///
    /// # Errors
    ///
    /// Returns a closed [`NativeMemoryError`] category when validation or the
    /// bounded copy fails.
    pub fn snapshot_identifier(
        self,
        source: *const c_char,
    ) -> Result<OwnedNativeString, NativeMemoryError> {
        snapshot_c_string(source, self.limits.identifier)
    }

    /// Copies a nul-terminated path into owned memory.
    ///
    /// # Errors
    ///
    /// Returns a closed [`NativeMemoryError`] category when validation or the
    /// bounded copy fails.
    pub fn snapshot_path(
        self,
        source: *const c_char,
    ) -> Result<OwnedNativeString, NativeMemoryError> {
        snapshot_c_string(source, self.limits.path)
    }

    /// Copies a nul-terminated URL into owned memory.
    ///
    /// # Errors
    ///
    /// Returns a closed [`NativeMemoryError`] category when validation or the
    /// bounded copy fails.
    pub fn snapshot_url(
        self,
        source: *const c_char,
    ) -> Result<OwnedNativeString, NativeMemoryError> {
        snapshot_c_string(source, self.limits.url)
    }

    /// Copies a nul-terminated message into owned memory.
    ///
    /// # Errors
    ///
    /// Returns a closed [`NativeMemoryError`] category when validation or the
    /// bounded copy fails.
    pub fn snapshot_message(
        self,
        source: *const c_char,
    ) -> Result<OwnedNativeString, NativeMemoryError> {
        snapshot_c_string(source, self.limits.message)
    }

    /// Copies an exact-length binary buffer into owned memory.
    ///
    /// Requests larger than the configured binary limit are rejected before
    /// allocation. Same-process mutation can race the operating-system copy;
    /// the returned owned snapshot is stable after a successful return.
    ///
    /// # Errors
    ///
    /// Returns a closed [`NativeMemoryError`] category when validation or the
    /// exact-length copy fails.
    pub fn snapshot_buffer(
        self,
        source: *const c_void,
        length: usize,
    ) -> Result<OwnedNativeBytes, NativeMemoryError> {
        snapshot_bytes(source, length, self.limits.binary)
    }
}

impl Default for NativeMemoryReader {
    fn default() -> Self {
        Self::new(NativeMemoryLimits::default())
    }
}

/// An owned snapshot of a bounded native C string.
#[derive(Clone, Eq, PartialEq)]
pub struct OwnedNativeString {
    value: CString,
}

impl OwnedNativeString {
    /// Returns the snapshot as a borrowed C string.
    #[must_use]
    pub fn as_c_str(&self) -> &CStr {
        &self.value
    }

    /// Returns the payload bytes without the terminating nul.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.value.as_bytes()
    }

    /// Returns the payload length without the terminating nul.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.value.as_bytes().len()
    }

    /// Consumes the snapshot and returns its owned C string.
    #[must_use]
    pub fn into_c_string(self) -> CString {
        self.value
    }
}

impl fmt::Debug for OwnedNativeString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnedNativeString")
            .field("byte_len", &self.byte_len())
            .finish()
    }
}

/// An owned snapshot of an exact-length native byte buffer.
#[derive(Clone, Eq, PartialEq)]
pub struct OwnedNativeBytes {
    value: Vec<u8>,
}

impl OwnedNativeBytes {
    /// Returns the snapshot bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.value
    }

    /// Returns the number of snapshot bytes.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.value.len()
    }

    /// Returns whether the snapshot is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// Consumes the snapshot and returns its owned bytes.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.value
    }
}

impl fmt::Debug for OwnedNativeBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnedNativeBytes")
            .field("byte_len", &self.byte_len())
            .finish()
    }
}

/// Closed failure categories for native-memory operations.
///
/// `Debug` and `Display` expose only categories and relevant lengths. They
/// never contain source bytes, pointer addresses, or platform error strings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NativeMemoryError {
    /// Native-memory operations are unsupported on this operating system.
    Unsupported,
    /// The native pointer was null.
    NullPointer,
    /// A requested hard maximum was zero.
    InvalidLimit,
    /// The requested byte length exceeded its hard maximum.
    LengthExceedsLimit {
        /// Requested byte length.
        requested: usize,
        /// Configured maximum byte length.
        maximum: usize,
    },
    /// The native address range overflowed the process address space.
    AddressOverflow,
    /// Rust could not allocate the bounded owned destination.
    AllocationFailed {
        /// Requested destination length.
        requested: usize,
    },
    /// The operating system could not describe the native region.
    MemoryQueryFailed,
    /// The operating system returned a malformed or non-progressing region.
    MalformedMemoryRegion,
    /// The native region was not committed.
    UncommittedRegion,
    /// The native region had guard-page protection.
    GuardedRegion,
    /// The native region had no-access protection.
    NoAccessRegion,
    /// The native region did not have a recognized readable protection.
    UnreadableRegion,
    /// The native region did not have a recognized writable protection.
    UnwritableRegion,
    /// The operating-system copy failed without copying a prefix.
    ReadFailed,
    /// The operating-system copy did not copy the entire requested chunk.
    PartialRead {
        /// Requested chunk length.
        expected: usize,
        /// Reported copied prefix length.
        actual: usize,
    },
    /// The operating-system write failed without writing the byte.
    WriteFailed,
    /// The operating-system write did not write the entire requested chunk.
    PartialWrite {
        /// Requested chunk length.
        expected: usize,
        /// Reported written prefix length.
        actual: usize,
    },
    /// No nul byte appeared within the configured maximum.
    MissingNul {
        /// Maximum number of bytes inspected.
        maximum: usize,
    },
    /// An internal owned-snapshot invariant was violated.
    SnapshotInvariantViolation,
}

impl fmt::Display for NativeMemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Unsupported => formatter.write_str("native memory operations are unsupported"),
            Self::NullPointer => formatter.write_str("native memory pointer was null"),
            Self::InvalidLimit => formatter.write_str("native memory maximum was zero"),
            Self::LengthExceedsLimit { requested, maximum } => write!(
                formatter,
                "native memory length {requested} exceeded maximum {maximum}"
            ),
            Self::AddressOverflow => formatter.write_str("native memory range overflowed"),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "native memory destination allocation failed for length {requested}"
            ),
            Self::MemoryQueryFailed => formatter.write_str("native memory query failed"),
            Self::MalformedMemoryRegion => {
                formatter.write_str("native memory query returned a malformed region")
            }
            Self::UncommittedRegion => formatter.write_str("native memory region was uncommitted"),
            Self::GuardedRegion => formatter.write_str("native memory region was guarded"),
            Self::NoAccessRegion => formatter.write_str("native memory region had no access"),
            Self::UnreadableRegion => formatter.write_str("native memory region was unreadable"),
            Self::UnwritableRegion => formatter.write_str("native memory region was unwritable"),
            Self::ReadFailed => formatter.write_str("native memory copy failed"),
            Self::PartialRead { expected, actual } => write!(
                formatter,
                "native memory copy was partial ({actual} of {expected} bytes)"
            ),
            Self::WriteFailed => formatter.write_str("native memory write failed"),
            Self::PartialWrite { expected, actual } => write!(
                formatter,
                "native memory write was partial ({actual} of {expected} bytes)"
            ),
            Self::MissingNul { maximum } => write!(
                formatter,
                "native memory string had no nul within {maximum} bytes"
            ),
            Self::SnapshotInvariantViolation => {
                formatter.write_str("native memory snapshot invariant failed")
            }
        }
    }
}

impl Error for NativeMemoryError {}

/// Copies a bounded native C string into owned memory.
///
/// `maximum_bytes` includes the terminating nul. The source may be any numeric
/// address: on Windows, invalid or inaccessible addresses return a closed error
/// instead of being dereferenced by Rust. The query and copy are not atomic
/// with concurrent same-process native mutation; the returned snapshot is
/// stable after a successful return.
///
/// # Errors
///
/// Returns [`NativeMemoryError`] when the platform is unsupported, the bound
/// is invalid, the address range is inaccessible, copying fails, or no nul is
/// found within the bound.
pub fn snapshot_c_string(
    source: *const c_char,
    maximum_bytes: usize,
) -> Result<OwnedNativeString, NativeMemoryError> {
    let bytes = platform::copy_c_string(source, maximum_bytes)?;
    let value = CString::from_vec_with_nul(bytes)
        .map_err(|_| NativeMemoryError::SnapshotInvariantViolation)?;
    Ok(OwnedNativeString { value })
}

/// Copies an exact-length native byte buffer into owned memory.
///
/// Lengths greater than `maximum_bytes` are rejected before allocation. The
/// source may be any numeric address: on Windows, invalid or inaccessible
/// addresses return a closed error instead of being dereferenced by Rust. The
/// query and copy are not atomic with concurrent same-process native mutation;
/// the returned snapshot is stable after a successful return.
///
/// # Errors
///
/// Returns [`NativeMemoryError`] when the platform is unsupported, the bound
/// or range is invalid, the region is inaccessible, or the exact copy fails.
pub fn snapshot_bytes(
    source: *const c_void,
    length: usize,
    maximum_bytes: usize,
) -> Result<OwnedNativeBytes, NativeMemoryError> {
    platform::copy_bytes(source, length, maximum_bytes).map(|value| OwnedNativeBytes { value })
}

/// Snapshots one byte from a native address without directly dereferencing it.
///
/// On Windows, the region query and operating-system copy are separate calls.
/// Concurrent native mutation can therefore race the copy. The returned byte
/// is the value copied by the operating system during that call and is
/// independent of later source mutations.
///
/// # Errors
///
/// Returns [`NativeMemoryError`] when the platform is unsupported, the address
/// overflows, the region is inaccessible, or the one-byte copy fails.
pub fn snapshot_u8(source: *const u8) -> Result<u8, NativeMemoryError> {
    platform::snapshot_u8(source)
}

/// Writes one byte to a native address without directly dereferencing it.
///
/// On Windows, the region query and `WriteProcessMemory` call are not atomic.
/// Protection, allocation lifetime, and other native writes can race this
/// operation. Success means the operating system reported writing the byte;
/// native code may change it again immediately afterward.
///
/// # Safety
///
/// On Windows, null and numerically overflowing addresses are accepted only to
/// produce their documented errors. Every other `destination` must remain part
/// of a live native allocation throughout this call, although its current page
/// protection may be inaccessible and produce an error. If a write occurs,
/// writing `value` must preserve every type and object invariant of the
/// destination, and the caller must prevent conflicting Rust or native accesses
/// and concurrent allocation/protection changes. Page accessibility alone
/// cannot establish these requirements. Other platforms do not inspect the
/// pointer and return [`NativeMemoryError::Unsupported`].
///
/// # Errors
///
/// Returns [`NativeMemoryError`] when the platform is unsupported, the address
/// overflows, the region is not writable, or the one-byte write fails.
pub unsafe fn write_u8(destination: *mut u8, value: u8) -> Result<(), NativeMemoryError> {
    // SAFETY: the public contract above is exactly the platform helper's
    // contract; this wrapper adds no weaker assumptions.
    unsafe { platform::write_u8(destination, value) }
}

/// Writes one native-endian pointer-width integer without directly
/// dereferencing its destination.
///
/// On Windows, all covered regions are classified before one
/// `WriteProcessMemory` call. The checks and write are not atomic, and success
/// does not prevent native code from changing the value immediately afterward.
/// Neither the destination address nor `value` appears in errors or `Debug`.
///
/// # Safety
///
/// On Windows, null and numerically overflowing addresses are accepted only to
/// produce their documented errors. Every other `destination` must be properly
/// aligned, carry valid provenance for a live initialized `usize` object, and
/// remain live throughout this call. The caller must provide exclusive access
/// for the write and prevent concurrent Rust/native accesses and allocation or
/// protection changes. Page accessibility alone cannot prove these invariants.
/// Other platforms do not inspect the pointer or value and return
/// [`NativeMemoryError::Unsupported`].
///
/// # Errors
///
/// Returns [`NativeMemoryError`] when the platform is unsupported, the range
/// overflows, a covered region is not writable, or the exact write fails.
pub unsafe fn write_usize(destination: *mut usize, value: usize) -> Result<(), NativeMemoryError> {
    // SAFETY: the public contract above is exactly the platform helper's
    // contract; this wrapper adds no weaker assumptions.
    unsafe { platform::write_usize(destination, value) }
}
