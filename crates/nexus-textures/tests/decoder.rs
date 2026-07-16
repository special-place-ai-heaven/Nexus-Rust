//! Production image-decoder limit tests.

use image::{ImageEncoder, codecs::png::PngEncoder};
use nexus_textures::{DecodeLimits, ImageDecoder, ImageRsDecoder};

#[test]
fn production_decoder_outputs_bounded_rgba8() {
    let mut encoded = Vec::new();
    PngEncoder::new(&mut encoded)
        .write_image(&[10, 20, 30, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .expect("test PNG should encode");
    let decoded = ImageRsDecoder
        .decode(
            &encoded,
            DecodeLimits {
                max_width: 16,
                max_height: 16,
                max_pixels: 256,
                max_allocation_bytes: 1_024 * 1_024,
            },
        )
        .expect("test PNG should decode");
    assert_eq!(decoded.width, 1);
    assert_eq!(decoded.height, 1);
    assert_eq!(decoded.rgba8, vec![10, 20, 30, 255]);
}

#[test]
fn production_decoder_rejects_dimension_limit() {
    let mut encoded = Vec::new();
    PngEncoder::new(&mut encoded)
        .write_image(&[0; 8], 2, 1, image::ExtendedColorType::Rgba8)
        .expect("test PNG should encode");
    let result = ImageRsDecoder.decode(
        &encoded,
        DecodeLimits {
            max_width: 1,
            max_height: 1,
            max_pixels: 1,
            max_allocation_bytes: 1_024 * 1_024,
        },
    );
    assert!(result.is_err());
}
