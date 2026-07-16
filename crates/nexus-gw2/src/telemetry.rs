use nexus_abi::{MumbleData, MumbleUiScale, MumbleVector3};

/// Legacy scaling factor for the small UI setting.
pub const SCALE_SMALL: f32 = 0.90;
/// Legacy scaling factor for the normal UI setting.
pub const SCALE_NORMAL: f32 = 1.00;
/// Legacy scaling factor for the large UI setting.
pub const SCALE_LARGE: f32 = 1.11;
/// Legacy scaling factor for the larger UI setting.
pub const SCALE_LARGER: f32 = 1.22;

/// Returns the legacy Nexus scaling factor, defaulting unknown future values
/// to the normal setting.
#[must_use]
pub const fn ui_scaling_factor(scale: MumbleUiScale) -> f32 {
    match scale.value() {
        0 => SCALE_SMALL,
        2 => SCALE_LARGE,
        3 => SCALE_LARGER,
        _ => SCALE_NORMAL,
    }
}

/// States derived from Mumble ticks and render-frame progress.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DerivedTelemetry {
    /// The avatar moved by at least one legacy millimeter unit.
    pub is_moving: bool,
    /// The camera direction moved by at least one legacy millimeter unit.
    pub is_camera_moving: bool,
    /// Gameplay is active according to the legacy tick/freeze heuristic.
    pub is_gameplay: bool,
}

/// Deterministic state machine for values exposed through `DL_NEXUS_LINK`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TelemetryTracker {
    previous_tick: u32,
    previous_avatar_position: MumbleVector3,
    previous_camera_front: MumbleVector3,
    previous_frame_count: u64,
    previous: DerivedTelemetry,
}

impl TelemetryTracker {
    /// Advances the derived state from one coherent Mumble snapshot.
    #[must_use]
    pub fn advance(&mut self, data: &MumbleData, frame_count: u64) -> DerivedTelemetry {
        let tick_changed = self.previous_tick != data.ui_tick;
        let game_frozen = self.previous_frame_count == frame_count;
        let next = DerivedTelemetry {
            is_moving: !vector3_equal_legacy(self.previous_avatar_position, data.avatar_position),
            is_camera_moving: !vector3_equal_legacy(self.previous_camera_front, data.camera_front),
            is_gameplay: tick_changed || (game_frozen && self.previous.is_gameplay),
        };

        self.previous_tick = data.ui_tick;
        self.previous_avatar_position = data.avatar_position;
        self.previous_camera_front = data.camera_front;
        self.previous_frame_count = frame_count;
        self.previous = next;
        next
    }
}

fn vector3_equal_legacy(left: MumbleVector3, right: MumbleVector3) -> bool {
    component_equal_legacy(left.x, right.x)
        && component_equal_legacy(left.y, right.y)
        && component_equal_legacy(left.z, right.z)
}

fn component_equal_legacy(left: f32, right: f32) -> bool {
    (f64::from(left) * 1_000.0).trunc() == (f64::from(right) * 1_000.0).trunc()
}

#[cfg(test)]
mod tests {
    use nexus_abi::{MumbleData, MumbleUiScale, MumbleVector3};

    use super::{SCALE_NORMAL, TelemetryTracker, ui_scaling_factor};

    #[test]
    fn scaling_matches_legacy_values_and_unknowns_are_safe() {
        assert_eq!(ui_scaling_factor(MumbleUiScale::SMALL), 0.90);
        assert_eq!(ui_scaling_factor(MumbleUiScale::NORMAL), 1.00);
        assert_eq!(ui_scaling_factor(MumbleUiScale::LARGE), 1.11);
        assert_eq!(ui_scaling_factor(MumbleUiScale::LARGER), 1.22);
        assert_eq!(
            ui_scaling_factor(MumbleUiScale::from_raw(255)),
            SCALE_NORMAL
        );
    }

    #[test]
    fn movement_uses_the_legacy_truncated_thousandth_threshold() {
        let mut tracker = TelemetryTracker::default();
        let mut data = MumbleData {
            ui_tick: 1,
            avatar_position: MumbleVector3 {
                x: 1.000_4,
                ..MumbleVector3::default()
            },
            ..MumbleData::default()
        };
        assert!(tracker.advance(&data, 1).is_moving);

        data.avatar_position.x = 1.000_9;
        assert!(!tracker.advance(&data, 2).is_moving);

        data.avatar_position.x = 1.001_1;
        assert!(tracker.advance(&data, 3).is_moving);
    }

    #[test]
    fn gameplay_survives_a_frozen_game_but_not_an_advancing_idle_frame() {
        let mut tracker = TelemetryTracker::default();
        let mut data = MumbleData {
            ui_tick: 1,
            ..MumbleData::default()
        };
        assert!(tracker.advance(&data, 10).is_gameplay);
        assert!(tracker.advance(&data, 10).is_gameplay);
        assert!(!tracker.advance(&data, 11).is_gameplay);

        data.ui_tick = 2;
        assert!(tracker.advance(&data, 12).is_gameplay);
    }
}
