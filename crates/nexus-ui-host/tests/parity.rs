//! Behavioral parity tests derived from the legacy C++ UI services.

use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use nexus_ui_host::{
    AlertKind, AlertQueueConfig, ContextRegistrationOutcome, ESCAPE_VIRTUAL_KEY,
    EscapeCloseOutcome, EscapeClosingConfig, EscapeKeyEvent, EscapeRegistrationOutcome,
    FrameRenderState, NativeRenderCallback, NativeVisibilityPointer, NotificationBadge,
    NotificationOutcome, OwnerGeneration, QA_GENERIC_KEY, QuickAccessConfig, QuickAccessPosition,
    QuickAccessSettings, QuickAccessVisibility, RegisterRenderOutcome, RenderPhase,
    RenderRegistryConfig, ShortcutRegistrationOutcome, UiCallback, UiHost, UiHostConfig,
    UiRegistryError, VisibilityTarget,
};

static NATIVE_CALLBACK_CALLS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn count_native_callback() {
    NATIVE_CALLBACK_CALLS.fetch_add(1, Ordering::AcqRel);
}

fn must<T, E: Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("unexpected error: {error:?}"),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn event(was_down: bool) -> EscapeKeyEvent {
    EscapeKeyEvent {
        is_key_down: true,
        virtual_key: ESCAPE_VIRTUAL_KEY,
        was_down,
    }
}

#[test]
fn render_phases_preserve_order_duplicates_removal_and_visibility() {
    let host = UiHost::default();
    let owner = must(host.owner(OwnerGeneration::new(7, 1)));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let first_calls = Arc::clone(&calls);
    let first = UiCallback::managed(owner.clone(), move || lock(&first_calls).push("first"));
    let second_calls = Arc::clone(&calls);
    let second = UiCallback::managed(owner, move || lock(&second_calls).push("second"));

    assert_eq!(
        must(
            host.render()
                .register(RenderPhase::PreRender, first.clone())
        ),
        RegisterRenderOutcome::Registered
    );
    assert_eq!(
        must(
            host.render()
                .register(RenderPhase::PreRender, first.clone())
        ),
        RegisterRenderOutcome::Duplicate
    );
    assert_eq!(
        must(
            host.render()
                .register(RenderPhase::PreRender, second.clone())
        ),
        RegisterRenderOutcome::Registered
    );
    assert_eq!(
        must(host.render().register(RenderPhase::Render, first.clone())),
        RegisterRenderOutcome::Registered
    );
    assert_eq!(
        must(
            host.render()
                .register(RenderPhase::PostRender, second.clone())
        ),
        RegisterRenderOutcome::Registered
    );
    assert_eq!(
        must(
            host.render()
                .register(RenderPhase::OptionsRender, first.clone())
        ),
        RegisterRenderOutcome::Registered
    );

    let hidden = host.render().snapshot_frame(FrameRenderState {
        initialized: true,
        eula_accepted: true,
        ui_visible: false,
    });
    assert!(hidden.render.is_none());
    assert_eq!(hidden.pre_render.invoke_all().invoked, 2);
    assert_eq!(hidden.post_render.invoke_all().invoked, 1);
    assert_eq!(&*lock(&calls), &["first", "second", "second"]);

    lock(&calls).clear();
    let visible = host.render().snapshot_frame(FrameRenderState {
        initialized: true,
        eula_accepted: true,
        ui_visible: true,
    });
    let main = match visible.render {
        Some(snapshot) => snapshot,
        None => panic!("main phase should be present"),
    };
    assert_eq!(main.invoke_all().invoked, 1);
    assert_eq!(&*lock(&calls), &["first"]);
    assert_eq!(
        host.render()
            .snapshot(RenderPhase::OptionsRender)
            .invoke_all()
            .invoked,
        1
    );

    let stale = host.render().snapshot(RenderPhase::PreRender);
    assert_eq!(host.render().deregister(&first), 3);
    assert_eq!(stale.invoke_all().skipped_inactive, 1);
    assert_eq!(stale.invoke_all().invoked, 1);
    assert_eq!(host.render().snapshot(RenderPhase::Render).len(), 0);
    assert_eq!(host.render().snapshot(RenderPhase::OptionsRender).len(), 0);

    for value in 0..=3 {
        let phase = must(RenderPhase::try_from(nexus_abi::RenderPhase(value)));
        assert_eq!(nexus_abi::RenderPhase::from(phase).0, value);
    }
    assert!(RenderPhase::try_from(nexus_abi::RenderPhase(4)).is_err());
}

#[test]
fn native_render_callback_obeys_duplicate_and_owner_cleanup_gates() {
    NATIVE_CALLBACK_CALLS.store(0, Ordering::Release);
    let host = UiHost::default();
    let identity = OwnerGeneration::new(8, 1);
    let owner = must(host.owner(identity));
    // SAFETY: this test callback is process-static, takes no arguments, and
    // cannot unwind; it therefore outlives the owner cleanup below.
    let native = unsafe { NativeRenderCallback::new(owner, count_native_callback) };
    let callback = UiCallback::native(native);
    assert_eq!(
        must(
            host.render()
                .register(RenderPhase::Render, callback.clone())
        ),
        RegisterRenderOutcome::Registered
    );
    assert_eq!(
        must(
            host.render()
                .register(RenderPhase::Render, callback.clone())
        ),
        RegisterRenderOutcome::Duplicate
    );
    let snapshot = host.render().snapshot(RenderPhase::Render);
    assert_eq!(snapshot.invoke_all().invoked, 1);
    assert_eq!(NATIVE_CALLBACK_CALLS.load(Ordering::Acquire), 1);
    let cleanup = host.cleanup_owner_generation(identity);
    assert!(cleanup.retirement.quiescent);
    assert_eq!(snapshot.invoke_all().skipped_inactive, 1);
    assert_eq!(NATIVE_CALLBACK_CALLS.load(Ordering::Acquire), 1);
}

#[test]
fn alerts_match_front_only_deduplication_and_timing() {
    let config = UiHostConfig {
        alerts: AlertQueueConfig {
            maximum_alerts: 2,
            maximum_message_bytes: 8,
            hold_millis: 5_000,
            fade_millis: 2_500,
        },
        ..UiHostConfig::default()
    };
    let host = must(UiHost::new(config));
    let first_owner = must(host.owner(OwnerGeneration::new(1, 1)));
    let second_owner = must(host.owner(OwnerGeneration::new(2, 1)));

    assert_eq!(
        must(
            host.alerts()
                .notify(&first_owner, AlertKind::None, "way too long")
        ),
        nexus_ui_host::NotifyOutcome::IgnoredNone
    );
    assert_eq!(
        must(host.alerts().notify(&first_owner, AlertKind::Info, "same")),
        nexus_ui_host::NotifyOutcome::Queued
    );
    assert_eq!(
        must(
            host.alerts()
                .notify(&second_owner, AlertKind::Error, "same")
        ),
        nexus_ui_host::NotifyOutcome::ResetFront
    );
    assert_eq!(host.alerts().len(), 1);

    let first = host.alerts().advance(100);
    let first = match first.alert {
        Some(alert) => alert,
        None => panic!("front alert missing"),
    };
    assert_eq!(first.kind, AlertKind::Info);
    assert_eq!(first.owner, first_owner.identity());
    assert_eq!(first.opacity, 1.0);
    assert_eq!(first.reset_revision, 1);

    assert_eq!(
        host.alerts()
            .advance(5_101)
            .alert
            .map(|alert| alert.opacity),
        Some(1.0)
    );
    let expired = host.alerts().advance(7_601);
    assert!(expired.expired);
    assert!(expired.alert.is_none());
}

#[test]
fn escape_closes_topmost_once_and_preserves_first_duplicate_target() {
    let host = UiHost::default();
    let owner = must(host.owner(OwnerGeneration::new(3, 1)));
    let a = Arc::new(AtomicBool::new(true));
    let b = Arc::new(AtomicBool::new(true));
    let duplicate = Arc::new(AtomicBool::new(true));
    assert_eq!(
        must(host.escape_closing().register(
            &owner,
            "A",
            VisibilityTarget::managed(Arc::clone(&a))
        )),
        EscapeRegistrationOutcome::Registered
    );
    assert_eq!(
        must(host.escape_closing().register(
            &owner,
            "B",
            VisibilityTarget::managed(Arc::clone(&b))
        )),
        EscapeRegistrationOutcome::Registered
    );
    assert_eq!(
        must(host.escape_closing().register(
            &owner,
            "A",
            VisibilityTarget::managed(Arc::clone(&duplicate))
        )),
        EscapeRegistrationOutcome::Duplicate
    );

    assert_eq!(
        host.escape_closing()
            .handle(event(false), &["Fallback", "A", "B"]),
        EscapeCloseOutcome::Consumed {
            window: Arc::from("B")
        }
    );
    assert!(!b.load(Ordering::Acquire));
    assert!(a.load(Ordering::Acquire));
    assert!(duplicate.load(Ordering::Acquire));
    assert_eq!(
        host.escape_closing()
            .handle(event(true), &["Fallback", "A"]),
        EscapeCloseOutcome::Passed
    );
    assert!(a.load(Ordering::Acquire));
    assert_eq!(
        host.escape_closing().handle(event(false), &["A"]),
        EscapeCloseOutcome::Passed
    );
    assert!(a.load(Ordering::Acquire));

    host.escape_closing().set_enabled(false);
    assert_eq!(
        host.escape_closing()
            .handle(event(false), &["Fallback", "A"]),
        EscapeCloseOutcome::Passed
    );
    host.escape_closing().set_enabled(true);
    assert!(
        host.escape_closing()
            .deregister_target(&VisibilityTarget::managed(Arc::clone(&a)))
    );
    assert_eq!(
        host.escape_closing().registered_windows().as_ref(),
        &[Arc::<str>::from("B")]
    );
}

#[test]
fn native_escape_pointer_is_only_touched_while_owner_is_active() {
    let host = UiHost::default();
    let identity = OwnerGeneration::new(4, 1);
    let owner = must(host.owner(identity));
    let mut visible = 1_u8;
    // SAFETY: this stack byte remains valid and exclusively accessed through
    // the registry until owner cleanup reaches quiescence below.
    let pointer =
        match unsafe { NativeVisibilityPointer::from_ptr(owner.clone(), &raw mut visible) } {
            Some(pointer) => pointer,
            None => panic!("stack pointer cannot be null"),
        };
    let target = VisibilityTarget::native(pointer);
    let other_host = UiHost::default();
    let other_owner = must(other_host.owner(identity));
    assert!(matches!(
        other_host
            .escape_closing()
            .register(&other_owner, "Wrong gate", target.clone()),
        Err(UiRegistryError::NativeOwnerMismatch { .. })
    ));
    assert_eq!(
        must(host.escape_closing().register(&owner, "Native", target)),
        EscapeRegistrationOutcome::Registered
    );
    assert!(matches!(
        host.escape_closing()
            .handle(event(false), &["Fallback", "Native"]),
        EscapeCloseOutcome::Consumed { .. }
    ));
    assert_eq!(visible, 0);
    let cleanup = host.cleanup_owner_generation(identity);
    assert!(cleanup.retirement.quiescent);
    assert_eq!(cleanup.escape_windows, 1);
}

#[test]
fn quick_access_matches_map_order_orphans_notifications_and_visibility() {
    let host = UiHost::default();
    let owner = must(host.owner(OwnerGeneration::new(11, 1)));
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let callback = UiCallback::managed(owner.clone(), move || {
        callback_calls.fetch_add(1, Ordering::AcqRel);
    });

    assert_eq!(
        must(
            host.quick_access()
                .add_context_item("z-item", "target", callback.clone())
        ),
        ContextRegistrationOutcome::Orphaned
    );
    let target = must(
        host.quick_access()
            .add_shortcut(&owner, "target", "first", "hover", "bind", "tip"),
    );
    assert_eq!(target.outcome, ShortcutRegistrationOutcome::Registered);
    assert_eq!(target.adopted_orphans, 1);
    let duplicate = must(host.quick_access().add_shortcut(
        &owner,
        "target",
        "replacement",
        "new",
        "new",
        "new",
    ));
    assert_eq!(duplicate.outcome, ShortcutRegistrationOutcome::Duplicate);
    assert!(duplicate.revision > target.revision);
    must(
        host.quick_access()
            .add_shortcut(&owner, "alpha", "a", "ah", "", "alpha"),
    );
    assert_eq!(
        must(
            host.quick_access()
                .add_context_item("a-item", "target", callback.clone())
        ),
        ContextRegistrationOutcome::Attached
    );

    assert_eq!(
        must(
            host.quick_access()
                .push_notification(&owner, "target", "second")
        ),
        NotificationOutcome::Added
    );
    assert_eq!(
        must(
            host.quick_access()
                .push_notification(&owner, "target", "first")
        ),
        NotificationOutcome::Added
    );
    assert_eq!(
        must(
            host.quick_access()
                .push_notification(&owner, "target", "second")
        ),
        NotificationOutcome::Duplicate
    );
    assert_eq!(
        must(
            host.quick_access()
                .set_generic_notification(&owner, "target", true)
        ),
        NotificationOutcome::Added
    );

    let snapshot = host.quick_access().snapshot(true, false);
    let ids = snapshot
        .shortcuts
        .iter()
        .map(|shortcut| shortcut.id.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["alpha", "target"]);
    let target = &snapshot.shortcuts[1];
    assert_eq!(target.texture.as_ref(), "first");
    assert_eq!(
        target
            .context_items
            .iter()
            .map(|item| item.id.as_ref())
            .collect::<Vec<_>>(),
        ["a-item", "z-item"]
    );
    assert_eq!(
        target
            .notifications
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<&str>>(),
        ["second", "first", QA_GENERIC_KEY]
    );
    assert_eq!(
        target.notification_badge(),
        Some(NotificationBadge::Count(3))
    );
    assert_eq!(
        target.context_items[0].invoke(),
        nexus_ui_host::CallbackInvocation::Invoked
    );
    assert_eq!(calls.load(Ordering::Acquire), 1);

    must(host.quick_access().set_suppressed("target", true));
    assert!(
        host.quick_access()
            .snapshot(true, false)
            .renderable_shortcuts(|_| true)
            .iter()
            .all(|shortcut| shortcut.id.as_ref() != "target")
    );

    host.quick_access().set_settings(QuickAccessSettings {
        visibility: QuickAccessVisibility::InCombat,
        position: QuickAccessPosition::Custom,
        ..QuickAccessSettings::default()
    });
    assert!(!host.quick_access().snapshot(true, false).globally_visible);
    assert!(host.quick_access().snapshot(true, true).globally_visible);

    assert!(must(host.quick_access().remove_shortcut("target")));
    let added_again = must(
        host.quick_access()
            .add_shortcut(&owner, "target", "again", "hover", "bind", "tip"),
    );
    assert_eq!(added_again.adopted_orphans, 2);
    assert_eq!(
        must(
            host.quick_access()
                .pop_notification("target", QA_GENERIC_KEY)
        ),
        NotificationOutcome::NotificationMissing
    );
}

#[test]
fn quick_access_cleanup_is_generation_exact_and_preserves_foreign_children() {
    let host = UiHost::default();
    let first_id = OwnerGeneration::new(20, 1);
    let child_id = OwnerGeneration::new(21, 1);
    let first = must(host.owner(first_id));
    let child = must(host.owner(child_id));
    must(
        host.quick_access()
            .add_shortcut(&first, "parent", "p", "ph", "bind", "parent"),
    );
    let child_callback = UiCallback::managed(child, || {});
    assert_eq!(
        must(
            host.quick_access()
                .add_context_item("child-item", "parent", child_callback)
        ),
        ContextRegistrationOutcome::Attached
    );

    let cleanup = host.cleanup_owner_generation(first_id);
    assert_eq!(cleanup.quick_access.shortcuts, 1);
    assert_eq!(cleanup.quick_access.context_items, 0);
    assert!(
        host.quick_access()
            .snapshot(true, false)
            .shortcuts
            .is_empty()
    );
    assert!(host.owner(first_id).is_err());

    let reloaded = must(host.owner(OwnerGeneration::new(20, 2)));
    let mutation = must(
        host.quick_access()
            .add_shortcut(&reloaded, "parent", "p2", "ph2", "bind2", "parent2"),
    );
    assert_eq!(mutation.adopted_orphans, 1);
    let snapshot = host.quick_access().snapshot(true, false);
    assert_eq!(snapshot.shortcuts[0].context_items[0].owner, child_id);
}

#[test]
fn visibility_truth_table_is_exact() {
    let rows = [
        (QuickAccessVisibility::AlwaysShow, false, false, true),
        (QuickAccessVisibility::Gameplay, false, false, false),
        (QuickAccessVisibility::Gameplay, true, false, true),
        (QuickAccessVisibility::OutOfCombat, true, false, true),
        (QuickAccessVisibility::OutOfCombat, true, true, false),
        (QuickAccessVisibility::InCombat, true, false, false),
        (QuickAccessVisibility::InCombat, true, true, true),
        (QuickAccessVisibility::Hide, true, true, false),
    ];
    for (visibility, gameplay, combat, expected) in rows {
        assert_eq!(visibility.is_visible(gameplay, combat), expected);
    }
}

#[test]
fn every_externally_growing_surface_enforces_configured_bounds() {
    let host = must(UiHost::new(UiHostConfig {
        render: RenderRegistryConfig {
            maximum_callbacks_per_phase: 1,
            maximum_panics_per_callback: 1,
        },
        escape_closing: EscapeClosingConfig {
            maximum_windows: 1,
            maximum_window_name_bytes: 3,
        },
        alerts: AlertQueueConfig {
            maximum_alerts: 1,
            maximum_message_bytes: 3,
            hold_millis: 1,
            fade_millis: 1,
        },
        quick_access: QuickAccessConfig {
            maximum_shortcuts: 1,
            maximum_context_items: 1,
            maximum_notifications_per_shortcut: 1,
            maximum_suppressed_identifiers: 1,
            maximum_string_bytes: 3,
            maximum_panics_per_callback: 1,
        },
    }));
    let owner = must(host.owner(OwnerGeneration::new(77, 1)));

    must(host.render().register(
        RenderPhase::Render,
        UiCallback::managed(owner.clone(), || {}),
    ));
    assert!(matches!(
        host.render().register(
            RenderPhase::Render,
            UiCallback::managed(owner.clone(), || {})
        ),
        Err(UiRegistryError::CapacityExceeded { .. })
    ));

    assert!(matches!(
        host.escape_closing().register(
            &owner,
            "long",
            VisibilityTarget::managed(Arc::new(AtomicBool::new(true)))
        ),
        Err(UiRegistryError::TextTooLong { .. })
    ));
    assert!(matches!(
        host.escape_closing().register(
            &owner,
            "a\0",
            VisibilityTarget::managed(Arc::new(AtomicBool::new(true)))
        ),
        Err(UiRegistryError::InteriorNul { .. })
    ));
    must(host.escape_closing().register(
        &owner,
        "a",
        VisibilityTarget::managed(Arc::new(AtomicBool::new(true))),
    ));
    assert!(matches!(
        host.escape_closing().register(
            &owner,
            "b",
            VisibilityTarget::managed(Arc::new(AtomicBool::new(true)))
        ),
        Err(UiRegistryError::CapacityExceeded { .. })
    ));

    assert!(matches!(
        host.alerts().notify(&owner, AlertKind::Info, "long"),
        Err(UiRegistryError::TextTooLong { .. })
    ));
    must(host.alerts().notify(&owner, AlertKind::Info, "a"));
    assert!(matches!(
        host.alerts().notify(&owner, AlertKind::Info, "b"),
        Err(UiRegistryError::CapacityExceeded { .. })
    ));

    must(
        host.quick_access()
            .add_shortcut(&owner, "a", "t", "h", "i", "p"),
    );
    assert!(matches!(
        host.quick_access()
            .add_shortcut(&owner, "b", "t", "h", "i", "p"),
        Err(UiRegistryError::CapacityExceeded { .. })
    ));
    must(
        host.quick_access()
            .add_context_item("a", "a", UiCallback::managed(owner.clone(), || {})),
    );
    assert!(matches!(
        host.quick_access()
            .add_context_item("b", "a", UiCallback::managed(owner.clone(), || {})),
        Err(UiRegistryError::CapacityExceeded { .. })
    ));
    must(host.quick_access().push_notification(&owner, "a", "a"));
    assert!(matches!(
        host.quick_access().push_notification(&owner, "a", "b"),
        Err(UiRegistryError::CapacityExceeded { .. })
    ));
    must(host.quick_access().set_suppressed("a", true));
    assert!(matches!(
        host.quick_access().set_suppressed("b", true),
        Err(UiRegistryError::CapacityExceeded { .. })
    ));
}

#[test]
fn panic_budget_is_bounded_per_registration() {
    let host = must(UiHost::new(UiHostConfig {
        render: RenderRegistryConfig {
            maximum_callbacks_per_phase: 4,
            maximum_panics_per_callback: 2,
        },
        ..UiHostConfig::default()
    }));
    let owner = must(host.owner(OwnerGeneration::new(99, 1)));
    let callback = UiCallback::managed(owner, || panic!("contained"));
    assert_eq!(
        must(host.render().register(RenderPhase::Render, callback)),
        RegisterRenderOutcome::Registered
    );
    let snapshot = host.render().snapshot(RenderPhase::Render);
    assert_eq!(snapshot.invoke_all().panicked, 1);
    assert_eq!(snapshot.invoke_all().panicked, 1);
    assert_eq!(snapshot.invoke_all().skipped_disabled, 1);
}
