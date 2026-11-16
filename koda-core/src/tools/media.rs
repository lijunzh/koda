//! Images Koda sends the model from files: what Read returns for an image,
//! and what an `@` mention of one sends. As Claude Code's Read, a large
//! image is scaled down and recompressed to fit the model's limits, and a
//! file over [`MAX_IMAGE_FILE_BYTES`](crate::tools::media::MAX_IMAGE_FILE_BYTES) is refused.
//!
//! The limits are Anthropic's: images are best at most [`MAX_EDGE`](crate::tools::media::MAX_EDGE) pixels
//! on the long edge (larger ones it scales down itself, so the extra pixels
//! only cost upload), and at most 5 MB each, which [`MAX_IMAGE_BYTES`](crate::tools::media::MAX_IMAGE_BYTES) keeps
//! under once base64-encoded.

use std::io::Cursor;

use base64::Engine;
use image::{GenericImageView, ImageFormat, ImageReader};

use crate::providers::ImageData;

/// The longest edge an image keeps.
pub const MAX_EDGE: u32 = 1568;

/// The most bytes an image may have before it is recompressed: 5 MB once
/// base64-encoded (4/3 larger).
pub const MAX_IMAGE_BYTES: usize = 3_750_000;

/// The largest image file Read takes, as Claude Code's.
pub const MAX_IMAGE_FILE_BYTES: u64 = 128 * 1024 * 1024;

/// An image ready to send, and what it is, for the tool's text.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    /// The image as sent.
    pub image: ImageData,
    /// Its size as sent, and as the file had it when that differs, such as
    /// "1568×980, scaled from 3136×1960".
    pub description: String,
}

/// `bytes` (an image of `media_type`) as sent: unchanged when it is within
/// the limits, else scaled to [`MAX_EDGE`] and recompressed, as PNG or, if
/// that is still too large, JPEG.
pub fn prepare_image(bytes: Vec<u8>, media_type: &str) -> anyhow::Result<PreparedImage> {
    let reader = ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?;
    let (width, height) = reader.into_dimensions()?;
    if width.max(height) <= MAX_EDGE && bytes.len() <= MAX_IMAGE_BYTES {
        return Ok(PreparedImage {
            image: encoded(media_type, &bytes),
            description: format!("{width}×{height}"),
        });
    }
    let decoded = image::load_from_memory(&bytes)?;
    let scaled = if width.max(height) > MAX_EDGE {
        decoded.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    let (w, h) = scaled.dimensions();
    let mut png = Cursor::new(Vec::new());
    scaled.write_to(&mut png, ImageFormat::Png)?;
    let (media_type, out) = if png.get_ref().len() <= MAX_IMAGE_BYTES {
        ("image/png", png.into_inner())
    } else {
        let mut jpeg = Cursor::new(Vec::new());
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85);
        scaled.to_rgb8().write_with_encoder(encoder)?;
        ("image/jpeg", jpeg.into_inner())
    };
    let description = if (w, h) == (width, height) {
        format!("{w}×{h}, recompressed")
    } else {
        format!("{w}×{h}, scaled from {width}×{height}")
    };
    Ok(PreparedImage {
        image: encoded(media_type, &out),
        description,
    })
}

fn encoded(media_type: &str, bytes: &[u8]) -> ImageData {
    ImageData {
        media_type: media_type.to_string(),
        base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([220, 20, 20]));
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn sent_dimensions(prepared: &PreparedImage) -> (u32, u32) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&prepared.image.base64)
            .unwrap();
        image::load_from_memory(&bytes).unwrap().dimensions()
    }

    #[test]
    fn a_small_image_is_sent_as_it_is() {
        let bytes = png(40, 30);
        let prepared = prepare_image(bytes.clone(), "image/png").unwrap();
        assert_eq!(prepared.description, "40×30");
        assert_eq!(
            prepared.image.base64,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
    }

    #[test]
    fn a_large_image_is_scaled_to_the_long_edge() {
        let prepared = prepare_image(png(3136, 1960), "image/png").unwrap();
        assert_eq!(sent_dimensions(&prepared), (MAX_EDGE, 980));
        assert_eq!(prepared.description, "1568×980, scaled from 3136×1960");
        assert_eq!(prepared.image.media_type, "image/png");
    }

    #[test]
    fn something_that_isnt_an_image_is_an_error() {
        assert!(prepare_image(b"\x89PNG\r\n\x1a\nnot really".to_vec(), "image/png").is_err());
    }
}
