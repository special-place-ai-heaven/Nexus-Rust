use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use nexus_core::CallbackId;

use crate::{CallbackLimits, OwnerGeneration};

/// Raw platform message. Handle-like values stay opaque integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMessage {
    /// Opaque native window token.
    pub window: usize,
    /// Native message identifier.
    pub message: u32,
    /// Native unsigned parameter.
    pub wparam: usize,
    /// Native signed parameter.
    pub lparam: isize,
}

/// Result returned by a raw WndProc callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawRoute {
    /// Continue to the next callback and later routing layers.
    Continue,
    /// Stop all later processing.
    Consume,
}

/// Opaque registration token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RawCallbackToken(u128);

impl RawCallbackToken {
    /// Exposes the stable non-address token for diagnostics.
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0
    }
}

/// Closed route diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawRouteReport {
    /// Final disposition.
    pub route: RawRoute,
    /// Number of enabled callbacks invoked.
    pub invoked: usize,
    /// Number of callback panics contained during this route.
    pub panics: u32,
    /// Number of disabled callbacks skipped.
    pub disabled: usize,
}

type RawCallback = dyn Fn(RawMessage) -> RawRoute + Send + Sync + 'static;

struct Entry {
    owner: OwnerGeneration,
    callback_id: Option<CallbackId>,
    callback: Arc<RawCallback>,
    panics: AtomicU32,
    disabled: AtomicBool,
    maximum_panics: u32,
}

impl Entry {
    fn record_panic(&self) {
        let count = self
            .panics
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_add(1))
            })
            .map_or(u32::MAX, |previous| previous.saturating_add(1));
        if count >= self.maximum_panics {
            self.disabled.store(true, Ordering::Release);
        }
    }
}

#[derive(Default)]
struct RegistryState {
    next_token: u128,
    entries: BTreeMap<RawCallbackToken, Arc<Entry>>,
}

/// Ordered, reentrant-safe raw WndProc callback registry.
///
/// The callback list is snapshotted under the mutex and invoked after the
/// mutex is released. A callback can therefore register or deregister without
/// reproducing the legacy registry's self-deadlock.
pub struct RawWndProcRegistry {
    state: Mutex<RegistryState>,
    limits: CallbackLimits,
}

impl Default for RawWndProcRegistry {
    fn default() -> Self {
        Self::new(CallbackLimits::default())
    }
}

impl RawWndProcRegistry {
    /// Creates a registry with the supplied panic policy.
    #[must_use]
    pub const fn new(limits: CallbackLimits) -> Self {
        Self {
            state: Mutex::new(RegistryState {
                next_token: 0,
                entries: BTreeMap::new(),
            }),
            limits,
        }
    }

    /// Appends a callback and returns an opaque token.
    pub fn register<F>(&self, owner: OwnerGeneration, callback: F) -> RawCallbackToken
    where
        F: Fn(RawMessage) -> RawRoute + Send + Sync + 'static,
    {
        self.register_inner(owner, None, callback)
    }

    /// Appends a callback with its authenticated foreign function identity.
    pub fn register_identified<F>(
        &self,
        owner: OwnerGeneration,
        callback_id: CallbackId,
        callback: F,
    ) -> RawCallbackToken
    where
        F: Fn(RawMessage) -> RawRoute + Send + Sync + 'static,
    {
        self.register_inner(owner, Some(callback_id), callback)
    }

    fn register_inner<F>(
        &self,
        owner: OwnerGeneration,
        callback_id: Option<CallbackId>,
        callback: F,
    ) -> RawCallbackToken
    where
        F: Fn(RawMessage) -> RawRoute + Send + Sync + 'static,
    {
        let entry = Arc::new(Entry {
            owner,
            callback_id,
            callback: Arc::new(callback),
            panics: AtomicU32::new(0),
            disabled: AtomicBool::new(false),
            maximum_panics: self.limits.max_panics.max(1),
        });
        let (token, replaced) = {
            let mut state = self.lock_state();
            state.next_token = state
                .next_token
                .checked_add(1)
                .expect("raw callback token space exhausted");
            let token = RawCallbackToken(state.next_token);
            let replaced = state.entries.insert(token, entry);
            (token, replaced)
        };
        drop(replaced);
        token
    }

    /// Removes one token.
    pub fn deregister(&self, token: RawCallbackToken) -> bool {
        let removed = {
            let mut state = self.lock_state();
            state.entries.remove(&token)
        };
        let was_registered = removed.is_some();
        drop(removed);
        was_registered
    }

    /// Removes callbacks matching one exact owner generation and function identity.
    pub fn deregister_callback(&self, owner: OwnerGeneration, callback_id: CallbackId) -> usize {
        let mut removed = Vec::new();
        {
            let mut state = self.lock_state();
            let tokens: Vec<RawCallbackToken> = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.owner == owner && entry.callback_id == Some(callback_id))
                .map(|(token, _)| *token)
                .collect();
            removed.reserve(tokens.len());
            for token in tokens {
                if let Some(entry) = state.entries.remove(&token) {
                    removed.push(entry);
                }
            }
        }
        let removed_count = removed.len();
        drop(removed);
        removed_count
    }

    /// Removes callbacks from exactly one addon load generation.
    pub fn cleanup_owner_generation(&self, owner: OwnerGeneration) -> usize {
        let mut removed = Vec::new();
        {
            let mut state = self.lock_state();
            let tokens: Vec<RawCallbackToken> = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.owner == owner)
                .map(|(token, _)| *token)
                .collect();
            removed.reserve(tokens.len());
            for token in tokens {
                if let Some(entry) = state.entries.remove(&token) {
                    removed.push(entry);
                }
            }
        }
        let removed_count = removed.len();
        drop(removed);
        removed_count
    }

    /// Routes in registration order and short-circuits on the first consumer.
    #[must_use]
    pub fn route(&self, message: RawMessage) -> RawRouteReport {
        let mut callbacks = Vec::new();
        {
            let state = self.lock_state();
            callbacks.reserve(state.entries.len());
            callbacks.extend(state.entries.values().cloned());
        }
        let mut report = RawRouteReport {
            route: RawRoute::Continue,
            invoked: 0,
            panics: 0,
            disabled: 0,
        };
        for entry in callbacks {
            if entry.disabled.load(Ordering::Acquire) {
                report.disabled += 1;
                continue;
            }
            report.invoked += 1;
            match catch_unwind(AssertUnwindSafe(|| (entry.callback)(message))) {
                Ok(RawRoute::Continue) => {}
                Ok(RawRoute::Consume) => {
                    report.route = RawRoute::Consume;
                    break;
                }
                Err(payload) => {
                    std::mem::forget(payload);
                    entry.record_panic();
                    report.panics += 1;
                }
            }
        }
        report
    }

    /// Returns the number of live registrations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock_state().entries.len()
    }

    /// Returns whether no callback is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock_state(&self) -> MutexGuard<'_, RegistryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use super::*;

    fn message() -> RawMessage {
        RawMessage {
            window: 1,
            message: 2,
            wparam: 3,
            lparam: 4,
        }
    }

    struct ReentrantDrop {
        registry: Arc<RawWndProcRegistry>,
        observed_unlocked: Arc<AtomicBool>,
    }

    impl Drop for ReentrantDrop {
        fn drop(&mut self) {
            let unlocked = self.registry.state.try_lock().is_ok();
            if unlocked {
                self.registry
                    .register(OwnerGeneration::new(999, 1), |_| RawRoute::Continue);
            }
            self.observed_unlocked.store(unlocked, Ordering::Release);
        }
    }

    struct PanicOnDrop;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("caught panic payload destructor must not run");
        }
    }

    fn register_drop_probe(
        registry: &Arc<RawWndProcRegistry>,
        owner: OwnerGeneration,
        observed_unlocked: Arc<AtomicBool>,
    ) -> RawCallbackToken {
        let probe = ReentrantDrop {
            registry: Arc::clone(registry),
            observed_unlocked,
        };
        registry.register(owner, move |_| {
            let _ = &probe;
            RawRoute::Continue
        })
    }

    #[test]
    fn callbacks_are_ordered_and_short_circuit() {
        let registry = RawWndProcRegistry::default();
        let calls = Arc::new(Mutex::new(Vec::new()));
        for (index, route) in [RawRoute::Continue, RawRoute::Consume, RawRoute::Continue]
            .into_iter()
            .enumerate()
        {
            let calls = Arc::clone(&calls);
            registry.register(OwnerGeneration::new(index as u64, 1), move |_| {
                calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(index);
                route
            });
        }
        let report = registry.route(message());
        assert_eq!(report.route, RawRoute::Consume);
        assert_eq!(report.invoked, 2);
        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [0, 1]
        );
    }

    #[test]
    fn callback_can_deregister_itself_without_deadlock() {
        let registry = Arc::new(RawWndProcRegistry::default());
        let token_value = Arc::new(Mutex::new(None));
        let callback_registry = Arc::clone(&registry);
        let callback_token = Arc::clone(&token_value);
        let token = registry.register(OwnerGeneration::new(1, 1), move |_| {
            let token = callback_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .expect("callback token should be published before routing");
            callback_registry.deregister(token);
            RawRoute::Continue
        });
        *token_value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(token);
        let _ = registry.route(message());
        assert!(registry.is_empty());
    }

    #[test]
    fn callback_identity_deregistration_is_owner_generation_scoped() {
        let registry = RawWndProcRegistry::default();
        let owner = OwnerGeneration::new(1, 1);
        let foreign = OwnerGeneration::new(2, 1);
        let callback = CallbackId::new(1).expect("test callback identity is nonzero");
        let other_callback = CallbackId::new(2).expect("test callback identity is nonzero");

        registry.register_identified(owner, callback, |_| RawRoute::Continue);
        registry.register_identified(foreign, callback, |_| RawRoute::Continue);
        registry.register_identified(owner, other_callback, |_| RawRoute::Continue);
        registry.register(owner, |_| RawRoute::Continue);

        assert_eq!(registry.deregister_callback(owner, callback), 1);
        assert_eq!(registry.len(), 3);
        assert_eq!(registry.deregister_callback(owner, callback), 0);
        assert_eq!(registry.deregister_callback(foreign, callback), 1);
        assert_eq!(registry.deregister_callback(owner, other_callback), 1);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn stale_registration_token_cannot_remove_a_later_callback() {
        let registry = RawWndProcRegistry::default();
        let stale = registry.register(OwnerGeneration::new(1, 1), |_| RawRoute::Continue);
        assert!(registry.deregister(stale));

        let current = registry.register(OwnerGeneration::new(1, 1), |_| RawRoute::Continue);
        assert_ne!(stale, current);
        assert!(!registry.deregister(stale));
        assert_eq!(registry.len(), 1);
        assert!(registry.deregister(current));
    }

    #[test]
    fn callback_destructors_run_after_registry_unlocks() {
        let registry = Arc::new(RawWndProcRegistry::default());
        let deregistered = Arc::new(AtomicBool::new(false));
        let token = register_drop_probe(
            &registry,
            OwnerGeneration::new(1, 1),
            Arc::clone(&deregistered),
        );
        assert!(registry.deregister(token));
        assert!(deregistered.load(Ordering::Acquire));

        let registry = Arc::new(RawWndProcRegistry::default());
        let cleaned = Arc::new(AtomicBool::new(false));
        register_drop_probe(&registry, OwnerGeneration::new(2, 1), Arc::clone(&cleaned));
        assert_eq!(
            registry.cleanup_owner_generation(OwnerGeneration::new(2, 1)),
            1
        );
        assert!(cleaned.load(Ordering::Acquire));

        let registry = Arc::new(RawWndProcRegistry::default());
        registry.lock_state().next_token = u128::from(u64::MAX) - 1;
        let replaced = Arc::new(AtomicBool::new(false));
        let first =
            register_drop_probe(&registry, OwnerGeneration::new(3, 1), Arc::clone(&replaced));
        let second = registry.register(OwnerGeneration::new(3, 2), |_| RawRoute::Continue);
        assert_ne!(first, second);
        assert!(!replaced.load(Ordering::Acquire));
        assert!(registry.deregister(first));
        assert!(replaced.load(Ordering::Acquire));
    }

    #[test]
    fn panic_payload_with_panicking_destructor_is_forgotten() {
        let registry = RawWndProcRegistry::default();
        registry.register(OwnerGeneration::new(1, 1), |_| {
            std::panic::panic_any(PanicOnDrop)
        });

        let report = registry.route(message());
        assert_eq!(report.panics, 1);
    }

    #[test]
    fn panic_budget_disables_only_the_failing_callback() {
        let registry = RawWndProcRegistry::new(CallbackLimits { max_panics: 2 });
        let later_calls = Arc::new(AtomicUsize::new(0));
        registry.register(OwnerGeneration::new(1, 1), |_| {
            panic!("test raw callback panic")
        });
        let later = Arc::clone(&later_calls);
        registry.register(OwnerGeneration::new(2, 1), move |_| {
            later.fetch_add(1, Ordering::Relaxed);
            RawRoute::Continue
        });
        assert_eq!(registry.route(message()).panics, 1);
        assert_eq!(registry.route(message()).panics, 1);
        let third = registry.route(message());
        assert_eq!(third.panics, 0);
        assert_eq!(third.disabled, 1);
        assert_eq!(later_calls.load(Ordering::Relaxed), 3);
    }
}
