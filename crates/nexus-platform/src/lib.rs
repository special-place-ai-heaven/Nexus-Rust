//! Owned platform services for the Rust Nexus runtime.
//!
//! The crate preserves the observable path, settings, and logging contracts of
//! the legacy host while keeping ownership and shutdown explicit. The bundled
//! scheduler is deliberately named [`MinimalScheduler`]: the pinned Clockwork
//! submodule is not present in this checkout, so only contracts demonstrated by
//! its call sites are implemented.

mod logging;
mod paths;
mod scheduler;
mod settings;

pub use logging::{
    ConsoleLogSink, DispatchReport, FileLogSink, LegacyLogFormatter, LogClock, LogLevel, LogRecord,
    LogRegistry, LogSink, LogSinkError, LogTimestamp, RegistrationId, SystemLogClock,
};
pub use paths::{PathError, PathIndex, PathKey, PathRoots};
pub use scheduler::{
    CancellationToken, MinimalScheduler, MinimalSchedulerError, TaskHandle, TaskId, TaskOutcome,
    TaskPriority,
};
pub use settings::{LoadOutcome, NotificationReport, SettingsError, SettingsStore, SubscriptionId};
