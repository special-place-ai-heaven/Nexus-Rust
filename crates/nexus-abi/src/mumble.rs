/// Data-link identifier for the raw Guild Wars 2 MumbleLink mapping.
pub const DL_MUMBLE_LINK: &str = "DL_MUMBLE_LINK";
/// Data-link identifier for the parsed Mumble identity.
pub const DL_MUMBLE_LINK_IDENTITY: &str = "DL_MUMBLE_LINK_IDENTITY";
/// Legacy identity-change event name.
pub const EV_MUMBLE_IDENTITY_UPDATED: &str = "EV_MUMBLE_IDENTITY_UPDATED";
/// Default Windows shared-memory object used by Guild Wars 2.
pub const DEFAULT_MUMBLE_MAPPING_NAME: &str = "MumbleLink";

macro_rules! open_u8 {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
        pub struct $name(u8);

        impl $name {
            /// Returns the raw ABI value, including values introduced by a newer game build.
            #[must_use]
            pub const fn value(self) -> u8 {
                self.0
            }

            /// Preserves an arbitrary raw ABI value without creating an invalid Rust enum.
            #[must_use]
            pub const fn from_raw(value: u8) -> Self {
                Self(value)
            }
        }
    };
}

open_u8!(/// Guild Wars 2 map category reported by MumbleLink.
    MumbleMapType);

impl MumbleMapType {
    /// Automatic redirect map.
    pub const AUTO_REDIRECT: Self = Self(0);
    /// Character creation map.
    pub const CHARACTER_CREATION: Self = Self(1);
    /// Structured PvP map.
    pub const PVP: Self = Self(2);
    /// Guild-versus-guild map.
    pub const GVG: Self = Self(3);
    /// Instanced map.
    pub const INSTANCE: Self = Self(4);
    /// Public map.
    pub const PUBLIC: Self = Self(5);
    /// Tournament map.
    pub const TOURNAMENT: Self = Self(6);
    /// Tutorial map.
    pub const TUTORIAL: Self = Self(7);
    /// User tournament map.
    pub const USER_TOURNAMENT: Self = Self(8);
    /// Eternal Battlegrounds.
    pub const WVW_ETERNAL_BATTLEGROUNDS: Self = Self(9);
    /// Blue Borderlands.
    pub const WVW_BLUE_BORDERLANDS: Self = Self(10);
    /// Green Borderlands.
    pub const WVW_GREEN_BORDERLANDS: Self = Self(11);
    /// Red Borderlands.
    pub const WVW_RED_BORDERLANDS: Self = Self(12);
    /// Fortune's Vale.
    pub const WVW_FORTUNES_VALE: Self = Self(13);
    /// Obsidian Sanctum.
    pub const WVW_OBSIDIAN_SANCTUM: Self = Self(14);
    /// Edge of the Mists.
    pub const WVW_EDGE_OF_THE_MISTS: Self = Self(15);
    /// Small public map.
    pub const PUBLIC_MINI: Self = Self(16);
    /// Large-battle map.
    pub const BIG_BATTLE: Self = Self(17);
    /// World-versus-world lounge.
    pub const WVW_LOUNGE: Self = Self(18);
}

open_u8!(/// Active mount reported by MumbleLink.
    MumbleMountIndex);

impl MumbleMountIndex {
    /// No mount.
    pub const NONE: Self = Self(0);
    /// Jackal.
    pub const JACKAL: Self = Self(1);
    /// Griffon.
    pub const GRIFFON: Self = Self(2);
    /// Springer.
    pub const SPRINGER: Self = Self(3);
    /// Skimmer.
    pub const SKIMMER: Self = Self(4);
    /// Raptor.
    pub const RAPTOR: Self = Self(5);
    /// Roller Beetle.
    pub const ROLLER_BEETLE: Self = Self(6);
    /// Warclaw.
    pub const WARCLAW: Self = Self(7);
    /// Skyscale.
    pub const SKYSCALE: Self = Self(8);
    /// Skiff.
    pub const SKIFF: Self = Self(9);
    /// Siege Turtle.
    pub const SIEGE_TURTLE: Self = Self(10);
}

open_u8!(/// Player profession reported in the identity document.
    MumbleProfession);

impl MumbleProfession {
    /// No profession.
    pub const NONE: Self = Self(0);
    /// Guardian.
    pub const GUARDIAN: Self = Self(1);
    /// Warrior.
    pub const WARRIOR: Self = Self(2);
    /// Engineer.
    pub const ENGINEER: Self = Self(3);
    /// Ranger.
    pub const RANGER: Self = Self(4);
    /// Thief.
    pub const THIEF: Self = Self(5);
    /// Elementalist.
    pub const ELEMENTALIST: Self = Self(6);
    /// Mesmer.
    pub const MESMER: Self = Self(7);
    /// Necromancer.
    pub const NECROMANCER: Self = Self(8);
    /// Revenant.
    pub const REVENANT: Self = Self(9);
}

open_u8!(/// Player race reported in the identity document.
    MumbleRace);

impl MumbleRace {
    /// Asura.
    pub const ASURA: Self = Self(0);
    /// Charr.
    pub const CHARR: Self = Self(1);
    /// Human.
    pub const HUMAN: Self = Self(2);
    /// Norn.
    pub const NORN: Self = Self(3);
    /// Sylvari.
    pub const SYLVARI: Self = Self(4);
}

open_u8!(/// Guild Wars 2 UI-size setting.
    MumbleUiScale);

impl MumbleUiScale {
    /// Small UI.
    pub const SMALL: Self = Self(0);
    /// Normal UI.
    pub const NORMAL: Self = Self(1);
    /// Large UI.
    pub const LARGE: Self = Self(2);
    /// Larger UI.
    pub const LARGER: Self = Self(3);
}

/// Two-dimensional MumbleLink vector.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MumbleVector2 {
    /// X component.
    pub x: f32,
    /// Y component.
    pub y: f32,
}

/// Three-dimensional MumbleLink vector.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MumbleVector3 {
    /// X component.
    pub x: f32,
    /// Y component.
    pub y: f32,
    /// Z component.
    pub z: f32,
}

/// Parsed `identity` JSON shared with legacy add-ons.
///
/// Open integer wrappers and byte-backed booleans prevent undefined behavior
/// when a newer or corrupt producer writes an unknown value.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MumbleIdentity {
    /// UTF-8 account or character name, nul-terminated when it fits.
    pub name: [u8; 20],
    /// Base profession.
    pub profession: MumbleProfession,
    /// Elite specialization identifier.
    pub specialization: u32,
    /// Character race.
    pub race: MumbleRace,
    /// Current map identifier.
    pub map_id: u32,
    /// Current world identifier.
    pub world_id: u32,
    /// Current team color identifier.
    pub team_color_id: u32,
    /// Non-zero while the player has an active commander tag.
    pub is_commander: u8,
    /// Camera field of view.
    pub fov: f32,
    /// UI-size setting.
    pub ui_size: MumbleUiScale,
}

impl Default for MumbleIdentity {
    fn default() -> Self {
        Self {
            name: [0; 20],
            profession: MumbleProfession::NONE,
            specialization: 0,
            race: MumbleRace::ASURA,
            map_id: 0,
            world_id: 0,
            team_color_id: 0,
            is_commander: 0,
            fov: 0.0,
            ui_size: MumbleUiScale::SMALL,
        }
    }
}

/// Minimap/compass telemetry embedded in the Mumble context.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MumbleCompass {
    /// Compass width in pixels.
    pub width: u16,
    /// Compass height in pixels.
    pub height: u16,
    /// Rotation in radians.
    pub rotation: f32,
    /// Player position in continent coordinates.
    pub player_position: MumbleVector2,
    /// Compass center in continent coordinates.
    pub center: MumbleVector2,
    /// Compass zoom scale.
    pub scale: f32,
}

/// Raw `sockaddr_in`/`sockaddr_in6` union from the legacy C++ ABI.
#[repr(C, align(4))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MumbleServerAddress {
    /// Union storage; interpreting it requires inspecting the address family.
    pub bytes: [u8; 28],
}

/// Bitfield storage used by `MumbleContext`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct MumbleContextFlags(u32);

impl MumbleContextFlags {
    /// World map is open.
    pub const MAP_OPEN: u32 = 1 << 0;
    /// Compass is placed at the top right.
    pub const COMPASS_TOP_RIGHT: u32 = 1 << 1;
    /// Compass rotates with the camera.
    pub const COMPASS_ROTATING: u32 = 1 << 2;
    /// Game window has focus.
    pub const GAME_FOCUSED: u32 = 1 << 3;
    /// Player is in a competitive mode.
    pub const COMPETITIVE: u32 = 1 << 4;
    /// A text box has focus.
    pub const TEXTBOX_FOCUSED: u32 = 1 << 5;
    /// Player is in combat.
    pub const IN_COMBAT: u32 = 1 << 6;

    /// Preserves all raw bits, including bits introduced by newer builds.
    #[must_use]
    pub const fn from_raw(value: u32) -> Self {
        Self(value)
    }

    /// Returns all raw bits.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }

    /// Returns whether every requested bit is set.
    #[must_use]
    pub const fn contains(self, mask: u32) -> bool {
        self.0 & mask == mask
    }
}

/// Guild Wars 2-specific binary context embedded in MumbleLink.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MumbleContext {
    /// Connected server address.
    pub server_address: MumbleServerAddress,
    /// Current map identifier.
    pub map_id: u32,
    /// Current map category.
    pub map_type: MumbleMapType,
    /// Shard identifier.
    pub shard_id: u32,
    /// Map instance identifier.
    pub instance_id: u32,
    /// Guild Wars 2 build identifier.
    pub build_id: u32,
    /// Packed context flags.
    pub flags: MumbleContextFlags,
    /// Minimap/compass telemetry.
    pub compass: MumbleCompass,
    /// Guild Wars 2 process identifier.
    pub process_id: u32,
    /// Active mount.
    pub mount_index: MumbleMountIndex,
}

/// Exact Guild Wars 2 MumbleLink shared-memory layout used by Nexus.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MumbleData {
    /// MumbleLink protocol version.
    pub ui_version: u32,
    /// Producer tick counter.
    pub ui_tick: u32,
    /// Avatar position.
    pub avatar_position: MumbleVector3,
    /// Avatar forward vector.
    pub avatar_front: MumbleVector3,
    /// Avatar up vector.
    pub avatar_top: MumbleVector3,
    /// UTF-16 application name.
    pub name: [u16; 256],
    /// Camera position.
    pub camera_position: MumbleVector3,
    /// Camera forward vector.
    pub camera_front: MumbleVector3,
    /// Camera up vector.
    pub camera_top: MumbleVector3,
    /// UTF-16 identity JSON.
    pub identity: [u16; 256],
    /// Legacy context length (Guild Wars 2 reports 48).
    pub context_length: u32,
    /// Guild Wars 2-specific context.
    pub context: MumbleContext,
    /// UTF-16 application description.
    pub description: [u16; 2048],
}

impl Default for MumbleData {
    fn default() -> Self {
        Self {
            ui_version: 0,
            ui_tick: 0,
            avatar_position: MumbleVector3::default(),
            avatar_front: MumbleVector3::default(),
            avatar_top: MumbleVector3::default(),
            name: [0; 256],
            camera_position: MumbleVector3::default(),
            camera_front: MumbleVector3::default(),
            camera_top: MumbleVector3::default(),
            identity: [0; 256],
            context_length: 0,
            context: MumbleContext::default(),
            description: [0; 2048],
        }
    }
}

#[cfg(test)]
mod tests {
    use core::mem::{align_of, offset_of, size_of};

    use super::{
        MumbleCompass, MumbleContext, MumbleData, MumbleIdentity, MumbleServerAddress,
        MumbleVector2, MumbleVector3,
    };

    #[test]
    fn primitive_layouts_match_msvc_x64() {
        assert_eq!(size_of::<MumbleVector2>(), 8);
        assert_eq!(align_of::<MumbleVector2>(), 4);
        assert_eq!(size_of::<MumbleVector3>(), 12);
        assert_eq!(align_of::<MumbleVector3>(), 4);
        assert_eq!(size_of::<MumbleServerAddress>(), 28);
        assert_eq!(align_of::<MumbleServerAddress>(), 4);
    }

    #[test]
    fn identity_layout_matches_msvc_x64() {
        assert_eq!(size_of::<MumbleIdentity>(), 56);
        assert_eq!(align_of::<MumbleIdentity>(), 4);
        assert_eq!(offset_of!(MumbleIdentity, name), 0);
        assert_eq!(offset_of!(MumbleIdentity, profession), 20);
        assert_eq!(offset_of!(MumbleIdentity, specialization), 24);
        assert_eq!(offset_of!(MumbleIdentity, race), 28);
        assert_eq!(offset_of!(MumbleIdentity, map_id), 32);
        assert_eq!(offset_of!(MumbleIdentity, world_id), 36);
        assert_eq!(offset_of!(MumbleIdentity, team_color_id), 40);
        assert_eq!(offset_of!(MumbleIdentity, is_commander), 44);
        assert_eq!(offset_of!(MumbleIdentity, fov), 48);
        assert_eq!(offset_of!(MumbleIdentity, ui_size), 52);
    }

    #[test]
    fn context_layout_matches_msvc_x64() {
        assert_eq!(size_of::<MumbleCompass>(), 28);
        assert_eq!(offset_of!(MumbleCompass, rotation), 4);
        assert_eq!(offset_of!(MumbleCompass, player_position), 8);
        assert_eq!(offset_of!(MumbleCompass, center), 16);
        assert_eq!(offset_of!(MumbleCompass, scale), 24);

        assert_eq!(size_of::<MumbleContext>(), 88);
        assert_eq!(align_of::<MumbleContext>(), 4);
        assert_eq!(offset_of!(MumbleContext, server_address), 0);
        assert_eq!(offset_of!(MumbleContext, map_id), 28);
        assert_eq!(offset_of!(MumbleContext, map_type), 32);
        assert_eq!(offset_of!(MumbleContext, shard_id), 36);
        assert_eq!(offset_of!(MumbleContext, instance_id), 40);
        assert_eq!(offset_of!(MumbleContext, build_id), 44);
        assert_eq!(offset_of!(MumbleContext, flags), 48);
        assert_eq!(offset_of!(MumbleContext, compass), 52);
        assert_eq!(offset_of!(MumbleContext, process_id), 80);
        assert_eq!(offset_of!(MumbleContext, mount_index), 84);
    }

    #[test]
    fn data_layout_matches_pinned_mumble_header() {
        assert_eq!(size_of::<MumbleData>(), 5292);
        assert_eq!(align_of::<MumbleData>(), 4);
        assert_eq!(offset_of!(MumbleData, ui_version), 0);
        assert_eq!(offset_of!(MumbleData, ui_tick), 4);
        assert_eq!(offset_of!(MumbleData, avatar_position), 8);
        assert_eq!(offset_of!(MumbleData, avatar_front), 20);
        assert_eq!(offset_of!(MumbleData, avatar_top), 32);
        assert_eq!(offset_of!(MumbleData, name), 44);
        assert_eq!(offset_of!(MumbleData, camera_position), 556);
        assert_eq!(offset_of!(MumbleData, camera_front), 568);
        assert_eq!(offset_of!(MumbleData, camera_top), 580);
        assert_eq!(offset_of!(MumbleData, identity), 592);
        assert_eq!(offset_of!(MumbleData, context_length), 1104);
        assert_eq!(offset_of!(MumbleData, context), 1108);
        assert_eq!(offset_of!(MumbleData, description), 1196);
    }

    #[test]
    fn open_values_and_flag_bits_are_preserved() {
        let unknown = super::MumbleMapType::from_raw(255);
        assert_eq!(unknown.value(), 255);

        let flags = super::MumbleContextFlags::from_raw(
            super::MumbleContextFlags::GAME_FOCUSED | super::MumbleContextFlags::IN_COMBAT,
        );
        assert!(flags.contains(super::MumbleContextFlags::GAME_FOCUSED));
        assert!(flags.contains(super::MumbleContextFlags::IN_COMBAT));
        assert!(!flags.contains(super::MumbleContextFlags::MAP_OPEN));
    }
}
