use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::OnceLock;

use nexus_control::{CommandLineParse, ControlIssue, RuntimeControls, parse_args};

/// Observable phase of the Nexus runtime lifecycle.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecyclePhase {
    /// No proxy export has started Nexus.
    Cold = 0,
    /// One caller owns runtime startup.
    Starting,
    /// Runtime services are available.
    Running,
    /// Shutdown has been requested and new work is closed.
    StopRequested,
    /// Existing callbacks and workers are draining.
    Draining,
    /// Runtime teardown completed.
    Stopped,
    /// Startup or runtime teardown failed safely.
    Failed,
}

/// Proxy export that first caused Nexus runtime initialization.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyFunction {
    /// Runtime has not been initialized through a proxy export.
    None = 0,
    /// `Direct3DCreate9`.
    D3d9Direct3dCreate9,
    /// `Direct3DCreate9Ex`.
    D3d9Direct3dCreate9Ex,
    /// `D3DPERF_BeginEvent`.
    D3d9PerfBeginEvent,
    /// `D3DPERF_EndEvent`.
    D3d9PerfEndEvent,
    /// `D3DPERF_SetMarker`.
    D3d9PerfSetMarker,
    /// `D3DPERF_SetRegion`.
    D3d9PerfSetRegion,
    /// `D3DPERF_QueryRepeatFrame`.
    D3d9PerfQueryRepeatFrame,
    /// `D3DPERF_SetOptions`.
    D3d9PerfSetOptions,
    /// `D3DPERF_GetStatus`.
    D3d9PerfGetStatus,
    /// `D3D11CreateDevice`.
    D3d11CreateDevice,
    /// `D3D11CreateDeviceAndSwapChain`.
    D3d11CreateDeviceAndSwapChain,
    /// `D3D11CoreCreateDevice`.
    D3d11CoreCreateDevice,
    /// `D3D11CoreCreateLayeredDevice`.
    D3d11CoreCreateLayeredDevice,
    /// `D3D11CoreGetLayeredDeviceSize`.
    D3d11CoreGetLayeredDeviceSize,
    /// `D3D11CoreRegisterLayers`.
    D3d11CoreRegisterLayers,
    /// `CreateDXGIFactory`.
    DxgiCreateFactory,
    /// `CreateDXGIFactory1`.
    DxgiCreateFactory1,
    /// `CreateDXGIFactory2`.
    DxgiCreateFactory2,
    /// `DXGIGetDebugInterface1`.
    DxgiGetDebugInterface1,
    /// `DXGIDeclareAdapterRemovalSupport`.
    DxgiDeclareAdapterRemovalSupport,
}

static FIRST_PROXY_FUNCTION: AtomicU32 = AtomicU32::new(ProxyFunction::None as u32);
static LIFECYCLE: Lifecycle = Lifecycle::new();
static PROCESS_CONTROLS: OnceLock<ProcessControls> = OnceLock::new();

struct ProcessControls {
    parsed: CommandLineParse,
    effective: RuntimeControls,
}

struct Lifecycle {
    phase: AtomicU8,
}

impl Lifecycle {
    const fn new() -> Self {
        Self {
            phase: AtomicU8::new(LifecyclePhase::Cold as u8),
        }
    }

    fn phase(&self) -> LifecyclePhase {
        lifecycle_phase_from_u8(self.phase.load(Ordering::Acquire))
    }

    fn begin_startup(&self) -> bool {
        self.phase
            .compare_exchange(
                LifecyclePhase::Cold as u8,
                LifecyclePhase::Starting as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn finish_startup(&self) -> bool {
        self.phase
            .compare_exchange(
                LifecyclePhase::Starting as u8,
                LifecyclePhase::Running as u8,
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn fail_startup(&self) {
        let _ = self.phase.compare_exchange(
            LifecyclePhase::Starting as u8,
            LifecyclePhase::Failed as u8,
            Ordering::Release,
            Ordering::Acquire,
        );
    }

    fn request_stop(&self) -> bool {
        self.phase
            .compare_exchange(
                LifecyclePhase::Running as u8,
                LifecyclePhase::StopRequested as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn begin_drain(&self) -> bool {
        self.phase
            .compare_exchange(
                LifecyclePhase::StopRequested as u8,
                LifecyclePhase::Draining as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn finish_stop(&self) -> bool {
        self.phase
            .compare_exchange(
                LifecyclePhase::Draining as u8,
                LifecyclePhase::Stopped as u8,
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn fail_stop(&self) {
        let mut current = self.phase.load(Ordering::Acquire);
        loop {
            if current != LifecyclePhase::StopRequested as u8
                && current != LifecyclePhase::Draining as u8
            {
                return;
            }
            match self.phase.compare_exchange_weak(
                current,
                LifecyclePhase::Failed as u8,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }
}

struct StartupGuard<'a> {
    lifecycle: &'a Lifecycle,
    completed: bool,
}

impl<'a> StartupGuard<'a> {
    fn new(lifecycle: &'a Lifecycle) -> Self {
        Self {
            lifecycle,
            completed: false,
        }
    }

    fn complete(mut self) {
        self.completed = self.lifecycle.finish_startup();
    }
}

impl Drop for StartupGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.lifecycle.fail_startup();
        }
    }
}

struct ShutdownGuard<'a> {
    lifecycle: &'a Lifecycle,
    completed: bool,
}

impl<'a> ShutdownGuard<'a> {
    fn new(lifecycle: &'a Lifecycle) -> Self {
        Self {
            lifecycle,
            completed: false,
        }
    }

    fn complete(mut self) {
        self.completed = self.lifecycle.finish_stop();
    }
}

impl Drop for ShutdownGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.lifecycle.fail_stop();
        }
    }
}

/// Records the first proxy export without racing concurrent graphics startup.
pub(crate) fn initialize(entry: ProxyFunction) {
    let _ = FIRST_PROXY_FUNCTION.compare_exchange(
        ProxyFunction::None as u32,
        entry as u32,
        Ordering::AcqRel,
        Ordering::Acquire,
    );

    if process_controls().parsed.config.legacy.vanilla {
        return;
    }

    if LIFECYCLE.begin_startup() {
        let startup = StartupGuard::new(&LIFECYCLE);
        #[cfg(target_os = "windows")]
        crate::services::initialize();
        startup.complete();
    }
}

/// Contains one teardown step without dropping a foreign panic payload.
fn contain_shutdown_step(step: impl FnOnce()) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(step)) {
        Ok(()) => true,
        Err(payload) => {
            // A foreign panic payload may itself have a panicking destructor.
            // Leaking it is preferable to allowing a second unwind to cross
            // the native DLL boundary while the runtime is already draining.
            core::mem::forget(payload);
            false
        }
    }
}

/// Requests an idempotent, ordered runtime shutdown outside the loader lock.
///
/// The first caller closes new work and owns teardown. Later callers return
/// immediately. Native proxy forwarding remains available after shutdown.
pub fn request_shutdown() {
    if !LIFECYCLE.request_stop() {
        return;
    }
    if !LIFECYCLE.begin_drain() {
        LIFECYCLE.fail_stop();
        return;
    }
    let shutdown = ShutdownGuard::new(&LIFECYCLE);

    #[cfg(target_os = "windows")]
    let completed = {
        let graphics_stopped = contain_shutdown_step(crate::dxgi::shutdown);
        let services_stopped = contain_shutdown_step(crate::services::shutdown);
        graphics_stopped && services_stopped
    };
    #[cfg(not(target_os = "windows"))]
    let completed = true;

    if completed {
        shutdown.complete();
    } else {
        #[cfg(target_os = "windows")]
        crate::diagnostics::report_proxy_panic();
    }
}

/// Returns the effective, user-controllable hook and safe-mode settings.
#[must_use]
pub fn runtime_controls() -> &'static RuntimeControls {
    &process_controls().effective
}

/// Returns non-fatal command-line issues without exposing raw argument values.
#[must_use]
pub fn control_issues() -> &'static [ControlIssue] {
    &process_controls().parsed.issues
}

pub(crate) fn debug_device_requested() -> bool {
    process_controls().parsed.config.legacy.debug_device
}

pub(crate) fn mumble_option() -> &'static nexus_control::MumbleOption {
    &process_controls().parsed.config.legacy.mumble
}

fn process_controls() -> &'static ProcessControls {
    PROCESS_CONTROLS.get_or_init(|| {
        let parsed = parse_args(std::env::args_os().skip(1));
        let effective = parsed.resolve();
        ProcessControls { parsed, effective }
    })
}

/// Returns the current runtime lifecycle phase.
#[must_use]
pub fn lifecycle_phase() -> LifecyclePhase {
    LIFECYCLE.phase()
}

fn lifecycle_phase_from_u8(value: u8) -> LifecyclePhase {
    match value {
        0 => LifecyclePhase::Cold,
        1 => LifecyclePhase::Starting,
        2 => LifecyclePhase::Running,
        3 => LifecyclePhase::StopRequested,
        4 => LifecyclePhase::Draining,
        5 => LifecyclePhase::Stopped,
        _ => LifecyclePhase::Failed,
    }
}

/// Returns the proxy export that first initialized this process.
#[must_use]
pub fn first_proxy_function() -> ProxyFunction {
    match FIRST_PROXY_FUNCTION.load(Ordering::Acquire) {
        1 => ProxyFunction::D3d9Direct3dCreate9,
        2 => ProxyFunction::D3d9Direct3dCreate9Ex,
        3 => ProxyFunction::D3d9PerfBeginEvent,
        4 => ProxyFunction::D3d9PerfEndEvent,
        5 => ProxyFunction::D3d9PerfSetMarker,
        6 => ProxyFunction::D3d9PerfSetRegion,
        7 => ProxyFunction::D3d9PerfQueryRepeatFrame,
        8 => ProxyFunction::D3d9PerfSetOptions,
        9 => ProxyFunction::D3d9PerfGetStatus,
        10 => ProxyFunction::D3d11CreateDevice,
        11 => ProxyFunction::D3d11CreateDeviceAndSwapChain,
        12 => ProxyFunction::D3d11CoreCreateDevice,
        13 => ProxyFunction::D3d11CoreCreateLayeredDevice,
        14 => ProxyFunction::D3d11CoreGetLayeredDeviceSize,
        15 => ProxyFunction::D3d11CoreRegisterLayers,
        16 => ProxyFunction::DxgiCreateFactory,
        17 => ProxyFunction::DxgiCreateFactory1,
        18 => ProxyFunction::DxgiCreateFactory2,
        19 => ProxyFunction::DxgiGetDebugInterface1,
        20 => ProxyFunction::DxgiDeclareAdapterRemovalSupport,
        _ => ProxyFunction::None,
    }
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::{
        Lifecycle, LifecyclePhase, ProxyFunction, ShutdownGuard, StartupGuard,
        contain_shutdown_step,
    };

    struct PanicOnDrop(Arc<AtomicUsize>);

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
            panic!("panic payload destructor must remain contained");
        }
    }

    #[test]
    fn proxy_function_matches_cpp_u32_layout() {
        assert_eq!(size_of::<ProxyFunction>(), size_of::<u32>());
        assert_eq!(ProxyFunction::D3d11CreateDevice as u32, 10);
        assert_eq!(ProxyFunction::DxgiDeclareAdapterRemovalSupport as u32, 20);
    }

    #[test]
    fn lifecycle_allows_exactly_one_startup_owner() {
        let lifecycle = Lifecycle::new();

        assert!(lifecycle.begin_startup());
        assert!(!lifecycle.begin_startup());
        StartupGuard::new(&lifecycle).complete();

        assert_eq!(lifecycle.phase(), LifecyclePhase::Running);
    }

    #[test]
    fn abandoned_startup_becomes_failed() {
        let lifecycle = Lifecycle::new();
        assert!(lifecycle.begin_startup());

        drop(StartupGuard::new(&lifecycle));

        assert_eq!(lifecycle.phase(), LifecyclePhase::Failed);
    }

    #[test]
    fn shutdown_has_one_owner_and_reaches_stopped() {
        let lifecycle = Lifecycle::new();
        assert!(lifecycle.begin_startup());
        StartupGuard::new(&lifecycle).complete();

        assert!(lifecycle.request_stop());
        assert!(!lifecycle.request_stop());
        assert!(lifecycle.begin_drain());
        ShutdownGuard::new(&lifecycle).complete();

        assert_eq!(lifecycle.phase(), LifecyclePhase::Stopped);
    }

    #[test]
    fn abandoned_shutdown_becomes_failed() {
        let lifecycle = Lifecycle::new();
        assert!(lifecycle.begin_startup());
        StartupGuard::new(&lifecycle).complete();
        assert!(lifecycle.request_stop());
        assert!(lifecycle.begin_drain());

        drop(ShutdownGuard::new(&lifecycle));

        assert_eq!(lifecycle.phase(), LifecyclePhase::Failed);
    }

    #[test]
    fn shutdown_containment_never_drops_a_foreign_panic_payload() {
        let payload_drops = Arc::new(AtomicUsize::new(0));
        let payload = PanicOnDrop(Arc::clone(&payload_drops));

        assert!(!contain_shutdown_step(|| std::panic::panic_any(payload)));
        assert_eq!(payload_drops.load(Ordering::Relaxed), 0);
    }
}
