use std::fmt;

/// A runtime-local adapter identifier.
///
/// This is intentionally not a hardware LUID, device name, or filesystem
/// value. Allocate it monotonically inside the process.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AdapterId(u64);

impl AdapterId {
    /// Creates an identifier from a process-local sequence number.
    #[must_use]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Returns the process-local sequence number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A runtime-local swap-chain identifier.
///
/// This identifier never contains a pointer, window title, or process path.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SwapChainId(u64);

impl SwapChainId {
    /// Creates an identifier from a process-local sequence number.
    #[must_use]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Returns the process-local sequence number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// System DLL selected by a proxy operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyModule {
    /// Direct3D 9.
    D3D9,
    /// Direct3D 11.
    D3D11,
    /// DirectX Graphics Infrastructure.
    Dxgi,
}

impl fmt::Display for ProxyModule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::D3D9 => "d3d9",
            Self::D3D11 => "d3d11",
            Self::Dxgi => "dxgi",
        })
    }
}

/// Closed set of exported functions supported by the compatibility proxy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyExport {
    /// `Direct3DCreate9`.
    Direct3DCreate9,
    /// `Direct3DCreate9Ex`.
    Direct3DCreate9Ex,
    /// `D3DPERF_BeginEvent`.
    D3dperfBeginEvent,
    /// `D3DPERF_EndEvent`.
    D3dperfEndEvent,
    /// `D3DPERF_SetMarker`.
    D3dperfSetMarker,
    /// `D3DPERF_SetRegion`.
    D3dperfSetRegion,
    /// `D3DPERF_QueryRepeatFrame`.
    D3dperfQueryRepeatFrame,
    /// `D3DPERF_SetOptions`.
    D3dperfSetOptions,
    /// `D3DPERF_GetStatus`.
    D3dperfGetStatus,
    /// `D3D11CreateDevice`.
    D3D11CreateDevice,
    /// `D3D11CreateDeviceAndSwapChain`.
    D3D11CreateDeviceAndSwapChain,
    /// `D3D11CoreCreateDevice`.
    D3D11CoreCreateDevice,
    /// `D3D11CoreRegisterLayers`.
    D3D11CoreRegisterLayers,
    /// `CreateDXGIFactory`.
    CreateDxgiFactory,
    /// `CreateDXGIFactory1`.
    CreateDxgiFactory1,
    /// `CreateDXGIFactory2`.
    CreateDxgiFactory2,
    /// `DXGIGetDebugInterface1`.
    DxgiGetDebugInterface1,
    /// `DXGIDeclareAdapterRemovalSupport`.
    DxgiDeclareAdapterRemovalSupport,
}

impl fmt::Display for ProxyExport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Direct3DCreate9 => "Direct3DCreate9",
            Self::Direct3DCreate9Ex => "Direct3DCreate9Ex",
            Self::D3dperfBeginEvent => "D3DPERF_BeginEvent",
            Self::D3dperfEndEvent => "D3DPERF_EndEvent",
            Self::D3dperfSetMarker => "D3DPERF_SetMarker",
            Self::D3dperfSetRegion => "D3DPERF_SetRegion",
            Self::D3dperfQueryRepeatFrame => "D3DPERF_QueryRepeatFrame",
            Self::D3dperfSetOptions => "D3DPERF_SetOptions",
            Self::D3dperfGetStatus => "D3DPERF_GetStatus",
            Self::D3D11CreateDevice => "D3D11CreateDevice",
            Self::D3D11CreateDeviceAndSwapChain => "D3D11CreateDeviceAndSwapChain",
            Self::D3D11CoreCreateDevice => "D3D11CoreCreateDevice",
            Self::D3D11CoreRegisterLayers => "D3D11CoreRegisterLayers",
            Self::CreateDxgiFactory => "CreateDXGIFactory",
            Self::CreateDxgiFactory1 => "CreateDXGIFactory1",
            Self::CreateDxgiFactory2 => "CreateDXGIFactory2",
            Self::DxgiGetDebugInterface1 => "DXGIGetDebugInterface1",
            Self::DxgiDeclareAdapterRemovalSupport => "DXGIDeclareAdapterRemovalSupport",
        })
    }
}

/// Proxy operation that failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyOperation {
    /// Loading a fully qualified System32 module.
    LoadSystemModule(ProxyModule),
    /// Loading an optional sibling chainload module.
    LoadChainloadModule(ProxyModule),
    /// Resolving an export from a loaded module.
    ResolveExport(ProxyExport),
    /// Forwarding a call to an export.
    InvokeExport(ProxyExport),
}

impl fmt::Display for ProxyOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadSystemModule(module) => write!(formatter, "load-system-module({module})"),
            Self::LoadChainloadModule(module) => {
                write!(formatter, "load-chainload-module({module})")
            }
            Self::ResolveExport(export) => write!(formatter, "resolve-export({export})"),
            Self::InvokeExport(export) => write!(formatter, "invoke-export({export})"),
        }
    }
}

/// Adapter operation that failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterOperation {
    /// Enumerating adapters.
    Enumerate,
    /// Inspecting adapter capabilities.
    Inspect,
    /// Selecting the adapter associated with a swap chain.
    Select,
    /// Creating the D3D11 device.
    CreateDevice,
}

impl fmt::Display for AdapterOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Enumerate => "enumerate",
            Self::Inspect => "inspect",
            Self::Select => "select",
            Self::CreateDevice => "create-device",
        })
    }
}

/// Swap-chain operation that failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwapChainOperation {
    /// Inspecting swap-chain metadata.
    Inspect,
    /// Classifying whether a swap chain belongs to the game.
    Classify,
    /// Attaching a typed per-object hook.
    AttachHook,
    /// Creating or replacing the render target.
    CreateRenderTarget,
    /// Handling buffer resize.
    ResizeBuffers,
    /// Forwarding or handling presentation.
    Present,
}

impl fmt::Display for SwapChainOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Inspect => "inspect",
            Self::Classify => "classify",
            Self::AttachHook => "attach-hook",
            Self::CreateRenderTarget => "create-render-target",
            Self::ResizeBuffers => "resize-buffers",
            Self::Present => "present",
        })
    }
}

/// Render operation that failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderOperation {
    /// Capturing the caller's complete D3D11 state.
    CaptureState,
    /// Preparing the Nexus-owned target and compositor state.
    PrepareTarget,
    /// Drawing the non-interactive render probe.
    DrawProbe,
    /// Drawing Nexus-owned UI.
    DrawCoreUi,
    /// Invoking addon rendering.
    DrawAddons,
    /// Compositing the overlay target.
    Composite,
    /// Restoring the caller's complete D3D11 state.
    RestoreState,
}

impl fmt::Display for RenderOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CaptureState => "capture-state",
            Self::PrepareTarget => "prepare-target",
            Self::DrawProbe => "draw-probe",
            Self::DrawCoreUi => "draw-core-ui",
            Self::DrawAddons => "draw-addons",
            Self::Composite => "composite",
            Self::RestoreState => "restore-state",
        })
    }
}

/// Internal failure category with no free-form message payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InternalFailure {
    /// Initialization recursively re-entered the same boundary.
    ReentrantInitialization,
    /// The object did not implement the required typed interface.
    UnsupportedInterface,
    /// A required D3D11 device was unavailable.
    MissingDevice,
    /// A required output window was unavailable.
    MissingWindow,
    /// Runtime state violated a lifecycle invariant.
    InvalidState,
    /// Another component already owns the requested hook.
    HookConflict,
    /// Graphics state could not be captured completely.
    IncompleteStateCapture,
    /// The D3D11 device was removed or reset.
    DeviceLost,
}

impl fmt::Display for InternalFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReentrantInitialization => "reentrant-initialization",
            Self::UnsupportedInterface => "unsupported-interface",
            Self::MissingDevice => "missing-device",
            Self::MissingWindow => "missing-window",
            Self::InvalidState => "invalid-state",
            Self::HookConflict => "hook-conflict",
            Self::IncompleteStateCapture => "incomplete-state-capture",
            Self::DeviceLost => "device-lost",
        })
    }
}

/// Bounded failure code that cannot retain OS error text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureCode {
    /// Numeric Win32 `GetLastError` value.
    Win32(u32),
    /// Numeric COM/D3D HRESULT value.
    HResult(i32),
    /// Nexus-owned invariant or lifecycle category.
    Internal(InternalFailure),
}

impl fmt::Display for FailureCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Win32(code) => write!(formatter, "win32(0x{code:08X})"),
            Self::HResult(code) => {
                let bits = u32::from_ne_bytes(code.to_ne_bytes());
                write!(formatter, "hresult(0x{bits:08X})")
            }
            Self::Internal(failure) => write!(formatter, "internal({failure})"),
        }
    }
}

/// Redaction-safe failure event suitable for logs and support bundles.
///
/// The type intentionally has no string or path fields. Adapter and swap-chain
/// identifiers must be runtime-local sequence numbers created with
/// [`AdapterId::new`] and [`SwapChainId::new`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticEvent {
    /// Proxy module or export forwarding failed.
    ProxyFailure {
        /// Operation that failed.
        operation: ProxyOperation,
        /// Bounded failure code.
        code: FailureCode,
    },
    /// Adapter discovery or device creation failed.
    AdapterFailure {
        /// Runtime-local adapter identifier, when allocation reached that point.
        adapter: Option<AdapterId>,
        /// Operation that failed.
        operation: AdapterOperation,
        /// Bounded failure code.
        code: FailureCode,
    },
    /// Swap-chain discovery, hook, resize, or present handling failed.
    SwapChainFailure {
        /// Runtime-local swap-chain identifier, when allocation reached that point.
        swap_chain: Option<SwapChainId>,
        /// Operation that failed.
        operation: SwapChainOperation,
        /// Bounded failure code.
        code: FailureCode,
    },
    /// Overlay rendering failed while presentation remained forwardable.
    RenderFailure {
        /// Runtime-local swap-chain identifier.
        swap_chain: SwapChainId,
        /// Render operation that failed.
        operation: RenderOperation,
        /// Bounded failure code.
        code: FailureCode,
    },
}

impl fmt::Display for DiagnosticEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProxyFailure { operation, code } => {
                write!(formatter, "proxy failure during {operation}: {code}")
            }
            Self::AdapterFailure {
                adapter,
                operation,
                code,
            } => write!(
                formatter,
                "adapter failure for {} during {operation}: {code}",
                OptionalId(adapter.map(AdapterId::get))
            ),
            Self::SwapChainFailure {
                swap_chain,
                operation,
                code,
            } => write!(
                formatter,
                "swap-chain failure for {} during {operation}: {code}",
                OptionalId(swap_chain.map(SwapChainId::get))
            ),
            Self::RenderFailure {
                swap_chain,
                operation,
                code,
            } => write!(
                formatter,
                "render failure for local-id({}) during {operation}: {code}",
                swap_chain.get()
            ),
        }
    }
}

struct OptionalId(Option<u64>);

impl fmt::Display for OptionalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(id) => write!(formatter, "local-id({id})"),
            None => formatter.write_str("unassigned"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_event_formats_only_closed_enum_values_and_numeric_code() {
        let event = DiagnosticEvent::ProxyFailure {
            operation: ProxyOperation::ResolveExport(ProxyExport::D3D11CreateDevice),
            code: FailureCode::Win32(126),
        };

        assert_eq!(
            event.to_string(),
            "proxy failure during resolve-export(D3D11CreateDevice): win32(0x0000007E)"
        );
    }

    #[test]
    fn hresult_is_formatted_as_stable_unsigned_bits() {
        let event = DiagnosticEvent::RenderFailure {
            swap_chain: SwapChainId::new(7),
            operation: RenderOperation::RestoreState,
            code: FailureCode::HResult(i32::from_ne_bytes(0x887A_0005_u32.to_ne_bytes())),
        };

        assert_eq!(
            event.to_string(),
            "render failure for local-id(7) during restore-state: hresult(0x887A0005)"
        );
    }

    #[test]
    fn unallocated_object_diagnostics_use_a_constant_marker() {
        let event = DiagnosticEvent::AdapterFailure {
            adapter: None,
            operation: AdapterOperation::Enumerate,
            code: FailureCode::Internal(InternalFailure::UnsupportedInterface),
        };

        assert_eq!(
            event.to_string(),
            "adapter failure for unassigned during enumerate: internal(unsupported-interface)"
        );
    }
}
