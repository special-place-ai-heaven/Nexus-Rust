use nexus_imgui_compat::sys;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::UI::Input::XboxController::{
    XINPUT_CAPABILITIES, XINPUT_FLAG_GAMEPAD, XINPUT_GAMEPAD, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B,
    XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
    XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB_DEADZONE,
    XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE,
    XInputGetCapabilities, XInputGetState,
};

const FLAG_HAS_GAMEPAD: i32 = sys::ImGuiBackendFlags_HasGamepad as i32;
const CONFIG_NAV_GAMEPAD: i32 = sys::ImGuiConfigFlags_NavEnableGamepad as i32;

pub(crate) struct GamepadState {
    refresh_capabilities: bool,
    available: bool,
}

impl Default for GamepadState {
    fn default() -> Self {
        Self {
            refresh_capabilities: true,
            available: false,
        }
    }
}

impl GamepadState {
    pub(crate) const fn request_refresh(&mut self) {
        self.refresh_capabilities = true;
    }

    pub(crate) fn update(&mut self, io: &mut sys::ImGuiIO) {
        io.NavInputs.fill(0.0);
        io.BackendFlags &= !FLAG_HAS_GAMEPAD;
        if io.ConfigFlags & CONFIG_NAV_GAMEPAD == 0 {
            return;
        }

        if self.refresh_capabilities {
            let mut capabilities = XINPUT_CAPABILITIES::default();
            // SAFETY: the output pointer is valid for the duration of the call.
            self.available = unsafe {
                XInputGetCapabilities(0, XINPUT_FLAG_GAMEPAD, &mut capabilities) == ERROR_SUCCESS
            };
            self.refresh_capabilities = false;
        }
        if !self.available {
            return;
        }

        let mut state = XINPUT_STATE::default();
        // SAFETY: the output pointer is valid for the duration of the call.
        if unsafe { XInputGetState(0, &mut state) } != ERROR_SUCCESS {
            self.available = false;
            return;
        }
        io.BackendFlags |= FLAG_HAS_GAMEPAD;
        map_gamepad(io, state.Gamepad);
    }
}

fn map_gamepad(io: &mut sys::ImGuiIO, gamepad: XINPUT_GAMEPAD) {
    map_button(io, sys::ImGuiNavInput_Activate, gamepad, XINPUT_GAMEPAD_A);
    map_button(io, sys::ImGuiNavInput_Cancel, gamepad, XINPUT_GAMEPAD_B);
    map_button(io, sys::ImGuiNavInput_Menu, gamepad, XINPUT_GAMEPAD_X);
    map_button(io, sys::ImGuiNavInput_Input, gamepad, XINPUT_GAMEPAD_Y);
    map_button(
        io,
        sys::ImGuiNavInput_DpadLeft,
        gamepad,
        XINPUT_GAMEPAD_DPAD_LEFT,
    );
    map_button(
        io,
        sys::ImGuiNavInput_DpadRight,
        gamepad,
        XINPUT_GAMEPAD_DPAD_RIGHT,
    );
    map_button(
        io,
        sys::ImGuiNavInput_DpadUp,
        gamepad,
        XINPUT_GAMEPAD_DPAD_UP,
    );
    map_button(
        io,
        sys::ImGuiNavInput_DpadDown,
        gamepad,
        XINPUT_GAMEPAD_DPAD_DOWN,
    );
    map_button(
        io,
        sys::ImGuiNavInput_FocusPrev,
        gamepad,
        XINPUT_GAMEPAD_LEFT_SHOULDER,
    );
    map_button(
        io,
        sys::ImGuiNavInput_FocusNext,
        gamepad,
        XINPUT_GAMEPAD_RIGHT_SHOULDER,
    );
    map_button(
        io,
        sys::ImGuiNavInput_TweakSlow,
        gamepad,
        XINPUT_GAMEPAD_LEFT_SHOULDER,
    );
    map_button(
        io,
        sys::ImGuiNavInput_TweakFast,
        gamepad,
        XINPUT_GAMEPAD_RIGHT_SHOULDER,
    );

    let deadzone = i32::from(XINPUT_GAMEPAD_LEFT_THUMB_DEADZONE);
    map_analog(
        io,
        sys::ImGuiNavInput_LStickLeft,
        i32::from(gamepad.sThumbLX),
        -deadzone,
        i32::from(i16::MIN),
    );
    map_analog(
        io,
        sys::ImGuiNavInput_LStickRight,
        i32::from(gamepad.sThumbLX),
        deadzone,
        i32::from(i16::MAX),
    );
    map_analog(
        io,
        sys::ImGuiNavInput_LStickUp,
        i32::from(gamepad.sThumbLY),
        deadzone,
        i32::from(i16::MAX),
    );
    map_analog(
        io,
        sys::ImGuiNavInput_LStickDown,
        i32::from(gamepad.sThumbLY),
        -deadzone,
        -i32::from(i16::MAX),
    );
}

fn map_button(io: &mut sys::ImGuiIO, nav: u32, gamepad: XINPUT_GAMEPAD, mask: u16) {
    io.NavInputs[nav as usize] = f32::from(gamepad.wButtons & mask != 0);
}

fn map_analog(io: &mut sys::ImGuiIO, nav: u32, value: i32, start: i32, end: i32) {
    let normalized = ((value - start) as f32 / (end - start) as f32).clamp(0.0, 1.0);
    let slot = &mut io.NavInputs[nav as usize];
    *slot = slot.max(normalized);
}

#[cfg(test)]
mod tests {
    use nexus_imgui_compat::sys;
    use windows_sys::Win32::UI::Input::XboxController::{
        XINPUT_GAMEPAD, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_LEFT_THUMB_DEADZONE,
    };

    use super::map_gamepad;

    #[test]
    fn button_and_deadzone_mapping_match_imgui_180_backend() {
        // SAFETY: the bindgen IO type is C data whose all-zero representation is
        // used only as a local mapping target in this test.
        let mut io: sys::ImGuiIO = unsafe { core::mem::zeroed() };
        let gamepad = XINPUT_GAMEPAD {
            wButtons: XINPUT_GAMEPAD_A,
            sThumbLX: i16::try_from(XINPUT_GAMEPAD_LEFT_THUMB_DEADZONE).unwrap_or_default(),
            ..XINPUT_GAMEPAD::default()
        };
        map_gamepad(&mut io, gamepad);
        assert_eq!(io.NavInputs[sys::ImGuiNavInput_Activate as usize], 1.0);
        assert_eq!(io.NavInputs[sys::ImGuiNavInput_LStickRight as usize], 0.0);
    }
}
