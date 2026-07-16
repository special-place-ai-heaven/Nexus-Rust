//! Safe ownership and lifecycle foundations for native Nexus add-ons.
//!
//! This crate does not load dynamic libraries and never invokes add-on
//! callbacks. A platform layer supplies a live definition lease, acknowledges
//! callback outcomes, and releases the native module only after cleanup.

mod api_table;
mod definition;
mod host;

pub use api_table::{
    ApiRevision, ApiTableCatalog, ApiTableError, ApiTableLayout, ApiTableRef, ApiTables,
};
pub use definition::{
    DefinitionError, DefinitionLease, LiveAddonModule, MetadataField, MetadataLimitError,
    MetadataLimits, ModuleAccessError, ModuleMemory, ModuleReadError, OwnedAddonDefinition,
    validate_and_copy_definition,
};
pub use host::{
    AddonHost, AddonRecord, CleanupError, CleanupPhase, HostError, LifecycleState, LoadMode,
    RegistrationCleaner, UnloadReason,
};
