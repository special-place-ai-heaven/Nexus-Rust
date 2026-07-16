use core::fmt;
use core::num::NonZeroUsize;

use crate::{DownloadTarget, ModuleHandle};

/// A closed backend failure. It deliberately carries no paths, URLs, or vendor messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendFailure {
    /// The backing facility is not available.
    Unavailable,
    /// The backing facility rejected malformed or unsupported input.
    Rejected,
    /// Work was cancelled during service shutdown.
    Cancelled,
}

/// Bounds supplied to an image decoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeLimits {
    /// Maximum width in pixels.
    pub max_width: u32,
    /// Maximum height in pixels.
    pub max_height: u32,
    /// Maximum total pixel count.
    pub max_pixels: u64,
    /// Best-effort allocation bound for the decoder.
    pub max_allocation_bytes: u64,
}

/// A decoded, tightly packed RGBA8 image.
#[derive(Clone, Eq, PartialEq)]
pub struct DecodedImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row-major RGBA8 pixels with a pitch of `width * 4`.
    pub rgba8: Vec<u8>,
}

impl fmt::Debug for DecodedImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DecodedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba8", &"<redacted>")
            .finish()
    }
}

/// Decodes an encoded image into tightly packed RGBA8 pixels.
pub trait ImageDecoder: Send + Sync + 'static {
    /// Decode `encoded` while honoring the supplied limits.
    fn decode(&self, encoded: &[u8], limits: DecodeLimits) -> Result<DecodedImage, BackendFailure>;
}

/// Fetches URL bytes. Implementations must enforce timeouts and must not log the target.
pub trait Downloader: Send + Sync + 'static {
    /// Fetch at most `max_bytes`; larger responses must fail rather than truncate.
    fn fetch(&self, target: &DownloadTarget, max_bytes: usize) -> Result<Vec<u8>, BackendFailure>;
}

/// Downloader which rejects every URL request.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDownloader;

impl Downloader for NoDownloader {
    fn fetch(
        &self,
        _target: &DownloadTarget,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, BackendFailure> {
        Err(BackendFailure::Unavailable)
    }
}

/// Owns one GPU shader-resource-view reference.
///
/// Implementations must keep the SRV alive until `Drop` and release exactly the
/// reference represented by [`GpuTexture::srv_address`] during destruction.
pub trait GpuTexture: Send + Sync + 'static {
    /// Address of the owned `ID3D11ShaderResourceView` interface.
    fn srv_address(&self) -> NonZeroUsize;
}

/// Creates an immutable RGBA8 texture and its shader-resource view.
pub trait GpuBackend: Send + Sync + 'static {
    /// Upload one validated image and return an object which owns the SRV reference.
    fn create_rgba8(&self, image: &DecodedImage) -> Result<Box<dyn GpuTexture>, BackendFailure>;
}

/// Looks up optional user-provided encoded image overrides.
pub trait OverrideProvider: Send + Sync + 'static {
    /// Return an encoded override, or `None` when the identifier has no override.
    fn load_override(
        &self,
        identifier: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, BackendFailure>;
}

/// Copies a PNG resource out of a loaded Windows module.
pub trait ResourceProvider: Send + Sync + 'static {
    /// Copy resource bytes before returning so the module may subsequently unload.
    fn load_png(
        &self,
        module: ModuleHandle,
        resource_id: u32,
        max_bytes: usize,
    ) -> Result<Vec<u8>, BackendFailure>;
}
