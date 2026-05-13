//! Decoded image data for replaced elements (`<img>`).
//!
//! Layout consumes intrinsic dimensions. Phase 5 (paint) will consume the
//! `rgba` pixel buffer to blit into the display list.

use std::collections::HashMap;
use std::io::Cursor;

use crate::dom::NodeId;

/// Hard cap on a single image's dimensions to defend against decompression
/// bombs. A 16k × 16k 32-bit RGBA buffer is already 1 GiB, so anything
/// larger we refuse to decode.
pub const MAX_IMAGE_DIMENSION: u32 = 16384;

#[derive(Debug)]
#[allow(dead_code)] // rgba consumed by paint in phase 5
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub type ImageCache = HashMap<NodeId, ImageInfo>;

/// Decode an image from its raw bytes. Recognises PNG, JPEG, WebP, GIF, BMP.
/// Returns `None` for unknown formats, decode errors, or oversized images.
pub fn decode_image(bytes: &[u8]) -> Option<ImageInfo> {
    let reader = ::image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let img = reader.decode().ok()?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return None;
    }
    Some(ImageInfo {
        width,
        height,
        rgba: rgba.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a minimal PNG via the `image` crate, then round-trip it
    /// through our decoder. Validates dimensions and the RGBA byte count.
    #[test]
    fn decode_round_trip_png() {
        use ::image::{
            codecs::png::PngEncoder, ExtendedColorType, ImageBuffer, ImageEncoder, Rgba,
        };
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(2, 3, |_, _| Rgba([255, 0, 0, 255]));
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(img.as_raw(), 2, 3, ExtendedColorType::Rgba8)
            .unwrap();
        let info = decode_image(&bytes).expect("PNG round-trip should decode");
        assert_eq!(info.width, 2);
        assert_eq!(info.height, 3);
        assert_eq!(info.rgba.len(), 2 * 3 * 4);
    }

    #[test]
    fn decode_garbage_returns_none() {
        assert!(decode_image(b"not an image").is_none());
    }
}
