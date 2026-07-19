use core::ffi::c_void;
use core::fmt;
use std::sync::Arc;

use nexus_abi::MinHookStatus;
use nexus_inline_hooks::{
    InlineHookService, MH_ERROR_MEMORY_PROTECT, MH_ERROR_UNSUPPORTED_FUNCTION, MH_OK,
};

use crate::{BackendFailure, NativeCallBoundary};

/// Caller-attributed MinHook compatibility adapter.
pub struct InlineHookApi {
    boundary: Arc<NativeCallBoundary>,
    hooks: Arc<InlineHookService>,
}

impl InlineHookApi {
    /// Creates an adapter around the process inline-hook registry.
    #[must_use]
    pub fn new(boundary: Arc<NativeCallBoundary>, hooks: Arc<InlineHookService>) -> Self {
        Self { boundary, hooks }
    }

    /// Creates a disabled owner-scoped hook and copies out its trampoline.
    ///
    /// The untrusted output pointer is never passed to the hook service. The
    /// trampoline is first written into local Rust memory, then copied through
    /// the checked native-memory boundary. A failed copy rolls the hook back.
    /// The detour address is never trusted as caller identity: the actual
    /// TLS/stack caller is resolved independently, and the address must map to
    /// that same current owner generation before registration is admitted.
    ///
    /// # Safety
    ///
    /// `target` and `detour` must denote live functions with compatible ABIs
    /// and signatures for the full hook lifetime. A non-null `original` must
    /// denote one live, aligned, exclusively writable pointer-sized object.
    /// Every trampoline user must have returned before hook removal or owner
    /// cleanup can destroy the trampoline; the backend cannot drain arbitrary
    /// application threads.
    pub unsafe fn create(
        &self,
        target: *mut c_void,
        detour: *mut c_void,
        original: *mut *mut c_void,
    ) -> MinHookStatus {
        let preflight = self.hooks.preflight_create(target, detour);
        if preflight != MH_OK {
            return self.service_status(preflight);
        }
        let owner = match self
            .boundary
            .resolve_owner_for_registered_address(detour.cast_const())
        {
            Ok(owner) => owner,
            Err(_) => return MH_ERROR_UNSUPPORTED_FUNCTION,
        };
        let mut publication_failed = false;
        let status = unsafe {
            // SAFETY: the legacy ABI owns target/detour signature validity,
            // lifetime, and trampoline-user quiescence. The publisher never
            // invokes the trampoline and writes it only through the checked
            // native-memory boundary.
            self.hooks
                .create_hook_transaction(owner, target, detour, |trampoline| {
                    if self.boundary.validate_current_owner(owner).is_err() {
                        publication_failed = true;
                        return MH_ERROR_UNSUPPORTED_FUNCTION;
                    }
                    if original.is_null() {
                        return MH_OK;
                    }

                    let copied = {
                        // SAFETY: the native ABI requires `original` to identify
                        // one live, aligned, exclusively writable pointer-sized
                        // output object. This closure runs inside the enclosing
                        // unsafe transaction call.
                        self.boundary
                            .write_usize(original.cast::<usize>(), trampoline as usize)
                    };
                    if copied.is_ok() {
                        MH_OK
                    } else {
                        publication_failed = true;
                        MH_ERROR_MEMORY_PROTECT
                    }
                })
        };
        if status != MH_OK && !publication_failed {
            self.service_status(status)
        } else {
            status
        }
    }

    /// Removes one caller-owned hook.
    ///
    /// The target is an opaque address identity and is never dereferenced here.
    /// Caller attribution comes from the actual TLS/stack call context; the
    /// supplied target can neither select nor impersonate an owner. A null
    /// target is rejected and never interpreted as an all-hooks operation.
    pub fn remove(&self, target: *mut c_void) -> MinHookStatus {
        let preflight = self.hooks.preflight_remove(target);
        if preflight != MH_OK {
            return self.service_status(preflight);
        }
        let owner = match self.boundary.resolve_owner(None) {
            Ok(owner) => owner,
            Err(_) => return MH_ERROR_UNSUPPORTED_FUNCTION,
        };
        let status = self.hooks.remove_owned_hook(owner, target);
        self.service_status(status)
    }

    /// Enables one caller-owned hook, or all hooks owned by that exact live
    /// generation when `target` is null.
    ///
    /// The target is used only as an opaque registry key. Ownership is resolved
    /// independently from the actual TLS/stack call context and checked under
    /// the hook registry lock before any code patch is attempted.
    pub fn enable(&self, target: *mut c_void) -> MinHookStatus {
        let preflight = self.hooks.preflight_change();
        if preflight != MH_OK {
            return self.service_status(preflight);
        }
        let owner = match self.boundary.resolve_owner(None) {
            Ok(owner) => owner,
            Err(_) => return MH_ERROR_UNSUPPORTED_FUNCTION,
        };
        let status = self.hooks.enable_owned_hook(owner, target);
        self.service_status(status)
    }

    /// Disables one caller-owned hook, or all hooks owned by that exact live
    /// generation when `target` is null.
    ///
    /// The target is used only as an opaque registry key. Ownership is resolved
    /// independently from the actual TLS/stack call context and checked under
    /// the hook registry lock before any code patch is attempted.
    pub fn disable(&self, target: *mut c_void) -> MinHookStatus {
        let preflight = self.hooks.preflight_change();
        if preflight != MH_OK {
            return self.service_status(preflight);
        }
        let owner = match self.boundary.resolve_owner(None) {
            Ok(owner) => owner,
            Err(_) => return MH_ERROR_UNSUPPORTED_FUNCTION,
        };
        let status = self.hooks.disable_owned_hook(owner, target);
        self.service_status(status)
    }

    fn service_status(&self, status: MinHookStatus) -> MinHookStatus {
        if status != MH_OK {
            self.boundary
                .failures()
                .record(BackendFailure::ServiceRejected);
        }
        status
    }
}

impl fmt::Debug for InlineHookApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InlineHookApi")
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, windows, target_arch = "x86_64"))]
mod tests {
    use core::ffi::c_void;
    use core::num::NonZeroUsize;
    use std::sync::Arc;

    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::OwnerToken;
    use nexus_inline_hooks::{
        InlineHookService, MH_ERROR_NOT_CREATED, MH_ERROR_NOT_EXECUTABLE, MH_ERROR_NOT_INITIALIZED,
        MH_ERROR_UNSUPPORTED_FUNCTION, MH_OK,
    };
    use nexus_native_memory::NativeMemoryReader;

    use super::InlineHookApi;
    use crate::{BackendFailures, NativeCallBoundary};

    struct NoOwners;

    unsafe extern "C" fn target() {}

    unsafe extern "C" fn detour() {}

    impl AddressOwnerResolver for NoOwners {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            None
        }

        fn is_current_owner(&self, _owner: OwnerToken) -> bool {
            false
        }
    }

    #[test]
    fn unattributed_valid_calls_fail_without_registry_mutation() {
        let boundary = Arc::new(NativeCallBoundary::new(
            Arc::new(AddonCallerResolver::new(Arc::new(NoOwners))),
            NativeMemoryReader::default(),
            Arc::new(BackendFailures::new()),
        ));
        let hooks = Arc::new(InlineHookService::new());
        assert_eq!(hooks.initialize(), MH_OK);
        let api = InlineHookApi::new(boundary, hooks);

        let status = unsafe {
            // SAFETY: the test functions have identical ABIs and signatures,
            // remain live for the test, and the output pointer is null.
            api.create(
                target as *const () as *mut c_void,
                detour as *const () as *mut c_void,
                core::ptr::null_mut(),
            )
        };
        assert_eq!(status, MH_ERROR_UNSUPPORTED_FUNCTION);
        assert_eq!(api.boundary.failures().snapshot().caller_attribution, 1);
    }

    #[test]
    fn vendored_minhook_validation_precedes_safe_owner_attribution() {
        let boundary = Arc::new(NativeCallBoundary::new(
            Arc::new(AddonCallerResolver::new(Arc::new(NoOwners))),
            NativeMemoryReader::default(),
            Arc::new(BackendFailures::new()),
        ));
        let api = InlineHookApi::new(boundary, Arc::new(InlineHookService::new()));

        let uninitialized_create = unsafe {
            // SAFETY: the functions have matching signatures and process lifetime.
            api.create(
                target as *const () as *mut c_void,
                detour as *const () as *mut c_void,
                core::ptr::null_mut(),
            )
        };
        assert_eq!(uninitialized_create, MH_ERROR_NOT_INITIALIZED);
        assert_eq!(api.remove(core::ptr::null_mut()), MH_ERROR_NOT_INITIALIZED);
        assert_eq!(api.enable(core::ptr::null_mut()), MH_ERROR_NOT_INITIALIZED);
        assert_eq!(api.disable(core::ptr::null_mut()), MH_ERROR_NOT_INITIALIZED);
        assert_eq!(api.hooks.initialize(), MH_OK);

        let invalid_target = unsafe {
            // SAFETY: null is intentionally supplied and rejected by executable validation.
            api.create(
                core::ptr::null_mut(),
                detour as *const () as *mut c_void,
                core::ptr::null_mut(),
            )
        };
        assert_eq!(invalid_target, MH_ERROR_NOT_EXECUTABLE);
        let invalid_detour = unsafe {
            // SAFETY: null is intentionally supplied and rejected by executable validation.
            api.create(
                target as *const () as *mut c_void,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert_eq!(invalid_detour, MH_ERROR_NOT_EXECUTABLE);
        assert_eq!(api.remove(core::ptr::null_mut()), MH_ERROR_NOT_CREATED);
        assert_eq!(api.boundary.failures().snapshot().caller_attribution, 0);
    }

    mod owner_scoping {
        use core::{
            ffi::c_void,
            mem::size_of,
            num::NonZeroUsize,
            ptr::{self, NonNull},
        };
        use std::{
            collections::HashMap,
            hint::black_box,
            sync::{
                Arc, Mutex, MutexGuard,
                atomic::{AtomicI32, AtomicUsize, Ordering},
            },
        };

        use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
        use nexus_core::OwnerToken;
        use nexus_inline_hooks::{
            InlineHookService, MH_ERROR_DISABLED, MH_ERROR_ENABLED, MH_ERROR_MEMORY_PROTECT,
            MH_ERROR_NOT_CREATED, MH_ERROR_UNSUPPORTED_FUNCTION, MH_OK,
        };
        use nexus_native_memory::NativeMemoryReader;

        use super::super::InlineHookApi;
        use crate::{BackendFailures, NativeCallBoundary};

        type HookFn = extern "C" fn(i32) -> i32;

        const OWNER_A: OwnerToken = OwnerToken {
            signature: 101,
            generation: 7,
        };
        const OWNER_B: OwnerToken = OwnerToken {
            signature: 202,
            generation: 11,
        };
        const STALE_OWNER: OwnerToken = OwnerToken {
            signature: 303,
            generation: 1,
        };
        const CURRENT_OWNER: OwnerToken = OwnerToken {
            signature: 303,
            generation: 2,
        };

        static TEST_SERIALIZER: Mutex<()> = Mutex::new(());
        static TARGET_A_VALUE: AtomicI32 = AtomicI32::new(13);
        static TARGET_B_VALUE: AtomicI32 = AtomicI32::new(29);
        static TARGET_C_VALUE: AtomicI32 = AtomicI32::new(47);

        const MEM_COMMIT: u32 = 0x0000_1000;
        const MEM_RESERVE: u32 = 0x0000_2000;
        const MEM_RELEASE: u32 = 0x0000_8000;
        const PAGE_READONLY: u32 = 0x02;
        const PAGE_READWRITE: u32 = 0x04;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            #[link_name = "VirtualAlloc"]
            fn virtual_alloc(
                address: *const c_void,
                size: usize,
                allocation_type: u32,
                protection: u32,
            ) -> *mut c_void;
            #[link_name = "VirtualProtect"]
            fn virtual_protect(
                address: *const c_void,
                size: usize,
                protection: u32,
                old_protection: *mut u32,
            ) -> i32;
            #[link_name = "VirtualFree"]
            fn virtual_free(address: *mut c_void, size: usize, free_type: u32) -> i32;
        }

        struct ReadOnlyOutputCell {
            base: NonNull<c_void>,
        }

        impl ReadOnlyOutputCell {
            fn new(value: usize) -> Self {
                let allocation = unsafe {
                    // SAFETY: the flags are a documented committed private allocation request.
                    virtual_alloc(
                        ptr::null(),
                        size_of::<usize>(),
                        MEM_RESERVE | MEM_COMMIT,
                        PAGE_READWRITE,
                    )
                };
                let base = NonNull::new(allocation).expect("test page allocation succeeds");
                unsafe {
                    // SAFETY: `base` begins a writable allocation large enough for one `usize`.
                    base.cast::<usize>().as_ptr().write(value);
                }
                let mut old_protection = 0_u32;
                let changed = unsafe {
                    // SAFETY: the range lies inside the live allocation and the output is writable.
                    virtual_protect(
                        base.as_ptr(),
                        size_of::<usize>(),
                        PAGE_READONLY,
                        &mut old_protection,
                    )
                };
                if changed == 0 {
                    unsafe {
                        // SAFETY: `base` is the original live allocation result.
                        virtual_free(base.as_ptr(), 0, MEM_RELEASE);
                    }
                    panic!("test page protection change succeeds");
                }
                Self { base }
            }

            fn as_output_ptr(&self) -> *mut *mut c_void {
                self.base.as_ptr().cast()
            }

            fn value(&self) -> usize {
                unsafe {
                    // SAFETY: the allocation remains live and readable for this object's lifetime.
                    self.base.cast::<usize>().as_ptr().read()
                }
            }
        }

        impl Drop for ReadOnlyOutputCell {
            fn drop(&mut self) {
                let released = unsafe {
                    // SAFETY: `base` is the original live allocation result and
                    // `MEM_RELEASE` requires a zero size.
                    virtual_free(self.base.as_ptr(), 0, MEM_RELEASE)
                };
                debug_assert_ne!(released, 0);
            }
        }

        #[inline(never)]
        extern "C" fn target_a(value: i32) -> i32 {
            TARGET_A_VALUE
                .load(Ordering::Relaxed)
                .wrapping_add(black_box(value))
        }

        #[inline(never)]
        extern "C" fn target_b(value: i32) -> i32 {
            TARGET_B_VALUE
                .load(Ordering::Relaxed)
                .wrapping_add(black_box(value))
        }

        #[inline(never)]
        extern "C" fn target_c(value: i32) -> i32 {
            TARGET_C_VALUE
                .load(Ordering::Relaxed)
                .wrapping_add(black_box(value))
        }

        #[inline(never)]
        extern "C" fn detour_a(value: i32) -> i32 {
            black_box(value).wrapping_add(101)
        }

        #[inline(never)]
        extern "C" fn detour_b(value: i32) -> i32 {
            black_box(value).wrapping_add(202)
        }

        #[inline(never)]
        extern "C" fn detour_c(value: i32) -> i32 {
            black_box(value).wrapping_add(303)
        }

        #[derive(Default)]
        struct OwnerState {
            addresses: HashMap<NonZeroUsize, OwnerToken>,
            current_generations: HashMap<u32, u64>,
        }

        struct TestOwners {
            state: Mutex<OwnerState>,
            current_checks: AtomicUsize,
            reject_at_current_check: AtomicUsize,
        }

        impl Default for TestOwners {
            fn default() -> Self {
                Self {
                    state: Mutex::new(OwnerState::default()),
                    current_checks: AtomicUsize::new(0),
                    reject_at_current_check: AtomicUsize::new(0),
                }
            }
        }

        impl TestOwners {
            fn activate(&self, owner: OwnerToken) {
                self.lock_state()
                    .current_generations
                    .insert(owner.signature, owner.generation);
            }

            fn register(&self, address: NonZeroUsize, owner: OwnerToken) {
                self.lock_state().addresses.insert(address, owner);
            }

            fn reject_after_additional_current_checks(&self, additional_checks: usize) {
                let current = self.current_checks.load(Ordering::Relaxed);
                self.reject_at_current_check
                    .store(current + additional_checks, Ordering::Relaxed);
            }

            fn lock_state(&self) -> MutexGuard<'_, OwnerState> {
                match self.state.lock() {
                    Ok(state) => state,
                    Err(poisoned) => poisoned.into_inner(),
                }
            }
        }

        impl AddressOwnerResolver for TestOwners {
            fn owner_for_address(&self, address: NonZeroUsize) -> Option<OwnerToken> {
                self.lock_state().addresses.get(&address).copied()
            }

            fn is_current_owner(&self, owner: OwnerToken) -> bool {
                let current_check = self.current_checks.fetch_add(1, Ordering::Relaxed) + 1;
                let reject_at = self.reject_at_current_check.load(Ordering::Relaxed);
                if reject_at != 0 && current_check >= reject_at {
                    return false;
                }
                self.lock_state()
                    .current_generations
                    .get(&owner.signature)
                    .is_some_and(|generation| *generation == owner.generation)
            }
        }

        struct Fixture {
            owners: Arc<TestOwners>,
            callers: Arc<AddonCallerResolver>,
            hooks: Arc<InlineHookService>,
            api: InlineHookApi,
        }

        impl Fixture {
            fn new() -> Self {
                let owners = Arc::new(TestOwners::default());
                let callers = Arc::new(AddonCallerResolver::new(owners.clone()));
                let boundary = Arc::new(NativeCallBoundary::new(
                    callers.clone(),
                    NativeMemoryReader::default(),
                    Arc::new(BackendFailures::new()),
                ));
                let hooks = Arc::new(InlineHookService::new());
                assert_eq!(hooks.initialize(), MH_OK);
                let api = InlineHookApi::new(boundary, hooks.clone());
                Self {
                    owners,
                    callers,
                    hooks,
                    api,
                }
            }

            fn activate(&self, owner: OwnerToken) {
                self.owners.activate(owner);
            }

            fn create(&self, owner: OwnerToken, target: HookFn, detour: HookFn) {
                self.owners.register(function_address(detour), owner);
                let _scope = self
                    .callers
                    .enter_owner_scope(owner)
                    .expect("test owner should be current");
                let status = unsafe {
                    // SAFETY: Every test pair has the same ABI/signature and
                    // process-lifetime code; no trampoline output is requested.
                    self.api.create(
                        function_pointer(target),
                        function_pointer(detour),
                        core::ptr::null_mut(),
                    )
                };
                assert_eq!(status, MH_OK);
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = self.hooks.uninitialize();
            }
        }

        fn function_address(function: HookFn) -> NonZeroUsize {
            NonZeroUsize::new(function_pointer(function) as usize)
                .expect("function pointers are non-null")
        }

        fn function_pointer(function: HookFn) -> *mut c_void {
            function as *const () as *mut c_void
        }

        fn serialize_tests() -> MutexGuard<'static, ()> {
            match TEST_SERIALIZER.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            }
        }

        #[test]
        fn cross_owner_control_is_denied_without_mutating_the_hook() {
            let _test = serialize_tests();
            let fixture = Fixture::new();
            fixture.activate(OWNER_A);
            fixture.activate(OWNER_B);
            fixture.create(OWNER_A, target_a, detour_a);

            let _scope = fixture
                .callers
                .enter_owner_scope(OWNER_B)
                .expect("second owner should be current");
            let target = function_pointer(target_a);
            assert_eq!(fixture.api.enable(target), MH_ERROR_NOT_CREATED);
            assert_eq!(fixture.api.disable(target), MH_ERROR_NOT_CREATED);
            assert_eq!(fixture.api.remove(target), MH_ERROR_NOT_CREATED);
            assert_eq!(fixture.hooks.owned_hook_count(OWNER_A), 1);
        }

        #[test]
        fn null_enable_and_disable_are_owner_scoped_but_remove_is_rejected() {
            let _test = serialize_tests();
            let fixture = Fixture::new();
            fixture.activate(OWNER_A);
            fixture.activate(OWNER_B);
            fixture.create(OWNER_A, target_a, detour_a);
            fixture.create(OWNER_A, target_b, detour_b);
            fixture.create(OWNER_B, target_c, detour_c);

            {
                let _scope = fixture
                    .callers
                    .enter_owner_scope(OWNER_A)
                    .expect("first owner should be current");
                assert_eq!(fixture.api.enable(core::ptr::null_mut()), MH_OK);
                assert_eq!(
                    fixture.api.enable(function_pointer(target_a)),
                    MH_ERROR_ENABLED
                );
            }
            {
                let _scope = fixture
                    .callers
                    .enter_owner_scope(OWNER_B)
                    .expect("second owner should be current");
                assert_eq!(
                    fixture.api.disable(function_pointer(target_c)),
                    MH_ERROR_DISABLED
                );
                assert_eq!(fixture.api.enable(function_pointer(target_c)), MH_OK);
            }
            {
                let _scope = fixture
                    .callers
                    .enter_owner_scope(OWNER_A)
                    .expect("first owner should be current");
                assert_eq!(fixture.api.disable(core::ptr::null_mut()), MH_OK);
                assert_eq!(
                    fixture.api.disable(function_pointer(target_a)),
                    MH_ERROR_DISABLED
                );
                assert_eq!(
                    fixture.api.remove(core::ptr::null_mut()),
                    MH_ERROR_NOT_CREATED
                );
            }

            assert_eq!(fixture.hooks.owned_hook_count(OWNER_A), 2);
            assert_eq!(fixture.hooks.owned_hook_count(OWNER_B), 1);
            let _scope = fixture
                .callers
                .enter_owner_scope(OWNER_B)
                .expect("second owner should be current");
            assert_eq!(
                fixture.api.enable(function_pointer(target_c)),
                MH_ERROR_ENABLED
            );
            assert_eq!(
                fixture.api.remove(core::ptr::null_mut()),
                MH_ERROR_NOT_CREATED
            );
            assert_eq!(fixture.hooks.hook_count(), 3);

            let cleanup = fixture
                .hooks
                .cleanup_owner(OWNER_A)
                .expect("trusted owner cleanup should remain available");
            assert_eq!(cleanup.retired(), 2);
            assert_eq!(fixture.hooks.owned_hook_count(OWNER_A), 0);
            assert_eq!(fixture.hooks.owned_hook_count(OWNER_B), 1);
            assert_eq!(fixture.hooks.uninitialize(), MH_OK);
            assert_eq!(fixture.hooks.hook_count(), 0);
        }

        #[test]
        fn failed_trampoline_copy_out_rolls_back_only_the_new_owner_hook() {
            let _test = serialize_tests();
            let fixture = Fixture::new();
            fixture.activate(OWNER_A);
            fixture.activate(OWNER_B);
            fixture.create(OWNER_B, target_a, detour_a);
            fixture.owners.register(function_address(detour_b), OWNER_A);

            let sentinel = usize::MAX / 3;
            let output = ReadOnlyOutputCell::new(sentinel);
            let _scope = fixture
                .callers
                .enter_owner_scope(OWNER_A)
                .expect("first owner should be current");
            let status = unsafe {
                // SAFETY: target/detour have matching ABIs and process lifetime.
                // The output is a live, aligned pointer-sized cell deliberately
                // protected read-only so the checked copy-out rejects it.
                fixture.api.create(
                    function_pointer(target_b),
                    function_pointer(detour_b),
                    output.as_output_ptr(),
                )
            };

            assert_eq!(status, MH_ERROR_MEMORY_PROTECT);
            assert_eq!(output.value(), sentinel);
            assert_eq!(fixture.hooks.owned_hook_count(OWNER_A), 0);
            assert_eq!(fixture.hooks.owned_hook_count(OWNER_B), 1);
            assert_eq!(fixture.hooks.hook_count(), 1);
            assert_eq!(
                fixture
                    .hooks
                    .enable_owned_hook(OWNER_B, function_pointer(target_a)),
                MH_OK
            );
        }

        #[test]
        fn registration_rolls_back_if_cleanup_closes_after_attribution() {
            let _serial = serialize_tests();
            let fixture = Fixture::new();
            fixture.activate(OWNER_A);
            fixture.activate(OWNER_B);
            fixture.create(OWNER_B, target_b, detour_b);
            assert_eq!(fixture.hooks.hook_count(), 1);

            fixture.owners.register(function_address(detour_a), OWNER_A);
            fixture.owners.reject_after_additional_current_checks(4);
            let _scope = fixture
                .callers
                .enter_owner_scope(OWNER_A)
                .expect("owner should be current before attribution");
            let status = unsafe {
                // SAFETY: the target/detour pair has the same ABI/signature and
                // process-lifetime code; no trampoline output is requested.
                fixture.api.create(
                    function_pointer(target_a),
                    function_pointer(detour_a),
                    ptr::null_mut(),
                )
            };

            assert_eq!(status, MH_ERROR_UNSUPPORTED_FUNCTION);
            assert_eq!(fixture.hooks.owned_hook_count(OWNER_A), 0);
            assert_eq!(fixture.hooks.owned_hook_count(OWNER_B), 1);
            assert_eq!(fixture.hooks.hook_count(), 1);
        }

        #[test]
        fn stale_caller_scope_cannot_control_the_reloaded_generation() {
            let _test = serialize_tests();
            let fixture = Fixture::new();
            fixture.activate(STALE_OWNER);
            fixture.create(STALE_OWNER, target_a, detour_a);
            let stale_scope = fixture
                .callers
                .enter_owner_scope(STALE_OWNER)
                .expect("old generation starts current");

            fixture.activate(CURRENT_OWNER);
            fixture.create(CURRENT_OWNER, target_b, detour_b);
            let current_target = function_pointer(target_b);
            assert_eq!(
                fixture.api.remove(current_target),
                MH_ERROR_UNSUPPORTED_FUNCTION
            );
            assert_eq!(
                fixture.api.enable(core::ptr::null_mut()),
                MH_ERROR_UNSUPPORTED_FUNCTION
            );
            assert_eq!(fixture.hooks.owned_hook_count(STALE_OWNER), 1);
            assert_eq!(fixture.hooks.owned_hook_count(CURRENT_OWNER), 1);
            assert_eq!(
                fixture.hooks.remove_owned_hook(STALE_OWNER, current_target),
                MH_ERROR_NOT_CREATED
            );

            drop(stale_scope);
            let _scope = fixture
                .callers
                .enter_owner_scope(CURRENT_OWNER)
                .expect("new generation should be current");
            assert_eq!(fixture.api.remove(current_target), MH_OK);
        }

        #[test]
        fn same_owner_can_enable_disable_and_remove_its_hook() {
            let _test = serialize_tests();
            let fixture = Fixture::new();
            fixture.activate(OWNER_A);
            fixture.create(OWNER_A, target_a, detour_a);

            let _scope = fixture
                .callers
                .enter_owner_scope(OWNER_A)
                .expect("owner should be current");
            let target = function_pointer(target_a);
            assert_eq!(fixture.api.enable(target), MH_OK);
            assert_eq!(fixture.api.disable(target), MH_OK);
            assert_eq!(fixture.api.remove(target), MH_OK);
            assert_eq!(fixture.hooks.owned_hook_count(OWNER_A), 0);
        }
    }
}
