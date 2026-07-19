use core::marker::PhantomData;
use core::mem;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};

use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, SetLastError, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWLP_WNDPROC, GetWindowLongPtrW, GetWindowThreadProcessId,
    IsWindow, PostMessageW, SetWindowLongPtrW, WM_NCDESTROY, WM_NULL, WNDPROC,
};

use crate::message_queue::{Win32Message, WindowTarget};
use crate::window_router::{WindowMessage, WindowMessageRoute};

static SUBCLASSES: LazyLock<Mutex<HashMap<usize, Arc<SubclassEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubclassError {
    InvalidWindow,
    InactiveTarget,
    ForeignProcess,
    HookConflict,
    Win32(u32),
}

#[derive(Debug)]
struct SubclassEntry {
    hwnd: usize,
    adapter_id: u64,
    original: AtomicIsize,
    target: Weak<WindowTarget>,
    leases: AtomicUsize,
}

/// Thread-bound ownership token for one explicitly selected window subclass.
#[derive(Debug)]
pub(crate) struct WindowSubclass {
    entry: Arc<SubclassEntry>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl WindowSubclass {
    pub(crate) fn install(
        hwnd: HWND,
        adapter_id: u64,
        target: &Arc<WindowTarget>,
    ) -> Result<Self, SubclassError> {
        if !target.is_active() {
            return Err(SubclassError::InactiveTarget);
        }
        if hwnd.is_null() {
            return Err(SubclassError::InvalidWindow);
        }
        // SAFETY: Win32 validates opaque window handles without dereferencing
        // memory supplied by the caller.
        if unsafe { IsWindow(hwnd) } == 0 {
            return Err(SubclassError::InvalidWindow);
        }
        let mut window_process = 0_u32;
        // SAFETY: the process-id output is valid and Win32 only inspects the
        // opaque window handle.
        let window_thread = unsafe { GetWindowThreadProcessId(hwnd, &mut window_process) };
        // SAFETY: querying the current process has no preconditions.
        let current_process = unsafe { GetCurrentProcessId() };
        if window_thread == 0 || window_process != current_process {
            return Err(SubclassError::ForeignProcess);
        }

        let key = hwnd as usize;
        let mut registry = recover(&SUBCLASSES);
        if !target.is_active() {
            return Err(SubclassError::InactiveTarget);
        }
        if let Some(entry) = registry.get(&key) {
            let shares_target = entry.adapter_id == adapter_id
                && entry
                    .target
                    .upgrade()
                    .is_some_and(|existing| Arc::ptr_eq(&existing, target));
            if !shares_target {
                return Err(SubclassError::HookConflict);
            }
            let leases = entry.leases.load(Ordering::Acquire);
            let Some(leases) = leases.checked_add(1) else {
                return Err(SubclassError::HookConflict);
            };
            entry.leases.store(leases, Ordering::Release);
            return Ok(Self {
                entry: Arc::clone(entry),
                _thread_bound: PhantomData,
            });
        }

        // SAFETY: `hwnd` is a live same-process window. Clearing last error is
        // required because a zero procedure value is otherwise ambiguous.
        unsafe { SetLastError(0) };
        // SAFETY: the index requests the current window procedure only.
        let observed_original = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
        // SAFETY: reading the calling thread's last-error slot has no preconditions.
        let read_error = unsafe { GetLastError() };
        if observed_original == 0 && read_error != 0 {
            return Err(SubclassError::Win32(read_error));
        }
        if observed_original == 0 {
            return Err(SubclassError::InvalidWindow);
        }

        let entry = Arc::new(SubclassEntry {
            hwnd: key,
            adapter_id,
            original: AtomicIsize::new(observed_original),
            target: Arc::downgrade(target),
            leases: AtomicUsize::new(1),
        });
        registry.insert(key, Arc::clone(&entry));

        // SAFETY: the registry entry is visible before the live same-process
        // window can dispatch to our ABI-compatible procedure.
        unsafe { SetLastError(0) };
        // SAFETY: `overlay_wnd_proc` has the exact WNDPROC ABI and the original
        // value remains stored for pass-through and restoration.
        let replaced = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, hook_address()) };
        // SAFETY: reading the calling thread's last-error slot has no preconditions.
        let replace_error = unsafe { GetLastError() };
        if replaced == 0 && replace_error != 0 {
            registry.remove(&key);
            return Err(SubclassError::Win32(replace_error));
        }
        if replaced != 0 {
            entry.original.store(replaced, Ordering::Release);
        }

        Ok(Self {
            entry,
            _thread_bound: PhantomData,
        })
    }
}

impl Drop for WindowSubclass {
    fn drop(&mut self) {
        release_subclass(&self.entry);
    }
}

unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let entry = {
        let registry = recover(&SUBCLASSES);
        registry.get(&(hwnd as usize)).cloned()
    };
    let Some(entry) = entry else {
        // SAFETY: the registry invariant was already lost, so the system
        // default is the only callable procedure available.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };

    if message == WM_NCDESTROY {
        crate::adapter::retire_thread_state_for_window(entry.adapter_id, hwnd as usize);
        let result = call_original(&entry, hwnd, message, wparam, lparam);
        remove_registered_entry(&entry);
        return result;
    }

    let target = entry.target.upgrade();
    let active = target.as_ref().is_some_and(|target| target.is_active());
    if !active {
        crate::adapter::retire_thread_state(entry.adapter_id);
        restore_inactive_target_if_topmost(&entry);
        return call_original(&entry, hwnd, message, wparam, lparam);
    }
    if entry.leases.load(Ordering::Acquire) == 0 && restore_if_unleased_and_topmost(&entry) {
        return call_original(&entry, hwnd, message, wparam, lparam);
    }

    if let Some(target) =
        target.filter(|target| target.is_active() && target.is_bound_window(hwnd as usize))
    {
        let original = WindowMessage {
            window: hwnd as usize,
            message,
            wparam,
            lparam,
        };
        let before_ui = target.route_before_ui(original);
        if let Some(result) = pass_through_if_routing_stopped(&entry, &target, hwnd, original) {
            return result;
        }
        let routed = match before_ui {
            WindowMessageRoute::Continue(routed) => routed,
            WindowMessageRoute::Consume => return 0,
        };
        target.enqueue(Win32Message {
            message: routed.message,
            wparam: routed.wparam,
            lparam: routed.lparam,
        });
        let consume = target.should_consume(routed.message);
        if let Some(result) = pass_through_if_routing_stopped(&entry, &target, hwnd, original) {
            return result;
        }
        if consume {
            return 0;
        }
        let after_ui = target.route_after_ui(routed);
        if let Some(result) = pass_through_if_routing_stopped(&entry, &target, hwnd, original) {
            return result;
        }
        let routed = match after_ui {
            WindowMessageRoute::Continue(routed) => routed,
            WindowMessageRoute::Consume => return 0,
        };
        target.route_shutdown(routed.message);
        if let Some(result) = pass_through_if_routing_stopped(&entry, &target, hwnd, original) {
            return result;
        }
        let routed = target.redirect_game_only(routed);
        if let Some(result) = pass_through_if_routing_stopped(&entry, &target, hwnd, original) {
            return result;
        }
        return call_original(
            &entry,
            routed.window as HWND,
            routed.message,
            routed.wparam,
            routed.lparam,
        );
    }

    call_original(&entry, hwnd, message, wparam, lparam)
}

fn pass_through_if_routing_stopped(
    entry: &Arc<SubclassEntry>,
    target: &WindowTarget,
    hwnd: HWND,
    original: WindowMessage,
) -> Option<LRESULT> {
    if target.is_active() && target.is_bound_window(hwnd as usize) {
        return None;
    }
    if !target.is_active() {
        target.reset();
        crate::adapter::retire_thread_state(entry.adapter_id);
        restore_inactive_target_if_topmost(entry);
    }
    Some(call_original(
        entry,
        original.window as HWND,
        original.message,
        original.wparam,
        original.lparam,
    ))
}

fn release_subclass(entry: &Arc<SubclassEntry>) {
    let registry = recover(&SUBCLASSES);
    let is_registered = registry
        .get(&entry.hwnd)
        .is_some_and(|registered| Arc::ptr_eq(registered, entry));
    if !is_registered {
        return;
    }
    let leases = entry.leases.load(Ordering::Acquire);
    let Some(remaining) = leases.checked_sub(1) else {
        debug_assert!(false, "a live subclass handle must own one lease");
        return;
    };
    entry.leases.store(remaining, Ordering::Release);
    if remaining == 0 {
        let hwnd = entry.hwnd as HWND;
        drop(registry);
        // SAFETY: this posts a pointer-free no-op message to the same-process
        // window whose live hook owns this entry. Restoration then executes on
        // the window thread, after all earlier dispatches have completed.
        let _ = unsafe { PostMessageW(hwnd, WM_NULL, 0, 0) };
    }
}

fn remove_registered_entry(entry: &Arc<SubclassEntry>) {
    let mut registry = recover(&SUBCLASSES);
    if registry
        .get(&entry.hwnd)
        .is_some_and(|registered| Arc::ptr_eq(registered, entry))
    {
        registry.remove(&entry.hwnd);
    }
}

fn restore_if_unleased_and_topmost(entry: &Arc<SubclassEntry>) -> bool {
    let mut registry = recover(&SUBCLASSES);
    let is_registered = registry
        .get(&entry.hwnd)
        .is_some_and(|registered| Arc::ptr_eq(registered, entry));
    if !is_registered {
        return true;
    }
    if entry.leases.load(Ordering::Acquire) != 0 {
        return false;
    }
    restore_if_topmost(entry, &mut registry);
    true
}

fn restore_inactive_target_if_topmost(entry: &Arc<SubclassEntry>) {
    let mut registry = recover(&SUBCLASSES);
    if registry
        .get(&entry.hwnd)
        .is_some_and(|registered| Arc::ptr_eq(registered, entry))
    {
        restore_if_topmost(entry, &mut registry);
    }
}

fn restore_if_topmost(
    entry: &Arc<SubclassEntry>,
    registry: &mut HashMap<usize, Arc<SubclassEntry>>,
) {
    let hwnd = entry.hwnd as HWND;
    // SAFETY: `hwnd` originated from a successfully installed subclass.
    let current = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
    if current != hook_address() {
        // A newer subclass chains through ours. Keeping the entry is required
        // so that later calls can still reach the exact predecessor.
        return;
    }
    let original = entry.original.load(Ordering::Acquire);
    // SAFETY: this window-thread callback restores the exact procedure value
    // returned by installation while the registry serializes lease changes.
    unsafe { SetLastError(0) };
    // SAFETY: `original` is the live predecessor returned by Win32.
    let replaced = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, original) };
    // SAFETY: reading the calling thread's last-error slot has no preconditions.
    let error = unsafe { GetLastError() };
    if replaced != 0 || error == 0 {
        registry.remove(&entry.hwnd);
    }
}

fn call_original(
    entry: &SubclassEntry,
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let original = entry.original.load(Ordering::Acquire);
    // SAFETY: installation obtained this exact pointer from the window's
    // `GWLP_WNDPROC` slot, and the entry stays alive across the call.
    let procedure = unsafe { mem::transmute::<isize, WNDPROC>(original) };
    // SAFETY: the procedure and message tuple are the exact values supplied by
    // Win32. This is the sole pass-through call on this control-flow path.
    unsafe { CallWindowProcW(procedure, hwnd, message, wparam, lparam) }
}

fn hook_address() -> isize {
    overlay_wnd_proc as *const () as isize
}

fn recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use core::mem;
    use core::ptr;
    use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, Weak};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, CreateWindowExW, DestroyWindow, GWLP_WNDPROC, GetWindowLongPtrW,
        SendMessageW, SetWindowLongPtrW, WM_KEYDOWN, WM_KEYUP, WM_NULL, WNDPROC,
    };

    use crate::message_queue::WindowTarget;
    use crate::window_router::{WindowMessage, WindowMessageRoute, WindowMessageRouter};

    use super::{SubclassError, WindowSubclass, hook_address};

    static BASE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static BASE_ORIGINAL: AtomicIsize = AtomicIsize::new(0);
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Default)]
    struct DeactivatingRouter {
        target: Mutex<Option<Weak<WindowTarget>>>,
    }

    impl WindowMessageRouter for DeactivatingRouter {
        fn before_ui(&self, _message: WindowMessage) -> WindowMessageRoute {
            if let Some(target) = self
                .target
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(Weak::upgrade)
            {
                target.deactivate();
            }
            WindowMessageRoute::Consume
        }
    }

    fn serialize_windows() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    unsafe extern "system" fn counting_wnd_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        BASE_CALLS.fetch_add(1, Ordering::Relaxed);
        let original = BASE_ORIGINAL.load(Ordering::Acquire);
        // SAFETY: the smoke-test window stored the exact predecessor returned
        // by its GWLP_WNDPROC slot and remains alive across this call.
        let procedure = unsafe { mem::transmute::<isize, WNDPROC>(original) };
        // SAFETY: this forwards the exact system-supplied message tuple.
        unsafe { CallWindowProcW(procedure, hwnd, message, wparam, lparam) }
    }

    struct TestWindow {
        hwnd: HWND,
        system_procedure: isize,
    }

    impl TestWindow {
        fn create() -> Self {
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            // SAFETY: all strings are terminated, optional handles are null,
            // and the hidden zero-sized system-class window is test-owned.
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    ptr::null(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            assert!(!hwnd.is_null(), "hidden Win32 smoke-test window failed");
            // SAFETY: the test owns this live window.
            let system_procedure = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
            assert_ne!(system_procedure, 0, "system WndProc must be available");
            BASE_ORIGINAL.store(system_procedure, Ordering::Release);
            // SAFETY: the counting procedure has the exact WNDPROC ABI and
            // forwards to the saved system predecessor.
            let replaced = unsafe {
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, counting_wnd_proc as *const () as isize)
            };
            assert_eq!(replaced, system_procedure);
            BASE_CALLS.store(0, Ordering::Release);
            Self {
                hwnd,
                system_procedure,
            }
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: best-effort restoration and destruction of the live
            // test-owned window; failures cannot escape Drop.
            let _ = unsafe { SetWindowLongPtrW(self.hwnd, GWLP_WNDPROC, self.system_procedure) };
            // SAFETY: this handle was returned by CreateWindowExW in this test.
            let _ = unsafe { DestroyWindow(self.hwnd) };
        }
    }

    #[test]
    fn procedure_address_is_nonzero() {
        assert_ne!(hook_address(), 0);
    }

    #[test]
    fn null_window_is_rejected_before_native_mutation() {
        let target = Arc::new(WindowTarget::new(
            true,
            Arc::new(crate::signal::NoopShutdownSignal),
        ));
        assert!(matches!(
            WindowSubclass::install(core::ptr::null_mut(), 1, &target),
            Err(SubclassError::InvalidWindow)
        ));
    }

    #[test]
    fn inactive_target_cannot_install_a_new_subclass() {
        let _serial = serialize_windows();
        let window = TestWindow::create();
        let target = Arc::new(WindowTarget::new(
            true,
            Arc::new(crate::signal::NoopShutdownSignal),
        ));
        target.deactivate();

        assert!(matches!(
            WindowSubclass::install(window.hwnd, 7, &target),
            Err(SubclassError::InactiveTarget)
        ));
        assert_eq!(
            // SAFETY: the test owns this live window.
            unsafe { GetWindowLongPtrW(window.hwnd, GWLP_WNDPROC) },
            counting_wnd_proc as *const () as isize
        );
    }

    #[test]
    fn real_window_consumes_by_policy_and_otherwise_calls_original_once() {
        let _serial = serialize_windows();
        let window = TestWindow::create();
        let target = Arc::new(WindowTarget::new(
            true,
            Arc::new(crate::signal::NoopShutdownSignal),
        ));
        target.set_capture(false, true);
        target.bind_window(window.hwnd as usize);
        let subclass = WindowSubclass::install(window.hwnd, 7, &target)
            .expect("same-process hidden test window should subclass");

        // SAFETY: SendMessageW synchronously dispatches this scalar test tuple.
        let _ = unsafe { SendMessageW(window.hwnd, WM_KEYDOWN, usize::from(b'A'), 0) };
        assert_eq!(BASE_CALLS.load(Ordering::Acquire), 0);

        target.set_capture(false, false);
        // SAFETY: SendMessageW synchronously dispatches this scalar test tuple.
        let _ = unsafe { SendMessageW(window.hwnd, WM_KEYUP, usize::from(b'A'), 0) };
        assert_eq!(BASE_CALLS.load(Ordering::Acquire), 1);

        drop(subclass);
        // SAFETY: the window remains live and WM_NULL has no pointer payload.
        let _ = unsafe { SendMessageW(window.hwnd, WM_NULL, 0, 0) };
        assert_eq!(BASE_CALLS.load(Ordering::Acquire), 2);
    }

    #[test]
    fn matching_adapter_and_target_share_one_refcounted_hook() {
        let _serial = serialize_windows();
        let window = TestWindow::create();
        let target = Arc::new(WindowTarget::new(
            true,
            Arc::new(crate::signal::NoopShutdownSignal),
        ));
        let first = WindowSubclass::install(window.hwnd, 7, &target)
            .expect("first matching subclass should install");
        let second = WindowSubclass::install(window.hwnd, 7, &target)
            .expect("second matching subclass should share the hook");

        assert_eq!(
            // SAFETY: the test owns this live window.
            unsafe { GetWindowLongPtrW(window.hwnd, GWLP_WNDPROC) },
            hook_address()
        );
        drop(first);
        assert_eq!(
            // SAFETY: the test owns this live window.
            unsafe { GetWindowLongPtrW(window.hwnd, GWLP_WNDPROC) },
            hook_address()
        );
        drop(second);
        // SAFETY: this synchronously executes the zero-lease cleanup path on
        // the test window's owning thread.
        let _ = unsafe { SendMessageW(window.hwnd, WM_NULL, 0, 0) };
        assert_eq!(
            // SAFETY: the test owns this live window.
            unsafe { GetWindowLongPtrW(window.hwnd, GWLP_WNDPROC) },
            counting_wnd_proc as *const () as isize
        );
    }

    #[test]
    fn same_window_rejects_a_different_adapter_or_target() {
        let _serial = serialize_windows();
        let window = TestWindow::create();
        let target = Arc::new(WindowTarget::new(
            true,
            Arc::new(crate::signal::NoopShutdownSignal),
        ));
        let other_target = Arc::new(WindowTarget::new(
            true,
            Arc::new(crate::signal::NoopShutdownSignal),
        ));
        let subclass = WindowSubclass::install(window.hwnd, 7, &target)
            .expect("first subclass should install");

        assert!(matches!(
            WindowSubclass::install(window.hwnd, 8, &target),
            Err(SubclassError::HookConflict)
        ));
        assert!(matches!(
            WindowSubclass::install(window.hwnd, 7, &other_target),
            Err(SubclassError::HookConflict)
        ));
        drop(subclass);
        // SAFETY: this synchronously executes zero-lease cleanup.
        let _ = unsafe { SendMessageW(window.hwnd, WM_NULL, 0, 0) };
    }

    #[test]
    fn stale_window_hook_is_pass_through_after_target_rebind() {
        let _serial = serialize_windows();
        let stale = TestWindow::create();
        let current = TestWindow::create();
        let target = Arc::new(WindowTarget::new(
            true,
            Arc::new(crate::signal::NoopShutdownSignal),
        ));
        target.set_capture(false, true);
        let stale_subclass = WindowSubclass::install(stale.hwnd, 7, &target)
            .expect("stale test window should subclass");
        let current_subclass = WindowSubclass::install(current.hwnd, 7, &target)
            .expect("current test window should subclass");
        target.bind_window(current.hwnd as usize);
        BASE_CALLS.store(0, Ordering::Release);

        // SAFETY: SendMessageW synchronously dispatches scalar test tuples.
        let _ = unsafe { SendMessageW(stale.hwnd, WM_KEYDOWN, usize::from(b'A'), 0) };
        assert_eq!(BASE_CALLS.load(Ordering::Acquire), 1);
        // SAFETY: SendMessageW synchronously dispatches scalar test tuples.
        let _ = unsafe { SendMessageW(current.hwnd, WM_KEYDOWN, usize::from(b'A'), 0) };
        assert_eq!(BASE_CALLS.load(Ordering::Acquire), 1);

        drop(stale_subclass);
        drop(current_subclass);
        // SAFETY: these synchronously execute zero-lease cleanup on each
        // test-owned window before its handle can be reused.
        let _ = unsafe { SendMessageW(stale.hwnd, WM_NULL, 0, 0) };
        // SAFETY: this synchronously executes zero-lease cleanup on the second
        // test-owned window before its handle can be reused.
        let _ = unsafe { SendMessageW(current.hwnd, WM_NULL, 0, 0) };
    }

    #[test]
    fn deactivated_target_is_immediately_pass_through() {
        let _serial = serialize_windows();
        let window = TestWindow::create();
        let target = Arc::new(WindowTarget::new(
            true,
            Arc::new(crate::signal::NoopShutdownSignal),
        ));
        target.set_capture(false, true);
        target.bind_window(window.hwnd as usize);
        let subclass = WindowSubclass::install(window.hwnd, 7, &target)
            .expect("same-process hidden test window should subclass");
        target.deactivate();
        BASE_CALLS.store(0, Ordering::Release);

        // SAFETY: SendMessageW synchronously dispatches this scalar test tuple.
        let _ = unsafe { SendMessageW(window.hwnd, WM_KEYDOWN, usize::from(b'A'), 0) };
        assert_eq!(BASE_CALLS.load(Ordering::Acquire), 1);
        assert_eq!(
            // SAFETY: the test owns this live window.
            unsafe { GetWindowLongPtrW(window.hwnd, GWLP_WNDPROC) },
            counting_wnd_proc as *const () as isize
        );

        drop(subclass);
        // SAFETY: this synchronously executes zero-lease cleanup.
        let _ = unsafe { SendMessageW(window.hwnd, WM_NULL, 0, 0) };
    }

    #[test]
    fn reentrant_deactivation_overrides_router_consumption() {
        let _serial = serialize_windows();
        let window = TestWindow::create();
        let router = Arc::new(DeactivatingRouter::default());
        let target = Arc::new(WindowTarget::with_router(
            true,
            Arc::new(crate::signal::NoopShutdownSignal),
            router.clone(),
        ));
        *router
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(&target));
        target.set_capture(false, true);
        target.bind_window(window.hwnd as usize);
        let subclass = WindowSubclass::install(window.hwnd, 7, &target)
            .expect("same-process hidden test window should subclass");
        BASE_CALLS.store(0, Ordering::Release);

        // SAFETY: SendMessageW synchronously dispatches this scalar test tuple.
        let _ = unsafe { SendMessageW(window.hwnd, WM_KEYDOWN, usize::from(b'A'), 0) };

        assert_eq!(BASE_CALLS.load(Ordering::Acquire), 1);
        assert!(!target.is_active());
        assert!(target.drain().is_empty());
        assert_eq!(
            // SAFETY: the test owns this live window.
            unsafe { GetWindowLongPtrW(window.hwnd, GWLP_WNDPROC) },
            counting_wnd_proc as *const () as isize
        );

        drop(subclass);
    }
}
