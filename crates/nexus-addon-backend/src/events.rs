use core::ffi::{c_char, c_void};
use core::fmt;
use std::sync::Arc;

use nexus_abi::EventCallback;
use nexus_data_services::EventService;

use crate::{BackendFailure, BackendOperationError, NativeCallBoundary};

/// Caller-attributed adapter for the legacy native event surface.
pub struct EventApi {
    boundary: Arc<NativeCallBoundary>,
    service: Arc<EventService>,
}

impl EventApi {
    /// Creates an event adapter around the process event service.
    #[must_use]
    pub fn new(boundary: Arc<NativeCallBoundary>, service: Arc<EventService>) -> Self {
        Self { boundary, service }
    }

    /// Subscribes one callback owned by the exact current add-on generation.
    pub fn subscribe(
        &self,
        identifier: *const c_char,
        callback: Option<EventCallback>,
    ) -> Result<(), BackendOperationError> {
        let owner = match callback {
            Some(callback) => self
                .boundary
                .resolve_owner_for_registered_address(callback_address(callback))?,
            None => self.boundary.resolve_owner(None)?,
        };
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let registration = unsafe {
            // SAFETY: caller attribution binds callback lifetime to one owner
            // generation. Composite teardown removes it before module unload.
            self.service
                .subscribe_native_tracked(owner, identifier.as_str(), callback)
        };
        let registration = self.service_result(registration)?;
        if let Err(error) = self.boundary.validate_current_owner(owner) {
            if self
                .service
                .unsubscribe_registration(identifier.as_str(), &registration)
                .is_err()
            {
                self.boundary
                    .failures()
                    .record(BackendFailure::ServiceRejected);
            }
            return Err(error.into());
        }
        Ok(())
    }

    /// Removes one callback identity owned by the exact calling generation.
    pub fn unsubscribe(
        &self,
        identifier: *const c_char,
        callback: Option<EventCallback>,
    ) -> Result<usize, BackendOperationError> {
        let owner = match callback {
            Some(callback) => self
                .boundary
                .resolve_owner_for_registered_address(callback_address(callback))?,
            None => self.boundary.resolve_owner(None)?,
        };
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let result =
            self.service
                .unsubscribe_native_for_owner(owner, identifier.as_str(), callback);
        self.service_result(result)
    }

    /// Raises an event to every matching subscription.
    ///
    /// # Safety
    ///
    /// `payload` must satisfy the event-specific native payload contract for
    /// the duration of synchronous dispatch. It remains opaque to Rust.
    pub unsafe fn raise(
        &self,
        identifier: *const c_char,
        payload: *mut c_void,
    ) -> Result<(), BackendOperationError> {
        self.raise_inner(identifier, payload)
    }

    fn raise_inner(
        &self,
        identifier: *const c_char,
        payload: *mut c_void,
    ) -> Result<(), BackendOperationError> {
        let _owner = self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let result = unsafe {
            // SAFETY: the payload contract is delegated to this method's
            // caller; EventService snapshots subscribers before dispatch.
            self.service.raise(identifier.as_str(), payload)
        };
        self.service_result(result).map(|_| ())
    }

    /// Raises a notification event with a null payload.
    pub fn raise_notification(
        &self,
        identifier: *const c_char,
    ) -> Result<(), BackendOperationError> {
        self.raise_inner(identifier, core::ptr::null_mut())
    }

    /// Raises an event only to subscriptions owned by `signature`.
    ///
    /// # Safety
    ///
    /// `payload` must satisfy the event-specific native payload contract for
    /// the duration of synchronous dispatch. It remains opaque to Rust.
    pub unsafe fn raise_targeted(
        &self,
        signature: u32,
        identifier: *const c_char,
        payload: *mut c_void,
    ) -> Result<(), BackendOperationError> {
        self.raise_targeted_inner(signature, identifier, payload)
    }

    fn raise_targeted_inner(
        &self,
        signature: u32,
        identifier: *const c_char,
        payload: *mut c_void,
    ) -> Result<(), BackendOperationError> {
        let _owner = self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let result = unsafe {
            // SAFETY: the payload contract is delegated to this method's
            // caller; EventService snapshots subscribers before dispatch.
            self.service
                .raise_targeted(signature, identifier.as_str(), payload)
        };
        self.service_result(result).map(|_| ())
    }

    /// Raises a targeted notification with a null payload.
    pub fn raise_notification_targeted(
        &self,
        signature: u32,
        identifier: *const c_char,
    ) -> Result<(), BackendOperationError> {
        self.raise_targeted_inner(signature, identifier, core::ptr::null_mut())
    }

    fn service_result<T, E>(&self, result: Result<T, E>) -> Result<T, BackendOperationError> {
        result.map_err(|_| {
            self.boundary
                .failures()
                .record(BackendFailure::ServiceRejected);
            BackendOperationError::ServiceRejected
        })
    }
}

impl fmt::Debug for EventApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventApi")
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}

fn callback_address(callback: EventCallback) -> *const c_void {
    callback as usize as *const c_void
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use core::ffi::c_void;
    use core::num::NonZeroUsize;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::ffi::CString;
    use std::sync::Arc;

    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::OwnerToken;
    use nexus_data_services::EventService;
    use nexus_native_memory::NativeMemoryReader;

    use super::EventApi;
    use crate::{BackendFailures, NativeCallBoundary};

    const OWNER: OwnerToken = OwnerToken {
        signature: 0xE001,
        generation: 4,
    };
    const OTHER_OWNER: OwnerToken = OwnerToken {
        signature: 0xE002,
        generation: 7,
    };
    const NEXT_GENERATION: OwnerToken = OwnerToken {
        signature: OWNER.signature,
        generation: OWNER.generation + 1,
    };
    const STALE_OWNER: OwnerToken = OwnerToken {
        signature: 0xE003,
        generation: 2,
    };
    static CALLS: AtomicUsize = AtomicUsize::new(0);

    struct FixedOwner;

    impl AddressOwnerResolver for FixedOwner {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            Some(OWNER)
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            owner == OWNER
        }
    }

    unsafe extern "C" fn count(_payload: *mut c_void) {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }

    unsafe extern "C" fn other_callback(_payload: *mut c_void) {}

    unsafe extern "C" fn stale_callback(_payload: *mut c_void) {}

    struct MappedOwners {
        owner_is_current: AtomicBool,
    }

    impl MappedOwners {
        fn new() -> Self {
            Self {
                owner_is_current: AtomicBool::new(true),
            }
        }

        fn retire_owner(&self) {
            self.owner_is_current.store(false, Ordering::Relaxed);
        }
    }

    impl AddressOwnerResolver for MappedOwners {
        fn owner_for_address(&self, address: NonZeroUsize) -> Option<OwnerToken> {
            match address.get() {
                value if value == count as *const () as usize => Some(OWNER),
                value if value == other_callback as *const () as usize => Some(OTHER_OWNER),
                value if value == stale_callback as *const () as usize => Some(STALE_OWNER),
                _ => None,
            }
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            (owner == OWNER && self.owner_is_current.load(Ordering::Relaxed))
                || owner == OTHER_OWNER
        }
    }

    struct ClosingOwners {
        current_checks: AtomicUsize,
    }

    impl AddressOwnerResolver for ClosingOwners {
        fn owner_for_address(&self, address: NonZeroUsize) -> Option<OwnerToken> {
            (address.get() == count as *const () as usize).then_some(OWNER)
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            owner == OWNER && self.current_checks.fetch_add(1, Ordering::Relaxed) < 3
        }
    }

    fn api() -> (EventApi, Arc<AddonCallerResolver>) {
        let callers = Arc::new(AddonCallerResolver::new(Arc::new(FixedOwner)));
        let failures = Arc::new(BackendFailures::new());
        let boundary = Arc::new(NativeCallBoundary::new(
            Arc::clone(&callers),
            NativeMemoryReader::default(),
            failures,
        ));
        (
            EventApi::new(boundary, Arc::new(EventService::new())),
            callers,
        )
    }

    fn mapped_api() -> (
        EventApi,
        Arc<AddonCallerResolver>,
        Arc<EventService>,
        Arc<MappedOwners>,
    ) {
        let owners = Arc::new(MappedOwners::new());
        let callers = Arc::new(AddonCallerResolver::new(owners.clone()));
        let failures = Arc::new(BackendFailures::new());
        let boundary = Arc::new(NativeCallBoundary::new(
            Arc::clone(&callers),
            NativeMemoryReader::default(),
            failures,
        ));
        let service = Arc::new(EventService::new());
        (
            EventApi::new(boundary, Arc::clone(&service)),
            callers,
            service,
            owners,
        )
    }

    #[test]
    fn registration_rolls_back_if_cleanup_closes_after_attribution() {
        let owners = Arc::new(ClosingOwners {
            current_checks: AtomicUsize::new(0),
        });
        let callers = Arc::new(AddonCallerResolver::new(owners));
        let failures = Arc::new(BackendFailures::new());
        let boundary = Arc::new(NativeCallBoundary::new(
            Arc::clone(&callers),
            NativeMemoryReader::default(),
            failures,
        ));
        let service = Arc::new(EventService::new());
        let api = EventApi::new(boundary, Arc::clone(&service));
        let identifier = CString::new("EV_CLOSE_RACE").expect("test identifier");

        // SAFETY: the static callback remains executable for the service
        // lifetime and this fixture never dispatches a payload to it.
        unsafe {
            service
                .subscribe_native(OWNER, "EV_CLOSE_RACE", Some(count))
                .expect("preexisting duplicate fixture");
        }
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");
        assert!(api.subscribe(identifier.as_ptr(), Some(count)).is_err());
        assert_eq!(service.subscription_count(), 1);
    }

    #[test]
    fn completed_cleanup_rejects_late_backend_publication_for_the_same_generation() {
        let (api, callers, service, _owners) = mapped_api();
        let identifier = CString::new("EV_RETIRED_OWNER").expect("test identifier");
        assert!(service.cleanup_owner(OWNER).quiescent());
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");
        let rejected_before = api.boundary.failures().snapshot().service_rejected;

        assert!(api.subscribe(identifier.as_ptr(), Some(count)).is_err());
        assert_eq!(service.subscription_count(), 0);
        assert_eq!(
            api.boundary.failures().snapshot().service_rejected,
            rejected_before + 1
        );
    }

    #[test]
    fn copied_identifier_and_generation_exact_scope_drive_dispatch() {
        CALLS.store(0, Ordering::Relaxed);
        let (api, callers) = api();
        let identifier = CString::new("EV_TEST").expect("test identifier");
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");

        api.subscribe(identifier.as_ptr(), Some(count))
            .expect("subscribe");
        api.raise_notification(identifier.as_ptr()).expect("raise");

        assert_eq!(CALLS.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn rejected_service_request_increments_only_the_closed_counter() {
        let (api, callers) = api();
        let identifier = CString::new("EV_TEST").expect("test identifier");
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");

        assert!(api.subscribe(identifier.as_ptr(), None).is_err());
        assert_eq!(api.boundary.failures().snapshot().service_rejected, 1);
    }

    #[test]
    fn another_owner_cannot_impersonate_a_callback_during_unsubscribe() {
        let (api, callers, service, _owners) = mapped_api();
        let identifier = CString::new("EV_OWNER_AUTH").expect("test identifier");

        {
            let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");
            api.subscribe(identifier.as_ptr(), Some(count))
                .expect("owner subscription");
        }
        {
            let _scope = callers
                .enter_owner_scope(OTHER_OWNER)
                .expect("other owner scope");
            api.subscribe(identifier.as_ptr(), Some(other_callback))
                .expect("other subscription");
        }

        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");
        let failures_before = api.boundary.failures().snapshot().caller_attribution;
        assert!(
            api.unsubscribe(identifier.as_ptr(), Some(other_callback))
                .is_err()
        );
        assert_eq!(service.subscription_count(), 2);
        assert_eq!(
            api.boundary.failures().snapshot().caller_attribution,
            failures_before + 1
        );
    }

    #[test]
    fn stale_callback_generation_is_rejected_without_mutating_subscriptions() {
        let (api, callers, service, _owners) = mapped_api();
        let identifier = CString::new("EV_STALE_CALLBACK").expect("test identifier");
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");
        api.subscribe(identifier.as_ptr(), Some(count))
            .expect("owner subscription");

        assert!(
            api.unsubscribe(identifier.as_ptr(), Some(stale_callback))
                .is_err()
        );
        assert_eq!(service.subscription_count(), 1);
    }

    #[test]
    fn stale_actual_caller_generation_is_rejected_before_unsubscribe() {
        let (api, callers, service, owners) = mapped_api();
        let identifier = CString::new("EV_STALE_CALLER").expect("test identifier");
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");
        api.subscribe(identifier.as_ptr(), Some(count))
            .expect("owner subscription");
        owners.retire_owner();

        assert!(api.unsubscribe(identifier.as_ptr(), Some(count)).is_err());
        assert_eq!(service.subscription_count(), 1);
    }

    #[test]
    fn unsubscribe_removes_only_the_exact_owner_generation() {
        let (api, callers, service, _owners) = mapped_api();
        let identifier = CString::new("EV_EXACT_GENERATION").expect("test identifier");
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");
        api.subscribe(identifier.as_ptr(), Some(count))
            .expect("owner subscription");
        // SAFETY: the static test callback remains executable for the service
        // lifetime and the fixture never dispatches an incompatible payload.
        unsafe {
            service
                .subscribe_native(NEXT_GENERATION, "EV_EXACT_GENERATION", Some(count))
                .expect("next-generation fixture subscription");
        }

        assert_eq!(
            api.unsubscribe(identifier.as_ptr(), Some(count))
                .expect("owner unsubscribe"),
            1
        );
        assert_eq!(service.subscription_count(), 1);
    }
}
