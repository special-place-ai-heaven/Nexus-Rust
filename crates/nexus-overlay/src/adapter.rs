use core::ffi::c_void;
use core::marker::PhantomData;
use core::ptr::NonNull;
use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nexus_control::{FailureCode, InternalFailure, RenderOperation};
use nexus_dxgi::{OverlayRenderer, PresentFrame, RenderCallbackError, ResizeFrame};
use nexus_imgui_compat::sys;
use nexus_imgui_d3d11::{D3d11Renderer, RendererError};
use nexus_imgui_win32::{PlatformError, Win32Platform};
use nexus_render::{RenderStage, SwapChainId};
use windows::Win32::Graphics::Dxgi::{IDXGISwapChain, IDXGISwapChain1};
use windows::core::Interface;
use windows_sys::Win32::Foundation::HWND;

use crate::affinity::{AffinityStatus, ThreadAffinity};
use crate::message_queue::WindowTarget;
use crate::signal::{NoopShutdownSignal, ShutdownSignal};
use crate::subclass::{SubclassError, WindowSubclass};
use crate::window_router::{NoopWindowMessageRouter, WindowMessageRouter};

static NEXT_ADAPTER_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static THREAD_STATES: RefCell<HashMap<u64, RenderThreadState>> = RefCell::new(HashMap::new());
}

/// Injected UI phase executed while the exact Dear ImGui 1.80 context is current.
pub trait UiFrameBuilder: Send + Sync + 'static {
    /// Executes legacy pre-render work after queued window messages but before
    /// the Win32 platform backend advances IO for the new frame.
    ///
    /// No ImGui frame is active. The context is current only for this call and
    /// must not be retained.
    fn before_frame(
        &self,
        _context: *mut sys::ImGuiContext,
        _stage: RenderStage,
    ) -> Result<(), RenderCallbackError> {
        Ok(())
    }

    /// Performs UI-thread work before Dear ImGui starts and locks a new frame.
    ///
    /// Atlas mutations are valid only in this phase. Returning a rebuild
    /// request causes the D3D11 font texture to be replaced before `NewFrame`.
    fn prepare(
        &self,
        _context: *mut sys::ImGuiContext,
        _stage: RenderStage,
    ) -> Result<UiFramePreparation, RenderCallbackError> {
        Ok(UiFramePreparation::UNCHANGED)
    }

    /// Builds the permitted UI stage for this frame.
    ///
    /// `context` is borrowed for this call. Implementations must not retain,
    /// destroy, or transfer it to another thread.
    fn build(
        &self,
        context: *mut sys::ImGuiContext,
        stage: RenderStage,
    ) -> Result<(), RenderCallbackError>;

    /// Executes stage work after Dear ImGui draw submission and exhaustive
    /// D3D11 state restoration.
    ///
    /// No ImGui frame is active. The exact context is made current only for
    /// compatibility with legacy post-render callbacks and must not be retained.
    fn after_render(
        &self,
        _context: *mut sys::ImGuiContext,
        _stage: RenderStage,
    ) -> Result<(), RenderCallbackError> {
        Ok(())
    }
}

/// Resource work requested by the pre-frame UI phase.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UiFramePreparation {
    rebuild_font_atlas: bool,
}

/// Borrowed native resources for one selected primary render session.
///
/// The value deliberately has no `Debug` implementation so diagnostics cannot
/// accidentally disclose process addresses. COM interfaces must be cloned by
/// an observer that needs to retain them. The ImGui context may only be used on
/// the owning render thread and never outlive the returned attachment lease.
#[derive(Clone, Copy)]
pub struct RenderSessionResources<'a> {
    swap_chain: NonNull<c_void>,
    device: NonNull<c_void>,
    imgui_context: NonNull<sys::ImGuiContext>,
    hwnd: usize,
    swap_chain_id: SwapChainId,
    generation: u64,
    _borrow: PhantomData<&'a mut ()>,
}

impl RenderSessionResources<'_> {
    /// Borrows the selected `IDXGISwapChain` interface.
    #[must_use]
    pub const fn swap_chain(self) -> NonNull<c_void> {
        self.swap_chain
    }

    /// Borrows the selected `ID3D11Device` interface.
    #[must_use]
    pub const fn device(self) -> NonNull<c_void> {
        self.device
    }

    /// Borrows the exact Dear ImGui 1.80 context exposed to add-ons.
    #[must_use]
    pub const fn imgui_context(self) -> NonNull<sys::ImGuiContext> {
        self.imgui_context
    }

    /// Returns the selected same-process output window as an opaque address.
    #[must_use]
    pub const fn hwnd(self) -> usize {
        self.hwnd
    }

    /// Returns the classifier-owned swap-chain identity.
    #[must_use]
    pub const fn swap_chain_id(self) -> SwapChainId {
        self.swap_chain_id
    }

    /// Returns the monotonic swap-chain resource generation.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Render-thread-bound lease created for a selected native render session.
///
/// Dropping the lease must synchronously retire every service that borrows the
/// ImGui context. The overlay drops it before destroying the context or D3D11
/// renderer. Implementations must not panic from `Drop`.
pub trait RenderSessionAttachment: 'static {}

impl<T: 'static> RenderSessionAttachment for T {}

/// Factory for services whose native resources follow the selected swap chain.
pub trait RenderSessionObserver: Send + Sync + 'static {
    /// Attaches services to a fully initialized primary render session.
    ///
    /// The returned lease owns the matching detach operation. An error must
    /// leave no partially published native pointers behind.
    fn attach(
        &self,
        resources: RenderSessionResources<'_>,
    ) -> Result<Box<dyn RenderSessionAttachment>, RenderCallbackError>;
}

/// Observer used when no runtime-native services are enabled.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopRenderSessionObserver;

impl RenderSessionObserver for NoopRenderSessionObserver {
    fn attach(
        &self,
        _resources: RenderSessionResources<'_>,
    ) -> Result<Box<dyn RenderSessionAttachment>, RenderCallbackError> {
        Ok(Box::new(()))
    }
}

impl UiFramePreparation {
    /// No renderer resource needs replacement.
    pub const UNCHANGED: Self = Self {
        rebuild_font_atlas: false,
    };

    /// Requests one font-atlas texture replacement before the frame begins.
    #[must_use]
    pub const fn rebuild_font_atlas() -> Self {
        Self {
            rebuild_font_atlas: true,
        }
    }

    const fn needs_font_rebuild(self) -> bool {
        self.rebuild_font_atlas
    }
}

impl<F> UiFrameBuilder for F
where
    F: Fn(*mut sys::ImGuiContext, RenderStage) -> Result<(), RenderCallbackError>
        + Send
        + Sync
        + 'static,
{
    fn build(
        &self,
        context: *mut sys::ImGuiContext,
        stage: RenderStage,
    ) -> Result<(), RenderCallbackError> {
        self(context, stage)
    }
}

/// Empty frame builder suitable for the isolated render-probe stage.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopUiFrameBuilder;

impl UiFrameBuilder for NoopUiFrameBuilder {
    fn build(
        &self,
        _context: *mut sys::ImGuiContext,
        _stage: RenderStage,
    ) -> Result<(), RenderCallbackError> {
        Ok(())
    }
}

/// Process-facing DXGI overlay callback with strictly thread-local native state.
pub struct OverlayAdapter {
    id: u64,
    builder: Arc<dyn UiFrameBuilder>,
    observer: Arc<dyn RenderSessionObserver>,
    target: Arc<WindowTarget>,
    affinity: ThreadAffinity,
}

impl OverlayAdapter {
    /// Creates an adapter without a runtime shutdown integration.
    #[must_use]
    pub fn new(builder: Arc<dyn UiFrameBuilder>) -> Self {
        Self::with_shutdown_signal(builder, Arc::new(NoopShutdownSignal))
    }

    /// Creates an adapter whose selected `WM_DESTROY` requests runtime shutdown.
    #[must_use]
    pub fn with_shutdown_signal(
        builder: Arc<dyn UiFrameBuilder>,
        shutdown: Arc<dyn ShutdownSignal>,
    ) -> Self {
        Self::with_window_router(builder, shutdown, Arc::new(NoopWindowMessageRouter))
    }

    /// Creates an adapter with explicit legacy WndProc routing stages.
    #[must_use]
    pub fn with_window_router(
        builder: Arc<dyn UiFrameBuilder>,
        shutdown: Arc<dyn ShutdownSignal>,
        router: Arc<dyn WindowMessageRouter>,
    ) -> Self {
        Self::with_render_observer(
            builder,
            shutdown,
            router,
            Arc::new(NoopRenderSessionObserver),
        )
    }

    /// Creates an adapter with explicit window routing and render-session
    /// service attachment.
    #[must_use]
    pub fn with_render_observer(
        builder: Arc<dyn UiFrameBuilder>,
        shutdown: Arc<dyn ShutdownSignal>,
        router: Arc<dyn WindowMessageRouter>,
        observer: Arc<dyn RenderSessionObserver>,
    ) -> Self {
        Self {
            id: next_adapter_id(),
            builder,
            observer,
            target: Arc::new(WindowTarget::with_router(true, shutdown, router)),
            affinity: ThreadAffinity::default(),
        }
    }

    /// Enables or disables explicit input capture without changing render policy.
    pub fn set_visible(&self, visible: bool) {
        self.target.set_visible(visible);
        if !visible {
            self.target.set_capture(false, false);
        }
    }

    /// Returns whether visible-overlay input capture is enabled.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.target.is_visible()
    }

    fn render_owned(&self, frame: &PresentFrame<'_>) -> Result<(), RenderCallbackError> {
        if self.affinity.claim() != AffinityStatus::Owner {
            return Err(invalid_state(RenderOperation::PrepareTarget));
        }

        let generation = frame.generation().get();
        let swap_chain_id = frame.id();
        self.target.select_swap_chain(swap_chain_id.get());

        THREAD_STATES
            .try_with(|states| {
                let mut states = states
                    .try_borrow_mut()
                    .map_err(|_| invalid_state(RenderOperation::PrepareTarget))?;
                let replace = states
                    .get(&self.id)
                    .is_none_or(|state| state.swap_chain_id != swap_chain_id);
                if replace {
                    states.remove(&self.id);
                    self.target.reset();
                    let state = RenderThreadState::attach(
                        frame.swap_chain().as_ptr(),
                        self.id,
                        swap_chain_id,
                        generation,
                        &self.target,
                        &self.observer,
                    )?;
                    states.insert(self.id, state);
                }
                let state = states
                    .get_mut(&self.id)
                    .ok_or_else(|| invalid_state(RenderOperation::PrepareTarget))?;
                state.render(
                    generation,
                    frame.stage(),
                    self.builder.as_ref(),
                    &self.target,
                )
            })
            .map_err(|_| invalid_state(RenderOperation::PrepareTarget))?
    }

    fn mark_resize(
        &self,
        frame: &ResizeFrame<'_>,
        phase: ResizePhase,
    ) -> Result<(), RenderCallbackError> {
        // DXGI invokes resize callbacks for every tracked chain. Comparing the
        // classifier-selected identity first prevents NVIDIA auxiliary chains
        // from claiming ownership or changing the bound HWND.
        if !self.target.is_selected_swap_chain(frame.id().get()) {
            return Ok(());
        }
        match self.affinity.status() {
            AffinityStatus::Unclaimed => return Ok(()),
            AffinityStatus::Foreign => {
                return Err(invalid_state(RenderOperation::PrepareTarget));
            }
            AffinityStatus::Owner => {}
        }
        THREAD_STATES
            .try_with(|states| {
                let mut states = states
                    .try_borrow_mut()
                    .map_err(|_| invalid_state(RenderOperation::PrepareTarget))?;
                if let Some(state) = states.get_mut(&self.id)
                    && state.swap_chain_id == frame.id()
                {
                    state.resize_phase = phase;
                }
                Ok(())
            })
            .map_err(|_| invalid_state(RenderOperation::PrepareTarget))?
    }
}

impl OverlayRenderer for OverlayAdapter {
    fn render(&self, frame: &PresentFrame<'_>) -> Result<(), RenderCallbackError> {
        self.render_owned(frame)
    }

    fn before_resize(&self, frame: &ResizeFrame<'_>) -> Result<(), RenderCallbackError> {
        self.mark_resize(frame, ResizePhase::Begun)
    }

    fn after_resize(&self, frame: &ResizeFrame<'_>) -> Result<(), RenderCallbackError> {
        self.mark_resize(frame, ResizePhase::Completed)
    }
}

impl Drop for OverlayAdapter {
    fn drop(&mut self) {
        self.target.deactivate();
        if self.affinity.status() == AffinityStatus::Owner {
            retire_thread_state(self.id);
        }
        self.target.request_thread_cleanup();
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ResizePhase {
    #[default]
    Idle,
    Begun,
    Completed,
}

struct RenderThreadState {
    swap_chain_id: SwapChainId,
    subclass: Option<WindowSubclass>,
    attachment: Option<Box<dyn RenderSessionAttachment>>,
    renderer: D3d11Renderer,
    platform: Win32Platform,
    resize_phase: ResizePhase,
}

impl RenderThreadState {
    fn attach(
        raw_swap_chain: *mut c_void,
        adapter_id: u64,
        swap_chain_id: SwapChainId,
        generation: u64,
        target: &Arc<WindowTarget>,
        observer: &Arc<dyn RenderSessionObserver>,
    ) -> Result<Self, RenderCallbackError> {
        // SAFETY: `PresentFrame` guarantees that the borrowed pointer is a live
        // IDXGISwapChain for the synchronous callback.
        let hwnd = unsafe { selected_output_window(raw_swap_chain) }.map_err(window_failure)?;
        // SAFETY: the same callback contract holds through this attach call;
        // the renderer acquires its own COM reference before returning.
        let mut renderer =
            unsafe { D3d11Renderer::attach_raw_borrowed(raw_swap_chain, generation) }
                .map_err(renderer_failure)?;
        let platform = renderer
            .with_context_owner(|context| Win32Platform::attach(hwnd, context))
            .map_err(platform_failure)?;
        let subclass =
            WindowSubclass::install(hwnd, adapter_id, target).map_err(subclass_failure)?;
        let imgui_context = NonNull::new(renderer.context_ptr())
            .ok_or_else(|| invalid_state(RenderOperation::PrepareTarget))?;
        let resources = RenderSessionResources {
            swap_chain: renderer.swap_chain_ptr(),
            device: renderer.device_ptr(),
            imgui_context,
            hwnd: hwnd as usize,
            swap_chain_id,
            generation,
            _borrow: PhantomData,
        };
        let attachment = panic::catch_unwind(AssertUnwindSafe(|| observer.attach(resources)))
            .unwrap_or_else(|_| Err(invalid_state(RenderOperation::PrepareTarget)))?;
        target.bind_window(hwnd as usize);
        Ok(Self {
            swap_chain_id,
            subclass: Some(subclass),
            attachment: Some(attachment),
            renderer,
            platform,
            resize_phase: ResizePhase::Idle,
        })
    }

    fn render(
        &mut self,
        generation: u64,
        stage: RenderStage,
        builder: &dyn UiFrameBuilder,
        target: &WindowTarget,
    ) -> Result<(), RenderCallbackError> {
        let current_generation = self.renderer.generation();
        if generation < current_generation {
            target.set_capture(false, false);
            return Err(invalid_state(RenderOperation::PrepareTarget));
        }
        if generation != current_generation {
            self.renderer
                .synchronize_generation(generation)
                .and_then(|()| self.renderer.recreate_back_buffer(generation))
                .map_err(renderer_failure)?;
            self.resize_phase = ResizePhase::Idle;
        } else {
            match self.resize_phase {
                ResizePhase::Completed => {
                    self.renderer
                        .recreate_back_buffer(generation)
                        .map_err(renderer_failure)?;
                    self.resize_phase = ResizePhase::Idle;
                }
                // A begun resize without the success-only after callback failed;
                // no renderer resource was retained or invalidated.
                ResizePhase::Begun => self.resize_phase = ResizePhase::Idle,
                ResizePhase::Idle => {}
            }
        }

        let messages = target.drain();
        let messages_handled = self.renderer.with_context_owner(|context| {
            for message in messages {
                self.platform.handle_message(
                    context,
                    self.platform.hwnd(),
                    message.message,
                    message.wparam,
                    message.lparam,
                )?;
            }
            Ok(())
        });
        messages_handled.map_err(platform_failure)?;

        let before_frame = self.renderer.with_current_context(|context| {
            panic::catch_unwind(AssertUnwindSafe(|| builder.before_frame(context, stage)))
                .unwrap_or_else(|_| Err(invalid_state(render_operation(stage))))
        });
        if let Err(error) = before_frame {
            target.set_capture(false, false);
            return Err(error);
        }

        let platform_frame = self
            .renderer
            .with_context_owner(|context| self.platform.prepare_frame(context));
        let frame_config = platform_frame.map_err(platform_failure)?;

        let preparation = self.renderer.with_current_context(|context| {
            panic::catch_unwind(AssertUnwindSafe(|| builder.prepare(context, stage)))
                .unwrap_or_else(|_| Err(invalid_state(render_operation(stage))))
        });
        let preparation = match preparation {
            Ok(preparation) => preparation,
            Err(error) => {
                target.set_capture(false, false);
                return Err(error);
            }
        };
        if preparation.needs_font_rebuild()
            && let Err(error) = self.renderer.rebuild_font_texture()
        {
            target.set_capture(false, false);
            return Err(renderer_failure(error));
        }

        let rendered = self
            .renderer
            .render_frame(generation, frame_config, |context| {
                let built = panic::catch_unwind(AssertUnwindSafe(|| builder.build(context, stage)))
                    .unwrap_or_else(|_| Err(invalid_state(render_operation(stage))));
                (built, capture_intent())
            });
        let ((built, (capture_mouse, capture_keyboard)), _stats) = match rendered {
            Ok(rendered) => rendered,
            Err(error) => {
                target.set_capture(false, false);
                return Err(renderer_failure(error));
            }
        };
        target.set_capture(capture_mouse, capture_keyboard);
        built?;

        let completed = self.renderer.with_current_context(|context| {
            panic::catch_unwind(AssertUnwindSafe(|| builder.after_render(context, stage)))
                .unwrap_or_else(|_| Err(invalid_state(render_operation(stage))))
        });
        if completed.is_err() {
            target.set_capture(false, false);
        }
        completed
    }
}

impl Drop for RenderThreadState {
    fn drop(&mut self) {
        if let Some(subclass) = self.subclass.take() {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| drop(subclass)));
        }
        if let Some(attachment) = self.attachment.take() {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| drop(attachment)));
        }
    }
}

unsafe fn selected_output_window(raw_swap_chain: *mut c_void) -> Result<HWND, WindowQueryError> {
    // SAFETY: the caller guarantees a live base swap-chain pointer for this call.
    let swap_chain = unsafe { IDXGISwapChain::from_raw_borrowed(&raw_swap_chain) }
        .ok_or(WindowQueryError::Missing)?;

    if let Ok(swap_chain1) = swap_chain.cast::<IDXGISwapChain1>() {
        // SAFETY: the generated wrapper owns a temporary queried reference.
        if let Ok(hwnd) = unsafe { swap_chain1.GetHwnd() }
            && !hwnd.0.is_null()
        {
            return Ok(hwnd.0);
        }
    }

    // SAFETY: the borrowed wrapper refers to the live callback object.
    let description = unsafe { swap_chain.GetDesc() }
        .map_err(|error| WindowQueryError::HResult(error.code().0))?;
    if description.OutputWindow.0.is_null() {
        Err(WindowQueryError::Missing)
    } else {
        Ok(description.OutputWindow.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowQueryError {
    Missing,
    HResult(i32),
}

fn capture_intent() -> (bool, bool) {
    // SAFETY: D3d11Renderer invokes the builder only while its owned context is
    // current; a null result is handled as no capture.
    let io = unsafe { sys::igGetIO() };
    if io.is_null() {
        return (false, false);
    }
    // SAFETY: the context owner remains exclusively borrowed for this closure.
    let io = unsafe { &*io };
    (io.WantCaptureMouse, io.WantCaptureKeyboard)
}

fn next_adapter_id() -> u64 {
    let id = NEXT_ADAPTER_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        NEXT_ADAPTER_ID.store(2, Ordering::Relaxed);
        1
    } else {
        id
    }
}

pub(crate) fn retire_thread_state(adapter_id: u64) {
    let _ = THREAD_STATES.try_with(|states| {
        if let Ok(mut states) = states.try_borrow_mut() {
            states.remove(&adapter_id);
        }
    });
}

fn render_operation(stage: RenderStage) -> RenderOperation {
    match stage {
        RenderStage::ProxyOnly | RenderStage::HooksOnly | RenderStage::RenderProbe => {
            RenderOperation::DrawProbe
        }
        RenderStage::CoreUi => RenderOperation::DrawCoreUi,
        RenderStage::Addons => RenderOperation::DrawAddons,
    }
}

fn invalid_state(operation: RenderOperation) -> RenderCallbackError {
    RenderCallbackError::new(
        operation,
        FailureCode::Internal(InternalFailure::InvalidState),
    )
}

fn window_failure(error: WindowQueryError) -> RenderCallbackError {
    let code = match error {
        WindowQueryError::Missing => FailureCode::Internal(InternalFailure::MissingWindow),
        WindowQueryError::HResult(code) => FailureCode::HResult(code),
    };
    RenderCallbackError::new(RenderOperation::PrepareTarget, code)
}

fn renderer_failure(error: RendererError) -> RenderCallbackError {
    match error {
        RendererError::HResult { code, .. } => {
            RenderCallbackError::new(RenderOperation::PrepareTarget, FailureCode::HResult(code))
        }
        RendererError::StateCapture { .. } => RenderCallbackError::new(
            RenderOperation::CaptureState,
            FailureCode::Internal(InternalFailure::IncompleteStateCapture),
        ),
        RendererError::StateRestore { .. } => RenderCallbackError::new(
            RenderOperation::RestoreState,
            FailureCode::Internal(InternalFailure::IncompleteStateCapture),
        ),
        RendererError::NullPointer(_) => RenderCallbackError::new(
            RenderOperation::PrepareTarget,
            FailureCode::Internal(InternalFailure::MissingDevice),
        ),
        _ => invalid_state(RenderOperation::Composite),
    }
}

fn platform_failure(error: PlatformError) -> RenderCallbackError {
    let code = match error {
        PlatformError::InvalidWindow => FailureCode::Internal(InternalFailure::MissingWindow),
        PlatformError::ClockInitialization { code }
        | PlatformError::ClockSample { code }
        | PlatformError::ClientRect { code } => FailureCode::Win32(code),
        PlatformError::Detached | PlatformError::NonMonotonicClock | PlatformError::Context(_) => {
            FailureCode::Internal(InternalFailure::InvalidState)
        }
    };
    RenderCallbackError::new(RenderOperation::PrepareTarget, code)
}

fn subclass_failure(error: SubclassError) -> RenderCallbackError {
    let code = match error {
        SubclassError::InvalidWindow => FailureCode::Internal(InternalFailure::MissingWindow),
        SubclassError::ForeignProcess => FailureCode::Internal(InternalFailure::InvalidState),
        SubclassError::HookConflict => FailureCode::Internal(InternalFailure::HookConflict),
        SubclassError::Win32(code) => FailureCode::Win32(code),
    };
    RenderCallbackError::new(RenderOperation::PrepareTarget, code)
}

#[cfg(test)]
mod tests {
    use core::ffi::c_void;
    use core::marker::PhantomData;
    use core::ptr::NonNull;
    use std::sync::Arc;

    use nexus_control::{FailureCode, InternalFailure, RenderOperation};
    use nexus_dxgi::RenderCallbackError;
    use nexus_imgui_compat::sys;
    use nexus_render::{RenderStage, SwapChainId};

    use super::{
        NoopRenderSessionObserver, NoopUiFrameBuilder, OverlayAdapter, RenderSessionObserver,
        RenderSessionResources, UiFrameBuilder, UiFramePreparation, next_adapter_id,
        render_operation,
    };

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn process_adapter_is_send_sync_without_native_state() {
        assert_send_sync::<OverlayAdapter>();
    }

    #[test]
    fn render_session_resources_preserve_exact_scoped_identity() {
        let swap_chain = NonNull::<u8>::dangling().cast::<c_void>();
        let device = NonNull::<u16>::dangling().cast::<c_void>();
        let imgui_context = NonNull::dangling();
        let resources = RenderSessionResources {
            swap_chain,
            device,
            imgui_context,
            hwnd: 17,
            swap_chain_id: SwapChainId::new(23),
            generation: 29,
            _borrow: PhantomData,
        };

        assert_eq!(resources.swap_chain(), swap_chain);
        assert_eq!(resources.device(), device);
        assert_eq!(resources.imgui_context(), imgui_context);
        assert_eq!(resources.hwnd(), 17);
        assert_eq!(resources.swap_chain_id(), SwapChainId::new(23));
        assert_eq!(resources.generation(), 29);
        let attachment = NoopRenderSessionObserver
            .attach(resources)
            .expect("the no-op observer should always produce a lease");
        drop(attachment);
    }

    #[test]
    fn ids_are_nonzero_and_monotonic() {
        let first = next_adapter_id();
        let second = next_adapter_id();
        assert_ne!(first, 0);
        assert!(second > first);
    }

    #[test]
    fn no_op_builder_supports_probe_bootstrap() {
        let builder = NoopUiFrameBuilder;
        assert_eq!(
            builder
                .prepare(
                    core::ptr::null_mut::<sys::ImGuiContext>(),
                    RenderStage::RenderProbe
                )
                .expect("no-op preparation should succeed"),
            UiFramePreparation::UNCHANGED
        );
        assert!(
            builder
                .before_frame(
                    core::ptr::null_mut::<sys::ImGuiContext>(),
                    RenderStage::RenderProbe
                )
                .is_ok()
        );
        assert!(
            builder
                .build(
                    core::ptr::null_mut::<sys::ImGuiContext>(),
                    RenderStage::RenderProbe
                )
                .is_ok()
        );
        assert!(
            builder
                .after_render(
                    core::ptr::null_mut::<sys::ImGuiContext>(),
                    RenderStage::RenderProbe
                )
                .is_ok()
        );
        let adapter = OverlayAdapter::new(Arc::new(builder));
        assert!(adapter.is_visible());
        adapter.set_visible(false);
        assert!(!adapter.is_visible());
    }

    #[test]
    fn builder_closures_keep_stage_specific_failure_control() {
        let builder = |_context: *mut sys::ImGuiContext, stage| {
            Err(RenderCallbackError::new(
                render_operation(stage),
                FailureCode::Internal(InternalFailure::InvalidState),
            ))
        };
        let error = builder
            .build(core::ptr::null_mut(), RenderStage::Addons)
            .expect_err("test builder should fail");
        assert_eq!(error.operation(), RenderOperation::DrawAddons);
        assert_eq!(
            builder
                .prepare(core::ptr::null_mut(), RenderStage::Addons)
                .expect("closure preparation should use the default"),
            UiFramePreparation::UNCHANGED
        );
        assert_ne!(
            UiFramePreparation::rebuild_font_atlas(),
            UiFramePreparation::UNCHANGED
        );
        assert!(
            builder
                .before_frame(core::ptr::null_mut(), RenderStage::Addons)
                .is_ok()
        );
        assert!(
            builder
                .after_render(core::ptr::null_mut(), RenderStage::Addons)
                .is_ok()
        );
    }
}
