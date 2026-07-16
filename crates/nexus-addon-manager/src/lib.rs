//! Deterministic, policy-aware orchestration for native Nexus add-ons.
//!
//! Directory discovery and watcher events are inert. Native DLL inspection,
//! activation, unload callbacks, and hot reload remain explicit `unsafe`
//! operations at the manager boundary.

mod config;
mod diagnostics;
mod discovery;
mod manager;
mod plans;

pub use nexus_addon_loader::AbsoluteDllPath;
pub use nexus_core::OwnerToken;
pub use nexus_host::{CleanupError, CleanupPhase, RegistrationCleaner, UnloadReason};

pub use config::{AddonConfig, AddonConfigDocument, ConfigAccess, ConfigError, UpdateMode};
pub(crate) use diagnostics::DiagnosticBuffer;
pub use diagnostics::{Diagnostic, DiagnosticCode, DiagnosticSeverity};

pub use discovery::{
    AddonDirectory, BinaryRevision, DirectoryEvent, DirectoryScanner, DiscoveredDll,
    DiscoveryError, StdDirectoryScanner,
};
pub use manager::{
    ActivationRequest, AddonManager, AddonSnapshot, AddressResolutionError, DefinitionSummary,
    DirectoryChangeKind, DirectoryImpact, DirectoryRecommendation, EnableEffect, HostIssue,
    InspectOutcome, LoadedModuleAddressResolver, ManagerBoundary, ManagerError, ManagerOptions,
    ManagerRuntime, ManagerState, ModuleAddressRange, ModuleAddressResolver, PolicyReason,
};
pub use plans::{
    UninstallPlan, UninstallStep, UninstallTiming, UpdateConsent, UpdatePlan, UpdateStep,
    UpdateTiming,
};
