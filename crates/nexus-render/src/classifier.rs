//! Deterministic primary-game-swap-chain selection with explainable reasons.

use std::cmp::{Ordering, Reverse};

use crate::{Extent2D, Hwnd, SwapChainId, SwapChainObservation};

/// User-controlled and automatic-classification thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassifierConfig {
    expected_game_window: Option<Hwnd>,
    require_expected_game_window: bool,
    user_override: Option<SwapChainId>,
    minimum_extent: Extent2D,
    stale_after_sequences: u64,
}

impl ClassifierConfig {
    /// Creates automatic-classification settings.
    #[must_use]
    pub const fn new(minimum_extent: Extent2D, stale_after_sequences: u64) -> Self {
        Self {
            expected_game_window: None,
            require_expected_game_window: false,
            user_override: None,
            minimum_extent,
            stale_after_sequences,
        }
    }

    /// Returns the known game window, if discovery has supplied one.
    #[must_use]
    pub const fn expected_game_window(self) -> Option<Hwnd> {
        self.expected_game_window
    }

    /// Sets the known game window used as the strongest automatic signal.
    pub const fn set_expected_game_window(&mut self, hwnd: Option<Hwnd>) {
        self.expected_game_window = hwnd;
    }

    /// Returns whether automatic selection waits for game-window discovery.
    #[must_use]
    pub const fn requires_expected_game_window(self) -> bool {
        self.require_expected_game_window
    }

    /// Requires a discovered game window before admitting automatic candidates.
    pub const fn set_require_expected_game_window(&mut self, required: bool) {
        self.require_expected_game_window = required;
    }

    /// Returns the exact user-selected swap chain, if configured.
    #[must_use]
    pub const fn user_override(self) -> Option<SwapChainId> {
        self.user_override
    }

    /// Selects one exact observed swap chain, bypassing automatic heuristics.
    pub const fn set_user_override(&mut self, id: Option<SwapChainId>) {
        self.user_override = id;
    }

    /// Returns the smallest surface accepted by automatic classification.
    #[must_use]
    pub const fn minimum_extent(self) -> Extent2D {
        self.minimum_extent
    }

    /// Changes the smallest surface accepted by automatic classification.
    pub const fn set_minimum_extent(&mut self, minimum_extent: Extent2D) {
        self.minimum_extent = minimum_extent;
    }

    /// Returns how far a candidate may lag the latest observed presentation.
    #[must_use]
    pub const fn stale_after_sequences(self) -> u64 {
        self.stale_after_sequences
    }

    /// Changes the maximum accepted presentation lag.
    pub const fn set_stale_after_sequences(&mut self, stale_after_sequences: u64) {
        self.stale_after_sequences = stale_after_sequences;
    }
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self::new(Extent2D::new(640, 480), 8)
    }
}

/// Why an automatic candidate was excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    /// A zero-sized back buffer cannot accept overlay rendering.
    ZeroSized,
    /// The candidate has no output window for the overlay to attach to.
    MissingWindowHandle,
    /// Automatic selection is waiting for discovery of the game window.
    ExpectedWindowNotDiscovered,
    /// The candidate belongs to a different window than the discovered game window.
    UnexpectedWindow {
        /// Discovered game window required for automatic admission.
        expected: Hwnd,
        /// Candidate's reported output window.
        actual: Hwnd,
    },
    /// The surface is smaller than the configured automatic-classification floor.
    BelowMinimumExtent {
        /// Configured minimum extent.
        minimum: Extent2D,
    },
    /// No presentation has been observed for this object.
    NeverPresented,
    /// The owning window is not visible.
    InvisibleWindow,
    /// Native presentation reports that the window is occluded.
    Occluded,
    /// The candidate has fallen too far behind active presentation traffic.
    Stale {
        /// Most recent retained or caller-supplied reference sequence.
        latest_sequence: u64,
        /// Candidate's most recent sequence.
        candidate_sequence: u64,
        /// Maximum permitted lag.
        allowed_lag: u64,
    },
}

/// One excluded automatic candidate and its typed diagnostic reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateRejection {
    id: SwapChainId,
    reason: RejectionReason,
}

impl CandidateRejection {
    /// Returns the excluded swap-chain identity.
    #[must_use]
    pub const fn id(self) -> SwapChainId {
        self.id
    }

    /// Returns the exclusion reason.
    #[must_use]
    pub const fn reason(self) -> RejectionReason {
        self.reason
    }
}

/// Status of the exact user override during one classification pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideStatus {
    /// No exact selection was requested.
    NotRequested,
    /// The requested object was observed and selected.
    Applied {
        /// Selected object.
        id: SwapChainId,
    },
    /// The requested object did not appear in the current observations.
    Unavailable {
        /// Requested object.
        id: SwapChainId,
    },
    /// The requested object was present but could not accept rendering.
    Ineligible {
        /// Requested object.
        id: SwapChainId,
        /// Hard safety reason that prevented selection.
        reason: RejectionReason,
    },
}

/// The first deterministic priority that separated the winner from its runner-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionReason {
    /// The user selected this exact object.
    UserOverride,
    /// It was the only eligible automatic candidate.
    OnlyEligibleCandidate,
    /// Its output window matched the known game window.
    ExpectedGameWindow,
    /// Its output window was foreground while the runner-up was not.
    ForegroundWindow,
    /// It retained the last healthy primary selection.
    RetainedPrimary,
    /// It had the larger renderable back-buffer area.
    LargestSurface,
    /// It had presented in more adjacent observation cycles.
    LongestActiveStreak,
    /// It had the most recent global presentation sequence.
    MostRecentPresentation,
    /// It had the higher total presentation count.
    HighestPresentationCount,
    /// All behavioral signals tied, so the lower stable identity won.
    StableIdentityTieBreak,
}

/// Why no object could be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoSelectionReason {
    /// The platform layer supplied no observations.
    NoObservations,
    /// Every automatic candidate was excluded by a typed rejection reason.
    NoEligibleCandidates,
}

/// Full result and diagnostics for one deterministic classification pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    selected: Option<SwapChainId>,
    selection_reason: Option<SelectionReason>,
    no_selection_reason: Option<NoSelectionReason>,
    override_status: OverrideStatus,
    rejected: Vec<CandidateRejection>,
}

impl Classification {
    fn selected(
        id: SwapChainId,
        reason: SelectionReason,
        override_status: OverrideStatus,
        rejected: Vec<CandidateRejection>,
    ) -> Self {
        Self {
            selected: Some(id),
            selection_reason: Some(reason),
            no_selection_reason: None,
            override_status,
            rejected,
        }
    }

    fn none(
        reason: NoSelectionReason,
        override_status: OverrideStatus,
        rejected: Vec<CandidateRejection>,
    ) -> Self {
        Self {
            selected: None,
            selection_reason: None,
            no_selection_reason: Some(reason),
            override_status,
            rejected,
        }
    }

    /// Returns the selected object, if any.
    #[must_use]
    pub const fn selected_id(&self) -> Option<SwapChainId> {
        self.selected
    }

    /// Returns why the selected object beat its runner-up.
    #[must_use]
    pub const fn selection_reason(&self) -> Option<SelectionReason> {
        self.selection_reason
    }

    /// Returns why no selection was possible.
    #[must_use]
    pub const fn no_selection_reason(&self) -> Option<NoSelectionReason> {
        self.no_selection_reason
    }

    /// Returns how an exact override affected this pass.
    #[must_use]
    pub const fn override_status(&self) -> OverrideStatus {
        self.override_status
    }

    /// Returns all automatic-candidate rejections in stable identity order.
    #[must_use]
    pub fn rejected(&self) -> &[CandidateRejection] {
        &self.rejected
    }
}

/// Stateful automatic classifier with stable-primary hysteresis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimarySwapChainClassifier {
    config: ClassifierConfig,
    current: Option<SwapChainId>,
}

impl PrimarySwapChainClassifier {
    /// Creates a classifier with no prior primary selection.
    #[must_use]
    pub const fn new(config: ClassifierConfig) -> Self {
        Self {
            config,
            current: None,
        }
    }

    /// Returns the active settings.
    #[must_use]
    pub const fn config(&self) -> ClassifierConfig {
        self.config
    }

    /// Replaces settings without discarding a still-valid current selection.
    pub const fn set_config(&mut self, config: ClassifierConfig) {
        self.config = config;
    }

    /// Returns the previous pass's selected object.
    #[must_use]
    pub const fn current(&self) -> Option<SwapChainId> {
        self.current
    }

    /// Forgets hysteresis while preserving all user settings.
    pub const fn reset_current(&mut self) {
        self.current = None;
    }

    /// Selects a primary game swap chain and records typed diagnostic reasons.
    pub fn classify(&mut self, observations: &[SwapChainObservation]) -> Classification {
        self.classify_with_latest_present_sequence(observations, 0)
    }

    /// Selects a primary game swap chain against an external presentation sequence.
    ///
    /// The effective freshness reference is never lower than the newest retained
    /// observation, so a stale caller reference cannot make retained candidates
    /// appear newer than they are.
    pub fn classify_with_latest_present_sequence(
        &mut self,
        observations: &[SwapChainObservation],
        latest_present_sequence: u64,
    ) -> Classification {
        let override_status = self.try_override(observations);
        if let OverrideStatus::Applied { id } = override_status {
            self.current = Some(id);
            return Classification::selected(
                id,
                SelectionReason::UserOverride,
                override_status,
                Vec::new(),
            );
        }

        let retained_latest_sequence = observations
            .iter()
            .map(|observation| observation.activity.last_present_sequence)
            .max()
            .unwrap_or_default();
        let latest_sequence = latest_present_sequence.max(retained_latest_sequence);
        let mut rejected = Vec::new();
        let mut eligible = Vec::new();

        for observation in observations {
            if let Some(reason) = self.rejection_reason(observation, latest_sequence) {
                rejected.push(CandidateRejection {
                    id: observation.id,
                    reason,
                });
            } else {
                eligible.push(observation);
            }
        }
        rejected.sort_by_key(|candidate| candidate.id);
        eligible.sort_by(|left, right| self.compare(left, right));

        let Some(winner) = eligible.last().copied() else {
            self.current = None;
            let reason = if observations.is_empty() {
                NoSelectionReason::NoObservations
            } else {
                NoSelectionReason::NoEligibleCandidates
            };
            return Classification::none(reason, override_status, rejected);
        };
        let runner_up = eligible.iter().rev().nth(1).copied();
        let reason = runner_up.map_or_else(
            || {
                if self.matches_expected_window(winner) {
                    SelectionReason::ExpectedGameWindow
                } else {
                    SelectionReason::OnlyEligibleCandidate
                }
            },
            |runner_up| self.selection_reason(winner, runner_up),
        );

        self.current = Some(winner.id);
        Classification::selected(winner.id, reason, override_status, rejected)
    }

    fn try_override(&self, observations: &[SwapChainObservation]) -> OverrideStatus {
        let Some(id) = self.config.user_override else {
            return OverrideStatus::NotRequested;
        };
        let Some(observation) = observations.iter().find(|observation| observation.id == id) else {
            return OverrideStatus::Unavailable { id };
        };
        if observation.size.is_zero() {
            return OverrideStatus::Ineligible {
                id,
                reason: RejectionReason::ZeroSized,
            };
        }
        OverrideStatus::Applied { id }
    }

    fn rejection_reason(
        &self,
        observation: &SwapChainObservation,
        latest_sequence: u64,
    ) -> Option<RejectionReason> {
        if observation.size.is_zero() {
            return Some(RejectionReason::ZeroSized);
        }
        let Some(hwnd) = observation.hwnd else {
            return Some(RejectionReason::MissingWindowHandle);
        };
        if self.config.require_expected_game_window && self.config.expected_game_window.is_none() {
            return Some(RejectionReason::ExpectedWindowNotDiscovered);
        }
        if let Some(expected) = self.config.expected_game_window
            && hwnd != expected
        {
            return Some(RejectionReason::UnexpectedWindow {
                expected,
                actual: hwnd,
            });
        }
        if observation.size.width < self.config.minimum_extent.width
            || observation.size.height < self.config.minimum_extent.height
        {
            return Some(RejectionReason::BelowMinimumExtent {
                minimum: self.config.minimum_extent,
            });
        }
        if observation.activity.present_count == 0 {
            return Some(RejectionReason::NeverPresented);
        }
        if !observation.activity.window_visible {
            return Some(RejectionReason::InvisibleWindow);
        }
        if observation.activity.occluded {
            return Some(RejectionReason::Occluded);
        }
        if latest_sequence.saturating_sub(observation.activity.last_present_sequence)
            > self.config.stale_after_sequences
        {
            return Some(RejectionReason::Stale {
                latest_sequence,
                candidate_sequence: observation.activity.last_present_sequence,
                allowed_lag: self.config.stale_after_sequences,
            });
        }
        None
    }

    fn compare(&self, left: &SwapChainObservation, right: &SwapChainObservation) -> Ordering {
        self.rank(left).cmp(&self.rank(right))
    }

    fn rank(
        &self,
        observation: &SwapChainObservation,
    ) -> (bool, bool, u64, bool, u32, u64, u64, Reverse<SwapChainId>) {
        (
            self.matches_expected_window(observation),
            observation.activity.foreground,
            observation.size.area(),
            // Equal-window, equal-extent chains have no trustworthy semantic discriminator.
            // Retain the current identity ahead of volatile activity to avoid per-present
            // flapping; an explicit user override remains the deterministic escape hatch.
            self.current == Some(observation.id),
            observation.activity.consecutive_present_cycles,
            observation.activity.last_present_sequence,
            observation.activity.present_count,
            Reverse(observation.id),
        )
    }

    fn matches_expected_window(&self, observation: &SwapChainObservation) -> bool {
        self.config.expected_game_window.is_some()
            && observation.hwnd == self.config.expected_game_window
    }

    fn selection_reason(
        &self,
        winner: &SwapChainObservation,
        runner_up: &SwapChainObservation,
    ) -> SelectionReason {
        if self.matches_expected_window(winner) != self.matches_expected_window(runner_up) {
            SelectionReason::ExpectedGameWindow
        } else if winner.activity.foreground != runner_up.activity.foreground {
            SelectionReason::ForegroundWindow
        } else if winner.size.area() != runner_up.size.area() {
            SelectionReason::LargestSurface
        } else if (self.current == Some(winner.id)) != (self.current == Some(runner_up.id)) {
            SelectionReason::RetainedPrimary
        } else if winner.activity.consecutive_present_cycles
            != runner_up.activity.consecutive_present_cycles
        {
            SelectionReason::LongestActiveStreak
        } else if winner.activity.last_present_sequence != runner_up.activity.last_present_sequence
        {
            SelectionReason::MostRecentPresentation
        } else if winner.activity.present_count != runner_up.activity.present_count {
            SelectionReason::HighestPresentationCount
        } else {
            SelectionReason::StableIdentityTieBreak
        }
    }
}

impl Default for PrimarySwapChainClassifier {
    fn default() -> Self {
        Self::new(ClassifierConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Activity, AdapterLuid, ColorSpace, DeviceId, Extent2D, Hwnd, PresentMethod, SurfaceFormat,
        SwapChainId, SwapChainObservation,
    };

    use super::{
        CandidateRejection, ClassifierConfig, NoSelectionReason, OverrideStatus,
        PrimarySwapChainClassifier, RejectionReason, SelectionReason,
    };

    fn observation(id: u64, hwnd: usize, size: Extent2D, sequence: u64) -> SwapChainObservation {
        SwapChainObservation {
            id: SwapChainId::new(id),
            hwnd: Some(Hwnd::new(hwnd)),
            device: DeviceId::new(1),
            adapter_luid: AdapterLuid::new(2, 0),
            format: SurfaceFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            size,
            present_method: PresentMethod::Present,
            activity: Activity::active(sequence, sequence, 10),
        }
    }

    #[test]
    fn high_frequency_auxiliary_overlay_cannot_steal_game_window() {
        let mut config = ClassifierConfig::default();
        config.set_expected_game_window(Some(Hwnd::new(100)));
        let mut classifier = PrimarySwapChainClassifier::new(config);

        let mut game = observation(1, 100, Extent2D::new(2560, 1440), 100);
        game.activity.foreground = true;
        let mut nvidia_overlay = observation(2, 200, Extent2D::new(1920, 1080), 101);
        nvidia_overlay.activity.present_count = 50_000;
        nvidia_overlay.activity.consecutive_present_cycles = 500;

        let result = classifier.classify(&[nvidia_overlay, game]);

        assert_eq!(result.selected_id(), Some(SwapChainId::new(1)));
        assert_eq!(
            result.selection_reason(),
            Some(SelectionReason::ExpectedGameWindow)
        );
        assert_eq!(result.rejected().len(), 1);
        assert_eq!(
            result.rejected()[0].reason(),
            RejectionReason::UnexpectedWindow {
                expected: Hwnd::new(100),
                actual: Hwnd::new(200),
            }
        );
    }

    #[test]
    fn automatic_candidate_without_window_is_rejected_before_window_discovery() {
        let mut classifier = PrimarySwapChainClassifier::default();
        let mut windowless = observation(1, 100, Extent2D::new(1920, 1080), 10);
        windowless.hwnd = None;

        let result = classifier.classify(&[windowless]);

        assert_eq!(result.selected_id(), None);
        assert_eq!(
            result.no_selection_reason(),
            Some(NoSelectionReason::NoEligibleCandidates)
        );
        assert_eq!(result.rejected().len(), 1);
        assert_eq!(
            result.rejected()[0].reason(),
            RejectionReason::MissingWindowHandle
        );
    }

    #[test]
    fn expected_window_discovery_policy_is_opt_in() {
        let config = ClassifierConfig::default();
        assert!(!config.requires_expected_game_window());
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let candidate = observation(1, 200, Extent2D::new(1920, 1080), 10);

        let result = classifier.classify(&[candidate]);

        assert_eq!(result.selected_id(), Some(SwapChainId::new(1)));
        assert_eq!(
            result.selection_reason(),
            Some(SelectionReason::OnlyEligibleCandidate)
        );
    }

    #[test]
    fn required_window_discovery_waits_then_selects_expected_chain() {
        let mut config = ClassifierConfig::default();
        config.set_require_expected_game_window(true);
        assert!(config.requires_expected_game_window());
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let auxiliary = observation(1, 200, Extent2D::new(2560, 1440), 10);

        let waiting = classifier.classify(std::slice::from_ref(&auxiliary));

        assert_eq!(waiting.selected_id(), None);
        assert_eq!(
            waiting.no_selection_reason(),
            Some(NoSelectionReason::NoEligibleCandidates)
        );
        assert_eq!(waiting.rejected().len(), 1);
        assert_eq!(
            waiting.rejected()[0].reason(),
            RejectionReason::ExpectedWindowNotDiscovered
        );

        let mut discovered = classifier.config();
        discovered.set_expected_game_window(Some(Hwnd::new(100)));
        classifier.set_config(discovered);
        let game = observation(2, 100, Extent2D::new(1920, 1080), 11);
        let selected = classifier.classify(&[auxiliary, game]);

        assert_eq!(selected.selected_id(), Some(SwapChainId::new(2)));
        assert_eq!(
            selected.selection_reason(),
            Some(SelectionReason::ExpectedGameWindow)
        );
        assert_eq!(selected.rejected().len(), 1);
        assert_eq!(
            selected.rejected()[0].reason(),
            RejectionReason::UnexpectedWindow {
                expected: Hwnd::new(100),
                actual: Hwnd::new(200),
            }
        );
    }

    #[test]
    fn auxiliary_windows_remain_rejected_until_expected_chain_arrives() {
        let mut config = ClassifierConfig::default();
        config.set_expected_game_window(Some(Hwnd::new(100)));
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let mut windowless = observation(1, 100, Extent2D::new(1920, 1080), 10);
        windowless.hwnd = None;
        let auxiliary = observation(2, 200, Extent2D::new(2560, 1440), 11);

        let waiting = classifier.classify(&[auxiliary.clone(), windowless.clone()]);

        assert_eq!(waiting.selected_id(), None);
        assert_eq!(waiting.rejected().len(), 2);
        assert_eq!(
            waiting.rejected()[0].reason(),
            RejectionReason::MissingWindowHandle
        );
        assert_eq!(
            waiting.rejected()[1].reason(),
            RejectionReason::UnexpectedWindow {
                expected: Hwnd::new(100),
                actual: Hwnd::new(200),
            }
        );

        let game = observation(3, 100, Extent2D::new(1920, 1080), 12);
        let admitted = classifier.classify(&[windowless, auxiliary, game]);

        assert_eq!(admitted.selected_id(), Some(SwapChainId::new(3)));
        assert_eq!(
            admitted.selection_reason(),
            Some(SelectionReason::ExpectedGameWindow)
        );
        assert_eq!(admitted.rejected().len(), 2);
    }

    #[test]
    fn same_window_equal_extent_retains_current_until_explicit_override() {
        let mut config = ClassifierConfig::default();
        config.set_expected_game_window(Some(Hwnd::new(100)));
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let primary = observation(3, 100, Extent2D::new(1920, 1080), 20);
        let initial = classifier.classify(std::slice::from_ref(&primary));
        assert_eq!(initial.selected_id(), Some(SwapChainId::new(3)));

        let mut newer = observation(4, 100, Extent2D::new(1920, 1080), 21);
        newer.activity.present_count = 10_000;
        newer.activity.consecutive_present_cycles = 1_000;
        let retained = classifier.classify(&[newer.clone(), primary.clone()]);

        assert_eq!(retained.selected_id(), Some(SwapChainId::new(3)));
        assert_eq!(
            retained.selection_reason(),
            Some(SelectionReason::RetainedPrimary)
        );

        let mut overridden = classifier.config();
        overridden.set_user_override(Some(SwapChainId::new(4)));
        classifier.set_config(overridden);
        let explicit = classifier.classify(&[newer, primary]);
        assert_eq!(explicit.selected_id(), Some(SwapChainId::new(4)));
        assert_eq!(
            explicit.selection_reason(),
            Some(SelectionReason::UserOverride)
        );
    }

    #[test]
    fn stronger_game_surface_replaces_same_window_auxiliary_without_activity_flapping() {
        let mut config = ClassifierConfig::default();
        config.set_expected_game_window(Some(Hwnd::new(100)));
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let auxiliary = observation(3, 100, Extent2D::new(640, 480), 20);
        let initial = classifier.classify(std::slice::from_ref(&auxiliary));
        assert_eq!(initial.selected_id(), Some(SwapChainId::new(3)));

        let mut game = observation(4, 100, Extent2D::new(1920, 1080), 21);
        game.activity.consecutive_present_cycles = 1;
        game.activity.present_count = 1;
        let recovered = classifier.classify(&[auxiliary, game]);

        assert_eq!(recovered.selected_id(), Some(SwapChainId::new(4)));
        assert_eq!(
            recovered.selection_reason(),
            Some(SelectionReason::LargestSurface)
        );
    }

    #[test]
    fn current_primary_stays_selected_when_activity_temporarily_ties_or_fluctuates() {
        let mut classifier = PrimarySwapChainClassifier::default();
        let first = observation(7, 100, Extent2D::new(1920, 1080), 10);
        let second = observation(8, 200, Extent2D::new(1920, 1080), 10);
        let initial = classifier.classify(&[second.clone(), first.clone()]);
        assert_eq!(initial.selected_id(), Some(SwapChainId::new(7)));

        let mut noisy_auxiliary = second;
        noisy_auxiliary.activity.last_present_sequence = 11;
        noisy_auxiliary.activity.present_count = 1_000;
        let retained = classifier.classify(&[first, noisy_auxiliary]);

        assert_eq!(retained.selected_id(), Some(SwapChainId::new(7)));
        assert_eq!(
            retained.selection_reason(),
            Some(SelectionReason::RetainedPrimary)
        );
    }

    #[test]
    fn explicit_override_bypasses_automatic_window_admission() {
        let mut config = ClassifierConfig::default();
        config.set_require_expected_game_window(true);
        config.set_user_override(Some(SwapChainId::new(2)));
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let game = observation(1, 100, Extent2D::new(2560, 1440), 100);
        let mut auxiliary = observation(2, 200, Extent2D::new(800, 600), 1);
        auxiliary.hwnd = None;
        auxiliary.activity.window_visible = false;
        auxiliary.activity.occluded = true;

        let result = classifier.classify(&[game, auxiliary]);

        assert_eq!(result.selected_id(), Some(SwapChainId::new(2)));
        assert_eq!(
            result.selection_reason(),
            Some(SelectionReason::UserOverride)
        );
        assert_eq!(
            result.override_status(),
            OverrideStatus::Applied {
                id: SwapChainId::new(2)
            }
        );
    }

    #[test]
    fn stale_old_primary_yields_to_recreated_game_swap_chain() {
        let mut config = ClassifierConfig::default();
        config.set_expected_game_window(Some(Hwnd::new(100)));
        config.set_stale_after_sequences(2);
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let old = observation(1, 100, Extent2D::new(1920, 1080), 10);
        let _ = classifier.classify(std::slice::from_ref(&old));

        let recreated = observation(3, 100, Extent2D::new(2560, 1440), 20);
        let result = classifier.classify(&[old, recreated]);

        assert_eq!(result.selected_id(), Some(SwapChainId::new(3)));
        assert_eq!(result.rejected().len(), 1);
    }

    #[test]
    fn filtered_recent_primary_sequence_keeps_old_auxiliary_stale() {
        let mut config = ClassifierConfig::default();
        config.set_stale_after_sequences(2);
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let recent_primary = observation(1, 100, Extent2D::new(2560, 1440), 20);
        let old_auxiliary = observation(2, 200, Extent2D::new(800, 600), 10);

        let result = classifier.classify_with_latest_present_sequence(
            std::slice::from_ref(&old_auxiliary),
            recent_primary.activity.last_present_sequence,
        );

        assert_eq!(result.selected_id(), None);
        assert_eq!(
            result.no_selection_reason(),
            Some(NoSelectionReason::NoEligibleCandidates)
        );
        assert_eq!(
            result.rejected(),
            &[CandidateRejection {
                id: SwapChainId::new(2),
                reason: RejectionReason::Stale {
                    latest_sequence: 20,
                    candidate_sequence: 10,
                    allowed_lag: 2,
                },
            }]
        );
    }

    #[test]
    fn external_sequence_below_retained_maximum_cannot_reduce_freshness_reference() {
        let mut config = ClassifierConfig::default();
        config.set_stale_after_sequences(2);
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let old_auxiliary = observation(2, 200, Extent2D::new(800, 600), 10);
        let recent_primary = observation(3, 100, Extent2D::new(2560, 1440), 20);

        let result =
            classifier.classify_with_latest_present_sequence(&[old_auxiliary, recent_primary], 5);

        assert_eq!(result.selected_id(), Some(SwapChainId::new(3)));
        assert_eq!(
            result.rejected(),
            &[CandidateRejection {
                id: SwapChainId::new(2),
                reason: RejectionReason::Stale {
                    latest_sequence: 20,
                    candidate_sequence: 10,
                    allowed_lag: 2,
                },
            }]
        );
    }
}
