//! Aggregate UI host lifecycle coordination.

use crate::owner::OwnerRegistry;
use crate::{
    AlertQueue, AlertQueueConfig, EscapeClosingConfig, EscapeClosingRegistry, OwnerGeneration,
    OwnerHandle, OwnerRetirement, QuickAccessCleanup, QuickAccessConfig, QuickAccessRegistry,
    RenderRegistry, RenderRegistryConfig, UiRegistryError,
};

/// Configuration for every bounded UI registry.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UiHostConfig {
    /// Render callback limits.
    pub render: RenderRegistryConfig,
    /// Close-on-Escape limits.
    pub escape_closing: EscapeClosingConfig,
    /// Alert queue limits.
    pub alerts: AlertQueueConfig,
    /// Quick Access limits.
    pub quick_access: QuickAccessConfig,
}

/// Complete cleanup report for one addon generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiHostCleanup {
    /// Owner gate retirement and quiescence state.
    pub retirement: OwnerRetirement,
    /// Render registrations removed across all phases.
    pub render_callbacks: usize,
    /// Close-on-Escape targets removed.
    pub escape_windows: usize,
    /// Queued alerts removed.
    pub alerts: usize,
    /// Quick Access state removed or reparented.
    pub quick_access: QuickAccessCleanup,
}

/// Renderer-independent UI host state and owner-generation coordinator.
///
/// The host must use one [`OwnerHandle`] obtained from [`Self::owner`] for all
/// registrations in an addon generation. Cleanup closes that shared gate
/// before touching any registry, so no new callback or native-pointer access
/// can race module unload.
#[derive(Debug, Default)]
pub struct UiHost {
    owners: OwnerRegistry,
    render: RenderRegistry,
    escape_closing: EscapeClosingRegistry,
    alerts: AlertQueue,
    quick_access: QuickAccessRegistry,
}

impl UiHost {
    /// Creates a host after validating every subsystem's limits.
    pub fn new(config: UiHostConfig) -> Result<Self, UiRegistryError> {
        Ok(Self {
            owners: OwnerRegistry::default(),
            render: RenderRegistry::new(config.render)?,
            escape_closing: EscapeClosingRegistry::new(config.escape_closing)?,
            alerts: AlertQueue::new(config.alerts)?,
            quick_access: QuickAccessRegistry::new(config.quick_access)?,
        })
    }

    /// Obtains the one shared lifecycle handle for an active generation.
    pub fn owner(&self, owner: OwnerGeneration) -> Result<OwnerHandle, UiRegistryError> {
        self.owners.acquire(owner)
    }

    /// Returns the render callback registry.
    #[must_use]
    pub const fn render(&self) -> &RenderRegistry {
        &self.render
    }

    /// Returns the close-on-Escape registry.
    #[must_use]
    pub const fn escape_closing(&self) -> &EscapeClosingRegistry {
        &self.escape_closing
    }

    /// Returns the alert queue.
    #[must_use]
    pub const fn alerts(&self) -> &AlertQueue {
        &self.alerts
    }

    /// Returns the Quick Access registry.
    #[must_use]
    pub const fn quick_access(&self) -> &QuickAccessRegistry {
        &self.quick_access
    }

    /// Retires an exact generation, drains other threads, and removes all of
    /// its registry state.
    ///
    /// When called reentrantly from that owner's callback, `quiescent` is
    /// false so the addon loader can defer unloading until
    /// [`Self::wait_owner_quiescent`] succeeds.
    pub fn cleanup_owner_generation(&self, owner: OwnerGeneration) -> UiHostCleanup {
        let retirement = self.owners.retire(owner);
        let (render_callbacks, render_retirement) = self.render.cleanup_owner_generation(owner);
        let (quick_access, quick_access_retirement) =
            self.quick_access.cleanup_owner_generation(owner);
        UiHostCleanup {
            retirement: retirement
                .merge(render_retirement)
                .merge(quick_access_retirement),
            render_callbacks,
            escape_windows: self.escape_closing.cleanup_owner_generation(owner),
            alerts: self.alerts.cleanup_owner_generation(owner),
            quick_access,
        }
    }

    /// Waits for a previously retired generation to reach quiescence.
    ///
    /// A native addon must not be unloaded until this returns `quiescent`.
    #[must_use]
    pub fn wait_owner_quiescent(&self, owner: OwnerGeneration) -> OwnerRetirement {
        self.owners.wait_for_quiescence(owner)
    }
}
