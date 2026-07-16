use std::fmt;

/// Open GW2 game-action identifier.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct GameBindId(pub u32);

/// One known GW2 action and its descriptive XML name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownGameBind {
    /// Numeric action identifier.
    pub id: GameBindId,
    /// Descriptive `name` attribute used by `GameBinds.xml`.
    pub name: &'static str,
}

macro_rules! game_bind_ids {
    ($($constant:ident = $value:literal => $name:literal;)+) => {
        impl GameBindId {
            $(
                #[doc = concat!("GW2 game binding `", $name, "`.")]
                pub const $constant: Self = Self($value);
            )+

            /// Returns the descriptive XML name for a known identifier.
            #[must_use]
            pub fn name(self) -> Option<&'static str> {
                KNOWN.iter().find(|entry| entry.id == self).map(|entry| entry.name)
            }

            /// Migrates the legacy standalone swim-up action to the merged jump action.
            #[must_use]
            pub const fn canonical(self) -> Self {
                if self.0 == Self::LEGACY_MOVE_SWIM_UP.0 {
                    Self::MOVE_JUMP_SWIM_UP_FLY_UP
                } else {
                    self
                }
            }
        }

        static KNOWN: &[KnownGameBind] = &[
            $(KnownGameBind { id: GameBindId::$constant, name: $name },)+
        ];
    };
}

game_bind_ids! {
    MOVE_FORWARD = 0 => "((MoveForward))";
    MOVE_BACKWARD = 1 => "((MoveBackward))";
    MOVE_LEFT = 2 => "((MoveLeft))";
    MOVE_RIGHT = 3 => "((MoveRight))";
    MOVE_TURN_LEFT = 4 => "((MoveTurnLeft))";
    MOVE_TURN_RIGHT = 5 => "((MoveTurnRight))";
    MOVE_DODGE = 6 => "((MoveDodge))";
    MOVE_AUTO_RUN = 7 => "((MoveAutoRun))";
    MOVE_WALK = 8 => "((MoveWalk))";
    MOVE_JUMP_SWIM_UP_FLY_UP = 9 => "((MoveJump))";
    LEGACY_MOVE_SWIM_UP = 10 => "((MoveSwimUp))";
    MOVE_SWIM_DOWN_FLY_DOWN = 11 => "((MoveSwimDown))";
    MOVE_ABOUT_FACE = 12 => "((MoveAboutFace))";
    SKILL_WEAPON_SWAP = 17 => "((SkillWeaponSwap))";
    SKILL_WEAPON_1 = 18 => "((SkillWeapon1))";
    SKILL_WEAPON_2 = 19 => "((SkillWeapon2))";
    SKILL_WEAPON_3 = 20 => "((SkillWeapon3))";
    SKILL_WEAPON_4 = 21 => "((SkillWeapon4))";
    SKILL_WEAPON_5 = 22 => "((SkillWeapon5))";
    SKILL_HEAL = 23 => "((SkillHeal))";
    SKILL_UTILITY_1 = 24 => "((SkillUtility1))";
    SKILL_UTILITY_2 = 25 => "((SkillUtility2))";
    SKILL_UTILITY_3 = 26 => "((SkillUtility3))";
    SKILL_ELITE = 27 => "((SkillElite))";
    SKILL_PROFESSION_1 = 28 => "((SkillProfession1))";
    SKILL_PROFESSION_2 = 29 => "((SkillProfession2))";
    SKILL_PROFESSION_3 = 30 => "((SkillProfession3))";
    SKILL_PROFESSION_4 = 31 => "((SkillProfession4))";
    SKILL_PROFESSION_5 = 79 => "((SkillProfession5))";
    SKILL_PROFESSION_6 = 201 => "((SkillProfession6))";
    SKILL_PROFESSION_7 = 202 => "((SkillProfession7))";
    SKILL_SPECIAL_ACTION = 82 => "((SkillSpecialAction))";
    TARGET_ALERT = 131 => "((TargetAlert))";
    TARGET_CALL = 32 => "((TargetCall))";
    TARGET_TAKE = 33 => "((TargetTake))";
    TARGET_CALL_LOCAL = 199 => "((TargetCallLocal))";
    TARGET_TAKE_LOCAL = 200 => "((TargetTakeLocal))";
    TARGET_ENEMY_NEAREST = 34 => "((TargetEnemyNearest))";
    TARGET_ENEMY_NEXT = 35 => "((TargetEnemyNext))";
    TARGET_ENEMY_PREVIOUS = 36 => "((TargetEnemyPrev))";
    TARGET_ALLY_NEAREST = 37 => "((TargetAllyNearest))";
    TARGET_ALLY_NEXT = 38 => "((TargetAllyNext))";
    TARGET_ALLY_PREVIOUS = 39 => "((TargetAllyPrev))";
    TARGET_LOCK = 40 => "((TargetLock))";
    TARGET_SNAP_GROUND = 80 => "((TargetSnapGroundTarget))";
    TARGET_SNAP_GROUND_TOGGLE = 115 => "((TargetSnapGroundTargetToggle))";
    TARGET_AUTO_TARGETING_DISABLE = 116 => "((TargetAutoTargetingDisable))";
    TARGET_AUTO_TARGETING_TOGGLE = 117 => "((TargetAutoTargetingToggle))";
    TARGET_ALLY_MODE = 197 => "((TargetAllyTargetingMode))";
    TARGET_ALLY_MODE_TOGGLE = 198 => "((TargetAllyTargetingModeToggle))";
    UI_COMMERCE = 41 => "((UiCommerce))";
    UI_CONTACTS = 42 => "((UiContacts))";
    UI_GUILD = 43 => "((UiGuild))";
    UI_HERO = 44 => "((UiHero))";
    UI_INVENTORY = 45 => "((UiInventory))";
    UI_KENNEL = 46 => "((UiKennel))";
    UI_LOGOUT = 47 => "((UiLogout))";
    UI_MAIL = 71 => "((UiMail))";
    UI_OPTIONS = 48 => "((UiOptions))";
    UI_PARTY = 49 => "((UiParty))";
    UI_PVP = 73 => "((UiPvp))";
    UI_PVP_BUILD = 75 => "((UiPvpBuild))";
    UI_SCOREBOARD = 50 => "((UiScoreboard))";
    UI_SEASONAL_OBJECTIVES_SHOP = 209 => "((UiSeasonalObjectivesShop))";
    UI_INFORMATION = 51 => "((UiInformation))";
    UI_CHAT_TOGGLE = 70 => "((UiChatToggle))";
    UI_CHAT_COMMAND = 52 => "((UiChatCommand))";
    UI_CHAT_FOCUS = 53 => "((UiChatFocus))";
    UI_CHAT_REPLY = 54 => "((UiChatReply))";
    UI_TOGGLE = 55 => "((UiToggle))";
    UI_SQUAD_BROADCAST_CHAT_TOGGLE = 85 => "((UiSquadBroadcastChatToggle))";
    UI_SQUAD_BROADCAST_CHAT_COMMAND = 83 => "((UiSquadBroadcastChatCommand))";
    UI_SQUAD_BROADCAST_CHAT_FOCUS = 84 => "((UiSquadBroadcastChatFocus))";
    CAMERA_FREE = 13 => "((CameraFree))";
    CAMERA_ZOOM_IN = 14 => "((CameraZoomIn))";
    CAMERA_ZOOM_OUT = 15 => "((CameraZoomOut))";
    CAMERA_REVERSE = 16 => "((CameraReverse))";
    CAMERA_ACTION_MODE = 78 => "((CameraActionMode))";
    CAMERA_ACTION_MODE_DISABLE = 114 => "((CameraActionModeDisable))";
    SCREENSHOT_NORMAL = 56 => "((ScreenshotNormal))";
    SCREENSHOT_STEREOSCOPIC = 57 => "((ScreenshotStereoscopic))";
    MAP_TOGGLE = 59 => "((MapToggle))";
    MAP_FOCUS_PLAYER = 60 => "((MapFocusPlayer))";
    MAP_FLOOR_DOWN = 61 => "((MapFloorDown))";
    MAP_FLOOR_UP = 62 => "((MapFloorUp))";
    MAP_ZOOM_IN = 63 => "((MapZoomIn))";
    MAP_ZOOM_OUT = 64 => "((MapZoomOut))";
    SPUMONI_TOGGLE = 152 => "((SpumoniToggle))";
    SPUMONI_MOVEMENT = 130 => "((SpumoniMovement))";
    SPUMONI_SECONDARY_MOVEMENT = 153 => "((SpumoniSecondaryMovement))";
    SPUMONI_MAM_01 = 155 => "((SpumoniMAM01))";
    SPUMONI_MAM_02 = 156 => "((SpumoniMAM02))";
    SPUMONI_MAM_03 = 157 => "((SpumoniMAM03))";
    SPUMONI_MAM_04 = 158 => "((SpumoniMAM04))";
    SPUMONI_MAM_05 = 159 => "((SpumoniMAM05))";
    SPUMONI_MAM_06 = 161 => "((SpumoniMAM06))";
    SPUMONI_MAM_07 = 169 => "((SpumoniMAM07))";
    SPUMONI_MAM_08 = 170 => "((SpumoniMAM08))";
    SPUMONI_MAM_09 = 203 => "((SpumoniMAM09))";
    SPECTATOR_NEAREST_FIXED = 102 => "((SpectatorNearestFixed))";
    SPECTATOR_NEAREST_PLAYER = 103 => "((SpectatorNearestPlayer))";
    SPECTATOR_PLAYER_RED_1 = 104 => "((SpectatorPlayerRed1))";
    SPECTATOR_PLAYER_RED_2 = 105 => "((SpectatorPlayerRed2))";
    SPECTATOR_PLAYER_RED_3 = 106 => "((SpectatorPlayerRed3))";
    SPECTATOR_PLAYER_RED_4 = 107 => "((SpectatorPlayerRed4))";
    SPECTATOR_PLAYER_RED_5 = 108 => "((SpectatorPlayerRed5))";
    SPECTATOR_PLAYER_BLUE_1 = 109 => "((SpectatorPlayerBlue1))";
    SPECTATOR_PLAYER_BLUE_2 = 110 => "((SpectatorPlayerBlue2))";
    SPECTATOR_PLAYER_BLUE_3 = 111 => "((SpectatorPlayerBlue3))";
    SPECTATOR_PLAYER_BLUE_4 = 112 => "((SpectatorPlayerBlue4))";
    SPECTATOR_PLAYER_BLUE_5 = 113 => "((SpectatorPlayerBlue5))";
    SPECTATOR_FREE_CAMERA = 120 => "((SpectatorFreeCamera))";
    SPECTATOR_FREE_CAMERA_MODE = 127 => "((SpectatorFreeCameraMode))";
    SPECTATOR_FREE_MOVE_FORWARD = 121 => "((SpectatorFreeMoveForward))";
    SPECTATOR_FREE_MOVE_BACKWARD = 122 => "((SpectatorFreeMoveBackward))";
    SPECTATOR_FREE_MOVE_LEFT = 123 => "((SpectatorFreeMoveLeft))";
    SPECTATOR_FREE_MOVE_RIGHT = 124 => "((SpectatorFreeMoveRight))";
    SPECTATOR_FREE_MOVE_UP = 125 => "((SpectatorFreeMoveUp))";
    SPECTATOR_FREE_MOVE_DOWN = 126 => "((SpectatorFreeMoveDown))";
    SQUAD_MARKER_PLACE_WORLD_1 = 86 => "((SquadMarkerPlaceWorld1))";
    SQUAD_MARKER_PLACE_WORLD_2 = 87 => "((SquadMarkerPlaceWorld2))";
    SQUAD_MARKER_PLACE_WORLD_3 = 88 => "((SquadMarkerPlaceWorld3))";
    SQUAD_MARKER_PLACE_WORLD_4 = 89 => "((SquadMarkerPlaceWorld4))";
    SQUAD_MARKER_PLACE_WORLD_5 = 90 => "((SquadMarkerPlaceWorld5))";
    SQUAD_MARKER_PLACE_WORLD_6 = 91 => "((SquadMarkerPlaceWorld6))";
    SQUAD_MARKER_PLACE_WORLD_7 = 92 => "((SquadMarkerPlaceWorld7))";
    SQUAD_MARKER_PLACE_WORLD_8 = 93 => "((SquadMarkerPlaceWorld8))";
    SQUAD_MARKER_CLEAR_ALL_WORLD = 119 => "((SquadMarkerClearAllWorld))";
    SQUAD_MARKER_SET_AGENT_1 = 94 => "((SquadMarkerSetAgent1))";
    SQUAD_MARKER_SET_AGENT_2 = 95 => "((SquadMarkerSetAgent2))";
    SQUAD_MARKER_SET_AGENT_3 = 96 => "((SquadMarkerSetAgent3))";
    SQUAD_MARKER_SET_AGENT_4 = 97 => "((SquadMarkerSetAgent4))";
    SQUAD_MARKER_SET_AGENT_5 = 98 => "((SquadMarkerSetAgent5))";
    SQUAD_MARKER_SET_AGENT_6 = 99 => "((SquadMarkerSetAgent6))";
    SQUAD_MARKER_SET_AGENT_7 = 100 => "((SquadMarkerSetAgent7))";
    SQUAD_MARKER_SET_AGENT_8 = 101 => "((SquadMarkerSetAgent8))";
    SQUAD_MARKER_CLEAR_ALL_AGENT = 118 => "((SquadMarkerClearAllAgent))";
    MASTERY_ACCESS = 196 => "((MasteryAccess))";
    MASTERY_ACCESS_01 = 204 => "((MasteryAccess01))";
    MASTERY_ACCESS_02 = 205 => "((MasteryAccess02))";
    MASTERY_ACCESS_03 = 206 => "((MasteryAccess03))";
    MASTERY_ACCESS_04 = 207 => "((MasteryAccess04))";
    MASTERY_ACCESS_05 = 208 => "((MasteryAccess05))";
    MASTERY_ACCESS_06 = 211 => "((MasteryAccess06))";
    MISC_AOE_LOOT = 74 => "((MiscAoELoot))";
    MISC_INTERACT = 65 => "((MiscInteract))";
    MISC_SHOW_ENEMIES = 66 => "((MiscShowEnemies))";
    MISC_SHOW_ALLIES = 67 => "((MiscShowAllies))";
    MISC_COMBAT_STANCE = 68 => "((MiscCombatStance))";
    MISC_TOGGLE_LANGUAGE = 69 => "((MiscToggleLanguage))";
    MISC_TOGGLE_PET_COMBAT = 76 => "((MiscTogglePetCombat))";
    MISC_TOGGLE_FULL_SCREEN = 160 => "((MiscToggleFullScreen))";
    MISC_TOGGLE_DECORATION_MODE = 210 => "((MiscToggleDecorationMode))";
    TOY_USE_DEFAULT = 162 => "((ToyUseDefault))";
    TOY_USE_SLOT_1 = 163 => "((ToyUseSlot1))";
    TOY_USE_SLOT_2 = 164 => "((ToyUseSlot2))";
    TOY_USE_SLOT_3 = 165 => "((ToyUseSlot3))";
    TOY_USE_SLOT_4 = 166 => "((ToyUseSlot4))";
    TOY_USE_SLOT_5 = 167 => "((ToyUseSlot5))";
    LOADOUT_1 = 171 => "((Loadout1))";
    LOADOUT_2 = 172 => "((Loadout2))";
    LOADOUT_3 = 173 => "((Loadout3))";
    LOADOUT_4 = 174 => "((Loadout4))";
    LOADOUT_5 = 175 => "((Loadout5))";
    LOADOUT_6 = 176 => "((Loadout6))";
    LOADOUT_7 = 177 => "((Loadout7))";
    LOADOUT_8 = 178 => "((Loadout8))";
    LOADOUT_9 = 179 => "((Loadout9))";
    GEAR_LOADOUT_1 = 182 => "((GearLoadout1))";
    GEAR_LOADOUT_2 = 183 => "((GearLoadout2))";
    GEAR_LOADOUT_3 = 184 => "((GearLoadout3))";
    GEAR_LOADOUT_4 = 185 => "((GearLoadout4))";
    GEAR_LOADOUT_5 = 186 => "((GearLoadout5))";
    GEAR_LOADOUT_6 = 187 => "((GearLoadout6))";
    GEAR_LOADOUT_7 = 188 => "((GearLoadout7))";
    GEAR_LOADOUT_8 = 189 => "((GearLoadout8))";
    GEAR_LOADOUT_9 = 190 => "((GearLoadout9))";
}

/// Returns every action recognized by the pinned C++ compatibility baseline.
#[must_use]
pub const fn known_game_binds() -> &'static [KnownGameBind] {
    KNOWN
}

impl fmt::Debug for GameBindId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = self.name() {
            formatter
                .debug_struct("GameBindId")
                .field("id", &self.0)
                .field("name", &name)
                .finish()
        } else {
            formatter.debug_tuple("GameBindId").field(&self.0).finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn known_ids_are_unique_and_legacy_alias_is_canonicalized() {
        let ids: BTreeSet<u32> = known_game_binds().iter().map(|entry| entry.id.0).collect();
        assert_eq!(ids.len(), known_game_binds().len());
        assert_eq!(
            GameBindId::LEGACY_MOVE_SWIM_UP.canonical(),
            GameBindId::MOVE_JUMP_SWIM_UP_FLY_UP
        );
    }

    #[test]
    fn unknown_ids_remain_open() {
        let unknown = GameBindId(u32::MAX);
        assert_eq!(unknown.name(), None);
        assert_eq!(unknown.canonical(), unknown);
    }
}
