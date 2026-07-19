use std::{ffi::c_void, marker::PhantomData, ptr::NonNull};

use nexus_control::{DiagnosticEvent, FailureCode, RenderOperation};
use nexus_render::{
    Extent2D, PresentMethod, RenderStage, SessionGeneration, SurfaceFormat, SwapChainId,
};

use crate::DxgiObservationEvent;

/// Receives bounded diagnostics and pointer-free DXGI observations.
pub trait DxgiCallbacks: Send + Sync + 'static {
    /// Receives a closed, redaction-safe failure event.
    fn diagnostic(&self, _event: DiagnosticEvent) {}

    /// Receives a closed, redaction-safe lifecycle or forwarding observation.
    fn observation(&self, _event: DxgiObservationEvent) {}
}

/// Structured failure returned by the synchronous renderer integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderCallbackError {
    operation: RenderOperation,
    code: FailureCode,
}

impl RenderCallbackError {
    /// Creates a bounded renderer failure.
    #[must_use]
    pub const fn new(operation: RenderOperation, code: FailureCode) -> Self {
        Self { operation, code }
    }

    /// Returns the failed render operation.
    #[must_use]
    pub const fn operation(self) -> RenderOperation {
        self.operation
    }

    /// Returns the bounded failure code.
    #[must_use]
    pub const fn code(self) -> FailureCode {
        self.code
    }
}

/// Synchronous presentation context valid only for the duration of one callback.
#[derive(Clone, Copy, Debug)]
pub struct PresentFrame<'callback> {
    swap_chain: NonNull<c_void>,
    id: SwapChainId,
    method: PresentMethod,
    sequence: u64,
    generation: SessionGeneration,
    stage: RenderStage,
    _scope: PhantomData<&'callback mut ()>,
}

impl<'callback> PresentFrame<'callback> {
    pub(crate) const fn new(
        swap_chain: NonNull<c_void>,
        id: SwapChainId,
        method: PresentMethod,
        sequence: u64,
        generation: SessionGeneration,
        stage: RenderStage,
    ) -> Self {
        Self {
            swap_chain,
            id,
            method,
            sequence,
            generation,
            stage,
            _scope: PhantomData,
        }
    }

    /// Returns the native swap-chain pointer for immediate same-thread use.
    ///
    /// The callback must not retain, release, or transfer this borrowed pointer.
    #[must_use]
    pub const fn swap_chain(&self) -> NonNull<c_void> {
        self.swap_chain
    }

    /// Returns the runtime-local swap-chain identity.
    #[must_use]
    pub const fn id(&self) -> SwapChainId {
        self.id
    }

    /// Returns the presentation method being intercepted.
    #[must_use]
    pub const fn method(&self) -> PresentMethod {
        self.method
    }

    /// Returns the global monotonic presentation sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the current render-resource generation.
    #[must_use]
    pub const fn generation(&self) -> SessionGeneration {
        self.generation
    }

    /// Returns the maximum render stage permitted for this callback.
    #[must_use]
    pub const fn stage(&self) -> RenderStage {
        self.stage
    }
}

/// Immutable upper bound for cooperative swap-chain renderer retirement.
///
/// A renderer may retire retained state only when it belongs to this chain,
/// its generation is not newer than [`Self::generation`], and its latest
/// admitted render callback is not newer than [`Self::sequence`]. This makes
/// a delayed retirement callback harmless after the same chain renders again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapChainRetirement {
    id: SwapChainId,
    generation: SessionGeneration,
    sequence: u64,
}

impl SwapChainRetirement {
    /// Creates an exact cooperative retirement cutoff.
    #[must_use]
    pub const fn new(id: SwapChainId, generation: SessionGeneration, sequence: u64) -> Self {
        Self {
            id,
            generation,
            sequence,
        }
    }

    /// Returns the runtime-local swap-chain identity.
    #[must_use]
    pub const fn id(self) -> SwapChainId {
        self.id
    }

    /// Returns the newest resource generation this callback may retire.
    #[must_use]
    pub const fn generation(self) -> SessionGeneration {
        self.generation
    }

    /// Returns the newest admitted render callback this retirement may cover.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Synchronous resize context valid only for the duration of one callback.
#[derive(Clone, Copy, Debug)]
pub struct ResizeFrame<'callback> {
    swap_chain: NonNull<c_void>,
    id: SwapChainId,
    requested_size: Extent2D,
    requested_format: SurfaceFormat,
    _scope: PhantomData<&'callback mut ()>,
}

impl<'callback> ResizeFrame<'callback> {
    pub(crate) const fn new(
        swap_chain: NonNull<c_void>,
        id: SwapChainId,
        requested_size: Extent2D,
        requested_format: SurfaceFormat,
    ) -> Self {
        Self {
            swap_chain,
            id,
            requested_size,
            requested_format,
            _scope: PhantomData,
        }
    }

    /// Returns the native swap-chain pointer for immediate same-thread use.
    #[must_use]
    pub const fn swap_chain(&self) -> NonNull<c_void> {
        self.swap_chain
    }

    /// Returns the runtime-local swap-chain identity.
    #[must_use]
    pub const fn id(&self) -> SwapChainId {
        self.id
    }

    /// Returns the requested extent.
    #[must_use]
    pub const fn requested_size(&self) -> Extent2D {
        self.requested_size
    }

    /// Returns the requested surface format.
    #[must_use]
    pub const fn requested_format(&self) -> SurfaceFormat {
        self.requested_format
    }
}

/// Synchronous integration point for the state-preserving D3D11 renderer.
///
/// Implementations must capture and restore every graphics state they modify.
/// The manager contains panics and failures, but cannot prove native D3D11
/// state restoration itself. Resize callbacks should release back-buffer-owned
/// resources before the original resize and recreate them only after success.
pub trait OverlayRenderer: Send + Sync + 'static {
    /// Draws the overlay immediately before the native presentation.
    fn render(&self, frame: &PresentFrame<'_>) -> Result<(), RenderCallbackError>;

    /// Retires native state within one exact swap-chain cutoff.
    ///
    /// Thread-bound implementations must not destroy another thread's state
    /// from this callback. Implementations must also ignore state newer than
    /// the supplied cutoff. A no-op default preserves renderers that do not
    /// keep per-chain native resources.
    fn retire_swap_chain(
        &self,
        _retirement: SwapChainRetirement,
    ) -> Result<(), RenderCallbackError> {
        Ok(())
    }

    /// Releases resources that would prevent the native resize from succeeding.
    fn before_resize(&self, _frame: &ResizeFrame<'_>) -> Result<(), RenderCallbackError> {
        Ok(())
    }

    /// Recreates resources after a successful native resize.
    fn after_resize(&self, _frame: &ResizeFrame<'_>) -> Result<(), RenderCallbackError> {
        Ok(())
    }
}
