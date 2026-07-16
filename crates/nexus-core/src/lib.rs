//! Ownership-aware core services for Nexus.

mod address_ownership;
mod callback_gate;
mod data_link;
mod events;

pub use address_ownership::{AddressOwnershipError, AddressOwnershipIndex, AddressPublish};
pub use callback_gate::{CallbackGate, CallbackGuard};
pub use data_link::{DataLink, DataLinkError, ResourceHandle};
pub use events::{CallbackId, DispatchReport, EventBus, EventHandler, OwnerToken, Subscription};
