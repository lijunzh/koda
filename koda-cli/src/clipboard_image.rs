//! Pasting an image from the clipboard, as Claude Code does ("Paste image
//! from clipboard": Ctrl+V, Cmd+V in iTerm2, Alt+V on Windows and WSL) and
//! Codex does (Ctrl+V and Ctrl+Alt+V, `clipboard_paste.rs`).
//!
//! The clipboard holds pixels, not a file, so they are encoded as PNG.
//! An image larger than [`MAX_EDGE`] on its long edge is scaled down first:
//! Anthropic scales such images down itself, so the extra pixels only cost
//! upload size, and a full Retina screenshot can pass the API's 5 MB limit
//! per image. Claude Code and Codex resize too.

use base64::Engine;

use crate::input::ImageData;

/// The longest edge a pasted image keeps, Anthropic's recommended maximum.
pub(crate) const MAX_EDGE: u32 = 1568;

/// Why no image came from the clipboard.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PasteImageError {
    /// Over SSH the clipboard is on another machine.
    OverSsh,
    /// The clipboard holds no image.
    NoImage,
    /// The clipboard couldn't be read, or the image couldn't be encoded.
    Failed(String),
}

impl std::fmt::Display for PasteImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OverSsh => write!(
                f,
                "Over SSH Koda can't read your clipboard. Give the image's path instead."
            ),
            Self::NoImage => write!(f, "No image on the clipboard."),
            Self::Failed(why) => write!(f, "Couldn't paste the image: {why}"),
        }
    }
}

/// The clipboard's image, as a PNG to send with the prompt.
pub(crate) fn read() -> Result<ImageData, PasteImageError> {
    if std::env::var_os("SSH_CONNECTION").is_some() {
        return Err(PasteImageError::OverSsh);
    }
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| PasteImageError::Failed(e.to_string()))?;
    let image = clipboard.get_image().map_err(|e| match e {
        arboard::Error::ContentNotAvailable => PasteImageError::NoImage,
        e => PasteImageError::Failed(e.to_string()),
    })?;
    let png = encode_png(
        image.width as u32,
        image.height as u32,
        image.bytes.into_owned(),
    )
    .map_err(PasteImageError::Failed)?;
    Ok(ImageData {
        media_type: "image/png".into(),
        base64: base64::engine::general_purpose::STANDARD.encode(png),
    })
}

/// RGBA pixels as a PNG, scaled down to [`MAX_EDGE`] on the long edge.
fn encode_png(width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut image = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| "the clipboard's image data is the wrong size".to_string())?;
    let long = width.max(height);
    if long > MAX_EDGE {
        let scale = f64::from(MAX_EDGE) / f64::from(long);
        let w = ((f64::from(width) * scale).round() as u32).max(1);
        let h = ((f64::from(height) * scale).round() as u32).max(1);
        image = image::imageops::resize(&image, w, h, image::imageops::FilterType::Triangle);
    }
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(png.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dimensions(png: &[u8]) -> (u32, u32) {
        let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).unwrap();
        (image.width(), image.height())
    }

    #[test]
    fn a_small_image_keeps_its_size() {
        let png = encode_png(4, 3, vec![255; 4 * 3 * 4]).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(dimensions(&png), (4, 3));
    }

    /// A Retina screenshot comes out at the recommended size, its shape
    /// kept.
    #[test]
    fn a_large_image_is_scaled_to_the_long_edge() {
        let png = encode_png(3136, 1960, vec![0; 3136 * 1960 * 4]).unwrap();
        assert_eq!(dimensions(&png), (MAX_EDGE, 980));
    }

    #[test]
    fn pixels_that_dont_fit_the_size_are_an_error() {
        assert!(encode_png(10, 10, vec![0; 12]).is_err());
    }
}
