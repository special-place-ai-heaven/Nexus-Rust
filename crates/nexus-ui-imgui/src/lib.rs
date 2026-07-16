//! Dear ImGui 1.80 implementations for renderer-independent Nexus UI services.

use core::ffi::c_void;
use core::ptr::NonNull;
use std::collections::BTreeSet;
use std::ffi::CStr;

use nexus_imgui_compat::sys;
use nexus_ui_services::{
    FontAtlasBackend, FontBackendError, FontBuildRequest, FontConfig, FontHandle, GlyphCoverage,
};

const MAX_STATIC_RANGE_UNITS: usize = 131_072;
const LATIN_EXTENDED_START: u16 = 0x0100;
const LATIN_EXTENDED_END: u16 = 0x024F;

/// Thread-bound Dear ImGui font-atlas builder.
///
/// The caller must make the target ImGui context current and invoke this only
/// before `NewFrame`. Font bytes and generated glyph ranges remain owned by the
/// calling [`nexus_ui_services::FontManager`] through the synchronous build.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImGuiFontAtlasBackend;

impl ImGuiFontAtlasBackend {
    /// Creates a stateless backend for the current Dear ImGui context.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl FontAtlasBackend for ImGuiFontAtlasBackend {
    fn rebuild(
        &mut self,
        fonts: &[FontBuildRequest<'_>],
        localized_texts: &[&CStr],
    ) -> Result<Vec<Option<FontHandle>>, FontBackendError> {
        let lengths = fonts
            .iter()
            .map(|font| i32::try_from(font.data.len()).map_err(|_| FontBackendError::RejectedInput))
            .collect::<Result<Vec<_>, _>>()?;

        // SAFETY: the manager calls this backend only while its live context is
        // current. The IO and atlas pointers are borrowed for this rebuild.
        let io = unsafe { sys::igGetIO().as_mut() }.ok_or(FontBackendError::RejectedInput)?;
        // SAFETY: `io` belongs to the current context for this call.
        let atlas = unsafe { io.Fonts.as_mut() }.ok_or(FontBackendError::RejectedInput)?;
        let localized_ranges = build_coverage_ranges(atlas, localized_texts, false)?;
        let host_ranges = build_coverage_ranges(atlas, localized_texts, true)?;

        // SAFETY: the atlas is live and no ImGui frame has started. Registered
        // font bytes remain owned by the manager, so every native config below
        // explicitly disables atlas ownership.
        unsafe { sys::ImFontAtlas_Clear(atlas) };
        if fonts.is_empty() {
            // SAFETY: a null config requests Dear ImGui's documented default.
            let fallback = unsafe { sys::ImFontAtlas_AddFontDefault(atlas, core::ptr::null()) };
            // SAFETY: the live atlas is unlocked and contains only the default
            // font just registered above.
            let built = unsafe { sys::ImFontAtlas_Build(atlas) };
            if fallback.is_null() || !built {
                return Err(FontBackendError::BuildFailed);
            }
            return Ok(Vec::new());
        }

        let mut handles = Vec::with_capacity(fonts.len());
        for (font, length) in fonts.iter().zip(lengths) {
            let ranges = match font.coverage {
                GlyphCoverage::Localized => &localized_ranges,
                GlyphCoverage::HostDefault => &host_ranges,
            };
            let Some(mut config) = NativeFontConfig::new(font.identifier, font.config) else {
                recover_default_atlas(atlas);
                return Err(FontBackendError::RejectedInput);
            };
            let config_ref = config.as_mut();
            config_ref.FontDataOwnedByAtlas = false;
            config_ref.GlyphRanges = ranges.as_ptr();
            // SAFETY: the checked byte count fits c_int, data remains live
            // through Build, config is initialized, and ranges are terminated.
            let pointer = unsafe {
                sys::ImFontAtlas_AddFontFromMemoryTTF(
                    atlas,
                    font.data.as_ptr().cast_mut().cast::<c_void>(),
                    length,
                    font.size,
                    config.as_ptr(),
                    ranges.as_ptr(),
                )
            };
            // SAFETY: a non-null return is owned by this live atlas until the
            // manager's mandatory invalidation notification before rebuilding.
            let handle = unsafe { FontHandle::from_ptr(pointer) };
            if handle.is_none() {
                recover_default_atlas(atlas);
                return Err(FontBackendError::RejectedInput);
            }
            handles.push(handle);
        }

        // SAFETY: every font input and range pointer remains live through this
        // synchronous atlas rasterization.
        if !unsafe { sys::ImFontAtlas_Build(atlas) } {
            recover_default_atlas(atlas);
            return Err(FontBackendError::BuildFailed);
        }
        Ok(handles)
    }
}

struct NativeFontConfig(NonNull<sys::ImFontConfig>);

impl NativeFontConfig {
    fn new(identifier: &CStr, source: &FontConfig) -> Option<Self> {
        // SAFETY: constructor returns an owned native configuration or null.
        let mut pointer = NonNull::new(unsafe { sys::ImFontConfig_ImFontConfig() })?;
        // SAFETY: this allocation is uniquely owned until Drop.
        let target = unsafe { pointer.as_mut() };
        target.FontNo = source.font_no;
        target.OversampleH = source.oversample_h;
        target.OversampleV = source.oversample_v;
        target.PixelSnapH = source.pixel_snap_h;
        target.GlyphExtraSpacing = sys::ImVec2 {
            x: source.glyph_extra_spacing[0],
            y: source.glyph_extra_spacing[1],
        };
        target.GlyphOffset = sys::ImVec2 {
            x: source.glyph_offset[0],
            y: source.glyph_offset[1],
        };
        target.GlyphMinAdvanceX = source.glyph_min_advance_x;
        target.GlyphMaxAdvanceX = source.glyph_max_advance_x;
        target.MergeMode = source.merge_mode;
        target.RasterizerFlags = source.rasterizer_flags;
        target.RasterizerMultiply = source.rasterizer_multiply;
        target.EllipsisChar = source.ellipsis_char;
        for (destination, byte) in target.Name.iter_mut().take(39).zip(identifier.to_bytes()) {
            *destination = *byte as _;
        }
        Some(Self(pointer))
    }

    fn as_ptr(&self) -> *const sys::ImFontConfig {
        self.0.as_ptr()
    }

    fn as_mut(&mut self) -> &mut sys::ImFontConfig {
        // SAFETY: the native allocation remains uniquely owned by this guard.
        unsafe { self.0.as_mut() }
    }
}

impl Drop for NativeFontConfig {
    fn drop(&mut self) {
        // SAFETY: this pointer was returned by the matching constructor and
        // has not been transferred to the atlas (which copies the config).
        unsafe { sys::ImFontConfig_destroy(self.0.as_ptr()) };
    }
}

fn build_coverage_ranges(
    atlas: &mut sys::ImFontAtlas,
    localized_texts: &[&CStr],
    host_default: bool,
) -> Result<Vec<u16>, FontBackendError> {
    let mut characters = BTreeSet::new();
    // SAFETY: these functions return static, zero-terminated range pairs owned
    // by the live atlas implementation.
    unsafe {
        add_static_ranges(
            &mut characters,
            sys::ImFontAtlas_GetGlyphRangesDefault(atlas),
        )?;
    }
    characters.extend(LATIN_EXTENDED_START..=LATIN_EXTENDED_END);
    if host_default {
        // SAFETY: same static range contract as above.
        unsafe {
            add_static_ranges(
                &mut characters,
                sys::ImFontAtlas_GetGlyphRangesChineseFull(atlas),
            )?;
            add_static_ranges(
                &mut characters,
                sys::ImFontAtlas_GetGlyphRangesCyrillic(atlas),
            )?;
        }
    }
    for text in localized_texts {
        for character in String::from_utf8_lossy(text.to_bytes()).chars() {
            let value = u32::from(character);
            if let Ok(value) = u16::try_from(value) {
                characters.insert(value);
            }
        }
    }
    Ok(collapse_ranges(characters))
}

unsafe fn add_static_ranges(
    characters: &mut BTreeSet<u16>,
    ranges: *const sys::ImWchar,
) -> Result<(), FontBackendError> {
    if ranges.is_null() {
        return Err(FontBackendError::RejectedInput);
    }
    for offset in (0..MAX_STATIC_RANGE_UNITS).step_by(2) {
        // SAFETY: the caller establishes a static zero-terminated range table;
        // the fixed bound prevents an unbounded walk if the contract is broken.
        let start = unsafe { ranges.add(offset).read() };
        if start == 0 {
            return Ok(());
        }
        // SAFETY: a nonzero start is followed by its inclusive end in ImGui's
        // documented range-pair representation.
        let end = unsafe { ranges.add(offset + 1).read() };
        if end < start {
            return Err(FontBackendError::RejectedInput);
        }
        characters.extend(start..=end);
    }
    Err(FontBackendError::RejectedInput)
}

fn collapse_ranges(characters: BTreeSet<u16>) -> Vec<u16> {
    let mut output = Vec::new();
    let mut iterator = characters.into_iter();
    let Some(mut start) = iterator.next() else {
        return vec![0];
    };
    let mut end = start;
    for character in iterator {
        if end.checked_add(1) == Some(character) {
            end = character;
        } else {
            output.extend([start, end]);
            start = character;
            end = character;
        }
    }
    output.extend([start, end, 0]);
    output
}

fn recover_default_atlas(atlas: *mut sys::ImFontAtlas) {
    // SAFETY: best-effort recovery runs with the same live, unlocked atlas.
    unsafe {
        sys::ImFontAtlas_Clear(atlas);
        let _ = sys::ImFontAtlas_AddFontDefault(atlas, core::ptr::null());
        let _ = sys::ImFontAtlas_Build(atlas);
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::sync::{Mutex, MutexGuard};

    use nexus_imgui_runtime::ImGuiContextOwner;
    use nexus_ui_services::{FontAtlasBackend, FontBuildRequest, FontConfig, GlyphCoverage};

    use super::{ImGuiFontAtlasBackend, collapse_ranges};

    static CONTEXT_LOCK: Mutex<()> = Mutex::new(());

    fn context_lock() -> MutexGuard<'static, ()> {
        CONTEXT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn range_collapse_is_sorted_contiguous_and_terminated() {
        let values = [0x41, 0x42, 0x44, 0x100]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            collapse_ranges(values),
            vec![0x41, 0x42, 0x44, 0x44, 0x100, 0x100, 0]
        );
    }

    #[test]
    fn real_imgui_180_context_builds_an_owned_memory_font() {
        let _lock = context_lock();
        let mut owner = ImGuiContextOwner::create().expect("test context should be available");
        let identifier = CString::new("FONT_DEFAULT").expect("static identifier is valid");
        let localized = CString::new("Zażółć gęślą jaźń").expect("fixture text is valid");
        let request = FontBuildRequest {
            identifier: &identifier,
            data: include_bytes!("../../../res/Fonts/FiraSans.ttf"),
            size: 16.0,
            config: &FontConfig::default(),
            coverage: GlyphCoverage::HostDefault,
        };
        let mut backend = ImGuiFontAtlasBackend::new();
        let handles = owner.with_current(|| {
            backend
                .rebuild(&[request], &[localized.as_c_str()])
                .expect("valid embedded font should build")
        });
        assert_eq!(handles.len(), 1);
        assert!(handles[0].is_some());
    }
}
