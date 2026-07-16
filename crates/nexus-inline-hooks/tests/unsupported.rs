//! Cross-platform contract tests for the deterministic unsupported service.

#![cfg(not(all(windows, target_arch = "x86_64")))]

use std::{ffi::c_void, ptr};

use nexus_core::OwnerToken;
use nexus_inline_hooks::{InlineHookService, MH_ERROR_UNSUPPORTED_FUNCTION};

#[test]
fn every_operation_is_deterministically_unsupported() {
    let service = InlineHookService::new();
    let owner = OwnerToken {
        signature: 1,
        generation: 2,
    };
    let target = ptr::dangling_mut::<c_void>();
    let mut original = target;

    assert_eq!(service.initialize(), MH_ERROR_UNSUPPORTED_FUNCTION);
    let create = unsafe {
        // SAFETY: The unsupported implementation never dereferences its pointer arguments.
        service.create_hook(owner, target, target, &mut original)
    };
    assert_eq!(create, MH_ERROR_UNSUPPORTED_FUNCTION);
    assert_eq!(original, target);
    assert_eq!(service.enable_hook(target), MH_ERROR_UNSUPPORTED_FUNCTION);
    assert_eq!(service.disable_hook(target), MH_ERROR_UNSUPPORTED_FUNCTION);
    assert_eq!(service.remove_hook(target), MH_ERROR_UNSUPPORTED_FUNCTION);
    assert_eq!(service.uninitialize(), MH_ERROR_UNSUPPORTED_FUNCTION);
    assert_eq!(service.hook_count(), 0);
    assert_eq!(service.owned_hook_count(owner), 0);

    let cleanup = match service.cleanup_owner(owner) {
        Ok(report) => panic!("unexpected cleanup report: {report:?}"),
        Err(error) => error,
    };
    assert_eq!(cleanup.status(), MH_ERROR_UNSUPPORTED_FUNCTION);
    assert_eq!(cleanup.retired(), 0);
    assert_eq!(cleanup.remaining(), 0);
}
