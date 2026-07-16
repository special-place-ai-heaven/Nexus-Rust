use core::ffi::c_void;
use core::marker::PhantomData;
use core::ptr;
use std::rc::Rc;
use std::time::Duration;

use nexus_imgui_compat::sys;
use nexus_imgui_runtime::{ContextError, FrameConfig, ImGuiContextOwner, PlatformCapabilities};
use thiserror::Error;
use windows_sys::Win32::Foundation::{GetLastError, HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_INSERT,
    VK_LEFT, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetCursorPos, GetForegroundWindow, IDC_ARROW, IDC_HAND, IDC_IBEAM, IDC_NO,
    IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IsChild, IsWindow,
    LoadCursorW, SetCursor, SetCursorPos,
};

use crate::gamepad::GamepadState;
use crate::message::{MessageOutcome, apply_message, clear_pressed_input};

const FLAG_NO_MOUSE_CURSOR_CHANGE: i32 = sys::ImGuiConfigFlags_NoMouseCursorChange as i32;
const CURSOR_UNINITIALIZED: i32 = i32::MIN;

/// Closed, redaction-safe Win32 platform failures.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PlatformError {
    /// A null or non-window `HWND` was supplied.
    #[error("the selected primary swap chain has no valid Win32 window")]
    InvalidWindow,
    /// The platform was explicitly detached.
    #[error("the Win32 platform backend is detached")]
    Detached,
    /// Win32 could not initialize the high-resolution clock.
    #[error("Win32 could not initialize the frame clock (error {code})")]
    ClockInitialization {
        /// Win32 error code.
        code: u32,
    },
    /// Win32 could not sample the high-resolution clock.
    #[error("Win32 could not sample the frame clock (error {code})")]
    ClockSample {
        /// Win32 error code.
        code: u32,
    },
    /// The sampled counter did not advance.
    #[error("the Win32 frame clock did not advance monotonically")]
    NonMonotonicClock,
    /// Win32 could not read the selected window's client rectangle.
    #[error("Win32 could not read the primary client rectangle (error {code})")]
    ClientRect {
        /// Win32 error code.
        code: u32,
    },
    /// The Dear ImGui context boundary rejected platform configuration.
    #[error(transparent)]
    Context(#[from] ContextError),
}

/// Explicit platform binding for exactly one classifier-selected primary HWND.
pub struct Win32Platform {
    hwnd: HWND,
    ticks_per_second: i64,
    previous_tick: i64,
    last_cursor: i32,
    gamepad: GamepadState,
    attached: bool,
    _thread_bound: PhantomData<Rc<()>>,
}

impl Win32Platform {
    /// Binds the platform backend to one already-selected primary window.
    pub fn attach(hwnd: HWND, context: &mut ImGuiContextOwner) -> Result<Self, PlatformError> {
        validate_window(hwnd)?;
        let (ticks_per_second, previous_tick) = initialize_clock()?;
        configure_io(context, hwnd)?;
        context.set_platform_capabilities(PlatformCapabilities {
            installed: true,
            has_gamepad: false,
            has_mouse_cursors: true,
            has_set_mouse_pos: true,
        })?;
        Ok(Self {
            hwnd,
            ticks_per_second,
            previous_tick,
            last_cursor: CURSOR_UNINITIALIZED,
            gamepad: GamepadState::default(),
            attached: true,
            _thread_bound: PhantomData,
        })
    }

    /// Returns the only HWND whose messages this backend will observe.
    #[must_use]
    pub const fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Explicitly rebinds after the DXGI classifier changes primary sessions.
    /// Messages from all other windows remain ignored.
    pub fn rebind(
        &mut self,
        hwnd: HWND,
        context: &mut ImGuiContextOwner,
    ) -> Result<(), PlatformError> {
        validate_window(hwnd)?;
        clear_context_input(context)?;
        let (ticks_per_second, previous_tick) = initialize_clock()?;
        self.hwnd = hwnd;
        self.ticks_per_second = ticks_per_second;
        self.previous_tick = previous_tick;
        self.last_cursor = CURSOR_UNINITIALIZED;
        self.gamepad = GamepadState::default();
        self.attached = true;
        configure_io(context, hwnd)
    }

    /// Clears input and backend claims before context teardown.
    pub fn detach(&mut self, context: &mut ImGuiContextOwner) -> Result<(), PlatformError> {
        clear_context_input(context)?;
        context.set_platform_capabilities(PlatformCapabilities::default())?;
        self.attached = false;
        Ok(())
    }

    /// Translates a message only when it belongs to the bound primary HWND.
    pub fn handle_message(
        &mut self,
        context: &mut ImGuiContextOwner,
        hwnd: HWND,
        message: u32,
        wparam: usize,
        lparam: isize,
    ) -> Result<MessageOutcome, PlatformError> {
        if !self.attached {
            return Err(PlatformError::Detached);
        }
        if hwnd != self.hwnd {
            return Ok(MessageOutcome::default());
        }

        context.with_current(|| {
            // SAFETY: the owner made its live context current.
            let io = unsafe { sys::igGetIO() };
            if io.is_null() {
                return Err(PlatformError::Context(ContextError::IoUnavailable));
            }
            // SAFETY: the exclusive context borrow keeps this IO valid.
            let io = unsafe { &mut *io };
            let outcome = apply_message(io, message, wparam, lparam);
            if outcome.refresh_gamepad {
                self.gamepad.request_refresh();
            }
            if outcome.refresh_cursor {
                self.last_cursor = CURSOR_UNINITIALIZED;
                self.update_cursor(io);
            }
            Ok(outcome)
        })
    }

    /// Samples window/input state and returns the validated inputs for
    /// `ImGuiContextOwner::begin_frame`.
    pub fn prepare_frame(
        &mut self,
        context: &mut ImGuiContextOwner,
    ) -> Result<FrameConfig, PlatformError> {
        if !self.attached {
            return Err(PlatformError::Detached);
        }
        let display_size = self.client_size()?;
        let delta_time = self.sample_delta()?;
        context.with_current(|| {
            // SAFETY: the owner made its live context current.
            let io = unsafe { sys::igGetIO() };
            if io.is_null() {
                return Err(PlatformError::Context(ContextError::IoUnavailable));
            }
            // SAFETY: the exclusive context borrow keeps this IO valid.
            let io = unsafe { &mut *io };
            update_modifiers(io);
            self.update_mouse_position(io);
            self.update_cursor_if_changed(io);
            self.gamepad.update(io);
            Ok(())
        })?;
        Ok(FrameConfig {
            display_size,
            framebuffer_scale: [1.0, 1.0],
            delta_time,
        })
    }

    fn client_size(&self) -> Result<[f32; 2], PlatformError> {
        let mut rect = RECT::default();
        // SAFETY: `self.hwnd` remains the explicitly bound live window and the
        // output rectangle is valid.
        if unsafe { GetClientRect(self.hwnd, &mut rect) } == 0 {
            // SAFETY: `GetLastError` has no preconditions.
            let code = unsafe { GetLastError() };
            return Err(PlatformError::ClientRect { code });
        }
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width < 0 || height < 0 {
            return Err(PlatformError::ClientRect { code: 0 });
        }
        Ok([width as f32, height as f32])
    }

    fn sample_delta(&mut self) -> Result<Duration, PlatformError> {
        let mut current = 0_i64;
        // SAFETY: the output pointer is valid.
        if unsafe { QueryPerformanceCounter(&mut current) } == 0 {
            // SAFETY: `GetLastError` has no preconditions.
            let code = unsafe { GetLastError() };
            return Err(PlatformError::ClockSample { code });
        }
        if current <= self.previous_tick {
            return Err(PlatformError::NonMonotonicClock);
        }
        let elapsed = (current - self.previous_tick) as f64 / self.ticks_per_second as f64;
        if !elapsed.is_finite() || elapsed <= 0.0 {
            return Err(PlatformError::NonMonotonicClock);
        }
        self.previous_tick = current;
        Ok(Duration::from_secs_f64(elapsed))
    }

    fn update_mouse_position(&self, io: &mut sys::ImGuiIO) {
        if io.WantSetMousePos {
            let mut point = POINT {
                x: io.MousePos.x as i32,
                y: io.MousePos.y as i32,
            };
            // SAFETY: the window and point are valid; failures simply leave the
            // cursor unchanged.
            if unsafe { ClientToScreen(self.hwnd, &mut point) } != 0 {
                // SAFETY: integer screen coordinates are always valid input.
                let _ = unsafe { SetCursorPos(point.x, point.y) };
            }
        }

        io.MousePos = sys::ImVec2 {
            x: -f32::MAX,
            y: -f32::MAX,
        };
        // SAFETY: querying the foreground window has no preconditions.
        let foreground = unsafe { GetForegroundWindow() };
        // SAFETY: both HWND values are live or null; `IsChild` tolerates null.
        let accepts_mouse = foreground == self.hwnd
            || (!foreground.is_null() && unsafe { IsChild(self.hwnd, foreground) } != 0);
        if !accepts_mouse {
            return;
        }
        let mut point = POINT::default();
        // SAFETY: the output pointer is valid.
        let read_cursor = unsafe { GetCursorPos(&mut point) } != 0;
        // SAFETY: the bound HWND and point pointer are valid.
        let converted = read_cursor && unsafe { ScreenToClient(self.hwnd, &mut point) } != 0;
        if converted {
            io.MousePos = sys::ImVec2 {
                x: point.x as f32,
                y: point.y as f32,
            };
        }
    }

    fn update_cursor_if_changed(&mut self, io: &sys::ImGuiIO) {
        let cursor = if io.MouseDrawCursor {
            sys::ImGuiMouseCursor_None
        } else {
            // SAFETY: the owning context is current during this method.
            unsafe { sys::igGetMouseCursor() }
        };
        if cursor != self.last_cursor {
            self.last_cursor = cursor;
            self.update_cursor(io);
        }
    }

    fn update_cursor(&self, io: &sys::ImGuiIO) {
        if io.ConfigFlags & FLAG_NO_MOUSE_CURSOR_CHANGE != 0 {
            return;
        }
        let cursor = if io.MouseDrawCursor {
            sys::ImGuiMouseCursor_None
        } else {
            // SAFETY: the owning context is current during this method.
            unsafe { sys::igGetMouseCursor() }
        };
        if cursor == sys::ImGuiMouseCursor_None {
            // SAFETY: a null cursor hides the OS cursor.
            unsafe { SetCursor(ptr::null_mut()) };
            return;
        }
        let resource = match cursor {
            sys::ImGuiMouseCursor_TextInput => IDC_IBEAM,
            sys::ImGuiMouseCursor_ResizeAll => IDC_SIZEALL,
            sys::ImGuiMouseCursor_ResizeEW => IDC_SIZEWE,
            sys::ImGuiMouseCursor_ResizeNS => IDC_SIZENS,
            sys::ImGuiMouseCursor_ResizeNESW => IDC_SIZENESW,
            sys::ImGuiMouseCursor_ResizeNWSE => IDC_SIZENWSE,
            sys::ImGuiMouseCursor_Hand => IDC_HAND,
            sys::ImGuiMouseCursor_NotAllowed => IDC_NO,
            _ => IDC_ARROW,
        };
        // SAFETY: a null module handle selects the predefined cursor resource.
        let handle = unsafe { LoadCursorW(ptr::null_mut(), resource) };
        if !handle.is_null() {
            // SAFETY: `LoadCursorW` returned a shared system cursor handle.
            unsafe { SetCursor(handle) };
        }
    }
}

fn validate_window(hwnd: HWND) -> Result<(), PlatformError> {
    // SAFETY: `IsWindow` accepts an arbitrary HWND token.
    if hwnd.is_null() || unsafe { IsWindow(hwnd) } == 0 {
        Err(PlatformError::InvalidWindow)
    } else {
        Ok(())
    }
}

fn initialize_clock() -> Result<(i64, i64), PlatformError> {
    let mut frequency = 0_i64;
    let mut current = 0_i64;
    // SAFETY: both output pointers are valid.
    if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 || frequency <= 0 {
        // SAFETY: `GetLastError` has no preconditions.
        let code = unsafe { GetLastError() };
        return Err(PlatformError::ClockInitialization { code });
    }
    // SAFETY: the output pointer is valid.
    if unsafe { QueryPerformanceCounter(&mut current) } == 0 {
        // SAFETY: `GetLastError` has no preconditions.
        let code = unsafe { GetLastError() };
        return Err(PlatformError::ClockInitialization { code });
    }
    Ok((frequency, current))
}

fn configure_io(context: &mut ImGuiContextOwner, hwnd: HWND) -> Result<(), PlatformError> {
    context.with_current(|| {
        // SAFETY: the owner made its live context current.
        let io = unsafe { sys::igGetIO() };
        if io.is_null() {
            return Err(PlatformError::Context(ContextError::IoUnavailable));
        }
        // SAFETY: the exclusive owner borrow keeps this IO valid.
        let io = unsafe { &mut *io };
        io.ImeWindowHandle = hwnd.cast::<c_void>();
        set_key_map(io);
        Ok(())
    })
}

fn clear_context_input(context: &mut ImGuiContextOwner) -> Result<(), PlatformError> {
    context.with_current(|| {
        // SAFETY: the owner made its live context current.
        let io = unsafe { sys::igGetIO() };
        if io.is_null() {
            return Err(PlatformError::Context(ContextError::IoUnavailable));
        }
        // SAFETY: the exclusive owner borrow keeps this IO valid.
        let io = unsafe { &mut *io };
        clear_pressed_input(io);
        io.MousePos = sys::ImVec2 {
            x: -f32::MAX,
            y: -f32::MAX,
        };
        io.ImeWindowHandle = ptr::null_mut();
        Ok(())
    })
}

fn set_key_map(io: &mut sys::ImGuiIO) {
    let mappings = [
        (sys::ImGuiKey_Tab, VK_TAB),
        (sys::ImGuiKey_LeftArrow, VK_LEFT),
        (sys::ImGuiKey_RightArrow, VK_RIGHT),
        (sys::ImGuiKey_UpArrow, VK_UP),
        (sys::ImGuiKey_DownArrow, VK_DOWN),
        (sys::ImGuiKey_PageUp, VK_PRIOR),
        (sys::ImGuiKey_PageDown, VK_NEXT),
        (sys::ImGuiKey_Home, VK_HOME),
        (sys::ImGuiKey_End, VK_END),
        (sys::ImGuiKey_Insert, VK_INSERT),
        (sys::ImGuiKey_Delete, VK_DELETE),
        (sys::ImGuiKey_Backspace, VK_BACK),
        (sys::ImGuiKey_Space, VK_SPACE),
        (sys::ImGuiKey_Enter, VK_RETURN),
        (sys::ImGuiKey_Escape, VK_ESCAPE),
        (sys::ImGuiKey_KeyPadEnter, VK_RETURN),
        (sys::ImGuiKey_A, u16::from(b'A')),
        (sys::ImGuiKey_C, u16::from(b'C')),
        (sys::ImGuiKey_V, u16::from(b'V')),
        (sys::ImGuiKey_X, u16::from(b'X')),
        (sys::ImGuiKey_Y, u16::from(b'Y')),
        (sys::ImGuiKey_Z, u16::from(b'Z')),
    ];
    for (key, virtual_key) in mappings {
        io.KeyMap[key as usize] = i32::from(virtual_key);
    }
}

fn update_modifiers(io: &mut sys::ImGuiIO) {
    // SAFETY: `GetKeyState` accepts any virtual-key code.
    io.KeyCtrl = unsafe { GetKeyState(i32::from(VK_CONTROL)) } < 0;
    // SAFETY: see above.
    io.KeyShift = unsafe { GetKeyState(i32::from(VK_SHIFT)) } < 0;
    // SAFETY: see above.
    io.KeyAlt = unsafe { GetKeyState(i32::from(VK_MENU)) } < 0;
    io.KeySuper = false;
}
