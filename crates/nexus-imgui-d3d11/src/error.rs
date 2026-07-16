//! Closed, redaction-safe renderer errors.

use nexus_imgui_runtime::{ContextError, FrameError};
use nexus_render_d3d11::{CaptureFailure, RestoreFailures, StateStep};
use thiserror::Error;

use crate::state::StateBackendError;

/// GPU operation associated with a failing HRESULT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpuOperation {
    /// Acquire a D3D11 device from a swap chain.
    GetDevice,
    /// Acquire the immediate device context.
    GetImmediateContext,
    /// Acquire the current swap-chain back buffer.
    GetBackBuffer,
    /// Create a render-target view.
    CreateRenderTargetView,
    /// Create a GPU buffer.
    CreateBuffer,
    /// Map a dynamic resource.
    MapResource,
    /// Create a vertex shader.
    CreateVertexShader,
    /// Create a pixel shader.
    CreatePixelShader,
    /// Create an input layout.
    CreateInputLayout,
    /// Create a blend state.
    CreateBlendState,
    /// Create a rasterizer state.
    CreateRasterizerState,
    /// Create a depth/stencil state.
    CreateDepthStencilState,
    /// Create a sampler state.
    CreateSamplerState,
    /// Create the font texture.
    CreateFontTexture,
    /// Create the font shader-resource view.
    CreateFontShaderResourceView,
}

/// Shader compiled for the renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShaderKind {
    /// Dear ImGui vertex shader.
    Vertex,
    /// Dear ImGui pixel shader.
    Pixel,
}

/// Closed renderer failure categories.
#[derive(Debug, Error)]
pub enum RendererError {
    /// Native-compatible ImGui context setup failed.
    #[error(transparent)]
    Context(#[from] ContextError),
    /// Dear ImGui frame validation or finalization failed.
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// A required raw pointer was null.
    #[error("a required {0} pointer was null")]
    NullPointer(&'static str),
    /// An ImGui count or pointer tuple was invalid.
    #[error("Dear ImGui draw data is structurally invalid")]
    InvalidDrawData,
    /// A count, byte size, or draw offset overflowed its API representation.
    #[error("Dear ImGui draw data exceeds D3D11 limits")]
    DrawDataOverflow,
    /// The draw data belongs to a different swap-chain generation.
    #[error("draw data targeted a stale swap-chain generation")]
    StaleGeneration,
    /// The framebuffer has no drawable pixels.
    #[error("the Dear ImGui framebuffer is empty or invalid")]
    InvalidFramebuffer,
    /// A deterministic built-in shader failed to compile.
    #[error("the built-in {0:?} shader failed to compile")]
    ShaderCompile(ShaderKind),
    /// A D3D11/DXGI operation returned a failing HRESULT.
    #[error("D3D11 operation {operation:?} failed with HRESULT {code:#010x}")]
    HResult {
        /// Operation that failed.
        operation: GpuOperation,
        /// Exact HRESULT value.
        code: i32,
    },
    /// Exhaustive state capture failed before drawing.
    #[error("D3D11 state capture failed at {step:?}")]
    StateCapture {
        /// Pipeline section that could not be captured.
        step: StateStep,
    },
    /// Exhaustive state restoration attempted every section but one or more failed.
    #[error("D3D11 state restoration failed in {failures} section(s)")]
    StateRestore {
        /// Number of failed restore sections.
        failures: usize,
    },
    /// The renderer has not uploaded a font atlas.
    #[error("the Dear ImGui font atlas is not installed")]
    FontAtlasUnavailable,
    /// Dear ImGui did not expose an atlas or RGBA pixels.
    #[error("the Dear ImGui RGBA font atlas is unavailable")]
    InvalidFontAtlas,
    /// A backend invariant was violated.
    #[error("the D3D11 state backend rejected an internal pipeline value")]
    StateBackend,
}

impl From<CaptureFailure<StateBackendError>> for RendererError {
    fn from(error: CaptureFailure<StateBackendError>) -> Self {
        Self::StateCapture { step: error.step }
    }
}

impl From<RestoreFailures<StateBackendError>> for RendererError {
    fn from(error: RestoreFailures<StateBackendError>) -> Self {
        Self::StateRestore {
            failures: error.failures().len(),
        }
    }
}
