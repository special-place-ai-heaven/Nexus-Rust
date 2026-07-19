//! Per-swap-chain resource generations and lifecycle telemetry.

use std::collections::BTreeMap;

use crate::{FailureController, FailurePolicy, RenderStage, SwapChainId, SwapChainObservation};

/// Monotonic render-resource generation for one swap-chain identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionGeneration(u64);

impl SessionGeneration {
    /// First resource generation assigned to a newly observed swap chain.
    pub const FIRST: Self = Self(1);

    /// Returns the one-based generation number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// Whether a known swap chain is eligible, selected, or gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLifecycle {
    /// Observed and renderable, but not currently selected.
    Candidate,
    /// Selected as the primary game swap chain.
    Primary,
    /// Missing from the latest complete observation snapshot.
    Retired,
}

/// Native-property change requiring render resources to be recreated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationChange {
    /// A previously retired identity appeared again.
    Reactivated,
    /// The output window changed.
    Window,
    /// The graphics device changed.
    Device,
    /// The graphics adapter changed.
    Adapter,
    /// Back-buffer dimensions changed.
    Size,
    /// Back-buffer pixel format changed.
    Format,
    /// Output color space changed, including SDR/HDR transitions.
    ColorSpace,
}

/// One typed lifecycle event suitable for diagnostics and telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEventKind {
    /// A native identity was observed for the first time.
    Discovered,
    /// Render resources must be recreated for the listed reasons.
    GenerationAdvanced {
        /// All changes found in the same atomic observation update.
        changes: Vec<GenerationChange>,
    },
    /// This identity became the primary game swap chain.
    BecamePrimary,
    /// This identity remains observed but is no longer primary.
    BecameCandidate,
    /// This identity disappeared from the latest complete snapshot.
    Retired,
}

/// Event tagged with the affected identity and resulting generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEvent {
    id: SwapChainId,
    generation: SessionGeneration,
    kind: SessionEventKind,
}

impl SessionEvent {
    /// Returns the affected swap-chain identity.
    #[must_use]
    pub const fn id(&self) -> SwapChainId {
        self.id
    }

    /// Returns the generation after applying the event.
    #[must_use]
    pub const fn generation(&self) -> SessionGeneration {
        self.generation
    }

    /// Returns the typed event payload.
    #[must_use]
    pub const fn kind(&self) -> &SessionEventKind {
        &self.kind
    }
}

/// All policy state owned by one observed native swap chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapChainSession {
    observation: SwapChainObservation,
    generation: SessionGeneration,
    lifecycle: SessionLifecycle,
    failures: FailureController,
    latest_outcome_sequences: BTreeMap<RenderStage, u64>,
}

impl SwapChainSession {
    /// Returns the stable native-object identity.
    #[must_use]
    pub const fn id(&self) -> SwapChainId {
        self.observation.id
    }

    /// Returns the current render-resource generation.
    #[must_use]
    pub const fn generation(&self) -> SessionGeneration {
        self.generation
    }

    /// Returns whether this session is candidate, primary, or retired.
    #[must_use]
    pub const fn lifecycle(&self) -> SessionLifecycle {
        self.lifecycle
    }

    /// Returns the most recent complete native observation.
    #[must_use]
    pub const fn observation(&self) -> &SwapChainObservation {
        &self.observation
    }

    /// Returns diagnostic failure state for this generation.
    #[must_use]
    pub const fn failures(&self) -> &FailureController {
        &self.failures
    }

    /// Returns mutable failure state for recording isolated stage outcomes.
    pub const fn failures_mut(&mut self) -> &mut FailureController {
        &mut self.failures
    }

    /// Applies a stage outcome only when it is newer than every prior outcome
    /// observed for that stage in the current generation.
    ///
    /// Returns `false` for stale or duplicate completions without changing the
    /// stage's failure state. A generation advance resets the ordering history.
    pub fn record_render_outcome(
        &mut self,
        stage: RenderStage,
        sequence: u64,
        succeeded: bool,
    ) -> bool {
        if self
            .latest_outcome_sequences
            .get(&stage)
            .is_some_and(|latest| sequence <= *latest)
        {
            return false;
        }

        self.latest_outcome_sequences.insert(stage, sequence);
        if succeeded {
            let _ = self.failures.record_success(stage);
        } else {
            let _ = self.failures.record_failure(stage, sequence);
        }
        true
    }
}

/// Deterministic registry of all observed swap-chain sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapChainRegistry {
    sessions: BTreeMap<SwapChainId, SwapChainSession>,
    failure_policy: FailurePolicy,
}

impl SwapChainRegistry {
    /// Creates an empty registry whose generations use the supplied failure policy.
    #[must_use]
    pub const fn new(failure_policy: FailurePolicy) -> Self {
        Self {
            sessions: BTreeMap::new(),
            failure_policy,
        }
    }

    /// Returns a session by stable native-object identity.
    #[must_use]
    pub fn get(&self, id: SwapChainId) -> Option<&SwapChainSession> {
        self.sessions.get(&id)
    }

    /// Returns a mutable session by stable native-object identity.
    pub fn get_mut(&mut self, id: SwapChainId) -> Option<&mut SwapChainSession> {
        self.sessions.get_mut(&id)
    }

    /// Iterates sessions in stable identity order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &SwapChainSession> {
        self.sessions.values()
    }

    /// Applies one complete observation snapshot and selected primary identity.
    ///
    /// Surface-affecting property changes advance exactly one generation and
    /// reset only that session's stage failure budgets. Presentation method and
    /// activity changes update telemetry without recreating resources.
    pub fn reconcile(
        &mut self,
        observations: &[SwapChainObservation],
        primary: Option<SwapChainId>,
    ) -> Vec<SessionEvent> {
        let snapshot: BTreeMap<_, _> = observations
            .iter()
            .map(|observation| (observation.id, observation))
            .collect();
        let mut events = Vec::new();

        for (&id, observation) in &snapshot {
            let target_lifecycle = if primary == Some(id) {
                SessionLifecycle::Primary
            } else {
                SessionLifecycle::Candidate
            };

            match self.sessions.get_mut(&id) {
                Some(session) => {
                    let changes = generation_changes(session, observation);
                    if !changes.is_empty() {
                        session.generation = session.generation.next();
                        session.failures = FailureController::new(self.failure_policy);
                        session.latest_outcome_sequences.clear();
                        events.push(SessionEvent {
                            id,
                            generation: session.generation,
                            kind: SessionEventKind::GenerationAdvanced { changes },
                        });
                    }
                    session.observation = (*observation).clone();

                    if session.lifecycle != target_lifecycle {
                        session.lifecycle = target_lifecycle;
                        events.push(SessionEvent {
                            id,
                            generation: session.generation,
                            kind: match target_lifecycle {
                                SessionLifecycle::Primary => SessionEventKind::BecamePrimary,
                                SessionLifecycle::Candidate => SessionEventKind::BecameCandidate,
                                SessionLifecycle::Retired => SessionEventKind::Retired,
                            },
                        });
                    }
                }
                None => {
                    let generation = SessionGeneration::FIRST;
                    self.sessions.insert(
                        id,
                        SwapChainSession {
                            observation: (*observation).clone(),
                            generation,
                            lifecycle: target_lifecycle,
                            failures: FailureController::new(self.failure_policy),
                            latest_outcome_sequences: BTreeMap::new(),
                        },
                    );
                    events.push(SessionEvent {
                        id,
                        generation,
                        kind: SessionEventKind::Discovered,
                    });
                    if target_lifecycle == SessionLifecycle::Primary {
                        events.push(SessionEvent {
                            id,
                            generation,
                            kind: SessionEventKind::BecamePrimary,
                        });
                    }
                }
            }
        }

        for (&id, session) in &mut self.sessions {
            if !snapshot.contains_key(&id) && session.lifecycle != SessionLifecycle::Retired {
                session.lifecycle = SessionLifecycle::Retired;
                events.push(SessionEvent {
                    id,
                    generation: session.generation,
                    kind: SessionEventKind::Retired,
                });
            }
        }

        events
    }
}

impl Default for SwapChainRegistry {
    fn default() -> Self {
        Self::new(FailurePolicy::default())
    }
}

fn generation_changes(
    session: &SwapChainSession,
    observation: &SwapChainObservation,
) -> Vec<GenerationChange> {
    let previous = &session.observation;
    let mut changes = Vec::new();
    if session.lifecycle == SessionLifecycle::Retired {
        changes.push(GenerationChange::Reactivated);
    }
    if previous.hwnd != observation.hwnd {
        changes.push(GenerationChange::Window);
    }
    if previous.device != observation.device {
        changes.push(GenerationChange::Device);
    }
    if previous.adapter_luid != observation.adapter_luid {
        changes.push(GenerationChange::Adapter);
    }
    if previous.size != observation.size {
        changes.push(GenerationChange::Size);
    }
    if previous.format != observation.format {
        changes.push(GenerationChange::Format);
    }
    if previous.color_space != observation.color_space {
        changes.push(GenerationChange::ColorSpace);
    }
    changes
}

#[cfg(test)]
mod tests {
    use crate::{
        Activity, AdapterLuid, ColorSpace, DeviceId, Extent2D, PresentMethod, RenderStage,
        SurfaceFormat, SwapChainId, SwapChainObservation,
    };

    use super::{
        GenerationChange, SessionEventKind, SessionGeneration, SessionLifecycle, SwapChainRegistry,
    };

    fn observation(id: u64) -> SwapChainObservation {
        SwapChainObservation {
            id: SwapChainId::new(id),
            hwnd: None,
            device: DeviceId::new(1),
            adapter_luid: AdapterLuid::new(2, 0),
            format: SurfaceFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            size: Extent2D::new(1920, 1080),
            present_method: PresentMethod::Present,
            activity: Activity::active(1, 1, 1),
        }
    }

    #[test]
    fn resize_advances_generation_and_resets_failure_state() {
        let mut registry = SwapChainRegistry::default();
        let initial = observation(1);
        let _ = registry.reconcile(std::slice::from_ref(&initial), Some(initial.id));
        let session = registry
            .get_mut(initial.id)
            .expect("session was discovered");
        let _ = session
            .failures_mut()
            .record_failure(RenderStage::CoreUi, 1);

        let mut resized = initial;
        resized.size = Extent2D::new(2560, 1440);
        let events = registry.reconcile(std::slice::from_ref(&resized), Some(resized.id));

        let session = registry.get(resized.id).expect("session remains present");
        assert_eq!(session.generation(), SessionGeneration(2));
        assert!(matches!(
            session.failures().state(RenderStage::CoreUi),
            crate::StageFailureState::Healthy {
                consecutive_failures: 0,
                cooldowns: 0
            }
        ));
        assert!(events.iter().any(|event| {
            matches!(
                event.kind(),
                SessionEventKind::GenerationAdvanced { changes }
                    if changes == &[GenerationChange::Size]
            )
        }));
    }

    #[test]
    fn late_older_success_cannot_erase_newer_failure() {
        let mut registry = SwapChainRegistry::default();
        let initial = observation(1);
        let _ = registry.reconcile(std::slice::from_ref(&initial), Some(initial.id));
        let session = registry
            .get_mut(initial.id)
            .expect("session was discovered");

        assert!(session.record_render_outcome(RenderStage::CoreUi, 2, false));
        assert!(!session.record_render_outcome(RenderStage::CoreUi, 1, true));
        assert!(!session.record_render_outcome(RenderStage::CoreUi, 2, true));
        assert!(matches!(
            session.failures().state(RenderStage::CoreUi),
            crate::StageFailureState::Healthy {
                consecutive_failures: 1,
                cooldowns: 0
            }
        ));
    }

    #[test]
    fn generation_advance_resets_render_outcome_ordering() {
        let mut registry = SwapChainRegistry::default();
        let initial = observation(1);
        let _ = registry.reconcile(std::slice::from_ref(&initial), Some(initial.id));
        let session = registry
            .get_mut(initial.id)
            .expect("session was discovered");
        assert!(session.record_render_outcome(RenderStage::CoreUi, 100, false));

        let mut resized = initial;
        resized.size = Extent2D::new(2560, 1440);
        let _ = registry.reconcile(std::slice::from_ref(&resized), Some(resized.id));

        let session = registry
            .get_mut(resized.id)
            .expect("resized session remains present");
        assert!(session.record_render_outcome(RenderStage::CoreUi, 1, false));
        assert!(matches!(
            session.failures().state(RenderStage::CoreUi),
            crate::StageFailureState::Healthy {
                consecutive_failures: 1,
                cooldowns: 0
            }
        ));
    }

    #[test]
    fn recreation_retires_old_identity_and_starts_new_generation_at_one() {
        let mut registry = SwapChainRegistry::default();
        let old = observation(1);
        let replacement = observation(2);
        let _ = registry.reconcile(std::slice::from_ref(&old), Some(old.id));

        let events = registry.reconcile(std::slice::from_ref(&replacement), Some(replacement.id));

        assert_eq!(
            registry
                .get(old.id)
                .expect("old session is retained for telemetry")
                .lifecycle(),
            SessionLifecycle::Retired
        );
        assert_eq!(
            registry
                .get(replacement.id)
                .expect("replacement was discovered")
                .generation(),
            SessionGeneration(1)
        );
        assert!(events.iter().any(|event| {
            event.id() == old.id && matches!(event.kind(), SessionEventKind::Retired)
        }));
    }

    #[test]
    fn hdr_transition_advances_generation_with_format_and_color_reasons() {
        let mut registry = SwapChainRegistry::default();
        let sdr = observation(1);
        let _ = registry.reconcile(std::slice::from_ref(&sdr), Some(sdr.id));

        let mut hdr = sdr;
        hdr.format = SurfaceFormat::Rgb10A2Unorm;
        hdr.color_space = ColorSpace::Hdr10Pq;
        let events = registry.reconcile(std::slice::from_ref(&hdr), Some(hdr.id));

        assert!(
            registry
                .get(hdr.id)
                .expect("HDR session exists")
                .observation()
                .is_hdr_output()
        );
        assert!(events.iter().any(|event| {
            matches!(
                event.kind(),
                SessionEventKind::GenerationAdvanced { changes }
                    if changes == &[GenerationChange::Format, GenerationChange::ColorSpace]
            )
        }));
    }

    #[test]
    fn switching_between_present_and_present1_does_not_recreate_resources() {
        let mut registry = SwapChainRegistry::default();
        let first = observation(1);
        let _ = registry.reconcile(std::slice::from_ref(&first), Some(first.id));

        let mut present1 = first;
        present1.present_method = PresentMethod::Present1;
        present1.activity.last_present_sequence = 2;
        let events = registry.reconcile(std::slice::from_ref(&present1), Some(present1.id));

        assert_eq!(
            registry
                .get(present1.id)
                .expect("session exists")
                .generation(),
            SessionGeneration(1)
        );
        assert!(
            !events.iter().any(|event| {
                matches!(event.kind(), SessionEventKind::GenerationAdvanced { .. })
            })
        );
    }
}
