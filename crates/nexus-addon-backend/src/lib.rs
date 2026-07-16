//! Production boundary behind the legacy native add-on API.
//!
//! This crate owns native caller attribution, bounded argument snapshots, and
//! process-lifetime return storage. Domain services remain in their dedicated
//! crates and receive only owned, validated Rust values.

mod boundary;
mod data_link;
mod diagnostics;
mod events;
mod inline_hooks;
mod logging;
mod operation;
mod paths;
mod ui;

pub use boundary::{CallBoundaryError, NativeCallBoundary, NativeText};
pub use data_link::DataLinkApi;
pub use diagnostics::{BackendFailure, BackendFailureSnapshot, BackendFailures};
pub use events::EventApi;
pub use inline_hooks::InlineHookApi;
pub use logging::LoggingApi;
pub use operation::BackendOperationError;
pub use paths::{PathApi, StablePathError, StablePathStore};
pub use ui::UiApi;
