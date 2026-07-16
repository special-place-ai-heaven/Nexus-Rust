//! Backend capability traits.

use crate::model::{
    ComputeState, InputAssemblerState, OutputMergerState, PredicationState, ProgrammableStageState,
    RasterizerState, StreamOutputState,
};

/// Backend capable of capturing and restoring the critical D3D11 state.
///
/// Each getter must return owned handles. Each restore method must consume no
/// ownership and leave the snapshot reusable for diagnostics.
pub trait CriticalPipelineBackend {
    /// Owned object handle stored in snapshots.
    type Handle;
    /// Backend error.
    type Error;

    /// Capture output-merger state.
    fn capture_output_merger(&mut self) -> Result<OutputMergerState<Self::Handle>, Self::Error>;
    /// Capture input-assembler state.
    fn capture_input_assembler(&mut self)
    -> Result<InputAssemblerState<Self::Handle>, Self::Error>;
    /// Capture rasterizer state.
    fn capture_rasterizer(&mut self) -> Result<RasterizerState<Self::Handle>, Self::Error>;
    /// Capture vertex-shader state.
    fn capture_vertex_shader(
        &mut self,
    ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error>;
    /// Capture pixel-shader state.
    fn capture_pixel_shader(&mut self)
    -> Result<ProgrammableStageState<Self::Handle>, Self::Error>;

    /// Restore output-merger state.
    fn restore_output_merger(
        &mut self,
        state: &OutputMergerState<Self::Handle>,
    ) -> Result<(), Self::Error>;
    /// Restore input-assembler state.
    fn restore_input_assembler(
        &mut self,
        state: &InputAssemblerState<Self::Handle>,
    ) -> Result<(), Self::Error>;
    /// Restore rasterizer state.
    fn restore_rasterizer(
        &mut self,
        state: &RasterizerState<Self::Handle>,
    ) -> Result<(), Self::Error>;
    /// Restore vertex-shader state.
    fn restore_vertex_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error>;
    /// Restore pixel-shader state.
    fn restore_pixel_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error>;

    /// Observe failures from an implicit Drop restore.
    ///
    /// The default intentionally does nothing. Implementations may send these
    /// failures to a non-panicking diagnostic sink.
    fn on_drop_restore_failures(&mut self, _failures: &crate::guard::RestoreFailures<Self::Error>) {
    }
}

/// Backend capability required by [`crate::FullStateGuard`].
///
/// This is intentionally a separate trait. The interim Windows backend does
/// not implement it, so exhaustive restoration cannot be selected by mistake.
pub trait ExhaustivePipelineBackend: CriticalPipelineBackend {
    /// Capture hull-shader state.
    fn capture_hull_shader(&mut self) -> Result<ProgrammableStageState<Self::Handle>, Self::Error>;
    /// Capture domain-shader state.
    fn capture_domain_shader(
        &mut self,
    ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error>;
    /// Capture geometry-shader state.
    fn capture_geometry_shader(
        &mut self,
    ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error>;
    /// Capture compute-stage state.
    fn capture_compute_shader(&mut self) -> Result<ComputeState<Self::Handle>, Self::Error>;
    /// Capture stream-output state, including tracked offsets.
    fn capture_stream_output(&mut self) -> Result<StreamOutputState<Self::Handle>, Self::Error>;
    /// Capture predication state.
    fn capture_predication(&mut self) -> Result<PredicationState<Self::Handle>, Self::Error>;

    /// Restore stream-output bindings before input resources.
    fn restore_stream_output(
        &mut self,
        state: &StreamOutputState<Self::Handle>,
    ) -> Result<(), Self::Error>;
    /// Restore compute UAV output bindings before input resources.
    fn restore_compute_outputs(
        &mut self,
        state: &ComputeState<Self::Handle>,
    ) -> Result<(), Self::Error>;
    /// Restore hull-shader state.
    fn restore_hull_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error>;
    /// Restore domain-shader state.
    fn restore_domain_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error>;
    /// Restore geometry-shader state.
    fn restore_geometry_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error>;
    /// Restore the compute shader and its input bindings.
    fn restore_compute_shader(
        &mut self,
        state: &ComputeState<Self::Handle>,
    ) -> Result<(), Self::Error>;
    /// Restore predication state.
    fn restore_predication(
        &mut self,
        state: &PredicationState<Self::Handle>,
    ) -> Result<(), Self::Error>;
}
