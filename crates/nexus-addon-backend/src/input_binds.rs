use core::ffi::{c_char, c_void};
use core::fmt;
use std::ffi::CString;
use std::sync::Arc;

use nexus_abi::{InputBindCallbackV1, InputBindCallbackV2, InputBindV1};
use nexus_core::OwnerToken;
use nexus_input::{LegacyInputBind, ManagedInputBinds, ManagedRegistrationToken, UsKeyNames};

use crate::{
    BackendFailure, BackendOperationError, InputBindBackend, NativeCallBoundary, NativeText,
    RequiredServiceResult,
};

/// Caller-attributed adapter for legacy native managed input bindings.
pub struct InputBindApi {
    boundary: Arc<NativeCallBoundary>,
    service: Arc<ManagedInputBinds>,
}

impl InputBindApi {
    /// Creates an input-bind adapter around the process managed-bind service.
    #[must_use]
    pub fn new(boundary: Arc<NativeCallBoundary>, service: Arc<ManagedInputBinds>) -> Self {
        Self { boundary, service }
    }

    /// Invokes one copied identifier after authenticating the actual caller.
    pub fn invoke(&self, identifier: *const c_char, is_release: u8) -> RequiredServiceResult<()> {
        let _owner = self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let _outcome = self.service.invoke(identifier.as_str(), is_release != 0);
        Ok(())
    }

    /// Registers a modern textual binding and an optional native callback.
    pub fn register_with_string(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: *const c_char,
    ) -> RequiredServiceResult<()> {
        let owner = self.resolve_v2_owner(callback)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let bind = self.boundary.snapshot_identifier(bind)?;
        let Some(callback) = callback else {
            return self
                .service_result(self.service.register_default_string(
                    identifier.as_str(),
                    bind.as_str(),
                    &UsKeyNames,
                ))
                .map(|_| ());
        };
        let gate = self.boundary.callback_gate_for_current(owner)?;
        let callback_identifier = self.callback_identifier(&identifier)?;
        let registration = self.service_result(self.service.register_v2_async_string_tracked(
            identifier.as_str(),
            owner.into(),
            move |_identifier, is_release| {
                let Some(_guard) = gate.try_enter() else {
                    return;
                };
                unsafe {
                    // SAFETY: caller attribution proves the function belongs
                    // to `owner`; the exact callback gate keeps that generation
                    // loaded for the complete foreign call.
                    callback(callback_identifier.as_ptr(), u8::from(is_release));
                }
            },
            bind.as_str(),
            &UsKeyNames,
        ))?;
        self.finish_registration(owner, identifier.as_str(), &registration.1)
    }

    /// Registers a modern structured binding and an optional native callback.
    pub fn register_with_struct(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: InputBindV1,
    ) -> RequiredServiceResult<()> {
        let owner = self.resolve_v2_owner(callback)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let bind = legacy_bind(bind);
        let Some(callback) = callback else {
            return self
                .service_result(self.service.register_default(identifier.as_str(), bind))
                .map(|_| ());
        };
        let gate = self.boundary.callback_gate_for_current(owner)?;
        let callback_identifier = self.callback_identifier(&identifier)?;
        let registration = self.service_result(self.service.register_v2_async_tracked(
            identifier.as_str(),
            owner.into(),
            move |_identifier, is_release| {
                let Some(_guard) = gate.try_enter() else {
                    return;
                };
                unsafe {
                    // SAFETY: the attributed generation remains admitted for
                    // the complete call through `_guard`.
                    callback(callback_identifier.as_ptr(), u8::from(is_release));
                }
            },
            bind,
        ))?;
        self.finish_registration(owner, identifier.as_str(), &registration.1)
    }

    /// Registers a revision-1-through-3 textual binding.
    pub fn register_with_string_v1(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: *const c_char,
    ) -> RequiredServiceResult<()> {
        let owner = self.resolve_v1_owner(callback)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let bind = self.boundary.snapshot_identifier(bind)?;
        let Some(callback) = callback else {
            return self
                .service_result(self.service.register_default_string(
                    identifier.as_str(),
                    bind.as_str(),
                    &UsKeyNames,
                ))
                .map(|_| ());
        };
        let gate = self.boundary.callback_gate_for_current(owner)?;
        let callback_identifier = self.callback_identifier(&identifier)?;
        let registration = self.service_result(self.service.register_v1_string_tracked(
            identifier.as_str(),
            owner.into(),
            move |_identifier| {
                let Some(_guard) = gate.try_enter() else {
                    return;
                };
                unsafe {
                    // SAFETY: the attributed generation remains admitted for
                    // the complete call through `_guard`.
                    callback(callback_identifier.as_ptr());
                }
            },
            bind.as_str(),
            &UsKeyNames,
        ))?;
        self.finish_registration(owner, identifier.as_str(), &registration.1)
    }

    /// Registers a revision-1-through-3 structured binding.
    pub fn register_with_struct_v1(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: InputBindV1,
    ) -> RequiredServiceResult<()> {
        let owner = self.resolve_v1_owner(callback)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let bind = legacy_bind(bind);
        let Some(callback) = callback else {
            return self
                .service_result(self.service.register_default(identifier.as_str(), bind))
                .map(|_| ());
        };
        let gate = self.boundary.callback_gate_for_current(owner)?;
        let callback_identifier = self.callback_identifier(&identifier)?;
        let registration = self.service_result(self.service.register_v1_tracked(
            identifier.as_str(),
            owner.into(),
            move |_identifier| {
                let Some(_guard) = gate.try_enter() else {
                    return;
                };
                unsafe {
                    // SAFETY: the attributed generation remains admitted for
                    // the complete call through `_guard`.
                    callback(callback_identifier.as_ptr());
                }
            },
            bind,
        ))?;
        self.finish_registration(owner, identifier.as_str(), &registration.1)
    }

    /// Deregisters only the authenticated owner's handler for one identifier.
    pub fn deregister(&self, identifier: *const c_char) -> RequiredServiceResult<()> {
        let owner = self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let _removed = self
            .service
            .deregister_for_owner(identifier.as_str(), owner.into());
        Ok(())
    }

    fn resolve_v1_owner(
        &self,
        callback: Option<InputBindCallbackV1>,
    ) -> Result<OwnerToken, BackendOperationError> {
        callback.map_or_else(
            || self.boundary.resolve_owner(None).map_err(Into::into),
            |callback| {
                self.boundary
                    .resolve_owner_for_registered_address(v1_callback_address(callback))
                    .map_err(Into::into)
            },
        )
    }

    fn resolve_v2_owner(
        &self,
        callback: Option<InputBindCallbackV2>,
    ) -> Result<OwnerToken, BackendOperationError> {
        callback.map_or_else(
            || self.boundary.resolve_owner(None).map_err(Into::into),
            |callback| {
                self.boundary
                    .resolve_owner_for_registered_address(v2_callback_address(callback))
                    .map_err(Into::into)
            },
        )
    }

    fn callback_identifier(&self, identifier: &NativeText) -> RequiredServiceResult<CString> {
        CString::new(identifier.as_str()).map_err(|_| self.service_rejected())
    }

    fn finish_registration(
        &self,
        owner: OwnerToken,
        identifier: &str,
        registration: &ManagedRegistrationToken,
    ) -> RequiredServiceResult<()> {
        if let Err(error) = self.boundary.validate_current_owner(owner) {
            let _removed = self.service.remove_registration(identifier, registration);
            return Err(error.into());
        }
        Ok(())
    }

    fn service_result<T, E>(&self, result: Result<T, E>) -> RequiredServiceResult<T> {
        result.map_err(|_| self.service_rejected())
    }

    fn service_rejected(&self) -> BackendOperationError {
        self.boundary
            .failures()
            .record(BackendFailure::ServiceRejected);
        BackendOperationError::ServiceRejected
    }
}

impl InputBindBackend for InputBindApi {
    fn invoke(&self, identifier: *const c_char, is_release: u8) -> RequiredServiceResult<()> {
        InputBindApi::invoke(self, identifier, is_release)
    }

    fn register_with_string(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: *const c_char,
    ) -> RequiredServiceResult<()> {
        InputBindApi::register_with_string(self, identifier, callback, bind)
    }

    fn register_with_struct(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: InputBindV1,
    ) -> RequiredServiceResult<()> {
        InputBindApi::register_with_struct(self, identifier, callback, bind)
    }

    fn register_with_string_v1(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: *const c_char,
    ) -> RequiredServiceResult<()> {
        InputBindApi::register_with_string_v1(self, identifier, callback, bind)
    }

    fn register_with_struct_v1(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: InputBindV1,
    ) -> RequiredServiceResult<()> {
        InputBindApi::register_with_struct_v1(self, identifier, callback, bind)
    }

    fn deregister(&self, identifier: *const c_char) -> RequiredServiceResult<()> {
        InputBindApi::deregister(self, identifier)
    }
}

impl fmt::Debug for InputBindApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputBindApi")
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}

fn legacy_bind(bind: InputBindV1) -> LegacyInputBind {
    LegacyInputBind {
        key: bind.key,
        alt: bind.alt != 0,
        control: bind.ctrl != 0,
        shift: bind.shift != 0,
    }
}

fn v1_callback_address(callback: InputBindCallbackV1) -> *const c_void {
    callback as usize as *const c_void
}

fn v2_callback_address(callback: InputBindCallbackV2) -> *const c_void {
    callback as usize as *const c_void
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use core::ffi::c_char;
    use core::num::NonZeroUsize;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::ffi::{CStr, CString};
    use std::sync::{Arc, Mutex, MutexGuard};

    use nexus_abi::InputBindV1;
    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::{CallbackGate, OwnerToken};
    use nexus_input::{
        CallbackExecutor, CallbackLimits, InlineExecutor, InputBind, ManagedInputBinds,
    };
    use nexus_native_memory::NativeMemoryReader;

    use super::InputBindApi;
    use crate::{
        BackendFailureSnapshot, BackendFailures, BackendOperationError, CallBoundaryError,
        InputBindBackend, NativeCallBoundary,
    };

    const OWNER: OwnerToken = OwnerToken {
        signature: 0x1B1D,
        generation: 4,
    };
    const OTHER_OWNER: OwnerToken = OwnerToken {
        signature: 0x1B1E,
        generation: 2,
    };

    static V1_CALLS: AtomicUsize = AtomicUsize::new(0);
    static V1_IDENTIFIER: Mutex<Option<String>> = Mutex::new(None);
    static V2_RELEASES: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    static GATED_CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn record_v1(identifier: *const c_char) {
        let identifier = unsafe {
            // SAFETY: the adapter passes its retained CString for this call.
            CStr::from_ptr(identifier)
        };
        *lock(&V1_IDENTIFIER) = Some(identifier.to_string_lossy().into_owned());
        V1_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    unsafe extern "C" fn record_v2(_identifier: *const c_char, is_release: u8) {
        lock(&V2_RELEASES).push(is_release);
    }

    unsafe extern "C" fn record_gated(_identifier: *const c_char, _is_release: u8) {
        GATED_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    unsafe extern "C" fn foreign_v2(_identifier: *const c_char, _is_release: u8) {}

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
            let address = address.get();
            if address == foreign_v2 as *const () as usize {
                return Some(OTHER_OWNER);
            }
            [
                record_v1 as *const () as usize,
                record_v2 as *const () as usize,
                record_gated as *const () as usize,
            ]
            .contains(&address)
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
    struct ManualExecutor {
        jobs: Mutex<Vec<Box<dyn FnOnce() + Send + 'static>>>,
    }

    impl ManualExecutor {
        fn run_all(&self) {
            let jobs = core::mem::take(&mut *lock(&self.jobs));
            for job in jobs {
                job();
            }
        }
    }

    impl CallbackExecutor for ManualExecutor {
        fn execute(&self, job: Box<dyn FnOnce() + Send + 'static>) {
            lock(&self.jobs).push(job);
        }
    }

    struct Harness {
        api: InputBindApi,
        service: Arc<ManagedInputBinds>,
        callers: Arc<AddonCallerResolver>,
        owners: Arc<TestOwners>,
        gate: Arc<CallbackGate>,
        failures: Arc<BackendFailures>,
    }

    impl Harness {
        fn new(executor: Arc<dyn CallbackExecutor>) -> Self {
            let gate = Arc::new(CallbackGate::open());
            let owners = Arc::new(TestOwners::new(Arc::clone(&gate)));
            let callers = Arc::new(AddonCallerResolver::new(owners.clone()));
            let failures = Arc::new(BackendFailures::new());
            let boundary = Arc::new(NativeCallBoundary::new(
                Arc::clone(&callers),
                NativeMemoryReader::default(),
                Arc::clone(&failures),
            ));
            let service = Arc::new(ManagedInputBinds::new(executor, CallbackLimits::default()));
            let api = InputBindApi::new(boundary, Arc::clone(&service));
            Self {
                api,
                service,
                callers,
                owners,
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

    fn c_string(value: &str) -> CString {
        CString::new(value).expect("test string contains no NUL")
    }

    fn bind(key: u16) -> InputBindV1 {
        InputBindV1 {
            key,
            alt: 0,
            ctrl: 0,
            shift: 0,
        }
    }

    #[test]
    fn implements_the_complete_input_backend_contract() {
        fn assert_backend<T: InputBindBackend>() {}
        assert_backend::<InputBindApi>();
    }

    #[test]
    fn callbacks_use_copied_identifiers_and_canonical_release_bytes() {
        V1_CALLS.store(0, Ordering::Relaxed);
        *lock(&V1_IDENTIFIER) = None;
        lock(&V2_RELEASES).clear();
        let harness = Harness::new(Arc::new(InlineExecutor));
        let _scope = harness.enter_owner();

        {
            let identifier = c_string("copied-v1");
            let default_bind = c_string("F1");
            harness
                .api
                .register_with_string_v1(
                    identifier.as_ptr(),
                    Some(record_v1),
                    default_bind.as_ptr(),
                )
                .expect("v1 callback should register");
        }
        let identifier = c_string("copied-v1");
        harness
            .api
            .invoke(identifier.as_ptr(), 0)
            .expect("v1 press should invoke");
        harness
            .api
            .invoke(identifier.as_ptr(), 1)
            .expect("v1 release remains a no-op");
        assert_eq!(V1_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(lock(&V1_IDENTIFIER).as_deref(), Some("copied-v1"));

        let identifier = c_string("copied-v2");
        harness
            .api
            .register_with_struct(identifier.as_ptr(), Some(record_v2), bind(0x3C))
            .expect("v2 callback should register");
        harness
            .api
            .invoke(identifier.as_ptr(), 0)
            .expect("v2 press should invoke");
        harness
            .api
            .invoke(identifier.as_ptr(), u8::MAX)
            .expect("nonzero release should invoke");
        assert_eq!(*lock(&V2_RELEASES), [0, 1]);
    }

    #[test]
    fn queued_callback_rechecks_the_gate_before_foreign_code() {
        GATED_CALLS.store(0, Ordering::Relaxed);
        let executor = Arc::new(ManualExecutor::default());
        let harness = Harness::new(executor.clone());
        let _scope = harness.enter_owner();
        let identifier = c_string("queued");
        harness
            .api
            .register_with_struct(identifier.as_ptr(), Some(record_gated), bind(0x3D))
            .expect("callback should register");
        harness
            .api
            .invoke(identifier.as_ptr(), 0)
            .expect("callback job should queue");

        harness.gate.close();
        executor.run_all();
        assert_eq!(GATED_CALLS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn foreign_callback_address_cannot_impersonate_the_actual_caller() {
        let harness = Harness::new(Arc::new(InlineExecutor));
        let _scope = harness.enter_owner();
        let identifier = c_string("foreign");
        assert_eq!(
            harness
                .api
                .register_with_struct(identifier.as_ptr(), Some(foreign_v2), bind(0x3E)),
            Err(BackendOperationError::Boundary(
                CallBoundaryError::CallerAttribution
            ))
        );
        assert!(!harness.service.has_handler("foreign"));
        assert_eq!(harness.failures.snapshot().caller_attribution, 1);
    }

    #[test]
    fn closing_generation_race_rolls_back_the_exact_publication() {
        let harness = Harness::new(Arc::new(InlineExecutor));
        let _scope = harness.enter_owner();
        harness.owners.close_on_gate.store(true, Ordering::Release);
        let identifier = c_string("closing");
        let error = harness
            .api
            .register_with_struct(identifier.as_ptr(), Some(record_gated), bind(0x3E))
            .expect_err("post-publication validation must fail closed");

        assert_eq!(
            error,
            BackendOperationError::Boundary(CallBoundaryError::CallerAttribution)
        );
        assert!(!harness.service.has_handler("closing"));
        assert_eq!(
            harness.failures.snapshot(),
            BackendFailureSnapshot {
                caller_attribution: 1,
                ..BackendFailureSnapshot::default()
            }
        );
    }

    #[test]
    fn null_callback_preserves_existing_binding_and_handler() {
        let harness = Harness::new(Arc::new(InlineExecutor));
        let _scope = harness.enter_owner();
        let identifier = c_string("preserved");
        harness
            .api
            .register_with_struct(identifier.as_ptr(), Some(record_v2), bind(0x3F))
            .expect("live callback should register");
        harness
            .api
            .register_with_struct(identifier.as_ptr(), None, bind(0x40))
            .expect("null callback should preserve state");

        assert!(harness.service.has_handler("preserved"));
        assert_eq!(
            harness.service.get("preserved"),
            Some(InputBind::from(nexus_input::LegacyInputBind {
                key: 0x3F,
                alt: false,
                control: false,
                shift: false,
            }))
        );
        harness
            .api
            .deregister(identifier.as_ptr())
            .expect("owner deregistration should be idempotent");
        assert!(!harness.service.has_handler("preserved"));
    }

    #[test]
    fn managed_rejections_increment_only_the_closed_service_counter() {
        let harness = Harness::new(Arc::new(InlineExecutor));
        let _scope = harness.enter_owner();
        let empty = c_string("");
        let default_bind = c_string("F1");
        assert_eq!(
            harness
                .api
                .register_with_string(empty.as_ptr(), None, default_bind.as_ptr()),
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
