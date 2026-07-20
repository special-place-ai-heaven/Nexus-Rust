//! Input binding compatibility and deterministic GW2 input dispatch.
//!
//! This crate deliberately does not own a window procedure and never injects
//! platform input by itself. The overlay feeds [`InputMessage`] values into the
//! managed binding engine and supplies a [`GameMessageSink`] for GW2 messages.

mod bind;
mod capture;
mod error;
mod game;
mod game_ids;
mod managed;
mod raw;

pub use bind::{
    BindParseError, InputBind, InputDevice, KeyNameResolver, LegacyInputBind, Modifier,
    ModifierState, MouseButton, UsKeyNames, format_bind, parse_bind, parse_bind_lossy,
};
pub use capture::{CaptureOutcome, InputCapture, InputMessage};
pub use error::{GameInputError, GameSinkError, PersistenceError};
pub use game::{
    GameBindRegistry, GameDispatch, GameInvoker, GameMessage, GameMessageSink, GameOnlyMessageSink,
    GameSlot, InvokeState, MultiInputBind, PhysicalInputState, game_scan_code_to_scan_code,
    scan_code_to_game_scan_code,
};
pub use game_ids::{GameBindId, KnownGameBind, known_game_binds};
pub use managed::{
    CallbackExecutor, CallbackKind, CallbackLimits, InlineExecutor, InvokeOutcome, LoadReport,
    ManagedBindSnapshot, ManagedInputBinds, ManagedRegistrationToken, OwnerGeneration,
    RegisterOutcome, RouteOutcome, SetBindError,
};
pub use raw::{RawCallbackToken, RawMessage, RawRoute, RawRouteReport, RawWndProcRegistry};
