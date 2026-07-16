//! Narrow compatibility boundary for native addons compiled against Dear ImGui 1.80.
//!
//! Nexus-owned rendering code consumes draw data itself. This crate exists only
//! because existing addons exchange a real `ImGuiContext*`, allocator pointers,
//! font pointers, and 1.80-specific structures across the binary ABI.

use core::ffi::CStr;

/// Exact Dear ImGui numeric version required by the native addon contract.
pub const IMGUI_VERSION_NUM: u32 = 18_000;

/// Raw Dear ImGui 1.80 bindings used only at the compatibility boundary.
pub use imgui_sys as sys;

/// Returns the version string compiled into the compatibility library.
#[must_use]
pub fn linked_version() -> Option<&'static CStr> {
    // SAFETY: calling this Dear ImGui accessor requires no context or arguments.
    let version = unsafe { sys::igGetVersion() };
    if version.is_null() {
        None
    } else {
        // SAFETY: Dear ImGui returns a process-lifetime NUL-terminated static
        // string, and the null case was rejected above.
        Some(unsafe { CStr::from_ptr(version) })
    }
}

/// Returns whether the linked library is the exact ABI version Nexus requires.
#[must_use]
pub fn has_expected_version() -> bool {
    linked_version().is_some_and(|version| version.to_bytes() == b"1.80")
}

#[cfg(test)]
mod tests {
    use core::mem::{align_of, offset_of, size_of};

    use super::{IMGUI_VERSION_NUM, has_expected_version, linked_version, sys};

    #[test]
    fn links_exact_dear_imgui_180() {
        assert_eq!(IMGUI_VERSION_NUM, 18_000);
        assert!(has_expected_version());
        assert_eq!(
            linked_version()
                .unwrap_or_else(|| panic!("Dear ImGui returned a null version string"))
                .to_bytes(),
            b"1.80"
        );
    }

    #[test]
    fn draw_vertex_and_index_layouts_match_native_addons() {
        assert_eq!(size_of::<sys::ImDrawVert>(), 20);
        assert_eq!(align_of::<sys::ImDrawVert>(), 4);
        assert_eq!(offset_of!(sys::ImDrawVert, pos), 0);
        assert_eq!(offset_of!(sys::ImDrawVert, uv), 8);
        assert_eq!(offset_of!(sys::ImDrawVert, col), 16);
        assert_eq!(size_of::<sys::ImDrawIdx>(), 2);
    }
}
