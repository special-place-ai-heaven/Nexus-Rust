//! Owner-generation-safe UI registries for Nexus host and addon integration.
//!
//! This crate owns no renderer. It provides bounded, deterministic state and
//! stable snapshots for a render-thread consumer while isolating the native
//! callback and `bool*` ABI behind explicit boundary types.

#![deny(unsafe_code)]

mod alerts;
mod callback;
mod error;
mod escape;
mod host;
mod native;
mod owner;
mod quick_access;
mod render;

pub use alerts::{
    AlertFrame, AlertKind, AlertQueue, AlertQueueConfig, AlertSnapshot, NotifyOutcome,
};
pub use callback::{CallbackInvocation, UiCallback};
pub use error::UiRegistryError;
pub use escape::{
    ESCAPE_VIRTUAL_KEY, EscapeCloseOutcome, EscapeClosingConfig, EscapeClosingRegistry,
    EscapeKeyEvent, EscapeRegistrationOutcome, VisibilityTarget,
};
pub use host::{UiHost, UiHostCleanup, UiHostConfig};
pub use native::{CheckedVisibilityAccess, NativeRenderCallback, NativeVisibilityPointer};
pub use owner::{OwnerGeneration, OwnerHandle, OwnerRetirement};
pub use quick_access::{
    ContextMenuItemSnapshot, ContextRegistrationOutcome, NotificationBadge, NotificationOutcome,
    QA_GENERIC_KEY, QA_MENU_ID, QuickAccessCleanup, QuickAccessConfig, QuickAccessPosition,
    QuickAccessRegistry, QuickAccessSettings, QuickAccessSnapshot, QuickAccessVisibility,
    ShortcutMutation, ShortcutRegistrationOutcome, ShortcutSnapshot,
};
pub use render::{
    FrameRenderState, RegisterRenderOutcome, RenderFrameSnapshot, RenderInvocationReport,
    RenderPhase, RenderRegistry, RenderRegistryConfig, RenderSnapshot,
};
