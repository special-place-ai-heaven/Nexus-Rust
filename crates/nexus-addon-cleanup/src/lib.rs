//! Generation-exact, fail-closed cleanup for native addon registrations.
//!
//! [`RegistrationCleaner`] implements the lifecycle contract from
//! `nexus-host`. Cleanup is split into the host's three safety phases and each
//! service slot is bound through a type-tagged [`TypedAdapter`]. A missing
//! binding is a reported gap and can never silently behave like a no-op.
//!
//! Adapter invocations are isolated from the cleaner's progress bookkeeping.
//! The cleaner owns no mutex and never invokes an adapter while holding an
//! internal lock. Panics are contained; caught payloads are deliberately
//! forgotten so an adversarial payload destructor cannot restart unwinding at
//! the lifecycle boundary.

mod adapter;
mod cleaner;
mod domain;
mod inventory;
mod report;

pub mod direct;

pub use adapter::{
    AdapterError, AdapterFailureKind, AdapterResult, CleanupEffect, GapReason, TypedAdapter,
};
pub use cleaner::{RegistrationCleaner, RegistrationCleanerBuilder};
pub use domain::{
    CleanupDomain, CleanupService, EventCallbacks, FontCallbacks, FontResources, InlineHooks,
    LocalizationOverrides, ManagedInputCallbacks, RawWndProcCallbacks, TextureCallbacks,
    UiHostCallbacks,
};
pub use inventory::{
    API_GAPS, ApiAccess, ApiGap, ApiGapKind, ApiInventoryEntry, ApiSemantics,
    CLEANUP_API_INVENTORY, DropSafety, INTEGRATION_GAPS, IntegrationGap, IntegrationGapKind,
};
pub use report::{
    CleanupFailure, CleanupReport, CoverageEntry, CoverageReport, PhaseStatus, StepFailure,
    StepReport, StepStatus,
};

#[cfg(test)]
mod tests;
