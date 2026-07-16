//! Exact legacy scaling factors with validated inputs and an injected sink.

use thiserror::Error;

/// Guild Wars 2 small UI factor.
pub const SMALL_SCALE: f32 = 0.90;
/// Guild Wars 2 normal UI factor.
pub const NORMAL_SCALE: f32 = 1.00;
/// Guild Wars 2 large UI factor.
pub const LARGE_SCALE: f32 = 1.11;
/// Guild Wars 2 larger UI factor.
pub const LARGER_SCALE: f32 = 1.22;

const BASE_DPI: f32 = 96.0;
const REFERENCE_WIDTH: f32 = 1024.0;
const REFERENCE_HEIGHT: f32 = 768.0;

/// Open representation of Mumble's UI-size enumeration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct UiScale(pub u8);

impl UiScale {
    /// Small game UI.
    pub const SMALL: Self = Self(0);
    /// Normal game UI.
    pub const NORMAL: Self = Self(1);
    /// Large game UI.
    pub const LARGE: Self = Self(2);
    /// Larger game UI.
    pub const LARGER: Self = Self(3);

    /// Returns the exact factor used by the C++ Nexus host.
    #[must_use]
    pub const fn factor(self) -> f32 {
        match self.0 {
            0 => SMALL_SCALE,
            2 => LARGE_SCALE,
            3 => LARGER_SCALE,
            _ => NORMAL_SCALE,
        }
    }
}

/// Closed failures returned by the scaling integration boundary.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ScalingSinkError {
    /// NexusLink publication failed.
    #[error("could not publish NexusLink scaling")]
    NexusLinkUnavailable,
    /// Dear ImGui font-global-scale publication failed.
    #[error("could not publish font global scaling")]
    ImGuiUnavailable,
    /// Persisting the last observed game scale failed.
    #[error("could not persist game UI scaling")]
    SettingsUnavailable,
}

/// Runtime integration used by the pure scaling state machine.
pub trait ScalingSink {
    /// Writes the cumulative factor to `NexusLinkData::Scaling`.
    fn publish_nexus_scale(&mut self, scale: f32) -> Result<(), ScalingSinkError>;

    /// Writes the effective DPI factor to Dear ImGui's `FontGlobalScale`.
    fn publish_font_global_scale(&mut self, scale: f32) -> Result<(), ScalingSinkError>;

    /// Persists the legacy `LastUIScale` setting.
    fn persist_game_scale(&mut self, scale: f32) -> Result<(), ScalingSinkError>;
}

/// Redaction-safe input or integration failure.
#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum ScalingError {
    /// Window dimensions must be finite and non-negative.
    #[error("window dimensions are invalid")]
    InvalidResolution,
    /// An injected sink rejected the update.
    #[error(transparent)]
    Sink(#[from] ScalingSinkError),
}

/// Current factors contributing to Nexus scaling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScalingSnapshot {
    /// Whether native DPI scaling is enabled by user preference.
    pub dpi_enabled: bool,
    /// Physical window DPI divided by 96.
    pub dpi: f32,
    /// DPI factor after applying the user preference.
    pub effective_dpi: f32,
    /// Guild Wars 2 UI-size factor.
    pub game_ui: f32,
    /// Downscale applied below 1024 by 768.
    pub minimum_resolution: f32,
    /// Product published through NexusLink.
    pub cumulative: f32,
}

impl ScalingSnapshot {
    fn new(dpi_enabled: bool, last_game_scale: f32) -> Self {
        let game_ui = if last_game_scale.is_finite() && last_game_scale > 0.0 {
            last_game_scale
        } else {
            NORMAL_SCALE
        };
        let effective_dpi = 1.0;
        let minimum_resolution = 1.0;
        Self {
            dpi_enabled,
            dpi: 1.0,
            effective_dpi,
            game_ui,
            minimum_resolution,
            cumulative: game_ui * effective_dpi * minimum_resolution,
        }
    }

    fn recompute(&mut self) {
        self.cumulative = self.game_ui * self.effective_dpi * self.minimum_resolution;
    }
}

/// Scaling state machine used by DPI, Mumble identity, and resize events.
pub struct ScalingService<S> {
    sink: S,
    state: ScalingSnapshot,
}

impl<S: ScalingSink> ScalingService<S> {
    /// Restores the preference and last observed game factor.
    #[must_use]
    pub fn new(sink: S, dpi_enabled: bool, last_game_scale: f32) -> Self {
        Self {
            sink,
            state: ScalingSnapshot::new(dpi_enabled, last_game_scale),
        }
    }

    /// Applies `GetDpiForWindow` output. A zero result has legacy factor 1.0.
    pub fn update_dpi(&mut self, dpi: u32) -> Result<ScalingSnapshot, ScalingError> {
        self.state.dpi = if dpi == 0 { 1.0 } else { dpi as f32 / BASE_DPI };
        self.state.effective_dpi = if self.state.dpi_enabled {
            self.state.dpi
        } else {
            1.0
        };
        self.sink
            .publish_font_global_scale(self.state.effective_dpi)?;
        self.publish()
    }

    /// Enables or disables native DPI scaling and republishes both outputs.
    pub fn set_dpi_enabled(&mut self, enabled: bool) -> Result<ScalingSnapshot, ScalingError> {
        self.state.dpi_enabled = enabled;
        self.state.effective_dpi = if enabled { self.state.dpi } else { 1.0 };
        self.sink
            .publish_font_global_scale(self.state.effective_dpi)?;
        self.publish()
    }

    /// Applies a Mumble UI-size update and persists only real changes.
    pub fn update_game_ui(&mut self, ui_scale: UiScale) -> Result<ScalingSnapshot, ScalingError> {
        let factor = ui_scale.factor();
        if self.state.game_ui != factor {
            self.sink.persist_game_scale(factor)?;
            self.state.game_ui = factor;
        }
        self.publish()
    }

    /// Applies the legacy sub-1024x768 minimum-resolution factor.
    pub fn update_resolution(
        &mut self,
        width: f32,
        height: f32,
    ) -> Result<ScalingSnapshot, ScalingError> {
        if !width.is_finite() || !height.is_finite() || width < 0.0 || height < 0.0 {
            return Err(ScalingError::InvalidResolution);
        }
        self.state.minimum_resolution = (width.min(REFERENCE_WIDTH) / REFERENCE_WIDTH)
            .min(height.min(REFERENCE_HEIGHT) / REFERENCE_HEIGHT);
        self.publish()
    }

    /// Returns the last successfully computed state.
    #[must_use]
    pub const fn snapshot(&self) -> ScalingSnapshot {
        self.state
    }

    /// Returns the injected sink after service shutdown.
    #[must_use]
    pub fn into_sink(self) -> S {
        self.sink
    }

    fn publish(&mut self) -> Result<ScalingSnapshot, ScalingError> {
        self.state.recompute();
        self.sink.publish_nexus_scale(self.state.cumulative)?;
        Ok(self.state)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LARGE_SCALE, LARGER_SCALE, NORMAL_SCALE, SMALL_SCALE, ScalingService, ScalingSink,
        ScalingSinkError, UiScale,
    };

    #[derive(Default)]
    struct Sink {
        nexus: Vec<f32>,
        font: Vec<f32>,
        persisted: Vec<f32>,
    }

    impl ScalingSink for Sink {
        fn publish_nexus_scale(&mut self, scale: f32) -> Result<(), ScalingSinkError> {
            self.nexus.push(scale);
            Ok(())
        }

        fn publish_font_global_scale(&mut self, scale: f32) -> Result<(), ScalingSinkError> {
            self.font.push(scale);
            Ok(())
        }

        fn persist_game_scale(&mut self, scale: f32) -> Result<(), ScalingSinkError> {
            self.persisted.push(scale);
            Ok(())
        }
    }

    #[test]
    fn game_ui_values_match_the_legacy_constants() {
        assert_eq!(UiScale::SMALL.factor(), SMALL_SCALE);
        assert_eq!(UiScale::NORMAL.factor(), NORMAL_SCALE);
        assert_eq!(UiScale::LARGE.factor(), LARGE_SCALE);
        assert_eq!(UiScale::LARGER.factor(), LARGER_SCALE);
        assert_eq!(UiScale(255).factor(), NORMAL_SCALE);
    }

    #[test]
    fn dpi_game_and_resolution_factors_multiply_exactly() {
        let mut scaling = ScalingService::new(Sink::default(), true, NORMAL_SCALE);
        assert!(scaling.update_dpi(144).is_ok());
        assert!(scaling.update_game_ui(UiScale::LARGE).is_ok());
        let snapshot = scaling
            .update_resolution(512.0, 384.0)
            .unwrap_or_else(|error| panic!("resolution update failed: {error}"));
        assert_eq!(snapshot.dpi, 1.5);
        assert_eq!(snapshot.minimum_resolution, 0.5);
        assert_eq!(snapshot.cumulative, LARGE_SCALE * 1.5 * 0.5);
        let sink = scaling.into_sink();
        assert_eq!(sink.font, vec![1.5]);
        assert_eq!(sink.persisted, vec![LARGE_SCALE]);
        assert_eq!(sink.nexus.last().copied(), Some(snapshot.cumulative));
    }

    #[test]
    fn disabling_dpi_keeps_physical_dpi_but_publishes_one() {
        let mut scaling = ScalingService::new(Sink::default(), true, NORMAL_SCALE);
        assert!(scaling.update_dpi(192).is_ok());
        let snapshot = scaling
            .set_dpi_enabled(false)
            .unwrap_or_else(|error| panic!("DPI setting failed: {error}"));
        assert_eq!(snapshot.dpi, 2.0);
        assert_eq!(snapshot.effective_dpi, 1.0);
        assert_eq!(scaling.into_sink().font, vec![2.0, 1.0]);
    }

    #[test]
    fn invalid_resolution_is_rejected_without_nan_publication() {
        let mut scaling = ScalingService::new(Sink::default(), true, NORMAL_SCALE);
        assert!(scaling.update_resolution(f32::NAN, 768.0).is_err());
        assert!(scaling.into_sink().nexus.is_empty());
    }
}
