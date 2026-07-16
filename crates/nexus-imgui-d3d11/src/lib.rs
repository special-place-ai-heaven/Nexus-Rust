//! Dear ImGui 1.80 rendering for an attached D3D11 swap-chain generation.
//!
//! Rendering is admitted only after [`ExhaustiveWindowsBackend`] captures every
//! pipeline section represented by [`nexus_render_d3d11::FullStateGuard`]. A
//! bound stream-output target is rejected because base D3D11 cannot report its
//! byte offset; no draw is issued in that case.

mod error;
mod plan;
mod renderer;
mod state;

pub use error::{GpuOperation, RendererError, ShaderKind};
pub use renderer::{D3d11Renderer, RenderStats};
pub use state::{ExhaustiveWindowsBackend, PipelineHandle, StateBackendError};
