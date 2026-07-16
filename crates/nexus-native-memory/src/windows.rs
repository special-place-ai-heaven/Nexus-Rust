use core::ffi::{c_char, c_void};
use core::mem::{size_of, zeroed};
use core::ptr::without_provenance;

use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
    PAGE_WRITECOPY, VirtualQuery,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::NativeMemoryError;

const STRING_COPY_CHUNK_BYTES: usize = 16 * 1_024;

#[derive(Clone, Copy)]
enum MemoryAccess {
    Read,
    Write,
}

pub(super) fn copy_c_string(
    source: *const c_char,
    maximum_bytes: usize,
) -> Result<Vec<u8>, NativeMemoryError> {
    let address = validate_address(source.cast(), maximum_bytes)?;
    let mut output = Vec::new();
    let mut copied = 0_usize;

    while copied < maximum_bytes {
        let cursor = address
            .checked_add(copied)
            .ok_or(NativeMemoryError::AddressOverflow)?;
        let region_end = region_end(cursor, MemoryAccess::Read)?;
        let available = region_end
            .checked_sub(cursor)
            .ok_or(NativeMemoryError::MalformedMemoryRegion)?;
        let step = available
            .min(maximum_bytes - copied)
            .min(STRING_COPY_CHUNK_BYTES);
        if step == 0 {
            return Err(NativeMemoryError::MalformedMemoryRegion);
        }

        let mut chunk = allocate_zeroed(step)?;
        read_chunk(cursor, &mut chunk)?;
        if let Some(nul) = chunk.iter().position(|byte| *byte == 0) {
            append_bytes(&mut output, &chunk[..=nul])?;
            return Ok(output);
        }

        append_bytes(&mut output, &chunk)?;
        copied += step;
    }

    Err(NativeMemoryError::MissingNul {
        maximum: maximum_bytes,
    })
}

pub(super) fn copy_bytes(
    source: *const c_void,
    length: usize,
    maximum_bytes: usize,
) -> Result<Vec<u8>, NativeMemoryError> {
    if source.is_null() {
        return Err(NativeMemoryError::NullPointer);
    }
    if maximum_bytes == 0 {
        return Err(NativeMemoryError::InvalidLimit);
    }
    if length > maximum_bytes {
        return Err(NativeMemoryError::LengthExceedsLimit {
            requested: length,
            maximum: maximum_bytes,
        });
    }

    let address = source.addr();
    address
        .checked_add(length)
        .ok_or(NativeMemoryError::AddressOverflow)?;
    if length == 0 {
        return Ok(Vec::new());
    }

    let mut output = allocate_zeroed(length)?;
    read_exact(address, &mut output)?;
    Ok(output)
}

pub(super) fn snapshot_u8(source: *const u8) -> Result<u8, NativeMemoryError> {
    let address = validate_address(source.cast(), 1)?;
    let mut output = [0_u8; 1];
    read_exact(address, &mut output)?;
    Ok(output[0])
}

/// # Safety
///
/// Null and overflowing addresses are accepted only for their documented
/// errors. Every other `destination` must remain part of a live native
/// allocation. If a write occurs, `value` must preserve the destination
/// object's invariants, and the caller must prevent conflicting accesses and
/// allocation/protection changes for the duration of this call.
pub(super) unsafe fn write_u8(destination: *mut u8, value: u8) -> Result<(), NativeMemoryError> {
    let address = validate_address(destination.cast_const().cast(), 1)?;
    validate_region_range(address, 1, MemoryAccess::Write)?;
    let source = [value];
    // SAFETY: the caller supplied the destination validity, invariant, and
    // synchronization guarantees documented on this function. The source is
    // a live one-byte Rust array for the duration of the operating-system call.
    unsafe { write_chunk(address, &source) }
}

/// # Safety
///
/// Null and overflowing addresses are accepted only for their documented
/// errors. Every other `destination` must be properly aligned, carry valid
/// provenance for a live initialized `usize`, remain live, and have exclusive
/// write access for this call. The caller must also prevent allocation and
/// protection changes.
pub(super) unsafe fn write_usize(
    destination: *mut usize,
    value: usize,
) -> Result<(), NativeMemoryError> {
    let length = size_of::<usize>();
    let address = validate_address(destination.cast_const().cast(), length)?;
    validate_region_range(address, length, MemoryAccess::Write)?;
    let source = value.to_ne_bytes();
    // SAFETY: the caller guarantees one live, aligned, exclusively accessible
    // `usize` destination. The source contains its exact native-endian bytes.
    unsafe { write_chunk(address, &source) }
}

fn validate_address(
    source: *const c_void,
    maximum_bytes: usize,
) -> Result<usize, NativeMemoryError> {
    if source.is_null() {
        return Err(NativeMemoryError::NullPointer);
    }
    if maximum_bytes == 0 {
        return Err(NativeMemoryError::InvalidLimit);
    }

    let address = source.addr();
    address
        .checked_add(maximum_bytes)
        .ok_or(NativeMemoryError::AddressOverflow)?;
    Ok(address)
}

fn read_exact(address: usize, destination: &mut [u8]) -> Result<(), NativeMemoryError> {
    let mut copied = 0_usize;
    while copied < destination.len() {
        let cursor = address
            .checked_add(copied)
            .ok_or(NativeMemoryError::AddressOverflow)?;
        let region_end = region_end(cursor, MemoryAccess::Read)?;
        let available = region_end
            .checked_sub(cursor)
            .ok_or(NativeMemoryError::MalformedMemoryRegion)?;
        let step = available.min(destination.len() - copied);
        if step == 0 {
            return Err(NativeMemoryError::MalformedMemoryRegion);
        }
        let next = copied
            .checked_add(step)
            .ok_or(NativeMemoryError::AddressOverflow)?;
        let chunk = destination
            .get_mut(copied..next)
            .ok_or(NativeMemoryError::SnapshotInvariantViolation)?;
        read_chunk(cursor, chunk)?;
        copied = next;
    }
    Ok(())
}

fn region_end(address: usize, required_access: MemoryAccess) -> Result<usize, NativeMemoryError> {
    // SAFETY: all-zero is a valid initial state for this Win32 output structure.
    let mut info = unsafe { zeroed::<MEMORY_BASIC_INFORMATION>() };
    // SAFETY: `info` is writable for the exact supplied size. `VirtualQuery`
    // treats `address` as a numeric process address and does not dereference it
    // through Rust.
    let returned = unsafe {
        VirtualQuery(
            without_provenance::<c_void>(address),
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if returned < size_of::<MEMORY_BASIC_INFORMATION>() {
        return Err(NativeMemoryError::MemoryQueryFailed);
    }

    if info.State != MEM_COMMIT {
        return Err(NativeMemoryError::UncommittedRegion);
    }
    if info.Protect & PAGE_GUARD != 0 {
        return Err(NativeMemoryError::GuardedRegion);
    }

    let base_protection = info.Protect & 0xff;
    if base_protection == PAGE_NOACCESS {
        return Err(NativeMemoryError::NoAccessRegion);
    }
    let readable = matches!(
        base_protection,
        PAGE_READONLY
            | PAGE_READWRITE
            | PAGE_WRITECOPY
            | PAGE_EXECUTE_READ
            | PAGE_EXECUTE_READWRITE
            | PAGE_EXECUTE_WRITECOPY
    );
    let writable = matches!(
        base_protection,
        PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    );
    match required_access {
        MemoryAccess::Read if !readable => return Err(NativeMemoryError::UnreadableRegion),
        MemoryAccess::Write if !writable => return Err(NativeMemoryError::UnwritableRegion),
        MemoryAccess::Read | MemoryAccess::Write => {}
    }

    let region_base = info.BaseAddress.addr();
    let region_end = region_base
        .checked_add(info.RegionSize)
        .ok_or(NativeMemoryError::MalformedMemoryRegion)?;
    if info.RegionSize == 0 || address < region_base || address >= region_end {
        return Err(NativeMemoryError::MalformedMemoryRegion);
    }
    Ok(region_end)
}

fn validate_region_range(
    address: usize,
    length: usize,
    required_access: MemoryAccess,
) -> Result<(), NativeMemoryError> {
    let mut checked = 0_usize;
    while checked < length {
        let cursor = address
            .checked_add(checked)
            .ok_or(NativeMemoryError::AddressOverflow)?;
        let end = region_end(cursor, required_access)?;
        let available = end
            .checked_sub(cursor)
            .ok_or(NativeMemoryError::MalformedMemoryRegion)?;
        let step = available.min(length - checked);
        if step == 0 {
            return Err(NativeMemoryError::MalformedMemoryRegion);
        }
        checked = checked
            .checked_add(step)
            .ok_or(NativeMemoryError::AddressOverflow)?;
    }
    Ok(())
}

fn read_chunk(address: usize, destination: &mut [u8]) -> Result<(), NativeMemoryError> {
    if destination.is_empty() {
        return Ok(());
    }

    let mut actual = 0_usize;
    // SAFETY: this retrieves the documented current-process pseudo-handle and
    // creates no owned handle that needs closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: `destination` is initialized, writable Rust-owned memory for the
    // exact requested length. The source is passed only to the Win32 copy API;
    // Rust does not dereference it. A concurrently changed source protection is
    // reported as a failed or partial copy.
    let success = unsafe {
        ReadProcessMemory(
            process,
            without_provenance::<c_void>(address),
            destination.as_mut_ptr().cast(),
            destination.len(),
            &mut actual,
        )
    };
    classify_read_result(success, destination.len(), actual)
}

/// # Safety
///
/// The numeric destination must remain live and valid for `source.len()` bytes,
/// the copied bytes must preserve its object invariants, and conflicting
/// accesses must be prevented for the duration of the call.
unsafe fn write_chunk(address: usize, source: &[u8]) -> Result<(), NativeMemoryError> {
    if source.is_empty() {
        return Ok(());
    }

    let mut actual = 0_usize;
    // SAFETY: this retrieves the documented current-process pseudo-handle and
    // creates no owned handle that needs closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: `source` is readable Rust-owned memory for the exact requested
    // length. The caller guarantees that the numeric destination is valid for
    // this byte write and that the mutation respects its object invariants.
    let success = unsafe {
        WriteProcessMemory(
            process,
            without_provenance::<c_void>(address),
            source.as_ptr().cast(),
            source.len(),
            &mut actual,
        )
    };
    classify_write_result(success, source.len(), actual)
}

fn classify_read_result(
    success: i32,
    expected: usize,
    actual: usize,
) -> Result<(), NativeMemoryError> {
    if actual != expected {
        if actual == 0 && success == 0 {
            Err(NativeMemoryError::ReadFailed)
        } else {
            Err(NativeMemoryError::PartialRead { expected, actual })
        }
    } else if success == 0 {
        Err(NativeMemoryError::ReadFailed)
    } else {
        Ok(())
    }
}

fn classify_write_result(
    success: i32,
    expected: usize,
    actual: usize,
) -> Result<(), NativeMemoryError> {
    if actual != expected {
        if actual == 0 && success == 0 {
            Err(NativeMemoryError::WriteFailed)
        } else {
            Err(NativeMemoryError::PartialWrite { expected, actual })
        }
    } else if success == 0 {
        Err(NativeMemoryError::WriteFailed)
    } else {
        Ok(())
    }
}

fn allocate_zeroed(length: usize) -> Result<Vec<u8>, NativeMemoryError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| NativeMemoryError::AllocationFailed { requested: length })?;
    bytes.resize(length, 0);
    Ok(bytes)
}

fn append_bytes(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), NativeMemoryError> {
    let requested = output
        .len()
        .checked_add(bytes.len())
        .ok_or(NativeMemoryError::AddressOverflow)?;
    output
        .try_reserve_exact(bytes.len())
        .map_err(|_| NativeMemoryError::AllocationFailed { requested })?;
    output.extend_from_slice(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{classify_read_result, classify_write_result};
    use crate::NativeMemoryError;

    #[test]
    fn partial_and_failed_win32_results_fail_closed() {
        assert_eq!(classify_read_result(1, 8, 8), Ok(()));
        assert_eq!(
            classify_read_result(1, 8, 7),
            Err(NativeMemoryError::PartialRead {
                expected: 8,
                actual: 7,
            })
        );
        assert_eq!(
            classify_read_result(0, 8, 3),
            Err(NativeMemoryError::PartialRead {
                expected: 8,
                actual: 3,
            })
        );
        assert_eq!(
            classify_read_result(0, 8, 0),
            Err(NativeMemoryError::ReadFailed)
        );
        assert_eq!(
            classify_read_result(0, 8, 8),
            Err(NativeMemoryError::ReadFailed)
        );

        assert_eq!(classify_write_result(1, 1, 1), Ok(()));
        assert_eq!(
            classify_write_result(1, 1, 0),
            Err(NativeMemoryError::PartialWrite {
                expected: 1,
                actual: 0,
            })
        );
        assert_eq!(
            classify_write_result(0, 1, 0),
            Err(NativeMemoryError::WriteFailed)
        );
        assert_eq!(
            classify_write_result(0, 1, 1),
            Err(NativeMemoryError::WriteFailed)
        );
    }
}
