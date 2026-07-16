use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;

const MAX_CONFIG_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONFIG_ENTRIES: usize = 16_384;

const SIGNATURE: &str = "Signature";
const FAVORITE: &str = "IsFavorite";
const UPDATE_MODE: &str = "UpdateMode";
const PRERELEASES: &str = "AllowPrereleases";
const LOADED: &str = "IsLoaded";
const DISABLE_VERSION: &str = "DisableVersion";
const LAST_GAME_BUILD: &str = "LastGameBuild";
const NAME: &str = "Name";
const MIGRATION_PAUSE_UPDATES: &str = "IsPausingUpdates";

/// Legacy add-on update behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum UpdateMode {
    /// No automatic update work. Legacy readers normalize this to background.
    None = 0,
    /// Check in the background without applying an update.
    Background = 1,
    /// Notify the user before applying an update.
    Notify = 2,
    /// Check and apply an update automatically.
    Automatic = 3,
}

impl UpdateMode {
    fn from_legacy(value: u64) -> Option<Self> {
        match value {
            0 => Some(Self::Background),
            1 => Some(Self::Background),
            2 => Some(Self::Notify),
            3 => Some(Self::Automatic),
            _ => None,
        }
    }
}

/// Whether persisted config may be changed during this process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigAccess {
    /// Ordinary writable configuration.
    Writable,
    /// Read-only launch policy that enables exactly the listed signatures.
    ReadOnlyAllowlist(BTreeSet<u32>),
}

impl ConfigAccess {
    /// Creates a read-only allowlist from signature values.
    #[must_use]
    pub fn read_only(signatures: impl IntoIterator<Item = u32>) -> Self {
        Self::ReadOnlyAllowlist(signatures.into_iter().collect())
    }

    /// Returns whether mutations may be persisted.
    #[must_use]
    pub const fn is_writable(&self) -> bool {
        matches!(self, Self::Writable)
    }
}

/// Compatible persistent policy for one add-on signature.
#[derive(Clone, Eq, PartialEq)]
pub struct AddonConfig {
    favorite: bool,
    update_mode: UpdateMode,
    allow_prereleases: bool,
    enabled: bool,
    disable_version: String,
    last_game_build: u32,
    last_name: String,
    persist: bool,
}

impl AddonConfig {
    /// Creates the legacy defaults used for a newly registered add-on.
    #[must_use]
    pub fn registered_default() -> Self {
        Self {
            favorite: false,
            update_mode: UpdateMode::Automatic,
            allow_prereleases: false,
            enabled: true,
            disable_version: String::new(),
            last_game_build: 0,
            last_name: String::new(),
            persist: true,
        }
    }

    /// Returns whether the add-on is marked as a favorite.
    #[must_use]
    pub const fn favorite(&self) -> bool {
        self.favorite
    }

    /// Changes the favorite marker.
    pub fn set_favorite(&mut self, favorite: bool) {
        self.favorite = favorite;
    }

    /// Returns the configured update behavior.
    #[must_use]
    pub const fn update_mode(&self) -> UpdateMode {
        self.update_mode
    }

    /// Changes the update behavior.
    pub fn set_update_mode(&mut self, mode: UpdateMode) {
        self.update_mode = mode;
    }

    /// Returns whether update providers may select prereleases.
    #[must_use]
    pub const fn allow_prereleases(&self) -> bool {
        self.allow_prereleases
    }

    /// Changes prerelease eligibility.
    pub fn set_allow_prereleases(&mut self, allow: bool) {
        self.allow_prereleases = allow;
    }

    /// Returns the desired launch state.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Changes the desired launch state.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Returns the legacy version revision disabled by the user.
    #[must_use]
    pub fn disable_version(&self) -> &str {
        &self.disable_version
    }

    /// Changes the disabled version revision.
    pub fn set_disable_version(&mut self, revision: impl Into<String>) {
        self.disable_version = revision.into();
    }

    /// Returns the game build recorded after the last successful activation.
    #[must_use]
    pub const fn last_game_build(&self) -> u32 {
        self.last_game_build
    }

    /// Records the game build after a successful activation.
    pub fn set_last_game_build(&mut self, build: u32) {
        self.last_game_build = build;
    }

    /// Returns the last known add-on name.
    #[must_use]
    pub fn last_name(&self) -> &str {
        &self.last_name
    }

    /// Records the last known add-on name.
    pub fn set_last_name(&mut self, name: impl Into<String>) {
        self.last_name = name.into();
    }

    /// Returns whether this record should be written on the next save.
    #[must_use]
    pub const fn persist(&self) -> bool {
        self.persist
    }

    /// Changes the runtime-only persistence marker.
    pub fn set_persist(&mut self, persist: bool) {
        self.persist = persist;
    }
}

impl Default for AddonConfig {
    fn default() -> Self {
        Self::registered_default()
    }
}

impl fmt::Debug for AddonConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AddonConfig")
            .field("favorite", &self.favorite)
            .field("update_mode", &self.update_mode)
            .field("allow_prereleases", &self.allow_prereleases)
            .field("enabled", &self.enabled)
            .field("last_game_build", &self.last_game_build)
            .field("persist", &self.persist)
            .finish_non_exhaustive()
    }
}

struct KnownEntry {
    signature: u32,
    config: AddonConfig,
    unknown: BTreeMap<String, Value>,
}

enum ConfigEntry {
    Known(KnownEntry),
    Opaque(Value),
}

/// Ordered, unknown-preserving legacy `AddonConfig.json` document.
pub struct AddonConfigDocument {
    entries: Vec<ConfigEntry>,
}

impl AddonConfigDocument {
    /// Creates an empty writable document.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Parses a bounded legacy JSON array atomically.
    pub fn parse(bytes: &[u8]) -> Result<Self, ConfigError> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        let root: Value =
            serde_json::from_slice(bytes).map_err(|_error| ConfigError::MalformedJson)?;
        let Value::Array(values) = root else {
            return Err(ConfigError::RootNotArray);
        };
        if values.len() > MAX_CONFIG_ENTRIES {
            return Err(ConfigError::TooManyEntries);
        }

        let mut entries = Vec::with_capacity(values.len());
        let mut seen = BTreeMap::<u32, ()>::new();
        for value in values {
            let Some(known) = parse_known_entry(&value) else {
                entries.push(ConfigEntry::Opaque(value));
                continue;
            };
            if seen.insert(known.signature, ()).is_some() {
                entries.push(ConfigEntry::Opaque(value));
            } else {
                entries.push(ConfigEntry::Known(known));
            }
        }
        Ok(Self { entries })
    }

    /// Loads a bounded document while discarding path-bearing I/O details.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let file = File::open(path).map_err(|_error| ConfigError::Read)?;
        let mut bytes = Vec::new();
        file.take((MAX_CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_error| ConfigError::Read)?;
        Self::parse(&bytes)
    }

    /// Serializes deterministic tab-indented legacy JSON with a trailing newline.
    pub fn to_json(&self) -> Result<Vec<u8>, ConfigError> {
        let values: Vec<_> = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ConfigEntry::Known(known) if known.config.persist => Some(known_to_value(known)),
                ConfigEntry::Known(_) => None,
                ConfigEntry::Opaque(value) => Some(value.clone()),
            })
            .collect();
        let mut bytes = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
        let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
        values
            .serialize(&mut serializer)
            .map_err(|_error| ConfigError::Serialize)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        Ok(bytes)
    }

    /// Writes the deterministic document and flushes it to stable storage.
    ///
    /// Callers requiring platform-specific atomic replacement should stage the
    /// bytes returned by [`Self::to_json`] through their own filesystem layer.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let bytes = self.to_json()?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .map_err(|_error| ConfigError::Write)?;
        file.write_all(&bytes)
            .map_err(|_error| ConfigError::Write)?;
        file.sync_all().map_err(|_error| ConfigError::Write)
    }

    /// Returns the first valid config for `signature`.
    #[must_use]
    pub fn get(&self, signature: u32) -> Option<&AddonConfig> {
        self.entries.iter().find_map(|entry| match entry {
            ConfigEntry::Known(known) if known.signature == signature => Some(&known.config),
            _ => None,
        })
    }

    /// Returns a mutable config, registering legacy defaults when absent.
    pub fn get_or_insert(&mut self, signature: u32) -> Result<&mut AddonConfig, ConfigError> {
        if signature == 0 {
            return Err(ConfigError::ZeroSignature);
        }
        let existing = self.entries.iter().position(
            |entry| matches!(entry, ConfigEntry::Known(known) if known.signature == signature),
        );
        let index = existing.unwrap_or_else(|| {
            self.entries.push(ConfigEntry::Known(KnownEntry {
                signature,
                config: AddonConfig::registered_default(),
                unknown: BTreeMap::new(),
            }));
            self.entries.len() - 1
        });
        match self.entries.get_mut(index) {
            Some(ConfigEntry::Known(known)) => Ok(&mut known.config),
            _ => Err(ConfigError::InternalInvariant),
        }
    }

    /// Removes every entry carrying `signature`, including opaque duplicates.
    pub fn remove(&mut self, signature: u32) {
        self.entries.retain(|entry| match entry {
            ConfigEntry::Known(known) => known.signature != signature,
            ConfigEntry::Opaque(Value::Object(object)) => object
                .get(SIGNATURE)
                .and_then(Value::as_u64)
                .is_none_or(|value| value != u64::from(signature)),
            ConfigEntry::Opaque(_) => true,
        });
    }

    /// Returns the number of recognized configs.
    #[must_use]
    pub fn known_len(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry, ConfigEntry::Known(_)))
            .count()
    }

    /// Returns the number of fully preserved opaque entries.
    #[must_use]
    pub fn opaque_len(&self) -> usize {
        self.entries.len() - self.known_len()
    }
}

impl Default for AddonConfigDocument {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for AddonConfigDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AddonConfigDocument")
            .field("known_entries", &self.known_len())
            .field("opaque_entries", &self.opaque_len())
            .finish()
    }
}

/// Closed config failures that never include JSON or filesystem paths.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ConfigError {
    /// The bounded input or output exceeded eight MiB.
    #[error("add-on config exceeds its size limit")]
    TooLarge,
    /// The JSON input was malformed.
    #[error("add-on config is malformed JSON")]
    MalformedJson,
    /// The root value was not the legacy array shape.
    #[error("add-on config root must be an array")]
    RootNotArray,
    /// The document exceeded its entry limit.
    #[error("add-on config contains too many entries")]
    TooManyEntries,
    /// Signature zero is reserved and cannot be registered.
    #[error("add-on config signature cannot be zero")]
    ZeroSignature,
    /// The config file could not be read.
    #[error("add-on config could not be read")]
    Read,
    /// The config file could not be written.
    #[error("add-on config could not be written")]
    Write,
    /// Deterministic JSON serialization failed.
    #[error("add-on config could not be serialized")]
    Serialize,
    /// An internal document invariant was violated.
    #[error("add-on config document invariant failed")]
    InternalInvariant,
}

fn parse_known_entry(value: &Value) -> Option<KnownEntry> {
    let Value::Object(object) = value else {
        return None;
    };
    let signature = object.get(SIGNATURE)?.as_u64()?;
    let signature = u32::try_from(signature).ok().filter(|value| *value != 0)?;

    let mut update_mode = UpdateMode::Automatic;
    if let Some(value) = object.get(MIGRATION_PAUSE_UPDATES) {
        update_mode = if value.as_bool()? {
            UpdateMode::Background
        } else {
            UpdateMode::Automatic
        };
    }
    if let Some(value) = object.get(UPDATE_MODE) {
        update_mode = UpdateMode::from_legacy(value.as_u64()?)?;
    }

    let config = AddonConfig {
        favorite: read_bool(object, FAVORITE, false)?,
        update_mode,
        allow_prereleases: read_bool(object, PRERELEASES, false)?,
        enabled: read_bool(object, LOADED, false)?,
        disable_version: read_string(object, DISABLE_VERSION, "")?,
        last_game_build: read_u32(object, LAST_GAME_BUILD, 0)?,
        last_name: read_string(object, NAME, "")?,
        persist: true,
    };

    let mut unknown: BTreeMap<_, _> = object.clone().into_iter().collect();
    for key in [
        SIGNATURE,
        FAVORITE,
        UPDATE_MODE,
        PRERELEASES,
        LOADED,
        DISABLE_VERSION,
        LAST_GAME_BUILD,
        NAME,
    ] {
        unknown.remove(key);
    }
    Some(KnownEntry {
        signature,
        config,
        unknown,
    })
}

fn known_to_value(entry: &KnownEntry) -> Value {
    let mut object: Map<String, Value> = entry.unknown.clone().into_iter().collect();
    object.insert(SIGNATURE.into(), Value::from(entry.signature));
    object.insert(FAVORITE.into(), Value::from(entry.config.favorite));
    object.insert(
        UPDATE_MODE.into(),
        Value::from(entry.config.update_mode as u32),
    );
    object.insert(
        PRERELEASES.into(),
        Value::from(entry.config.allow_prereleases),
    );
    object.insert(LOADED.into(), Value::from(entry.config.enabled));
    object.insert(
        DISABLE_VERSION.into(),
        Value::from(entry.config.disable_version.clone()),
    );
    object.insert(
        LAST_GAME_BUILD.into(),
        Value::from(entry.config.last_game_build),
    );
    object.insert(NAME.into(), Value::from(entry.config.last_name.clone()));
    Value::Object(object)
}

fn read_bool(object: &Map<String, Value>, key: &str, default: bool) -> Option<bool> {
    object.get(key).map_or(Some(default), Value::as_bool)
}

fn read_string(object: &Map<String, Value>, key: &str, default: &str) -> Option<String> {
    object.get(key).map_or_else(
        || Some(default.to_owned()),
        |value| value.as_str().map(str::to_owned),
    )
}

fn read_u32(object: &Map<String, Value>, key: &str, default: u32) -> Option<u32> {
    object.get(key).map_or(Some(default), |value| {
        value.as_u64().and_then(|number| u32::try_from(number).ok())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{AddonConfigDocument, UpdateMode};

    #[test]
    fn golden_round_trip_preserves_unknown_fields_entries_and_migration_key() {
        let input = br#"[
          {
            "Signature": 7,
            "IsLoaded": true,
            "IsPausingUpdates": true,
            "FuturePolicy": {"opaque": [1, 2, 3]}
          },
          {"FutureRootEntry": true},
          {"Signature": 7, "DuplicateFuture": "preserve"}
        ]"#;
        let mut document = AddonConfigDocument::parse(input).expect("fixture should parse");
        assert_eq!(document.known_len(), 1);
        assert_eq!(document.opaque_len(), 2);
        let config = document.get(7).expect("known config should exist");
        assert!(config.enabled());
        assert_eq!(config.update_mode(), UpdateMode::Background);

        document
            .get_or_insert(7)
            .expect("known config should be mutable")
            .set_favorite(true);
        let output = document.to_json().expect("document should serialize");
        let golden = concat!(
            "[\n",
            "\t{\n",
            "\t\t\"AllowPrereleases\": false,\n",
            "\t\t\"DisableVersion\": \"\",\n",
            "\t\t\"FuturePolicy\": {\n",
            "\t\t\t\"opaque\": [\n",
            "\t\t\t\t1,\n",
            "\t\t\t\t2,\n",
            "\t\t\t\t3\n",
            "\t\t\t]\n",
            "\t\t},\n",
            "\t\t\"IsFavorite\": true,\n",
            "\t\t\"IsLoaded\": true,\n",
            "\t\t\"IsPausingUpdates\": true,\n",
            "\t\t\"LastGameBuild\": 0,\n",
            "\t\t\"Name\": \"\",\n",
            "\t\t\"Signature\": 7,\n",
            "\t\t\"UpdateMode\": 1\n",
            "\t},\n",
            "\t{\n",
            "\t\t\"FutureRootEntry\": true\n",
            "\t},\n",
            "\t{\n",
            "\t\t\"DuplicateFuture\": \"preserve\",\n",
            "\t\t\"Signature\": 7\n",
            "\t}\n",
            "]\n",
        );
        assert_eq!(output, golden.as_bytes());
        assert_eq!(output.last(), Some(&b'\n'));
        let value: Value = serde_json::from_slice(&output).expect("output should remain JSON");
        let array = value.as_array().expect("root should remain an array");
        assert_eq!(array.len(), 3);
        assert_eq!(array[0]["FuturePolicy"]["opaque"][2], 3);
        assert_eq!(array[0]["IsPausingUpdates"], true);
        assert_eq!(array[0]["UpdateMode"], 1);
        assert_eq!(array[0]["IsFavorite"], true);
        assert_eq!(array[1]["FutureRootEntry"], true);
        assert_eq!(array[2]["DuplicateFuture"], "preserve");
    }

    #[test]
    fn invalid_known_types_are_preserved_opaquely_without_partial_apply() {
        let input = br#"[
          {"Signature": 1, "IsLoaded": "not-a-bool", "Future": 9},
          {"Signature": 2, "IsLoaded": false}
        ]"#;
        let document = AddonConfigDocument::parse(input).expect("valid JSON should parse");
        assert!(document.get(1).is_none());
        assert_eq!(document.opaque_len(), 1);
        assert!(
            !document
                .get(2)
                .expect("second config should parse")
                .enabled()
        );
        let output = document.to_json().expect("opaque value should serialize");
        let value: Value = serde_json::from_slice(&output).expect("output should parse");
        assert_eq!(value[0]["IsLoaded"], "not-a-bool");
        assert_eq!(value[0]["Future"], 9);
    }

    #[test]
    fn new_configs_use_the_legacy_registered_enabled_default() {
        let mut document = AddonConfigDocument::new();
        assert!(
            document
                .get_or_insert(42)
                .expect("non-zero signature should register")
                .enabled()
        );
    }
}
