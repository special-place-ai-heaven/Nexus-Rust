//! Bounded networking, persistent cache, and transactional update foundations.
//!
//! The crate deliberately separates policy from I/O: applications inject a
//! [`Transport`], [`Clock`], and [`FileSystem`], while the library enforces body
//! limits, legacy cache lifetimes, and rollback-safe replacement ordering.
//!
//! ```
//! use nexus_network::Version;
//!
//! let release: Version = "v2.4.1.7".parse()?;
//! assert_eq!(release.to_string(), "2.4.1.7");
//! # Ok::<(), nexus_network::VersionParseError>(())
//! ```

#![deny(unsafe_code)]

pub mod cache;
pub mod clock;
pub mod filesystem;
pub mod http;
pub mod update;

#[cfg(test)]
mod test_support;

pub use cache::{
    CacheError, CachePolicy, CacheStore, CacheStoreError, DirectoryCacheStore, HttpCache,
    MemoryCacheStore, NullCacheStore, cache_key_for_query, legacy_normalize_query,
};
pub use clock::{Clock, SystemClock};
pub use filesystem::{FileSystem, FileSystemError, StdFileSystem};
pub use http::{
    BaseUrl, BodyDecodeError, ClientError, DEFAULT_CACHE_MAX_AGE, DownloadError, DownloadReceipt,
    GITHUB_API_CACHE_MAX_AGE, HttpClient, HttpClientConfig, HttpRequest, HttpResponse,
    RAIDCORE_API_CACHE_MAX_AGE, RequestError, Transport, TransportError, TransportResponse,
    legacy_cache_max_age, status_message,
};
pub use update::{
    AddonRelease, CommitError, CommitOutcome, CommitPlan, DigestParseError, DownloadAttemptError,
    DownloadPlan, DownloadSource, GithubReleaseError, GithubRepositoryError, Md5Digest,
    MetadataError, NEXUS_VERSION_ENDPOINT, PlanError, PlannedDownloader, ReleaseMetadata,
    ReplacementError, ReplacementPlan, SelfUpdateDecision, StageError, StagedArtifact,
    UpdateCheckError, Version, VersionParseError, apply_replacement, commit_download,
    direct_addon_update_available, fetch_release_metadata, github_latest_release_endpoint,
    github_releases_endpoint, legacy_direct_checksum_sources, legacy_self_update_sources,
    plan_self_update, select_github_addon_update, select_latest_github_dll, stage_with_fallback,
};
