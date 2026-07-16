//! Concurrency and reentrancy tests for owner cleanup and callback snapshots.

use std::fmt::Debug;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use nexus_ui_host::{
    CallbackInvocation, OwnerGeneration, RegisterRenderOutcome, RenderPhase, UiCallback, UiHost,
};

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

fn join<T>(thread: thread::JoinHandle<T>) -> T {
    match thread.join() {
        Ok(value) => value,
        Err(_) => panic!("worker thread panicked"),
    }
}

#[test]
fn callback_can_deregister_itself_without_registry_deadlock() {
    let host = Arc::new(UiHost::default());
    let owner = must(host.owner(OwnerGeneration::new(1, 1)));
    let callback_cell = Arc::new(Mutex::new(None::<UiCallback>));
    let callback_host = Arc::clone(&host);
    let callback_cell_inner = Arc::clone(&callback_cell);
    let callback = UiCallback::managed(owner, move || {
        let callback = lock(&callback_cell_inner).clone();
        if let Some(callback) = callback {
            assert_eq!(callback_host.render().deregister(&callback), 1);
        }
    });
    *lock(&callback_cell) = Some(callback.clone());
    assert_eq!(
        must(host.render().register(RenderPhase::Render, callback)),
        RegisterRenderOutcome::Registered
    );

    let snapshot = host.render().snapshot(RenderPhase::Render);
    assert_eq!(snapshot.invoke_all().invoked, 1);
    assert!(host.render().snapshot(RenderPhase::Render).is_empty());
    assert_eq!(snapshot.invoke_all().skipped_inactive, 1);
}

#[test]
fn cleanup_closes_generation_then_waits_for_other_thread_callback() {
    let host = Arc::new(UiHost::default());
    let identity = OwnerGeneration::new(2, 1);
    let owner = must(host.owner(identity));
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let callback_release = Arc::clone(&release_rx);
    let callback = UiCallback::managed(owner, move || {
        let _ignored = started_tx.send(());
        let _ignored = lock(&callback_release).recv();
    });
    assert_eq!(
        must(host.render().register(RenderPhase::Render, callback)),
        RegisterRenderOutcome::Registered
    );
    let snapshot = host.render().snapshot(RenderPhase::Render);
    let invoke_thread = thread::spawn(move || snapshot.invoke_all());
    must(started_rx.recv_timeout(Duration::from_secs(1)));

    let cleanup_host = Arc::clone(&host);
    let (cleanup_tx, cleanup_rx) = mpsc::channel();
    let cleanup_thread = thread::spawn(move || {
        let report = cleanup_host.cleanup_owner_generation(identity);
        let _ignored = cleanup_tx.send(report);
    });
    assert_eq!(
        cleanup_rx.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    );
    assert!(host.owner(identity).is_err());

    must(release_tx.send(()));
    let invocation = join(invoke_thread);
    assert_eq!(invocation.invoked, 1);
    join(cleanup_thread);
    let cleanup = must(cleanup_rx.recv_timeout(Duration::from_secs(1)));
    assert!(cleanup.retirement.quiescent);
    assert_eq!(cleanup.render_callbacks, 1);
    assert!(host.wait_owner_quiescent(identity).quiescent);
}

#[test]
fn reentrant_cleanup_retires_without_waiting_on_itself() {
    let host = Arc::new(UiHost::default());
    let identity = OwnerGeneration::new(3, 1);
    let owner = must(host.owner(identity));
    let callback_host = Arc::clone(&host);
    let observed = Arc::new(Mutex::new(None));
    let callback_observed = Arc::clone(&observed);
    let callback = UiCallback::managed(owner, move || {
        let cleanup = callback_host.cleanup_owner_generation(identity);
        *lock(&callback_observed) = Some(cleanup.retirement);
    });
    must(host.render().register(RenderPhase::Render, callback));

    let snapshot = host.render().snapshot(RenderPhase::Render);
    assert_eq!(snapshot.invoke_all().invoked, 1);
    let retirement = match *lock(&observed) {
        Some(retirement) => retirement,
        None => panic!("callback did not record cleanup"),
    };
    assert!(!retirement.quiescent);
    assert_eq!(retirement.in_flight, 1);
    assert!(host.wait_owner_quiescent(identity).quiescent);
    assert_eq!(snapshot.invoke_all().skipped_inactive, 1);
    assert_eq!(
        CallbackInvocation::SkippedInactive,
        CallbackInvocation::SkippedInactive
    );
}
