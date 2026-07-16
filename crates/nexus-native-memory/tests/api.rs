//! Cross-platform public API contract tests.

use core::ffi::{c_char, c_void};

use nexus_native_memory::{
    DEFAULT_BINARY_MAX_BYTES, DEFAULT_IDENTIFIER_MAX_BYTES, DEFAULT_MESSAGE_MAX_BYTES,
    DEFAULT_PATH_MAX_BYTES, DEFAULT_URL_MAX_BYTES, NativeMemoryError, NativeMemoryLimits,
    NativeMemoryReader, snapshot_bytes, snapshot_c_string, snapshot_u8, write_u8, write_usize,
};

#[test]
fn conservative_defaults_and_custom_limits_are_exact() {
    let defaults = NativeMemoryLimits::default();
    assert_eq!(defaults.identifier(), DEFAULT_IDENTIFIER_MAX_BYTES);
    assert_eq!(defaults.path(), DEFAULT_PATH_MAX_BYTES);
    assert_eq!(defaults.url(), DEFAULT_URL_MAX_BYTES);
    assert_eq!(defaults.message(), DEFAULT_MESSAGE_MAX_BYTES);
    assert_eq!(defaults.binary(), DEFAULT_BINARY_MAX_BYTES);

    let custom = NativeMemoryLimits::new(11, 22, 33, 44, 55).expect("nonzero limits");
    assert_eq!(NativeMemoryReader::new(custom).limits(), custom);
}

#[test]
fn every_zero_limit_is_rejected() {
    for limits in [
        (0, 1, 1, 1, 1),
        (1, 0, 1, 1, 1),
        (1, 1, 0, 1, 1),
        (1, 1, 1, 0, 1),
        (1, 1, 1, 1, 0),
    ] {
        assert_eq!(
            NativeMemoryLimits::new(limits.0, limits.1, limits.2, limits.3, limits.4),
            Err(NativeMemoryError::InvalidLimit)
        );
    }
}

#[cfg(not(windows))]
#[test]
fn unsupported_platform_is_deterministic_for_all_pointer_shapes() {
    assert_eq!(
        snapshot_c_string(core::ptr::null::<c_char>(), usize::MAX),
        Err(NativeMemoryError::Unsupported)
    );
    assert_eq!(
        snapshot_bytes(core::ptr::null::<c_void>(), usize::MAX, 0),
        Err(NativeMemoryError::Unsupported)
    );
    assert_eq!(
        snapshot_u8(core::ptr::null::<u8>()),
        Err(NativeMemoryError::Unsupported)
    );
    // SAFETY: the unsupported-platform implementation does not inspect the
    // pointer and deterministically returns before attempting a write.
    let write = unsafe { write_u8(core::ptr::null_mut::<u8>(), u8::MAX) };
    assert_eq!(write, Err(NativeMemoryError::Unsupported));
    // SAFETY: the unsupported-platform implementation inspects neither the
    // pointer nor value and returns before attempting a write.
    let wide_write = unsafe { write_usize(core::ptr::null_mut::<usize>(), usize::MAX) };
    assert_eq!(wide_write, Err(NativeMemoryError::Unsupported));
}

#[cfg(windows)]
#[test]
fn platform_specific_imports_remain_used() {
    let _ = core::ptr::null::<c_char>();
    let _ = core::ptr::null::<c_void>();
    let _ = snapshot_c_string;
    let _ = snapshot_bytes;
    let _ = snapshot_u8;
    let _ = write_u8;
    let _ = write_usize;
}
