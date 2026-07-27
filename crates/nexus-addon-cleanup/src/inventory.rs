use crate::CleanupService;

/// Visibility of an existing owner-cleanup API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiAccess {
    /// Callable from the composite crate.
    Public,
    /// Callable only through a public composite owner.
    PublicComposite,
    /// Not callable outside the defining crate.
    CratePrivate,
    /// No phase-correct API exists yet.
    Missing,
}

/// Observable behavior of an existing owner-cleanup API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiSemantics {
    /// Completes exact-owner removal before returning.
    Synchronous,
    /// Removes several related registries as one operation.
    SynchronousComposite,
    /// Requires exclusive access on the owning UI thread.
    ThreadBound,
    /// Enqueues work which another service call must advance.
    Queued,
    /// Required behavior has no upstream operation.
    Unavailable,
}

/// Static qualification of callback/destructor behavior during cleanup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DropSafety {
    /// Removed callback values are retained until after the service lock drops.
    DetachedBeforeDrop,
    /// The operation owns no internal mutex and requires exclusive thread access.
    CallerExclusive,
    /// The operation only queues a value and invokes no callback.
    QueueOnly,
    /// Static inspection found a callback-capable value removed under a lock.
    CallbackDropUnderLock,
    /// Static inspection found ordinary owned state removed under a lock.
    ValueDropUnderLock,
    /// Static inspection found a native destructor run under a service lock.
    NativeDestructorUnderLock,
    /// The composite contains both detached and under-lock removals.
    Mixed,
    /// No implementation exists to inspect.
    Unavailable,
}

/// Exact static inventory row for an upstream cleanup method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApiInventoryEntry {
    /// Composite service slot associated with the method.
    pub service: CleanupService,
    /// Exact Rust method signature, without runtime values.
    pub method: &'static str,
    /// Cross-crate visibility.
    pub access: ApiAccess,
    /// Completion behavior.
    pub semantics: ApiSemantics,
    /// Lock/drop qualification from static inspection.
    pub drop_safety: DropSafety,
}

/// Exact owner-cleanup APIs present when this composite was authored.
pub const CLEANUP_API_INVENTORY: &[ApiInventoryEntry] = &[
    ApiInventoryEntry {
        service: CleanupService::InlineHooks,
        method: "nexus_inline_hooks::InlineHookService::cleanup_owner(&self, OwnerToken) -> Result<CleanupReport, CleanupError>",
        access: ApiAccess::Public,
        semantics: ApiSemantics::Synchronous,
        drop_safety: DropSafety::NativeDestructorUnderLock,
    },
    ApiInventoryEntry {
        service: CleanupService::UiHostCallbacks,
        method: "nexus_ui_host::UiHost::cleanup_owner_generation(&self, OwnerGeneration) -> UiHostCleanup",
        access: ApiAccess::PublicComposite,
        semantics: ApiSemantics::SynchronousComposite,
        drop_safety: DropSafety::Mixed,
    },
    ApiInventoryEntry {
        service: CleanupService::UiHostCallbacks,
        method: "nexus_ui_host::RenderRegistry::cleanup_owner_generation(&self, OwnerGeneration) -> (usize, OwnerRetirement)",
        access: ApiAccess::CratePrivate,
        semantics: ApiSemantics::Synchronous,
        drop_safety: DropSafety::DetachedBeforeDrop,
    },
    ApiInventoryEntry {
        service: CleanupService::UiHostCallbacks,
        method: "nexus_ui_host::EscapeClosingRegistry::cleanup_owner_generation(&self, OwnerGeneration) -> usize",
        access: ApiAccess::CratePrivate,
        semantics: ApiSemantics::Synchronous,
        drop_safety: DropSafety::DetachedBeforeDrop,
    },
    ApiInventoryEntry {
        service: CleanupService::UiHostCallbacks,
        method: "nexus_ui_host::AlertQueue::cleanup_owner_generation(&self, OwnerGeneration) -> usize",
        access: ApiAccess::CratePrivate,
        semantics: ApiSemantics::Synchronous,
        drop_safety: DropSafety::ValueDropUnderLock,
    },
    ApiInventoryEntry {
        service: CleanupService::UiHostCallbacks,
        method: "nexus_ui_host::QuickAccessRegistry::cleanup_owner_generation(&self, OwnerGeneration) -> (QuickAccessCleanup, OwnerRetirement)",
        access: ApiAccess::CratePrivate,
        semantics: ApiSemantics::SynchronousComposite,
        drop_safety: DropSafety::CallbackDropUnderLock,
    },
    ApiInventoryEntry {
        service: CleanupService::RawWndProcCallbacks,
        method: "nexus_input::RawWndProcRegistry::cleanup_owner_generation(&self, OwnerGeneration) -> usize",
        access: ApiAccess::Public,
        semantics: ApiSemantics::Synchronous,
        drop_safety: DropSafety::CallbackDropUnderLock,
    },
    ApiInventoryEntry {
        service: CleanupService::ManagedInputCallbacks,
        method: "nexus_input::ManagedInputBinds::cleanup_owner_generation(&self, OwnerGeneration) -> usize",
        access: ApiAccess::Public,
        semantics: ApiSemantics::Synchronous,
        drop_safety: DropSafety::CallbackDropUnderLock,
    },
    ApiInventoryEntry {
        service: CleanupService::EventCallbacks,
        method: "nexus_data_services::EventService::cleanup_owner(&self, OwnerToken) -> usize",
        access: ApiAccess::Public,
        semantics: ApiSemantics::Synchronous,
        drop_safety: DropSafety::CallbackDropUnderLock,
    },
    ApiInventoryEntry {
        service: CleanupService::TextureCallbacks,
        method: "nexus_textures::TextureService::cleanup_owner_generation(&self, OwnerGeneration) -> usize",
        access: ApiAccess::Public,
        semantics: ApiSemantics::Synchronous,
        drop_safety: DropSafety::CallbackDropUnderLock,
    },
    ApiInventoryEntry {
        service: CleanupService::FontCallbacks,
        method: "nexus_ui_services::FontManager::cleanup_owner_callbacks(&mut self, OwnerId) -> usize",
        access: ApiAccess::Public,
        semantics: ApiSemantics::ThreadBound,
        drop_safety: DropSafety::CallerExclusive,
    },
    ApiInventoryEntry {
        service: CleanupService::FontResources,
        method: "nexus_ui_services::FontManager::cleanup_owner_resources(&mut self, OwnerId) -> usize",
        access: ApiAccess::Public,
        semantics: ApiSemantics::ThreadBound,
        drop_safety: DropSafety::CallerExclusive,
    },
    ApiInventoryEntry {
        service: CleanupService::LocalizationOverrides,
        method: "nexus_ui_services::LocalizationHandle::cleanup_owner(&self, OwnerId) -> Result<(), LocalizationError>",
        access: ApiAccess::Public,
        semantics: ApiSemantics::Queued,
        drop_safety: DropSafety::QueueOnly,
    },
    ApiInventoryEntry {
        service: CleanupService::LocalizationOverrides,
        method: "nexus_ui_services::LocalizationService::advance(&mut self) -> LocalizationAdvanceReport",
        access: ApiAccess::Public,
        semantics: ApiSemantics::ThreadBound,
        drop_safety: DropSafety::DetachedBeforeDrop,
    },
];

/// Category for a concrete integration gap found during static inspection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiGapKind {
    /// Standalone render cleanup is crate-private; callers must own `UiHost`.
    StandaloneRenderCleanupPrivate,
    /// Font callbacks and owner resources cannot be retired in separate phases.
    FontPhaseSplitMissing,
    /// Localization queue acceptance does not prove owner cleanup completed.
    LocalizationAcknowledgementMissing,
    /// Runtime coordinator types are private and need a typed closure bridge.
    RuntimeCoordinatorPrivate,
    /// Upstream removal may drop a callback-capable value while locked.
    CallbackDropUnderLock,
    /// Upstream hook retirement runs a native destructor while locked.
    NativeDestructorUnderLock,
}

/// Static integration gap associated with one cleaner service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApiGap {
    /// Affected service slot.
    pub service: CleanupService,
    /// Stable gap category.
    pub kind: ApiGapKind,
}

/// Known gaps which an embedding runtime must bridge or upstream must fix.
pub const API_GAPS: &[ApiGap] = &[
    ApiGap {
        service: CleanupService::InlineHooks,
        kind: ApiGapKind::NativeDestructorUnderLock,
    },
    ApiGap {
        service: CleanupService::UiHostCallbacks,
        kind: ApiGapKind::StandaloneRenderCleanupPrivate,
    },
    ApiGap {
        service: CleanupService::UiHostCallbacks,
        kind: ApiGapKind::CallbackDropUnderLock,
    },
    ApiGap {
        service: CleanupService::LocalizationOverrides,
        kind: ApiGapKind::LocalizationAcknowledgementMissing,
    },
    ApiGap {
        service: CleanupService::TextureCallbacks,
        kind: ApiGapKind::RuntimeCoordinatorPrivate,
    },
    ApiGap {
        service: CleanupService::FontResources,
        kind: ApiGapKind::RuntimeCoordinatorPrivate,
    },
    ApiGap {
        service: CleanupService::RawWndProcCallbacks,
        kind: ApiGapKind::CallbackDropUnderLock,
    },
    ApiGap {
        service: CleanupService::ManagedInputCallbacks,
        kind: ApiGapKind::CallbackDropUnderLock,
    },
    ApiGap {
        service: CleanupService::EventCallbacks,
        kind: ApiGapKind::CallbackDropUnderLock,
    },
    ApiGap {
        service: CleanupService::TextureCallbacks,
        kind: ApiGapKind::CallbackDropUnderLock,
    },
];

/// Category for a lifecycle integration gap outside an individual service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrationGapKind {
    /// The addon manager's shared wrapper holds its mutex across adapter calls.
    ManagerOuterLockDuringCleanup,
}

/// Static lifecycle integration gap which binding coverage cannot resolve.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntegrationGap {
    /// Stable gap category.
    pub kind: IntegrationGapKind,
}

/// Known external integration gaps.
///
/// `nexus-addon-manager::SharedCleaner` currently retains its outer mutex while
/// calling `RegistrationCleaner::cleanup`. The composite itself owns no lock,
/// but same-wrapper reentrancy is not safe until that boundary is redesigned.
pub const INTEGRATION_GAPS: &[IntegrationGap] = &[IntegrationGap {
    kind: IntegrationGapKind::ManagerOuterLockDuringCleanup,
}];
