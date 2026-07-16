use crate::{AdapterFailureKind, CleanupService, GapReason};
use nexus_core::OwnerToken;
use nexus_host::CleanupPhase;
use std::fmt;

/// Failure recorded for one callable adapter attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepFailure {
    /// The adapter returned a structured failure.
    Adapter(AdapterFailureKind),
    /// The adapter panicked; its payload was caught and forgotten.
    Panicked,
}

/// Persistent status for one service slot and exact owner generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepStatus {
    /// The phase has not attempted this slot.
    Pending,
    /// The slot completed and will not be called again on a phase retry.
    Complete {
        /// Cumulative removals across all attempts.
        removed: usize,
        /// Total callable attempts.
        attempts: u32,
    },
    /// The slot failed and remains eligible for retry.
    Failed {
        /// Redaction-safe failure category.
        failure: StepFailure,
        /// Cumulative partial removals across all attempts.
        removed: usize,
        /// Registrations known to remain, when reported by the adapter.
        remaining: Option<usize>,
        /// Total callable attempts.
        attempts: u32,
    },
    /// No safe callable adapter is bound.
    Gap {
        /// Explicit reason the service cannot be cleaned.
        reason: GapReason,
    },
}

/// Report for one service slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StepReport {
    service: CleanupService,
    status: StepStatus,
}

impl StepReport {
    pub(crate) const fn new(service: CleanupService, status: StepStatus) -> Self {
        Self { service, status }
    }

    /// Service represented by this report row.
    #[must_use]
    pub const fn service(self) -> CleanupService {
        self.service
    }

    /// Persistent retry status for this service.
    #[must_use]
    pub const fn status(self) -> StepStatus {
        self.status
    }
}

/// Overall state of one requested phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhaseStatus {
    /// Every required service slot completed.
    Complete,
    /// At least one service failed or is a reported gap.
    Incomplete,
    /// A preceding host phase has not completed.
    Blocked {
        /// Earliest phase that must complete first.
        required: CleanupPhase,
    },
}

/// Retry-safe report for one exact owner and cleanup phase.
#[derive(Clone, Eq, PartialEq)]
pub struct CleanupReport {
    owner: OwnerToken,
    phase: CleanupPhase,
    status: PhaseStatus,
    steps: Box<[StepReport]>,
}

impl CleanupReport {
    pub(crate) fn new(
        owner: OwnerToken,
        phase: CleanupPhase,
        status: PhaseStatus,
        steps: Box<[StepReport]>,
    ) -> Self {
        Self {
            owner,
            phase,
            status,
            steps,
        }
    }

    /// Exact signature and load generation this report belongs to.
    #[must_use]
    pub const fn owner(&self) -> OwnerToken {
        self.owner
    }

    /// Host cleanup phase represented by this report.
    #[must_use]
    pub const fn phase(&self) -> CleanupPhase {
        self.phase
    }

    /// Aggregate phase status.
    #[must_use]
    pub const fn status(&self) -> PhaseStatus {
        self.status
    }

    /// Fixed-order service rows for this phase.
    #[must_use]
    pub fn steps(&self) -> &[StepReport] {
        &self.steps
    }

    /// Whether every required service completed.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self.status, PhaseStatus::Complete)
    }
}

impl fmt::Debug for CleanupReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CleanupReport")
            .field("owner", &"<redacted>")
            .field("phase", &self.phase)
            .field("status", &self.status)
            .field("steps", &self.steps)
            .finish()
    }
}

/// Rich failure returned by [`crate::RegistrationCleaner::cleanup_phase`].
#[derive(Clone, Eq, PartialEq)]
pub struct CleanupFailure {
    report: CleanupReport,
}

impl CleanupFailure {
    pub(crate) const fn new(report: CleanupReport) -> Self {
        Self { report }
    }

    /// Retry-safe report describing every service outcome.
    #[must_use]
    pub const fn report(&self) -> &CleanupReport {
        &self.report
    }
}

impl fmt::Display for CleanupFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let failed = self
            .report
            .steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Failed { .. }))
            .count();
        let gaps = self
            .report
            .steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Gap { .. }))
            .count();
        write!(
            formatter,
            "addon registration cleanup incomplete: phase={:?}, failed={failed}, gaps={gaps}",
            self.report.phase
        )
    }
}

impl fmt::Debug for CleanupFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CleanupFailure")
            .field("report", &self.report)
            .finish()
    }
}

impl std::error::Error for CleanupFailure {}

/// Availability of one configured service slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoverageEntry {
    service: CleanupService,
    gap: Option<GapReason>,
}

impl CoverageEntry {
    pub(crate) const fn new(service: CleanupService, gap: Option<GapReason>) -> Self {
        Self { service, gap }
    }

    /// Service represented by this row.
    #[must_use]
    pub const fn service(self) -> CleanupService {
        self.service
    }

    /// Missing-adapter reason, or `None` when callable.
    #[must_use]
    pub const fn gap(self) -> Option<GapReason> {
        self.gap
    }
}

/// Static binding coverage for a cleaner instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverageReport {
    entries: Box<[CoverageEntry]>,
}

impl CoverageReport {
    pub(crate) const fn new(entries: Box<[CoverageEntry]>) -> Self {
        Self { entries }
    }

    /// Fixed-order coverage rows.
    #[must_use]
    pub fn entries(&self) -> &[CoverageEntry] {
        &self.entries
    }

    /// Whether all required service slots are callable.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.entries.iter().all(|entry| entry.gap.is_none())
    }
}
