//! D3D11 resource ownership and draw submission.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::mem::size_of;
use std::ptr::NonNull;
use std::rc::Rc;
use std::slice;

use nexus_imgui_compat::sys;
use nexus_imgui_runtime::{DrawData, FrameConfig, ImGuiContextOwner};
use nexus_render_d3d11::{ExhaustivePipelineBackend, FullStateGuard};
use windows::Win32::Foundation::{E_POINTER, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::{
    D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob, ID3DInclude,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_INDEX_BUFFER, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BIND_VERTEX_BUFFER, D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE,
    D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA, D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL,
    D3D11_COMPARISON_ALWAYS, D3D11_CPU_ACCESS_WRITE, D3D11_CULL_NONE, D3D11_DEPTH_STENCIL_DESC,
    D3D11_DEPTH_WRITE_MASK_ZERO, D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
    D3D11_INPUT_ELEMENT_DESC, D3D11_INPUT_PER_VERTEX_DATA, D3D11_MAP_WRITE_DISCARD,
    D3D11_MAPPED_SUBRESOURCE, D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_BLEND_DESC,
    D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE_ADDRESS_WRAP, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT, ID3D11BlendState, ID3D11Buffer,
    ID3D11DepthStencilState, ID3D11Device, ID3D11DeviceContext, ID3D11InputLayout,
    ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R32_UINT,
    DXGI_FORMAT_R32G32_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;
use windows::core::{BOOL, Interface, PCSTR};

use crate::error::{GpuOperation, RendererError, ShaderKind};
use crate::plan::{
    CommandKind, FrameGeometry, TranslatedCommand, translate_command, validate_geometry,
    validate_totals,
};
use crate::state::ExhaustiveWindowsBackend;

const INITIAL_VERTEX_CAPACITY: usize = 5_000;
const INITIAL_INDEX_CAPACITY: usize = 10_000;
const VERTEX_GROWTH_SLACK: usize = 5_000;
const INDEX_GROWTH_SLACK: usize = 10_000;

const VERTEX_SHADER_SOURCE: &[u8] = br#"
cbuffer vertexBuffer : register(b0)
{
    float4x4 ProjectionMatrix;
};
struct VS_INPUT
{
    float2 pos : POSITION;
    float2 uv  : TEXCOORD0;
    float4 col : COLOR0;
};
struct PS_INPUT
{
    float4 pos : SV_POSITION;
    float4 col : COLOR0;
    float2 uv  : TEXCOORD0;
};
PS_INPUT main(VS_INPUT input)
{
    PS_INPUT output;
    output.pos = mul(ProjectionMatrix, float4(input.pos.xy, 0.f, 1.f));
    output.col = input.col;
    output.uv = input.uv;
    return output;
}
"#;

const PIXEL_SHADER_SOURCE: &[u8] = br#"
struct PS_INPUT
{
    float4 pos : SV_POSITION;
    float4 col : COLOR0;
    float2 uv  : TEXCOORD0;
};
sampler sampler0 : register(s0);
Texture2D texture0 : register(t0);
float4 main(PS_INPUT input) : SV_Target
{
    return input.col * texture0.Sample(sampler0, input.uv);
}
"#;

/// Per-frame draw statistics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenderStats {
    /// Number of indexed draw calls submitted.
    pub draw_calls: u32,
    /// Number of indices submitted across all draw calls.
    pub elements: u64,
    /// Number of native user callbacks invoked.
    pub user_callbacks: u32,
    /// Number of reset-render-state callbacks honored.
    pub reset_state_callbacks: u32,
}

/// Attached, thread-bound Dear ImGui/D3D11 rendering session.
///
/// The session owns the native-compatible ImGui context so the font atlas'
/// `TexID` cannot outlive its shader-resource view. Add-ons receive
/// [`Self::context_ptr`] but must not destroy or retain it past this session.
pub struct D3d11Renderer {
    context_owner: ImGuiContextOwner,
    gpu: GpuRenderer,
    _thread_bound: PhantomData<Rc<()>>,
}

impl D3d11Renderer {
    /// Attaches to a swap chain and creates the process' Rust-owned ImGui
    /// context.
    ///
    /// # Errors
    ///
    /// Returns a closed context, DXGI, D3D11, shader, or font-atlas error.
    pub fn attach(swap_chain: IDXGISwapChain, generation: u64) -> Result<Self, RendererError> {
        let owner = ImGuiContextOwner::create()?;
        Self::attach_with_context(swap_chain, generation, owner)
    }

    /// Attaches using an already-created Rust-owned ImGui context, transferring
    /// its ownership into the renderer session.
    ///
    /// # Errors
    ///
    /// Returns a closed DXGI, D3D11, shader, or font-atlas error.
    pub fn attach_with_context(
        swap_chain: IDXGISwapChain,
        generation: u64,
        mut context_owner: ImGuiContextOwner,
    ) -> Result<Self, RendererError> {
        let gpu = GpuRenderer::attach(swap_chain, generation, &mut context_owner)?;
        context_owner.set_renderer_ready(true)?;
        Ok(Self {
            context_owner,
            gpu,
            _thread_bound: PhantomData,
        })
    }

    /// Acquires a swap-chain reference from a borrowed raw COM pointer and
    /// attaches a new renderer session.
    ///
    /// # Safety
    ///
    /// `raw` must be a live `IDXGISwapChain*` for this call. The renderer
    /// acquires its own reference before returning.
    ///
    /// # Errors
    ///
    /// Returns a closed pointer, context, DXGI, D3D11, shader, or font error.
    pub unsafe fn attach_raw_borrowed(
        raw: *mut c_void,
        generation: u64,
    ) -> Result<Self, RendererError> {
        // SAFETY: the caller guarantees the exact live COM interface type.
        let swap_chain = unsafe { IDXGISwapChain::from_raw_borrowed(&raw) }
            .cloned()
            .ok_or(RendererError::NullPointer("swap-chain"))?;
        Self::attach(swap_chain, generation)
    }

    /// Returns the exact native context pointer passed to existing add-ons.
    #[must_use]
    pub fn context_ptr(&self) -> *mut sys::ImGuiContext {
        self.context_owner.as_ptr()
    }

    /// Borrows the selected swap-chain interface for synchronous service
    /// attachment.
    ///
    /// Consumers that retain this interface past the current call must acquire
    /// their own COM reference.
    #[must_use]
    pub fn swap_chain_ptr(&self) -> NonNull<c_void> {
        // SAFETY: a `windows` COM interface is always represented by a non-null
        // pointer, and `GpuRenderer` owns this reference for `self`'s lifetime.
        unsafe { NonNull::new_unchecked(self.gpu.swap_chain.as_raw()) }
    }

    /// Borrows the renderer's D3D11 device for synchronous service attachment.
    ///
    /// Consumers that retain this interface past the current call must acquire
    /// their own COM reference.
    #[must_use]
    pub fn device_ptr(&self) -> NonNull<c_void> {
        // SAFETY: a `windows` COM interface is always represented by a non-null
        // pointer, and `GpuRenderer` owns this reference for `self`'s lifetime.
        unsafe { NonNull::new_unchecked(self.gpu.device.as_raw()) }
    }

    /// Runs initialization or shutdown work with this ImGui context current.
    pub fn with_current_context<R>(
        &mut self,
        operation: impl FnOnce(*mut sys::ImGuiContext) -> R,
    ) -> R {
        let pointer = self.context_owner.as_ptr();
        self.context_owner.with_current(|| operation(pointer))
    }

    /// Provides scoped mutable access to the owned context for the platform
    /// backend. The owner cannot escape the closure or outlive this session.
    pub fn with_context_owner<R>(
        &mut self,
        operation: impl FnOnce(&mut ImGuiContextOwner) -> R,
    ) -> R {
        operation(&mut self.context_owner)
    }

    /// Builds and renders one frame.
    ///
    /// `build_ui` runs while the owned ImGui context is current. The D3D11
    /// draw phase begins only after exhaustive state capture succeeds.
    ///
    /// # Errors
    ///
    /// Returns a closed frame-validation, draw-data, generation, state, or GPU
    /// error. State capture failure guarantees that no indexed draw was issued.
    pub fn render_frame<R>(
        &mut self,
        generation: u64,
        config: FrameConfig,
        build_ui: impl FnOnce(*mut sys::ImGuiContext) -> R,
    ) -> Result<(R, RenderStats), RendererError> {
        let Self {
            context_owner,
            gpu,
            _thread_bound: _,
        } = self;
        let context_pointer = context_owner.as_ptr();
        let mut frame = context_owner.begin_frame(config)?;
        let result = build_ui(context_pointer);
        let draw_data = frame.render()?;
        let stats = gpu.render(&draw_data, generation)?;
        Ok((result, stats))
    }

    /// Rebuilds the GPU font texture after a pre-frame atlas mutation.
    ///
    /// Callers must perform atlas changes before starting an ImGui frame. The
    /// existing texture remains owned if creation of its replacement fails.
    ///
    /// # Errors
    ///
    /// Returns a closed atlas, overflow, or D3D11 resource-creation error.
    pub fn rebuild_font_texture(&mut self) -> Result<(), RendererError> {
        let Self {
            context_owner,
            gpu,
            _thread_bound: _,
        } = self;
        gpu.rebuild_font_texture(context_owner)
    }

    /// Releases every reference to the current swap-chain back buffer before
    /// `ResizeBuffers` and advances the expected generation.
    pub fn invalidate_back_buffer(&mut self, next_generation: u64) -> Result<(), RendererError> {
        self.gpu.synchronize_generation(next_generation)
    }

    /// Reacquires the back buffer and render-target view after `ResizeBuffers`.
    ///
    /// # Errors
    ///
    /// Returns an error if `generation` does not match the invalidation token or
    /// if DXGI/D3D11 cannot recreate the render target.
    pub fn recreate_back_buffer(&mut self, generation: u64) -> Result<(), RendererError> {
        self.gpu.recreate_back_buffer(generation)
    }

    /// Synchronizes a newer session generation after a resize performed on a
    /// different thread. No back-buffer reference is retained by this session.
    ///
    /// # Errors
    ///
    /// Rejects an older generation so delayed callbacks cannot move the
    /// renderer backwards.
    pub fn synchronize_generation(&mut self, generation: u64) -> Result<(), RendererError> {
        self.gpu.synchronize_generation(generation)
    }

    /// Current swap-chain generation accepted by [`Self::render_frame`].
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.gpu.generation
    }

    /// Monotonic count of renderer-owned GPU resource recreations.
    #[must_use]
    pub const fn resource_generation(&self) -> u64 {
        self.gpu.resource_generation
    }
}

impl Drop for D3d11Renderer {
    fn drop(&mut self) {
        let _ = self.context_owner.set_renderer_ready(false);
        self.gpu.font.clear_tex_id(&mut self.context_owner);
    }
}

struct GpuRenderer {
    swap_chain: IDXGISwapChain,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    backend: ExhaustiveWindowsBackend,
    objects: DeviceObjects,
    vertex_buffer: ID3D11Buffer,
    index_buffer: ID3D11Buffer,
    vertex_capacity: usize,
    index_capacity: usize,
    font: FontResources,
    generation: u64,
    resource_generation: u64,
}

impl GpuRenderer {
    fn attach(
        swap_chain: IDXGISwapChain,
        generation: u64,
        context_owner: &mut ImGuiContextOwner,
    ) -> Result<Self, RendererError> {
        // SAFETY: the swap chain is a live owned interface.
        let device = unsafe { swap_chain.GetDevice::<ID3D11Device>() }
            .map_err(|error| hresult(GpuOperation::GetDevice, error))?;
        // SAFETY: the device is live for the returned context reference.
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|error| hresult(GpuOperation::GetImmediateContext, error))?;
        let backend = ExhaustiveWindowsBackend::new(context.clone())
            .map_err(|_| RendererError::StateBackend)?;
        let objects = DeviceObjects::create(&device)?;
        let vertex_buffer = create_dynamic_buffer(
            &device,
            INITIAL_VERTEX_CAPACITY,
            size_of::<sys::ImDrawVert>(),
            D3D11_BIND_VERTEX_BUFFER.0 as u32,
        )?;
        let index_buffer = create_dynamic_buffer(
            &device,
            INITIAL_INDEX_CAPACITY,
            size_of::<sys::ImDrawIdx>(),
            D3D11_BIND_INDEX_BUFFER.0 as u32,
        )?;
        let font = FontResources::create(&device, context_owner)?;
        // Validate the initial target, then release both references before the
        // renderer becomes observable so cross-thread resize is never blocked.
        let (_back_buffer, _render_target) = acquire_back_buffer(&swap_chain, &device)?;
        Ok(Self {
            swap_chain,
            device,
            context,
            backend,
            objects,
            vertex_buffer,
            index_buffer,
            vertex_capacity: INITIAL_VERTEX_CAPACITY,
            index_capacity: INITIAL_INDEX_CAPACITY,
            font,
            generation,
            resource_generation: 1,
        })
    }

    fn rebuild_font_texture(
        &mut self,
        context_owner: &mut ImGuiContextOwner,
    ) -> Result<(), RendererError> {
        let next_generation = next_resource_generation(self.resource_generation)?;
        let replacement = FontResources::create(&self.device, context_owner)?;
        self.font = replacement;
        self.resource_generation = next_generation;
        Ok(())
    }

    fn synchronize_generation(&mut self, generation: u64) -> Result<(), RendererError> {
        if generation_transition(self.generation, generation)? {
            self.generation = generation;
            self.bump_resource_generation()?;
        }
        Ok(())
    }

    fn recreate_back_buffer(&mut self, generation: u64) -> Result<(), RendererError> {
        if generation != self.generation {
            return Err(RendererError::StaleGeneration);
        }
        let (_back_buffer, _render_target) = acquire_back_buffer(&self.swap_chain, &self.device)?;
        self.bump_resource_generation()?;
        Ok(())
    }

    fn render(
        &mut self,
        draw_data: &DrawData<'_>,
        generation: u64,
    ) -> Result<RenderStats, RendererError> {
        if generation != self.generation {
            return Err(RendererError::StaleGeneration);
        }
        // These are deliberately local to one present callback. Every success,
        // error, or unwind releases them before returning to DXGI.
        let (_back_buffer, render_target) = acquire_back_buffer(&self.swap_chain, &self.device)?;
        let raw = draw_data.as_ptr();
        let raw = NonNull::new(raw).ok_or(RendererError::NullPointer("draw-data"))?;
        // SAFETY: `DrawData` borrows a rendered frame, so its native pointer and
        // all transitively referenced vectors remain live for this call.
        let draw_data = unsafe { raw.as_ref() };
        let geometry = validate_geometry(draw_data)?;
        let (vertex_count, index_count) = validate_totals(draw_data)?;
        // SAFETY: the same rendered-frame lifetime covers every list/vector.
        let lists = unsafe { collect_draw_lists(draw_data, vertex_count, index_count) }?;

        self.ensure_dynamic_buffers(vertex_count, index_count)?;
        self.upload_geometry(&lists, vertex_count, index_count)?;
        self.update_projection(geometry)?;

        let Self {
            backend,
            context: _,
            objects,
            vertex_buffer,
            index_buffer,
            ..
        } = self;
        guarded_operation(backend, |backend| {
            submit_draws(
                DrawResources {
                    context: backend.context(),
                    objects,
                    vertex_buffer,
                    index_buffer,
                    render_target: &render_target,
                },
                draw_data,
                geometry,
                &lists,
            )
        })
    }

    fn ensure_dynamic_buffers(
        &mut self,
        vertex_count: usize,
        index_count: usize,
    ) -> Result<(), RendererError> {
        if vertex_count > self.vertex_capacity {
            let capacity = grown_capacity(vertex_count, VERTEX_GROWTH_SLACK)?;
            self.vertex_buffer = create_dynamic_buffer(
                &self.device,
                capacity,
                size_of::<sys::ImDrawVert>(),
                D3D11_BIND_VERTEX_BUFFER.0 as u32,
            )?;
            self.vertex_capacity = capacity;
            self.bump_resource_generation()?;
        }
        if index_count > self.index_capacity {
            let capacity = grown_capacity(index_count, INDEX_GROWTH_SLACK)?;
            self.index_buffer = create_dynamic_buffer(
                &self.device,
                capacity,
                size_of::<sys::ImDrawIdx>(),
                D3D11_BIND_INDEX_BUFFER.0 as u32,
            )?;
            self.index_capacity = capacity;
            self.bump_resource_generation()?;
        }
        Ok(())
    }

    fn upload_geometry(
        &self,
        lists: &[DrawListView<'_>],
        vertex_count: usize,
        index_count: usize,
    ) -> Result<(), RendererError> {
        if vertex_count > 0 {
            let mapped = map_discard(&self.context, &self.vertex_buffer)?;
            let mut destination = mapped.cast::<sys::ImDrawVert>();
            for list in lists {
                // SAFETY: validation proved the aggregate destination capacity,
                // and source slices remain live under the rendered frame.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        list.vertices.as_ptr(),
                        destination,
                        list.vertices.len(),
                    );
                    destination = destination.add(list.vertices.len());
                }
            }
            // SAFETY: the buffer is currently mapped exactly once by this path.
            unsafe { self.context.Unmap(&self.vertex_buffer, 0) };
        }
        if index_count > 0 {
            let mapped = map_discard(&self.context, &self.index_buffer)?;
            let mut destination = mapped.cast::<sys::ImDrawIdx>();
            for list in lists {
                // SAFETY: validation proved the aggregate destination capacity.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        list.indices.as_ptr(),
                        destination,
                        list.indices.len(),
                    );
                    destination = destination.add(list.indices.len());
                }
            }
            // SAFETY: the buffer is currently mapped exactly once by this path.
            unsafe { self.context.Unmap(&self.index_buffer, 0) };
        }
        Ok(())
    }

    fn update_projection(&self, geometry: FrameGeometry) -> Result<(), RendererError> {
        let left = geometry.display_pos[0];
        let right = geometry.display_pos[0] + geometry.display_size[0];
        let top = geometry.display_pos[1];
        let bottom = geometry.display_pos[1] + geometry.display_size[1];
        let matrix = [
            [2.0 / (right - left), 0.0, 0.0, 0.0],
            [0.0, 2.0 / (top - bottom), 0.0, 0.0],
            [0.0, 0.0, 0.5, 0.0],
            [
                (right + left) / (left - right),
                (top + bottom) / (bottom - top),
                0.5,
                1.0,
            ],
        ];
        let mapped = map_discard(&self.context, &self.objects.constant_buffer)?;
        // SAFETY: the constant buffer is exactly one 4x4 f32 matrix in size.
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(&matrix).cast::<u8>(),
                mapped.cast::<u8>(),
                size_of::<[[f32; 4]; 4]>(),
            );
            self.context.Unmap(&self.objects.constant_buffer, 0);
        }
        Ok(())
    }

    fn bump_resource_generation(&mut self) -> Result<(), RendererError> {
        self.resource_generation = next_resource_generation(self.resource_generation)?;
        Ok(())
    }
}

fn guarded_operation<B, T>(
    backend: &mut B,
    operation: impl FnOnce(&mut B) -> Result<T, RendererError>,
) -> Result<T, RendererError>
where
    B: ExhaustivePipelineBackend<Error = crate::StateBackendError>,
{
    let mut guard = FullStateGuard::capture(backend)?;
    let operation_result = operation(guard.backend_mut());
    let restore_result = guard.restore();
    restore_result?;
    operation_result
}

struct DeviceObjects {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    input_layout: ID3D11InputLayout,
    constant_buffer: ID3D11Buffer,
    blend_state: ID3D11BlendState,
    rasterizer_state: ID3D11RasterizerState,
    depth_stencil_state: ID3D11DepthStencilState,
    sampler: ID3D11SamplerState,
}

impl DeviceObjects {
    fn create(device: &ID3D11Device) -> Result<Self, RendererError> {
        let vertex_blob = compile_shader(VERTEX_SHADER_SOURCE, ShaderKind::Vertex, b"vs_4_0\0")?;
        let pixel_blob = compile_shader(PIXEL_SHADER_SOURCE, ShaderKind::Pixel, b"ps_4_0\0")?;
        let vertex_bytecode = blob_bytes(&vertex_blob, ShaderKind::Vertex)?;
        let pixel_bytecode = blob_bytes(&pixel_blob, ShaderKind::Pixel)?;

        let mut vertex_shader = None;
        // SAFETY: bytecode comes from the deterministic in-process compiler.
        unsafe {
            device
                .CreateVertexShader(
                    vertex_bytecode,
                    None::<&windows::Win32::Graphics::Direct3D11::ID3D11ClassLinkage>,
                    Some(&mut vertex_shader),
                )
                .map_err(|error| hresult(GpuOperation::CreateVertexShader, error))?;
        }
        let vertex_shader = required_object(vertex_shader, GpuOperation::CreateVertexShader)?;

        let mut pixel_shader = None;
        // SAFETY: bytecode comes from the deterministic in-process compiler.
        unsafe {
            device
                .CreatePixelShader(
                    pixel_bytecode,
                    None::<&windows::Win32::Graphics::Direct3D11::ID3D11ClassLinkage>,
                    Some(&mut pixel_shader),
                )
                .map_err(|error| hresult(GpuOperation::CreatePixelShader, error))?;
        }
        let pixel_shader = required_object(pixel_shader, GpuOperation::CreatePixelShader)?;

        let elements = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(c"POSITION".as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(c"TEXCOORD".as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 8,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(c"COLOR".as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                InputSlot: 0,
                AlignedByteOffset: 16,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];
        let mut input_layout = None;
        // SAFETY: descriptors and bytecode remain live for the call.
        unsafe {
            device
                .CreateInputLayout(&elements, vertex_bytecode, Some(&mut input_layout))
                .map_err(|error| hresult(GpuOperation::CreateInputLayout, error))?;
        }
        let input_layout = required_object(input_layout, GpuOperation::CreateInputLayout)?;

        let constant_buffer = create_dynamic_buffer(
            device,
            1,
            size_of::<[[f32; 4]; 4]>(),
            D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        )?;

        let target_blend = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: BOOL::from(true),
            SrcBlend: D3D11_BLEND_SRC_ALPHA,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let blend_desc = D3D11_BLEND_DESC {
            AlphaToCoverageEnable: BOOL::from(false),
            IndependentBlendEnable: BOOL::from(false),
            RenderTarget: [target_blend; 8],
        };
        let mut blend_state = None;
        // SAFETY: descriptor is initialized and output storage is valid.
        unsafe {
            device
                .CreateBlendState(&blend_desc, Some(&mut blend_state))
                .map_err(|error| hresult(GpuOperation::CreateBlendState, error))?;
        }
        let blend_state = required_object(blend_state, GpuOperation::CreateBlendState)?;

        let rasterizer_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            FrontCounterClockwise: BOOL::from(false),
            DepthBias: 0,
            DepthBiasClamp: 0.0,
            SlopeScaledDepthBias: 0.0,
            DepthClipEnable: BOOL::from(true),
            ScissorEnable: BOOL::from(true),
            MultisampleEnable: BOOL::from(false),
            AntialiasedLineEnable: BOOL::from(false),
        };
        let mut rasterizer_state = None;
        // SAFETY: descriptor is initialized and output storage is valid.
        unsafe {
            device
                .CreateRasterizerState(&rasterizer_desc, Some(&mut rasterizer_state))
                .map_err(|error| hresult(GpuOperation::CreateRasterizerState, error))?;
        }
        let rasterizer_state =
            required_object(rasterizer_state, GpuOperation::CreateRasterizerState)?;

        let depth_desc = D3D11_DEPTH_STENCIL_DESC {
            DepthEnable: BOOL::from(false),
            DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ZERO,
            DepthFunc: D3D11_COMPARISON_ALWAYS,
            StencilEnable: BOOL::from(false),
            ..D3D11_DEPTH_STENCIL_DESC::default()
        };
        let mut depth_stencil_state = None;
        // SAFETY: descriptor is initialized and output storage is valid.
        unsafe {
            device
                .CreateDepthStencilState(&depth_desc, Some(&mut depth_stencil_state))
                .map_err(|error| hresult(GpuOperation::CreateDepthStencilState, error))?;
        }
        let depth_stencil_state =
            required_object(depth_stencil_state, GpuOperation::CreateDepthStencilState)?;

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_WRAP,
            AddressV: D3D11_TEXTURE_ADDRESS_WRAP,
            AddressW: D3D11_TEXTURE_ADDRESS_WRAP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_ALWAYS,
            BorderColor: [0.0; 4],
            MinLOD: 0.0,
            MaxLOD: f32::MAX,
        };
        let mut sampler = None;
        // SAFETY: descriptor is initialized and output storage is valid.
        unsafe {
            device
                .CreateSamplerState(&sampler_desc, Some(&mut sampler))
                .map_err(|error| hresult(GpuOperation::CreateSamplerState, error))?;
        }
        let sampler = required_object(sampler, GpuOperation::CreateSamplerState)?;

        Ok(Self {
            vertex_shader,
            pixel_shader,
            input_layout,
            constant_buffer,
            blend_state,
            rasterizer_state,
            depth_stencil_state,
            sampler,
        })
    }
}

struct FontResources {
    _texture: ID3D11Texture2D,
    _view: ID3D11ShaderResourceView,
    atlas: NonNull<sys::ImFontAtlas>,
    tex_id: *mut c_void,
}

impl FontResources {
    fn create(device: &ID3D11Device, owner: &mut ImGuiContextOwner) -> Result<Self, RendererError> {
        let (atlas, pixels, width, height, row_pitch) = owner.with_current(|| {
            // SAFETY: the owner made its live context current.
            let io =
                NonNull::new(unsafe { sys::igGetIO() }).ok_or(RendererError::InvalidFontAtlas)?;
            // SAFETY: IO belongs to the current context.
            let atlas = NonNull::new(unsafe { io.as_ref().Fonts })
                .ok_or(RendererError::InvalidFontAtlas)?;
            let mut pixels = std::ptr::null_mut();
            let mut width = 0;
            let mut height = 0;
            let mut bytes_per_pixel = 0;
            // SAFETY: all outputs are valid and the atlas is live.
            unsafe {
                sys::ImFontAtlas_GetTexDataAsRGBA32(
                    atlas.as_ptr(),
                    &mut pixels,
                    &mut width,
                    &mut height,
                    &mut bytes_per_pixel,
                );
            }
            if pixels.is_null() || width <= 0 || height <= 0 || bytes_per_pixel != 4 {
                return Err(RendererError::InvalidFontAtlas);
            }
            let width = u32::try_from(width).map_err(|_| RendererError::DrawDataOverflow)?;
            let height = u32::try_from(height).map_err(|_| RendererError::DrawDataOverflow)?;
            let row_pitch = width
                .checked_mul(4)
                .ok_or(RendererError::DrawDataOverflow)?;
            let _total = usize::try_from(width)
                .ok()
                .and_then(|width| {
                    usize::try_from(height)
                        .ok()
                        .and_then(|height| width.checked_mul(height))
                })
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or(RendererError::DrawDataOverflow)?;
            Ok((atlas, pixels, width, height, row_pitch))
        })?;

        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.cast(),
            SysMemPitch: row_pitch,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        // SAFETY: atlas pixels cover the validated texture extent for the call.
        unsafe {
            device
                .CreateTexture2D(&texture_desc, Some(&initial), Some(&mut texture))
                .map_err(|error| hresult(GpuOperation::CreateFontTexture, error))?;
        }
        let texture = required_object(texture, GpuOperation::CreateFontTexture)?;
        let mut view = None;
        // SAFETY: texture is live and a default SRV is valid for this format.
        unsafe {
            device
                .CreateShaderResourceView(&texture, None, Some(&mut view))
                .map_err(|error| hresult(GpuOperation::CreateFontShaderResourceView, error))?;
        }
        let view = required_object(view, GpuOperation::CreateFontShaderResourceView)?;
        let tex_id = view.as_raw();
        owner.with_current(|| {
            // SAFETY: atlas belongs to the current live context; the view is
            // retained by the returned FontResources before pixels are cleared.
            unsafe {
                sys::ImFontAtlas_SetTexID(atlas.as_ptr(), tex_id);
                sys::ImFontAtlas_ClearTexData(atlas.as_ptr());
            }
        });
        Ok(Self {
            _texture: texture,
            _view: view,
            atlas,
            tex_id,
        })
    }

    fn clear_tex_id(&mut self, owner: &mut ImGuiContextOwner) {
        let atlas = self.atlas;
        let tex_id = self.tex_id;
        owner.with_current(|| {
            // SAFETY: the renderer owns the context and drops this binding
            // before that context. Only clear an unchanged atlas assignment.
            unsafe {
                if atlas.as_ref().TexID == tex_id {
                    sys::ImFontAtlas_SetTexID(atlas.as_ptr(), std::ptr::null_mut());
                }
            }
        });
    }
}

struct DrawListView<'frame> {
    raw: *const sys::ImDrawList,
    vertices: &'frame [sys::ImDrawVert],
    indices: &'frame [sys::ImDrawIdx],
    commands: &'frame [sys::ImDrawCmd],
}

unsafe fn collect_draw_lists<'frame>(
    draw_data: &'frame sys::ImDrawData,
    expected_vertices: usize,
    expected_indices: usize,
) -> Result<Vec<DrawListView<'frame>>, RendererError> {
    let list_count =
        usize::try_from(draw_data.CmdListsCount).map_err(|_| RendererError::InvalidDrawData)?;
    // SAFETY: caller ties all draw-data allocations to the rendered frame.
    let pointers = unsafe { checked_slice(draw_data.CmdLists, list_count) }?;
    let mut lists = Vec::with_capacity(list_count);
    let mut total_vertices = 0usize;
    let mut total_indices = 0usize;
    for &pointer in pointers {
        let list = NonNull::new(pointer).ok_or(RendererError::InvalidDrawData)?;
        // SAFETY: ImDrawData owns every listed ImDrawList for the frame.
        let list = unsafe { list.as_ref() };
        let vertex_count =
            usize::try_from(list.VtxBuffer.Size).map_err(|_| RendererError::InvalidDrawData)?;
        let index_count =
            usize::try_from(list.IdxBuffer.Size).map_err(|_| RendererError::InvalidDrawData)?;
        let command_count =
            usize::try_from(list.CmdBuffer.Size).map_err(|_| RendererError::InvalidDrawData)?;
        // SAFETY: ImVector guarantees contiguous storage for each live element.
        let vertices = unsafe { checked_slice(list.VtxBuffer.Data, vertex_count) }?;
        // SAFETY: same ImVector invariant.
        let indices = unsafe { checked_slice(list.IdxBuffer.Data, index_count) }?;
        // SAFETY: same ImVector invariant.
        let commands = unsafe { checked_slice(list.CmdBuffer.Data, command_count) }?;
        for command in commands {
            if command.UserCallback.is_none() {
                let end = (command.IdxOffset as usize)
                    .checked_add(command.ElemCount as usize)
                    .ok_or(RendererError::DrawDataOverflow)?;
                if end > index_count || command.VtxOffset as usize > vertex_count {
                    return Err(RendererError::InvalidDrawData);
                }
            }
        }
        total_vertices = total_vertices
            .checked_add(vertex_count)
            .ok_or(RendererError::DrawDataOverflow)?;
        total_indices = total_indices
            .checked_add(index_count)
            .ok_or(RendererError::DrawDataOverflow)?;
        lists.push(DrawListView {
            raw: pointer,
            vertices,
            indices,
            commands,
        });
    }
    if total_vertices != expected_vertices || total_indices != expected_indices {
        return Err(RendererError::InvalidDrawData);
    }
    Ok(lists)
}

unsafe fn checked_slice<'a, T>(pointer: *const T, length: usize) -> Result<&'a [T], RendererError> {
    if length == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() || length > isize::MAX as usize / size_of::<T>().max(1) {
        return Err(RendererError::InvalidDrawData);
    }
    // SAFETY: caller supplies an ImVector/ImDrawData allocation with `length`
    // initialized contiguous elements, and the arithmetic bound was checked.
    Ok(unsafe { slice::from_raw_parts(pointer, length) })
}

struct DrawResources<'gpu> {
    context: &'gpu ID3D11DeviceContext,
    objects: &'gpu DeviceObjects,
    vertex_buffer: &'gpu ID3D11Buffer,
    index_buffer: &'gpu ID3D11Buffer,
    render_target: &'gpu ID3D11RenderTargetView,
}

fn submit_draws(
    resources: DrawResources<'_>,
    draw_data: &sys::ImDrawData,
    geometry: FrameGeometry,
    lists: &[DrawListView<'_>],
) -> Result<RenderStats, RendererError> {
    let DrawResources {
        context,
        objects,
        vertex_buffer,
        index_buffer,
        render_target,
    } = resources;
    setup_render_state(
        context,
        objects,
        vertex_buffer,
        index_buffer,
        render_target,
        geometry,
    );
    let mut stats = RenderStats::default();
    let mut global_vertex_offset = 0usize;
    let mut global_index_offset = 0usize;
    for list in lists {
        for command in list.commands {
            let Some(translated) =
                translate_command(command, geometry, global_vertex_offset, global_index_offset)?
            else {
                continue;
            };
            match translated.kind {
                CommandKind::ResetState => {
                    setup_render_state(
                        context,
                        objects,
                        vertex_buffer,
                        index_buffer,
                        render_target,
                        geometry,
                    );
                    stats.reset_state_callbacks = stats
                        .reset_state_callbacks
                        .checked_add(1)
                        .ok_or(RendererError::DrawDataOverflow)?;
                }
                CommandKind::Callback => {
                    let Some(callback) = command.UserCallback else {
                        return Err(RendererError::InvalidDrawData);
                    };
                    // SAFETY: native ImGui command callbacks are synchronous;
                    // both pointers belong to the rendered frame.
                    unsafe { callback(list.raw, std::ptr::from_ref(command)) };
                    stats.user_callbacks = stats
                        .user_callbacks
                        .checked_add(1)
                        .ok_or(RendererError::DrawDataOverflow)?;
                }
                CommandKind::Draw => {
                    issue_draw(context, translated);
                    stats.draw_calls = stats
                        .draw_calls
                        .checked_add(1)
                        .ok_or(RendererError::DrawDataOverflow)?;
                    stats.elements = stats
                        .elements
                        .checked_add(u64::from(translated.element_count))
                        .ok_or(RendererError::DrawDataOverflow)?;
                }
            }
        }
        global_vertex_offset = global_vertex_offset
            .checked_add(list.vertices.len())
            .ok_or(RendererError::DrawDataOverflow)?;
        global_index_offset = global_index_offset
            .checked_add(list.indices.len())
            .ok_or(RendererError::DrawDataOverflow)?;
    }
    let _ = draw_data;
    Ok(stats)
}

fn setup_render_state(
    context: &ID3D11DeviceContext,
    objects: &DeviceObjects,
    vertex_buffer: &ID3D11Buffer,
    index_buffer: &ID3D11Buffer,
    render_target: &ID3D11RenderTargetView,
    geometry: FrameGeometry,
) {
    let viewport = D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: geometry.framebuffer_size[0] as f32,
        Height: geometry.framebuffer_size[1] as f32,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    };
    let vertex_buffers = [Some(vertex_buffer.clone())];
    let strides = [size_of::<sys::ImDrawVert>() as u32];
    let offsets = [0_u32];
    let constants = [Some(objects.constant_buffer.clone())];
    let samplers = [Some(objects.sampler.clone())];
    let render_targets = [Some(render_target.clone())];
    // SAFETY: every interface is live, arrays remain valid for each immediate
    // call, and all values satisfy their documented D3D11 ranges.
    unsafe {
        context.RSSetViewports(Some(&[viewport]));
        context.IASetInputLayout(&objects.input_layout);
        context.IASetVertexBuffers(
            0,
            1,
            Some(vertex_buffers.as_ptr()),
            Some(strides.as_ptr()),
            Some(offsets.as_ptr()),
        );
        let index_format = if size_of::<sys::ImDrawIdx>() == 2 {
            DXGI_FORMAT_R16_UINT
        } else {
            DXGI_FORMAT_R32_UINT
        };
        context.IASetIndexBuffer(index_buffer, index_format, 0);
        context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        context.VSSetShader(&objects.vertex_shader, None);
        context.VSSetConstantBuffers(0, Some(&constants));
        context.HSSetShader(
            None::<&windows::Win32::Graphics::Direct3D11::ID3D11HullShader>,
            None,
        );
        context.DSSetShader(
            None::<&windows::Win32::Graphics::Direct3D11::ID3D11DomainShader>,
            None,
        );
        context.GSSetShader(
            None::<&windows::Win32::Graphics::Direct3D11::ID3D11GeometryShader>,
            None,
        );
        context.PSSetShader(&objects.pixel_shader, None);
        context.PSSetSamplers(0, Some(&samplers));
        context.CSSetShader(
            None::<&windows::Win32::Graphics::Direct3D11::ID3D11ComputeShader>,
            None,
        );
        context.SetPredication(
            None::<&windows::Win32::Graphics::Direct3D11::ID3D11Predicate>,
            false,
        );
        context.OMSetBlendState(&objects.blend_state, Some(&[0.0; 4]), u32::MAX);
        context.OMSetDepthStencilState(&objects.depth_stencil_state, 0);
        context.OMSetRenderTargets(
            Some(&render_targets),
            None::<&windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilView>,
        );
        context.RSSetState(&objects.rasterizer_state);
    }
}

fn issue_draw(context: &ID3D11DeviceContext, command: TranslatedCommand) {
    let rect = RECT {
        left: command.scissor[0],
        top: command.scissor[1],
        right: command.scissor[2],
        bottom: command.scissor[3],
    };
    let texture = [command.texture];
    let vtable = Interface::vtable(context);
    // SAFETY: ImTextureID is the native D3D11 SRV contract for this backend.
    // The command retains the resource for the synchronous draw call.
    unsafe {
        context.RSSetScissorRects(Some(&[rect]));
        (vtable.PSSetShaderResources)(context.as_raw(), 0, 1, texture.as_ptr());
        context.DrawIndexed(
            command.element_count,
            command.start_index,
            command.base_vertex,
        );
    }
}

fn acquire_back_buffer(
    swap_chain: &IDXGISwapChain,
    device: &ID3D11Device,
) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView), RendererError> {
    // SAFETY: swap chain is live and buffer zero is the current back buffer.
    let back_buffer = unsafe { swap_chain.GetBuffer::<ID3D11Texture2D>(0) }
        .map_err(|error| hresult(GpuOperation::GetBackBuffer, error))?;
    let mut render_target = None;
    // SAFETY: back buffer is live; a default RTV is valid for swap-chain buffers.
    unsafe {
        device
            .CreateRenderTargetView(&back_buffer, None, Some(&mut render_target))
            .map_err(|error| hresult(GpuOperation::CreateRenderTargetView, error))?;
    }
    let render_target = required_object(render_target, GpuOperation::CreateRenderTargetView)?;
    Ok((back_buffer, render_target))
}

fn create_dynamic_buffer(
    device: &ID3D11Device,
    capacity: usize,
    element_size: usize,
    bind_flags: u32,
) -> Result<ID3D11Buffer, RendererError> {
    let byte_width = capacity
        .checked_mul(element_size)
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or(RendererError::DrawDataOverflow)?;
    let descriptor = D3D11_BUFFER_DESC {
        ByteWidth: byte_width,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: bind_flags,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut buffer = None;
    // SAFETY: descriptor is initialized and output storage is valid.
    unsafe {
        device
            .CreateBuffer(&descriptor, None, Some(&mut buffer))
            .map_err(|error| hresult(GpuOperation::CreateBuffer, error))?;
    }
    required_object(buffer, GpuOperation::CreateBuffer)
}

fn map_discard(
    context: &ID3D11DeviceContext,
    buffer: &ID3D11Buffer,
) -> Result<*mut c_void, RendererError> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: buffer is a live dynamic CPU-write resource and output is valid.
    unsafe {
        context
            .Map(buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
            .map_err(|error| hresult(GpuOperation::MapResource, error))?;
    }
    if mapped.pData.is_null() {
        // SAFETY: a successful Map must be balanced even for a bad driver output.
        unsafe { context.Unmap(buffer, 0) };
        return Err(RendererError::HResult {
            operation: GpuOperation::MapResource,
            code: E_POINTER.0,
        });
    }
    Ok(mapped.pData)
}

fn compile_shader(
    source: &[u8],
    kind: ShaderKind,
    target: &'static [u8],
) -> Result<ID3DBlob, RendererError> {
    let mut code = None;
    let mut diagnostics = None;
    // SAFETY: source and target are bounded process-static byte sequences;
    // outputs are valid. Compiler diagnostics are deliberately discarded.
    let result = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR::null(),
            None,
            None::<&ID3DInclude>,
            PCSTR(c"main".as_ptr().cast()),
            PCSTR(target.as_ptr()),
            D3DCOMPILE_ENABLE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut diagnostics),
        )
    };
    drop(diagnostics);
    result.map_err(|_| RendererError::ShaderCompile(kind))?;
    code.ok_or(RendererError::ShaderCompile(kind))
}

fn blob_bytes(blob: &ID3DBlob, kind: ShaderKind) -> Result<&[u8], RendererError> {
    // SAFETY: blob is a live compiler output.
    let pointer = unsafe { blob.GetBufferPointer() };
    // SAFETY: same live blob query.
    let length = unsafe { blob.GetBufferSize() };
    if pointer.is_null() || length == 0 || length > isize::MAX as usize {
        return Err(RendererError::ShaderCompile(kind));
    }
    // SAFETY: ID3DBlob owns `length` initialized bytes for its lifetime.
    Ok(unsafe { slice::from_raw_parts(pointer.cast(), length) })
}

fn required_object<T>(object: Option<T>, operation: GpuOperation) -> Result<T, RendererError> {
    object.ok_or(RendererError::HResult {
        operation,
        code: E_POINTER.0,
    })
}

fn hresult(operation: GpuOperation, error: windows::core::Error) -> RendererError {
    RendererError::HResult {
        operation,
        code: error.code().0,
    }
}

fn grown_capacity(required: usize, slack: usize) -> Result<usize, RendererError> {
    required
        .checked_add(slack)
        .ok_or(RendererError::DrawDataOverflow)
}

fn generation_transition(current: u64, requested: u64) -> Result<bool, RendererError> {
    if requested < current {
        Err(RendererError::StaleGeneration)
    } else {
        Ok(requested > current)
    }
}

fn next_resource_generation(current: u64) -> Result<u64, RendererError> {
    current
        .checked_add(1)
        .ok_or(RendererError::DrawDataOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_render_d3d11::{
        COMMONSHADER_CONSTANT_BUFFER_SLOTS, COMMONSHADER_INPUT_RESOURCE_SLOTS,
        COMMONSHADER_SAMPLER_SLOTS, ComputeState, CriticalPipelineBackend, HiddenCounterState,
        IndexBufferBinding, InputAssemblerState, OM_RENDER_TARGET_SLOTS, OutputMergerState,
        PS_CS_UAV_SLOTS, PredicationState, ProgrammableStageState, RasterizerState,
        SO_BUFFER_SLOTS, StateStep, StreamOutputOffsets, StreamOutputState, VertexBufferBinding,
    };

    #[test]
    fn buffer_growth_is_bounded_and_has_slack() {
        assert_eq!(
            grown_capacity(5_001, VERTEX_GROWTH_SLACK).expect("growth fits"),
            10_001
        );
        assert!(matches!(
            grown_capacity(usize::MAX, 1),
            Err(RendererError::DrawDataOverflow)
        ));
    }

    #[test]
    fn projection_layout_matches_imgui_d3d_convention() {
        let geometry = FrameGeometry {
            display_pos: [0.0, 0.0],
            display_size: [100.0, 50.0],
            framebuffer_scale: [1.0, 1.0],
            framebuffer_size: [100, 50],
        };
        let left = geometry.display_pos[0];
        let right = left + geometry.display_size[0];
        let top = geometry.display_pos[1];
        let bottom = top + geometry.display_size[1];
        assert_eq!(2.0 / (right - left), 0.02);
        assert_eq!(2.0 / (top - bottom), -0.04);
    }

    #[test]
    fn generation_transitions_are_monotonic_and_resource_count_is_checked() {
        assert!(!generation_transition(7, 7).expect("equal generation is current"));
        assert!(generation_transition(7, 8).expect("newer generation is accepted"));
        assert!(matches!(
            generation_transition(8, 7),
            Err(RendererError::StaleGeneration)
        ));
        assert_eq!(
            next_resource_generation(8).expect("resource generation advances"),
            9
        );
        assert!(matches!(
            next_resource_generation(u64::MAX),
            Err(RendererError::DrawDataOverflow)
        ));
    }

    #[test]
    fn exhaustive_capture_failure_prevents_draw_admission() {
        let mut backend = SentinelBackend {
            fail_first_capture: true,
            ..SentinelBackend::default()
        };
        let mut draws = 0;
        let result = guarded_operation(&mut backend, |_| {
            draws += 1;
            Ok(())
        });
        assert!(matches!(
            result,
            Err(RendererError::StateCapture {
                step: StateStep::OutputMerger
            })
        ));
        assert_eq!(draws, 0);
        assert!(backend.restores.is_empty());
    }

    #[test]
    fn exhaustive_guard_restores_every_section_in_hazard_safe_order() {
        let mut backend = SentinelBackend::default();
        let mut draws = 0;
        guarded_operation(&mut backend, |_| {
            draws += 1;
            Ok(())
        })
        .expect("complete capture, draw, and restore succeed");
        assert_eq!(draws, 1);
        assert_eq!(
            backend.restores,
            [
                StateStep::OutputMerger,
                StateStep::StreamOutput,
                StateStep::ComputeOutputs,
                StateStep::InputAssembler,
                StateStep::Rasterizer,
                StateStep::VertexShader,
                StateStep::HullShader,
                StateStep::DomainShader,
                StateStep::GeometryShader,
                StateStep::PixelShader,
                StateStep::ComputeShader,
                StateStep::Predication,
            ]
        );
    }

    #[derive(Default)]
    struct SentinelBackend {
        fail_first_capture: bool,
        restores: Vec<StateStep>,
    }

    impl CriticalPipelineBackend for SentinelBackend {
        type Handle = ();
        type Error = crate::StateBackendError;

        fn capture_output_merger(
            &mut self,
        ) -> Result<OutputMergerState<Self::Handle>, Self::Error> {
            if self.fail_first_capture {
                return Err(crate::StateBackendError::IncompatibleHandle);
            }
            Ok(output_merger_state())
        }

        fn capture_input_assembler(
            &mut self,
        ) -> Result<InputAssemblerState<Self::Handle>, Self::Error> {
            Ok(input_assembler_state())
        }

        fn capture_rasterizer(&mut self) -> Result<RasterizerState<Self::Handle>, Self::Error> {
            Ok(RasterizerState {
                state: None,
                viewports: Vec::new(),
                scissor_rects: Vec::new(),
            })
        }

        fn capture_vertex_shader(
            &mut self,
        ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
            Ok(stage_state())
        }

        fn capture_pixel_shader(
            &mut self,
        ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
            Ok(stage_state())
        }

        fn restore_output_merger(
            &mut self,
            _state: &OutputMergerState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::OutputMerger);
            Ok(())
        }

        fn restore_input_assembler(
            &mut self,
            _state: &InputAssemblerState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::InputAssembler);
            Ok(())
        }

        fn restore_rasterizer(
            &mut self,
            _state: &RasterizerState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::Rasterizer);
            Ok(())
        }

        fn restore_vertex_shader(
            &mut self,
            _state: &ProgrammableStageState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::VertexShader);
            Ok(())
        }

        fn restore_pixel_shader(
            &mut self,
            _state: &ProgrammableStageState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::PixelShader);
            Ok(())
        }
    }

    impl ExhaustivePipelineBackend for SentinelBackend {
        fn capture_hull_shader(
            &mut self,
        ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
            Ok(stage_state())
        }

        fn capture_domain_shader(
            &mut self,
        ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
            Ok(stage_state())
        }

        fn capture_geometry_shader(
            &mut self,
        ) -> Result<ProgrammableStageState<Self::Handle>, Self::Error> {
            Ok(stage_state())
        }

        fn capture_compute_shader(&mut self) -> Result<ComputeState<Self::Handle>, Self::Error> {
            Ok(ComputeState {
                stage: stage_state(),
                unordered_access_views: [None; PS_CS_UAV_SLOTS],
                unordered_access_counters: HiddenCounterState::Preserve,
            })
        }

        fn capture_stream_output(
            &mut self,
        ) -> Result<StreamOutputState<Self::Handle>, Self::Error> {
            Ok(StreamOutputState {
                targets: [None; SO_BUFFER_SLOTS],
                offsets: StreamOutputOffsets::Tracked([0; SO_BUFFER_SLOTS]),
            })
        }

        fn capture_predication(&mut self) -> Result<PredicationState<Self::Handle>, Self::Error> {
            Ok(PredicationState {
                predicate: None,
                value: false,
            })
        }

        fn restore_stream_output(
            &mut self,
            _state: &StreamOutputState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::StreamOutput);
            Ok(())
        }

        fn restore_compute_outputs(
            &mut self,
            _state: &ComputeState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::ComputeOutputs);
            Ok(())
        }

        fn restore_hull_shader(
            &mut self,
            _state: &ProgrammableStageState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::HullShader);
            Ok(())
        }

        fn restore_domain_shader(
            &mut self,
            _state: &ProgrammableStageState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::DomainShader);
            Ok(())
        }

        fn restore_geometry_shader(
            &mut self,
            _state: &ProgrammableStageState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::GeometryShader);
            Ok(())
        }

        fn restore_compute_shader(
            &mut self,
            _state: &ComputeState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::ComputeShader);
            Ok(())
        }

        fn restore_predication(
            &mut self,
            _state: &PredicationState<Self::Handle>,
        ) -> Result<(), Self::Error> {
            self.restores.push(StateStep::Predication);
            Ok(())
        }
    }

    fn output_merger_state() -> OutputMergerState<()> {
        OutputMergerState {
            render_targets: [None; OM_RENDER_TARGET_SLOTS],
            depth_stencil_view: None,
            unordered_access_views: [None; PS_CS_UAV_SLOTS],
            unordered_access_counters: HiddenCounterState::Preserve,
            blend_state: None,
            blend_factor: [0.0; 4],
            sample_mask: 0,
            depth_stencil_state: None,
            stencil_reference: 0,
        }
    }

    fn input_assembler_state() -> InputAssemblerState<()> {
        InputAssemblerState {
            input_layout: None,
            vertex_buffers: std::array::from_fn(|_| VertexBufferBinding {
                buffer: None,
                stride: 0,
                offset: 0,
            }),
            index_buffer: IndexBufferBinding {
                buffer: None,
                format: 0,
                offset: 0,
            },
            primitive_topology: 0,
        }
    }

    fn stage_state() -> ProgrammableStageState<()> {
        ProgrammableStageState {
            shader: None,
            class_instances: Vec::new(),
            constant_buffers: [None; COMMONSHADER_CONSTANT_BUFFER_SLOTS],
            shader_resources: [None; COMMONSHADER_INPUT_RESOURCE_SLOTS],
            samplers: [None; COMMONSHADER_SAMPLER_SLOTS],
        }
    }
}
