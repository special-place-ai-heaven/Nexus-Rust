use core::ffi::{c_char, c_void};

use crate::NativeMemoryError;

pub(super) fn copy_c_string(
    _source: *const c_char,
    _maximum_bytes: usize,
) -> Result<Vec<u8>, NativeMemoryError> {
    Err(NativeMemoryError::Unsupported)
}

pub(super) fn copy_bytes(
    _source: *const c_void,
    _length: usize,
    _maximum_bytes: usize,
) -> Result<Vec<u8>, NativeMemoryError> {
    Err(NativeMemoryError::Unsupported)
}

pub(super) fn snapshot_u8(_source: *const u8) -> Result<u8, NativeMemoryError> {
    Err(NativeMemoryError::Unsupported)
}

/// # Safety
///
/// This unsupported-platform stub never inspects `destination`; its caller
/// carries the cross-platform contract used by the Windows implementation.
pub(super) unsafe fn write_u8(_destination: *mut u8, _value: u8) -> Result<(), NativeMemoryError> {
    Err(NativeMemoryError::Unsupported)
}

/// # Safety
///
/// This unsupported-platform stub never inspects `destination` or `value`; its
/// caller carries the cross-platform contract used by the Windows implementation.
pub(super) unsafe fn write_usize(
    _destination: *mut usize,
    _value: usize,
) -> Result<(), NativeMemoryError> {
    Err(NativeMemoryError::Unsupported)
}
