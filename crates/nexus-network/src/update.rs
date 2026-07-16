//! Pure update discovery, bounded download plans, and rollback-safe commits.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::cache::{CachePolicy, CacheStore};
use crate::clock::Clock;
use crate::filesystem::{FileSystem, FileSystemError};
use crate::http::{BaseUrl, BodyDecodeError, ClientError, HttpClient, RequestError, Transport};

/// Legacy Nexus update metadata endpoint path.
pub const NEXUS_VERSION_ENDPOINT: &str = "/nexusversion";

/// Four-component Nexus version ordered lexicographically.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Version {
    /// Major compatibility generation.
    pub major: u16,
    /// Minor feature generation.
    pub minor: u16,
    /// Build generation.
    pub build: u16,
    /// Optional revision generation.
    pub revision: u16,
}

impl Version {
    /// Creates a version from its four components.
    #[must_use]
    pub const fn new(major: u16, minor: u16, build: u16, revision: u16) -> Self {
        Self {
            major,
            minor,
            build,
            revision,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.build)?;
        if self.revision > 0 {
            write!(formatter, ".{}", self.revision)?;
        }
        Ok(())
    }
}

impl FromStr for Version {
    type Err = VersionParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.strip_prefix('v').unwrap_or(value);
        let parts: Vec<&str> = value.split('.').collect();
        if !(2..=4).contains(&parts.len()) {
            return Err(VersionParseError);
        }

        let mut parsed = [0_u16; 4];
        for (index, part) in parts.iter().enumerate() {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(VersionParseError);
            }
            parsed[index] = part.parse().map_err(|_error| VersionParseError)?;
        }

        Ok(Self::new(parsed[0], parsed[1], parsed[2], parsed[3]))
    }
}

/// A version string was not `v?major.minor[.build[.revision]]` with `u16` parts.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("invalid Nexus version")]
pub struct VersionParseError;

/// Parsed response from the Nexus version service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseMetadata {
    /// Remote release version.
    pub version: Version,
    /// Optional release notes supplied by the service.
    pub changelog: Option<String>,
}

impl ReleaseMetadata {
    /// Parses the legacy uppercase-field JSON contract.
    pub fn from_json(bytes: &[u8]) -> Result<Self, MetadataError> {
        let wire: ReleaseMetadataWire =
            serde_json::from_slice(bytes).map_err(|_error| MetadataError::InvalidJson)?;
        Ok(Self {
            version: Version::new(wire.major, wire.minor, wire.build, wire.revision),
            changelog: wire.changelog,
        })
    }
}

#[derive(Deserialize)]
struct ReleaseMetadataWire {
    #[serde(rename = "Major")]
    major: u16,
    #[serde(rename = "Minor")]
    minor: u16,
    #[serde(rename = "Build")]
    build: u16,
    #[serde(rename = "Revision")]
    revision: u16,
    #[serde(rename = "Changelog", default)]
    changelog: Option<String>,
}

/// A redaction-safe update metadata parsing failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MetadataError {
    /// The service response did not match the metadata JSON contract.
    #[error("update metadata was invalid")]
    InvalidJson,
}

/// Fetches metadata while bypassing and invalidating any cached copy.
pub fn fetch_release_metadata<T, C, S>(
    client: &mut HttpClient<T, C, S>,
    endpoint: &str,
) -> Result<ReleaseMetadata, UpdateCheckError>
where
    T: Transport,
    C: Clock,
    S: CacheStore,
{
    let response = client
        .get(endpoint, "", CachePolicy::MaxAge(Duration::ZERO))
        .map_err(UpdateCheckError::from)?;
    if !response.is_success() {
        return Err(UpdateCheckError::HttpStatus(response.status_code()));
    }
    ReleaseMetadata::from_json(response.body()).map_err(UpdateCheckError::from)
}

/// A redaction-safe update check failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum UpdateCheckError {
    /// The request failed before a response was available.
    #[error("update metadata request failed")]
    Request,
    /// The server returned a failing status.
    #[error("update metadata returned HTTP status {0}")]
    HttpStatus(u16),
    /// The response did not match the metadata contract.
    #[error("update metadata response was invalid")]
    InvalidMetadata,
}

impl From<ClientError> for UpdateCheckError {
    fn from(_error: ClientError) -> Self {
        Self::Request
    }
}

impl From<MetadataError> for UpdateCheckError {
    fn from(_error: MetadataError) -> Self {
        Self::InvalidMetadata
    }
}

impl From<BodyDecodeError> for UpdateCheckError {
    fn from(_error: BodyDecodeError) -> Self {
        Self::InvalidMetadata
    }
}

/// An absolute source split into a validated origin and target.
#[derive(Clone, Eq, PartialEq)]
pub struct DownloadSource {
    base_url: BaseUrl,
    target: String,
}

impl DownloadSource {
    /// Parses an absolute HTTP(S) download URL.
    pub fn parse(absolute_url: &str) -> Result<Self, RequestError> {
        let (base_url, target) = BaseUrl::split_absolute(absolute_url)?;
        Ok(Self { base_url, target })
    }

    /// Returns the validated origin.
    #[must_use]
    pub const fn base_url(&self) -> &BaseUrl {
        &self.base_url
    }

    /// Returns the origin-relative target.
    ///
    /// The target may contain sensitive query parameters and should not be
    /// logged. The type's [`Debug`](fmt::Debug) output is redacted.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Reconstructs the absolute source URL for explicit transport use.
    ///
    /// The result may contain sensitive query parameters and should not be
    /// logged. The type's [`Debug`](fmt::Debug) output is redacted.
    #[must_use]
    pub fn absolute_url(&self) -> String {
        format!("{}{}", self.base_url.as_str(), self.target)
    }
}

impl fmt::Debug for DownloadSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DownloadSource([redacted])")
    }
}

/// Returns the two legacy self-update download sources in fallback order.
pub fn legacy_self_update_sources() -> Result<Vec<DownloadSource>, RequestError> {
    [
        "https://github.com/RaidcoreGG/Nexus/releases/latest/download/d3d11.dll",
        "https://api.raidcore.gg/d3d11.dll",
    ]
    .into_iter()
    .map(DownloadSource::parse)
    .collect()
}

/// Builds the GitHub API endpoint used to enumerate an add-on's releases.
pub fn github_releases_endpoint(repository_url: &str) -> Result<String, GithubRepositoryError> {
    Ok(format!(
        "/repos{}/releases",
        github_repository_path(repository_url)?
    ))
}

/// Builds the GitHub API endpoint used to resolve a library add-on's latest release.
pub fn github_latest_release_endpoint(
    repository_url: &str,
) -> Result<String, GithubRepositoryError> {
    Ok(format!(
        "/repos{}/releases/latest",
        github_repository_path(repository_url)?
    ))
}

fn github_repository_path(repository_url: &str) -> Result<String, GithubRepositoryError> {
    let (base_url, target) =
        BaseUrl::split_absolute(repository_url).map_err(|_error| GithubRepositoryError)?;
    if base_url.as_str() != "https://github.com" || target.contains('?') {
        return Err(GithubRepositoryError);
    }

    let trimmed = target.trim_matches('/');
    let mut segments = trimmed.split('/');
    let (Some(owner), Some(repository), None) = (segments.next(), segments.next(), segments.next())
    else {
        return Err(GithubRepositoryError);
    };
    if owner.is_empty() || repository.is_empty() {
        return Err(GithubRepositoryError);
    }
    Ok(format!("/{owner}/{repository}"))
}

/// A GitHub repository URL could not be mapped to the releases API.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("invalid GitHub repository URL")]
pub struct GithubRepositoryError;

/// Builds the legacy `.md5` then `.md5sum` direct-update fallback sources.
#[must_use]
pub fn legacy_direct_checksum_sources(source: &DownloadSource) -> [DownloadSource; 2] {
    let (path, query) = source
        .target
        .split_once('?')
        .map_or((source.target.as_str(), None), |(path, query)| {
            (path, Some(query))
        });
    let build = |suffix: &str| {
        let mut target = format!("{path}{suffix}");
        if let Some(query) = query {
            target.push('?');
            target.push_str(query);
        }
        DownloadSource {
            base_url: source.base_url.clone(),
            target,
        }
    };
    [build(".md5"), build(".md5sum")]
}

/// A selected GitHub add-on release and its first DLL asset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddonRelease {
    /// Parsed release tag version.
    pub version: Version,
    /// Selected DLL asset.
    pub source: DownloadSource,
}

/// Selects the highest eligible GitHub release containing a DLL asset.
pub fn select_github_addon_update(
    bytes: &[u8],
    current: Version,
    allow_prereleases: bool,
) -> Result<Option<AddonRelease>, GithubReleaseError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_error| GithubReleaseError::InvalidJson)?;
    let Some(releases) = value.as_array() else {
        return Err(GithubReleaseError::InvalidShape);
    };

    let mut selected: Option<AddonRelease> = None;
    for release in releases {
        let Some(prerelease) = release.get("prerelease").and_then(Value::as_bool) else {
            continue;
        };
        if prerelease && !allow_prereleases {
            continue;
        }
        let Some(tag_name) = release.get("tag_name").and_then(Value::as_str) else {
            continue;
        };
        let Ok(version) = Version::from_str(tag_name) else {
            continue;
        };
        let threshold = selected.as_ref().map_or(current, |item| item.version);
        if version <= threshold {
            continue;
        }
        let Some(assets) = release.get("assets").and_then(Value::as_array) else {
            continue;
        };
        if let Some(source) = first_dll_source(assets) {
            selected = Some(AddonRelease { version, source });
        }
    }

    Ok(selected)
}

/// Selects the first DLL asset from a GitHub `releases/latest` response.
pub fn select_latest_github_dll(
    bytes: &[u8],
) -> Result<Option<DownloadSource>, GithubReleaseError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_error| GithubReleaseError::InvalidJson)?;
    let Some(assets) = value.get("assets").and_then(Value::as_array) else {
        return Err(GithubReleaseError::InvalidShape);
    };
    Ok(first_dll_source(assets))
}

fn first_dll_source(assets: &[Value]) -> Option<DownloadSource> {
    assets.iter().find_map(|asset| {
        let name = asset.get("name")?.as_str()?;
        if !name.ends_with(".dll") {
            return None;
        }
        let url = asset.get("browser_download_url")?.as_str()?;
        DownloadSource::parse(url).ok()
    })
}

/// A redaction-safe GitHub release response failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum GithubReleaseError {
    /// The response was not JSON.
    #[error("GitHub release response was not valid JSON")]
    InvalidJson,
    /// The response did not have the expected release or asset shape.
    #[error("GitHub release response had an invalid shape")]
    InvalidShape,
}

/// A 16-byte MD5 digest used by legacy direct add-on updates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Md5Digest([u8; 16]);

impl Md5Digest {
    /// Parses the first whitespace-delimited, 32-digit hexadecimal token.
    pub fn parse_checksum_file(bytes: &[u8]) -> Result<Self, DigestParseError> {
        let text = std::str::from_utf8(bytes).map_err(|_error| DigestParseError)?;
        let Some(token) = text.split_ascii_whitespace().next() else {
            return Err(DigestParseError);
        };
        if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DigestParseError);
        }

        let mut digest = [0_u8; 16];
        for (index, output) in digest.iter_mut().enumerate() {
            let offset = index * 2;
            *output = u8::from_str_radix(&token[offset..offset + 2], 16)
                .map_err(|_error| DigestParseError)?;
        }
        Ok(Self(digest))
    }

    /// Returns the digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// An MD5 checksum response was missing or malformed.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("invalid MD5 checksum response")]
pub struct DigestParseError;

/// Determines whether a direct-update checksum differs from the local digest.
pub fn direct_addon_update_available(
    local: Md5Digest,
    checksum_response: &[u8],
) -> Result<bool, DigestParseError> {
    Ok(local != Md5Digest::parse_checksum_file(checksum_response)?)
}

/// Paths involved in a rollback-safe replacement.
#[derive(Clone, Eq, PartialEq)]
pub struct ReplacementPlan {
    current: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
}

impl ReplacementPlan {
    /// Creates a replacement plan with three distinct paths.
    pub fn new(
        current: impl Into<PathBuf>,
        staged: impl Into<PathBuf>,
        backup: impl Into<PathBuf>,
    ) -> Result<Self, PlanError> {
        let plan = Self {
            current: current.into(),
            staged: staged.into(),
            backup: backup.into(),
        };
        if plan.current == plan.staged || plan.current == plan.backup || plan.staged == plan.backup
        {
            return Err(PlanError::OverlappingPaths);
        }
        Ok(plan)
    }

    /// Returns the currently active file path.
    #[must_use]
    pub fn current(&self) -> &Path {
        &self.current
    }

    /// Returns the fully downloaded staging path.
    #[must_use]
    pub fn staged(&self) -> &Path {
        &self.staged
    }

    /// Returns the no-clobber rollback backup path.
    #[must_use]
    pub fn backup(&self) -> &Path {
        &self.backup
    }
}

impl fmt::Debug for ReplacementPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReplacementPlan([paths redacted])")
    }
}

/// How a staged download will be committed.
#[derive(Clone, Eq, PartialEq)]
pub enum CommitPlan {
    /// Move into a previously unused destination.
    Install {
        /// Final destination, which must not already exist.
        destination: PathBuf,
    },
    /// Replace an active file while retaining a rollback backup.
    Replace(ReplacementPlan),
}

impl fmt::Debug for CommitPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Install { .. } => formatter.write_str("CommitPlan::Install([path redacted])"),
            Self::Replace(_) => formatter.write_str("CommitPlan::Replace([paths redacted])"),
        }
    }
}

/// A bounded download plus its eventual commit action.
#[derive(Clone, Eq, PartialEq)]
pub struct DownloadPlan {
    sources: Vec<DownloadSource>,
    staging: PathBuf,
    max_bytes: usize,
    commit: CommitPlan,
}

impl DownloadPlan {
    /// Plans a remote add-on or Nexus replacement.
    pub fn replacement(
        sources: Vec<DownloadSource>,
        replacement: ReplacementPlan,
        max_bytes: usize,
    ) -> Result<Self, PlanError> {
        validate_download_parameters(&sources, max_bytes)?;
        Ok(Self {
            sources,
            staging: replacement.staged.clone(),
            max_bytes,
            commit: CommitPlan::Replace(replacement),
        })
    }

    /// Plans a new add-on installation into an unused destination.
    pub fn installation(
        sources: Vec<DownloadSource>,
        staging: impl Into<PathBuf>,
        destination: impl Into<PathBuf>,
        max_bytes: usize,
    ) -> Result<Self, PlanError> {
        validate_download_parameters(&sources, max_bytes)?;
        let staging = staging.into();
        let destination = destination.into();
        if staging == destination {
            return Err(PlanError::OverlappingPaths);
        }
        Ok(Self {
            sources,
            staging,
            max_bytes,
            commit: CommitPlan::Install { destination },
        })
    }

    /// Returns the ordered download sources.
    #[must_use]
    pub fn sources(&self) -> &[DownloadSource] {
        &self.sources
    }

    /// Returns the atomic staging destination.
    #[must_use]
    pub fn staging(&self) -> &Path {
        &self.staging
    }

    /// Returns the maximum accepted artifact size.
    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Returns the commit action.
    #[must_use]
    pub const fn commit(&self) -> &CommitPlan {
        &self.commit
    }
}

impl fmt::Debug for DownloadPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DownloadPlan")
            .field("source_count", &self.sources.len())
            .field("staging", &"[redacted]")
            .field("max_bytes", &self.max_bytes)
            .field("commit", &self.commit)
            .finish()
    }
}

fn validate_download_parameters(
    sources: &[DownloadSource],
    max_bytes: usize,
) -> Result<(), PlanError> {
    if sources.is_empty() {
        return Err(PlanError::NoSources);
    }
    if max_bytes == 0 {
        return Err(PlanError::InvalidLimit);
    }
    Ok(())
}

/// A download or replacement plan was unsafe or incomplete.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PlanError {
    /// No fallback source was supplied.
    #[error("update plan has no download sources")]
    NoSources,
    /// The maximum artifact size was zero.
    #[error("update plan size limit must be nonzero")]
    InvalidLimit,
    /// Staging, current, backup, or destination paths overlapped.
    #[error("update plan paths must be distinct")]
    OverlappingPaths,
}

/// Result of comparing local and remote Nexus versions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelfUpdateDecision {
    /// Local and remote versions are identical.
    UpToDate,
    /// The remote service reported an older version.
    RemoteOlder,
    /// A newer version is available with an executable download plan.
    Available {
        /// Remote release metadata.
        release: ReleaseMetadata,
        /// Ordered fallback sources and rollback-safe commit plan.
        download: DownloadPlan,
    },
}

/// Compares versions and creates a self-update plan only when needed.
pub fn plan_self_update(
    current: Version,
    release: ReleaseMetadata,
    sources: Vec<DownloadSource>,
    replacement: ReplacementPlan,
    max_bytes: usize,
) -> Result<SelfUpdateDecision, PlanError> {
    if release.version == current {
        return Ok(SelfUpdateDecision::UpToDate);
    }
    if release.version < current {
        return Ok(SelfUpdateDecision::RemoteOlder);
    }
    let download = DownloadPlan::replacement(sources, replacement, max_bytes)?;
    Ok(SelfUpdateDecision::Available { release, download })
}

/// Download-attempt failure exposed to an injected downloader.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("artifact download attempt failed")]
pub struct DownloadAttemptError;

/// Boundary that stages one source atomically without requiring internet in tests.
pub trait PlannedDownloader {
    /// Downloads `source` to `staging`, enforcing `max_bytes` at the transport.
    ///
    /// Failure must leave any prior staging file untouched. Success must report
    /// the exact number of bytes atomically committed to `staging`.
    fn stage(
        &mut self,
        source: &DownloadSource,
        staging: &Path,
        max_bytes: usize,
    ) -> Result<usize, DownloadAttemptError>;
}

/// Stages the first successful source in a plan.
pub fn stage_with_fallback<D: PlannedDownloader>(
    downloader: &mut D,
    plan: &DownloadPlan,
) -> Result<StagedArtifact, StageError> {
    for source in plan.sources() {
        if let Ok(bytes) = downloader.stage(source, plan.staging(), plan.max_bytes())
            && bytes > 0
            && bytes <= plan.max_bytes()
        {
            return Ok(StagedArtifact { bytes });
        }
    }
    Err(StageError::AllSourcesFailed)
}

/// A staged artifact that passed the plan's size constraints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StagedArtifact {
    bytes: usize,
}

impl StagedArtifact {
    /// Returns the staged artifact size.
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.bytes
    }
}

/// Every source in a fallback chain failed or violated its bounds.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum StageError {
    /// No source produced a nonempty artifact within the configured bound.
    #[error("all artifact download sources failed")]
    AllSourcesFailed,
}

/// Applies the commit action for a fully staged download.
pub fn commit_download<F: FileSystem>(
    filesystem: &F,
    plan: &DownloadPlan,
) -> Result<CommitOutcome, CommitError> {
    match plan.commit() {
        CommitPlan::Install { destination } => {
            validate_staged(filesystem, plan.staging()).map_err(CommitError::from)?;
            if filesystem
                .file_len(destination)
                .map_err(|_error| CommitError::Storage)?
                .is_some()
            {
                return Err(CommitError::DestinationOccupied);
            }
            filesystem
                .move_noreplace(plan.staging(), destination)
                .map_err(|_error| CommitError::Storage)?;
            Ok(CommitOutcome::Installed)
        }
        CommitPlan::Replace(replacement) => {
            apply_replacement(filesystem, replacement).map_err(CommitError::from)?;
            Ok(CommitOutcome::Replaced)
        }
    }
}

/// Successful staged-file commit kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitOutcome {
    /// A previously absent destination was installed.
    Installed,
    /// An active file was replaced and its backup was retained.
    Replaced,
}

/// A redaction-safe staged-file commit failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CommitError {
    /// The staged artifact was absent or empty.
    #[error("staged artifact was absent or empty")]
    InvalidStagedArtifact,
    /// A new-install destination already existed.
    #[error("artifact destination was already occupied")]
    DestinationOccupied,
    /// Storage failed while installing a new file.
    #[error("artifact commit failed in storage")]
    Storage,
    /// A replacement could not be completed safely.
    #[error("artifact replacement failed")]
    Replacement,
}

impl From<ReplacementError> for CommitError {
    fn from(error: ReplacementError) -> Self {
        match error {
            ReplacementError::MissingStaged | ReplacementError::EmptyStaged => {
                Self::InvalidStagedArtifact
            }
            _ => Self::Replacement,
        }
    }
}

/// Atomically swaps a staged file into place, restoring the old file on failure.
///
/// Success intentionally retains `plan.backup()` so cleanup can happen only
/// after the new artifact is proven healthy.
pub fn apply_replacement<F: FileSystem>(
    filesystem: &F,
    plan: &ReplacementPlan,
) -> Result<(), ReplacementError> {
    validate_staged(filesystem, plan.staged())?;
    if filesystem
        .file_len(plan.current())
        .map_err(|_error| ReplacementError::Storage)?
        .is_none()
    {
        return Err(ReplacementError::MissingCurrent);
    }
    if filesystem
        .file_len(plan.backup())
        .map_err(|_error| ReplacementError::Storage)?
        .is_some()
    {
        return Err(ReplacementError::BackupOccupied);
    }

    filesystem
        .move_noreplace(plan.current(), plan.backup())
        .map_err(|_error| ReplacementError::MoveCurrent)?;

    if filesystem
        .move_noreplace(plan.staged(), plan.current())
        .is_ok()
    {
        return Ok(());
    }

    if filesystem
        .move_noreplace(plan.backup(), plan.current())
        .is_ok()
    {
        Err(ReplacementError::InstallFailedRolledBack)
    } else {
        Err(ReplacementError::RollbackFailed)
    }
}

fn validate_staged<F: FileSystem>(filesystem: &F, staged: &Path) -> Result<(), ReplacementError> {
    match filesystem
        .file_len(staged)
        .map_err(|_error| ReplacementError::Storage)?
    {
        None => Err(ReplacementError::MissingStaged),
        Some(0) => Err(ReplacementError::EmptyStaged),
        Some(_) => Ok(()),
    }
}

/// A redaction-safe replacement transaction failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ReplacementError {
    /// The staged file did not exist.
    #[error("replacement staging file was missing")]
    MissingStaged,
    /// The staged file contained no bytes.
    #[error("replacement staging file was empty")]
    EmptyStaged,
    /// The active file did not exist.
    #[error("replacement current file was missing")]
    MissingCurrent,
    /// The backup path was already occupied and was not overwritten.
    #[error("replacement backup path was occupied")]
    BackupOccupied,
    /// Storage inspection failed before mutation.
    #[error("replacement storage inspection failed")]
    Storage,
    /// The active file could not be moved to its backup.
    #[error("replacement could not preserve the current file")]
    MoveCurrent,
    /// Installing the stage failed, but the prior file was restored.
    #[error("replacement failed and the current file was restored")]
    InstallFailedRolledBack,
    /// Installing and restoring both failed; manual recovery is required.
    #[error("replacement and rollback both failed")]
    RollbackFailed,
}

impl From<FileSystemError> for ReplacementError {
    fn from(_error: FileSystemError) -> Self {
        Self::Storage
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::str::FromStr;

    use super::{
        CommitError, CommitOutcome, DigestParseError, DownloadAttemptError, DownloadPlan,
        DownloadSource, Md5Digest, PlanError, PlannedDownloader, ReleaseMetadata, ReplacementError,
        ReplacementPlan, SelfUpdateDecision, Version, apply_replacement, commit_download,
        direct_addon_update_available, fetch_release_metadata, github_latest_release_endpoint,
        github_releases_endpoint, legacy_direct_checksum_sources, legacy_self_update_sources,
        plan_self_update, select_github_addon_update, select_latest_github_dll,
        stage_with_fallback,
    };
    use crate::clock::Clock;
    use crate::http::{HttpClient, HttpRequest, Transport, TransportError, TransportResponse};
    use crate::test_support::TestFileSystem;

    fn source(url: &str) -> DownloadSource {
        let parsed = DownloadSource::parse(url);
        let Ok(parsed) = parsed else {
            panic!("test source must be valid");
        };
        parsed
    }

    fn replacement() -> ReplacementPlan {
        let plan = ReplacementPlan::new("current.dll", "staged.dll", "backup.dll");
        let Ok(plan) = plan else {
            panic!("test replacement must be valid");
        };
        plan
    }

    #[test]
    fn versions_parse_order_and_format_without_cpp_constructor_quirks() {
        assert_eq!(Version::from_str("1.2"), Ok(Version::new(1, 2, 0, 0)));
        assert_eq!(Version::from_str("v1.2.3"), Ok(Version::new(1, 2, 3, 0)));
        assert_eq!(Version::from_str("1.2.3.4"), Ok(Version::new(1, 2, 3, 4)));
        assert!(Version::from_str("V1.2.3").is_err());
        assert!(Version::from_str("1.2.3.4.5").is_err());
        assert!(Version::from_str("1.2.-3").is_err());
        assert!(Version::new(2, 0, 0, 0) > Version::new(1, u16::MAX, u16::MAX, u16::MAX));
        assert_eq!(Version::new(1, 2, 3, 0).to_string(), "1.2.3");
        assert_eq!(Version::new(1, 2, 3, 4).to_string(), "1.2.3.4");
    }

    #[test]
    fn metadata_parser_preserves_the_legacy_uppercase_contract() {
        let metadata = ReleaseMetadata::from_json(
            br#"{"Major":1,"Minor":2,"Build":3,"Revision":4,"Changelog":"notes"}"#,
        );
        let Ok(metadata) = metadata else {
            panic!("expected valid metadata");
        };
        assert_eq!(metadata.version, Version::new(1, 2, 3, 4));
        assert_eq!(metadata.changelog.as_deref(), Some("notes"));

        assert!(ReleaseMetadata::from_json(br#"{"Major":1}"#).is_err());
    }

    #[derive(Clone, Copy)]
    struct FixedClock;

    impl Clock for FixedClock {
        fn unix_timestamp(&self) -> i64 {
            123
        }
    }

    struct MetadataTransport {
        response: Option<TransportResponse>,
        requested_target: Option<String>,
    }

    impl Transport for MetadataTransport {
        fn get(
            &mut self,
            request: &HttpRequest,
            _max_body_bytes: usize,
        ) -> Result<TransportResponse, TransportError> {
            self.requested_target = Some(request.target().to_owned());
            self.response.take().ok_or(TransportError::RequestFailed)
        }
    }

    #[test]
    fn metadata_fetch_uses_the_legacy_endpoint_without_internet() {
        let transport = MetadataTransport {
            response: Some(TransportResponse::new(
                200,
                br#"{"Major":2,"Minor":3,"Build":4,"Revision":5}"#.to_vec(),
                None,
            )),
            requested_target: None,
        };
        let client = HttpClient::new("https://api.raidcore.gg", transport, FixedClock);
        let Ok(mut client) = client else {
            panic!("test client must be valid");
        };

        let metadata = fetch_release_metadata(&mut client, super::NEXUS_VERSION_ENDPOINT);
        let Ok(metadata) = metadata else {
            panic!("expected valid update metadata");
        };
        assert_eq!(metadata.version, Version::new(2, 3, 4, 5));
        assert_eq!(
            client.transport_mut().requested_target.as_deref(),
            Some("/nexusversion")
        );
    }

    #[test]
    fn github_selection_uses_highest_eligible_release_with_a_dll() {
        let releases = br#"
        [
          {"prerelease":true,"tag_name":"v3.0.0","assets":[
            {"name":"pre.dll","browser_download_url":"https://files.test/pre.dll"}
          ]},
          {"prerelease":false,"tag_name":"v4.0.0","assets":[
            {"name":"readme.txt","browser_download_url":"https://files.test/readme.txt"}
          ]},
          {"prerelease":false,"tag_name":"v2.1.0","assets":[
            {"name":"addon.dll","browser_download_url":"https://files.test/addon.dll"}
          ]},
          {"prerelease":false,"tag_name":"not-a-version","assets":[]}
        ]
        "#;

        let stable = select_github_addon_update(releases, Version::new(1, 0, 0, 0), false);
        let Ok(Some(stable)) = stable else {
            panic!("expected a stable update");
        };
        assert_eq!(stable.version, Version::new(2, 1, 0, 0));
        assert_eq!(stable.source.target(), "/addon.dll");

        let prerelease = select_github_addon_update(releases, Version::new(1, 0, 0, 0), true);
        let Ok(Some(prerelease)) = prerelease else {
            panic!("expected a prerelease update");
        };
        assert_eq!(prerelease.version, Version::new(3, 0, 0, 0));
    }

    #[test]
    fn latest_github_asset_uses_the_first_dll_like_the_legacy_library() {
        let response = br#"
        {"assets":[
          {"name":"notes.txt","browser_download_url":"https://files.test/notes.txt"},
          {"name":"first.dll","browser_download_url":"https://files.test/first.dll"},
          {"name":"second.dll","browser_download_url":"https://files.test/second.dll"}
        ]}
        "#;
        let selected = select_latest_github_dll(response);
        let Ok(Some(selected)) = selected else {
            panic!("expected a DLL asset");
        };
        assert_eq!(selected.target(), "/first.dll");
        assert!(format!("{selected:?}").contains("[redacted]"));
        assert!(!format!("{selected:?}").contains("first.dll"));
    }

    #[test]
    fn direct_checksum_parser_accepts_md5_and_md5sum_shapes() {
        let expected = Md5Digest::parse_checksum_file(b"00112233445566778899aabbccddeeff");
        let Ok(expected) = expected else {
            panic!("expected a valid digest");
        };
        assert_eq!(expected.as_bytes()[0], 0x00);
        assert_eq!(expected.as_bytes()[15], 0xff);
        assert_eq!(
            direct_addon_update_available(expected, b"00112233445566778899aabbccddeeff  addon.dll"),
            Ok(false)
        );
        assert_eq!(
            direct_addon_update_available(expected, b"10112233445566778899aabbccddeeff  addon.dll"),
            Ok(true)
        );
        assert_eq!(
            Md5Digest::parse_checksum_file(b"not-a-checksum"),
            Err(DigestParseError)
        );
    }

    #[test]
    fn self_update_sources_keep_github_then_raidcore_fallback_order() {
        let sources = legacy_self_update_sources();
        let Ok(sources) = sources else {
            panic!("legacy constants must be valid URLs");
        };
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].base_url().as_str(), "https://github.com");
        assert_eq!(sources[1].base_url().as_str(), "https://api.raidcore.gg");
    }

    #[test]
    fn addon_update_helpers_preserve_github_and_direct_fallback_routes() {
        assert_eq!(
            github_releases_endpoint("https://github.com/owner/repository/"),
            Ok("/repos/owner/repository/releases".to_owned())
        );
        assert_eq!(
            github_latest_release_endpoint("https://github.com/owner/repository"),
            Ok("/repos/owner/repository/releases/latest".to_owned())
        );
        assert!(github_releases_endpoint("https://example.test/owner/repository").is_err());
        assert!(github_releases_endpoint("https://github.com/owner/repository/extra").is_err());

        let checksums =
            legacy_direct_checksum_sources(&source("https://files.test/addon.dll?ticket=opaque"));
        assert_eq!(checksums[0].target(), "/addon.dll.md5?ticket=opaque");
        assert_eq!(checksums[1].target(), "/addon.dll.md5sum?ticket=opaque");
    }

    #[test]
    fn self_update_planning_distinguishes_equal_older_and_newer_versions() {
        let current = Version::new(2, 0, 0, 0);
        let equal = ReleaseMetadata {
            version: current,
            changelog: None,
        };
        assert_eq!(
            plan_self_update(
                current,
                equal,
                vec![source("https://files.test/nexus.dll")],
                replacement(),
                1024
            ),
            Ok(SelfUpdateDecision::UpToDate)
        );

        let older = ReleaseMetadata {
            version: Version::new(1, 9, 0, 0),
            changelog: None,
        };
        assert_eq!(
            plan_self_update(
                current,
                older,
                vec![source("https://files.test/nexus.dll")],
                replacement(),
                1024
            ),
            Ok(SelfUpdateDecision::RemoteOlder)
        );

        let newer = ReleaseMetadata {
            version: Version::new(2, 1, 0, 0),
            changelog: Some("notes".to_owned()),
        };
        assert!(matches!(
            plan_self_update(
                current,
                newer,
                vec![source("https://files.test/nexus.dll")],
                replacement(),
                1024
            ),
            Ok(SelfUpdateDecision::Available { .. })
        ));
    }

    #[test]
    fn unsafe_or_incomplete_download_plans_are_rejected() {
        assert_eq!(
            ReplacementPlan::new("same", "same", "backup"),
            Err(PlanError::OverlappingPaths)
        );
        assert_eq!(
            DownloadPlan::replacement(Vec::new(), replacement(), 1),
            Err(PlanError::NoSources)
        );
        assert_eq!(
            DownloadPlan::installation(
                vec![source("https://files.test/addon.dll")],
                "stage",
                "target",
                0
            ),
            Err(PlanError::InvalidLimit)
        );
    }

    struct ScriptedDownloader {
        results: VecDeque<Result<usize, DownloadAttemptError>>,
        attempts: usize,
    }

    impl PlannedDownloader for ScriptedDownloader {
        fn stage(
            &mut self,
            _source: &DownloadSource,
            _staging: &Path,
            _max_bytes: usize,
        ) -> Result<usize, DownloadAttemptError> {
            self.attempts += 1;
            self.results.pop_front().ok_or(DownloadAttemptError)?
        }
    }

    #[test]
    fn fallback_staging_tries_sources_in_order_and_checks_reported_size() {
        let plan = DownloadPlan::replacement(
            vec![
                source("https://first.test/nexus.dll"),
                source("https://second.test/nexus.dll"),
                source("https://third.test/nexus.dll"),
            ],
            replacement(),
            10,
        );
        let Ok(plan) = plan else {
            panic!("expected a valid plan");
        };
        let mut downloader = ScriptedDownloader {
            results: [Err(DownloadAttemptError), Ok(11), Ok(8)]
                .into_iter()
                .collect(),
            attempts: 0,
        };

        let staged = stage_with_fallback(&mut downloader, &plan);
        let Ok(staged) = staged else {
            panic!("expected the third source to succeed");
        };
        assert_eq!(staged.bytes(), 8);
        assert_eq!(downloader.attempts, 3);
    }

    #[test]
    fn successful_replacement_retains_the_old_file_as_backup() {
        let filesystem = TestFileSystem::default();
        filesystem.put("current.dll", b"old".to_vec());
        filesystem.put("staged.dll", b"new".to_vec());

        assert_eq!(apply_replacement(&filesystem, &replacement()), Ok(()));
        assert_eq!(filesystem.get("current.dll"), Some(b"new".to_vec()));
        assert_eq!(filesystem.get("backup.dll"), Some(b"old".to_vec()));
        assert!(!filesystem.contains("staged.dll"));
    }

    #[test]
    fn occupied_backup_prevents_any_replacement_mutation() {
        let filesystem = TestFileSystem::default();
        filesystem.put("current.dll", b"old".to_vec());
        filesystem.put("staged.dll", b"new".to_vec());
        filesystem.put("backup.dll", b"keep".to_vec());

        assert_eq!(
            apply_replacement(&filesystem, &replacement()),
            Err(ReplacementError::BackupOccupied)
        );
        assert_eq!(filesystem.get("current.dll"), Some(b"old".to_vec()));
        assert_eq!(filesystem.get("staged.dll"), Some(b"new".to_vec()));
        assert_eq!(filesystem.get("backup.dll"), Some(b"keep".to_vec()));
    }

    #[test]
    fn failed_install_rolls_the_current_file_back() {
        let filesystem = TestFileSystem::default();
        filesystem.put("current.dll", b"old".to_vec());
        filesystem.put("staged.dll", b"new".to_vec());
        filesystem.fail_move_calls([2]);

        assert_eq!(
            apply_replacement(&filesystem, &replacement()),
            Err(ReplacementError::InstallFailedRolledBack)
        );
        assert_eq!(filesystem.get("current.dll"), Some(b"old".to_vec()));
        assert_eq!(filesystem.get("staged.dll"), Some(b"new".to_vec()));
        assert!(!filesystem.contains("backup.dll"));
    }

    #[test]
    fn rollback_failure_is_explicit_and_leaves_the_backup_recoverable() {
        let filesystem = TestFileSystem::default();
        filesystem.put("current.dll", b"old".to_vec());
        filesystem.put("staged.dll", b"new".to_vec());
        filesystem.fail_move_calls([2, 3]);

        assert_eq!(
            apply_replacement(&filesystem, &replacement()),
            Err(ReplacementError::RollbackFailed)
        );
        assert!(!filesystem.contains("current.dll"));
        assert_eq!(filesystem.get("staged.dll"), Some(b"new".to_vec()));
        assert_eq!(filesystem.get("backup.dll"), Some(b"old".to_vec()));
    }

    #[test]
    fn new_addon_commit_is_no_clobber() {
        let plan = DownloadPlan::installation(
            vec![source("https://files.test/addon.dll")],
            "stage.dll",
            "addon.dll",
            1024,
        );
        let Ok(plan) = plan else {
            panic!("expected a valid plan");
        };
        let filesystem = TestFileSystem::default();
        filesystem.put("stage.dll", b"addon".to_vec());
        assert_eq!(
            commit_download(&filesystem, &plan),
            Ok(CommitOutcome::Installed)
        );
        assert_eq!(filesystem.get("addon.dll"), Some(b"addon".to_vec()));

        filesystem.put("stage.dll", b"replacement".to_vec());
        assert_eq!(
            commit_download(&filesystem, &plan),
            Err(CommitError::DestinationOccupied)
        );
        assert_eq!(filesystem.get("addon.dll"), Some(b"addon".to_vec()));
        assert_eq!(filesystem.get("stage.dll"), Some(b"replacement".to_vec()));
    }
}
