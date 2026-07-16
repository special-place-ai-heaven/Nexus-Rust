//! Pure validation and draw-command translation.

use std::ffi::c_void;

use nexus_imgui_compat::sys;

use crate::RendererError;

pub(crate) const RESET_RENDER_STATE_CALLBACK: usize = usize::MAX;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FrameGeometry {
    pub(crate) display_pos: [f32; 2],
    pub(crate) display_size: [f32; 2],
    pub(crate) framebuffer_scale: [f32; 2],
    pub(crate) framebuffer_size: [u32; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommandKind {
    Draw,
    ResetState,
    Callback,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TranslatedCommand {
    pub(crate) kind: CommandKind,
    pub(crate) element_count: u32,
    pub(crate) start_index: u32,
    pub(crate) base_vertex: i32,
    pub(crate) scissor: [i32; 4],
    pub(crate) texture: *mut c_void,
}

pub(crate) fn validate_geometry(
    draw_data: &sys::ImDrawData,
) -> Result<FrameGeometry, RendererError> {
    let display_pos = [draw_data.DisplayPos.x, draw_data.DisplayPos.y];
    let display_size = [draw_data.DisplaySize.x, draw_data.DisplaySize.y];
    let scale = [draw_data.FramebufferScale.x, draw_data.FramebufferScale.y];
    if display_pos.iter().any(|value| !value.is_finite())
        || display_size
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
        || scale
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
    {
        return Err(RendererError::InvalidFramebuffer);
    }
    let width = display_size[0] * scale[0];
    let height = display_size[1] * scale[1];
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(RendererError::InvalidFramebuffer);
    }
    let width = float_to_u32_ceil(width)?;
    let height = float_to_u32_ceil(height)?;
    Ok(FrameGeometry {
        display_pos,
        display_size,
        framebuffer_scale: scale,
        framebuffer_size: [width, height],
    })
}

pub(crate) fn validate_totals(
    draw_data: &sys::ImDrawData,
) -> Result<(usize, usize), RendererError> {
    if !draw_data.Valid
        || draw_data.CmdListsCount < 0
        || draw_data.TotalVtxCount < 0
        || draw_data.TotalIdxCount < 0
        || (draw_data.CmdListsCount > 0 && draw_data.CmdLists.is_null())
    {
        return Err(RendererError::InvalidDrawData);
    }
    let vertices =
        usize::try_from(draw_data.TotalVtxCount).map_err(|_| RendererError::DrawDataOverflow)?;
    let indices =
        usize::try_from(draw_data.TotalIdxCount).map_err(|_| RendererError::DrawDataOverflow)?;
    Ok((vertices, indices))
}

pub(crate) fn translate_command(
    command: &sys::ImDrawCmd,
    geometry: FrameGeometry,
    global_vertex_offset: usize,
    global_index_offset: usize,
) -> Result<Option<TranslatedCommand>, RendererError> {
    if let Some(callback) = command.UserCallback {
        let address = callback as *const () as usize;
        let kind = if address == RESET_RENDER_STATE_CALLBACK {
            CommandKind::ResetState
        } else {
            CommandKind::Callback
        };
        return Ok(Some(TranslatedCommand {
            kind,
            element_count: 0,
            start_index: 0,
            base_vertex: 0,
            scissor: [0; 4],
            texture: command.TextureId,
        }));
    }

    let clip_min_x = (command.ClipRect.x - geometry.display_pos[0]) * geometry.framebuffer_scale[0];
    let clip_min_y = (command.ClipRect.y - geometry.display_pos[1]) * geometry.framebuffer_scale[1];
    let clip_max_x = (command.ClipRect.z - geometry.display_pos[0]) * geometry.framebuffer_scale[0];
    let clip_max_y = (command.ClipRect.w - geometry.display_pos[1]) * geometry.framebuffer_scale[1];
    if [clip_min_x, clip_min_y, clip_max_x, clip_max_y]
        .iter()
        .any(|value| !value.is_finite())
    {
        return Err(RendererError::InvalidDrawData);
    }

    let left = clip_min_x.max(0.0).floor();
    let top = clip_min_y.max(0.0).floor();
    let right = clip_max_x.min(geometry.framebuffer_size[0] as f32).ceil();
    let bottom = clip_max_y.min(geometry.framebuffer_size[1] as f32).ceil();
    if right <= left || bottom <= top {
        return Ok(None);
    }

    let start_index = global_index_offset
        .checked_add(command.IdxOffset as usize)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(RendererError::DrawDataOverflow)?;
    let base_vertex = global_vertex_offset
        .checked_add(command.VtxOffset as usize)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(RendererError::DrawDataOverflow)?;
    Ok(Some(TranslatedCommand {
        kind: CommandKind::Draw,
        element_count: command.ElemCount,
        start_index,
        base_vertex,
        scissor: [
            float_to_i32(left)?,
            float_to_i32(top)?,
            float_to_i32(right)?,
            float_to_i32(bottom)?,
        ],
        texture: command.TextureId,
    }))
}

fn float_to_u32_ceil(value: f32) -> Result<u32, RendererError> {
    if value >= 4_294_967_296.0 {
        return Err(RendererError::DrawDataOverflow);
    }
    Ok(value.ceil() as u32)
}

fn float_to_i32(value: f32) -> Result<i32, RendererError> {
    if value < i32::MIN as f32 || value >= 2_147_483_648.0 {
        return Err(RendererError::DrawDataOverflow);
    }
    Ok(value as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw_data() -> sys::ImDrawData {
        sys::ImDrawData {
            Valid: true,
            DisplayPos: sys::ImVec2 { x: 10.0, y: 20.0 },
            DisplaySize: sys::ImVec2 { x: 100.0, y: 50.0 },
            FramebufferScale: sys::ImVec2 { x: 2.0, y: 2.0 },
            ..sys::ImDrawData::default()
        }
    }

    #[test]
    fn translates_clip_and_both_draw_offsets() {
        let data = draw_data();
        let geometry = validate_geometry(&data).expect("geometry is valid");
        let command = sys::ImDrawCmd {
            ClipRect: sys::ImVec4 {
                x: 12.25,
                y: 23.25,
                z: 30.25,
                w: 40.25,
            },
            VtxOffset: 9,
            IdxOffset: 7,
            ElemCount: 6,
            TextureId: std::ptr::dangling_mut::<c_void>(),
            ..sys::ImDrawCmd::default()
        };
        let translated = translate_command(&command, geometry, 100, 200)
            .expect("translation succeeds")
            .expect("clip is visible");
        assert_eq!(translated.kind, CommandKind::Draw);
        assert_eq!(translated.start_index, 207);
        assert_eq!(translated.base_vertex, 109);
        assert_eq!(translated.scissor, [4, 6, 41, 41]);
    }

    #[test]
    fn rejects_offset_overflow() {
        let geometry = validate_geometry(&draw_data()).expect("geometry is valid");
        let command = sys::ImDrawCmd {
            IdxOffset: 1,
            ElemCount: 1,
            ClipRect: sys::ImVec4 {
                x: 10.0,
                y: 20.0,
                z: 11.0,
                w: 21.0,
            },
            ..sys::ImDrawCmd::default()
        };
        assert!(matches!(
            translate_command(&command, geometry, 0, u32::MAX as usize),
            Err(RendererError::DrawDataOverflow)
        ));
    }

    #[test]
    fn recognizes_reset_render_state_sentinel() {
        // SAFETY: This value is compared by address and is never invoked.
        let reset = unsafe {
            std::mem::transmute::<
                usize,
                unsafe extern "C" fn(*const sys::ImDrawList, *const sys::ImDrawCmd),
            >(RESET_RENDER_STATE_CALLBACK)
        };
        let command = sys::ImDrawCmd {
            UserCallback: Some(reset),
            ..sys::ImDrawCmd::default()
        };
        let translated = translate_command(
            &command,
            validate_geometry(&draw_data()).expect("geometry is valid"),
            0,
            0,
        )
        .expect("translation succeeds")
        .expect("callbacks are retained");
        assert_eq!(translated.kind, CommandKind::ResetState);
    }

    #[test]
    fn invalid_totals_fail_before_pointer_walk() {
        let mut data = draw_data();
        data.CmdListsCount = 1;
        data.CmdLists = std::ptr::null_mut();
        assert!(matches!(
            validate_totals(&data),
            Err(RendererError::InvalidDrawData)
        ));
    }
}
