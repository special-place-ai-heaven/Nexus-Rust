use core::fmt;

use nexus_render::{
    Classification, ColorSpace, Extent2D, NoSelectionReason, OverrideStatus, PresentMethod,
    RejectionReason, RenderStage, SelectionReason, SessionGeneration, SurfaceFormat,
};

/// Supported factory interface layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FactoryInterface {
    /// `IDXGIFactory`.
    Base = 0,
    /// `IDXGIFactory1`.
    V1 = 1,
    /// `IDXGIFactory2`.
    V2 = 2,
    /// `IDXGIFactory3`.
    V3 = 3,
    /// `IDXGIFactory4`.
    V4 = 4,
    /// `IDXGIFactory5`.
    V5 = 5,
    /// `IDXGIFactory6`.
    V6 = 6,
    /// `IDXGIFactory7`.
    V7 = 7,
    /// `IDXGIFactoryMedia`, an independent `IUnknown`-derived interface.
    Media = 8,
}

/// Supported inherited swap-chain interface layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum SwapChainInterface {
    /// `IDXGISwapChain`.
    Base = 0,
    /// `IDXGISwapChain1`.
    V1 = 1,
    /// `IDXGISwapChain2`.
    V2 = 2,
    /// `IDXGISwapChain3`.
    V3 = 3,
    /// `IDXGISwapChain4`.
    V4 = 4,
}

/// Native object family involved in an interception event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    /// A concrete DXGI factory interface.
    Factory,
    /// A concrete DXGI swap-chain interface.
    SwapChain,
}

/// Result of an explicit attachment request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachOutcome {
    /// One or more concrete interface pointers were newly intercepted.
    Attached {
        /// Number of per-instance vtables published by this request.
        interfaces: u32,
    },
    /// This manager already owned the concrete interface.
    AlreadyAttached,
}

/// Extern boundary at which a Rust panic was contained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    /// `IUnknown::QueryInterface` on a factory.
    FactoryQueryInterface,
    /// A factory swap-chain creation method.
    FactoryCreateSwapChain,
    /// `IUnknown::QueryInterface` on a swap chain.
    SwapChainQueryInterface,
    /// `Present` or `Present1`.
    Present,
    /// `ResizeBuffers` or `ResizeBuffers1`.
    ResizeBuffers,
    /// `IDXGISwapChain3::SetColorSpace1`.
    SetColorSpace1,
    /// A user-provided diagnostic or observation callback.
    ObserverCallback,
    /// A user-provided overlay-render callback.
    RendererCallback,
}

/// Metadata that could not be authoritatively sampled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationField {
    /// The output window.
    Window,
    /// The D3D/DXGI device identity.
    Device,
    /// The adapter LUID.
    Adapter,
    /// The back-buffer extent or format.
    Surface,
    /// The active color space, which DXGI cannot query directly.
    ColorSpace,
}

/// Closed classification of a forwarded HRESULT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HResultDisposition {
    /// The native call returned success.
    Success,
    /// Presentation reported that the target was occluded.
    Occluded,
    /// The graphics device was removed.
    DeviceRemoved,
    /// The graphics device was reset.
    DeviceReset,
    /// Another native status or failure was returned.
    Other(i32),
}

/// Redaction-safe category for one automatically rejected render candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RenderCandidateRejection {
    /// The back buffer had no renderable extent.
    ZeroSized,
    /// No output window was available.
    MissingWindowHandle,
    /// Automatic admission is waiting for game-window discovery.
    ExpectedWindowNotDiscovered,
    /// The candidate belonged to a window other than the discovered game window.
    UnexpectedWindow,
    /// The surface was below the configured automatic-selection floor.
    BelowMinimumExtent,
    /// The candidate had not presented yet.
    NeverPresented,
    /// The owning window was not visible.
    InvisibleWindow,
    /// Native presentation reported occlusion.
    Occluded,
    /// The candidate had fallen behind active presentation traffic.
    Stale,
}

impl From<RejectionReason> for RenderCandidateRejection {
    fn from(reason: RejectionReason) -> Self {
        match reason {
            RejectionReason::ZeroSized => Self::ZeroSized,
            RejectionReason::MissingWindowHandle => Self::MissingWindowHandle,
            RejectionReason::ExpectedWindowNotDiscovered => Self::ExpectedWindowNotDiscovered,
            RejectionReason::UnexpectedWindow { .. } => Self::UnexpectedWindow,
            RejectionReason::BelowMinimumExtent { .. } => Self::BelowMinimumExtent,
            RejectionReason::NeverPresented => Self::NeverPresented,
            RejectionReason::InvisibleWindow => Self::InvisibleWindow,
            RejectionReason::Occluded => Self::Occluded,
            RejectionReason::Stale { .. } => Self::Stale,
        }
    }
}

impl fmt::Display for RenderCandidateRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroSized => "zero-sized surface",
            Self::MissingWindowHandle => "missing window handle",
            Self::ExpectedWindowNotDiscovered => "game window not discovered",
            Self::UnexpectedWindow => "unexpected window",
            Self::BelowMinimumExtent => "surface below minimum extent",
            Self::NeverPresented => "candidate never presented",
            Self::InvisibleWindow => "window not visible",
            Self::Occluded => "window occluded",
            Self::Stale => "stale presentation activity",
        })
    }
}

/// Why an exact user override could not provide the primary render chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderOverrideFailure {
    /// The requested swap-chain identity was not observed.
    Unavailable,
    /// The requested chain was observed but withheld by failure policy.
    FailurePolicySuppressed(RenderFailurePolicySuppression),
    /// The requested chain was observed but failed a hard safety check.
    Ineligible {
        /// Redaction-safe rejection category; the requested identity is omitted.
        reason: RenderCandidateRejection,
    },
}

/// Closed summary of the render candidates withheld by failure policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderFailurePolicySuppression {
    /// Every suppressed candidate is waiting for its next permitted retry.
    CoolingDown,
    /// Every suppressed candidate has exhausted its permitted retries.
    Disabled,
    /// Some candidates are cooling down while others are disabled.
    CoolingDownAndDisabled,
}

/// Closed reason that automatic primary selection could not proceed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderSelectionFailure {
    /// The platform supplied no swap-chain observations.
    NoObservations,
    /// Every observed automatic candidate was rejected.
    NoEligibleCandidates,
    /// Failure policy withheld candidates and no remaining candidate was selected.
    FailurePolicySuppressed(RenderFailurePolicySuppression),
}

/// Redaction-safe result of a successful primary-chain classification.
///
/// This records the classifier's closed selection reason and any redacted exact-
/// override failure that preceded automatic fallback. It deliberately omits the
/// selected swap-chain identity so it can be emitted even when the currently
/// presenting chain is not the selected chain and no render callback will run
/// for this presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderSelectionResolved {
    reason: SelectionReason,
    override_failure: Option<RenderOverrideFailure>,
}

impl RenderSelectionResolved {
    /// Creates a resolved state from a closed classifier reason.
    #[must_use]
    pub const fn new(reason: SelectionReason) -> Self {
        Self {
            reason,
            override_failure: None,
        }
    }

    /// Converts a successful classification into a redaction-safe state.
    #[must_use]
    pub fn from_classification(classification: &Classification) -> Option<Self> {
        Self::from_classification_with_override_suppression(classification, None)
    }

    pub(crate) fn from_classification_with_override_suppression(
        classification: &Classification,
        override_suppression: Option<RenderFailurePolicySuppression>,
    ) -> Option<Self> {
        classification.selected_id()?;
        Some(Self {
            reason: classification.selection_reason()?,
            override_failure: redacted_override_failure(
                classification.override_status(),
                override_suppression,
            ),
        })
    }

    /// Returns the closed reason that selected the primary chain.
    #[must_use]
    pub const fn reason(self) -> SelectionReason {
        self.reason
    }

    /// Returns the redacted exact-override failure that automatic fallback recovered from.
    #[must_use]
    pub const fn override_failure(self) -> Option<RenderOverrideFailure> {
        self.override_failure
    }
}

fn redacted_override_failure(
    status: OverrideStatus,
    override_suppression: Option<RenderFailurePolicySuppression>,
) -> Option<RenderOverrideFailure> {
    if let Some(suppression) = override_suppression {
        debug_assert!(matches!(status, OverrideStatus::Unavailable { .. }));
        return Some(RenderOverrideFailure::FailurePolicySuppressed(suppression));
    }
    match status {
        OverrideStatus::NotRequested | OverrideStatus::Applied { .. } => None,
        OverrideStatus::Unavailable { .. } => Some(RenderOverrideFailure::Unavailable),
        OverrideStatus::Ineligible { reason, .. } => Some(RenderOverrideFailure::Ineligible {
            reason: reason.into(),
        }),
    }
}

fn failure_policy_suppression(
    has_cooling_down: bool,
    has_disabled: bool,
) -> Option<RenderFailurePolicySuppression> {
    match (has_cooling_down, has_disabled) {
        (true, false) => Some(RenderFailurePolicySuppression::CoolingDown),
        (false, true) => Some(RenderFailurePolicySuppression::Disabled),
        (true, true) => Some(RenderFailurePolicySuppression::CoolingDownAndDisabled),
        (false, false) => None,
    }
}

/// Typed, redaction-safe diagnostic for a deferred primary-render selection.
///
/// Swap-chain identities, raw HWND values, extents, and presentation counters
/// are deliberately removed. Rejection categories are sorted and deduplicated
/// so equivalent per-frame failures compare equal even as candidate identities
/// change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderSelectionDeferred {
    failure: RenderSelectionFailure,
    override_failure: Option<RenderOverrideFailure>,
    rejections: Vec<RenderCandidateRejection>,
}

impl RenderSelectionDeferred {
    /// Converts an unsuccessful classifier result into an observable diagnostic.
    #[must_use]
    pub fn from_classification(classification: &Classification) -> Option<Self> {
        Self::from_classification_with_override_suppression(classification, None)
    }

    fn from_classification_with_override_suppression(
        classification: &Classification,
        override_suppression: Option<RenderFailurePolicySuppression>,
    ) -> Option<Self> {
        if classification.selected_id().is_some() {
            return None;
        }
        let failure = match classification.no_selection_reason()? {
            NoSelectionReason::NoObservations => RenderSelectionFailure::NoObservations,
            NoSelectionReason::NoEligibleCandidates => RenderSelectionFailure::NoEligibleCandidates,
        };
        if matches!(
            classification.override_status(),
            OverrideStatus::Applied { .. }
        ) {
            return None;
        }
        let override_failure =
            redacted_override_failure(classification.override_status(), override_suppression);
        let mut rejections = classification
            .rejected()
            .iter()
            .map(|rejection| rejection.reason().into())
            .collect::<Vec<_>>();
        rejections.sort_unstable();
        rejections.dedup();
        Some(Self {
            failure,
            override_failure,
            rejections,
        })
    }

    /// Combines classifier rejection detail with any failure-policy suppression.
    ///
    /// Suppression is the primary failure whenever at least one observation was
    /// withheld, while override and rejection details from the remaining
    /// classifier candidates are retained.
    #[must_use]
    pub(crate) fn from_classification_with_failure_policy_and_override_suppression(
        classification: &Classification,
        has_cooling_down: bool,
        has_disabled: bool,
        override_suppression: Option<RenderFailurePolicySuppression>,
    ) -> Option<Self> {
        let mut diagnostic = Self::from_classification_with_override_suppression(
            classification,
            override_suppression,
        )?;
        if let Some(suppression) = failure_policy_suppression(has_cooling_down, has_disabled) {
            diagnostic.failure = RenderSelectionFailure::FailurePolicySuppressed(suppression);
        }
        Some(diagnostic)
    }

    /// Creates a diagnostic when failure policy filtered every observed chain.
    ///
    /// The two flags report whether any filtered candidate was cooling down or
    /// disabled. `None` is returned when neither state was present, preventing
    /// callers from manufacturing a suppression diagnostic without evidence.
    #[must_use]
    pub fn from_failure_policy(has_cooling_down: bool, has_disabled: bool) -> Option<Self> {
        let suppression = failure_policy_suppression(has_cooling_down, has_disabled)?;
        Some(Self {
            failure: RenderSelectionFailure::FailurePolicySuppressed(suppression),
            override_failure: None,
            rejections: Vec::new(),
        })
    }

    /// Returns the closed automatic-selection failure.
    #[must_use]
    pub const fn failure(&self) -> RenderSelectionFailure {
        self.failure
    }

    /// Returns the redacted exact-override failure, when one was requested.
    #[must_use]
    pub const fn override_failure(&self) -> Option<RenderOverrideFailure> {
        self.override_failure
    }

    /// Returns sorted, unique automatic-candidate rejection categories.
    #[must_use]
    pub fn rejections(&self) -> &[RenderCandidateRejection] {
        &self.rejections
    }
}

impl fmt::Display for RenderSelectionDeferred {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.failure {
            RenderSelectionFailure::NoObservations => "no swap-chain observations",
            RenderSelectionFailure::NoEligibleCandidates => "no eligible swap chains",
            RenderSelectionFailure::FailurePolicySuppressed(
                RenderFailurePolicySuppression::CoolingDown,
            ) => "render candidates cooling down under failure policy",
            RenderSelectionFailure::FailurePolicySuppressed(
                RenderFailurePolicySuppression::Disabled,
            ) => "render candidates disabled by failure policy",
            RenderSelectionFailure::FailurePolicySuppressed(
                RenderFailurePolicySuppression::CoolingDownAndDisabled,
            ) => "render candidates cooling down or disabled by failure policy",
        })?;
        match self.override_failure {
            Some(RenderOverrideFailure::Unavailable) => {
                formatter.write_str("; requested override unavailable")?;
            }
            Some(RenderOverrideFailure::FailurePolicySuppressed(suppression)) => {
                formatter.write_str(match suppression {
                    RenderFailurePolicySuppression::CoolingDown => {
                        "; requested override cooling down under failure policy"
                    }
                    RenderFailurePolicySuppression::Disabled => {
                        "; requested override disabled by failure policy"
                    }
                    RenderFailurePolicySuppression::CoolingDownAndDisabled => {
                        "; requested override cooling down or disabled by failure policy"
                    }
                })?;
            }
            Some(RenderOverrideFailure::Ineligible { reason }) => {
                write!(formatter, "; requested override ineligible: {reason}")?;
            }
            None => {}
        }
        if !self.rejections.is_empty() {
            formatter.write_str("; rejected: ")?;
            for (index, reason) in self.rejections.iter().enumerate() {
                if index != 0 {
                    formatter.write_str(", ")?;
                }
                write!(formatter, "{reason}")?;
            }
        }
        Ok(())
    }
}

/// Redaction-safe observation emitted by the interception manager.
///
/// No variant contains a pointer, window title, executable path, adapter name,
/// or free-form native error text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DxgiObservationEvent {
    /// A concrete factory interface was intercepted.
    FactoryAttached {
        /// Concrete layout attached for that interface pointer.
        interface: FactoryInterface,
    },
    /// A concrete swap-chain interface was intercepted and assigned a local ID.
    SwapChainAttached {
        /// Runtime-local monotonically assigned swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Highest layout attached for that concrete pointer.
        interface: SwapChainInterface,
    },
    /// An authoritative metadata field was unavailable.
    MetadataIncomplete {
        /// Runtime-local swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Field that remains explicitly unknown.
        field: ObservationField,
    },
    /// A presentation reached the native implementation and returned.
    PresentForwarded {
        /// Runtime-local swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Presentation entry point used.
        method: PresentMethod,
        /// Global monotonic presentation sequence.
        sequence: u64,
        /// Closed interpretation of the native result.
        result: HResultDisposition,
    },
    /// A resize reached the native implementation and returned.
    ResizeForwarded {
        /// Runtime-local swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Requested extent. Zero retains DXGI's native automatic sizing meaning.
        requested_size: Extent2D,
        /// Requested format translated without losing unknown native values.
        requested_format: SurfaceFormat,
        /// Closed interpretation of the native result.
        result: HResultDisposition,
    },
    /// A color-space mutation reached the native implementation and returned.
    ///
    /// On failure, `active` remains the last successfully applied value (or
    /// explicit unknown if no successful mutation has been observed).
    ColorSpaceForwarded {
        /// Runtime-local swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Requested color space, retaining unknown native numeric values.
        requested: ColorSpace,
        /// Authoritative manager state after the native call.
        active: ColorSpace,
        /// Closed interpretation of the native result.
        result: HResultDisposition,
    },
    /// Classification resolved a primary chain for this presentation pass.
    ///
    /// This does not imply that an overlay frame was rendered. Safe-mode stage,
    /// failure policy, or a different currently presenting chain may still
    /// prevent the render callback from running.
    RenderSelectionResolved {
        /// Global monotonic sequence of applied classification transitions.
        sequence: u64,
        /// Closed classifier state with native identity removed.
        resolution: RenderSelectionResolved,
    },
    /// Policy selected a generation for synchronous overlay rendering.
    RenderSelected {
        /// Global monotonic sequence of applied classification transitions.
        sequence: u64,
        /// Runtime-local swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Current resource generation.
        generation: SessionGeneration,
        /// Maximum stage permitted for this callback.
        stage: RenderStage,
        /// Closed classifier reason for selecting this chain.
        reason: SelectionReason,
    },
    /// No safe primary chain could be selected for this classification pass.
    RenderSelectionDeferred {
        /// Global monotonic sequence of applied classification transitions.
        sequence: u64,
        /// Closed diagnostic with raw native identities and values removed.
        diagnostic: RenderSelectionDeferred,
    },
    /// A Rust panic was caught before it could cross the native ABI.
    PanicContained {
        /// Boundary that contained the panic.
        boundary: Boundary,
    },
    /// Hooks were restored and callback admission was closed.
    Shutdown {
        /// Whether every callback admitted before closure drained in time.
        drained: bool,
        /// Number of shadow vtables restored to their original pointer.
        restored: u32,
        /// Number already displaced by another component.
        displaced: u32,
    },
}

#[cfg(test)]
mod tests {
    use nexus_render::{
        Activity, AdapterLuid, ClassifierConfig, ColorSpace, DeviceId, Extent2D, Hwnd,
        PresentMethod, PrimarySwapChainClassifier, SelectionReason, SurfaceFormat, SwapChainId,
        SwapChainObservation,
    };

    use super::{
        RenderCandidateRejection, RenderFailurePolicySuppression, RenderOverrideFailure,
        RenderSelectionDeferred, RenderSelectionFailure, RenderSelectionResolved,
    };

    fn eligible_observation() -> SwapChainObservation {
        SwapChainObservation {
            id: SwapChainId::new(7),
            hwnd: Some(Hwnd::new(11)),
            device: DeviceId::new(1),
            adapter_luid: AdapterLuid::new(2, 0),
            format: SurfaceFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            size: Extent2D::new(1_920, 1_080),
            present_method: PresentMethod::Present,
            activity: Activity::active(5, 5, 3),
        }
    }

    #[test]
    fn deferred_diagnostic_formats_without_native_identifiers() {
        let diagnostic = RenderSelectionDeferred {
            failure: RenderSelectionFailure::NoEligibleCandidates,
            override_failure: Some(RenderOverrideFailure::Ineligible {
                reason: RenderCandidateRejection::UnexpectedWindow,
            }),
            rejections: vec![
                RenderCandidateRejection::MissingWindowHandle,
                RenderCandidateRejection::UnexpectedWindow,
            ],
        };

        assert_eq!(
            diagnostic.to_string(),
            "no eligible swap chains; requested override ineligible: unexpected window; rejected: missing window handle, unexpected window"
        );
    }

    #[test]
    fn unavailable_override_is_preserved_when_no_observations_exist() {
        let mut config = ClassifierConfig::default();
        config.set_user_override(Some(SwapChainId::new(41)));
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let classification = classifier.classify(&[]);
        let diagnostic = RenderSelectionDeferred::from_classification(&classification)
            .expect("an unsuccessful classification should produce a diagnostic");

        assert_eq!(diagnostic.failure(), RenderSelectionFailure::NoObservations);
        assert_eq!(
            diagnostic.override_failure(),
            Some(RenderOverrideFailure::Unavailable)
        );
        assert!(diagnostic.rejections().is_empty());
        assert_eq!(
            diagnostic.to_string(),
            "no swap-chain observations; requested override unavailable"
        );
    }

    #[test]
    fn resolved_state_preserves_failed_override_during_automatic_fallback() {
        let mut config = ClassifierConfig::default();
        config.set_user_override(Some(SwapChainId::new(41)));
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let classification = classifier.classify(&[eligible_observation()]);
        let resolution = RenderSelectionResolved::from_classification(&classification)
            .expect("an eligible observation should resolve selection");

        assert_eq!(resolution.reason(), SelectionReason::OnlyEligibleCandidate);
        assert_eq!(
            resolution.override_failure(),
            Some(RenderOverrideFailure::Unavailable)
        );

        let mut rejected_override = eligible_observation();
        rejected_override.id = SwapChainId::new(41);
        rejected_override.size = Extent2D::new(0, 1_080);
        let classification = classifier.classify(&[rejected_override, eligible_observation()]);
        let resolution = RenderSelectionResolved::from_classification(&classification)
            .expect("automatic fallback should recover from an ineligible override");
        assert_eq!(
            resolution.override_failure(),
            Some(RenderOverrideFailure::Ineligible {
                reason: RenderCandidateRejection::ZeroSized,
            })
        );

        let empty = classifier.classify(&[]);
        assert!(RenderSelectionResolved::from_classification(&empty).is_none());
    }

    #[test]
    fn failure_policy_suppression_distinguishes_an_observed_override() {
        let mut config = ClassifierConfig::default();
        config.set_user_override(Some(SwapChainId::new(41)));
        let mut classifier = PrimarySwapChainClassifier::new(config);
        let classification = classifier.classify(&[eligible_observation()]);
        let resolution = RenderSelectionResolved::from_classification_with_override_suppression(
            &classification,
            Some(RenderFailurePolicySuppression::Disabled),
        )
        .expect("automatic fallback should resolve selection");
        assert_eq!(
            resolution.override_failure(),
            Some(RenderOverrideFailure::FailurePolicySuppressed(
                RenderFailurePolicySuppression::Disabled
            ))
        );

        let classification = classifier.classify(&[]);
        let diagnostic = RenderSelectionDeferred::
            from_classification_with_failure_policy_and_override_suppression(
                &classification,
                true,
                false,
                Some(RenderFailurePolicySuppression::CoolingDown),
            )
            .expect("the suppressed override should remain observable");
        assert_eq!(
            diagnostic.override_failure(),
            Some(RenderOverrideFailure::FailurePolicySuppressed(
                RenderFailurePolicySuppression::CoolingDown
            ))
        );
        assert_eq!(
            diagnostic.to_string(),
            "render candidates cooling down under failure policy; requested override cooling down under failure policy"
        );
    }

    #[test]
    fn failure_policy_suppression_keeps_classifier_rejections() {
        let mut rejected = eligible_observation();
        rejected.size = Extent2D::new(0, 1_080);
        let mut classifier = PrimarySwapChainClassifier::default();
        let classification = classifier.classify(&[rejected]);

        let diagnostic = RenderSelectionDeferred::
            from_classification_with_failure_policy_and_override_suppression(
                &classification,
                true,
                true,
                None,
            )
            .expect("suppression and rejection should produce one diagnostic");

        assert_eq!(
            diagnostic.failure(),
            RenderSelectionFailure::FailurePolicySuppressed(
                RenderFailurePolicySuppression::CoolingDownAndDisabled
            )
        );
        assert_eq!(
            diagnostic.rejections(),
            &[RenderCandidateRejection::ZeroSized]
        );
    }

    #[test]
    fn failure_policy_diagnostic_distinguishes_cooldown_and_disablement() {
        assert!(RenderSelectionDeferred::from_failure_policy(false, false).is_none());

        let cooling = RenderSelectionDeferred::from_failure_policy(true, false)
            .expect("cooling candidates should produce a diagnostic");
        assert_eq!(
            cooling.failure(),
            RenderSelectionFailure::FailurePolicySuppressed(
                RenderFailurePolicySuppression::CoolingDown
            )
        );
        assert_eq!(
            cooling.to_string(),
            "render candidates cooling down under failure policy"
        );

        let disabled = RenderSelectionDeferred::from_failure_policy(false, true)
            .expect("disabled candidates should produce a diagnostic");
        assert_eq!(
            disabled.failure(),
            RenderSelectionFailure::FailurePolicySuppressed(
                RenderFailurePolicySuppression::Disabled
            )
        );

        let mixed = RenderSelectionDeferred::from_failure_policy(true, true)
            .expect("mixed suppression should produce a diagnostic");
        assert_eq!(
            mixed.failure(),
            RenderSelectionFailure::FailurePolicySuppressed(
                RenderFailurePolicySuppression::CoolingDownAndDisabled
            )
        );
        assert_eq!(
            mixed.to_string(),
            "render candidates cooling down or disabled by failure policy"
        );
    }
}

/// Result of closing callback admission and restoring every owned vtable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Whether all admitted callbacks drained before the timeout.
    pub drained: bool,
    /// Shadow vtables restored to their original pointer.
    pub restored: u32,
    /// Shadow vtables that another component had already displaced.
    pub displaced: u32,
    /// Callbacks still admitted when the timeout expired.
    pub in_flight: usize,
}
