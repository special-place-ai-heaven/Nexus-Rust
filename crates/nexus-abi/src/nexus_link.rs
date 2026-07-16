use core::ffi::c_void;

/// Shared `DL_NEXUS_LINK` resource exposed to add-ons.
///
/// Boolean values are stored as bytes to match MSVC `bool` while avoiding
/// invalid Rust `bool` values if an external module writes corrupt data.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NexusLinkData {
    /// Current swap-chain width in pixels.
    pub width: u32,
    /// Current swap-chain height in pixels.
    pub height: u32,
    /// Effective UI scale.
    pub scaling: f32,
    /// Non-zero while the player character is moving.
    pub is_moving: u8,
    /// Non-zero while the camera is moving.
    pub is_camera_moving: u8,
    /// Non-zero while gameplay input should be active.
    pub is_gameplay: u8,
    /// Current regular Dear ImGui font (`ImFont*`).
    pub font: *mut c_void,
    /// Current large Dear ImGui font (`ImFont*`).
    pub font_big: *mut c_void,
    /// Current UI Dear ImGui font (`ImFont*`).
    pub font_ui: *mut c_void,
    /// Number of registered quick-access icons.
    pub quick_access_icons_count: i32,
    /// Active quick-access layout mode.
    pub quick_access_mode: i32,
    /// Non-zero when quick access is vertical.
    pub quick_access_is_vertical: u8,
}

#[cfg(test)]
mod tests {
    use core::mem::{align_of, offset_of, size_of};

    use super::NexusLinkData;

    #[test]
    fn layout_matches_cpp_nexus_link_data_t_on_x64() {
        assert_eq!(size_of::<NexusLinkData>(), 56);
        assert_eq!(align_of::<NexusLinkData>(), 8);
        assert_eq!(offset_of!(NexusLinkData, width), 0);
        assert_eq!(offset_of!(NexusLinkData, height), 4);
        assert_eq!(offset_of!(NexusLinkData, scaling), 8);
        assert_eq!(offset_of!(NexusLinkData, is_moving), 12);
        assert_eq!(offset_of!(NexusLinkData, is_camera_moving), 13);
        assert_eq!(offset_of!(NexusLinkData, is_gameplay), 14);
        assert_eq!(offset_of!(NexusLinkData, font), 16);
        assert_eq!(offset_of!(NexusLinkData, font_big), 24);
        assert_eq!(offset_of!(NexusLinkData, font_ui), 32);
        assert_eq!(offset_of!(NexusLinkData, quick_access_icons_count), 40);
        assert_eq!(offset_of!(NexusLinkData, quick_access_mode), 44);
        assert_eq!(offset_of!(NexusLinkData, quick_access_is_vertical), 48);
    }
}
