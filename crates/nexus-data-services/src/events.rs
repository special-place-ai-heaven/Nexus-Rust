use core::ffi::{c_char, c_void};
use std::sync::Arc;

use nexus_core::{
    CallbackId, DispatchReport, EventBus, EventHandler, EventOwnerRetirement,
    EventRegistrationError, OwnerToken, Subscription, SubscriptionToken,
};
use thiserror::Error;

use crate::name::{NameError, ValidatedName, identifier_from_c};

/// Native Nexus event consumer callback.
pub type NativeEventCallback = unsafe extern "C" fn(*mut c_void);

/// Redaction-safe event-service boundary failures.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum EventServiceError {
    /// The event identifier was invalid.
    #[error("invalid event identifier: {0}")]
    InvalidIdentifier(#[source] NameError),
    /// A subscription did not provide a callback.
    #[error("the event callback is null")]
    MissingCallback,
    /// Cleanup already retired this exact owner generation.
    #[error("the event owner generation is retired")]
    OwnerRetired,
}

/// Owner-generation-aware ordered event service.
///
/// Dispatch snapshots subscriptions before invocation, so callbacks may safely
/// subscribe or unsubscribe. `nexus-core` isolates Rust panics per callback.
#[derive(Default)]
pub struct EventService {
    bus: EventBus,
}

impl EventService {
    /// Creates an empty event service.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a validated safe handler in registration order.
    pub fn subscribe_handler(
        &self,
        owner: OwnerToken,
        identifier: &str,
        callback_id: CallbackId,
        handler: Arc<EventHandler>,
    ) -> Result<(), EventServiceError> {
        self.subscribe_handler_tracked(owner, identifier, callback_id, handler)
            .map(drop)
    }

    /// Registers a safe handler and returns its exact insertion token.
    pub fn subscribe_handler_tracked(
        &self,
        owner: OwnerToken,
        identifier: &str,
        callback_id: CallbackId,
        handler: Arc<EventHandler>,
    ) -> Result<SubscriptionToken, EventServiceError> {
        let identifier =
            ValidatedName::identifier(identifier).map_err(EventServiceError::InvalidIdentifier)?;
        self.bus
            .subscribe(
                identifier.into_string(),
                Subscription::new(owner, callback_id, handler),
            )
            .map_err(|error| match error {
                EventRegistrationError::OwnerRetired => EventServiceError::OwnerRetired,
            })
    }

    /// Registers one native callback for an explicit addon generation.
    ///
    /// # Safety
    ///
    /// `callback` must remain executable through every invocation admitted
    /// before its subscription is removed or `owner` cleanup reaches
    /// quiescence. Reentrant unsubscription can return while current-thread or
    /// nested callback frames are still active, so that return alone does not
    /// end the executable lifetime. The callback must accept every payload
    /// raised for this identifier according to that event's native contract.
    pub unsafe fn subscribe_native(
        &self,
        owner: OwnerToken,
        identifier: &str,
        callback: Option<NativeEventCallback>,
    ) -> Result<(), EventServiceError> {
        // SAFETY: forwarded from this method's callback lifetime and payload
        // contract.
        unsafe { self.subscribe_native_tracked(owner, identifier, callback) }.map(drop)
    }

    /// Registers one native callback and returns its exact insertion token.
    ///
    /// # Safety
    ///
    /// `callback` must remain executable through every invocation admitted
    /// before the returned insertion is removed or `owner` cleanup reaches
    /// quiescence. Reentrant removal can return while current-thread or nested
    /// callback frames are still active, so that return alone does not end the
    /// executable lifetime. It must accept every payload raised for
    /// `identifier` according to that event's native contract.
    pub unsafe fn subscribe_native_tracked(
        &self,
        owner: OwnerToken,
        identifier: &str,
        callback: Option<NativeEventCallback>,
    ) -> Result<SubscriptionToken, EventServiceError> {
        let callback = callback.ok_or(EventServiceError::MissingCallback)?;
        let callback_id =
            CallbackId::new(callback as usize).ok_or(EventServiceError::MissingCallback)?;
        let handler: Arc<EventHandler> = Arc::new(move |data| {
            // SAFETY: native registration guarantees that the callback remains
            // executable through every admitted invocation, including frames
            // that outlive reentrant removal. Quiescent generation cleanup
            // removes the wrapper before the owning module is unloaded.
            unsafe { callback(data) };
        });
        self.subscribe_handler_tracked(owner, identifier, callback_id, handler)
    }

    /// Removes one exact insertion without affecting duplicate callbacks.
    pub fn unsubscribe_registration(
        &self,
        identifier: &str,
        token: &SubscriptionToken,
    ) -> Result<bool, EventServiceError> {
        let identifier =
            ValidatedName::identifier(identifier).map_err(EventServiceError::InvalidIdentifier)?;
        Ok(self
            .bus
            .unsubscribe_registration(identifier.as_str(), token))
    }

    /// Removes all matching callback registrations from one event.
    pub fn unsubscribe(
        &self,
        identifier: &str,
        callback_id: CallbackId,
    ) -> Result<usize, EventServiceError> {
        let identifier =
            ValidatedName::identifier(identifier).map_err(EventServiceError::InvalidIdentifier)?;
        Ok(self.bus.unsubscribe(identifier.as_str(), callback_id))
    }

    /// Removes matching callback registrations for one exact owner generation.
    pub fn unsubscribe_for_owner(
        &self,
        owner: OwnerToken,
        identifier: &str,
        callback_id: CallbackId,
    ) -> Result<usize, EventServiceError> {
        let identifier =
            ValidatedName::identifier(identifier).map_err(EventServiceError::InvalidIdentifier)?;
        Ok(self
            .bus
            .unsubscribe_owner(identifier.as_str(), owner, callback_id))
    }

    /// Removes one native callback identity from one event.
    pub fn unsubscribe_native(
        &self,
        identifier: &str,
        callback: Option<NativeEventCallback>,
    ) -> Result<usize, EventServiceError> {
        let Some(callback) = callback else {
            return Ok(0);
        };
        let Some(callback_id) = CallbackId::new(callback as usize) else {
            return Ok(0);
        };
        self.unsubscribe(identifier, callback_id)
    }

    /// Removes one native callback identity for one exact owner generation.
    pub fn unsubscribe_native_for_owner(
        &self,
        owner: OwnerToken,
        identifier: &str,
        callback: Option<NativeEventCallback>,
    ) -> Result<usize, EventServiceError> {
        let Some(callback) = callback else {
            return Ok(0);
        };
        let Some(callback_id) = CallbackId::new(callback as usize) else {
            return Ok(0);
        };
        self.unsubscribe_for_owner(owner, identifier, callback_id)
    }

    /// Retires every subscription owned by exactly one addon generation.
    ///
    /// A non-quiescent report must be retried after admitted callbacks return;
    /// native module unload is unsafe until [`EventOwnerRetirement::quiescent`]
    /// is true.
    pub fn cleanup_owner(&self, owner: OwnerToken) -> EventOwnerRetirement {
        self.bus.remove_owner(owner)
    }

    /// Raises an event to all subscribers in registration order.
    ///
    /// # Safety
    ///
    /// `data` must satisfy the event-specific payload contract for every
    /// native callback registered under `identifier` for the full dispatch.
    pub unsafe fn raise(
        &self,
        identifier: &str,
        data: *mut c_void,
    ) -> Result<DispatchReport, EventServiceError> {
        let identifier =
            ValidatedName::identifier(identifier).map_err(EventServiceError::InvalidIdentifier)?;
        Ok(self.bus.raise(identifier.as_str(), data))
    }

    /// Raises an event only to subscriptions with the requested signature.
    ///
    /// # Safety
    ///
    /// `data` must satisfy the event-specific payload contract for every
    /// matching native callback for the full dispatch.
    pub unsafe fn raise_targeted(
        &self,
        signature: u32,
        identifier: &str,
        data: *mut c_void,
    ) -> Result<DispatchReport, EventServiceError> {
        let identifier =
            ValidatedName::identifier(identifier).map_err(EventServiceError::InvalidIdentifier)?;
        Ok(self
            .bus
            .raise_targeted(signature, identifier.as_str(), data))
    }

    /// Returns the number of live callback registrations.
    #[must_use]
    pub fn subscription_count(&self) -> usize {
        self.bus.subscription_count()
    }

    /// Registers one native callback from a bounded C identifier.
    ///
    /// # Safety
    ///
    /// `identifier` must be a readable bounded C string. The callback must
    /// remain executable until it is unsubscribed or its owner generation is
    /// cleaned up.
    pub unsafe fn subscribe_native_abi(
        &self,
        owner: OwnerToken,
        identifier: *const c_char,
        callback: Option<NativeEventCallback>,
    ) -> Result<(), EventServiceError> {
        // SAFETY: forwarded from this method's native string contract.
        let identifier = unsafe { identifier_from_c(identifier) }
            .map_err(EventServiceError::InvalidIdentifier)?;
        // SAFETY: this method carries the same callback lifetime and payload
        // requirements in addition to validating the native string.
        unsafe { self.subscribe_native(owner, identifier.as_str(), callback) }
    }

    /// Removes one native callback using a bounded C identifier.
    ///
    /// # Safety
    ///
    /// `identifier` must be a readable bounded C string.
    pub unsafe fn unsubscribe_native_abi(
        &self,
        identifier: *const c_char,
        callback: Option<NativeEventCallback>,
    ) -> Result<usize, EventServiceError> {
        // SAFETY: forwarded from this method's native string contract.
        let identifier = unsafe { identifier_from_c(identifier) }
            .map_err(EventServiceError::InvalidIdentifier)?;
        self.unsubscribe_native(identifier.as_str(), callback)
    }

    /// Raises one native event after bounded C-string validation.
    ///
    /// # Safety
    ///
    /// `identifier` must be a readable bounded C string. `data` follows the
    /// event-specific borrowed payload contract for the duration of dispatch.
    pub unsafe fn raise_abi(
        &self,
        identifier: *const c_char,
        data: *mut c_void,
    ) -> Result<DispatchReport, EventServiceError> {
        // SAFETY: forwarded from this method's native string contract.
        let identifier = unsafe { identifier_from_c(identifier) }
            .map_err(EventServiceError::InvalidIdentifier)?;
        // SAFETY: forwarded from this method's event payload contract.
        unsafe { self.raise(identifier.as_str(), data) }
    }

    /// Raises one targeted native event after bounded C-string validation.
    ///
    /// # Safety
    ///
    /// `identifier` must be a readable bounded C string. `data` follows the
    /// event-specific borrowed payload contract for the duration of dispatch.
    pub unsafe fn raise_targeted_abi(
        &self,
        signature: u32,
        identifier: *const c_char,
        data: *mut c_void,
    ) -> Result<DispatchReport, EventServiceError> {
        // SAFETY: forwarded from this method's native string contract.
        let identifier = unsafe { identifier_from_c(identifier) }
            .map_err(EventServiceError::InvalidIdentifier)?;
        // SAFETY: forwarded from this method's event payload contract.
        unsafe { self.raise_targeted(signature, identifier.as_str(), data) }
    }
}

#[cfg(test)]
mod tests {
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use nexus_core::{CallbackId, EventHandler, OwnerToken};

    use super::{EventService, EventServiceError};

    fn callback_id(value: usize) -> CallbackId {
        CallbackId::new(value).expect("test callback identities are non-zero")
    }

    unsafe extern "C" fn count_native(data: *mut c_void) {
        // SAFETY: the native-boundary test passes a live `AtomicUsize` for the
        // synchronous duration of dispatch.
        let counter = unsafe { &*data.cast::<AtomicUsize>() };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn dispatch_is_ordered_targeted_and_panic_isolated() {
        let events = EventService::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        for (id, owner, should_panic) in [
            (
                1,
                OwnerToken {
                    signature: 7,
                    generation: 1,
                },
                false,
            ),
            (
                2,
                OwnerToken {
                    signature: 9,
                    generation: 1,
                },
                true,
            ),
            (
                3,
                OwnerToken {
                    signature: 7,
                    generation: 2,
                },
                false,
            ),
        ] {
            let calls = Arc::clone(&calls);
            let handler: Arc<EventHandler> = Arc::new(move |_| {
                calls.lock().expect("test mutex is not poisoned").push(id);
                assert!(!should_panic, "intentional callback panic");
            });
            events
                .subscribe_handler(owner, "EV_ORDERED", callback_id(id), handler)
                .expect("test subscription should succeed");
        }

        // SAFETY: these test handlers ignore the null payload.
        let report = unsafe { events.raise("EV_ORDERED", core::ptr::null_mut::<c_void>()) }
            .expect("test event name should be valid");
        assert_eq!(report.invoked, 3);
        assert_eq!(report.panicked, 1);
        assert_eq!(
            *calls.lock().expect("test mutex is not poisoned"),
            [1, 2, 3]
        );

        calls.lock().expect("test mutex is not poisoned").clear();
        // SAFETY: these test handlers ignore the null payload.
        let targeted = unsafe { events.raise_targeted(7, "EV_ORDERED", core::ptr::null_mut()) }
            .expect("test event name should be valid");
        assert_eq!(targeted.invoked, 2);
        assert_eq!(*calls.lock().expect("test mutex is not poisoned"), [1, 3]);
    }

    #[test]
    fn generation_cleanup_never_removes_a_reloaded_owner() {
        let events = EventService::new();
        let handler: Arc<EventHandler> = Arc::new(|_| {});
        let stale = OwnerToken {
            signature: 42,
            generation: 3,
        };
        let current = OwnerToken {
            signature: 42,
            generation: 4,
        };
        events
            .subscribe_handler(stale, "EV_RELOAD", callback_id(1), Arc::clone(&handler))
            .expect("test subscription should succeed");
        events
            .subscribe_handler(current, "EV_RELOAD", callback_id(2), handler)
            .expect("test subscription should succeed");

        let cleanup = events.cleanup_owner(stale);
        assert_eq!(cleanup.retired(), 1);
        assert!(cleanup.quiescent());
        assert_eq!(events.subscription_count(), 1);
        assert_eq!(
            events
                .subscribe_handler(stale, "EV_RELOAD", callback_id(3), Arc::new(|_| {}),)
                .expect_err("retired generation must not republish callbacks"),
            EventServiceError::OwnerRetired
        );
        // SAFETY: the remaining test handler ignores the null payload.
        let report = unsafe { events.raise_targeted(42, "EV_RELOAD", core::ptr::null_mut()) }
            .expect("test event name should be valid");
        assert_eq!(report.invoked, 1);
    }

    #[test]
    fn native_unsubscribe_is_scoped_to_the_authenticated_owner_generation() {
        let events = EventService::new();
        let stale = OwnerToken {
            signature: 42,
            generation: 3,
        };
        let current = OwnerToken {
            signature: 42,
            generation: 4,
        };

        // SAFETY: the test keeps `count_native` executable for the complete
        // service lifetime and raises only its documented counter payload.
        unsafe {
            events
                .subscribe_native(stale, "EV_RELOAD", Some(count_native))
                .expect("stale test subscription should succeed");
            events
                .subscribe_native(current, "EV_RELOAD", Some(count_native))
                .expect("current test subscription should succeed");
        }

        assert_eq!(
            events
                .unsubscribe_native_for_owner(stale, "EV_RELOAD", Some(count_native))
                .expect("test event name should be valid"),
            1
        );
        assert_eq!(events.subscription_count(), 1);

        let counter = AtomicUsize::new(0);
        // SAFETY: `count_native` expects the live counter pointer supplied for
        // this synchronous dispatch.
        unsafe {
            events
                .raise("EV_RELOAD", (&raw const counter).cast_mut().cast())
                .expect("test event name should be valid");
        }
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn native_boundary_uses_explicit_owner_lifetime_and_payload_contracts() {
        let events = EventService::new();
        let owner = OwnerToken {
            signature: 77,
            generation: 5,
        };
        // SAFETY: `count_native` is a static function and remains executable
        // through the explicit owner cleanup below.
        unsafe { events.subscribe_native(owner, "EV_NATIVE", Some(count_native)) }
            .expect("the native fixture should subscribe");

        let counter = AtomicUsize::new(0);
        // SAFETY: the synchronous callback receives a live `AtomicUsize` and
        // the registered callback accepts exactly that payload.
        let report = unsafe {
            events.raise(
                "EV_NATIVE",
                (&raw const counter).cast_mut().cast::<c_void>(),
            )
        }
        .expect("the native fixture should dispatch");
        assert_eq!(report.invoked, 1);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        let cleanup = events.cleanup_owner(owner);
        assert_eq!(cleanup.retired(), 1);
        assert!(cleanup.quiescent());
    }
}
