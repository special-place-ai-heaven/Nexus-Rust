//! Dear ImGui 1.80 style persistence compatible with Nexus settings and
//! `.imstyle180` preset files.

use std::fs;
use std::marker::PhantomData;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use nexus_imgui_compat::sys;
use thiserror::Error;

/// Legacy settings key containing a base64-encoded `ImGuiStyle` 1.80 blob.
pub const IMGUI_STYLE_SETTING: &str = "ImGuiStyle";
/// Extension used by legacy style preset files.
pub const STYLE_FILE_EXTENSION: &str = "imstyle180";
/// Exact x64 byte size of Dear ImGui 1.80's `ImGuiStyle`.
pub const IMGUI_STYLE_180_BYTES: usize = 1_044;

const ARC_STYLE_PREFIX_BYTES: usize = 196;
const ARC_COLOR_BYTES: usize = 848;

const _: () = assert!(std::mem::size_of::<sys::ImGuiStyle>() == IMGUI_STYLE_180_BYTES);
const _: () = assert!(ARC_STYLE_PREFIX_BYTES + ARC_COLOR_BYTES == IMGUI_STYLE_180_BYTES);

/// ABI-compatible opaque bytes persisted by Nexus settings and presets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StyleBlob([u8; IMGUI_STYLE_180_BYTES]);

impl StyleBlob {
    /// Validates exact Dear ImGui 1.80 size and copies the bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, StyleError> {
        let bytes: [u8; IMGUI_STYLE_180_BYTES] =
            bytes.try_into().map_err(|_| StyleError::IncompatibleBlob)?;
        Ok(Self(bytes))
    }

    /// Decodes the legacy standard-base64 representation.
    pub fn decode(code: &str) -> Result<Self, StyleError> {
        let code = trim_line_ending(code);
        let bytes = STANDARD.decode(code).map_err(|_| StyleError::InvalidCode)?;
        Self::from_bytes(&bytes)
    }

    /// Encodes the exact representation stored in `Settings.json` and presets.
    #[must_use]
    pub fn encode(&self) -> String {
        STANDARD.encode(self.as_bytes())
    }

    /// Returns the opaque bytes for synchronous backend application.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; IMGUI_STYLE_180_BYTES] {
        &self.0
    }
}

fn generated_default_style() -> StyleBlob {
    let mut storage = Box::new(std::mem::MaybeUninit::<sys::ImGuiStyle>::zeroed());
    // SAFETY: every field in `ImGuiStyle` is a float, integer, boolean, or an
    // array of those types, so the all-zero representation is valid. Starting
    // from zero also gives deterministic padding bytes for persistence.
    let style = unsafe { storage.assume_init_mut() };
    style.Alpha = 1.0;
    style.WindowPadding = sys::ImVec2 { x: 8.0, y: 8.0 };
    style.WindowRounding = 0.0;
    style.WindowBorderSize = 1.0;
    style.WindowMinSize = sys::ImVec2 { x: 32.0, y: 32.0 };
    style.WindowTitleAlign = sys::ImVec2 { x: 0.0, y: 0.5 };
    style.WindowMenuButtonPosition = sys::ImGuiDir_Left;
    style.ChildRounding = 0.0;
    style.ChildBorderSize = 1.0;
    style.PopupRounding = 0.0;
    style.PopupBorderSize = 1.0;
    style.FramePadding = sys::ImVec2 { x: 4.0, y: 3.0 };
    style.FrameRounding = 0.0;
    style.FrameBorderSize = 0.0;
    style.ItemSpacing = sys::ImVec2 { x: 8.0, y: 4.0 };
    style.ItemInnerSpacing = sys::ImVec2 { x: 4.0, y: 4.0 };
    style.CellPadding = sys::ImVec2 { x: 4.0, y: 2.0 };
    style.TouchExtraPadding = sys::ImVec2 { x: 0.0, y: 0.0 };
    style.IndentSpacing = 21.0;
    style.ColumnsMinSpacing = 6.0;
    style.ScrollbarSize = 14.0;
    style.ScrollbarRounding = 9.0;
    style.GrabMinSize = 10.0;
    style.GrabRounding = 0.0;
    style.LogSliderDeadzone = 4.0;
    style.TabRounding = 4.0;
    style.TabBorderSize = 0.0;
    style.TabMinWidthForCloseButton = 0.0;
    style.ColorButtonPosition = sys::ImGuiDir_Right;
    style.ButtonTextAlign = sys::ImVec2 { x: 0.5, y: 0.5 };
    style.SelectableTextAlign = sys::ImVec2 { x: 0.0, y: 0.0 };
    style.DisplayWindowPadding = sys::ImVec2 { x: 19.0, y: 19.0 };
    style.DisplaySafeAreaPadding = sys::ImVec2 { x: 3.0, y: 3.0 };
    style.MouseCursorScale = 1.0;
    style.AntiAliasedLines = true;
    style.AntiAliasedLinesUseTex = true;
    style.AntiAliasedFill = true;
    style.CurveTessellationTol = 1.25;
    style.CircleSegmentMaxError = 1.60;
    // SAFETY: `style` is a live, uniquely borrowed destination. Dear ImGui's
    // palette helper does not require a current context when one is provided.
    unsafe { sys::igStyleColorsDark(style) };

    let mut bytes = [0_u8; IMGUI_STYLE_180_BYTES];
    // SAFETY: the compile-time size assertion above proves both buffers have
    // the same length. `style` is fully initialized and remains live here.
    unsafe {
        std::ptr::copy_nonoverlapping(
            std::ptr::from_ref(style).cast::<u8>(),
            bytes.as_mut_ptr(),
            IMGUI_STYLE_180_BYTES,
        );
    }
    StyleBlob(bytes)
}

/// Stable compatibility names for built-in host styles.
///
/// All three currently resolve to a freshly generated Dear ImGui 1.80 dark
/// default. The original preset blobs are intentionally not redistributed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmbeddedStyle {
    /// Fresh Dear ImGui 1.80 default layout and dark colors.
    DefaultLayout,
    /// Nexus-compatible dark fallback.
    Nexus,
    /// ArcDPS-compatible dark fallback.
    ArcDpsDefault,
}

/// Dear ImGui color palette applied after a fresh default layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Palette {
    /// `StyleColorsClassic`.
    Classic,
    /// `StyleColorsLight`.
    Light,
    /// `StyleColorsDark`.
    Dark,
}

/// Closed failures from the current-context ImGui backend.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum StyleBackendError {
    /// No compatible current context is available.
    #[error("Dear ImGui style context is unavailable")]
    ContextUnavailable,
    /// Style capture failed.
    #[error("Dear ImGui style capture failed")]
    CaptureFailed,
    /// Style application failed.
    #[error("Dear ImGui style application failed")]
    ApplyFailed,
}

/// Thread-local Dear ImGui style integration.
pub trait StyleBackend {
    /// Copies the current exact 1.80 style bytes.
    fn capture(&mut self) -> Result<StyleBlob, StyleBackendError>;

    /// Replaces the current exact 1.80 style bytes.
    fn apply(&mut self, style: &StyleBlob) -> Result<(), StyleBackendError>;

    /// Applies one Dear ImGui color palette without changing other fields.
    fn apply_palette(&mut self, palette: Palette) -> Result<(), StyleBackendError>;
}

/// Closed, path-free settings and preset I/O failures.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum StyleIoError {
    /// The settings backend could not read or write the style key.
    #[error("style setting is unavailable")]
    SettingsUnavailable,
    /// A preset name was empty, escaped the style directory, or had the wrong extension.
    #[error("style preset name is invalid")]
    InvalidPresetName,
    /// A preset could not be read.
    #[error("style preset could not be read")]
    PresetUnreadable,
    /// A preset could not be written.
    #[error("style preset could not be written")]
    PresetUnwritable,
    /// ArcDPS appearance data is unavailable.
    #[error("ArcDPS style data is unavailable")]
    ArcStyleUnavailable,
}

/// Combined style operation failure.
#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum StyleError {
    /// Input is not valid standard base64.
    #[error("style code is not valid base64")]
    InvalidCode,
    /// Decoded data is not an exact x64 Dear ImGui 1.80 style.
    #[error("style is not compatible with Dear ImGui 1.80")]
    IncompatibleBlob,
    /// Backend operation failed.
    #[error(transparent)]
    Backend(#[from] StyleBackendError),
    /// Settings, preset, or ArcDPS I/O failed.
    #[error(transparent)]
    Io(#[from] StyleIoError),
}

/// Narrow settings interface for the legacy `ImGuiStyle` string value.
pub trait StyleSettings {
    /// Reads a string setting without inserting a default.
    fn get_string(&mut self, key: &str) -> Result<Option<String>, StyleIoError>;

    /// Persists a string setting in the existing JSON settings store.
    fn set_string(&mut self, key: &str, value: &str) -> Result<(), StyleIoError>;
}

/// Storage for one-line standard-base64 `.imstyle180` presets.
pub trait StyleStorage {
    /// Loads the first line of a preset.
    fn load_code(&mut self, name: &str) -> Result<String, StyleIoError>;

    /// Overwrites a preset with one base64 line.
    fn save_code(&mut self, name: &str, code: &str) -> Result<(), StyleIoError>;
}

/// Directory-backed legacy preset storage with traversal-safe names.
#[derive(Clone, Debug)]
pub struct DirectoryStyleStorage {
    directory: PathBuf,
}

impl DirectoryStyleStorage {
    /// Creates storage under the configured Nexus styles directory.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    fn resolve(&self, name: &str) -> Result<PathBuf, StyleIoError> {
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(StyleIoError::InvalidPresetName);
        }
        let input = Path::new(name);
        let mut components = input.components();
        let Some(Component::Normal(_)) = components.next() else {
            return Err(StyleIoError::InvalidPresetName);
        };
        if components.next().is_some() {
            return Err(StyleIoError::InvalidPresetName);
        }

        let mut filename = input.to_path_buf();
        match filename.extension().and_then(|value| value.to_str()) {
            None => {
                filename.set_extension(STYLE_FILE_EXTENSION);
            }
            Some(STYLE_FILE_EXTENSION) => {}
            Some(_) => return Err(StyleIoError::InvalidPresetName),
        }
        Ok(self.directory.join(filename))
    }
}

impl StyleStorage for DirectoryStyleStorage {
    fn load_code(&mut self, name: &str) -> Result<String, StyleIoError> {
        let path = self.resolve(name)?;
        let contents = fs::read_to_string(path).map_err(|_| StyleIoError::PresetUnreadable)?;
        contents
            .lines()
            .next()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .ok_or(StyleIoError::PresetUnreadable)
    }

    fn save_code(&mut self, name: &str, code: &str) -> Result<(), StyleIoError> {
        let path = self.resolve(name)?;
        fs::create_dir_all(&self.directory).map_err(|_| StyleIoError::PresetUnwritable)?;
        fs::write(path, trim_line_ending(code)).map_err(|_| StyleIoError::PresetUnwritable)
    }
}

/// Base64 fields read from ArcDPS's `appearance_imgui_*180` settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArcStyleParts {
    /// First 196 bytes of `ImGuiStyle` before `Colors`.
    pub style: String,
    /// 53 `ImVec4` color entries (848 bytes).
    pub colors: String,
}

/// Injected reader for the current ArcDPS appearance settings.
pub trait ArcStyleSource {
    /// Returns current style fields, or `None` when ArcDPS is absent.
    fn current(&mut self) -> Result<Option<ArcStyleParts>, StyleIoError>;
}

/// Thread-bound style coordinator.
pub struct StyleService<B> {
    backend: B,
    _thread_bound: PhantomData<Rc<()>>,
}

impl<B: StyleBackend> StyleService<B> {
    /// Creates a style service for the runtime's Dear ImGui 1.80 context.
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            _thread_bound: PhantomData,
        }
    }

    /// Applies an exact base64 style code.
    pub fn apply_code(&mut self, code: &str) -> Result<(), StyleError> {
        self.backend.apply(&StyleBlob::decode(code)?)?;
        Ok(())
    }

    /// Applies a self-contained style selected by a stable compatibility name.
    pub fn apply_embedded(&mut self, style: EmbeddedStyle) -> Result<(), StyleError> {
        let _compatibility_name = style;
        let blob = generated_default_style();
        self.backend.apply(&blob)?;
        Ok(())
    }

    /// Applies a fresh default layout followed by an ImGui color palette.
    pub fn apply_palette(&mut self, palette: Palette) -> Result<(), StyleError> {
        self.apply_embedded(EmbeddedStyle::DefaultLayout)?;
        self.backend.apply_palette(palette)?;
        Ok(())
    }

    /// Applies the user setting, falling back to the generated dark style.
    pub fn apply_user(&mut self, settings: &mut impl StyleSettings) -> Result<(), StyleError> {
        match settings.get_string(IMGUI_STYLE_SETTING)? {
            Some(code) if !code.is_empty() => self.apply_code(&code),
            _ => self.apply_embedded(EmbeddedStyle::Nexus),
        }
    }

    /// Captures and persists the current style under the legacy settings key.
    pub fn save_user(&mut self, settings: &mut impl StyleSettings) -> Result<String, StyleError> {
        let code = self.capture_code()?;
        settings.set_string(IMGUI_STYLE_SETTING, &code)?;
        Ok(code)
    }

    /// Loads and applies a `.imstyle180` preset.
    pub fn import_preset(
        &mut self,
        storage: &mut impl StyleStorage,
        name: &str,
    ) -> Result<(), StyleError> {
        let code = storage.load_code(name)?;
        self.apply_code(&code)
    }

    /// Captures and writes a `.imstyle180` preset.
    pub fn export_preset(
        &mut self,
        storage: &mut impl StyleStorage,
        name: &str,
    ) -> Result<String, StyleError> {
        let code = self.capture_code()?;
        storage.save_code(name, &code)?;
        Ok(code)
    }

    /// Applies current ArcDPS 1.80 style fields.
    pub fn apply_arc_current(
        &mut self,
        source: &mut impl ArcStyleSource,
    ) -> Result<(), StyleError> {
        let parts = source.current()?.ok_or(StyleIoError::ArcStyleUnavailable)?;
        let blob = decode_arc_parts(&parts.style, &parts.colors)?;
        self.backend.apply(&blob)?;
        Ok(())
    }

    /// Captures the current style in the legacy standard-base64 format.
    pub fn capture_code(&mut self) -> Result<String, StyleError> {
        Ok(self.backend.capture()?.encode())
    }

    /// Returns the injected backend after service shutdown.
    #[must_use]
    pub fn into_backend(self) -> B {
        self.backend
    }
}

fn decode_arc_parts(style: &str, colors: &str) -> Result<StyleBlob, StyleError> {
    let style = STANDARD
        .decode(trim_line_ending(style))
        .map_err(|_| StyleError::InvalidCode)?;
    let colors = STANDARD
        .decode(trim_line_ending(colors))
        .map_err(|_| StyleError::InvalidCode)?;
    if style.len() != ARC_STYLE_PREFIX_BYTES || colors.len() != ARC_COLOR_BYTES {
        return Err(StyleError::IncompatibleBlob);
    }
    let mut combined = [0_u8; IMGUI_STYLE_180_BYTES];
    combined[..ARC_STYLE_PREFIX_BYTES].copy_from_slice(&style);
    combined[ARC_STYLE_PREFIX_BYTES..].copy_from_slice(&colors);
    Ok(StyleBlob(combined))
}

fn trim_line_ending(value: &str) -> &str {
    value.trim_end_matches(['\r', '\n'])
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    use super::{
        ARC_STYLE_PREFIX_BYTES, EmbeddedStyle, IMGUI_STYLE_180_BYTES, IMGUI_STYLE_SETTING, Palette,
        StyleBackend, StyleBackendError, StyleBlob, StyleIoError, StyleService, StyleSettings,
        StyleStorage, decode_arc_parts, generated_default_style,
    };

    #[derive(Clone)]
    struct Backend {
        current: StyleBlob,
        palettes: Vec<Palette>,
        applications: usize,
    }

    impl Default for Backend {
        fn default() -> Self {
            Self {
                current: StyleBlob([0; IMGUI_STYLE_180_BYTES]),
                palettes: Vec::new(),
                applications: 0,
            }
        }
    }

    impl StyleBackend for Backend {
        fn capture(&mut self) -> Result<StyleBlob, StyleBackendError> {
            Ok(self.current.clone())
        }

        fn apply(&mut self, style: &StyleBlob) -> Result<(), StyleBackendError> {
            self.current = style.clone();
            self.applications += 1;
            Ok(())
        }

        fn apply_palette(&mut self, palette: Palette) -> Result<(), StyleBackendError> {
            self.palettes.push(palette);
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemoryIo {
        values: BTreeMap<String, String>,
    }

    impl StyleSettings for MemoryIo {
        fn get_string(&mut self, key: &str) -> Result<Option<String>, StyleIoError> {
            Ok(self.values.get(key).cloned())
        }

        fn set_string(&mut self, key: &str, value: &str) -> Result<(), StyleIoError> {
            self.values.insert(key.to_owned(), value.to_owned());
            Ok(())
        }
    }

    impl StyleStorage for MemoryIo {
        fn load_code(&mut self, name: &str) -> Result<String, StyleIoError> {
            self.values
                .get(name)
                .cloned()
                .ok_or(StyleIoError::PresetUnreadable)
        }

        fn save_code(&mut self, name: &str, code: &str) -> Result<(), StyleIoError> {
            self.values.insert(name.to_owned(), code.to_owned());
            Ok(())
        }
    }

    #[test]
    fn generated_default_style_round_trips_with_stable_abi_shape() {
        let blob = generated_default_style();
        let code = blob.encode();
        let decoded = StyleBlob::decode(&format!("{code}\r\n"))
            .unwrap_or_else(|error| panic!("generated style failed: {error}"));
        assert_eq!(blob.as_bytes().len(), IMGUI_STYLE_180_BYTES);
        assert!(blob.as_bytes().iter().any(|byte| *byte != 0));
        assert_eq!(decoded, blob);
        assert_eq!(decoded.encode(), code);
    }

    #[test]
    fn user_setting_and_preset_use_the_same_legacy_code() {
        let mut service = StyleService::new(Backend::default());
        assert!(service.apply_embedded(EmbeddedStyle::Nexus).is_ok());
        let expected = service
            .capture_code()
            .unwrap_or_else(|error| panic!("capture failed: {error}"));
        let mut io = MemoryIo::default();
        let saved = service
            .save_user(&mut io)
            .unwrap_or_else(|error| panic!("save failed: {error}"));
        assert_eq!(saved, expected);
        assert_eq!(io.values.get(IMGUI_STYLE_SETTING), Some(&expected));
        let exported = service
            .export_preset(&mut io, "preset.imstyle180")
            .unwrap_or_else(|error| panic!("export failed: {error}"));
        assert_eq!(exported, expected);
        assert!(service.import_preset(&mut io, "preset.imstyle180").is_ok());
    }

    #[test]
    fn missing_user_style_falls_back_to_generated_dark() {
        let mut service = StyleService::new(Backend::default());
        assert!(service.apply_user(&mut MemoryIo::default()).is_ok());
        assert_eq!(service.into_backend().applications, 1);
    }

    #[test]
    fn palette_first_restores_the_legacy_default_layout() {
        let mut service = StyleService::new(Backend::default());
        assert!(service.apply_palette(Palette::Dark).is_ok());
        let backend = service.into_backend();
        assert_eq!(backend.applications, 1);
        assert_eq!(backend.palettes, vec![Palette::Dark]);
    }

    #[test]
    fn compatibility_names_use_the_generated_fallback() {
        let expected = generated_default_style();
        for name in [
            EmbeddedStyle::DefaultLayout,
            EmbeddedStyle::Nexus,
            EmbeddedStyle::ArcDpsDefault,
        ] {
            let mut service = StyleService::new(Backend::default());
            assert!(service.apply_embedded(name).is_ok());
            let backend = service.into_backend();
            assert_eq!(backend.current, expected);
            assert_eq!(backend.applications, 1);
        }
    }

    #[test]
    fn arc_style_parts_reconstruct_an_arbitrary_complete_style() {
        let expected = generated_default_style();
        let style = STANDARD.encode(&expected.as_bytes()[..ARC_STYLE_PREFIX_BYTES]);
        let colors = STANDARD.encode(&expected.as_bytes()[ARC_STYLE_PREFIX_BYTES..]);
        let decoded = decode_arc_parts(&style, &colors)
            .unwrap_or_else(|error| panic!("Arc style reconstruction failed: {error}"));
        assert_eq!(decoded, expected);
    }

    #[test]
    fn incompatible_style_length_is_rejected_before_backend_mutation() {
        assert!(StyleBlob::decode("AAAA").is_err());
    }
}
