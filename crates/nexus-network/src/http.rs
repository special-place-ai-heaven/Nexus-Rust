//! Bounded HTTP GET and download orchestration over an injected transport.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::cache::{CacheError, CachePolicy, CacheStore, HttpCache, NullCacheStore};
use crate::clock::Clock;
use crate::filesystem::{FileSystem, FileSystemError};

const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Legacy default cache lifetime used for ordinary hosts.
pub const DEFAULT_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 60);

/// Legacy cache lifetime used for `api.raidcore.gg`.
pub const RAIDCORE_API_CACHE_MAX_AGE: Duration = Duration::from_secs(5 * 60);

/// Legacy cache lifetime used for `api.github.com`.
pub const GITHUB_API_CACHE_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// A validated HTTP or HTTPS origin.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct BaseUrl(String);

impl BaseUrl {
    /// Parses an absolute URL and retains only its scheme and authority.
    pub fn parse(value: &str) -> Result<Self, RequestError> {
        let Some((scheme, remainder)) = value.split_once("://") else {
            return Err(RequestError::InvalidBaseUrl);
        };
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(RequestError::InvalidBaseUrl);
        }

        let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
        let authority = &remainder[..authority_end];
        if authority.is_empty()
            || authority.contains('@')
            || authority.chars().any(char::is_whitespace)
            || authority.chars().any(char::is_control)
        {
            return Err(RequestError::InvalidBaseUrl);
        }

        Ok(Self(format!("{scheme}://{authority}")))
    }

    /// Returns the validated origin.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Splits an absolute URL into an origin and request target.
    pub fn split_absolute(value: &str) -> Result<(Self, String), RequestError> {
        if value.contains('#')
            || value
                .chars()
                .any(|character| ['\r', '\n'].contains(&character))
        {
            return Err(RequestError::InvalidEndpoint);
        }

        let base = Self::parse(value)?;
        let remainder = &value[base.0.len()..];
        let target = if remainder.is_empty() {
            "/".to_owned()
        } else if remainder.starts_with('?') {
            format!("/{remainder}")
        } else {
            remainder.to_owned()
        };
        validate_target(&target)?;
        Ok((base, target))
    }
}

impl fmt::Debug for BaseUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BaseUrl([redacted])")
    }
}

/// Returns the host-specific cache lifetime configured by the C++ network context.
#[must_use]
pub fn legacy_cache_max_age(base_url: &BaseUrl) -> Duration {
    match base_url.as_str() {
        "https://api.raidcore.gg" => RAIDCORE_API_CACHE_MAX_AGE,
        "https://api.github.com" => GITHUB_API_CACHE_MAX_AGE,
        _ => DEFAULT_CACHE_MAX_AGE,
    }
}

/// A validated GET request passed to a transport implementation.
pub struct HttpRequest {
    base_url: BaseUrl,
    target: String,
    absolute_url: String,
}

impl HttpRequest {
    fn new(base_url: BaseUrl, target: String) -> Result<Self, RequestError> {
        validate_target(&target)?;
        let absolute_url = format!("{}{target}", base_url.as_str());
        Ok(Self {
            base_url,
            target,
            absolute_url,
        })
    }

    /// Returns the validated request origin.
    #[must_use]
    pub const fn base_url(&self) -> &BaseUrl {
        &self.base_url
    }

    /// Returns the origin-relative request target, including its query string.
    ///
    /// The target can contain credentials supplied as query parameters. Callers
    /// should not log it; [`Debug`](fmt::Debug) intentionally redacts it.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Returns the full URL for the transport to execute.
    ///
    /// The URL can contain credentials supplied as query parameters. Callers
    /// should not log it; [`Debug`](fmt::Debug) intentionally redacts it.
    #[must_use]
    pub fn absolute_url(&self) -> &str {
        &self.absolute_url
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpRequest")
            .field("method", &"GET")
            .field("target", &"[redacted]")
            .finish()
    }
}

/// A redaction-safe transport failure category.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TransportError {
    /// Name resolution, connection, TLS, or transfer failed.
    #[error("HTTP transport failed")]
    RequestFailed,
    /// The body exceeded the limit supplied to the transport.
    #[error("HTTP response exceeded the configured body limit")]
    BodyTooLarge,
    /// The transport produced response metadata that could not be trusted.
    #[error("HTTP transport returned an invalid response")]
    InvalidResponse,
}

/// Raw response returned by an injected transport.
pub struct TransportResponse {
    status_code: u16,
    content_length: Option<u64>,
    body: Vec<u8>,
}

impl TransportResponse {
    /// Creates a transport response.
    #[must_use]
    pub fn new(status_code: u16, body: Vec<u8>, content_length: Option<u64>) -> Self {
        Self {
            status_code,
            content_length,
            body,
        }
    }

    /// Returns the HTTP status code.
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        self.status_code
    }

    /// Returns the declared response size when one was supplied.
    #[must_use]
    pub const fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    /// Returns the raw body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Debug for TransportResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransportResponse")
            .field("status_code", &self.status_code)
            .field("content_length", &self.content_length)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Synchronous GET transport boundary.
///
/// Implementations must stop reading and return [`TransportError::BodyTooLarge`]
/// before retaining more than `max_body_bytes`. This makes the bound effective
/// at the network edge rather than only after allocation.
pub trait Transport {
    /// Executes one GET request with an explicit response-body bound.
    fn get(
        &mut self,
        request: &HttpRequest,
        max_body_bytes: usize,
    ) -> Result<TransportResponse, TransportError>;
}

/// Request construction failure without URL or query disclosure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RequestError {
    /// The origin was not a supported, credential-free HTTP(S) URL.
    #[error("invalid HTTP base URL")]
    InvalidBaseUrl,
    /// The endpoint was absolute, malformed, or contained unsafe delimiters.
    #[error("invalid HTTP endpoint")]
    InvalidEndpoint,
    /// Query parameters contained unsafe line or fragment delimiters.
    #[error("invalid HTTP query parameters")]
    InvalidParameters,
}

/// A bounded GET failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ClientError {
    /// Request construction failed.
    #[error("HTTP request could not be constructed")]
    InvalidRequest,
    /// The injected transport failed.
    #[error("HTTP request failed in transport")]
    Transport,
    /// The response exceeded the configured limit.
    #[error("HTTP response exceeded the configured body limit")]
    BodyTooLarge,
    /// The response's declared size did not match its body.
    #[error("HTTP response length did not match its declaration")]
    ContentLengthMismatch,
    /// The response status code was outside the supported HTTP range.
    #[error("HTTP response had an invalid status code")]
    InvalidStatus,
}

/// A decoded-body failure that never embeds response content.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BodyDecodeError {
    /// The body was not valid UTF-8.
    #[error("HTTP response body was not valid UTF-8")]
    InvalidUtf8,
    /// The body was not valid JSON for the requested type.
    #[error("HTTP response body was not valid JSON")]
    InvalidJson,
}

/// A completed HTTP response.
pub struct HttpResponse {
    time: i64,
    status_code: u16,
    body: Vec<u8>,
    from_cache: bool,
}

impl HttpResponse {
    pub(crate) fn from_parts(time: i64, status_code: u16, body: Vec<u8>, from_cache: bool) -> Self {
        Self {
            time,
            status_code,
            body,
            from_cache,
        }
    }

    /// Returns the timestamp captured before the request began.
    #[must_use]
    pub const fn time(&self) -> i64 {
        self.time
    }

    /// Returns the HTTP status code.
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        self.status_code
    }

    /// Returns the response body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Returns whether this response came from cache.
    #[must_use]
    pub const fn is_cached(&self) -> bool {
        self.from_cache
    }

    /// Reproduces the legacy success rule: all statuses below 400 succeed.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.status_code < 400
    }

    /// Returns the legacy-style numeric status and reason phrase.
    #[must_use]
    pub fn status_line(&self) -> String {
        format!("{} {}", self.status_code, status_message(self.status_code))
    }

    /// Decodes the body as UTF-8 without including body content in errors.
    pub fn text(&self) -> Result<&str, BodyDecodeError> {
        std::str::from_utf8(&self.body).map_err(|_error| BodyDecodeError::InvalidUtf8)
    }

    /// Deserializes JSON without including body content in errors.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, BodyDecodeError> {
        serde_json::from_slice(&self.body).map_err(|_error| BodyDecodeError::InvalidJson)
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("time", &self.time)
            .field("status_code", &self.status_code)
            .field("body_bytes", &self.body.len())
            .field("from_cache", &self.from_cache)
            .finish()
    }
}

/// Limits applied to ordinary GET responses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpClientConfig {
    /// Maximum retained response body size.
    pub max_response_bytes: usize,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

/// A base-URL HTTP client with injected transport, time, and optional cache.
pub struct HttpClient<T, C, S = NullCacheStore> {
    base_url: BaseUrl,
    transport: T,
    clock: C,
    cache: Option<HttpCache<S>>,
    config: HttpClientConfig,
    last_cache_error: Option<CacheError>,
}

impl<T, C> HttpClient<T, C, NullCacheStore> {
    /// Creates a client with caching disabled.
    pub fn new(base_url: &str, transport: T, clock: C) -> Result<Self, RequestError> {
        Ok(Self {
            base_url: BaseUrl::parse(base_url)?,
            transport,
            clock,
            cache: None,
            config: HttpClientConfig::default(),
            last_cache_error: None,
        })
    }
}

impl<T, C, S> HttpClient<T, C, S> {
    /// Creates a client with a supplied cache.
    pub fn new_with_cache(
        base_url: &str,
        transport: T,
        clock: C,
        cache: HttpCache<S>,
    ) -> Result<Self, RequestError> {
        Ok(Self {
            base_url: BaseUrl::parse(base_url)?,
            transport,
            clock,
            cache: Some(cache),
            config: HttpClientConfig::default(),
            last_cache_error: None,
        })
    }

    /// Applies ordinary-response limits. A zero limit is promoted to one byte.
    #[must_use]
    pub fn with_config(mut self, mut config: HttpClientConfig) -> Self {
        config.max_response_bytes = config.max_response_bytes.max(1);
        self.config = config;
        self
    }

    /// Returns the validated base URL.
    #[must_use]
    pub const fn base_url(&self) -> &BaseUrl {
        &self.base_url
    }

    /// Returns the transport mutably, primarily for explicit orchestration.
    #[must_use]
    pub const fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Returns the most recent best-effort cache failure, if any.
    #[must_use]
    pub const fn last_cache_error(&self) -> Option<CacheError> {
        self.last_cache_error
    }

    /// Consumes the client and returns the transport.
    #[must_use]
    pub fn into_transport(self) -> T {
        self.transport
    }
}

impl<T: Transport, C: Clock, S: CacheStore> HttpClient<T, C, S> {
    /// Executes a bounded GET with legacy-compatible caching semantics.
    pub fn get(
        &mut self,
        endpoint: &str,
        parameters: &str,
        cache_policy: CachePolicy,
    ) -> Result<HttpResponse, ClientError> {
        let target =
            build_target(endpoint, parameters).map_err(|_error| ClientError::InvalidRequest)?;
        let request = HttpRequest::new(self.base_url.clone(), target.clone())
            .map_err(|_error| ClientError::InvalidRequest)?;
        let request_time = self.clock.unix_timestamp();
        self.last_cache_error = None;

        if let Some(cache) = &mut self.cache {
            match cache.lookup(&target, request_time, cache_policy) {
                Ok(Some(response)) => return Ok(response),
                Ok(None) => {}
                Err(error) => self.last_cache_error = Some(error),
            }
        }

        let raw = self
            .transport
            .get(&request, self.config.max_response_bytes)
            .map_err(map_transport_error)?;
        validate_response(&raw, self.config.max_response_bytes)?;
        let response = HttpResponse::from_parts(request_time, raw.status_code, raw.body, false);

        if let Some(cache) = &mut self.cache
            && let Err(error) = cache.store_response(&target, &response)
        {
            self.last_cache_error = Some(error);
        }

        Ok(response)
    }

    /// Downloads a bounded body and atomically commits it to `destination`.
    ///
    /// Status must be exactly 200, the body must be nonempty, and any declared
    /// content length must match before the destination is touched.
    pub fn download<F: FileSystem>(
        &mut self,
        filesystem: &F,
        destination: &Path,
        endpoint: &str,
        parameters: &str,
        max_bytes: usize,
    ) -> Result<DownloadReceipt, DownloadError> {
        if max_bytes == 0 {
            return Err(DownloadError::InvalidLimit);
        }

        let target =
            build_target(endpoint, parameters).map_err(|_error| DownloadError::InvalidRequest)?;
        let request = HttpRequest::new(self.base_url.clone(), target)
            .map_err(|_error| DownloadError::InvalidRequest)?;
        let raw = self
            .transport
            .get(&request, max_bytes)
            .map_err(map_download_transport_error)?;

        if raw.body.len() > max_bytes {
            return Err(DownloadError::BodyTooLarge);
        }
        if raw.status_code != 200 {
            return Err(DownloadError::HttpStatus(raw.status_code));
        }
        if raw.body.is_empty() {
            return Err(DownloadError::EmptyBody);
        }
        if let Some(declared) = raw.content_length
            && declared != usize_to_u64(raw.body.len())
        {
            return Err(DownloadError::ContentLengthMismatch);
        }

        let bytes = raw.body.len();
        filesystem
            .write_atomic(destination, &raw.body)
            .map_err(|_error| DownloadError::Storage)?;
        Ok(DownloadReceipt { bytes })
    }
}

/// Metadata for a successfully committed download.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DownloadReceipt {
    bytes: usize,
}

impl DownloadReceipt {
    /// Returns the number of committed bytes.
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.bytes
    }
}

/// A download failure that never embeds its URL or destination path.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DownloadError {
    /// The endpoint or query parameters were invalid.
    #[error("download request could not be constructed")]
    InvalidRequest,
    /// The configured maximum size was zero.
    #[error("download size limit must be nonzero")]
    InvalidLimit,
    /// The transport failed.
    #[error("download transport failed")]
    Transport,
    /// The body exceeded the configured bound.
    #[error("download exceeded the configured size limit")]
    BodyTooLarge,
    /// The server returned a status other than 200.
    #[error("download returned HTTP status {0}")]
    HttpStatus(u16),
    /// The server returned no bytes.
    #[error("download returned an empty body")]
    EmptyBody,
    /// The server's declared length did not match the received bytes.
    #[error("download length did not match its declaration")]
    ContentLengthMismatch,
    /// The destination could not be atomically written.
    #[error("download could not be committed to storage")]
    Storage,
}

impl From<FileSystemError> for DownloadError {
    fn from(_error: FileSystemError) -> Self {
        Self::Storage
    }
}

fn build_target(endpoint: &str, parameters: &str) -> Result<String, RequestError> {
    validate_target(endpoint)?;
    if parameters
        .chars()
        .any(|character| ['\r', '\n', '#'].contains(&character))
    {
        return Err(RequestError::InvalidParameters);
    }
    if parameters.is_empty() {
        return Ok(endpoint.to_owned());
    }

    let delimiter = if endpoint.contains('?') { '&' } else { '?' };
    Ok(format!("{endpoint}{delimiter}{parameters}"))
}

fn validate_target(target: &str) -> Result<(), RequestError> {
    if !target.starts_with('/')
        || target.contains("://")
        || target.contains('#')
        || target
            .chars()
            .any(|character| ['\r', '\n'].contains(&character))
    {
        return Err(RequestError::InvalidEndpoint);
    }
    Ok(())
}

fn validate_response(raw: &TransportResponse, max_bytes: usize) -> Result<(), ClientError> {
    if !(100..=599).contains(&raw.status_code) {
        return Err(ClientError::InvalidStatus);
    }
    if raw.body.len() > max_bytes {
        return Err(ClientError::BodyTooLarge);
    }
    if let Some(declared) = raw.content_length
        && declared != usize_to_u64(raw.body.len())
    {
        return Err(ClientError::ContentLengthMismatch);
    }
    Ok(())
}

fn map_transport_error(error: TransportError) -> ClientError {
    match error {
        TransportError::BodyTooLarge => ClientError::BodyTooLarge,
        TransportError::RequestFailed | TransportError::InvalidResponse => ClientError::Transport,
    }
}

fn map_download_transport_error(error: TransportError) -> DownloadError {
    match error {
        TransportError::BodyTooLarge => DownloadError::BodyTooLarge,
        TransportError::RequestFailed | TransportError::InvalidResponse => DownloadError::Transport,
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Returns the legacy reason phrase for a known HTTP status code.
#[must_use]
pub const fn status_message(status_code: u16) -> &'static str {
    match status_code {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        306 => "Switch Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "Unknown status code",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::time::Duration;

    use super::{
        BaseUrl, ClientError, DownloadError, HttpClient, HttpClientConfig, HttpRequest,
        RequestError, Transport, TransportError, TransportResponse, legacy_cache_max_age,
        status_message,
    };
    use crate::cache::{CachePolicy, CacheStore, CacheStoreError, HttpCache, MemoryCacheStore};
    use crate::clock::Clock;
    use crate::test_support::TestFileSystem;

    #[derive(Clone, Copy)]
    struct FixedClock(i64);

    impl Clock for FixedClock {
        fn unix_timestamp(&self) -> i64 {
            self.0
        }
    }

    #[derive(Default)]
    struct ScriptedTransport {
        responses: VecDeque<Result<TransportResponse, TransportError>>,
        requested_urls: Vec<String>,
        limits: Vec<usize>,
    }

    impl ScriptedTransport {
        fn with_responses(
            responses: impl IntoIterator<Item = Result<TransportResponse, TransportError>>,
        ) -> Self {
            Self {
                responses: responses.into_iter().collect(),
                requested_urls: Vec::new(),
                limits: Vec::new(),
            }
        }
    }

    impl Transport for ScriptedTransport {
        fn get(
            &mut self,
            request: &HttpRequest,
            max_body_bytes: usize,
        ) -> Result<TransportResponse, TransportError> {
            self.requested_urls.push(request.absolute_url().to_owned());
            self.limits.push(max_body_bytes);
            self.responses
                .pop_front()
                .ok_or(TransportError::RequestFailed)?
        }
    }

    #[test]
    fn base_urls_are_sanitized_and_request_debug_is_redacted() {
        let parsed = BaseUrl::parse("HTTPS://example.test/path");
        let Ok(parsed) = parsed else {
            panic!("expected a valid base URL");
        };
        assert_eq!(parsed.as_str(), "https://example.test");

        let split = BaseUrl::split_absolute("https://example.test/file.dll?ticket=opaque");
        let Ok((base, target)) = split else {
            panic!("expected a valid absolute URL");
        };
        assert_eq!(base.as_str(), "https://example.test");
        assert_eq!(target, "/file.dll?ticket=opaque");

        let request = HttpRequest::new(base, target);
        let Ok(request) = request else {
            panic!("expected a valid request");
        };
        let debug = format!("{request:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("ticket"));
        assert_eq!(
            BaseUrl::parse("https://identity@example.test"),
            Err(RequestError::InvalidBaseUrl)
        );
    }

    #[test]
    fn legacy_host_cache_lifetimes_are_preserved() {
        let raidcore = BaseUrl::parse("https://api.raidcore.gg");
        let github = BaseUrl::parse("https://api.github.com");
        let ordinary = BaseUrl::parse("https://example.test");
        let (Ok(raidcore), Ok(github), Ok(ordinary)) = (raidcore, github, ordinary) else {
            panic!("test origins must be valid");
        };

        assert_eq!(legacy_cache_max_age(&raidcore), Duration::from_secs(300));
        assert_eq!(legacy_cache_max_age(&github), Duration::from_secs(3_600));
        assert_eq!(legacy_cache_max_age(&ordinary), Duration::from_secs(1_800));
    }

    #[test]
    fn get_builds_the_legacy_query_and_reuses_a_fresh_cache_entry() {
        let transport = ScriptedTransport::with_responses([Ok(TransportResponse::new(
            200,
            b"body".to_vec(),
            Some(4),
        ))]);
        let cache = HttpCache::new(MemoryCacheStore::default(), Duration::from_secs(30));
        let client =
            HttpClient::new_with_cache("https://example.test", transport, FixedClock(100), cache);
        let Ok(mut client) = client else {
            panic!("expected a valid client");
        };

        let first = client.get("/endpoint", "a=1", CachePolicy::Default);
        let Ok(first) = first else {
            panic!("expected a network response");
        };
        assert!(!first.is_cached());

        let second = client.get("/endpoint", "a=1", CachePolicy::Default);
        let Ok(second) = second else {
            panic!("expected a cache response");
        };
        assert!(second.is_cached());
        assert_eq!(client.transport_mut().requested_urls.len(), 1);
        assert_eq!(
            client.transport_mut().requested_urls[0],
            "https://example.test/endpoint?a=1"
        );
    }

    #[test]
    fn zero_age_refreshes_and_replaces_a_cached_response() {
        let transport = ScriptedTransport::with_responses([
            Ok(TransportResponse::new(200, b"one".to_vec(), Some(3))),
            Ok(TransportResponse::new(200, b"two".to_vec(), Some(3))),
        ]);
        let cache = HttpCache::new(MemoryCacheStore::default(), Duration::from_secs(30));
        let client =
            HttpClient::new_with_cache("https://example.test", transport, FixedClock(100), cache);
        let Ok(mut client) = client else {
            panic!("expected a valid client");
        };
        assert!(client.get("/endpoint", "", CachePolicy::Default).is_ok());

        let refreshed = client.get("/endpoint", "", CachePolicy::MaxAge(Duration::ZERO));
        let Ok(refreshed) = refreshed else {
            panic!("expected a refreshed response");
        };
        assert_eq!(refreshed.body(), b"two");
        assert_eq!(client.transport_mut().requested_urls.len(), 2);
    }

    #[derive(Default)]
    struct FailingCacheStore;

    impl CacheStore for FailingCacheStore {
        fn load(&mut self, _key: &str, _limit: usize) -> Result<Option<Vec<u8>>, CacheStoreError> {
            Err(CacheStoreError::OperationFailed)
        }

        fn store_atomic(&mut self, _key: &str, _value: &[u8]) -> Result<(), CacheStoreError> {
            Err(CacheStoreError::OperationFailed)
        }

        fn remove(&mut self, _key: &str) -> Result<(), CacheStoreError> {
            Err(CacheStoreError::OperationFailed)
        }

        fn clear(&mut self) -> Result<(), CacheStoreError> {
            Err(CacheStoreError::OperationFailed)
        }
    }

    #[test]
    fn cache_storage_failure_does_not_take_down_the_network_request() {
        let transport = ScriptedTransport::with_responses([Ok(TransportResponse::new(
            200,
            b"body".to_vec(),
            None,
        ))]);
        let cache = HttpCache::new(FailingCacheStore, Duration::from_secs(30));
        let client =
            HttpClient::new_with_cache("https://example.test", transport, FixedClock(100), cache);
        let Ok(mut client) = client else {
            panic!("expected a valid client");
        };

        let response = client.get("/endpoint", "", CachePolicy::Default);
        assert!(response.is_ok());
        assert_eq!(client.last_cache_error(), Some(crate::CacheError::Storage));
    }

    #[test]
    fn client_rechecks_transport_bounds_and_content_length() {
        let transport = ScriptedTransport::with_responses([
            Ok(TransportResponse::new(200, vec![0; 5], None)),
            Ok(TransportResponse::new(200, vec![0; 3], Some(4))),
        ]);
        let client = HttpClient::new("https://example.test", transport, FixedClock(1));
        let Ok(client) = client else {
            panic!("expected a valid client");
        };
        let mut client = client.with_config(HttpClientConfig {
            max_response_bytes: 4,
        });

        assert!(matches!(
            client.get("/large", "", CachePolicy::Default),
            Err(ClientError::BodyTooLarge)
        ));
        assert!(matches!(
            client.get("/short", "", CachePolicy::Default),
            Err(ClientError::ContentLengthMismatch)
        ));
        assert_eq!(client.transport_mut().limits, vec![4, 4]);
    }

    #[test]
    fn legacy_success_rule_accepts_redirects_but_not_client_errors() {
        let transport = ScriptedTransport::with_responses([
            Ok(TransportResponse::new(302, Vec::new(), Some(0))),
            Ok(TransportResponse::new(404, b"missing".to_vec(), None)),
        ]);
        let client = HttpClient::new("https://example.test", transport, FixedClock(1));
        let Ok(mut client) = client else {
            panic!("expected a valid client");
        };
        let redirected = client.get("/redirect", "", CachePolicy::Default);
        let Ok(redirected) = redirected else {
            panic!("expected a redirect response");
        };
        assert!(redirected.is_success());
        assert_eq!(redirected.status_line(), "302 Found");

        let missing = client.get("/missing", "", CachePolicy::Default);
        let Ok(missing) = missing else {
            panic!("expected a missing response");
        };
        assert!(!missing.is_success());
        assert_eq!(status_message(999), "Unknown status code");
    }

    #[test]
    fn successful_download_atomically_replaces_the_destination() {
        let filesystem = TestFileSystem::default();
        filesystem.put("artifact.dll", b"old".to_vec());
        let transport = ScriptedTransport::with_responses([Ok(TransportResponse::new(
            200,
            b"new artifact".to_vec(),
            Some(12),
        ))]);
        let client = HttpClient::new("https://example.test", transport, FixedClock(1));
        let Ok(mut client) = client else {
            panic!("expected a valid client");
        };

        let receipt = client.download(
            &filesystem,
            Path::new("artifact.dll"),
            "/artifact.dll",
            "",
            1024,
        );
        let Ok(receipt) = receipt else {
            panic!("expected a successful download");
        };
        assert_eq!(receipt.bytes(), 12);
        assert_eq!(
            filesystem.get("artifact.dll"),
            Some(b"new artifact".to_vec())
        );
    }

    #[test]
    fn failed_download_validations_leave_the_destination_untouched() {
        let filesystem = TestFileSystem::default();
        filesystem.put("artifact.dll", b"old".to_vec());
        let transport = ScriptedTransport::with_responses([
            Ok(TransportResponse::new(500, b"server error".to_vec(), None)),
            Ok(TransportResponse::new(200, Vec::new(), Some(0))),
            Ok(TransportResponse::new(200, b"short".to_vec(), Some(99))),
            Ok(TransportResponse::new(200, vec![0; 5], None)),
        ]);
        let client = HttpClient::new("https://example.test", transport, FixedClock(1));
        let Ok(mut client) = client else {
            panic!("expected a valid client");
        };

        assert_eq!(
            client.download(&filesystem, Path::new("artifact.dll"), "/status", "", 100),
            Err(DownloadError::HttpStatus(500))
        );
        assert_eq!(
            client.download(&filesystem, Path::new("artifact.dll"), "/empty", "", 100),
            Err(DownloadError::EmptyBody)
        );
        assert_eq!(
            client.download(&filesystem, Path::new("artifact.dll"), "/length", "", 100),
            Err(DownloadError::ContentLengthMismatch)
        );
        assert_eq!(
            client.download(&filesystem, Path::new("artifact.dll"), "/large", "", 4),
            Err(DownloadError::BodyTooLarge)
        );
        assert_eq!(filesystem.get("artifact.dll"), Some(b"old".to_vec()));
    }

    #[test]
    fn storage_failure_is_redacted_and_preserves_existing_destination() {
        let filesystem = TestFileSystem::default();
        filesystem.put("artifact.dll", b"old".to_vec());
        filesystem.fail_writes(true);
        let transport = ScriptedTransport::with_responses([Ok(TransportResponse::new(
            200,
            b"new".to_vec(),
            None,
        ))]);
        let client = HttpClient::new("https://example.test", transport, FixedClock(1));
        let Ok(mut client) = client else {
            panic!("expected a valid client");
        };
        assert_eq!(
            client.download(&filesystem, Path::new("artifact.dll"), "/artifact", "", 100),
            Err(DownloadError::Storage)
        );
        assert_eq!(filesystem.get("artifact.dll"), Some(b"old".to_vec()));
        assert!(!DownloadError::Storage.to_string().contains("artifact.dll"));
    }
}
