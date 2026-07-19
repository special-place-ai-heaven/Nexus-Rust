//! Behavioral parity tests derived from the legacy C++ UI services.

use core::num::NonZeroUsize;
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Barrier, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use nexus_ui_host::{
    AlertKind, AlertQueueConfig, CheckedVisibilityAccess, ContextRegistrationOutcome,
    ESCAPE_VIRTUAL_KEY, EscapeCloseOutcome, EscapeClosingConfig, EscapeKeyEvent,
    EscapeRegistrationOutcome, FrameRenderState, NativeRenderCallback, NativeVisibilityPointer,
    NotificationBadge, NotificationOutcome, OwnerGeneration, QA_GENERIC_KEY, QuickAccessConfig,
    QuickAccessPosition, QuickAccessSettings, QuickAccessVisibility, RegisterRenderOutcome,
    RenderPhase, RenderRegistryConfig, ShortcutRegistrationOutcome, UiCallback, UiHost,
    UiHostConfig, UiRegistryError, VisibilityTarget,
};

static NATIVE_CALLBACK_CALLS: AtomicUsize = AtomicUsize::new(0);

struct TestVisibilityAccess {
    visible: AtomicU8,
    reads: AtomicUsize,
    writes: AtomicUsize,
}

impl TestVisibilityAccess {
    fn new(visible: bool) -> Self {
        Self {
            visible: AtomicU8::new(u8::from(visible)),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
        }
    }

    fn address(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.visible.as_ptr() as usize).expect("atomic test cell is non-null")
    }
}

// SAFETY: the adapter accepts only its own live `AtomicU8` allocation, uses
// atomic synchronization for every access, and never re-enters a registry or
// owner-cleanup path from either callback.
unsafe impl CheckedVisibilityAccess for TestVisibilityAccess {
    fn read_visible(&self, address: NonZeroUsize) -> Option<bool> {
        if address != self.address() {
            return None;
        }
        self.reads.fetch_add(1, Ordering::AcqRel);
        Some(self.visible.load(Ordering::Acquire) != 0)
    }

    fn write_hidden(&self, address: NonZeroUsize) -> bool {
        if address != self.address() {
            return false;
        }
        self.writes.fetch_add(1, Ordering::AcqRel);
        self.visible.store(0, Ordering::Release);
        true
    }
}

struct BlockingVisibilityAccess {
    visible: AtomicU8,
    entered: Sender<()>,
    release: (Mutex<bool>, Condvar),
}

impl BlockingVisibilityAccess {
    fn new() -> (Arc<Self>, Receiver<()>) {
        let (entered, observed) = mpsc::channel();
        (
            Arc::new(Self {
                visible: AtomicU8::new(1),
                entered,
                release: (Mutex::new(false), Condvar::new()),
            }),
            observed,
        )
    }

    fn address(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.visible.as_ptr() as usize).expect("atomic test cell is non-null")
    }

    fn release(&self) {
        *lock(&self.release.0) = true;
        self.release.1.notify_all();
    }
}

// SAFETY: the adapter accepts only its own live `AtomicU8`, synchronizes all
// accesses, and never re-enters the registry or owner-cleanup paths. The test
// release gate only holds the adapter call open so concurrent entry is visible.
unsafe impl CheckedVisibilityAccess for BlockingVisibilityAccess {
    fn read_visible(&self, address: NonZeroUsize) -> Option<bool> {
        if address != self.address() || self.entered.send(()).is_err() {
            return None;
        }
        let mut released = lock(&self.release.0);
        while !*released {
            released = match self.release.1.wait(released) {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
        Some(self.visible.load(Ordering::Acquire) != 0)
    }

    fn write_hidden(&self, address: NonZeroUsize) -> bool {
        if address != self.address() {
            return false;
        }
        self.visible.store(0, Ordering::Release);
        true
    }
}

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
    let foreign_owner = must(host.owner(OwnerGeneration::new(30, 1)));
    assert!(
        !host.escape_closing().deregister_target_for_owner(
            &foreign_owner,
            &VisibilityTarget::managed(Arc::clone(&a)),
        )
    );
    assert!(
        host.escape_closing()
            .deregister_target_for_owner(&owner, &VisibilityTarget::managed(Arc::clone(&a)))
    );
    assert_eq!(
        host.escape_closing().registered_windows().as_ref(),
        &[Arc::<str>::from("B")]
    );
    let trusted_target = Arc::new(AtomicBool::new(true));
    assert_eq!(
        must(host.escape_closing().register(
            &owner,
            "Trusted target",
            VisibilityTarget::managed(Arc::clone(&trusted_target)),
        )),
        EscapeRegistrationOutcome::Registered
    );
    assert!(
        host.escape_closing()
            .deregister_target(&VisibilityTarget::managed(Arc::clone(&trusted_target)),)
    );
    assert!(host.escape_closing().deregister_window("B"));
    assert!(host.escape_closing().registered_windows().is_empty());
}

#[test]
fn native_escape_pointer_is_only_touched_while_owner_is_active() {
    let host = UiHost::default();
    let identity = OwnerGeneration::new(4, 1);
    let owner = must(host.owner(identity));
    let access = Arc::new(TestVisibilityAccess::new(true));
    let address = access.address();
    let other_host = UiHost::default();
    let other_owner = must(other_host.owner(identity));
    let mismatched_pointer = unsafe {
        // SAFETY: the adapter-owned atomic is live for this value, all accesses
        // are atomic, and the adapter never re-enters registry cleanup.
        NativeVisibilityPointer::checked(owner.clone(), address, access.clone())
    };
    assert!(matches!(
        other_host.escape_closing().register(
            &other_owner,
            "Wrong gate",
            VisibilityTarget::native(mismatched_pointer),
        ),
        Err(UiRegistryError::NativeOwnerMismatch { .. })
    ));
    let pointer = unsafe {
        // SAFETY: the adapter-owned atomic remains live through owner cleanup,
        // all accesses are atomic, and the adapter cannot re-enter the registry.
        NativeVisibilityPointer::checked(owner.clone(), address, access.clone())
    };
    assert_eq!(
        must(
            host.escape_closing()
                .register(&owner, "Native", VisibilityTarget::native(pointer),)
        ),
        EscapeRegistrationOutcome::Registered
    );
    assert!(matches!(
        host.escape_closing()
            .handle(event(false), &["Fallback", "Native"]),
        EscapeCloseOutcome::Consumed { .. }
    ));
    assert_eq!(access.visible.load(Ordering::Acquire), 0);
    assert_eq!(access.reads.load(Ordering::Acquire), 1);
    assert_eq!(access.writes.load(Ordering::Acquire), 1);
    access.visible.store(1, Ordering::Release);
    let cleanup = host.cleanup_owner_generation(identity);
    assert!(cleanup.retirement.quiescent);
    assert_eq!(cleanup.escape_windows, 1);
    assert!(host.escape_closing().registered_windows().is_empty());
    assert_eq!(
        host.escape_closing()
            .handle(event(false), &["Fallback", "Native"]),
        EscapeCloseOutcome::Passed
    );
    assert_eq!(access.visible.load(Ordering::Acquire), 1);
    assert_eq!(access.reads.load(Ordering::Acquire), 1);
    assert_eq!(access.writes.load(Ordering::Acquire), 1);
    let stale_pointer = unsafe {
        // SAFETY: this new linear value owns an adapter Arc keeping the atomic
        // allocation live; registration rejects its retired owner before use.
        NativeVisibilityPointer::checked(owner.clone(), address, access.clone())
    };
    assert!(matches!(
        host.escape_closing().register(
            &owner,
            "Stale",
            VisibilityTarget::native(stale_pointer),
        ),
        Err(UiRegistryError::OwnerRetired(retired)) if retired == identity
    ));
    assert_eq!(access.reads.load(Ordering::Acquire), 1);
    assert_eq!(access.writes.load(Ordering::Acquire), 1);
}

#[test]
fn retained_native_cell_transactions_are_serial_across_escape_handlers() {
    let host = Arc::new(UiHost::default());
    let owner = must(host.owner(OwnerGeneration::new(4, 2)));
    let (access, entered) = BlockingVisibilityAccess::new();
    for window in ["Serial native one", "Serial native two"] {
        let pointer = unsafe {
            // SAFETY: the adapter owns the atomic for both registrations,
            // synchronizes it, and does not re-enter either registration.
            NativeVisibilityPointer::checked(owner.clone(), access.address(), access.clone())
        };
        assert_eq!(
            must(
                host.escape_closing()
                    .register(&owner, window, VisibilityTarget::native(pointer),)
            ),
            EscapeRegistrationOutcome::Registered
        );
    }

    let start = Arc::new(Barrier::new(3));
    let mut handlers = Vec::new();
    for window in ["Serial native one", "Serial native two"] {
        let host = Arc::clone(&host);
        let start = Arc::clone(&start);
        handlers.push(std::thread::spawn(move || {
            start.wait();
            host.escape_closing()
                .handle(event(false), &["Fallback", window])
        }));
    }
    start.wait();

    entered
        .recv_timeout(Duration::from_secs(1))
        .expect("one handler enters the retained adapter");
    let concurrent_entry = entered.recv_timeout(Duration::from_millis(100)).is_ok();
    access.release();

    let outcomes = handlers
        .into_iter()
        .map(|handler| handler.join().expect("Escape handler completes"))
        .collect::<Vec<_>>();
    assert!(
        !concurrent_entry,
        "retained adapter was entered concurrently"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, EscapeCloseOutcome::Consumed { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, EscapeCloseOutcome::Passed))
            .count(),
        1
    );
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
fn quick_access_owner_scoped_removal_rejects_cross_owner() {
    let host = UiHost::default();
    let resource_owner = must(host.owner(OwnerGeneration::new(30, 1)));
    let foreign_owner = must(host.owner(OwnerGeneration::new(31, 1)));
    must(host.quick_access().add_shortcut(
        &resource_owner,
        "owned-shortcut",
        "texture",
        "hover",
        "bind",
        "tooltip",
    ));
    let callback = UiCallback::managed(resource_owner.clone(), || {});
    assert_eq!(
        must(
            host.quick_access()
                .add_context_item("owned-context", "owned-shortcut", callback,)
        ),
        ContextRegistrationOutcome::Attached
    );

    let before = host.quick_access().snapshot(true, false);
    assert_eq!(
        must(
            host.quick_access()
                .remove_context_item_for_owner(&foreign_owner, "owned-context")
        ),
        0
    );
    assert!(!must(
        host.quick_access()
            .remove_shortcut_for_owner(&foreign_owner, "owned-shortcut")
    ));

    let after = host.quick_access().snapshot(true, false);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.shortcuts.len(), 1);
    assert_eq!(after.shortcuts[0].context_items.len(), 1);
}

#[test]
fn quick_access_owner_scoped_removal_rejects_stale_generation() {
    let host = UiHost::default();
    let stale_identity = OwnerGeneration::new(32, 1);
    let stale_owner = must(host.owner(stale_identity));
    host.cleanup_owner_generation(stale_identity);

    let active_owner = must(host.owner(OwnerGeneration::new(32, 2)));
    must(host.quick_access().add_shortcut(
        &active_owner,
        "reloaded-shortcut",
        "texture",
        "hover",
        "bind",
        "tooltip",
    ));
    let callback = UiCallback::managed(active_owner, || {});
    assert_eq!(
        must(host.quick_access().add_context_item(
            "reloaded-context",
            "reloaded-shortcut",
            callback,
        )),
        ContextRegistrationOutcome::Attached
    );

    let before = host.quick_access().snapshot(true, false);
    assert!(matches!(
        host.quick_access()
            .remove_context_item_for_owner(&stale_owner, "reloaded-context"),
        Err(UiRegistryError::OwnerRetired(owner)) if owner == stale_identity
    ));
    assert!(matches!(
        host.quick_access()
            .remove_shortcut_for_owner(&stale_owner, "reloaded-shortcut"),
        Err(UiRegistryError::OwnerRetired(owner)) if owner == stale_identity
    ));

    let after = host.quick_access().snapshot(true, false);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.shortcuts.len(), 1);
    assert_eq!(after.shortcuts[0].context_items.len(), 1);
}

#[test]
fn quick_access_owner_scoped_removal_allows_same_owner() {
    let host = UiHost::default();
    let owner = must(host.owner(OwnerGeneration::new(33, 1)));
    must(host.quick_access().add_shortcut(
        &owner,
        "owned-shortcut",
        "texture",
        "hover",
        "bind",
        "tooltip",
    ));
    let callback = UiCallback::managed(owner.clone(), || {});
    assert_eq!(
        must(host.quick_access().add_context_item(
            "owned-context",
            "owned-shortcut",
            callback.clone(),
        )),
        ContextRegistrationOutcome::Attached
    );
    assert_eq!(
        must(
            host.quick_access()
                .add_context_item("owned-context", "missing-shortcut", callback,)
        ),
        ContextRegistrationOutcome::Orphaned
    );

    let before = host.quick_access().snapshot(true, false);
    assert_eq!(
        must(
            host.quick_access()
                .remove_context_item_for_owner(&owner, "owned-context")
        ),
        2
    );
    let contexts_removed = host.quick_access().snapshot(true, false);
    assert!(contexts_removed.revision > before.revision);
    assert!(contexts_removed.shortcuts[0].context_items.is_empty());

    assert!(must(
        host.quick_access()
            .remove_shortcut_for_owner(&owner, "owned-shortcut")
    ));
    let shortcut_removed = host.quick_access().snapshot(true, false);
    assert!(shortcut_removed.revision > contexts_removed.revision);
    assert!(shortcut_removed.shortcuts.is_empty());
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
