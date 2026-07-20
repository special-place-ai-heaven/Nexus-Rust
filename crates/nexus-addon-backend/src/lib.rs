//! Production boundary behind the legacy native add-on API.
//!
//! This crate owns native caller attribution, bounded argument snapshots, and
//! process-lifetime return storage. Domain services remain in their dedicated
//! crates and receive only owned, validated Rust values.

mod boundary;
mod data_link;
mod diagnostics;
mod events;
mod fonts;
mod game_binds;
mod inline_hooks;
mod input_binds;
mod localization;
mod logging;
mod operation;
mod paths;
mod production;
mod services;
mod textures;
mod ui;
mod updates;
mod wnd_proc;

pub use boundary::{CallBoundaryError, NativeCallBoundary, NativeText};
pub use data_link::DataLinkApi;
pub use diagnostics::{BackendFailure, BackendFailureSnapshot, BackendFailures};
pub use events::EventApi;
pub use fonts::{FontApi, RenderFontService, SendFontCallback};
pub use game_binds::GameBindApi;
pub use inline_hooks::InlineHookApi;
pub use input_binds::InputBindApi;
pub use localization::LocalizationApi;
pub use logging::LoggingApi;
pub use operation::BackendOperationError;
pub use paths::{PathApi, StablePathError, StablePathStore};
pub use production::{CoreAddonApiServices, ProductionAddonApiBackend, RequiredAddonApiServices};
pub use services::{
    FontBackend, GameBindBackend, InputBindBackend, LocalizationBackend, RequiredServiceResult,
    TextureBackend, UpdateBackend, WndProcBackend,
};
pub use textures::{
    TextureApi, TextureFacadeError, TextureServiceFacade, TextureSourceFactory,
    TextureSourceFailurePolicy,
};
pub use ui::UiApi;
pub use updates::{AddonUpdateRequest, DEFAULT_UPDATE_QUEUE_CAPACITY, UpdateApi};
pub use wnd_proc::WndProcApi;
