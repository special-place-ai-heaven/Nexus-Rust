use std::{
    collections::HashMap,
    ffi::c_void,
    fmt,
    mem::{ManuallyDrop, size_of},
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Mutex, MutexGuard},
};

use nexus_core::OwnerToken;
use retour::{Error as RetourError, RawDetour};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, VirtualQuery,
};

use crate::{
    CleanupError, CleanupReport, MH_ERROR_ALREADY_CREATED, MH_ERROR_ALREADY_INITIALIZED,
    MH_ERROR_DISABLED, MH_ERROR_ENABLED, MH_ERROR_MEMORY_ALLOC, MH_ERROR_MEMORY_PROTECT,
    MH_ERROR_NOT_CREATED, MH_ERROR_NOT_EXECUTABLE, MH_ERROR_NOT_INITIALIZED,
    MH_ERROR_UNSUPPORTED_FUNCTION, MH_OK, MH_UNKNOWN, MinHookStatus,
    quiescence::{QuiescenceGuard, flush_instruction_cache},
};

/// A serialized, owner-aware MinHook-compatible inline-hook registry.
///
/// Lifecycle code should explicitly clean an owner or uninitialize the service
/// before unloading detour code. If final drop cannot safely quiesce the
/// process, the backing detour objects are intentionally leaked rather than
/// allowing their destructor to patch live code without the transaction guard.
pub struct InlineHookService {
    state: Mutex<State>,
}

impl InlineHookService {
    /// Creates an uninitialized hook service.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
        }
    }

    /// Initializes the MinHook-compatible registry.
    pub fn initialize(&self) -> MinHookStatus {
        self.with_status(|state| {
            if state.initialized {
                MH_ERROR_ALREADY_INITIALIZED
            } else {
                state.initialized = true;
                MH_OK
            }
        })
    }

    /// Disables and retires every hook, then uninitializes the registry.
    ///
    /// If any transition fails, the registry remains initialized and all
    /// still-registered entries remain available for a retry.
    pub fn uninitialize(&self) -> MinHookStatus {
        self.with_status(|state| {
            if !state.initialized {
                return MH_ERROR_NOT_INITIALIZED;
            }

            let keys = match collect_keys(state.hooks.keys().copied(), state.hooks.len()) {
                Ok(keys) => keys,
                Err(status) => return status,
            };
            let status = disable_for_retirement(state, &keys);
            if status != MH_OK {
                return status;
            }
            let retired = retire_keys(state, &keys);
            if retired != keys.len() || !state.hooks.is_empty() {
                return MH_UNKNOWN;
            }
            state.initialized = false;
            MH_OK
        })
    }

    /// Checks initialization and executable-address precedence for create.
    ///
    /// This is a snapshot-only compatibility preflight. Callers must still use
    /// [`Self::create_hook`] or [`Self::create_hook_transaction`], which repeat
    /// every check atomically with the registry mutation.
    pub fn preflight_create(&self, target: *mut c_void, detour: *mut c_void) -> MinHookStatus {
        self.with_status(|state| {
            if !state.initialized {
                return MH_ERROR_NOT_INITIALIZED;
            }
            if !is_executable(target.cast_const()) || !is_executable(detour.cast_const()) {
                return MH_ERROR_NOT_EXECUTABLE;
            }
            MH_OK
        })
    }

    /// Checks initialization and null-target precedence for remove.
    ///
    /// A non-null target is deliberately not looked up here because add-on
    /// ownership must be authenticated before registry membership is exposed.
    pub fn preflight_remove(&self, target: *mut c_void) -> MinHookStatus {
        self.with_status(|state| {
            if !state.initialized {
                return MH_ERROR_NOT_INITIALIZED;
            }
            if target.is_null() {
                return MH_ERROR_NOT_CREATED;
            }
            MH_OK
        })
    }

    /// Checks initialization precedence for enable and disable operations.
    ///
    /// Registry membership is deliberately not inspected because add-on
    /// ownership must be authenticated before hook presence is exposed.
    pub fn preflight_change(&self) -> MinHookStatus {
        self.with_status(|state| {
            if state.initialized {
                MH_OK
            } else {
                MH_ERROR_NOT_INITIALIZED
            }
        })
    }

    /// Creates a disabled hook owned by one exact addon generation.
    ///
    /// On success, a non-null `original` receives the generated trampoline.
    /// Failed calls leave both the registry and `original` unchanged.
    ///
    /// # Safety
    ///
    /// `target` and `detour` must have compatible function signatures and ABIs.
    /// When non-null, `original` must be valid and writable for one pointer. The
    /// caller must also ensure the target and detour code outlive the hook and
    /// quiesce every trampoline user before hook removal or owner cleanup can
    /// destroy it; this service cannot drain arbitrary application threads.
    pub unsafe fn create_hook(
        &self,
        owner: OwnerToken,
        target: *mut c_void,
        detour: *mut c_void,
        original: *mut *mut c_void,
    ) -> MinHookStatus {
        unsafe {
            // SAFETY: forwarded from this method's public contract. The
            // publisher writes only to the caller-provided output object.
            self.create_hook_transaction(owner, target, detour, |trampoline| {
                if !original.is_null() {
                    // SAFETY: the public contract requires one writable pointer.
                    original.write(trampoline);
                }
                MH_OK
            })
        }
    }

    /// Creates a disabled hook and publishes its trampoline transactionally.
    ///
    /// The registry lock remains held from owner admission through `publish`.
    /// Cleanup therefore either retires the owner before this call (and this
    /// call is rejected) or waits until publication or rollback is complete.
    /// Returning any status other than [`MH_OK`] from `publish` rolls back the
    /// exact newly-created hook before cleanup can continue. The publisher must
    /// not re-enter this hook service or retain the trampoline after returning
    /// a failure status. After success, the trampoline remains live only until
    /// the hook is removed or its exact owner generation is cleaned up. Every
    /// trampoline user must be quiescent before either lifetime-ending
    /// operation can destroy it; this service cannot drain arbitrary
    /// application threads.
    ///
    /// # Safety
    ///
    /// `target` and `detour` must have compatible function signatures and ABIs
    /// and must remain live for the hook lifetime. `publish` may use the
    /// trampoline only according to that same signature and ABI.
    pub unsafe fn create_hook_transaction(
        &self,
        owner: OwnerToken,
        target: *mut c_void,
        detour: *mut c_void,
        publish: impl FnOnce(*mut c_void) -> MinHookStatus,
    ) -> MinHookStatus {
        self.with_status(move |state| {
            if !state.initialized {
                return MH_ERROR_NOT_INITIALIZED;
            }
            let Some(target_key) = NonZeroUsize::new(target as usize) else {
                return MH_ERROR_NOT_EXECUTABLE;
            };
            if !is_executable(target.cast_const()) || !is_executable(detour.cast_const()) {
                return MH_ERROR_NOT_EXECUTABLE;
            }
            if owner_admission_retired(state, owner) {
                return MH_ERROR_UNSUPPORTED_FUNCTION;
            }
            if state.hooks.contains_key(&target_key) {
                return MH_ERROR_ALREADY_CREATED;
            }
            if state.hooks.try_reserve(1).is_err() {
                return MH_ERROR_MEMORY_ALLOC;
            }

            let raw = unsafe {
                // SAFETY: The public safety contract requires compatible, live code pointers;
                // retour validates executable memory and supported target instructions.
                RawDetour::new(target.cast_const().cast(), detour.cast_const().cast())
            };
            let raw = match raw {
                Ok(raw) => raw,
                Err(error) => return map_create_error(&error),
            };
            let trampoline = raw.trampoline() as *const () as *mut c_void;
            let replaced = state.hooks.insert(
                target_key,
                HookEntry {
                    owner,
                    detour: ManagedDetour::new(raw),
                },
            );
            debug_assert!(replaced.is_none());

            let publication = catch_unwind(AssertUnwindSafe(|| publish(trampoline)));
            let status = match publication {
                Ok(status) => status,
                Err(payload) => {
                    // A hostile panic payload must not reopen unwinding when
                    // forgotten at this native transaction boundary.
                    std::mem::forget(payload);
                    MH_UNKNOWN
                }
            };
            if status == MH_OK {
                return MH_OK;
            }

            if retire_keys(state, std::slice::from_ref(&target_key)) == 1 {
                status
            } else {
                MH_UNKNOWN
            }
        })
    }

    /// Removes one hook without applying an owner filter.
    ///
    /// This is a host-only operation for rollback and process shutdown. Add-on
    /// ABI adapters must use [`Self::remove_owned_hook`] so one add-on cannot
    /// remove another generation's hook. A null target is not treated as
    /// `MH_ALL_HOOKS`.
    pub fn remove_hook(&self, target: *mut c_void) -> MinHookStatus {
        self.with_status(|state| {
            if !state.initialized {
                return MH_ERROR_NOT_INITIALIZED;
            }
            let Some(target) = NonZeroUsize::new(target as usize) else {
                return MH_ERROR_NOT_CREATED;
            };
            remove_one(state, target)
        })
    }

    /// Removes one hook owned by `owner`.
    ///
    /// A non-null target owned by any other generation is reported as not
    /// created. A null target is rejected rather than interpreted as an
    /// all-hooks operation. The owner check and removal happen under the same
    /// registry lock.
    pub fn remove_owned_hook(&self, owner: OwnerToken, target: *mut c_void) -> MinHookStatus {
        self.with_status(|state| {
            if !state.initialized {
                return MH_ERROR_NOT_INITIALIZED;
            }
            if target.is_null() {
                return MH_ERROR_NOT_CREATED;
            }
            let Some(target) = NonZeroUsize::new(target as usize) else {
                return MH_ERROR_NOT_CREATED;
            };
            if !state
                .hooks
                .get(&target)
                .is_some_and(|hook| hook.owner == owner)
            {
                return MH_ERROR_NOT_CREATED;
            }
            remove_one(state, target)
        })
    }

    /// Enables one hook, or every hook when `target` is null (`MH_ALL_HOOKS`).
    ///
    /// This unfiltered compatibility operation is reserved for the host.
    pub fn enable_hook(&self, target: *mut c_void) -> MinHookStatus {
        self.change_hook(target, true)
    }

    /// Disables one hook, or every hook when `target` is null (`MH_ALL_HOOKS`).
    ///
    /// This unfiltered compatibility operation is reserved for the host.
    pub fn disable_hook(&self, target: *mut c_void) -> MinHookStatus {
        self.change_hook(target, false)
    }

    /// Enables one hook owned by `owner`, or all hooks owned by that exact
    /// generation when `target` is null.
    pub fn enable_owned_hook(&self, owner: OwnerToken, target: *mut c_void) -> MinHookStatus {
        self.change_owned_hook(owner, target, true)
    }

    /// Disables one hook owned by `owner`, or all hooks owned by that exact
    /// generation when `target` is null.
    pub fn disable_owned_hook(&self, owner: OwnerToken, target: *mut c_void) -> MinHookStatus {
        self.change_owned_hook(owner, target, false)
    }

    /// Retires hooks belonging to exactly `owner`, including its generation.
    ///
    /// The operation is idempotent. On failure, the error reports how many
    /// exact-owner entries remain so a lifecycle cleaner can retry safely.
    pub fn cleanup_owner(&self, owner: OwnerToken) -> Result<CleanupReport, CleanupError> {
        let mut state = self.lock_state();
        retire_owner_admission(&mut state, owner);
        let initial = count_owner(&state, owner);
        let result = catch_unwind(AssertUnwindSafe(|| cleanup_owner_inner(&mut state, owner)));
        match result {
            Ok(result) => result,
            Err(payload) => {
                // Native callers can panic with arbitrary payloads. Forget a
                // caught payload so an adversarial `Drop` cannot reopen the
                // unwind at this hook-management boundary.
                std::mem::forget(payload);
                let remaining = count_owner(&state, owner);
                Err(CleanupError::new(
                    owner,
                    MH_UNKNOWN,
                    initial.saturating_sub(remaining),
                    remaining,
                ))
            }
        }
    }

    /// Returns the total registered-hook count without exposing targets.
    #[must_use]
    pub fn hook_count(&self) -> usize {
        self.lock_state().hooks.len()
    }

    /// Returns the registered-hook count for one exact addon generation.
    #[must_use]
    pub fn owned_hook_count(&self, owner: OwnerToken) -> usize {
        count_owner(&self.lock_state(), owner)
    }

    fn change_hook(&self, target: *mut c_void, enable: bool) -> MinHookStatus {
        self.with_status(|state| {
            if !state.initialized {
                return MH_ERROR_NOT_INITIALIZED;
            }
            if target.is_null() {
                change_all(state, enable)
            } else {
                let Some(target) = NonZeroUsize::new(target as usize) else {
                    return MH_ERROR_NOT_CREATED;
                };
                change_one(state, target, enable)
            }
        })
    }

    fn change_owned_hook(
        &self,
        owner: OwnerToken,
        target: *mut c_void,
        enable: bool,
    ) -> MinHookStatus {
        self.with_status(|state| {
            if !state.initialized {
                return MH_ERROR_NOT_INITIALIZED;
            }
            if target.is_null() {
                change_all_owned(state, owner, enable)
            } else {
                let Some(target) = NonZeroUsize::new(target as usize) else {
                    return MH_ERROR_NOT_CREATED;
                };
                if !state
                    .hooks
                    .get(&target)
                    .is_some_and(|hook| hook.owner == owner)
                {
                    return MH_ERROR_NOT_CREATED;
                }
                change_one(state, target, enable)
            }
        })
    }

    fn with_status(&self, operation: impl FnOnce(&mut State) -> MinHookStatus) -> MinHookStatus {
        let mut state = self.lock_state();
        match catch_unwind(AssertUnwindSafe(|| operation(&mut state))) {
            Ok(status) => status,
            Err(payload) => {
                std::mem::forget(payload);
                MH_UNKNOWN
            }
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Default for InlineHookService {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for InlineHookService {
    fn drop(&mut self) {
        let _ = self.uninitialize();
    }
}

impl fmt::Debug for InlineHookService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock_state();
        formatter
            .debug_struct("InlineHookService")
            .field("initialized", &state.initialized)
            .field("hook_count", &state.hooks.len())
            .field("retired_signature_count", &state.retired_generations.len())
            .finish()
    }
}

#[derive(Default)]
struct State {
    initialized: bool,
    hooks: HashMap<NonZeroUsize, HookEntry>,
    retired_generations: HashMap<u32, u64>,
}

struct HookEntry {
    owner: OwnerToken,
    detour: ManagedDetour,
}

struct ManagedDetour {
    // RawDetour's debug destructor calls disable without our quiescence guard. ManuallyDrop makes
    // every real destructor call explicit and leaves failed final teardown safely leaked.
    raw: ManuallyDrop<RawDetour>,
}

impl ManagedDetour {
    fn new(raw: RawDetour) -> Self {
        Self {
            raw: ManuallyDrop::new(raw),
        }
    }

    fn is_enabled(&self) -> bool {
        self.raw.is_enabled()
    }

    unsafe fn enable(&self) -> retour::Result<()> {
        unsafe {
            // SAFETY: The caller establishes process-thread quiescence around the code patch.
            self.raw.enable()
        }
    }

    unsafe fn disable(&self) -> retour::Result<()> {
        unsafe {
            // SAFETY: The caller establishes quiescence whenever this can change target code.
            self.raw.disable()
        }
    }

    fn retire(self) {
        let raw = ManuallyDrop::into_inner(self.raw);
        drop(raw);
    }
}

fn remove_one(state: &mut State, target: NonZeroUsize) -> MinHookStatus {
    let Some(hook) = state.hooks.get(&target) else {
        return MH_ERROR_NOT_CREATED;
    };
    let was_enabled = hook.detour.is_enabled();
    let guard = if was_enabled {
        match QuiescenceGuard::acquire(&[target]) {
            Ok(guard) => Some(guard),
            Err(_) => return MH_ERROR_MEMORY_PROTECT,
        }
    } else {
        None
    };
    let result = unsafe {
        // SAFETY: An enabled hook is covered by `guard`, which keeps every
        // enumerated peer thread outside the patched target window. Disabling
        // an already-disabled hook performs no code write.
        hook.detour.disable()
    };
    if let Err(error) = result {
        return map_toggle_error(&error);
    }
    if was_enabled {
        flush_instruction_cache(target);
    }
    drop(guard);

    match state.hooks.remove(&target) {
        Some(entry) => {
            entry.detour.retire();
            MH_OK
        }
        None => MH_UNKNOWN,
    }
}

fn change_one(state: &mut State, target: NonZeroUsize, enable: bool) -> MinHookStatus {
    let Some(hook) = state.hooks.get(&target) else {
        return MH_ERROR_NOT_CREATED;
    };
    if hook.detour.is_enabled() == enable {
        return if enable {
            MH_ERROR_ENABLED
        } else {
            MH_ERROR_DISABLED
        };
    }
    let _guard = match QuiescenceGuard::acquire(&[target]) {
        Ok(guard) => guard,
        Err(_) => return MH_ERROR_MEMORY_PROTECT,
    };
    let result = unsafe {
        // SAFETY: `_guard` keeps every enumerated peer thread outside the target window.
        if enable {
            hook.detour.enable()
        } else {
            hook.detour.disable()
        }
    };
    match result {
        Ok(()) => {
            flush_instruction_cache(target);
            MH_OK
        }
        Err(error) => map_toggle_error(&error),
    }
}

fn change_all(state: &mut State, enable: bool) -> MinHookStatus {
    let targets = match collect_keys(
        state
            .hooks
            .iter()
            .filter_map(|(target, hook)| (hook.detour.is_enabled() != enable).then_some(*target)),
        state.hooks.len(),
    ) {
        Ok(targets) => targets,
        Err(status) => return status,
    };
    if targets.is_empty() {
        return MH_OK;
    }
    let _guard = match QuiescenceGuard::acquire(&targets) {
        Ok(guard) => guard,
        Err(_) => return MH_ERROR_MEMORY_PROTECT,
    };

    for target in targets {
        let Some(hook) = state.hooks.get(&target) else {
            return MH_UNKNOWN;
        };
        let result = unsafe {
            // SAFETY: `_guard` covers every target collected for this transaction.
            if enable {
                hook.detour.enable()
            } else {
                hook.detour.disable()
            }
        };
        if let Err(error) = result {
            return map_toggle_error(&error);
        }
        flush_instruction_cache(target);
    }
    MH_OK
}

fn change_all_owned(state: &mut State, owner: OwnerToken, enable: bool) -> MinHookStatus {
    let owned = count_owner(state, owner);
    let targets = match collect_keys(
        state.hooks.iter().filter_map(|(target, hook)| {
            (hook.owner == owner && hook.detour.is_enabled() != enable).then_some(*target)
        }),
        owned,
    ) {
        Ok(targets) => targets,
        Err(status) => return status,
    };
    if targets.is_empty() {
        return MH_OK;
    }
    let _guard = match QuiescenceGuard::acquire(&targets) {
        Ok(guard) => guard,
        Err(_) => return MH_ERROR_MEMORY_PROTECT,
    };

    for target in targets {
        let Some(hook) = state.hooks.get(&target) else {
            return MH_UNKNOWN;
        };
        debug_assert_eq!(hook.owner, owner);
        let result = unsafe {
            // SAFETY: `_guard` covers every exact-owner target collected for
            // this transaction, and the registry lock prevents replacement.
            if enable {
                hook.detour.enable()
            } else {
                hook.detour.disable()
            }
        };
        if let Err(error) = result {
            return map_toggle_error(&error);
        }
        flush_instruction_cache(target);
    }
    MH_OK
}

fn disable_for_retirement(state: &State, targets: &[NonZeroUsize]) -> MinHookStatus {
    let enabled = match collect_keys(
        targets.iter().copied().filter(|target| {
            state
                .hooks
                .get(target)
                .is_some_and(|hook| hook.detour.is_enabled())
        }),
        targets.len(),
    ) {
        Ok(enabled) => enabled,
        Err(status) => return status,
    };
    let guard = if enabled.is_empty() {
        None
    } else {
        match QuiescenceGuard::acquire(&enabled) {
            Ok(guard) => Some(guard),
            Err(_) => return MH_ERROR_MEMORY_PROTECT,
        }
    };

    for target in targets {
        let Some(hook) = state.hooks.get(target) else {
            return MH_UNKNOWN;
        };
        let was_enabled = hook.detour.is_enabled();
        let result = unsafe {
            // SAFETY: Enabled targets are covered by `guard`; disabled targets cause no write.
            hook.detour.disable()
        };
        if let Err(error) = result {
            return map_toggle_error(&error);
        }
        if was_enabled {
            flush_instruction_cache(*target);
        }
    }
    drop(guard);
    MH_OK
}

fn cleanup_owner_inner(
    state: &mut State,
    owner: OwnerToken,
) -> Result<CleanupReport, CleanupError> {
    let initial = count_owner(state, owner);
    if initial == 0 {
        return Ok(CleanupReport::new(owner, 0));
    }
    if !state.initialized {
        return Err(CleanupError::new(
            owner,
            MH_ERROR_NOT_INITIALIZED,
            0,
            initial,
        ));
    }
    let keys = collect_keys(
        state
            .hooks
            .iter()
            .filter_map(|(target, hook)| (hook.owner == owner).then_some(*target)),
        initial,
    )
    .map_err(|status| CleanupError::new(owner, status, 0, initial))?;
    let status = disable_for_retirement(state, &keys);
    if status != MH_OK {
        return Err(CleanupError::new(owner, status, 0, initial));
    }
    let retired = retire_keys(state, &keys);
    let remaining = count_owner(state, owner);
    if remaining == 0 {
        Ok(CleanupReport::new(owner, retired))
    } else {
        Err(CleanupError::new(owner, MH_UNKNOWN, retired, remaining))
    }
}

fn retire_keys(state: &mut State, targets: &[NonZeroUsize]) -> usize {
    let mut retired = 0;
    for target in targets {
        if let Some(entry) = state.hooks.remove(target) {
            entry.detour.retire();
            retired += 1;
        }
    }
    retired
}

fn collect_keys(
    targets: impl Iterator<Item = NonZeroUsize>,
    capacity: usize,
) -> Result<Vec<NonZeroUsize>, MinHookStatus> {
    let mut keys = Vec::new();
    keys.try_reserve(capacity)
        .map_err(|_| MH_ERROR_MEMORY_ALLOC)?;
    keys.extend(targets);
    Ok(keys)
}

fn retire_owner_admission(state: &mut State, owner: OwnerToken) {
    state
        .retired_generations
        .entry(owner.signature)
        .and_modify(|generation| *generation = (*generation).max(owner.generation))
        .or_insert(owner.generation);
}

fn owner_admission_retired(state: &State, owner: OwnerToken) -> bool {
    state
        .retired_generations
        .get(&owner.signature)
        .is_some_and(|generation| owner.generation <= *generation)
}

fn count_owner(state: &State, owner: OwnerToken) -> usize {
    state
        .hooks
        .values()
        .filter(|hook| hook.owner == owner)
        .count()
}

fn is_executable(address: *const c_void) -> bool {
    if address.is_null() {
        return false;
    }

    let mut information = MEMORY_BASIC_INFORMATION::default();
    let queried = unsafe {
        // SAFETY: VirtualQuery only describes the region containing `address` and writes to the
        // fully initialized output structure; it does not dereference the queried address.
        VirtualQuery(
            address,
            &mut information,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    const EXECUTABLE: u32 =
        PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    queried != 0 && information.State == MEM_COMMIT && information.Protect & EXECUTABLE != 0
}

fn map_create_error(error: &RetourError) -> MinHookStatus {
    match error {
        RetourError::NotExecutable => MH_ERROR_NOT_EXECUTABLE,
        RetourError::OutOfMemory => MH_ERROR_MEMORY_ALLOC,
        RetourError::RegionFailure(_) => MH_ERROR_MEMORY_PROTECT,
        RetourError::AlreadyInitialized => MH_ERROR_ALREADY_CREATED,
        RetourError::NotInitialized => MH_ERROR_NOT_CREATED,
        RetourError::SameAddress
        | RetourError::InvalidCode
        | RetourError::NoPatchArea
        | RetourError::UnsupportedInstruction => MH_ERROR_UNSUPPORTED_FUNCTION,
    }
}

fn map_toggle_error(error: &RetourError) -> MinHookStatus {
    match error {
        RetourError::RegionFailure(_) => MH_ERROR_MEMORY_PROTECT,
        RetourError::OutOfMemory => MH_ERROR_MEMORY_ALLOC,
        RetourError::NotInitialized => MH_ERROR_NOT_CREATED,
        RetourError::AlreadyInitialized => MH_ERROR_ENABLED,
        RetourError::NotExecutable => MH_ERROR_NOT_EXECUTABLE,
        RetourError::SameAddress
        | RetourError::InvalidCode
        | RetourError::NoPatchArea
        | RetourError::UnsupportedInstruction => MH_ERROR_UNSUPPORTED_FUNCTION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caught_panic_does_not_poison_service_mutex() {
        struct PanicOnDrop;

        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                panic!("panic payload destructor must not run");
            }
        }

        let service = InlineHookService::new();
        let status = service.with_status(|_| panic!("injected mutation panic"));
        assert_eq!(status, MH_UNKNOWN);
        let status = service.with_status(|_| std::panic::panic_any(PanicOnDrop));
        assert_eq!(status, MH_UNKNOWN);
        assert_eq!(service.initialize(), MH_OK);
        assert_eq!(service.uninitialize(), MH_OK);
    }

    #[test]
    fn service_is_send_and_sync_without_local_unsafe_impls() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InlineHookService>();
    }
}
