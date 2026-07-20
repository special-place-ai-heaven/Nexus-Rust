use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Map, Value};

use crate::capture::{InputCapture, InputMessage};
use crate::{
    InputBind, InputDevice, KeyNameResolver, LegacyInputBind, PersistenceError, parse_bind_lossy,
};

const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_BINDINGS: usize = 16_384;
const MAX_IDENTIFIER_BYTES: usize = 1_024;

/// Stable owner identity paired with one load generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerGeneration {
    /// Host-assigned owner identifier.
    pub owner: u64,
    /// Monotonically increasing load generation.
    pub generation: u64,
}

impl OwnerGeneration {
    /// Constructs an owner-generation token.
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

/// Callback panic policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackLimits {
    /// Number of callback panics after which that registration is disabled.
    pub max_panics: u32,
}

impl Default for CallbackLimits {
    fn default() -> Self {
        Self { max_panics: 3 }
    }
}

/// Dispatches callbacks whose historical API contract is asynchronous.
pub trait CallbackExecutor: Send + Sync + 'static {
    /// Accepts one isolated callback job.
    fn execute(&self, job: Box<dyn FnOnce() + Send + 'static>);
}

/// Executes callback jobs immediately while retaining async API semantics.
#[derive(Debug, Default)]
pub struct InlineExecutor;

impl CallbackExecutor for InlineExecutor {
    fn execute(&self, job: Box<dyn FnOnce() + Send + 'static>) {
        job();
    }
}

/// Public callback shape without exposing function addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackKind {
    /// No registered handler.
    None,
    /// Legacy/v1 down-only asynchronous handler.
    DownOnlyAsync,
    /// V2 down-and-release asynchronous handler.
    DownReleaseAsync,
    /// V2 down-and-release synchronous handler returning consumption.
    DownRelease,
}

/// Result of registering a callback and default binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// A new identifier retained the requested default binding.
    BoundDefault,
    /// A persisted binding already existed and was retained.
    PreservedExisting,
    /// Another identifier used the requested default, so the new default was cleared.
    ConflictCleared,
}

/// Opaque receipt for one exact managed callback publication.
///
/// A receipt remains specific to the callback instance it was issued for, even
/// when the same owner replaces that identifier later.
#[derive(Clone)]
pub struct ManagedRegistrationToken {
    marker: Arc<()>,
}

impl std::fmt::Debug for ManagedRegistrationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRegistrationToken")
            .finish_non_exhaustive()
    }
}

impl ManagedRegistrationToken {
    fn new() -> Self {
        Self {
            marker: Arc::new(()),
        }
    }

    fn matches(&self, registration: &RegisteredCallback) -> bool {
        Arc::ptr_eq(&self.marker, &registration.marker)
    }
}

/// Error raised while registering or explicitly changing a managed binding.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetBindError {
    /// The identifier violates defensive length constraints.
    #[error("input binding identifier is invalid")]
    InvalidIdentifier,
    /// Another identifier already owns the exact non-null combination.
    #[error("input binding conflicts with another identifier")]
    Conflict {
        /// Existing identifier. This is data, not part of the closed error display.
        identifier: String,
    },
    /// Another owner generation already has a handler under this identifier.
    #[error("input binding handler belongs to another owner generation")]
    ForeignHandler,
}

/// Callback invocation result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InvokeOutcome {
    /// A callback accepted the transition.
    pub dispatched: bool,
    /// The synchronous callback requested consumption, or an async callback was dispatched.
    pub consumed: bool,
    /// A callback or executor panic was contained during this call.
    pub panicked: bool,
    /// The callback has exhausted its panic budget.
    pub disabled: bool,
}

/// Result of routing one platform-neutral input message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouteOutcome {
    /// The message must not continue to later WndProc consumers.
    pub consumed: bool,
    /// Number of callback transitions dispatched.
    pub dispatched: usize,
    /// Number of contained panics observed synchronously.
    pub callback_panics: u32,
}

impl RouteOutcome {
    fn merge_invoke(&mut self, outcome: InvokeOutcome) {
        self.dispatched += usize::from(outcome.dispatched);
        self.callback_panics += u32::from(outcome.panicked);
    }
}

/// Closed summary of one registered managed binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedBindSnapshot {
    /// Identifier used by addon APIs.
    pub identifier: String,
    /// Current binding.
    pub binding: InputBind,
    /// Whether input continues after a consuming callback.
    pub passthrough: bool,
    /// Registered callback shape.
    pub callback: CallbackKind,
    /// Callback owner, if present.
    pub owner: Option<OwnerGeneration>,
    /// Number of contained callback panics.
    pub callback_panics: u32,
    /// Whether the callback is disabled.
    pub callback_disabled: bool,
}

/// Result of loading `InputBinds.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadReport {
    /// Valid entries applied to the registry.
    pub loaded: usize,
    /// Invalid or opaque entries preserved but not applied.
    pub skipped: usize,
}

type DownOnly = dyn Fn(&str) + Send + Sync + 'static;
type DownReleaseAsync = dyn Fn(&str, bool) + Send + Sync + 'static;
type DownRelease = dyn Fn(&str, bool) -> bool + Send + Sync + 'static;

#[derive(Clone)]
enum Callback {
    OnlyAsync(Arc<DownOnly>),
    ReleaseAsync(Arc<DownReleaseAsync>),
    Release(Arc<DownRelease>),
}

impl Callback {
    const fn kind(&self) -> CallbackKind {
        match self {
            Self::OnlyAsync(_) => CallbackKind::DownOnlyAsync,
            Self::ReleaseAsync(_) => CallbackKind::DownReleaseAsync,
            Self::Release(_) => CallbackKind::DownRelease,
        }
    }
}

#[derive(Debug)]
struct PanicGate {
    count: AtomicU32,
    disabled: AtomicBool,
    maximum: u32,
}

impl PanicGate {
    fn new(maximum: u32) -> Self {
        Self {
            count: AtomicU32::new(0),
            disabled: AtomicBool::new(false),
            maximum: maximum.max(1),
        }
    }

    fn record(&self) {
        let next = self
            .count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_add(1))
            })
            .map_or(u32::MAX, |previous| previous.saturating_add(1));
        if next >= self.maximum {
            self.disabled.store(true, Ordering::Release);
        }
    }
}

#[derive(Clone)]
struct RegisteredCallback {
    owner: OwnerGeneration,
    callback: Callback,
    gate: Arc<PanicGate>,
    marker: Arc<()>,
}

impl RegisteredCallback {
    fn new(
        owner: OwnerGeneration,
        callback: Callback,
        limits: CallbackLimits,
        marker: Arc<()>,
    ) -> Self {
        Self {
            owner,
            callback,
            gate: Arc::new(PanicGate::new(limits.max_panics)),
            marker,
        }
    }
}

#[derive(Clone)]
struct Mapping {
    binding: InputBind,
    passthrough: bool,
    callback: Option<RegisteredCallback>,
    extra_json: Map<String, Value>,
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            binding: InputBind::default(),
            passthrough: false,
            callback: None,
            extra_json: Map::new(),
        }
    }
}

#[derive(Clone)]
enum JsonSlot {
    Known(String),
    Opaque(Value),
}

#[derive(Default)]
struct State {
    registry: BTreeMap<String, Mapping>,
    held: BTreeMap<String, InputBind>,
    capture: InputCapture,
    json_slots: Vec<JsonSlot>,
}

/// Thread-safe managed Nexus input binding registry.
pub struct ManagedInputBinds {
    state: Mutex<State>,
    executor: Arc<dyn CallbackExecutor>,
    limits: CallbackLimits,
}

impl Default for ManagedInputBinds {
    fn default() -> Self {
        Self::new(Arc::new(InlineExecutor), CallbackLimits::default())
    }
}

impl ManagedInputBinds {
    /// Creates an engine with an injected executor and callback panic policy.
    #[must_use]
    pub fn new(executor: Arc<dyn CallbackExecutor>, limits: CallbackLimits) -> Self {
        Self {
            state: Mutex::new(State::default()),
            executor,
            limits,
        }
    }

    /// Registers a legacy/v1 down-only callback from a parsed string default.
    pub fn register_v1_string<F>(
        &self,
        identifier: &str,
        owner: OwnerGeneration,
        callback: F,
        default_binding: &str,
        resolver: &dyn KeyNameResolver,
    ) -> Result<RegisterOutcome, SetBindError>
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.register_v1_string_tracked(identifier, owner, callback, default_binding, resolver)
            .map(|(outcome, _)| outcome)
    }

    /// Registers a legacy/v1 callback and returns an exact publication receipt.
    pub fn register_v1_string_tracked<F>(
        &self,
        identifier: &str,
        owner: OwnerGeneration,
        callback: F,
        default_binding: &str,
        resolver: &dyn KeyNameResolver,
    ) -> Result<(RegisterOutcome, ManagedRegistrationToken), SetBindError>
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.register(
            identifier,
            owner,
            Callback::OnlyAsync(Arc::new(callback)),
            parse_bind_lossy(default_binding, resolver),
        )
    }

    /// Registers a legacy/v1 down-only callback from the ABI struct.
    pub fn register_v1<F>(
        &self,
        identifier: &str,
        owner: OwnerGeneration,
        callback: F,
        default_binding: LegacyInputBind,
    ) -> Result<RegisterOutcome, SetBindError>
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.register_v1_tracked(identifier, owner, callback, default_binding)
            .map(|(outcome, _)| outcome)
    }

    /// Registers a structured legacy/v1 callback and returns an exact receipt.
    pub fn register_v1_tracked<F>(
        &self,
        identifier: &str,
        owner: OwnerGeneration,
        callback: F,
        default_binding: LegacyInputBind,
    ) -> Result<(RegisterOutcome, ManagedRegistrationToken), SetBindError>
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.register(
            identifier,
            owner,
            Callback::OnlyAsync(Arc::new(callback)),
            default_binding.into(),
        )
    }

    /// Registers a v2 asynchronous down-and-release callback.
    pub fn register_v2_async<F, B>(
        &self,
        identifier: &str,
        owner: OwnerGeneration,
        callback: F,
        default_binding: B,
    ) -> Result<RegisterOutcome, SetBindError>
    where
        F: Fn(&str, bool) + Send + Sync + 'static,
        B: Into<InputBind>,
    {
        self.register_v2_async_tracked(identifier, owner, callback, default_binding)
            .map(|(outcome, _)| outcome)
    }

    /// Registers a v2 asynchronous callback and returns an exact receipt.
    pub fn register_v2_async_tracked<F, B>(
        &self,
        identifier: &str,
        owner: OwnerGeneration,
        callback: F,
        default_binding: B,
    ) -> Result<(RegisterOutcome, ManagedRegistrationToken), SetBindError>
    where
        F: Fn(&str, bool) + Send + Sync + 'static,
        B: Into<InputBind>,
    {
        self.register(
            identifier,
            owner,
            Callback::ReleaseAsync(Arc::new(callback)),
            default_binding.into(),
        )
    }

    /// Registers a v2 asynchronous down-and-release callback from a binding string.
    pub fn register_v2_async_string<F>(
        &self,
        identifier: &str,
        owner: OwnerGeneration,
        callback: F,
        default_binding: &str,
        resolver: &dyn KeyNameResolver,
    ) -> Result<RegisterOutcome, SetBindError>
    where
        F: Fn(&str, bool) + Send + Sync + 'static,
    {
        self.register_v2_async_string_tracked(
            identifier,
            owner,
            callback,
            default_binding,
            resolver,
        )
        .map(|(outcome, _)| outcome)
    }

    /// Registers a textual v2 asynchronous callback and returns an exact receipt.
    pub fn register_v2_async_string_tracked<F>(
        &self,
        identifier: &str,
        owner: OwnerGeneration,
        callback: F,
        default_binding: &str,
        resolver: &dyn KeyNameResolver,
    ) -> Result<(RegisterOutcome, ManagedRegistrationToken), SetBindError>
    where
        F: Fn(&str, bool) + Send + Sync + 'static,
    {
        self.register_v2_async_tracked(
            identifier,
            owner,
            callback,
            parse_bind_lossy(default_binding, resolver),
        )
    }

    /// Registers a v2 synchronous down-and-release callback.
    pub fn register_v2<F, B>(
        &self,
        identifier: &str,
        owner: OwnerGeneration,
        callback: F,
        default_binding: B,
    ) -> Result<RegisterOutcome, SetBindError>
    where
        F: Fn(&str, bool) -> bool + Send + Sync + 'static,
        B: Into<InputBind>,
    {
        self.register(
            identifier,
            owner,
            Callback::Release(Arc::new(callback)),
            default_binding.into(),
        )
        .map(|(outcome, _)| outcome)
    }

    /// Registers a v2 synchronous down-and-release callback from a binding string.
    pub fn register_v2_string<F>(
        &self,
        identifier: &str,
        owner: OwnerGeneration,
        callback: F,
        default_binding: &str,
        resolver: &dyn KeyNameResolver,
    ) -> Result<RegisterOutcome, SetBindError>
    where
        F: Fn(&str, bool) -> bool + Send + Sync + 'static,
    {
        self.register_v2(
            identifier,
            owner,
            callback,
            parse_bind_lossy(default_binding, resolver),
        )
    }

    /// Creates a persisted binding without installing a callback.
    pub fn register_default<B>(
        &self,
        identifier: &str,
        default_binding: B,
    ) -> Result<RegisterOutcome, SetBindError>
    where
        B: Into<InputBind>,
    {
        validate_identifier(identifier)?;
        let default_binding = default_binding.into();
        let mut state = self.lock_state();
        if state.registry.contains_key(identifier) {
            return Ok(RegisterOutcome::PreservedExisting);
        }
        let conflict = find_conflict(&state.registry, identifier, default_binding);
        let outcome = if conflict.is_some() {
            RegisterOutcome::ConflictCleared
        } else {
            RegisterOutcome::BoundDefault
        };
        state.registry.insert(
            identifier.to_owned(),
            Mapping {
                binding: if conflict.is_some() {
                    InputBind::default()
                } else {
                    default_binding.normalized()
                },
                ..Mapping::default()
            },
        );
        Ok(outcome)
    }

    /// Creates a persisted textual binding without installing a callback.
    pub fn register_default_string(
        &self,
        identifier: &str,
        default_binding: &str,
        resolver: &dyn KeyNameResolver,
    ) -> Result<RegisterOutcome, SetBindError> {
        self.register_default(identifier, parse_bind_lossy(default_binding, resolver))
    }

    fn register(
        &self,
        identifier: &str,
        owner: OwnerGeneration,
        callback: Callback,
        default_binding: InputBind,
    ) -> Result<(RegisterOutcome, ManagedRegistrationToken), SetBindError> {
        validate_identifier(identifier)?;
        let token = ManagedRegistrationToken::new();
        let mut pending = Some(RegisteredCallback::new(
            owner,
            callback,
            self.limits,
            Arc::clone(&token.marker),
        ));
        let update = {
            let mut state = self.lock_state();
            if let Some(existing) = state.registry.get_mut(identifier) {
                if existing
                    .callback
                    .as_ref()
                    .is_some_and(|registration| registration.owner != owner)
                {
                    Err(SetBindError::ForeignHandler)
                } else {
                    let registration = pending
                        .take()
                        .expect("pending managed registration must exist");
                    let replaced = existing.callback.replace(registration);
                    Ok((RegisterOutcome::PreservedExisting, replaced))
                }
            } else {
                let conflict = find_conflict(&state.registry, identifier, default_binding);
                let outcome = if conflict.is_some() {
                    RegisterOutcome::ConflictCleared
                } else {
                    RegisterOutcome::BoundDefault
                };
                let registration = pending
                    .take()
                    .expect("pending managed registration must exist");
                state.registry.insert(
                    identifier.to_owned(),
                    Mapping {
                        binding: if conflict.is_some() {
                            InputBind::default()
                        } else {
                            default_binding.normalized()
                        },
                        callback: Some(registration),
                        ..Mapping::default()
                    },
                );
                Ok((outcome, None))
            }
        };
        let abandoned = pending.take();
        drop(abandoned);
        let (outcome, replaced) = update?;
        drop(replaced);
        Ok((outcome, token))
    }

    /// Removes only the handler, retaining its persisted binding and passthrough flag.
    pub fn deregister(&self, identifier: &str) -> bool {
        let removed = {
            let mut state = self.lock_state();
            state.held.remove(identifier);
            state
                .registry
                .get_mut(identifier)
                .and_then(|mapping| mapping.callback.take())
        };
        let was_registered = removed.is_some();
        drop(removed);
        was_registered
    }

    /// Removes a handler only when it belongs to the authenticated owner generation.
    pub fn deregister_for_owner(&self, identifier: &str, owner: OwnerGeneration) -> bool {
        let removed = {
            let mut state = self.lock_state();
            let is_owned = state
                .registry
                .get(identifier)
                .and_then(|mapping| mapping.callback.as_ref())
                .is_some_and(|registration| registration.owner == owner);
            if is_owned {
                state.held.remove(identifier);
                state
                    .registry
                    .get_mut(identifier)
                    .and_then(|mapping| mapping.callback.take())
            } else {
                None
            }
        };
        let was_registered = removed.is_some();
        drop(removed);
        was_registered
    }

    /// Removes only the exact callback publication represented by `token`.
    pub fn remove_registration(&self, identifier: &str, token: &ManagedRegistrationToken) -> bool {
        let removed = {
            let mut state = self.lock_state();
            let is_exact = state
                .registry
                .get(identifier)
                .and_then(|mapping| mapping.callback.as_ref())
                .is_some_and(|registration| token.matches(registration));
            if is_exact {
                state.held.remove(identifier);
                state
                    .registry
                    .get_mut(identifier)
                    .and_then(|mapping| mapping.callback.take())
            } else {
                None
            }
        };
        let was_registered = removed.is_some();
        drop(removed);
        was_registered
    }

    /// Removes callbacks owned by exactly one load generation.
    ///
    /// Exact matching prevents a delayed cleanup for an old addon generation
    /// from deleting callbacks installed by its replacement.
    pub fn cleanup_owner_generation(&self, owner: OwnerGeneration) -> usize {
        let mut removed = Vec::new();
        {
            let mut state = self.lock_state();
            let identifiers: Vec<String> = state
                .registry
                .iter()
                .filter(|(_, mapping)| {
                    mapping
                        .callback
                        .as_ref()
                        .is_some_and(|callback| callback.owner == owner)
                })
                .map(|(identifier, _)| identifier.clone())
                .collect();
            removed.reserve(identifiers.len());
            for identifier in &identifiers {
                state.held.remove(identifier);
                if let Some(callback) = state
                    .registry
                    .get_mut(identifier)
                    .and_then(|mapping| mapping.callback.take())
                {
                    removed.push(callback);
                }
            }
        }
        let removed_count = removed.len();
        drop(removed);
        removed_count
    }

    /// Returns the conflicting identifier, if any.
    #[must_use]
    pub fn is_in_use(&self, binding: InputBind) -> Option<String> {
        if !binding.is_bound() {
            return None;
        }
        find_conflict(&self.lock_state().registry, "", binding)
    }

    /// Returns whether an enabled callback is registered.
    #[must_use]
    pub fn has_handler(&self, identifier: &str) -> bool {
        self.lock_state()
            .registry
            .get(identifier)
            .and_then(|mapping| mapping.callback.as_ref())
            .is_some_and(|callback| !callback.gate.disabled.load(Ordering::Acquire))
    }

    /// Returns a copy of one binding.
    #[must_use]
    pub fn get(&self, identifier: &str) -> Option<InputBind> {
        self.lock_state()
            .registry
            .get(identifier)
            .map(|mapping| mapping.binding)
    }

    /// Changes a binding, rejecting exact conflicts.
    pub fn set(&self, identifier: &str, binding: InputBind) -> Result<(), SetBindError> {
        validate_identifier(identifier)?;
        let mut state = self.lock_state();
        if let Some(conflict) = find_conflict(&state.registry, identifier, binding) {
            return Err(SetBindError::Conflict {
                identifier: conflict,
            });
        }
        state
            .registry
            .entry(identifier.to_owned())
            .or_default()
            .binding = binding.normalized();
        Ok(())
    }

    /// Changes whether callback consumption should pass through.
    pub fn set_passthrough(&self, identifier: &str, passthrough: bool) -> bool {
        let mut state = self.lock_state();
        let Some(mapping) = state.registry.get_mut(identifier) else {
            return false;
        };
        mapping.passthrough = passthrough;
        true
    }

    /// Deletes the binding and its handler entirely.
    pub fn delete(&self, identifier: &str) -> bool {
        let removed = {
            let mut state = self.lock_state();
            state.held.remove(identifier);
            state.registry.remove(identifier)
        };
        let was_registered = removed.is_some();
        drop(removed);
        was_registered
    }

    /// Starts continuous binding capture.
    pub fn start_capture(&self) {
        self.lock_state().capture.start();
    }

    /// Stops capture while retaining the captured combination.
    pub fn stop_capture(&self) {
        self.lock_state().capture.stop();
    }

    /// Returns the latest captured combination.
    #[must_use]
    pub fn captured(&self) -> InputBind {
        self.lock_state().capture.binding()
    }

    /// Invokes a binding by identifier.
    pub fn invoke(&self, identifier: &str, release: bool) -> InvokeOutcome {
        let callback = self
            .lock_state()
            .registry
            .get(identifier)
            .and_then(|mapping| mapping.callback.clone());
        callback.map_or_else(InvokeOutcome::default, |callback| {
            self.invoke_callback(identifier, release, callback)
        })
    }

    /// Routes an event after raw-input and overlay consumers have had a chance to consume it.
    pub fn route(&self, message: InputMessage) -> RouteOutcome {
        let capture = self.lock_state().capture.process(message);
        if capture.consumed {
            return RouteOutcome {
                consumed: true,
                ..RouteOutcome::default()
            };
        }

        match message {
            InputMessage::ActivateApp => self.release_all(),
            InputMessage::KeyDown { .. } | InputMessage::MouseDown(_)
                if capture.press_candidate && capture.binding.is_bound() =>
            {
                self.press(capture.binding)
            }
            InputMessage::KeyUp {
                modifier,
                scan_code,
            } => {
                let mut outcome = RouteOutcome::default();
                if let Some(modifier) = modifier {
                    outcome = self.release_matching(|binding| binding.requires(modifier));
                }
                let by_key = self.release_matching(|binding| {
                    binding.device == InputDevice::KEYBOARD && binding.code == scan_code
                });
                merge_route(&mut outcome, by_key);
                outcome
            }
            InputMessage::MouseUp(button) => self.release_matching(|binding| {
                binding.device == InputDevice::MOUSE && binding.code == button as u16
            }),
            _ => RouteOutcome::default(),
        }
    }

    fn press(&self, binding: InputBind) -> RouteOutcome {
        let selected = {
            let mut state = self.lock_state();
            let found = state.registry.iter().find_map(|(identifier, mapping)| {
                (mapping.binding == binding).then(|| (identifier.clone(), mapping.passthrough))
            });
            if let Some((identifier, passthrough)) = found {
                if state.held.contains_key(&identifier) {
                    None
                } else {
                    state.held.insert(identifier.clone(), binding);
                    let callback = state
                        .registry
                        .get(&identifier)
                        .and_then(|mapping| mapping.callback.clone());
                    Some((identifier, passthrough, callback))
                }
            } else {
                None
            }
        };

        let Some((identifier, passthrough, callback)) = selected else {
            return RouteOutcome::default();
        };
        let invoke = callback.map_or_else(InvokeOutcome::default, |callback| {
            self.invoke_callback(&identifier, false, callback)
        });
        RouteOutcome {
            consumed: invoke.consumed && !passthrough,
            dispatched: usize::from(invoke.dispatched),
            callback_panics: u32::from(invoke.panicked),
        }
    }

    fn release_matching(&self, predicate: impl Fn(InputBind) -> bool) -> RouteOutcome {
        let mut released = Vec::new();
        {
            let mut state = self.lock_state();
            let identifiers: Vec<String> = state
                .held
                .iter()
                .filter(|(_, binding)| predicate(**binding))
                .map(|(identifier, _)| identifier.clone())
                .collect();
            released.reserve(identifiers.len());
            for identifier in identifiers {
                state.held.remove(&identifier);
                let callback = state
                    .registry
                    .get(&identifier)
                    .and_then(|mapping| mapping.callback.clone());
                released.push((identifier, callback));
            }
        }
        self.invoke_releases(released)
    }

    /// Releases every currently held binding and dispatches matching callbacks.
    pub fn release_all(&self) -> RouteOutcome {
        let mut released = Vec::new();
        {
            let mut state = self.lock_state();
            let identifiers: Vec<String> = state.held.keys().cloned().collect();
            state.held.clear();
            released.reserve(identifiers.len());
            for identifier in identifiers {
                let callback = state
                    .registry
                    .get(&identifier)
                    .and_then(|mapping| mapping.callback.clone());
                released.push((identifier, callback));
            }
        }
        self.invoke_releases(released)
    }

    fn invoke_releases(
        &self,
        callbacks: Vec<(String, Option<RegisteredCallback>)>,
    ) -> RouteOutcome {
        let mut result = RouteOutcome::default();
        for (identifier, callback) in callbacks {
            if let Some(callback) = callback {
                result.merge_invoke(self.invoke_callback(&identifier, true, callback));
            }
        }
        result
    }

    fn invoke_callback(
        &self,
        identifier: &str,
        release: bool,
        registration: RegisteredCallback,
    ) -> InvokeOutcome {
        if registration.gate.disabled.load(Ordering::Acquire) {
            return InvokeOutcome {
                disabled: true,
                ..InvokeOutcome::default()
            };
        }

        match registration.callback {
            Callback::OnlyAsync(callback) => {
                if release {
                    return InvokeOutcome::default();
                }
                self.dispatch_async(identifier, registration.gate, move |identifier| {
                    callback(identifier);
                })
            }
            Callback::ReleaseAsync(callback) => {
                self.dispatch_async(identifier, registration.gate, move |identifier| {
                    callback(identifier, release);
                })
            }
            Callback::Release(callback) => {
                let before = registration.gate.count.load(Ordering::Acquire);
                let (dispatched, consumed) =
                    match catch_unwind(AssertUnwindSafe(|| callback(identifier, release))) {
                        Ok(consumed) => (true, consumed),
                        Err(payload) => {
                            std::mem::forget(payload);
                            registration.gate.record();
                            (false, false)
                        }
                    };
                let after = registration.gate.count.load(Ordering::Acquire);
                InvokeOutcome {
                    dispatched,
                    consumed,
                    panicked: after > before,
                    disabled: registration.gate.disabled.load(Ordering::Acquire),
                }
            }
        }
    }

    fn dispatch_async<F>(
        &self,
        identifier: &str,
        gate: Arc<PanicGate>,
        callback: F,
    ) -> InvokeOutcome
    where
        F: FnOnce(&str) + Send + 'static,
    {
        let identifier = identifier.to_owned();
        let before = gate.count.load(Ordering::Acquire);
        let job_gate = Arc::clone(&gate);
        let job =
            Box::new(
                move || match catch_unwind(AssertUnwindSafe(|| callback(&identifier))) {
                    Ok(()) => {}
                    Err(payload) => {
                        std::mem::forget(payload);
                        job_gate.record();
                    }
                },
            );
        let dispatched = match catch_unwind(AssertUnwindSafe(|| self.executor.execute(job))) {
            Ok(()) => true,
            Err(payload) => {
                std::mem::forget(payload);
                gate.record();
                false
            }
        };
        let after = gate.count.load(Ordering::Acquire);
        InvokeOutcome {
            dispatched,
            consumed: dispatched,
            panicked: after > before,
            disabled: gate.disabled.load(Ordering::Acquire),
        }
    }

    /// Returns a deterministic registry snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Vec<ManagedBindSnapshot> {
        self.lock_state()
            .registry
            .iter()
            .map(|(identifier, mapping)| {
                let callback = mapping.callback.as_ref();
                ManagedBindSnapshot {
                    identifier: identifier.clone(),
                    binding: mapping.binding,
                    passthrough: mapping.passthrough,
                    callback: callback.map_or(CallbackKind::None, |item| item.callback.kind()),
                    owner: callback.map(|item| item.owner),
                    callback_panics: callback
                        .map_or(0, |item| item.gate.count.load(Ordering::Acquire)),
                    callback_disabled: callback
                        .is_some_and(|item| item.gate.disabled.load(Ordering::Acquire)),
                }
            })
            .collect()
    }

    /// Atomically parses and applies compatible `InputBinds.json` text.
    pub fn load_json(&self, source: &str) -> Result<LoadReport, PersistenceError> {
        if source.len() > MAX_DOCUMENT_BYTES {
            return Err(PersistenceError::LimitExceeded);
        }
        let document: Value =
            serde_json::from_str(source).map_err(PersistenceError::InvalidJson)?;
        let entries = document.as_array().ok_or_else(|| {
            PersistenceError::InvalidJson(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "root is not an array",
            )))
        })?;
        if entries.len() > MAX_BINDINGS {
            return Err(PersistenceError::LimitExceeded);
        }

        let mut parsed = Vec::with_capacity(entries.len());
        let mut report = LoadReport::default();
        for entry in entries {
            match parse_json_mapping(entry) {
                Some((identifier, binding, passthrough, extras)) => {
                    parsed.push(JsonParsed::Known {
                        identifier,
                        binding,
                        passthrough,
                        extras,
                    });
                    report.loaded += 1;
                }
                None => {
                    parsed.push(JsonParsed::Opaque(entry.clone()));
                    report.skipped += 1;
                }
            }
        }

        let mut state = self.lock_state();
        state.json_slots.clear();
        let mut seen = BTreeSet::new();
        for entry in parsed {
            match entry {
                JsonParsed::Known {
                    identifier,
                    binding,
                    passthrough,
                    extras,
                } => {
                    if seen.insert(identifier.clone()) {
                        state.json_slots.push(JsonSlot::Known(identifier.clone()));
                    }
                    let mapping = state.registry.entry(identifier).or_default();
                    mapping.binding = binding;
                    mapping.passthrough = passthrough;
                    mapping.extra_json = extras;
                }
                JsonParsed::Opaque(value) => state.json_slots.push(JsonSlot::Opaque(value)),
            }
        }
        Ok(report)
    }

    /// Serializes compatible JSON with tab indentation and a trailing newline.
    pub fn save_json(&self) -> Result<String, PersistenceError> {
        let state = self.lock_state();
        let mut output = Vec::new();
        let mut emitted = BTreeSet::new();
        for slot in &state.json_slots {
            match slot {
                JsonSlot::Known(identifier) => {
                    if let Some(mapping) = state.registry.get(identifier) {
                        output.push(mapping_to_json(identifier, mapping));
                        emitted.insert(identifier.clone());
                    }
                }
                JsonSlot::Opaque(value) => output.push(value.clone()),
            }
        }
        for (identifier, mapping) in &state.registry {
            if !emitted.contains(identifier) {
                output.push(mapping_to_json(identifier, mapping));
            }
        }

        let mut bytes = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
        let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
        serde::Serialize::serialize(&output, &mut serializer)
            .map_err(PersistenceError::InvalidJson)?;
        bytes.push(b'\n');
        String::from_utf8(bytes).map_err(|error| {
            PersistenceError::InvalidJson(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )))
        })
    }

    /// Loads `InputBinds.json` from disk with a bounded read.
    pub fn load_json_file(&self, path: &Path) -> Result<LoadReport, PersistenceError> {
        let bytes = fs::read(path)?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(PersistenceError::LimitExceeded);
        }
        let source = std::str::from_utf8(&bytes).map_err(|error| {
            PersistenceError::InvalidJson(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )))
        })?;
        self.load_json(source)
    }

    /// Writes `InputBinds.json`, truncating the previous complete document.
    pub fn save_json_file(&self, path: &Path) -> Result<(), PersistenceError> {
        let json = self.save_json()?;
        let mut file = fs::File::create(path)?;
        file.write_all(json.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

enum JsonParsed {
    Known {
        identifier: String,
        binding: InputBind,
        passthrough: bool,
        extras: Map<String, Value>,
    },
    Opaque(Value),
}

fn parse_json_mapping(value: &Value) -> Option<(String, InputBind, bool, Map<String, Value>)> {
    let object = value.as_object()?;
    let identifier = object.get("Identifier")?.as_str()?.to_owned();
    if validate_identifier(&identifier).is_err() {
        return None;
    }
    if object.get("Key").is_none_or(Value::is_null) && object.get("Code").is_none_or(Value::is_null)
    {
        return None;
    }

    let alt = object.get("Alt").and_then(Value::as_bool).unwrap_or(false);
    let control = object.get("Ctrl").and_then(Value::as_bool).unwrap_or(false);
    let shift = object
        .get("Shift")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let passthrough = object
        .get("Passthrough")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (device, code) = match (object.get("Type"), object.get("Code")) {
        (Some(device), Some(code)) if !device.is_null() && !code.is_null() => (
            InputDevice(u32::try_from(device.as_u64()?).ok()?),
            u16::try_from(code.as_u64()?).ok()?,
        ),
        _ => (
            InputDevice::KEYBOARD,
            u16::try_from(object.get("Key")?.as_u64()?).ok()?,
        ),
    };
    let binding = InputBind::new(alt, control, shift, device, code).normalized();
    let mut extras = object.clone();
    for key in [
        "Identifier",
        "Alt",
        "Ctrl",
        "Shift",
        "Type",
        "Code",
        "Key",
        "Passthrough",
    ] {
        extras.remove(key);
    }
    Some((identifier, binding, passthrough, extras))
}

fn mapping_to_json(identifier: &str, mapping: &Mapping) -> Value {
    let mut object = mapping.extra_json.clone();
    object.insert(
        "Identifier".to_owned(),
        Value::String(identifier.to_owned()),
    );
    object.insert("Alt".to_owned(), Value::Bool(mapping.binding.alt));
    object.insert("Ctrl".to_owned(), Value::Bool(mapping.binding.control));
    object.insert("Shift".to_owned(), Value::Bool(mapping.binding.shift));
    object.insert(
        "Type".to_owned(),
        Value::from(if mapping.binding.code == 0 {
            InputDevice::NONE.0
        } else {
            mapping.binding.device.0
        }),
    );
    object.insert("Code".to_owned(), Value::from(mapping.binding.code));
    object.insert("Passthrough".to_owned(), Value::Bool(mapping.passthrough));
    Value::Object(object)
}

fn validate_identifier(identifier: &str) -> Result<(), SetBindError> {
    if identifier.is_empty() || identifier.len() > MAX_IDENTIFIER_BYTES || identifier.contains('\0')
    {
        Err(SetBindError::InvalidIdentifier)
    } else {
        Ok(())
    }
}

fn find_conflict(
    registry: &BTreeMap<String, Mapping>,
    identifier: &str,
    binding: InputBind,
) -> Option<String> {
    if !binding.is_bound() {
        return None;
    }
    registry.iter().find_map(|(candidate, mapping)| {
        (candidate != identifier && mapping.binding == binding).then(|| candidate.clone())
    })
}

fn merge_route(target: &mut RouteOutcome, source: RouteOutcome) {
    target.consumed |= source.consumed;
    target.dispatched += source.dispatched;
    target.callback_panics += source.callback_panics;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::{Modifier, MouseButton, UsKeyNames};

    fn keyboard(code: u16) -> InputBind {
        InputBind::new(false, false, false, InputDevice::KEYBOARD, code)
    }

    struct ReentrantDrop {
        engine: Arc<ManagedInputBinds>,
        observed_unlocked: Arc<AtomicBool>,
    }

    impl Drop for ReentrantDrop {
        fn drop(&mut self) {
            let unlocked = self.engine.state.try_lock().is_ok();
            if unlocked {
                let _ = self.engine.has_handler("drop-probe");
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

    struct PanicExecutor;

    impl CallbackExecutor for PanicExecutor {
        fn execute(&self, _job: Box<dyn FnOnce() + Send + 'static>) {
            std::panic::panic_any(PanicOnDrop);
        }
    }

    fn register_drop_probe(
        engine: &Arc<ManagedInputBinds>,
        identifier: &str,
        owner: OwnerGeneration,
        observed_unlocked: Arc<AtomicBool>,
    ) {
        let probe = ReentrantDrop {
            engine: Arc::clone(engine),
            observed_unlocked,
        };
        engine
            .register_v2(
                identifier,
                owner,
                move |_, _| {
                    let _ = &probe;
                    true
                },
                keyboard(0x3B),
            )
            .expect("drop probe should register");
    }

    #[test]
    fn v1_only_receives_press_and_v2_receives_press_then_release() {
        let engine = ManagedInputBinds::default();
        let v1_calls = Arc::new(AtomicUsize::new(0));
        let v1_capture = Arc::clone(&v1_calls);
        engine
            .register_v1_string(
                "v1",
                OwnerGeneration::new(1, 1),
                move |_| {
                    v1_capture.fetch_add(1, Ordering::Relaxed);
                },
                "F1",
                &UsKeyNames,
            )
            .expect("v1 registration should succeed");

        let v2_events = Arc::new(Mutex::new(Vec::new()));
        let v2_capture = Arc::clone(&v2_events);
        engine
            .register_v2_async(
                "v2",
                OwnerGeneration::new(2, 1),
                move |_, release| {
                    v2_capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(release);
                },
                keyboard(0x3C),
            )
            .expect("v2 registration should succeed");

        engine.route(InputMessage::KeyDown {
            modifier: None,
            scan_code: 0x3B,
            repeat: false,
        });
        engine.route(InputMessage::KeyUp {
            modifier: None,
            scan_code: 0x3B,
        });
        engine.route(InputMessage::KeyDown {
            modifier: None,
            scan_code: 0x3C,
            repeat: false,
        });
        engine.route(InputMessage::KeyUp {
            modifier: None,
            scan_code: 0x3C,
        });

        assert_eq!(v1_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            *v2_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [false, true]
        );
    }

    #[test]
    fn required_modifier_release_releases_held_bind() {
        let engine = ManagedInputBinds::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&events);
        engine
            .register_v2_async(
                "modified",
                OwnerGeneration::new(1, 1),
                move |_, release| {
                    capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(release);
                },
                InputBind::new(false, true, false, InputDevice::KEYBOARD, 0x3B),
            )
            .expect("registration should succeed");
        engine.route(InputMessage::KeyDown {
            modifier: Some(Modifier::Control),
            scan_code: 0x1D,
            repeat: false,
        });
        engine.route(InputMessage::KeyDown {
            modifier: None,
            scan_code: 0x3B,
            repeat: false,
        });
        engine.route(InputMessage::KeyUp {
            modifier: Some(Modifier::Control),
            scan_code: 0x1D,
        });
        assert_eq!(
            *events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            [false, true]
        );
    }

    #[test]
    fn conflict_and_passthrough_match_legacy_rules() {
        let engine = ManagedInputBinds::default();
        engine
            .register_v2(
                "first",
                OwnerGeneration::new(1, 1),
                |_, _| true,
                keyboard(0x3B),
            )
            .expect("first registration should succeed");
        assert_eq!(
            engine
                .register_v2(
                    "second",
                    OwnerGeneration::new(2, 1),
                    |_, _| true,
                    keyboard(0x3B),
                )
                .expect("second registration should succeed"),
            RegisterOutcome::ConflictCleared
        );
        assert_eq!(engine.get("second"), Some(InputBind::default()));
        assert!(engine.set_passthrough("first", true));
        let outcome = engine.route(InputMessage::KeyDown {
            modifier: None,
            scan_code: 0x3B,
            repeat: false,
        });
        assert!(outcome.dispatched > 0);
        assert!(!outcome.consumed);
    }

    #[test]
    fn cleanup_is_generation_exact() {
        let engine = ManagedInputBinds::default();
        engine
            .register_v2(
                "old-bind",
                OwnerGeneration::new(7, 1),
                |_, _| true,
                keyboard(0x3B),
            )
            .expect("first generation should register");
        engine
            .register_v2(
                "new-bind",
                OwnerGeneration::new(7, 2),
                |_, _| true,
                keyboard(0x3C),
            )
            .expect("second generation should register");
        assert_eq!(
            engine.cleanup_owner_generation(OwnerGeneration::new(7, 1)),
            1
        );
        assert!(!engine.has_handler("old-bind"));
        assert!(engine.has_handler("new-bind"));
        assert_eq!(
            engine.cleanup_owner_generation(OwnerGeneration::new(7, 2)),
            1
        );
        assert!(!engine.has_handler("new-bind"));
    }

    #[test]
    fn handler_replacement_and_deregistration_are_owner_scoped() {
        let engine = ManagedInputBinds::default();
        let owner = OwnerGeneration::new(7, 1);
        let foreign = OwnerGeneration::new(8, 1);
        engine
            .register_v2("bind", owner, |_, _| true, keyboard(0x3B))
            .expect("first owner should register");

        let error = engine
            .register_v2("bind", foreign, |_, _| true, keyboard(0x3C))
            .expect_err("foreign owner must not replace a live handler");
        assert_eq!(error, SetBindError::ForeignHandler);
        assert_eq!(engine.snapshot()[0].owner, Some(owner));
        assert!(!engine.deregister_for_owner("bind", foreign));
        assert!(engine.has_handler("bind"));
        assert!(engine.deregister_for_owner("bind", owner));
        assert!(!engine.has_handler("bind"));
    }

    #[test]
    fn stale_registration_receipt_cannot_remove_same_owner_replacement() {
        let engine = ManagedInputBinds::default();
        let owner = OwnerGeneration::new(7, 1);
        let (_, stale) = engine
            .register_v2_async_tracked("bind", owner, |_, _| {}, keyboard(0x3B))
            .expect("first registration should succeed");
        let (_, current) = engine
            .register_v2_async_tracked("bind", owner, |_, _| {}, keyboard(0x3C))
            .expect("same owner should replace its handler");

        assert!(!engine.remove_registration("bind", &stale));
        assert!(engine.has_handler("bind"));
        assert!(engine.remove_registration("bind", &current));
        assert!(!engine.has_handler("bind"));
    }

    #[test]
    fn null_callback_registration_preserves_persisted_state_and_live_handler() {
        let engine = ManagedInputBinds::default();
        assert_eq!(
            engine
                .register_default_string("persisted", "F1", &UsKeyNames)
                .expect("default should register"),
            RegisterOutcome::BoundDefault
        );
        assert_eq!(engine.get("persisted"), Some(keyboard(0x3B)));
        assert!(!engine.has_handler("persisted"));

        engine
            .register_v2(
                "live",
                OwnerGeneration::new(9, 1),
                |_, _| true,
                keyboard(0x3C),
            )
            .expect("live handler should register");
        assert_eq!(
            engine
                .register_default("live", keyboard(0x3D))
                .expect("existing state should be preserved"),
            RegisterOutcome::PreservedExisting
        );
        assert_eq!(engine.get("live"), Some(keyboard(0x3C)));
        assert!(engine.has_handler("live"));
    }

    #[test]
    fn callback_destructors_run_after_registry_unlocks() {
        let engine = Arc::new(ManagedInputBinds::default());
        let deregistered = Arc::new(AtomicBool::new(false));
        register_drop_probe(
            &engine,
            "drop-probe",
            OwnerGeneration::new(1, 1),
            Arc::clone(&deregistered),
        );
        assert!(engine.deregister("drop-probe"));
        assert!(deregistered.load(Ordering::Acquire));

        let engine = Arc::new(ManagedInputBinds::default());
        let replaced = Arc::new(AtomicBool::new(false));
        register_drop_probe(
            &engine,
            "drop-probe",
            OwnerGeneration::new(2, 1),
            Arc::clone(&replaced),
        );
        engine
            .register_v2(
                "drop-probe",
                OwnerGeneration::new(2, 1),
                |_, _| true,
                keyboard(0x3C),
            )
            .expect("replacement should register");
        assert!(replaced.load(Ordering::Acquire));

        let engine = Arc::new(ManagedInputBinds::default());
        let cleaned = Arc::new(AtomicBool::new(false));
        register_drop_probe(
            &engine,
            "drop-probe",
            OwnerGeneration::new(3, 1),
            Arc::clone(&cleaned),
        );
        assert_eq!(
            engine.cleanup_owner_generation(OwnerGeneration::new(3, 1)),
            1
        );
        assert!(cleaned.load(Ordering::Acquire));

        let engine = Arc::new(ManagedInputBinds::default());
        let deleted = Arc::new(AtomicBool::new(false));
        register_drop_probe(
            &engine,
            "drop-probe",
            OwnerGeneration::new(4, 1),
            Arc::clone(&deleted),
        );
        assert!(engine.delete("drop-probe"));
        assert!(deleted.load(Ordering::Acquire));
    }

    #[test]
    fn panic_payloads_with_panicking_destructors_are_forgotten() {
        let engine = ManagedInputBinds::default();
        engine
            .register_v2(
                "sync-panic",
                OwnerGeneration::new(1, 1),
                |_, _| -> bool { std::panic::panic_any(PanicOnDrop) },
                keyboard(0x3B),
            )
            .expect("sync panic callback should register");
        assert!(engine.invoke("sync-panic", false).panicked);

        engine
            .register_v2_async(
                "async-panic",
                OwnerGeneration::new(2, 1),
                |_, _| std::panic::panic_any(PanicOnDrop),
                keyboard(0x3C),
            )
            .expect("async panic callback should register");
        assert!(engine.invoke("async-panic", false).panicked);

        let engine =
            ManagedInputBinds::new(Arc::new(PanicExecutor), CallbackLimits { max_panics: 1 });
        engine
            .register_v1(
                "executor-panic",
                OwnerGeneration::new(3, 1),
                |_| {},
                LegacyInputBind {
                    key: 0x3D,
                    alt: false,
                    control: false,
                    shift: false,
                },
            )
            .expect("executor panic callback should register");
        let outcome = engine.invoke("executor-panic", false);
        assert!(outcome.panicked);
        assert!(!outcome.dispatched);
    }

    #[test]
    fn callback_panics_are_contained_and_bounded() {
        let engine =
            ManagedInputBinds::new(Arc::new(InlineExecutor), CallbackLimits { max_panics: 2 });
        engine
            .register_v2(
                "panic",
                OwnerGeneration::new(1, 1),
                |_, _| -> bool { panic!("test callback panic") },
                keyboard(0x3B),
            )
            .expect("registration should succeed");
        assert!(engine.invoke("panic", false).panicked);
        let second = engine.invoke("panic", false);
        assert!(second.panicked);
        assert!(second.disabled);
        let third = engine.invoke("panic", false);
        assert!(!third.dispatched);
        assert!(third.disabled);
    }

    #[test]
    fn json_load_save_preserves_unknown_fields_and_opaque_entries() {
        let engine = ManagedInputBinds::default();
        let source = r#"[
            {"Identifier":"known","Alt":false,"Ctrl":true,"Shift":false,"Type":1,"Code":59,"Passthrough":true,"Future":{"x":1}},
            {"FutureOnly":42},
            {"Identifier":"legacy","Key":60,"Alt":false,"Ctrl":false,"Shift":true}
        ]"#;
        let report = engine.load_json(source).expect("fixture JSON should load");
        assert_eq!(
            report,
            LoadReport {
                loaded: 2,
                skipped: 1
            }
        );
        assert_eq!(
            engine.get("legacy"),
            Some(InputBind::new(
                false,
                false,
                true,
                InputDevice::KEYBOARD,
                60
            ))
        );
        let saved = engine.save_json().expect("registry should serialize");
        let value: Value = serde_json::from_str(&saved).expect("saved JSON should parse");
        let entries = value.as_array().expect("saved root should be an array");
        assert_eq!(entries[0]["Future"]["x"], 1);
        assert_eq!(entries[1]["FutureOnly"], 42);
        assert_eq!(entries[2]["Type"], 1);
        assert_eq!(entries[2]["Code"], 60);
    }

    #[test]
    fn capture_short_circuits_binding_callbacks() {
        let engine = ManagedInputBinds::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        engine
            .register_v1_string(
                "capture",
                OwnerGeneration::new(1, 1),
                move |_| {
                    callback_calls.fetch_add(1, Ordering::Relaxed);
                },
                "MMB",
                &UsKeyNames,
            )
            .expect("registration should succeed");
        engine.start_capture();
        let outcome = engine.route(InputMessage::MouseDown(MouseButton::Middle));
        assert!(outcome.consumed);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(engine.captured().code, MouseButton::Middle as u16);
    }
}
