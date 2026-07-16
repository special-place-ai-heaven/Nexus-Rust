use std::io::Cursor;

use image::{ImageReader, Limits};

use crate::{BackendFailure, DecodeLimits, DecodedImage, ImageDecoder};

/// Production decoder backed by the memory-safe `image` crate.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImageRsDecoder;

impl ImageDecoder for ImageRsDecoder {
    fn decode(&self, encoded: &[u8], limits: DecodeLimits) -> Result<DecodedImage, BackendFailure> {
        let mut reader = ImageReader::new(Cursor::new(encoded))
            .with_guessed_format()
            .map_err(|_| BackendFailure::Rejected)?;
        let mut image_limits = Limits::default();
        image_limits.max_image_width = Some(limits.max_width);
        image_limits.max_image_height = Some(limits.max_height);
        image_limits.max_alloc = Some(limits.max_allocation_bytes);
        reader.limits(image_limits);

        let image = reader
            .decode()
            .map_err(|_| BackendFailure::Rejected)?
            .into_rgba8();
        let (width, height) = image.dimensions();
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or(BackendFailure::Rejected)?;
        if pixels == 0 || pixels > limits.max_pixels {
            return Err(BackendFailure::Rejected);
        }

        Ok(DecodedImage {
            width,
            height,
            rgba8: image.into_raw(),
        })
    }
}
