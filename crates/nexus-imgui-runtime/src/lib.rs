//! Rust ownership and frame lifecycle for Nexus' Dear ImGui 1.80 context.
//!
//! Native add-ons require a real `ImGuiContext*`, so the context itself lives
//! in the narrow compatibility library. This crate owns that pointer, keeps it
//! thread-bound, restores any previously-current context after scoped work,
//! and guarantees that an unwinding frame is ended before control escapes.

use core::ffi::CStr;
use core::marker::PhantomData;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, Ordering};
use std::rc::Rc;
use std::time::Duration;

use nexus_imgui_compat::{has_expected_version, sys};
use thiserror::Error;

const PLATFORM_BACKEND_NAME: &CStr = c"nexus-rust-win32";
const RENDERER_BACKEND_NAME: &CStr = c"nexus-rust-d3d11";
const FLAG_HAS_GAMEPAD: i32 = sys::ImGuiBackendFlags_HasGamepad as i32;
const FLAG_HAS_MOUSE_CURSORS: i32 = sys::ImGuiBackendFlags_HasMouseCursors as i32;
const FLAG_HAS_SET_MOUSE_POS: i32 = sys::ImGuiBackendFlags_HasSetMousePos as i32;
const FLAG_RENDERER_HAS_VTX_OFFSET: i32 = sys::ImGuiBackendFlags_RendererHasVtxOffset as i32;
const MANAGED_PLATFORM_FLAGS: i32 =
    FLAG_HAS_GAMEPAD | FLAG_HAS_MOUSE_CURSORS | FLAG_HAS_SET_MOUSE_POS;

static CONTEXT_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Closed failure categories for context creation and access.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ContextError {
    /// The linked compatibility library is not Dear ImGui 1.80.
    #[error("the linked Dear ImGui library is not ABI version 1.80")]
    IncompatibleVersion,
    /// This process already has a Rust-owned Nexus context.
    #[error("a Rust-owned Nexus Dear ImGui context already exists")]
    AlreadyOwned,
    /// Dear ImGui returned a null context pointer.
    #[error("Dear ImGui failed to create a context")]
    CreationFailed,
    /// Dear ImGui returned a null IO pointer for a live context.
    #[error("Dear ImGui IO is unavailable")]
    IoUnavailable,
}

/// Capabilities supplied by the Win32 platform backend.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlatformCapabilities {
    /// The platform backend is installed.
    pub installed: bool,
    /// Gamepad navigation is currently supported.
    pub has_gamepad: bool,
    /// Native mouse cursor shapes are supported.
    pub has_mouse_cursors: bool,
    /// Dear ImGui may request native cursor repositioning.
    pub has_set_mouse_pos: bool,
}

/// Validated inputs for one Dear ImGui frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameConfig {
    /// Logical display width and height.
    pub display_size: [f32; 2],
    /// Framebuffer pixels per logical display unit.
    pub framebuffer_scale: [f32; 2],
    /// Elapsed time since the previous frame.
    pub delta_time: Duration,
}

/// Closed failure categories for beginning or rendering a frame.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FrameError {
    /// A display dimension was negative or non-finite.
    #[error("the Dear ImGui display size is invalid")]
    InvalidDisplaySize,
    /// A framebuffer scale was non-positive or non-finite.
    #[error("the Dear ImGui framebuffer scale is invalid")]
    InvalidFramebufferScale,
    /// Delta time was zero or could not be represented as a positive finite value.
    #[error("the Dear ImGui frame delta is invalid")]
    InvalidDeltaTime,
    /// Dear ImGui returned a null IO pointer for a live context.
    #[error("Dear ImGui IO is unavailable")]
    IoUnavailable,
    /// `render` was called more than once for the frame.
    #[error("the Dear ImGui frame has already been rendered")]
    AlreadyRendered,
    /// Dear ImGui did not publish draw data after rendering.
    #[error("Dear ImGui draw data is unavailable")]
    DrawDataUnavailable,
}

/// Unique, thread-bound owner of Nexus' native-compatible Dear ImGui context.
///
/// The `Rc` marker deliberately makes this type neither `Send` nor `Sync`.
/// Context creation, frame calls, and destruction therefore remain on one
/// render thread even though the raw C API uses process-global state.
pub struct ImGuiContextOwner {
    context: NonNull<sys::ImGuiContext>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ImGuiContextOwner {
    /// Creates the process' single Rust-owned Dear ImGui context.
    pub fn create() -> Result<Self, ContextError> {
        if !has_expected_version() {
            return Err(ContextError::IncompatibleVersion);
        }
        if CONTEXT_CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ContextError::AlreadyOwned);
        }

        // SAFETY: querying and creating a context are the documented Dear ImGui
        // C API. A null shared atlas requests an owned atlas.
        let previous = unsafe { sys::igGetCurrentContext() };
        // SAFETY: see above.
        let raw = unsafe { sys::igCreateContext(ptr::null_mut()) };
        let Some(context) = NonNull::new(raw) else {
            // SAFETY: restoring a pointer obtained from `igGetCurrentContext`
            // is valid even when it is null.
            unsafe { sys::igSetCurrentContext(previous) };
            CONTEXT_CLAIMED.store(false, Ordering::Release);
            return Err(ContextError::CreationFailed);
        };
        // `igCreateContext` makes the new context current; creation itself must
        // not leak that global state into its caller.
        if previous != raw {
            // SAFETY: `previous` came from Dear ImGui and may be null.
            unsafe { sys::igSetCurrentContext(previous) };
        }

        let mut owner = Self {
            context,
            _thread_bound: PhantomData,
        };
        let initialized = owner.with_io(|io| {
            // Nexus owns persistence explicitly; the compatibility context must
            // not read or write process-working-directory files implicitly.
            io.IniFilename = ptr::null();
            io.LogFilename = ptr::null();
        });
        if initialized.is_err() {
            drop(owner);
            return Err(ContextError::IoUnavailable);
        }
        Ok(owner)
    }

    /// Returns the exact pointer passed to native add-ons.
    ///
    /// The pointer is valid only while this owner remains alive. Add-ons must
    /// not destroy it or retain it beyond their unload callback.
    #[must_use]
    pub fn as_ptr(&self) -> *mut sys::ImGuiContext {
        self.context.as_ptr()
    }

    /// Runs an operation while this context is current, restoring the previous
    /// context even if the operation unwinds.
    pub fn with_current<R>(&mut self, operation: impl FnOnce() -> R) -> R {
        let _current = CurrentContext::enter(self);
        operation()
    }

    /// Publishes only the platform capabilities that are actually installed.
    pub fn set_platform_capabilities(
        &mut self,
        capabilities: PlatformCapabilities,
    ) -> Result<(), ContextError> {
        self.with_io(|io| {
            io.BackendFlags &= !MANAGED_PLATFORM_FLAGS;
            io.BackendPlatformName = if capabilities.installed {
                PLATFORM_BACKEND_NAME.as_ptr()
            } else {
                ptr::null()
            };
            if capabilities.installed && capabilities.has_gamepad {
                io.BackendFlags |= FLAG_HAS_GAMEPAD;
            }
            if capabilities.installed && capabilities.has_mouse_cursors {
                io.BackendFlags |= FLAG_HAS_MOUSE_CURSORS;
            }
            if capabilities.installed && capabilities.has_set_mouse_pos {
                io.BackendFlags |= FLAG_HAS_SET_MOUSE_POS;
            }
        })
    }

    /// Marks the Rust D3D11 renderer installed or absent.
    ///
    /// The installed renderer always honors `ImDrawCmd::VtxOffset`, so that
    /// capability is published atomically with its backend name.
    pub fn set_renderer_ready(&mut self, ready: bool) -> Result<(), ContextError> {
        self.with_io(|io| {
            io.BackendFlags &= !FLAG_RENDERER_HAS_VTX_OFFSET;
            io.BackendRendererName = if ready {
                io.BackendFlags |= FLAG_RENDERER_HAS_VTX_OFFSET;
                RENDERER_BACKEND_NAME.as_ptr()
            } else {
                ptr::null()
            };
        })
    }

    /// Begins one frame and keeps this context current until the returned guard
    /// is dropped.
    pub fn begin_frame(&mut self, config: FrameConfig) -> Result<Frame<'_>, FrameError> {
        validate_frame_config(config)?;
        let current = CurrentContext::enter(self);
        // SAFETY: the guard made a live context current.
        let Some(mut io) = NonNull::new(unsafe { sys::igGetIO() }) else {
            return Err(FrameError::IoUnavailable);
        };
        // SAFETY: `io` belongs to the current live context and remains valid for
        // the context lifetime, which the guard borrows exclusively.
        let io = unsafe { io.as_mut() };
        io.DisplaySize = sys::ImVec2 {
            x: config.display_size[0],
            y: config.display_size[1],
        };
        io.DisplayFramebufferScale = sys::ImVec2 {
            x: config.framebuffer_scale[0],
            y: config.framebuffer_scale[1],
        };
        io.DeltaTime = config.delta_time.as_secs_f32();
        // SAFETY: configuration was validated and this live context is current.
        unsafe { sys::igNewFrame() };
        Ok(Frame {
            _current: current,
            phase: FramePhase::Active,
        })
    }

    fn with_io<R>(
        &mut self,
        operation: impl FnOnce(&mut sys::ImGuiIO) -> R,
    ) -> Result<R, ContextError> {
        self.with_current(|| {
            // SAFETY: `with_current` made the owned live context current.
            let Some(mut io) = NonNull::new(unsafe { sys::igGetIO() }) else {
                return Err(ContextError::IoUnavailable);
            };
            // SAFETY: the exclusive owner borrow prevents another safe caller
            // from accessing this IO object during the operation.
            Ok(operation(unsafe { io.as_mut() }))
        })
    }
}

impl Drop for ImGuiContextOwner {
    fn drop(&mut self) {
        // SAFETY: this owner holds the unique live context pointer.
        let previous = unsafe { sys::igGetCurrentContext() };
        // SAFETY: make the owned context current for orderly shutdown.
        unsafe { sys::igSetCurrentContext(self.context.as_ptr()) };
        // SAFETY: the context is uniquely owned and destroyed exactly once.
        unsafe { sys::igDestroyContext(self.context.as_ptr()) };
        if previous != self.context.as_ptr() {
            // SAFETY: `previous` was supplied by Dear ImGui and may be null.
            unsafe { sys::igSetCurrentContext(previous) };
        }
        CONTEXT_CLAIMED.store(false, Ordering::Release);
    }
}

struct CurrentContext<'owner> {
    previous: *mut sys::ImGuiContext,
    _owner: PhantomData<&'owner mut ImGuiContextOwner>,
}

impl<'owner> CurrentContext<'owner> {
    fn enter(owner: &'owner mut ImGuiContextOwner) -> Self {
        // SAFETY: both operations use a live context controlled by the exclusive
        // owner borrow. The prior pointer is saved for scope restoration.
        let previous = unsafe { sys::igGetCurrentContext() };
        if previous != owner.context.as_ptr() {
            // SAFETY: the owner's context is live for this guard's lifetime.
            unsafe { sys::igSetCurrentContext(owner.context.as_ptr()) };
        }
        Self {
            previous,
            _owner: PhantomData,
        }
    }
}

impl Drop for CurrentContext<'_> {
    fn drop(&mut self) {
        // SAFETY: the saved pointer was current immediately before entering this
        // scope and may validly be null.
        unsafe { sys::igSetCurrentContext(self.previous) };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FramePhase {
    Active,
    Rendered,
}

/// Active Dear ImGui frame with unwind-safe completion.
pub struct Frame<'owner> {
    _current: CurrentContext<'owner>,
    phase: FramePhase,
}

impl Frame<'_> {
    /// Finalizes the frame and borrows its draw data until this frame guard is
    /// dropped.
    pub fn render(&mut self) -> Result<DrawData<'_>, FrameError> {
        if self.phase == FramePhase::Rendered {
            return Err(FrameError::AlreadyRendered);
        }
        // SAFETY: this frame's guard keeps the originating context current.
        unsafe { sys::igRender() };
        self.phase = FramePhase::Rendered;
        // SAFETY: the same live context remains current after `igRender`.
        let draw_data = unsafe { sys::igGetDrawData() };
        let Some(raw) = NonNull::new(draw_data) else {
            return Err(FrameError::DrawDataUnavailable);
        };
        Ok(DrawData {
            raw,
            _frame: PhantomData,
        })
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        if self.phase == FramePhase::Active {
            // SAFETY: the current-context guard is still alive and `EndFrame`
            // is the documented cancellation path after `NewFrame`.
            unsafe { sys::igEndFrame() };
        }
    }
}

/// Draw data tied to the rendered frame and originating context.
pub struct DrawData<'frame> {
    raw: NonNull<sys::ImDrawData>,
    _frame: PhantomData<&'frame mut Frame<'frame>>,
}

impl DrawData<'_> {
    /// Returns the native draw-data pointer consumed synchronously by the D3D11
    /// renderer.
    #[must_use]
    pub fn as_ptr(&self) -> *mut sys::ImDrawData {
        self.raw.as_ptr()
    }
}

fn validate_frame_config(config: FrameConfig) -> Result<(), FrameError> {
    if config
        .display_size
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(FrameError::InvalidDisplaySize);
    }
    if config
        .framebuffer_scale
        .iter()
        .any(|value| !value.is_finite() || *value <= 0.0)
    {
        return Err(FrameError::InvalidFramebufferScale);
    }
    let delta = config.delta_time.as_secs_f32();
    if !delta.is_finite() || delta <= 0.0 {
        return Err(FrameError::InvalidDeltaTime);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use core::ptr;
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    use nexus_imgui_compat::sys;

    use super::{
        ContextError, FLAG_HAS_MOUSE_CURSORS, FLAG_RENDERER_HAS_VTX_OFFSET, FrameConfig,
        FrameError, ImGuiContextOwner, PlatformCapabilities,
    };

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_lock() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn frame_config() -> FrameConfig {
        FrameConfig {
            display_size: [1920.0, 1080.0],
            framebuffer_scale: [1.0, 1.0],
            delta_time: Duration::from_secs_f32(1.0 / 60.0),
        }
    }

    fn build_font_atlas(owner: &mut ImGuiContextOwner) {
        owner.with_current(|| {
            // SAFETY: the owner made its live context current for this closure.
            let io = unsafe { sys::igGetIO() };
            assert!(!io.is_null());
            let mut pixels = ptr::null_mut();
            let mut width = 0;
            let mut height = 0;
            let mut bytes_per_pixel = 0;
            // SAFETY: all output pointers are valid and `Fonts` belongs to the
            // current context.
            unsafe {
                sys::ImFontAtlas_GetTexDataAsRGBA32(
                    (*io).Fonts,
                    &mut pixels,
                    &mut width,
                    &mut height,
                    &mut bytes_per_pixel,
                );
            }
            assert!(!pixels.is_null());
            assert!(width > 0);
            assert!(height > 0);
            assert_eq!(bytes_per_pixel, 4);
        });
    }

    #[test]
    fn creation_is_unique_and_does_not_leak_current_context() {
        let _lock = test_lock();
        // SAFETY: tests are serialized around Dear ImGui's process-global state.
        unsafe { sys::igSetCurrentContext(ptr::null_mut()) };
        let owner = ImGuiContextOwner::create();
        assert!(owner.is_ok());
        let owner = owner.unwrap_or_else(|error| panic!("context failed: {error}"));
        // SAFETY: querying current context has no preconditions.
        assert!(unsafe { sys::igGetCurrentContext() }.is_null());
        assert_eq!(
            ImGuiContextOwner::create().err(),
            Some(ContextError::AlreadyOwned)
        );
        drop(owner);
        // SAFETY: querying current context has no preconditions.
        assert!(unsafe { sys::igGetCurrentContext() }.is_null());
    }

    #[test]
    fn scoped_current_context_restores_an_external_context() {
        let _lock = test_lock();
        // SAFETY: tests are serialized and a null atlas requests owned storage.
        let external = unsafe { sys::igCreateContext(ptr::null_mut()) };
        assert!(!external.is_null());
        let mut owner =
            ImGuiContextOwner::create().unwrap_or_else(|error| panic!("context failed: {error}"));
        // SAFETY: querying current context has no preconditions.
        assert_eq!(unsafe { sys::igGetCurrentContext() }, external);
        let owned = owner.as_ptr();
        owner.with_current(|| {
            // SAFETY: querying current context has no preconditions.
            assert_eq!(unsafe { sys::igGetCurrentContext() }, owned);
        });
        // SAFETY: querying current context has no preconditions.
        assert_eq!(unsafe { sys::igGetCurrentContext() }, external);
        drop(owner);
        // SAFETY: the external context is live, current, and uniquely test-owned.
        unsafe { sys::igDestroyContext(external) };
    }

    #[test]
    fn backend_flags_never_claim_uninstalled_capabilities() {
        let _lock = test_lock();
        let mut owner =
            ImGuiContextOwner::create().unwrap_or_else(|error| panic!("context failed: {error}"));
        assert!(
            owner
                .set_platform_capabilities(PlatformCapabilities {
                    installed: true,
                    has_mouse_cursors: true,
                    ..PlatformCapabilities::default()
                })
                .is_ok()
        );
        assert!(owner.set_renderer_ready(true).is_ok());
        owner.with_current(|| {
            // SAFETY: this context is current and its IO pointer is live.
            let io = unsafe { &*sys::igGetIO() };
            assert_ne!(io.BackendFlags & FLAG_HAS_MOUSE_CURSORS, 0);
            assert_ne!(io.BackendFlags & FLAG_RENDERER_HAS_VTX_OFFSET, 0);
            assert!(!io.BackendPlatformName.is_null());
            assert!(!io.BackendRendererName.is_null());
        });
        assert!(owner.set_renderer_ready(false).is_ok());
        owner.with_current(|| {
            // SAFETY: this context is current and its IO pointer is live.
            let io = unsafe { &*sys::igGetIO() };
            assert_eq!(io.BackendFlags & FLAG_RENDERER_HAS_VTX_OFFSET, 0);
            assert!(io.BackendRendererName.is_null());
        });
    }

    #[test]
    fn frame_render_is_single_use_and_draw_data_is_borrowed() {
        let _lock = test_lock();
        let mut owner =
            ImGuiContextOwner::create().unwrap_or_else(|error| panic!("context failed: {error}"));
        build_font_atlas(&mut owner);
        let mut frame = owner
            .begin_frame(frame_config())
            .unwrap_or_else(|error| panic!("frame failed: {error}"));
        {
            let draw_data = frame
                .render()
                .unwrap_or_else(|error| panic!("render failed: {error}"));
            assert!(!draw_data.as_ptr().is_null());
        }
        assert_eq!(frame.render().err(), Some(FrameError::AlreadyRendered));
    }

    #[test]
    fn dropping_an_unrendered_frame_allows_the_next_frame() {
        let _lock = test_lock();
        let mut owner =
            ImGuiContextOwner::create().unwrap_or_else(|error| panic!("context failed: {error}"));
        build_font_atlas(&mut owner);
        let frame = owner
            .begin_frame(frame_config())
            .unwrap_or_else(|error| panic!("frame failed: {error}"));
        drop(frame);
        let mut next = owner
            .begin_frame(frame_config())
            .unwrap_or_else(|error| panic!("next frame failed: {error}"));
        assert!(next.render().is_ok());
    }

    #[test]
    fn invalid_frame_inputs_are_rejected_before_new_frame() {
        let _lock = test_lock();
        let mut owner =
            ImGuiContextOwner::create().unwrap_or_else(|error| panic!("context failed: {error}"));
        let mut config = frame_config();
        config.display_size[0] = f32::NAN;
        assert!(matches!(
            owner.begin_frame(config),
            Err(FrameError::InvalidDisplaySize)
        ));
        config = frame_config();
        config.framebuffer_scale[1] = 0.0;
        assert!(matches!(
            owner.begin_frame(config),
            Err(FrameError::InvalidFramebufferScale)
        ));
        config = frame_config();
        config.delta_time = Duration::ZERO;
        assert!(matches!(
            owner.begin_frame(config),
            Err(FrameError::InvalidDeltaTime)
        ));
    }
}
