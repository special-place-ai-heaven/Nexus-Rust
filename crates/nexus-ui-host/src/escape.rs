//! Close-on-Escape registry and visibility targets.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::error::validate_text;
use crate::{NativeVisibilityPointer, OwnerGeneration, OwnerHandle, UiRegistryError};

/// Win32 virtual-key value for Escape.
pub const ESCAPE_VIRTUAL_KEY: u32 = 0x1B;

/// Key state needed by the close-on-Escape service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscapeKeyEvent {
    /// True only for a key-down message.
    pub is_key_down: bool,
    /// Win32-compatible virtual-key value.
    pub virtual_key: u32,
    /// Previous-key-state bit from the message; true means key repeat.
    pub was_down: bool,
}

/// Visibility storage registered for one named window.
#[derive(Debug)]
pub enum VisibilityTarget {
    /// Safe Rust-owned visibility state.
    Managed(Arc<AtomicBool>),
    /// Addon-owned legacy `bool*` storage.
    Native(NativeVisibilityPointer),
}

impl VisibilityTarget {
    /// Creates managed visibility storage.
    #[must_use]
    pub fn managed(visible: Arc<AtomicBool>) -> Self {
        Self::Managed(visible)
    }

    /// Creates a target from an explicitly validated native pointer wrapper.
    #[must_use]
    pub const fn native(pointer: NativeVisibilityPointer) -> Self {
        Self::Native(pointer)
    }

    fn identity(&self) -> (u8, usize) {
        match self {
            Self::Managed(visible) => (0, Arc::as_ptr(visible) as usize),
            Self::Native(pointer) => (1, pointer.address()),
        }
    }

    fn native_owner(&self) -> Option<&OwnerHandle> {
        match self {
            Self::Managed(_) => None,
            Self::Native(pointer) => Some(pointer.owner()),
        }
    }

    fn close_if_visible(&self) -> bool {
        match self {
            Self::Managed(visible) => visible.swap(false, Ordering::AcqRel),
            Self::Native(pointer) => pointer.close_if_visible(),
        }
    }
}

/// Capacity limits for [`EscapeClosingRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscapeClosingConfig {
    /// Maximum registered windows.
    pub maximum_windows: usize,
    /// Maximum UTF-8 bytes in a window name.
    pub maximum_window_name_bytes: usize,
}

impl Default for EscapeClosingConfig {
    fn default() -> Self {
        Self {
            maximum_windows: 1_024,
            maximum_window_name_bytes: 512,
        }
    }
}

impl EscapeClosingConfig {
    pub(crate) fn validate(self) -> Result<Self, UiRegistryError> {
        if self.maximum_windows == 0 {
            return Err(UiRegistryError::InvalidConfiguration(
                "Escape window capacity must be non-zero",
            ));
        }
        if self.maximum_window_name_bytes == 0 {
            return Err(UiRegistryError::InvalidConfiguration(
                "Escape window-name limit must be non-zero",
            ));
        }
        Ok(self)
    }
}

/// Result of registering a named close-on-Escape target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeRegistrationOutcome {
    /// The name and target were inserted at the end of registration order.
    Registered,
    /// The name already existed; its original target remains authoritative.
    Duplicate,
}

/// Routing result for one key message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscapeCloseOutcome {
    /// The service made no change and the message should continue.
    Passed,
    /// A visible registered window was closed and the message was consumed.
    Consumed {
        /// Name of the closed topmost window.
        window: Arc<str>,
    },
}

#[derive(Debug)]
struct EscapeEntry {
    owner: OwnerHandle,
    window: Arc<str>,
    target: VisibilityTarget,
    lifecycle: Mutex<EscapeEntryState>,
    drained: Condvar,
}

#[derive(Debug)]
struct EscapeEntryState {
    active: bool,
    in_flight: usize,
}

impl EscapeEntry {
    fn close_if_visible(&self) -> bool {
        let Some(_entry_activity) = self.try_enter() else {
            return false;
        };
        let Some(_activity) = self.owner.try_enter() else {
            return false;
        };
        self.target.close_if_visible()
    }

    fn try_enter(&self) -> Option<EscapeEntryActivity<'_>> {
        let mut state = self.lock_lifecycle();
        if !state.active {
            return None;
        }
        state.in_flight = state.in_flight.checked_add(1)?;
        Some(EscapeEntryActivity { entry: self })
    }

    fn deactivate_and_drain(&self) {
        let mut state = self.lock_lifecycle();
        state.active = false;
        while state.in_flight != 0 {
            state = match self.drained.wait(state) {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }

    fn lock_lifecycle(&self) -> MutexGuard<'_, EscapeEntryState> {
        match self.lifecycle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

struct EscapeEntryActivity<'a> {
    entry: &'a EscapeEntry,
}

impl Drop for EscapeEntryActivity<'_> {
    fn drop(&mut self) {
        let mut state = self.entry.lock_lifecycle();
        state.in_flight = state.in_flight.saturating_sub(1);
        self.entry.drained.notify_all();
    }
}

#[derive(Debug, Default)]
struct EscapeState {
    entries: Vec<Arc<EscapeEntry>>,
}

/// Deterministic, reentrant-safe close-on-Escape registry.
///
/// The supplied window stack is bottom-to-top. Index zero is intentionally
/// ignored to match the legacy ImGui window-stack scan.
#[derive(Debug)]
pub struct EscapeClosingRegistry {
    state: Mutex<EscapeState>,
    enabled: AtomicBool,
    config: EscapeClosingConfig,
}

impl Default for EscapeClosingRegistry {
    fn default() -> Self {
        Self {
            state: Mutex::new(EscapeState::default()),
            enabled: AtomicBool::new(true),
            config: EscapeClosingConfig::default(),
        }
    }
}

impl EscapeClosingRegistry {
    /// Creates a registry with validated bounds.
    pub fn new(config: EscapeClosingConfig) -> Result<Self, UiRegistryError> {
        Ok(Self {
            state: Mutex::new(EscapeState::default()),
            enabled: AtomicBool::new(true),
            config: config.validate()?,
        })
    }

    /// Enables or disables Escape closing globally.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    /// Returns whether Escape closing is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Registers a window name. Duplicate names preserve the first target.
    pub fn register(
        &self,
        owner: &OwnerHandle,
        window: &str,
        target: VisibilityTarget,
    ) -> Result<EscapeRegistrationOutcome, UiRegistryError> {
        validate_text(
            "Escape window name",
            window,
            self.config.maximum_window_name_bytes,
        )?;
        if let Some(bound_owner) = target.native_owner()
            && !bound_owner.shares_lifecycle(owner)
        {
            return Err(UiRegistryError::NativeOwnerMismatch {
                bound: bound_owner.identity(),
                registration: owner.identity(),
            });
        }
        let Some(_activity) = owner.try_enter() else {
            return Err(UiRegistryError::OwnerRetired(owner.identity()));
        };
        let mut state = self.lock();
        if state
            .entries
            .iter()
            .any(|entry| entry.window.as_ref() == window)
        {
            return Ok(EscapeRegistrationOutcome::Duplicate);
        }
        if state.entries.len() >= self.config.maximum_windows {
            return Err(UiRegistryError::CapacityExceeded {
                registry: "close-on-Escape windows",
                maximum: self.config.maximum_windows,
            });
        }
        state.entries.push(Arc::new(EscapeEntry {
            owner: owner.clone(),
            window: Arc::from(window),
            target,
            lifecycle: Mutex::new(EscapeEntryState {
                active: true,
                in_flight: 0,
            }),
            drained: Condvar::new(),
        }));
        Ok(EscapeRegistrationOutcome::Registered)
    }

    /// Removes the first registration with this window name.
    ///
    /// This is a trusted host-wide administrative operation. Native add-on API
    /// adapters should use [`Self::deregister_window_for_owner`] instead.
    pub fn deregister_window(&self, window: &str) -> bool {
        let entry = {
            let mut state = self.lock();
            let Some(index) = state
                .entries
                .iter()
                .position(|entry| entry.window.as_ref() == window)
            else {
                return false;
            };
            state.entries.remove(index)
        };
        entry.deactivate_and_drain();
        true
    }

    /// Removes this owner's registration by its window name.
    pub fn deregister_window_for_owner(&self, owner: &OwnerHandle, window: &str) -> bool {
        let Some(_activity) = owner.try_enter() else {
            return false;
        };
        let entry = {
            let mut state = self.lock();
            let Some(index) = state.entries.iter().position(|entry| {
                entry.owner.shares_lifecycle(owner) && entry.window.as_ref() == window
            }) else {
                return false;
            };
            state.entries.remove(index)
        };
        entry.deactivate_and_drain();
        true
    }

    /// Removes the first registration using this target, in registration order.
    ///
    /// This is a trusted host-wide administrative operation. Native add-on API
    /// adapters should use [`Self::deregister_target_for_owner`] instead.
    pub fn deregister_target(&self, target: &VisibilityTarget) -> bool {
        let identity = target.identity();
        let entry = {
            let mut state = self.lock();
            let Some(index) = state
                .entries
                .iter()
                .position(|entry| entry.target.identity() == identity)
            else {
                return false;
            };
            state.entries.remove(index)
        };
        entry.deactivate_and_drain();
        true
    }

    /// Removes this owner's first matching target, in registration order.
    pub fn deregister_target_for_owner(
        &self,
        owner: &OwnerHandle,
        target: &VisibilityTarget,
    ) -> bool {
        let Some(_activity) = owner.try_enter() else {
            return false;
        };
        let identity = target.identity();
        let entry = {
            let mut state = self.lock();
            let Some(index) = state.entries.iter().position(|entry| {
                entry.owner.shares_lifecycle(owner) && entry.target.identity() == identity
            }) else {
                return false;
            };
            state.entries.remove(index)
        };
        entry.deactivate_and_drain();
        true
    }

    /// Handles an Escape key transition using a bottom-to-top window stack.
    #[must_use]
    pub fn handle(&self, event: EscapeKeyEvent, window_stack: &[&str]) -> EscapeCloseOutcome {
        if !self.is_enabled()
            || !event.is_key_down
            || event.virtual_key != ESCAPE_VIRTUAL_KEY
            || event.was_down
        {
            return EscapeCloseOutcome::Passed;
        }

        let entries = self.lock().entries.clone();
        for window in window_stack.iter().skip(1).rev() {
            if let Some(entry) = entries
                .iter()
                .find(|entry| entry.window.as_ref() == *window)
                && entry.close_if_visible()
            {
                return EscapeCloseOutcome::Consumed {
                    window: Arc::clone(&entry.window),
                };
            }
        }
        EscapeCloseOutcome::Passed
    }

    /// Returns registered names in stable insertion order.
    #[must_use]
    pub fn registered_windows(&self) -> Arc<[Arc<str>]> {
        Arc::from(
            self.lock()
                .entries
                .iter()
                .map(|entry| Arc::clone(&entry.window))
                .collect::<Vec<_>>(),
        )
    }

    pub(crate) fn cleanup_owner_generation(&self, owner: OwnerGeneration) -> usize {
        let mut state = self.lock();
        let removed = state
            .entries
            .iter()
            .filter(|entry| entry.owner.identity() == owner)
            .cloned()
            .collect::<Vec<_>>();
        state
            .entries
            .retain(|entry| entry.owner.identity() != owner);
        drop(state);
        for entry in &removed {
            entry.deactivate_and_drain();
        }
        removed.len()
    }

    fn lock(&self) -> MutexGuard<'_, EscapeState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}
