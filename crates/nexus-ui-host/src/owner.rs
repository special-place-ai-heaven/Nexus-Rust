use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::UiRegistryError;

thread_local! {
    static ACTIVE_GATES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Stable identity for exactly one addon load generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerGeneration {
    /// Host-assigned addon identity.
    pub owner: u64,
    /// Monotonically increasing load generation for that addon.
    pub generation: u64,
}

impl OwnerGeneration {
    /// Creates an owner-generation identity.
    #[must_use]
    pub const fn new(owner: u64, generation: u64) -> Self {
        Self { owner, generation }
    }
}

impl From<nexus_core::OwnerToken> for OwnerGeneration {
    fn from(owner: nexus_core::OwnerToken) -> Self {
        Self::new(u64::from(owner.signature), owner.generation)
    }
}

#[derive(Debug)]
struct GateState {
    active: bool,
    in_flight: usize,
}

#[derive(Debug)]
struct GenerationGate {
    state: Mutex<GateState>,
    drained: Condvar,
}

impl GenerationGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(GateState {
                active: true,
                in_flight: 0,
            }),
            drained: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn wait<'a>(&self, state: MutexGuard<'a, GateState>) -> MutexGuard<'a, GateState> {
        match self.drained.wait(state) {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Shared lifecycle handle used by every registration owned by one generation.
///
/// Handles are issued by [`crate::UiHost::owner`]. Clones share a gate, so a
/// cleanup closes render callbacks, context menus, and native visibility
/// pointers atomically before registry state is removed.
#[derive(Clone, Debug)]
pub struct OwnerHandle {
    identity: OwnerGeneration,
    gate: Arc<GenerationGate>,
}

impl OwnerHandle {
    fn new(identity: OwnerGeneration) -> Self {
        Self {
            identity,
            gate: Arc::new(GenerationGate::new()),
        }
    }

    /// Returns the generation represented by this handle.
    #[must_use]
    pub const fn identity(&self) -> OwnerGeneration {
        self.identity
    }

    /// Returns whether cleanup has not yet retired this generation.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.gate.lock().active
    }

    pub(crate) fn shares_lifecycle(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.gate, &other.gate)
    }

    pub(crate) fn try_enter(&self) -> Option<OwnerActivity> {
        let mut state = self.gate.lock();
        if !state.active {
            return None;
        }
        let next = state.in_flight.checked_add(1)?;
        state.in_flight = next;
        drop(state);

        let key = Arc::as_ptr(&self.gate) as usize;
        ACTIVE_GATES.with(|gates| gates.borrow_mut().push(key));
        Some(OwnerActivity {
            gate: Arc::clone(&self.gate),
            key,
        })
    }

    pub(crate) fn retire_and_drain(&self) -> OwnerRetirement {
        let key = Arc::as_ptr(&self.gate) as usize;
        let local_depth = ACTIVE_GATES.with(|gates| {
            gates
                .borrow()
                .iter()
                .filter(|active| **active == key)
                .count()
        });
        let mut state = self.gate.lock();
        let was_active = state.active;
        state.active = false;

        // Waiting for this thread's own callback would self-deadlock. Other
        // threads still drain before cleanup proceeds.
        while state.in_flight > local_depth {
            state = self.gate.wait(state);
        }

        OwnerRetirement {
            was_active,
            quiescent: state.in_flight == 0,
            in_flight: state.in_flight,
        }
    }
}

pub(crate) struct OwnerActivity {
    gate: Arc<GenerationGate>,
    key: usize,
}

impl Drop for OwnerActivity {
    fn drop(&mut self) {
        ACTIVE_GATES.with(|gates| {
            let mut gates = gates.borrow_mut();
            if let Some(index) = gates.iter().rposition(|active| *active == self.key) {
                gates.remove(index);
            }
        });

        let mut state = self.gate.lock();
        state.in_flight = state.in_flight.saturating_sub(1);
        self.gate.drained.notify_all();
    }
}

/// Result of closing an owner generation's activity gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerRetirement {
    /// Whether the generation was active before this retirement request.
    pub was_active: bool,
    /// Whether no callback or registration is still executing.
    pub quiescent: bool,
    /// Remaining activity on the calling thread, if cleanup was reentrant.
    pub in_flight: usize,
}

impl OwnerRetirement {
    pub(crate) const fn already_quiescent() -> Self {
        Self {
            was_active: false,
            quiescent: true,
            in_flight: 0,
        }
    }

    pub(crate) const fn merge(self, other: Self) -> Self {
        Self {
            was_active: self.was_active || other.was_active,
            quiescent: self.quiescent && other.quiescent,
            in_flight: if self.in_flight > other.in_flight {
                self.in_flight
            } else {
                other.in_flight
            },
        }
    }
}

#[derive(Debug, Default)]
struct OwnerRegistryState {
    handles: BTreeMap<OwnerGeneration, OwnerHandle>,
    retired: BTreeSet<OwnerGeneration>,
}

/// Issues one shared lifecycle handle per addon generation.
#[derive(Debug, Default)]
pub(crate) struct OwnerRegistry {
    state: Mutex<OwnerRegistryState>,
}

impl OwnerRegistry {
    pub(crate) fn acquire(&self, owner: OwnerGeneration) -> Result<OwnerHandle, UiRegistryError> {
        let mut state = self.lock();
        if state.retired.contains(&owner) {
            return Err(UiRegistryError::OwnerRetired(owner));
        }
        if let Some(handle) = state.handles.get(&owner) {
            return Ok(handle.clone());
        }
        let handle = OwnerHandle::new(owner);
        state.handles.insert(owner, handle.clone());
        Ok(handle)
    }

    pub(crate) fn retire(&self, owner: OwnerGeneration) -> OwnerRetirement {
        let handle = {
            let mut state = self.lock();
            state.retired.insert(owner);
            state.handles.get(&owner).cloned()
        };
        handle.map_or_else(OwnerRetirement::already_quiescent, |handle| {
            handle.retire_and_drain()
        })
    }

    pub(crate) fn wait_for_quiescence(&self, owner: OwnerGeneration) -> OwnerRetirement {
        let handle = self.lock().handles.get(&owner).cloned();
        handle.map_or_else(OwnerRetirement::already_quiescent, |handle| {
            handle.retire_and_drain()
        })
    }

    fn lock(&self) -> MutexGuard<'_, OwnerRegistryState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}
