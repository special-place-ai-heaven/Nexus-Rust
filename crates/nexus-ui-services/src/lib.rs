//! UI-adjacent services that preserve the Nexus addon contract without owning
//! a renderer, platform window, settings database, or resource loader.
//!
//! The services expose narrow injected backends so the runtime can coordinate
//! Dear ImGui 1.80 only from its UI thread while tests remain deterministic.

mod fonts;
mod localization;
mod owner;
mod scaling;
mod style;

pub use fonts::{
    FileFontAssetLoader, FontAdvance, FontAssetError, FontAssetLoader, FontAtlasBackend,
    FontBackendError, FontBuildRequest, FontCallback, FontConfig, FontError, FontGetResult,
    FontHandle, FontManager, FontMemoryReplacement, FontOwnerReplaceError, FontOwnerReplaceReport,
    FontRegistration, FontRegistrationRequest, GlyphCoverage, ResourceFont, SubscriptionId,
};
pub use localization::{
    AdvanceReport as LocalizationAdvanceReport, DirectoryLocaleSource, LanguageInfo, LocaleAsset,
    LocaleLoadReport, LocaleSource, LocaleSourceError, LocalizationError, LocalizationHandle,
    LocalizationService,
};
pub use owner::OwnerId;
pub use scaling::{
    LARGE_SCALE, LARGER_SCALE, NORMAL_SCALE, SMALL_SCALE, ScalingError, ScalingService,
    ScalingSink, ScalingSinkError, ScalingSnapshot, UiScale,
};
pub use style::{
    ArcStyleParts, ArcStyleSource, DirectoryStyleStorage, EmbeddedStyle, IMGUI_STYLE_180_BYTES,
    IMGUI_STYLE_SETTING, Palette, STYLE_FILE_EXTENSION, StyleBackend, StyleBackendError, StyleBlob,
    StyleError, StyleIoError, StyleService, StyleSettings, StyleStorage,
};
