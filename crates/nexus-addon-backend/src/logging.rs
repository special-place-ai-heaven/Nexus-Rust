use core::ffi::c_char;
use core::fmt;
use std::sync::Arc;

use nexus_abi::LogLevel as AbiLogLevel;
use nexus_platform::{LogLevel, LogRegistry};

use crate::{BackendFailure, BackendOperationError, NativeCallBoundary};

/// Caller-attributed adapter for native add-on logging.
pub struct LoggingApi {
    boundary: Arc<NativeCallBoundary>,
    logs: Arc<LogRegistry>,
}

impl LoggingApi {
    /// Creates a logging adapter around the process registry.
    #[must_use]
    pub fn new(boundary: Arc<NativeCallBoundary>, logs: Arc<LogRegistry>) -> Self {
        Self { boundary, logs }
    }

    /// Copies and emits one revision-2-and-newer log record.
    pub fn log(
        &self,
        level: AbiLogLevel,
        channel: *const c_char,
        message: *const c_char,
    ) -> Result<(), BackendOperationError> {
        let _owner = self.boundary.resolve_owner(None)?;
        let level = self.message_level(level)?;
        let channel = self.boundary.snapshot_identifier(channel)?;
        let message = self.boundary.snapshot_message(message)?;
        self.logs
            .log(level, channel.into_string(), message.into_string());
        Ok(())
    }

    /// Copies and emits one revision-1 record on the legacy Addon channel.
    pub fn log_v1(
        &self,
        level: AbiLogLevel,
        message: *const c_char,
    ) -> Result<(), BackendOperationError> {
        let _owner = self.boundary.resolve_owner(None)?;
        let level = self.message_level(level)?;
        let message = self.boundary.snapshot_message(message)?;
        self.logs.log_addon(level, message.into_string());
        Ok(())
    }

    /// Maps an add-on's severity onto a host level.
    ///
    /// `0` and `6` are accepted rather than rejected. The reference's `StringFrom`
    /// (`src/Core/Logging/LogConst.cpp:18-31`) renders any value outside `CRITICAL..TRACE`
    /// as `(null)` and still writes the line, and its dispatch is a plain
    /// `msg->Level <= sink.Level`. Rejecting these dropped an add-on's log line outright,
    /// which is a silent loss of the diagnostic a bug report is built from.
    ///
    /// A value above `6` has no host representation. The reference would render it
    /// `(null)` and then filter it out at every sink, since no sink level is that high, so
    /// discarding it here produces the same observable result: nothing in the log.
    fn message_level(&self, level: AbiLogLevel) -> Result<LogLevel, BackendOperationError> {
        let level = match level.0 {
            0 => LogLevel::Off,
            1 => LogLevel::Critical,
            2 => LogLevel::Warning,
            3 => LogLevel::Info,
            4 => LogLevel::Debug,
            5 => LogLevel::Trace,
            6 => LogLevel::All,
            _ => {
                self.boundary
                    .failures()
                    .record(BackendFailure::ServiceRejected);
                return Err(BackendOperationError::ServiceRejected);
            }
        };
        Ok(level)
    }
}

impl fmt::Debug for LoggingApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoggingApi")
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nexus_abi::LogLevel as AbiLogLevel;
    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::OwnerToken;
    use nexus_native_memory::NativeMemoryReader;
    use nexus_platform::{LogLevel, LogRegistry};

    use super::LoggingApi;
    use crate::{BackendFailures, NativeCallBoundary};

    struct NoOwners;

    impl AddressOwnerResolver for NoOwners {
        fn owner_for_address(&self, _address: core::num::NonZeroUsize) -> Option<OwnerToken> {
            None
        }

        fn is_current_owner(&self, _owner: OwnerToken) -> bool {
            false
        }
    }

    fn api() -> LoggingApi {
        let boundary = Arc::new(NativeCallBoundary::new(
            Arc::new(AddonCallerResolver::new(Arc::new(NoOwners))),
            NativeMemoryReader::default(),
            Arc::new(BackendFailures::new()),
        ));
        LoggingApi::new(boundary, Arc::new(LogRegistry::new()))
    }

    #[test]
    fn every_level_the_reference_accepts_crosses_the_native_boundary() {
        let api = api();
        assert_eq!(api.message_level(AbiLogLevel(1)), Ok(LogLevel::Critical));
        assert_eq!(api.message_level(AbiLogLevel(5)), Ok(LogLevel::Trace));

        // The reference renders these `(null)` and still writes the line, so rejecting
        // them silently loses an add-on's log output.
        assert_eq!(api.message_level(AbiLogLevel(0)), Ok(LogLevel::Off));
        assert_eq!(api.message_level(AbiLogLevel(6)), Ok(LogLevel::All));

        // Above 6 there is no host level, and the reference would filter it out of every
        // sink anyway, so nothing observable is lost by refusing it here.
        assert!(api.message_level(AbiLogLevel(7)).is_err());
        assert!(api.message_level(AbiLogLevel(u32::MAX)).is_err());
        assert_eq!(api.boundary.failures().snapshot().service_rejected, 2);
    }
}
