use std::borrow::Cow;
use std::fmt;

use thiserror::Error;

/// Open numeric input-device identifier used by the Nexus v2 ABI.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct InputDevice(pub u32);

impl InputDevice {
    /// No input device.
    pub const NONE: Self = Self(0);
    /// Keyboard scan code.
    pub const KEYBOARD: Self = Self(1);
    /// Mouse button.
    pub const MOUSE: Self = Self(2);
}

impl fmt::Debug for InputDevice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::NONE => formatter.write_str("None"),
            Self::KEYBOARD => formatter.write_str("Keyboard"),
            Self::MOUSE => formatter.write_str("Mouse"),
            Self(value) => formatter.debug_tuple("Other").field(&value).finish(),
        }
    }
}

/// Nexus mouse-button codes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum MouseButton {
    /// No button.
    #[default]
    None = 0,
    /// Left button.
    Left = 1,
    /// Right button.
    Right = 2,
    /// Middle button.
    Middle = 3,
    /// First extended button.
    X1 = 4,
    /// Second extended button.
    X2 = 5,
}

impl MouseButton {
    /// Converts a Nexus mouse code into a known button.
    #[must_use]
    pub const fn from_code(code: u16) -> Option<Self> {
        match code {
            0 => Some(Self::None),
            1 => Some(Self::Left),
            2 => Some(Self::Right),
            3 => Some(Self::Middle),
            4 => Some(Self::X1),
            5 => Some(Self::X2),
            _ => None,
        }
    }
}

/// A modifier key tracked by capture and held-bind release logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Modifier {
    /// Alt/menu modifier.
    Alt,
    /// Control modifier.
    Control,
    /// Shift modifier.
    Shift,
}

/// Physical or desired modifier state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ModifierState {
    /// Alt is held.
    pub alt: bool,
    /// Control is held.
    pub control: bool,
    /// Shift is held.
    pub shift: bool,
}

impl ModifierState {
    /// Returns the state of one modifier.
    #[must_use]
    pub const fn get(self, modifier: Modifier) -> bool {
        match modifier {
            Modifier::Alt => self.alt,
            Modifier::Control => self.control,
            Modifier::Shift => self.shift,
        }
    }

    pub(crate) fn set(&mut self, modifier: Modifier, value: bool) {
        match modifier {
            Modifier::Alt => self.alt = value,
            Modifier::Control => self.control = value,
            Modifier::Shift => self.shift = value,
        }
    }
}

/// Nexus v2 input binding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(C)]
pub struct InputBind {
    /// Required Alt modifier.
    pub alt: bool,
    /// Required Control modifier.
    pub control: bool,
    /// Required Shift modifier.
    pub shift: bool,
    /// Input device identifier.
    pub device: InputDevice,
    /// Keyboard scan code or Nexus mouse-button code.
    pub code: u16,
}

impl InputBind {
    /// Constructs a binding.
    #[must_use]
    pub const fn new(
        alt: bool,
        control: bool,
        shift: bool,
        device: InputDevice,
        code: u16,
    ) -> Self {
        Self {
            alt,
            control,
            shift,
            device,
            code,
        }
    }

    /// Returns whether both a device and nonzero code are set.
    #[must_use]
    pub const fn is_bound(self) -> bool {
        self.device.0 != InputDevice::NONE.0 && self.code != 0
    }

    /// Returns the required modifier state.
    #[must_use]
    pub const fn modifiers(self) -> ModifierState {
        ModifierState {
            alt: self.alt,
            control: self.control,
            shift: self.shift,
        }
    }

    /// Produces the canonical unbound representation used by persistence.
    #[must_use]
    pub const fn normalized(self) -> Self {
        if self.code == 0 {
            Self::new(false, false, false, InputDevice::NONE, 0)
        } else {
            self
        }
    }

    /// Returns whether releasing `modifier` must release this binding.
    #[must_use]
    pub const fn requires(self, modifier: Modifier) -> bool {
        match modifier {
            Modifier::Alt => self.alt,
            Modifier::Control => self.control,
            Modifier::Shift => self.shift,
        }
    }
}

/// Legacy Nexus v1 keyboard-only binding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct LegacyInputBind {
    /// Keyboard scan code.
    pub key: u16,
    /// Required Alt modifier.
    pub alt: bool,
    /// Required Control modifier.
    pub control: bool,
    /// Required Shift modifier.
    pub shift: bool,
}

impl From<LegacyInputBind> for InputBind {
    fn from(value: LegacyInputBind) -> Self {
        Self::new(
            value.alt,
            value.control,
            value.shift,
            InputDevice::KEYBOARD,
            value.key,
        )
    }
}

/// Resolves localized keyboard names without coupling the core to Win32.
pub trait KeyNameResolver {
    /// Resolves an uppercase key name to a Windows scan code.
    fn scan_code(&self, uppercase_name: &str) -> Option<u16>;

    /// Returns a display name for a Windows scan code.
    fn key_name(&self, scan_code: u16) -> Option<Cow<'static, str>>;
}

/// Stable US-English key names used when no platform resolver is supplied.
#[derive(Debug, Clone, Copy, Default)]
pub struct UsKeyNames;

impl KeyNameResolver for UsKeyNames {
    fn scan_code(&self, uppercase_name: &str) -> Option<u16> {
        portable_scan_code(uppercase_name)
    }

    fn key_name(&self, scan_code: u16) -> Option<Cow<'static, str>> {
        portable_key_name(scan_code).map(Cow::Borrowed)
    }
}

/// A strict binding-string parse failure.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum BindParseError {
    /// The input exceeds the defensive parser limit.
    #[error("input binding string is too long")]
    TooLong,
    /// The final key name is empty or unknown.
    #[error("input binding key is unknown")]
    UnknownKey,
    /// A modifier-like token appears in an invalid position.
    #[error("input binding syntax is invalid")]
    InvalidSyntax,
}

/// Parses the legacy `ALT+CTRL+SHIFT+KEY` syntax.
pub fn parse_bind(
    input: &str,
    resolver: &dyn KeyNameResolver,
) -> Result<InputBind, BindParseError> {
    if input.len() > 256 {
        return Err(BindParseError::TooLong);
    }
    if input.eq_ignore_ascii_case("(null)") {
        return Ok(InputBind::default());
    }

    let mut parts = input.split('+').map(str::trim).peekable();
    let mut modifiers = ModifierState::default();
    let mut key = None;
    while let Some(part) = parts.next() {
        let upper = part.to_ascii_uppercase();
        let is_last = parts.peek().is_none();
        match upper.as_str() {
            "ALT" if !is_last => modifiers.alt = true,
            "CTRL" | "CONTROL" if !is_last => modifiers.control = true,
            "SHIFT" if !is_last => modifiers.shift = true,
            _ if is_last && !upper.is_empty() => key = Some(upper),
            _ => return Err(BindParseError::InvalidSyntax),
        }
    }
    let key = key.ok_or(BindParseError::UnknownKey)?;
    let (device, code) = match key.as_str() {
        "LMB" => (InputDevice::MOUSE, MouseButton::Left as u16),
        "RMB" => (InputDevice::MOUSE, MouseButton::Right as u16),
        "MMB" => (InputDevice::MOUSE, MouseButton::Middle as u16),
        "M4" => (InputDevice::MOUSE, MouseButton::X1 as u16),
        "M5" => (InputDevice::MOUSE, MouseButton::X2 as u16),
        _ => {
            let code = resolver
                .scan_code(&key)
                .or_else(|| parse_scan_code_literal(&key))
                .ok_or(BindParseError::UnknownKey)?;
            (InputDevice::KEYBOARD, code)
        }
    };
    Ok(InputBind::new(
        modifiers.alt,
        modifiers.control,
        modifiers.shift,
        device,
        code,
    ))
}

/// Parses legacy syntax while preserving the old API's unknown-key-to-unbound behavior.
#[must_use]
pub fn parse_bind_lossy(input: &str, resolver: &dyn KeyNameResolver) -> InputBind {
    parse_bind(input, resolver).unwrap_or_default()
}

/// Formats a binding using the legacy modifier order.
#[must_use]
pub fn format_bind(bind: InputBind, resolver: &dyn KeyNameResolver, padded: bool) -> String {
    if !bind.is_bound() {
        return "(null)".to_owned();
    }
    let separator = if padded { " + " } else { "+" };
    let mut parts = Vec::with_capacity(4);
    if bind.alt {
        parts.push("ALT".to_owned());
    }
    if bind.control {
        parts.push("CTRL".to_owned());
    }
    if bind.shift {
        parts.push("SHIFT".to_owned());
    }
    let key = if bind.device == InputDevice::MOUSE {
        match MouseButton::from_code(bind.code) {
            Some(MouseButton::Left) => "LMB".to_owned(),
            Some(MouseButton::Right) => "RMB".to_owned(),
            Some(MouseButton::Middle) => "MMB".to_owned(),
            Some(MouseButton::X1) => "M4".to_owned(),
            Some(MouseButton::X2) => "M5".to_owned(),
            _ => return "(null)".to_owned(),
        }
    } else if bind.device == InputDevice::KEYBOARD {
        resolver
            .key_name(bind.code)
            .map_or_else(|| format!("SC:{:04X}", bind.code), |name| name.into_owned())
    } else {
        return "(null)".to_owned();
    };
    parts.push(key.to_ascii_uppercase());
    parts.join(separator)
}

fn parse_scan_code_literal(value: &str) -> Option<u16> {
    let digits = value.strip_prefix("SC:")?;
    u16::from_str_radix(digits, 16)
        .ok()
        .filter(|code| *code != 0)
}

fn portable_scan_code(name: &str) -> Option<u16> {
    let code = match name {
        "ESC" | "ESCAPE" => 0x01,
        "1" => 0x02,
        "2" => 0x03,
        "3" => 0x04,
        "4" => 0x05,
        "5" => 0x06,
        "6" => 0x07,
        "7" => 0x08,
        "8" => 0x09,
        "9" => 0x0A,
        "0" => 0x0B,
        "-" | "MINUS" => 0x0C,
        "=" | "EQUALS" => 0x0D,
        "BACKSPACE" => 0x0E,
        "TAB" => 0x0F,
        "Q" => 0x10,
        "W" => 0x11,
        "E" => 0x12,
        "R" => 0x13,
        "T" => 0x14,
        "Y" => 0x15,
        "U" => 0x16,
        "I" => 0x17,
        "O" => 0x18,
        "P" => 0x19,
        "ENTER" | "RETURN" => 0x1C,
        "A" => 0x1E,
        "S" => 0x1F,
        "D" => 0x20,
        "F" => 0x21,
        "G" => 0x22,
        "H" => 0x23,
        "J" => 0x24,
        "K" => 0x25,
        "L" => 0x26,
        "Z" => 0x2C,
        "X" => 0x2D,
        "C" => 0x2E,
        "V" => 0x2F,
        "B" => 0x30,
        "N" => 0x31,
        "M" => 0x32,
        "SPACE" => 0x39,
        "F1" => 0x3B,
        "F2" => 0x3C,
        "F3" => 0x3D,
        "F4" => 0x3E,
        "F5" => 0x3F,
        "F6" => 0x40,
        "F7" => 0x41,
        "F8" => 0x42,
        "F9" => 0x43,
        "F10" => 0x44,
        "F11" => 0x57,
        "F12" => 0x58,
        "HOME" => 0xE047,
        "UP" | "ARROW UP" => 0xE048,
        "PAGE UP" => 0xE049,
        "LEFT" | "ARROW LEFT" => 0xE04B,
        "RIGHT" | "ARROW RIGHT" => 0xE04D,
        "END" => 0xE04F,
        "DOWN" | "ARROW DOWN" => 0xE050,
        "PAGE DOWN" => 0xE051,
        "INSERT" => 0xE052,
        "DELETE" => 0xE053,
        _ => return None,
    };
    Some(code)
}

fn portable_key_name(code: u16) -> Option<&'static str> {
    let name = match code {
        0x01 => "ESCAPE",
        0x02 => "1",
        0x03 => "2",
        0x04 => "3",
        0x05 => "4",
        0x06 => "5",
        0x07 => "6",
        0x08 => "7",
        0x09 => "8",
        0x0A => "9",
        0x0B => "0",
        0x0C => "MINUS",
        0x0D => "EQUALS",
        0x0E => "BACKSPACE",
        0x0F => "TAB",
        0x10 => "Q",
        0x11 => "W",
        0x12 => "E",
        0x13 => "R",
        0x14 => "T",
        0x15 => "Y",
        0x16 => "U",
        0x17 => "I",
        0x18 => "O",
        0x19 => "P",
        0x1C => "ENTER",
        0x1E => "A",
        0x1F => "S",
        0x20 => "D",
        0x21 => "F",
        0x22 => "G",
        0x23 => "H",
        0x24 => "J",
        0x25 => "K",
        0x26 => "L",
        0x2C => "Z",
        0x2D => "X",
        0x2E => "C",
        0x2F => "V",
        0x30 => "B",
        0x31 => "N",
        0x32 => "M",
        0x39 => "SPACE",
        0x3B => "F1",
        0x3C => "F2",
        0x3D => "F3",
        0x3E => "F4",
        0x3F => "F5",
        0x40 => "F6",
        0x41 => "F7",
        0x42 => "F8",
        0x43 => "F9",
        0x44 => "F10",
        0x57 => "F11",
        0x58 => "F12",
        0xE047 => "HOME",
        0xE048 => "UP",
        0xE049 => "PAGE UP",
        0xE04B => "LEFT",
        0xE04D => "RIGHT",
        0xE04F => "END",
        0xE050 => "DOWN",
        0xE051 => "PAGE DOWN",
        0xE052 => "INSERT",
        0xE053 => "DELETE",
        _ => return None,
    };
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_layout_and_conversion_match_v1() {
        assert_eq!(std::mem::size_of::<LegacyInputBind>(), 6);
        assert_eq!(std::mem::size_of::<InputBind>(), 12);
        assert_eq!(std::mem::offset_of!(InputBind, alt), 0);
        assert_eq!(std::mem::offset_of!(InputBind, control), 1);
        assert_eq!(std::mem::offset_of!(InputBind, shift), 2);
        assert_eq!(std::mem::offset_of!(InputBind, device), 4);
        assert_eq!(std::mem::offset_of!(InputBind, code), 8);
        let legacy = LegacyInputBind {
            key: 0x3B,
            alt: true,
            control: false,
            shift: true,
        };
        assert_eq!(
            InputBind::from(legacy),
            InputBind::new(true, false, true, InputDevice::KEYBOARD, 0x3B)
        );
    }

    #[test]
    fn syntax_round_trips_keyboard_and_mouse() {
        let keyboard = parse_bind("Ctrl + Shift + F1", &UsKeyNames)
            .expect("portable keyboard bind should parse");
        assert_eq!(format_bind(keyboard, &UsKeyNames, false), "CTRL+SHIFT+F1");
        let mouse = parse_bind("ALT+M4", &UsKeyNames).expect("mouse bind should parse");
        assert_eq!(mouse.device, InputDevice::MOUSE);
        assert_eq!(format_bind(mouse, &UsKeyNames, true), "ALT + M4");
    }

    #[test]
    fn unknown_keys_are_closed_and_lossy_api_is_compatible() {
        assert_eq!(
            parse_bind("CTRL+NOT-A-KEY", &UsKeyNames),
            Err(BindParseError::UnknownKey)
        );
        assert_eq!(
            parse_bind_lossy("CTRL+NOT-A-KEY", &UsKeyNames),
            InputBind::default()
        );
    }

    #[test]
    fn numeric_literal_preserves_extended_scan_code() {
        let bind =
            parse_bind("ALT+SC:E01D", &UsKeyNames).expect("numeric scan-code literal should parse");
        assert_eq!(bind.code, 0xE01D);
        assert_eq!(format_bind(bind, &UsKeyNames, false), "ALT+SC:E01D");
    }
}
