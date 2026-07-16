use core::{
    ffi::c_void,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};
use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use nexus_control::{DiagnosticEvent, HookMode, RuntimeControls, SafeModeStage};
use nexus_dxgi::{
    DxgiCallbacks, DxgiConfig, DxgiInterceptionManager, DxgiObservationEvent, SwapChainInterface,
    swap_chain_iid,
};
use nexus_overlay::OverlayAdapter;
use nexus_render::{ClassifierConfig, FailurePolicy, RenderControls, RenderStage, SafeMode};
use windows_sys::core::GUID;

use crate::{
    diagnostics::{report_proxy_failure, report_proxy_panic},
    runtime,
};

const OBSERVATION_CAPACITY: usize = 256;

static DXGI_SERVICES: OnceLock<Option<DxgiServices>> = OnceLock::new();
static PRIMARY_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

struct DxgiServices {
    manager: DxgiInterceptionManager,
    callbacks: Arc<RuntimeDxgiCallbacks>,
}

#[derive(Default)]
struct RuntimeDxgiCallbacks {
    observations: Mutex<VecDeque<DxgiObservationEvent>>,
}

impl RuntimeDxgiCallbacks {
    fn observations(&self) -> MutexGuard<'_, VecDeque<DxgiObservationEvent>> {
        self.observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl DxgiCallbacks for RuntimeDxgiCallbacks {
    fn diagnostic(&self, event: DiagnosticEvent) {
        report_proxy_failure(&event);
    }

    fn observation(&self, event: DxgiObservationEvent) {
        if matches!(&event, DxgiObservationEvent::RenderSelected { .. }) {
            let _ = PRIMARY_FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
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

fn services() -> Option<&'static DxgiServices> {
    if runtime::lifecycle_phase() != runtime::LifecyclePhase::Running {
        return None;
    }
    DXGI_SERVICES.get_or_init(build_services).as_ref()
}

pub(crate) fn shutdown() {
    if let Some(Some(services)) = DXGI_SERVICES.get() {
        let _ = services
            .manager
            .close_and_drain(std::time::Duration::from_secs(2));
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
    let config = DxgiConfig::new(
        render_controls,
        ClassifierConfig::default(),
        FailurePolicy::default(),
    );

    let manager = DxgiInterceptionManager::new(config, callbacks.clone(), Some(renderer));
    Some(DxgiServices { manager, callbacks })
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
    use nexus_control::{HookMode, RuntimeControls, SafeModeStage};
    use nexus_dxgi::{DxgiCallbacks, DxgiObservationEvent, FactoryInterface};
    use nexus_render::{RenderStage, SafeMode};

    use super::{RuntimeDxgiCallbacks, map_render_controls};

    fn controls(hook_mode: HookMode, safe_mode: SafeModeStage) -> RuntimeControls {
        RuntimeControls {
            hook_mode,
            safe_mode,
            constrained_by: None,
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
}
