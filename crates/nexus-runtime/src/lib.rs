//! Nexus runtime DLL.
//!
//! The crate is named `d3d11` so Cargo emits the drop-in `d3d11.dll` expected
//! by Guild Wars 2. Public exports live in a narrow FFI module; all other code
//! uses typed Rust errors and ownership.

#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

#[cfg(target_os = "windows")]
mod cursor;
#[cfg(target_os = "windows")]
mod diagnostics;
#[cfg(target_os = "windows")]
mod dxgi;
#[cfg(target_os = "windows")]
mod fonts;
#[cfg(target_os = "windows")]
mod game_input;
#[cfg(target_os = "windows")]
mod input;
#[cfg(target_os = "windows")]
mod proxy;
mod runtime;
#[cfg(target_os = "windows")]
mod services;
#[cfg(target_os = "windows")]
mod textures;
#[cfg(target_os = "windows")]
mod ui;

pub use nexus_control::{ControlIssue, HookMode, RuntimeControls, SafeModeStage};
pub use runtime::{
    LifecyclePhase, ProxyFunction, control_issues, first_proxy_function, lifecycle_phase,
    request_shutdown, runtime_controls,
};
