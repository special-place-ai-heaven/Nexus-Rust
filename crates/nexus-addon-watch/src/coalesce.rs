use crate::{ChangeKinds, ChangeSignal, WatchConfig};
use std::time::Duration;

pub(crate) struct Coalescer {
    config: WatchConfig,
    first_at: Option<Duration>,
    last_at: Option<Duration>,
    kinds: ChangeKinds,
}

impl Coalescer {
    pub(crate) fn new(config: WatchConfig) -> Self {
        Self {
            config,
            first_at: None,
            last_at: None,
            kinds: ChangeKinds::default(),
        }
    }

    pub(crate) fn record(&mut self, now: Duration, kinds: ChangeKinds) {
        if kinds.is_empty() {
            return;
        }

        if self.first_at.is_none() {
            self.first_at = Some(now);
        }
        self.last_at = Some(self.last_at.map_or(now, |last| last.max(now)));
        self.kinds |= kinds;
    }

    pub(crate) fn deadline(&self) -> Option<Duration> {
        let first = self.first_at?;
        let last = self.last_at?;
        let quiet_deadline = add_or_max(last, self.config.quiet_period());
        let latency_deadline = add_or_max(first, self.config.max_latency());
        Some(quiet_deadline.min(latency_deadline))
    }

    pub(crate) fn take_if_due(&mut self, now: Duration) -> Option<ChangeSignal> {
        if !self.deadline().is_some_and(|deadline| now >= deadline) {
            return None;
        }

        let kinds = std::mem::take(&mut self.kinds);
        self.first_at = None;
        self.last_at = None;
        Some(ChangeSignal::new(kinds))
    }

    pub(crate) fn clear(&mut self) {
        self.first_at = None;
        self.last_at = None;
        self.kinds = ChangeKinds::default();
    }
}

fn add_or_max(lhs: Duration, rhs: Duration) -> Duration {
    lhs.checked_add(rhs).unwrap_or(Duration::MAX)
}

#[cfg(test)]
mod tests {
    use super::Coalescer;
    use crate::{ChangeKinds, ChangeSignal, WatchConfig};
    use std::time::Duration;

    fn config() -> WatchConfig {
        match WatchConfig::new(Duration::from_millis(100), Duration::from_millis(500)) {
            Ok(config) => config,
            Err(_) => panic!("test configuration is valid"),
        }
    }

    #[test]
    fn trailing_quiet_period_moves_with_each_change() {
        let mut coalescer = Coalescer::new(config());
        coalescer.record(Duration::ZERO, ChangeKinds::CREATED);
        coalescer.record(Duration::from_millis(80), ChangeKinds::WRITTEN);

        assert_eq!(coalescer.deadline(), Some(Duration::from_millis(180)));
        assert_eq!(coalescer.take_if_due(Duration::from_millis(179)), None);
        assert_eq!(
            coalescer.take_if_due(Duration::from_millis(180)),
            Some(ChangeSignal::new(
                ChangeKinds::CREATED | ChangeKinds::WRITTEN
            ))
        );
    }

    #[test]
    fn continuous_changes_are_capped_by_maximum_latency() {
        let mut coalescer = Coalescer::new(config());
        coalescer.record(Duration::ZERO, ChangeKinds::CREATED);
        coalescer.record(Duration::from_millis(450), ChangeKinds::DELETED);

        assert_eq!(coalescer.deadline(), Some(Duration::from_millis(500)));
        assert_eq!(
            coalescer.take_if_due(Duration::from_millis(500)),
            Some(ChangeSignal::new(
                ChangeKinds::CREATED | ChangeKinds::DELETED
            ))
        );
    }

    #[test]
    fn clearing_discards_a_pending_signal() {
        let mut coalescer = Coalescer::new(config());
        coalescer.record(Duration::ZERO, ChangeKinds::RENAMED);
        coalescer.clear();

        assert_eq!(coalescer.take_if_due(Duration::from_secs(1)), None);
    }
}
