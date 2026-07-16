//! Win32 delivery for the platform-neutral game-input state machine.
//!
//! The adapter preserves the legacy Nexus wire protocol: ordinary Win32 input
//! messages are posted in the private pass-through range beginning at
//! WM_USER + 7997, where the game hook translates them back before delivery.
//!
//! The legacy bit layout is retained: repeat count one, the low scan-code byte,
//! E0/E1 extended-key state, system context, release previous/transition bits,
//! mouse modifier/button flags, XBUTTON identity in the high word, and the
//! current cursor point.
//!
//! Deliberate stability corrections are explicit:
//!
//! - physical modifiers use only GetAsyncKeyState's high-order current-state
//!   bit instead of treating its ambiguous low-order bit as held state;
//! - a keyboard message marked as a system key uses WM_SYSKEYDOWN or
//!   WM_SYSKEYUP, rather than the C++ sender's mismatched ordinary key message
//!   with a system-context bit;
//! - invalid or destroyed windows and failed PostMessageW calls return closed
//!   errors instead of being silently ignored.

#[cfg(target_os = "windows")]
mod encoding;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::{GameWindowLease, Win32GameInput, WindowAttachError};

#[cfg(all(test, target_os = "windows"))]
mod tests;
