use crate::encoding::{
    PASSTHROUGH_FIRST, async_key_is_down, key_lparam, key_message_id, mouse_wparam, point_lparam,
    wire_message_id,
};
use crate::{Win32GameInput, WindowAttachError};
use nexus_input::{
    GameBindId, GameBindRegistry, GameInvoker, GameMessage, GameMessageSink, GameOnlyMessageSink,
    GameSinkError, GameSlot, InputBind, InputDevice, Modifier, ModifierState, MouseButton,
    PhysicalInputState,
};
use std::collections::BTreeMap;
use std::mem;
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Barrier, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows_sys::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::SystemServices::{MK_CONTROL, MK_LBUTTON, MK_SHIFT, MK_XBUTTON1};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_MENU, VK_RIGHT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_WNDPROC, GetCursorPos, GetMessageW, GetWindowLongPtrW, IsWindow, MSG, PostMessageW,
    PostQuitMessage, PostThreadMessageW, SetWindowLongPtrW, TranslateMessage, WM_CLOSE, WM_DESTROY,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDPROC,
};

const RECEIVE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecordedMessage {
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
}

#[derive(Clone)]
struct WindowRoute {
    original: isize,
    sender: Sender<RecordedMessage>,
}

struct ReleasedPhysicalModifiers;

impl PhysicalInputState for ReleasedPhysicalModifiers {
    fn modifiers(&self) -> ModifierState {
        ModifierState {
            alt: false,
            control: false,
            shift: false,
        }
    }
}

static WINDOW_ROUTES: OnceLock<Mutex<BTreeMap<usize, WindowRoute>>> = OnceLock::new();

fn window_routes() -> &'static Mutex<BTreeMap<usize, WindowRoute>> {
    WINDOW_ROUTES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn lock_routes() -> MutexGuard<'static, BTreeMap<usize, WindowRoute>> {
    window_routes()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

unsafe extern "system" fn recording_wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if (PASSTHROUGH_FIRST..PASSTHROUGH_FIRST + WM_USER).contains(&message) {
        let sender = lock_routes()
            .get(&(hwnd as usize))
            .map(|route| route.sender.clone());
        if let Some(sender) = sender {
            let _ = sender.send(RecordedMessage {
                message,
                wparam,
                lparam,
            });
        }
        return 0;
    }

    if message == WM_CLOSE {
        // SAFETY: the message-pump thread owns this live test window.
        let _ = unsafe { DestroyWindow(hwnd) };
        return 0;
    }
    if message == WM_DESTROY {
        // SAFETY: this is running on the owning message-pump thread.
        unsafe { PostQuitMessage(0) };
        return 0;
    }

    let original = lock_routes()
        .get(&(hwnd as usize))
        .map(|route| route.original);
    if let Some(original) = original {
        // SAFETY: the route stores the exact predecessor returned by the live
        // window's GWLP_WNDPROC slot.
        let procedure = unsafe { mem::transmute::<isize, WNDPROC>(original) };
        // SAFETY: this forwards the exact system-supplied message tuple.
        unsafe { CallWindowProcW(procedure, hwnd, message, wparam, lparam) }
    } else {
        // SAFETY: DefWindowProcW accepts this exact system-supplied tuple.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }
}

struct HiddenWindow {
    hwnd: usize,
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
    receiver: Receiver<RecordedMessage>,
}

impl HiddenWindow {
    fn create() -> Self {
        let (message_sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let worker = match thread::Builder::new()
            .name(String::from("nexus-input-win32-test-window"))
            .spawn(move || run_window(message_sender, ready_sender))
        {
            Ok(worker) => worker,
            Err(_) => panic!("hidden window thread could not start"),
        };
        let (hwnd, thread_id) = match ready_receiver.recv_timeout(RECEIVE_TIMEOUT) {
            Ok(Ok(ready)) => ready,
            Ok(Err(())) | Err(_) => {
                let _ = worker.join();
                panic!("hidden window could not be created");
            }
        };
        Self {
            hwnd,
            thread_id,
            worker: Some(worker),
            receiver,
        }
    }

    fn hwnd(&self) -> HWND {
        self.hwnd as HWND
    }

    fn receive(&self) -> RecordedMessage {
        match self.receiver.recv_timeout(RECEIVE_TIMEOUT) {
            Ok(message) => message,
            Err(_) => panic!("hidden window did not receive a posted message"),
        }
    }

    fn assert_no_message(&self) {
        assert!(
            self.receiver
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "an unexpected message reached the hidden window"
        );
    }

    fn close(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        // SAFETY: PostMessageW copies this scalar request for the test window.
        let posted = unsafe { PostMessageW(self.hwnd(), WM_CLOSE, 0, 0) };
        if posted == FALSE {
            // SAFETY: this thread id belongs to the stored message-pump worker.
            let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        }
        if worker.join().is_err() {
            panic!("hidden window thread panicked");
        }
        self.hwnd = 0;
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        self.close();
    }
}

fn run_window(
    message_sender: Sender<RecordedMessage>,
    ready: SyncSender<Result<(usize, u32), ()>>,
) {
    let class = "STATIC\0".encode_utf16().collect::<Vec<_>>();
    // SAFETY: the class string is terminated, optional handles are null, and
    // the hidden zero-sized system-class window is owned by this thread.
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
    if hwnd.is_null() {
        let _ = ready.send(Err(()));
        return;
    }

    // SAFETY: this thread owns the live hidden window.
    let original = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
    if original == 0 {
        // SAFETY: this thread owns the live hidden window.
        let _ = unsafe { DestroyWindow(hwnd) };
        let _ = ready.send(Err(()));
        return;
    }
    // SAFETY: recording_wnd_proc has the exact WNDPROC ABI.
    let replaced =
        unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, recording_wnd_proc as *const () as isize) };
    if replaced != original {
        // SAFETY: this thread owns the live hidden window.
        let _ = unsafe { DestroyWindow(hwnd) };
        let _ = ready.send(Err(()));
        return;
    }

    lock_routes().insert(
        hwnd as usize,
        WindowRoute {
            original,
            sender: message_sender,
        },
    );
    // SAFETY: GetCurrentThreadId has no preconditions.
    let thread_id = unsafe { GetCurrentThreadId() };
    if ready.send(Ok((hwnd as usize, thread_id))).is_err() {
        lock_routes().remove(&(hwnd as usize));
        // SAFETY: this thread owns the live hidden window.
        let _ = unsafe { DestroyWindow(hwnd) };
        return;
    }

    let mut message = MSG::default();
    loop {
        // SAFETY: message is valid writable storage and a null HWND selects
        // this thread's full queue.
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        // SAFETY: both calls receive the exact initialized message from GetMessageW.
        let _ = unsafe { TranslateMessage(&message) };
        // SAFETY: DispatchMessageW receives the exact initialized message.
        let _ = unsafe { DispatchMessageW(&message) };
    }

    lock_routes().remove(&(hwnd as usize));
    // SAFETY: IsWindow accepts an arbitrary token.
    if unsafe { IsWindow(hwnd) } != FALSE {
        // SAFETY: this thread still owns the live hidden window.
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}

#[test]
fn key_and_mouse_encoders_match_the_legacy_bit_layout() {
    assert_eq!(wire_message_id(WM_KEYDOWN), PASSTHROUGH_FIRST + WM_KEYDOWN);
    assert_eq!(wire_message_id(WM_USER), WM_USER);
    assert_eq!(key_message_id(true, false), WM_KEYDOWN);
    assert_eq!(key_message_id(false, false), WM_KEYUP);
    assert_eq!(key_message_id(true, true), WM_SYSKEYDOWN);
    assert_eq!(key_message_id(false, true), WM_SYSKEYUP);

    assert_eq!(key_lparam(0x001e, true, false), 1 | (0x1e << 16));
    assert_eq!(
        key_lparam(0xe04d, true, true),
        1 | (0x4d << 16) | (1 << 24) | (1 << 29)
    );
    assert_eq!(
        key_lparam(0xe04d, false, true),
        (1_u32 | (0x4d << 16) | (1 << 24) | (1 << 29) | (1 << 30) | (1 << 31)) as isize
    );

    let modifiers = ModifierState {
        alt: true,
        control: true,
        shift: true,
    };
    assert_eq!(
        mouse_wparam(MouseButton::Left, true, modifiers),
        Some((MK_LBUTTON | MK_CONTROL | MK_SHIFT) as usize)
    );
    assert_eq!(
        mouse_wparam(MouseButton::X1, true, modifiers),
        Some(((1_u32 << 16) | MK_XBUTTON1 | MK_CONTROL | MK_SHIFT) as usize)
    );
    assert_eq!(
        mouse_wparam(MouseButton::X1, false, modifiers),
        Some(((1_u32 << 16) | MK_CONTROL | MK_SHIFT) as usize)
    );
    assert_eq!(point_lparam(-1, -2), 0xfffe_ffff_u32 as isize);
}

#[test]
fn async_key_poll_ignores_the_legacy_low_order_ambiguity() {
    assert!(!async_key_is_down(0));
    assert!(!async_key_is_down(1));
    assert!(async_key_is_down(i16::MIN));
    assert!(async_key_is_down(0x8001_u16 as i16));
}

#[test]
fn adapter_traits_and_debug_output_are_address_free() {
    fn assert_traits<
        T: GameMessageSink + GameOnlyMessageSink + PhysicalInputState + Send + Sync,
    >() {
    }
    assert_traits::<Win32GameInput>();

    let adapter = Arc::new(Win32GameInput::new());
    assert_eq!(format!("{adapter:?}"), "Win32GameInput { attached: false }");
    match adapter.attach(ptr::null_mut()) {
        Err(WindowAttachError::InvalidWindow) => {}
        Ok(_) | Err(_) => panic!("a null HWND must be rejected"),
    }
}

#[test]
fn game_only_messages_use_the_attached_window_and_legacy_wire_translation() {
    let window = HiddenWindow::create();
    let adapter = Arc::new(Win32GameInput::new());
    let _lease = adapter
        .attach(window.hwnd())
        .unwrap_or_else(|_| panic!("hidden window attachment failed"));

    adapter
        .send_to_game_only(17, 0x1234, -7)
        .expect("low message should post");
    assert_eq!(
        window.receive(),
        RecordedMessage {
            message: PASSTHROUGH_FIRST + 17,
            wparam: 0x1234,
            lparam: -7,
        }
    );

    let high_message = PASSTHROUGH_FIRST + WM_USER - 1;
    adapter
        .send_to_game_only(high_message, 0x5678, -9)
        .expect("high message should post unchanged");
    assert_eq!(
        window.receive(),
        RecordedMessage {
            message: high_message,
            wparam: 0x5678,
            lparam: -9,
        }
    );
}

#[test]
fn concurrent_batches_remain_contiguous_and_ordered() {
    let window = HiddenWindow::create();
    let adapter = Arc::new(Win32GameInput::new());
    let lease = match adapter.attach(window.hwnd()) {
        Ok(lease) => lease,
        Err(_) => panic!("hidden window attachment failed"),
    };
    let barrier = Arc::new(Barrier::new(3));

    let first_adapter = Arc::clone(&adapter);
    let first_barrier = Arc::clone(&barrier);
    let first = thread::spawn(move || {
        first_barrier.wait();
        first_adapter.send_batch(&[
            GameMessage::Keyboard {
                scan_code: 0x001e,
                pressed: true,
                system: false,
            },
            GameMessage::Keyboard {
                scan_code: 0x001e,
                pressed: false,
                system: false,
            },
        ])
    });
    let second_adapter = Arc::clone(&adapter);
    let second_barrier = Arc::clone(&barrier);
    let second = thread::spawn(move || {
        second_barrier.wait();
        second_adapter.send_batch(&[
            GameMessage::Keyboard {
                scan_code: 0x0030,
                pressed: true,
                system: false,
            },
            GameMessage::Keyboard {
                scan_code: 0x0030,
                pressed: false,
                system: false,
            },
        ])
    });
    barrier.wait();

    for worker in [first, second] {
        match worker.join() {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => panic!("concurrent batch dispatch failed"),
        }
    }

    let received = [
        window.receive(),
        window.receive(),
        window.receive(),
        window.receive(),
    ];
    let virtual_keys = received.map(|message| message.wparam);
    let a_then_b = [0x41, 0x41, 0x42, 0x42];
    let b_then_a = [0x42, 0x42, 0x41, 0x41];
    assert!(
        virtual_keys == a_then_b || virtual_keys == b_then_a,
        "two batches interleaved"
    );
    for pair in received.chunks_exact(2) {
        assert_eq!(pair[0].message, PASSTHROUGH_FIRST + WM_KEYDOWN);
        assert_eq!(pair[1].message, PASSTHROUGH_FIRST + WM_KEYUP);
    }
    drop(lease);
}

#[test]
fn game_invoker_press_release_matches_modifier_and_restore_order() {
    let window = HiddenWindow::create();
    let adapter = Arc::new(Win32GameInput::new());
    let lease = match adapter.attach(window.hwnd()) {
        Ok(lease) => lease,
        Err(_) => panic!("hidden window attachment failed"),
    };
    let mut registry = GameBindRegistry::default();
    assert!(registry.set(
        GameBindId::SKILL_WEAPON_1,
        GameSlot::Primary,
        InputBind::new(true, true, false, InputDevice::KEYBOARD, 0xe04d,),
    ));
    let sink: Arc<dyn GameMessageSink> = adapter.clone();
    let physical: Arc<dyn PhysicalInputState> = Arc::new(ReleasedPhysicalModifiers);
    let mut invoker = GameInvoker::new(registry, sink, physical);

    assert!(
        invoker.press(GameBindId::SKILL_WEAPON_1).is_ok(),
        "game invoker press failed"
    );
    assert!(
        invoker.release(GameBindId::SKILL_WEAPON_1).is_ok(),
        "game invoker release failed"
    );
    let received = [
        window.receive(),
        window.receive(),
        window.receive(),
        window.receive(),
        window.receive(),
        window.receive(),
        window.receive(),
    ];
    assert_eq!(
        received.map(|message| message.message),
        [
            PASSTHROUGH_FIRST + WM_SYSKEYDOWN,
            PASSTHROUGH_FIRST + WM_KEYDOWN,
            PASSTHROUGH_FIRST + WM_SYSKEYDOWN,
            PASSTHROUGH_FIRST + WM_SYSKEYUP,
            PASSTHROUGH_FIRST + WM_SYSKEYUP,
            PASSTHROUGH_FIRST + WM_KEYUP,
            PASSTHROUGH_FIRST + WM_KEYUP,
        ]
    );
    assert_eq!(
        received.map(|message| message.wparam),
        [
            usize::from(VK_MENU),
            0x11,
            usize::from(VK_RIGHT),
            usize::from(VK_RIGHT),
            usize::from(VK_MENU),
            0x11,
            0x10,
        ]
    );
    assert_eq!(received[2].lparam, key_lparam(0xe04d, true, true));
    assert_eq!(received[3].lparam, key_lparam(0xe04d, false, true));
    drop(lease);
}

#[test]
fn stale_lease_cannot_detach_a_replacement_and_explicit_detach_is_final() {
    let first_window = HiddenWindow::create();
    let second_window = HiddenWindow::create();
    let adapter = Arc::new(Win32GameInput::new());
    let first_lease = match adapter.attach(first_window.hwnd()) {
        Ok(lease) => lease,
        Err(_) => panic!("first hidden window attachment failed"),
    };
    let second_lease = match adapter.attach(second_window.hwnd()) {
        Ok(lease) => lease,
        Err(_) => panic!("replacement hidden window attachment failed"),
    };
    assert!(!first_lease.is_current());
    assert!(second_lease.is_current());
    drop(first_lease);

    let message = [GameMessage::Keyboard {
        scan_code: 0x002e,
        pressed: true,
        system: false,
    }];
    assert!(adapter.send_batch(&message).is_ok());
    assert_eq!(
        second_window.receive().message,
        PASSTHROUGH_FIRST + WM_KEYDOWN
    );
    first_window.assert_no_message();

    second_lease.detach();
    assert!(!adapter.is_attached());
    assert_eq!(adapter.send_batch(&message), Err(GameSinkError));
}

#[test]
fn destroyed_window_failure_recovers_with_extended_system_keys() {
    let mut failed_window = HiddenWindow::create();
    let adapter = Arc::new(Win32GameInput::new());
    let stale_lease = match adapter.attach(failed_window.hwnd()) {
        Ok(lease) => lease,
        Err(_) => panic!("initial hidden window attachment failed"),
    };
    failed_window.close();

    let probe = [GameMessage::Keyboard {
        scan_code: 0x001e,
        pressed: true,
        system: false,
    }];
    assert_eq!(adapter.send_batch(&probe), Err(GameSinkError));
    assert!(!adapter.is_attached());

    let recovered_window = HiddenWindow::create();
    let recovered_lease = match adapter.attach(recovered_window.hwnd()) {
        Ok(lease) => lease,
        Err(_) => panic!("replacement hidden window attachment failed"),
    };
    drop(stale_lease);
    assert!(recovered_lease.is_current());

    let messages = [
        GameMessage::Modifier {
            modifier: Modifier::Alt,
            pressed: true,
            system: true,
        },
        GameMessage::Keyboard {
            scan_code: 0xe04d,
            pressed: true,
            system: true,
        },
        GameMessage::Keyboard {
            scan_code: 0xe04d,
            pressed: false,
            system: true,
        },
    ];
    assert!(adapter.send_batch(&messages).is_ok());
    let alt = recovered_window.receive();
    let down = recovered_window.receive();
    let up = recovered_window.receive();
    assert_eq!(alt.message, PASSTHROUGH_FIRST + WM_SYSKEYDOWN);
    assert_eq!(alt.wparam, usize::from(VK_MENU));
    assert_eq!(down.message, PASSTHROUGH_FIRST + WM_SYSKEYDOWN);
    assert_eq!(down.wparam, usize::from(VK_RIGHT));
    assert_eq!(down.lparam, key_lparam(0xe04d, true, true));
    assert_eq!(up.message, PASSTHROUGH_FIRST + WM_SYSKEYUP);
    assert_eq!(up.wparam, usize::from(VK_RIGHT));
    assert_eq!(up.lparam, key_lparam(0xe04d, false, true));
    drop(recovered_lease);
}

#[test]
fn real_mouse_and_xbutton_messages_use_current_cursor_coordinates() {
    let window = HiddenWindow::create();
    let adapter = Arc::new(Win32GameInput::new());
    let lease = match adapter.attach(window.hwnd()) {
        Ok(lease) => lease,
        Err(_) => panic!("hidden window attachment failed"),
    };
    let before = cursor_point();
    let modifiers = ModifierState {
        alt: true,
        control: true,
        shift: true,
    };
    let messages = [
        GameMessage::Mouse {
            button: MouseButton::Left,
            pressed: true,
            modifiers,
        },
        GameMessage::Mouse {
            button: MouseButton::X1,
            pressed: true,
            modifiers,
        },
        GameMessage::Mouse {
            button: MouseButton::X1,
            pressed: false,
            modifiers,
        },
    ];
    assert!(adapter.send_batch(&messages).is_ok());
    let left = window.receive();
    let xdown = window.receive();
    let xup = window.receive();
    let after = cursor_point();

    assert_eq!(left.message, PASSTHROUGH_FIRST + WM_LBUTTONDOWN);
    assert_eq!(left.wparam, (MK_LBUTTON | MK_CONTROL | MK_SHIFT) as usize);
    assert_eq!(xdown.message, PASSTHROUGH_FIRST + WM_XBUTTONDOWN);
    assert_eq!(
        xdown.wparam,
        ((1_u32 << 16) | MK_XBUTTON1 | MK_CONTROL | MK_SHIFT) as usize
    );
    assert_eq!(xup.message, PASSTHROUGH_FIRST + WM_XBUTTONUP);
    assert_eq!(xup.wparam, ((1_u32 << 16) | MK_CONTROL | MK_SHIFT) as usize);

    for message in [left, xdown, xup] {
        let (x, y) = decode_point(message.lparam);
        if before.x.abs_diff(after.x) <= 32 && before.y.abs_diff(after.y) <= 32 {
            assert!(within(x, before.x, after.x));
            assert!(within(y, before.y, after.y));
        }
    }
    drop(lease);
}

fn cursor_point() -> POINT {
    let mut point = POINT::default();
    // SAFETY: point is valid writable storage.
    let sampled = unsafe { GetCursorPos(&mut point) };
    assert_ne!(sampled, FALSE, "cursor position could not be sampled");
    point
}

fn decode_point(lparam: LPARAM) -> (i32, i32) {
    let bits = lparam as u32;
    (
        (bits as u16) as i16 as i32,
        ((bits >> 16) as u16) as i16 as i32,
    )
}

fn within(value: i32, first: i32, second: i32) -> bool {
    let low = first.min(second).saturating_sub(2);
    let high = first.max(second).saturating_add(2);
    (low..=high).contains(&value)
}
