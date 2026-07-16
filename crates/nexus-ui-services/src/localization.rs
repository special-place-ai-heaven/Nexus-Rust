//! Deterministic locale atlas loading and stable C-string translation storage.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{CStr, CString, c_char};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::OwnerId;

/// A single locale JSON document supplied in merge order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocaleAsset {
    bytes: Vec<u8>,
}

impl LocaleAsset {
    /// Copies a locale document into owned storage.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Closed, path-free errors returned by locale sources.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LocaleSourceError {
    /// The configured locale directory could not be enumerated.
    #[error("locale directory is unavailable")]
    DirectoryUnavailable,
    /// One of the selected locale assets could not be read.
    #[error("a locale asset could not be read")]
    AssetUnreadable,
}

/// Injected source of locale JSON documents.
pub trait LocaleSource {
    /// Loads documents in the order in which they should be merged.
    fn load(&mut self) -> Result<Vec<LocaleAsset>, LocaleSourceError>;
}

/// Filesystem locale source with deterministic lexical merge ordering.
#[derive(Clone, Debug)]
pub struct DirectoryLocaleSource {
    directory: PathBuf,
}

impl DirectoryLocaleSource {
    /// Creates a source rooted at `directory`.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    fn locale_paths(&self) -> Result<Vec<PathBuf>, LocaleSourceError> {
        let entries =
            fs::read_dir(&self.directory).map_err(|_| LocaleSourceError::DirectoryUnavailable)?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|_| LocaleSourceError::DirectoryUnavailable)?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }
}

impl LocaleSource for DirectoryLocaleSource {
    fn load(&mut self) -> Result<Vec<LocaleAsset>, LocaleSourceError> {
        self.locale_paths()?
            .into_iter()
            .filter(|path| is_non_empty_file(path))
            .map(|path| {
                fs::read(path)
                    .map(LocaleAsset::new)
                    .map_err(|_| LocaleSourceError::AssetUnreadable)
            })
            .collect()
    }
}

fn is_non_empty_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

/// Redaction-safe localization failures.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LocalizationError {
    /// A string cannot cross the native ABI because it contains a NUL byte.
    #[error("localization text contains an interior NUL byte")]
    InteriorNul,
    /// The cross-thread command queue reached its configured bound.
    #[error("localization command queue is full")]
    QueueFull,
    /// A zero-sized command queue cannot make progress.
    #[error("localization command queue capacity must be non-zero")]
    InvalidQueueCapacity,
}

/// Result of rebuilding the locale atlas.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LocaleLoadReport {
    /// Documents that contributed a locale.
    pub loaded_assets: usize,
    /// Malformed or incompatible documents skipped without aborting the load.
    pub skipped_assets: usize,
    /// Non-string or non-C-compatible text values skipped during merge.
    pub skipped_texts: usize,
    /// Number of locales in the resulting atlas.
    pub locales: usize,
}

/// Result of applying queued runtime changes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdvanceReport {
    /// Number of translation overrides applied.
    pub applied_texts: usize,
    /// Overrides targeting locales that do not exist.
    pub dropped_texts: usize,
    /// Overrides removed during owner cleanup.
    pub removed_overrides: usize,
    /// Whether the requested active language changed successfully.
    pub language_changed: bool,
    /// Whether a requested language was unknown and therefore ignored.
    pub unknown_language: bool,
}

/// Locale identifier and display name exposed to settings UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LanguageInfo {
    /// Short locale identifier, such as `en`.
    pub identifier: String,
    /// Human-readable display name.
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
struct RawLocale {
    #[serde(rename = "Identifier")]
    identifier: String,
    #[serde(rename = "DisplayName", default)]
    display_name: Option<String>,
    #[serde(rename = "Texts")]
    texts: BTreeMap<String, Value>,
}

#[derive(Debug, Default)]
struct Locale {
    display_name: String,
    texts: BTreeMap<String, usize>,
}

#[derive(Clone, Debug)]
struct RuntimeOverride {
    owner: OwnerId,
    sequence: u64,
    text: usize,
}

#[derive(Debug)]
enum Command {
    Set {
        owner: OwnerId,
        identifier: String,
        language: String,
        text: String,
    },
    SetLanguage(String),
    Cleanup(OwnerId),
}

#[derive(Debug)]
struct Queue {
    capacity: usize,
    commands: VecDeque<Command>,
}

impl Queue {
    fn push(&mut self, command: Command) -> Result<(), LocalizationError> {
        if self.commands.len() >= self.capacity {
            return Err(LocalizationError::QueueFull);
        }
        self.commands.push_back(command);
        Ok(())
    }
}

/// Cloneable producer for changes consumed by the UI-thread service.
#[derive(Clone, Debug)]
pub struct LocalizationHandle {
    queue: Arc<Mutex<Queue>>,
}

impl LocalizationHandle {
    /// Queues an owner-scoped translation override.
    pub fn set(
        &self,
        owner: OwnerId,
        identifier: &str,
        language: &str,
        text: &str,
    ) -> Result<(), LocalizationError> {
        validate_c_string(identifier)?;
        validate_c_string(language)?;
        validate_c_string(text)?;
        self.with_queue(|queue| {
            queue.push(Command::Set {
                owner,
                identifier: identifier.to_owned(),
                language: language.to_owned(),
                text: text.to_owned(),
            })
        })
    }

    /// Queues an active-language change by identifier or display name.
    pub fn set_language(&self, language: &str) -> Result<(), LocalizationError> {
        validate_c_string(language)?;
        self.with_queue(|queue| queue.push(Command::SetLanguage(language.to_owned())))
    }

    /// Queues removal of every override owned by an addon generation.
    pub fn cleanup_owner(&self, owner: OwnerId) -> Result<(), LocalizationError> {
        self.with_queue(|queue| queue.push(Command::Cleanup(owner)))
    }

    fn with_queue<R>(&self, operation: impl FnOnce(&mut Queue) -> R) -> R {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        operation(&mut queue)
    }
}

/// Owned locale atlas with process-stable native string pointers.
///
/// C strings are intentionally retained until this service is dropped. An
/// override can therefore replace a translation without invalidating a pointer
/// previously returned to a native addon.
#[derive(Debug)]
pub struct LocalizationService {
    locales: BTreeMap<String, Locale>,
    arena: Vec<Box<CStr>>,
    overrides: BTreeMap<String, BTreeMap<String, Vec<RuntimeOverride>>>,
    active_language: Option<String>,
    requested_language: String,
    next_sequence: u64,
    queue: Arc<Mutex<Queue>>,
}

impl LocalizationService {
    /// Creates an empty atlas and a bounded cross-thread update handle.
    pub fn new(default_language: &str, queue_capacity: usize) -> Result<Self, LocalizationError> {
        validate_c_string(default_language)?;
        if queue_capacity == 0 {
            return Err(LocalizationError::InvalidQueueCapacity);
        }
        Ok(Self {
            locales: BTreeMap::new(),
            arena: Vec::new(),
            overrides: BTreeMap::new(),
            active_language: None,
            requested_language: default_language.to_owned(),
            next_sequence: 0,
            queue: Arc::new(Mutex::new(Queue {
                capacity: queue_capacity,
                commands: VecDeque::new(),
            })),
        })
    }

    /// Returns a producer that may queue updates from non-UI threads.
    #[must_use]
    pub fn handle(&self) -> LocalizationHandle {
        LocalizationHandle {
            queue: Arc::clone(&self.queue),
        }
    }

    /// Replaces base locale data while preserving returned pointer stability and
    /// owner-scoped runtime overrides.
    pub fn reload(
        &mut self,
        source: &mut impl LocaleSource,
    ) -> Result<LocaleLoadReport, LocaleSourceError> {
        let assets = source.load()?;
        let mut locales = BTreeMap::<String, Locale>::new();
        let mut report = LocaleLoadReport::default();

        for asset in assets {
            let Ok(raw) = serde_json::from_slice::<RawLocale>(asset.bytes()) else {
                report.skipped_assets += 1;
                continue;
            };
            if raw.identifier.is_empty() || validate_c_string(&raw.identifier).is_err() {
                report.skipped_assets += 1;
                continue;
            }
            let locale = locales.entry(raw.identifier.clone()).or_default();
            if locale.display_name.is_empty()
                && let Some(display_name) = raw.display_name
                && !display_name.is_empty()
                && validate_c_string(&display_name).is_ok()
            {
                locale.display_name = display_name;
            }
            for (identifier, value) in raw.texts {
                let Some(text) = value.as_str() else {
                    report.skipped_texts += 1;
                    continue;
                };
                if validate_c_string(text).is_err() {
                    report.skipped_texts += 1;
                    continue;
                }
                let text_index = self
                    .retain_c_string(text)
                    .map_err(|_| LocaleSourceError::AssetUnreadable)?;
                locale.texts.insert(identifier, text_index);
            }
            report.loaded_assets += 1;
        }

        for (identifier, locale) in &mut locales {
            if locale.display_name.is_empty() {
                locale.display_name.clone_from(identifier);
            }
        }
        report.locales = locales.len();
        self.locales = locales;
        self.resolve_requested_language_with_fallback();
        Ok(report)
    }

    /// Applies queued translations, language changes, and owner cleanup.
    pub fn advance(&mut self) -> AdvanceReport {
        let commands = {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            queue.commands.drain(..).collect::<Vec<_>>()
        };
        let mut report = AdvanceReport::default();
        for command in commands {
            match command {
                Command::Set {
                    owner,
                    identifier,
                    language,
                    text,
                } => {
                    if !self.locales.contains_key(&language) {
                        report.dropped_texts += 1;
                        continue;
                    }
                    let Ok(text_index) = self.retain_c_string(&text) else {
                        report.dropped_texts += 1;
                        continue;
                    };
                    self.next_sequence = self.next_sequence.saturating_add(1);
                    let overrides = self
                        .overrides
                        .entry(language)
                        .or_default()
                        .entry(identifier)
                        .or_default();
                    overrides.retain(|entry| entry.owner != owner);
                    overrides.push(RuntimeOverride {
                        owner,
                        sequence: self.next_sequence,
                        text: text_index,
                    });
                    report.applied_texts += 1;
                }
                Command::SetLanguage(language) => {
                    self.requested_language = language;
                    if let Some(resolved) = self.resolve_language(&self.requested_language) {
                        report.language_changed = self.active_language.as_ref() != Some(&resolved);
                        self.active_language = Some(resolved);
                    } else {
                        report.unknown_language = true;
                    }
                }
                Command::Cleanup(owner) => {
                    for locale in self.overrides.values_mut() {
                        for entries in locale.values_mut() {
                            let before = entries.len();
                            entries.retain(|entry| entry.owner != owner);
                            report.removed_overrides += before - entries.len();
                        }
                        locale.retain(|_, entries| !entries.is_empty());
                    }
                    self.overrides.retain(|_, locale| !locale.is_empty());
                }
            }
        }
        report
    }

    /// Maps the unofficial-extras language enumeration used by the legacy host.
    pub fn set_game_language(&self, language: u32) -> Result<bool, LocalizationError> {
        let identifier = match language {
            0 => "en",
            1 => "kr",
            2 => "fr",
            3 => "de",
            4 => "es",
            5 => "cn",
            _ => return Ok(false),
        };
        self.handle().set_language(identifier)?;
        Ok(true)
    }

    /// Translates through explicit locale, active locale, then English.
    ///
    /// If no translation exists, the exact input C string is returned.
    #[must_use]
    pub fn translate<'a>(&'a self, identifier: &'a CStr, language: Option<&CStr>) -> &'a CStr {
        let Ok(identifier_text) = identifier.to_str() else {
            return identifier;
        };
        if let Some(language) = language.and_then(|value| value.to_str().ok())
            && let Some(text) = self.lookup(language, identifier_text)
        {
            return text;
        }
        if let Some(active) = &self.active_language
            && let Some(text) = self.lookup(active, identifier_text)
        {
            return text;
        }
        if self.active_language.as_deref() != Some("en")
            && let Some(text) = self.lookup("en", identifier_text)
        {
            return text;
        }
        identifier
    }

    /// Returns the stable native pointer produced by [`Self::translate`].
    #[must_use]
    pub fn translate_ptr(&self, identifier: &CStr, language: Option<&CStr>) -> *const c_char {
        self.translate(identifier, language).as_ptr()
    }

    /// Returns every currently reachable translated string for glyph discovery.
    #[must_use]
    pub fn all_texts(&self) -> Vec<&CStr> {
        let mut result = Vec::new();
        for (language, locale) in &self.locales {
            let mut identifiers = locale.texts.keys().cloned().collect::<BTreeSet<_>>();
            if let Some(overrides) = self.overrides.get(language) {
                identifiers.extend(overrides.keys().cloned());
            }
            result.extend(
                identifiers
                    .iter()
                    .filter_map(|identifier| self.lookup(language, identifier)),
            );
        }
        result
    }

    /// Lists available languages in stable identifier order.
    #[must_use]
    pub fn languages(&self) -> Vec<LanguageInfo> {
        self.locales
            .iter()
            .map(|(identifier, locale)| LanguageInfo {
                identifier: identifier.clone(),
                display_name: locale.display_name.clone(),
            })
            .collect()
    }

    /// Returns the active language, if an atlas has been loaded.
    #[must_use]
    pub fn active_language(&self) -> Option<LanguageInfo> {
        let identifier = self.active_language.as_ref()?;
        let locale = self.locales.get(identifier)?;
        Some(LanguageInfo {
            identifier: identifier.clone(),
            display_name: locale.display_name.clone(),
        })
    }

    /// Number of allocations retained to honor native pointer stability.
    #[must_use]
    pub fn retained_c_strings(&self) -> usize {
        self.arena.len()
    }

    fn retain_c_string(&mut self, text: &str) -> Result<usize, LocalizationError> {
        let text = CString::new(text).map_err(|_| LocalizationError::InteriorNul)?;
        self.arena.push(text.into_boxed_c_str());
        Ok(self.arena.len() - 1)
    }

    fn lookup(&self, language: &str, identifier: &str) -> Option<&CStr> {
        let locale = self.locales.get(language)?;
        let index = self
            .overrides
            .get(language)
            .and_then(|texts| texts.get(identifier))
            .and_then(|entries| entries.iter().max_by_key(|entry| entry.sequence))
            .map(|entry| entry.text)
            .or_else(|| locale.texts.get(identifier).copied())?;
        self.arena.get(index).map(AsRef::as_ref)
    }

    fn resolve_language(&self, requested: &str) -> Option<String> {
        if self.locales.contains_key(requested) {
            return Some(requested.to_owned());
        }
        self.locales
            .iter()
            .find(|(_, locale)| locale.display_name == requested)
            .map(|(identifier, _)| identifier.clone())
    }

    fn resolve_requested_language_with_fallback(&mut self) {
        self.active_language = self
            .resolve_language(&self.requested_language)
            .or_else(|| self.locales.contains_key("en").then(|| "en".to_owned()))
            .or_else(|| self.locales.keys().next().cloned());
    }
}

fn validate_c_string(value: &str) -> Result<(), LocalizationError> {
    if value.as_bytes().contains(&0) {
        Err(LocalizationError::InteriorNul)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{CStr, CString};

    use super::{LocaleAsset, LocaleSource, LocaleSourceError, LocalizationService};
    use crate::OwnerId;

    struct MemorySource(Vec<LocaleAsset>);

    impl LocaleSource for MemorySource {
        fn load(&mut self) -> Result<Vec<LocaleAsset>, LocaleSourceError> {
            Ok(self.0.clone())
        }
    }

    fn fixture_source() -> MemorySource {
        MemorySource(vec![
            LocaleAsset::new(include_bytes!("../tests/fixtures/en-base.json").as_slice()),
            LocaleAsset::new(include_bytes!("../tests/fixtures/en-patch.json").as_slice()),
            LocaleAsset::new(include_bytes!("../tests/fixtures/de.json").as_slice()),
        ])
    }

    fn c_string(value: &str) -> CString {
        CString::new(value).unwrap_or_else(|_| panic!("test string contained NUL"))
    }

    #[test]
    fn golden_locale_documents_merge_and_fallback_like_legacy_nexus() {
        let mut service = LocalizationService::new("de", 16)
            .unwrap_or_else(|error| panic!("service failed: {error}"));
        let report = service
            .reload(&mut fixture_source())
            .unwrap_or_else(|error| panic!("reload failed: {error}"));
        assert_eq!(report.loaded_assets, 3);
        assert_eq!(report.skipped_texts, 1);
        assert_eq!(
            service.active_language().map(|value| value.identifier),
            Some("de".into())
        );

        let hello = c_string("hello");
        let only_base = c_string("only-base");
        assert_eq!(service.translate(&hello, None).to_bytes(), b"Hallo");
        assert_eq!(
            service.translate(&only_base, None).to_bytes(),
            b"Base",
            "missing German text must fall back to merged English"
        );
        assert_eq!(
            service
                .translate(&hello, Some(c_string("en").as_c_str()))
                .to_bytes(),
            b"Hello patched"
        );
    }

    #[test]
    fn replaced_strings_keep_stable_pointers_and_cleanup_restores_base() {
        let mut service = LocalizationService::new("en", 16)
            .unwrap_or_else(|error| panic!("service failed: {error}"));
        service
            .reload(&mut fixture_source())
            .unwrap_or_else(|error| panic!("reload failed: {error}"));
        let hello = c_string("hello");
        let old_pointer = service.translate_ptr(&hello, None);
        let handle = service.handle();
        assert!(
            handle
                .set(OwnerId::new(7, 1), "hello", "en", "Runtime")
                .is_ok()
        );
        assert_eq!(service.advance().applied_texts, 1);
        assert_eq!(service.translate(&hello, None).to_bytes(), b"Runtime");

        // SAFETY: the service explicitly retains all C-string allocations until
        // drop, so the earlier pointer remains valid after an override.
        let retained_text = unsafe { CStr::from_ptr(old_pointer) };
        assert_eq!(retained_text.to_bytes(), b"Hello patched");

        assert!(handle.cleanup_owner(OwnerId::new(7, 1)).is_ok());
        assert_eq!(service.advance().removed_overrides, 1);
        assert_eq!(service.translate(&hello, None).to_bytes(), b"Hello patched");
    }

    #[test]
    fn queue_is_bounded_and_unknown_languages_are_non_destructive() {
        let mut service = LocalizationService::new("en", 1)
            .unwrap_or_else(|error| panic!("service failed: {error}"));
        service
            .reload(&mut fixture_source())
            .unwrap_or_else(|error| panic!("reload failed: {error}"));
        let handle = service.handle();
        assert!(handle.set_language("missing").is_ok());
        assert!(handle.set_language("de").is_err());
        let report = service.advance();
        assert!(report.unknown_language);
        assert_eq!(
            service.active_language().map(|value| value.identifier),
            Some("en".into())
        );
    }
}
