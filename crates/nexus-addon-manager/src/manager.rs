use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fmt,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
    time::Duration,
};

use nexus_abi::{AddonDefinitionFlags, UpdateProvider, Version};
use nexus_addon_loader::{
    AbsoluteDllPath, AddonLoader, AddonModule, LoadError, LoaderPlatform, ModuleError,
};
use nexus_core::{AddressOwnershipError, AddressOwnershipIndex, CallbackGate, OwnerToken};
use nexus_host::{
    AddonHost, ApiTableCatalog, CleanupError, CleanupPhase, HostError, LoadMode, MetadataLimits,
    RegistrationCleaner, UnloadReason,
};
use thiserror::Error;

use crate::{
    AddonConfig, AddonConfigDocument, AddonDirectory, BinaryRevision, ConfigAccess, ConfigError,
    Diagnostic, DiagnosticBuffer, DiagnosticCode, DiagnosticSeverity, DirectoryEvent,
    DirectoryScanner, DiscoveredDll, DiscoveryError, UninstallPlan, UninstallStep, UninstallTiming,
    UpdateConsent, UpdateMode, UpdatePlan, UpdateStep, UpdateTiming,
    discovery::{normalize_discovery, path_key},
};

/// Explicit request that determines launch-only and hot-loading policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivationRequest {
    /// Initial process-start activation while the startup window remains open.
    Startup,
    /// User-requested activation after startup.
    Runtime,
    /// Replacement of a fully released earlier generation.
    HotReload,
}

/// Closed manager state for one discovered path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagerState {
    /// The DLL is known but native code has not been inspected.
    Discovered,
    /// Policy rejected activation and the inspected module was released.
    PolicyBlocked,
    /// A validated module and owner generation are ready for activation.
    Inspected,
    /// Native activation completed.
    Active,
    /// Callback ingress is closed and cleanup has been requested.
    UnloadRequested,
    /// Registration cleanup or callback draining is in progress.
    Draining,
    /// Every managed callback and registration has drained.
    Drained,
    /// The native unload callback completed or its Rust unwind was contained.
    NativeUnloadComplete,
    /// Native activation failed and must be explicitly cleaned up.
    ActivationFailed,
    /// Module release failed; retryability is reported by the operation error.
    ReleaseFailed,
    /// The module was released while the candidate remains on disk.
    Unloaded,
    /// The candidate is absent and has no live module.
    Removed,
}

/// Policy reason that can block inspection or activation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyReason {
    /// The persisted desired state is disabled.
    DisabledByUser,
    /// Read-only launch policy did not allow the signature.
    NotAllowlisted,
    /// The current binary revision is explicitly disabled.
    VersionDisabled,
    /// Startup activation was requested after the startup window closed.
    StartupClosed,
    /// The definition permits only its first startup attempt.
    LaunchOnly,
    /// The definition forbids runtime hot loading.
    HotLoadingLocked,
}

/// Result of explicit native inspection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InspectOutcome {
    /// The generation is validated and ready for explicit activation.
    Ready(OwnerToken),
    /// Policy blocked activation; the native module was released.
    Blocked {
        /// Validated add-on signature.
        signature: u32,
        /// Closed policy reason.
        reason: PolicyReason,
    },
}

/// Runtime consequence of changing the persisted enabled flag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnableEffect {
    /// No current generation requires an action.
    NoRuntimeChange,
    /// A discovered candidate may be explicitly inspected and activated.
    ActivationAvailable,
    /// The active generation can be explicitly unloaded now.
    UnloadAvailable(OwnerToken),
    /// A locked or launch-only policy defers the change until restart.
    RestartRequired(Option<OwnerToken>),
}

/// Closed process boundary whose Rust panic payload was discarded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagerBoundary {
    /// Injected directory scanner.
    DirectoryScanner,
    /// Injected module-address resolver.
    AddressResolver,
    /// Host lifecycle coordinator or injected cleaner.
    HostCoordinator,
    /// Direct activation-failure cleanup.
    RegistrationCleaner,
}

/// Validated half-open mapped-image address range.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ModuleAddressRange {
    start: NonZeroUsize,
    end: usize,
}

impl ModuleAddressRange {
    /// Creates a non-empty, non-overflowing half-open range.
    pub fn new(start: NonZeroUsize, size: usize) -> Result<Self, AddressResolutionError> {
        if size == 0 {
            return Err(AddressResolutionError::Empty);
        }
        let end = start
            .get()
            .checked_add(size)
            .ok_or(AddressResolutionError::Overflow)?;
        Ok(Self { start, end })
    }

    /// Returns the image length without disclosing either address.
    #[must_use]
    pub const fn len(self) -> usize {
        self.end - self.start.get()
    }

    /// Returns the non-zero start of the half-open range.
    #[must_use]
    pub const fn start(self) -> NonZeroUsize {
        self.start
    }

    /// Returns whether the range is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.end == self.start.get()
    }

    /// Returns whether an address belongs to the half-open image range.
    #[must_use]
    pub const fn contains(self, address: NonZeroUsize) -> bool {
        address.get() >= self.start.get() && address.get() < self.end
    }

    fn overlaps(self, other: Self) -> bool {
        self.start.get() < other.end && other.start.get() < self.end
    }
}

impl fmt::Debug for ModuleAddressRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModuleAddressRange")
            .field("image_size", &self.len())
            .finish_non_exhaustive()
    }
}

/// Injected address-resolution failure with no raw address or platform text.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AddressResolutionError {
    /// The adapter could not determine trustworthy bounds.
    #[error("module address range is unavailable")]
    Unavailable,
    /// The mapped image was empty.
    #[error("module address range is empty")]
    Empty,
    /// The mapped image overflowed the address space.
    #[error("module address range overflowed")]
    Overflow,
    /// The adapter range disagreed with the validated loader image size.
    #[error("module address range disagrees with loader image size")]
    SizeMismatch,
    /// The adapter range overlaps another live module.
    #[error("module address range overlaps another live add-on")]
    Overlap,
}

/// Resolves numeric image ownership for a validated live module.
///
/// Implementations may use a platform API, but must not dereference arbitrary
/// addresses or change the module reference count. Tests should inject a fixed
/// range resolver.
pub trait ModuleAddressResolver<P: LoaderPlatform>: Send + Sync + 'static {
    /// Returns the exact half-open range for `module`.
    fn resolve(
        &self,
        module: &AddonModule<P>,
    ) -> Result<ModuleAddressRange, AddressResolutionError>;
}

/// Production resolver using the loader's already-validated mapped-image bounds.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadedModuleAddressResolver;

impl<P: LoaderPlatform> ModuleAddressResolver<P> for LoadedModuleAddressResolver {
    fn resolve(
        &self,
        module: &AddonModule<P>,
    ) -> Result<ModuleAddressRange, AddressResolutionError> {
        ModuleAddressRange::new(module.image_base(), module.image_size())
    }
}

/// Pointer-free metadata retained across module release.
#[derive(Clone, Eq, PartialEq)]
pub struct DefinitionSummary {
    signature: u32,
    name: String,
    version: Version,
    author: String,
    description: String,
    flags: AddonDefinitionFlags,
    provider: UpdateProvider,
    update_link: Option<String>,
}

impl DefinitionSummary {
    /// Returns the legacy add-on signature.
    #[must_use]
    pub const fn signature(&self) -> u32 {
        self.signature
    }

    /// Returns the copied add-on name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the copied add-on version.
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// Returns the copied author metadata.
    #[must_use]
    pub fn author(&self) -> &str {
        &self.author
    }

    /// Returns the copied description metadata.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the definition behavior flags.
    #[must_use]
    pub const fn flags(&self) -> AddonDefinitionFlags {
        self.flags
    }

    /// Returns the update provider identifier.
    #[must_use]
    pub const fn provider(&self) -> UpdateProvider {
        self.provider
    }

    /// Returns the copied optional update link.
    #[must_use]
    pub fn update_link(&self) -> Option<&str> {
        self.update_link.as_deref()
    }
}

impl fmt::Debug for DefinitionSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DefinitionSummary")
            .field("signature", &format_args!("{:#010x}", self.signature))
            .field("version", &self.version)
            .field("flags", &self.flags)
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

/// Read-only snapshot of one managed path.
#[derive(Clone, Eq, PartialEq)]
pub struct AddonSnapshot {
    path: AbsoluteDllPath,
    revision: BinaryRevision,
    loaded_revision: Option<BinaryRevision>,
    state: ManagerState,
    present: bool,
    owner: Option<OwnerToken>,
    last_owner: Option<OwnerToken>,
    summary: Option<DefinitionSummary>,
    blocked_by: Option<PolicyReason>,
}

impl AddonSnapshot {
    /// Borrows the managed absolute path.
    #[must_use]
    pub const fn path(&self) -> &AbsoluteDllPath {
        &self.path
    }

    /// Returns the latest discovered binary revision.
    #[must_use]
    pub const fn revision(&self) -> BinaryRevision {
        self.revision
    }

    /// Returns the revision backing the live or most recently loaded generation.
    #[must_use]
    pub const fn loaded_revision(&self) -> Option<BinaryRevision> {
        self.loaded_revision
    }

    /// Returns the manager state.
    #[must_use]
    pub const fn state(&self) -> ManagerState {
        self.state
    }

    /// Returns whether the latest scan still contains the path.
    #[must_use]
    pub const fn present(&self) -> bool {
        self.present
    }

    /// Returns the current live owner generation, if any.
    #[must_use]
    pub const fn owner(&self) -> Option<OwnerToken> {
        self.owner
    }

    /// Returns the most recently admitted owner generation.
    #[must_use]
    pub const fn last_owner(&self) -> Option<OwnerToken> {
        self.last_owner
    }

    /// Borrows copied definition metadata, when the DLL has been inspected.
    #[must_use]
    pub const fn summary(&self) -> Option<&DefinitionSummary> {
        self.summary.as_ref()
    }

    /// Returns the last policy block reason.
    #[must_use]
    pub const fn blocked_by(&self) -> Option<PolicyReason> {
        self.blocked_by
    }
}

impl fmt::Debug for AddonSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AddonSnapshot")
            .field("path", &self.path)
            .field("revision", &self.revision)
            .field("loaded_revision", &self.loaded_revision)
            .field("state", &self.state)
            .field("present", &self.present)
            .field("owner", &self.owner)
            .field("last_owner", &self.last_owner)
            .field("summary", &self.summary)
            .field("blocked_by", &self.blocked_by)
            .finish()
    }
}

/// Suggested action resulting from an inert directory change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryRecommendation {
    /// No runtime action is needed.
    None,
    /// The active generation may be explicitly hot reloaded.
    HotReload(OwnerToken),
    /// The active generation may be explicitly unloaded.
    Unload(OwnerToken),
    /// A locked generation requires a restart to apply the change.
    RestartRequired(OwnerToken),
}

/// Kind of deterministic directory inventory change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryChangeKind {
    /// A new candidate was added.
    Added,
    /// An existing candidate revision changed.
    Modified,
    /// An existing candidate disappeared.
    Removed,
    /// A candidate path changed.
    Renamed,
    /// The injected event did not change inventory state.
    Unchanged,
}

/// Result of applying one inert directory event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryImpact {
    path: AbsoluteDllPath,
    kind: DirectoryChangeKind,
    recommendation: DirectoryRecommendation,
}

impl DirectoryImpact {
    fn new(
        path: AbsoluteDllPath,
        kind: DirectoryChangeKind,
        recommendation: DirectoryRecommendation,
    ) -> Self {
        Self {
            path,
            kind,
            recommendation,
        }
    }

    /// Borrows the affected redacted-debug path.
    #[must_use]
    pub const fn path(&self) -> &AbsoluteDllPath {
        &self.path
    }

    /// Returns the inventory change kind.
    #[must_use]
    pub const fn kind(&self) -> DirectoryChangeKind {
        self.kind
    }

    /// Returns the non-executing runtime recommendation.
    #[must_use]
    pub const fn recommendation(&self) -> DirectoryRecommendation {
        self.recommendation
    }
}

/// Manager construction and resource limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagerOptions {
    /// Bounded copied-metadata limits passed to the native loader and host.
    pub metadata_limits: MetadataLimits,
    /// Maximum number of retained redaction-safe diagnostics.
    pub diagnostic_capacity: usize,
    /// Current game build recorded after successful activation.
    pub game_build: u32,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            metadata_limits: MetadataLimits::default(),
            diagnostic_capacity: 256,
            game_build: 0,
        }
    }
}

/// Closed host coordination category with cleaner-provided text discarded.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum HostIssue {
    /// Definition validation failed.
    #[error("host rejected the add-on definition")]
    Definition,
    /// API table setup or lookup failed.
    #[error("host API table is unavailable")]
    ApiTable,
    /// Another generation already owns the signature.
    #[error("add-on signature already has an active generation")]
    DuplicateSignature,
    /// A generation counter was exhausted.
    #[error("add-on generation counter was exhausted")]
    GenerationExhausted,
    /// The owner token is unknown.
    #[error("add-on owner is unknown")]
    UnknownOwner,
    /// The host lifecycle transition was invalid.
    #[error("host lifecycle transition is invalid")]
    InvalidTransition,
    /// Definition flags forbid hot reload.
    #[error("add-on definition forbids hot reload")]
    HotReloadForbidden,
    /// Definition flags forbid runtime unload.
    #[error("add-on definition forbids runtime unload")]
    RuntimeUnloadForbidden,
    /// Host callbacks did not drain before the deadline.
    #[error("host callbacks did not drain before the deadline")]
    DrainTimedOut,
    /// A registration cleanup phase failed; its text was discarded.
    #[error("host registration cleanup failed")]
    Cleanup,
    /// Module release was acknowledged before cleanup completed.
    #[error("host registration drain is incomplete")]
    DrainIncomplete,
}

impl From<HostError> for HostIssue {
    fn from(error: HostError) -> Self {
        match error {
            HostError::Definition(_) => Self::Definition,
            HostError::ApiTable(_) => Self::ApiTable,
            HostError::DuplicateSignature { .. } => Self::DuplicateSignature,
            HostError::GenerationExhausted { .. } => Self::GenerationExhausted,
            HostError::UnknownOwner { .. } => Self::UnknownOwner,
            HostError::InvalidTransition { .. } => Self::InvalidTransition,
            HostError::HotReloadForbidden { .. } => Self::HotReloadForbidden,
            HostError::RuntimeUnloadForbidden { .. } => Self::RuntimeUnloadForbidden,
            HostError::DrainTimedOut { .. } => Self::DrainTimedOut,
            HostError::Cleanup { .. } => Self::Cleanup,
            HostError::DrainIncomplete { .. } => Self::DrainIncomplete,
        }
    }
}

/// Redaction-safe orchestration failure.
#[derive(Debug, Error)]
pub enum ManagerError {
    /// Deterministic directory discovery failed.
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    /// Compatible config processing failed.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Native inspection failed inside the hardened loader.
    #[error(transparent)]
    Load(#[from] LoadError),
    /// Native module lifecycle advancement failed.
    #[error(transparent)]
    Module(#[from] ModuleError),
    /// Host coordination failed; arbitrary cleaner text was discarded.
    #[error("host coordination failed: {0}")]
    Host(HostIssue),
    /// Address ownership could not be established.
    #[error(transparent)]
    Address(#[from] AddressResolutionError),
    /// The concurrent caller-attribution index rejected a range transition.
    #[error(transparent)]
    OwnershipIndex(#[from] AddressOwnershipError),
    /// No candidate exists for the supplied path.
    #[error("add-on path is not tracked")]
    UnknownPath,
    /// No current or historical entry exists for the supplied owner.
    #[error("add-on owner is not tracked")]
    UnknownOwner,
    /// The operation is not valid in the manager's current state.
    #[error("manager state must be {expected}, but is {actual:?}")]
    InvalidState {
        /// Human-readable accepted state set.
        expected: &'static str,
        /// Actual manager state.
        actual: ManagerState,
    },
    /// The candidate disappeared before native inspection.
    #[error("add-on candidate is no longer present")]
    CandidateMissing,
    /// A policy check rejected the requested operation.
    #[error("add-on policy rejected the operation: {0:?}")]
    Policy(PolicyReason),
    /// Config changes are forbidden by read-only launch policy.
    #[error("add-on config is read-only")]
    ConfigReadOnly,
    /// An injected Rust boundary panicked; its payload was discarded.
    #[error("Rust unwind was contained at manager boundary {0:?}")]
    BoundaryPanic(ManagerBoundary),
    /// Module release failed and retained the module for explicit retry.
    #[error("module release failed; retryable: {retryable}")]
    ReleaseFailed {
        /// Whether the loader guarantees an explicit retry is safe.
        retryable: bool,
    },
    /// The retained module cannot be released again safely.
    #[error("module release is not retryable")]
    ReleaseNotRetryable,
}

struct SharedCleaner<C> {
    inner: Arc<Mutex<Option<C>>>,
}

impl<C> Clone for SharedCleaner<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<C> SharedCleaner<C> {
    fn new(cleaner: C) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(cleaner))),
        }
    }

    fn try_lease(&self) -> Result<SharedCleanerLease<C>, CleanupError> {
        let cleaner = {
            let mut slot = self
                .inner
                .try_lock()
                .map_err(|_unavailable| CleanupError::new("registration cleaner is busy"))?;
            slot.take()
                .ok_or_else(|| CleanupError::new("registration cleaner is busy"))?
        };
        Ok(SharedCleanerLease {
            inner: Arc::clone(&self.inner),
            cleaner: Some(cleaner),
        })
    }
}

struct SharedCleanerLease<C> {
    inner: Arc<Mutex<Option<C>>>,
    cleaner: Option<C>,
}

impl<C: RegistrationCleaner> SharedCleanerLease<C> {
    fn cleanup(&mut self, owner: OwnerToken, phase: CleanupPhase) -> Result<(), CleanupError> {
        let Some(cleaner) = self.cleaner.as_mut() else {
            return Err(CleanupError::new("registration cleaner lease is empty"));
        };
        cleaner.cleanup(owner, phase)
    }
}

impl<C> Drop for SharedCleanerLease<C> {
    fn drop(&mut self) {
        let mut cleaner = self.cleaner.take();
        {
            let mut slot = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_none() {
                *slot = cleaner.take();
            }
        }
        // A violated internal invariant may leave an extra cleaner. Its Drop
        // must run only after the shared-state lock has been released.
        drop(cleaner);
    }
}

impl<C: RegistrationCleaner> RegistrationCleaner for SharedCleaner<C> {
    fn cleanup(&mut self, owner: OwnerToken, phase: CleanupPhase) -> Result<(), CleanupError> {
        self.try_lease()?.cleanup(owner, phase)
    }
}

struct Entry<P: LoaderPlatform> {
    discovered: DiscoveredDll,
    present: bool,
    state: ManagerState,
    module: Option<AddonModule<P>>,
    range: Option<ModuleAddressRange>,
    owner: Option<OwnerToken>,
    last_owner: Option<OwnerToken>,
    summary: Option<DefinitionSummary>,
    loaded_revision: Option<BinaryRevision>,
    blocked_by: Option<PolicyReason>,
    failed_cleanup_cursor: usize,
    release_needs_host_ack: bool,
    release_return_state: Option<ManagerState>,
}

impl<P: LoaderPlatform> Entry<P> {
    fn discovered(discovered: DiscoveredDll) -> Self {
        Self {
            discovered,
            present: true,
            state: ManagerState::Discovered,
            module: None,
            range: None,
            owner: None,
            last_owner: None,
            summary: None,
            loaded_revision: None,
            blocked_by: None,
            failed_cleanup_cursor: 0,
            release_needs_host_ack: false,
            release_return_state: None,
        }
    }

    fn snapshot(&self) -> AddonSnapshot {
        AddonSnapshot {
            path: self.discovered.path().clone(),
            revision: self.discovered.revision(),
            loaded_revision: self.loaded_revision,
            state: self.state,
            present: self.present,
            owner: self.owner,
            last_owner: self.last_owner,
            summary: self.summary.clone(),
            blocked_by: self.blocked_by,
        }
    }
}

/// Injected runtime boundaries used by an [`AddonManager`].
pub struct ManagerRuntime<P: LoaderPlatform, C: RegistrationCleaner> {
    scanner: Box<dyn DirectoryScanner>,
    platform: Arc<P>,
    resolver: Box<dyn ModuleAddressResolver<P>>,
    cleaner: C,
    api_tables: Option<Arc<ApiTableCatalog>>,
    ownership: Arc<AddressOwnershipIndex>,
}

impl<P: LoaderPlatform, C: RegistrationCleaner> ManagerRuntime<P, C> {
    /// Groups inert discovery, native platform, address, and cleanup adapters.
    #[must_use]
    pub fn new(
        scanner: impl DirectoryScanner,
        platform: Arc<P>,
        resolver: impl ModuleAddressResolver<P>,
        cleaner: C,
    ) -> Self {
        Self {
            scanner: Box::new(scanner),
            platform,
            resolver: Box::new(resolver),
            cleaner,
            api_tables: None,
            ownership: Arc::new(AddressOwnershipIndex::new()),
        }
    }

    /// Installs the render-session catalog that native activation may expose.
    #[must_use]
    pub fn with_api_tables(mut self, api_tables: Arc<ApiTableCatalog>) -> Self {
        self.api_tables = Some(api_tables);
        self
    }

    /// Uses a shared caller-attribution index for this manager session.
    #[must_use]
    pub fn with_address_ownership_index(mut self, ownership: Arc<AddressOwnershipIndex>) -> Self {
        self.ownership = ownership;
        self
    }

    /// Returns the shared caller-attribution index.
    #[must_use]
    pub fn address_ownership_index(&self) -> Arc<AddressOwnershipIndex> {
        Arc::clone(&self.ownership)
    }
}

/// Deterministic runtime orchestrator over `nexus-addon-loader` and `nexus-host`.
pub struct AddonManager<P: LoaderPlatform, C: RegistrationCleaner> {
    directory: AddonDirectory,
    scanner: Box<dyn DirectoryScanner>,
    resolver: Box<dyn ModuleAddressResolver<P>>,
    loader: AddonLoader<P>,
    host: AddonHost<SharedCleaner<C>>,
    cleaner: SharedCleaner<C>,
    ownership: Arc<AddressOwnershipIndex>,
    configs: AddonConfigDocument,
    config_access: ConfigAccess,
    options: ManagerOptions,
    entries: BTreeMap<String, Entry<P>>,
    owner_paths: HashMap<OwnerToken, String>,
    launch_attempted: HashSet<u32>,
    startup_open: bool,
    diagnostics: DiagnosticBuffer,
}

impl<P: LoaderPlatform, C: RegistrationCleaner> AddonManager<P, C> {
    /// Creates an inert manager around injected platform, scanner, resolver, and cleaner boundaries.
    pub fn new(
        directory: AddonDirectory,
        runtime: ManagerRuntime<P, C>,
        configs: AddonConfigDocument,
        config_access: ConfigAccess,
        options: ManagerOptions,
    ) -> Result<Self, ManagerError> {
        let ManagerRuntime {
            scanner,
            platform,
            resolver,
            cleaner,
            api_tables,
            ownership,
        } = runtime;
        let cleaner = SharedCleaner::new(cleaner);
        let host_cleaner = cleaner.clone();
        let host = match api_tables {
            Some(api_tables) => AddonHost::with_api_tables(host_cleaner, api_tables),
            None => {
                AddonHost::new(host_cleaner).map_err(|error| ManagerError::Host(error.into()))?
            }
        };
        Ok(Self {
            directory,
            scanner,
            resolver,
            loader: AddonLoader::from_shared(platform),
            host,
            cleaner,
            ownership,
            configs,
            config_access,
            options,
            entries: BTreeMap::new(),
            owner_paths: HashMap::new(),
            launch_attempted: HashSet::new(),
            startup_open: true,
            diagnostics: DiagnosticBuffer::new(options.diagnostic_capacity),
        })
    }

    /// Ends the only window in which launch-only add-ons may be activated.
    pub fn finish_startup(&mut self) {
        self.startup_open = false;
    }

    /// Returns whether startup-only activation is still accepted.
    #[must_use]
    pub const fn startup_open(&self) -> bool {
        self.startup_open
    }

    /// Performs an inert injected directory scan and applies a deterministic diff.
    pub fn refresh_discovery(&mut self) -> Result<Vec<DirectoryImpact>, ManagerError> {
        let scanned = contain_panic(|| self.scanner.scan(&self.directory)).map_err(|()| {
            self.emit(
                DiagnosticSeverity::Error,
                DiagnosticCode::BoundaryPanicContained,
                None,
            );
            ManagerError::BoundaryPanic(ManagerBoundary::DirectoryScanner)
        })??;
        let scanned = normalize_discovery(&self.directory, scanned)?;
        let seen: BTreeSet<_> = scanned.iter().map(|item| path_key(item.path())).collect();
        let mut impacts = Vec::new();
        for candidate in scanned {
            impacts.push(self.apply_upsert(candidate)?);
        }
        let removed: Vec<_> = self
            .entries
            .iter()
            .filter(|(key, entry)| entry.present && !seen.contains(*key))
            .map(|(_key, entry)| entry.discovered.path().clone())
            .collect();
        for path in removed {
            impacts.push(self.apply_removed(&path)?);
        }
        Ok(impacts)
    }

    /// Applies an injected watcher event without loading or calling a DLL.
    pub fn apply_directory_event(
        &mut self,
        event: DirectoryEvent,
    ) -> Result<Vec<DirectoryImpact>, ManagerError> {
        match event {
            DirectoryEvent::Upsert(candidate) => Ok(vec![self.apply_upsert(candidate)?]),
            DirectoryEvent::Removed(path) => Ok(vec![self.apply_removed(&path)?]),
            DirectoryEvent::Renamed { from, to } => Ok(vec![self.apply_renamed(&from, to)?]),
            DirectoryEvent::Rescan => self.refresh_discovery(),
        }
    }

    /// Returns snapshots in deterministic case-insensitive path order.
    #[must_use]
    pub fn snapshots(&self) -> Vec<AddonSnapshot> {
        self.entries.values().map(Entry::snapshot).collect()
    }

    /// Returns the snapshot for one tracked path.
    #[must_use]
    pub fn snapshot(&self, path: &AbsoluteDllPath) -> Option<AddonSnapshot> {
        self.entries.get(&path_key(path)).map(Entry::snapshot)
    }

    /// Resolves the exact live generation owning an address.
    #[must_use]
    pub fn owner_for_address(&self, address: NonZeroUsize) -> Option<OwnerToken> {
        self.ownership.owner_for_address(address)
    }

    /// Returns the concurrent caller-attribution index used by native shims.
    #[must_use]
    pub fn address_ownership_index(&self) -> Arc<AddressOwnershipIndex> {
        Arc::clone(&self.ownership)
    }

    /// Returns the host's current generation for a signature.
    #[must_use]
    pub fn active_owner(&self, signature: u32) -> Option<OwnerToken> {
        self.host.active_owner(signature)
    }

    /// Clones the generation-aware callback gate used by registration wrappers.
    pub fn callback_gate(&self, owner: OwnerToken) -> Result<Arc<CallbackGate>, ManagerError> {
        self.host
            .callback_gate(owner)
            .map_err(|error| ManagerError::Host(error.into()))
    }

    /// Borrows the compatible config document.
    #[must_use]
    pub const fn configs(&self) -> &AddonConfigDocument {
        &self.configs
    }

    /// Returns retained redaction-safe diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> &VecDeque<Diagnostic> {
        self.diagnostics.entries()
    }

    /// Drains all retained diagnostics.
    pub fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        self.diagnostics.take()
    }

    /// Loads and validates a trusted DLL without invoking its load callback.
    ///
    /// A blocked policy outcome releases the inspected module. No native code
    /// is reached by directory discovery alone.
    ///
    /// # Safety
    ///
    /// Loading a DLL and calling its definition export execute trusted native
    /// code. The caller must trust the exact candidate and satisfy the safety
    /// contract of [`AddonLoader::load_absolute`].
    pub unsafe fn inspect(
        &mut self,
        path: &AbsoluteDllPath,
        request: ActivationRequest,
    ) -> Result<InspectOutcome, ManagerError> {
        let key = path_key(path);
        let (candidate, state, present) = self
            .entries
            .get(&key)
            .map(|entry| (entry.discovered.clone(), entry.state, entry.present))
            .ok_or(ManagerError::UnknownPath)?;
        if !present {
            return Err(ManagerError::CandidateMissing);
        }
        if !matches!(
            state,
            ManagerState::Discovered
                | ManagerState::PolicyBlocked
                | ManagerState::Unloaded
                | ManagerState::Removed
        ) {
            return Err(ManagerError::InvalidState {
                expected: "Discovered, PolicyBlocked, Unloaded, or Removed",
                actual: state,
            });
        }

        // SAFETY: this public method carries the loader's native-code trust
        // obligation unchanged and the path came from inert discovery.
        let module = unsafe {
            self.loader
                .load_absolute(candidate.path(), self.options.metadata_limits)
        }?;
        let summary = summary_from_module(&module);
        let policy = self.activation_policy(&summary, candidate.revision(), request)?;
        if let Some(reason) = policy {
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.summary = Some(summary.clone());
                entry.blocked_by = Some(reason);
                entry.state = ManagerState::PolicyBlocked;
            }
            self.release_rejected_module(&key, module, ManagerState::PolicyBlocked)?;
            self.emit(
                DiagnosticSeverity::Warning,
                DiagnosticCode::PolicyBlocked,
                None,
            );
            return Ok(InspectOutcome::Blocked {
                signature: summary.signature,
                reason,
            });
        }

        let range = contain_panic(|| self.resolver.resolve(&module)).map_err(|()| {
            self.emit(
                DiagnosticSeverity::Error,
                DiagnosticCode::BoundaryPanicContained,
                None,
            );
            ManagerError::BoundaryPanic(ManagerBoundary::AddressResolver)
        })??;
        if range.len() != module.image_size() {
            self.release_rejected_module(&key, module, state)?;
            return Err(AddressResolutionError::SizeMismatch.into());
        }
        if self.entries.iter().any(|(other_key, entry)| {
            other_key != &key && entry.range.is_some_and(|other| range.overlaps(other))
        }) {
            self.release_rejected_module(&key, module, state)?;
            return Err(AddressResolutionError::Overlap.into());
        }

        let mode = match request {
            ActivationRequest::HotReload => LoadMode::HotReload,
            ActivationRequest::Startup | ActivationRequest::Runtime => LoadMode::Initial,
        };
        let owner = match contain_panic(|| {
            self.host
                .discover(&module, mode, self.options.metadata_limits)
        }) {
            Ok(Ok(owner)) => owner,
            Ok(Err(error)) => {
                self.release_rejected_module(&key, module, state)?;
                return Err(ManagerError::Host(error.into()));
            }
            Err(()) => {
                self.release_rejected_module(&key, module, state)?;
                self.emit(
                    DiagnosticSeverity::Error,
                    DiagnosticCode::BoundaryPanicContained,
                    None,
                );
                return Err(ManagerError::BoundaryPanic(
                    ManagerBoundary::HostCoordinator,
                ));
            }
        };

        let callback_gate = match self.host.callback_gate(owner) {
            Ok(callback_gate) => callback_gate,
            Err(error) => {
                let _ = self.host.cancel_discovery(owner);
                self.release_rejected_module(&key, module, state)?;
                return Err(ManagerError::Host(error.into()));
            }
        };
        if let Err(error) = self
            .ownership
            .publish(owner, range.start(), range.len(), callback_gate)
        {
            let host_result = self
                .host
                .cancel_discovery(owner)
                .map_err(|host_error| ManagerError::Host(host_error.into()));
            self.release_rejected_module(&key, module, state)?;
            host_result?;
            return Err(error.into());
        }

        let entry = self
            .entries
            .get_mut(&key)
            .ok_or(ManagerError::UnknownPath)?;
        entry.module = Some(module);
        entry.range = Some(range);
        entry.owner = Some(owner);
        entry.last_owner = Some(owner);
        entry.summary = Some(summary);
        entry.loaded_revision = Some(candidate.revision());
        entry.blocked_by = None;
        entry.state = ManagerState::Inspected;
        entry.failed_cleanup_cursor = 0;
        self.owner_paths.insert(owner, key);
        self.emit(
            DiagnosticSeverity::Info,
            DiagnosticCode::DefinitionInspected,
            Some(owner),
        );
        Ok(InspectOutcome::Ready(owner))
    }

    /// Invokes the validated native load callback for an inspected generation.
    ///
    /// # Safety
    ///
    /// The selected API table must be fully populated for the requested ABI.
    /// Native faults, aborts, and process termination are outside Rust unwind
    /// containment.
    pub unsafe fn activate(&mut self, owner: OwnerToken) -> Result<(), ManagerError> {
        if !self.host.api_tables_populated() {
            return Err(ManagerError::Host(HostIssue::ApiTable));
        }
        let key = self.owner_key(owner)?;
        let state = self.entry(&key)?.state;
        if state != ManagerState::Inspected {
            return Err(ManagerError::InvalidState {
                expected: "Inspected",
                actual: state,
            });
        }
        let api = self
            .host
            .api_table(owner)
            .map_err(|error| ManagerError::Host(error.into()))?
            .as_opaque_ptr();
        let outcome = {
            let module =
                self.entry_mut(&key)?
                    .module
                    .as_mut()
                    .ok_or(ManagerError::InvalidState {
                        expected: "live inspected module",
                        actual: state,
                    })?;
            // SAFETY: this method exposes the same native callback and API
            // validity obligations as `AddonModule::activate`.
            unsafe { module.activate(api) }
        };
        if let Err(error) = outcome {
            let _ = self.host.acknowledge_activation_failed(owner);
            self.ownership.close(owner);
            self.entry_mut(&key)?.state = ManagerState::ActivationFailed;
            self.emit(
                DiagnosticSeverity::Error,
                DiagnosticCode::ActivationFailed,
                Some(owner),
            );
            return Err(error.into());
        }
        self.host
            .acknowledge_loaded(owner)
            .map_err(|error| ManagerError::Host(error.into()))?;
        let (signature, name) = {
            let summary = self
                .entry(&key)?
                .summary
                .as_ref()
                .ok_or(ManagerError::InvalidState {
                    expected: "copied definition metadata",
                    actual: ManagerState::Inspected,
                })?;
            (summary.signature, summary.name.clone())
        };
        if self.config_access.is_writable() {
            let config = self.configs.get_or_insert(signature)?;
            config.set_last_game_build(self.options.game_build);
            config.set_last_name(name);
        }
        self.entry_mut(&key)?.state = ManagerState::Active;
        self.emit(
            DiagnosticSeverity::Info,
            DiagnosticCode::Activated,
            Some(owner),
        );
        Ok(())
    }

    /// Closes host and loader callback ingress for an active generation.
    pub fn request_unload(
        &mut self,
        owner: OwnerToken,
        reason: UnloadReason,
    ) -> Result<(), ManagerError> {
        let key = self.owner_key(owner)?;
        let state = self.entry(&key)?.state;
        if state != ManagerState::Active {
            return Err(ManagerError::InvalidState {
                expected: "Active",
                actual: state,
            });
        }
        self.host
            .request_unload(owner, reason)
            .map_err(|error| ManagerError::Host(error.into()))?;
        self.entry_mut(&key)?
            .module
            .as_mut()
            .ok_or(ManagerError::InvalidState {
                expected: "live active module",
                actual: state,
            })?
            .request_shutdown()?;
        self.entry_mut(&key)?.state = ManagerState::UnloadRequested;
        self.ownership.close(owner);
        self.emit(
            DiagnosticSeverity::Info,
            DiagnosticCode::UnloadRequested,
            Some(owner),
        );
        Ok(())
    }

    /// Runs ordered registration cleanup and waits for both callback gates.
    pub fn drain(&mut self, owner: OwnerToken, timeout: Duration) -> Result<(), ManagerError> {
        let key = self.owner_key(owner)?;
        let state = self.entry(&key)?.state;
        if !matches!(
            state,
            ManagerState::UnloadRequested | ManagerState::Draining
        ) {
            return Err(ManagerError::InvalidState {
                expected: "UnloadRequested or Draining",
                actual: state,
            });
        }
        self.entry_mut(&key)?.state = ManagerState::Draining;
        let host_result = contain_panic(|| self.host.drain_registrations(owner, timeout));
        match host_result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.emit(
                    DiagnosticSeverity::Warning,
                    DiagnosticCode::CleanupRetryRequired,
                    Some(owner),
                );
                return Err(ManagerError::Host(error.into()));
            }
            Err(()) => {
                self.emit(
                    DiagnosticSeverity::Error,
                    DiagnosticCode::BoundaryPanicContained,
                    Some(owner),
                );
                return Err(ManagerError::BoundaryPanic(
                    ManagerBoundary::HostCoordinator,
                ));
            }
        }
        self.entry_mut(&key)?
            .module
            .as_mut()
            .ok_or(ManagerError::InvalidState {
                expected: "live draining module",
                actual: ManagerState::Draining,
            })?
            .wait_for_callbacks(timeout)?;
        self.entry_mut(&key)?.state = ManagerState::Drained;
        self.emit(
            DiagnosticSeverity::Info,
            DiagnosticCode::DrainComplete,
            Some(owner),
        );
        Ok(())
    }

    /// Invokes the optional native unload callback after callback drain.
    ///
    /// The state advances even when a Rust unwind is contained, preventing a
    /// side-effecting unload routine from being invoked twice.
    ///
    /// # Safety
    ///
    /// The caller must accept execution of the trusted native unload callback.
    pub unsafe fn invoke_native_unload(&mut self, owner: OwnerToken) -> Result<(), ManagerError> {
        let key = self.owner_key(owner)?;
        let state = self.entry(&key)?.state;
        if state != ManagerState::Drained {
            return Err(ManagerError::InvalidState {
                expected: "Drained",
                actual: state,
            });
        }
        let result = {
            let module =
                self.entry_mut(&key)?
                    .module
                    .as_mut()
                    .ok_or(ManagerError::InvalidState {
                        expected: "live drained module",
                        actual: state,
                    })?;
            // SAFETY: both manager-owned callback gates and registration
            // groups completed their explicit drain before this transition.
            unsafe { module.invoke_unload() }
        };
        self.entry_mut(&key)?.state = ManagerState::NativeUnloadComplete;
        self.emit(
            if result.is_ok() {
                DiagnosticSeverity::Info
            } else {
                DiagnosticSeverity::Error
            },
            DiagnosticCode::NativeUnloadComplete,
            Some(owner),
        );
        result.map_err(Into::into)
    }

    /// Records host cleanup completion and releases the native module reference.
    pub fn finish_unload(&mut self, owner: OwnerToken) -> Result<(), ManagerError> {
        let key = self.owner_key(owner)?;
        let state = self.entry(&key)?.state;
        if state != ManagerState::NativeUnloadComplete {
            return Err(ManagerError::InvalidState {
                expected: "NativeUnloadComplete",
                actual: state,
            });
        }
        self.entry_mut(&key)?
            .module
            .as_mut()
            .ok_or(ManagerError::InvalidState {
                expected: "live cleaned module",
                actual: state,
            })?
            .complete_host_cleanup(|| Ok::<(), ()>(()))?;

        let module = self
            .entry_mut(&key)?
            .module
            .take()
            .ok_or(ManagerError::InvalidState {
                expected: "live cleaned module",
                actual: state,
            })?;
        match module.release() {
            Ok(()) => self.finish_successful_release(&key, Some(owner), true),
            Err(failure) => {
                let retryable = failure.is_retryable();
                let (module, _error) = failure.into_parts();
                let entry = self.entry_mut(&key)?;
                entry.module = Some(module);
                entry.state = ManagerState::ReleaseFailed;
                entry.release_needs_host_ack = true;
                entry.release_return_state = Some(if entry.present {
                    ManagerState::Unloaded
                } else {
                    ManagerState::Removed
                });
                self.emit_release_failure(owner, retryable);
                Err(ManagerError::ReleaseFailed { retryable })
            }
        }
    }

    /// Retries a loader-guaranteed-safe failed module release.
    pub fn retry_release(&mut self, path: &AbsoluteDllPath) -> Result<(), ManagerError> {
        let key = path_key(path);
        let state = self.entry(&key)?.state;
        if state != ManagerState::ReleaseFailed {
            return Err(ManagerError::InvalidState {
                expected: "ReleaseFailed",
                actual: state,
            });
        }
        let module = self
            .entry_mut(&key)?
            .module
            .take()
            .ok_or(ManagerError::ReleaseNotRetryable)?;
        let owner = self.entry(&key)?.owner;
        let needs_ack = self.entry(&key)?.release_needs_host_ack;
        match module.release() {
            Ok(()) => self.finish_successful_release(&key, owner, needs_ack),
            Err(failure) => {
                let retryable = failure.is_retryable();
                let (module, _error) = failure.into_parts();
                self.entry_mut(&key)?.module = Some(module);
                if let Some(owner) = owner {
                    self.emit_release_failure(owner, retryable);
                }
                if retryable {
                    Err(ManagerError::ReleaseFailed { retryable })
                } else {
                    Err(ManagerError::ReleaseNotRetryable)
                }
            }
        }
    }

    /// Cleans and releases a generation whose activation failed.
    ///
    /// # Safety
    ///
    /// Cleanup may invoke the native unload callback after closing both gates.
    /// The same native-code trust requirements as [`Self::invoke_native_unload`]
    /// apply.
    pub unsafe fn cleanup_activation_failure(
        &mut self,
        owner: OwnerToken,
        timeout: Duration,
    ) -> Result<(), ManagerError> {
        let key = self.owner_key(owner)?;
        let state = self.entry(&key)?.state;
        if !matches!(
            state,
            ManagerState::ActivationFailed | ManagerState::Draining
        ) {
            return Err(ManagerError::InvalidState {
                expected: "ActivationFailed or Draining",
                actual: state,
            });
        }
        if state == ManagerState::ActivationFailed {
            self.ownership.close(owner);
            self.entry_mut(&key)?
                .module
                .as_mut()
                .ok_or(ManagerError::InvalidState {
                    expected: "failed live module",
                    actual: state,
                })?
                .request_shutdown()?;
            self.entry_mut(&key)?.state = ManagerState::Draining;
        }

        while self.entry(&key)?.failed_cleanup_cursor < 2 {
            let cursor = self.entry(&key)?.failed_cleanup_cursor;
            self.run_direct_cleanup(owner, CleanupPhase::ORDER[cursor])?;
            self.entry_mut(&key)?.failed_cleanup_cursor += 1;
        }
        let gate = self
            .host
            .callback_gate(owner)
            .map_err(|error| ManagerError::Host(error.into()))?;
        if !gate.wait_for_drain(timeout) {
            return Err(ManagerError::Host(HostIssue::DrainTimedOut));
        }
        self.entry_mut(&key)?
            .module
            .as_mut()
            .ok_or(ManagerError::InvalidState {
                expected: "failed live module",
                actual: ManagerState::Draining,
            })?
            .wait_for_callbacks(timeout)?;
        self.run_direct_cleanup(owner, CleanupPhase::OwnedResources)?;
        self.entry_mut(&key)?.failed_cleanup_cursor = CleanupPhase::ORDER.len();
        {
            let module =
                self.entry_mut(&key)?
                    .module
                    .as_mut()
                    .ok_or(ManagerError::InvalidState {
                        expected: "failed live module",
                        actual: ManagerState::Draining,
                    })?;
            // SAFETY: activation-failure cleanup closed and drained both gates.
            unsafe { module.invoke_unload() }?;
            module.complete_host_cleanup(|| Ok::<(), ()>(()))?;
        }
        self.entry_mut(&key)?.state = ManagerState::NativeUnloadComplete;
        let module = self
            .entry_mut(&key)?
            .module
            .take()
            .ok_or(ManagerError::ReleaseNotRetryable)?;
        match module.release() {
            Ok(()) => self.finish_successful_release(&key, Some(owner), false),
            Err(failure) => {
                let retryable = failure.is_retryable();
                let (module, _error) = failure.into_parts();
                let entry = self.entry_mut(&key)?;
                entry.module = Some(module);
                entry.state = ManagerState::ReleaseFailed;
                entry.release_needs_host_ack = false;
                entry.release_return_state = Some(if entry.present {
                    ManagerState::Unloaded
                } else {
                    ManagerState::Removed
                });
                self.emit_release_failure(owner, retryable);
                Err(ManagerError::ReleaseFailed { retryable })
            }
        }
    }

    /// Performs the complete explicit unload, replacement generation, and activation sequence.
    ///
    /// Every intermediate failure leaves its exact state available for retry.
    ///
    /// # Safety
    ///
    /// This operation executes the current unload callback and loads and
    /// activates the replacement DLL. Both binaries must be trusted.
    pub unsafe fn hot_reload(
        &mut self,
        owner: OwnerToken,
        timeout: Duration,
    ) -> Result<OwnerToken, ManagerError> {
        let key = self.owner_key(owner)?;
        let flags = self
            .entry(&key)?
            .summary
            .as_ref()
            .map(DefinitionSummary::flags)
            .ok_or(ManagerError::InvalidState {
                expected: "copied definition metadata",
                actual: self.entry(&key)?.state,
            })?;
        if flags.contains(AddonDefinitionFlags::DISABLE_HOT_LOADING) {
            return Err(ManagerError::Policy(PolicyReason::HotLoadingLocked));
        }
        if flags.contains(AddonDefinitionFlags::LAUNCH_ONLY) {
            return Err(ManagerError::Policy(PolicyReason::LaunchOnly));
        }
        let path = self.entry(&key)?.discovered.path().clone();
        self.request_unload(owner, UnloadReason::Runtime)?;
        self.drain(owner, timeout)?;
        // SAFETY: this method carries the native unload trust obligation.
        unsafe { self.invoke_native_unload(owner) }?;
        self.finish_unload(owner)?;
        // SAFETY: this method carries trust for the replacement DLL.
        let outcome = unsafe { self.inspect(&path, ActivationRequest::HotReload) }?;
        let InspectOutcome::Ready(new_owner) = outcome else {
            let InspectOutcome::Blocked { reason, .. } = outcome else {
                unreachable!("inspect outcome has only ready and blocked variants")
            };
            return Err(ManagerError::Policy(reason));
        };
        // SAFETY: the replacement was inspected and the caller accepts its
        // native activation under this method's contract.
        unsafe { self.activate(new_owner) }?;
        self.emit(
            DiagnosticSeverity::Info,
            DiagnosticCode::HotReloadComplete,
            Some(new_owner),
        );
        Ok(new_owner)
    }

    /// Changes the desired launch state without implicitly running native code.
    pub fn set_enabled(
        &mut self,
        signature: u32,
        enabled: bool,
    ) -> Result<EnableEffect, ManagerError> {
        if !self.config_access.is_writable() {
            return Err(ManagerError::ConfigReadOnly);
        }
        self.configs.get_or_insert(signature)?.set_enabled(enabled);
        let matching = self.entries.values().find(|entry| {
            entry
                .summary
                .as_ref()
                .is_some_and(|summary| summary.signature == signature)
        });
        let Some(entry) = matching else {
            return Ok(EnableEffect::NoRuntimeChange);
        };
        if !enabled && entry.state == ManagerState::Active {
            let owner = entry.owner.ok_or(ManagerError::UnknownOwner)?;
            let flags = entry
                .summary
                .as_ref()
                .map(DefinitionSummary::flags)
                .ok_or(ManagerError::UnknownOwner)?;
            return Ok(
                if flags.contains(AddonDefinitionFlags::DISABLE_HOT_LOADING) {
                    EnableEffect::RestartRequired(Some(owner))
                } else {
                    EnableEffect::UnloadAvailable(owner)
                },
            );
        }
        if enabled && entry.present && entry.module.is_none() {
            let launch_only = entry
                .summary
                .as_ref()
                .is_some_and(|summary| summary.flags.contains(AddonDefinitionFlags::LAUNCH_ONLY));
            return Ok(if launch_only && !self.startup_open {
                EnableEffect::RestartRequired(entry.last_owner)
            } else {
                EnableEffect::ActivationAvailable
            });
        }
        Ok(EnableEffect::NoRuntimeChange)
    }

    /// Produces a deterministic update plan without touching the filesystem.
    pub fn plan_update(
        &self,
        owner: OwnerToken,
        staged: AbsoluteDllPath,
    ) -> Result<UpdatePlan, ManagerError> {
        let key = self.owner_key(owner)?;
        let entry = self.entry(&key)?;
        let summary = entry.summary.as_ref().ok_or(ManagerError::UnknownOwner)?;
        let config = self.config_for(summary.signature);
        let consent = match config.update_mode() {
            UpdateMode::None => UpdateConsent::Disabled,
            UpdateMode::Background => UpdateConsent::StageOnly,
            UpdateMode::Notify => UpdateConsent::ConfirmationRequired,
            UpdateMode::Automatic => UpdateConsent::Automatic,
        };
        let target = entry.discovered.path().clone();
        let active = entry.owner == Some(owner) && entry.state == ManagerState::Active;
        let locked = summary
            .flags
            .contains(AddonDefinitionFlags::DISABLE_HOT_LOADING)
            || summary.flags.contains(AddonDefinitionFlags::LAUNCH_ONLY);
        let (timing, steps) = if active && locked {
            (
                UpdateTiming::RestartRequired,
                vec![
                    UpdateStep::ReplaceOnRestart { staged, target },
                    UpdateStep::Rescan,
                ],
            )
        } else if active {
            (
                UpdateTiming::RuntimeHotReload,
                vec![
                    UpdateStep::RequestUnload(owner),
                    UpdateStep::ReplaceAfterUnload { staged, target },
                    UpdateStep::Rescan,
                    UpdateStep::ActivateHotReload,
                ],
            )
        } else {
            (
                UpdateTiming::BeforeNextActivation,
                vec![
                    UpdateStep::ReplaceInactive { staged, target },
                    UpdateStep::Rescan,
                ],
            )
        };
        Ok(UpdatePlan::new(owner, consent, timing, steps))
    }

    /// Produces a deterministic uninstall plan without deleting or moving files.
    pub fn plan_uninstall(&self, owner: OwnerToken) -> Result<UninstallPlan, ManagerError> {
        let key = self.owner_key(owner)?;
        let entry = self.entry(&key)?;
        let summary = entry.summary.as_ref().ok_or(ManagerError::UnknownOwner)?;
        let target = entry.discovered.path().clone();
        let active = entry.owner == Some(owner) && entry.state == ManagerState::Active;
        let locked = summary
            .flags
            .contains(AddonDefinitionFlags::DISABLE_HOT_LOADING);
        let (timing, steps) = if active && locked {
            (
                UninstallTiming::RestartRequired,
                vec![
                    UninstallStep::RemoveOnRestart(target),
                    UninstallStep::RemoveConfig(summary.signature),
                    UninstallStep::Rescan,
                ],
            )
        } else if active {
            (
                UninstallTiming::RuntimeAfterUnload,
                vec![
                    UninstallStep::RequestUnload(owner),
                    UninstallStep::RemoveAfterUnload(target),
                    UninstallStep::RemoveConfig(summary.signature),
                    UninstallStep::Rescan,
                ],
            )
        } else {
            (
                UninstallTiming::Immediate,
                vec![
                    UninstallStep::RemoveInactive(target),
                    UninstallStep::RemoveConfig(summary.signature),
                    UninstallStep::Rescan,
                ],
            )
        };
        Ok(UninstallPlan::new(owner, timing, steps))
    }

    fn apply_upsert(&mut self, candidate: DiscoveredDll) -> Result<DirectoryImpact, ManagerError> {
        if !self.directory.contains_direct_child(candidate.path()) {
            return Err(DiscoveryError::OutsideDirectory.into());
        }
        let key = path_key(candidate.path());
        let path = candidate.path().clone();
        let (kind, recommendation, code) = if let Some(entry) = self.entries.get_mut(&key) {
            let changed = entry.discovered.revision() != candidate.revision()
                || entry.discovered.byte_len() != candidate.byte_len();
            entry.discovered = candidate;
            entry.present = true;
            if changed {
                (
                    DirectoryChangeKind::Modified,
                    recommendation_for(entry, true, false),
                    DiagnosticCode::CandidateChanged,
                )
            } else {
                (
                    DirectoryChangeKind::Unchanged,
                    DirectoryRecommendation::None,
                    DiagnosticCode::CandidateDiscovered,
                )
            }
        } else {
            self.entries.insert(key, Entry::discovered(candidate));
            (
                DirectoryChangeKind::Added,
                DirectoryRecommendation::None,
                DiagnosticCode::CandidateDiscovered,
            )
        };
        if kind != DirectoryChangeKind::Unchanged {
            self.emit(DiagnosticSeverity::Info, code, recommendation.owner());
        }
        Ok(DirectoryImpact::new(path, kind, recommendation))
    }

    fn apply_removed(&mut self, path: &AbsoluteDllPath) -> Result<DirectoryImpact, ManagerError> {
        if !self.directory.contains_direct_child(path) {
            return Err(DiscoveryError::OutsideDirectory.into());
        }
        let key = path_key(path);
        let entry = self
            .entries
            .get_mut(&key)
            .ok_or(ManagerError::UnknownPath)?;
        if !entry.present {
            return Ok(DirectoryImpact::new(
                path.clone(),
                DirectoryChangeKind::Unchanged,
                DirectoryRecommendation::None,
            ));
        }
        entry.present = false;
        let recommendation = recommendation_for(entry, false, true);
        if entry.module.is_none() {
            entry.state = ManagerState::Removed;
        }
        self.emit(
            DiagnosticSeverity::Info,
            DiagnosticCode::CandidateRemoved,
            recommendation.owner(),
        );
        Ok(DirectoryImpact::new(
            path.clone(),
            DirectoryChangeKind::Removed,
            recommendation,
        ))
    }

    fn apply_renamed(
        &mut self,
        from: &AbsoluteDllPath,
        to: DiscoveredDll,
    ) -> Result<DirectoryImpact, ManagerError> {
        if !self.directory.contains_direct_child(from)
            || !self.directory.contains_direct_child(to.path())
        {
            return Err(DiscoveryError::OutsideDirectory.into());
        }
        let old_key = path_key(from);
        let new_key = path_key(to.path());
        if old_key == new_key {
            return self.apply_upsert(to);
        }
        if self.entries.contains_key(&new_key) {
            return Err(DiscoveryError::DuplicateEntry.into());
        }
        let mut entry = self
            .entries
            .remove(&old_key)
            .ok_or(ManagerError::UnknownPath)?;
        entry.discovered = to;
        entry.present = true;
        let path = entry.discovered.path().clone();
        let recommendation = recommendation_for(&entry, true, false);
        for stored_key in self.owner_paths.values_mut() {
            if stored_key == &old_key {
                *stored_key = new_key.clone();
            }
        }
        self.entries.insert(new_key, entry);
        self.emit(
            DiagnosticSeverity::Info,
            DiagnosticCode::CandidateRenamed,
            recommendation.owner(),
        );
        Ok(DirectoryImpact::new(
            path,
            DirectoryChangeKind::Renamed,
            recommendation,
        ))
    }

    fn activation_policy(
        &mut self,
        summary: &DefinitionSummary,
        revision: BinaryRevision,
        request: ActivationRequest,
    ) -> Result<Option<PolicyReason>, ManagerError> {
        let config = self.config_for_or_register(summary.signature)?;
        let enabled = match &self.config_access {
            ConfigAccess::Writable => config.enabled(),
            ConfigAccess::ReadOnlyAllowlist(allowed) => allowed.contains(&summary.signature),
        };
        if !enabled {
            return Ok(Some(match self.config_access {
                ConfigAccess::Writable => PolicyReason::DisabledByUser,
                ConfigAccess::ReadOnlyAllowlist(_) => PolicyReason::NotAllowlisted,
            }));
        }
        if !config.disable_version().is_empty()
            && revision.matches_legacy_hex(config.disable_version())
        {
            return Ok(Some(PolicyReason::VersionDisabled));
        }
        if request == ActivationRequest::Startup && !self.startup_open {
            return Ok(Some(PolicyReason::StartupClosed));
        }
        let launch_only = summary.flags.contains(AddonDefinitionFlags::LAUNCH_ONLY);
        if launch_only {
            let already_attempted = !self.launch_attempted.insert(summary.signature);
            if request != ActivationRequest::Startup || already_attempted {
                return Ok(Some(PolicyReason::LaunchOnly));
            }
        }
        if request == ActivationRequest::HotReload
            && summary
                .flags
                .contains(AddonDefinitionFlags::DISABLE_HOT_LOADING)
        {
            return Ok(Some(PolicyReason::HotLoadingLocked));
        }
        Ok(None)
    }

    fn config_for_or_register(&mut self, signature: u32) -> Result<AddonConfig, ManagerError> {
        Ok(match &self.config_access {
            ConfigAccess::Writable => self.configs.get_or_insert(signature)?.clone(),
            ConfigAccess::ReadOnlyAllowlist(_) => self
                .configs
                .get(signature)
                .cloned()
                .unwrap_or_else(AddonConfig::registered_default),
        })
    }

    fn config_for(&self, signature: u32) -> AddonConfig {
        self.configs
            .get(signature)
            .cloned()
            .unwrap_or_else(AddonConfig::registered_default)
    }

    fn release_rejected_module(
        &mut self,
        key: &str,
        module: AddonModule<P>,
        return_state: ManagerState,
    ) -> Result<(), ManagerError> {
        match module.release() {
            Ok(()) => Ok(()),
            Err(failure) => {
                let retryable = failure.is_retryable();
                let (module, _error) = failure.into_parts();
                let entry = self.entry_mut(key)?;
                entry.module = Some(module);
                entry.state = ManagerState::ReleaseFailed;
                entry.release_needs_host_ack = false;
                entry.release_return_state = Some(return_state);
                self.emit(
                    DiagnosticSeverity::Error,
                    if retryable {
                        DiagnosticCode::ReleaseRetryRequired
                    } else {
                        DiagnosticCode::ReleasePinned
                    },
                    None,
                );
                Err(ManagerError::ReleaseFailed { retryable })
            }
        }
    }

    fn finish_successful_release(
        &mut self,
        key: &str,
        owner: Option<OwnerToken>,
        acknowledge_host: bool,
    ) -> Result<(), ManagerError> {
        let host_result = if acknowledge_host {
            match owner {
                Some(owner) => self
                    .host
                    .acknowledge_module_released(owner)
                    .map_err(|error| ManagerError::Host(error.into())),
                None => Err(ManagerError::UnknownOwner),
            }
        } else {
            Ok(())
        };
        let entry = self.entry_mut(key)?;
        entry.module = None;
        entry.range = None;
        entry.owner = None;
        let next_state = entry
            .release_return_state
            .take()
            .unwrap_or(if entry.present {
                ManagerState::Unloaded
            } else {
                ManagerState::Removed
            });
        if next_state != ManagerState::PolicyBlocked {
            entry.blocked_by = None;
        }
        entry.state = next_state;
        entry.release_needs_host_ack = false;
        if let Some(owner) = owner {
            self.ownership.retire(owner);
        }
        self.emit(
            DiagnosticSeverity::Info,
            DiagnosticCode::ModuleReleased,
            owner,
        );
        host_result
    }

    fn run_direct_cleanup(
        &mut self,
        owner: OwnerToken,
        phase: CleanupPhase,
    ) -> Result<(), ManagerError> {
        let mut cleaner = self.cleaner.clone();
        let result = contain_panic(|| cleaner.cleanup(owner, phase).map_err(|_error| ()));
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(())) => Err(ManagerError::Host(HostIssue::Cleanup)),
            Err(()) => {
                self.emit(
                    DiagnosticSeverity::Error,
                    DiagnosticCode::BoundaryPanicContained,
                    Some(owner),
                );
                Err(ManagerError::BoundaryPanic(
                    ManagerBoundary::RegistrationCleaner,
                ))
            }
        }
    }

    fn owner_key(&self, owner: OwnerToken) -> Result<String, ManagerError> {
        self.owner_paths
            .get(&owner)
            .cloned()
            .ok_or(ManagerError::UnknownOwner)
    }

    fn entry(&self, key: &str) -> Result<&Entry<P>, ManagerError> {
        self.entries.get(key).ok_or(ManagerError::UnknownPath)
    }

    fn entry_mut(&mut self, key: &str) -> Result<&mut Entry<P>, ManagerError> {
        self.entries.get_mut(key).ok_or(ManagerError::UnknownPath)
    }

    fn emit(
        &mut self,
        severity: DiagnosticSeverity,
        code: DiagnosticCode,
        owner: Option<OwnerToken>,
    ) {
        self.diagnostics.push(severity, code, owner);
    }

    fn emit_release_failure(&mut self, owner: OwnerToken, retryable: bool) {
        self.emit(
            DiagnosticSeverity::Error,
            if retryable {
                DiagnosticCode::ReleaseRetryRequired
            } else {
                DiagnosticCode::ReleasePinned
            },
            Some(owner),
        );
    }
}

fn contain_panic<T>(operation: impl FnOnce() -> T) -> Result<T, ()> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(value) => Ok(value),
        Err(payload) => {
            // A custom panic payload can itself panic from Drop. Forget it so
            // this native lifecycle boundary cannot unwind a second time.
            std::mem::forget(payload);
            Err(())
        }
    }
}

fn summary_from_module<P: LoaderPlatform>(module: &AddonModule<P>) -> DefinitionSummary {
    let definition = module.owned_definition();
    DefinitionSummary {
        signature: definition.signature(),
        name: definition.name().to_owned(),
        version: definition.version(),
        author: definition.author().to_owned(),
        description: definition.description().to_owned(),
        flags: definition.flags(),
        provider: definition.provider(),
        update_link: definition.update_link().map(str::to_owned),
    }
}

fn recommendation_for<P: LoaderPlatform>(
    entry: &Entry<P>,
    changed: bool,
    removed: bool,
) -> DirectoryRecommendation {
    let Some(owner) = entry.owner else {
        return DirectoryRecommendation::None;
    };
    if entry.state != ManagerState::Active {
        return DirectoryRecommendation::None;
    }
    let flags = entry
        .summary
        .as_ref()
        .map_or(AddonDefinitionFlags::NONE, DefinitionSummary::flags);
    if flags.contains(AddonDefinitionFlags::DISABLE_HOT_LOADING)
        || flags.contains(AddonDefinitionFlags::LAUNCH_ONLY)
    {
        DirectoryRecommendation::RestartRequired(owner)
    } else if removed {
        DirectoryRecommendation::Unload(owner)
    } else if changed {
        DirectoryRecommendation::HotReload(owner)
    } else {
        DirectoryRecommendation::None
    }
}

trait RecommendationOwner {
    fn owner(self) -> Option<OwnerToken>;
}

impl RecommendationOwner for DirectoryRecommendation {
    fn owner(self) -> Option<OwnerToken> {
        match self {
            Self::None => None,
            Self::HotReload(owner) | Self::Unload(owner) | Self::RestartRequired(owner) => {
                Some(owner)
            }
        }
    }
}

#[cfg(test)]
mod panic_tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use nexus_core::OwnerToken;
    use nexus_host::{CleanupError, CleanupPhase, RegistrationCleaner};

    use super::{SharedCleaner, contain_panic};

    #[test]
    fn adversarial_panic_payload_destructor_is_never_reopened() {
        struct PanicOnDrop;

        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                panic!("panic payload destructor must not run");
            }
        }

        assert_eq!(
            contain_panic(|| std::panic::panic_any(PanicOnDrop)),
            Err(())
        );
    }

    struct ReentrantCleaner {
        shared: Arc<Mutex<Option<SharedCleaner<Self>>>>,
        state_unlocked: Arc<AtomicBool>,
        rejected: Arc<AtomicBool>,
    }

    impl RegistrationCleaner for ReentrantCleaner {
        fn cleanup(&mut self, owner: OwnerToken, phase: CleanupPhase) -> Result<(), CleanupError> {
            let reentrant = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let Some(mut reentrant) = reentrant else {
                return Err(CleanupError::new("test cleaner was not initialized"));
            };
            self.state_unlocked
                .store(reentrant.inner.try_lock().is_ok(), Ordering::Relaxed);
            self.rejected
                .store(reentrant.cleanup(owner, phase).is_err(), Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn shared_cleaner_runs_callbacks_without_holding_its_state_lock() {
        let shared_slot = Arc::new(Mutex::new(None));
        let state_unlocked = Arc::new(AtomicBool::new(false));
        let rejected = Arc::new(AtomicBool::new(false));
        let shared = SharedCleaner::new(ReentrantCleaner {
            shared: Arc::clone(&shared_slot),
            state_unlocked: Arc::clone(&state_unlocked),
            rejected: Arc::clone(&rejected),
        });
        *shared_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(shared.clone());

        let mut outer = shared.clone();
        assert!(
            outer
                .cleanup(
                    OwnerToken {
                        signature: 17,
                        generation: 3,
                    },
                    CleanupPhase::HookRegistrations,
                )
                .is_ok()
        );
        assert!(state_unlocked.load(Ordering::Relaxed));
        assert!(rejected.load(Ordering::Relaxed));

        *shared_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    struct PanicOnceCleaner(Arc<AtomicUsize>);

    impl RegistrationCleaner for PanicOnceCleaner {
        fn cleanup(
            &mut self,
            _owner: OwnerToken,
            _phase: CleanupPhase,
        ) -> Result<(), CleanupError> {
            if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                panic!("first cleanup panics");
            }
            Ok(())
        }
    }

    #[test]
    fn unwind_restores_the_leased_cleaner_for_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut shared = SharedCleaner::new(PanicOnceCleaner(Arc::clone(&calls)));
        let owner = OwnerToken {
            signature: 19,
            generation: 4,
        };

        let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = shared.cleanup(owner, CleanupPhase::HookRegistrations);
        }));
        if let Err(payload) = first {
            core::mem::forget(payload);
        }

        assert!(
            shared
                .cleanup(owner, CleanupPhase::HookRegistrations)
                .is_ok()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
