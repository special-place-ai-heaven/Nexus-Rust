use nexus_input::{Modifier, ModifierState, MouseButton};
use windows_sys::Win32::System::SystemServices::{
    MK_CONTROL, MK_LBUTTON, MK_MBUTTON, MK_RBUTTON, MK_SHIFT, MK_XBUTTON1, MK_XBUTTON2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN,
    WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

pub(crate) const PASSTHROUGH_FIRST: u32 = WM_USER + 7997;

pub(crate) const fn wire_message_id(message: u32) -> u32 {
    if message < WM_USER {
        message + PASSTHROUGH_FIRST
    } else {
        message
    }
}

pub(crate) const fn key_message_id(pressed: bool, system: bool) -> u32 {
    match (pressed, system) {
        (true, true) => WM_SYSKEYDOWN,
        (false, true) => WM_SYSKEYUP,
        (true, false) => WM_KEYDOWN,
        (false, false) => WM_KEYUP,
    }
}

pub(crate) const fn key_lparam(scan_code: u16, pressed: bool, system: bool) -> isize {
    let mut bits = 1_u32 | (((scan_code & 0x00ff) as u32) << 16);
    let prefix = scan_code >> 8;
    if prefix == 0x00e0 || prefix == 0x00e1 {
        bits |= 1 << 24;
    }
    if system {
        bits |= 1 << 29;
    }
    if !pressed {
        bits |= (1 << 30) | (1 << 31);
    }
    bits as isize
}

pub(crate) const fn modifier_virtual_key(modifier: Modifier) -> u32 {
    match modifier {
        Modifier::Alt => 0x12,
        Modifier::Control => 0x11,
        Modifier::Shift => 0x10,
    }
}

pub(crate) const fn mouse_message_id(button: MouseButton, pressed: bool) -> Option<u32> {
    match (button, pressed) {
        (MouseButton::Left, true) => Some(WM_LBUTTONDOWN),
        (MouseButton::Left, false) => Some(WM_LBUTTONUP),
        (MouseButton::Right, true) => Some(WM_RBUTTONDOWN),
        (MouseButton::Right, false) => Some(WM_RBUTTONUP),
        (MouseButton::Middle, true) => Some(WM_MBUTTONDOWN),
        (MouseButton::Middle, false) => Some(WM_MBUTTONUP),
        (MouseButton::X1 | MouseButton::X2, true) => Some(WM_XBUTTONDOWN),
        (MouseButton::X1 | MouseButton::X2, false) => Some(WM_XBUTTONUP),
        (MouseButton::None, _) => None,
    }
}

pub(crate) const fn mouse_wparam(
    button: MouseButton,
    pressed: bool,
    modifiers: ModifierState,
) -> Option<usize> {
    let (button_state, xbutton) = match button {
        MouseButton::Left => (MK_LBUTTON, 0),
        MouseButton::Right => (MK_RBUTTON, 0),
        MouseButton::Middle => (MK_MBUTTON, 0),
        MouseButton::X1 => (MK_XBUTTON1, XBUTTON1 as u32),
        MouseButton::X2 => (MK_XBUTTON2, XBUTTON2 as u32),
        MouseButton::None => return None,
    };
    let low_word = (if modifiers.control { MK_CONTROL } else { 0 })
        | (if modifiers.shift { MK_SHIFT } else { 0 })
        | (if pressed { button_state } else { 0 });
    Some(((xbutton << 16) | low_word) as usize)
}

pub(crate) const fn point_lparam(x: i32, y: i32) -> isize {
    let low_word = x as u16;
    let high_word = y as u16;
    (((high_word as u32) << 16) | low_word as u32) as isize
}

pub(crate) const fn async_key_is_down(state: i16) -> bool {
    (state as u16 & 0x8000) != 0
}
