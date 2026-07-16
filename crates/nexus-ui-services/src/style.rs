//! Dear ImGui 1.80 style persistence compatible with legacy Nexus settings and
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
const DEFAULT_LAYOUT_CODE: &str = include_str!("../tests/fixtures/default-layout.imstyle180");
const NEXUS_CODE: &str = include_str!("../tests/fixtures/nexus.imstyle180");
const ARC_STYLE_CODE: &str = include_str!("../tests/fixtures/arc-style-part.b64");
const ARC_COLOR_CODE: &str = include_str!("../tests/fixtures/arc-colors-part.b64");

const _: () = assert!(std::mem::size_of::<sys::ImGuiStyle>() == IMGUI_STYLE_180_BYTES);
const _: () = assert!(ARC_STYLE_PREFIX_BYTES + ARC_COLOR_BYTES == IMGUI_STYLE_180_BYTES);

/// Exact opaque bytes persisted by the C++ Nexus host.
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

/// Built-in styles shipped by the C++ host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmbeddedStyle {
    /// Legacy default field layout used before applying ImGui palettes.
    DefaultLayout,
    /// Nexus dark preset.
    Nexus,
    /// ArcDPS default style and color pair.
    ArcDpsDefault,
}

/// Dear ImGui color palette applied after the legacy default layout.
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

    /// Applies one of the embedded legacy presets.
    pub fn apply_embedded(&mut self, style: EmbeddedStyle) -> Result<(), StyleError> {
        let blob = match style {
            EmbeddedStyle::DefaultLayout => StyleBlob::decode(DEFAULT_LAYOUT_CODE)?,
            EmbeddedStyle::Nexus => StyleBlob::decode(NEXUS_CODE)?,
            EmbeddedStyle::ArcDpsDefault => decode_arc_parts(ARC_STYLE_CODE, ARC_COLOR_CODE)?,
        };
        self.backend.apply(&blob)?;
        Ok(())
    }

    /// Applies the legacy default layout followed by an ImGui color palette.
    pub fn apply_palette(&mut self, palette: Palette) -> Result<(), StyleError> {
        self.apply_embedded(EmbeddedStyle::DefaultLayout)?;
        self.backend.apply_palette(palette)?;
        Ok(())
    }

    /// Applies the user setting, falling back to the Nexus preset when missing.
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

    use super::{
        EmbeddedStyle, IMGUI_STYLE_180_BYTES, IMGUI_STYLE_SETTING, Palette, StyleBackend,
        StyleBackendError, StyleBlob, StyleIoError, StyleService, StyleSettings, StyleStorage,
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
    fn golden_legacy_nexus_style_round_trips_byte_for_byte() {
        let golden = include_str!("../tests/fixtures/nexus.imstyle180").trim();
        let blob = StyleBlob::decode(golden)
            .unwrap_or_else(|error| panic!("golden style failed: {error}"));
        assert_eq!(blob.as_bytes().len(), IMGUI_STYLE_180_BYTES);
        assert_eq!(blob.encode(), golden);
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
    fn missing_user_style_falls_back_to_embedded_nexus() {
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
    fn arc_default_parts_reconstruct_a_complete_style() {
        let mut service = StyleService::new(Backend::default());
        assert!(service.apply_embedded(EmbeddedStyle::ArcDpsDefault).is_ok());
        assert_eq!(
            service.into_backend().current.as_bytes().len(),
            IMGUI_STYLE_180_BYTES
        );
    }

    #[test]
    fn incompatible_style_length_is_rejected_before_backend_mutation() {
        assert!(StyleBlob::decode("AAAA").is_err());
    }
}
