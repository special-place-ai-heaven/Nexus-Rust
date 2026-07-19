use std::{
    collections::BTreeMap,
    ffi::c_void,
    ptr::NonNull,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use nexus_control::{
    DiagnosticEvent, FailureCode, InternalFailure, RenderOperation, SwapChainId as DiagnosticId,
    SwapChainOperation,
};
use nexus_core::CallbackGate;
use nexus_render::{
    Activity, AttemptPermission, ClassifierConfig, ColorSpace, DeviceId, FailurePolicy, Hwnd,
    PresentMethod, PrimarySwapChainClassifier, RenderControls, RenderStage, SelectionReason,
    SessionGeneration, SessionLifecycle, StageFailureState, SwapChainId, SwapChainObservation,
    SwapChainRegistry,
};
use windows_sys::core::GUID;

use crate::{
    AttachOutcome, Boundary, DxgiCallbacks, DxgiError, DxgiObservationEvent, HResultDisposition,
    ObjectKind, ObservationField, OverlayRenderer, PresentFrame, RenderCallbackError,
    RenderFailurePolicySuppression, RenderSelectionDeferred, RenderSelectionResolved, ResizeFrame,
    ShutdownReport, SwapChainInterface, SwapChainRetirement,
    detours::{self, HookGuard},
    sdk::{self, UNKNOWN_COLOR_SPACE},
};

static NEXT_MANAGER_ID: AtomicU64 = AtomicU64::new(1);

/// Configuration for swap-chain classification and staged rendering.
#[derive(Clone, Debug)]
pub struct DxgiConfig {
    render_controls: RenderControls,
    classifier: ClassifierConfig,
    failure_policy: FailurePolicy,
}

impl DxgiConfig {
    /// Creates a manager configuration from existing policy types.
    #[must_use]
    pub const fn new(
        render_controls: RenderControls,
        classifier: ClassifierConfig,
        failure_policy: FailurePolicy,
    ) -> Self {
        Self {
            render_controls,
            classifier,
            failure_policy,
        }
    }

    /// Returns the initial staged-render controls.
    #[must_use]
    pub const fn render_controls(&self) -> RenderControls {
        self.render_controls
    }

    /// Returns the initial classifier configuration.
    #[must_use]
    pub const fn classifier(&self) -> &ClassifierConfig {
        &self.classifier
    }

    /// Returns the per-generation failure policy.
    #[must_use]
    pub const fn failure_policy(&self) -> FailurePolicy {
        self.failure_policy
    }
}

impl Default for DxgiConfig {
    fn default() -> Self {
        Self::new(
            RenderControls::default(),
            ClassifierConfig::default(),
            FailurePolicy::default(),
        )
    }
}

/// Owner of every per-instance DXGI hook and its policy state.
///
/// Clone values share one manager. The runtime must retain at least one clone
/// until native DXGI dispatch has quiesced; in normal use this is a process-
/// lifetime runtime service. [`close_and_drain`](Self::close_and_drain)
/// restores vtable pointers but intentionally keeps the restored shadow guards
/// allocated so a thread that cached the old vtable immediately before restore
/// cannot observe freed table storage.
#[derive(Clone)]
pub struct DxgiInterceptionManager {
    inner: Arc<Inner>,
}

impl DxgiInterceptionManager {
    /// Creates a closed-payload observer and optional synchronous renderer integration.
    #[must_use]
    pub fn new(
        config: DxgiConfig,
        callbacks: Arc<dyn DxgiCallbacks>,
        renderer: Option<Arc<dyn OverlayRenderer>>,
    ) -> Self {
        let id = NEXT_MANAGER_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::new(Inner {
                id,
                callbacks,
                renderer,
                gate: CallbackGate::open(),
                closing: AtomicBool::new(false),
                hooks: Mutex::new(Vec::new()),
                render_lane: Arc::new(RenderLane::default()),
                pending_retirements: Mutex::new(BTreeMap::new()),
                policy: Mutex::new(PolicyState::new(config)),
            }),
        }
    }

    /// Attaches the concrete factory returned by a successful proxy export.
    ///
    /// The manager probes the highest SDK interface supported by the object so
    /// a base pointer that shares storage with a derived interface is shadowed
    /// with the complete derived layout from the start.
    ///
    /// # Errors
    ///
    /// Returns an error for a null pointer, unsupported IID, closed manager,
    /// hook conflict, or invalid vtable.
    ///
    /// # Safety
    ///
    /// `factory` must be a live owned-or-borrowed COM reference implementing
    /// `iid`; its first word must remain writable while the manager is active.
    pub unsafe fn attach_factory(
        &self,
        factory: *mut c_void,
        iid: &GUID,
    ) -> Result<AttachOutcome, DxgiError> {
        // SAFETY: the public contract is forwarded unchanged to the typed hook installer.
        unsafe { detours::attach_factory(&self.inner, factory, iid) }
    }

    /// Attaches a concrete swap chain returned by D3D11 or a factory method.
    ///
    /// # Errors
    ///
    /// Returns an error for a null pointer, unsupported IID, closed manager,
    /// hook conflict, or invalid vtable.
    ///
    /// # Safety
    ///
    /// `swap_chain` must be a live owned-or-borrowed COM reference implementing
    /// `iid`; its first word must remain writable while the manager is active.
    pub unsafe fn attach_swap_chain(
        &self,
        swap_chain: *mut c_void,
        iid: &GUID,
    ) -> Result<AttachOutcome, DxgiError> {
        // SAFETY: the public contract is forwarded unchanged to the typed hook installer.
        unsafe { detours::attach_swap_chain(&self.inner, swap_chain, iid) }
    }

    /// Atomically changes the stage and safe-mode policy used by future frames.
    pub fn set_render_controls(&self, controls: RenderControls) {
        lock(&self.inner.policy).render_controls = controls;
    }

    /// Returns a snapshot of the current stage and safe-mode policy.
    #[must_use]
    pub fn render_controls(&self) -> RenderControls {
        lock(&self.inner.policy).render_controls
    }

    /// Changes the game-window identity used by future classifications.
    ///
    /// `SwapChainAttached` and `PresentForwarded` observations are emitted only
    /// after the manager releases its policy lock, so those callbacks may use
    /// this setter to publish late window discovery without lock recursion.
    pub fn set_expected_game_window(&self, window: Option<Hwnd>) {
        let mut policy = lock(&self.inner.policy);
        let mut config = policy.classifier.config();
        if config.expected_game_window() == window {
            return;
        }
        config.set_expected_game_window(window);
        policy.classifier.set_config(config);
    }

    /// Closes callback admission, restores every owned vtable, and waits for drain.
    ///
    /// The operation is idempotent. Native forwarding remains available to a
    /// callback already holding a cached detour address.
    #[must_use]
    pub fn close_and_drain(&self, timeout: Duration) -> ShutdownReport {
        self.inner.closing.store(true, Ordering::Release);
        self.inner.gate.close();

        let (restored, displaced) = {
            let mut hooks = lock(&self.inner.hooks);
            let mut restored = 0_u32;
            let mut displaced = 0_u32;
            for hook in &mut *hooks {
                match hook.restore() {
                    Ok(changed) => restored = restored.saturating_add(u32::from(changed)),
                    Err(_) => displaced = displaced.saturating_add(1),
                }
            }
            (restored, displaced)
        };

        let drained = self.inner.gate.wait_for_drain(timeout);
        let report = ShutdownReport {
            drained,
            restored,
            displaced,
            in_flight: self.inner.gate.in_flight(),
        };
        self.inner.emit_observation(DxgiObservationEvent::Shutdown {
            drained,
            restored,
            displaced,
        });
        report
    }
}

pub(crate) struct Inner {
    pub(crate) id: u64,
    callbacks: Arc<dyn DxgiCallbacks>,
    renderer: Option<Arc<dyn OverlayRenderer>>,
    pub(crate) gate: CallbackGate,
    pub(crate) closing: AtomicBool,
    pub(crate) hooks: Mutex<Vec<HookGuard>>,
    render_lane: Arc<RenderLane>,
    pending_retirements: Mutex<BTreeMap<(SwapChainId, SessionGeneration), u64>>,
    policy: Mutex<PolicyState>,
}

#[derive(Default)]
struct RenderLane {
    occupied: AtomicBool,
}

impl RenderLane {
    fn try_enter(self: &Arc<Self>) -> Option<RenderLaneLease> {
        self.occupied
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
            .then(|| RenderLaneLease {
                lane: Arc::clone(self),
            })
    }
}

struct RenderLaneLease {
    lane: Arc<RenderLane>,
}

impl Drop for RenderLaneLease {
    fn drop(&mut self) {
        self.lane.occupied.store(false, Ordering::Release);
    }
}

impl Inner {
    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    pub(crate) fn emit_observation(&self, event: DxgiObservationEvent) {
        let callback = Arc::clone(&self.callbacks);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback.observation(event);
        }));
    }

    pub(crate) fn emit_diagnostic(&self, event: DiagnosticEvent) {
        let callback = Arc::clone(&self.callbacks);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback.diagnostic(event);
        }));
    }

    fn emit_render_selection_transition(
        &self,
        sequence: u64,
        resolved: Option<RenderSelectionResolved>,
        deferred: Option<RenderSelectionDeferred>,
    ) {
        if let Some(resolution) = resolved {
            self.emit_observation(DxgiObservationEvent::RenderSelectionResolved {
                sequence,
                resolution,
            });
        } else if let Some(diagnostic) = deferred {
            self.emit_observation(DxgiObservationEvent::RenderSelectionDeferred {
                sequence,
                diagnostic,
            });
        }
    }

    pub(crate) fn report_panic(&self, boundary: Boundary) {
        self.emit_observation(DxgiObservationEvent::PanicContained { boundary });
    }

    pub(crate) fn report_attach_error(
        &self,
        kind: ObjectKind,
        swap_chain: Option<SwapChainId>,
        error: &DxgiError,
    ) {
        let code = match error {
            DxgiError::UnsupportedInterface => {
                FailureCode::Internal(InternalFailure::UnsupportedInterface)
            }
            DxgiError::HookConflict => FailureCode::Internal(InternalFailure::HookConflict),
            DxgiError::NullInterface | DxgiError::ManagerClosed | DxgiError::Vtable(_) => {
                FailureCode::Internal(InternalFailure::InvalidState)
            }
        };
        if kind == ObjectKind::SwapChain {
            self.emit_diagnostic(DiagnosticEvent::SwapChainFailure {
                swap_chain: swap_chain.map(diagnostic_id),
                operation: SwapChainOperation::AttachHook,
                code,
            });
        }
    }

    pub(crate) fn track_swap_chain(
        &self,
        pointer: *mut c_void,
        interface: SwapChainInterface,
    ) -> SwapChainId {
        let key = pointer as usize;
        let mut policy = lock(&self.policy);
        if let Some(existing) = policy.tracked.get(&key) {
            return existing.id;
        }
        let id = SwapChainId::new(policy.next_swap_chain_id);
        policy.next_swap_chain_id = policy.next_swap_chain_id.saturating_add(1);
        policy.tracked.insert(
            key,
            TrackedSwapChain {
                id,
                interface,
                activity: Activity::default(),
                observation: None,
                color_space: ColorSpace::Other(UNKNOWN_COLOR_SPACE),
                color_space_reported: false,
            },
        );
        drop(policy);
        self.emit_observation(DxgiObservationEvent::SwapChainAttached {
            swap_chain: id,
            interface,
        });
        id
    }

    pub(crate) fn before_present(
        &self,
        pointer: *mut c_void,
        method: PresentMethod,
    ) -> Option<PresentInvocation> {
        let render_lane = self.try_render_lane();
        let key = pointer as usize;
        let state = {
            let mut policy = lock(&self.policy);
            policy.sequence = policy.sequence.saturating_add(1);
            let sequence = policy.sequence;
            policy.tracked.get_mut(&key).map(|tracked| {
                tracked.activity.present_count = tracked.activity.present_count.saturating_add(1);
                tracked.activity.last_present_sequence = sequence;
                tracked.activity.consecutive_present_cycles = tracked
                    .activity
                    .consecutive_present_cycles
                    .saturating_add(1);
                let report_unknown = tracked.color_space == ColorSpace::Other(UNKNOWN_COLOR_SPACE)
                    && !tracked.color_space_reported;
                if report_unknown {
                    tracked.color_space_reported = true;
                }
                (tracked.id, tracked.interface, sequence, report_unknown)
            })
        };
        let Some((id, interface, sequence, report_unknown_color)) = state else {
            self.finish_render_lane(render_lane);
            return None;
        };
        let invocation = PresentInvocation {
            id,
            sequence,
            render_lane,
        };

        if report_unknown_color {
            self.emit_observation(DxgiObservationEvent::MetadataIncomplete {
                swap_chain: id,
                field: ObservationField::ColorSpace,
            });
        }

        // Native Present must remain nonblocking during reentrancy or
        // cross-thread contention. Keep its exact result token, but skip
        // inspection and overlay rendering unless this call owns the lane.
        if invocation.render_lane.is_none() {
            return Some(invocation);
        }

        // SAFETY: the hook guard holds a COM reference for this concrete pointer.
        let metadata = match unsafe { sdk::inspect_swap_chain(pointer, interface) } {
            Ok(metadata) => metadata,
            Err(result) => {
                self.emit_diagnostic(DiagnosticEvent::SwapChainFailure {
                    swap_chain: Some(diagnostic_id(id)),
                    operation: SwapChainOperation::Inspect,
                    code: FailureCode::HResult(result),
                });
                self.handle_inspection_failure(key, id, sequence);
                return Some(invocation);
            }
        };

        let (transition_sequence, transition, retirement) = {
            let mut policy = lock(&self.policy);
            if !policy.is_current_present(key, id, sequence) {
                return Some(invocation);
            }
            let device = policy.device_id(metadata.device_identity);
            let Some(tracked) = policy.tracked.get_mut(&key) else {
                return Some(invocation);
            };
            tracked.activity.window_visible = metadata.window_visible;
            tracked.activity.foreground = metadata.foreground;
            let color_space = tracked.color_space;
            let observation = SwapChainObservation {
                id,
                hwnd: metadata.hwnd.map(Hwnd::new),
                device,
                adapter_luid: metadata.adapter_luid,
                format: metadata.format,
                color_space,
                size: metadata.size,
                present_method: method,
                activity: tracked.activity,
            };
            tracked.observation = Some(observation);

            let policy_horizon = policy.sequence;
            let transition = policy.reconcile_render_selection(Some(id), policy_horizon);
            let transition_sequence = policy.next_selection_sequence();
            let retirement = transition
                .render
                .is_none()
                .then(|| policy.swap_chain_retirement(id, sequence))
                .flatten();
            (transition_sequence, transition, retirement)
        };

        self.emit_render_selection_transition(
            transition_sequence,
            transition.resolved,
            transition.deferred,
        );
        if let Some((generation, stage, reason)) = transition.render {
            if let Some(pointer) = NonNull::new(pointer) {
                self.render_selected_in_lane(
                    pointer,
                    RenderTicket {
                        key,
                        id,
                        method,
                        sequence,
                        generation,
                        stage,
                        reason,
                        transition_sequence,
                    },
                );
            }
        } else if let Some(retirement) = retirement {
            self.retire_renderer_swap_chain(retirement);
        }

        self.drain_deferred_retirements_in_lane();

        Some(invocation)
    }

    #[cfg(test)]
    fn try_render_selected(&self, pointer: NonNull<c_void>, ticket: RenderTicket) {
        let Some(render_lane) = self.try_render_lane() else {
            return;
        };
        self.render_selected_in_lane(pointer, ticket);
        self.finish_render_lane(Some(render_lane));
    }

    fn render_selected_in_lane(&self, pointer: NonNull<c_void>, ticket: RenderTicket) {
        if !lock(&self.policy).permits_render_ticket(ticket) {
            return;
        }

        self.render(PresentFrame::new(
            pointer,
            generation_id(ticket.id),
            ticket.method,
            ticket.sequence,
            ticket.generation,
            ticket.stage,
        ));
        if !lock(&self.policy).permits_render_ticket(ticket) {
            return;
        }
        self.emit_observation(DxgiObservationEvent::RenderSelected {
            sequence: ticket.transition_sequence,
            swap_chain: ticket.id,
            generation: ticket.generation,
            stage: ticket.stage,
            reason: ticket.reason,
        });
    }

    fn try_render_lane(&self) -> Option<RenderLaneLease> {
        let lease = self.render_lane.try_enter()?;
        self.drain_deferred_retirements_in_lane();
        Some(lease)
    }

    fn retire_or_defer(&self, retirement: SwapChainRetirement, owns_render_lane: bool) {
        if owns_render_lane {
            self.retire_renderer_swap_chain(retirement);
            return;
        }
        {
            let mut pending = lock(&self.pending_retirements);
            pending
                .entry((retirement.id(), retirement.generation()))
                .and_modify(|sequence| *sequence = (*sequence).max(retirement.sequence()))
                .or_insert(retirement.sequence());
        }
        self.drain_deferred_retirements_if_available();
    }

    fn drain_deferred_retirements_in_lane(&self) {
        loop {
            let pending = {
                let mut pending = lock(&self.pending_retirements);
                if pending.is_empty() {
                    return;
                }
                std::mem::take(&mut *pending)
            };
            for ((id, generation), sequence) in pending {
                self.retire_renderer_swap_chain(SwapChainRetirement::new(id, generation, sequence));
            }
        }
    }

    fn drain_deferred_retirements_if_available(&self) {
        loop {
            let Some(lease) = self.render_lane.try_enter() else {
                return;
            };
            self.drain_deferred_retirements_in_lane();
            drop(lease);
            if lock(&self.pending_retirements).is_empty() {
                return;
            }
        }
    }

    fn finish_render_lane(&self, render_lane: Option<RenderLaneLease>) {
        let Some(render_lane) = render_lane else {
            return;
        };
        self.drain_deferred_retirements_in_lane();
        drop(render_lane);
        // Close the enqueue-vs-release race: either this call reacquires and
        // drains work queued just before release, or the enqueuer acquires and
        // drains it itself.
        self.drain_deferred_retirements_if_available();
    }

    fn handle_inspection_failure(&self, key: usize, id: SwapChainId, sequence: u64) {
        let (transition_sequence, transition, retirement) = {
            let mut policy = lock(&self.policy);
            if !policy.invalidate_observation(key, id, sequence) {
                return;
            }
            let render_sequence = policy.sequence;
            let transition = policy.reconcile_render_selection(None, render_sequence);
            let transition_sequence = policy.next_selection_sequence();
            let retirement = policy.swap_chain_retirement(id, sequence);
            (transition_sequence, transition, retirement)
        };
        self.emit_render_selection_transition(
            transition_sequence,
            transition.resolved,
            transition.deferred,
        );
        if let Some(retirement) = retirement {
            self.retire_renderer_swap_chain(retirement);
        }
    }

    pub(crate) fn after_present(
        &self,
        pointer: *mut c_void,
        method: PresentMethod,
        invocation: Option<PresentInvocation>,
        result: i32,
    ) {
        let Some(invocation) = invocation else {
            return;
        };
        let PresentInvocation {
            id,
            sequence,
            render_lane,
        } = invocation;
        let render_lane = render_lane.or_else(|| self.try_render_lane());
        let disposition = sdk::hresult_disposition(result);
        let selection_transition = {
            let mut policy = lock(&self.policy);
            let key = pointer as usize;
            if !policy.is_current_present(key, id, sequence) {
                None
            } else {
                let device_lost = matches!(
                    disposition,
                    HResultDisposition::DeviceRemoved | HResultDisposition::DeviceReset
                );
                let should_reconcile = if let Some(tracked) = policy.tracked.get_mut(&key) {
                    let was_occluded = tracked.activity.occluded;
                    tracked.activity.occluded = disposition == HResultDisposition::Occluded;
                    if device_lost {
                        tracked.observation = None;
                        true
                    } else if let Some(observation) = &mut tracked.observation {
                        observation.activity.occluded = tracked.activity.occluded;
                        was_occluded != tracked.activity.occluded
                    } else {
                        false
                    }
                } else {
                    false
                };
                if should_reconcile {
                    let render_sequence = policy.sequence;
                    let transition = policy.reconcile_render_selection(None, render_sequence);
                    let retirement = (policy.classifier.current() != Some(id))
                        .then(|| policy.swap_chain_retirement(id, sequence))
                        .flatten();
                    let transition_sequence = policy.next_selection_sequence();
                    Some((transition_sequence, transition, retirement))
                } else {
                    None
                }
            }
        };
        self.emit_observation(DxgiObservationEvent::PresentForwarded {
            swap_chain: id,
            method,
            sequence,
            result: disposition,
        });
        if matches!(
            disposition,
            HResultDisposition::DeviceRemoved | HResultDisposition::DeviceReset
        ) {
            self.emit_diagnostic(DiagnosticEvent::SwapChainFailure {
                swap_chain: Some(diagnostic_id(id)),
                operation: SwapChainOperation::Present,
                code: FailureCode::Internal(InternalFailure::DeviceLost),
            });
        }
        if let Some((transition_sequence, transition, retirement)) = selection_transition {
            self.emit_render_selection_transition(
                transition_sequence,
                transition.resolved,
                transition.deferred,
            );
            if let Some(retirement) = retirement {
                self.retire_or_defer(retirement, render_lane.is_some());
            }
        }
        self.finish_render_lane(render_lane);
    }

    pub(crate) fn before_resize(
        &self,
        pointer: *mut c_void,
        width: u32,
        height: u32,
        format: i32,
    ) -> Option<ResizeInvocation> {
        let render_lane = self.try_render_lane();
        let state = lock(&self.policy)
            .tracked
            .get(&(pointer as usize))
            .map(|tracked| (tracked.id, tracked.activity.last_present_sequence));
        let Some((id, present_sequence)) = state else {
            self.finish_render_lane(render_lane);
            return None;
        };
        let mut invocation = ResizeInvocation {
            id,
            present_sequence,
            requested_size: nexus_render::Extent2D::new(width, height),
            requested_format: sdk::raw_surface_format(format),
            render_lane,
            renderer_prepared: false,
        };
        if invocation.render_lane.is_some()
            && let (Some(renderer), Some(pointer)) = (&self.renderer, NonNull::new(pointer))
        {
            let frame = ResizeFrame::new(
                pointer,
                generation_id(id),
                invocation.requested_size,
                invocation.requested_format,
            );
            invocation.renderer_prepared = self.invoke_renderer(
                || renderer.before_resize(&frame),
                id,
                Boundary::RendererCallback,
            );
        }
        if invocation.render_lane.is_some() {
            self.drain_deferred_retirements_in_lane();
        }
        Some(invocation)
    }

    pub(crate) fn after_resize(
        &self,
        pointer: *mut c_void,
        invocation: Option<ResizeInvocation>,
        result: i32,
    ) {
        let Some(invocation) = invocation else {
            return;
        };
        let ResizeInvocation {
            id,
            present_sequence,
            requested_size,
            requested_format,
            render_lane,
            renderer_prepared,
        } = invocation;
        let render_lane = render_lane.or_else(|| self.try_render_lane());
        let disposition = sdk::hresult_disposition(result);
        let resize_transition = if result >= 0 {
            let state = {
                let mut policy = lock(&self.policy);
                let retirement_sequence = match policy.tracked.get_mut(&(pointer as usize)) {
                    Some(tracked) => {
                        let applies = tracked.id == id
                            && tracked.observation.as_ref().is_none_or(|observation| {
                                observation.activity.last_present_sequence <= present_sequence
                            });
                        if applies {
                            tracked.observation = None;
                            tracked.activity.consecutive_present_cycles = 0;
                            Some(tracked.activity.last_present_sequence.max(present_sequence))
                        } else {
                            None
                        }
                    }
                    None => None,
                };
                if let Some(retirement_sequence) = retirement_sequence {
                    let retirement = policy.swap_chain_retirement(id, retirement_sequence);
                    let policy_horizon = policy.sequence;
                    let transition = policy.reconcile_render_selection(None, policy_horizon);
                    Some((policy.next_selection_sequence(), transition, retirement))
                } else {
                    None
                }
            };
            if state.is_some()
                && renderer_prepared
                && render_lane.is_some()
                && let (Some(renderer), Some(pointer)) = (&self.renderer, NonNull::new(pointer))
            {
                let frame =
                    ResizeFrame::new(pointer, generation_id(id), requested_size, requested_format);
                self.invoke_renderer(
                    || renderer.after_resize(&frame),
                    id,
                    Boundary::RendererCallback,
                );
            }
            state
        } else {
            None
        };
        self.emit_observation(DxgiObservationEvent::ResizeForwarded {
            swap_chain: id,
            requested_size,
            requested_format,
            result: disposition,
        });
        if result < 0 {
            self.emit_diagnostic(DiagnosticEvent::SwapChainFailure {
                swap_chain: Some(diagnostic_id(id)),
                operation: SwapChainOperation::ResizeBuffers,
                code: FailureCode::HResult(result),
            });
        }
        if let Some((transition_sequence, transition, retirement)) = resize_transition {
            self.emit_render_selection_transition(
                transition_sequence,
                transition.resolved,
                transition.deferred,
            );
            if let Some(retirement) = retirement {
                self.retire_or_defer(retirement, render_lane.is_some());
            }
        }
        self.finish_render_lane(render_lane);
    }

    pub(crate) fn after_set_color_space(
        &self,
        pointer: *mut c_void,
        requested_raw: i32,
        result: i32,
    ) {
        let requested = sdk::color_space(requested_raw);
        let disposition = sdk::hresult_disposition(result);
        let render_lane = self.try_render_lane();
        let state = {
            let mut policy = lock(&self.policy);
            let (id, active, observation_changed) = {
                let Some(tracked) = policy.tracked.get_mut(&(pointer as usize)) else {
                    drop(policy);
                    self.finish_render_lane(render_lane);
                    return;
                };
                if result >= 0 {
                    tracked.color_space = requested;
                    tracked.color_space_reported = true;
                    if let Some(observation) = &mut tracked.observation {
                        observation.color_space = requested;
                    }
                }
                (
                    tracked.id,
                    tracked.color_space,
                    result >= 0 && tracked.observation.is_some(),
                )
            };

            let selection = if observation_changed {
                let previous_primary = policy.classifier.current();
                let retirement = previous_primary.and_then(|primary| {
                    let sequence = policy
                        .tracked
                        .values()
                        .find(|tracked| tracked.id == primary)
                        .map(|tracked| tracked.activity.last_present_sequence)?;
                    policy.swap_chain_retirement(primary, sequence)
                });
                let policy_horizon = policy.sequence;
                let transition = policy.reconcile_render_selection(None, policy_horizon);
                let retirement = retirement.filter(|retirement| {
                    policy.classifier.current() != Some(retirement.id())
                        || policy
                            .registry
                            .get(retirement.id())
                            .is_none_or(|session| session.generation() != retirement.generation())
                });
                Some((policy.next_selection_sequence(), transition, retirement))
            } else {
                None
            };
            (id, active, selection)
        };

        self.emit_observation(DxgiObservationEvent::ColorSpaceForwarded {
            swap_chain: state.0,
            requested,
            active: state.1,
            result: disposition,
        });
        if let Some((transition_sequence, transition, retirement)) = state.2 {
            self.emit_render_selection_transition(
                transition_sequence,
                transition.resolved,
                transition.deferred,
            );
            if let Some(retirement) = retirement {
                self.retire_or_defer(retirement, render_lane.is_some());
            }
        }
        self.finish_render_lane(render_lane);
    }

    fn render(&self, frame: PresentFrame<'_>) {
        let Some(renderer) = &self.renderer else {
            return;
        };
        let id = frame.id();
        let generation = frame.generation();
        let stage = frame.stage();
        let sequence = frame.sequence();
        let succeeded =
            self.invoke_renderer(|| renderer.render(&frame), id, Boundary::RendererCallback);
        lock(&self.policy).record_render_outcome(id, generation, stage, sequence, succeeded);
    }

    fn retire_renderer_swap_chain(&self, retirement: SwapChainRetirement) {
        let Some(renderer) = &self.renderer else {
            return;
        };
        let id = retirement.id();
        let _ = self.invoke_renderer(
            || renderer.retire_swap_chain(retirement),
            id,
            Boundary::RendererCallback,
        );
    }

    fn invoke_renderer(
        &self,
        callback: impl FnOnce() -> Result<(), RenderCallbackError>,
        id: SwapChainId,
        boundary: Boundary,
    ) -> bool {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)) {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                self.emit_diagnostic(DiagnosticEvent::RenderFailure {
                    swap_chain: diagnostic_id(id),
                    operation: error.operation(),
                    code: error.code(),
                });
                false
            }
            Err(_) => {
                self.report_panic(boundary);
                self.emit_diagnostic(DiagnosticEvent::RenderFailure {
                    swap_chain: diagnostic_id(id),
                    operation: RenderOperation::RestoreState,
                    code: FailureCode::Internal(InternalFailure::InvalidState),
                });
                false
            }
        }
    }
}

pub(crate) struct PresentInvocation {
    id: SwapChainId,
    sequence: u64,
    render_lane: Option<RenderLaneLease>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenderTicket {
    key: usize,
    id: SwapChainId,
    method: PresentMethod,
    sequence: u64,
    generation: SessionGeneration,
    stage: RenderStage,
    reason: SelectionReason,
    transition_sequence: u64,
}

pub(crate) struct ResizeInvocation {
    id: SwapChainId,
    present_sequence: u64,
    requested_size: nexus_render::Extent2D,
    requested_format: nexus_render::SurfaceFormat,
    render_lane: Option<RenderLaneLease>,
    renderer_prepared: bool,
}

struct TrackedSwapChain {
    id: SwapChainId,
    interface: SwapChainInterface,
    activity: Activity,
    observation: Option<SwapChainObservation>,
    color_space: ColorSpace,
    color_space_reported: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RenderableObservationSet {
    observations: Vec<SwapChainObservation>,
    has_cooling_down: bool,
    has_disabled: bool,
    override_suppression: Option<RenderFailurePolicySuppression>,
}

struct RenderSelectionTransition {
    render: Option<(SessionGeneration, RenderStage, SelectionReason)>,
    resolved: Option<RenderSelectionResolved>,
    deferred: Option<RenderSelectionDeferred>,
}

struct PolicyState {
    next_swap_chain_id: u64,
    next_device_id: u64,
    sequence: u64,
    selection_sequence: u64,
    tracked: BTreeMap<usize, TrackedSwapChain>,
    devices: BTreeMap<usize, DeviceId>,
    classifier: PrimarySwapChainClassifier,
    registry: SwapChainRegistry,
    render_controls: RenderControls,
}

impl PolicyState {
    fn new(config: DxgiConfig) -> Self {
        Self {
            next_swap_chain_id: 1,
            next_device_id: 1,
            sequence: 0,
            selection_sequence: 0,
            tracked: BTreeMap::new(),
            devices: BTreeMap::new(),
            classifier: PrimarySwapChainClassifier::new(config.classifier),
            registry: SwapChainRegistry::new(config.failure_policy),
            render_controls: config.render_controls,
        }
    }

    fn device_id(&mut self, native_identity: usize) -> DeviceId {
        if let Some(id) = self.devices.get(&native_identity) {
            return *id;
        }
        let id = DeviceId::new(self.next_device_id);
        self.next_device_id = self.next_device_id.saturating_add(1);
        self.devices.insert(native_identity, id);
        id
    }

    fn next_selection_sequence(&mut self) -> u64 {
        self.selection_sequence = self.selection_sequence.saturating_add(1);
        self.selection_sequence
    }

    fn is_current_present(&self, key: usize, id: SwapChainId, sequence: u64) -> bool {
        self.tracked.get(&key).is_some_and(|tracked| {
            tracked.id == id && tracked.activity.last_present_sequence == sequence
        })
    }

    fn permits_render_ticket(&self, ticket: RenderTicket) -> bool {
        self.is_current_present(ticket.key, ticket.id, ticket.sequence)
            && self.classifier.current() == Some(ticket.id)
            && self.render_controls.permits(RenderStage::RenderProbe)
            && self.render_controls.effective_stage() == ticket.stage
            && self.registry.get(ticket.id).is_some_and(|session| {
                session.generation() == ticket.generation
                    && session.lifecycle() == SessionLifecycle::Primary
            })
    }

    fn swap_chain_retirement(&self, id: SwapChainId, sequence: u64) -> Option<SwapChainRetirement> {
        self.registry
            .get(id)
            .map(|session| SwapChainRetirement::new(id, session.generation(), sequence))
    }

    fn invalidate_observation(&mut self, key: usize, id: SwapChainId, sequence: u64) -> bool {
        let Some(tracked) = self.tracked.get_mut(&key) else {
            return false;
        };
        if tracked.id != id || tracked.activity.last_present_sequence != sequence {
            return false;
        }
        tracked.observation = None;
        true
    }

    fn reconcile_render_selection(
        &mut self,
        presenting_id: Option<SwapChainId>,
        sequence: u64,
    ) -> RenderSelectionTransition {
        let observations: Vec<_> = self
            .tracked
            .values()
            .filter_map(|tracked| tracked.observation.clone())
            .collect();
        let latest_present_sequence = self
            .tracked
            .values()
            .map(|tracked| tracked.activity.last_present_sequence)
            .max()
            .unwrap_or_default()
            .max(sequence);
        let stage = self.render_controls.effective_stage();
        let renderable = self.renderable_observations(&observations, stage, sequence);
        let classification = self.classifier.classify_with_latest_present_sequence(
            &renderable.observations,
            latest_present_sequence,
        );
        let primary = classification.selected_id();
        let reason = classification.selection_reason();
        let resolved = RenderSelectionResolved::from_classification_with_override_suppression(
            &classification,
            renderable.override_suppression,
        );
        let deferred = RenderSelectionDeferred::
            from_classification_with_failure_policy_and_override_suppression(
                &classification,
                renderable.has_cooling_down,
                renderable.has_disabled,
                renderable.override_suppression,
            );
        let _events = self.registry.reconcile(&observations, primary);
        let render = match presenting_id {
            Some(id)
                if primary == Some(id)
                    && self.render_controls.permits(RenderStage::RenderProbe) =>
            {
                self.permit_render(id, stage, sequence)
                    .zip(reason)
                    .map(|(generation, reason)| (generation, stage, reason))
            }
            Some(_) | None => None,
        };
        RenderSelectionTransition {
            render,
            resolved,
            deferred,
        }
    }

    fn renderable_observations(
        &self,
        observations: &[SwapChainObservation],
        stage: RenderStage,
        sequence: u64,
    ) -> RenderableObservationSet {
        let mut renderable = RenderableObservationSet::default();
        let user_override = self.classifier.config().user_override();
        for observation in observations {
            let Some(session) = self.registry.get(observation.id) else {
                renderable.observations.push(observation.clone());
                continue;
            };
            if !same_render_generation(session, observation) {
                renderable.observations.push(observation.clone());
                continue;
            }
            match session.failures().state(stage) {
                StageFailureState::Healthy { .. } => {
                    renderable.observations.push(observation.clone());
                }
                StageFailureState::CoolingDown {
                    retry_at_sequence, ..
                } if sequence >= retry_at_sequence => {
                    renderable.observations.push(observation.clone());
                }
                StageFailureState::CoolingDown { .. } => {
                    renderable.has_cooling_down = true;
                    if user_override == Some(observation.id) {
                        renderable.override_suppression =
                            Some(RenderFailurePolicySuppression::CoolingDown);
                    }
                }
                StageFailureState::Disabled { .. } => {
                    renderable.has_disabled = true;
                    if user_override == Some(observation.id) {
                        renderable.override_suppression =
                            Some(RenderFailurePolicySuppression::Disabled);
                    }
                }
            }
        }
        renderable
    }

    fn permit_render(
        &mut self,
        id: SwapChainId,
        stage: RenderStage,
        sequence: u64,
    ) -> Option<SessionGeneration> {
        let session = self.registry.get_mut(id)?;
        let generation = session.generation();
        match session.failures_mut().permission(stage, sequence) {
            AttemptPermission::Attempt | AttemptPermission::Retry => Some(generation),
            AttemptPermission::CoolingDown { .. } | AttemptPermission::Disabled => None,
        }
    }

    fn record_render_outcome(
        &mut self,
        id: SwapChainId,
        generation: SessionGeneration,
        stage: RenderStage,
        sequence: u64,
        succeeded: bool,
    ) -> bool {
        let Some(session) = self.registry.get_mut(id) else {
            return false;
        };
        if session.generation() != generation {
            return false;
        }
        session.record_render_outcome(stage, sequence, succeeded)
    }
}

fn same_render_generation(
    session: &nexus_render::SwapChainSession,
    observation: &SwapChainObservation,
) -> bool {
    session.lifecycle() != SessionLifecycle::Retired
        && session.observation().hwnd == observation.hwnd
        && session.observation().device == observation.device
        && session.observation().adapter_luid == observation.adapter_luid
        && session.observation().size == observation.size
        && session.observation().format == observation.format
        && session.observation().color_space == observation.color_space
}

fn diagnostic_id(id: SwapChainId) -> DiagnosticId {
    DiagnosticId::new(id.get())
}

const fn generation_id(id: SwapChainId) -> SwapChainId {
    id
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU32, sync::Barrier};

    use nexus_render::{AdapterLuid, Extent2D, SurfaceFormat};
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;

    use super::*;

    #[derive(Default)]
    struct NoopCallbacks;

    impl DxgiCallbacks for NoopCallbacks {}

    #[derive(Default)]
    struct RecordingCallbacks {
        observations: Mutex<Vec<DxgiObservationEvent>>,
    }

    impl DxgiCallbacks for RecordingCallbacks {
        fn observation(&self, event: DxgiObservationEvent) {
            lock(&self.observations).push(event);
        }
    }

    #[derive(Default)]
    struct RetiringRenderer {
        retired: Mutex<Vec<SwapChainRetirement>>,
        resize_preparations: Mutex<Vec<SwapChainId>>,
        resize_completions: Mutex<Vec<SwapChainId>>,
    }

    impl OverlayRenderer for RetiringRenderer {
        fn render(&self, _frame: &PresentFrame<'_>) -> Result<(), RenderCallbackError> {
            Ok(())
        }

        fn retire_swap_chain(
            &self,
            retirement: SwapChainRetirement,
        ) -> Result<(), RenderCallbackError> {
            lock(&self.retired).push(retirement);
            Ok(())
        }

        fn before_resize(&self, frame: &ResizeFrame<'_>) -> Result<(), RenderCallbackError> {
            lock(&self.resize_preparations).push(frame.id());
            Ok(())
        }

        fn after_resize(&self, frame: &ResizeFrame<'_>) -> Result<(), RenderCallbackError> {
            lock(&self.resize_completions).push(frame.id());
            Ok(())
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ActiveRender {
        id: SwapChainId,
        generation: SessionGeneration,
        sequence: u64,
    }

    #[derive(Default)]
    struct RetirementAwareRenderer {
        active: Mutex<Option<ActiveRender>>,
    }

    impl OverlayRenderer for RetirementAwareRenderer {
        fn render(&self, frame: &PresentFrame<'_>) -> Result<(), RenderCallbackError> {
            *lock(&self.active) = Some(ActiveRender {
                id: frame.id(),
                generation: frame.generation(),
                sequence: frame.sequence(),
            });
            Ok(())
        }

        fn retire_swap_chain(
            &self,
            retirement: SwapChainRetirement,
        ) -> Result<(), RenderCallbackError> {
            let mut active = lock(&self.active);
            let retire = active.is_some_and(|state| {
                state.id == retirement.id()
                    && state.generation.get() <= retirement.generation().get()
                    && state.sequence <= retirement.sequence()
            });
            if retire {
                *active = None;
            }
            Ok(())
        }
    }

    struct BlockingSelectionCallbacks {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    }

    impl DxgiCallbacks for BlockingSelectionCallbacks {
        fn observation(&self, event: DxgiObservationEvent) {
            if matches!(event, DxgiObservationEvent::RenderSelectionDeferred { .. }) {
                self.entered.wait();
                self.release.wait();
            }
        }
    }

    fn observation(id: u64, size: Extent2D, sequence: u64) -> SwapChainObservation {
        SwapChainObservation {
            id: SwapChainId::new(id),
            hwnd: Some(Hwnd::new(100)),
            device: DeviceId::new(1),
            adapter_luid: AdapterLuid::new(2, 0),
            format: SurfaceFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            size,
            present_method: PresentMethod::Present,
            activity: Activity::active(sequence, sequence, 10),
        }
    }

    fn failure_policy_state() -> PolicyState {
        PolicyState::new(DxgiConfig::new(
            RenderControls::default(),
            ClassifierConfig::default(),
            FailurePolicy::new(
                NonZeroU32::MIN,
                2,
                NonZeroU32::new(2).expect("two is non-zero"),
            ),
        ))
    }

    fn track_observation(policy: &mut PolicyState, key: usize, observation: SwapChainObservation) {
        let id = observation.id;
        let activity = observation.activity;
        let color_space = observation.color_space;
        policy.tracked.insert(
            key,
            TrackedSwapChain {
                id,
                interface: SwapChainInterface::Base,
                activity,
                observation: Some(observation),
                color_space,
                color_space_reported: true,
            },
        );
    }

    fn render_ticket(
        policy: &mut PolicyState,
        key: usize,
        id: SwapChainId,
        sequence: u64,
    ) -> RenderTicket {
        let policy_horizon = policy.sequence;
        let transition = policy.reconcile_render_selection(Some(id), policy_horizon);
        let (generation, stage, reason) = transition
            .render
            .expect("the presenting primary should receive a render ticket");
        RenderTicket {
            key,
            id,
            method: PresentMethod::Present,
            sequence,
            generation,
            stage,
            reason,
            transition_sequence: policy.next_selection_sequence(),
        }
    }

    #[test]
    fn render_lane_is_exclusive_nonblocking_and_releases_exactly_once() {
        let lane = Arc::new(RenderLane::default());
        let lease = lane
            .try_enter()
            .expect("the first transaction should own the lane");
        let contender = {
            let lane = lane.clone();
            std::thread::spawn(move || lane.try_enter().is_none())
        };
        assert!(
            contender
                .join()
                .expect("the nonblocking contender should finish")
        );
        drop(lease);
        assert!(lane.try_enter().is_some());
    }

    #[test]
    fn invalidated_inspection_reclassifies_and_retires_the_exact_observation() {
        let mut policy = failure_policy_state();
        let failed = observation(1, Extent2D::new(2_560, 1_440), 1);
        let remaining = observation(2, Extent2D::new(1_920, 1_080), 1);
        track_observation(&mut policy, 10, failed.clone());
        track_observation(&mut policy, 20, remaining.clone());

        let initial = policy.reconcile_render_selection(Some(failed.id), 1);
        assert!(initial.render.is_some());
        assert_eq!(policy.classifier.current(), Some(failed.id));

        assert!(policy.invalidate_observation(10, failed.id, 1));
        let transition = policy.reconcile_render_selection(None, 2);

        assert!(transition.render.is_none());
        assert_eq!(
            transition
                .resolved
                .expect("the remaining chain should resolve selection")
                .reason(),
            SelectionReason::OnlyEligibleCandidate
        );
        assert!(transition.deferred.is_none());
        assert_eq!(policy.classifier.current(), Some(remaining.id));
        assert_eq!(
            policy
                .registry
                .get(failed.id)
                .expect("the failed chain remains as retired history")
                .lifecycle(),
            SessionLifecycle::Retired
        );
        assert_eq!(
            policy
                .registry
                .get(remaining.id)
                .expect("the remaining chain stays registered")
                .lifecycle(),
            SessionLifecycle::Primary
        );

        assert!(policy.invalidate_observation(20, remaining.id, 1));
        let transition = policy.reconcile_render_selection(None, 3);
        assert!(transition.render.is_none());
        assert!(transition.resolved.is_none());
        assert_eq!(
            transition
                .deferred
                .expect("no remaining observations should defer selection")
                .failure(),
            crate::RenderSelectionFailure::NoObservations
        );
        assert_eq!(policy.classifier.current(), None);
        assert_eq!(
            policy
                .registry
                .get(remaining.id)
                .expect("the last chain remains as retired history")
                .lifecycle(),
            SessionLifecycle::Retired
        );
    }

    #[test]
    fn stale_inspection_failure_cannot_clear_or_retire_newer_present_state() {
        let renderer = Arc::new(RetiringRenderer::default());
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            Arc::new(NoopCallbacks),
            Some(renderer.clone()),
        );
        let observed = observation(1, Extent2D::new(2_560, 1_440), 2);
        {
            let mut policy = lock(&manager.inner.policy);
            track_observation(&mut policy, 10, observed.clone());
            let _transition = policy.reconcile_render_selection(Some(observed.id), 2);
        }

        manager.inner.handle_inspection_failure(10, observed.id, 1);
        assert!(
            lock(&manager.inner.policy)
                .tracked
                .get(&10)
                .and_then(|tracked| tracked.observation.as_ref())
                .is_some()
        );
        assert!(lock(&renderer.retired).is_empty());

        manager.inner.handle_inspection_failure(10, observed.id, 2);
        assert!(
            lock(&manager.inner.policy)
                .tracked
                .get(&10)
                .and_then(|tracked| tracked.observation.as_ref())
                .is_none()
        );
        let retired = lock(&renderer.retired);
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].id(), observed.id);
        assert_eq!(retired[0].sequence(), 2);
    }

    #[test]
    fn invalidated_recent_observation_keeps_ancient_auxiliary_stale() {
        let mut classifier = ClassifierConfig::default();
        classifier.set_stale_after_sequences(2);
        let mut policy = PolicyState::new(DxgiConfig::new(
            RenderControls::default(),
            classifier,
            FailurePolicy::default(),
        ));
        let recent = observation(1, Extent2D::new(2_560, 1_440), 100);
        let auxiliary = observation(2, Extent2D::new(1_920, 1_080), 10);
        policy.sequence = 100;
        track_observation(&mut policy, 10, recent.clone());
        track_observation(&mut policy, 20, auxiliary);
        assert!(
            policy
                .reconcile_render_selection(Some(recent.id), 100)
                .render
                .is_some()
        );
        assert!(policy.invalidate_observation(10, recent.id, 100));

        let transition = policy.reconcile_render_selection(None, 100);
        let deferred = transition
            .deferred
            .expect("the remaining ancient auxiliary must stay stale");
        assert_eq!(policy.classifier.current(), None);
        assert!(
            deferred
                .rejections()
                .contains(&crate::RenderCandidateRejection::Stale)
        );
    }

    #[test]
    fn stale_retirement_cannot_remove_a_newer_render_generation() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let callbacks = Arc::new(BlockingSelectionCallbacks {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let renderer = Arc::new(RetirementAwareRenderer::default());
        let manager =
            DxgiInterceptionManager::new(DxgiConfig::default(), callbacks, Some(renderer.clone()));
        let observed = observation(1, Extent2D::new(2_560, 1_440), 1);
        let (generation, stage) = {
            let mut policy = lock(&manager.inner.policy);
            policy.sequence = 1;
            track_observation(&mut policy, 10, observed.clone());
            let transition = policy.reconcile_render_selection(Some(observed.id), 1);
            let (generation, stage, _) = transition
                .render
                .expect("the initial observation should receive a render ticket");
            (generation, stage)
        };
        manager.inner.render(PresentFrame::new(
            NonNull::<u8>::dangling().cast(),
            generation_id(observed.id),
            PresentMethod::Present,
            1,
            generation,
            stage,
        ));

        let stale = manager.clone();
        let id = observed.id;
        let retirement = std::thread::spawn(move || {
            stale.inner.handle_inspection_failure(10, id, 1);
        });
        entered.wait();

        let newer = observation(1, Extent2D::new(2_560, 1_440), 2);
        let newer_generation = {
            let mut policy = lock(&manager.inner.policy);
            policy.sequence = 2;
            track_observation(&mut policy, 10, newer);
            let transition = policy.reconcile_render_selection(Some(observed.id), 2);
            transition
                .render
                .map(|(generation, _, _)| generation)
                .expect("the recreated observation should receive a render ticket")
        };
        manager.inner.render(PresentFrame::new(
            NonNull::<u8>::dangling().cast(),
            generation_id(observed.id),
            PresentMethod::Present,
            2,
            newer_generation,
            stage,
        ));
        release.wait();
        retirement
            .join()
            .expect("the retirement callback should finish");

        assert_eq!(
            *lock(&renderer.active),
            Some(ActiveRender {
                id: observed.id,
                generation: newer_generation,
                sequence: 2,
            })
        );
        assert_eq!(
            lock(&manager.inner.policy)
                .tracked
                .get(&10)
                .expect("the newer same-chain observation should remain active")
                .activity
                .last_present_sequence,
            2
        );
    }

    #[test]
    fn stale_render_ticket_cannot_run_after_a_newer_primary_render() {
        let callbacks = Arc::new(RecordingCallbacks::default());
        let renderer = Arc::new(RetirementAwareRenderer::default());
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            callbacks.clone(),
            Some(renderer.clone()),
        );
        let older = observation(1, Extent2D::new(1_280, 720), 1);
        let older_ticket = {
            let mut policy = lock(&manager.inner.policy);
            policy.sequence = 1;
            track_observation(&mut policy, 10, older.clone());
            render_ticket(&mut policy, 10, older.id, 1)
        };

        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let stale_manager = manager.clone();
        let stale_entered = entered.clone();
        let stale_release = release.clone();
        let stale = std::thread::spawn(move || {
            stale_entered.wait();
            stale_release.wait();
            stale_manager
                .inner
                .try_render_selected(NonNull::<u8>::dangling().cast(), older_ticket);
        });
        entered.wait();

        let newer = observation(2, Extent2D::new(2_560, 1_440), 2);
        let newer_ticket = {
            let mut policy = lock(&manager.inner.policy);
            policy.sequence = 2;
            track_observation(&mut policy, 20, newer.clone());
            render_ticket(&mut policy, 20, newer.id, 2)
        };
        manager
            .inner
            .try_render_selected(NonNull::<u8>::dangling().cast(), newer_ticket);
        release.wait();
        stale
            .join()
            .expect("the stale render attempt should finish");

        assert_eq!(
            *lock(&renderer.active),
            Some(ActiveRender {
                id: newer.id,
                generation: newer_ticket.generation,
                sequence: 2,
            })
        );
        let selected: Vec<_> = lock(&callbacks.observations)
            .iter()
            .filter_map(|event| match event {
                DxgiObservationEvent::RenderSelected {
                    swap_chain,
                    sequence,
                    ..
                } => Some((*swap_chain, *sequence)),
                _ => None,
            })
            .collect();
        assert_eq!(selected, vec![(newer.id, newer_ticket.transition_sequence)]);
    }

    #[test]
    fn unrelated_policy_horizon_does_not_change_render_retirement_identity() {
        let renderer = Arc::new(RetirementAwareRenderer::default());
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            Arc::new(NoopCallbacks),
            Some(renderer.clone()),
        );
        let observed = observation(1, Extent2D::new(2_560, 1_440), 1);
        let ticket = {
            let mut policy = lock(&manager.inner.policy);
            policy.sequence = 1;
            track_observation(&mut policy, 10, observed.clone());
            let ticket = render_ticket(&mut policy, 10, observed.id, 1);
            policy.sequence = 2;
            ticket
        };

        manager
            .inner
            .try_render_selected(NonNull::<u8>::dangling().cast(), ticket);
        assert_eq!(
            lock(&renderer.active).as_ref().map(|state| state.sequence),
            Some(1)
        );
        manager.inner.handle_inspection_failure(10, observed.id, 1);
        assert_eq!(*lock(&renderer.active), None);
    }

    #[test]
    fn resize_completion_mutates_only_the_captured_present_revision() {
        let callbacks = Arc::new(RecordingCallbacks::default());
        let renderer = Arc::new(RetiringRenderer::default());
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            callbacks.clone(),
            Some(renderer.clone()),
        );
        let key = 10_usize;
        let initial = observation(1, Extent2D::new(1_920, 1_080), 1);
        {
            let mut policy = lock(&manager.inner.policy);
            track_observation(&mut policy, key, initial.clone());
        }
        let stale = manager
            .inner
            .before_resize(key as *mut c_void, 2_560, 1_440, 0)
            .expect("the tracked chain should capture a resize revision");

        let newer = observation(1, Extent2D::new(2_560, 1_440), 2);
        {
            let mut policy = lock(&manager.inner.policy);
            track_observation(&mut policy, key, newer.clone());
        }
        manager
            .inner
            .after_resize(key as *mut c_void, Some(stale), 0);

        {
            let policy = lock(&manager.inner.policy);
            let tracked = policy
                .tracked
                .get(&key)
                .expect("the newer same-chain revision should remain tracked");
            assert_eq!(tracked.observation.as_ref(), Some(&newer));
            assert_eq!(
                tracked.activity.last_present_sequence,
                newer.activity.last_present_sequence
            );
            assert_eq!(
                tracked.activity.consecutive_present_cycles,
                newer.activity.consecutive_present_cycles
            );
        }
        assert!(lock(&renderer.resize_completions).is_empty());

        let current = manager
            .inner
            .before_resize(key as *mut c_void, 2_560, 1_440, 0)
            .expect("the current revision should capture a resize token");
        manager
            .inner
            .after_resize(key as *mut c_void, Some(current), 0);

        let policy = lock(&manager.inner.policy);
        let tracked = policy
            .tracked
            .get(&key)
            .expect("the exact revision should remain tracked without an observation");
        assert!(tracked.observation.is_none());
        assert_eq!(tracked.activity.consecutive_present_cycles, 0);
        drop(policy);
        assert_eq!(
            lock(&renderer.resize_preparations).as_slice(),
            &[newer.id, newer.id]
        );
        assert_eq!(lock(&renderer.resize_completions).as_slice(), &[newer.id]);
        assert_eq!(
            lock(&callbacks.observations)
                .iter()
                .filter(|event| matches!(event, DxgiObservationEvent::ResizeForwarded { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn contended_resize_invalidates_policy_without_a_partial_renderer_handshake() {
        let callbacks = Arc::new(RecordingCallbacks::default());
        let renderer = Arc::new(RetiringRenderer::default());
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            callbacks.clone(),
            Some(renderer.clone()),
        );
        let key = 10_usize;
        let observed = observation(1, Extent2D::new(1_920, 1_080), 1);
        let initial_generation = {
            let mut policy = lock(&manager.inner.policy);
            policy.sequence = 1;
            track_observation(&mut policy, key, observed.clone());
            let transition = policy.reconcile_render_selection(Some(observed.id), 1);
            assert!(transition.render.is_some());
            policy
                .registry
                .get(observed.id)
                .expect("the initial session should be registered")
                .generation()
        };

        let occupied = manager
            .inner
            .try_render_lane()
            .expect("the test should own the render lane");
        let invocation = manager
            .inner
            .before_resize(key as *mut c_void, 1_920, 1_080, 0)
            .expect("contended resize still needs a forwarding observation");
        let present = manager
            .inner
            .before_present(key as *mut c_void, PresentMethod::Present)
            .expect("a concurrent Present still needs a forwarding token");
        assert_eq!(present.sequence, 2);
        assert!(present.render_lane.is_none());
        manager
            .inner
            .after_present(key as *mut c_void, PresentMethod::Present, Some(present), 0);
        manager
            .inner
            .after_resize(key as *mut c_void, Some(invocation), 0);
        manager.inner.finish_render_lane(Some(occupied));

        {
            let policy = lock(&manager.inner.policy);
            let tracked = policy
                .tracked
                .get(&key)
                .expect("the exact tracked identity should remain known");
            assert!(tracked.observation.is_none());
            assert_eq!(
                policy
                    .registry
                    .get(observed.id)
                    .expect("the invalidated session should remain as history")
                    .lifecycle(),
                SessionLifecycle::Retired
            );
        }
        assert!(lock(&renderer.resize_preparations).is_empty());
        assert!(lock(&renderer.resize_completions).is_empty());
        assert_eq!(
            lock(&renderer.retired).as_slice(),
            &[SwapChainRetirement::new(observed.id, initial_generation, 2,)]
        );

        let reappeared = observation(1, Extent2D::new(1_920, 1_080), 3);
        let reactivated_generation = {
            let mut policy = lock(&manager.inner.policy);
            policy.sequence = 3;
            track_observation(&mut policy, key, reappeared.clone());
            let transition = policy.reconcile_render_selection(Some(reappeared.id), 3);
            assert!(transition.render.is_some());
            policy
                .registry
                .get(reappeared.id)
                .expect("the reactivated session should be registered")
                .generation()
        };
        assert_ne!(reactivated_generation, initial_generation);
        assert_eq!(
            lock(&callbacks.observations)
                .iter()
                .filter(|event| matches!(event, DxgiObservationEvent::ResizeForwarded { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn independent_current_chains_accept_out_of_order_inspection_results() {
        let mut policy = failure_policy_state();
        let older = observation(1, Extent2D::new(1_920, 1_080), 1);
        let newer = observation(2, Extent2D::new(2_560, 1_440), 2);
        track_observation(&mut policy, 10, older.clone());
        track_observation(&mut policy, 20, newer.clone());

        assert!(policy.invalidate_observation(20, newer.id, 2));
        assert!(policy.invalidate_observation(10, older.id, 1));
        assert!(
            policy
                .tracked
                .values()
                .all(|tracked| tracked.observation.is_none())
        );
    }

    #[test]
    fn late_independent_completion_gets_a_newer_selection_transition_sequence() {
        let mut policy = failure_policy_state();
        let fast_auxiliary = observation(2, Extent2D::new(1_920, 1_080), 2);
        track_observation(&mut policy, 20, fast_auxiliary.clone());
        let first = policy.reconcile_render_selection(Some(fast_auxiliary.id), 2);
        assert!(first.render.is_some());
        let first_sequence = policy.next_selection_sequence();

        let late_game = observation(1, Extent2D::new(2_560, 1_440), 1);
        track_observation(&mut policy, 10, late_game.clone());
        let second = policy.reconcile_render_selection(Some(late_game.id), 2);
        assert!(second.render.is_some());
        let second_sequence = policy.next_selection_sequence();

        assert!(second_sequence > first_sequence);
        assert_eq!(policy.classifier.current(), Some(late_game.id));
    }

    #[test]
    fn occluded_present_reconciles_selection_and_retires_renderer_state() {
        let callbacks = Arc::new(RecordingCallbacks::default());
        let renderer = Arc::new(RetiringRenderer::default());
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            callbacks.clone(),
            Some(renderer.clone()),
        );
        let primary = observation(1, Extent2D::new(2_560, 1_440), 1);
        {
            let mut policy = lock(&manager.inner.policy);
            policy.sequence = 1;
            track_observation(&mut policy, 10, primary.clone());
            let transition = policy.reconcile_render_selection(Some(primary.id), 1);
            assert!(transition.render.is_some());
        }

        manager.inner.after_present(
            10_usize as *mut c_void,
            PresentMethod::Present,
            Some(PresentInvocation {
                id: primary.id,
                sequence: 1,
                render_lane: None,
            }),
            windows::Win32::Foundation::DXGI_STATUS_OCCLUDED.0,
        );

        let policy = lock(&manager.inner.policy);
        assert_eq!(policy.classifier.current(), None);
        assert!(
            policy
                .tracked
                .get(&10)
                .and_then(|tracked| tracked.observation.as_ref())
                .is_some_and(|observation| observation.activity.occluded)
        );
        assert_eq!(
            policy
                .registry
                .get(primary.id)
                .expect("the occluded session remains tracked as a candidate")
                .lifecycle(),
            SessionLifecycle::Candidate
        );
        drop(policy);
        let retired = lock(&renderer.retired);
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].id(), primary.id);
        assert_eq!(retired[0].sequence(), 1);
        drop(retired);
        assert!(
            lock(&callbacks.observations)
                .iter()
                .any(|event| matches!(event, DxgiObservationEvent::RenderSelectionDeferred { .. }))
        );
    }

    #[test]
    fn device_lost_present_invalidates_observation_and_retires_renderer_state() {
        let renderer = Arc::new(RetiringRenderer::default());
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            Arc::new(NoopCallbacks),
            Some(renderer.clone()),
        );
        let primary = observation(1, Extent2D::new(2_560, 1_440), 1);
        {
            let mut policy = lock(&manager.inner.policy);
            policy.sequence = 1;
            track_observation(&mut policy, 10, primary.clone());
            let transition = policy.reconcile_render_selection(Some(primary.id), 1);
            assert!(transition.render.is_some());
        }

        manager.inner.after_present(
            10_usize as *mut c_void,
            PresentMethod::Present,
            Some(PresentInvocation {
                id: primary.id,
                sequence: 1,
                render_lane: None,
            }),
            windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED.0,
        );

        let policy = lock(&manager.inner.policy);
        assert_eq!(policy.classifier.current(), None);
        assert!(
            policy
                .tracked
                .get(&10)
                .is_some_and(|tracked| tracked.observation.is_none())
        );
        assert_eq!(
            policy
                .registry
                .get(primary.id)
                .expect("the device-lost session remains as retired history")
                .lifecycle(),
            SessionLifecycle::Retired
        );
        drop(policy);
        let retired = lock(&renderer.retired);
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].id(), primary.id);
        assert_eq!(retired[0].sequence(), 1);
    }

    #[test]
    fn contended_present_still_records_forwarding_and_device_loss() {
        let callbacks = Arc::new(RecordingCallbacks::default());
        let renderer = Arc::new(RetiringRenderer::default());
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            callbacks.clone(),
            Some(renderer.clone()),
        );
        let key = 10_usize;
        let primary = observation(1, Extent2D::new(2_560, 1_440), 1);
        {
            let mut policy = lock(&manager.inner.policy);
            policy.sequence = 1;
            track_observation(&mut policy, key, primary.clone());
            let transition = policy.reconcile_render_selection(Some(primary.id), 1);
            assert!(transition.render.is_some());
        }

        let occupied = manager
            .inner
            .try_render_lane()
            .expect("the test should own the render lane");
        let invocation = manager
            .inner
            .before_present(key as *mut c_void, PresentMethod::Present)
            .expect("a tracked contended Present still needs an exact result token");
        assert!(invocation.render_lane.is_none());
        assert_eq!(invocation.sequence, 2);
        manager.inner.after_present(
            key as *mut c_void,
            PresentMethod::Present,
            Some(invocation),
            windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED.0,
        );
        manager.inner.finish_render_lane(Some(occupied));

        let policy = lock(&manager.inner.policy);
        assert_eq!(policy.classifier.current(), None);
        assert!(
            policy
                .tracked
                .get(&key)
                .is_some_and(|tracked| tracked.observation.is_none())
        );
        assert_eq!(
            policy
                .registry
                .get(primary.id)
                .expect("the device-lost session remains as retired history")
                .lifecycle(),
            SessionLifecycle::Retired
        );
        drop(policy);
        let retired = lock(&renderer.retired);
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].id(), primary.id);
        assert_eq!(retired[0].sequence(), 2);
        drop(retired);
        assert!(lock(&callbacks.observations).iter().any(|event| matches!(
            event,
            DxgiObservationEvent::PresentForwarded {
                swap_chain,
                sequence: 2,
                result: HResultDisposition::DeviceRemoved,
                ..
            } if *swap_chain == primary.id
        )));
    }

    #[test]
    fn late_render_success_cannot_clear_a_newer_failure_policy_state() {
        let mut policy = failure_policy_state();
        let primary = observation(1, Extent2D::new(2_560, 1_440), 1);
        track_observation(&mut policy, 10, primary.clone());
        let transition = policy.reconcile_render_selection(Some(primary.id), 1);
        let (generation, stage, _) = transition
            .render
            .expect("the observed primary should receive a render attempt");

        assert!(policy.record_render_outcome(primary.id, generation, stage, 3, false));
        assert!(!policy.record_render_outcome(primary.id, generation, stage, 2, true));
        assert!(matches!(
            policy
                .registry
                .get(primary.id)
                .expect("the primary session should remain registered")
                .failures()
                .state(stage),
            StageFailureState::CoolingDown { .. }
        ));
    }

    #[test]
    fn filtered_recent_primary_cannot_freshen_an_ancient_auxiliary_chain() {
        let mut classifier = ClassifierConfig::default();
        classifier.set_stale_after_sequences(2);
        let mut policy = PolicyState::new(DxgiConfig::new(
            RenderControls::default(),
            classifier,
            FailurePolicy::new(
                NonZeroU32::MIN,
                2,
                NonZeroU32::new(2).expect("two is non-zero"),
            ),
        ));
        let auxiliary = observation(2, Extent2D::new(1_920, 1_080), 10);
        let primary = observation(1, Extent2D::new(2_560, 1_440), 100);
        policy.sequence = 100;
        track_observation(&mut policy, 20, auxiliary);
        track_observation(&mut policy, 10, primary.clone());
        let ticket = render_ticket(&mut policy, 10, primary.id, 100);
        assert!(policy.record_render_outcome(
            primary.id,
            ticket.generation,
            ticket.stage,
            ticket.sequence,
            false,
        ));

        policy.sequence = 101;
        let transition = policy.reconcile_render_selection(None, 101);
        let deferred = transition
            .deferred
            .expect("cooldown plus a stale auxiliary should defer selection");
        assert_eq!(policy.classifier.current(), None);
        assert_eq!(
            deferred.failure(),
            crate::RenderSelectionFailure::FailurePolicySuppressed(
                RenderFailurePolicySuppression::CoolingDown,
            )
        );
        assert!(
            deferred
                .rejections()
                .contains(&crate::RenderCandidateRejection::Stale)
        );
    }

    #[test]
    fn color_space_change_reconciles_through_failure_policy_and_emits_causally() {
        let callbacks = Arc::new(RecordingCallbacks::default());
        let renderer = Arc::new(RetiringRenderer::default());
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            callbacks.clone(),
            Some(renderer.clone()),
        );
        let primary = observation(1, Extent2D::new(2_560, 1_440), 100);
        let fallback = observation(2, Extent2D::new(1_920, 1_080), 101);
        let fallback_generation = {
            let mut policy = failure_policy_state();
            policy.sequence = 101;
            track_observation(&mut policy, 10, primary.clone());
            track_observation(&mut policy, 20, fallback.clone());
            let primary_ticket = render_ticket(&mut policy, 10, primary.id, 100);
            assert!(policy.record_render_outcome(
                primary.id,
                primary_ticket.generation,
                primary_ticket.stage,
                primary_ticket.sequence,
                false,
            ));
            let transition = policy.reconcile_render_selection(None, 101);
            assert_eq!(policy.classifier.current(), Some(fallback.id));
            assert!(transition.resolved.is_some());
            let generation = policy
                .registry
                .get(fallback.id)
                .expect("the fallback session should exist")
                .generation();
            *lock(&manager.inner.policy) = policy;
            generation
        };

        manager.inner.after_set_color_space(
            20usize as *mut c_void,
            DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020.0,
            0,
        );

        let policy = lock(&manager.inner.policy);
        assert_eq!(policy.classifier.current(), Some(fallback.id));
        let updated_generation = policy
            .registry
            .get(fallback.id)
            .expect("the updated fallback session should exist")
            .generation();
        assert_ne!(updated_generation, fallback_generation);
        drop(policy);
        let events = lock(&callbacks.observations);
        assert!(matches!(
            events.as_slice(),
            [
                DxgiObservationEvent::ColorSpaceForwarded { .. },
                DxgiObservationEvent::RenderSelectionResolved { .. }
            ]
        ));
        assert_eq!(
            lock(&renderer.retired).as_slice(),
            &[SwapChainRetirement::new(
                fallback.id,
                fallback_generation,
                101,
            )]
        );
    }

    #[test]
    fn contended_color_space_change_advances_generation_and_rejects_stale_render() {
        let callbacks = Arc::new(RecordingCallbacks::default());
        let renderer = Arc::new(RetirementAwareRenderer::default());
        let manager = DxgiInterceptionManager::new(
            DxgiConfig::default(),
            callbacks.clone(),
            Some(renderer.clone()),
        );
        let key = 10_usize;
        let primary = observation(1, Extent2D::new(2_560, 1_440), 1);
        let stale_ticket = {
            let mut policy = lock(&manager.inner.policy);
            policy.sequence = 1;
            track_observation(&mut policy, key, primary.clone());
            render_ticket(&mut policy, key, primary.id, 1)
        };

        let occupied = manager
            .inner
            .try_render_lane()
            .expect("the test should own the render lane");
        manager.inner.after_set_color_space(
            key as *mut c_void,
            DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020.0,
            0,
        );

        {
            let policy = lock(&manager.inner.policy);
            assert_eq!(policy.classifier.current(), Some(primary.id));
            assert_ne!(
                policy
                    .registry
                    .get(primary.id)
                    .expect("the updated session should remain registered")
                    .generation(),
                stale_ticket.generation
            );
            assert!(!policy.permits_render_ticket(stale_ticket));
        }
        manager.inner.finish_render_lane(Some(occupied));
        assert!(lock(&manager.inner.pending_retirements).is_empty());

        manager
            .inner
            .try_render_selected(NonNull::<u8>::dangling().cast(), stale_ticket);
        assert_eq!(*lock(&renderer.active), None);
        assert!(matches!(
            lock(&callbacks.observations).as_slice(),
            [
                DxgiObservationEvent::ColorSpaceForwarded { .. },
                DxgiObservationEvent::RenderSelectionResolved { .. }
            ]
        ));
    }

    #[test]
    fn mixed_failure_policy_and_classifier_rejection_remain_observable() {
        let mut policy = failure_policy_state();
        let cooling = observation(1, Extent2D::new(1_920, 1_080), 1);
        let rejected = observation(2, Extent2D::new(0, 1_080), 1);
        let observations = [cooling.clone(), rejected.clone()];
        policy.registry.reconcile(&observations, Some(cooling.id));
        let stage = policy.render_controls.effective_stage();
        let generation = policy
            .permit_render(cooling.id, stage, 1)
            .expect("the initial primary is renderable");
        assert!(policy.record_render_outcome(cooling.id, generation, stage, 1, false,));
        track_observation(&mut policy, 10, cooling);
        track_observation(&mut policy, 20, rejected);

        let transition = policy.reconcile_render_selection(None, 2);
        let diagnostic = transition
            .deferred
            .expect("suppression and classifier rejection should defer selection");

        assert_eq!(
            diagnostic.failure(),
            crate::RenderSelectionFailure::FailurePolicySuppressed(
                crate::RenderFailurePolicySuppression::CoolingDown
            )
        );
        assert_eq!(
            diagnostic.rejections(),
            &[crate::RenderCandidateRejection::ZeroSized]
        );
    }

    #[test]
    fn observed_override_suppression_is_not_reported_as_unavailable() {
        let mut policy = failure_policy_state();
        let requested = observation(1, Extent2D::new(2_560, 1_440), 1);
        let fallback = observation(2, Extent2D::new(1_920, 1_080), 1);
        let mut config = policy.classifier.config();
        config.set_user_override(Some(requested.id));
        policy.classifier.set_config(config);
        let observations = [requested.clone(), fallback.clone()];
        let _events = policy.registry.reconcile(&observations, Some(requested.id));
        let stage = policy.render_controls.effective_stage();
        let generation = policy
            .permit_render(requested.id, stage, 1)
            .expect("the requested chain should receive its initial attempt");
        assert!(policy.record_render_outcome(requested.id, generation, stage, 1, false,));
        track_observation(&mut policy, 10, requested);
        track_observation(&mut policy, 20, fallback);

        let transition = policy.reconcile_render_selection(None, 2);
        assert_eq!(
            transition
                .resolved
                .expect("the healthy automatic fallback should be selected")
                .override_failure(),
            Some(crate::RenderOverrideFailure::FailurePolicySuppressed(
                crate::RenderFailurePolicySuppression::CoolingDown
            ))
        );
    }

    #[test]
    fn failed_primary_yields_and_recreated_generation_discards_old_failure() {
        let mut policy = failure_policy_state();
        let primary = observation(1, Extent2D::new(1920, 1080), 1);
        let alternate = observation(2, Extent2D::new(1920, 1080), 1);
        let observations = [primary.clone(), alternate.clone()];
        policy.registry.reconcile(&observations, Some(primary.id));
        let generation = policy
            .permit_render(primary.id, RenderStage::RenderProbe, 1)
            .expect("the initial primary is renderable");
        assert!(policy.record_render_outcome(
            primary.id,
            generation,
            RenderStage::RenderProbe,
            1,
            false,
        ));

        let renderable = policy.renderable_observations(&observations, RenderStage::RenderProbe, 2);
        assert_eq!(renderable.observations, vec![alternate]);
        assert!(renderable.has_cooling_down);
        assert!(!renderable.has_disabled);

        let recreated = observation(1, Extent2D::new(2560, 1440), 2);
        let renderable = policy.renderable_observations(
            std::slice::from_ref(&recreated),
            RenderStage::RenderProbe,
            2,
        );
        assert_eq!(renderable.observations, vec![recreated.clone()]);
        assert!(!renderable.has_cooling_down);
        assert!(!renderable.has_disabled);
        policy
            .registry
            .reconcile(std::slice::from_ref(&recreated), Some(recreated.id));
        let next_generation = policy
            .registry
            .get(recreated.id)
            .expect("recreated session exists")
            .generation();
        assert_ne!(next_generation, generation);
        assert!(!policy.record_render_outcome(
            recreated.id,
            generation,
            RenderStage::RenderProbe,
            2,
            false,
        ));
    }

    #[test]
    fn cooled_down_primary_retries_when_no_alternative_exists() {
        let mut policy = failure_policy_state();
        let primary = observation(1, Extent2D::new(1920, 1080), 10);
        policy
            .registry
            .reconcile(std::slice::from_ref(&primary), Some(primary.id));
        let generation = policy
            .permit_render(primary.id, RenderStage::RenderProbe, 10)
            .expect("the initial primary is renderable");
        assert!(policy.record_render_outcome(
            primary.id,
            generation,
            RenderStage::RenderProbe,
            10,
            false,
        ));
        let renderable = policy.renderable_observations(
            std::slice::from_ref(&primary),
            RenderStage::RenderProbe,
            11,
        );
        assert!(renderable.observations.is_empty());
        assert!(renderable.has_cooling_down);
        assert!(!renderable.has_disabled);
        assert!(
            policy
                .permit_render(primary.id, RenderStage::RenderProbe, 12)
                .is_some()
        );
        assert!(policy.record_render_outcome(
            primary.id,
            generation,
            RenderStage::RenderProbe,
            12,
            true,
        ));
        assert_eq!(
            policy
                .registry
                .get(primary.id)
                .expect("primary session exists")
                .failures()
                .state(RenderStage::RenderProbe),
            StageFailureState::Healthy {
                consecutive_failures: 0,
                cooldowns: 0,
            }
        );
    }
}
