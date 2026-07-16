#![cfg_attr(not(test), forbid(unsafe_code))]
//! Exact legacy Nexus host fonts with an owned, render-thread rebuild boundary.
//!
//! [`FontRebuildRequest`] is safe to prepare on any thread. It owns optional
//! user-font bytes and exposes borrowed [`CatalogRegistration`] values only for
//! the duration of registration; [`FontManager`] copies every byte and
//! configuration it accepts. Actual atlas work happens solely through
//! [`FontRebuildRequest::apply_pre_new_frame`], which preserves the existing
//! thread-bound manager and pre-`NewFrame` backend contract.
//!
//! ```
//! use nexus_ui_fonts::{DEFAULT_FONT_SIZE, FontRebuildRequest};
//!
//! let request = FontRebuildRequest::new(Some(16.0), None);
//! assert_eq!(request.default_size(), 16.0);
//! assert_eq!(FontRebuildRequest::default().default_size(), DEFAULT_FONT_SIZE);
//! ```

use std::ffi::CStr;
use std::fmt;
use std::sync::Arc;

use nexus_ui_imgui::ImGuiFontAtlasBackend;
use nexus_ui_services::{
    FontAdvance, FontAtlasBackend, FontConfig, FontError, FontHandle, FontManager,
    FontMemoryReplacement, FontOwnerReplaceError, FontOwnerReplaceReport, OwnerId, UiScale,
};
use thiserror::Error;

/// Legacy settings fallback for the default font, in pixels.
pub const DEFAULT_FONT_SIZE: f32 = 15.0;
/// Smallest accepted default-font setting, in pixels.
pub const MIN_DEFAULT_FONT_SIZE: f32 = 1.0;
/// Largest accepted default-font setting, in pixels.
pub const MAX_DEFAULT_FONT_SIZE: f32 = 50.0;
/// Practical host-settings limit for one user-font payload (128 MiB).
///
/// This remains below Dear ImGui's signed byte-count ABI limit and prevents a
/// configured file from triggering an effectively unbounded registry copy.
pub const MAX_USER_FONT_BYTES: usize = 128 * 1024 * 1024;
/// Number of host catalog entries when no user font is configured.
pub const BASE_CATALOG_FONT_COUNT: usize = 13;
/// Number of host catalog entries when user-font merges are configured.
pub const MERGED_CATALOG_FONT_COUNT: usize = 25;

/// Legacy default-font identifier.
pub const FONT_DEFAULT: &str = "FONT_DEFAULT";
/// Legacy small Menomonia identifier.
pub const MENOMONIA_S: &str = "MENOMONIA_S";
/// Legacy small large-title Menomonia identifier.
pub const MENOMONIA_BIG_S: &str = "MENOMONIA_BIG_S";
/// Legacy small Fira Sans UI identifier.
pub const FIRASANS_S: &str = "FIRASANS_S";
/// Legacy normal Menomonia identifier.
pub const MENOMONIA_N: &str = "MENOMONIA_N";
/// Legacy normal large-title Menomonia identifier.
pub const MENOMONIA_BIG_N: &str = "MENOMONIA_BIG_N";
/// Legacy normal Fira Sans UI identifier.
pub const FIRASANS_N: &str = "FIRASANS_N";
/// Legacy large Menomonia identifier.
pub const MENOMONIA_L: &str = "MENOMONIA_L";
/// Legacy large large-title Menomonia identifier.
pub const MENOMONIA_BIG_L: &str = "MENOMONIA_BIG_L";
/// Legacy large Fira Sans UI identifier.
pub const FIRASANS_L: &str = "FIRASANS_L";
/// Legacy larger Menomonia identifier.
pub const MENOMONIA_XL: &str = "MENOMONIA_XL";
/// Legacy larger large-title Menomonia identifier.
pub const MENOMONIA_BIG_XL: &str = "MENOMONIA_BIG_XL";
/// Legacy larger Fira Sans UI identifier.
pub const FIRASANS_XL: &str = "FIRASANS_XL";

const MENOMONIA_S_MERGE: &str = "MENOMONIA_S_MERGE";
const MENOMONIA_BIG_S_MERGE: &str = "MENOMONIA_BIG_S_MERGE";
const FIRASANS_S_MERGE: &str = "FIRASANS_S_MERGE";
const MENOMONIA_N_MERGE: &str = "MENOMONIA_N_MERGE";
const MENOMONIA_BIG_N_MERGE: &str = "MENOMONIA_BIG_N_MERGE";
const FIRASANS_N_MERGE: &str = "FIRASANS_N_MERGE";
const MENOMONIA_L_MERGE: &str = "MENOMONIA_L_MERGE";
const MENOMONIA_BIG_L_MERGE: &str = "MENOMONIA_BIG_L_MERGE";
const FIRASANS_L_MERGE: &str = "FIRASANS_L_MERGE";
const MENOMONIA_XL_MERGE: &str = "MENOMONIA_XL_MERGE";
const MENOMONIA_BIG_XL_MERGE: &str = "MENOMONIA_BIG_XL_MERGE";
const FIRASANS_XL_MERGE: &str = "FIRASANS_XL_MERGE";

const INTER_BYTES: &[u8] = include_bytes!("../../../res/Fonts/Inter.ttf");
const MENOMONIA_BYTES: &[u8] = include_bytes!("../../../res/Fonts/Menomonia.ttf");
const FIRASANS_BYTES: &[u8] = include_bytes!("../../../res/Fonts/FiraSans.ttf");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EmbeddedSource {
    Menomonia,
    FiraSans,
}

impl EmbeddedSource {
    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Menomonia => MENOMONIA_BYTES,
            Self::FiraSans => FIRASANS_BYTES,
        }
    }

    const fn public_source(self) -> FontSource {
        match self {
            Self::Menomonia => FontSource::Menomonia,
            Self::FiraSans => FontSource::FiraSans,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FontSpec {
    identifier: &'static str,
    merge_identifier: &'static str,
    size: f32,
    source: EmbeddedSource,
}

const HOST_FONT_SPECS: [FontSpec; 12] = [
    FontSpec {
        identifier: MENOMONIA_S,
        merge_identifier: MENOMONIA_S_MERGE,
        size: 16.0,
        source: EmbeddedSource::Menomonia,
    },
    FontSpec {
        identifier: MENOMONIA_BIG_S,
        merge_identifier: MENOMONIA_BIG_S_MERGE,
        size: 22.0,
        source: EmbeddedSource::Menomonia,
    },
    FontSpec {
        identifier: FIRASANS_S,
        merge_identifier: FIRASANS_S_MERGE,
        size: 15.0,
        source: EmbeddedSource::FiraSans,
    },
    FontSpec {
        identifier: MENOMONIA_N,
        merge_identifier: MENOMONIA_N_MERGE,
        size: 18.0,
        source: EmbeddedSource::Menomonia,
    },
    FontSpec {
        identifier: MENOMONIA_BIG_N,
        merge_identifier: MENOMONIA_BIG_N_MERGE,
        size: 24.0,
        source: EmbeddedSource::Menomonia,
    },
    FontSpec {
        identifier: FIRASANS_N,
        merge_identifier: FIRASANS_N_MERGE,
        size: 16.0,
        source: EmbeddedSource::FiraSans,
    },
    FontSpec {
        identifier: MENOMONIA_L,
        merge_identifier: MENOMONIA_L_MERGE,
        size: 20.0,
        source: EmbeddedSource::Menomonia,
    },
    FontSpec {
        identifier: MENOMONIA_BIG_L,
        merge_identifier: MENOMONIA_BIG_L_MERGE,
        size: 26.0,
        source: EmbeddedSource::Menomonia,
    },
    FontSpec {
        identifier: FIRASANS_L,
        merge_identifier: FIRASANS_L_MERGE,
        size: 17.5,
        source: EmbeddedSource::FiraSans,
    },
    FontSpec {
        identifier: MENOMONIA_XL,
        merge_identifier: MENOMONIA_XL_MERGE,
        size: 22.0,
        source: EmbeddedSource::Menomonia,
    },
    FontSpec {
        identifier: MENOMONIA_BIG_XL,
        merge_identifier: MENOMONIA_BIG_XL_MERGE,
        size: 28.0,
        source: EmbeddedSource::Menomonia,
    },
    FontSpec {
        identifier: FIRASANS_XL,
        merge_identifier: FIRASANS_XL_MERGE,
        size: 19.5,
        source: EmbeddedSource::FiraSans,
    },
];

/// Source selected for one catalog registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontSource {
    /// Embedded Inter fallback used by the host default.
    Inter,
    /// Embedded Menomonia display font.
    Menomonia,
    /// Embedded Fira Sans UI font.
    FiraSans,
    /// Owned user-font bytes, either as the default or a merge input.
    User,
}

/// Closed validation failures for optional user-font bytes.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum UserFontError {
    /// An empty payload cannot be registered as a font.
    #[error("user font data is empty")]
    Empty,
    /// Dear ImGui's signed byte-count ABI cannot represent the payload.
    #[error("user font data is too large")]
    TooLarge,
}

/// Shared ownership of optional user-font bytes.
///
/// Cloning this value or a rebuild request does not duplicate the payload.
/// Debug output reports only its byte length.
#[derive(Clone, Eq, PartialEq)]
pub struct UserFont {
    bytes: Arc<[u8]>,
}

impl UserFont {
    /// Takes ownership of validated font bytes without another full copy.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, UserFontError> {
        validate_user_font_len(bytes.len())?;
        Ok(Self {
            bytes: Arc::<[u8]>::from(bytes),
        })
    }

    /// Copies and validates borrowed font bytes.
    pub fn copy_from_slice(bytes: &[u8]) -> Result<Self, UserFontError> {
        Self::from_bytes(bytes.to_vec())
    }

    /// Borrows the owned payload.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the owned payload length.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }
}

impl fmt::Debug for UserFont {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserFont")
            .field("byte_len", &self.byte_len())
            .finish()
    }
}

fn validate_user_font_len(length: usize) -> Result<(), UserFontError> {
    match length {
        0 => Err(UserFontError::Empty),
        value if value > MAX_USER_FONT_BYTES => Err(UserFontError::TooLarge),
        _ => Ok(()),
    }
}

/// One manager-ready registration borrowed from an owned rebuild request.
///
/// The service manager copies both [`Self::data`] and [`Self::config`] during
/// registration, so neither can dangle after this value is dropped.
#[derive(Clone)]
pub struct CatalogRegistration<'request> {
    /// Exact legacy identifier.
    pub identifier: &'static str,
    /// Legacy rasterized size in pixels.
    pub size: f32,
    /// Embedded or user source classification.
    pub source: FontSource,
    /// Font bytes kept alive by the request or the executable image.
    pub data: &'request [u8],
    /// Deep-owned Dear ImGui configuration for this input.
    pub config: FontConfig,
}

impl fmt::Debug for CatalogRegistration<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CatalogRegistration")
            .field("identifier", &self.identifier)
            .field("size", &self.size)
            .field("source", &self.source)
            .field("byte_len", &self.data.len())
            .field("config", &self.config)
            .finish()
    }
}

/// Owned command for replacing and rebuilding the exact host font catalog.
///
/// This type is `Send + Sync`; the [`FontManager`] it is eventually applied to
/// remains deliberately thread-bound.
#[derive(Clone, Debug, PartialEq)]
pub struct FontRebuildRequest {
    default_size: f32,
    user_font: Option<UserFont>,
}

impl FontRebuildRequest {
    /// Creates a request from settings state.
    ///
    /// Missing or NaN sizes use [`DEFAULT_FONT_SIZE`]. All other values,
    /// including infinities, are clamped to the inclusive 1–50 pixel range.
    #[must_use]
    pub fn new(configured_default_size: Option<f32>, user_font: Option<UserFont>) -> Self {
        Self {
            default_size: normalize_default_font_size(configured_default_size),
            user_font,
        }
    }

    /// Returns the normalized default-font size.
    #[must_use]
    pub const fn default_size(&self) -> f32 {
        self.default_size
    }

    /// Returns whether the user font replaces the default and merges into the
    /// twelve scale-specific embedded inputs.
    #[must_use]
    pub const fn has_user_font(&self) -> bool {
        self.user_font.is_some()
    }

    /// Returns the exact number of manager registrations this request emits.
    #[must_use]
    pub const fn registration_count(&self) -> usize {
        if self.has_user_font() {
            MERGED_CATALOG_FONT_COUNT
        } else {
            BASE_CATALOG_FONT_COUNT
        }
    }

    /// Materializes manager-ready registrations in exact legacy insertion order.
    #[must_use]
    pub fn registrations(&self) -> Vec<CatalogRegistration<'_>> {
        let mut registrations = Vec::with_capacity(self.registration_count());
        let (default_data, default_source) = self
            .user_font
            .as_ref()
            .map_or((INTER_BYTES, FontSource::Inter), |font| {
                (font.as_bytes(), FontSource::User)
            });
        registrations.push(CatalogRegistration {
            identifier: FONT_DEFAULT,
            size: self.default_size,
            source: default_source,
            data: default_data,
            config: FontConfig::default(),
        });

        for specification in HOST_FONT_SPECS {
            registrations.push(CatalogRegistration {
                identifier: specification.identifier,
                size: specification.size,
                source: specification.source.public_source(),
                data: specification.source.bytes(),
                config: FontConfig::default(),
            });
            if let Some(user_font) = &self.user_font {
                registrations.push(CatalogRegistration {
                    identifier: specification.merge_identifier,
                    size: specification.size,
                    source: FontSource::User,
                    data: user_font.as_bytes(),
                    config: merge_config(),
                });
            }
        }
        registrations
    }

    /// Atomically replaces this owner's catalog, rebuilds, and resolves handles.
    ///
    /// The owner must be dedicated to this host catalog. Existing registrations
    /// owned by other identities remain intact. Existing catalog subscribers
    /// and foreign claims survive replacement, while a reserved identifier not
    /// already claimed by this owner is rejected before any mutation.
    ///
    /// # Render-thread contract
    ///
    /// Call only on the thread that owns `manager`, with its Dear ImGui context
    /// current, after the previous frame has ended and before `NewFrame`. The
    /// manager is not `Send`, and its ImGui backend performs the only atlas
    /// mutation synchronously inside this method.
    pub fn apply_pre_new_frame<B: FontAtlasBackend>(
        &self,
        manager: &mut FontManager<B>,
        owner: OwnerId,
        localized_texts: &[&CStr],
    ) -> Result<AppliedFontCatalog, FontCatalogError> {
        let registrations = self.registrations();
        let replacements = registrations
            .iter()
            .map(|registration| FontMemoryReplacement {
                identifier: registration.identifier,
                size: registration.size,
                data: registration.data,
                config: &registration.config,
            })
            .collect::<Vec<_>>();
        let replacement = manager
            .replace_owner_memory(owner, &replacements)
            .map_err(FontCatalogError::Replacement)?;

        let advance = manager
            .advance(localized_texts)
            .map_err(FontCatalogError::Rebuild)?;
        let handles = FontCatalogHandles::resolve(manager)?;
        Ok(AppliedFontCatalog {
            advance,
            handles,
            replacement,
        })
    }
}

impl Default for FontRebuildRequest {
    fn default() -> Self {
        Self::new(None, None)
    }
}

fn normalize_default_font_size(configured_size: Option<f32>) -> f32 {
    match configured_size {
        Some(size) if !size.is_nan() => size.clamp(MIN_DEFAULT_FONT_SIZE, MAX_DEFAULT_FONT_SIZE),
        _ => DEFAULT_FONT_SIZE,
    }
}

fn merge_config() -> FontConfig {
    FontConfig {
        merge_mode: true,
        ..FontConfig::default()
    }
}

/// Exact legacy identifiers selected for one Mumble UI-size value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectedFontIdentifiers {
    /// NexusLink regular display font identifier.
    pub font: &'static str,
    /// NexusLink large display font identifier.
    pub font_big: &'static str,
    /// NexusLink general UI font identifier.
    pub font_ui: &'static str,
}

/// Selects exact host identifiers for Mumble's open UI-size representation.
///
/// Unknown values intentionally select the normal catalog, matching the legacy
/// switch default.
#[must_use]
pub const fn selected_font_identifiers(ui_scale: UiScale) -> SelectedFontIdentifiers {
    match ui_scale.0 {
        0 => SelectedFontIdentifiers {
            font: MENOMONIA_S,
            font_big: MENOMONIA_BIG_S,
            font_ui: FIRASANS_S,
        },
        2 => SelectedFontIdentifiers {
            font: MENOMONIA_L,
            font_big: MENOMONIA_BIG_L,
            font_ui: FIRASANS_L,
        },
        3 => SelectedFontIdentifiers {
            font: MENOMONIA_XL,
            font_big: MENOMONIA_BIG_XL,
            font_ui: FIRASANS_XL,
        },
        _ => SelectedFontIdentifiers {
            font: MENOMONIA_N,
            font_big: MENOMONIA_BIG_N,
            font_ui: FIRASANS_N,
        },
    }
}

/// Current atlas handles selected for one Mumble UI size.
///
/// These handles remain valid until the manager begins its next rebuild. Users
/// must replace, rather than retain, an older set after rebuilding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectedFontHandles {
    /// NexusLink regular display font.
    pub font: FontHandle,
    /// NexusLink large display font.
    pub font_big: FontHandle,
    /// NexusLink general UI font.
    pub font_ui: FontHandle,
}

/// Every stable base handle from one successfully rebuilt atlas generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FontCatalogHandles {
    /// Current default font handle.
    pub default: FontHandle,
    small: SelectedFontHandles,
    normal: SelectedFontHandles,
    large: SelectedFontHandles,
    larger: SelectedFontHandles,
}

impl FontCatalogHandles {
    /// Resolves every required base handle from a successfully built manager.
    pub fn resolve<B: FontAtlasBackend>(
        manager: &FontManager<B>,
    ) -> Result<Self, FontCatalogError> {
        Ok(Self {
            default: required_handle(manager, FONT_DEFAULT)?,
            small: resolve_selection(manager, UiScale::SMALL)?,
            normal: resolve_selection(manager, UiScale::NORMAL)?,
            large: resolve_selection(manager, UiScale::LARGE)?,
            larger: resolve_selection(manager, UiScale::LARGER)?,
        })
    }

    /// Selects handles using the exact Mumble UI-size policy.
    #[must_use]
    pub const fn selected(self, ui_scale: UiScale) -> SelectedFontHandles {
        match ui_scale.0 {
            0 => self.small,
            2 => self.large,
            3 => self.larger,
            _ => self.normal,
        }
    }
}

fn resolve_selection<B: FontAtlasBackend>(
    manager: &FontManager<B>,
    ui_scale: UiScale,
) -> Result<SelectedFontHandles, FontCatalogError> {
    let identifiers = selected_font_identifiers(ui_scale);
    Ok(SelectedFontHandles {
        font: required_handle(manager, identifiers.font)?,
        font_big: required_handle(manager, identifiers.font_big)?,
        font_ui: required_handle(manager, identifiers.font_ui)?,
    })
}

fn required_handle<B: FontAtlasBackend>(
    manager: &FontManager<B>,
    identifier: &str,
) -> Result<FontHandle, FontCatalogError> {
    manager
        .handle(identifier)
        .ok_or(FontCatalogError::MissingHandle)
}

/// Successful render-thread application of one rebuild request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppliedFontCatalog {
    /// Underlying manager rebuild report.
    pub advance: FontAdvance,
    /// Handles resolved from the new atlas generation.
    pub handles: FontCatalogHandles,
    /// Atomic registry replacement report.
    pub replacement: FontOwnerReplaceReport,
}

/// Closed, redaction-safe failures at the host-font integration boundary.
#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum FontCatalogError {
    /// The service manager rejected the atomic catalog replacement.
    #[error("font catalog replacement failed")]
    Replacement(#[source] FontOwnerReplaceError),
    /// The atlas backend rejected or failed the rebuild.
    #[error("font catalog rebuild failed")]
    Rebuild(#[source] FontError),
    /// A successful rebuild did not publish every required base handle.
    #[error("font catalog is missing a required handle")]
    MissingHandle,
}

/// Thread-bound manager type backed by the Dear ImGui 1.80 atlas adapter.
pub type ImGuiFontManager = FontManager<ImGuiFontAtlasBackend>;

/// Creates a dirty Dear ImGui manager for construction on the render thread.
///
/// The target context must be current before applying a request, not while
/// constructing this stateless manager.
#[must_use]
pub fn new_imgui_font_manager() -> ImGuiFontManager {
    FontManager::new(ImGuiFontAtlasBackend::new())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ffi::CStr;
    use std::rc::Rc;

    use nexus_ui_services::{
        FontAtlasBackend, FontBackendError, FontBuildRequest, FontConfig, FontHandle, FontManager,
        FontOwnerReplaceError, GlyphCoverage, OwnerId, UiScale,
    };

    use super::{
        BASE_CATALOG_FONT_COUNT, DEFAULT_FONT_SIZE, FIRASANS_L, FIRASANS_L_MERGE, FIRASANS_N,
        FIRASANS_N_MERGE, FIRASANS_S, FIRASANS_S_MERGE, FIRASANS_XL, FIRASANS_XL_MERGE,
        FONT_DEFAULT, FontCatalogError, FontRebuildRequest, FontSource, MAX_DEFAULT_FONT_SIZE,
        MAX_USER_FONT_BYTES, MENOMONIA_BIG_L, MENOMONIA_BIG_L_MERGE, MENOMONIA_BIG_N,
        MENOMONIA_BIG_N_MERGE, MENOMONIA_BIG_S, MENOMONIA_BIG_S_MERGE, MENOMONIA_BIG_XL,
        MENOMONIA_BIG_XL_MERGE, MENOMONIA_L, MENOMONIA_L_MERGE, MENOMONIA_N, MENOMONIA_N_MERGE,
        MENOMONIA_S, MENOMONIA_S_MERGE, MENOMONIA_XL, MENOMONIA_XL_MERGE,
        MERGED_CATALOG_FONT_COUNT, MIN_DEFAULT_FONT_SIZE, UserFont, UserFontError,
        selected_font_identifiers, validate_user_font_len,
    };

    #[derive(Clone, Debug, PartialEq)]
    struct CapturedFont {
        identifier: String,
        size: f32,
        merge_mode: bool,
        coverage: GlyphCoverage,
    }

    #[derive(Default)]
    struct CapturingBackend {
        builds: Vec<Vec<CapturedFont>>,
    }

    impl FontAtlasBackend for CapturingBackend {
        fn rebuild(
            &mut self,
            fonts: &[FontBuildRequest<'_>],
            _localized_texts: &[&CStr],
        ) -> Result<Vec<Option<FontHandle>>, FontBackendError> {
            self.builds.push(
                fonts
                    .iter()
                    .map(|font| CapturedFont {
                        identifier: font.identifier.to_string_lossy().into_owned(),
                        size: font.size,
                        merge_mode: font.config.merge_mode,
                        coverage: font.coverage,
                    })
                    .collect(),
            );
            // SAFETY: this non-null sentinel is never dereferenced. Tests use
            // it only to exercise manager publication and selection behavior.
            let handle = unsafe { FontHandle::from_ptr(std::ptr::without_provenance_mut(1)) }
                .ok_or(FontBackendError::RejectedInput)?;
            Ok(vec![Some(handle); fonts.len()])
        }
    }

    #[test]
    fn default_size_policy_clamps_and_recovers_from_nan() {
        assert_eq!(
            FontRebuildRequest::new(None, None).default_size(),
            DEFAULT_FONT_SIZE
        );
        assert_eq!(
            FontRebuildRequest::new(Some(f32::NAN), None).default_size(),
            DEFAULT_FONT_SIZE
        );
        assert_eq!(
            FontRebuildRequest::new(Some(f32::NEG_INFINITY), None).default_size(),
            MIN_DEFAULT_FONT_SIZE
        );
        assert_eq!(
            FontRebuildRequest::new(Some(-10.0), None).default_size(),
            MIN_DEFAULT_FONT_SIZE
        );
        assert_eq!(
            FontRebuildRequest::new(Some(f32::INFINITY), None).default_size(),
            MAX_DEFAULT_FONT_SIZE
        );
        assert_eq!(
            FontRebuildRequest::new(Some(80.0), None).default_size(),
            MAX_DEFAULT_FONT_SIZE
        );
    }

    #[test]
    fn embedded_catalog_has_exact_legacy_order_sizes_and_sources() {
        let request = FontRebuildRequest::default();
        let registrations = request.registrations();
        assert_eq!(registrations.len(), BASE_CATALOG_FONT_COUNT);
        let actual = registrations
            .iter()
            .map(|font| {
                (
                    font.identifier,
                    font.size,
                    font.source,
                    font.config.merge_mode,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![
                (FONT_DEFAULT, 15.0, FontSource::Inter, false),
                (MENOMONIA_S, 16.0, FontSource::Menomonia, false),
                (MENOMONIA_BIG_S, 22.0, FontSource::Menomonia, false),
                (FIRASANS_S, 15.0, FontSource::FiraSans, false),
                (MENOMONIA_N, 18.0, FontSource::Menomonia, false),
                (MENOMONIA_BIG_N, 24.0, FontSource::Menomonia, false),
                (FIRASANS_N, 16.0, FontSource::FiraSans, false),
                (MENOMONIA_L, 20.0, FontSource::Menomonia, false),
                (MENOMONIA_BIG_L, 26.0, FontSource::Menomonia, false),
                (FIRASANS_L, 17.5, FontSource::FiraSans, false),
                (MENOMONIA_XL, 22.0, FontSource::Menomonia, false),
                (MENOMONIA_BIG_XL, 28.0, FontSource::Menomonia, false),
                (FIRASANS_XL, 19.5, FontSource::FiraSans, false),
            ]
        );
        assert!(registrations.iter().all(|font| !font.data.is_empty()));
    }

    #[test]
    fn user_font_replaces_default_and_follows_each_base_as_a_merge() {
        let user_font = UserFont::copy_from_slice(b"owned user font")
            .unwrap_or_else(|error| panic!("user font failed: {error}"));
        let request = FontRebuildRequest::new(Some(21.0), Some(user_font));
        let registrations = request.registrations();
        assert_eq!(registrations.len(), MERGED_CATALOG_FONT_COUNT);
        assert_eq!(registrations[0].identifier, FONT_DEFAULT);
        assert_eq!(registrations[0].source, FontSource::User);
        assert_eq!(registrations[0].data, b"owned user font");
        assert!(!registrations[0].config.merge_mode);

        let identifiers = registrations
            .iter()
            .map(|font| font.identifier)
            .collect::<Vec<_>>();
        assert_eq!(
            identifiers,
            vec![
                FONT_DEFAULT,
                MENOMONIA_S,
                MENOMONIA_S_MERGE,
                MENOMONIA_BIG_S,
                MENOMONIA_BIG_S_MERGE,
                FIRASANS_S,
                FIRASANS_S_MERGE,
                MENOMONIA_N,
                MENOMONIA_N_MERGE,
                MENOMONIA_BIG_N,
                MENOMONIA_BIG_N_MERGE,
                FIRASANS_N,
                FIRASANS_N_MERGE,
                MENOMONIA_L,
                MENOMONIA_L_MERGE,
                MENOMONIA_BIG_L,
                MENOMONIA_BIG_L_MERGE,
                FIRASANS_L,
                FIRASANS_L_MERGE,
                MENOMONIA_XL,
                MENOMONIA_XL_MERGE,
                MENOMONIA_BIG_XL,
                MENOMONIA_BIG_XL_MERGE,
                FIRASANS_XL,
                FIRASANS_XL_MERGE,
            ]
        );
        for pair in registrations[1..].chunks_exact(2) {
            assert!(!pair[0].config.merge_mode);
            assert!(pair[1].config.merge_mode);
            assert_eq!(pair[0].size, pair[1].size);
            assert_eq!(pair[1].source, FontSource::User);
            assert_eq!(pair[1].data, b"owned user font");
        }
    }

    #[test]
    fn ui_scale_selection_matches_legacy_and_unknown_defaults_to_normal() {
        assert_eq!(selected_font_identifiers(UiScale::SMALL).font, MENOMONIA_S);
        assert_eq!(
            selected_font_identifiers(UiScale::NORMAL).font_big,
            MENOMONIA_BIG_N
        );
        assert_eq!(
            selected_font_identifiers(UiScale::LARGE).font_ui,
            FIRASANS_L
        );
        assert_eq!(
            selected_font_identifiers(UiScale::LARGER).font,
            MENOMONIA_XL
        );
        assert_eq!(selected_font_identifiers(UiScale(99)).font_ui, FIRASANS_N);
    }

    #[test]
    fn render_thread_apply_replaces_the_owned_generation() {
        let first = FontRebuildRequest::new(Some(17.0), None);
        let mut manager = FontManager::new(CapturingBackend::default());
        let first_report = first
            .apply_pre_new_frame(&mut manager, OwnerId::HOST, &[])
            .unwrap_or_else(|error| panic!("first rebuild failed: {error}"));
        assert!(first_report.advance.rebuilt);
        assert_eq!(first_report.replacement.requested, BASE_CATALOG_FONT_COUNT);
        assert_eq!(first_report.replacement.created, BASE_CATALOG_FONT_COUNT);
        assert_eq!(first_report.replacement.updated, 0);
        assert_eq!(manager.len(), BASE_CATALOG_FONT_COUNT);

        let callback_events = Rc::new(RefCell::new(Vec::new()));
        let event_sink = Rc::clone(&callback_events);
        assert!(
            manager
                .get(
                    OwnerId::new(8, 1),
                    FONT_DEFAULT,
                    Box::new(move |_, handle| event_sink.borrow_mut().push(handle.is_some())),
                )
                .is_ok()
        );
        assert_eq!(&*callback_events.borrow(), &[true]);

        let user_font = UserFont::copy_from_slice(b"test font")
            .unwrap_or_else(|error| panic!("user font failed: {error}"));
        let second = FontRebuildRequest::new(Some(19.0), Some(user_font));
        let expected_second_order = second
            .registrations()
            .iter()
            .map(|registration| registration.identifier)
            .collect::<Vec<_>>();
        let second_report = second
            .apply_pre_new_frame(&mut manager, OwnerId::HOST, &[])
            .unwrap_or_else(|error| panic!("second rebuild failed: {error}"));
        assert!(second_report.advance.rebuilt);
        assert_eq!(
            second_report.replacement.requested,
            MERGED_CATALOG_FONT_COUNT
        );
        assert_eq!(second_report.replacement.updated, BASE_CATALOG_FONT_COUNT);
        assert_eq!(second_report.replacement.created, 12);
        assert_eq!(&*callback_events.borrow(), &[true, false, true]);
        assert_eq!(manager.len(), MERGED_CATALOG_FONT_COUNT);

        let third = FontRebuildRequest::new(Some(20.0), None);
        let third_report = third
            .apply_pre_new_frame(&mut manager, OwnerId::HOST, &[])
            .unwrap_or_else(|error| panic!("third rebuild failed: {error}"));
        assert!(third_report.advance.rebuilt);
        assert_eq!(third_report.replacement.requested, BASE_CATALOG_FONT_COUNT);
        assert_eq!(third_report.replacement.updated, BASE_CATALOG_FONT_COUNT);
        assert_eq!(third_report.replacement.created, 0);
        assert_eq!(third_report.replacement.removed_claims, 12);
        assert_eq!(third_report.replacement.removed_entries, 12);
        assert_eq!(
            &*callback_events.borrow(),
            &[true, false, true, false, true]
        );
        assert_eq!(manager.len(), BASE_CATALOG_FONT_COUNT);

        let backend = manager.into_backend();
        assert_eq!(backend.builds.len(), 3);
        assert_eq!(backend.builds[0][0].coverage, GlyphCoverage::HostDefault);
        assert_eq!(backend.builds[1][0].size, 19.0);
        assert_eq!(backend.builds[1].len(), MERGED_CATALOG_FONT_COUNT);
        assert_eq!(
            backend.builds[1]
                .iter()
                .map(|font| font.identifier.as_str())
                .collect::<Vec<_>>(),
            expected_second_order
        );
        for pair in backend.builds[1][1..].chunks_exact(2) {
            assert!(!pair[0].merge_mode);
            assert!(pair[1].merge_mode);
        }
        assert_eq!(backend.builds[2][0].size, 20.0);
        assert_eq!(backend.builds[2].len(), BASE_CATALOG_FONT_COUNT);
        assert!(
            backend.builds[2]
                .iter()
                .skip(1)
                .all(|font| { font.coverage == GlyphCoverage::Localized && !font.merge_mode })
        );
    }

    #[test]
    fn foreign_reserved_identifier_conflict_is_non_mutating() {
        let mut manager = FontManager::new(CapturingBackend::default());
        let addon_owner = OwnerId::new(7, 1);
        assert!(
            manager
                .register_memory(
                    addon_owner,
                    MENOMONIA_N,
                    18.0,
                    b"addon font",
                    FontConfig::default(),
                    None,
                )
                .is_ok()
        );
        assert!(manager.advance(&[]).is_ok());
        let before_handle = manager.handle(MENOMONIA_N);
        let result =
            FontRebuildRequest::default().apply_pre_new_frame(&mut manager, OwnerId::HOST, &[]);
        assert_eq!(
            result,
            Err(FontCatalogError::Replacement(
                FontOwnerReplaceError::OwnerConflict { request_index: 4 }
            ))
        );
        assert_eq!(manager.len(), 1);
        assert_eq!(manager.handle(MENOMONIA_N), before_handle);
        assert_eq!(
            manager.advance(&[]),
            Ok(nexus_ui_services::FontAdvance::default())
        );
        assert_eq!(manager.into_backend().builds.len(), 1);
    }

    #[test]
    fn practical_user_font_cap_is_checked_without_allocating_a_fixture() {
        assert_eq!(MAX_USER_FONT_BYTES, 128 * 1024 * 1024);
        assert!(MAX_USER_FONT_BYTES <= i32::MAX as usize);
        assert_eq!(
            validate_user_font_len(MAX_USER_FONT_BYTES + 1),
            Err(UserFontError::TooLarge)
        );
    }

    #[test]
    fn user_font_ownership_and_errors_are_bounded() {
        assert_eq!(UserFont::from_bytes(Vec::new()), Err(UserFontError::Empty));
        let font = UserFont::copy_from_slice(b"shared")
            .unwrap_or_else(|error| panic!("user font failed: {error}"));
        let cloned = font.clone();
        assert_eq!(font.as_bytes(), cloned.as_bytes());
        assert_eq!(format!("{font:?}"), "UserFont { byte_len: 6 }");
    }

    #[test]
    fn rebuild_request_can_cross_into_the_render_thread() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FontRebuildRequest>();
    }
}
