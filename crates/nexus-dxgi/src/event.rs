use nexus_render::{
    ColorSpace, Extent2D, PresentMethod, RenderStage, SessionGeneration, SurfaceFormat,
};

/// Supported factory interface layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FactoryInterface {
    /// `IDXGIFactory`.
    Base = 0,
    /// `IDXGIFactory1`.
    V1 = 1,
    /// `IDXGIFactory2`.
    V2 = 2,
    /// `IDXGIFactory3`.
    V3 = 3,
    /// `IDXGIFactory4`.
    V4 = 4,
    /// `IDXGIFactory5`.
    V5 = 5,
    /// `IDXGIFactory6`.
    V6 = 6,
    /// `IDXGIFactory7`.
    V7 = 7,
    /// `IDXGIFactoryMedia`, an independent `IUnknown`-derived interface.
    Media = 8,
}

/// Supported inherited swap-chain interface layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum SwapChainInterface {
    /// `IDXGISwapChain`.
    Base = 0,
    /// `IDXGISwapChain1`.
    V1 = 1,
    /// `IDXGISwapChain2`.
    V2 = 2,
    /// `IDXGISwapChain3`.
    V3 = 3,
    /// `IDXGISwapChain4`.
    V4 = 4,
}

/// Native object family involved in an interception event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    /// A concrete DXGI factory interface.
    Factory,
    /// A concrete DXGI swap-chain interface.
    SwapChain,
}

/// Result of an explicit attachment request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachOutcome {
    /// One or more concrete interface pointers were newly intercepted.
    Attached {
        /// Number of per-instance vtables published by this request.
        interfaces: u32,
    },
    /// This manager already owned the concrete interface.
    AlreadyAttached,
}

/// Extern boundary at which a Rust panic was contained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    /// `IUnknown::QueryInterface` on a factory.
    FactoryQueryInterface,
    /// A factory swap-chain creation method.
    FactoryCreateSwapChain,
    /// `IUnknown::QueryInterface` on a swap chain.
    SwapChainQueryInterface,
    /// `Present` or `Present1`.
    Present,
    /// `ResizeBuffers` or `ResizeBuffers1`.
    ResizeBuffers,
    /// `IDXGISwapChain3::SetColorSpace1`.
    SetColorSpace1,
    /// A user-provided diagnostic or observation callback.
    ObserverCallback,
    /// A user-provided overlay-render callback.
    RendererCallback,
}

/// Metadata that could not be authoritatively sampled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationField {
    /// The output window.
    Window,
    /// The D3D/DXGI device identity.
    Device,
    /// The adapter LUID.
    Adapter,
    /// The back-buffer extent or format.
    Surface,
    /// The active color space, which DXGI cannot query directly.
    ColorSpace,
}

/// Closed classification of a forwarded HRESULT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HResultDisposition {
    /// The native call returned success.
    Success,
    /// Presentation reported that the target was occluded.
    Occluded,
    /// The graphics device was removed.
    DeviceRemoved,
    /// The graphics device was reset.
    DeviceReset,
    /// Another native status or failure was returned.
    Other(i32),
}

/// Redaction-safe observation emitted by the interception manager.
///
/// No variant contains a pointer, window title, executable path, adapter name,
/// or free-form native error text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DxgiObservationEvent {
    /// A concrete factory interface was intercepted.
    FactoryAttached {
        /// Concrete layout attached for that interface pointer.
        interface: FactoryInterface,
    },
    /// A concrete swap-chain interface was intercepted and assigned a local ID.
    SwapChainAttached {
        /// Runtime-local monotonically assigned swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Highest layout attached for that concrete pointer.
        interface: SwapChainInterface,
    },
    /// An authoritative metadata field was unavailable.
    MetadataIncomplete {
        /// Runtime-local swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Field that remains explicitly unknown.
        field: ObservationField,
    },
    /// A presentation reached the native implementation and returned.
    PresentForwarded {
        /// Runtime-local swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Presentation entry point used.
        method: PresentMethod,
        /// Global monotonic presentation sequence.
        sequence: u64,
        /// Closed interpretation of the native result.
        result: HResultDisposition,
    },
    /// A resize reached the native implementation and returned.
    ResizeForwarded {
        /// Runtime-local swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Requested extent. Zero retains DXGI's native automatic sizing meaning.
        requested_size: Extent2D,
        /// Requested format translated without losing unknown native values.
        requested_format: SurfaceFormat,
        /// Closed interpretation of the native result.
        result: HResultDisposition,
    },
    /// A color-space mutation reached the native implementation and returned.
    ///
    /// On failure, `active` remains the last successfully applied value (or
    /// explicit unknown if no successful mutation has been observed).
    ColorSpaceForwarded {
        /// Runtime-local swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Requested color space, retaining unknown native numeric values.
        requested: ColorSpace,
        /// Authoritative manager state after the native call.
        active: ColorSpace,
        /// Closed interpretation of the native result.
        result: HResultDisposition,
    },
    /// Policy selected a generation for synchronous overlay rendering.
    RenderSelected {
        /// Runtime-local swap-chain identity.
        swap_chain: nexus_render::SwapChainId,
        /// Current resource generation.
        generation: SessionGeneration,
        /// Maximum stage permitted for this callback.
        stage: RenderStage,
    },
    /// A Rust panic was caught before it could cross the native ABI.
    PanicContained {
        /// Boundary that contained the panic.
        boundary: Boundary,
    },
    /// Hooks were restored and callback admission was closed.
    Shutdown {
        /// Whether every callback admitted before closure drained in time.
        drained: bool,
        /// Number of shadow vtables restored to their original pointer.
        restored: u32,
        /// Number already displaced by another component.
        displaced: u32,
    },
}

/// Result of closing callback admission and restoring every owned vtable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Whether all admitted callbacks drained before the timeout.
    pub drained: bool,
    /// Shadow vtables restored to their original pointer.
    pub restored: u32,
    /// Shadow vtables that another component had already displaced.
    pub displaced: u32,
    /// Callbacks still admitted when the timeout expired.
    pub in_flight: usize,
}
