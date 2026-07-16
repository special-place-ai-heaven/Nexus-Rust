use std::sync::{Arc, Mutex, MutexGuard};

use crate::callback::CallbackSlot;
use crate::{CallbackInvocation, OwnerGeneration, OwnerRetirement, UiCallback, UiRegistryError};

/// Legacy-compatible render phase ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum RenderPhase {
    /// Runs before a new ImGui frame is required.
    PreRender = 0,
    /// Runs inside the visible, initialized UI frame.
    Render = 1,
    /// Runs after the main UI block, including when it was hidden.
    PostRender = 2,
    /// Runs only when the Addons options surface requests it.
    OptionsRender = 3,
}

impl RenderPhase {
    const COUNT: usize = 4;

    const fn index(self) -> usize {
        self as usize
    }
}

impl TryFrom<nexus_abi::RenderPhase> for RenderPhase {
    type Error = UiRegistryError;

    fn try_from(value: nexus_abi::RenderPhase) -> Result<Self, Self::Error> {
        match value.0 {
            0 => Ok(Self::PreRender),
            1 => Ok(Self::Render),
            2 => Ok(Self::PostRender),
            3 => Ok(Self::OptionsRender),
            value => Err(UiRegistryError::InvalidRenderPhase(value)),
        }
    }
}

impl From<RenderPhase> for nexus_abi::RenderPhase {
    fn from(value: RenderPhase) -> Self {
        Self(value as u32)
    }
}

/// Capacity and panic-isolation limits for [`RenderRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderRegistryConfig {
    /// Maximum registrations in each phase.
    pub maximum_callbacks_per_phase: usize,
    /// Managed panics allowed before a registration is disabled.
    pub maximum_panics_per_callback: u32,
}

impl Default for RenderRegistryConfig {
    fn default() -> Self {
        Self {
            maximum_callbacks_per_phase: 4_096,
            maximum_panics_per_callback: 3,
        }
    }
}

impl RenderRegistryConfig {
    pub(crate) fn validate(self) -> Result<Self, UiRegistryError> {
        if self.maximum_callbacks_per_phase == 0 {
            return Err(UiRegistryError::InvalidConfiguration(
                "render callback capacity must be non-zero",
            ));
        }
        if self.maximum_panics_per_callback == 0 {
            return Err(UiRegistryError::InvalidConfiguration(
                "render callback panic budget must be non-zero",
            ));
        }
        Ok(self)
    }
}

/// Result of registering a callback in one render phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterRenderOutcome {
    /// The callback was appended to the phase.
    Registered,
    /// The same callback identity was already in that phase.
    Duplicate,
}

/// Counts the outcomes from invoking one stable render snapshot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RenderInvocationReport {
    /// Callbacks that returned normally.
    pub invoked: usize,
    /// Managed callback panics that were contained.
    pub panicked: usize,
    /// Registrations removed after snapshot creation.
    pub skipped_inactive: usize,
    /// Callbacks skipped because their owner was retired.
    pub skipped_retired: usize,
    /// Registrations disabled by their panic budget.
    pub skipped_disabled: usize,
}

impl RenderInvocationReport {
    fn record(&mut self, outcome: CallbackInvocation) {
        match outcome {
            CallbackInvocation::Invoked => self.invoked += 1,
            CallbackInvocation::Panicked { .. } => self.panicked += 1,
            CallbackInvocation::SkippedInactive => self.skipped_inactive += 1,
            CallbackInvocation::SkippedOwnerRetired => self.skipped_retired += 1,
            CallbackInvocation::SkippedPanicDisabled => self.skipped_disabled += 1,
        }
    }
}

/// Immutable callback sequence safe to hold across render frames.
#[derive(Debug, Clone, Default)]
pub struct RenderSnapshot {
    callbacks: Arc<[Arc<CallbackSlot>]>,
}

impl RenderSnapshot {
    /// Returns the number of registrations captured in this snapshot.
    #[must_use]
    pub fn len(&self) -> usize {
        self.callbacks.len()
    }

    /// Returns whether no registrations were captured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.callbacks.is_empty()
    }

    /// Invokes callbacks in registration order without holding registry locks.
    #[must_use]
    pub fn invoke_all(&self) -> RenderInvocationReport {
        let mut report = RenderInvocationReport::default();
        for callback in self.callbacks.iter() {
            report.record(callback.invoke());
        }
        report
    }
}

/// State that controls the legacy main-render visibility gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRenderState {
    /// Whether the UI backend completed initialization.
    pub initialized: bool,
    /// Whether the EULA gate is open.
    pub eula_accepted: bool,
    /// Whether the user has the overall UI visible.
    pub ui_visible: bool,
}

impl FrameRenderState {
    const fn renders_main(self) -> bool {
        self.initialized && self.eula_accepted && self.ui_visible
    }
}

/// Phase snapshots for one host frame.
///
/// `pre_render` and `post_render` are always present. The consumer can render
/// built-in windows after `render` and before `post_render`, matching the C++
/// call site. Options callbacks are requested separately.
#[derive(Debug, Clone)]
pub struct RenderFrameSnapshot {
    /// Callbacks that run before main-frame initialization checks.
    pub pre_render: RenderSnapshot,
    /// Main callbacks, absent when any visibility gate is closed.
    pub render: Option<RenderSnapshot>,
    /// Callbacks that run after the optional main UI block.
    pub post_render: RenderSnapshot,
}

#[derive(Debug)]
struct RenderState {
    phases: [Vec<Arc<CallbackSlot>>; RenderPhase::COUNT],
}

impl Default for RenderState {
    fn default() -> Self {
        Self {
            phases: std::array::from_fn(|_| Vec::new()),
        }
    }
}

/// Ordered, duplicate-safe render callback registry.
#[derive(Debug)]
pub struct RenderRegistry {
    state: Mutex<RenderState>,
    config: RenderRegistryConfig,
}

impl Default for RenderRegistry {
    fn default() -> Self {
        Self {
            state: Mutex::new(RenderState::default()),
            config: RenderRegistryConfig::default(),
        }
    }
}

impl RenderRegistry {
    /// Creates a registry with validated bounds.
    pub fn new(config: RenderRegistryConfig) -> Result<Self, UiRegistryError> {
        Ok(Self {
            state: Mutex::new(RenderState::default()),
            config: config.validate()?,
        })
    }

    /// Appends a callback unless the same identity is already in this phase.
    pub fn register(
        &self,
        phase: RenderPhase,
        callback: UiCallback,
    ) -> Result<RegisterRenderOutcome, UiRegistryError> {
        let Some(_activity) = callback.try_enter_owner() else {
            return Err(UiRegistryError::OwnerRetired(callback.owner()));
        };
        let mut state = self.lock();
        let callbacks = &mut state.phases[phase.index()];
        if callbacks
            .iter()
            .any(|slot| slot.callback().same_identity(&callback))
        {
            return Ok(RegisterRenderOutcome::Duplicate);
        }
        if callbacks.len() >= self.config.maximum_callbacks_per_phase {
            return Err(UiRegistryError::CapacityExceeded {
                registry: "render phase",
                maximum: self.config.maximum_callbacks_per_phase,
            });
        }
        callbacks.push(CallbackSlot::new(
            callback,
            self.config.maximum_panics_per_callback,
        ));
        Ok(RegisterRenderOutcome::Registered)
    }

    /// Removes every occurrence of a callback identity from all four phases.
    pub fn deregister(&self, callback: &UiCallback) -> usize {
        let mut state = self.lock();
        let mut removed = 0;
        for callbacks in &mut state.phases {
            callbacks.retain(|slot| {
                if slot.callback().same_identity(callback) {
                    slot.deactivate();
                    removed += 1;
                    false
                } else {
                    true
                }
            });
        }
        removed
    }

    /// Captures one phase in registration order.
    #[must_use]
    pub fn snapshot(&self, phase: RenderPhase) -> RenderSnapshot {
        let callbacks = self.lock().phases[phase.index()].clone();
        RenderSnapshot {
            callbacks: Arc::from(callbacks),
        }
    }

    /// Captures the three centrally dispatched phases with legacy visibility.
    #[must_use]
    pub fn snapshot_frame(&self, frame: FrameRenderState) -> RenderFrameSnapshot {
        let state = self.lock();
        let snapshot = |phase: RenderPhase| RenderSnapshot {
            callbacks: Arc::from(state.phases[phase.index()].clone()),
        };
        RenderFrameSnapshot {
            pre_render: snapshot(RenderPhase::PreRender),
            render: frame.renders_main().then(|| snapshot(RenderPhase::Render)),
            post_render: snapshot(RenderPhase::PostRender),
        }
    }

    pub(crate) fn cleanup_owner_generation(
        &self,
        owner: OwnerGeneration,
    ) -> (usize, OwnerRetirement) {
        let mut state = self.lock();
        let mut removed = Vec::new();
        for callbacks in &mut state.phases {
            callbacks.retain(|slot| {
                if slot.owner() == owner {
                    slot.deactivate();
                    removed.push(Arc::clone(slot));
                    false
                } else {
                    true
                }
            });
        }
        drop(state);
        let retirement = removed
            .iter()
            .fold(OwnerRetirement::already_quiescent(), |retirement, slot| {
                retirement.merge(slot.owner_handle().retire_and_drain())
            });
        (removed.len(), retirement)
    }

    fn lock(&self) -> MutexGuard<'_, RenderState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}
