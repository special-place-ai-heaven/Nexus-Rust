use std::{error::Error, fmt};

use nexus_abi::MinHookStatus;
use nexus_core::OwnerToken;

/// Proof that all hooks registered by one exact addon generation were retired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CleanupReport {
    owner: OwnerToken,
    retired: usize,
}

impl CleanupReport {
    #[cfg(all(windows, target_arch = "x86_64"))]
    pub(crate) const fn new(owner: OwnerToken, retired: usize) -> Self {
        Self { owner, retired }
    }

    /// Returns the exact addon generation that was cleaned.
    #[must_use]
    pub const fn owner(self) -> OwnerToken {
        self.owner
    }

    /// Returns how many hooks were retired.
    #[must_use]
    pub const fn retired(self) -> usize {
        self.retired
    }

    /// Returns the number of matching hooks left registered after success.
    #[must_use]
    pub const fn remaining(self) -> usize {
        0
    }
}

/// A redacted owner-cleanup failure that never exposes hook addresses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CleanupError {
    owner: OwnerToken,
    status: MinHookStatus,
    retired: usize,
    remaining: usize,
}

impl CleanupError {
    pub(crate) const fn new(
        owner: OwnerToken,
        status: MinHookStatus,
        retired: usize,
        remaining: usize,
    ) -> Self {
        Self {
            owner,
            status,
            retired,
            remaining,
        }
    }

    /// Returns the exact addon generation whose cleanup failed.
    #[must_use]
    pub const fn owner(self) -> OwnerToken {
        self.owner
    }

    /// Returns the compatible MinHook status that caused cleanup to stop.
    #[must_use]
    pub const fn status(self) -> MinHookStatus {
        self.status
    }

    /// Returns how many matching hooks were retired before the failure.
    #[must_use]
    pub const fn retired(self) -> usize {
        self.retired
    }

    /// Returns how many hooks for the exact generation remain retryable.
    #[must_use]
    pub const fn remaining(self) -> usize {
        self.remaining
    }
}

impl fmt::Display for CleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "inline-hook cleanup failed with status {}; {} hook(s) remain",
            self.status.0, self.remaining
        )
    }
}

impl Error for CleanupError {}
