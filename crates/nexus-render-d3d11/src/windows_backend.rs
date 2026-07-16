//! Critical D3D11 backend over an exact local C ABI.

use std::ffi::c_void;
use std::fmt;
use std::ptr::NonNull;

use crate::backend::CriticalPipelineBackend;
use crate::com::{HResultError, OwnedComObject};
use crate::model::{
    COMMONSHADER_CONSTANT_BUFFER_SLOTS, COMMONSHADER_INPUT_RESOURCE_SLOTS,
    COMMONSHADER_SAMPLER_SLOTS, HiddenCounterState, IA_VERTEX_BUFFER_SLOTS, IndexBufferBinding,
    InputAssemblerState, OM_RENDER_TARGET_SLOTS, OutputMergerState, PS_CS_UAV_SLOTS,
    ProgrammableStageState, RASTERIZER_MAX_RECTS, RasterizerState, Rect,
    SHADER_MAX_CLASS_INSTANCES, VertexBufferBinding, Viewport,
};
use crate::raw::{IID_ID3D11_DEVICE_CONTEXT, RawDeviceContext, RawDeviceContextVTable};

const KEEP_UAV_COUNTER: u32 = u32::MAX;

/// Failure from the critical Windows backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowsBackendError {
    /// A required input COM pointer was null.
    NullComPointer,
    /// `QueryInterface(ID3D11DeviceContext)` failed.
    QueryInterface(HResultError),
    /// A D3D11 getter reported more entries than the SDK maximum.
    ReportedCountOutOfRange {
        /// State section reporting the count.
        section: &'static str,
        /// Count reported by D3D11.
        reported: u32,
        /// Capacity guaranteed by the SDK ABI.
        capacity: usize,
    },
    /// A caller-provided snapshot exceeds a D3D11 SDK limit.
    SnapshotCountOutOfRange {
        /// State section containing the count.
        section: &'static str,
        /// Count present in the snapshot.
        count: usize,
        /// Capacity guaranteed by the SDK ABI.
        capacity: usize,
    },
    /// A snapshot attempts to bind an OM UAV in an RTV-occupied slot.
    ConflictingOutputSlots {
        /// Inferred number of active RTV slots.
        render_target_count: usize,
        /// Conflicting UAV slot.
        unordered_access_slot: usize,
    },
    /// A shader getter reported a null class instance inside its active range.
    NullClassInstance {
        /// Shader stage.
        stage: &'static str,
        /// Null entry index.
        index: usize,
    },
}

impl fmt::Display for WindowsBackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullComPointer => formatter.write_str("the COM pointer was null"),
            Self::QueryInterface(error) => {
                write!(formatter, "ID3D11DeviceContext query failed: {error}")
            }
            Self::ReportedCountOutOfRange {
                section,
                reported,
                capacity,
            } => write!(
                formatter,
                "{section} reported {reported} entries, exceeding capacity {capacity}"
            ),
            Self::SnapshotCountOutOfRange {
                section,
                count,
                capacity,
            } => write!(
                formatter,
                "{section} snapshot has {count} entries, exceeding capacity {capacity}"
            ),
            Self::ConflictingOutputSlots {
                render_target_count,
                unordered_access_slot,
            } => write!(
                formatter,
                "OM UAV slot {unordered_access_slot} overlaps {render_target_count} active RTV slots"
            ),
            Self::NullClassInstance { stage, index } => {
                write!(formatter, "{stage} class instance {index} was null")
            }
        }
    }
}

impl std::error::Error for WindowsBackendError {}

impl From<HResultError> for WindowsBackendError {
    fn from(error: HResultError) -> Self {
        Self::QueryInterface(error)
    }
}

/// Rust-owned critical D3D11 state backend.
///
/// This backend deliberately implements only [`CriticalPipelineBackend`].
/// Until the exhaustive capability is implemented, it cannot construct a
/// [`crate::FullStateGuard`].
pub struct WindowsD3d11Backend {
    context: OwnedComObject,
    raw_context: NonNull<RawDeviceContext>,
}

impl WindowsD3d11Backend {
    /// Query and own an `ID3D11DeviceContext` from a borrowed COM interface.
    ///
    /// # Safety
    ///
    /// A non-null `unknown` must identify a live IUnknown-compatible COM
    /// interface for the duration of this call.
    ///
    /// # Errors
    ///
    /// Returns an error for null input, a failing HRESULT, or a successful
    /// query that produces no interface pointer.
    pub unsafe fn from_unknown_borrowed(unknown: *mut c_void) -> Result<Self, WindowsBackendError> {
        // SAFETY: The caller guarantees a live IUnknown-compatible pointer.
        let borrowed = unsafe { OwnedComObject::from_raw_borrowed(unknown) }
            .ok_or(WindowsBackendError::NullComPointer)?;
        let context = borrowed.query_interface(&IID_ID3D11_DEVICE_CONTEXT)?;
        drop(borrowed);
        let raw_context = NonNull::new(context.as_raw().cast::<RawDeviceContext>())
            .ok_or(WindowsBackendError::NullComPointer)?;
        Ok(Self {
            context,
            raw_context,
        })
    }

    /// Borrow the type-erased context pointer for direct rendering calls.
    #[must_use]
    pub fn context_raw(&self) -> *mut c_void {
        self.context.as_raw()
    }

    fn context(&self) -> *mut RawDeviceContext {
        self.raw_context.as_ptr()
    }

    fn vtable(&self) -> &RawDeviceContextVTable {
        // SAFETY: Construction succeeds only after QueryInterface returned an
        // owned ID3D11DeviceContext pointer with its documented vtable.
        unsafe { &*(*self.context()).vtable }
    }

    fn capture_stage(
        &self,
        stage: ShaderStage,
    ) -> Result<ProgrammableStageState<OwnedComObject>, WindowsBackendError> {
        let vtable = self.vtable();
        let (get_constant_buffers, get_shader_resources, get_shader, get_samplers) = match stage {
            ShaderStage::Vertex => (
                vtable.vs_get_constant_buffers,
                vtable.vs_get_shader_resources,
                vtable.vs_get_shader,
                vtable.vs_get_samplers,
            ),
            ShaderStage::Pixel => (
                vtable.ps_get_constant_buffers,
                vtable.ps_get_shader_resources,
                vtable.ps_get_shader,
                vtable.ps_get_samplers,
            ),
        };

        let mut constant_buffers = [std::ptr::null_mut(); COMMONSHADER_CONSTANT_BUFFER_SLOTS];
        let mut shader_resources = [std::ptr::null_mut(); COMMONSHADER_INPUT_RESOURCE_SLOTS];
        let mut samplers = [std::ptr::null_mut(); COMMONSHADER_SAMPLER_SLOTS];
        let mut shader = std::ptr::null_mut();
        let mut class_instances = [std::ptr::null_mut(); SHADER_MAX_CLASS_INSTANCES];
        let mut class_instance_count = SHADER_MAX_CLASS_INSTANCES as u32;

        // SAFETY: All arrays have the exact SDK slot count and writable
        // storage. D3D11 getters AddRef every non-null returned interface.
        unsafe {
            get_constant_buffers(
                self.context(),
                0,
                COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                constant_buffers.as_mut_ptr(),
            );
            get_shader_resources(
                self.context(),
                0,
                COMMONSHADER_INPUT_RESOURCE_SLOTS as u32,
                shader_resources.as_mut_ptr(),
            );
            get_samplers(
                self.context(),
                0,
                COMMONSHADER_SAMPLER_SLOTS as u32,
                samplers.as_mut_ptr(),
            );
            get_shader(
                self.context(),
                &mut shader,
                class_instances.as_mut_ptr(),
                &mut class_instance_count,
            );
        }

        // Convert every getter-owned pointer before any fallible validation so
        // every early-return path releases every acquired COM reference.
        // SAFETY: The getters returned owned nullable references.
        let shader = unsafe { OwnedComObject::from_raw_owned(shader) };
        // SAFETY: Each getter returned owned nullable references.
        let constant_buffers = unsafe { own_array(constant_buffers) };
        // SAFETY: Each getter returned owned nullable references.
        let shader_resources = unsafe { own_array(shader_resources) };
        // SAFETY: Each getter returned owned nullable references.
        let samplers = unsafe { own_array(samplers) };
        // SAFETY: Each getter returned owned nullable references.
        let mut class_instances = unsafe { own_array(class_instances) };

        let class_instance_count = checked_count(
            stage.name(),
            class_instance_count,
            SHADER_MAX_CLASS_INSTANCES,
        )?;
        let mut owned_classes = Vec::with_capacity(class_instance_count);
        for (index, class_instance) in class_instances
            .iter_mut()
            .take(class_instance_count)
            .enumerate()
        {
            let class_instance =
                class_instance
                    .take()
                    .ok_or(WindowsBackendError::NullClassInstance {
                        stage: stage.name(),
                        index,
                    })?;
            owned_classes.push(class_instance);
        }

        Ok(ProgrammableStageState {
            shader,
            class_instances: owned_classes,
            constant_buffers,
            shader_resources,
            samplers,
        })
    }

    fn restore_stage(
        &self,
        stage: ShaderStage,
        state: &ProgrammableStageState<OwnedComObject>,
    ) -> Result<(), WindowsBackendError> {
        let class_instance_count = checked_snapshot_count(
            stage.name(),
            state.class_instances.len(),
            SHADER_MAX_CLASS_INSTANCES,
        )?;
        let vtable = self.vtable();
        let (set_constant_buffers, set_shader_resources, set_shader, set_samplers) = match stage {
            ShaderStage::Vertex => (
                vtable.vs_set_constant_buffers,
                vtable.vs_set_shader_resources,
                vtable.vs_set_shader,
                vtable.vs_set_samplers,
            ),
            ShaderStage::Pixel => (
                vtable.ps_set_constant_buffers,
                vtable.ps_set_shader_resources,
                vtable.ps_set_shader,
                vtable.ps_set_samplers,
            ),
        };
        let constant_buffers = raw_array(&state.constant_buffers);
        let shader_resources = raw_array(&state.shader_resources);
        let samplers = raw_array(&state.samplers);
        let class_instances: Vec<_> = state
            .class_instances
            .iter()
            .map(OwnedComObject::as_raw)
            .collect();
        let shader = raw_optional(&state.shader);

        // SAFETY: Snapshot arrays have exact SDK slot counts. Every raw
        // pointer remains alive through the call because the snapshot owns it.
        unsafe {
            set_shader(
                self.context(),
                shader,
                slice_pointer(&class_instances),
                class_instance_count,
            );
            set_constant_buffers(
                self.context(),
                0,
                COMMONSHADER_CONSTANT_BUFFER_SLOTS as u32,
                constant_buffers.as_ptr(),
            );
            set_shader_resources(
                self.context(),
                0,
                COMMONSHADER_INPUT_RESOURCE_SLOTS as u32,
                shader_resources.as_ptr(),
            );
            set_samplers(
                self.context(),
                0,
                COMMONSHADER_SAMPLER_SLOTS as u32,
                samplers.as_ptr(),
            );
        }
        Ok(())
    }
}

impl CriticalPipelineBackend for WindowsD3d11Backend {
    type Handle = OwnedComObject;
    type Error = WindowsBackendError;

    fn capture_output_merger(&mut self) -> Result<OutputMergerState<Self::Handle>, Self::Error> {
        let mut render_targets = [std::ptr::null_mut(); OM_RENDER_TARGET_SLOTS];
        let mut depth_stencil_view = std::ptr::null_mut();
        let mut unordered_access_views = [std::ptr::null_mut(); PS_CS_UAV_SLOTS];
        let mut blend_state = std::ptr::null_mut();
        let mut blend_factor = [0.0; 4];
        let mut sample_mask = 0;
        let mut depth_stencil_state = std::ptr::null_mut();
        let mut stencil_reference = 0;

        // SAFETY: Output arrays have the exact SDK capacities. Getters AddRef
        // every returned non-null interface. The UAV-only getter call uses
        // zero RTVs so all eight UAV slots can be observed without overlap.
        unsafe {
            (self.vtable().om_get_render_targets)(
                self.context(),
                OM_RENDER_TARGET_SLOTS as u32,
                render_targets.as_mut_ptr(),
                &mut depth_stencil_view,
            );
            (self.vtable().om_get_render_targets_and_uavs)(
                self.context(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                PS_CS_UAV_SLOTS as u32,
                unordered_access_views.as_mut_ptr(),
            );
            (self.vtable().om_get_blend_state)(
                self.context(),
                &mut blend_state,
                blend_factor.as_mut_ptr(),
                &mut sample_mask,
            );
            (self.vtable().om_get_depth_stencil_state)(
                self.context(),
                &mut depth_stencil_state,
                &mut stencil_reference,
            );
        }

        Ok(OutputMergerState {
            // SAFETY: Each getter returned owned nullable references.
            render_targets: unsafe { own_array(render_targets) },
            // SAFETY: The getter returned an owned nullable reference.
            depth_stencil_view: unsafe { OwnedComObject::from_raw_owned(depth_stencil_view) },
            // SAFETY: Each getter returned owned nullable references.
            unordered_access_views: unsafe { own_array(unordered_access_views) },
            unordered_access_counters: HiddenCounterState::Preserve,
            // SAFETY: The getter returned an owned nullable reference.
            blend_state: unsafe { OwnedComObject::from_raw_owned(blend_state) },
            blend_factor,
            sample_mask,
            // SAFETY: The getter returned an owned nullable reference.
            depth_stencil_state: unsafe { OwnedComObject::from_raw_owned(depth_stencil_state) },
            stencil_reference,
        })
    }

    fn capture_input_assembler(
        &mut self,
    ) -> Result<InputAssemblerState<Self::Handle>, Self::Error> {
        let mut input_layout = std::ptr::null_mut();
        let mut vertex_buffers = [std::ptr::null_mut(); IA_VERTEX_BUFFER_SLOTS];
        let mut strides = [0; IA_VERTEX_BUFFER_SLOTS];
        let mut offsets = [0; IA_VERTEX_BUFFER_SLOTS];
        let mut index_buffer = std::ptr::null_mut();
        let mut index_format = 0;
        let mut index_offset = 0;
        let mut primitive_topology = 0;

        // SAFETY: Arrays have exact SDK capacities and all scalar outputs are
        // valid writable storage. Getters AddRef returned interfaces.
        unsafe {
            (self.vtable().ia_get_input_layout)(self.context(), &mut input_layout);
            (self.vtable().ia_get_vertex_buffers)(
                self.context(),
                0,
                IA_VERTEX_BUFFER_SLOTS as u32,
                vertex_buffers.as_mut_ptr(),
                strides.as_mut_ptr(),
                offsets.as_mut_ptr(),
            );
            (self.vtable().ia_get_index_buffer)(
                self.context(),
                &mut index_buffer,
                &mut index_format,
                &mut index_offset,
            );
            (self.vtable().ia_get_primitive_topology)(self.context(), &mut primitive_topology);
        }

        // SAFETY: The getter returned owned nullable references.
        let mut owned_vertex_buffers = unsafe { own_array(vertex_buffers) };
        let vertex_buffers = std::array::from_fn(|index| VertexBufferBinding {
            buffer: owned_vertex_buffers[index].take(),
            stride: strides[index],
            offset: offsets[index],
        });

        Ok(InputAssemblerState {
            // SAFETY: The getter returned an owned nullable reference.
            input_layout: unsafe { OwnedComObject::from_raw_owned(input_layout) },
            vertex_buffers,
            index_buffer: IndexBufferBinding {
                // SAFETY: The getter returned an owned nullable reference.
                buffer: unsafe { OwnedComObject::from_raw_owned(index_buffer) },
                format: index_format,
                offset: index_offset,
            },
            primitive_topology,
        })
    }

    fn capture_rasterizer(&mut self) -> Result<RasterizerState<Self::Handle>, Self::Error> {
        let mut state = std::ptr::null_mut();
        let mut viewport_count = 0;
        let mut scissor_count = 0;

        // SAFETY: Null arrays request counts only; scalar outputs are valid.
        unsafe {
            (self.vtable().rs_get_state)(self.context(), &mut state);
            (self.vtable().rs_get_viewports)(
                self.context(),
                &mut viewport_count,
                std::ptr::null_mut(),
            );
            (self.vtable().rs_get_scissor_rects)(
                self.context(),
                &mut scissor_count,
                std::ptr::null_mut(),
            );
        }
        // Establish ownership before fallible count validation.
        // SAFETY: The getter returned an owned nullable reference.
        let state = unsafe { OwnedComObject::from_raw_owned(state) };
        let viewport_count = checked_count("viewports", viewport_count, RASTERIZER_MAX_RECTS)?;
        let scissor_count =
            checked_count("scissor rectangles", scissor_count, RASTERIZER_MAX_RECTS)?;
        let mut viewports = [Viewport::default(); RASTERIZER_MAX_RECTS];
        let mut scissor_rects = [Rect::default(); RASTERIZER_MAX_RECTS];
        let mut viewport_capacity = viewport_count as u32;
        let mut scissor_capacity = scissor_count as u32;

        // SAFETY: Counts were validated against each writable array capacity.
        unsafe {
            (self.vtable().rs_get_viewports)(
                self.context(),
                &mut viewport_capacity,
                viewports.as_mut_ptr(),
            );
            (self.vtable().rs_get_scissor_rects)(
                self.context(),
                &mut scissor_capacity,
                scissor_rects.as_mut_ptr(),
            );
        }
        let final_viewport_count =
            checked_count("viewports", viewport_capacity, RASTERIZER_MAX_RECTS)?;
        let final_scissor_count =
            checked_count("scissor rectangles", scissor_capacity, RASTERIZER_MAX_RECTS)?;

        Ok(RasterizerState {
            state,
            viewports: viewports[..final_viewport_count].to_vec(),
            scissor_rects: scissor_rects[..final_scissor_count].to_vec(),
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
        if let Some(unordered_access_slot) = state.unordered_access_views[..render_target_count]
            .iter()
            .position(Option::is_some)
        {
            return Err(WindowsBackendError::ConflictingOutputSlots {
                render_target_count,
                unordered_access_slot,
            });
        }

        let render_targets = raw_array(&state.render_targets);
        let unordered_access_views = raw_array(&state.unordered_access_views);
        let initial_counts = [KEEP_UAV_COUNTER; PS_CS_UAV_SLOTS];
        let unordered_access_start = render_target_count;
        let unordered_access_count = PS_CS_UAV_SLOTS - unordered_access_start;

        // SAFETY: Snapshot pointers remain alive throughout these calls. RTVs,
        // DSV, and UAVs are restored in one call because D3D11 shares their
        // output slots and does not permit independent binding.
        unsafe {
            (self.vtable().om_set_render_targets_and_uavs)(
                self.context(),
                render_target_count as u32,
                slice_pointer(&render_targets[..render_target_count]),
                raw_optional(&state.depth_stencil_view),
                unordered_access_start as u32,
                unordered_access_count as u32,
                slice_pointer(&unordered_access_views[unordered_access_start..]),
                slice_pointer(&initial_counts[unordered_access_start..]),
            );
            (self.vtable().om_set_blend_state)(
                self.context(),
                raw_optional(&state.blend_state),
                state.blend_factor.as_ptr(),
                state.sample_mask,
            );
            (self.vtable().om_set_depth_stencil_state)(
                self.context(),
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
        let vertex_buffers: [*mut c_void; IA_VERTEX_BUFFER_SLOTS] =
            std::array::from_fn(|index| raw_optional(&state.vertex_buffers[index].buffer));
        let strides: [u32; IA_VERTEX_BUFFER_SLOTS] =
            std::array::from_fn(|index| state.vertex_buffers[index].stride);
        let offsets: [u32; IA_VERTEX_BUFFER_SLOTS] =
            std::array::from_fn(|index| state.vertex_buffers[index].offset);

        // SAFETY: All snapshot pointers remain alive through the calls and
        // arrays have exact SDK slot counts.
        unsafe {
            (self.vtable().ia_set_input_layout)(self.context(), raw_optional(&state.input_layout));
            (self.vtable().ia_set_vertex_buffers)(
                self.context(),
                0,
                IA_VERTEX_BUFFER_SLOTS as u32,
                vertex_buffers.as_ptr(),
                strides.as_ptr(),
                offsets.as_ptr(),
            );
            (self.vtable().ia_set_index_buffer)(
                self.context(),
                raw_optional(&state.index_buffer.buffer),
                state.index_buffer.format,
                state.index_buffer.offset,
            );
            (self.vtable().ia_set_primitive_topology)(self.context(), state.primitive_topology);
        }
        Ok(())
    }

    fn restore_rasterizer(
        &mut self,
        state: &RasterizerState<Self::Handle>,
    ) -> Result<(), Self::Error> {
        let viewport_count =
            checked_snapshot_count("viewports", state.viewports.len(), RASTERIZER_MAX_RECTS)?;
        let scissor_count = checked_snapshot_count(
            "scissor rectangles",
            state.scissor_rects.len(),
            RASTERIZER_MAX_RECTS,
        )?;

        // SAFETY: Counts were validated against SDK limits. Snapshot pointers and
        // slices remain alive through each call.
        unsafe {
            (self.vtable().rs_set_state)(self.context(), raw_optional(&state.state));
            (self.vtable().rs_set_viewports)(
                self.context(),
                viewport_count,
                slice_pointer(&state.viewports),
            );
            (self.vtable().rs_set_scissor_rects)(
                self.context(),
                scissor_count,
                slice_pointer(&state.scissor_rects),
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
}

#[derive(Clone, Copy)]
enum ShaderStage {
    Vertex,
    Pixel,
}

impl ShaderStage {
    const fn name(self) -> &'static str {
        match self {
            Self::Vertex => "vertex shader",
            Self::Pixel => "pixel shader",
        }
    }
}

fn last_bound_slot<const N: usize, T>(slots: &[Option<T>; N]) -> usize {
    slots
        .iter()
        .rposition(Option::is_some)
        .map_or(0, |index| index + 1)
}

fn checked_snapshot_count(
    section: &'static str,
    count: usize,
    capacity: usize,
) -> Result<u32, WindowsBackendError> {
    if count > capacity {
        Err(WindowsBackendError::SnapshotCountOutOfRange {
            section,
            count,
            capacity,
        })
    } else {
        Ok(count as u32)
    }
}
fn checked_count(
    section: &'static str,
    reported: u32,
    capacity: usize,
) -> Result<usize, WindowsBackendError> {
    let reported_usize = reported as usize;
    if reported_usize > capacity {
        Err(WindowsBackendError::ReportedCountOutOfRange {
            section,
            reported,
            capacity,
        })
    } else {
        Ok(reported_usize)
    }
}

fn raw_optional(object: &Option<OwnedComObject>) -> *mut c_void {
    object
        .as_ref()
        .map_or(std::ptr::null_mut(), OwnedComObject::as_raw)
}

fn raw_array<const N: usize>(objects: &[Option<OwnedComObject>; N]) -> [*mut c_void; N] {
    std::array::from_fn(|index| raw_optional(&objects[index]))
}

fn slice_pointer<T>(slice: &[T]) -> *const T {
    if slice.is_empty() {
        std::ptr::null()
    } else {
        slice.as_ptr()
    }
}

unsafe fn own_array<const N: usize>(pointers: [*mut c_void; N]) -> [Option<OwnedComObject>; N] {
    pointers.map(|pointer| {
        // SAFETY: The caller guarantees every non-null pointer owns one COM
        // reference returned by a D3D11 getter.
        unsafe { OwnedComObject::from_raw_owned(pointer) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_validation_preserves_reported_values() {
        let error = checked_count("viewports", 17, 16)
            .expect_err("a count above the SDK maximum must fail");
        assert_eq!(
            error,
            WindowsBackendError::ReportedCountOutOfRange {
                section: "viewports",
                reported: 17,
                capacity: 16,
            }
        );
    }

    #[test]
    fn count_validation_accepts_capacity_boundary() {
        assert_eq!(checked_count("viewports", 16, 16), Ok(16));
    }

    #[test]
    fn last_bound_slot_uses_highest_non_null_entry() {
        let slots = [None, Some(()), None, Some(()), None];
        assert_eq!(last_bound_slot(&slots), 4);
        assert_eq!(last_bound_slot(&[None::<()>; 5]), 0);
    }

    #[test]
    fn snapshot_count_validation_distinguishes_caller_data() {
        assert_eq!(
            checked_snapshot_count("class instances", 254, 253),
            Err(WindowsBackendError::SnapshotCountOutOfRange {
                section: "class instances",
                count: 254,
                capacity: 253,
            })
        );
    }
}
