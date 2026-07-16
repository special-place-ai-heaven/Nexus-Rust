//! Minimal exact D3D11 C ABI missing from `windows-sys` 0.61.2.
//!
//! `windows-sys` 0.61.2 exposes `GUID`, `HRESULT`, and `IUnknown_Vtbl`, but no
//! Win32 Direct3D11 module. This file isolates the required SDK ABI and asserts
//! every critical method's documented vtable slot.

use std::ffi::c_void;
use std::mem::offset_of;

use windows_sys::core::{GUID, IUnknown_Vtbl};

use crate::model::{Rect, Viewport};

pub(crate) const IID_ID3D11_DEVICE_CONTEXT: GUID =
    GUID::from_u128(0xc0bfa96c_e089_44fb_8eaf_26f8796190da);

#[repr(C)]
pub(crate) struct RawDeviceContext {
    pub(crate) vtable: *const RawDeviceContextVTable,
}

type SetObjectsFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    start_slot: u32,
    object_count: u32,
    objects: *const *mut c_void,
);
type GetObjectsFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    start_slot: u32,
    object_count: u32,
    objects: *mut *mut c_void,
);
type SetShaderFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    shader: *mut c_void,
    class_instances: *const *mut c_void,
    class_instance_count: u32,
);
type GetShaderFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    shader: *mut *mut c_void,
    class_instances: *mut *mut c_void,
    class_instance_count: *mut u32,
);
type SetOneFn = unsafe extern "system" fn(this: *mut RawDeviceContext, object: *mut c_void);
type GetOneFn = unsafe extern "system" fn(this: *mut RawDeviceContext, object: *mut *mut c_void);
type IaSetVertexBuffersFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    start_slot: u32,
    buffer_count: u32,
    buffers: *const *mut c_void,
    strides: *const u32,
    offsets: *const u32,
);
type IaGetVertexBuffersFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    start_slot: u32,
    buffer_count: u32,
    buffers: *mut *mut c_void,
    strides: *mut u32,
    offsets: *mut u32,
);
type IaSetIndexBufferFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    buffer: *mut c_void,
    format: u32,
    offset: u32,
);
type IaGetIndexBufferFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    buffer: *mut *mut c_void,
    format: *mut u32,
    offset: *mut u32,
);
type IaSetPrimitiveTopologyFn =
    unsafe extern "system" fn(this: *mut RawDeviceContext, topology: u32);
type IaGetPrimitiveTopologyFn =
    unsafe extern "system" fn(this: *mut RawDeviceContext, topology: *mut u32);
type OmSetRenderTargetsFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    render_target_count: u32,
    render_targets: *const *mut c_void,
    depth_stencil_view: *mut c_void,
);
type OmGetRenderTargetsFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    render_target_count: u32,
    render_targets: *mut *mut c_void,
    depth_stencil_view: *mut *mut c_void,
);
type OmSetRenderTargetsAndUavsFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    render_target_count: u32,
    render_targets: *const *mut c_void,
    depth_stencil_view: *mut c_void,
    uav_start_slot: u32,
    uav_count: u32,
    uavs: *const *mut c_void,
    uav_initial_counts: *const u32,
);
type OmGetRenderTargetsAndUavsFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    render_target_count: u32,
    render_targets: *mut *mut c_void,
    depth_stencil_view: *mut *mut c_void,
    uav_start_slot: u32,
    uav_count: u32,
    uavs: *mut *mut c_void,
);
type OmSetBlendStateFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    blend_state: *mut c_void,
    blend_factor: *const f32,
    sample_mask: u32,
);
type OmGetBlendStateFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    blend_state: *mut *mut c_void,
    blend_factor: *mut f32,
    sample_mask: *mut u32,
);
type OmSetDepthStencilStateFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    depth_stencil_state: *mut c_void,
    stencil_reference: u32,
);
type OmGetDepthStencilStateFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    depth_stencil_state: *mut *mut c_void,
    stencil_reference: *mut u32,
);
type RsSetViewportsFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    viewport_count: u32,
    viewports: *const Viewport,
);
type RsGetViewportsFn = unsafe extern "system" fn(
    this: *mut RawDeviceContext,
    viewport_count: *mut u32,
    viewports: *mut Viewport,
);
type RsSetScissorRectsFn =
    unsafe extern "system" fn(this: *mut RawDeviceContext, rect_count: u32, rects: *const Rect);
type RsGetScissorRectsFn =
    unsafe extern "system" fn(this: *mut RawDeviceContext, rect_count: *mut u32, rects: *mut Rect);

/// Exact 115-entry `ID3D11DeviceContext` vtable through Windows 11 SDK
/// 10.0.26100.0. Unused methods retain pointer-sized slots; called methods
/// have their exact C signatures.
#[repr(C)]
pub(crate) struct RawDeviceContextVTable {
    pub(crate) iunknown: IUnknown_Vtbl,
    _device_child_3_6: [usize; 4],
    pub(crate) vs_set_constant_buffers: SetObjectsFn,
    pub(crate) ps_set_shader_resources: SetObjectsFn,
    pub(crate) ps_set_shader: SetShaderFn,
    pub(crate) ps_set_samplers: SetObjectsFn,
    pub(crate) vs_set_shader: SetShaderFn,
    _reserved_12_15: [usize; 4],
    pub(crate) ps_set_constant_buffers: SetObjectsFn,
    pub(crate) ia_set_input_layout: SetOneFn,
    pub(crate) ia_set_vertex_buffers: IaSetVertexBuffersFn,
    pub(crate) ia_set_index_buffer: IaSetIndexBufferFn,
    _reserved_20_23: [usize; 4],
    pub(crate) ia_set_primitive_topology: IaSetPrimitiveTopologyFn,
    pub(crate) vs_set_shader_resources: SetObjectsFn,
    pub(crate) vs_set_samplers: SetObjectsFn,
    _reserved_27_32: [usize; 6],
    pub(crate) om_set_render_targets: OmSetRenderTargetsFn,
    pub(crate) om_set_render_targets_and_uavs: OmSetRenderTargetsAndUavsFn,
    pub(crate) om_set_blend_state: OmSetBlendStateFn,
    pub(crate) om_set_depth_stencil_state: OmSetDepthStencilStateFn,
    _reserved_37_42: [usize; 6],
    pub(crate) rs_set_state: SetOneFn,
    pub(crate) rs_set_viewports: RsSetViewportsFn,
    pub(crate) rs_set_scissor_rects: RsSetScissorRectsFn,
    _reserved_46_71: [usize; 26],
    pub(crate) vs_get_constant_buffers: GetObjectsFn,
    pub(crate) ps_get_shader_resources: GetObjectsFn,
    pub(crate) ps_get_shader: GetShaderFn,
    pub(crate) ps_get_samplers: GetObjectsFn,
    pub(crate) vs_get_shader: GetShaderFn,
    pub(crate) ps_get_constant_buffers: GetObjectsFn,
    pub(crate) ia_get_input_layout: GetOneFn,
    pub(crate) ia_get_vertex_buffers: IaGetVertexBuffersFn,
    pub(crate) ia_get_index_buffer: IaGetIndexBufferFn,
    _reserved_81_82: [usize; 2],
    pub(crate) ia_get_primitive_topology: IaGetPrimitiveTopologyFn,
    pub(crate) vs_get_shader_resources: GetObjectsFn,
    pub(crate) vs_get_samplers: GetObjectsFn,
    _reserved_86_88: [usize; 3],
    pub(crate) om_get_render_targets: OmGetRenderTargetsFn,
    pub(crate) om_get_render_targets_and_uavs: OmGetRenderTargetsAndUavsFn,
    pub(crate) om_get_blend_state: OmGetBlendStateFn,
    pub(crate) om_get_depth_stencil_state: OmGetDepthStencilStateFn,
    _reserved_93: [usize; 1],
    pub(crate) rs_get_state: GetOneFn,
    pub(crate) rs_get_viewports: RsGetViewportsFn,
    pub(crate) rs_get_scissor_rects: RsGetScissorRectsFn,
    _reserved_97_114: [usize; 18],
}

const POINTER_SIZE: usize = size_of::<usize>();
const _: () = assert!(size_of::<RawDeviceContextVTable>() == 115 * POINTER_SIZE);

macro_rules! assert_slot {
    ($field:ident, $slot:literal) => {
        const _: () = assert!(offset_of!(RawDeviceContextVTable, $field) == $slot * POINTER_SIZE);
    };
}

assert_slot!(iunknown, 0);
assert_slot!(vs_set_constant_buffers, 7);
assert_slot!(ps_set_shader_resources, 8);
assert_slot!(ps_set_shader, 9);
assert_slot!(ps_set_samplers, 10);
assert_slot!(vs_set_shader, 11);
assert_slot!(ps_set_constant_buffers, 16);
assert_slot!(ia_set_input_layout, 17);
assert_slot!(ia_set_vertex_buffers, 18);
assert_slot!(ia_set_index_buffer, 19);
assert_slot!(ia_set_primitive_topology, 24);
assert_slot!(vs_set_shader_resources, 25);
assert_slot!(vs_set_samplers, 26);
assert_slot!(om_set_render_targets, 33);
assert_slot!(om_set_render_targets_and_uavs, 34);
assert_slot!(om_set_blend_state, 35);
assert_slot!(om_set_depth_stencil_state, 36);
assert_slot!(rs_set_state, 43);
assert_slot!(rs_set_viewports, 44);
assert_slot!(rs_set_scissor_rects, 45);
assert_slot!(vs_get_constant_buffers, 72);
assert_slot!(ps_get_shader_resources, 73);
assert_slot!(ps_get_shader, 74);
assert_slot!(ps_get_samplers, 75);
assert_slot!(vs_get_shader, 76);
assert_slot!(ps_get_constant_buffers, 77);
assert_slot!(ia_get_input_layout, 78);
assert_slot!(ia_get_vertex_buffers, 79);
assert_slot!(ia_get_index_buffer, 80);
assert_slot!(ia_get_primitive_topology, 83);
assert_slot!(vs_get_shader_resources, 84);
assert_slot!(vs_get_samplers, 85);
assert_slot!(om_get_render_targets, 89);
assert_slot!(om_get_render_targets_and_uavs, 90);
assert_slot!(om_get_blend_state, 91);
assert_slot!(om_get_depth_stencil_state, 92);
assert_slot!(rs_get_state, 94);
assert_slot!(rs_get_viewports, 95);
assert_slot!(rs_get_scissor_rects, 96);
