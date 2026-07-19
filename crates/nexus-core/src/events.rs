use std::{
    collections::HashMap,
    error::Error,
    ffi::c_void,
    fmt,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard},
    thread::{self, ThreadId},
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

#[derive(Default)]
struct AdmissionState {
    accepting: bool,
    in_flight: usize,
    by_thread: HashMap<ThreadId, usize>,
}

struct CallbackAdmission {
    state: Mutex<AdmissionState>,
    drained: Condvar,
}

impl CallbackAdmission {
    fn open() -> Self {
        Self {
            state: Mutex::new(AdmissionState {
                accepting: true,
                ..AdmissionState::default()
            }),
            drained: Condvar::new(),
        }
    }

    fn try_enter(self: &Arc<Self>) -> Option<CallbackPermit> {
        let thread = thread::current().id();
        let mut state = mutex_lock(&self.state);
        if !state.accepting {
            return None;
        }
        let in_flight = state.in_flight.checked_add(1)?;
        let thread_in_flight = state
            .by_thread
            .get(&thread)
            .copied()
            .unwrap_or(0)
            .checked_add(1)?;
        state.in_flight = in_flight;
        state.by_thread.insert(thread, thread_in_flight);
        drop(state);
        Some(CallbackPermit {
            admission: Arc::clone(self),
            thread,
        })
    }

    fn close(&self) {
        mutex_lock(&self.state).accepting = false;
    }

    fn wait_for_other_threads(&self) {
        let thread = thread::current().id();
        let mut state = mutex_lock(&self.state);
        loop {
            let current_thread = state.by_thread.get(&thread).copied().unwrap_or(0);
            if state.in_flight <= current_thread {
                return;
            }
            state = self
                .drained
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn in_flight(&self) -> usize {
        mutex_lock(&self.state).in_flight
    }

    fn leave(&self, thread: ThreadId) {
        let mut state = mutex_lock(&self.state);
        debug_assert!(state.in_flight > 0, "event callback counter underflow");
        state.in_flight = state.in_flight.saturating_sub(1);
        if let Some(thread_count) = state.by_thread.get_mut(&thread) {
            debug_assert!(*thread_count > 0, "event thread counter underflow");
            *thread_count = thread_count.saturating_sub(1);
            if *thread_count == 0 {
                state.by_thread.remove(&thread);
            }
        }
        self.drained.notify_all();
    }
}

struct CallbackPermit {
    admission: Arc<CallbackAdmission>,
    thread: ThreadId,
}

impl Drop for CallbackPermit {
    fn drop(&mut self) {
        self.admission.leave(self.thread);
    }
}

/// Closed reason why an event registration was not published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventRegistrationError {
    /// Cleanup already retired this owner generation.
    OwnerRetired,
}

impl fmt::Display for EventRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OwnerRetired => formatter.write_str("the event owner generation is retired"),
        }
    }
}

impl Error for EventRegistrationError {}

/// Result of retiring every event callback for one owner generation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventOwnerRetirement {
    retired: usize,
    in_flight: usize,
}

impl EventOwnerRetirement {
    /// Registrations newly removed from event lookup during this attempt.
    #[must_use]
    pub const fn retired(self) -> usize {
        self.retired
    }

    /// Already-admitted callbacks that still have native frames in flight.
    #[must_use]
    pub const fn in_flight(self) -> usize {
        self.in_flight
    }

    /// Returns whether module retirement may safely continue.
    #[must_use]
    pub const fn quiescent(self) -> bool {
        self.in_flight == 0
    }
}

/// Opaque identity for one exact event subscription insertion.
///
/// Cloning this token does not keep the callback registered and dropping it
/// does not unsubscribe. It allows precise rollback without disturbing an
/// older duplicate callback.
#[derive(Clone, Debug)]
pub struct SubscriptionToken {
    marker: Arc<()>,
}

/// One ordered event subscription.
pub struct Subscription {
    /// Addon generation that owns the registration.
    pub owner: OwnerToken,
    /// Foreign function address used for legacy unsubscribe behavior.
    pub callback_id: CallbackId,
    handler: Arc<EventHandler>,
    admission: Arc<CallbackAdmission>,
    token: SubscriptionToken,
}

impl Subscription {
    /// Creates a subscription around a validated callback wrapper.
    #[must_use]
    pub fn new(owner: OwnerToken, callback_id: CallbackId, handler: Arc<EventHandler>) -> Self {
        Self {
            owner,
            callback_id,
            handler,
            admission: Arc::new(CallbackAdmission::open()),
            token: SubscriptionToken {
                marker: Arc::new(()),
            },
        }
    }

    fn token(&self) -> SubscriptionToken {
        self.token.clone()
    }

    fn close(&self) {
        self.admission.close();
    }

    fn wait_for_other_threads(&self) {
        self.admission.wait_for_other_threads();
    }

    fn dispatch_entry(&self) -> DispatchEntry {
        DispatchEntry {
            owner: self.owner,
            handler: Arc::clone(&self.handler),
            admission: Arc::clone(&self.admission),
        }
    }
}

struct DispatchEntry {
    owner: OwnerToken,
    handler: Arc<EventHandler>,
    admission: Arc<CallbackAdmission>,
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
struct EventRegistry {
    subscriptions: HashMap<String, Vec<Subscription>>,
    retired_generations: HashMap<u32, u64>,
    retiring: HashMap<OwnerToken, Vec<Arc<CallbackAdmission>>>,
}

/// Synchronous ordered event registry matching the native addon contract.
#[derive(Default)]
pub struct EventBus {
    registry: RwLock<EventRegistry>,
}

#[derive(Default)]
struct RetirementBatch {
    subscriptions: Vec<Subscription>,
    owners: Vec<OwnerToken>,
}

impl EventBus {
    /// Creates an empty event registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a subscription. Duplicate callbacks are intentionally retained.
    ///
    /// Publication and owner retirement share one write lock, so a cleanup
    /// that wins the race permanently rejects this generation while a
    /// subscription that wins is observed and retired by cleanup.
    pub fn subscribe(
        &self,
        identifier: impl Into<String>,
        subscription: Subscription,
    ) -> Result<SubscriptionToken, EventRegistrationError> {
        let token = subscription.token();
        let mut registry = write_lock(&self.registry);
        if registry
            .retired_generations
            .get(&subscription.owner.signature)
            .is_some_and(|generation| subscription.owner.generation <= *generation)
        {
            return Err(EventRegistrationError::OwnerRetired);
        }
        registry
            .subscriptions
            .entry(identifier.into())
            .or_default()
            .push(subscription);
        Ok(token)
    }

    /// Removes one exact insertion previously returned by [`Self::subscribe`].
    pub fn unsubscribe_registration(&self, identifier: &str, token: &SubscriptionToken) -> bool {
        let batch = self.retire_matching(identifier, |subscription| {
            Arc::ptr_eq(&subscription.token.marker, &token.marker)
        });
        let found = !batch.subscriptions.is_empty();
        self.finish_exact_retirement(batch);
        found
    }

    /// Removes every matching callback from one event, matching legacy behavior.
    pub fn unsubscribe(&self, identifier: &str, callback_id: CallbackId) -> usize {
        let batch = self.retire_matching(identifier, |subscription| {
            subscription.callback_id == callback_id
        });
        self.finish_exact_retirement(batch)
    }

    /// Removes matching callbacks owned by exactly one addon generation.
    ///
    /// This is the mutation primitive for addon-originated unsubscription.
    /// Host cleanup may continue to use [`Self::remove_owner`], while trusted
    /// compatibility code that intentionally spans owners may use
    /// [`Self::unsubscribe`].
    pub fn unsubscribe_owner(
        &self,
        identifier: &str,
        owner: OwnerToken,
        callback_id: CallbackId,
    ) -> usize {
        let batch = self.retire_matching(identifier, |subscription| {
            subscription.owner == owner && subscription.callback_id == callback_id
        });
        self.finish_exact_retirement(batch)
    }

    /// Retires all registrations owned by one addon generation.
    ///
    /// Cleanup closes callback admission and removes registry entries in one
    /// critical section. It does not block on an already-running callback, so
    /// reentrant cleanup is deadlock-free; callers must require a quiescent
    /// report before unloading native code and retry otherwise.
    pub fn remove_owner(&self, owner: OwnerToken) -> EventOwnerRetirement {
        let batch = {
            let mut registry = write_lock(&self.registry);
            let generation = registry
                .retired_generations
                .entry(owner.signature)
                .or_insert(owner.generation);
            *generation = (*generation).max(owner.generation);

            let mut removed = Vec::new();
            registry.subscriptions.retain(|_, callbacks| {
                removed
                    .extend(callbacks.extract_if(.., |subscription| subscription.owner == owner));
                !callbacks.is_empty()
            });
            let owners = Self::stage_retirement(&mut registry, &removed);
            RetirementBatch {
                subscriptions: removed,
                owners,
            }
        };
        let retired = batch.subscriptions.len();
        drop(batch.subscriptions);
        self.owner_retirement(owner, retired)
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
        read_lock(&self.registry)
            .subscriptions
            .values()
            .fold(0_usize, |count, subscriptions| {
                count.saturating_add(subscriptions.len())
            })
    }

    fn dispatch(
        &self,
        identifier: &str,
        data: *mut c_void,
        filter: impl Fn(OwnerToken) -> bool,
    ) -> DispatchReport {
        let callbacks = read_lock(&self.registry)
            .subscriptions
            .get(identifier)
            .into_iter()
            .flatten()
            .filter(|subscription| filter(subscription.owner))
            .map(Subscription::dispatch_entry)
            .collect::<Vec<_>>();

        let mut report = DispatchReport::default();
        let mut owners = Vec::new();
        for callback in callbacks {
            if !owners.contains(&callback.owner) {
                owners.push(callback.owner);
            }
            let Some(_permit) = callback.admission.try_enter() else {
                continue;
            };
            report.invoked = report.invoked.saturating_add(1);
            match catch_unwind(AssertUnwindSafe(|| (callback.handler)(data))) {
                Ok(()) => {}
                Err(payload) => {
                    // A custom panic payload may itself panic from Drop. Forgetting it
                    // prevents that destructor from reopening the unwind boundary.
                    core::mem::forget(payload);
                    report.panicked = report.panicked.saturating_add(1);
                }
            }
        }
        for owner in owners {
            self.reap_quiescent(owner);
        }
        report
    }

    fn retire_matching(
        &self,
        identifier: &str,
        mut matches: impl FnMut(&Subscription) -> bool,
    ) -> RetirementBatch {
        let mut registry = write_lock(&self.registry);
        let Some(callbacks) = registry.subscriptions.get_mut(identifier) else {
            return RetirementBatch::default();
        };
        let removed = callbacks
            .extract_if(.., |subscription| matches(subscription))
            .collect::<Vec<_>>();
        if callbacks.is_empty() {
            registry.subscriptions.remove(identifier);
        }
        let owners = Self::stage_retirement(&mut registry, &removed);
        RetirementBatch {
            subscriptions: removed,
            owners,
        }
    }

    fn stage_retirement(
        registry: &mut EventRegistry,
        subscriptions: &[Subscription],
    ) -> Vec<OwnerToken> {
        let mut owners = Vec::new();
        for subscription in subscriptions {
            subscription.close();
            registry
                .retiring
                .entry(subscription.owner)
                .or_default()
                .push(Arc::clone(&subscription.admission));
            if !owners.contains(&subscription.owner) {
                owners.push(subscription.owner);
            }
        }
        owners
    }

    fn finish_exact_retirement(&self, batch: RetirementBatch) -> usize {
        let retired = batch.subscriptions.len();
        for subscription in &batch.subscriptions {
            subscription.wait_for_other_threads();
        }
        for owner in batch.owners {
            self.reap_quiescent(owner);
        }
        drop(batch.subscriptions);
        retired
    }

    fn owner_retirement(&self, owner: OwnerToken, retired: usize) -> EventOwnerRetirement {
        let (in_flight, drained) = {
            let mut registry = write_lock(&self.registry);
            let in_flight = registry
                .retiring
                .get(&owner)
                .into_iter()
                .flatten()
                .fold(0_usize, |in_flight, admission| {
                    in_flight.saturating_add(admission.in_flight())
                });
            let drained = (in_flight == 0)
                .then(|| registry.retiring.remove(&owner))
                .flatten()
                .unwrap_or_default();
            (in_flight, drained)
        };
        drop(drained);
        EventOwnerRetirement { retired, in_flight }
    }

    fn reap_quiescent(&self, owner: OwnerToken) {
        let drained = {
            let mut registry = write_lock(&self.registry);
            let quiescent = registry.retiring.get(&owner).is_none_or(|admissions| {
                admissions
                    .iter()
                    .all(|admission| admission.in_flight() == 0)
            });
            quiescent
                .then(|| registry.retiring.remove(&owner))
                .flatten()
                .unwrap_or_default()
        };
        drop(drained);
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex, Weak, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        CallbackId, EventBus, EventHandler, EventRegistrationError, OwnerToken, Subscription,
    };

    fn callback_id(value: usize) -> CallbackId {
        CallbackId::new(value).unwrap_or_else(|| panic!("test callback ID must be nonzero"))
    }

    #[test]
    fn admission_counter_overflow_is_rejected_without_partial_mutation() {
        let admission = Arc::new(super::CallbackAdmission::open());
        let thread = thread::current().id();
        {
            let mut state = super::mutex_lock(&admission.state);
            state.in_flight = usize::MAX;
        }
        assert!(admission.try_enter().is_none());
        {
            let state = super::mutex_lock(&admission.state);
            assert_eq!(state.in_flight, usize::MAX);
            assert!(state.by_thread.is_empty());
        }

        {
            let mut state = super::mutex_lock(&admission.state);
            state.in_flight = usize::MAX - 1;
            state.by_thread.insert(thread, usize::MAX);
        }
        assert!(admission.try_enter().is_none());
        let state = super::mutex_lock(&admission.state);
        assert_eq!(state.in_flight, usize::MAX - 1);
        assert_eq!(state.by_thread.get(&thread), Some(&usize::MAX));
    }

    #[test]
    fn retirement_aggregation_saturates_instead_of_wrapping() {
        let bus = EventBus::new();
        let owner = OwnerToken {
            signature: 2,
            generation: 1,
        };
        let first = Arc::new(super::CallbackAdmission::open());
        let second = Arc::new(super::CallbackAdmission::open());
        super::mutex_lock(&first.state).in_flight = usize::MAX - 1;
        super::mutex_lock(&second.state).in_flight = 2;
        super::write_lock(&bus.registry)
            .retiring
            .insert(owner, vec![first, second]);

        let report = bus.owner_retirement(owner, 0);
        assert_eq!(report.in_flight(), usize::MAX);
        assert!(!report.quiescent());
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
            let lock_was_free = bus.registry.try_write().is_ok();
            self.lock_was_free.store(lock_was_free, Ordering::SeqCst);
            if lock_was_free {
                let reentered = bus
                    .subscribe(
                        "drop-reentry",
                        Subscription::new(
                            OwnerToken {
                                signature: u32::MAX,
                                generation: u64::MAX,
                            },
                            callback_id(usize::MAX),
                            Arc::new(|_| {}),
                        ),
                    )
                    .is_ok();
                self.reentered.store(reentered, Ordering::SeqCst);
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
            )
            .expect("open owner should accept test subscription");
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
                    bus_for_callback
                        .subscribe(
                            "event",
                            Subscription::new(owner, callback_id(2), Arc::new(|_| {})),
                        )
                        .expect("reentrant subscription should remain admissible");
                }),
            ),
        )
        .expect("open owner should accept test subscription");

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
            )
            .expect("open owner should accept test subscription");
        }

        assert_eq!(
            bus.raise_targeted(10, "event", core::ptr::null_mut())
                .invoked,
            2
        );
        assert_eq!(bus.remove_owner(old).retired(), 1);
        assert_eq!(bus.subscription_count(), 2);
    }

    #[test]
    fn owner_scoped_unsubscribe_cannot_remove_another_generation() {
        let bus = EventBus::new();
        let stale = OwnerToken {
            signature: 10,
            generation: 1,
        };
        let current = OwnerToken {
            signature: 10,
            generation: 2,
        };
        let callback = callback_id(7);

        for owner in [stale, stale, current] {
            bus.subscribe(
                "event",
                Subscription::new(owner, callback, Arc::new(|_| {})),
            )
            .expect("open owner should accept test subscription");
        }

        assert_eq!(bus.unsubscribe_owner("event", stale, callback), 2);
        assert_eq!(bus.subscription_count(), 1);
        assert_eq!(bus.unsubscribe_owner("event", stale, callback), 0);
        assert_eq!(bus.unsubscribe_owner("event", current, callback), 1);
        assert_eq!(bus.subscription_count(), 0);
    }

    #[test]
    fn exact_registration_token_removes_only_the_selected_duplicate() {
        let bus = EventBus::new();
        let owner = OwnerToken {
            signature: 12,
            generation: 3,
        };
        let callback = callback_id(9);
        let handler: Arc<EventHandler> = Arc::new(|_| {});
        let first = bus
            .subscribe(
                "event",
                Subscription::new(owner, callback, Arc::clone(&handler)),
            )
            .expect("first duplicate should subscribe");
        let second = bus
            .subscribe(
                "event",
                Subscription::new(owner, callback, Arc::clone(&handler)),
            )
            .expect("second duplicate should subscribe");
        let _third = bus
            .subscribe("event", Subscription::new(owner, callback, handler))
            .expect("third duplicate should subscribe");

        assert!(bus.unsubscribe_registration("event", &second));
        assert!(!bus.unsubscribe_registration("event", &second));
        assert_eq!(bus.subscription_count(), 2);
        assert!(bus.unsubscribe_registration("event", &first));
        assert_eq!(bus.subscription_count(), 1);
        assert!(!format!("{second:?}").contains("0x"));
    }

    #[test]
    fn exact_removal_waits_for_an_admitted_callback_on_another_thread() {
        let bus = Arc::new(EventBus::new());
        let owner = OwnerToken {
            signature: 20,
            generation: 1,
        };
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let release_for_callback = Arc::clone(&release_rx);
        let token = bus
            .subscribe(
                "event",
                Subscription::new(
                    owner,
                    callback_id(1),
                    Arc::new(move |_| {
                        entered_tx
                            .send(())
                            .expect("entry receiver should remain live");
                        release_for_callback
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .recv()
                            .expect("release sender should remain live");
                    }),
                ),
            )
            .expect("blocking fixture should subscribe");

        let dispatch_bus = Arc::clone(&bus);
        let dispatch = thread::spawn(move || dispatch_bus.raise("event", core::ptr::null_mut()));
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("callback should enter");

        let removal_bus = Arc::clone(&bus);
        let (removed_tx, removed_rx) = mpsc::channel();
        let removal = thread::spawn(move || {
            let removed = removal_bus.unsubscribe_registration("event", &token);
            removed_tx
                .send(removed)
                .expect("removal receiver should remain live");
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        while bus.subscription_count() != 0 {
            assert!(Instant::now() < deadline, "removal did not close lookup");
            thread::yield_now();
        }
        assert!(matches!(
            removed_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        release_tx.send(()).expect("callback should remain live");
        assert!(
            removed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("removal should finish after callback drain")
        );
        removal.join().expect("removal thread should not panic");
        assert_eq!(
            dispatch
                .join()
                .expect("dispatch thread should not panic")
                .invoked,
            1
        );
    }

    #[test]
    fn closed_snapshot_entry_is_not_admitted_after_exact_removal() {
        let bus = Arc::new(EventBus::new());
        let owner = OwnerToken {
            signature: 21,
            generation: 1,
        };
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let release_for_callback = Arc::clone(&release_rx);
        bus.subscribe(
            "event",
            Subscription::new(
                owner,
                callback_id(1),
                Arc::new(move |_| {
                    entered_tx
                        .send(())
                        .expect("entry receiver should remain live");
                    release_for_callback
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv()
                        .expect("release sender should remain live");
                }),
            ),
        )
        .expect("blocking fixture should subscribe");
        let later_calls = Arc::new(AtomicUsize::new(0));
        let later_calls_for_callback = Arc::clone(&later_calls);
        let later = bus
            .subscribe(
                "event",
                Subscription::new(
                    owner,
                    callback_id(2),
                    Arc::new(move |_| {
                        later_calls_for_callback.fetch_add(1, Ordering::SeqCst);
                    }),
                ),
            )
            .expect("later fixture should subscribe");

        let dispatch_bus = Arc::clone(&bus);
        let dispatch = thread::spawn(move || dispatch_bus.raise("event", core::ptr::null_mut()));
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first callback should enter");
        assert!(bus.unsubscribe_registration("event", &later));
        release_tx.send(()).expect("callback should remain live");

        let report = dispatch.join().expect("dispatch thread should not panic");
        assert_eq!(report.invoked, 1);
        assert_eq!(later_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn callback_can_remove_itself_without_lock_recursion_or_deadlock() {
        let bus = Arc::new(EventBus::new());
        let owner = OwnerToken {
            signature: 22,
            generation: 1,
        };
        for callback in 1..=32 {
            let token = Arc::new(Mutex::new(None));
            let token_for_callback = Arc::clone(&token);
            let bus_for_callback = Arc::clone(&bus);
            let removed = Arc::new(AtomicBool::new(false));
            let removed_for_callback = Arc::clone(&removed);
            let registration = bus
                .subscribe(
                    "event",
                    Subscription::new(
                        owner,
                        callback_id(callback),
                        Arc::new(move |_| {
                            let registration = token_for_callback
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .clone()
                                .expect("registration token should be published before dispatch");
                            removed_for_callback.store(
                                bus_for_callback.unsubscribe_registration("event", &registration),
                                Ordering::SeqCst,
                            );
                        }),
                    ),
                )
                .expect("self-removing fixture should subscribe");
            *token
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(registration);

            assert_eq!(bus.raise("event", core::ptr::null_mut()).invoked, 1);
            assert!(removed.load(Ordering::SeqCst));
            assert_eq!(bus.subscription_count(), 0);
            assert!(
                !super::read_lock(&bus.registry)
                    .retiring
                    .contains_key(&owner)
            );
        }
        assert!(bus.remove_owner(owner).quiescent());
    }

    #[test]
    fn owner_cleanup_is_retryable_until_callbacks_drain_and_blocks_stale_publication() {
        let bus = Arc::new(EventBus::new());
        let owner = OwnerToken {
            signature: 23,
            generation: 4,
        };
        let next = OwnerToken {
            signature: owner.signature,
            generation: owner.generation + 1,
        };
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let release_for_callback = Arc::clone(&release_rx);
        bus.subscribe(
            "event",
            Subscription::new(
                owner,
                callback_id(1),
                Arc::new(move |_| {
                    entered_tx
                        .send(())
                        .expect("entry receiver should remain live");
                    release_for_callback
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv()
                        .expect("release sender should remain live");
                }),
            ),
        )
        .expect("blocking fixture should subscribe");

        let dispatch_bus = Arc::clone(&bus);
        let dispatch = thread::spawn(move || dispatch_bus.raise("event", core::ptr::null_mut()));
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("callback should enter");
        let first = bus.remove_owner(owner);
        assert_eq!(first.retired(), 1);
        assert_eq!(first.in_flight(), 1);
        assert!(!first.quiescent());
        assert!(matches!(
            bus.subscribe(
                "event",
                Subscription::new(owner, callback_id(2), Arc::new(|_| {})),
            ),
            Err(EventRegistrationError::OwnerRetired)
        ));
        bus.subscribe(
            "event",
            Subscription::new(next, callback_id(3), Arc::new(|_| {})),
        )
        .expect("a monotonic reload generation should remain admissible");

        release_tx.send(()).expect("callback should remain live");
        assert_eq!(
            dispatch
                .join()
                .expect("dispatch thread should not panic")
                .invoked,
            1
        );
        let retry = bus.remove_owner(owner);
        assert_eq!(retry.retired(), 0);
        assert!(retry.quiescent());
        assert_eq!(bus.remove_owner(next).retired(), 1);
    }

    #[test]
    fn concurrent_cleanup_and_publication_are_linearized_without_a_late_registration() {
        let bus = Arc::new(EventBus::new());
        let owner = OwnerToken {
            signature: 24,
            generation: 8,
        };
        let start = Arc::new(Barrier::new(3));

        let publication_bus = Arc::clone(&bus);
        let publication_start = Arc::clone(&start);
        let publication = thread::spawn(move || {
            publication_start.wait();
            publication_bus.subscribe(
                "event",
                Subscription::new(owner, callback_id(1), Arc::new(|_| {})),
            )
        });
        let cleanup_bus = Arc::clone(&bus);
        let cleanup_start = Arc::clone(&start);
        let cleanup = thread::spawn(move || {
            cleanup_start.wait();
            cleanup_bus.remove_owner(owner)
        });

        start.wait();
        let publication = publication
            .join()
            .expect("publication thread should not panic");
        let cleanup = cleanup.join().expect("cleanup thread should not panic");
        match publication {
            Ok(_token) => assert_eq!(cleanup.retired(), 1),
            Err(EventRegistrationError::OwnerRetired) => assert_eq!(cleanup.retired(), 0),
        }
        assert!(cleanup.quiescent());
        assert_eq!(bus.subscription_count(), 0);
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
            )
            .expect("open owner should accept test subscription");

            let removed = if remove_by_owner {
                bus.remove_owner(owner).retired()
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
        )
        .expect("panic fixture should subscribe");
        bus.subscribe(
            "event",
            Subscription::new(owner, callback_id(2), Arc::new(|_| {})),
        )
        .expect("following fixture should subscribe");

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
        )
        .expect("panic payload fixture should subscribe");

        let report = bus.raise("event", core::ptr::null_mut());
        assert_eq!(report.invoked, 1);
        assert_eq!(report.panicked, 1);
        assert_eq!(payload_drops.load(Ordering::SeqCst), 0);
    }
}
