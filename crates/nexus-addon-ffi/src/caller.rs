//! Native caller attribution with explicit lifecycle scopes and stack fallback.

use core::cell::RefCell;
use core::ffi::c_void;
use core::fmt;
use core::marker::PhantomData;
use core::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;

use nexus_core::{AddressOwnershipIndex, CallbackGate, OwnerToken};

#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;

const STACK_FRAME_CAPACITY: usize = 64;

/// Authoritative mapping between native code addresses and live add-on owners.
///
/// The production runtime uses [`AddressOwnershipIndex`] so native API calls
/// never need to reacquire the manager lock while the manager is invoking an
/// addon callback. Implementations must be thread-safe and reentrancy-safe.
/// The caller resolver contains implementation panics and treats them as an
/// attribution failure.
pub trait AddressOwnerResolver: Send + Sync + 'static {
    /// Returns the owner of one non-null native code address, if it is owned.
    ///
    /// Implementations must not dereference the address and must return the
    /// currently registered generation rather than cached retired ownership.
    fn owner_for_address(&self, address: NonZeroUsize) -> Option<OwnerToken>;

    /// Returns whether the complete owner token is still the active generation.
    ///
    /// This check is required for address results and explicit owner scopes so
    /// a stale generation always fails closed.
    fn is_current_owner(&self, owner: OwnerToken) -> bool;

    /// Clones the host-owned callback gate for one exact current generation.
    ///
    /// Resolvers that cannot prove the gate comes from their own ownership
    /// authority must fail closed with `None`.
    fn callback_gate_for_current(&self, _owner: OwnerToken) -> Option<Arc<CallbackGate>> {
        None
    }
}

impl AddressOwnerResolver for AddressOwnershipIndex {
    fn owner_for_address(&self, address: NonZeroUsize) -> Option<OwnerToken> {
        self.owner_for_address(address)
    }

    fn is_current_owner(&self, owner: OwnerToken) -> bool {
        self.is_current_owner(owner)
    }

    fn callback_gate_for_current(&self, owner: OwnerToken) -> Option<Arc<CallbackGate>> {
        AddressOwnershipIndex::callback_gate_for_current(self, owner)
    }
}

/// Resolves the native add-on generation responsible for an API call.
///
/// Resolution is deterministic and ordered: an optional function-address hint
/// is validated first, then the innermost explicit thread-local owner scope,
/// then a fixed-size Windows stack capture. Every candidate is revalidated by
/// the injected [`AddressOwnerResolver`] before it is returned.
pub struct AddonCallerResolver {
    address_owners: Arc<dyn AddressOwnerResolver>,
    stack_capture: Arc<dyn StackCapture>,
}

impl AddonCallerResolver {
    /// Creates a production resolver backed by Windows stack capture.
    #[must_use]
    pub fn new(address_owners: Arc<dyn AddressOwnerResolver>) -> Self {
        Self {
            address_owners,
            stack_capture: Arc::new(WindowsStackCapture),
        }
    }

    /// Resolves the current add-on caller, failing closed on absence or panic.
    ///
    /// `function_hint` is an optional non-null function address already copied
    /// from the native ABI. This method never dereferences, formats, or stores
    /// it. The injected ownership resolver is the validation authority.
    ///
    /// A validated hint is intentionally a compatibility attribution source,
    /// not proof that the owner supplied the hint. Authorization paths that
    /// accept a caller-controlled function address must use
    /// [`Self::resolve_registered_address`] instead.
    #[must_use]
    pub fn resolve(&self, function_hint: Option<NonZeroUsize>) -> Option<OwnerToken> {
        contain_panic(|| self.resolve_inner(function_hint)).unwrap_or_default()
    }

    fn resolve_inner(&self, function_hint: Option<NonZeroUsize>) -> Option<OwnerToken> {
        if let Some(address) = function_hint {
            match self.validated_address(address) {
                Ok(Some(owner)) => return Some(owner),
                Ok(None) => {}
                Err(()) => return None,
            }
        }

        self.resolve_actual_inner()
    }

    /// Resolves the actual live caller without trusting a supplied address.
    ///
    /// Explicit callback scopes are checked first, followed by native stack
    /// capture. This is the authority source for owner-scoped mutations.
    #[must_use]
    pub fn resolve_actual(&self) -> Option<OwnerToken> {
        contain_panic(|| self.resolve_actual_inner()).unwrap_or_default()
    }

    /// Authenticates a caller-controlled registered function address.
    ///
    /// The actual caller is resolved independently from TLS/stack state. The
    /// supplied address is then mapped exactly and must belong to the same
    /// current owner generation. The address is never dereferenced or logged.
    #[must_use]
    pub fn resolve_registered_address(&self, address: NonZeroUsize) -> Option<OwnerToken> {
        contain_panic(|| self.resolve_registered_address_inner(address)).unwrap_or_default()
    }

    /// Returns whether an opaque native address belongs to one exact current
    /// addon generation.
    ///
    /// The address is never dereferenced, retained, or formatted. This method
    /// is intended for validating retained data addresses after the caller was
    /// independently authenticated.
    #[must_use]
    pub fn address_belongs_to_owner(&self, address: NonZeroUsize, owner: OwnerToken) -> bool {
        contain_panic(
            || matches!(self.validated_address(address), Ok(Some(found)) if found == owner),
        )
        .unwrap_or(false)
    }

    /// Checks whether a retained data address is compatible with one live
    /// owner generation.
    ///
    /// Addon-image addresses are authoritative: a range mapped to another
    /// generation is rejected, including a closed generation whose mapping is
    /// retained until unload. An address absent from the addon-image index is
    /// admitted for legacy heap and TLS compatibility. That compatibility case
    /// deliberately does not prove allocation ownership or lifetime; the
    /// unsafe retained-pointer API must carry those obligations. Resolver
    /// panics and retired owners always fail closed.
    #[must_use]
    pub fn retained_address_allowed_for_owner(
        &self,
        address: NonZeroUsize,
        owner: OwnerToken,
    ) -> bool {
        contain_panic(|| {
            if !self.address_owners.is_current_owner(owner) {
                return false;
            }
            match self.address_owners.owner_for_address(address) {
                Some(mapped) => mapped == owner,
                None => true,
            }
        })
        .unwrap_or(false)
    }

    /// Returns whether an exact owner generation still accepts addon API calls.
    #[must_use]
    pub fn is_current_owner(&self, owner: OwnerToken) -> bool {
        contain_panic(|| matches!(self.owner_is_current(owner), Ok(true))).unwrap_or(false)
    }

    /// Clones the callback gate from the same authority used for attribution.
    ///
    /// Missing gates and resolver panics fail closed.
    #[must_use]
    pub fn callback_gate_for_current(&self, owner: OwnerToken) -> Option<Arc<CallbackGate>> {
        contain_panic(|| {
            self.address_owners
                .is_current_owner(owner)
                .then(|| self.address_owners.callback_gate_for_current(owner))
                .flatten()
        })
        .unwrap_or_default()
    }

    fn resolve_registered_address_inner(&self, address: NonZeroUsize) -> Option<OwnerToken> {
        let actual = self.resolve_actual_inner()?;
        let registered = self.validated_address(address).ok()??;
        (actual == registered).then_some(actual)
    }

    fn resolve_actual_inner(&self) -> Option<OwnerToken> {
        match current_scoped_owner() {
            Ok(Some(owner)) => match self.owner_is_current(owner) {
                Ok(true) => return Some(owner),
                Ok(false) => {}
                Err(()) => return None,
            },
            Ok(None) => {}
            Err(()) => return None,
        }

        self.resolve_stack()
    }

    /// Enters a nested explicit owner scope on the current thread.
    ///
    /// Native load and unload callback invocations should hold the returned
    /// guard for exactly the synchronous callback extent. The token is checked
    /// both before entry and on every resolution. Returns `None` when the owner
    /// is stale, the injected resolver panics, or thread-local state is not
    /// available.
    #[must_use]
    pub fn enter_owner_scope(&self, owner: OwnerToken) -> Option<AddonOwnerScope> {
        contain_panic(|| self.enter_owner_scope_inner(owner)).unwrap_or_default()
    }

    fn enter_owner_scope_inner(&self, owner: OwnerToken) -> Option<AddonOwnerScope> {
        match self.owner_is_current(owner) {
            Ok(true) => push_owner_scope(owner),
            Ok(false) | Err(()) => None,
        }
    }

    fn resolve_stack(&self) -> Option<OwnerToken> {
        let mut frames = [core::ptr::null_mut::<c_void>(); STACK_FRAME_CAPACITY];
        let captured = match contain_panic(|| self.stack_capture.capture(&mut frames)) {
            Ok(captured) => captured.min(frames.len()),
            Err(()) => return None,
        };

        for frame in &frames[..captured] {
            let Some(address) = NonZeroUsize::new(frame.addr()) else {
                continue;
            };
            match self.validated_address(address) {
                Ok(Some(owner)) => return Some(owner),
                Ok(None) => {}
                Err(()) => return None,
            }
        }
        None
    }

    fn validated_address(&self, address: NonZeroUsize) -> Result<Option<OwnerToken>, ()> {
        let owner = contain_panic(|| self.address_owners.owner_for_address(address))?;
        let Some(owner) = owner else {
            return Ok(None);
        };
        if self.owner_is_current(owner)? {
            Ok(Some(owner))
        } else {
            Ok(None)
        }
    }

    fn owner_is_current(&self, owner: OwnerToken) -> Result<bool, ()> {
        contain_panic(|| self.address_owners.is_current_owner(owner))
    }

    #[cfg(test)]
    fn with_stack_capture(
        address_owners: Arc<dyn AddressOwnerResolver>,
        stack_capture: Arc<dyn StackCapture>,
    ) -> Self {
        Self {
            address_owners,
            stack_capture,
        }
    }
}

impl fmt::Debug for AddonCallerResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AddonCallerResolver")
            .field("address_owners", &"injected resolver")
            .field("stack_capacity", &STACK_FRAME_CAPACITY)
            .finish()
    }
}

/// RAII guard for one explicit native-callback owner scope.
///
/// Scopes may nest and restore the previous innermost owner when dropped,
/// including during unwinding. A guard is deliberately not `Send` or `Sync`
/// because its state belongs to the thread that created it. Out-of-order drops
/// remove only the matching scope and do not resurrect an already-dropped one.
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<nexus_addon_ffi::AddonOwnerScope>();
/// ```
#[must_use = "dropping the guard immediately exits the owner scope"]
pub struct AddonOwnerScope {
    id: u64,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl fmt::Debug for AddonOwnerScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AddonOwnerScope")
            .field("state", &"active thread-local scope")
            .finish()
    }
}

impl Drop for AddonOwnerScope {
    fn drop(&mut self) {
        remove_owner_scope(self.id);
    }
}

#[derive(Clone, Copy)]
struct ScopeFrame {
    id: u64,
    owner: OwnerToken,
}

struct ScopeStack {
    next_id: u64,
    frames: Vec<ScopeFrame>,
}

impl ScopeStack {
    const fn new() -> Self {
        Self {
            next_id: 1,
            frames: Vec::new(),
        }
    }

    fn push(&mut self, owner: OwnerToken) -> Option<u64> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1)?;
        self.frames.push(ScopeFrame { id, owner });
        Some(id)
    }
}

std::thread_local! {
    static OWNER_SCOPES: RefCell<ScopeStack> = const { RefCell::new(ScopeStack::new()) };
}

fn push_owner_scope(owner: OwnerToken) -> Option<AddonOwnerScope> {
    let id = OWNER_SCOPES
        .try_with(|scopes| scopes.try_borrow_mut().ok()?.push(owner))
        .ok()
        .flatten()?;
    Some(AddonOwnerScope {
        id,
        _not_send_or_sync: PhantomData,
    })
}

fn current_scoped_owner() -> Result<Option<OwnerToken>, ()> {
    OWNER_SCOPES
        .try_with(|scopes| {
            scopes
                .try_borrow()
                .map(|scopes| scopes.frames.last().map(|frame| frame.owner))
                .map_err(|_| ())
        })
        .map_err(|_| ())?
}

fn remove_owner_scope(id: u64) {
    let _ = OWNER_SCOPES.try_with(|scopes| {
        let Ok(mut scopes) = scopes.try_borrow_mut() else {
            return;
        };
        if let Some(index) = scopes.frames.iter().position(|frame| frame.id == id) {
            scopes.frames.remove(index);
        }
    });
}

trait StackCapture: Send + Sync + 'static {
    fn capture(&self, frames: &mut [*mut c_void]) -> usize;
}

struct WindowsStackCapture;

impl StackCapture for WindowsStackCapture {
    fn capture(&self, frames: &mut [*mut c_void]) -> usize {
        #[cfg(windows)]
        {
            // SAFETY: `frames` is a writable fixed-size output buffer for at
            // least `STACK_FRAME_CAPACITY` pointers, and the optional hash
            // output is intentionally null.
            let captured = unsafe {
                RtlCaptureStackBackTrace(
                    0,
                    STACK_FRAME_CAPACITY as u32,
                    frames.as_mut_ptr(),
                    core::ptr::null_mut(),
                )
            };
            usize::from(captured)
        }
        #[cfg(not(windows))]
        {
            let _ = frames;
            0
        }
    }
}

fn contain_panic<T>(operation: impl FnOnce() -> T) -> Result<T, ()> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(value) => Ok(value),
        Err(payload) => {
            // A custom panic payload may itself panic from Drop. Forgetting it
            // prevents that destructor from reopening an unwind path.
            core::mem::forget(payload);
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::collections::{HashMap, HashSet};
    use std::sync::{Mutex, MutexGuard, OnceLock, Weak};

    use nexus_core::CallbackGate;

    use super::*;

    #[derive(Default)]
    struct TestAddressOwners {
        owners: Mutex<HashMap<usize, OwnerToken>>,
        current: Mutex<HashSet<OwnerToken>>,
        panic_lookup: AtomicBool,
        panic_current: AtomicBool,
        lookup_count: AtomicUsize,
        reenter_once: AtomicBool,
        reentrant_caller: OnceLock<Weak<AddonCallerResolver>>,
        nested_result: Mutex<Option<Option<OwnerToken>>>,
    }

    impl TestAddressOwners {
        fn map(&self, address: NonZeroUsize, owner: OwnerToken) {
            lock(&self.owners).insert(address.get(), owner);
        }

        fn set_current(&self, owners: impl IntoIterator<Item = OwnerToken>) {
            let mut current = lock(&self.current);
            current.clear();
            current.extend(owners);
        }

        fn injected(self: &Arc<Self>) -> Arc<dyn AddressOwnerResolver> {
            Arc::clone(self) as Arc<dyn AddressOwnerResolver>
        }
    }

    impl AddressOwnerResolver for TestAddressOwners {
        fn owner_for_address(&self, address: NonZeroUsize) -> Option<OwnerToken> {
            self.lookup_count.fetch_add(1, Ordering::SeqCst);
            if self.panic_lookup.load(Ordering::SeqCst) {
                panic!("injected address lookup panic");
            }
            if self.reenter_once.swap(false, Ordering::SeqCst) {
                let caller = self.reentrant_caller.get().and_then(Weak::upgrade);
                let nested = caller.and_then(|caller| caller.resolve(None));
                *lock(&self.nested_result) = Some(nested);
            }
            lock(&self.owners).get(&address.get()).copied()
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            if self.panic_current.load(Ordering::SeqCst) {
                panic!("injected generation lookup panic");
            }
            lock(&self.current).contains(&owner)
        }
    }

    struct FixedStackCapture<const N: usize> {
        frames: [usize; N],
    }

    impl<const N: usize> StackCapture for FixedStackCapture<N> {
        fn capture(&self, frames: &mut [*mut c_void]) -> usize {
            for (output, address) in frames.iter_mut().zip(&self.frames) {
                *output = core::ptr::without_provenance_mut(*address);
            }
            self.frames.len()
        }
    }

    struct PanicStackCapture;

    impl StackCapture for PanicStackCapture {
        fn capture(&self, _frames: &mut [*mut c_void]) -> usize {
            panic!("injected stack capture panic");
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn address(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap_or_else(|| panic!("test address must be non-zero"))
    }

    const fn owner(signature: u32, generation: u64) -> OwnerToken {
        OwnerToken {
            signature,
            generation,
        }
    }

    fn fixed_capture<const N: usize>(frames: [usize; N]) -> Arc<dyn StackCapture> {
        Arc::new(FixedStackCapture { frames })
    }

    fn enter_scope(caller: &AddonCallerResolver, owner: OwnerToken) -> AddonOwnerScope {
        caller
            .enter_owner_scope(owner)
            .unwrap_or_else(|| panic!("test owner scope should be admitted"))
    }

    #[test]
    fn validated_hint_precedes_validated_scope_and_injected_frames() {
        let owners = Arc::new(TestAddressOwners::default());
        let hint_owner = owner(1, 1);
        let scope_owner = owner(2, 1);
        let stack_owner = owner(3, 1);
        owners.map(address(0x10), hint_owner);
        owners.map(address(0x30), stack_owner);
        owners.set_current([hint_owner, scope_owner, stack_owner]);
        let caller =
            AddonCallerResolver::with_stack_capture(owners.injected(), fixed_capture([0x20, 0x30]));

        let scope = enter_scope(&caller, scope_owner);
        assert_eq!(caller.resolve(Some(address(0x10))), Some(hint_owner));
        assert_eq!(caller.resolve(Some(address(0x40))), Some(scope_owner));
        drop(scope);
        assert_eq!(caller.resolve(Some(address(0x40))), Some(stack_owner));
    }

    #[test]
    fn registered_address_requires_the_independently_resolved_owner_generation() {
        let owners = Arc::new(TestAddressOwners::default());
        let actual = owner(10, 4);
        let other = owner(11, 2);
        let stale_actual = owner(actual.signature, actual.generation - 1);
        owners.map(address(0x10), other);
        owners.map(address(0x20), stale_actual);
        owners.map(address(0x30), actual);
        owners.set_current([actual, other]);
        let caller =
            AddonCallerResolver::with_stack_capture(owners.injected(), fixed_capture([0x30]));

        let scope = enter_scope(&caller, actual);
        assert_eq!(caller.resolve_actual(), Some(actual));
        assert_eq!(caller.resolve_registered_address(address(0x10)), None);
        assert_eq!(caller.resolve_registered_address(address(0x20)), None);
        assert!(!caller.address_belongs_to_owner(address(0x10), actual));
        assert!(!caller.address_belongs_to_owner(address(0x20), actual));
        assert!(caller.address_belongs_to_owner(address(0x30), actual));
        assert!(caller.is_current_owner(actual));
        assert_eq!(
            caller.resolve_registered_address(address(0x30)),
            Some(actual)
        );
        assert_eq!(caller.resolve_registered_address(address(0x40)), None);
        drop(scope);

        owners.set_current([other]);
        assert!(!caller.is_current_owner(actual));
        owners.set_current([actual, other]);

        assert_eq!(caller.resolve_actual(), Some(actual));
        assert_eq!(
            caller.resolve_registered_address(address(0x30)),
            Some(actual)
        );
    }

    #[test]
    fn retained_addresses_allow_unmapped_storage_but_fail_closed_for_foreign_or_untrusted_state() {
        let owners = Arc::new(TestAddressOwners::default());
        let actual = owner(12, 4);
        let other = owner(13, 2);
        owners.map(address(0x10), actual);
        owners.map(address(0x20), other);
        owners.set_current([actual, other]);
        let caller = AddonCallerResolver::new(owners.injected());

        assert!(caller.retained_address_allowed_for_owner(address(0x10), actual));
        assert!(!caller.retained_address_allowed_for_owner(address(0x20), actual));
        assert!(caller.retained_address_allowed_for_owner(address(0x30), actual));

        owners.set_current([other]);
        assert!(!caller.retained_address_allowed_for_owner(address(0x30), actual));
        owners.set_current([actual, other]);

        owners.panic_lookup.store(true, Ordering::SeqCst);
        assert!(!caller.retained_address_allowed_for_owner(address(0x30), actual));
        owners.panic_lookup.store(false, Ordering::SeqCst);
        owners.panic_current.store(true, Ordering::SeqCst);
        assert!(!caller.retained_address_allowed_for_owner(address(0x30), actual));
    }

    #[test]
    fn production_index_closes_admission_without_forgetting_mapped_bounds() {
        let index = Arc::new(AddressOwnershipIndex::new());
        let owner = owner(71, 3);
        let hint = address(0x7_1000);
        let gate = Arc::new(CallbackGate::open());
        index
            .publish(owner, hint, 0x1000, Arc::clone(&gate))
            .expect("fixture range should publish");
        let resolver = AddonCallerResolver::new(index.clone());

        assert_eq!(resolver.resolve(Some(hint)), Some(owner));
        let resolved_gate = resolver
            .callback_gate_for_current(owner)
            .expect("current owner should expose its exact callback gate");
        assert!(Arc::ptr_eq(&resolved_gate, &gate));
        assert!(index.close(owner));
        assert_eq!(index.owner_for_address(hint), Some(owner));
        assert_eq!(resolver.resolve(Some(hint)), None);
        assert!(resolver.callback_gate_for_current(owner).is_none());
    }

    #[test]
    fn authoritative_generation_checks_reject_stale_hints_scopes_and_frames() {
        let owners = Arc::new(TestAddressOwners::default());
        let stale = owner(7, 1);
        let current = owner(7, 2);
        owners.map(address(0x10), stale);
        owners.set_current([stale]);
        let caller =
            AddonCallerResolver::with_stack_capture(owners.injected(), fixed_capture([0x10]));
        let scope = enter_scope(&caller, stale);

        owners.set_current([current]);
        assert_eq!(caller.resolve(Some(address(0x10))), None);
        drop(scope);

        owners.map(address(0x10), current);
        assert_eq!(caller.resolve(Some(address(0x10))), Some(current));
    }

    #[test]
    fn nested_scopes_restore_through_ordered_out_of_order_and_unwind_drops() {
        let owners = Arc::new(TestAddressOwners::default());
        let outer_owner = owner(1, 10);
        let inner_owner = owner(2, 20);
        owners.set_current([outer_owner, inner_owner]);
        let caller =
            AddonCallerResolver::with_stack_capture(owners.injected(), fixed_capture::<0>([]));

        let outer = enter_scope(&caller, outer_owner);
        assert_eq!(caller.resolve(None), Some(outer_owner));
        let inner = enter_scope(&caller, inner_owner);
        assert_eq!(caller.resolve(None), Some(inner_owner));
        drop(inner);
        assert_eq!(caller.resolve(None), Some(outer_owner));

        let inner = enter_scope(&caller, inner_owner);
        drop(outer);
        assert_eq!(caller.resolve(None), Some(inner_owner));
        drop(inner);
        assert_eq!(caller.resolve(None), None);

        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _scope = enter_scope(&caller, outer_owner);
            panic!("test scope unwind");
        }));
        assert!(outcome.is_err());
        assert_eq!(caller.resolve(None), None);
    }

    #[test]
    fn resolver_callbacks_may_reenter_caller_resolution() {
        let owners = Arc::new(TestAddressOwners::default());
        let expected = owner(4, 5);
        owners.map(address(0x10), expected);
        owners.set_current([expected]);
        let caller = Arc::new(AddonCallerResolver::with_stack_capture(
            owners.injected(),
            fixed_capture([0x10]),
        ));
        assert!(owners.reentrant_caller.set(Arc::downgrade(&caller)).is_ok());
        owners.reenter_once.store(true, Ordering::SeqCst);

        assert_eq!(caller.resolve(Some(address(0x10))), Some(expected));
        assert_eq!(*lock(&owners.nested_result), Some(Some(expected)));
    }

    #[test]
    fn injected_panics_are_contained_and_fail_closed() {
        let owners = Arc::new(TestAddressOwners::default());
        let expected = owner(1, 1);
        owners.map(address(0x10), expected);
        owners.set_current([expected]);
        let caller =
            AddonCallerResolver::with_stack_capture(owners.injected(), fixed_capture([0x10]));

        owners.panic_lookup.store(true, Ordering::SeqCst);
        assert_eq!(caller.resolve(Some(address(0x10))), None);
        owners.panic_lookup.store(false, Ordering::SeqCst);
        owners.panic_current.store(true, Ordering::SeqCst);
        assert_eq!(caller.resolve(Some(address(0x10))), None);
        assert!(caller.enter_owner_scope(expected).is_none());

        owners.panic_current.store(false, Ordering::SeqCst);
        let panic_capture =
            AddonCallerResolver::with_stack_capture(owners.injected(), Arc::new(PanicStackCapture));
        assert_eq!(panic_capture.resolve(None), None);
    }

    #[test]
    fn real_non_owned_stack_fails_closed_without_exposing_addresses() {
        let owners = Arc::new(TestAddressOwners::default());
        let caller = AddonCallerResolver::new(owners.injected());
        assert_eq!(caller.resolve(None), None);
        #[cfg(windows)]
        assert!(owners.lookup_count.load(Ordering::SeqCst) > 0);

        let debug = format!("{caller:?}");
        assert!(!debug.contains("0x"));
        assert!(!debug.contains("pointer"));
    }

    #[test]
    fn scope_debug_output_is_address_free() {
        let owners = Arc::new(TestAddressOwners::default());
        let expected = owner(1, 1);
        owners.set_current([expected]);
        let caller =
            AddonCallerResolver::with_stack_capture(owners.injected(), fixed_capture::<0>([]));
        let scope = enter_scope(&caller, expected);
        let debug = format!("{scope:?}");
        assert!(!debug.contains("0x"));
        assert!(!debug.contains("pointer"));
    }
}
