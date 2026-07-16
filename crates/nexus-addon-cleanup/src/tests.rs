use crate::{
    AdapterError, AdapterFailureKind, CleanupDomain, CleanupEffect, CleanupService, EventCallbacks,
    FontCallbacks, FontResources, INTEGRATION_GAPS, InlineHooks, IntegrationGapKind,
    LocalizationOverrides, ManagedInputCallbacks, PhaseStatus, RawWndProcCallbacks,
    RegistrationCleaner, RegistrationCleanerBuilder, StepFailure, StepStatus, TextureCallbacks,
    TypedAdapter, UiHostCallbacks,
};
use nexus_core::OwnerToken;
use nexus_host::CleanupPhase;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::panic::panic_any;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

type CallLog = Rc<RefCell<Vec<(CleanupService, OwnerToken)>>>;

fn owner(signature: u32, generation: u64) -> OwnerToken {
    OwnerToken {
        signature,
        generation,
    }
}

fn recording<D: CleanupDomain>(log: &CallLog) -> TypedAdapter<D> {
    let log = Rc::clone(log);
    TypedAdapter::new(move |owner| {
        log.borrow_mut().push((D::SERVICE, owner));
        Ok(CleanupEffect::complete(1))
    })
}

fn configured_builder(log: &CallLog) -> RegistrationCleanerBuilder {
    RegistrationCleaner::builder()
        .inline_hooks(recording::<InlineHooks>(log))
        .ui_host_callbacks(recording::<UiHostCallbacks>(log))
        .raw_wndproc_callbacks(recording::<RawWndProcCallbacks>(log))
        .managed_input_callbacks(recording::<ManagedInputCallbacks>(log))
        .event_callbacks(recording::<EventCallbacks>(log))
        .texture_callbacks(recording::<TextureCallbacks>(log))
        .font_callbacks(recording::<FontCallbacks>(log))
        .font_resources(recording::<FontResources>(log))
        .localization_overrides(recording::<LocalizationOverrides>(log))
}

#[test]
fn exact_host_phase_and_service_order_is_enforced() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut cleaner = configured_builder(&log).build();
    let owner = owner(41, 7);

    let blocked = cleaner.cleanup_phase(owner, CleanupPhase::CallbackRegistrations);
    assert!(matches!(
        blocked
            .as_ref()
            .map_err(|failure| failure.report().status()),
        Err(PhaseStatus::Blocked {
            required: CleanupPhase::HookRegistrations
        })
    ));
    assert!(log.borrow().is_empty());

    assert!(
        cleaner
            .cleanup_phase(owner, CleanupPhase::HookRegistrations)
            .is_ok()
    );
    assert!(
        cleaner
            .cleanup_phase(owner, CleanupPhase::CallbackRegistrations)
            .is_ok()
    );
    assert!(
        cleaner
            .cleanup_phase(owner, CleanupPhase::OwnedResources)
            .is_ok()
    );

    let actual = log
        .borrow()
        .iter()
        .map(|(service, _owner)| *service)
        .collect::<Vec<_>>();
    assert_eq!(actual, CleanupService::ORDER);
}

#[test]
fn stale_generation_cleanup_never_reaches_reloaded_owner() {
    let old = owner(73, 10);
    let reloaded = owner(73, 11);
    let registrations = Rc::new(RefCell::new(HashSet::from([old, reloaded])));
    let observed = Rc::clone(&registrations);
    let adapter = TypedAdapter::<InlineHooks>::new(move |target| {
        let removed = usize::from(observed.borrow_mut().remove(&target));
        Ok(CleanupEffect::complete(removed))
    });
    let mut cleaner = RegistrationCleaner::builder().inline_hooks(adapter).build();

    let report = cleaner
        .cleanup_phase(old, CleanupPhase::HookRegistrations)
        .expect("old generation hook cleanup should complete");
    assert_eq!(report.owner(), old);
    assert!(!registrations.borrow().contains(&old));
    assert!(registrations.borrow().contains(&reloaded));

    let replay = cleaner
        .cleanup_phase(old, CleanupPhase::HookRegistrations)
        .expect("completed phase replay should be idempotent");
    assert_eq!(replay, report);
    assert!(registrations.borrow().contains(&reloaded));
}

#[test]
fn partial_phase_retry_calls_only_failed_slot_and_preserves_counts() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let raw_attempts = Rc::new(Cell::new(0_u32));
    let raw_attempts_for_adapter = Rc::clone(&raw_attempts);
    let raw_log = Rc::clone(&log);
    let raw = TypedAdapter::<RawWndProcCallbacks>::new(move |owner| {
        raw_log
            .borrow_mut()
            .push((CleanupService::RawWndProcCallbacks, owner));
        let attempt = raw_attempts_for_adapter.get().saturating_add(1);
        raw_attempts_for_adapter.set(attempt);
        if attempt == 1 {
            Err(AdapterError::new(AdapterFailureKind::Busy)
                .with_removed(2)
                .with_remaining(1))
        } else {
            Ok(CleanupEffect::complete(1))
        }
    });

    let mut cleaner = RegistrationCleaner::builder()
        .inline_hooks(recording::<InlineHooks>(&log))
        .ui_host_callbacks(recording::<UiHostCallbacks>(&log))
        .raw_wndproc_callbacks(raw)
        .managed_input_callbacks(recording::<ManagedInputCallbacks>(&log))
        .event_callbacks(recording::<EventCallbacks>(&log))
        .texture_callbacks(recording::<TextureCallbacks>(&log))
        .font_callbacks(recording::<FontCallbacks>(&log))
        .font_resources(recording::<FontResources>(&log))
        .localization_overrides(recording::<LocalizationOverrides>(&log))
        .build();
    let owner = owner(99, 3);

    assert!(
        cleaner
            .cleanup_phase(owner, CleanupPhase::HookRegistrations)
            .is_ok()
    );
    let first = cleaner.cleanup_phase(owner, CleanupPhase::CallbackRegistrations);
    assert!(first.is_err());
    let first_callback_count = log
        .borrow()
        .iter()
        .filter(|(service, _owner)| service.phase() == CleanupPhase::CallbackRegistrations)
        .count();
    assert_eq!(first_callback_count, 6);

    let second = cleaner
        .cleanup_phase(owner, CleanupPhase::CallbackRegistrations)
        .expect("busy callback slot should succeed on retry");
    let second_callback_count = log
        .borrow()
        .iter()
        .filter(|(service, _owner)| service.phase() == CleanupPhase::CallbackRegistrations)
        .count();
    assert_eq!(second_callback_count, 7);
    assert_eq!(raw_attempts.get(), 2);

    let raw_status = second
        .steps()
        .iter()
        .find(|step| step.service() == CleanupService::RawWndProcCallbacks)
        .map(|step| step.status());
    assert_eq!(
        raw_status,
        Some(StepStatus::Complete {
            removed: 3,
            attempts: 2,
        })
    );
}

struct PanicOnDrop {
    drops: Arc<AtomicUsize>,
}

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        panic!("caught panic payload destructor must be forgotten");
    }
}

#[test]
fn reentrant_callback_and_adversarial_panic_are_contained_and_retryable() {
    let callback_depth = Rc::new(Cell::new(0_usize));
    let callback_depth_for_adapter = Rc::clone(&callback_depth);
    let first_attempt = Rc::new(Cell::new(true));
    let first_attempt_for_adapter = Rc::clone(&first_attempt);
    let payload_drops = Arc::new(AtomicUsize::new(0));
    let payload_drops_for_adapter = Arc::clone(&payload_drops);

    let adapter = TypedAdapter::<InlineHooks>::new(move |_owner| {
        // The adapter deliberately invokes a nested user callback. The cleaner
        // owns no progress mutex, so adapter-side reentrancy cannot deadlock on
        // cleaner bookkeeping.
        let nested = || {
            callback_depth_for_adapter.set(callback_depth_for_adapter.get() + 1);
        };
        nested();
        if first_attempt_for_adapter.replace(false) {
            panic_any(PanicOnDrop {
                drops: Arc::clone(&payload_drops_for_adapter),
            });
        }
        Ok(CleanupEffect::complete(1))
    });
    let mut cleaner = RegistrationCleaner::builder().inline_hooks(adapter).build();
    let owner = owner(5, 12);

    let first = cleaner.cleanup_phase(owner, CleanupPhase::HookRegistrations);
    let first_status = first
        .as_ref()
        .err()
        .and_then(|failure| failure.report().steps().first())
        .map(|step| step.status());
    assert!(matches!(
        first_status,
        Some(StepStatus::Failed {
            failure: StepFailure::Panicked,
            attempts: 1,
            ..
        })
    ));
    assert_eq!(payload_drops.load(Ordering::SeqCst), 0);

    assert!(
        cleaner
            .cleanup_phase(owner, CleanupPhase::HookRegistrations)
            .is_ok()
    );
    assert_eq!(callback_depth.get(), 2);
    assert_eq!(payload_drops.load(Ordering::SeqCst), 0);
}

#[test]
fn missing_adapter_is_a_gap_and_diagnostics_redact_owner() {
    let mut cleaner = RegistrationCleaner::builder().build();
    let owner = owner(u32::MAX - 17, u64::MAX - 29);
    let failure = cleaner
        .cleanup_phase(owner, CleanupPhase::HookRegistrations)
        .expect_err("an unconfigured required cleanup must fail closed");
    assert!(matches!(
        failure.report().steps().first().map(|step| step.status()),
        Some(StepStatus::Gap { .. })
    ));

    let diagnostic = format!("{failure:?} {failure} {cleaner:?}");
    assert!(!diagnostic.contains(&owner.signature.to_string()));
    assert!(!diagnostic.contains(&owner.generation.to_string()));
}

#[test]
fn explicit_gap_can_be_replaced_between_retries() {
    let mut cleaner = RegistrationCleaner::builder().build();
    let owner = owner(8, 2);
    assert!(
        cleaner
            .cleanup_phase(owner, CleanupPhase::HookRegistrations)
            .is_err()
    );
    cleaner.install(TypedAdapter::<InlineHooks>::new(|_owner| {
        Ok(CleanupEffect::complete(4))
    }));
    let report = cleaner
        .cleanup_phase(owner, CleanupPhase::HookRegistrations)
        .expect("replacement adapter should resolve the reported gap");
    assert_eq!(
        report.steps().first().map(|step| step.status()),
        Some(StepStatus::Complete {
            removed: 4,
            attempts: 1,
        })
    );
}

#[test]
fn external_manager_lock_is_never_hidden_by_binding_coverage() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let cleaner = configured_builder(&log).build();
    assert!(cleaner.coverage().is_complete());
    assert!(
        INTEGRATION_GAPS
            .iter()
            .any(|gap| { gap.kind == IntegrationGapKind::ManagerOuterLockDuringCleanup })
    );
}
