use core::fmt;

use thiserror::Error;

/// Identifies a bounded service queue without exposing request data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueKind {
    /// File, resource, override, and memory decoding work.
    Work,
    /// URL download work.
    Download,
    /// Decoded results and ready callbacks.
    Completion,
}

impl fmt::Display for QueueKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Work => "work",
            Self::Download => "download",
            Self::Completion => "completion",
        })
    }
}

/// Closed errors returned while constructing a texture service.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    /// A configured bound was zero.
    #[error("texture service limits must be non-zero")]
    ZeroLimit,
    /// The decoded byte bound overflowed its integer representation.
    #[error("decoded texture size limit overflows")]
    DecodedSizeOverflow,
    /// The decoder allocation budget cannot hold the maximum RGBA image.
    #[error("decoder allocation budget is below the RGBA output limit")]
    DecodeBudgetTooSmall,
    /// A worker thread could not be started.
    #[error("texture worker could not be started")]
    WorkerSpawnFailed,
}

/// Closed, redaction-safe failure returned to callers and callbacks.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TextureError {
    /// The registry identifier was empty, too long, or contained a NUL byte.
    #[error("invalid texture identifier")]
    InvalidIdentifier,
    /// The URL exceeded the configured byte bound.
    #[error("texture URL exceeds its configured bound")]
    UrlTooLong,
    /// Encoded input exceeded its configured byte bound.
    #[error("encoded texture exceeds its configured bound")]
    EncodedTooLarge,
    /// A decoded image had invalid dimensions or byte length.
    #[error("decoded texture violates its configured bounds")]
    InvalidDecodedImage,
    /// A bounded queue had no remaining capacity.
    #[error("texture {0} queue is full")]
    QueueFull(QueueKind),
    /// A file could not be read.
    #[error("texture file is unavailable")]
    FileUnavailable,
    /// An embedded resource could not be copied.
    #[error("texture resource is unavailable")]
    ResourceUnavailable,
    /// An override lookup failed.
    #[error("texture override is unavailable")]
    OverrideUnavailable,
    /// A download failed or was rejected.
    #[error("texture download failed")]
    DownloadFailed,
    /// Encoded data could not be decoded.
    #[error("texture decode failed")]
    DecodeFailed,
    /// GPU texture or shader-resource-view creation failed.
    #[error("texture GPU upload failed")]
    GpuUploadFailed,
    /// Too many callbacks joined the same pending identifier.
    #[error("texture callback limit reached")]
    CallbackLimit,
    /// The service is stopping and accepts no new requests.
    #[error("texture service is stopped")]
    ServiceStopped,
}
