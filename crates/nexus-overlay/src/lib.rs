//! Safe assembly of the DXGI callback, Win32 input, and Dear ImGui D3D11 layers.
//!
//! The public adapter is `Send + Sync`, while every native renderer and platform
//! object remains in thread-local storage on the first successful render thread.
//! A callback from any other thread fails closed; no native object is moved and
//! no `unsafe impl Send` or `unsafe impl Sync` is used.

#![cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]

#[cfg(not(all(windows, target_arch = "x86_64")))]
compile_error!("nexus-overlay supports only 64-bit Windows targets");

mod adapter;
mod affinity;
mod message_queue;
mod signal;
mod subclass;
mod window_router;

pub use adapter::{
    NoopRenderSessionObserver, NoopUiFrameBuilder, OverlayAdapter, RenderSessionAttachment,
    RenderSessionObserver, RenderSessionResources, UiFrameBuilder, UiFramePreparation,
};
pub use signal::{NoopShutdownSignal, ShutdownSignal};
pub use window_router::{
    NoopWindowMessageRouter, WindowMessage, WindowMessageRoute, WindowMessageRouter,
};
