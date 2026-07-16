use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    PostMessageW, WM_CHAR, WM_DESTROY, WM_DEVICECHANGE, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NULL, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDBLCLK,
    WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use crate::signal::ShutdownSignal;
#[cfg(test)]
use crate::window_router::NoopWindowMessageRouter;
use crate::window_router::{
    RouteStage, WindowMessage, WindowMessageRoute, WindowMessageRouter, redirect_safely,
    route_safely,
};

const WM_MOUSELEAVE: u32 = 0x02A3;
pub(crate) const MESSAGE_CAPACITY: usize = 256;

/// Pointer-free copy of one scalar Win32 input message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Win32Message {
    pub(crate) message: u32,
    pub(crate) wparam: usize,
    pub(crate) lparam: isize,
}

pub(crate) struct WindowTarget {
    active: AtomicBool,
    visible: AtomicBool,
    capture_mouse: AtomicBool,
    capture_keyboard: AtomicBool,
    selected_swap_chain: AtomicU64,
    bound_hwnd: AtomicUsize,
    shutdown_requested: AtomicBool,
    input_lost: AtomicBool,
    queue: Mutex<VecDeque<Win32Message>>,
    shutdown: Arc<dyn ShutdownSignal>,
    router: Arc<dyn WindowMessageRouter>,
}

impl WindowTarget {
    #[cfg(test)]
    pub(crate) fn new(visible: bool, shutdown: Arc<dyn ShutdownSignal>) -> Self {
        Self::with_router(visible, shutdown, Arc::new(NoopWindowMessageRouter))
    }

    pub(crate) fn with_router(
        visible: bool,
        shutdown: Arc<dyn ShutdownSignal>,
        router: Arc<dyn WindowMessageRouter>,
    ) -> Self {
        Self {
            active: AtomicBool::new(true),
            visible: AtomicBool::new(visible),
            capture_mouse: AtomicBool::new(false),
            capture_keyboard: AtomicBool::new(false),
            selected_swap_chain: AtomicU64::new(0),
            bound_hwnd: AtomicUsize::new(0),
            shutdown_requested: AtomicBool::new(false),
            input_lost: AtomicBool::new(false),
            queue: Mutex::new(VecDeque::with_capacity(MESSAGE_CAPACITY)),
            shutdown,
            router,
        }
    }

    pub(crate) fn route_before_ui(&self, message: WindowMessage) -> WindowMessageRoute {
        route_safely(self.router.as_ref(), message, RouteStage::BeforeUi)
    }

    pub(crate) fn route_after_ui(&self, message: WindowMessage) -> WindowMessageRoute {
        route_safely(self.router.as_ref(), message, RouteStage::AfterUi)
    }

    pub(crate) fn redirect_game_only(&self, message: WindowMessage) -> WindowMessage {
        redirect_safely(self.router.as_ref(), message)
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
        self.visible.store(false, Ordering::Release);
        self.set_capture(false, false);
    }

    pub(crate) fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Release);
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    pub(crate) fn set_capture(&self, mouse: bool, keyboard: bool) {
        self.capture_mouse.store(mouse, Ordering::Release);
        self.capture_keyboard.store(keyboard, Ordering::Release);
    }

    pub(crate) fn select_swap_chain(&self, id: u64) {
        self.selected_swap_chain.store(id, Ordering::Release);
    }

    pub(crate) fn is_selected_swap_chain(&self, id: u64) -> bool {
        id != 0 && self.selected_swap_chain.load(Ordering::Acquire) == id
    }

    pub(crate) fn bind_window(&self, hwnd: usize) {
        self.bound_hwnd.store(hwnd, Ordering::Release);
    }

    pub(crate) fn request_thread_cleanup(&self) {
        let hwnd = self.bound_hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            // SAFETY: this posts a pointer-free no-op message to the opaque
            // window token recorded during successful same-process subclassing.
            let _ = unsafe { PostMessageW(hwnd, WM_NULL, 0, 0) };
        }
    }

    pub(crate) fn should_consume(&self, message: u32) -> bool {
        if !self.is_active() || !self.is_visible() {
            return false;
        }
        match message_class(message) {
            MessageClass::Mouse => self.capture_mouse.load(Ordering::Acquire),
            MessageClass::Keyboard => self.capture_keyboard.load(Ordering::Acquire),
            MessageClass::Focus => {
                self.capture_mouse.load(Ordering::Acquire)
                    || self.capture_keyboard.load(Ordering::Acquire)
            }
            MessageClass::Other => false,
        }
    }

    pub(crate) fn enqueue(&self, mut message: Win32Message) {
        if !is_pointer_free_message(message.message) {
            return;
        }
        if message.message == WM_DEVICECHANGE {
            // The platform backend consumes only the scalar wparam for this
            // event. Never retain the optional device-broadcast pointer.
            message.lparam = 0;
        }
        match self.queue.try_lock() {
            Ok(mut queue) => self.push_bounded(&mut queue, message),
            Err(TryLockError::Poisoned(poisoned)) => {
                let mut queue = poisoned.into_inner();
                self.push_bounded(&mut queue, message);
            }
            Err(TryLockError::WouldBlock) => {
                self.input_lost.store(true, Ordering::Release);
            }
        }
    }

    pub(crate) fn route_shutdown(&self, message: u32) {
        if message == WM_DESTROY
            && self.is_active()
            && self
                .shutdown_requested
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| self.shutdown.request_shutdown()));
        }
    }

    pub(crate) fn drain(&self) -> Vec<Win32Message> {
        let mut queue = recover(&self.queue);
        if self.input_lost.swap(false, Ordering::AcqRel) {
            queue.clear();
            return vec![Win32Message {
                message: WM_KILLFOCUS,
                wparam: 0,
                lparam: 0,
            }];
        }
        queue.drain(..).collect()
    }

    pub(crate) fn reset(&self) {
        recover(&self.queue).clear();
        self.input_lost.store(false, Ordering::Release);
        self.set_capture(false, false);
    }

    fn push_bounded(&self, queue: &mut VecDeque<Win32Message>, message: Win32Message) {
        if queue.len() == MESSAGE_CAPACITY {
            queue.clear();
            self.input_lost.store(true, Ordering::Release);
        }
        queue.push_back(message);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageClass {
    Mouse,
    Keyboard,
    Focus,
    Other,
}

fn message_class(message: u32) -> MessageClass {
    match message {
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK
        | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONDBLCLK | WM_MBUTTONUP | WM_XBUTTONDOWN
        | WM_XBUTTONDBLCLK | WM_XBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_MOUSEMOVE
        | WM_MOUSELEAVE => MessageClass::Mouse,
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_CHAR => MessageClass::Keyboard,
        WM_KILLFOCUS | WM_SETFOCUS => MessageClass::Focus,
        _ => MessageClass::Other,
    }
}

fn is_pointer_free_message(message: u32) -> bool {
    !matches!(message_class(message), MessageClass::Other)
        || matches!(message, WM_DEVICECHANGE | WM_SETCURSOR)
}

fn recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WM_DESTROY, WM_DEVICECHANGE, WM_KEYDOWN, WM_KEYUP, WM_MOUSEMOVE, WM_PAINT,
    };

    use crate::signal::NoopShutdownSignal;

    use super::{MESSAGE_CAPACITY, Win32Message, WindowTarget};

    fn message(message: u32, wparam: usize) -> Win32Message {
        Win32Message {
            message,
            wparam,
            lparam: 0,
        }
    }

    fn target(visible: bool) -> WindowTarget {
        WindowTarget::new(visible, Arc::new(NoopShutdownSignal))
    }

    #[test]
    fn queue_accepts_only_scalar_input_messages() {
        let target = target(true);
        target.enqueue(message(WM_PAINT, 1));
        target.enqueue(message(WM_KEYDOWN, 2));
        assert_eq!(target.drain(), vec![message(WM_KEYDOWN, 2)]);
    }

    #[test]
    fn overflow_resets_input_instead_of_leaving_stuck_keys() {
        let target = target(true);
        for value in 0..=MESSAGE_CAPACITY {
            target.enqueue(message(WM_KEYDOWN, value));
        }
        let drained = target.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0].message,
            windows_sys::Win32::UI::WindowsAndMessaging::WM_KILLFOCUS
        );
    }

    #[test]
    fn capture_requires_explicit_visibility_and_matching_input_class() {
        let target = target(false);
        target.set_capture(true, false);
        assert!(!target.should_consume(WM_MOUSEMOVE));
        target.set_visible(true);
        assert!(target.should_consume(WM_MOUSEMOVE));
        assert!(!target.should_consume(WM_KEYUP));
        target.set_capture(false, true);
        assert!(target.should_consume(WM_KEYUP));
        target.deactivate();
        assert!(!target.should_consume(WM_KEYUP));
    }

    #[test]
    fn reset_drops_messages_and_capture_intent() {
        let target = target(true);
        target.set_capture(true, true);
        target.enqueue(message(WM_KEYDOWN, 1));
        target.reset();
        assert!(target.drain().is_empty());
        assert!(!target.should_consume(WM_KEYDOWN));
    }

    #[test]
    fn destroy_routes_one_injected_shutdown_request() {
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let target = WindowTarget::new(
            true,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::Relaxed);
            }),
        );
        target.route_shutdown(WM_DESTROY);
        target.route_shutdown(WM_DESTROY);
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        target.deactivate();
        target.route_shutdown(WM_DESTROY);
        assert_eq!(requests.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn shutdown_callback_panics_never_cross_wndproc_boundary() {
        let target = WindowTarget::new(true, Arc::new(|| panic!("test shutdown panic")));
        target.route_shutdown(WM_DESTROY);
    }

    #[test]
    fn device_change_never_retains_a_broadcast_pointer() {
        let target = target(true);
        target.enqueue(Win32Message {
            message: WM_DEVICECHANGE,
            wparam: 7,
            lparam: 0x1234,
        });
        assert_eq!(
            target.drain(),
            vec![Win32Message {
                message: WM_DEVICECHANGE,
                wparam: 7,
                lparam: 0,
            }]
        );
    }
}
