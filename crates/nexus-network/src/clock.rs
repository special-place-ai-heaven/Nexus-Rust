//! Time boundaries used by cache and updater code.

use std::time::{SystemTime, UNIX_EPOCH};

/// Supplies Unix timestamps without coupling request logic to the system clock.
pub trait Clock {
    /// Returns the current number of whole seconds since the Unix epoch.
    fn unix_timestamp(&self) -> i64;
}

/// The production clock backed by [`SystemTime`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_timestamp(&self) -> i64 {
        let Ok(elapsed) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return 0;
        };

        i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, SystemClock};

    #[test]
    fn system_clock_returns_a_nonnegative_timestamp() {
        assert!(SystemClock.unix_timestamp() >= 0);
    }
}
