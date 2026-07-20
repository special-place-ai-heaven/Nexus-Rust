use crate::encoding::{
    async_key_is_down, key_lparam, key_message_id, modifier_virtual_key, mouse_message_id,
    mouse_wparam, point_lparam, wire_message_id,
};
use nexus_input::{
    GameMessage, GameMessageSink, GameOnlyMessageSink, GameSinkError, ModifierState,
    PhysicalInputState,
};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use windows_sys::Win32::Foundation::{FALSE, HWND, LPARAM, POINT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MAPVK_VSC_TO_VK_EX, MapVirtualKeyW, VK_CONTROL, VK_MENU,
    VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, IsWindow, PostMessageW};

/// A redacted window-attachment failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WindowAttachError {
    /// The supplied token is null or does not identify a live Win32 window.
    #[error("the game window is invalid")]
    InvalidWindow,
    /// The process exhausted the monotonic attachment generation.
    #[error("the game window attachment generation is exhausted")]
    GenerationExhausted,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct WindowAttachment {
    generation: u64,
    hwnd: NonZeroUsize,
}

struct WindowState {
    next_generation: u64,
    active: Option<WindowAttachment>,
}

/// Thread-safe Win32 boundary for game messages and physical modifiers.
///
/// Store this value in an Arc to attach a window and to supply cloned
/// trait-object references to nexus_input::GameInvoker.
pub struct Win32GameInput {
    state: Mutex<WindowState>,
}

impl Win32GameInput {
    /// Creates a detached adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(WindowState {
                next_generation: 0,
                active: None,
            }),
        }
    }

    /// Validates and selects a game window, replacing any previous selection.
    ///
    /// Dropping an older lease cannot detach the replacement.
    pub fn attach(self: &Arc<Self>, hwnd: HWND) -> Result<GameWindowLease, WindowAttachError> {
        let hwnd = validate_window(hwnd)?;
        let generation = {
            let mut state = self.lock_state();
            let generation = state
                .next_generation
                .checked_add(1)
                .ok_or(WindowAttachError::GenerationExhausted)?;
            state.next_generation = generation;
            state.active = Some(WindowAttachment { generation, hwnd });
            generation
        };
        Ok(GameWindowLease {
            adapter: Arc::clone(self),
            generation,
        })
    }

    /// Returns whether a currently selected window remains attached.
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.lock_state().active.is_some()
    }

    fn detach(&self, generation: u64) {
        let mut state = self.lock_state();
        if state
            .active
            .is_some_and(|active| active.generation == generation)
        {
            state.active = None;
        }
    }

    fn is_generation_current(&self, generation: u64) -> bool {
        self.lock_state()
            .active
            .is_some_and(|active| active.generation == generation)
    }

    fn lock_state(&self) -> MutexGuard<'_, WindowState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for Win32GameInput {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Win32GameInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Win32GameInput")
            .field("attached", &self.is_attached())
            .finish()
    }
}

impl PhysicalInputState for Win32GameInput {
    fn modifiers(&self) -> ModifierState {
        ModifierState {
            alt: physical_key_down(VK_MENU),
            control: physical_key_down(VK_CONTROL),
            shift: physical_key_down(VK_SHIFT),
        }
    }
}

impl GameMessageSink for Win32GameInput {
    fn send_batch(&self, messages: &[GameMessage]) -> Result<(), GameSinkError> {
        let mut state = self.lock_state();
        let active = state.active.ok_or(GameSinkError)?;
        let hwnd = active.hwnd.get() as HWND;
        if !is_window(hwnd) {
            state.active = None;
            return Err(GameSinkError);
        }

        for message in messages {
            let native = encode_message(message)?;
            // SAFETY: hwnd was validated above and PostMessageW copies the
            // scalar message tuple before returning. It invokes no callback.
            let posted = unsafe {
                PostMessageW(
                    hwnd,
                    wire_message_id(native.message),
                    native.wparam,
                    native.lparam,
                )
            };
            if posted == FALSE {
                if !is_window(hwnd) {
                    state.active = None;
                }
                return Err(GameSinkError);
            }
        }
        Ok(())
    }
}

impl GameOnlyMessageSink for Win32GameInput {
    fn send_to_game_only(
        &self,
        message: u32,
        w_param: usize,
        l_param: isize,
    ) -> Result<(), GameSinkError> {
        let mut state = self.lock_state();
        let active = state.active.ok_or(GameSinkError)?;
        let hwnd = active.hwnd.get() as HWND;
        if !is_window(hwnd) {
            state.active = None;
            return Err(GameSinkError);
        }

        let posted = unsafe {
            // SAFETY: hwnd was validated above and PostMessageW copies this
            // scalar tuple before returning. It invokes no callback.
            PostMessageW(hwnd, wire_message_id(message), w_param, l_param)
        };
        if posted == FALSE {
            if !is_window(hwnd) {
                state.active = None;
            }
            return Err(GameSinkError);
        }
        Ok(())
    }
}

/// Generation-scoped ownership of the selected game window.
///
/// Dropping or explicitly detaching this lease clears the window only while
/// this generation is still current.
pub struct GameWindowLease {
    adapter: Arc<Win32GameInput>,
    generation: u64,
}

impl GameWindowLease {
    /// Returns whether this lease still owns the selected window.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.adapter.is_generation_current(self.generation)
    }

    /// Explicitly detaches this generation.
    pub fn detach(self) {}
}

impl fmt::Debug for GameWindowLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GameWindowLease")
            .field("current", &self.is_current())
            .finish()
    }
}

impl Drop for GameWindowLease {
    fn drop(&mut self) {
        self.adapter.detach(self.generation);
    }
}

#[derive(Clone, Copy)]
struct NativeMessage {
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
}

fn encode_message(message: &GameMessage) -> Result<NativeMessage, GameSinkError> {
    match *message {
        GameMessage::Modifier {
            modifier,
            pressed,
            system,
        } => {
            let virtual_key = modifier_virtual_key(modifier);
            // SAFETY: MapVirtualKeyW accepts every numeric virtual-key token.
            let scan_code = unsafe { MapVirtualKeyW(virtual_key, MAPVK_VK_TO_VSC_EX) };
            let scan_code = u16::try_from(scan_code).map_err(|_| GameSinkError)?;
            if scan_code == 0 {
                return Err(GameSinkError);
            }
            Ok(NativeMessage {
                message: key_message_id(pressed, system),
                wparam: virtual_key as usize,
                lparam: key_lparam(scan_code, pressed, system),
            })
        }
        GameMessage::Keyboard {
            scan_code,
            pressed,
            system,
        } => {
            // SAFETY: MapVirtualKeyW accepts every numeric scan-code token.
            let virtual_key = unsafe { MapVirtualKeyW(u32::from(scan_code), MAPVK_VSC_TO_VK_EX) };
            if virtual_key == 0 {
                return Err(GameSinkError);
            }
            Ok(NativeMessage {
                message: key_message_id(pressed, system),
                wparam: virtual_key as usize,
                lparam: key_lparam(scan_code, pressed, system),
            })
        }
        GameMessage::Mouse {
            button,
            pressed,
            modifiers,
        } => {
            let mut point = POINT::default();
            // SAFETY: point is valid writable storage for the duration of the call.
            if unsafe { GetCursorPos(&mut point) } == FALSE {
                return Err(GameSinkError);
            }
            Ok(NativeMessage {
                message: mouse_message_id(button, pressed).ok_or(GameSinkError)?,
                wparam: mouse_wparam(button, pressed, modifiers).ok_or(GameSinkError)?,
                lparam: point_lparam(point.x, point.y),
            })
        }
    }
}

fn physical_key_down(virtual_key: u16) -> bool {
    // SAFETY: GetAsyncKeyState accepts every virtual-key token.
    async_key_is_down(unsafe { GetAsyncKeyState(i32::from(virtual_key)) })
}

fn validate_window(hwnd: HWND) -> Result<NonZeroUsize, WindowAttachError> {
    if !is_window(hwnd) {
        return Err(WindowAttachError::InvalidWindow);
    }
    NonZeroUsize::new(hwnd as usize).ok_or(WindowAttachError::InvalidWindow)
}

fn is_window(hwnd: HWND) -> bool {
    // SAFETY: IsWindow accepts an arbitrary HWND token without dereferencing it.
    !hwnd.is_null() && unsafe { IsWindow(hwnd) } != FALSE
}
