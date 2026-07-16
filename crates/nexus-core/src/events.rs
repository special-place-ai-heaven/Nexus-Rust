use std::{
    collections::HashMap,
    ffi::c_void,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

/// Identifies one loaded generation of a native addon.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OwnerToken {
    /// Addon signature exposed through the legacy ABI.
    pub signature: u32,
    /// Monotonic load generation, preventing a reload from inheriting stale registrations.
    pub generation: u64,
}

/// Stable identity of a foreign callback address.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CallbackId(NonZeroUsize);

impl CallbackId {
    /// Creates an identity from a non-null function address.
    #[must_use]
    pub const fn new(address: usize) -> Option<Self> {
        match NonZeroUsize::new(address) {
            Some(address) => Some(Self(address)),
            None => None,
        }
    }

    /// Returns the original function address.
    #[must_use]
    pub const fn address(self) -> usize {
        self.0.get()
    }
}

/// Safe callable wrapper installed for a native event callback.
pub type EventHandler = dyn Fn(*mut c_void) + Send + Sync + 'static;

/// One ordered event subscription.
#[derive(Clone)]
pub struct Subscription {
    /// Addon generation that owns the registration.
    pub owner: OwnerToken,
    /// Foreign function address used for legacy unsubscribe behavior.
    pub callback_id: CallbackId,
    handler: Arc<EventHandler>,
}

impl Subscription {
    /// Creates a subscription around a validated callback wrapper.
    #[must_use]
    pub fn new(owner: OwnerToken, callback_id: CallbackId, handler: Arc<EventHandler>) -> Self {
        Self {
            owner,
            callback_id,
            handler,
        }
    }
}

/// Result of a synchronous event dispatch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DispatchReport {
    /// Callbacks that were invoked in registration order.
    pub invoked: usize,
    /// Callbacks whose Rust wrapper panicked and was isolated.
    pub panicked: usize,
}

/// Synchronous ordered event registry matching the native addon contract.
#[derive(Default)]
pub struct EventBus {
    subscriptions: RwLock<HashMap<String, Vec<Subscription>>>,
}

impl EventBus {
    /// Creates an empty event registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a subscription. Duplicate callbacks are intentionally retained.
    pub fn subscribe(&self, identifier: impl Into<String>, subscription: Subscription) {
        write_lock(&self.subscriptions)
            .entry(identifier.into())
            .or_default()
            .push(subscription);
    }

    /// Removes every matching callback from one event, matching legacy behavior.
    pub fn unsubscribe(&self, identifier: &str, callback_id: CallbackId) -> usize {
        let removed_subscriptions = {
            let mut subscriptions = write_lock(&self.subscriptions);
            let Some(callbacks) = subscriptions.get_mut(identifier) else {
                return 0;
            };
            let removed = callbacks
                .extract_if(.., |subscription| subscription.callback_id == callback_id)
                .collect::<Vec<_>>();
            if callbacks.is_empty() {
                subscriptions.remove(identifier);
            }
            removed
        };

        let removed = removed_subscriptions.len();
        drop(removed_subscriptions);
        removed
    }

    /// Removes all registrations owned by one addon generation.
    pub fn remove_owner(&self, owner: OwnerToken) -> usize {
        let removed_subscriptions = {
            let mut subscriptions = write_lock(&self.subscriptions);
            let mut removed = Vec::new();
            subscriptions.retain(|_, callbacks| {
                removed
                    .extend(callbacks.extract_if(.., |subscription| subscription.owner == owner));
                !callbacks.is_empty()
            });
            removed
        };

        let removed = removed_subscriptions.len();
        drop(removed_subscriptions);
        removed
    }

    /// Raises an event to all subscribers in registration order.
    pub fn raise(&self, identifier: &str, data: *mut c_void) -> DispatchReport {
        self.dispatch(identifier, data, |_| true)
    }

    /// Raises an event only to subscriptions with the requested addon signature.
    pub fn raise_targeted(
        &self,
        signature: u32,
        identifier: &str,
        data: *mut c_void,
    ) -> DispatchReport {
        self.dispatch(identifier, data, |owner| owner.signature == signature)
    }

    /// Returns the number of live subscriptions for diagnostics.
    #[must_use]
    pub fn subscription_count(&self) -> usize {
        read_lock(&self.subscriptions).values().map(Vec::len).sum()
    }

    fn dispatch(
        &self,
        identifier: &str,
        data: *mut c_void,
        filter: impl Fn(OwnerToken) -> bool,
    ) -> DispatchReport {
        let callbacks = read_lock(&self.subscriptions)
            .get(identifier)
            .into_iter()
            .flatten()
            .filter(|subscription| filter(subscription.owner))
            .map(|subscription| Arc::clone(&subscription.handler))
            .collect::<Vec<_>>();

        let mut report = DispatchReport::default();
        for callback in callbacks {
            report.invoked += 1;
            match catch_unwind(AssertUnwindSafe(|| callback(data))) {
                Ok(()) => {}
                Err(payload) => {
                    // A custom panic payload may itself panic from Drop. Forgetting it
                    // prevents that destructor from reopening the unwind boundary.
                    core::mem::forget(payload);
                    report.panicked += 1;
                }
            }
        }
        report
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, Weak};

    use super::{CallbackId, EventBus, OwnerToken, Subscription};

    fn callback_id(value: usize) -> CallbackId {
        CallbackId::new(value).unwrap_or_else(|| panic!("test callback ID must be nonzero"))
    }

    struct ReentrantHandlerDrop {
        bus: Weak<EventBus>,
        lock_was_free: Arc<AtomicBool>,
        reentered: Arc<AtomicBool>,
    }

    impl Drop for ReentrantHandlerDrop {
        fn drop(&mut self) {
            let Some(bus) = self.bus.upgrade() else {
                return;
            };
            let lock_was_free = bus.subscriptions.try_write().is_ok();
            self.lock_was_free.store(lock_was_free, Ordering::SeqCst);
            if lock_was_free {
                bus.subscribe(
                    "drop-reentry",
                    Subscription::new(
                        OwnerToken {
                            signature: u32::MAX,
                            generation: u64::MAX,
                        },
                        callback_id(usize::MAX),
                        Arc::new(|_| {}),
                    ),
                );
                self.reentered.store(true, Ordering::SeqCst);
            }
        }
    }

    #[test]
    fn dispatches_duplicates_in_registration_order() {
        let bus = EventBus::new();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let owner = OwnerToken {
            signature: 7,
            generation: 1,
        };

        for value in [1, 2, 1] {
            let observed = Arc::clone(&observed);
            bus.subscribe(
                "event",
                Subscription::new(
                    owner,
                    callback_id(value),
                    Arc::new(move |_| {
                        observed
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(value);
                    }),
                ),
            );
        }

        assert_eq!(bus.raise("event", core::ptr::null_mut()).invoked, 3);
        assert_eq!(
            *observed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![1, 2, 1]
        );
        assert_eq!(bus.unsubscribe("event", callback_id(1)), 2);
        assert_eq!(bus.subscription_count(), 1);
    }

    #[test]
    fn snapshots_before_reentrant_callbacks() {
        let bus = Arc::new(EventBus::new());
        let owner = OwnerToken {
            signature: 4,
            generation: 2,
        };
        let bus_for_callback = Arc::clone(&bus);
        bus.subscribe(
            "event",
            Subscription::new(
                owner,
                callback_id(1),
                Arc::new(move |_| {
                    bus_for_callback.subscribe(
                        "event",
                        Subscription::new(owner, callback_id(2), Arc::new(|_| {})),
                    );
                }),
            ),
        );

        assert_eq!(bus.raise("event", core::ptr::null_mut()).invoked, 1);
        assert_eq!(bus.raise("event", core::ptr::null_mut()).invoked, 2);
    }

    #[test]
    fn targeted_dispatch_and_generation_cleanup_are_exact() {
        let bus = EventBus::new();
        let old = OwnerToken {
            signature: 10,
            generation: 1,
        };
        let current = OwnerToken {
            signature: 10,
            generation: 2,
        };
        let other = OwnerToken {
            signature: 11,
            generation: 1,
        };
        for (index, owner) in [old, current, other].into_iter().enumerate() {
            bus.subscribe(
                "event",
                Subscription::new(owner, callback_id(index + 1), Arc::new(|_| {})),
            );
        }

        assert_eq!(
            bus.raise_targeted(10, "event", core::ptr::null_mut())
                .invoked,
            2
        );
        assert_eq!(bus.remove_owner(old), 1);
        assert_eq!(bus.subscription_count(), 2);
    }

    #[test]
    fn removal_drops_handlers_after_unlock_and_allows_reentrancy() {
        let bus = Arc::new(EventBus::new());
        let owner = OwnerToken {
            signature: 10,
            generation: 3,
        };

        for (identifier, callback, remove_by_owner) in
            [("unsubscribe", 1, false), ("owner-cleanup", 2, true)]
        {
            let lock_was_free = Arc::new(AtomicBool::new(false));
            let reentered = Arc::new(AtomicBool::new(false));
            let probe = ReentrantHandlerDrop {
                bus: Arc::downgrade(&bus),
                lock_was_free: Arc::clone(&lock_was_free),
                reentered: Arc::clone(&reentered),
            };
            bus.subscribe(
                identifier,
                Subscription::new(
                    owner,
                    callback_id(callback),
                    Arc::new(move |_| {
                        let _keep_probe_alive = &probe;
                    }),
                ),
            );

            let removed = if remove_by_owner {
                bus.remove_owner(owner)
            } else {
                bus.unsubscribe(identifier, callback_id(callback))
            };

            assert_eq!(removed, 1);
            assert!(lock_was_free.load(Ordering::SeqCst));
            assert!(reentered.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn isolates_panicking_callback_wrappers() {
        let bus = EventBus::new();
        let owner = OwnerToken {
            signature: 1,
            generation: 1,
        };
        bus.subscribe(
            "event",
            Subscription::new(owner, callback_id(1), Arc::new(|_| panic!("expected"))),
        );
        bus.subscribe(
            "event",
            Subscription::new(owner, callback_id(2), Arc::new(|_| {})),
        );

        let report = bus.raise("event", core::ptr::null_mut());
        assert_eq!(report.invoked, 2);
        assert_eq!(report.panicked, 1);
    }

    #[test]
    fn panic_payload_destructors_cannot_reopen_dispatch_unwinding() {
        struct PanicOnDrop(Arc<AtomicUsize>);

        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("panic payload destructor must not run");
            }
        }

        let bus = EventBus::new();
        let payload_drops = Arc::new(AtomicUsize::new(0));
        let payload_drops_for_callback = Arc::clone(&payload_drops);
        bus.subscribe(
            "event",
            Subscription::new(
                OwnerToken {
                    signature: 1,
                    generation: 2,
                },
                callback_id(1),
                Arc::new(move |_| {
                    std::panic::panic_any(PanicOnDrop(Arc::clone(&payload_drops_for_callback)));
                }),
            ),
        );

        let report = bus.raise("event", core::ptr::null_mut());
        assert_eq!(report.invoked, 1);
        assert_eq!(report.panicked, 1);
        assert_eq!(payload_drops.load(Ordering::SeqCst), 0);
    }
}
