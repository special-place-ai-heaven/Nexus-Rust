//! Win32 platform input for the classifier-selected Nexus primary window.
//!
//! This crate deliberately does not discover windows, subclass `WndProc`, or
//! replace its bound `HWND` in response to arbitrary messages. The DXGI layer
//! selects a primary swap chain; runtime assembly explicitly binds that one
//! window here. Message translation reports capture intent but leaves the
//! final swallow/pass-through policy under Nexus control.

#![cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]

#[cfg(not(all(windows, target_arch = "x86_64")))]
compile_error!("nexus-imgui-win32 supports only 64-bit Windows targets");

mod gamepad;
mod message;
mod platform;

pub use message::MessageOutcome;
pub use platform::{PlatformError, Win32Platform};
