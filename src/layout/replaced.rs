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

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub enum ImageSlot {
    /// The element is `<img>`; its `src` produced this bitmap.
    Img,
    /// The element has a `background-image: url(...)` that resolved to this
    /// bitmap.
    Background,
}

pub type ImageCache = HashMap<(NodeId, ImageSlot), ImageInfo>;

/// Decode an image from its raw bytes. Recognises PNG, JPEG, WebP, GIF,
/// BMP, SVG (via `resvg`), and AVIF (via `avif-decode`). Returns
/// `None` for unknown formats, decode errors, or oversized images.
pub fn decode_image(bytes: &[u8]) -> Option<ImageInfo> {
    // Try SVG first when the bytes look textual + match the SVG sniff.
    if looks_like_svg(bytes) {
        if let Some(info) = decode_svg(bytes) {
            return Some(info);
        }
    }
    // AVIF magic: "ftypavif" / "ftypavis" inside the first 32 bytes.
    if looks_like_avif(bytes) {
        if let Some(info) = decode_avif(bytes) {
            return Some(info);
        }
    }
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

fn looks_like_avif(bytes: &[u8]) -> bool {
    if bytes.len() < 16 {
        return false;
    }
    let head = &bytes[..bytes.len().min(64)];
    // Look for "ftypavi" inside the box header — covers `avif`, `avis`,
    // and the `mif1` major brand with `avif` compatible-brand tagging.
    head.windows(7).any(|w| w == b"ftypavi")
}

fn decode_avif(bytes: &[u8]) -> Option<ImageInfo> {
    let img = avif_decode::Decoder::from_avif(bytes).ok()?.to_image().ok()?;
    let (rgba, width, height) = match img {
        avif_decode::Image::Rgba8(buf) => {
            let (w, h) = (buf.width() as u32, buf.height() as u32);
            let pixels = buf.buf().iter().flat_map(|p| [p.r, p.g, p.b, p.a]).collect();
            (pixels, w, h)
        }
        avif_decode::Image::Rgb8(buf) => {
            let (w, h) = (buf.width() as u32, buf.height() as u32);
            let pixels = buf
                .buf()
                .iter()
                .flat_map(|p| [p.r, p.g, p.b, 255])
                .collect();
            (pixels, w, h)
        }
        avif_decode::Image::Gray8(buf) => {
            let (w, h) = (buf.width() as u32, buf.height() as u32);
            let pixels = buf
                .buf()
                .iter()
                .flat_map(|p| {
                    let v = p.value();
                    [v, v, v, 255]
                })
                .collect();
            (pixels, w, h)
        }
        // 16-bit variants: downsample to 8-bit. Rare in practice for
        // web AVIF, so the lossy approximation is fine here.
        avif_decode::Image::Rgba16(buf) => {
            let (w, h) = (buf.width() as u32, buf.height() as u32);
            let pixels = buf
                .buf()
                .iter()
                .flat_map(|p| {
                    [
                        (p.r >> 8) as u8,
                        (p.g >> 8) as u8,
                        (p.b >> 8) as u8,
                        (p.a >> 8) as u8,
                    ]
                })
                .collect();
            (pixels, w, h)
        }
        avif_decode::Image::Rgb16(buf) => {
            let (w, h) = (buf.width() as u32, buf.height() as u32);
            let pixels = buf
                .buf()
                .iter()
                .flat_map(|p| [(p.r >> 8) as u8, (p.g >> 8) as u8, (p.b >> 8) as u8, 255])
                .collect();
            (pixels, w, h)
        }
        avif_decode::Image::Gray16(buf) => {
            let (w, h) = (buf.width() as u32, buf.height() as u32);
            let pixels = buf
                .buf()
                .iter()
                .flat_map(|p| {
                    let v = (p.value() >> 8) as u8;
                    [v, v, v, 255]
                })
                .collect();
            (pixels, w, h)
        }
    };
    if width == 0 || height == 0 {
        return None;
    }
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return None;
    }
    Some(ImageInfo {
        width,
        height,
        rgba,
    })
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head_len = bytes.len().min(512);
    let head = std::str::from_utf8(&bytes[..head_len]).unwrap_or("").trim_start();
    head.starts_with("<?xml") && head.contains("<svg")
        || head.starts_with("<svg")
        || (head.starts_with("<!DOCTYPE") && head.to_ascii_lowercase().contains("svg"))
}

fn decode_svg(bytes: &[u8]) -> Option<ImageInfo> {
    use resvg::tiny_skia;
    use resvg::usvg;

    let opts = usvg::Options::default();
    let tree = usvg::Tree::from_data(bytes, &opts).ok()?;
    let size = tree.size().to_int_size();
    let width = size.width().min(MAX_IMAGE_DIMENSION);
    let height = size.height().min(MAX_IMAGE_DIMENSION);
    if width == 0 || height == 0 {
        return None;
    }
    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    Some(ImageInfo {
        width,
        height,
        rgba: pixmap.take(),
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

    #[test]
    fn decode_inline_svg() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="5">
            <rect width="10" height="5" fill="red"/>
        </svg>"#;
        let info = decode_image(svg).expect("svg should decode");
        assert_eq!(info.width, 10);
        assert_eq!(info.height, 5);
        assert_eq!(info.rgba.len(), 10 * 5 * 4);
        // Top-left pixel should be red.
        assert_eq!(info.rgba[0], 255);
        assert_eq!(info.rgba[3], 255);
    }
}
