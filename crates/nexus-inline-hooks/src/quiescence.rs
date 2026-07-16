use std::{ffi::c_void, mem::size_of, num::NonZeroUsize, process, thread};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_NO_MORE_FILES, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    },
    System::{
        Diagnostics::{
            Debug::{CONTEXT, CONTEXT_CONTROL_AMD64, FlushInstructionCache, GetThreadContext},
            ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
        },
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread,
            SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
            WaitForSingleObject,
        },
    },
};

const PATCH_WINDOW_BEFORE: usize = 32;
const PATCH_WINDOW_AFTER: usize = 64;
const QUIESCENCE_ATTEMPTS: usize = 8;
const RESUME_ATTEMPTS: usize = 64;
const API_FAILURE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QuiescenceError {
    OutOfMemory,
    Snapshot,
    Enumeration,
    OpenThread,
    SuspendThread,
    ThreadContext(u32),
    TargetBusy,
}

pub(crate) struct QuiescenceGuard {
    threads: Vec<SuspendedThread>,
}

impl QuiescenceGuard {
    pub(crate) fn acquire(targets: &[NonZeroUsize]) -> Result<Self, QuiescenceError> {
        let mut last_error = QuiescenceError::TargetBusy;

        for _ in 0..QUIESCENCE_ATTEMPTS {
            match Self::try_acquire(targets) {
                Ok(guard) => return Ok(guard),
                Err(error) => last_error = error,
            }
            thread::yield_now();
        }

        Err(last_error)
    }

    fn try_acquire(targets: &[NonZeroUsize]) -> Result<Self, QuiescenceError> {
        let snapshot = SnapshotHandle::create()?;
        let process_id = unsafe {
            // SAFETY: This function has no preconditions and returns the current process id.
            GetCurrentProcessId()
        };
        let current_thread_id = unsafe {
            // SAFETY: This function has no preconditions and returns the current thread id.
            GetCurrentThreadId()
        };
        let mut entry = THREADENTRY32 {
            dwSize: u32::try_from(size_of::<THREADENTRY32>())
                .map_err(|_| QuiescenceError::Enumeration)?,
            ..THREADENTRY32::default()
        };
        let first = unsafe {
            // SAFETY: `snapshot` is valid and `entry` points to a correctly sized structure.
            Thread32First(snapshot.raw(), &mut entry)
        };
        if first == 0 {
            return Err(QuiescenceError::Enumeration);
        }

        let mut threads = Vec::new();
        loop {
            if entry.th32OwnerProcessID == process_id && entry.th32ThreadID != current_thread_id {
                threads
                    .try_reserve(1)
                    .map_err(|_| QuiescenceError::OutOfMemory)?;
                let suspended = SuspendedThread::open_and_suspend(entry.th32ThreadID)?;
                if rip_in_patch_window(suspended.instruction_pointer()?, targets) {
                    return Err(QuiescenceError::TargetBusy);
                }
                threads.push(suspended);
            }

            let next = unsafe {
                // SAFETY: `snapshot` and the correctly sized output structure remain valid.
                Thread32Next(snapshot.raw(), &mut entry)
            };
            if next == 0 {
                let error = unsafe {
                    // SAFETY: Reading the calling thread's last-error value has no preconditions.
                    GetLastError()
                };
                if error != ERROR_NO_MORE_FILES {
                    return Err(QuiescenceError::Enumeration);
                }
                break;
            }
        }

        Ok(Self { threads })
    }
}

impl Drop for QuiescenceGuard {
    fn drop(&mut self) {
        while let Some(thread) = self.threads.pop() {
            drop(thread);
        }
    }
}

pub(crate) fn flush_instruction_cache(target: NonZeroUsize) {
    let start = target.get().saturating_sub(PATCH_WINDOW_BEFORE);
    let size = PATCH_WINDOW_BEFORE + PATCH_WINDOW_AFTER;
    let process = unsafe {
        // SAFETY: This function has no preconditions and returns a process pseudo-handle.
        GetCurrentProcess()
    };
    let _ = unsafe {
        // SAFETY: The range is a conservative executable-code window in this process. Windows
        // accepts a pseudo-handle and does not dereference the base address in the caller.
        FlushInstructionCache(process, start as *const c_void, size)
    };
}

fn rip_in_patch_window(rip: usize, targets: &[NonZeroUsize]) -> bool {
    targets.iter().any(|target| {
        let start = target.get().saturating_sub(PATCH_WINDOW_BEFORE);
        let end = target.get().saturating_add(PATCH_WINDOW_AFTER);
        (start..end).contains(&rip)
    })
}

struct SnapshotHandle(OwnedHandle);

impl SnapshotHandle {
    fn create() -> Result<Self, QuiescenceError> {
        let raw = unsafe {
            // SAFETY: TH32CS_SNAPTHREAD ignores the process-id argument.
            CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)
        };
        if raw == INVALID_HANDLE_VALUE {
            Err(QuiescenceError::Snapshot)
        } else {
            Ok(Self(OwnedHandle(raw)))
        }
    }

    fn raw(&self) -> HANDLE {
        self.0.raw()
    }
}

struct SuspendedThread {
    handle: OwnedHandle,
    needs_resume: bool,
}

impl SuspendedThread {
    fn open_and_suspend(thread_id: u32) -> Result<Self, QuiescenceError> {
        let access = THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION;
        let raw = unsafe {
            // SAFETY: The access mask is valid, handles are not inherited, and the id came from
            // the process-wide thread snapshot.
            OpenThread(access, 0, thread_id)
        };
        if raw.is_null() {
            return Err(QuiescenceError::OpenThread);
        }
        let handle = OwnedHandle(raw);
        let previous_count = unsafe {
            // SAFETY: `handle` is an open thread handle with suspend permission.
            SuspendThread(handle.raw())
        };
        if previous_count == API_FAILURE {
            return Err(QuiescenceError::SuspendThread);
        }

        Ok(Self {
            handle,
            needs_resume: true,
        })
    }

    fn instruction_pointer(&self) -> Result<usize, QuiescenceError> {
        let mut context = AlignedContext(CONTEXT {
            ContextFlags: CONTEXT_CONTROL_AMD64,
            ..CONTEXT::default()
        });
        let succeeded = unsafe {
            // SAFETY: The thread is suspended, the handle has context permission, and `context`
            // is a valid writable x64 CONTEXT structure with its flags initialized.
            GetThreadContext(self.handle.raw(), &mut context.0)
        };
        if succeeded == 0 {
            let error = unsafe {
                // SAFETY: Reading the calling thread's last-error value has no preconditions.
                GetLastError()
            };
            Err(QuiescenceError::ThreadContext(error))
        } else {
            usize::try_from(context.0.Rip).map_err(|_| QuiescenceError::ThreadContext(0))
        }
    }

    fn resume_or_abort(&mut self) {
        if !self.needs_resume {
            return;
        }

        for _ in 0..RESUME_ATTEMPTS {
            let previous_count = unsafe {
                // SAFETY: This valid handle owns exactly one suspension added by this guard.
                ResumeThread(self.handle.raw())
            };
            if previous_count != API_FAILURE {
                self.needs_resume = false;
                return;
            }

            let wait = unsafe {
                // SAFETY: Waiting with a zero timeout on an open thread handle is non-blocking.
                WaitForSingleObject(self.handle.raw(), 0)
            };
            if wait == WAIT_OBJECT_0 {
                self.needs_resume = false;
                return;
            }
            thread::yield_now();
        }

        // Continuing with a live thread still suspended by us is less safe than terminating the
        // process. Process teardown also closes every handle and retires every thread.
        process::abort();
    }
}

// Windows requires an x64 CONTEXT buffer to be 16-byte aligned, but the generated
// `windows-sys` structure deliberately mirrors C layout without strengthening alignment.
#[repr(C, align(16))]
struct AlignedContext(CONTEXT);

impl Drop for SuspendedThread {
    fn drop(&mut self) {
        self.resume_or_abort();
    }
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let closed = unsafe {
            // SAFETY: This wrapper is the sole owner of a valid closeable Win32 handle.
            CloseHandle(self.0)
        };
        if closed == 0 {
            process::abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, panic_any};

    use super::*;

    #[test]
    fn patch_window_is_bounded_and_saturating() {
        let target = NonZeroUsize::new(16).unwrap_or(NonZeroUsize::MIN);
        assert!(rip_in_patch_window(0, &[target]));
        assert!(rip_in_patch_window(79, &[target]));
        assert!(!rip_in_patch_window(80, &[target]));
    }

    #[test]
    fn can_quiesce_peer_threads() {
        drop(test_guard());
    }

    #[test]
    fn x64_context_buffer_has_required_alignment() {
        assert!(std::mem::align_of::<AlignedContext>() >= 16);
    }

    #[test]
    fn unwind_resumes_every_suspended_peer() {
        let unwind = catch_unwind(|| {
            let _guard = test_guard();
            panic_any(());
        });
        assert!(unwind.is_err());

        // A second complete transaction proves no peer remained suspended by the first guard.
        drop(test_guard());
    }

    fn test_guard() -> QuiescenceGuard {
        match QuiescenceGuard::acquire(&[NonZeroUsize::MIN]) {
            Ok(guard) => guard,
            Err(error) => panic!("quiescence stage failed: {error:?}"),
        }
    }
}
