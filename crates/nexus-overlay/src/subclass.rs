use core::marker::PhantomData;
use core::mem;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};

use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, SetLastError, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWLP_WNDPROC, GetWindowLongPtrW, GetWindowThreadProcessId,
    IsWindow, SetWindowLongPtrW, WNDPROC,
};

use crate::message_queue::{Win32Message, WindowTarget};
use crate::window_router::{WindowMessage, WindowMessageRoute};

static SUBCLASSES: LazyLock<Mutex<HashMap<usize, Arc<SubclassEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubclassError {
    InvalidWindow,
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
        if registry.contains_key(&key) {
            return Err(SubclassError::HookConflict);
        }

        // SAFETY: `hwnd` is a live same-thread window. Clearing last error is
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
        });
        registry.insert(key, Arc::clone(&entry));

        // SAFETY: the registry entry is visible before the live same-thread
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
        restore_if_topmost(&self.entry);
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

    let target = entry.target.upgrade();
    let active = target.as_ref().is_some_and(|target| target.is_active());
    if !active {
        restore_if_topmost(&entry);
        crate::adapter::retire_thread_state(entry.adapter_id);
    }

    if let Some(target) = target.filter(|target| target.is_active()) {
        let routed = WindowMessage {
            window: hwnd as usize,
            message,
            wparam,
            lparam,
        };
        let routed = match target.route_before_ui(routed) {
            WindowMessageRoute::Continue(routed) => routed,
            WindowMessageRoute::Consume => return 0,
        };
        target.enqueue(Win32Message {
            message: routed.message,
            wparam: routed.wparam,
            lparam: routed.lparam,
        });
        if target.should_consume(routed.message) {
            return 0;
        }
        let routed = match target.route_after_ui(routed) {
            WindowMessageRoute::Continue(routed) => routed,
            WindowMessageRoute::Consume => return 0,
        };
        target.route_shutdown(routed.message);
        let routed = target.redirect_game_only(routed);
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

fn restore_if_topmost(entry: &Arc<SubclassEntry>) {
    let hwnd = entry.hwnd as HWND;
    let mut registry = recover(&SUBCLASSES);
    let is_registered = registry
        .get(&entry.hwnd)
        .is_some_and(|registered| Arc::ptr_eq(registered, entry));
    if !is_registered {
        return;
    }
    // SAFETY: `hwnd` originated from a successfully installed subclass.
    let current = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
    if current != hook_address() {
        // A newer subclass chains through ours. Keeping the entry is required
        // so that later calls can still reach the exact predecessor.
        return;
    }
    let original = entry.original.load(Ordering::Acquire);
    // SAFETY: same-thread callers restore the exact procedure value returned
    // by installation. WndProc retirement also runs on the window thread.
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, CreateWindowExW, DestroyWindow, GWLP_WNDPROC, GetWindowLongPtrW,
        SendMessageW, SetWindowLongPtrW, WM_KEYDOWN, WM_KEYUP, WM_NULL, WNDPROC,
    };

    use crate::message_queue::WindowTarget;

    use super::{SubclassError, WindowSubclass, hook_address};

    static BASE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static BASE_ORIGINAL: AtomicIsize = AtomicIsize::new(0);

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
    fn real_window_consumes_by_policy_and_otherwise_calls_original_once() {
        let window = TestWindow::create();
        let target = Arc::new(WindowTarget::new(
            true,
            Arc::new(crate::signal::NoopShutdownSignal),
        ));
        target.set_capture(false, true);
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
}
