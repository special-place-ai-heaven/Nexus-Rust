//! Real Win32 memory-query and copy boundary tests.

#![cfg(windows)]

use core::ffi::{c_char, c_void};
use core::ptr::{self, NonNull};
use std::ffi::CString;
use std::mem::{size_of, zeroed};

use nexus_native_memory::{
    NativeMemoryError, NativeMemoryReader, OwnedNativeBytes, OwnedNativeString, snapshot_bytes,
    snapshot_c_string, snapshot_u8, write_u8, write_usize,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
    VirtualAlloc, VirtualFree, VirtualProtect,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

struct PageAllocation {
    base: NonNull<c_void>,
    page_size: usize,
    byte_len: usize,
}

impl PageAllocation {
    fn committed(page_count: usize) -> Self {
        Self::allocate(page_count, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
    }

    fn reserved(page_count: usize) -> Self {
        Self::allocate(page_count, MEM_RESERVE, PAGE_NOACCESS)
    }

    fn allocate(page_count: usize, allocation_type: u32, protection: u32) -> Self {
        assert!(page_count > 0);
        // SAFETY: all-zero is a valid initial state for this Win32 output structure.
        let mut system_info = unsafe { zeroed::<SYSTEM_INFO>() };
        // SAFETY: `system_info` is writable for the structure's full size.
        unsafe { GetSystemInfo(&mut system_info) };
        let page_size = usize::try_from(system_info.dwPageSize).expect("page size fits usize");
        let byte_len = page_size
            .checked_mul(page_count)
            .expect("test allocation length does not overflow");
        // SAFETY: the request uses documented allocation/protection flag combinations.
        let base = unsafe { VirtualAlloc(ptr::null(), byte_len, allocation_type, protection) };
        Self {
            base: NonNull::new(base).expect("test page allocation succeeds"),
            page_size,
            byte_len,
        }
    }

    fn byte_ptr(&self, offset: usize) -> *mut u8 {
        assert!(offset < self.byte_len);
        // SAFETY: the asserted offset stays inside this live allocation.
        unsafe { self.base.as_ptr().cast::<u8>().add(offset) }
    }

    fn protect_page(&self, page_index: usize, protection: u32) {
        let offset = self
            .page_size
            .checked_mul(page_index)
            .expect("test page offset does not overflow");
        assert!(offset < self.byte_len);
        let mut old_protection = 0_u32;
        // SAFETY: the address and page length designate one live page in this allocation.
        let changed = unsafe {
            VirtualProtect(
                self.byte_ptr(offset).cast(),
                self.page_size,
                protection,
                &mut old_protection,
            )
        };
        assert_ne!(changed, 0, "test page protection change succeeds");
    }
}

impl Drop for PageAllocation {
    fn drop(&mut self) {
        // SAFETY: `base` is the original live `VirtualAlloc` result and
        // `MEM_RELEASE` requires a zero size.
        let released = unsafe { VirtualFree(self.base.as_ptr(), 0, MEM_RELEASE) };
        debug_assert_ne!(released, 0);
    }
}

fn write_bytes(destination: *mut u8, bytes: &[u8]) {
    // SAFETY: callers provide writable test-owned storage large enough for `bytes`.
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len()) };
}

#[test]
fn readable_strings_and_buffers_become_owned_snapshots() {
    let string_source = CString::new("owned native snapshot").expect("literal has no nul");
    let string = NativeMemoryReader::default()
        .snapshot_message(string_source.as_ptr())
        .expect("readable C string copies");
    assert_eq!(string.as_bytes(), b"owned native snapshot");
    assert_eq!(
        string.as_c_str().to_bytes_with_nul(),
        b"owned native snapshot\0"
    );

    let mut source = [1_u8, 2, 3, 4, 5];
    let snapshot = NativeMemoryReader::default()
        .snapshot_buffer(source.as_ptr().cast(), source.len())
        .expect("readable buffer copies");
    source.fill(9);
    assert_eq!(snapshot.as_slice(), &[1, 2, 3, 4, 5]);
}

#[test]
fn one_byte_snapshot_and_write_have_explicit_toctou_semantics() {
    let page = PageAllocation::committed(1);
    let destination = page.byte_ptr(0);
    write_bytes(destination, &[41]);

    let before = snapshot_u8(destination.cast_const()).expect("readable byte snapshots");
    // SAFETY: `destination` is a live test-owned byte with no concurrent
    // accesses, and every `u8` value preserves its object invariant.
    unsafe { write_u8(destination, 99) }.expect("writable byte changes");
    assert_eq!(snapshot_u8(destination.cast_const()), Ok(99));

    write_bytes(destination, &[7]);
    assert_eq!(before, 41, "the earlier value is an owned snapshot");
    assert_eq!(snapshot_u8(destination.cast_const()), Ok(7));
}

#[test]
fn pointer_width_write_uses_owned_native_endian_source_bytes() {
    let page = PageAllocation::committed(1);
    let destination = page.byte_ptr(0).cast::<usize>();
    write_bytes(page.byte_ptr(0), &0_usize.to_ne_bytes());
    let expected = usize::MAX / 3;

    // SAFETY: the page-aligned destination contains a valid initialized
    // `usize`, remains live, and has no concurrent or aliased accesses.
    unsafe { write_usize(destination, expected) }.expect("writable usize changes");

    let snapshot = snapshot_bytes(
        destination.cast_const().cast(),
        size_of::<usize>(),
        size_of::<usize>(),
    )
    .expect("written usize snapshots");
    let bytes = snapshot
        .as_slice()
        .try_into()
        .expect("snapshot has one usize of bytes");
    assert_eq!(usize::from_ne_bytes(bytes), expected);
}

#[test]
fn readable_distinct_regions_copy_across_a_page_boundary() {
    let pages = PageAllocation::committed(2);
    let source_offset = pages.page_size - 4;
    let bytes = b"cross-region\0";
    write_bytes(pages.byte_ptr(source_offset), bytes);
    pages.protect_page(1, PAGE_READONLY);

    let string = snapshot_c_string(pages.byte_ptr(source_offset).cast::<c_char>(), bytes.len())
        .expect("nul-terminated string crosses readable regions");
    assert_eq!(string.as_bytes(), b"cross-region");

    let buffer = snapshot_bytes(
        pages.byte_ptr(source_offset).cast(),
        bytes.len(),
        bytes.len(),
    )
    .expect("buffer crosses readable regions");
    assert_eq!(buffer.as_slice(), bytes);
}

#[test]
fn missing_nul_is_bounded_and_reported() {
    let source = [b'x'; 64];
    assert_eq!(
        snapshot_c_string(source.as_ptr().cast(), source.len()),
        Err(NativeMemoryError::MissingNul {
            maximum: source.len(),
        })
    );
}

#[test]
fn no_access_and_guard_boundaries_fail_without_dereferencing_them() {
    let no_access = PageAllocation::committed(2);
    let source_offset = no_access.page_size - 8;
    write_bytes(no_access.byte_ptr(source_offset), &[b'a'; 8]);
    no_access.protect_page(1, PAGE_NOACCESS);
    assert_eq!(
        snapshot_c_string(no_access.byte_ptr(source_offset).cast(), 16),
        Err(NativeMemoryError::NoAccessRegion)
    );
    assert_eq!(
        snapshot_u8(no_access.byte_ptr(no_access.page_size).cast_const()),
        Err(NativeMemoryError::NoAccessRegion)
    );
    // SAFETY: the address is a live test-owned byte and would accept every
    // `u8`; the OS protection is intentionally left for the helper to reject.
    let no_access_write = unsafe { write_u8(no_access.byte_ptr(no_access.page_size), 1) };
    assert_eq!(no_access_write, Err(NativeMemoryError::NoAccessRegion));

    let guarded = PageAllocation::committed(2);
    let source_offset = guarded.page_size - 8;
    write_bytes(guarded.byte_ptr(source_offset), &[b'b'; 8]);
    guarded.protect_page(1, PAGE_READWRITE | PAGE_GUARD);
    assert_eq!(
        snapshot_c_string(guarded.byte_ptr(source_offset).cast(), 16),
        Err(NativeMemoryError::GuardedRegion)
    );
    assert_eq!(
        snapshot_u8(guarded.byte_ptr(guarded.page_size).cast_const()),
        Err(NativeMemoryError::GuardedRegion)
    );
    // SAFETY: the address is a live test-owned byte and would accept every
    // `u8`; the guard protection is intentionally rejected before writing.
    let guarded_write = unsafe { write_u8(guarded.byte_ptr(guarded.page_size), 1) };
    assert_eq!(guarded_write, Err(NativeMemoryError::GuardedRegion));
    // SAFETY: the page-aligned address contains a zero-initialized `usize`, is
    // live and unaliased, and the guard is intentionally rejected pre-write.
    let guarded_wide_write =
        unsafe { write_usize(guarded.byte_ptr(guarded.page_size).cast(), usize::MAX) };
    assert_eq!(guarded_wide_write, Err(NativeMemoryError::GuardedRegion));
}

#[test]
fn reserved_but_uncommitted_memory_is_rejected() {
    let reserved = PageAllocation::reserved(1);
    assert_eq!(
        snapshot_bytes(reserved.base.as_ptr(), 1, 1),
        Err(NativeMemoryError::UncommittedRegion)
    );
    assert_eq!(
        snapshot_u8(reserved.base.as_ptr().cast()),
        Err(NativeMemoryError::UncommittedRegion)
    );
}

#[test]
fn null_overflow_invalid_and_oversized_requests_fail_before_copying() {
    assert_eq!(
        snapshot_c_string(ptr::null::<c_char>(), 1),
        Err(NativeMemoryError::NullPointer)
    );
    assert_eq!(
        snapshot_bytes(ptr::null::<c_void>(), 0, 1),
        Err(NativeMemoryError::NullPointer)
    );
    assert_eq!(
        snapshot_u8(ptr::null::<u8>()),
        Err(NativeMemoryError::NullPointer)
    );
    // SAFETY: null is accepted as an error-only numeric address and is
    // rejected before the operating-system write path.
    let null_write = unsafe { write_u8(ptr::null_mut::<u8>(), 1) };
    assert_eq!(null_write, Err(NativeMemoryError::NullPointer));
    // SAFETY: null is accepted as an error-only numeric address and rejected
    // before region classification or writing.
    let null_wide_write = unsafe { write_usize(ptr::null_mut::<usize>(), usize::MAX) };
    assert_eq!(null_wide_write, Err(NativeMemoryError::NullPointer));

    let byte = 7_u8;
    assert_eq!(
        snapshot_c_string((&raw const byte).cast(), 0),
        Err(NativeMemoryError::InvalidLimit)
    );
    assert_eq!(
        snapshot_bytes((&raw const byte).cast(), 1, 0),
        Err(NativeMemoryError::InvalidLimit)
    );
    assert_eq!(
        snapshot_bytes((&raw const byte).cast(), 2, 1),
        Err(NativeMemoryError::LengthExceedsLimit {
            requested: 2,
            maximum: 1,
        })
    );

    assert_eq!(
        snapshot_c_string(ptr::without_provenance::<c_char>(usize::MAX), 1),
        Err(NativeMemoryError::AddressOverflow)
    );
    assert_eq!(
        snapshot_bytes(ptr::without_provenance::<c_void>(usize::MAX), 1, 1),
        Err(NativeMemoryError::AddressOverflow)
    );
    assert_eq!(
        snapshot_u8(ptr::without_provenance::<u8>(usize::MAX)),
        Err(NativeMemoryError::AddressOverflow)
    );
    // SAFETY: the overflowing address is rejected before any operating-system
    // write, so it never needs to designate storage.
    let overflow_write = unsafe { write_u8(ptr::without_provenance_mut::<u8>(usize::MAX), 1) };
    assert_eq!(overflow_write, Err(NativeMemoryError::AddressOverflow));
    // SAFETY: the range overflows and is rejected before any OS write.
    let overflow_wide_write =
        unsafe { write_usize(ptr::without_provenance_mut::<usize>(usize::MAX), usize::MAX) };
    assert_eq!(overflow_wide_write, Err(NativeMemoryError::AddressOverflow));

    let read_only = PageAllocation::committed(1);
    read_only.protect_page(0, PAGE_READONLY);
    // SAFETY: the address is a live test-owned byte and would accept every
    // `u8`; read-only page protection is intentionally rejected by the helper.
    let read_only_write = unsafe { write_u8(read_only.byte_ptr(0), 1) };
    assert_eq!(read_only_write, Err(NativeMemoryError::UnwritableRegion));
}

#[test]
fn snapshot_and_result_debug_never_echo_payloads() {
    let string_marker = "debug-marker-native-string";
    let string_source = CString::new(string_marker).expect("marker has no nul");
    let string = snapshot_c_string(string_source.as_ptr(), string_marker.len() + 1)
        .expect("marker string copies");
    let string_debug = format!("{string:?}");
    let result_debug = format!("{:?}", Ok::<OwnedNativeString, NativeMemoryError>(string));
    assert!(!string_debug.contains(string_marker));
    assert!(!result_debug.contains(string_marker));
    assert!(string_debug.contains("byte_len"));

    let bytes_marker = b"debug-marker-native-bytes";
    let bytes = snapshot_bytes(
        bytes_marker.as_ptr().cast(),
        bytes_marker.len(),
        bytes_marker.len(),
    )
    .expect("marker bytes copy");
    let bytes_debug = format!("{bytes:?}");
    let result_debug = format!("{:?}", Ok::<OwnedNativeBytes, NativeMemoryError>(bytes));
    assert!(!bytes_debug.contains("debug-marker-native-bytes"));
    assert!(!result_debug.contains("debug-marker-native-bytes"));

    let error = NativeMemoryError::LengthExceedsLimit {
        requested: 23,
        maximum: 7,
    };
    assert_eq!(
        format!("{error:?}"),
        "LengthExceedsLimit { requested: 23, maximum: 7 }"
    );
    assert_eq!(
        error.to_string(),
        "native memory length 23 exceeded maximum 7"
    );
}
