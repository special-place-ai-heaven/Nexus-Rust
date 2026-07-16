//! End-to-end Windows tests for MinHook parity and owner-aware retirement.

#![cfg(all(windows, target_arch = "x86_64"))]

use std::{
    ffi::c_void,
    hint::black_box,
    ptr,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicI32, Ordering},
    },
};

use nexus_core::OwnerToken;
use nexus_inline_hooks::{
    InlineHookService, MH_ERROR_ALREADY_CREATED, MH_ERROR_ALREADY_INITIALIZED, MH_ERROR_DISABLED,
    MH_ERROR_ENABLED, MH_ERROR_NOT_CREATED, MH_ERROR_NOT_EXECUTABLE, MH_ERROR_NOT_INITIALIZED,
    MH_ERROR_UNSUPPORTED_FUNCTION, MH_OK,
};

type HookFn = extern "C" fn(i32) -> i32;

static TEST_SERIALIZER: Mutex<()> = Mutex::new(());
static TARGET_A_VALUE: AtomicI32 = AtomicI32::new(11);
static TARGET_B_VALUE: AtomicI32 = AtomicI32::new(22);
static TARGET_C_VALUE: AtomicI32 = AtomicI32::new(33);
static TARGET_D_VALUE: AtomicI32 = AtomicI32::new(44);
static TARGET_E_VALUE: AtomicI32 = AtomicI32::new(55);
static TARGET_F_VALUE: AtomicI32 = AtomicI32::new(66);

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
extern "C" fn target_d(value: i32) -> i32 {
    TARGET_D_VALUE
        .load(Ordering::Relaxed)
        .wrapping_add(black_box(value))
}

#[inline(never)]
extern "C" fn target_e(value: i32) -> i32 {
    TARGET_E_VALUE
        .load(Ordering::Relaxed)
        .wrapping_add(black_box(value))
}

#[inline(never)]
extern "C" fn target_f(value: i32) -> i32 {
    TARGET_F_VALUE
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

#[inline(never)]
extern "C" fn detour_d(value: i32) -> i32 {
    black_box(value).wrapping_add(404)
}

#[inline(never)]
extern "C" fn detour_e(value: i32) -> i32 {
    black_box(value).wrapping_add(505)
}

#[inline(never)]
extern "C" fn detour_f(value: i32) -> i32 {
    black_box(value).wrapping_add(606)
}

#[test]
fn exact_statuses_trampoline_and_single_hook_lifecycle() {
    let _test = serialize_tests();
    let service = InlineHookService::new();
    let target = function_pointer(target_a);
    let detour = function_pointer(detour_a);

    assert_eq!(service.remove_hook(target), MH_ERROR_NOT_INITIALIZED);
    assert_eq!(
        service.enable_hook(ptr::null_mut()),
        MH_ERROR_NOT_INITIALIZED
    );
    assert_eq!(service.initialize(), MH_OK);
    assert_eq!(service.initialize(), MH_ERROR_ALREADY_INITIALIZED);

    let mut original = ptr::null_mut();
    let status = unsafe {
        // SAFETY: The test functions share a signature and remain live for the process.
        service.create_hook(owner(1), target, detour, &mut original)
    };
    assert_eq!(status, MH_OK);
    assert!(!original.is_null());
    assert_eq!(service.hook_count(), 1);
    assert_eq!(invoke(target_a, 5), 16);

    let mut unchanged = target;
    let duplicate = unsafe {
        // SAFETY: The pointers remain valid; the duplicate is rejected before mutation.
        service.create_hook(owner(1), target, detour, &mut unchanged)
    };
    assert_eq!(duplicate, MH_ERROR_ALREADY_CREATED);
    assert_eq!(unchanged, target);

    assert_eq!(service.enable_hook(target), MH_OK);
    assert_eq!(invoke(target_a, 5), 106);
    assert_eq!(service.enable_hook(target), MH_ERROR_ENABLED);
    let trampoline = unsafe {
        // SAFETY: A successful create returned a trampoline with `HookFn`'s signature.
        pointer_function(original)
    };
    assert_eq!(invoke(trampoline, 5), 16);

    assert_eq!(service.disable_hook(target), MH_OK);
    assert_eq!(invoke(target_a, 5), 16);
    assert_eq!(service.disable_hook(target), MH_ERROR_DISABLED);
    assert_eq!(service.remove_hook(ptr::null_mut()), MH_ERROR_NOT_CREATED);
    assert_eq!(service.remove_hook(target), MH_OK);
    assert_eq!(service.remove_hook(target), MH_ERROR_NOT_CREATED);
    assert_eq!(service.uninitialize(), MH_OK);
    assert_eq!(service.uninitialize(), MH_ERROR_NOT_INITIALIZED);
}

#[test]
fn all_hooks_and_generation_exact_cleanup_are_idempotent() {
    let _test = serialize_tests();
    let service = InlineHookService::new();
    let first_owner = OwnerToken {
        signature: 77,
        generation: 1,
    };
    let second_owner = OwnerToken {
        signature: 77,
        generation: 2,
    };
    assert_eq!(service.initialize(), MH_OK);
    create_without_original(&service, first_owner, target_b, detour_b);
    create_without_original(&service, second_owner, target_c, detour_c);

    assert_eq!(service.enable_hook(ptr::null_mut()), MH_OK);
    assert_eq!(invoke(target_b, 1), 203);
    assert_eq!(invoke(target_c, 1), 304);
    assert_eq!(service.enable_hook(ptr::null_mut()), MH_OK);
    assert_eq!(service.disable_hook(ptr::null_mut()), MH_OK);
    assert_eq!(invoke(target_b, 1), 23);
    assert_eq!(invoke(target_c, 1), 34);
    assert_eq!(service.disable_hook(ptr::null_mut()), MH_OK);
    assert_eq!(service.enable_hook(ptr::null_mut()), MH_OK);

    let report = match service.cleanup_owner(first_owner) {
        Ok(report) => report,
        Err(error) => panic!("unexpected cleanup error: {error}"),
    };
    assert_eq!(report.owner(), first_owner);
    assert_eq!(report.retired(), 1);
    assert_eq!(report.remaining(), 0);
    assert_eq!(service.owned_hook_count(first_owner), 0);
    assert_eq!(service.owned_hook_count(second_owner), 1);
    assert_eq!(invoke(target_b, 1), 23);
    assert_eq!(invoke(target_c, 1), 304);

    let retry = match service.cleanup_owner(first_owner) {
        Ok(report) => report,
        Err(error) => panic!("unexpected retry error: {error}"),
    };
    assert_eq!(retry.retired(), 0);
    assert_eq!(
        service.remove_hook(function_pointer(target_b)),
        MH_ERROR_NOT_CREATED
    );

    let second = match service.cleanup_owner(second_owner) {
        Ok(report) => report,
        Err(error) => panic!("unexpected cleanup error: {error}"),
    };
    assert_eq!(second.retired(), 1);
    assert_eq!(invoke(target_c, 1), 34);
    assert_eq!(service.hook_count(), 0);
    assert_eq!(service.uninitialize(), MH_OK);
}

#[test]
fn errors_recover_debug_is_redacted_and_drop_disables() {
    let _test = serialize_tests();
    let service = InlineHookService::new();
    assert_eq!(service.initialize(), MH_OK);
    let target = function_pointer(target_d);
    let detour = function_pointer(detour_d);
    let mut output = target;

    let null_target = unsafe {
        // SAFETY: Null is intentionally supplied to exercise validation; output is writable.
        service.create_hook(owner(9), ptr::null_mut(), detour, &mut output)
    };
    assert_eq!(null_target, MH_ERROR_NOT_EXECUTABLE);
    assert_eq!(output, target);
    let null_detour = unsafe {
        // SAFETY: The target is valid and the null detour is rejected before use.
        service.create_hook(owner(9), target, ptr::null_mut(), &mut output)
    };
    assert_eq!(null_detour, MH_ERROR_NOT_EXECUTABLE);
    assert_eq!(output, target);
    let inaccessible_target = unsafe {
        // SAFETY: VirtualQuery rejects the non-null inaccessible address before retour uses it.
        service.create_hook(owner(9), ptr::dangling_mut(), detour, &mut output)
    };
    assert_eq!(inaccessible_target, MH_ERROR_NOT_EXECUTABLE);
    assert_eq!(output, target);
    let same_address = unsafe {
        // SAFETY: The executable pointer is valid; retour rejects identical endpoints.
        service.create_hook(owner(9), target, target, &mut output)
    };
    assert_eq!(same_address, MH_ERROR_UNSUPPORTED_FUNCTION);
    assert_eq!(output, target);
    assert_eq!(service.hook_count(), 0);

    create_without_original(&service, owner(9), target_d, detour_d);
    let duplicate_with_inaccessible_detour = unsafe {
        // SAFETY: Executability is validated before the duplicate registry lookup.
        service.create_hook(owner(9), target, ptr::dangling_mut(), &mut output)
    };
    assert_eq!(duplicate_with_inaccessible_detour, MH_ERROR_NOT_EXECUTABLE);
    assert_eq!(output, target);
    let debug = format!("{service:?}");
    assert!(debug.contains("hook_count: 1"));
    assert!(!debug.contains("trampoline"));
    assert!(!debug.contains(&format!("{target:p}")));
    assert_eq!(service.enable_hook(target), MH_OK);
    assert_eq!(invoke(target_d, 2), 406);
    drop(service);
    assert_eq!(invoke(target_d, 2), 46);

    let second = InlineHookService::new();
    assert_eq!(second.initialize(), MH_OK);
    create_without_original(&second, owner(10), target_e, detour_e);
    create_without_original(&second, owner(10), target_f, detour_f);
    assert_eq!(second.enable_hook(ptr::null_mut()), MH_OK);
    assert_eq!(invoke(target_e, 2), 507);
    assert_eq!(invoke(target_f, 2), 608);
    assert_eq!(second.uninitialize(), MH_OK);
    assert_eq!(invoke(target_e, 2), 57);
    assert_eq!(invoke(target_f, 2), 68);
}

fn create_without_original(
    service: &InlineHookService,
    owner: OwnerToken,
    target: HookFn,
    detour: HookFn,
) {
    let status = unsafe {
        // SAFETY: Each pair has the same ABI/signature and process-lifetime code.
        service.create_hook(
            owner,
            function_pointer(target),
            function_pointer(detour),
            ptr::null_mut(),
        )
    };
    assert_eq!(status, MH_OK);
}

fn function_pointer(function: HookFn) -> *mut c_void {
    function as *const () as *mut c_void
}

unsafe fn pointer_function(pointer: *mut c_void) -> HookFn {
    unsafe {
        // SAFETY: The caller guarantees this is a `HookFn` trampoline.
        std::mem::transmute::<*mut c_void, HookFn>(pointer)
    }
}

fn invoke(function: HookFn, value: i32) -> i32 {
    black_box(function)(black_box(value))
}

fn owner(generation: u64) -> OwnerToken {
    OwnerToken {
        signature: 42,
        generation,
    }
}

fn serialize_tests() -> MutexGuard<'static, ()> {
    match TEST_SERIALIZER.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
