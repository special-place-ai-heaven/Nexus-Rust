use core::ffi::c_void;
use core::fmt;
use std::sync::Arc;

use nexus_abi::WndProcCallback;
use nexus_core::CallbackId;
use nexus_input::{GameOnlyMessageSink, RawMessage, RawRoute, RawWndProcRegistry};

use crate::{
    BackendFailure, BackendOperationError, NativeCallBoundary, RequiredServiceResult,
    WndProcBackend,
};

/// Caller-attributed adapter for raw WndProc callbacks and game-only posting.
pub struct WndProcApi {
    boundary: Arc<NativeCallBoundary>,
    callbacks: Arc<RawWndProcRegistry>,
    game: Arc<dyn GameOnlyMessageSink>,
}

impl WndProcApi {
    /// Creates a WndProc adapter around process input services.
    #[must_use]
    pub fn new(
        boundary: Arc<NativeCallBoundary>,
        callbacks: Arc<RawWndProcRegistry>,
        game: Arc<dyn GameOnlyMessageSink>,
    ) -> Self {
        Self {
            boundary,
            callbacks,
            game,
        }
    }

    /// Registers one native callback for its exact current owner generation.
    pub fn register(&self, callback: Option<WndProcCallback>) -> RequiredServiceResult<()> {
        let Some(callback) = callback else {
            let _owner = self.boundary.resolve_owner(None)?;
            return Ok(());
        };
        let owner = self
            .boundary
            .resolve_owner_for_registered_address(callback_address(callback))?;
        let gate = self.boundary.callback_gate_for_current(owner)?;
        let callback_id = callback_id(callback);
        let registration = self.callbacks.register_identified(
            owner.into(),
            callback_id,
            move |message: RawMessage| {
                let Some(_guard) = gate.try_enter() else {
                    return RawRoute::Continue;
                };
                let result = unsafe {
                    // SAFETY: attribution proves the function belongs to this
                    // exact owner; `_guard` keeps its generation loaded for the
                    // complete foreign call. The message contains opaque scalars.
                    callback(
                        message.window as *mut c_void,
                        message.message,
                        message.wparam,
                        message.lparam,
                    )
                };
                if result == 0 {
                    RawRoute::Consume
                } else {
                    RawRoute::Continue
                }
            },
        );
        if let Err(error) = self.boundary.validate_current_owner(owner) {
            let _removed = self.callbacks.deregister(registration);
            return Err(error.into());
        }
        Ok(())
    }

    /// Deregisters every duplicate matching the authenticated owner and address.
    pub fn deregister(&self, callback: Option<WndProcCallback>) -> RequiredServiceResult<()> {
        let Some(callback) = callback else {
            let _owner = self.boundary.resolve_owner(None)?;
            return Ok(());
        };
        let owner = self
            .boundary
            .resolve_owner_for_registered_address(callback_address(callback))?;
        let _removed = self
            .callbacks
            .deregister_callback(owner.into(), callback_id(callback));
        Ok(())
    }

    /// Posts one tuple to the attached game window, ignoring the supplied HWND.
    pub fn send_to_game_only(
        &self,
        _hwnd: *mut c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
    ) -> RequiredServiceResult<isize> {
        let _owner = self.boundary.resolve_owner(None)?;
        self.game
            .send_to_game_only(message, w_param, l_param)
            .map(|()| 1)
            .map_err(|_| self.service_rejected())
    }

    fn service_rejected(&self) -> BackendOperationError {
        self.boundary
            .failures()
            .record(BackendFailure::ServiceRejected);
        BackendOperationError::ServiceRejected
    }
}

impl WndProcBackend for WndProcApi {
    fn register(&self, callback: Option<WndProcCallback>) -> RequiredServiceResult<()> {
        WndProcApi::register(self, callback)
    }

    fn deregister(&self, callback: Option<WndProcCallback>) -> RequiredServiceResult<()> {
        WndProcApi::deregister(self, callback)
    }

    fn send_to_game_only(
        &self,
        hwnd: *mut c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
    ) -> RequiredServiceResult<isize> {
        WndProcApi::send_to_game_only(self, hwnd, message, w_param, l_param)
    }
}

impl fmt::Debug for WndProcApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WndProcApi")
            .field("boundary", &self.boundary)
            .field("callback_count", &self.callbacks.len())
            .finish_non_exhaustive()
    }
}

fn callback_id(callback: WndProcCallback) -> CallbackId {
    CallbackId::new(callback as *const () as usize).expect("a present function pointer is non-null")
}

fn callback_address(callback: WndProcCallback) -> *const c_void {
    callback as *const () as *const c_void
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use core::ffi::c_void;
    use core::num::NonZeroUsize;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};

    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::{CallbackGate, OwnerToken};
    use nexus_input::{
        GameOnlyMessageSink, GameSinkError, RawMessage, RawRoute, RawWndProcRegistry,
    };
    use nexus_native_memory::NativeMemoryReader;

    use super::WndProcApi;
    use crate::{
        BackendFailureSnapshot, BackendFailures, BackendOperationError, CallBoundaryError,
        NativeCallBoundary, WndProcBackend,
    };

    const OWNER: OwnerToken = OwnerToken {
        signature: 0xD00D,
        generation: 6,
    };
    const OTHER_OWNER: OwnerToken = OwnerToken {
        signature: 0xD00E,
        generation: 3,
    };

    static CALLBACK_MESSAGE: Mutex<Option<RawMessage>> = Mutex::new(None);
    static OBSERVED_GATE: Mutex<Option<Arc<CallbackGate>>> = Mutex::new(None);
    static OBSERVED_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    static CONTINUE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static GATED_CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn consume(
        hwnd: *mut c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
    ) -> u32 {
        *lock(&CALLBACK_MESSAGE) = Some(RawMessage {
            window: hwnd as usize,
            message,
            wparam: w_param,
            lparam: l_param,
        });
        let in_flight = lock(&OBSERVED_GATE)
            .as_ref()
            .map_or(0, |gate| gate.in_flight());
        OBSERVED_IN_FLIGHT.store(in_flight, Ordering::Relaxed);
        0
    }

    unsafe extern "C" fn continue_routing(
        _hwnd: *mut c_void,
        _message: u32,
        _w_param: usize,
        _l_param: isize,
    ) -> u32 {
        CONTINUE_CALLS.fetch_add(1, Ordering::Relaxed);
        1
    }

    unsafe extern "C" fn gated(
        _hwnd: *mut c_void,
        _message: u32,
        _w_param: usize,
        _l_param: isize,
    ) -> u32 {
        GATED_CALLS.fetch_add(1, Ordering::Relaxed);
        0
    }

    unsafe extern "C" fn foreign_callback(
        _hwnd: *mut c_void,
        _message: u32,
        _w_param: usize,
        _l_param: isize,
    ) -> u32 {
        1
    }

    struct TestOwners {
        current: AtomicBool,
        close_on_gate: AtomicBool,
        gate: Arc<CallbackGate>,
    }

    impl TestOwners {
        fn new(gate: Arc<CallbackGate>) -> Self {
            Self {
                current: AtomicBool::new(true),
                close_on_gate: AtomicBool::new(false),
                gate,
            }
        }
    }

    impl AddressOwnerResolver for TestOwners {
        fn owner_for_address(&self, address: NonZeroUsize) -> Option<OwnerToken> {
            if address.get() == foreign_callback as *const () as usize {
                return Some(OTHER_OWNER);
            }
            [
                consume as *const () as usize,
                continue_routing as *const () as usize,
                gated as *const () as usize,
            ]
            .contains(&address.get())
            .then_some(OWNER)
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            (owner == OWNER || owner == OTHER_OWNER) && self.current.load(Ordering::Acquire)
        }

        fn callback_gate_for_current(&self, owner: OwnerToken) -> Option<Arc<CallbackGate>> {
            if owner != OWNER || !self.current.load(Ordering::Acquire) {
                return None;
            }
            if self.close_on_gate.swap(false, Ordering::AcqRel) {
                self.gate.close();
                self.current.store(false, Ordering::Release);
            }
            Some(Arc::clone(&self.gate))
        }
    }

    #[derive(Default)]
    struct RecordingGame {
        calls: Mutex<Vec<(u32, usize, isize)>>,
        reject: AtomicBool,
    }

    impl GameOnlyMessageSink for RecordingGame {
        fn send_to_game_only(
            &self,
            message: u32,
            w_param: usize,
            l_param: isize,
        ) -> Result<(), GameSinkError> {
            if self.reject.load(Ordering::Acquire) {
                return Err(GameSinkError);
            }
            lock(&self.calls).push((message, w_param, l_param));
            Ok(())
        }
    }

    struct Harness {
        api: WndProcApi,
        callbacks: Arc<RawWndProcRegistry>,
        callers: Arc<AddonCallerResolver>,
        owners: Arc<TestOwners>,
        game: Arc<RecordingGame>,
        gate: Arc<CallbackGate>,
        failures: Arc<BackendFailures>,
    }

    impl Harness {
        fn new() -> Self {
            let gate = Arc::new(CallbackGate::open());
            let owners = Arc::new(TestOwners::new(Arc::clone(&gate)));
            let callers = Arc::new(AddonCallerResolver::new(owners.clone()));
            let failures = Arc::new(BackendFailures::new());
            let boundary = Arc::new(NativeCallBoundary::new(
                Arc::clone(&callers),
                NativeMemoryReader::default(),
                Arc::clone(&failures),
            ));
            let callbacks = Arc::new(RawWndProcRegistry::default());
            let game = Arc::new(RecordingGame::default());
            let api = WndProcApi::new(boundary, Arc::clone(&callbacks), game.clone());
            Self {
                api,
                callbacks,
                callers,
                owners,
                game,
                gate,
                failures,
            }
        }

        fn enter_owner(&self) -> nexus_addon_ffi::AddonOwnerScope {
            self.callers
                .enter_owner_scope(OWNER)
                .expect("test owner should be current")
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn message() -> RawMessage {
        RawMessage {
            window: 0x1234,
            message: 0x4321,
            wparam: 0x5678,
            lparam: -9,
        }
    }

    #[test]
    fn implements_the_complete_wnd_proc_backend_contract() {
        fn assert_backend<T: WndProcBackend>() {}
        assert_backend::<WndProcApi>();
    }

    #[test]
    fn callbacks_preserve_arguments_mapping_and_gate_extent() {
        *lock(&CALLBACK_MESSAGE) = None;
        OBSERVED_IN_FLIGHT.store(0, Ordering::Relaxed);
        CONTINUE_CALLS.store(0, Ordering::Relaxed);
        let harness = Harness::new();
        *lock(&OBSERVED_GATE) = Some(Arc::clone(&harness.gate));
        let _scope = harness.enter_owner();
        harness
            .api
            .register(Some(consume))
            .expect("consume callback should register");
        harness
            .api
            .register(Some(continue_routing))
            .expect("continue callback should register");

        let report = harness.callbacks.route(message());
        assert_eq!(report.route, RawRoute::Consume);
        assert_eq!(*lock(&CALLBACK_MESSAGE), Some(message()));
        assert_eq!(OBSERVED_IN_FLIGHT.load(Ordering::Relaxed), 1);
        assert_eq!(CONTINUE_CALLS.load(Ordering::Relaxed), 0);

        harness
            .api
            .deregister(Some(consume))
            .expect("consume callback should deregister");
        let report = harness.callbacks.route(message());
        assert_eq!(report.route, RawRoute::Continue);
        assert_eq!(CONTINUE_CALLS.load(Ordering::Relaxed), 1);
        *lock(&OBSERVED_GATE) = None;
    }

    #[test]
    fn deregistration_removes_duplicates_and_nulls_are_authenticated_noops() {
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        harness
            .api
            .register(None)
            .expect("null registration should be a no-op");
        harness
            .api
            .register(Some(continue_routing))
            .expect("first duplicate should register");
        harness
            .api
            .register(Some(continue_routing))
            .expect("second duplicate should register");
        assert_eq!(harness.callbacks.len(), 2);

        harness
            .api
            .deregister(Some(continue_routing))
            .expect("all exact duplicates should deregister");
        harness
            .api
            .deregister(None)
            .expect("null deregistration should be a no-op");
        assert!(harness.callbacks.is_empty());
    }

    #[test]
    fn closed_gate_skips_foreign_code_and_continues_routing() {
        GATED_CALLS.store(0, Ordering::Relaxed);
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        harness
            .api
            .register(Some(gated))
            .expect("gated callback should register");
        harness.gate.close();

        assert_eq!(harness.callbacks.route(message()).route, RawRoute::Continue);
        assert_eq!(GATED_CALLS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn foreign_callback_address_cannot_impersonate_the_actual_caller() {
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        assert_eq!(
            harness.api.register(Some(foreign_callback)),
            Err(BackendOperationError::Boundary(
                CallBoundaryError::CallerAttribution
            ))
        );
        assert!(harness.callbacks.is_empty());
        assert_eq!(harness.failures.snapshot().caller_attribution, 1);
    }

    #[test]
    fn closing_generation_race_removes_only_the_new_registration() {
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        harness.owners.close_on_gate.store(true, Ordering::Release);
        assert_eq!(
            harness.api.register(Some(gated)),
            Err(BackendOperationError::Boundary(
                CallBoundaryError::CallerAttribution
            ))
        );
        assert!(harness.callbacks.is_empty());
        assert_eq!(
            harness.failures.snapshot(),
            BackendFailureSnapshot {
                caller_attribution: 1,
                ..BackendFailureSnapshot::default()
            }
        );
    }

    #[test]
    fn game_only_send_ignores_hwnd_and_reports_closed_failures() {
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        let ignored_hwnd = core::ptr::without_provenance_mut::<c_void>(0xDEAD);
        assert_eq!(
            harness
                .api
                .send_to_game_only(ignored_hwnd, 0x123, 0x456, -7),
            Ok(1)
        );
        assert_eq!(*lock(&harness.game.calls), [(0x123, 0x456, -7)]);

        harness.game.reject.store(true, Ordering::Release);
        assert_eq!(
            harness
                .api
                .send_to_game_only(core::ptr::null_mut(), 1, 2, 3),
            Err(BackendOperationError::ServiceRejected)
        );
        assert_eq!(
            harness.failures.snapshot(),
            BackendFailureSnapshot {
                service_rejected: 1,
                ..BackendFailureSnapshot::default()
            }
        );
    }
}
