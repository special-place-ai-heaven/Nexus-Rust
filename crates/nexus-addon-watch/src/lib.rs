//! Bounded addon-directory change notifications.
//!
//! The watcher reports only finite change categories. It intentionally does not
//! retain or expose changed paths: consumers should rescan their owned addon
//! directory after receiving a signal.

mod coalesce;

#[cfg(target_os = "windows")]
mod windows;

use std::fmt;
use std::ops::{BitOr, BitOrAssign};
use std::time::Duration;

#[cfg(target_os = "windows")]
pub use windows::{AddonDirectoryWatcher, WatchReport};

/// A finite set of directory-change categories.
#[derive(Clone, Copy, Default, Eq, Hash, PartialEq)]
pub struct ChangeKinds(u8);

impl ChangeKinds {
    /// At least one entry was created.
    pub const CREATED: Self = Self(1 << 0);
    /// At least one entry was written or had relevant metadata changed.
    pub const WRITTEN: Self = Self(1 << 1);
    /// At least one entry was renamed.
    pub const RENAMED: Self = Self(1 << 2);
    /// At least one entry was deleted.
    pub const DELETED: Self = Self(1 << 3);
    /// The native event stream was incomplete, so a full rescan is required.
    pub const RESCAN: Self = Self(1 << 4);

    /// Returns true when no category is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns true when every category in other is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Returns true when at least one category is shared with other.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }
}

impl BitOr for ChangeKinds {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ChangeKinds {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl fmt::Debug for ChangeKinds {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut set = formatter.debug_set();
        if self.contains(Self::CREATED) {
            set.entry(&"created");
        }
        if self.contains(Self::WRITTEN) {
            set.entry(&"written");
        }
        if self.contains(Self::RENAMED) {
            set.entry(&"renamed");
        }
        if self.contains(Self::DELETED) {
            set.entry(&"deleted");
        }
        if self.contains(Self::RESCAN) {
            set.entry(&"rescan");
        }
        set.finish()
    }
}

/// A coalesced request to rescan the addon directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChangeSignal {
    kinds: ChangeKinds,
}

impl ChangeSignal {
    pub(crate) const fn new(kinds: ChangeKinds) -> Self {
        Self { kinds }
    }

    /// Returns the categories observed during the coalescing window.
    #[must_use]
    pub const fn kinds(self) -> ChangeKinds {
        self.kinds
    }
}

/// Debounce and maximum-latency settings for a watcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WatchConfig {
    quiet_period: Duration,
    max_latency: Duration,
}

impl WatchConfig {
    /// Creates validated watcher settings.
    ///
    /// The quiet period must be nonzero and the maximum latency must be at
    /// least as long as the quiet period.
    pub fn new(quiet_period: Duration, max_latency: Duration) -> Result<Self, WatchError> {
        if quiet_period.is_zero() || max_latency < quiet_period {
            return Err(WatchError::InvalidConfig);
        }

        Ok(Self {
            quiet_period,
            max_latency,
        })
    }

    /// Returns the time without new events required before delivery.
    #[must_use]
    pub const fn quiet_period(self) -> Duration {
        self.quiet_period
    }

    /// Returns the longest time an active event burst may delay delivery.
    #[must_use]
    pub const fn max_latency(self) -> Duration {
        self.max_latency
    }
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            quiet_period: Duration::from_millis(250),
            max_latency: Duration::from_secs(2),
        }
    }
}

/// A redacted watcher failure.
///
/// Variants intentionally contain no paths, handles, native error codes, or
/// other environment values, making both Display and Debug safe for logs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WatchError {
    /// The supplied path cannot be represented as a Windows path argument.
    #[error("the addon directory path is invalid")]
    InvalidPath,
    /// The debounce settings are internally inconsistent.
    #[error("the watcher configuration is invalid")]
    InvalidConfig,
    /// The addon directory could not be opened.
    #[error("the addon directory could not be opened")]
    DirectoryOpenFailed,
    /// The stop event could not be created.
    #[error("the watcher stop event could not be created")]
    StopEventCreationFailed,
    /// The overlapped-I/O event could not be created.
    #[error("the watcher I/O event could not be created")]
    IoEventCreationFailed,
    /// The overlapped-I/O event could not be reset.
    #[error("the watcher I/O event could not be reset")]
    IoEventResetFailed,
    /// A native directory read could not be started.
    #[error("the watcher directory read could not be started")]
    ReadStartFailed,
    /// A native directory read did not complete successfully.
    #[error("the watcher directory read failed")]
    ReadCompletionFailed,
    /// A pending native directory read could not be cancelled safely.
    #[error("the watcher directory read could not be cancelled")]
    ReadCancellationFailed,
    /// Waiting for a native watcher event failed.
    #[error("the watcher event wait failed")]
    WaitFailed,
    /// The worker thread could not be created.
    #[error("the watcher worker could not be started")]
    WorkerStartFailed,
    /// The worker ended before completing its startup handshake.
    #[error("the watcher worker did not complete startup")]
    WorkerStartupFailed,
    /// The worker panicked outside the contained consumer callback.
    #[error("the watcher worker panicked")]
    WorkerPanicked,
    /// The worker could not be signalled to stop normally.
    #[error("the watcher stop signal failed")]
    StopSignalFailed,
}

#[cfg(test)]
mod tests {
    use super::{ChangeKinds, WatchConfig, WatchError};
    use std::time::Duration;

    #[test]
    fn change_kinds_form_a_finite_bit_set() {
        let kinds = ChangeKinds::CREATED | ChangeKinds::RENAMED;

        assert!(kinds.contains(ChangeKinds::CREATED));
        assert!(kinds.intersects(ChangeKinds::RENAMED | ChangeKinds::DELETED));
        assert!(!kinds.contains(ChangeKinds::WRITTEN));
        assert!(!kinds.is_empty());
    }

    #[test]
    fn watch_config_rejects_zero_quiet_period() {
        assert_eq!(
            WatchConfig::new(Duration::ZERO, Duration::from_secs(1)),
            Err(WatchError::InvalidConfig)
        );
    }

    #[test]
    fn watch_config_rejects_latency_shorter_than_quiet_period() {
        assert_eq!(
            WatchConfig::new(Duration::from_secs(2), Duration::from_secs(1)),
            Err(WatchError::InvalidConfig)
        );
    }
}
