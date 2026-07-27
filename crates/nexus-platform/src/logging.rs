use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

/// Legacy-compatible log severity and filter values.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u32)]
pub enum LogLevel {
    /// Disable all messages for a sink.
    Off = 0,
    /// Critical failure.
    Critical = 1,
    /// Warning.
    Warning = 2,
    /// Informational message.
    Info = 3,
    /// Debug message.
    Debug = 4,
    /// Fine-grained trace message.
    Trace = 5,
    /// Enable every message for a sink.
    All = 6,
}

impl LogLevel {
    /// Whether a sink at this level accepts `message`.
    ///
    /// A plain numeric comparison, matching the reference's
    /// `if (msg->Level <= aLogger->GetLogLevel())` (`src/Core/Logging/LogApi.cpp:40`).
    /// It deliberately does **not** require `message.is_message()`: an add-on may pass
    /// `Off` or `All` as a severity, and the reference emits such a record labelled
    /// `(null)` rather than discarding it. `Off` is numerically lowest so it passes every
    /// sink; `All` is highest so only a sink set to `All` accepts it.
    const fn allows(self, message: Self) -> bool {
        (message as u32) <= (self as u32)
    }

    const fn legacy_label(self) -> &'static str {
        match self {
            Self::Critical => "[CRITICAL]",
            Self::Warning => "[WARNING]",
            Self::Info => "[INFO]",
            Self::Debug => "[DEBUG]",
            Self::Trace => "[TRACE]",
            Self::Off | Self::All => "(null)",
        }
    }
}

/// Calendar fields captured when a log record is created.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogTimestamp {
    /// Four-digit local year.
    pub year: u16,
    /// One-based local month.
    pub month: u8,
    /// One-based local day.
    pub day: u8,
    /// Local hour.
    pub hour: u8,
    /// Local minute.
    pub minute: u8,
    /// Local second.
    pub second: u8,
    /// Milliseconds within the second.
    pub millisecond: u16,
    /// Whole seconds since the Unix epoch for legacy consumers.
    pub unix_seconds: i64,
}

/// Injectable source of log timestamps.
pub trait LogClock: Send + Sync + 'static {
    /// Captures the current timestamp without the registry state lock held.
    ///
    /// Implementations may inspect the registry, but must not recursively call
    /// [`LogRegistry::log`] on the same registry. Synchronous record ordering
    /// requires timestamp capture to retain the non-reentrant sequence slot.
    fn now(&self) -> LogTimestamp;
}

/// Operating-system-backed local log clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemLogClock;

impl LogClock for SystemLogClock {
    fn now(&self) -> LogTimestamp {
        system_timestamp()
    }
}

/// One owned log record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogRecord {
    level: LogLevel,
    timestamp: LogTimestamp,
    channel: Arc<str>,
    message: Arc<str>,
    repeat_count: u32,
}

impl LogRecord {
    /// Returns the message severity.
    #[must_use]
    pub const fn level(&self) -> LogLevel {
        self.level
    }

    /// Returns the captured timestamp.
    #[must_use]
    pub const fn timestamp(&self) -> LogTimestamp {
        self.timestamp
    }

    /// Returns the channel.
    #[must_use]
    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// Returns the unformatted message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the legacy consecutive-repeat count.
    #[must_use]
    pub const fn repeat_count(&self) -> u32 {
        self.repeat_count
    }
}

/// A destination owned by a [`LogRegistry`].
pub trait LogSink: Send + Sync + 'static {
    /// Returns the most verbose message accepted by the sink.
    fn max_level(&self) -> LogLevel;

    /// Processes one owned record snapshot.
    ///
    /// # Errors
    ///
    /// Returns a closed sink error. Implementations must not put record text in
    /// error values.
    fn write(&self, record: &LogRecord) -> Result<(), LogSinkError>;

    /// Flushes buffered output.
    ///
    /// # Errors
    ///
    /// Returns a closed sink error.
    fn flush(&self) -> Result<(), LogSinkError> {
        Ok(())
    }
}

/// A stable process-local registration identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RegistrationId(u64);

impl RegistrationId {
    /// Returns the numeric registration identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Results from a contained sink dispatch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DispatchReport {
    /// Number of sink calls that succeeded.
    pub delivered: usize,
    /// Number of sink calls that returned an error.
    pub failed: usize,
    /// Number of sink panics that were contained.
    pub panicked: usize,
}

impl DispatchReport {
    fn add(&mut self, other: Self) {
        self.delivered += other.delivered;
        self.failed += other.failed;
        self.panicked += other.panicked;
    }
}

/// An owned, replaying logging registry for core and addon messages.
pub struct LogRegistry {
    state: Mutex<RegistryState>,
    record_sequence: Mutex<()>,
    clock: Arc<dyn LogClock>,
}

impl std::fmt::Debug for LogRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock_unpoison(&self.state);
        formatter
            .debug_struct("LogRegistry")
            .field("history_len", &state.history.len())
            .field("sink_count", &state.sinks.len())
            .finish()
    }
}

struct RegistryState {
    next_registration: u64,
    history: Vec<Arc<LogRecord>>,
    sinks: BTreeMap<RegistrationId, Arc<SinkSlot>>,
}

struct SinkSlot {
    sink: Arc<dyn LogSink>,
    dispatch: Mutex<()>,
}

impl Default for LogRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl LogRegistry {
    /// Creates an empty registry using the system local clock.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemLogClock))
    }

    /// Creates an empty registry with an injectable clock.
    #[must_use]
    pub fn with_clock(clock: Arc<dyn LogClock>) -> Self {
        Self {
            state: Mutex::new(RegistryState {
                next_registration: 1,
                history: Vec::new(),
                sinks: BTreeMap::new(),
            }),
            record_sequence: Mutex::new(()),
            clock,
        }
    }

    /// Registers an owned sink and replays compatible history before any newer
    /// message can reach that sink.
    pub fn register(&self, sink: Arc<dyn LogSink>) -> (RegistrationId, DispatchReport) {
        let slot = Arc::new(SinkSlot {
            sink,
            dispatch: Mutex::new(()),
        });
        // Declare this before the guard so unwinding releases the dispatch
        // mutex before any displaced sink destructor can reenter the registry.
        let displaced;
        let dispatch_guard = lock_unpoison(&slot.dispatch);
        let (id, history) = {
            let mut state = lock_unpoison(&self.state);
            let id = RegistrationId(state.next_registration);
            state.next_registration = state.next_registration.saturating_add(1);
            let history = state.history.clone();
            displaced = state.sinks.insert(id, Arc::clone(&slot));
            (id, history)
        };

        let mut report = DispatchReport::default();
        for record in history {
            match sink_allows(&*slot.sink, record.level) {
                Ok(true) => report.add(dispatch_one(&*slot.sink, &record)),
                Ok(false) => {}
                Err(()) => report.panicked += 1,
            }
        }
        drop(dispatch_guard);
        drop(displaced);
        (id, report)
    }

    /// Deregisters a sink. Existing external `Arc` owners remain valid.
    #[must_use]
    pub fn deregister(&self, id: RegistrationId) -> bool {
        let removed = {
            let mut state = lock_unpoison(&self.state);
            state.sinks.remove(&id)
        };
        let was_registered = removed.is_some();
        drop(removed);
        was_registered
    }

    /// Logs one unformatted message and returns contained dispatch results.
    ///
    /// Consecutive records coalesce by message and level, intentionally
    /// preserving the legacy behavior that does not include channel in that
    /// comparison.
    pub fn log(
        &self,
        level: LogLevel,
        channel: impl Into<Arc<str>>,
        message: impl Into<Arc<str>>,
    ) -> DispatchReport {
        let channel = channel.into();
        let message = message.into();
        let record_sequence_guard = lock_unpoison(&self.record_sequence);
        let (record, sinks) = {
            let mut state = lock_unpoison(&self.state);
            let is_repeat = state
                .history
                .last()
                .is_some_and(|last| last.level == level && last.message == message);
            let record = if is_repeat {
                let last_index = state.history.len() - 1;
                let mut updated = LogRecord::clone(&state.history[last_index]);
                updated.repeat_count = updated.repeat_count.saturating_add(1);
                let updated = Arc::new(updated);
                state.history[last_index] = Arc::clone(&updated);
                updated
            } else {
                // Keep history mutation serialized while allowing state-facing
                // clock implementations to reenter the registry safely.
                drop(state);
                let timestamp = self.clock.now();
                state = lock_unpoison(&self.state);
                let record = Arc::new(LogRecord {
                    level,
                    timestamp,
                    channel,
                    message,
                    repeat_count: 1,
                });
                state.history.push(Arc::clone(&record));
                record
            };
            let sinks = state.sinks.values().cloned().collect::<Vec<_>>();
            (record, sinks)
        };
        drop(record_sequence_guard);

        let mut report = DispatchReport::default();
        for slot in sinks {
            match sink_allows(&*slot.sink, record.level) {
                Ok(true) => {
                    let _guard = lock_unpoison(&slot.dispatch);
                    report.add(dispatch_one(&*slot.sink, &record));
                }
                Ok(false) => {}
                Err(()) => report.panicked += 1,
            }
        }
        report
    }

    /// Logs through the legacy addon channel.
    pub fn log_addon(&self, level: LogLevel, message: impl Into<Arc<str>>) -> DispatchReport {
        self.log(level, Arc::<str>::from("Addon"), message)
    }

    /// Returns an owned snapshot of coalesced history.
    #[must_use]
    pub fn history(&self) -> Vec<LogRecord> {
        lock_unpoison(&self.state)
            .history
            .iter()
            .map(|record| LogRecord::clone(record))
            .collect()
    }

    /// Flushes every registered sink with panic containment.
    pub fn flush(&self) -> DispatchReport {
        let sinks = lock_unpoison(&self.state)
            .sinks
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut report = DispatchReport::default();
        for slot in sinks {
            let _guard = lock_unpoison(&slot.dispatch);
            let result = panic::catch_unwind(AssertUnwindSafe(|| slot.sink.flush()));
            match result {
                Ok(Ok(())) => report.delivered += 1,
                Ok(Err(_)) => report.failed += 1,
                Err(payload) => {
                    // A custom panic payload may itself panic from Drop. Forgetting it
                    // prevents that destructor from reopening the unwind boundary.
                    core::mem::forget(payload);
                    report.panicked += 1;
                }
            }
        }
        report
    }
}

/// Stateful formatter matching the legacy file/console line layout.
#[derive(Debug)]
pub struct LegacyLogFormatter {
    max_channel_bytes: Mutex<usize>,
}

impl Default for LegacyLogFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl LegacyLogFormatter {
    /// Creates a formatter with the legacy initial 12-byte channel width.
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_channel_bytes: Mutex::new(12),
        }
    }

    /// Formats a record using legacy timestamp, channel, level, and multiline
    /// alignment rules.
    #[must_use]
    pub fn format(&self, record: &LogRecord) -> String {
        let mut channel_width = lock_unpoison(&self.max_channel_bytes);
        *channel_width = (*channel_width).max(record.channel.len());
        let channel_width = *channel_width;

        let timestamp = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{}",
            record.timestamp.year,
            record.timestamp.month,
            record.timestamp.day,
            record.timestamp.hour,
            record.timestamp.minute,
            record.timestamp.second,
            record.timestamp.millisecond
        );
        let channel = format!("[{}]  ", record.channel);
        let level = format!("{}  ", record.level.legacy_label());
        let timestamp_width = 25;
        let channel_field_width = channel_width + 4;
        let level_width = 12;
        let continuation_width = timestamp_width + channel_field_width + level_width;

        let mut output = String::new();
        push_padded(&mut output, &timestamp, timestamp_width);
        push_padded(&mut output, &channel, channel_field_width);
        push_padded(&mut output, &level, level_width);
        for (index, part) in record.message.split('\n').enumerate() {
            if index > 0 {
                output.push_str(&" ".repeat(continuation_width.saturating_sub(1)));
            }
            output.push_str(part);
            output.push('\n');
        }
        output
    }
}

/// Synchronous, owned file sink with deterministic flush and shutdown.
pub struct FileLogSink {
    level: LogLevel,
    formatter: Arc<LegacyLogFormatter>,
    writer: Mutex<BufWriter<File>>,
}

impl std::fmt::Debug for FileLogSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileLogSink")
            .field("level", &self.level)
            .field("path", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl FileLogSink {
    /// Creates or truncates a log file.
    ///
    /// # Errors
    ///
    /// Returns a path-redacted open error.
    pub fn create(path: impl AsRef<Path>, level: LogLevel) -> Result<Self, LogSinkError> {
        Self::with_formatter(path, level, shared_formatter())
    }

    /// Creates a file sink with an explicitly shared formatter.
    ///
    /// # Errors
    ///
    /// Returns a path-redacted open error.
    pub fn with_formatter(
        path: impl AsRef<Path>,
        level: LogLevel,
        formatter: Arc<LegacyLogFormatter>,
    ) -> Result<Self, LogSinkError> {
        let file = File::create(path).map_err(|source| LogSinkError::Open { source })?;
        Ok(Self {
            level,
            formatter,
            writer: Mutex::new(BufWriter::new(file)),
        })
    }
}

impl LogSink for FileLogSink {
    fn max_level(&self) -> LogLevel {
        self.level
    }

    fn write(&self, record: &LogRecord) -> Result<(), LogSinkError> {
        lock_unpoison(&self.writer)
            .write_all(self.formatter.format(record).as_bytes())
            .map_err(|source| LogSinkError::Write { source })
    }

    fn flush(&self) -> Result<(), LogSinkError> {
        lock_unpoison(&self.writer)
            .flush()
            .map_err(|source| LogSinkError::Flush { source })
    }
}

/// Owned UTF-8 console/writer sink. Repeated coalesced records are suppressed
/// like the legacy console logger.
pub struct ConsoleLogSink {
    level: LogLevel,
    formatter: Arc<LegacyLogFormatter>,
    writer: Mutex<Box<dyn Write + Send>>,
}

impl std::fmt::Debug for ConsoleLogSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConsoleLogSink")
            .field("level", &self.level)
            .finish_non_exhaustive()
    }
}

impl ConsoleLogSink {
    /// Creates a sink around an owned writer.
    #[must_use]
    pub fn new(writer: impl Write + Send + 'static, level: LogLevel) -> Self {
        Self::with_formatter(writer, level, shared_formatter())
    }

    /// Creates a sink around standard output.
    #[must_use]
    pub fn stdout(level: LogLevel) -> Self {
        Self::new(io::stdout(), level)
    }

    /// Creates a sink with an explicitly shared formatter.
    #[must_use]
    pub fn with_formatter(
        writer: impl Write + Send + 'static,
        level: LogLevel,
        formatter: Arc<LegacyLogFormatter>,
    ) -> Self {
        Self {
            level,
            formatter,
            writer: Mutex::new(Box::new(writer)),
        }
    }
}

impl LogSink for ConsoleLogSink {
    fn max_level(&self) -> LogLevel {
        self.level
    }

    fn write(&self, record: &LogRecord) -> Result<(), LogSinkError> {
        if record.repeat_count != 1 {
            return Ok(());
        }
        let mut writer = lock_unpoison(&self.writer);
        writer
            .write_all(self.formatter.format(record).as_bytes())
            .and_then(|()| writer.flush())
            .map_err(|source| LogSinkError::Write { source })
    }

    fn flush(&self) -> Result<(), LogSinkError> {
        lock_unpoison(&self.writer)
            .flush()
            .map_err(|source| LogSinkError::Flush { source })
    }
}

/// A closed sink error that never formats paths or record contents.
#[derive(Debug, Error)]
pub enum LogSinkError {
    /// A file sink could not be opened.
    #[error("log sink could not open its destination")]
    Open {
        /// The path-free operating-system error.
        #[source]
        source: io::Error,
    },
    /// A sink could not write a record.
    #[error("log sink could not write a record")]
    Write {
        /// The path-free operating-system error.
        #[source]
        source: io::Error,
    },
    /// A sink could not flush buffered records.
    #[error("log sink could not flush")]
    Flush {
        /// The path-free operating-system error.
        #[source]
        source: io::Error,
    },
}

fn shared_formatter() -> Arc<LegacyLogFormatter> {
    static FORMATTER: OnceLock<Arc<LegacyLogFormatter>> = OnceLock::new();
    Arc::clone(FORMATTER.get_or_init(|| Arc::new(LegacyLogFormatter::new())))
}

fn sink_allows(sink: &dyn LogSink, level: LogLevel) -> Result<bool, ()> {
    match panic::catch_unwind(AssertUnwindSafe(|| sink.max_level())) {
        Ok(max_level) => Ok(max_level.allows(level)),
        Err(payload) => {
            // A custom panic payload may itself panic from Drop. Forgetting it
            // prevents that destructor from reopening the unwind boundary.
            core::mem::forget(payload);
            Err(())
        }
    }
}

fn dispatch_one(sink: &dyn LogSink, record: &LogRecord) -> DispatchReport {
    match panic::catch_unwind(AssertUnwindSafe(|| sink.write(record))) {
        Ok(Ok(())) => DispatchReport {
            delivered: 1,
            ..DispatchReport::default()
        },
        Ok(Err(_)) => DispatchReport {
            failed: 1,
            ..DispatchReport::default()
        },
        Err(payload) => {
            // A custom panic payload may itself panic from Drop. Forgetting it
            // prevents that destructor from reopening the unwind boundary.
            core::mem::forget(payload);
            DispatchReport {
                panicked: 1,
                ..DispatchReport::default()
            }
        }
    }
}

fn push_padded(output: &mut String, value: &str, width: usize) {
    output.push_str(value);
    output.push_str(&" ".repeat(width.saturating_sub(value.len())));
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(windows)]
fn system_timestamp() -> LogTimestamp {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    let mut local = MaybeUninit::<SYSTEMTIME>::uninit();
    // SAFETY: `GetLocalTime` initializes the complete caller-provided
    // `SYSTEMTIME` structure and does not retain the pointer.
    unsafe { GetLocalTime(local.as_mut_ptr()) };
    // SAFETY: the preceding Windows API call initialized the structure.
    let local = unsafe { local.assume_init() };
    let unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64);
    LogTimestamp {
        year: local.wYear,
        month: local.wMonth as u8,
        day: local.wDay as u8,
        hour: local.wHour as u8,
        minute: local.wMinute as u8,
        second: local.wSecond as u8,
        millisecond: local.wMilliseconds,
        unix_seconds,
    }
}

#[cfg(not(windows))]
fn system_timestamp() -> LogTimestamp {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let unix_seconds = duration.as_secs() as i64;
    let days = unix_seconds.div_euclid(86_400);
    let seconds = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    LogTimestamp {
        year,
        month,
        day,
        hour: (seconds / 3_600) as u8,
        minute: ((seconds % 3_600) / 60) as u8,
        second: (seconds % 60) as u8,
        millisecond: duration.subsec_millis() as u16,
        unix_seconds,
    }
}

#[cfg(not(windows))]
fn civil_from_days(days_since_epoch: i64) -> (u16, u8, u8) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year as u16, month as u8, day as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Barrier, Weak};

    #[derive(Debug)]
    struct FixedClock;

    impl LogClock for FixedClock {
        fn now(&self) -> LogTimestamp {
            LogTimestamp {
                year: 2026,
                month: 7,
                day: 16,
                hour: 12,
                minute: 34,
                second: 56,
                millisecond: 7,
                unix_seconds: 1_768_000_000,
            }
        }
    }

    fn join_worker(worker: std::thread::JoinHandle<()>) {
        match worker.join() {
            Ok(()) => {}
            Err(payload) => {
                core::mem::forget(payload);
                panic!("logging test worker panicked");
            }
        }
    }

    #[test]
    fn clock_runs_outside_state_lock_and_can_reenter_history() {
        struct ReentrantClock {
            registry: Mutex<Weak<LogRegistry>>,
            calls: AtomicU64,
            state_was_free: AtomicBool,
            reentered: AtomicBool,
        }

        impl LogClock for ReentrantClock {
            fn now(&self) -> LogTimestamp {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let registry = lock_unpoison(&self.registry).upgrade();
                if let Some(registry) = registry {
                    let state_was_free = registry.state.try_lock().is_ok();
                    self.state_was_free.store(state_was_free, Ordering::SeqCst);
                    if state_was_free {
                        let _ = registry.history();
                        self.reentered.store(true, Ordering::SeqCst);
                    }
                }
                FixedClock.now()
            }
        }

        let clock = Arc::new(ReentrantClock {
            registry: Mutex::new(Weak::new()),
            calls: AtomicU64::new(0),
            state_was_free: AtomicBool::new(false),
            reentered: AtomicBool::new(false),
        });
        let registry = Arc::new(LogRegistry::with_clock(clock.clone()));
        *lock_unpoison(&clock.registry) = Arc::downgrade(&registry);

        registry.log(LogLevel::Info, "Clock", "state-free timestamp");

        assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
        assert!(clock.state_was_free.load(Ordering::SeqCst));
        assert!(clock.reentered.load(Ordering::SeqCst));
        assert_eq!(registry.history().len(), 1);
    }

    #[test]
    fn concurrent_repeats_use_one_timestamp_and_keep_the_exact_count() {
        struct CountingClock(AtomicU64);

        impl LogClock for CountingClock {
            fn now(&self) -> LogTimestamp {
                self.0.fetch_add(1, Ordering::SeqCst);
                FixedClock.now()
            }
        }

        const WORKERS: usize = 16;
        let clock = Arc::new(CountingClock(AtomicU64::new(0)));
        let registry = Arc::new(LogRegistry::with_clock(clock.clone()));
        let start = Arc::new(Barrier::new(WORKERS));
        let workers = (0..WORKERS)
            .map(|_| {
                let registry = Arc::clone(&registry);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    registry.log(LogLevel::Info, "Concurrent", "same");
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            join_worker(worker);
        }

        let history = registry.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].repeat_count(), WORKERS as u32);
        assert_eq!(clock.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn overlapping_new_records_keep_sequence_order() {
        struct BlockingFirstClock {
            calls: AtomicU64,
            first_entered: Barrier,
            release_first: Barrier,
        }

        impl LogClock for BlockingFirstClock {
            fn now(&self) -> LogTimestamp {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    self.first_entered.wait();
                    self.release_first.wait();
                }
                let mut timestamp = FixedClock.now();
                timestamp.millisecond = call as u16;
                timestamp
            }
        }

        let clock = Arc::new(BlockingFirstClock {
            calls: AtomicU64::new(0),
            first_entered: Barrier::new(2),
            release_first: Barrier::new(2),
        });
        let registry = Arc::new(LogRegistry::with_clock(clock.clone()));
        let first_registry = Arc::clone(&registry);
        let first = std::thread::spawn(move || {
            first_registry.log(LogLevel::Info, "Concurrent", "first");
        });
        clock.first_entered.wait();

        let second_ready = Arc::new(Barrier::new(2));
        let second_registry = Arc::clone(&registry);
        let second_ready_for_worker = Arc::clone(&second_ready);
        let second = std::thread::spawn(move || {
            second_ready_for_worker.wait();
            second_registry.log(LogLevel::Info, "Concurrent", "second");
        });
        second_ready.wait();
        clock.release_first.wait();

        join_worker(first);
        join_worker(second);

        let history = registry.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].message(), "first");
        assert_eq!(history[0].timestamp().millisecond, 0);
        assert_eq!(history[1].message(), "second");
        assert_eq!(history[1].timestamp().millisecond, 1);
        assert_eq!(clock.calls.load(Ordering::SeqCst), 2);
    }

    /// An add-on may pass `Off` or `All` as a severity. The reference dispatches on a
    /// plain `msg->Level <= sink.Level` and renders anything outside `CRITICAL..TRACE` as
    /// `(null)`, so such a record must reach the sink rather than be discarded.
    #[test]
    fn a_non_message_severity_still_reaches_sinks_labelled_null() {
        let registry = LogRegistry::with_clock(Arc::new(FixedClock));
        let sink = Arc::new(RecordingSink::new(LogLevel::Trace));
        registry.register(Arc::clone(&sink) as Arc<dyn LogSink>);

        // Off is numerically lowest, so it passes every sink.
        registry.log(LogLevel::Off, "Core", "level zero");
        // All is highest, so a Trace sink must filter it, exactly as the reference does.
        registry.log(LogLevel::All, "Core", "level six");

        let records = lock_unpoison(&sink.records);
        assert_eq!(
            records.len(),
            1,
            "Off must be written and All must be filtered by a Trace sink"
        );
        assert_eq!(records[0].message.as_ref(), "level zero");
        assert_eq!(records[0].level.legacy_label(), "(null)");
    }

    /// The same record is accepted once the sink itself is set to `All`.
    #[test]
    fn a_sink_set_to_all_accepts_the_all_severity() {
        let registry = LogRegistry::with_clock(Arc::new(FixedClock));
        let sink = Arc::new(RecordingSink::new(LogLevel::All));
        registry.register(Arc::clone(&sink) as Arc<dyn LogSink>);

        registry.log(LogLevel::All, "Core", "level six");

        let records = lock_unpoison(&sink.records);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].level.legacy_label(), "(null)");
    }

    struct RecordingSink {
        level: LogLevel,
        records: Mutex<Vec<LogRecord>>,
    }

    impl RecordingSink {
        fn new(level: LogLevel) -> Self {
            Self {
                level,
                records: Mutex::new(Vec::new()),
            }
        }
    }

    impl LogSink for RecordingSink {
        fn max_level(&self) -> LogLevel {
            self.level
        }

        fn write(&self, record: &LogRecord) -> Result<(), LogSinkError> {
            lock_unpoison(&self.records).push(record.clone());
            Ok(())
        }
    }

    struct ReentrantSinkDrop {
        registry: Weak<LogRegistry>,
        locks_were_free: Arc<AtomicBool>,
        reentered: Arc<AtomicBool>,
    }

    impl LogSink for ReentrantSinkDrop {
        fn max_level(&self) -> LogLevel {
            LogLevel::All
        }

        fn write(&self, _record: &LogRecord) -> Result<(), LogSinkError> {
            Ok(())
        }
    }

    impl Drop for ReentrantSinkDrop {
        fn drop(&mut self) {
            let Some(registry) = self.registry.upgrade() else {
                return;
            };
            let locks_were_free = match registry.state.try_lock() {
                Ok(state) => state
                    .sinks
                    .values()
                    .all(|slot| slot.dispatch.try_lock().is_ok()),
                Err(_) => false,
            };
            self.locks_were_free
                .store(locks_were_free, Ordering::SeqCst);
            if locks_were_free {
                let _ = registry.log(LogLevel::Info, "Drop", "reentrant sink destructor");
                self.reentered.store(true, Ordering::SeqCst);
            }
        }
    }

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            lock_unpoison(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn filters_replays_and_preserves_legacy_repeat_coalescing() {
        let registry = LogRegistry::with_clock(Arc::new(FixedClock));
        let first = Arc::new(RecordingSink::new(LogLevel::Info));
        registry.register(first.clone());

        assert_eq!(registry.log(LogLevel::Debug, "Core", "hidden").delivered, 0);
        assert_eq!(registry.log(LogLevel::Info, "Core", "same").delivered, 1);
        assert_eq!(registry.log(LogLevel::Info, "Other", "same").delivered, 1);

        let history = registry.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].channel(), "Core");
        assert_eq!(history[1].repeat_count(), 2);

        let late = Arc::new(RecordingSink::new(LogLevel::All));
        let (_, replay) = registry.register(late.clone());
        assert_eq!(replay.delivered, 2);
        let replayed = lock_unpoison(&late.records);
        assert_eq!(replayed[1].repeat_count(), 2);
    }

    #[test]
    fn console_suppresses_repeats_and_formatter_matches_legacy_shape() {
        let registry = LogRegistry::with_clock(Arc::new(FixedClock));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let formatter = Arc::new(LegacyLogFormatter::new());
        let sink = Arc::new(ConsoleLogSink::with_formatter(
            SharedWriter(Arc::clone(&bytes)),
            LogLevel::All,
            formatter,
        ));
        registry.register(sink);

        registry.log(LogLevel::Info, "Addon", "line one\nline two");
        registry.log(LogLevel::Info, "Different", "line one\nline two");

        let output =
            String::from_utf8(lock_unpoison(&bytes).clone()).expect("formatter should emit UTF-8");
        assert!(output.starts_with("2026-07-16 12:34:56.7    [Addon]"));
        assert!(output.contains("[INFO]"));
        assert!(output.contains("line one\n"));
        assert!(output.ends_with("line two\n"));
        assert_eq!(output.matches("line one").count(), 1);
    }

    #[test]
    fn file_sink_owns_file_and_flushes_deterministically() {
        static NEXT_FILE: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nexus-platform-log-{}-{id}-日本.log",
            std::process::id()
        ));
        let registry = LogRegistry::with_clock(Arc::new(FixedClock));
        let sink = Arc::new(
            FileLogSink::with_formatter(&path, LogLevel::All, Arc::new(LegacyLogFormatter::new()))
                .expect("test log should open"),
        );
        let (id, _) = registry.register(sink);
        registry.log_addon(LogLevel::Warning, "written");
        assert_eq!(registry.flush().failed, 0);
        assert!(registry.deregister(id));

        let output = std::fs::read_to_string(&path).expect("test log should be readable");
        assert!(output.contains("[Addon]"));
        assert!(output.contains("[WARNING]"));
        assert!(output.ends_with("written\n"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn deregistration_drops_sinks_after_unlock_and_allows_reentrancy() {
        let registry = Arc::new(LogRegistry::with_clock(Arc::new(FixedClock)));
        let locks_were_free = Arc::new(AtomicBool::new(false));
        let reentered = Arc::new(AtomicBool::new(false));
        let (id, _) = registry.register(Arc::new(ReentrantSinkDrop {
            registry: Arc::downgrade(&registry),
            locks_were_free: Arc::clone(&locks_were_free),
            reentered: Arc::clone(&reentered),
        }));

        assert!(registry.deregister(id));
        assert!(locks_were_free.load(Ordering::SeqCst));
        assert!(reentered.load(Ordering::SeqCst));
        assert_eq!(registry.history().len(), 1);
    }

    #[test]
    fn registration_collision_releases_dispatch_before_displaced_sink_drop() {
        let registry = Arc::new(LogRegistry::with_clock(Arc::new(FixedClock)));
        let locks_were_free = Arc::new(AtomicBool::new(false));
        let reentered = Arc::new(AtomicBool::new(false));
        let (displaced_id, _) = registry.register(Arc::new(ReentrantSinkDrop {
            registry: Arc::downgrade(&registry),
            locks_were_free: Arc::clone(&locks_were_free),
            reentered: Arc::clone(&reentered),
        }));
        lock_unpoison(&registry.state).next_registration = displaced_id.get();

        let replacement = Arc::new(RecordingSink::new(LogLevel::All));
        let (replacement_id, _) = registry.register(replacement.clone());

        assert_eq!(replacement_id, displaced_id);
        assert!(locks_were_free.load(Ordering::SeqCst));
        assert!(reentered.load(Ordering::SeqCst));
        assert_eq!(lock_unpoison(&replacement.records).len(), 1);
    }

    #[test]
    fn sink_panics_are_contained() {
        struct PanicSink;
        impl LogSink for PanicSink {
            fn max_level(&self) -> LogLevel {
                LogLevel::All
            }

            fn write(&self, _record: &LogRecord) -> Result<(), LogSinkError> {
                panic!("contained sink panic")
            }
        }

        let registry = LogRegistry::with_clock(Arc::new(FixedClock));
        registry.register(Arc::new(PanicSink));
        let report = registry.log(LogLevel::Info, "Core", "ordinary message");
        assert_eq!(report.panicked, 1);
    }

    #[test]
    fn max_level_panics_are_contained_without_invoking_write() {
        struct PanicOnDrop(Arc<AtomicU64>);

        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("panic payload destructor must not run");
            }
        }

        struct AdversarialFilterSink {
            payload_drops: Arc<AtomicU64>,
            writes: Arc<AtomicU64>,
        }

        impl LogSink for AdversarialFilterSink {
            fn max_level(&self) -> LogLevel {
                std::panic::panic_any(PanicOnDrop(Arc::clone(&self.payload_drops)));
            }

            fn write(&self, _record: &LogRecord) -> Result<(), LogSinkError> {
                self.writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let payload_drops = Arc::new(AtomicU64::new(0));
        let writes = Arc::new(AtomicU64::new(0));
        let registry = LogRegistry::with_clock(Arc::new(FixedClock));
        registry.log(LogLevel::Info, "Core", "history");
        let (_, replay) = registry.register(Arc::new(AdversarialFilterSink {
            payload_drops: Arc::clone(&payload_drops),
            writes: Arc::clone(&writes),
        }));

        assert_eq!(replay.panicked, 1);
        assert_eq!(replay.delivered, 0);
        assert_eq!(replay.failed, 0);
        let live = registry.log(LogLevel::Info, "Core", "live");
        assert_eq!(live.panicked, 1);
        assert_eq!(live.delivered, 0);
        assert_eq!(live.failed, 0);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert_eq!(payload_drops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn panic_payload_destructors_cannot_reopen_sink_unwinding() {
        struct PanicOnDrop(Arc<AtomicU64>);

        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("panic payload destructor must not run");
            }
        }

        struct AdversarialSink(Arc<AtomicU64>);

        impl LogSink for AdversarialSink {
            fn max_level(&self) -> LogLevel {
                LogLevel::All
            }

            fn write(&self, _record: &LogRecord) -> Result<(), LogSinkError> {
                std::panic::panic_any(PanicOnDrop(Arc::clone(&self.0)));
            }

            fn flush(&self) -> Result<(), LogSinkError> {
                std::panic::panic_any(PanicOnDrop(Arc::clone(&self.0)));
            }
        }

        let payload_drops = Arc::new(AtomicU64::new(0));
        let registry = LogRegistry::with_clock(Arc::new(FixedClock));
        registry.register(Arc::new(AdversarialSink(Arc::clone(&payload_drops))));

        assert_eq!(
            registry
                .log(LogLevel::Info, "Core", "ordinary message")
                .panicked,
            1
        );
        assert_eq!(registry.flush().panicked, 1);
        assert_eq!(payload_drops.load(Ordering::SeqCst), 0);
    }
}
