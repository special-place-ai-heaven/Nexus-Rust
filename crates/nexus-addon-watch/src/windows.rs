use crate::coalesce::Coalescer;
use crate::{ChangeKinds, ChangeSignal, WatchConfig, WatchError};
use std::ffi::c_void;
use std::fmt;
use std::os::windows::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, FALSE, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, TRUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects,
};

const NATIVE_BUFFER_BYTES: usize = 64 * 1024;
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(250);
const CALLBACK_IDLE: u8 = 0;
const CALLBACK_ACTIVE: u8 = 1;
const STOPPING: u8 = 2;
const CALLBACK_STOPPING: u8 = 3;

/// Closed watcher shutdown statistics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WatchReport {
    callback_panics: u64,
}

impl WatchReport {
    /// Returns the number of consumer callback panics that were contained.
    #[must_use]
    pub const fn callback_panics(self) -> u64 {
        self.callback_panics
    }
}

/// A non-recursive watcher for one addon directory.
///
/// The watcher owns a dedicated worker. Stopping or dropping it requests
/// cancellation and joins that worker before returning.
pub struct AddonDirectoryWatcher {
    stop_event: Arc<KernelHandle>,
    callback_gate: Arc<CallbackGate>,
    worker: Option<JoinHandle<Result<WatchReport, WatchError>>>,
    outcome: Option<Result<WatchReport, WatchError>>,
}

impl AddonDirectoryWatcher {
    /// Starts watching an addon directory.
    ///
    /// The callback runs on the dedicated watcher worker, never while an
    /// internal lock is held. Callback panics are contained and counted.
    pub fn start<P, F>(directory: P, config: WatchConfig, callback: F) -> Result<Self, WatchError>
    where
        P: AsRef<Path>,
        F: FnMut(ChangeSignal) + Send + 'static,
    {
        let directory = OwnedWidePath::new(directory.as_ref())?;
        let stop_event = Arc::new(KernelHandle::new_event(
            WatchError::StopEventCreationFailed,
        )?);
        let callback_gate = Arc::new(CallbackGate::new());
        let (ready_sender, ready_receiver) = sync_channel(1);
        let worker_stop_event = Arc::clone(&stop_event);
        let worker_callback_gate = Arc::clone(&callback_gate);

        let worker = thread::Builder::new()
            .name(String::from("nexus-addon-watch"))
            .spawn(move || {
                contain_panic(|| {
                    run_worker(
                        directory,
                        config,
                        worker_stop_event,
                        worker_callback_gate,
                        ready_sender,
                        callback,
                    )
                })
                .unwrap_or(Err(WatchError::WorkerPanicked))
            })
            .map_err(|_| WatchError::WorkerStartFailed)?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                stop_event,
                callback_gate,
                worker: Some(worker),
                outcome: None,
            }),
            Ok(Err(error)) => {
                callback_gate.request_stop();
                set_event(&stop_event);
                let _ = join_worker(worker);
                Err(error)
            }
            Err(_) => {
                callback_gate.request_stop();
                set_event(&stop_event);
                match join_worker(worker) {
                    Err(error) => Err(error),
                    Ok(_) => Err(WatchError::WorkerStartupFailed),
                }
            }
        }
    }

    /// Returns whether the worker is currently active.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.outcome.is_none()
            && self
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
    }

    /// Stops the watcher and joins its worker.
    ///
    /// A callback already admitted before this call may finish while this call
    /// waits. No callback is admitted after shutdown begins, and none can run
    /// after this method returns.
    pub fn stop(&mut self) -> Result<WatchReport, WatchError> {
        if let Some(outcome) = self.outcome {
            return outcome;
        }

        self.callback_gate.request_stop();
        let stop_signalled = set_event(&self.stop_event);
        let outcome = self
            .worker
            .take()
            .map_or(Err(WatchError::WorkerStartupFailed), join_worker);
        let outcome = if stop_signalled || outcome.is_err() {
            outcome
        } else {
            Err(WatchError::StopSignalFailed)
        };
        self.outcome = Some(outcome);
        outcome
    }
}

impl fmt::Debug for AddonDirectoryWatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AddonDirectoryWatcher")
            .field("running", &self.is_running())
            .finish()
    }
}

impl Drop for AddonDirectoryWatcher {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn join_worker(
    worker: JoinHandle<Result<WatchReport, WatchError>>,
) -> Result<WatchReport, WatchError> {
    match worker.join() {
        Ok(outcome) => outcome,
        Err(payload) => {
            // A custom panic payload may panic again from Drop. The watcher is
            // an FFI-adjacent shutdown boundary, so do not reopen unwinding.
            std::mem::forget(payload);
            Err(WatchError::WorkerPanicked)
        }
    }
}

fn contain_panic<T>(operation: impl FnOnce() -> T) -> Result<T, ()> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(value) => Ok(value),
        Err(payload) => {
            // Forget adversarial payloads whose destructors could panic and
            // escape the boundary a second time.
            std::mem::forget(payload);
            Err(())
        }
    }
}

struct DirectoryRead {
    directory: KernelHandle,
    io_event: KernelHandle,
    buffer: Vec<u8>,
    overlapped: Box<OVERLAPPED>,
    pending: bool,
}

impl DirectoryRead {
    fn new(path: &OwnedWidePath) -> Result<Self, WatchError> {
        let directory = KernelHandle::open_directory(path)?;
        let io_event = KernelHandle::new_event(WatchError::IoEventCreationFailed)?;
        let mut read = Self {
            directory,
            io_event,
            buffer: vec![0_u8; NATIVE_BUFFER_BYTES],
            overlapped: Box::new(OVERLAPPED::default()),
            pending: false,
        };
        read.begin()?;
        Ok(read)
    }

    fn begin(&mut self) -> Result<(), WatchError> {
        if self.pending {
            return Err(WatchError::ReadStartFailed);
        }

        // SAFETY: io_event is a valid event handle owned for the worker lifetime.
        if unsafe { ResetEvent(self.io_event.raw()) } == FALSE {
            return Err(WatchError::IoEventResetFailed);
        }

        *self.overlapped = OVERLAPPED::default();
        self.overlapped.hEvent = self.io_event.raw();
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_ATTRIBUTES
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE
            | FILE_NOTIFY_CHANGE_CREATION;
        // SAFETY: the directory was opened for overlapped directory reads. The
        // fixed buffer and boxed OVERLAPPED remain alive and unmoved while
        // pending, including during unwinding through this type's Drop.
        let started = unsafe {
            ReadDirectoryChangesW(
                self.directory.raw(),
                self.buffer.as_mut_ptr().cast::<c_void>(),
                u32::try_from(self.buffer.len()).unwrap_or(u32::MAX),
                FALSE,
                filter,
                null_mut(),
                self.overlapped.as_mut(),
                None,
            )
        };
        if started != FALSE {
            self.pending = true;
            return Ok(());
        }

        // SAFETY: GetLastError is read immediately after the failed native call.
        let error = unsafe { GetLastError() };
        if error == ERROR_IO_PENDING {
            self.pending = true;
            Ok(())
        } else {
            Err(WatchError::ReadStartFailed)
        }
    }

    fn complete(&mut self) -> Result<u32, WatchError> {
        if !self.pending {
            return Err(WatchError::ReadCompletionFailed);
        }

        let mut transferred = 0;
        // SAFETY: the event signalled completion for this exact live OVERLAPPED.
        let completed = unsafe {
            GetOverlappedResult(
                self.directory.raw(),
                self.overlapped.as_ref(),
                &mut transferred,
                FALSE,
            )
        };
        self.pending = false;
        if completed == FALSE {
            Err(WatchError::ReadCompletionFailed)
        } else {
            Ok(transferred)
        }
    }

    fn cancel(&mut self) -> Result<(), WatchError> {
        if !self.pending {
            return Ok(());
        }

        // SAFETY: directory and overlapped identify the live request issued by
        // this worker. Cancellation is followed by a blocking completion drain.
        let cancelled = unsafe { CancelIoEx(self.directory.raw(), self.overlapped.as_ref()) };
        let cancellation_failed = if cancelled == FALSE {
            // SAFETY: GetLastError is read immediately after CancelIoEx.
            (unsafe { GetLastError() }) != ERROR_NOT_FOUND
        } else {
            false
        };

        let mut transferred = 0;
        // SAFETY: waiting here keeps the request's buffer and OVERLAPPED alive
        // until the kernel has stopped accessing them.
        let completed = unsafe {
            GetOverlappedResult(
                self.directory.raw(),
                self.overlapped.as_ref(),
                &mut transferred,
                TRUE,
            )
        };
        let completion_error = if completed == FALSE {
            // SAFETY: GetLastError is read immediately after GetOverlappedResult.
            Some(unsafe { GetLastError() })
        } else {
            None
        };
        self.pending = false;

        if cancellation_failed {
            return Err(WatchError::ReadCancellationFailed);
        }
        if completion_error.is_some_and(|error| error != ERROR_OPERATION_ABORTED) {
            return Err(WatchError::ReadCompletionFailed);
        }
        Ok(())
    }

    fn event_handle(&self) -> HANDLE {
        self.io_event.raw()
    }

    fn buffer(&self) -> &[u8] {
        &self.buffer
    }
}

impl Drop for DirectoryRead {
    fn drop(&mut self) {
        let _ = self.cancel();
    }
}

fn run_worker<F>(
    directory_path: OwnedWidePath,
    config: WatchConfig,
    stop_event: Arc<KernelHandle>,
    callback_gate: Arc<CallbackGate>,
    ready: SyncSender<Result<(), WatchError>>,
    mut callback: F,
) -> Result<WatchReport, WatchError>
where
    F: FnMut(ChangeSignal),
{
    let mut directory_read = match DirectoryRead::new(&directory_path) {
        Ok(read) => read,
        Err(error) => return startup_failure(&ready, error),
    };
    if ready.send(Ok(())).is_err() {
        callback_gate.request_stop();
        return directory_read
            .cancel()
            .and(Err(WatchError::WorkerStartupFailed));
    }

    let started = Instant::now();
    let mut coalescer = Coalescer::new(config);
    let mut report = WatchReport::default();
    let handles = [stop_event.raw(), directory_read.event_handle()];

    loop {
        if callback_gate.is_stopping() {
            coalescer.clear();
            directory_read.cancel()?;
            return Ok(report);
        }

        let timeout = wait_timeout(&coalescer, started.elapsed());
        // SAFETY: both handles remain owned for the call, the array has exactly
        // the supplied length, and no thread closes either handle while waiting.
        let wait_result = unsafe {
            WaitForMultipleObjects(
                u32::try_from(handles.len()).unwrap_or(2),
                handles.as_ptr(),
                FALSE,
                timeout,
            )
        };

        match wait_result {
            WAIT_OBJECT_0 => {
                coalescer.clear();
                directory_read.cancel()?;
                return Ok(report);
            }
            value if value == WAIT_OBJECT_0 + 1 => {
                if callback_gate.is_stopping() {
                    coalescer.clear();
                    directory_read.cancel()?;
                    return Ok(report);
                }

                let bytes = directory_read.complete()?;
                let kinds = parse_change_kinds(directory_read.buffer(), bytes);
                coalescer.record(started.elapsed(), kinds);

                if callback_gate.is_stopping() {
                    coalescer.clear();
                    return Ok(report);
                }
                directory_read.begin()?;
            }
            WAIT_TIMEOUT => {
                if callback_gate.is_stopping() {
                    coalescer.clear();
                    directory_read.cancel()?;
                    return Ok(report);
                }
                if let Some(signal) = coalescer.take_if_due(started.elapsed()) {
                    dispatch_callback(
                        &callback_gate,
                        &mut callback,
                        signal,
                        &mut report.callback_panics,
                    );
                }
            }
            WAIT_FAILED => {
                directory_read.cancel()?;
                return Err(WatchError::WaitFailed);
            }
            _ => {
                directory_read.cancel()?;
                return Err(WatchError::WaitFailed);
            }
        }
    }
}

fn startup_failure(
    ready: &SyncSender<Result<(), WatchError>>,
    error: WatchError,
) -> Result<WatchReport, WatchError> {
    let _ = ready.send(Err(error));
    Err(error)
}

fn wait_timeout(coalescer: &Coalescer, now: Duration) -> u32 {
    let until_delivery = coalescer
        .deadline()
        .map_or(STOP_POLL_INTERVAL, |deadline| deadline.saturating_sub(now));
    duration_millis_ceil(until_delivery.min(STOP_POLL_INTERVAL))
}

fn duration_millis_ceil(duration: Duration) -> u32 {
    let whole_millis = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX - 1);
    if duration.subsec_nanos().is_multiple_of(1_000_000) {
        whole_millis
    } else {
        whole_millis.saturating_add(1)
    }
}

fn dispatch_callback<F>(
    gate: &CallbackGate,
    callback: &mut F,
    signal: ChangeSignal,
    callback_panics: &mut u64,
) where
    F: FnMut(ChangeSignal),
{
    if !gate.try_begin_callback() {
        return;
    }

    let result = contain_panic(|| callback(signal));
    gate.finish_callback();
    if result.is_err() {
        *callback_panics = callback_panics.saturating_add(1);
    }
}

fn parse_change_kinds(buffer: &[u8], transferred: u32) -> ChangeKinds {
    let Ok(length) = usize::try_from(transferred) else {
        return ChangeKinds::RESCAN;
    };
    if length == 0 || length > buffer.len() {
        return ChangeKinds::RESCAN;
    }

    let mut kinds = ChangeKinds::default();
    let mut offset = 0;
    while offset < length {
        let Some(next_offset) = read_u32(buffer, offset) else {
            return kinds | ChangeKinds::RESCAN;
        };
        let Some(action) = read_u32(buffer, offset + 4) else {
            return kinds | ChangeKinds::RESCAN;
        };
        let Some(file_name_bytes) = read_u32(buffer, offset + 8) else {
            return kinds | ChangeKinds::RESCAN;
        };

        let record_length = if next_offset == 0 {
            length - offset
        } else {
            let Ok(next_offset) = usize::try_from(next_offset) else {
                return kinds | ChangeKinds::RESCAN;
            };
            let Some(next_record) = offset.checked_add(next_offset) else {
                return kinds | ChangeKinds::RESCAN;
            };
            if next_offset < 12 || next_offset % 4 != 0 || next_record > length {
                return kinds | ChangeKinds::RESCAN;
            }
            next_offset
        };
        let Ok(file_name_bytes) = usize::try_from(file_name_bytes) else {
            return kinds | ChangeKinds::RESCAN;
        };
        if file_name_bytes % 2 != 0
            || 12_usize
                .checked_add(file_name_bytes)
                .is_none_or(|used| used > record_length)
        {
            return kinds | ChangeKinds::RESCAN;
        }

        kinds |= match action {
            FILE_ACTION_ADDED => ChangeKinds::CREATED,
            FILE_ACTION_REMOVED => ChangeKinds::DELETED,
            FILE_ACTION_MODIFIED => ChangeKinds::WRITTEN,
            FILE_ACTION_RENAMED_OLD_NAME | FILE_ACTION_RENAMED_NEW_NAME => ChangeKinds::RENAMED,
            _ => ChangeKinds::RESCAN,
        };

        if next_offset == 0 {
            break;
        }
        let Ok(next_offset) = usize::try_from(next_offset) else {
            return kinds | ChangeKinds::RESCAN;
        };
        offset += next_offset;
    }

    if kinds.is_empty() {
        ChangeKinds::RESCAN
    } else {
        kinds
    }
}

fn read_u32(buffer: &[u8], offset: usize) -> Option<u32> {
    let bytes = buffer.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

struct OwnedWidePath(Vec<u16>);

impl OwnedWidePath {
    fn new(path: &Path) -> Result<Self, WatchError> {
        let mut units = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if units.contains(&0) {
            return Err(WatchError::InvalidPath);
        }
        units.push(0);
        Ok(Self(units))
    }

    fn as_ptr(&self) -> *const u16 {
        self.0.as_ptr()
    }
}

struct KernelHandle(usize);

impl KernelHandle {
    fn new_event(error: WatchError) -> Result<Self, WatchError> {
        // SAFETY: null security and name pointers request an unnamed event with
        // default security. The returned handle is immediately owned.
        let handle = unsafe { CreateEventW(null(), TRUE, FALSE, null()) };
        if handle.is_null() {
            Err(error)
        } else {
            Ok(Self(handle as usize))
        }
    }

    fn open_directory(path: &OwnedWidePath) -> Result<Self, WatchError> {
        // SAFETY: path owns a nul-terminated UTF-16 buffer for the duration of
        // the call. The returned handle is either invalid or immediately owned.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(WatchError::DirectoryOpenFailed)
        } else {
            Ok(Self(handle as usize))
        }
    }

    fn raw(&self) -> HANDLE {
        self.0 as HANDLE
    }
}

impl Drop for KernelHandle {
    fn drop(&mut self) {
        // SAFETY: each non-null, non-invalid handle is closed exactly once by
        // its sole KernelHandle owner.
        let _ = unsafe { CloseHandle(self.raw()) };
    }
}

struct CallbackGate {
    state: AtomicU8,
}

impl CallbackGate {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(CALLBACK_IDLE),
        }
    }

    fn try_begin_callback(&self) -> bool {
        self.state
            .compare_exchange(
                CALLBACK_IDLE,
                CALLBACK_ACTIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn finish_callback(&self) {
        match self.state.compare_exchange(
            CALLBACK_ACTIVE,
            CALLBACK_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(CALLBACK_STOPPING) => self.state.store(STOPPING, Ordering::Release),
            Err(_) => self.state.store(STOPPING, Ordering::Release),
        }
    }

    fn request_stop(&self) {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            let next = match state {
                CALLBACK_IDLE => STOPPING,
                CALLBACK_ACTIVE => CALLBACK_STOPPING,
                STOPPING | CALLBACK_STOPPING => return,
                _ => STOPPING,
            };
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(observed) => state = observed,
            }
        }
    }

    fn is_stopping(&self) -> bool {
        matches!(
            self.state.load(Ordering::Acquire),
            STOPPING | CALLBACK_STOPPING
        )
    }
}

fn set_event(event: &KernelHandle) -> bool {
    // SAFETY: event is a valid manual-reset event owned for this call.
    (unsafe { SetEvent(event.raw()) }) != FALSE
}

#[cfg(test)]
mod tests {
    use super::{
        AddonDirectoryWatcher, CALLBACK_STOPPING, CallbackGate, ChangeKinds, ChangeSignal,
        STOPPING, WatchConfig, dispatch_callback, parse_change_kinds,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn native_actions_map_to_finite_change_categories() {
        let actions = [1_u32, 3, 4, 5, 2];
        let mut buffer = Vec::new();
        for (index, action) in actions.into_iter().enumerate() {
            let next = if index + 1 == actions.len() { 0 } else { 12 };
            buffer.extend_from_slice(&u32::try_from(next).unwrap_or(0).to_le_bytes());
            buffer.extend_from_slice(&action.to_le_bytes());
            buffer.extend_from_slice(&0_u32.to_le_bytes());
        }

        let kinds = parse_change_kinds(&buffer, u32::try_from(buffer.len()).unwrap_or(0));
        assert!(kinds.contains(ChangeKinds::CREATED));
        assert!(kinds.contains(ChangeKinds::WRITTEN));
        assert!(kinds.contains(ChangeKinds::RENAMED));
        assert!(kinds.contains(ChangeKinds::DELETED));
        assert!(!kinds.contains(ChangeKinds::RESCAN));
    }

    #[test]
    fn empty_or_malformed_native_data_requests_a_rescan() {
        assert_eq!(parse_change_kinds(&[], 0), ChangeKinds::RESCAN);

        let mut malformed = Vec::new();
        malformed.extend_from_slice(&16_u32.to_le_bytes());
        malformed.extend_from_slice(&1_u32.to_le_bytes());
        malformed.extend_from_slice(&0_u32.to_le_bytes());
        assert!(parse_change_kinds(&malformed, 12).contains(ChangeKinds::RESCAN));
    }

    #[test]
    fn callback_panics_are_contained_and_later_callbacks_continue() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let callback_attempts = Arc::clone(&attempts);
        let mut callback = move |_signal: ChangeSignal| {
            if callback_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("contained callback panic");
            }
        };
        let gate = CallbackGate::new();
        let signal = ChangeSignal::new(ChangeKinds::WRITTEN);
        let mut callback_panics = 0;

        dispatch_callback(&gate, &mut callback, signal, &mut callback_panics);
        dispatch_callback(&gate, &mut callback, signal, &mut callback_panics);

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(callback_panics, 1);
    }

    #[test]
    fn panic_payload_destructors_cannot_reopen_callback_unwinding() {
        struct PanicOnDrop;

        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                panic!("panic payload destructor must not run");
            }
        }

        let mut callback = |_signal: ChangeSignal| std::panic::panic_any(PanicOnDrop);
        let gate = CallbackGate::new();
        let mut callback_panics = 0;

        dispatch_callback(
            &gate,
            &mut callback,
            ChangeSignal::new(ChangeKinds::WRITTEN),
            &mut callback_panics,
        );

        assert_eq!(callback_panics, 1);
        assert!(!gate.is_stopping());
    }

    #[test]
    fn shutdown_prevents_new_callback_admission() {
        let gate = CallbackGate::new();
        assert!(gate.try_begin_callback());
        gate.request_stop();
        assert_eq!(gate.state.load(Ordering::Acquire), CALLBACK_STOPPING);
        gate.finish_callback();
        assert_eq!(gate.state.load(Ordering::Acquire), STOPPING);
        assert!(!gate.try_begin_callback());
    }

    #[test]
    fn real_directory_changes_are_coalesced_and_stop_is_final() {
        let directory = match TempDirectory::new() {
            Ok(directory) => directory,
            Err(_) => panic!("temporary watcher directory could not be created"),
        };
        let (sender, receiver) = mpsc::channel();
        let config = match WatchConfig::new(Duration::from_millis(50), Duration::from_millis(500)) {
            Ok(config) => config,
            Err(_) => panic!("smoke-test watcher configuration is valid"),
        };
        let mut watcher =
            match AddonDirectoryWatcher::start(directory.path(), config, move |signal| {
                let _ = sender.send(signal);
            }) {
                Ok(watcher) => watcher,
                Err(_) => panic!("watcher could not start"),
            };

        let original = directory.path().join("probe.dll");
        let renamed = directory.path().join("probe-renamed.dll");
        assert!(
            fs::write(&original, b"one").is_ok(),
            "test file write failed"
        );
        assert!(
            fs::write(&original, b"two").is_ok(),
            "test file rewrite failed"
        );
        assert!(
            fs::rename(&original, &renamed).is_ok(),
            "test file rename failed"
        );
        assert!(
            fs::remove_file(&renamed).is_ok(),
            "test file removal failed"
        );

        let signal = match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(signal) => signal,
            Err(_) => panic!("watcher did not emit a coalesced signal"),
        };
        assert!(!signal.kinds().is_empty());
        assert!(watcher.stop().is_ok(), "watcher did not stop cleanly");

        assert!(
            fs::write(directory.path().join("after-stop.dll"), b"ignored").is_ok(),
            "post-stop test file write failed"
        );
        assert!(
            receiver.recv_timeout(Duration::from_millis(200)).is_err(),
            "a callback ran after stop returned"
        );
    }

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> std::io::Result<Self> {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "nexus-addon-watch-{}-{timestamp}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
