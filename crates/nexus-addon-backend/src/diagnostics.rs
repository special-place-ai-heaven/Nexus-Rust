use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

/// Closed, address-free failure classes for the native API boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendFailure {
    /// No current add-on generation owns the call.
    CallerAttribution,
    /// A native string, byte range, or retained cell could not be snapshotted.
    NativeMemory,
    /// A bounded native string was not valid UTF-8.
    InvalidText,
    /// A validated request was rejected by its domain service.
    ServiceRejected,
}

/// Monotonic counters that can be published without retaining native inputs.
pub struct BackendFailures {
    caller_attribution: AtomicU64,
    native_memory: AtomicU64,
    invalid_text: AtomicU64,
    service_rejected: AtomicU64,
}

impl BackendFailures {
    /// Creates an empty process-lifetime counter set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            caller_attribution: AtomicU64::new(0),
            native_memory: AtomicU64::new(0),
            invalid_text: AtomicU64::new(0),
            service_rejected: AtomicU64::new(0),
        }
    }

    /// Records one contained failure without storing its native arguments.
    pub fn record(&self, failure: BackendFailure) {
        let counter = match failure {
            BackendFailure::CallerAttribution => &self.caller_attribution,
            BackendFailure::NativeMemory => &self.native_memory,
            BackendFailure::InvalidText => &self.invalid_text,
            BackendFailure::ServiceRejected => &self.service_rejected,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns a coherent-enough diagnostic snapshot of monotonic counters.
    #[must_use]
    pub fn snapshot(&self) -> BackendFailureSnapshot {
        BackendFailureSnapshot {
            caller_attribution: self.caller_attribution.load(Ordering::Relaxed),
            native_memory: self.native_memory.load(Ordering::Relaxed),
            invalid_text: self.invalid_text.load(Ordering::Relaxed),
            service_rejected: self.service_rejected.load(Ordering::Relaxed),
        }
    }
}

impl Default for BackendFailures {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for BackendFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackendFailures")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

/// Copyable diagnostic view containing counts only.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BackendFailureSnapshot {
    /// Calls rejected because their active owner could not be established.
    pub caller_attribution: u64,
    /// Native memory snapshots or retained-cell operations that failed.
    pub native_memory: u64,
    /// Native strings rejected because they were not valid UTF-8.
    pub invalid_text: u64,
    /// Requests rejected after reaching a typed service.
    pub service_rejected: u64,
}

#[cfg(test)]
mod tests {
    use super::{BackendFailure, BackendFailureSnapshot, BackendFailures};

    #[test]
    fn counters_are_closed_monotonic_and_address_free() {
        let failures = BackendFailures::new();
        failures.record(BackendFailure::CallerAttribution);
        failures.record(BackendFailure::NativeMemory);
        failures.record(BackendFailure::NativeMemory);
        failures.record(BackendFailure::InvalidText);
        failures.record(BackendFailure::ServiceRejected);

        assert_eq!(
            failures.snapshot(),
            BackendFailureSnapshot {
                caller_attribution: 1,
                native_memory: 2,
                invalid_text: 1,
                service_rejected: 1,
            }
        );
        let debug = format!("{failures:?}");
        assert!(!debug.contains("0x"));
    }
}
