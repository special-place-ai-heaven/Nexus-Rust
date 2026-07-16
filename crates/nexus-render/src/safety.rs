//! User-controlled render stages and per-stage failure containment.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

/// Hook strategy selected by the user or automatic compatibility logic.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SafeMode {
    /// Start with per-object interception and let the platform layer fall back safely.
    #[default]
    Automatic,
    /// Intercept only the concrete factory and swap-chain objects Nexus observes.
    PerObjectHooks,
    /// Intercept swap-chain creation through the concrete DXGI factory object.
    FactoryHooks,
    /// Use the compatibility-oriented global hook fallback.
    GlobalHookFallback,
    /// Observe lifecycle and presentations without issuing overlay draw calls.
    ObserveOnly,
    /// Install no graphics hooks; the proxy continues forwarding native calls.
    Off,
}

impl SafeMode {
    /// Returns whether this mode permits the platform layer to install observation hooks.
    #[must_use]
    pub const fn permits_hooks(self) -> bool {
        !matches!(self, Self::Off)
    }

    const fn stage_ceiling(self, requested: RenderStage) -> RenderStage {
        match self {
            Self::ObserveOnly => RenderStage::HooksOnly,
            Self::Off => RenderStage::ProxyOnly,
            Self::Automatic
            | Self::PerObjectHooks
            | Self::FactoryHooks
            | Self::GlobalHookFallback => requested,
        }
    }
}

/// Progressively riskier initialization and rendering stages.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum RenderStage {
    /// Load Nexus as a transparent system-DLL proxy only.
    #[default]
    ProxyOnly = 0,
    /// Observe creation and presentation, but issue no GPU work.
    HooksOnly = 1,
    /// Perform a minimal, isolated render probe.
    RenderProbe = 2,
    /// Render only Nexus-owned core UI.
    CoreUi = 3,
    /// Render core UI and third-party add-ons.
    Addons = 4,
}

/// Runtime controls that cap both interception strategy and render scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderControls {
    safe_mode: SafeMode,
    max_stage: RenderStage,
}

impl RenderControls {
    /// Creates controls with the requested mode and maximum stage.
    #[must_use]
    pub const fn new(safe_mode: SafeMode, max_stage: RenderStage) -> Self {
        Self {
            safe_mode,
            max_stage,
        }
    }

    /// Returns the configured interception mode.
    #[must_use]
    pub const fn safe_mode(self) -> SafeMode {
        self.safe_mode
    }

    /// Changes the interception mode without changing the user's stage cap.
    pub const fn set_safe_mode(&mut self, safe_mode: SafeMode) {
        self.safe_mode = safe_mode;
    }

    /// Returns the user-requested maximum stage.
    #[must_use]
    pub const fn max_stage(self) -> RenderStage {
        self.max_stage
    }

    /// Changes the maximum stage the runtime may attempt.
    pub const fn set_max_stage(&mut self, max_stage: RenderStage) {
        self.max_stage = max_stage;
    }

    /// Returns the effective stage after applying safe-mode restrictions.
    #[must_use]
    pub const fn effective_stage(self) -> RenderStage {
        self.safe_mode.stage_ceiling(self.max_stage)
    }

    /// Returns whether the effective controls allow a stage to execute.
    #[must_use]
    pub const fn permits(self, stage: RenderStage) -> bool {
        (stage as u8) <= (self.effective_stage() as u8)
    }
}

impl Default for RenderControls {
    fn default() -> Self {
        Self::new(SafeMode::Automatic, RenderStage::Addons)
    }
}

/// Thresholds for independently suppressing a failing render stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailurePolicy {
    failures_before_cooldown: NonZeroU32,
    cooldown_sequences: u64,
    cooldowns_before_disable: NonZeroU32,
}

impl FailurePolicy {
    /// Creates a validated policy; zero thresholds are impossible by construction.
    #[must_use]
    pub const fn new(
        failures_before_cooldown: NonZeroU32,
        cooldown_sequences: u64,
        cooldowns_before_disable: NonZeroU32,
    ) -> Self {
        Self {
            failures_before_cooldown,
            cooldown_sequences,
            cooldowns_before_disable,
        }
    }

    /// Returns the consecutive-failure threshold for starting a cooldown.
    #[must_use]
    pub const fn failures_before_cooldown(self) -> NonZeroU32 {
        self.failures_before_cooldown
    }

    /// Returns the monotonic presentation sequences to wait before retrying.
    #[must_use]
    pub const fn cooldown_sequences(self) -> u64 {
        self.cooldown_sequences
    }

    /// Returns how many failed cooldown cycles permanently disable a stage.
    #[must_use]
    pub const fn cooldowns_before_disable(self) -> NonZeroU32 {
        self.cooldowns_before_disable
    }
}

impl Default for FailurePolicy {
    fn default() -> Self {
        Self::new(
            NonZeroU32::new(3).expect("three is non-zero"),
            120,
            NonZeroU32::new(3).expect("three is non-zero"),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StageStatus {
    Healthy,
    CoolingDown { retry_at_sequence: u64 },
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StageRecord {
    consecutive_failures: u32,
    cooldowns: u32,
    status: StageStatus,
}

impl Default for StageRecord {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            cooldowns: 0,
            status: StageStatus::Healthy,
        }
    }
}

/// Public diagnostic state for one isolated render stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageFailureState {
    /// The stage may run; the count records failures before the next cooldown.
    Healthy {
        /// Consecutive failures in the current attempt cycle.
        consecutive_failures: u32,
        /// Failed cooldown cycles since the last success or manual reset.
        cooldowns: u32,
    },
    /// The stage is suppressed until the monotonic presentation sequence is reached.
    CoolingDown {
        /// First sequence at which one retry may run.
        retry_at_sequence: u64,
        /// Failed cooldown cycles since the last success or manual reset.
        cooldowns: u32,
    },
    /// The stage stays suppressed until the user or lifecycle code resets it.
    Disabled {
        /// Number of cooldown cycles that caused the disable.
        cooldowns: u32,
    },
}

impl From<StageRecord> for StageFailureState {
    fn from(record: StageRecord) -> Self {
        match record.status {
            StageStatus::Healthy => Self::Healthy {
                consecutive_failures: record.consecutive_failures,
                cooldowns: record.cooldowns,
            },
            StageStatus::CoolingDown { retry_at_sequence } => Self::CoolingDown {
                retry_at_sequence,
                cooldowns: record.cooldowns,
            },
            StageStatus::Disabled => Self::Disabled {
                cooldowns: record.cooldowns,
            },
        }
    }
}

/// Whether a stage should run at the current presentation sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptPermission {
    /// Run the ordinary attempt path.
    Attempt,
    /// Run exactly one retry after a completed cooldown.
    Retry,
    /// Skip the stage until the recorded sequence.
    CoolingDown {
        /// First sequence at which the caller should poll again.
        retry_at_sequence: u64,
    },
    /// Skip the stage until an explicit reset or a new swap-chain generation.
    Disabled,
}

/// State transition emitted after recording a stage failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureAction {
    /// Keep attempting; the threshold has not yet been reached.
    Continue {
        /// Additional consecutive failures required to start a cooldown.
        failures_remaining: u32,
    },
    /// Suppress this stage temporarily while all other stages retain their own state.
    CooldownStarted {
        /// First presentation sequence at which a retry is permitted.
        retry_at_sequence: u64,
        /// One-based failed cooldown cycle number.
        cooldown: u32,
    },
    /// Suppress this stage until an explicit reset or generation change.
    Disabled {
        /// Number of failed cooldown cycles that triggered the disable.
        cooldowns: u32,
    },
    /// Ignore a failure reported while the stage was already suppressed.
    AlreadySuppressed {
        /// Current state explaining why no transition occurred.
        state: StageFailureState,
    },
}

/// Independent failure budgets for each stage of one swap-chain generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureController {
    policy: FailurePolicy,
    records: BTreeMap<RenderStage, StageRecord>,
}

impl FailureController {
    /// Creates an empty controller using the supplied thresholds.
    #[must_use]
    pub const fn new(policy: FailurePolicy) -> Self {
        Self {
            policy,
            records: BTreeMap::new(),
        }
    }

    /// Returns the immutable policy used by this generation.
    #[must_use]
    pub const fn policy(&self) -> FailurePolicy {
        self.policy
    }

    /// Returns a diagnostic snapshot for one stage.
    #[must_use]
    pub fn state(&self, stage: RenderStage) -> StageFailureState {
        self.records.get(&stage).copied().unwrap_or_default().into()
    }

    /// Polls whether one stage may execute, opening one retry when cooldown expires.
    pub fn permission(&mut self, stage: RenderStage, present_sequence: u64) -> AttemptPermission {
        let Some(record) = self.records.get_mut(&stage) else {
            return AttemptPermission::Attempt;
        };

        match record.status {
            StageStatus::Healthy => AttemptPermission::Attempt,
            StageStatus::CoolingDown { retry_at_sequence }
                if present_sequence >= retry_at_sequence =>
            {
                record.status = StageStatus::Healthy;
                record.consecutive_failures = 0;
                AttemptPermission::Retry
            }
            StageStatus::CoolingDown { retry_at_sequence } => {
                AttemptPermission::CoolingDown { retry_at_sequence }
            }
            StageStatus::Disabled => AttemptPermission::Disabled,
        }
    }

    /// Records a failed attempt for only the specified stage.
    pub fn record_failure(&mut self, stage: RenderStage, present_sequence: u64) -> FailureAction {
        let record = self.records.entry(stage).or_default();

        match record.status {
            StageStatus::CoolingDown { retry_at_sequence }
                if present_sequence < retry_at_sequence =>
            {
                return FailureAction::AlreadySuppressed {
                    state: (*record).into(),
                };
            }
            StageStatus::Disabled => {
                return FailureAction::AlreadySuppressed {
                    state: (*record).into(),
                };
            }
            StageStatus::CoolingDown { .. } => {
                record.status = StageStatus::Healthy;
                record.consecutive_failures = 0;
            }
            StageStatus::Healthy => {}
        }

        record.consecutive_failures = record.consecutive_failures.saturating_add(1);
        let threshold = self.policy.failures_before_cooldown.get();
        if record.consecutive_failures < threshold {
            return FailureAction::Continue {
                failures_remaining: threshold - record.consecutive_failures,
            };
        }

        record.consecutive_failures = 0;
        record.cooldowns = record.cooldowns.saturating_add(1);
        if record.cooldowns >= self.policy.cooldowns_before_disable.get() {
            record.status = StageStatus::Disabled;
            return FailureAction::Disabled {
                cooldowns: record.cooldowns,
            };
        }

        let retry_at_sequence = present_sequence.saturating_add(self.policy.cooldown_sequences);
        record.status = StageStatus::CoolingDown { retry_at_sequence };
        FailureAction::CooldownStarted {
            retry_at_sequence,
            cooldown: record.cooldowns,
        }
    }

    /// Clears all failure history for a successful stage execution.
    pub fn record_success(&mut self, stage: RenderStage) -> bool {
        self.records.remove(&stage).is_some()
    }

    /// Manually resets one stage and returns whether any failure history existed.
    pub fn reset_stage(&mut self, stage: RenderStage) -> bool {
        self.records.remove(&stage).is_some()
    }

    /// Manually resets every isolated stage.
    pub fn reset_all(&mut self) {
        self.records.clear();
    }
}

impl Default for FailureController {
    fn default() -> Self {
        Self::new(FailurePolicy::default())
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::{
        AttemptPermission, FailureAction, FailureController, FailurePolicy, RenderControls,
        RenderStage, SafeMode, StageFailureState,
    };

    fn policy() -> FailurePolicy {
        FailurePolicy::new(
            NonZeroU32::new(2).expect("two is non-zero"),
            5,
            NonZeroU32::new(2).expect("two is non-zero"),
        )
    }

    #[test]
    fn safe_modes_cap_render_stages_without_stopping_proxy_forwarding() {
        let off = RenderControls::new(SafeMode::Off, RenderStage::Addons);
        assert!(off.permits(RenderStage::ProxyOnly));
        assert!(!off.permits(RenderStage::HooksOnly));

        let observe = RenderControls::new(SafeMode::ObserveOnly, RenderStage::Addons);
        assert!(observe.permits(RenderStage::HooksOnly));
        assert!(!observe.permits(RenderStage::RenderProbe));
    }

    #[test]
    fn repeated_failures_cool_down_retry_and_then_disable_only_that_stage() {
        let mut controller = FailureController::new(policy());

        assert_eq!(
            controller.record_failure(RenderStage::Addons, 10),
            FailureAction::Continue {
                failures_remaining: 1
            }
        );
        assert_eq!(
            controller.record_failure(RenderStage::Addons, 11),
            FailureAction::CooldownStarted {
                retry_at_sequence: 16,
                cooldown: 1
            }
        );
        assert_eq!(
            controller.permission(RenderStage::Addons, 15),
            AttemptPermission::CoolingDown {
                retry_at_sequence: 16
            }
        );
        assert_eq!(
            controller.permission(RenderStage::CoreUi, 15),
            AttemptPermission::Attempt
        );
        assert_eq!(
            controller.permission(RenderStage::Addons, 16),
            AttemptPermission::Retry
        );

        assert!(matches!(
            controller.record_failure(RenderStage::Addons, 16),
            FailureAction::Continue { .. }
        ));
        assert_eq!(
            controller.record_failure(RenderStage::Addons, 17),
            FailureAction::Disabled { cooldowns: 2 }
        );
        assert_eq!(
            controller.permission(RenderStage::Addons, u64::MAX),
            AttemptPermission::Disabled
        );
        assert_eq!(
            controller.state(RenderStage::CoreUi),
            StageFailureState::Healthy {
                consecutive_failures: 0,
                cooldowns: 0
            }
        );
    }

    #[test]
    fn success_fully_restores_a_stage_failure_budget() {
        let mut controller = FailureController::new(policy());
        let _ = controller.record_failure(RenderStage::CoreUi, 1);

        assert!(controller.record_success(RenderStage::CoreUi));
        assert_eq!(
            controller.state(RenderStage::CoreUi),
            StageFailureState::Healthy {
                consecutive_failures: 0,
                cooldowns: 0
            }
        );
    }
}
