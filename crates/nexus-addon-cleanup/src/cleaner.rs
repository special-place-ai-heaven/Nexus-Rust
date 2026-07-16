use crate::adapter::{ErasedAdapter, TypedAdapter};
use crate::domain::{
    CleanupDomain, CleanupService, EventCallbacks, FontCallbacks, FontResources, InlineHooks,
    LocalizationOverrides, ManagedInputCallbacks, RawWndProcCallbacks, TextureCallbacks,
    UiHostCallbacks, phase_from_index, phase_index,
};
use crate::report::{
    CleanupFailure, CleanupReport, CoverageEntry, CoverageReport, PhaseStatus, StepFailure,
    StepReport, StepStatus,
};
use nexus_core::OwnerToken;
use nexus_host::CleanupPhase;
use std::collections::HashMap;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

const PHASE_COUNT: usize = 3;

struct Slot {
    service: CleanupService,
    adapter: ErasedAdapter,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProgressState {
    Pending,
    Complete {
        removed: usize,
        attempts: u32,
    },
    Failed {
        failure: StepFailure,
        removed: usize,
        remaining: Option<usize>,
        attempts: u32,
    },
}

impl ProgressState {
    const fn is_complete(self) -> bool {
        matches!(self, Self::Complete { .. })
    }

    const fn counters(self) -> (usize, u32) {
        match self {
            Self::Pending => (0, 0),
            Self::Complete { removed, attempts }
            | Self::Failed {
                removed, attempts, ..
            } => (removed, attempts),
        }
    }

    const fn report(self) -> StepStatus {
        match self {
            Self::Pending => StepStatus::Pending,
            Self::Complete { removed, attempts } => StepStatus::Complete { removed, attempts },
            Self::Failed {
                failure,
                removed,
                remaining,
                attempts,
            } => StepStatus::Failed {
                failure,
                removed,
                remaining,
                attempts,
            },
        }
    }
}

struct OwnerProgress {
    slots: Box<[ProgressState]>,
    completed_phases: [bool; PHASE_COUNT],
}

impl OwnerProgress {
    fn new(slot_count: usize) -> Self {
        Self {
            slots: vec![ProgressState::Pending; slot_count].into_boxed_slice(),
            completed_phases: [false; PHASE_COUNT],
        }
    }
}

/// Production composite implementing `nexus_host::RegistrationCleaner`.
///
/// Progress is isolated by the complete [`OwnerToken`] key. A successful slot
/// is not reinvoked when another slot in the same phase fails, while failed
/// slots retain cumulative counts and are retried. No service adapter is ever
/// called under a cleaner-owned lock.
pub struct RegistrationCleaner {
    slots: Box<[Slot]>,
    progress: HashMap<OwnerToken, OwnerProgress>,
    last_report: Option<CleanupReport>,
}

impl RegistrationCleaner {
    /// Starts a fail-closed builder with every required slot marked missing.
    #[must_use]
    pub fn builder() -> RegistrationCleanerBuilder {
        RegistrationCleanerBuilder::default()
    }

    /// Runs one host phase for exactly one addon signature and generation.
    ///
    /// All still-pending adapters in the requested phase are attempted so the
    /// report exposes every failure and gap. Successfully completed adapters
    /// are skipped on retry.
    ///
    /// # Errors
    ///
    /// Returns [`CleanupFailure`] when a prior phase is incomplete, an adapter
    /// is missing, an adapter returns failure, or an adapter panics.
    pub fn cleanup_phase(
        &mut self,
        owner: OwnerToken,
        phase: CleanupPhase,
    ) -> Result<CleanupReport, CleanupFailure> {
        let slot_count = self.slots.len();
        self.progress
            .entry(owner)
            .or_insert_with(|| OwnerProgress::new(slot_count));

        if let Some(required) = self.first_incomplete_prior_phase(owner, phase) {
            let report = self.build_report(owner, phase, PhaseStatus::Blocked { required });
            self.last_report = Some(report.clone());
            return Err(CleanupFailure::new(report));
        }

        if self.phase_completed(owner, phase) {
            let report = self.build_report(owner, phase, PhaseStatus::Complete);
            self.last_report = Some(report.clone());
            return Ok(report);
        }

        self.run_pending_slots(owner, phase);
        let complete = self.mark_phase_if_complete(owner, phase);
        let status = if complete {
            PhaseStatus::Complete
        } else {
            PhaseStatus::Incomplete
        };
        let report = self.build_report(owner, phase, status);
        self.last_report = Some(report.clone());
        if complete {
            Ok(report)
        } else {
            Err(CleanupFailure::new(report))
        }
    }

    /// Returns the persistent report for an exact owner and phase without
    /// invoking any adapter.
    #[must_use]
    pub fn report(&self, owner: OwnerToken, phase: CleanupPhase) -> CleanupReport {
        if self.phase_completed(owner, phase) {
            return self.build_report(owner, phase, PhaseStatus::Complete);
        }
        if let Some(required) = self.first_incomplete_prior_phase(owner, phase) {
            return self.build_report(owner, phase, PhaseStatus::Blocked { required });
        }
        self.build_report(owner, phase, PhaseStatus::Incomplete)
    }

    /// Most recent phase report, if cleanup has been requested.
    #[must_use]
    pub const fn last_report(&self) -> Option<&CleanupReport> {
        self.last_report.as_ref()
    }

    /// Reports which required service slots are callable.
    ///
    /// This covers cleaner-owned bindings only. It does not certify the lock
    /// behavior of an external wrapper; see [`crate::INTEGRATION_GAPS`].
    #[must_use]
    pub fn coverage(&self) -> CoverageReport {
        let entries = self
            .slots
            .iter()
            .map(|slot| CoverageEntry::new(slot.service, slot.adapter.gap()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        CoverageReport::new(entries)
    }

    /// Installs or replaces one type-matched adapter.
    ///
    /// This may resolve a reported gap between retries. Existing successful
    /// owner progress remains complete; failed and pending progress remains
    /// eligible for the replacement adapter. The replaced adapter is dropped
    /// after it has been detached from the slot and while no internal lock is
    /// held.
    pub fn install<D: CleanupDomain>(&mut self, adapter: TypedAdapter<D>) {
        let replacement = adapter.erase();
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.service == D::SERVICE)
        else {
            return;
        };
        let retired = std::mem::replace(&mut slot.adapter, replacement);
        drop(retired);
    }

    /// Forgets completed bookkeeping for an addon generation.
    ///
    /// Callers should use this only after the host has released that exact
    /// generation. It does not invoke adapters or alter service registrations.
    pub fn forget_owner(&mut self, owner: OwnerToken) -> bool {
        self.progress.remove(&owner).is_some()
    }

    fn run_pending_slots(&mut self, owner: OwnerToken, phase: CleanupPhase) {
        let (slots, all_progress) = (&mut self.slots, &mut self.progress);
        let Some(owner_progress) = all_progress.get_mut(&owner) else {
            return;
        };

        for (index, slot) in slots.iter_mut().enumerate() {
            if slot.service.phase() != phase || owner_progress.slots[index].is_complete() {
                continue;
            }
            if slot.adapter.gap().is_some() {
                continue;
            }

            let (previously_removed, attempts) = owner_progress.slots[index].counters();
            let attempts = attempts.saturating_add(1);
            let outcome = catch_unwind(AssertUnwindSafe(|| slot.adapter.invoke(owner)));
            owner_progress.slots[index] = match outcome {
                Ok(Some(Ok(effect))) => ProgressState::Complete {
                    removed: previously_removed.saturating_add(effect.removed()),
                    attempts,
                },
                Ok(Some(Err(error))) => ProgressState::Failed {
                    failure: StepFailure::Adapter(error.kind()),
                    removed: previously_removed.saturating_add(error.removed()),
                    remaining: error.remaining(),
                    attempts,
                },
                Ok(None) => ProgressState::Pending,
                Err(payload) => {
                    // A panic payload may have an adversarial destructor. It is
                    // intentionally leaked at this FFI-adjacent containment
                    // boundary so dropping it cannot restart unwinding.
                    std::mem::forget(payload);
                    ProgressState::Failed {
                        failure: StepFailure::Panicked,
                        removed: previously_removed,
                        remaining: None,
                        attempts,
                    }
                }
            };
        }
    }

    fn mark_phase_if_complete(&mut self, owner: OwnerToken, phase: CleanupPhase) -> bool {
        let Some(owner_progress) = self.progress.get_mut(&owner) else {
            return false;
        };
        let complete = self.slots.iter().enumerate().all(|(index, slot)| {
            slot.service.phase() != phase || owner_progress.slots[index].is_complete()
        });
        owner_progress.completed_phases[phase_index(phase)] = complete;
        complete
    }

    fn phase_completed(&self, owner: OwnerToken, phase: CleanupPhase) -> bool {
        self.progress
            .get(&owner)
            .is_some_and(|progress| progress.completed_phases[phase_index(phase)])
    }

    fn first_incomplete_prior_phase(
        &self,
        owner: OwnerToken,
        phase: CleanupPhase,
    ) -> Option<CleanupPhase> {
        let requested = phase_index(phase);
        let progress = self.progress.get(&owner);
        (0..requested)
            .find(|index| progress.is_none_or(|progress| !progress.completed_phases[*index]))
            .map(phase_from_index)
    }

    fn build_report(
        &self,
        owner: OwnerToken,
        phase: CleanupPhase,
        status: PhaseStatus,
    ) -> CleanupReport {
        let owner_progress = self.progress.get(&owner);
        let steps = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.service.phase() == phase)
            .map(|(index, slot)| {
                let progress_status = owner_progress.map(|progress| progress.slots[index].report());
                let status = match progress_status {
                    Some(status @ StepStatus::Complete { .. }) => status,
                    _ => match slot.adapter.gap() {
                        Some(reason) => StepStatus::Gap { reason },
                        None => progress_status.unwrap_or(StepStatus::Pending),
                    },
                };
                StepReport::new(slot.service, status)
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        CleanupReport::new(owner, phase, status, steps)
    }
}

impl nexus_host::RegistrationCleaner for RegistrationCleaner {
    fn cleanup(
        &mut self,
        owner: OwnerToken,
        phase: CleanupPhase,
    ) -> Result<(), nexus_host::CleanupError> {
        self.cleanup_phase(owner, phase)
            .map(|_report| ())
            .map_err(|failure| nexus_host::CleanupError::new(failure.to_string()))
    }
}

impl fmt::Debug for RegistrationCleaner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistrationCleaner")
            .field("coverage", &self.coverage())
            .field("tracked_owner_count", &self.progress.len())
            .field("last_report", &self.last_report)
            .finish()
    }
}

/// Fail-closed builder for [`RegistrationCleaner`].
///
/// Every slot begins as [`crate::GapReason::NotConfigured`]. Builder methods
/// accept only the matching type-tagged adapter.
#[derive(Debug)]
pub struct RegistrationCleanerBuilder {
    inline_hooks: TypedAdapter<InlineHooks>,
    ui_host_callbacks: TypedAdapter<UiHostCallbacks>,
    raw_wndproc_callbacks: TypedAdapter<RawWndProcCallbacks>,
    managed_input_callbacks: TypedAdapter<ManagedInputCallbacks>,
    event_callbacks: TypedAdapter<EventCallbacks>,
    texture_callbacks: TypedAdapter<TextureCallbacks>,
    font_callbacks: TypedAdapter<FontCallbacks>,
    font_resources: TypedAdapter<FontResources>,
    localization_overrides: TypedAdapter<LocalizationOverrides>,
}

impl Default for RegistrationCleanerBuilder {
    fn default() -> Self {
        use crate::GapReason;

        Self {
            inline_hooks: TypedAdapter::gap(GapReason::NotConfigured),
            ui_host_callbacks: TypedAdapter::gap(GapReason::NotConfigured),
            raw_wndproc_callbacks: TypedAdapter::gap(GapReason::NotConfigured),
            managed_input_callbacks: TypedAdapter::gap(GapReason::NotConfigured),
            event_callbacks: TypedAdapter::gap(GapReason::NotConfigured),
            texture_callbacks: TypedAdapter::gap(GapReason::NotConfigured),
            font_callbacks: TypedAdapter::gap(GapReason::NotConfigured),
            font_resources: TypedAdapter::gap(GapReason::NotConfigured),
            localization_overrides: TypedAdapter::gap(GapReason::NotConfigured),
        }
    }
}

impl RegistrationCleanerBuilder {
    /// Binds inline-hook cleanup.
    #[must_use]
    pub fn inline_hooks(mut self, adapter: TypedAdapter<InlineHooks>) -> Self {
        self.inline_hooks = adapter;
        self
    }

    /// Binds public `UiHost` composite callback cleanup.
    #[must_use]
    pub fn ui_host_callbacks(mut self, adapter: TypedAdapter<UiHostCallbacks>) -> Self {
        self.ui_host_callbacks = adapter;
        self
    }

    /// Binds raw WndProc callback cleanup.
    #[must_use]
    pub fn raw_wndproc_callbacks(mut self, adapter: TypedAdapter<RawWndProcCallbacks>) -> Self {
        self.raw_wndproc_callbacks = adapter;
        self
    }

    /// Binds managed input callback cleanup.
    #[must_use]
    pub fn managed_input_callbacks(mut self, adapter: TypedAdapter<ManagedInputCallbacks>) -> Self {
        self.managed_input_callbacks = adapter;
        self
    }

    /// Binds event subscription cleanup.
    #[must_use]
    pub fn event_callbacks(mut self, adapter: TypedAdapter<EventCallbacks>) -> Self {
        self.event_callbacks = adapter;
        self
    }

    /// Binds texture request and callback cleanup.
    #[must_use]
    pub fn texture_callbacks(mut self, adapter: TypedAdapter<TextureCallbacks>) -> Self {
        self.texture_callbacks = adapter;
        self
    }

    /// Binds the pre-drain font callback fence.
    #[must_use]
    pub fn font_callbacks(mut self, adapter: TypedAdapter<FontCallbacks>) -> Self {
        self.font_callbacks = adapter;
        self
    }

    /// Binds post-drain font resource cleanup.
    #[must_use]
    pub fn font_resources(mut self, adapter: TypedAdapter<FontResources>) -> Self {
        self.font_resources = adapter;
        self
    }

    /// Binds synchronously acknowledged localization cleanup.
    #[must_use]
    pub fn localization_overrides(mut self, adapter: TypedAdapter<LocalizationOverrides>) -> Self {
        self.localization_overrides = adapter;
        self
    }

    /// Builds the cleaner, preserving every explicit or default gap.
    #[must_use]
    pub fn build(self) -> RegistrationCleaner {
        let slots = vec![
            Slot {
                service: CleanupService::InlineHooks,
                adapter: self.inline_hooks.erase(),
            },
            Slot {
                service: CleanupService::UiHostCallbacks,
                adapter: self.ui_host_callbacks.erase(),
            },
            Slot {
                service: CleanupService::RawWndProcCallbacks,
                adapter: self.raw_wndproc_callbacks.erase(),
            },
            Slot {
                service: CleanupService::ManagedInputCallbacks,
                adapter: self.managed_input_callbacks.erase(),
            },
            Slot {
                service: CleanupService::EventCallbacks,
                adapter: self.event_callbacks.erase(),
            },
            Slot {
                service: CleanupService::TextureCallbacks,
                adapter: self.texture_callbacks.erase(),
            },
            Slot {
                service: CleanupService::FontCallbacks,
                adapter: self.font_callbacks.erase(),
            },
            Slot {
                service: CleanupService::FontResources,
                adapter: self.font_resources.erase(),
            },
            Slot {
                service: CleanupService::LocalizationOverrides,
                adapter: self.localization_overrides.erase(),
            },
        ];
        RegistrationCleaner {
            slots: slots.into_boxed_slice(),
            progress: HashMap::new(),
            last_report: None,
        }
    }
}
