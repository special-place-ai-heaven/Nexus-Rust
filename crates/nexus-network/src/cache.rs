//! Legacy-compatible HTTP response caching with safe on-disk keys.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::filesystem::{FileSystem, FileSystemError};
use crate::http::HttpResponse;

const DEFAULT_MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;

/// Selects the lifetime used for one cache lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CachePolicy {
    /// Uses the cache's configured default lifetime.
    Default,
    /// Uses an explicit maximum age. Zero invalidates any existing entry.
    MaxAge(Duration),
}

/// A redaction-safe persistent-cache storage failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CacheStoreError {
    /// The stored entry exceeded its configured bound.
    #[error("cache entry exceeded the configured bound")]
    EntryTooLarge,
    /// A storage operation failed.
    #[error("cache storage operation failed")]
    OperationFailed,
}

impl From<FileSystemError> for CacheStoreError {
    fn from(error: FileSystemError) -> Self {
        if error == FileSystemError::LimitExceeded {
            Self::EntryTooLarge
        } else {
            Self::OperationFailed
        }
    }
}

/// A redaction-safe cache lookup or serialization failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CacheError {
    /// Persistent storage was unavailable.
    #[error("HTTP cache storage was unavailable")]
    Storage,
    /// A response body could not be represented by the legacy string cache.
    #[error("HTTP response body was not valid UTF-8 and was not cached")]
    NonUtf8Body,
    /// A cache record could not be encoded.
    #[error("HTTP cache record could not be encoded")]
    Encoding,
}

/// Persistent storage boundary for HTTP cache records.
pub trait CacheStore {
    /// Loads an encoded record by its safe storage key.
    fn load(&mut self, key: &str, limit: usize) -> Result<Option<Vec<u8>>, CacheStoreError>;

    /// Atomically stores an encoded record by its safe storage key.
    fn store_atomic(&mut self, key: &str, value: &[u8]) -> Result<(), CacheStoreError>;

    /// Removes one encoded record; a missing record is treated as success.
    fn remove(&mut self, key: &str) -> Result<(), CacheStoreError>;

    /// Removes every encoded record owned by this store.
    fn clear(&mut self) -> Result<(), CacheStoreError>;
}

/// A no-op cache store used by clients with caching disabled.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullCacheStore;

impl CacheStore for NullCacheStore {
    fn load(&mut self, _key: &str, _limit: usize) -> Result<Option<Vec<u8>>, CacheStoreError> {
        Ok(None)
    }

    fn store_atomic(&mut self, _key: &str, _value: &[u8]) -> Result<(), CacheStoreError> {
        Ok(())
    }

    fn remove(&mut self, _key: &str) -> Result<(), CacheStoreError> {
        Ok(())
    }

    fn clear(&mut self) -> Result<(), CacheStoreError> {
        Ok(())
    }
}

/// In-memory persistent-store substitute useful for embedded and test clients.
#[derive(Clone, Debug, Default)]
pub struct MemoryCacheStore {
    entries: HashMap<String, Vec<u8>>,
}

impl MemoryCacheStore {
    /// Returns the number of encoded records in the store.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the store contains no encoded records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl CacheStore for MemoryCacheStore {
    fn load(&mut self, key: &str, limit: usize) -> Result<Option<Vec<u8>>, CacheStoreError> {
        let Some(value) = self.entries.get(key) else {
            return Ok(None);
        };

        if value.len() > limit {
            return Err(CacheStoreError::EntryTooLarge);
        }

        Ok(Some(value.clone()))
    }

    fn store_atomic(&mut self, key: &str, value: &[u8]) -> Result<(), CacheStoreError> {
        self.entries.insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    fn remove(&mut self, key: &str) -> Result<(), CacheStoreError> {
        self.entries.remove(key);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), CacheStoreError> {
        self.entries.clear();
        Ok(())
    }
}

/// A directory-backed cache store using an injected filesystem.
pub struct DirectoryCacheStore<F> {
    filesystem: F,
    directory: PathBuf,
}

impl<F> DirectoryCacheStore<F> {
    /// Creates a cache store rooted at `directory`.
    #[must_use]
    pub fn new(filesystem: F, directory: impl Into<PathBuf>) -> Self {
        Self {
            filesystem,
            directory: directory.into(),
        }
    }

    /// Returns the underlying filesystem boundary.
    #[must_use]
    pub const fn filesystem(&self) -> &F {
        &self.filesystem
    }

    /// Consumes the store and returns its filesystem boundary.
    #[must_use]
    pub fn into_filesystem(self) -> F {
        self.filesystem
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.directory.join(key)
    }
}

impl<F: FileSystem> CacheStore for DirectoryCacheStore<F> {
    fn load(&mut self, key: &str, limit: usize) -> Result<Option<Vec<u8>>, CacheStoreError> {
        self.filesystem
            .read_bounded(&self.entry_path(key), limit)
            .map_err(CacheStoreError::from)
    }

    fn store_atomic(&mut self, key: &str, value: &[u8]) -> Result<(), CacheStoreError> {
        self.filesystem
            .write_atomic(&self.entry_path(key), value)
            .map_err(CacheStoreError::from)
    }

    fn remove(&mut self, key: &str) -> Result<(), CacheStoreError> {
        self.filesystem
            .remove_file(&self.entry_path(key))
            .map_err(CacheStoreError::from)
    }

    fn clear(&mut self) -> Result<(), CacheStoreError> {
        self.filesystem
            .remove_dir_all(&self.directory)
            .map_err(CacheStoreError::from)?;
        self.filesystem
            .create_dir_all(&self.directory)
            .map_err(CacheStoreError::from)
    }
}

/// Thread-confined HTTP cache with memory-first lookup and persistent backing.
pub struct HttpCache<S> {
    entries: HashMap<String, CacheRecord>,
    store: S,
    default_max_age: Duration,
    max_record_bytes: usize,
}

impl<S: CacheStore> HttpCache<S> {
    /// Creates a cache with the legacy default lifetime behavior.
    #[must_use]
    pub fn new(store: S, default_max_age: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            store,
            default_max_age,
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
        }
    }

    /// Overrides the maximum encoded record size accepted from storage.
    #[must_use]
    pub fn with_max_record_bytes(mut self, max_record_bytes: usize) -> Self {
        self.max_record_bytes = max_record_bytes.max(1);
        self
    }

    /// Returns the configured default maximum age.
    #[must_use]
    pub const fn default_max_age(&self) -> Duration {
        self.default_max_age
    }

    /// Returns the persistent store.
    #[must_use]
    pub const fn store(&self) -> &S {
        &self.store
    }

    /// Returns the persistent store mutably.
    #[must_use]
    pub const fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// Consumes the cache and returns its persistent store.
    #[must_use]
    pub fn into_store(self) -> S {
        self.store
    }

    /// Looks up `query` using the selected lifetime and a caller-supplied time.
    pub fn lookup(
        &mut self,
        query: &str,
        now: i64,
        policy: CachePolicy,
    ) -> Result<Option<HttpResponse>, CacheError> {
        let max_age = match policy {
            CachePolicy::Default => self.default_max_age,
            CachePolicy::MaxAge(max_age) => max_age,
        };
        let key = cache_key_for_query(query);

        if max_age.is_zero() {
            self.entries.remove(query);
            self.store
                .remove(&key)
                .map_err(|_error| CacheError::Storage)?;
            return Ok(None);
        }

        if let Some(record) = self.entries.get(query).cloned() {
            if record.is_fresh(now, max_age) {
                return Ok(Some(record.into_response(true)));
            }

            self.entries.remove(query);
            self.store
                .remove(&key)
                .map_err(|_error| CacheError::Storage)?;
            return Ok(None);
        }

        let Some(encoded) = self
            .store
            .load(&key, self.max_record_bytes)
            .map_err(|_error| CacheError::Storage)?
        else {
            return Ok(None);
        };

        let Ok(record) = serde_json::from_slice::<CacheRecord>(&encoded) else {
            self.store
                .remove(&key)
                .map_err(|_error| CacheError::Storage)?;
            return Ok(None);
        };

        if !record.is_fresh(now, max_age) {
            self.store
                .remove(&key)
                .map_err(|_error| CacheError::Storage)?;
            return Ok(None);
        }

        let response = record.clone().into_response(true);
        self.entries.insert(query.to_owned(), record);
        Ok(Some(response))
    }

    /// Stores a successful response under `query`.
    pub fn store_response(
        &mut self,
        query: &str,
        response: &HttpResponse,
    ) -> Result<(), CacheError> {
        if !response.is_success() {
            return Ok(());
        }

        let content = std::str::from_utf8(response.body())
            .map_err(|_error| CacheError::NonUtf8Body)?
            .to_owned();
        let record = CacheRecord {
            time: response.time(),
            status_code: response.status_code(),
            error: String::new(),
            content,
        };
        let encoded = serde_json::to_vec_pretty(&record).map_err(|_error| CacheError::Encoding)?;

        if encoded.len() > self.max_record_bytes {
            return Err(CacheError::Encoding);
        }

        let key = cache_key_for_query(query);
        self.store
            .store_atomic(&key, &encoded)
            .map_err(|_error| CacheError::Storage)?;
        self.entries.insert(query.to_owned(), record);
        Ok(())
    }

    /// Clears memory and, when requested, the persistent backing store.
    pub fn flush(&mut self, clear_persistent: bool) -> Result<(), CacheError> {
        self.entries.clear();
        if clear_persistent {
            self.store.clear().map_err(|_error| CacheError::Storage)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheRecord {
    #[serde(rename = "Time")]
    time: i64,
    #[serde(rename = "StatusCode")]
    status_code: u16,
    #[serde(rename = "Error")]
    error: String,
    #[serde(rename = "Content")]
    content: String,
}

impl CacheRecord {
    fn is_fresh(&self, now: i64, max_age: Duration) -> bool {
        if !self.error.is_empty() || self.status_code >= 400 || self.time > now {
            return false;
        }

        let Ok(age) = u64::try_from(now.saturating_sub(self.time)) else {
            return false;
        };
        age < max_age.as_secs()
    }

    fn into_response(self, from_cache: bool) -> HttpResponse {
        HttpResponse::from_parts(
            self.time,
            self.status_code,
            self.content.into_bytes(),
            from_cache,
        )
    }
}

/// Produces a collision-resistant, path-safe filename for a request query.
#[must_use]
pub fn cache_key_for_query(query: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let digest = Sha256::digest(query.as_bytes());
    let mut output = String::with_capacity(64 + ".json".len());
    for byte in digest {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output.push_str(".json");
    output
}

/// Reproduces the C++ cache's historical query-to-path normalization.
///
/// New stores use [`cache_key_for_query`] because the historical output can
/// collide and can still contain path-significant dot segments.
#[must_use]
pub fn legacy_normalize_query(query: &str) -> String {
    let query = query.strip_prefix('/').unwrap_or(query);
    query
        .replace(':', "{col}")
        .replace('*', "{ast}")
        .replace('?', "{qst}")
        .replace('"', "{quot}")
        .replace('<', "{lt}")
        .replace('>', "{gt}")
        .replace('|', "{pipe}")
        .replace('/', "\\")
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::{
        CacheError, CachePolicy, CacheStore, DirectoryCacheStore, HttpCache, MemoryCacheStore,
        cache_key_for_query, legacy_normalize_query,
    };
    use crate::http::HttpResponse;
    use crate::test_support::TestFileSystem;

    fn response(time: i64, status_code: u16, body: &[u8]) -> HttpResponse {
        HttpResponse::from_parts(time, status_code, body.to_vec(), false)
    }

    #[test]
    fn legacy_query_normalization_matches_the_cpp_mapping() {
        assert_eq!(
            legacy_normalize_query("/a:b*c?d\"e<f>g|h/i"),
            "a{col}b{ast}c{qst}d{quot}e{lt}f{gt}g{pipe}h\\i"
        );
        assert_eq!(legacy_normalize_query("//nested"), "\\nested");
    }

    #[test]
    fn cache_keys_are_fixed_length_and_path_safe() {
        let key = cache_key_for_query("/../../outside?token=not-logged");
        assert_eq!(key.len(), 69);
        assert!(key.ends_with(".json"));
        assert!(!key.contains('/'));
        assert!(!key.contains('\\'));
        assert!(!key.contains(".."));
        assert_eq!(key, cache_key_for_query("/../../outside?token=not-logged"));
    }

    #[test]
    fn cache_uses_strict_legacy_expiry_boundary() {
        let mut cache = HttpCache::new(MemoryCacheStore::default(), Duration::from_secs(30));
        assert!(
            cache
                .store_response("/query", &response(100, 200, b"body"))
                .is_ok()
        );

        let hit = cache.lookup("/query", 129, CachePolicy::Default);
        let Ok(Some(hit)) = hit else {
            panic!("expected a fresh cache hit");
        };
        assert!(hit.is_cached());
        assert_eq!(hit.body(), b"body");

        assert!(matches!(
            cache.lookup("/query", 130, CachePolicy::Default),
            Ok(None)
        ));
        assert!(cache.store().is_empty());
    }

    #[test]
    fn zero_max_age_invalidates_memory_and_persistent_entries() {
        let mut cache = HttpCache::new(MemoryCacheStore::default(), Duration::from_secs(30));
        assert!(
            cache
                .store_response("/query", &response(100, 200, b"body"))
                .is_ok()
        );
        assert!(matches!(
            cache.lookup("/query", 100, CachePolicy::MaxAge(Duration::ZERO)),
            Ok(None)
        ));
        assert!(cache.store().is_empty());
        assert!(matches!(
            cache.lookup("/query", 100, CachePolicy::Default),
            Ok(None)
        ));
    }

    #[test]
    fn a_new_cache_instance_reads_the_legacy_json_record() {
        let mut cache = HttpCache::new(MemoryCacheStore::default(), Duration::from_secs(60));
        assert!(
            cache
                .store_response("/query", &response(50, 201, b"created"))
                .is_ok()
        );
        let store = cache.into_store();
        let mut reloaded = HttpCache::new(store, Duration::from_secs(60));

        let hit = reloaded.lookup("/query", 75, CachePolicy::Default);
        let Ok(Some(hit)) = hit else {
            panic!("expected a persistent cache hit");
        };
        assert_eq!(hit.status_code(), 201);
        assert_eq!(hit.text(), Ok("created"));
    }

    #[test]
    fn corrupt_and_future_records_are_deleted_instead_of_served() {
        let key = cache_key_for_query("/corrupt");
        let mut store = MemoryCacheStore::default();
        assert!(store.store_atomic(&key, b"not json").is_ok());
        let mut cache = HttpCache::new(store, Duration::from_secs(60));
        assert!(matches!(
            cache.lookup("/corrupt", 100, CachePolicy::Default),
            Ok(None)
        ));
        assert!(cache.store().is_empty());

        let future = br#"{"Time":101,"StatusCode":200,"Error":"","Content":"body"}"#;
        let future_key = cache_key_for_query("/future");
        assert!(cache.store_mut().store_atomic(&future_key, future).is_ok());
        assert!(matches!(
            cache.lookup("/future", 100, CachePolicy::Default),
            Ok(None)
        ));
    }

    #[test]
    fn only_successful_utf8_responses_are_cached() {
        let mut cache = HttpCache::new(MemoryCacheStore::default(), Duration::from_secs(60));
        assert!(
            cache
                .store_response("/missing", &response(1, 404, b"missing"))
                .is_ok()
        );
        assert!(cache.store().is_empty());
        assert_eq!(
            cache.store_response("/binary", &response(1, 200, &[0xff])),
            Err(CacheError::NonUtf8Body)
        );
        assert!(cache.store().is_empty());
    }

    #[test]
    fn directory_store_never_uses_the_legacy_query_as_a_path() {
        let filesystem = TestFileSystem::default();
        let store = DirectoryCacheStore::new(filesystem, Path::new("cache"));
        let mut cache = HttpCache::new(store, Duration::from_secs(60));
        let query = "/../../escape";
        assert!(
            cache
                .store_response(query, &response(1, 200, b"safe"))
                .is_ok()
        );

        let expected = Path::new("cache").join(cache_key_for_query(query));
        assert!(cache.store().filesystem().contains(expected));
        assert!(!cache.store().filesystem().contains("escape.json"));
    }

    #[test]
    fn flush_can_preserve_or_clear_persistent_records() {
        let mut cache = HttpCache::new(MemoryCacheStore::default(), Duration::from_secs(60));
        assert!(
            cache
                .store_response("/query", &response(1, 200, b"body"))
                .is_ok()
        );
        assert!(cache.flush(false).is_ok());
        assert_eq!(cache.store().len(), 1);
        assert!(cache.flush(true).is_ok());
        assert!(cache.store().is_empty());
    }
}
