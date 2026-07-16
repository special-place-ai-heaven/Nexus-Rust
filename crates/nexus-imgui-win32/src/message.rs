use nexus_imgui_compat::sys;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DBT_DEVNODES_CHANGED, HTCLIENT, WM_CHAR, WM_DEVICECHANGE, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SETCURSOR, WM_SETFOCUS, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDBLCLK, WM_XBUTTONDOWN,
    WM_XBUTTONUP, XBUTTON1,
};

const WHEEL_DELTA: f32 = 120.0;
// `WM_MOUSELEAVE` is defined by the Windows SDK headers but omitted from the
// windows-sys 0.61.2 generated constants.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Translation result with an explicit, caller-controlled capture policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MessageOutcome {
    /// The platform backend recognized and observed the message.
    pub observed: bool,
    /// Dear ImGui requests capture of this mouse message.
    pub capture_mouse: bool,
    /// Dear ImGui requests capture of this keyboard/text message.
    pub capture_keyboard: bool,
    /// A device-change message requested a gamepad capability refresh.
    pub refresh_gamepad: bool,
    /// A client-area cursor message requested an immediate cursor update.
    pub refresh_cursor: bool,
}

impl MessageOutcome {
    /// Returns whether policy should suppress the message from the game.
    ///
    /// Capture is never automatic: hiding the overlay forces pass-through even
    /// if Dear ImGui's previous frame still requested input.
    #[must_use]
    pub const fn should_consume(self, overlay_visible: bool) -> bool {
        overlay_visible && (self.capture_mouse || self.capture_keyboard)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageClass {
    Mouse,
    Keyboard,
    Focus,
    Device,
    Cursor,
}

pub(crate) fn apply_message(
    io: &mut sys::ImGuiIO,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> MessageOutcome {
    let class = match message {
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK | WM_MBUTTONDOWN
        | WM_MBUTTONDBLCLK | WM_XBUTTONDOWN | WM_XBUTTONDBLCLK => {
            io.MouseDown[mouse_button(message, wparam)] = true;
            Some(MessageClass::Mouse)
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => {
            io.MouseDown[mouse_button(message, wparam)] = false;
            Some(MessageClass::Mouse)
        }
        WM_MOUSEWHEEL => {
            io.MouseWheel += f32::from(signed_high_word(wparam)) / WHEEL_DELTA;
            Some(MessageClass::Mouse)
        }
        WM_MOUSEHWHEEL => {
            io.MouseWheelH += f32::from(signed_high_word(wparam)) / WHEEL_DELTA;
            Some(MessageClass::Mouse)
        }
        WM_MOUSEMOVE => {
            io.MousePos = sys::ImVec2 {
                x: f32::from(signed_low_word_isize(lparam)),
                y: f32::from(signed_high_word_isize(lparam)),
            };
            Some(MessageClass::Mouse)
        }
        WM_MOUSELEAVE => {
            io.MousePos = sys::ImVec2 {
                x: -f32::MAX,
                y: -f32::MAX,
            };
            Some(MessageClass::Mouse)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if wparam < 256 {
                io.KeysDown[wparam] = true;
            }
            Some(MessageClass::Keyboard)
        }
        WM_KEYUP | WM_SYSKEYUP => {
            if wparam < 256 {
                io.KeysDown[wparam] = false;
            }
            Some(MessageClass::Keyboard)
        }
        WM_CHAR => {
            if (1..0x1_0000).contains(&wparam) {
                // SAFETY: `io` is the current live context's IO and the value
                // was restricted to one UTF-16 code unit.
                unsafe { sys::ImGuiIO_AddInputCharacterUTF16(io, wparam as u16) };
            }
            Some(MessageClass::Keyboard)
        }
        WM_KILLFOCUS => {
            clear_pressed_input(io);
            Some(MessageClass::Focus)
        }
        WM_SETFOCUS => Some(MessageClass::Focus),
        WM_DEVICECHANGE if wparam == DBT_DEVNODES_CHANGED as usize => Some(MessageClass::Device),
        WM_SETCURSOR if low_word_isize(lparam) == HTCLIENT as u16 => Some(MessageClass::Cursor),
        _ => None,
    };

    let Some(class) = class else {
        return MessageOutcome::default();
    };
    MessageOutcome {
        observed: true,
        capture_mouse: matches!(class, MessageClass::Mouse | MessageClass::Focus)
            && io.WantCaptureMouse,
        capture_keyboard: matches!(class, MessageClass::Keyboard | MessageClass::Focus)
            && io.WantCaptureKeyboard,
        refresh_gamepad: class == MessageClass::Device,
        refresh_cursor: class == MessageClass::Cursor,
    }
}

pub(crate) fn clear_pressed_input(io: &mut sys::ImGuiIO) {
    io.MouseDown.fill(false);
    io.KeysDown.fill(false);
    io.NavInputs.fill(0.0);
    io.MouseWheel = 0.0;
    io.MouseWheelH = 0.0;
    io.KeyCtrl = false;
    io.KeyShift = false;
    io.KeyAlt = false;
    io.KeySuper = false;
}

fn mouse_button(message: u32, wparam: usize) -> usize {
    match message {
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_LBUTTONUP => 0,
        WM_RBUTTONDOWN | WM_RBUTTONDBLCLK | WM_RBUTTONUP => 1,
        WM_MBUTTONDOWN | WM_MBUTTONDBLCLK | WM_MBUTTONUP => 2,
        _ if high_word(wparam) == XBUTTON1 => 3,
        _ => 4,
    }
}

fn high_word(value: usize) -> u16 {
    ((value >> 16) & usize::from(u16::MAX)) as u16
}

fn signed_high_word(value: usize) -> i16 {
    i16::from_ne_bytes(high_word(value).to_ne_bytes())
}

fn low_word_isize(value: isize) -> u16 {
    (value as usize & usize::from(u16::MAX)) as u16
}

fn signed_low_word_isize(value: isize) -> i16 {
    i16::from_ne_bytes(low_word_isize(value).to_ne_bytes())
}

fn signed_high_word_isize(value: isize) -> i16 {
    let word = ((value as usize >> 16) & usize::from(u16::MAX)) as u16;
    i16::from_ne_bytes(word.to_ne_bytes())
}

#[cfg(test)]
mod tests {
    use core::ptr;
    use std::sync::{Mutex, MutexGuard};

    use nexus_imgui_compat::sys;
    use nexus_imgui_runtime::ImGuiContextOwner;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WM_CHAR, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    };

    use super::apply_message;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_lock() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn with_io(operation: impl FnOnce(&mut sys::ImGuiIO)) {
        let mut owner =
            ImGuiContextOwner::create().unwrap_or_else(|error| panic!("context failed: {error}"));
        owner.with_current(|| {
            // SAFETY: the owner made its live context current.
            let io = unsafe { sys::igGetIO() };
            assert!(!io.is_null());
            // SAFETY: this serialized test holds the only owner borrow.
            operation(unsafe { &mut *io });
        });
    }

    #[test]
    fn capture_is_reported_but_remains_visibility_policy() {
        let _lock = test_lock();
        with_io(|io| {
            io.WantCaptureMouse = true;
            let outcome = apply_message(io, WM_LBUTTONDOWN, 0, 0);
            assert!(outcome.observed);
            assert!(outcome.capture_mouse);
            assert!(!outcome.should_consume(false));
            assert!(outcome.should_consume(true));
            assert!(io.MouseDown[0]);
        });
    }

    #[test]
    fn signed_mouse_coordinates_and_wheel_delta_are_preserved() {
        let _lock = test_lock();
        with_io(|io| {
            let x = u16::from_ne_bytes((-12_i16).to_ne_bytes());
            let y = u16::from_ne_bytes((-34_i16).to_ne_bytes());
            let lparam = isize::try_from((u32::from(y) << 16) | u32::from(x)).unwrap_or_default();
            let _ = apply_message(io, WM_MOUSEMOVE, 0, lparam);
            assert_eq!(io.MousePos.x, -12.0);
            assert_eq!(io.MousePos.y, -34.0);

            let negative_delta = u16::from_ne_bytes((-120_i16).to_ne_bytes());
            let wparam = usize::from(negative_delta) << 16;
            let _ = apply_message(io, WM_MOUSEWHEEL, wparam, 0);
            assert_eq!(io.MouseWheel, -1.0);
        });
    }

    #[test]
    fn focus_loss_clears_stuck_input() {
        let _lock = test_lock();
        with_io(|io| {
            let _ = apply_message(io, WM_LBUTTONDOWN, 0, 0);
            let _ = apply_message(io, WM_KEYDOWN, usize::from(b'A'), 0);
            io.KeyCtrl = true;
            let _ = apply_message(io, WM_KILLFOCUS, 0, 0);
            assert!(!io.MouseDown.iter().any(|pressed| *pressed));
            assert!(!io.KeysDown.iter().any(|pressed| *pressed));
            assert!(!io.KeyCtrl);
        });
    }

    #[test]
    fn utf16_character_is_queued() {
        let _lock = test_lock();
        with_io(|io| {
            assert_eq!(io.InputQueueCharacters.Size, 0);
            let outcome = apply_message(io, WM_CHAR, usize::from(b'Z'), 0);
            assert!(outcome.observed);
            assert_eq!(io.InputQueueCharacters.Size, 1);
            // Keep the imported pointer module exercised in this raw ABI test.
            assert!(!ptr::addr_of!(io.InputQueueCharacters).is_null());
        });
    }
}
