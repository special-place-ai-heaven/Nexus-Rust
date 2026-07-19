//! Bounded networking, persistent cache, and transactional update foundations.
//!
//! The crate deliberately separates policy from I/O: applications inject a
//! [`Transport`], [`Clock`], and [`FileSystem`], while the library enforces body
//! limits, caller-selected cache policy, and rollback-safe replacement ordering.
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
    HttpClient, HttpClientConfig, HttpRequest, HttpResponse, RequestError, Transport,
    TransportError, TransportResponse, status_message,
};
pub use update::{
    AddonRelease, CommitError, CommitOutcome, CommitPlan, DigestParseError, DownloadAttemptError,
    DownloadPlan, DownloadSource, GITHUB_API_BASE_URL, GithubReleaseError, GithubRepositoryError,
    Md5Digest, MetadataError, NEXUS_RUST_LATEST_RELEASE_ENDPOINT, PlanError, PlannedDownloader,
    ReleaseMetadata, ReplacementError, ReplacementPlan, SelfUpdateDecision, StageError,
    StagedArtifact, UpdateCheckError, Version, VersionParseError, apply_replacement,
    commit_download, direct_addon_update_available, fetch_release_metadata,
    github_latest_release_endpoint, github_releases_endpoint, legacy_direct_checksum_sources,
    plan_self_update, select_github_addon_update, select_latest_github_dll, self_update_sources,
    stage_with_fallback,
};
