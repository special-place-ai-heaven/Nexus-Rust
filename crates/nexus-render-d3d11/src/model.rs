//! Complete, backend-independent D3D11 state model.

/// D3D11 render-target slot count.
pub const OM_RENDER_TARGET_SLOTS: usize = 8;
/// D3D11 pixel/compute UAV slot count exposed by the base 11.0 context.
pub const PS_CS_UAV_SLOTS: usize = 8;
/// D3D11 input-assembler vertex-buffer slot count.
pub const IA_VERTEX_BUFFER_SLOTS: usize = 32;
/// D3D11 common-shader constant-buffer slot count.
pub const COMMONSHADER_CONSTANT_BUFFER_SLOTS: usize = 14;
/// D3D11 common-shader input-resource slot count.
pub const COMMONSHADER_INPUT_RESOURCE_SLOTS: usize = 128;
/// D3D11 common-shader sampler slot count.
pub const COMMONSHADER_SAMPLER_SLOTS: usize = 16;
/// D3D11 maximum number of dynamic shader class interfaces.
pub const SHADER_MAX_CLASS_INSTANCES: usize = 253;
/// D3D11 maximum viewport/scissor count.
pub const RASTERIZER_MAX_RECTS: usize = 16;
/// D3D11 stream-output buffer slot count.
pub const SO_BUFFER_SLOTS: usize = 4;

/// D3D11 viewport ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Viewport {
    /// Top-left X coordinate.
    pub top_left_x: f32,
    /// Top-left Y coordinate.
    pub top_left_y: f32,
    /// Width in pixels.
    pub width: f32,
    /// Height in pixels.
    pub height: f32,
    /// Minimum depth.
    pub min_depth: f32,
    /// Maximum depth.
    pub max_depth: f32,
}

/// Win32 `RECT` ABI used for D3D11 scissor rectangles.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// Left edge.
    pub left: i32,
    /// Top edge.
    pub top: i32,
    /// Right edge.
    pub right: i32,
    /// Bottom edge.
    pub bottom: i32,
}

/// A single input-assembler vertex-buffer binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VertexBufferBinding<H> {
    /// Buffer object, or no buffer for an unbound slot.
    pub buffer: Option<H>,
    /// Vertex stride.
    pub stride: u32,
    /// Byte offset.
    pub offset: u32,
}

/// Input-assembler index-buffer binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexBufferBinding<H> {
    /// Buffer object, or no buffer when unbound.
    pub buffer: Option<H>,
    /// Raw `DXGI_FORMAT` value.
    pub format: u32,
    /// Byte offset.
    pub offset: u32,
}

/// Complete input-assembler state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputAssemblerState<H> {
    /// Input layout.
    pub input_layout: Option<H>,
    /// All vertex-buffer slots.
    pub vertex_buffers: [VertexBufferBinding<H>; IA_VERTEX_BUFFER_SLOTS],
    /// Index-buffer binding.
    pub index_buffer: IndexBufferBinding<H>,
    /// Raw `D3D11_PRIMITIVE_TOPOLOGY` value.
    pub primitive_topology: u32,
}

/// Hidden UAV counter policy.
///
/// D3D11 exposes UAV bindings but does not expose their hidden append/consume
/// counters. Restoring with `u32::MAX` preserves the counter value rather than
/// inventing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HiddenCounterState {
    /// Preserve the counter through the restore call.
    Preserve,
}

/// Complete output-merger state exposed by the base D3D11 context.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputMergerState<H> {
    /// All render-target views.
    pub render_targets: [Option<H>; OM_RENDER_TARGET_SLOTS],
    /// Depth/stencil view.
    pub depth_stencil_view: Option<H>,
    /// All base-context output-merger UAV slots.
    pub unordered_access_views: [Option<H>; PS_CS_UAV_SLOTS],
    /// Policy for unobservable UAV counters.
    pub unordered_access_counters: HiddenCounterState,
    /// Blend state.
    pub blend_state: Option<H>,
    /// Blend factor.
    pub blend_factor: [f32; 4],
    /// Sample mask.
    pub sample_mask: u32,
    /// Depth/stencil state.
    pub depth_stencil_state: Option<H>,
    /// Stencil reference value.
    pub stencil_reference: u32,
}

/// Complete rasterizer state.
#[derive(Clone, Debug, PartialEq)]
pub struct RasterizerState<H> {
    /// Rasterizer state object.
    pub state: Option<H>,
    /// Active viewports in API order.
    pub viewports: Vec<Viewport>,
    /// Active scissor rectangles in API order.
    pub scissor_rects: Vec<Rect>,
}

/// Complete common programmable-shader stage state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgrammableStageState<H> {
    /// Shader object.
    pub shader: Option<H>,
    /// Dynamic class instances in API order.
    pub class_instances: Vec<H>,
    /// All constant-buffer slots.
    pub constant_buffers: [Option<H>; COMMONSHADER_CONSTANT_BUFFER_SLOTS],
    /// All shader-resource-view slots.
    pub shader_resources: [Option<H>; COMMONSHADER_INPUT_RESOURCE_SLOTS],
    /// All sampler slots.
    pub samplers: [Option<H>; COMMONSHADER_SAMPLER_SLOTS],
}

/// Complete compute-stage state, including output UAV bindings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputeState<H> {
    /// Common compute-shader stage state.
    pub stage: ProgrammableStageState<H>,
    /// Compute UAV bindings.
    pub unordered_access_views: [Option<H>; PS_CS_UAV_SLOTS],
    /// Policy for unobservable UAV counters.
    pub unordered_access_counters: HiddenCounterState,
}

/// Stream-output offsets.
///
/// `ID3D11DeviceContext::SOGetTargets` cannot report offsets. An exhaustive
/// backend therefore needs a shadow tracker and must return [`Self::Tracked`];
/// [`Self::Unobservable`] is diagnostic data and is not sufficient to create
/// a truthful exhaustive guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamOutputOffsets {
    /// Offsets supplied by an authoritative state tracker.
    Tracked([u32; SO_BUFFER_SLOTS]),
    /// Native D3D11 getters cannot observe the offsets.
    Unobservable,
}

/// Stream-output state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamOutputState<H> {
    /// Stream-output target buffers.
    pub targets: [Option<H>; SO_BUFFER_SLOTS],
    /// Target offsets.
    pub offsets: StreamOutputOffsets,
}

/// Predication state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredicationState<H> {
    /// Predicate object.
    pub predicate: Option<H>,
    /// Predicate comparison value.
    pub value: bool,
}

/// The state covered by the first Windows backend milestone.
#[derive(Clone, Debug, PartialEq)]
pub struct CriticalPipelineState<H> {
    /// Output-merger state.
    pub output_merger: OutputMergerState<H>,
    /// Input-assembler state.
    pub input_assembler: InputAssemblerState<H>,
    /// Rasterizer state.
    pub rasterizer: RasterizerState<H>,
    /// Vertex-shader stage.
    pub vertex_shader: ProgrammableStageState<H>,
    /// Pixel-shader stage.
    pub pixel_shader: ProgrammableStageState<H>,
}

/// Exhaustive D3D11 pipeline state model.
#[derive(Clone, Debug, PartialEq)]
pub struct PipelineState<H> {
    /// State implemented by the critical backend capability.
    pub critical: CriticalPipelineState<H>,
    /// Hull-shader stage.
    pub hull_shader: ProgrammableStageState<H>,
    /// Domain-shader stage.
    pub domain_shader: ProgrammableStageState<H>,
    /// Geometry-shader stage.
    pub geometry_shader: ProgrammableStageState<H>,
    /// Compute stage.
    pub compute_shader: ComputeState<H>,
    /// Stream-output state.
    pub stream_output: StreamOutputState<H>,
    /// Predication state.
    pub predication: PredicationState<H>,
}
