//! Platform-neutral RGBA8 upload validation.

use crate::D3d11TextureError;

/// Maximum width or height of a D3D11 Texture2D resource.
pub const MAX_TEXTURE2D_DIMENSION: u32 = 16_384;

/// Validated row-major RGBA8 upload layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgba8UploadLayout {
    width: u32,
    height: u32,
    row_pitch: u32,
    byte_len: usize,
}

impl Rgba8UploadLayout {
    /// Validates dimensions and an exact RGBA8 byte count.
    ///
    /// # Errors
    ///
    /// Returns a closed validation error for zero/oversized dimensions,
    /// arithmetic overflow, or a mismatched byte count.
    pub fn validate(width: u32, height: u32, pixel_len: usize) -> Result<Self, D3d11TextureError> {
        if width == 0 || height == 0 {
            return Err(D3d11TextureError::ZeroDimension);
        }
        if width > MAX_TEXTURE2D_DIMENSION || height > MAX_TEXTURE2D_DIMENSION {
            return Err(D3d11TextureError::DimensionTooLarge);
        }

        let (row_pitch, byte_len) = checked_layout(width, height)?;
        if pixel_len != byte_len {
            return Err(D3d11TextureError::PixelLengthMismatch);
        }

        Ok(Self {
            width,
            height,
            row_pitch,
            byte_len,
        })
    }

    /// Returns the width in pixels.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Returns the height in pixels.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    /// Returns the exact byte pitch of one row.
    #[must_use]
    pub const fn row_pitch(self) -> u32 {
        self.row_pitch
    }

    /// Returns the exact total upload size.
    #[must_use]
    pub const fn byte_len(self) -> usize {
        self.byte_len
    }
}

fn checked_layout(width: u32, height: u32) -> Result<(u32, usize), D3d11TextureError> {
    let row_pitch = width
        .checked_mul(4)
        .ok_or(D3d11TextureError::LayoutOverflow)?;
    let width = usize::try_from(width).map_err(|_error| D3d11TextureError::LayoutOverflow)?;
    let height = usize::try_from(height).map_err(|_error| D3d11TextureError::LayoutOverflow)?;
    let byte_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(D3d11TextureError::LayoutOverflow)?;
    Ok((row_pitch, byte_len))
}

#[cfg(test)]
mod tests {
    use super::{MAX_TEXTURE2D_DIMENSION, Rgba8UploadLayout, checked_layout};
    use crate::D3d11TextureError;

    #[test]
    fn valid_layout_has_exact_pitch_and_byte_count() {
        let layout = Rgba8UploadLayout::validate(3, 2, 24);
        let Ok(layout) = layout else {
            panic!("the descriptor should be valid");
        };
        assert_eq!(layout.width(), 3);
        assert_eq!(layout.height(), 2);
        assert_eq!(layout.row_pitch(), 12);
        assert_eq!(layout.byte_len(), 24);
    }

    #[test]
    fn zero_oversized_and_mismatched_inputs_are_distinct() {
        assert_eq!(
            Rgba8UploadLayout::validate(0, 1, 0),
            Err(D3d11TextureError::ZeroDimension)
        );
        assert_eq!(
            Rgba8UploadLayout::validate(MAX_TEXTURE2D_DIMENSION + 1, 1, 0),
            Err(D3d11TextureError::DimensionTooLarge)
        );
        assert_eq!(
            Rgba8UploadLayout::validate(2, 2, 15),
            Err(D3d11TextureError::PixelLengthMismatch)
        );
    }

    #[test]
    fn unchecked_descriptor_arithmetic_detects_pitch_overflow() {
        assert_eq!(
            checked_layout(u32::MAX, 1),
            Err(D3d11TextureError::LayoutOverflow)
        );

        #[cfg(target_pointer_width = "32")]
        assert_eq!(
            checked_layout(u32::MAX / 4, u32::MAX),
            Err(D3d11TextureError::LayoutOverflow)
        );

        #[cfg(target_pointer_width = "64")]
        assert!(checked_layout(u32::MAX / 4, u32::MAX).is_ok());
    }

    #[test]
    fn maximum_legal_descriptor_arithmetic_is_exact() {
        let expected = usize::try_from(MAX_TEXTURE2D_DIMENSION)
            .ok()
            .and_then(|side| side.checked_mul(side))
            .and_then(|pixels| pixels.checked_mul(4));
        let Some(expected) = expected else {
            panic!("supported targets can represent the D3D11 maximum layout");
        };
        assert_eq!(
            Rgba8UploadLayout::validate(
                MAX_TEXTURE2D_DIMENSION,
                MAX_TEXTURE2D_DIMENSION,
                expected,
            )
            .map(Rgba8UploadLayout::byte_len),
            Ok(expected)
        );
    }
}
