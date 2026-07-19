//! Owned compatibility services for the Nexus DataLink and event APIs.
//!
//! `nexus-core` owns internal allocations and ordered callback dispatch. This
//! crate adds validated native boundaries, public named-mapping ownership, and
//! exact `DL_NEXUS_LINK` publication without exposing mapping names in errors.

mod data_link;
mod events;
mod mapping;
mod mumble;
mod name;
mod nexus_link;
#[cfg(test)]
mod test_support;
#[cfg(windows)]
mod windows;

pub use data_link::{DataLinkService, DataServiceError, ResourceKind, ResourceLease};
pub use events::{EventService, EventServiceError, NativeEventCallback};
pub use mapping::{MappingBackend, MappingDisposition, MappingFailure, MappingView};
pub use mumble::{MumbleResourceError, MumbleResourceSource};
pub use name::{MAX_IDENTIFIER_BYTES, NameError};
pub use nexus_core::{CallbackId, DispatchReport, EventHandler, EventOwnerRetirement, OwnerToken};
pub use nexus_link::{
    DL_NEXUS_LINK, FontSnapshot, NexusLinkOpenError, NexusLinkPublisher, NexusLinkSnapshot,
    NexusLinkSnapshotError, QuickAccessPosition, QuickAccessSnapshot, RenderSnapshot,
};
#[cfg(windows)]
pub use windows::WindowsMappingBackend;
