use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::ser::PrettyFormatter;
use serde_json::{Map, Serializer, Value};
use thiserror::Error;

type Callback = Arc<dyn Fn(&Value) + Send + Sync + 'static>;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

/// The result of loading the settings file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadOutcome {
    /// No settings file existed, so the current in-memory value was retained.
    Missing,
    /// A settings file was parsed and replaced the current in-memory value.
    Loaded,
}

/// A stable identifier for an installed settings callback.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SubscriptionId(u64);

impl SubscriptionId {
    /// Returns the process-local numeric identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Results from a contained notification dispatch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NotificationReport {
    /// Number of callbacks invoked.
    pub attempted: usize,
    /// Number of callbacks that panicked and were contained.
    pub panicked: usize,
}

/// A thread-safe `Settings.json` store with legacy notification semantics.
pub struct SettingsStore {
    path: PathBuf,
    state: Mutex<State>,
}

impl std::fmt::Debug for SettingsStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettingsStore")
            .field("path", &"<redacted>")
            .finish_non_exhaustive()
    }
}

struct State {
    value: Value,
    next_subscription: u64,
    callbacks: HashMap<String, BTreeMap<SubscriptionId, Callback>>,
}

impl SettingsStore {
    /// Creates an empty store for an injected path without performing I/O.
    #[must_use]
    pub fn empty(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            state: Mutex::new(State {
                value: Value::Object(Map::new()),
                next_subscription: 1,
                callbacks: HashMap::new(),
            }),
        }
    }

    /// Opens a store and loads it if present.
    ///
    /// # Errors
    ///
    /// Returns a closed error when the file cannot be read or parsed. No path,
    /// key, or setting value is included in its display text.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SettingsError> {
        let store = Self::empty(path);
        store.load()?;
        Ok(store)
    }

    /// Loads the complete JSON document without notifying subscribers.
    ///
    /// A parse failure retains the previous in-memory document, matching the
    /// assignment behavior of the legacy parser.
    ///
    /// # Errors
    ///
    /// Returns a redacted read or parse error.
    pub fn load(&self) -> Result<LoadOutcome, SettingsError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(LoadOutcome::Missing);
            }
            Err(source) => return Err(SettingsError::Read { source }),
        };

        let value =
            serde_json::from_slice::<Value>(&bytes).map_err(|error| SettingsError::Parse {
                category: error.classify(),
                line: error.line(),
                column: error.column(),
            })?;
        lock_unpoison(&self.state).value = value;
        Ok(LoadOutcome::Loaded)
    }

    /// Atomically persists the complete JSON document.
    ///
    /// # Errors
    ///
    /// Returns a redacted serialization or persistence error.
    pub fn save(&self) -> Result<(), SettingsError> {
        let state = lock_unpoison(&self.state);
        persist_value(&self.path, &state.value)
    }

    /// Returns a clone of the full JSON document, including unknown fields.
    #[must_use]
    pub fn snapshot(&self) -> Value {
        lock_unpoison(&self.state).value.clone()
    }

    /// Reads and deserializes a non-null setting.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::RootIsNotObject`] for a non-object document or
    /// [`SettingsError::ValueTypeMismatch`] when the value has another type.
    pub fn get<T>(&self, key: &str) -> Result<Option<T>, SettingsError>
    where
        T: DeserializeOwned,
    {
        let state = lock_unpoison(&self.state);
        let object = object_ref(&state.value)?;
        let Some(value) = object.get(key).filter(|value| !value.is_null()) else {
            return Ok(None);
        };
        deserialize_closed(value).map(Some)
    }

    /// Gets a non-null setting or inserts, persists, and notifies its default.
    ///
    /// Persistence happens before notification. Callback panics are contained.
    /// The in-memory mutation and notification still occur if persistence
    /// fails, matching the observable legacy order, while the failure is no
    /// longer silently discarded.
    ///
    /// # Errors
    ///
    /// Returns a closed encoding, type, root, or persistence error.
    pub fn get_or_insert<T>(&self, key: &str, default: T) -> Result<T, SettingsError>
    where
        T: DeserializeOwned + Serialize,
    {
        let encoded = serialize_closed(&default)?;
        let (callbacks, persistence) = {
            let mut state = lock_unpoison(&self.state);
            if let Some(existing) = object_ref_or_promote_null(&mut state.value)?
                .get(key)
                .filter(|value| !value.is_null())
            {
                return deserialize_closed(existing);
            }
            object_ref_or_promote_null(&mut state.value)?.insert(key.to_owned(), encoded.clone());
            let persistence = persist_value(&self.path, &state.value);
            let callbacks = callbacks_for(&state, key);
            (callbacks, persistence)
        };

        let _ = notify(&callbacks, &encoded);
        persistence?;
        Ok(default)
    }

    /// Stores, atomically persists, and notifies a setting.
    ///
    /// A null value is persisted but does not notify, matching the legacy
    /// callback contract.
    ///
    /// # Errors
    ///
    /// Returns a closed encoding, root, or persistence error.
    pub fn set<T>(&self, key: &str, value: &T) -> Result<NotificationReport, SettingsError>
    where
        T: Serialize + ?Sized,
    {
        let value = serialize_closed(value)?;
        let (callbacks, persistence) = {
            let mut state = lock_unpoison(&self.state);
            object_ref_or_promote_null(&mut state.value)?.insert(key.to_owned(), value.clone());
            let persistence = persist_value(&self.path, &state.value);
            let callbacks = callbacks_for(&state, key);
            (callbacks, persistence)
        };

        let report = if value.is_null() {
            NotificationReport::default()
        } else {
            notify(&callbacks, &value)
        };
        persistence?;
        Ok(report)
    }

    /// Removes a setting, persists the document, and clears its subscribers.
    ///
    /// Removal does not emit a null notification, matching the legacy API.
    ///
    /// # Errors
    ///
    /// Returns a closed root or persistence error.
    pub fn remove(&self, key: &str) -> Result<bool, SettingsError> {
        let (removed, persistence) = {
            let mut state = lock_unpoison(&self.state);
            let removed = object_ref_or_promote_null(&mut state.value)?
                .remove(key)
                .is_some();
            let persistence = persist_value(&self.path, &state.value);
            state.callbacks.remove(key);
            (removed, persistence)
        };
        persistence?;
        Ok(removed)
    }

    /// Subscribes to raw JSON changes for one key.
    ///
    /// The callback is immediately invoked when a current non-null value
    /// exists. Panics are contained. Use [`Self::unsubscribe`] for explicit
    /// ownership cleanup, or [`Self::remove`] to clear every callback for a key.
    pub fn subscribe<F>(&self, key: impl Into<String>, callback: F) -> SubscriptionId
    where
        F: Fn(&Value) + Send + Sync + 'static,
    {
        self.subscribe_arc(key.into(), Arc::new(callback))
    }

    /// Subscribes to typed changes, silently skipping conversion failures like
    /// the legacy templated callback wrapper.
    pub fn subscribe_typed<T, F>(&self, key: impl Into<String>, callback: F) -> SubscriptionId
    where
        T: DeserializeOwned,
        F: Fn(T) + Send + Sync + 'static,
    {
        self.subscribe(key, move |value| {
            if let Ok(value) = serde_json::from_value::<T>(value.clone()) {
                callback(value);
            }
        })
    }

    /// Removes one callback without changing its setting.
    #[must_use]
    pub fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut state = lock_unpoison(&self.state);
        state
            .callbacks
            .values_mut()
            .any(|callbacks| callbacks.remove(&id).is_some())
    }

    fn subscribe_arc(&self, key: String, callback: Callback) -> SubscriptionId {
        let (id, current) = {
            let mut state = lock_unpoison(&self.state);
            let id = SubscriptionId(state.next_subscription);
            state.next_subscription = state.next_subscription.saturating_add(1);
            state
                .callbacks
                .entry(key.clone())
                .or_default()
                .insert(id, Arc::clone(&callback));
            let current = state
                .value
                .as_object()
                .and_then(|object| object.get(&key))
                .filter(|value| !value.is_null())
                .cloned();
            (id, current)
        };

        if let Some(current) = current {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| callback(&current)));
        }
        id
    }
}

/// A closed settings error that never formats paths, keys, or JSON values.
#[derive(Debug, Error)]
pub enum SettingsError {
    /// The settings file could not be read.
    #[error("Settings.json could not be read")]
    Read {
        /// The path-free operating-system error.
        #[source]
        source: io::Error,
    },
    /// The settings file was not valid JSON.
    #[error("Settings.json could not be parsed ({category:?} at {line}:{column})")]
    Parse {
        /// The broad serde JSON error category.
        category: serde_json::error::Category,
        /// One-based line number.
        line: usize,
        /// One-based column number.
        column: usize,
    },
    /// A loaded document was not an object and cannot support keyed access.
    #[error("Settings.json root is not an object")]
    RootIsNotObject,
    /// A requested value could not be converted to the requested type.
    #[error("setting value has an incompatible type")]
    ValueTypeMismatch,
    /// A value could not be encoded as JSON.
    #[error("setting value could not be encoded")]
    ValueEncoding,
    /// The complete document could not be serialized.
    #[error("Settings.json could not be serialized")]
    Serialize,
    /// The atomic temporary file could not be created.
    #[error("Settings.json atomic temporary file could not be created")]
    CreateTemporary {
        /// The path-free operating-system error.
        #[source]
        source: io::Error,
    },
    /// The temporary file could not be written or synchronized.
    #[error("Settings.json atomic temporary file could not be written")]
    WriteTemporary {
        /// The path-free operating-system error.
        #[source]
        source: io::Error,
    },
    /// The atomic replacement failed.
    #[error("Settings.json could not be atomically replaced")]
    Replace {
        /// The path-free operating-system error.
        #[source]
        source: io::Error,
    },
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn object_ref(value: &Value) -> Result<&Map<String, Value>, SettingsError> {
    value.as_object().ok_or(SettingsError::RootIsNotObject)
}

fn object_ref_or_promote_null(value: &mut Value) -> Result<&mut Map<String, Value>, SettingsError> {
    if value.is_null() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().ok_or(SettingsError::RootIsNotObject)
}

fn serialize_closed<T>(value: &T) -> Result<Value, SettingsError>
where
    T: Serialize + ?Sized,
{
    serde_json::to_value(value).map_err(|_| SettingsError::ValueEncoding)
}

fn deserialize_closed<T>(value: &Value) -> Result<T, SettingsError>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value.clone()).map_err(|_| SettingsError::ValueTypeMismatch)
}

fn callbacks_for(state: &State, key: &str) -> Vec<Callback> {
    state
        .callbacks
        .get(key)
        .map(|callbacks| callbacks.values().cloned().collect())
        .unwrap_or_default()
}

fn notify(callbacks: &[Callback], value: &Value) -> NotificationReport {
    let mut report = NotificationReport {
        attempted: callbacks.len(),
        panicked: 0,
    };
    for callback in callbacks {
        if panic::catch_unwind(AssertUnwindSafe(|| callback(value))).is_err() {
            report.panicked += 1;
        }
    }
    report
}

fn persist_value(path: &Path, value: &Value) -> Result<(), SettingsError> {
    let mut bytes = Vec::new();
    let formatter = PrettyFormatter::with_indent(b"\t");
    let mut serializer = Serializer::with_formatter(&mut bytes, formatter);
    value
        .serialize(&mut serializer)
        .map_err(|_| SettingsError::Serialize)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SettingsError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut last_collision = None;

    for _ in 0..16 {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".nexus-settings-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(mut file) => {
                let cleanup = TempCleanup(temp.clone());
                file.write_all(bytes)
                    .and_then(|()| file.sync_all())
                    .map_err(|source| SettingsError::WriteTemporary { source })?;
                drop(file);
                atomic_replace(&temp, path).map_err(|source| SettingsError::Replace { source })?;
                cleanup.disarm();
                return Ok(());
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                last_collision = Some(source);
            }
            Err(source) => return Err(SettingsError::CreateTemporary { source }),
        }
    }

    Err(SettingsError::CreateTemporary {
        source: last_collision.unwrap_or_else(|| io::Error::other("temporary name collision")),
    })
}

struct TempCleanup(PathBuf);

impl TempCleanup {
    fn disarm(mut self) {
        self.0.clear();
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both pointers reference live, NUL-terminated UTF-16 buffers for
    // the duration of the call. The flags request a same-volume replacement.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(1);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let id = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nexus-platform-settings-{}-{id}-nastavitve_日本",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("test root should be created");
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn golden_legacy_json_round_trips_with_unknown_fields() {
        let temp = TempRoot::new();
        let path = temp.0.join("Settings.json");
        let golden = include_str!("../tests/fixtures/settings_legacy.json");
        std::fs::write(&path, golden).expect("fixture should be written");

        let settings = SettingsStore::open(&path).expect("fixture should load");
        settings.save().expect("unchanged fixture should save");
        assert_eq!(
            std::fs::read_to_string(&path).expect("saved fixture should be readable"),
            golden,
            "legacy tab indentation, key order, and trailing newline must remain exact"
        );
        assert_eq!(
            settings.get::<bool>("ShowAddons").expect("typed read"),
            Some(true)
        );
        settings
            .set("ShowAddons", &false)
            .expect("setting should persist atomically");

        let reloaded = SettingsStore::open(&path).expect("saved file should load");
        assert_eq!(
            reloaded
                .snapshot()
                .pointer("/FutureSection/Nested")
                .and_then(Value::as_str),
            Some("preserved")
        );
        assert_eq!(
            reloaded.get::<bool>("ShowAddons").expect("typed read"),
            Some(false)
        );
        let saved = std::fs::read_to_string(&path).expect("saved JSON should be readable");
        assert!(saved.contains("\n\t\"FutureSection\""));
        assert!(saved.ends_with('\n'));
        assert_eq!(
            std::fs::read_dir(&temp.0)
                .expect("test root should be readable")
                .count(),
            1,
            "atomic temporary file must not remain"
        );
    }

    #[test]
    fn missing_default_persists_then_notifies_and_subscribe_is_immediate() {
        let temp = TempRoot::new();
        let path = temp.0.join("Settings.json");
        let settings = SettingsStore::empty(&path);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let id = settings.subscribe_typed::<u64, _>("Scale", move |value| {
            assert_eq!(value, 125);
            observed.fetch_add(1, AtomicOrdering::Relaxed);
        });

        assert_eq!(
            settings
                .get_or_insert("Scale", 125_u64)
                .expect("default should persist"),
            125
        );
        assert_eq!(calls.load(AtomicOrdering::Relaxed), 1);

        let immediate = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&immediate);
        settings.subscribe_typed::<u64, _>("Scale", move |_| {
            observed.fetch_add(1, AtomicOrdering::Relaxed);
        });
        assert_eq!(immediate.load(AtomicOrdering::Relaxed), 1);
        assert!(settings.unsubscribe(id));
    }

    #[test]
    fn callbacks_run_outside_lock_and_panics_are_contained() {
        let temp = TempRoot::new();
        let settings = Arc::new(SettingsStore::empty(temp.0.join("Settings.json")));
        let nested = Arc::clone(&settings);
        settings.subscribe("First", move |_| {
            nested
                .set("Second", &2_u32)
                .expect("nested set must not deadlock");
        });
        settings.subscribe("First", |_| panic!("contained test panic"));

        let report = settings
            .set("First", &1_u32)
            .expect("outer setting should persist");
        assert_eq!(report.attempted, 2);
        assert_eq!(report.panicked, 1);
        assert_eq!(settings.get::<u32>("Second").expect("typed read"), Some(2));
    }

    #[test]
    fn remove_clears_subscribers_without_null_notification() {
        let temp = TempRoot::new();
        let settings = SettingsStore::empty(temp.0.join("Settings.json"));
        settings.set("Mode", &1_u32).expect("initial write");
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        settings.subscribe("Mode", move |_| {
            observed.fetch_add(1, AtomicOrdering::Relaxed);
        });
        assert_eq!(calls.load(AtomicOrdering::Relaxed), 1);

        assert!(settings.remove("Mode").expect("remove should persist"));
        settings
            .set("Mode", &2_u32)
            .expect("replacement should persist");
        assert_eq!(calls.load(AtomicOrdering::Relaxed), 1);
    }

    #[test]
    fn parse_failure_preserves_memory_and_redacts_path_and_values() {
        let temp = TempRoot::new();
        let path = temp.0.join("private-folder-name.json");
        let settings = SettingsStore::empty(&path);
        settings
            .set("Visible", &"ordinary-value")
            .expect("initial write");
        std::fs::write(&path, b"{invalid").expect("invalid JSON should be written");

        let error = settings.load().expect_err("invalid JSON should fail");
        let display = error.to_string();
        assert!(!display.contains("private-folder-name"));
        assert!(!display.contains("ordinary-value"));
        assert_eq!(
            settings.get::<String>("Visible").expect("typed read"),
            Some("ordinary-value".to_owned())
        );
    }
}
