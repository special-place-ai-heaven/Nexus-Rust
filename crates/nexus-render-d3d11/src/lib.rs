//! D3D11 pipeline-state capture and restoration.
//!
//! The public API deliberately distinguishes the currently implemented
//! critical Windows backend from the exhaustive capability. A
//! [`WindowsD3d11Backend`] implements [`CriticalPipelineBackend`] only, so a
//! [`FullStateGuard`] cannot be constructed with it until every remaining
//! stage has a real Windows implementation.
//!
//! No operation calls `ID3D11DeviceContext::ClearState`. State is restored
//! explicitly, and owned COM references keep every captured object alive.

mod backend;
mod com;
mod guard;

#[cfg(test)]
mod guard_tests;
mod model;
mod raw;
mod windows_backend;

pub use backend::{CriticalPipelineBackend, ExhaustivePipelineBackend};
pub use com::{HResultError, OwnedComObject};
pub use guard::{
    CaptureFailure, CriticalStateGuard, FailureCause, FullStateGuard, GuardOperation,
    RestoreFailure, RestoreFailures, StateStep,
};
pub use model::{
    COMMONSHADER_CONSTANT_BUFFER_SLOTS, COMMONSHADER_INPUT_RESOURCE_SLOTS,
    COMMONSHADER_SAMPLER_SLOTS, ComputeState, CriticalPipelineState, HiddenCounterState,
    IA_VERTEX_BUFFER_SLOTS, IndexBufferBinding, InputAssemblerState, OM_RENDER_TARGET_SLOTS,
    OutputMergerState, PS_CS_UAV_SLOTS, PipelineState, PredicationState, ProgrammableStageState,
    RASTERIZER_MAX_RECTS, RasterizerState, Rect, SHADER_MAX_CLASS_INSTANCES, SO_BUFFER_SLOTS,
    StreamOutputOffsets, StreamOutputState, VertexBufferBinding, Viewport,
};
pub use windows_backend::{WindowsBackendError, WindowsD3d11Backend};
