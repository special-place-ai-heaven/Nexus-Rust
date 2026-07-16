//! Exhaustive Windows D3D11 pipeline backend.

use std::ffi::c_void;
use std::fmt;

use nexus_render_d3d11::{
    COMMONSHADER_CONSTANT_BUFFER_SLOTS, COMMONSHADER_INPUT_RESOURCE_SLOTS,
    COMMONSHADER_SAMPLER_SLOTS, ComputeState, CriticalPipelineBackend, ExhaustivePipelineBackend,
    HiddenCounterState, IA_VERTEX_BUFFER_SLOTS, IndexBufferBinding, InputAssemblerState,
    OM_RENDER_TARGET_SLOTS, OutputMergerState, PS_CS_UAV_SLOTS, PredicationState,
    ProgrammableStageState, RASTERIZER_MAX_RECTS, RasterizerState, Rect,
    SHADER_MAX_CLASS_INSTANCES, SO_BUFFER_SLOTS, StreamOutputOffsets, StreamOutputState,
    VertexBufferBinding, Viewport,
};
use thiserror::Error;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_11_1, D3D_PRIMITIVE_TOPOLOGY};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_1_UAV_SLOT_COUNT, D3D11_DEVICE_CONTEXT_IMMEDIATE, D3D11_VIEWPORT, ID3D11DeviceContext,
    ID3D11DeviceContext1,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::{BOOL, Interface};

const KEEP_UAV_COUNTER: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ConstantRange {
    first: u32,
    count: u32,
}

/// Type-erased owned COM reference captured from the D3D11 pipeline.
///
/// The optional constant-buffer range is retained when the immediate context
/// supports D3D11.1 range getters. Raw object pointers are intentionally not
/// exposed through `Debug` or the public API.
#[derive(Clone, PartialEq, Eq)]
pub struct PipelineHandle {
    object: nexus_render_d3d11::OwnedComObject,
    constant_range: Option<ConstantRange>,
}

impl fmt::Debug for PipelineHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PipelineHandle")
            .field("has_constant_range", &self.constant_range.is_some())
            .finish_non_exhaustive()
    }
}

impl PipelineHandle {
    fn raw(&self) -> *mut c_void {
        self.object.as_raw()
    }
}

/// Closed failures produced by exhaustive state capture or restoration.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum StateBackendError {
    /// The supplied context is not an immediate context.
    #[error("the D3D11 renderer requires an immediate device context")]
    DeferredContext,
    /// A COM query returned a failing HRESULT.
    #[error("a D3D11 interface query failed with HRESULT {0:#010x}")]
    Interface(i32),
    /// A driver reported a state count larger than the SDK maximum.
    #[error("a D3D11 getter reported a count above its SDK capacity")]
    InvalidReportedCount,
    /// Stream-output targets are bound and their byte offsets are unobservable.
    #[error("bound stream-output offsets are not observable through D3D11")]
    StreamOutputOffsetsUnobservable,
    /// A captured handle was used in an incompatible pipeline section.
    #[error("a captured D3D11 handle has incompatible metadata")]
    IncompatibleHandle,
    /// Extended UAV capture data was unavailable during restoration.
    #[error("extended D3D11 UAV state is unavailable")]
    ExtendedUavStateUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShaderStage {
    Vertex,
    Hull,
    Domain,
    Geometry,
    Pixel,
    Compute,
}

/// Full D3D11 pipeline backend used exclusively by the Dear ImGui renderer.
///
/// D3D11.1 constant-buffer ranges are captured with `*GetConstantBuffers1`
/// and restored with the corresponding range setter. Feature level 11.1 and
/// newer captures all 64 OM/CS UAV slots; older devices capture the base eight.
/// A bound stream-output buffer fails capture because `SOGetTargets` cannot
/// report its byte offset.
pub struct ExhaustiveWindowsBackend {
    context: ID3D11DeviceContext,
    context1: Option<ID3D11DeviceContext1>,
    uav_slots: usize,
    extended_om_uavs: Option<Vec<Option<PipelineHandle>>>,
    extended_cs_uavs: Option<Vec<Option<PipelineHandle>>>,
    drop_restore_failures: usize,
}

impl ExhaustiveWindowsBackend {
    /// Creates an exhaustive backend for a live immediate context.
    ///
    /// # Errors
    ///
    /// Returns an error for deferred contexts or an invalid device query.
    pub fn new(context: ID3D11DeviceContext) -> Result<Self, StateBackendError> {
        // SAFETY: `context` is an owned live COM interface.
        if unsafe { context.GetType() } != D3D11_DEVICE_CONTEXT_IMMEDIATE {
            return Err(StateBackendError::DeferredContext);
        }
        // SAFETY: `GetDevice` is valid for a live device context.
        let device = unsafe { context.GetDevice() }
            .map_err(|error| StateBackendError::Interface(error.code().0))?;
        // SAFETY: the returned feature level is a value query on a live device.
        let uav_slots = if unsafe { device.GetFeatureLevel() }.0 >= D3D_FEATURE_LEVEL_11_1.0 {
            D3D11_1_UAV_SLOT_COUNT as usize
        } else {
            PS_CS_UAV_SLOTS
        };
        let context1 = context.cast::<ID3D11DeviceContext1>().ok();
        Ok(Self {
            context,
            context1,
            uav_slots,
            extended_om_uavs: None,
            extended_cs_uavs: None,
            drop_restore_failures: 0,
        })
    }

    /// Acquires a context reference from a borrowed raw COM pointer.
    ///
    /// # Safety
    ///
    /// `raw` must be a live `ID3D11DeviceContext*` for the duration of this
    /// call. The returned backend acquires and owns its own COM reference.
    ///
    /// # Errors
    ///
    /// Returns an error for null pointers, deferred contexts, or invalid device
    /// queries.
    pub unsafe fn from_raw_borrowed(raw: *mut c_void) -> Result<Self, StateBackendError> {
        // SAFETY: the caller guarantees the exact live interface type.
        let context = unsafe { ID3D11DeviceContext::from_raw_borrowed(&raw) }
            .cloned()
            .ok_or(StateBackendError::Interface(
                windows::Win32::Foundation::E_POINTER.0,
            ))?;
        Self::new(context)
    }

    /// Number of restore sections that failed during the most recent implicit
    /// guard drop.
    #[must_use]
    pub const fn drop_restore_failure_count(&self) -> usize {
        self.drop_restore_failures
    }

    pub(crate) fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    fn capture_stage(
        &self,
        stage: ShaderStage,
    ) -> Result<ProgrammableStageState<PipelineHandle>, StateBackendError> {
        let vtable = Interface::vtable(&self.context);
        let this = self.context.as_raw();
        let mut shader = std::ptr::null_mut();
        let mut class_count = SHADER_MAX_CLASS_INSTANCES as u32;
        let mut class_instances = vec![std::ptr::null_mut(); SHADER_MAX_CLASS_INSTANCES];
        // SAFETY: all outputs have SDK-sized storage and every getter AddRefs
        // returned interfaces. Stage vtable entries share this exact ABI.
        unsafe {
            match stage {
                ShaderStage::Vertex => (vtable.VSGetShader)(
                    this,
                    &mut shader,
                    class_instances.as_mut_ptr(),
                    &mut class_count,
                ),
                ShaderStage::Hull => (vtable.HSGetShader)(
                    this,
                    &mut shader,
                    class_instances.as_mut_ptr(),
                    &mut class_count,
                ),
                ShaderStage::Domain => (vtable.DSGetShader)(
                    this,
                    &mut shader,
                    class_instances.as_mut_ptr(),
                    &mut class_count,
                ),
                ShaderStage::Geometry => (vtable.GSGetShader)(
                    this,
                    &mut shader,
                    class_instances.as_mut_ptr(),
                    &mut class_count,
                ),
                ShaderStage::Pixel => (vtable.PSGetShader)(
                    this,
                    &mut shader,
                    class_instances.as_mut_ptr(),
                    &mut class_count,
                ),
                ShaderStage::Compute => (vtable.CSGetShader)(
                    this,
                    &mut shader,
                    class_instances.as_mut_ptr(),
                    &mut class_count,
                ),
            }
        }
        let class_count =
            usize::try_from(class_count).map_err(|_| StateBackendError::InvalidReportedCount)?;
        if class_count > SHADER_MAX_CLASS_INSTANCES {
            // Release any references the driver placed inside the bounded array.
            let _owned = class_instances
                .into_iter()
                .filter_map(|raw| {
                    // SAFETY: each non-null getter output owns one reference.
                    unsafe { own_handle(raw, None) }
                })
                .collect::<Vec<_>>();
            // SAFETY: the shader getter returned one owned nullable reference.
            let _shader = unsafe { own_handle(shader, None) };
            return Err(StateBackendError::InvalidReportedCount);
        }
        class_instances.truncate(class_count);

        let (constant_buffers, ranges) = self.capture_constant_buffers(stage);
        let mut shader_resources = [std::ptr::null_mut(); COMMONSHADER_INPUT_RESOURCE_SLOTS];
        let mut samplers = [std::ptr::null_mut(); COMMONSHADER_SAMPLER_SLOTS];
        // SAFETY: arrays have exact SDK capacities; getters AddRef returned
        // interfaces and do not retain the output pointers.
        unsafe {
            match stage {
                ShaderStage::Vertex => {
                    (vtable.VSGetShaderResources)(
                        this,
                        0,
                        COMMONSHADER_INPUT_RESOURCE_SLOTS as u32,
                        shader_resources.as_mut_ptr(),
                    );
                    (vtable.VSGetSamplers)(
                        this,
                        0,
                        COMMONSHADER_SAMPLER_SLOTS as u32,
                        samplers.as_mut_ptr(),
                    );
                }
                ShaderStage::Hull => {
                    (vtable.HSGetShaderResources)(
                        this,
                        0,
                        COMMONSHADER_INPUT_RESOURCE_SLOTS as u32,
                        shader_resources.as_mut_ptr(),
                    );
                    (vtable.HSGetSamplers)(
                        this,
                        0,
                        COMMONSHADER_SAMPLER_SLOTS as u32,
                        samplers.as_mut_ptr(),
                    );
                }
                ShaderStage::Domain => {
                    (vtable.DSGetShaderResources)(
                        this,
                        0,
                        COMMONSHADER_INPUT_RESOURCE_SLOTS as u32,
                        shader_resources.as_mut_ptr(),
                    );
                    (vtable.DSGetSamplers)(
                        this,
                        0,
                        COMMONSHADER_SAMPLER_SLOTS as u32,
                        samplers.as_mut_ptr(),
                    );
                }
                ShaderStage::Geometry => {
                    (vtable.GSGetShaderResources)(
                        this,
                        0,
                        COMMONSHADER_INPUT_RESOURCE_SLOTS as u32,
                        shader_resources.as_mut_ptr(),
                    );
                    (vtable.GSGetSamplers)(
                        this,
                        0,
                        COMMONSHADER_SAMPLER_SLOTS as u32,
                        samplers.as_mut_ptr(),
                    );
                }
                ShaderStage::Pixel => {
                    (vtable.PSGetShaderResources)(
                        this,
                        0,
                        COMMONSHADER_INPUT_RESOURCE_SLOTS as u32,
                        shader_resources.as_mut_ptr(),
                    );
                    (vtable.PSGetSamplers)(
                        this,
                        0,
                        COMMONSHADER_SAMPLER_SLOTS as u32,
                        samplers.as_mut_ptr(),
                    );
                }
                ShaderStage::Compute => {
                    (vtable.CSGetShaderResources)(
                        this,
                        0,
                        COMMONSHADER_INPUT_RESOURCE_SLOTS as u32,
                        shader_resources.as_mut_ptr(),
                    );
                    (vtable.CSGetSamplers)(
                        this,
                        0,
                        COMMONSHADER_SAMPLER_SLOTS as u32,
                        samplers.as_mut_ptr(),
                    );
                }
            }
        }

        Ok(ProgrammableStageState {
            // SAFETY: the shader getter returned an owned nullable reference.
            shader: unsafe { own_handle(shader, None) },
            class_instances: class_instances
                .into_iter()
                // SAFETY: each non-null getter output is one owned reference.
                .filter_map(|raw| unsafe { own_handle(raw, None) })
                .collect(),
            constant_buffers: std::array::from_fn(|index| {
                // SAFETY: each non-null getter output is one owned reference.
                unsafe { own_handle(constant_buffers[index], ranges[index]) }
            }),
            shader_resources: std::array::from_fn(|index| {
                // SAFETY: each non-null getter output is one owned reference.
                unsafe { own_handle(shader_resources[index], None) }
            }),
            samplers: std::array::from_fn(|index| {
                // SAFETY: each non-null getter output is one owned reference.
                unsafe { own_handle(samplers[index], None) }
            }),
        })
    }

    fn capture_constant_buffers(
        &self,
        stage: ShaderStage,
    ) -> (
        [*mut c_void; COMMONSHADER_CONSTANT_BUFFER_SLOTS],
        [Option<ConstantRange>; COMMONSHADER_CONSTANT_BUFFER_SLOTS],
    ) {
        let mut buffers = [std::ptr::null_mut(); COMMONSHADER_CONSTANT_BUFFER_SLOTS];
        let mut first = [0; COMMONSHADER_CONSTANT_BUFFER_SLOTS];
        let mut count = [0; COMMONSHADER_CONSTANT_BUFFER_SLOTS];
        if let Some(context1) = &self.context1 {
            let vtable = Interface::vtable(context1);
            let this = context1.as_raw();
            // SAFETY: every output array has the exact 14-slot SDK capacity.
            unsafe {
                match stage {
                    ShaderStage::Vertex => (vtable.VSGetConstantBuffers1)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                        first.as_mut_ptr(),
                        count.as_mut_ptr(),
                    ),
                    ShaderStage::Hull => (vtable.HSGetConstantBuffers1)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                        first.as_mut_ptr(),
                        count.as_mut_ptr(),
                    ),
                    ShaderStage::Domain => (vtable.DSGetConstantBuffers1)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                        first.as_mut_ptr(),
                        count.as_mut_ptr(),
                    ),
                    ShaderStage::Geometry => (vtable.GSGetConstantBuffers1)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                        first.as_mut_ptr(),
                        count.as_mut_ptr(),
                    ),
                    ShaderStage::Pixel => (vtable.PSGetConstantBuffers1)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                        first.as_mut_ptr(),
                        count.as_mut_ptr(),
                    ),
                    ShaderStage::Compute => (vtable.CSGetConstantBuffers1)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                        first.as_mut_ptr(),
                        count.as_mut_ptr(),
                    ),
                }
            }
            (
                buffers,
                std::array::from_fn(|index| {
                    Some(ConstantRange {
                        first: first[index],
                        count: count[index],
                    })
                }),
            )
        } else {
            let vtable = Interface::vtable(&self.context);
            let this = self.context.as_raw();
            // SAFETY: output storage has the exact 14-slot SDK capacity.
            unsafe {
                match stage {
                    ShaderStage::Vertex => (vtable.VSGetConstantBuffers)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                    ),
                    ShaderStage::Hull => (vtable.HSGetConstantBuffers)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                    ),
                    ShaderStage::Domain => (vtable.DSGetConstantBuffers)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                    ),
                    ShaderStage::Geometry => (vtable.GSGetConstantBuffers)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                    ),
                    ShaderStage::Pixel => (vtable.PSGetConstantBuffers)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                    ),
                    ShaderStage::Compute => (vtable.CSGetConstantBuffers)(
                        this,
                        0,
                        COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                        buffers.as_mut_ptr(),
                    ),
                }
            }
            (buffers, [None; COMMONSHADER_CONSTANT_BUFFER_SLOTS])
        }
    }

    fn restore_stage(
        &self,
        stage: ShaderStage,
        state: &ProgrammableStageState<PipelineHandle>,
    ) -> Result<(), StateBackendError> {
        let vtable = Interface::vtable(&self.context);
        let this = self.context.as_raw();
        let shader = raw_optional(&state.shader);
        let classes = state
            .class_instances
            .iter()
            .map(PipelineHandle::raw)
            .collect::<Vec<_>>();
        let class_pointer = pointer_or_null(&classes);
        let class_count =
            u32::try_from(classes.len()).map_err(|_| StateBackendError::InvalidReportedCount)?;
        // SAFETY: all raw pointers remain owned by `state` throughout the call.
        unsafe {
            match stage {
                ShaderStage::Vertex => {
                    (vtable.VSSetShader)(this, shader, class_pointer, class_count)
                }
                ShaderStage::Hull => (vtable.HSSetShader)(this, shader, class_pointer, class_count),
                ShaderStage::Domain => {
                    (vtable.DSSetShader)(this, shader, class_pointer, class_count)
                }
                ShaderStage::Geometry => {
                    (vtable.GSSetShader)(this, shader, class_pointer, class_count)
                }
                ShaderStage::Pixel => {
                    (vtable.PSSetShader)(this, shader, class_pointer, class_count)
                }
                ShaderStage::Compute => {
                    (vtable.CSSetShader)(this, shader, class_pointer, class_count)
                }
            }
        }
        self.restore_constant_buffers(stage, &state.constant_buffers)?;

        let resources = raw_array(&state.shader_resources);
        let samplers = raw_array(&state.samplers);
        // SAFETY: snapshot references keep every raw interface alive and the
        // arrays have the exact common-shader slot capacities.
        unsafe {
            match stage {
                ShaderStage::Vertex => {
                    (vtable.VSSetShaderResources)(
                        this,
                        0,
                        resources.len() as u32,
                        resources.as_ptr(),
                    );
                    (vtable.VSSetSamplers)(this, 0, samplers.len() as u32, samplers.as_ptr());
                }
                ShaderStage::Hull => {
                    (vtable.HSSetShaderResources)(
                        this,
                        0,
                        resources.len() as u32,
                        resources.as_ptr(),
                    );
                    (vtable.HSSetSamplers)(this, 0, samplers.len() as u32, samplers.as_ptr());
                }
                ShaderStage::Domain => {
                    (vtable.DSSetShaderResources)(
                        this,
                        0,
                        resources.len() as u32,
                        resources.as_ptr(),
                    );
                    (vtable.DSSetSamplers)(this, 0, samplers.len() as u32, samplers.as_ptr());
                }
                ShaderStage::Geometry => {
                    (vtable.GSSetShaderResources)(
                        this,
                        0,
                        resources.len() as u32,
                        resources.as_ptr(),
                    );
                    (vtable.GSSetSamplers)(this, 0, samplers.len() as u32, samplers.as_ptr());
                }
                ShaderStage::Pixel => {
                    (vtable.PSSetShaderResources)(
                        this,
                        0,
                        resources.len() as u32,
                        resources.as_ptr(),
                    );
                    (vtable.PSSetSamplers)(this, 0, samplers.len() as u32, samplers.as_ptr());
                }
                ShaderStage::Compute => {
                    (vtable.CSSetShaderResources)(
                        this,
                        0,
                        resources.len() as u32,
                        resources.as_ptr(),
                    );
                    (vtable.CSSetSamplers)(this, 0, samplers.len() as u32, samplers.as_ptr());
                }
            }
        }
        Ok(())
    }

    fn restore_constant_buffers(
        &self,
        stage: ShaderStage,
        state: &[Option<PipelineHandle>; COMMONSHADER_CONSTANT_BUFFER_SLOTS],
    ) -> Result<(), StateBackendError> {
        let buffers = raw_array(state);
        if let Some(context1) = &self.context1 {
            let mut first = [0; COMMONSHADER_CONSTANT_BUFFER_SLOTS];
            let mut count = [0; COMMONSHADER_CONSTANT_BUFFER_SLOTS];
            for (index, handle) in state.iter().enumerate() {
                if let Some(handle) = handle {
                    let range = handle
                        .constant_range
                        .ok_or(StateBackendError::IncompatibleHandle)?;
                    first[index] = range.first;
                    count[index] = range.count;
                }
            }
            let vtable = Interface::vtable(context1);
            let this = context1.as_raw();
            // SAFETY: snapshot references keep buffers alive and all arrays
            // have the exact common-shader constant-buffer slot capacity.
            unsafe {
                match stage {
                    ShaderStage::Vertex => (vtable.VSSetConstantBuffers1)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                        first.as_ptr(),
                        count.as_ptr(),
                    ),
                    ShaderStage::Hull => (vtable.HSSetConstantBuffers1)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                        first.as_ptr(),
                        count.as_ptr(),
                    ),
                    ShaderStage::Domain => (vtable.DSSetConstantBuffers1)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                        first.as_ptr(),
                        count.as_ptr(),
                    ),
                    ShaderStage::Geometry => (vtable.GSSetConstantBuffers1)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                        first.as_ptr(),
                        count.as_ptr(),
                    ),
                    ShaderStage::Pixel => (vtable.PSSetConstantBuffers1)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                        first.as_ptr(),
                        count.as_ptr(),
                    ),
                    ShaderStage::Compute => (vtable.CSSetConstantBuffers1)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                        first.as_ptr(),
                        count.as_ptr(),
                    ),
                }
            }
        } else {
            let vtable = Interface::vtable(&self.context);
            let this = self.context.as_raw();
            // SAFETY: snapshot references keep buffers alive and the array has
            // the exact common-shader constant-buffer capacity.
            unsafe {
                match stage {
                    ShaderStage::Vertex => (vtable.VSSetConstantBuffers)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                    ),
                    ShaderStage::Hull => (vtable.HSSetConstantBuffers)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                    ),
                    ShaderStage::Domain => (vtable.DSSetConstantBuffers)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                    ),
                    ShaderStage::Geometry => (vtable.GSSetConstantBuffers)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                    ),
                    ShaderStage::Pixel => (vtable.PSSetConstantBuffers)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                    ),
                    ShaderStage::Compute => (vtable.CSSetConstantBuffers)(
                        this,
                        0,
                        buffers.len() as u32,
                        buffers.as_ptr(),
                    ),
                }
            }
        }
        Ok(())
    }

    fn capture_uavs(&self, output_merger: bool) -> Vec<Option<PipelineHandle>> {
        let mut raw = vec![std::ptr::null_mut(); self.uav_slots];
        let vtable = Interface::vtable(&self.context);
        // SAFETY: the getter receives a `uav_slots`-sized output. OM uses zero
        // RTVs so UAV register zero is observable without slot overlap.
        unsafe {
            if output_merger {
                (vtable.OMGetRenderTargetsAndUnorderedAccessViews)(
                    self.context.as_raw(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                    self.uav_slots as u32,
                    raw.as_mut_ptr(),
                );
            } else {
                (vtable.CSGetUnorderedAccessViews)(
                    self.context.as_raw(),
                    0,
                    self.uav_slots as u32,
                    raw.as_mut_ptr(),
                );
            }
        }
        raw.into_iter()
            // SAFETY: every non-null getter output owns one reference.
            .map(|raw| unsafe { own_handle(raw, None) })
            .collect()
    }

    fn combined_uavs(
        &self,
        base: &[Option<PipelineHandle>; PS_CS_UAV_SLOTS],
        extended: &Option<Vec<Option<PipelineHandle>>>,
    ) -> Result<Vec<*mut c_void>, StateBackendError> {
        let mut raw = base.iter().map(raw_optional).collect::<Vec<_>>();
        if self.uav_slots > PS_CS_UAV_SLOTS {
            let extended = extended
                .as_ref()
                .ok_or(StateBackendError::ExtendedUavStateUnavailable)?;
            if extended.len() != self.uav_slots - PS_CS_UAV_SLOTS {
                return Err(StateBackendError::ExtendedUavStateUnavailable);
            }
            raw.extend(extended.iter().map(raw_optional));
        }
        Ok(raw)
    }
}

impl CriticalPipelineBackend for ExhaustiveWindowsBackend {
    type Handle = PipelineHandle;
    type Error = StateBackendError;

    fn capture_output_merger(&mut self) -> Result<OutputMergerState<Self::Handle>, Self::Error> {
        let vtable = Interface::vtable(&self.context);
        let mut render_targets = [std::ptr::null_mut(); OM_RENDER_TARGET_SLOTS];
        let mut depth_stencil = std::ptr::null_mut();
        let mut blend_state = std::ptr::null_mut();
        let mut blend_factor = [0.0; 4];
        let mut sample_mask = 0;
        let mut depth_state = std::ptr::null_mut();
        let mut stencil_reference = 0;
        // SAFETY: output storage has exact SDK sizes; getters AddRef objects.
        unsafe {
            (vtable.OMGetRenderTargets)(
                self.context.as_raw(),
                OM_RENDER_TARGET_SLOTS as u32,
                render_targets.as_mut_ptr(),
                &mut depth_stencil,
            );
            (vtable.OMGetBlendState)(
                self.context.as_raw(),
                &mut blend_state,
                blend_factor.as_mut_ptr(),
                &mut sample_mask,
            );
            (vtable.OMGetDepthStencilState)(
                self.context.as_raw(),
                &mut depth_state,
                &mut stencil_reference,
            );
        }
        let mut uavs = self.capture_uavs(true);
        self.extended_om_uavs = Some(uavs.drain(PS_CS_UAV_SLOTS..).collect());
        Ok(OutputMergerState {
            render_targets: std::array::from_fn(|index| {
                // SAFETY: every non-null getter output owns one reference.
                unsafe { own_handle(render_targets[index], None) }
            }),
            // SAFETY: the getter output owns one nullable reference.
            depth_stencil_view: unsafe { own_handle(depth_stencil, None) },
            unordered_access_views: std::array::from_fn(|index| uavs[index].clone()),
            unordered_access_counters: HiddenCounterState::Preserve,
            // SAFETY: the getter output owns one nullable reference.
            blend_state: unsafe { own_handle(blend_state, None) },
            blend_factor,
            sample_mask,
            // SAFETY: the getter output owns one nullable reference.
            depth_stencil_state: unsafe { own_handle(depth_state, None) },
            stencil_reference,
        })
    }

    fn capture_input_assembler(
        &mut self,
    ) -> Result<InputAssemblerState<Self::Handle>, Self::Error> {
        let vtable = Interface::vtable(&self.context);
        let mut input_layout = std::ptr::null_mut();
        let mut buffers = [std::ptr::null_mut(); IA_VERTEX_BUFFER_SLOTS];
        let mut strides = [0; IA_VERTEX_BUFFER_SLOTS];
        let mut offsets = [0; IA_VERTEX_BUFFER_SLOTS];
        let mut index_buffer = std::ptr::null_mut();
        let mut format = DXGI_FORMAT::default();
        let mut index_offset = 0;
        let mut topology = D3D_PRIMITIVE_TOPOLOGY::default();
        // SAFETY: outputs have exact SDK sizes and getters AddRef objects.
        unsafe {
            (vtable.IAGetInputLayout)(self.context.as_raw(), &mut input_layout);
            (vtable.IAGetVertexBuffers)(
                self.context.as_raw(),
                0,
                IA_VERTEX_BUFFER_SLOTS as u32,
                buffers.as_mut_ptr(),
                strides.as_mut_ptr(),
                offsets.as_mut_ptr(),
            );
            (vtable.IAGetIndexBuffer)(
                self.context.as_raw(),
                &mut index_buffer,
                &mut format,
                &mut index_offset,
            );
            (vtable.IAGetPrimitiveTopology)(self.context.as_raw(), &mut topology);
        }
        Ok(InputAssemblerState {
            // SAFETY: the getter output owns one nullable reference.
            input_layout: unsafe { own_handle(input_layout, None) },
            vertex_buffers: std::array::from_fn(|index| VertexBufferBinding {
                // SAFETY: each non-null getter output owns one reference.
                buffer: unsafe { own_handle(buffers[index], None) },
                stride: strides[index],
                offset: offsets[index],
            }),
            index_buffer: IndexBufferBinding {
                // SAFETY: the getter output owns one nullable reference.
                buffer: unsafe { own_handle(index_buffer, None) },
                format: format.0 as u32,
                offset: index_offset,
            },
            primitive_topology: topology.0 as u32,
        })
    }

    fn capture_rasterizer(&mut self) -> Result<RasterizerState<Self::Handle>, Self::Error> {
        let vtable = Interface::vtable(&self.context);
        let mut state = std::ptr::null_mut();
        let mut viewport_count = 0;
        let mut rect_count = 0;
        // SAFETY: count-only calls use null arrays as documented.
        unsafe {
            (vtable.RSGetState)(self.context.as_raw(), &mut state);
            (vtable.RSGetViewports)(
                self.context.as_raw(),
                &mut viewport_count,
                std::ptr::null_mut(),
            );
            (vtable.RSGetScissorRects)(
                self.context.as_raw(),
                &mut rect_count,
                std::ptr::null_mut(),
            );
        }
        if viewport_count as usize > RASTERIZER_MAX_RECTS
            || rect_count as usize > RASTERIZER_MAX_RECTS
        {
            // SAFETY: the state getter output owns one nullable reference.
            let _state = unsafe { own_handle(state, None) };
            return Err(StateBackendError::InvalidReportedCount);
        }
        let mut viewports = vec![D3D11_VIEWPORT::default(); viewport_count as usize];
        let mut rects = vec![RECT::default(); rect_count as usize];
        // SAFETY: arrays were sized from validated SDK-bounded counts.
        unsafe {
            (vtable.RSGetViewports)(
                self.context.as_raw(),
                &mut viewport_count,
                pointer_or_null_mut(&mut viewports),
            );
            (vtable.RSGetScissorRects)(
                self.context.as_raw(),
                &mut rect_count,
                pointer_or_null_mut(&mut rects),
            );
        }
        Ok(RasterizerState {
            // SAFETY: the getter output owns one nullable reference.
            state: unsafe { own_handle(state, None) },
            viewports: viewports
                .into_iter()
                .map(|viewport| Viewport {
                    top_left_x: viewport.TopLeftX,
                    top_left_y: viewport.TopLeftY,
                    width: viewport.Width,
                    height: viewport.Height,
                    min_depth: viewport.MinDepth,
                    max_depth: viewport.MaxDepth,
                })
                .collect(),
            scissor_rects: rects
                .into_iter()
                .map(|rect| Rect {
                    left: rect.left,
                    top: rect.top,
                    right: rect.right,
                    bottom: rect.bottom,
                })
                .collect(),
        })
    }

    fn capture_vertex_shader(
        &mut self,
    ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
        self.capture_stage(ShaderStage::Vertex)
    }

    fn capture_pixel_shader(
        &mut self,
    ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
        self.capture_stage(ShaderStage::Pixel)
    }

    fn restore_output_merger(
        &mut self,
        state: &OutputMergerState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        let render_target_count = last_bound_slot(&state.render_targets);
        let uavs = self.combined_uavs(&state.unordered_access_views, &self.extended_om_uavs)?;
        if uavs[..render_target_count]
            .iter()
            .any(|pointer| !pointer.is_null())
        {
            return Err(StateBackendError::IncompatibleHandle);
        }
        let render_targets = raw_array(&state.render_targets);
        let initial_counts = vec![KEEP_UAV_COUNTER; self.uav_slots];
        let uav_count = self.uav_slots - render_target_count;
        let vtable = Interface::vtable(&self.context);
        // SAFETY: all snapshot objects remain alive and the shared OM slots are
        // restored atomically, followed by independent blend/depth state.
        unsafe {
            (vtable.OMSetRenderTargetsAndUnorderedAccessViews)(
                self.context.as_raw(),
                render_target_count as u32,
                pointer_or_null(&render_targets[..render_target_count]),
                raw_optional(&state.depth_stencil_view),
                render_target_count as u32,
                uav_count as u32,
                pointer_or_null(&uavs[render_target_count..]),
                pointer_or_null(&initial_counts[render_target_count..]),
            );
            (vtable.OMSetBlendState)(
                self.context.as_raw(),
                raw_optional(&state.blend_state),
                state.blend_factor.as_ptr(),
                state.sample_mask,
            );
            (vtable.OMSetDepthStencilState)(
                self.context.as_raw(),
                raw_optional(&state.depth_stencil_state),
                state.stencil_reference,
            );
        }
        Ok(())
    }

    fn restore_input_assembler(
        &mut self,
        state: &InputAssemblerState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        let buffers = std::array::from_fn::<_, IA_VERTEX_BUFFER_SLOTS, _>(|index| {
            raw_optional(&state.vertex_buffers[index].buffer)
        });
        let strides = std::array::from_fn::<_, IA_VERTEX_BUFFER_SLOTS, _>(|index| {
            state.vertex_buffers[index].stride
        });
        let offsets = std::array::from_fn::<_, IA_VERTEX_BUFFER_SLOTS, _>(|index| {
            state.vertex_buffers[index].offset
        });
        let vtable = Interface::vtable(&self.context);
        // SAFETY: snapshot objects remain alive and arrays cover every IA slot.
        unsafe {
            (vtable.IASetInputLayout)(self.context.as_raw(), raw_optional(&state.input_layout));
            (vtable.IASetVertexBuffers)(
                self.context.as_raw(),
                0,
                IA_VERTEX_BUFFER_SLOTS as u32,
                buffers.as_ptr(),
                strides.as_ptr(),
                offsets.as_ptr(),
            );
            (vtable.IASetIndexBuffer)(
                self.context.as_raw(),
                raw_optional(&state.index_buffer.buffer),
                DXGI_FORMAT(state.index_buffer.format as i32),
                state.index_buffer.offset,
            );
            (vtable.IASetPrimitiveTopology)(
                self.context.as_raw(),
                D3D_PRIMITIVE_TOPOLOGY(state.primitive_topology as i32),
            );
        }
        Ok(())
    }

    fn restore_rasterizer(
        &mut self,
        state: &RasterizerState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        let viewports = state
            .viewports
            .iter()
            .map(|viewport| D3D11_VIEWPORT {
                TopLeftX: viewport.top_left_x,
                TopLeftY: viewport.top_left_y,
                Width: viewport.width,
                Height: viewport.height,
                MinDepth: viewport.min_depth,
                MaxDepth: viewport.max_depth,
            })
            .collect::<Vec<_>>();
        let rects = state
            .scissor_rects
            .iter()
            .map(|rect| RECT {
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
            })
            .collect::<Vec<_>>();
        let vtable = Interface::vtable(&self.context);
        // SAFETY: snapshot objects and local arrays remain live for each call.
        unsafe {
            (vtable.RSSetState)(self.context.as_raw(), raw_optional(&state.state));
            (vtable.RSSetViewports)(
                self.context.as_raw(),
                viewports.len() as u32,
                pointer_or_null(&viewports),
            );
            (vtable.RSSetScissorRects)(
                self.context.as_raw(),
                rects.len() as u32,
                pointer_or_null(&rects),
            );
        }
        Ok(())
    }

    fn restore_vertex_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore_stage(ShaderStage::Vertex, state)
    }

    fn restore_pixel_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore_stage(ShaderStage::Pixel, state)
    }

    fn on_drop_restore_failures(
        &mut self,
        failures: &nexus_render_d3d11::RestoreFailures<Self::Error>,
    ) {
        self.drop_restore_failures = failures.failures().len();
    }
}

impl ExhaustivePipelineBackend for ExhaustiveWindowsBackend {
    fn capture_hull_shader(&mut self) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
        self.capture_stage(ShaderStage::Hull)
    }

    fn capture_domain_shader(
        &mut self,
    ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
        self.capture_stage(ShaderStage::Domain)
    }

    fn capture_geometry_shader(
        &mut self,
    ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
        self.capture_stage(ShaderStage::Geometry)
    }

    fn capture_compute_shader(&mut self) -> Result<ComputeState<Self::Handle>, Self::Error> {
        let mut uavs = self.capture_uavs(false);
        self.extended_cs_uavs = Some(uavs.drain(PS_CS_UAV_SLOTS..).collect());
        Ok(ComputeState {
            stage: self.capture_stage(ShaderStage::Compute)?,
            unordered_access_views: std::array::from_fn(|index| uavs[index].clone()),
            unordered_access_counters: HiddenCounterState::Preserve,
        })
    }

    fn capture_stream_output(&mut self) -> Result<StreamOutputState<Self::Handle>, Self::Error> {
        let mut targets = [std::ptr::null_mut(); SO_BUFFER_SLOTS];
        let vtable = Interface::vtable(&self.context);
        // SAFETY: output storage covers all four SO slots and getter AddRefs.
        unsafe {
            (vtable.SOGetTargets)(
                self.context.as_raw(),
                SO_BUFFER_SLOTS as u32,
                targets.as_mut_ptr(),
            );
        }
        let targets = std::array::from_fn(|index| {
            // SAFETY: each non-null getter output owns one reference.
            unsafe { own_handle(targets[index], None) }
        });
        if targets.iter().any(Option::is_some) {
            return Err(StateBackendError::StreamOutputOffsetsUnobservable);
        }
        Ok(StreamOutputState {
            targets,
            offsets: StreamOutputOffsets::Tracked([0; SO_BUFFER_SLOTS]),
        })
    }

    fn capture_predication(&mut self) -> Result<PredicationState<Self::Handle>, Self::Error> {
        let mut predicate = std::ptr::null_mut();
        let mut value = BOOL::default();
        let vtable = Interface::vtable(&self.context);
        // SAFETY: outputs are valid and the getter AddRefs the predicate.
        unsafe {
            (vtable.GetPredication)(self.context.as_raw(), &mut predicate, &mut value);
        }
        Ok(PredicationState {
            // SAFETY: the getter output owns one nullable reference.
            predicate: unsafe { own_handle(predicate, None) },
            value: value.as_bool(),
        })
    }

    fn restore_stream_output(
        &mut self,
        state: &StreamOutputState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        let StreamOutputOffsets::Tracked(offsets) = state.offsets else {
            return Err(StateBackendError::StreamOutputOffsetsUnobservable);
        };
        let targets = raw_array(&state.targets);
        let vtable = Interface::vtable(&self.context);
        // SAFETY: snapshot objects remain alive and arrays cover every SO slot.
        unsafe {
            (vtable.SOSetTargets)(
                self.context.as_raw(),
                SO_BUFFER_SLOTS as u32,
                targets.as_ptr(),
                offsets.as_ptr(),
            );
        }
        Ok(())
    }

    fn restore_compute_outputs(
        &mut self,
        state: &ComputeState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        let uavs = self.combined_uavs(&state.unordered_access_views, &self.extended_cs_uavs)?;
        let counters = vec![KEEP_UAV_COUNTER; self.uav_slots];
        let vtable = Interface::vtable(&self.context);
        // SAFETY: all snapshot UAVs remain alive; preserving counters avoids
        // inventing values for append/consume hidden state.
        unsafe {
            (vtable.CSSetUnorderedAccessViews)(
                self.context.as_raw(),
                0,
                self.uav_slots as u32,
                uavs.as_ptr(),
                counters.as_ptr(),
            );
        }
        Ok(())
    }

    fn restore_hull_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore_stage(ShaderStage::Hull, state)
    }

    fn restore_domain_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore_stage(ShaderStage::Domain, state)
    }

    fn restore_geometry_shader(
        &mut self,
        state: &ProgrammableStageState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore_stage(ShaderStage::Geometry, state)
    }

    fn restore_compute_shader(
        &mut self,
        state: &ComputeState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        self.restore_stage(ShaderStage::Compute, &state.stage)
    }

    fn restore_predication(
        &mut self,
        state: &PredicationState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        let vtable = Interface::vtable(&self.context);
        // SAFETY: the captured predicate remains alive throughout the call.
        unsafe {
            (vtable.SetPredication)(
                self.context.as_raw(),
                raw_optional(&state.predicate),
                BOOL::from(state.value),
            );
        }
        Ok(())
    }
}

unsafe fn own_handle(
    raw: *mut c_void,
    constant_range: Option<ConstantRange>,
) -> Option<PipelineHandle> {
    // SAFETY: caller transfers exactly one getter-owned COM reference.
    unsafe { nexus_render_d3d11::OwnedComObject::from_raw_owned(raw) }.map(|object| {
        PipelineHandle {
            object,
            constant_range,
        }
    })
}

fn raw_optional(handle: &Option<PipelineHandle>) -> *mut c_void {
    handle
        .as_ref()
        .map_or(std::ptr::null_mut(), PipelineHandle::raw)
}

fn raw_array<const N: usize>(handles: &[Option<PipelineHandle>; N]) -> [*mut c_void; N] {
    std::array::from_fn(|index| raw_optional(&handles[index]))
}

fn pointer_or_null<T>(slice: &[T]) -> *const T {
    if slice.is_empty() {
        std::ptr::null()
    } else {
        slice.as_ptr()
    }
}

fn pointer_or_null_mut<T>(slice: &mut [T]) -> *mut T {
    if slice.is_empty() {
        std::ptr::null_mut()
    } else {
        slice.as_mut_ptr()
    }
}

fn last_bound_slot<const N: usize>(handles: &[Option<PipelineHandle>; N]) -> usize {
    handles
        .iter()
        .rposition(Option::is_some)
        .map_or(0, |index| index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_render_d3d11::FullStateGuard;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDevice,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;

    #[test]
    fn last_bound_slot_preserves_sparse_output_shape() {
        let handles: [Option<PipelineHandle>; 4] = std::array::from_fn(|_| None);
        assert_eq!(last_bound_slot(&handles), 0);
    }

    #[test]
    fn empty_slice_pointer_is_null() {
        assert!(pointer_or_null::<u32>(&[]).is_null());
        assert!(!pointer_or_null(&[1_u32]).is_null());
    }

    #[test]
    fn warp_immediate_context_round_trips_the_full_guard() {
        let mut device = None;
        let mut context = None;
        // SAFETY: WARP creates a process-local software device; all optional
        // inputs are absent and output storage remains live for the call.
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG::default(),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .expect("the Windows WARP software device is available");
        let _device = device.expect("D3D11 returned the requested device");
        let context = context.expect("D3D11 returned the requested immediate context");
        let mut backend =
            ExhaustiveWindowsBackend::new(context).expect("WARP context is immediate and valid");
        FullStateGuard::capture(&mut backend)
            .expect("every idle WARP state is observable")
            .restore()
            .expect("every captured WARP state restores");
    }
}
