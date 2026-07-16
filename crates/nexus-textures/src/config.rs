use crate::ConfigError;

/// Resource and queue limits enforced by a [`crate::TextureService`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextureConfig {
    /// Maximum UTF-8 byte length of a registry identifier.
    pub max_identifier_bytes: usize,
    /// Maximum byte length of a URL passed to the injected downloader.
    pub max_url_bytes: usize,
    /// Maximum encoded image or downloaded response size.
    pub max_encoded_bytes: usize,
    /// Maximum decoded image width.
    pub max_width: u32,
    /// Maximum decoded image height.
    pub max_height: u32,
    /// Maximum decoded pixel count.
    pub max_pixels: u64,
    /// Best-effort decoder allocation budget.
    pub max_decode_allocation_bytes: u64,
    /// Number of non-network requests which may wait for the decode worker.
    pub work_queue_capacity: usize,
    /// Number of URL requests which may wait for the download worker.
    pub download_queue_capacity: usize,
    /// Number of decoded results and ready callbacks which may wait for a frame.
    pub completion_queue_capacity: usize,
    /// Maximum callbacks which may join one in-flight identifier.
    pub max_callbacks_per_texture: usize,
    /// Maximum completions processed by one call to `advance`.
    pub max_completions_per_advance: usize,
}

impl TextureConfig {
    pub(crate) fn validate(self) -> Result<Self, ConfigError> {
        if self.max_identifier_bytes == 0
            || self.max_url_bytes == 0
            || self.max_encoded_bytes == 0
            || self.max_width == 0
            || self.max_height == 0
            || self.max_pixels == 0
            || self.max_decode_allocation_bytes == 0
            || self.work_queue_capacity == 0
            || self.download_queue_capacity == 0
            || self.completion_queue_capacity == 0
            || self.max_callbacks_per_texture == 0
            || self.max_completions_per_advance == 0
        {
            return Err(ConfigError::ZeroLimit);
        }

        let decoded_bytes = self
            .max_pixels
            .checked_mul(4)
            .ok_or(ConfigError::DecodedSizeOverflow)?;
        if decoded_bytes > self.max_decode_allocation_bytes {
            return Err(ConfigError::DecodeBudgetTooSmall);
        }

        Ok(self)
    }
}

impl Default for TextureConfig {
    fn default() -> Self {
        Self {
            max_identifier_bytes: 512,
            max_url_bytes: 8 * 1024,
            max_encoded_bytes: 16 * 1024 * 1024,
            max_width: 8_192,
            max_height: 8_192,
            max_pixels: 16 * 1024 * 1024,
            max_decode_allocation_bytes: 256 * 1024 * 1024,
            work_queue_capacity: 16,
            download_queue_capacity: 4,
            completion_queue_capacity: 8,
            max_callbacks_per_texture: 64,
            max_completions_per_advance: 8,
        }
    }
}
