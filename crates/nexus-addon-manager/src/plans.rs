use nexus_addon_loader::AbsoluteDllPath;
use nexus_core::OwnerToken;

/// Whether an update plan may proceed without additional user consent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateConsent {
    /// Updates are disabled for this add-on.
    Disabled,
    /// Background mode permits checking or staging, but not replacement.
    StageOnly,
    /// Replacement requires explicit user confirmation.
    ConfirmationRequired,
    /// Policy permits automatic replacement.
    Automatic,
}

/// When a staged update can safely replace the current binary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateTiming {
    /// The module can be drained and replaced during this process.
    RuntimeHotReload,
    /// A locked or launch-only module must be replaced before the next launch.
    RestartRequired,
    /// No module is active, so replacement can precede a later activation.
    BeforeNextActivation,
}

/// Semantic update operation for an external filesystem executor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateStep {
    /// Explicitly unload and drain one generation.
    RequestUnload(OwnerToken),
    /// Replace the target only after the active generation is released.
    ReplaceAfterUnload {
        /// Already staged replacement DLL.
        staged: AbsoluteDllPath,
        /// Current managed DLL path.
        target: AbsoluteDllPath,
    },
    /// Defer replacement until no process mapping can hold the target open.
    ReplaceOnRestart {
        /// Already staged replacement DLL.
        staged: AbsoluteDllPath,
        /// Current managed DLL path.
        target: AbsoluteDllPath,
    },
    /// Replace an inactive binary immediately.
    ReplaceInactive {
        /// Already staged replacement DLL.
        staged: AbsoluteDllPath,
        /// Current managed DLL path.
        target: AbsoluteDllPath,
    },
    /// Rescan the directory after external replacement succeeds.
    Rescan,
    /// Inspect and activate the replacement as a new hot-reload generation.
    ActivateHotReload,
}

/// Deterministic, non-executing update plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdatePlan {
    owner: OwnerToken,
    consent: UpdateConsent,
    timing: UpdateTiming,
    steps: Vec<UpdateStep>,
}

impl UpdatePlan {
    pub(crate) fn new(
        owner: OwnerToken,
        consent: UpdateConsent,
        timing: UpdateTiming,
        steps: Vec<UpdateStep>,
    ) -> Self {
        Self {
            owner,
            consent,
            timing,
            steps,
        }
    }

    /// Returns the generation for which the plan was produced.
    #[must_use]
    pub const fn owner(&self) -> OwnerToken {
        self.owner
    }

    /// Returns the required consent level.
    #[must_use]
    pub const fn consent(&self) -> UpdateConsent {
        self.consent
    }

    /// Returns the safe application timing.
    #[must_use]
    pub const fn timing(&self) -> UpdateTiming {
        self.timing
    }

    /// Borrows the ordered semantic operations.
    #[must_use]
    pub fn steps(&self) -> &[UpdateStep] {
        &self.steps
    }
}

/// When an add-on binary can be removed safely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UninstallTiming {
    /// The active generation can be drained before deletion.
    RuntimeAfterUnload,
    /// A locked module requires deferred deletion or a tombstone move.
    RestartRequired,
    /// The binary is not mapped and may be removed immediately.
    Immediate,
}

/// Semantic uninstall operation for an external filesystem executor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UninstallStep {
    /// Explicitly unload and drain one generation.
    RequestUnload(OwnerToken),
    /// Remove the binary after the generation is released.
    RemoveAfterUnload(AbsoluteDllPath),
    /// Move or remove the binary during the next process start.
    RemoveOnRestart(AbsoluteDllPath),
    /// Remove an inactive binary immediately.
    RemoveInactive(AbsoluteDllPath),
    /// Delete every persisted config entry for the signature.
    RemoveConfig(u32),
    /// Rescan after the external filesystem operation succeeds.
    Rescan,
}

/// Deterministic, non-executing uninstall plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UninstallPlan {
    owner: OwnerToken,
    timing: UninstallTiming,
    steps: Vec<UninstallStep>,
}

impl UninstallPlan {
    pub(crate) fn new(
        owner: OwnerToken,
        timing: UninstallTiming,
        steps: Vec<UninstallStep>,
    ) -> Self {
        Self {
            owner,
            timing,
            steps,
        }
    }

    /// Returns the generation for which the plan was produced.
    #[must_use]
    pub const fn owner(&self) -> OwnerToken {
        self.owner
    }

    /// Returns the safe removal timing.
    #[must_use]
    pub const fn timing(&self) -> UninstallTiming {
        self.timing
    }

    /// Borrows the ordered semantic operations.
    #[must_use]
    pub fn steps(&self) -> &[UninstallStep] {
        &self.steps
    }
}
