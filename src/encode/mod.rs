//! Image encoders.
//!
//! Encoders take a finished [`Frame`] and produce encoded bytes. They never
//! see a display, never resize, and never base64 encode: those are separate
//! stages by design.
//!
//! Both encoders are pure Rust and require no system image libraries, which
//! keeps `eensh` buildable on any machine with a stock Rust toolchain.

pub mod jpeg;
pub mod png;

use crate::error::Error;
use crate::frame::Frame;
use serde::{Deserialize, Serialize};

/// Supported output image formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    /// Parse a format name as accepted by `--format`.
    pub fn from_name(name: &str) -> Result<Self, Error> {
        match name.to_ascii_lowercase().as_str() {
            "png" => Ok(ImageFormat::Png),
            "jpg" | "jpeg" => Ok(ImageFormat::Jpeg),
            other => Err(Error::invalid_arguments(format!(
                "unsupported format {other:?}; expected one of: png, jpeg"
            ))),
        }
    }

    /// Canonical lower-case name.
    pub fn name(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpeg",
        }
    }

    /// IANA media type used in JSON output.
    pub fn media_type(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        }
    }

    /// Infer a format from an output path's extension, if it has one.
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?;
        Self::from_name(extension).ok()
    }
}

impl std::fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// How much effort to spend compressing PNG output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PngEffort {
    Fast,
    Default,
    Best,
}

impl PngEffort {
    /// Parse the value of `--compression`.
    pub fn from_name(name: &str) -> Result<Self, Error> {
        match name.to_ascii_lowercase().as_str() {
            "fast" => Ok(PngEffort::Fast),
            "default" => Ok(PngEffort::Default),
            "best" => Ok(PngEffort::Best),
            other => Err(Error::invalid_arguments(format!(
                "unsupported PNG compression {other:?}; expected one of: fast, default, best"
            ))),
        }
    }
}

/// Encoder settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeOptions {
    pub format: ImageFormat,
    /// JPEG quality in `1..=100`. Ignored for PNG.
    pub quality: u8,
    /// PNG compression effort. Ignored for JPEG.
    pub png_effort: PngEffort,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        EncodeOptions {
            format: ImageFormat::Png,
            quality: jpeg::DEFAULT_QUALITY,
            png_effort: PngEffort::Default,
        }
    }
}

/// An encoded image, ready to be written out or base64 encoded.
#[derive(Debug, Clone)]
pub struct EncodedImage {
    /// The encoded bytes (a complete PNG or JPEG file).
    pub bytes: Vec<u8>,
    /// The format that produced these bytes.
    pub format: ImageFormat,
    /// Width of the image described by these bytes.
    pub width: u32,
    /// Height of the image described by these bytes.
    pub height: u32,
}

impl EncodedImage {
    /// IANA media type of the encoded image.
    pub fn media_type(&self) -> &'static str {
        self.format.media_type()
    }
}

/// Encode a frame using the requested options.
pub fn encode(frame: &Frame, options: &EncodeOptions) -> Result<EncodedImage, Error> {
    let bytes = match options.format {
        ImageFormat::Png => png::encode(frame, options.png_effort)?,
        ImageFormat::Jpeg => jpeg::encode(frame, options.quality)?,
    };

    if bytes.is_empty() {
        return Err(Error::encode_failed(
            "the encoder produced no output".to_string(),
        ));
    }

    Ok(EncodedImage {
        bytes,
        format: options.format,
        width: frame.width(),
        height: frame.height(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_names_round_trip() {
        assert_eq!(ImageFormat::from_name("png").unwrap(), ImageFormat::Png);
        assert_eq!(ImageFormat::from_name("PNG").unwrap(), ImageFormat::Png);
        assert_eq!(ImageFormat::from_name("jpg").unwrap(), ImageFormat::Jpeg);
        assert_eq!(ImageFormat::from_name("jpeg").unwrap(), ImageFormat::Jpeg);
        assert_eq!(ImageFormat::from_name("JPEG").unwrap(), ImageFormat::Jpeg);
        assert!(ImageFormat::from_name("gif").is_err());
    }

    #[test]
    fn media_types_are_correct() {
        assert_eq!(ImageFormat::Png.media_type(), "image/png");
        assert_eq!(ImageFormat::Jpeg.media_type(), "image/jpeg");
    }

    #[test]
    fn format_is_inferred_from_path_extension() {
        assert_eq!(
            ImageFormat::from_path(std::path::Path::new("/tmp/shot.png")),
            Some(ImageFormat::Png)
        );
        assert_eq!(
            ImageFormat::from_path(std::path::Path::new("/tmp/shot.JPG")),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(
            ImageFormat::from_path(std::path::Path::new("/tmp/shot")),
            None
        );
    }

    #[test]
    fn png_effort_parsing() {
        assert_eq!(PngEffort::from_name("fast").unwrap(), PngEffort::Fast);
        assert_eq!(PngEffort::from_name("best").unwrap(), PngEffort::Best);
        assert!(PngEffort::from_name("turbo").is_err());
    }
}
