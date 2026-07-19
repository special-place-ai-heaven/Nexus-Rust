use core::{
    ffi::c_void,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};
use std::{
    collections::{BTreeMap, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use nexus_control::{DiagnosticEvent, HookMode, RuntimeControls, SafeModeStage};
use nexus_dxgi::{
    DxgiCallbacks, DxgiConfig, DxgiInterceptionManager, DxgiObservationEvent,
    RenderSelectionDeferred, SwapChainInterface, swap_chain_iid,
};
use nexus_overlay::OverlayAdapter;
use nexus_platform::{NativeWindowHandle, discover_current_process_top_level_window_by_class};
use nexus_render::{ClassifierConfig, FailurePolicy, Hwnd, RenderControls, RenderStage, SafeMode};
use windows_sys::core::GUID;

use crate::{
    diagnostics::{report_proxy_failure, report_proxy_panic},
    runtime,
};

const OBSERVATION_CAPACITY: usize = 256;
const SELECTION_TRANSITION_CAPACITY: usize = OBSERVATION_CAPACITY;
const SELECTION_REPORT_ATTEMPTS: usize = 2;
const EXPECTED_GAME_WINDOW_REVALIDATION_PRESENTS: u32 = 60;
const GUILD_WARS_2_WINDOW_CLASS: &str = "ArenaNet_Dx_Window_Class";

static DXGI_SERVICES: OnceLock<Option<DxgiServices>> = OnceLock::new();
static PRIMARY_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

struct DxgiServices {
    manager: DxgiInterceptionManager,
    renderer: Arc<OverlayAdapter>,
    callbacks: Arc<RuntimeDxgiCallbacks>,
    expected_game_window: Mutex<ExpectedGameWindowState>,
}

impl DxgiServices {
    fn expected_game_window(&self) -> MutexGuard<'_, ExpectedGameWindowState> {
        self.expected_game_window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectedGameWindowRefresh {
    Attachment,
    Present,
}

#[derive(Debug)]
struct ExpectedGameWindowState {
    published: Option<ExpectedGameWindow>,
    presents_until_validation: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExpectedGameWindow {
    hwnd: Hwnd,
    client_area: u64,
}

impl ExpectedGameWindow {
    const fn new(hwnd: Hwnd, client_area: u64) -> Self {
        Self { hwnd, client_area }
    }
}

impl From<NativeWindowHandle> for ExpectedGameWindow {
    fn from(window: NativeWindowHandle) -> Self {
        Self::new(Hwnd::new(window.get()), window.client_area())
    }
}

impl ExpectedGameWindowState {
    const fn new(published: Option<ExpectedGameWindow>) -> Self {
        Self {
            published,
            presents_until_validation: if published.is_some() {
                EXPECTED_GAME_WINDOW_REVALIDATION_PRESENTS
            } else {
                0
            },
        }
    }

    fn should_discover(&mut self, refresh: ExpectedGameWindowRefresh) -> bool {
        if refresh == ExpectedGameWindowRefresh::Attachment || self.published.is_none() {
            return true;
        }
        if self.presents_until_validation <= 1 {
            return true;
        }
        self.presents_until_validation -= 1;
        false
    }

    fn record_discovery(
        &mut self,
        validated_current: Option<ExpectedGameWindow>,
        discovered: Option<ExpectedGameWindow>,
    ) -> bool {
        let validated_current = match (self.published, validated_current) {
            (Some(published), Some(validated)) if published.hwnd == validated.hwnd => {
                Some(validated)
            }
            _ => None,
        };
        let next = match validated_current {
            Some(current) => match discovered {
                Some(candidate) if candidate.hwnd == current.hwnd => Some(candidate),
                Some(candidate) if candidate.client_area > current.client_area => Some(candidate),
                _ => Some(current),
            },
            None => discovered,
        };

        self.presents_until_validation = if next.is_some() {
            EXPECTED_GAME_WINDOW_REVALIDATION_PRESENTS
        } else {
            0
        };
        let changed = self.published.map(|window| window.hwnd) != next.map(|window| window.hwnd);
        self.published = next;
        changed
    }
}

enum SelectionTransition {
    Resolved,
    Deferred(RenderSelectionDeferred),
}

enum SelectionTransitionAction {
    Continue,
    Report {
        sequence: u64,
        diagnostic: RenderSelectionDeferred,
    },
}

struct SelectionTransitionInFlight {
    sequence: u64,
    reset_deferral: bool,
}

#[derive(Default)]
struct SelectionTransitionState {
    latest_sequence: Option<u64>,
    skipped_through: Option<u64>,
    deferral: Option<RenderSelectionDeferred>,
    pending: BTreeMap<u64, SelectionTransition>,
    in_flight: Option<SelectionTransitionInFlight>,
    retry: Option<SelectionTransitionInFlight>,
    draining: bool,
    reset_deferral_before_next: bool,
}

impl SelectionTransitionState {
    fn enqueue(&mut self, sequence: u64, transition: SelectionTransition) -> bool {
        let is_retry = self
            .retry
            .as_ref()
            .is_some_and(|retry| retry.sequence == sequence);
        let is_after_committed = self.latest_sequence.is_none_or(|latest| sequence > latest);
        let is_after_skipped = self
            .skipped_through
            .is_none_or(|skipped| sequence > skipped);
        if sequence != 0
            && !is_retry
            && is_after_committed
            && is_after_skipped
            && !self.pending.contains_key(&sequence)
        {
            self.pending.insert(sequence, transition);
            if self.pending.len() > SELECTION_TRANSITION_CAPACITY {
                let protected_sequence = self
                    .in_flight
                    .as_ref()
                    .or(self.retry.as_ref())
                    .map(|entry| entry.sequence);
                let evicted_sequence = self
                    .pending
                    .keys()
                    .copied()
                    .find(|pending_sequence| Some(*pending_sequence) != protected_sequence);
                let evicted = evicted_sequence.and_then(|evicted_sequence| {
                    self.pending
                        .remove(&evicted_sequence)
                        .map(|transition| (evicted_sequence, transition))
                });
                debug_assert!(
                    evicted.is_some(),
                    "a bounded selection queue must have an evictable transition"
                );
                // Dropping an intermediate transition can hide a resolution.
                // Retain only the newest contiguous tail so the overload
                // boundary cannot leave an internal sequence gap that stalls
                // delivery forever. Reset deduplication before that tail so
                // its newest deferred state remains externally visible.
                if let Some((evicted_sequence, _)) = evicted {
                    let skipped_through =
                        self.compact_overloaded_tail(protected_sequence, evicted_sequence);
                    self.skipped_through = Some(
                        self.skipped_through
                            .map_or(skipped_through, |skipped| skipped.max(skipped_through)),
                    );
                    self.reset_deferral_before_next = true;
                }
            }
        }

        self.advance_overflow_skip();
        if self.draining || !self.has_ready_transition() {
            return false;
        }
        self.draining = true;
        true
    }

    fn next_action(&mut self) -> Option<SelectionTransitionAction> {
        debug_assert!(
            self.in_flight.is_none(),
            "only a committed or abandoned report may advance the queue"
        );
        self.advance_overflow_skip();
        let expected_sequence = self
            .retry
            .as_ref()
            .map_or_else(|| self.next_expected_sequence(), |retry| retry.sequence);
        let (sequence, transition) = self.pending.first_key_value()?;
        let sequence = *sequence;
        if sequence != expected_sequence {
            return None;
        }
        let transition = match transition {
            SelectionTransition::Resolved => SelectionTransition::Resolved,
            SelectionTransition::Deferred(diagnostic) => {
                SelectionTransition::Deferred(diagnostic.clone())
            }
        };
        debug_assert!(
            self.latest_sequence.is_none_or(|latest| sequence > latest),
            "queued selection transitions must advance monotonically"
        );
        let reset_deferral = if let Some(retry) = self.retry.take() {
            debug_assert_eq!(
                retry.sequence, sequence,
                "the failed report must remain the next transition"
            );
            retry.reset_deferral
        } else {
            std::mem::take(&mut self.reset_deferral_before_next)
        };

        Some(match transition {
            SelectionTransition::Resolved => {
                self.pending.pop_first();
                self.latest_sequence = Some(sequence);
                self.deferral = None;
                SelectionTransitionAction::Continue
            }
            SelectionTransition::Deferred(diagnostic) => {
                if !reset_deferral && self.deferral.as_ref() == Some(&diagnostic) {
                    self.pending.pop_first();
                    self.latest_sequence = Some(sequence);
                    SelectionTransitionAction::Continue
                } else {
                    self.in_flight = Some(SelectionTransitionInFlight {
                        sequence,
                        reset_deferral,
                    });
                    SelectionTransitionAction::Report {
                        sequence,
                        diagnostic,
                    }
                }
            }
        })
    }

    fn commit_report(&mut self, sequence: u64) {
        let in_flight = self
            .in_flight
            .take()
            .expect("a successful report must have an in-flight transition");
        assert_eq!(
            in_flight.sequence, sequence,
            "only the active selection transition may be committed"
        );
        let transition = self
            .pending
            .remove(&sequence)
            .expect("an in-flight selection transition must remain queued");
        let SelectionTransition::Deferred(diagnostic) = transition else {
            unreachable!("only deferred transitions require external reporting");
        };
        if in_flight.reset_deferral {
            self.deferral = None;
        }
        self.latest_sequence = Some(sequence);
        self.deferral = Some(diagnostic);
    }

    fn abandon_reporter_backlog(&mut self) {
        debug_assert!(
            self.in_flight.is_some(),
            "only a persistently failing report may abandon the backlog"
        );
        let final_transition_is_resolved = self
            .pending
            .last_key_value()
            .is_some_and(|(_, transition)| matches!(transition, SelectionTransition::Resolved));
        let advanced_through = self
            .pending
            .last_key_value()
            .map(|(sequence, _)| *sequence)
            .into_iter()
            .chain(self.skipped_through)
            .max();
        if let Some(advanced_through) = advanced_through
            && self
                .latest_sequence
                .is_none_or(|latest| advanced_through > latest)
        {
            self.latest_sequence = Some(advanced_through);
        }

        self.pending.clear();
        self.in_flight = None;
        self.retry = None;
        self.skipped_through = None;
        self.draining = false;
        // A final resolution is safe to apply without an external side effect.
        // A deferred tail was not reported, so clear deduplication and force a
        // future transition to make that state externally visible again.
        self.deferral = None;
        self.reset_deferral_before_next = !final_transition_is_resolved;
    }

    fn next_expected_sequence(&self) -> u64 {
        self.latest_sequence
            .map_or(1, |latest| latest.saturating_add(1))
    }

    fn has_ready_transition(&self) -> bool {
        let expected_sequence = self
            .retry
            .as_ref()
            .map_or_else(|| self.next_expected_sequence(), |retry| retry.sequence);
        self.pending
            .first_key_value()
            .is_some_and(|(sequence, _)| *sequence == expected_sequence)
    }

    fn advance_overflow_skip(&mut self) {
        if self.in_flight.is_some() || self.retry.is_some() {
            return;
        }
        let Some(skipped_through) = self.skipped_through.take() else {
            return;
        };
        if self
            .latest_sequence
            .is_none_or(|latest| skipped_through > latest)
        {
            debug_assert!(
                self.pending
                    .first_key_value()
                    .is_none_or(|(sequence, _)| *sequence > skipped_through),
                "retained selection transitions must follow the overload boundary"
            );
            self.latest_sequence = Some(skipped_through);
        }
    }

    fn compact_overloaded_tail(
        &mut self,
        protected_sequence: Option<u64>,
        evicted_sequence: u64,
    ) -> u64 {
        let mut retained_tail_start = None;
        for sequence in self.pending.keys().rev().copied() {
            if Some(sequence) == protected_sequence {
                continue;
            }
            match retained_tail_start {
                None => retained_tail_start = Some(sequence),
                Some(start) if sequence.checked_add(1) == Some(start) => {
                    retained_tail_start = Some(sequence);
                }
                Some(_) => break,
            }
        }

        let Some(retained_tail_start) = retained_tail_start else {
            return evicted_sequence;
        };
        self.pending.retain(|sequence, _| {
            Some(*sequence) == protected_sequence || *sequence >= retained_tail_start
        });
        evicted_sequence.max(retained_tail_start.saturating_sub(1))
    }
}

struct SelectionTransitionDrainGuard<'a> {
    state: &'a Mutex<SelectionTransitionState>,
    armed: bool,
}

impl<'a> SelectionTransitionDrainGuard<'a> {
    const fn new(state: &'a Mutex<SelectionTransitionState>) -> Self {
        Self { state, armed: true }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SelectionTransitionDrainGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(in_flight) = state.in_flight.take() {
            debug_assert!(
                state.retry.is_none(),
                "only one failed selection report may be retried"
            );
            state.retry = Some(in_flight);
        }
        state.draining = false;
    }
}

#[derive(Default)]
struct RuntimeDxgiCallbacks {
    observations: Mutex<VecDeque<DxgiObservationEvent>>,
    selection_transition: Mutex<SelectionTransitionState>,
}

impl RuntimeDxgiCallbacks {
    fn observations(&self) -> MutexGuard<'_, VecDeque<DxgiObservationEvent>> {
        self.observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn enqueue_selection_transition(&self, sequence: u64, transition: SelectionTransition) -> bool {
        self.selection_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enqueue(sequence, transition)
    }

    fn record_selection_transition(&self, sequence: u64, transition: SelectionTransition) {
        if self.enqueue_selection_transition(sequence, transition) {
            self.drain_selection_transitions_with(&mut |diagnostic| {
                report_proxy_failure(&SelectionDeferredDiagnostic(diagnostic));
            });
        }
    }

    fn drain_selection_transitions_with(&self, report: &mut impl FnMut(&RenderSelectionDeferred)) {
        let mut guard = SelectionTransitionDrainGuard::new(&self.selection_transition);
        loop {
            let action = {
                let mut state = self
                    .selection_transition
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(action) = state.next_action() else {
                    state.draining = false;
                    guard.disarm();
                    return;
                };
                action
            };
            if let SelectionTransitionAction::Report {
                sequence,
                diagnostic,
            } = action
            {
                // The active-drainer flag remains set, but the state mutex is
                // deliberately released. Reentrant/newer events can queue and
                // only this drainer may observe or report them afterward.
                let mut reported = false;
                for _ in 0..SELECTION_REPORT_ATTEMPTS {
                    if catch_unwind(AssertUnwindSafe(|| report(&diagnostic))).is_ok() {
                        reported = true;
                        break;
                    }
                }
                let mut state = self
                    .selection_transition
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if reported {
                    // Commit only after the external side effect succeeds.
                    // A transient panic retries this exact in-flight payload,
                    // so concurrent/reentrant tail entries cannot overtake it.
                    state.commit_report(sequence);
                } else {
                    // A persistently panicking reporter cannot be allowed to
                    // strand a ready retry after every enqueue caller returns.
                    // Collapse the bounded snapshot and leave future deferred
                    // state visible to deduplication as unreported.
                    state.abandon_reporter_backlog();
                    guard.disarm();
                    return;
                }
            }
        }
    }
}

struct SelectionDeferredDiagnostic<'a>(&'a RenderSelectionDeferred);

impl fmt::Display for SelectionDeferredDiagnostic<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "primary render selection deferred: {}", self.0)
    }
}

impl DxgiCallbacks for RuntimeDxgiCallbacks {
    fn diagnostic(&self, event: DiagnosticEvent) {
        report_proxy_failure(&event);
    }

    fn observation(&self, event: DxgiObservationEvent) {
        let game_window_refresh = match &event {
            DxgiObservationEvent::FactoryAttached { .. }
            | DxgiObservationEvent::SwapChainAttached { .. } => {
                Some(ExpectedGameWindowRefresh::Attachment)
            }
            DxgiObservationEvent::PresentForwarded { .. } => {
                Some(ExpectedGameWindowRefresh::Present)
            }
            _ => None,
        };
        if let Some(refresh) = game_window_refresh
            && let Some(Some(services)) = DXGI_SERVICES.get()
        {
            refresh_expected_game_window(services, refresh);
        }
        match &event {
            DxgiObservationEvent::RenderSelectionResolved { sequence, .. } => {
                self.record_selection_transition(*sequence, SelectionTransition::Resolved);
            }
            DxgiObservationEvent::RenderSelected { sequence, .. } => {
                let _ = PRIMARY_FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
                self.record_selection_transition(*sequence, SelectionTransition::Resolved);
            }
            DxgiObservationEvent::RenderSelectionDeferred {
                sequence,
                diagnostic,
            } => {
                self.record_selection_transition(
                    *sequence,
                    SelectionTransition::Deferred(diagnostic.clone()),
                );
            }
            _ => {}
        }
        let mut observations = self.observations();
        if observations.len() == OBSERVATION_CAPACITY {
            observations.pop_front();
        }
        observations.push_back(event);
    }
}

struct UnsupportedGlobalFallback;

impl fmt::Display for UnsupportedGlobalFallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("explicit global DXGI fallback is not implemented")
    }
}

struct DxgiShutdownTimeout {
    in_flight: usize,
}

impl fmt::Display for DxgiShutdownTimeout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "DXGI shutdown timed out with {} callback(s) still active; overlay input remains deactivated",
            self.in_flight
        )
    }
}

fn services() -> Option<&'static DxgiServices> {
    if runtime::lifecycle_phase() != runtime::LifecyclePhase::Running {
        return None;
    }
    DXGI_SERVICES.get_or_init(build_services).as_ref()
}

pub(crate) fn shutdown() {
    if let Some(Some(services)) = DXGI_SERVICES.get() {
        services.renderer.deactivate();
        let report = services
            .manager
            .close_and_drain(std::time::Duration::from_secs(2));
        services.renderer.deactivate();
        if !report.drained {
            report_proxy_failure(&DxgiShutdownTimeout {
                in_flight: report.in_flight,
            });
        }
    }
    crate::services::shutdown_game_input();
}

fn build_services() -> Option<DxgiServices> {
    let controls = runtime::runtime_controls();
    let render_controls = match map_render_controls(controls) {
        Some(controls) => controls,
        None => {
            if controls.safe_mode != SafeModeStage::ProxyOnly
                && controls.hook_mode == HookMode::GlobalFallback
            {
                report_proxy_failure(&UnsupportedGlobalFallback);
            }
            return None;
        }
    };
    let callbacks = Arc::new(RuntimeDxgiCallbacks::default());
    let renderer = Arc::new(OverlayAdapter::with_render_observer(
        Arc::new(crate::ui::CoreUiFrameBuilder::new(
            crate::services::ui_host(),
            crate::services::font_coordinator(),
            crate::services::texture_coordinator(),
        )),
        Arc::new(runtime::request_shutdown),
        crate::services::window_router(),
        crate::services::render_observer(),
    ));
    let mut classifier = ClassifierConfig::default();
    classifier.set_require_expected_game_window(true);
    let expected_game_window = discover_expected_game_window();
    if let Some(window) = expected_game_window {
        classifier.set_expected_game_window(Some(window.hwnd));
    }
    let config = DxgiConfig::new(render_controls, classifier, FailurePolicy::default());

    let manager = DxgiInterceptionManager::new(config, callbacks.clone(), Some(renderer.clone()));
    Some(DxgiServices {
        manager,
        renderer,
        callbacks,
        expected_game_window: Mutex::new(ExpectedGameWindowState::new(expected_game_window)),
    })
}

fn discover_expected_game_window() -> Option<ExpectedGameWindow> {
    discover_current_process_top_level_window_by_class(GUILD_WARS_2_WINDOW_CLASS)
        .map(ExpectedGameWindow::from)
}

fn revalidate_expected_game_window(window: ExpectedGameWindow) -> Option<ExpectedGameWindow> {
    NativeWindowHandle::inspect_current_process_top_level_by_class(
        window.hwnd.get(),
        GUILD_WARS_2_WINDOW_CLASS,
    )
    .map(ExpectedGameWindow::from)
}

fn refresh_expected_game_window(services: &DxgiServices, refresh: ExpectedGameWindowRefresh) {
    let mut state = services.expected_game_window();
    if !state.should_discover(refresh) {
        return;
    }
    let validated_current = state.published.and_then(revalidate_expected_game_window);
    let discovered = discover_expected_game_window();
    if !state.record_discovery(validated_current, discovered) {
        return;
    }

    // Keep the discovery state serialized until the manager receives the same
    // value. Otherwise two concurrent Present callbacks could publish results
    // in the opposite order. The manager setter emits no callback, and all
    // observation callbacks enter here only after its policy lock is released,
    // so the lock order is always discovery state -> manager policy.
    services
        .manager
        .set_expected_game_window(state.published.map(|window| window.hwnd));
}

pub(crate) fn observation_snapshot() -> Vec<DxgiObservationEvent> {
    DXGI_SERVICES
        .get()
        .and_then(Option::as_ref)
        .map_or_else(Vec::new, |services| {
            services.callbacks.observations().iter().cloned().collect()
        })
}

pub(crate) fn primary_frame_count() -> u64 {
    PRIMARY_FRAME_COUNT.load(Ordering::Relaxed)
}

fn map_render_controls(controls: &RuntimeControls) -> Option<RenderControls> {
    if controls.safe_mode == SafeModeStage::ProxyOnly {
        return None;
    }

    let safe_mode = match controls.hook_mode {
        HookMode::Auto => SafeMode::Automatic,
        HookMode::Object => SafeMode::PerObjectHooks,
        HookMode::Observe => SafeMode::ObserveOnly,
        HookMode::GlobalFallback | HookMode::Off => return None,
    };
    let max_stage = match controls.safe_mode {
        SafeModeStage::ProxyOnly => RenderStage::ProxyOnly,
        SafeModeStage::HooksOnly => RenderStage::HooksOnly,
        SafeModeStage::RenderProbe => RenderStage::RenderProbe,
        SafeModeStage::CoreUi => RenderStage::CoreUi,
        SafeModeStage::Addons => RenderStage::Addons,
    };

    Some(RenderControls::new(safe_mode, max_stage))
}

/// Attaches a successfully returned factory without changing its native result.
///
/// # Safety
///
/// On a successful `result`, `interface_id` and `factory` must satisfy the
/// corresponding `CreateDXGIFactory*` output contract.
pub(crate) unsafe fn after_factory(
    result: i32,
    interface_id: *const GUID,
    factory: *mut *mut c_void,
) {
    let attached = catch_unwind(AssertUnwindSafe(|| {
        if result < 0 || interface_id.is_null() {
            return;
        }
        // SAFETY: the caller establishes the successful native output contract.
        let Some(factory) = (unsafe { read_output(factory) }) else {
            return;
        };
        let Some(services) = services() else {
            return;
        };
        refresh_expected_game_window(services, ExpectedGameWindowRefresh::Attachment);
        if runtime::lifecycle_phase() != runtime::LifecyclePhase::Running {
            return;
        }
        // SAFETY: the caller establishes that the IID remains valid through
        // this exported call and that the output implements it.
        let interface_id = unsafe { &*interface_id };
        // SAFETY: the successful native output is a live COM reference that
        // implements the requested interface.
        if let Err(error) = unsafe { services.manager.attach_factory(factory, interface_id) } {
            report_proxy_failure(&error);
        }
    }));

    if attached.is_err() {
        report_proxy_panic();
    }
}

/// Attaches a successfully returned base swap chain without changing its native result.
///
/// # Safety
///
/// On a successful `result`, `swap_chain` must satisfy the
/// `D3D11CreateDeviceAndSwapChain` output contract.
pub(crate) unsafe fn after_swap_chain(result: i32, swap_chain: *mut *mut c_void) {
    let attached = catch_unwind(AssertUnwindSafe(|| {
        if result < 0 {
            return;
        }
        // SAFETY: the caller establishes the successful native output contract.
        let Some(swap_chain) = (unsafe { read_output(swap_chain) }) else {
            return;
        };
        let Some(services) = services() else {
            return;
        };
        refresh_expected_game_window(services, ExpectedGameWindowRefresh::Attachment);
        if runtime::lifecycle_phase() != runtime::LifecyclePhase::Running {
            return;
        }
        // SAFETY: D3D11 returns an owned base IDXGISwapChain reference.
        if let Err(error) = unsafe {
            services
                .manager
                .attach_swap_chain(swap_chain, &swap_chain_iid(SwapChainInterface::Base))
        } {
            report_proxy_failure(&error);
        }
    }));

    if attached.is_err() {
        report_proxy_panic();
    }
}

unsafe fn read_output(output: *mut *mut c_void) -> Option<*mut c_void> {
    if output.is_null() {
        return None;
    }
    // SAFETY: the caller establishes that a successful native call initialized
    // this output slot, and reading a pointer does not take ownership from it.
    let value = unsafe { output.read() };
    (!value.is_null()).then_some(value)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex, mpsc},
        thread,
    };

    use nexus_control::{HookMode, RuntimeControls, SafeModeStage};
    use nexus_dxgi::{
        DxgiCallbacks, DxgiObservationEvent, FactoryInterface, RenderSelectionDeferred,
    };
    use nexus_render::{PrimarySwapChainClassifier, RenderStage, SafeMode};

    use super::{
        EXPECTED_GAME_WINDOW_REVALIDATION_PRESENTS, ExpectedGameWindow, ExpectedGameWindowRefresh,
        ExpectedGameWindowState, RuntimeDxgiCallbacks, SELECTION_REPORT_ATTEMPTS,
        SELECTION_TRANSITION_CAPACITY, SelectionDeferredDiagnostic, SelectionTransition,
        SelectionTransitionAction, SelectionTransitionState, map_render_controls,
    };

    fn controls(hook_mode: HookMode, safe_mode: SafeModeStage) -> RuntimeControls {
        RuntimeControls {
            hook_mode,
            safe_mode,
            constrained_by: None,
        }
    }

    const fn window(handle: usize, client_area: u64) -> ExpectedGameWindow {
        ExpectedGameWindow::new(nexus_render::Hwnd::new(handle), client_area)
    }

    fn record_transition(
        callbacks: &RuntimeDxgiCallbacks,
        sequence: u64,
        transition: SelectionTransition,
        reports: &mut Vec<RenderSelectionDeferred>,
    ) {
        if callbacks.enqueue_selection_transition(sequence, transition) {
            callbacks.drain_selection_transitions_with(&mut |diagnostic| {
                reports.push(diagnostic.clone());
            });
        }
    }

    #[test]
    fn object_hooks_preserve_the_requested_stage() {
        let mapped = map_render_controls(&controls(HookMode::Object, SafeModeStage::CoreUi))
            .expect("object hooks should be enabled");

        assert_eq!(mapped.safe_mode(), SafeMode::PerObjectHooks);
        assert_eq!(mapped.max_stage(), RenderStage::CoreUi);
    }

    #[test]
    fn proxy_only_off_and_unimplemented_global_modes_attach_nothing() {
        assert!(map_render_controls(&controls(HookMode::Auto, SafeModeStage::ProxyOnly)).is_none());
        assert!(map_render_controls(&controls(HookMode::Off, SafeModeStage::Addons)).is_none());
        assert!(
            map_render_controls(&controls(HookMode::GlobalFallback, SafeModeStage::Addons))
                .is_none()
        );
    }

    #[test]
    fn observations_are_bounded() {
        let callbacks = RuntimeDxgiCallbacks::default();

        for _ in 0..=super::OBSERVATION_CAPACITY {
            callbacks.observation(DxgiObservationEvent::FactoryAttached {
                interface: FactoryInterface::Base,
            });
        }

        let observations = callbacks.observations();
        assert_eq!(observations.len(), super::OBSERVATION_CAPACITY);
    }

    #[test]
    fn selection_deferral_dedup_resets_on_classification_resolution_without_render() {
        let callbacks = RuntimeDxgiCallbacks::default();
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let mut reports = Vec::new();

        record_transition(
            &callbacks,
            1,
            SelectionTransition::Deferred(diagnostic.clone()),
            &mut reports,
        );
        record_transition(
            &callbacks,
            2,
            SelectionTransition::Deferred(diagnostic.clone()),
            &mut reports,
        );
        record_transition(&callbacks, 4, SelectionTransition::Resolved, &mut reports);
        record_transition(
            &callbacks,
            3,
            SelectionTransition::Deferred(diagnostic.clone()),
            &mut reports,
        );
        record_transition(
            &callbacks,
            5,
            SelectionTransition::Deferred(diagnostic.clone()),
            &mut reports,
        );

        assert_eq!(reports, vec![diagnostic.clone(), diagnostic]);
    }

    #[test]
    fn failure_policy_state_is_a_distinct_deduplicated_transition() {
        let callbacks = RuntimeDxgiCallbacks::default();
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let missing = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let cooling = RenderSelectionDeferred::from_failure_policy(true, false)
            .expect("cooling candidates should produce a diagnostic");
        let mut reports = Vec::new();

        record_transition(
            &callbacks,
            1,
            SelectionTransition::Deferred(missing.clone()),
            &mut reports,
        );
        record_transition(
            &callbacks,
            2,
            SelectionTransition::Deferred(cooling.clone()),
            &mut reports,
        );
        record_transition(
            &callbacks,
            3,
            SelectionTransition::Deferred(cooling.clone()),
            &mut reports,
        );

        assert_eq!(reports, vec![missing, cooling]);
    }

    #[test]
    fn reentrant_out_of_order_transitions_are_drained_by_sequence() {
        let callbacks = RuntimeDxgiCallbacks::default();
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        assert!(callbacks.enqueue_selection_transition(
            1,
            SelectionTransition::Deferred(diagnostic.clone()),
        ));

        let mut queued_reentrantly = false;
        let mut reports = Vec::new();
        callbacks.drain_selection_transitions_with(&mut |reported| {
            reports.push(reported.clone());
            if !queued_reentrantly {
                queued_reentrantly = true;
                assert!(!callbacks.enqueue_selection_transition(
                    3,
                    SelectionTransition::Deferred(diagnostic.clone()),
                ));
                assert!(!callbacks.enqueue_selection_transition(2, SelectionTransition::Resolved));
            }
        });

        assert_eq!(reports, vec![diagnostic.clone(), diagnostic]);
        let state = callbacks
            .selection_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.latest_sequence, Some(3));
        assert!(state.pending.is_empty());
        assert!(!state.draining);
    }

    #[test]
    fn later_transition_waits_for_its_missing_predecessor() {
        let callbacks = RuntimeDxgiCallbacks::default();
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let missing = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let cooling = RenderSelectionDeferred::from_failure_policy(true, false)
            .expect("cooling candidates should produce a diagnostic");
        let mut reports = Vec::new();

        record_transition(
            &callbacks,
            2,
            SelectionTransition::Deferred(cooling.clone()),
            &mut reports,
        );
        assert!(reports.is_empty());
        {
            let state = callbacks
                .selection_transition
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(state.pending.keys().copied().collect::<Vec<_>>(), vec![2]);
            assert_eq!(state.latest_sequence, None);
            assert!(!state.draining);
        }

        record_transition(
            &callbacks,
            1,
            SelectionTransition::Deferred(missing.clone()),
            &mut reports,
        );

        assert_eq!(reports, vec![missing, cooling]);
        let state = callbacks
            .selection_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.latest_sequence, Some(2));
        assert!(state.pending.is_empty());
        assert!(!state.draining);
    }

    #[test]
    fn active_transition_drainer_cannot_be_overtaken() {
        let callbacks = Arc::new(RuntimeDxgiCallbacks::default());
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let missing = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let cooling = RenderSelectionDeferred::from_failure_policy(true, false)
            .expect("cooling candidates should produce a diagnostic");
        let reports = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let worker_callbacks = Arc::clone(&callbacks);
        let worker_reports = Arc::clone(&reports);
        let worker_missing = missing.clone();
        let worker = thread::spawn(move || {
            assert!(worker_callbacks.enqueue_selection_transition(
                1,
                SelectionTransition::Deferred(worker_missing),
            ));
            let mut first = true;
            worker_callbacks.drain_selection_transitions_with(&mut |diagnostic| {
                if first {
                    first = false;
                    started_tx
                        .send(())
                        .expect("the coordinating receiver remains alive");
                    release_rx
                        .recv()
                        .expect("the coordinating sender remains alive");
                }
                worker_reports
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(diagnostic.clone());
            });
        });

        started_rx
            .recv()
            .expect("the active reporter should announce itself");
        assert!(
            !callbacks
                .enqueue_selection_transition(2, SelectionTransition::Deferred(cooling.clone()),)
        );
        assert!(
            reports
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        release_tx
            .send(())
            .expect("the active reporter remains alive");
        worker.join().expect("the transition drainer should finish");

        assert_eq!(
            *reports
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![missing, cooling]
        );
    }

    #[test]
    fn pending_transition_backlog_is_bounded_to_the_newest_tail() {
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let mut state = SelectionTransitionState {
            draining: true,
            ..SelectionTransitionState::default()
        };
        let overflow = 8_u64;

        for sequence in 1..=(SELECTION_TRANSITION_CAPACITY as u64 + overflow) {
            assert!(!state.enqueue(sequence, SelectionTransition::Deferred(diagnostic.clone()),));
        }

        assert_eq!(state.pending.len(), SELECTION_TRANSITION_CAPACITY);
        assert_eq!(
            state
                .pending
                .first_key_value()
                .map(|(sequence, _)| *sequence),
            Some(overflow + 1)
        );
        assert_eq!(
            state
                .pending
                .last_key_value()
                .map(|(sequence, _)| *sequence),
            Some(SELECTION_TRANSITION_CAPACITY as u64 + overflow)
        );
        assert_eq!(state.latest_sequence, Some(overflow));
        assert_eq!(state.skipped_through, None);
        assert!(state.reset_deferral_before_next);
    }

    #[test]
    fn overload_that_discards_resolution_resets_external_deferral_deduplication() {
        let callbacks = RuntimeDxgiCallbacks::default();
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let mut reports = Vec::new();
        record_transition(
            &callbacks,
            1,
            SelectionTransition::Deferred(diagnostic.clone()),
            &mut reports,
        );

        {
            let mut state = callbacks
                .selection_transition
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.draining = true;
            assert!(!state.enqueue(2, SelectionTransition::Resolved));
            for sequence in 3..=(SELECTION_TRANSITION_CAPACITY as u64 + 2) {
                assert!(
                    !state.enqueue(sequence, SelectionTransition::Deferred(diagnostic.clone()),)
                );
            }
            assert_eq!(state.latest_sequence, Some(2));
            assert!(state.reset_deferral_before_next);
            state.draining = false;
        }

        assert!(callbacks.enqueue_selection_transition(
            3,
            SelectionTransition::Deferred(diagnostic.clone()),
        ));
        callbacks.drain_selection_transitions_with(&mut |reported| {
            reports.push(reported.clone());
        });

        assert_eq!(reports, vec![diagnostic.clone(), diagnostic]);
        let state = callbacks
            .selection_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            state.latest_sequence,
            Some(SELECTION_TRANSITION_CAPACITY as u64 + 2)
        );
        assert!(state.pending.is_empty());
        assert!(!state.draining);
    }

    #[test]
    fn bounded_overload_skips_a_missing_head_without_stalling_the_retained_tail() {
        let callbacks = RuntimeDxgiCallbacks::default();
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let last_sequence = SELECTION_TRANSITION_CAPACITY as u64 + 2;
        let mut reports = Vec::new();

        for sequence in 2..=last_sequence {
            record_transition(
                &callbacks,
                sequence,
                SelectionTransition::Deferred(diagnostic.clone()),
                &mut reports,
            );
        }

        assert_eq!(reports, vec![diagnostic]);
        let state = callbacks
            .selection_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.latest_sequence, Some(last_sequence));
        assert_eq!(state.skipped_through, None);
        assert!(state.pending.is_empty());
        assert!(!state.draining);
        drop(state);
        assert!(!callbacks.enqueue_selection_transition(1, SelectionTransition::Resolved));
    }

    #[test]
    fn noncontiguous_overload_compacts_to_a_drainable_newest_tail() {
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let mut state = SelectionTransitionState {
            draining: true,
            ..SelectionTransitionState::default()
        };
        let last_sequence = SELECTION_TRANSITION_CAPACITY as u64 * 2 + 1;

        for index in 0..=SELECTION_TRANSITION_CAPACITY {
            let sequence = index as u64 * 2 + 1;
            assert!(!state.enqueue(sequence, SelectionTransition::Deferred(diagnostic.clone()),));
        }

        assert_eq!(state.latest_sequence, Some(last_sequence - 1));
        assert_eq!(
            state.pending.keys().copied().collect::<Vec<_>>(),
            vec![last_sequence]
        );
        assert!(matches!(
            state.next_action(),
            Some(SelectionTransitionAction::Report { sequence, .. })
                if sequence == last_sequence
        ));
        state.commit_report(last_sequence);
        assert_eq!(state.latest_sequence, Some(last_sequence));
        assert!(state.pending.is_empty());
    }

    #[test]
    fn pending_transition_backlog_never_evicts_the_in_flight_report() {
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let mut state = SelectionTransitionState::default();
        let overflow = 8_u64;

        assert!(state.enqueue(1, SelectionTransition::Deferred(diagnostic.clone())));
        assert!(matches!(
            state.next_action(),
            Some(SelectionTransitionAction::Report { sequence: 1, .. })
        ));
        for sequence in 2..=(SELECTION_TRANSITION_CAPACITY as u64 + overflow) {
            assert!(!state.enqueue(sequence, SelectionTransition::Deferred(diagnostic.clone()),));
        }

        assert_eq!(state.pending.len(), SELECTION_TRANSITION_CAPACITY);
        assert!(state.pending.contains_key(&1));
        assert_eq!(state.pending.keys().copied().nth(1), Some(overflow + 2));
        assert_eq!(
            state
                .pending
                .last_key_value()
                .map(|(sequence, _)| *sequence),
            Some(SELECTION_TRANSITION_CAPACITY as u64 + overflow)
        );
        assert_eq!(state.latest_sequence, None);
        assert_eq!(state.skipped_through, Some(overflow + 1));
        assert!(state.reset_deferral_before_next);
    }

    #[test]
    fn transient_reporter_panic_retries_without_missing_wakeup() {
        let callbacks = RuntimeDxgiCallbacks::default();
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        assert!(
            callbacks.enqueue_selection_transition(
                1,
                SelectionTransition::Deferred(diagnostic.clone()),
            )
        );

        let mut attempts = 0;
        let mut reports = Vec::new();
        callbacks.drain_selection_transitions_with(&mut |reported| {
            attempts += 1;
            if attempts == 1 {
                assert!(!callbacks.enqueue_selection_transition(2, SelectionTransition::Resolved));
                panic!("synthetic reporter failure");
            }
            reports.push(reported.clone());
        });

        assert_eq!(attempts, 2);
        assert_eq!(reports, vec![diagnostic]);
        let state = callbacks
            .selection_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.latest_sequence, Some(2));
        assert!(state.pending.is_empty());
        assert!(state.in_flight.is_none());
        assert!(state.retry.is_none());
        assert!(!state.draining);
        assert!(state.deferral.is_none());
    }

    #[test]
    fn transient_reporter_panic_with_overflow_retries_before_the_retained_tail() {
        let callbacks = RuntimeDxgiCallbacks::default();
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        let overflow = 4_u64;
        let last_sequence = SELECTION_TRANSITION_CAPACITY as u64 + overflow;
        assert!(callbacks.enqueue_selection_transition(
            1,
            SelectionTransition::Deferred(diagnostic.clone()),
        ));

        let mut attempts = 0;
        let mut reports = Vec::new();
        callbacks.drain_selection_transitions_with(&mut |reported| {
            attempts += 1;
            if attempts == 1 {
                for sequence in 2..=last_sequence {
                    assert!(!callbacks.enqueue_selection_transition(
                        sequence,
                        SelectionTransition::Deferred(diagnostic.clone()),
                    ));
                }
                panic!("synthetic reporter failure after queue overflow");
            }
            reports.push(reported.clone());
        });

        assert_eq!(attempts, 3);
        assert_eq!(reports, vec![diagnostic.clone(), diagnostic]);
        let state = callbacks
            .selection_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.latest_sequence, Some(last_sequence));
        assert_eq!(state.skipped_through, None);
        assert!(state.pending.is_empty());
        assert!(state.in_flight.is_none());
        assert!(state.retry.is_none());
        assert!(!state.draining);
    }

    #[test]
    fn persistent_reporter_panic_abandons_the_ready_backlog_without_unwinding() {
        let callbacks = RuntimeDxgiCallbacks::default();
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");
        assert!(callbacks.enqueue_selection_transition(
            1,
            SelectionTransition::Deferred(diagnostic.clone()),
        ));

        let mut attempts = 0;
        callbacks.drain_selection_transitions_with(&mut |_| {
            attempts += 1;
            if attempts == 1 {
                assert!(!callbacks.enqueue_selection_transition(
                    2,
                    SelectionTransition::Deferred(diagnostic.clone()),
                ));
            }
            panic!("synthetic persistent reporter failure");
        });

        assert_eq!(attempts, SELECTION_REPORT_ATTEMPTS);
        let state = callbacks
            .selection_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.latest_sequence, Some(2));
        assert!(state.pending.is_empty());
        assert!(state.in_flight.is_none());
        assert!(state.retry.is_none());
        assert!(!state.draining);
        assert!(state.deferral.is_none());
        assert!(state.reset_deferral_before_next);
    }

    #[test]
    fn selection_deferral_diagnostic_has_stable_redacted_text() {
        let classification = PrimarySwapChainClassifier::default().classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("empty observations should defer selection");

        assert_eq!(
            SelectionDeferredDiagnostic(&diagnostic).to_string(),
            "primary render selection deferred: no swap-chain observations"
        );
    }

    #[test]
    fn missing_window_retries_on_every_forwarded_present() {
        let mut state = ExpectedGameWindowState::new(None);

        for _ in 0..3 {
            assert!(state.should_discover(ExpectedGameWindowRefresh::Present));
            assert!(!state.record_discovery(None, None));
        }
    }

    #[test]
    fn discovered_window_is_revalidated_periodically() {
        let window = window(7, 10_000);
        let mut state = ExpectedGameWindowState::new(Some(window));

        for _ in 1..EXPECTED_GAME_WINDOW_REVALIDATION_PRESENTS {
            assert!(!state.should_discover(ExpectedGameWindowRefresh::Present));
        }
        assert!(state.should_discover(ExpectedGameWindowRefresh::Present));
        assert!(!state.record_discovery(Some(window), Some(window)));
    }

    #[test]
    fn stale_window_is_cleared_fail_closed_and_retried_immediately() {
        let mut state = ExpectedGameWindowState::new(Some(window(7, 10_000)));

        assert!(state.should_discover(ExpectedGameWindowRefresh::Attachment));
        assert!(state.record_discovery(None, None));
        assert_eq!(state.published, None);
        assert!(state.should_discover(ExpectedGameWindowRefresh::Present));
    }

    #[test]
    fn larger_window_can_replace_a_valid_splash_without_a_permanent_threshold() {
        let first = window(7, 100);
        let replacement = window(8, 101);
        let mut state = ExpectedGameWindowState::new(Some(first));

        assert!(state.should_discover(ExpectedGameWindowRefresh::Attachment));
        assert!(state.record_discovery(Some(first), Some(replacement)));
        assert_eq!(state.published, Some(replacement));
    }

    #[test]
    fn larger_discovered_window_cannot_leave_a_valid_startup_window_pinned() {
        let game = window(7, 10_000);
        let replacement = window(8, 12_000);
        let mut state = ExpectedGameWindowState::new(Some(game));

        assert!(state.record_discovery(Some(game), Some(replacement)));
        assert_eq!(state.published, Some(replacement));
    }

    #[test]
    fn smaller_auxiliary_window_does_not_replace_a_valid_game_window() {
        let game = window(7, 10_000);
        let auxiliary = window(8, 9_999);
        let mut state = ExpectedGameWindowState::new(Some(game));

        assert!(!state.record_discovery(Some(game), Some(auxiliary)));
        assert_eq!(state.published, Some(game));
    }

    #[test]
    fn invalid_or_recreated_window_is_replaced_without_area_hysteresis() {
        let stale = window(7, 10_000);
        let replacement = window(8, 100);
        let mut state = ExpectedGameWindowState::new(Some(stale));

        assert!(state.record_discovery(None, Some(replacement)));
        assert_eq!(state.published, Some(replacement));
    }

    #[test]
    fn same_window_refreshes_area_without_republishing_identity() {
        let initial = window(7, 100);
        let resized = window(7, 10_000);
        let mut state = ExpectedGameWindowState::new(Some(initial));

        assert!(!state.record_discovery(Some(resized), Some(resized)));
        assert_eq!(state.published, Some(resized));
    }
}
