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
    Activity, ClassifierConfig, ColorSpace, DeviceId, FailurePolicy, Hwnd, PresentMethod,
    PrimarySwapChainClassifier, RenderControls, RenderStage, SwapChainId, SwapChainObservation,
    SwapChainRegistry,
};
use windows_sys::core::GUID;

use crate::{
    AttachOutcome, Boundary, DxgiCallbacks, DxgiError, DxgiObservationEvent, HResultDisposition,
    ObjectKind, ObservationField, OverlayRenderer, PresentFrame, RenderCallbackError, ResizeFrame,
    ShutdownReport, SwapChainInterface,
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
    policy: Mutex<PolicyState>,
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
        let key = pointer as usize;
        let (id, interface, sequence, color_space, report_unknown_color) = {
            let mut policy = lock(&self.policy);
            policy.sequence = policy.sequence.saturating_add(1);
            let sequence = policy.sequence;
            let tracked = policy.tracked.get_mut(&key)?;
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
            (
                tracked.id,
                tracked.interface,
                sequence,
                tracked.color_space,
                report_unknown,
            )
        };

        if report_unknown_color {
            self.emit_observation(DxgiObservationEvent::MetadataIncomplete {
                swap_chain: id,
                field: ObservationField::ColorSpace,
            });
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
                return Some(PresentInvocation { id, sequence });
            }
        };

        let selection = {
            let mut policy = lock(&self.policy);
            let device = policy.device_id(metadata.device_identity);
            let Some(tracked) = policy.tracked.get_mut(&key) else {
                return Some(PresentInvocation { id, sequence });
            };
            tracked.activity.window_visible = metadata.window_visible;
            tracked.activity.foreground = metadata.foreground;
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

            let observations: Vec<_> = policy
                .tracked
                .values()
                .filter_map(|tracked| tracked.observation.clone())
                .collect();
            let classification = policy.classifier.classify(&observations);
            let primary = classification.selected_id();
            let _events = policy.registry.reconcile(&observations, primary);
            let stage = policy.render_controls.effective_stage();
            let selected =
                primary == Some(id) && policy.render_controls.permits(RenderStage::RenderProbe);
            let generation = policy.registry.get(id).map(|session| session.generation());
            selected
                .then_some((generation, stage))
                .and_then(|(generation, stage)| generation.map(|generation| (generation, stage)))
        };

        if let Some((generation, stage)) = selection {
            self.emit_observation(DxgiObservationEvent::RenderSelected {
                swap_chain: id,
                generation,
                stage,
            });
            if let Some(pointer) = NonNull::new(pointer) {
                self.render(PresentFrame::new(
                    pointer,
                    generation_id(id),
                    method,
                    sequence,
                    generation,
                    stage,
                ));
            }
        }

        Some(PresentInvocation { id, sequence })
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
        let disposition = sdk::hresult_disposition(result);
        {
            let mut policy = lock(&self.policy);
            if let Some(tracked) = policy.tracked.get_mut(&(pointer as usize)) {
                tracked.activity.occluded = disposition == HResultDisposition::Occluded;
                if let Some(observation) = &mut tracked.observation {
                    observation.activity.occluded = tracked.activity.occluded;
                }
            }
        }
        self.emit_observation(DxgiObservationEvent::PresentForwarded {
            swap_chain: invocation.id,
            method,
            sequence: invocation.sequence,
            result: disposition,
        });
        if matches!(
            disposition,
            HResultDisposition::DeviceRemoved | HResultDisposition::DeviceReset
        ) {
            self.emit_diagnostic(DiagnosticEvent::SwapChainFailure {
                swap_chain: Some(diagnostic_id(invocation.id)),
                operation: SwapChainOperation::Present,
                code: FailureCode::Internal(InternalFailure::DeviceLost),
            });
        }
    }

    pub(crate) fn before_resize(
        &self,
        pointer: *mut c_void,
        width: u32,
        height: u32,
        format: i32,
    ) -> Option<ResizeInvocation> {
        let id = lock(&self.policy)
            .tracked
            .get(&(pointer as usize))
            .map(|tracked| tracked.id)?;
        let invocation = ResizeInvocation {
            id,
            requested_size: nexus_render::Extent2D::new(width, height),
            requested_format: sdk::raw_surface_format(format),
        };
        if let (Some(renderer), Some(pointer)) = (&self.renderer, NonNull::new(pointer)) {
            let frame = ResizeFrame::new(
                pointer,
                generation_id(id),
                invocation.requested_size,
                invocation.requested_format,
            );
            self.invoke_renderer(
                || renderer.before_resize(&frame),
                id,
                Boundary::RendererCallback,
            );
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
        let disposition = sdk::hresult_disposition(result);
        if result >= 0 {
            if let Some(tracked) = lock(&self.policy).tracked.get_mut(&(pointer as usize)) {
                tracked.observation = None;
                tracked.activity.consecutive_present_cycles = 0;
            }
            if let (Some(renderer), Some(pointer)) = (&self.renderer, NonNull::new(pointer)) {
                let frame = ResizeFrame::new(
                    pointer,
                    generation_id(invocation.id),
                    invocation.requested_size,
                    invocation.requested_format,
                );
                self.invoke_renderer(
                    || renderer.after_resize(&frame),
                    invocation.id,
                    Boundary::RendererCallback,
                );
            }
        }
        self.emit_observation(DxgiObservationEvent::ResizeForwarded {
            swap_chain: invocation.id,
            requested_size: invocation.requested_size,
            requested_format: invocation.requested_format,
            result: disposition,
        });
        if result < 0 {
            self.emit_diagnostic(DiagnosticEvent::SwapChainFailure {
                swap_chain: Some(diagnostic_id(invocation.id)),
                operation: SwapChainOperation::ResizeBuffers,
                code: FailureCode::HResult(result),
            });
        }
    }

    pub(crate) fn after_set_color_space(
        &self,
        pointer: *mut c_void,
        requested_raw: i32,
        result: i32,
    ) {
        let requested = sdk::color_space(requested_raw);
        let disposition = sdk::hresult_disposition(result);
        let state = {
            let mut policy = lock(&self.policy);
            let (id, active, observation_changed) = {
                let Some(tracked) = policy.tracked.get_mut(&(pointer as usize)) else {
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

            if observation_changed {
                let observations: Vec<_> = policy
                    .tracked
                    .values()
                    .filter_map(|tracked| tracked.observation.clone())
                    .collect();
                let classification = policy.classifier.classify(&observations);
                let primary = classification.selected_id();
                let _events = policy.registry.reconcile(&observations, primary);
            }
            (id, active)
        };

        self.emit_observation(DxgiObservationEvent::ColorSpaceForwarded {
            swap_chain: state.0,
            requested,
            active: state.1,
            result: disposition,
        });
    }

    fn render(&self, frame: PresentFrame<'_>) {
        let Some(renderer) = &self.renderer else {
            return;
        };
        self.invoke_renderer(
            || renderer.render(&frame),
            frame.id(),
            Boundary::RendererCallback,
        );
    }

    fn invoke_renderer(
        &self,
        callback: impl FnOnce() -> Result<(), RenderCallbackError>,
        id: SwapChainId,
        boundary: Boundary,
    ) {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.emit_diagnostic(DiagnosticEvent::RenderFailure {
                swap_chain: diagnostic_id(id),
                operation: error.operation(),
                code: error.code(),
            }),
            Err(_) => {
                self.report_panic(boundary);
                self.emit_diagnostic(DiagnosticEvent::RenderFailure {
                    swap_chain: diagnostic_id(id),
                    operation: RenderOperation::RestoreState,
                    code: FailureCode::Internal(InternalFailure::InvalidState),
                });
            }
        }
    }
}

pub(crate) struct PresentInvocation {
    id: SwapChainId,
    sequence: u64,
}

pub(crate) struct ResizeInvocation {
    id: SwapChainId,
    requested_size: nexus_render::Extent2D,
    requested_format: nexus_render::SurfaceFormat,
}

struct TrackedSwapChain {
    id: SwapChainId,
    interface: SwapChainInterface,
    activity: Activity,
    observation: Option<SwapChainObservation>,
    color_space: ColorSpace,
    color_space_reported: bool,
}

struct PolicyState {
    next_swap_chain_id: u64,
    next_device_id: u64,
    sequence: u64,
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
