use nexus_host::CleanupPhase;

mod private {
    pub trait Sealed {}
}

/// One fixed service slot in addon cleanup order.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CleanupService {
    /// Owner-scoped inline detours.
    InlineHooks,
    /// Render, Escape, alert, and Quick Access callbacks owned by `UiHost`.
    UiHostCallbacks,
    /// Raw window-procedure callbacks.
    RawWndProcCallbacks,
    /// Managed input-bind callbacks.
    ManagedInputCallbacks,
    /// Native and managed event subscriptions.
    EventCallbacks,
    /// Pending and ready texture callbacks.
    TextureCallbacks,
    /// A phase-specific fence for font callbacks before callback drain.
    FontCallbacks,
    /// Thread-bound font registrations and owner claims.
    FontResources,
    /// Runtime localization overrides.
    LocalizationOverrides,
}

impl CleanupService {
    /// All service slots in deterministic execution order.
    pub const ORDER: [Self; 9] = [
        Self::InlineHooks,
        Self::UiHostCallbacks,
        Self::RawWndProcCallbacks,
        Self::ManagedInputCallbacks,
        Self::EventCallbacks,
        Self::TextureCallbacks,
        Self::FontCallbacks,
        Self::FontResources,
        Self::LocalizationOverrides,
    ];

    /// Returns the host cleanup phase for this slot.
    #[must_use]
    pub const fn phase(self) -> CleanupPhase {
        match self {
            Self::InlineHooks => CleanupPhase::HookRegistrations,
            Self::UiHostCallbacks
            | Self::RawWndProcCallbacks
            | Self::ManagedInputCallbacks
            | Self::EventCallbacks
            | Self::TextureCallbacks
            | Self::FontCallbacks => CleanupPhase::CallbackRegistrations,
            Self::FontResources | Self::LocalizationOverrides => CleanupPhase::OwnedResources,
        }
    }

    /// Stable, redaction-safe diagnostic label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::InlineHooks => "inline-hooks",
            Self::UiHostCallbacks => "ui-host-callbacks",
            Self::RawWndProcCallbacks => "raw-wndproc-callbacks",
            Self::ManagedInputCallbacks => "managed-input-callbacks",
            Self::EventCallbacks => "event-callbacks",
            Self::TextureCallbacks => "texture-callbacks",
            Self::FontCallbacks => "font-callbacks",
            Self::FontResources => "font-resources",
            Self::LocalizationOverrides => "localization-overrides",
        }
    }
}

/// Type-level tag for one cleanup service.
///
/// The trait is sealed so an adapter constructed for one domain cannot be
/// passed into a different builder slot.
pub trait CleanupDomain: private::Sealed + 'static {
    /// Runtime service represented by the domain.
    const SERVICE: CleanupService;
}

macro_rules! cleanup_domains {
    ($(($name:ident, $service:ident, $doc:literal)),+ $(,)?) => {
        $(
            #[doc = $doc]
            #[derive(Debug)]
            pub struct $name;

            impl private::Sealed for $name {}

            impl CleanupDomain for $name {
                const SERVICE: CleanupService = CleanupService::$service;
            }
        )+
    };
}

cleanup_domains!(
    (
        InlineHooks,
        InlineHooks,
        "Typed domain for inline-hook teardown."
    ),
    (
        UiHostCallbacks,
        UiHostCallbacks,
        "Typed domain for the public `UiHost` callback cleanup composite."
    ),
    (
        RawWndProcCallbacks,
        RawWndProcCallbacks,
        "Typed domain for raw WndProc callback teardown."
    ),
    (
        ManagedInputCallbacks,
        ManagedInputCallbacks,
        "Typed domain for managed input callback teardown."
    ),
    (
        EventCallbacks,
        EventCallbacks,
        "Typed domain for event subscription teardown."
    ),
    (
        TextureCallbacks,
        TextureCallbacks,
        "Typed domain for texture request and callback teardown."
    ),
    (
        FontCallbacks,
        FontCallbacks,
        "Typed domain for the font callback fence required before drain."
    ),
    (
        FontResources,
        FontResources,
        "Typed domain for thread-bound font ownership teardown."
    ),
    (
        LocalizationOverrides,
        LocalizationOverrides,
        "Typed domain for synchronously completed localization teardown."
    ),
);

pub(crate) const fn phase_index(phase: CleanupPhase) -> usize {
    match phase {
        CleanupPhase::HookRegistrations => 0,
        CleanupPhase::CallbackRegistrations => 1,
        CleanupPhase::OwnedResources => 2,
    }
}

pub(crate) const fn phase_from_index(index: usize) -> CleanupPhase {
    match index {
        0 => CleanupPhase::HookRegistrations,
        1 => CleanupPhase::CallbackRegistrations,
        _ => CleanupPhase::OwnedResources,
    }
}
