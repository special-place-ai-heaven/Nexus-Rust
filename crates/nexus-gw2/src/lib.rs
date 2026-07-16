//! Safe Guild Wars 2 telemetry built on the binary-compatible MumbleLink ABI.
//!
//! The wire types live in `nexus-abi`. This crate owns parsing, coherent
//! snapshots, derived movement state, and the Windows mapping lifetime. It
//! never includes mapping names or identity contents in diagnostic errors.

mod identity;
#[cfg(windows)]
mod mapping;
mod reader;
mod telemetry;

pub use identity::{IdentityParseError, parse_identity};
#[cfg(windows)]
pub use mapping::{MappedMumbleLink, MappingDisposition, MappingOpenError};
pub use reader::{IdentityUpdate, MumblePoll, MumbleReader, MumbleSource, SnapshotError};
pub use telemetry::{DerivedTelemetry, TelemetryTracker, ui_scaling_factor};
