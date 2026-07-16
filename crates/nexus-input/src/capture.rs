use crate::{InputBind, InputDevice, Modifier, ModifierState, MouseButton};

/// Platform-neutral input event accepted by capture and managed routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMessage {
    /// Application activation changed. Both activation and deactivation clear held state.
    ActivateApp,
    /// A keyboard key was pressed.
    KeyDown {
        /// Virtual key when the key is a generic Alt, Control, or Shift modifier.
        modifier: Option<Modifier>,
        /// Full Windows scan code, including the `0xE000` extended prefix.
        scan_code: u16,
        /// Whether this is an autorepeat keydown.
        repeat: bool,
    },
    /// A keyboard key was released.
    KeyUp {
        /// Generic modifier identity when applicable.
        modifier: Option<Modifier>,
        /// Full Windows scan code.
        scan_code: u16,
    },
    /// A mouse button was pressed.
    MouseDown(MouseButton),
    /// A mouse button was released.
    MouseUp(MouseButton),
}

/// Result of capture preprocessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureOutcome {
    /// Current captured combination.
    pub binding: InputBind,
    /// Whether capture mode consumes this event.
    pub consumed: bool,
    /// Whether this event is an eligible managed-bind press.
    pub press_candidate: bool,
}

/// Stateful Nexus-compatible input capture.
#[derive(Debug, Default, Clone)]
pub struct InputCapture {
    capturing: bool,
    binding: InputBind,
    modifiers: ModifierState,
}

impl InputCapture {
    /// Starts capture and clears the previous result. Calling twice is idempotent.
    pub fn start(&mut self) {
        if !self.capturing {
            self.binding = InputBind::default();
            self.capturing = true;
        }
    }

    /// Stops capture without clearing the captured result.
    pub fn stop(&mut self) {
        self.capturing = false;
    }

    /// Returns whether capture is active.
    #[must_use]
    pub const fn is_capturing(&self) -> bool {
        self.capturing
    }

    /// Returns the latest captured binding.
    #[must_use]
    pub const fn binding(&self) -> InputBind {
        self.binding
    }

    /// Preprocesses one event using the legacy ordering.
    pub fn process(&mut self, message: InputMessage) -> CaptureOutcome {
        if matches!(message, InputMessage::ActivateApp) {
            self.modifiers = ModifierState::default();
            self.binding.alt = false;
            self.binding.control = false;
            self.binding.shift = false;
            return self.outcome(false, false);
        }

        match message {
            InputMessage::KeyDown {
                modifier: Some(modifier),
                repeat: false,
                ..
            } => self.modifiers.set(modifier, true),
            InputMessage::KeyUp {
                modifier: Some(modifier),
                ..
            } => self.modifiers.set(modifier, false),
            _ => {}
        }

        match message {
            InputMessage::KeyDown {
                modifier,
                scan_code,
                repeat,
            } => {
                if repeat {
                    return self.outcome(false, false);
                }
                let required = if modifier.is_some() {
                    ModifierState::default()
                } else {
                    self.modifiers
                };
                self.binding = InputBind::new(
                    required.alt,
                    required.control,
                    required.shift,
                    InputDevice::KEYBOARD,
                    scan_code,
                );
                self.outcome(self.capturing, true)
            }
            InputMessage::MouseDown(
                button @ (MouseButton::Middle | MouseButton::X1 | MouseButton::X2),
            ) => {
                self.binding = InputBind::new(
                    self.modifiers.alt,
                    self.modifiers.control,
                    self.modifiers.shift,
                    InputDevice::MOUSE,
                    button as u16,
                );
                self.outcome(self.capturing, true)
            }
            _ => self.outcome(false, false),
        }
    }

    const fn outcome(&self, consumed: bool, press_candidate: bool) -> CaptureOutcome {
        CaptureOutcome {
            binding: self.binding,
            consumed,
            press_candidate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_tracks_modifiers_before_building_non_modifier_bind() {
        let mut capture = InputCapture::default();
        capture.start();
        capture.process(InputMessage::KeyDown {
            modifier: Some(Modifier::Control),
            scan_code: 0x1D,
            repeat: false,
        });
        let outcome = capture.process(InputMessage::KeyDown {
            modifier: None,
            scan_code: 0x3B,
            repeat: false,
        });
        assert!(outcome.consumed);
        assert_eq!(
            outcome.binding,
            InputBind::new(false, true, false, InputDevice::KEYBOARD, 0x3B)
        );
    }

    #[test]
    fn standalone_modifier_does_not_include_other_modifiers() {
        let mut capture = InputCapture::default();
        capture.process(InputMessage::KeyDown {
            modifier: Some(Modifier::Shift),
            scan_code: 0x2A,
            repeat: false,
        });
        let outcome = capture.process(InputMessage::KeyDown {
            modifier: Some(Modifier::Control),
            scan_code: 0x1D,
            repeat: false,
        });
        assert_eq!(
            outcome.binding,
            InputBind::new(false, false, false, InputDevice::KEYBOARD, 0x1D)
        );
    }

    #[test]
    fn legacy_capture_ignores_left_and_right_mouse_buttons() {
        let mut capture = InputCapture::default();
        capture.start();
        let outcome = capture.process(InputMessage::MouseDown(MouseButton::Left));
        assert!(!outcome.consumed);
        assert!(!outcome.press_candidate);
        assert_eq!(outcome.binding, InputBind::default());
    }
}
