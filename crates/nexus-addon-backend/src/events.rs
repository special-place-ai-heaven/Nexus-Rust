use core::ffi::{c_char, c_void};
use core::fmt;
use core::num::NonZeroUsize;
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
        let owner = self.boundary.resolve_owner(callback_hint(callback))?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let result = unsafe {
            // SAFETY: caller attribution binds callback lifetime to one owner
            // generation. Composite teardown removes it before module unload.
            self.service
                .subscribe_native(owner, identifier.as_str(), callback)
        };
        self.service_result(result)
    }

    /// Removes one callback identity after validating the calling generation.
    pub fn unsubscribe(
        &self,
        identifier: *const c_char,
        callback: Option<EventCallback>,
    ) -> Result<usize, BackendOperationError> {
        let _owner = self.boundary.resolve_owner(callback_hint(callback))?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let result = self
            .service
            .unsubscribe_native(identifier.as_str(), callback);
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

fn callback_hint(callback: Option<EventCallback>) -> Option<NonZeroUsize> {
    callback.and_then(|callback| NonZeroUsize::new(callback as usize))
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use core::ffi::c_void;
    use core::num::NonZeroUsize;
    use core::sync::atomic::{AtomicUsize, Ordering};
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

    #[test]
    fn copied_identifier_and_generation_exact_scope_drive_dispatch() {
        CALLS.store(0, Ordering::Relaxed);
        let (api, callers) = api();
        let identifier = CString::new("EV_TEST").expect("test identifier");

        api.subscribe(identifier.as_ptr(), Some(count))
            .expect("subscribe");
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");
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
}
