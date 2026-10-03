//! Decoding saved images into raw frames.
//!
//! This is the *input boundary* for `eensh diff`. File formats are a
//! presentation concern, so decoding lives here at the edge and produces the
//! same raw [`Frame`] the capture backend produces. The comparison engine never
//! sees a PNG or a JPEG, which is what keeps it usable on live captures.
//!
//! Conceptually:
//!
//! ```text
//! PNG/JPEG file
//!     -> decode to Frame-compatible pixels  (this module)
//!     -> compare_frames(...)                (compare)
//! ```

use std::path::Path;
use std::time::Instant;

use crate::error::Error;
use crate::frame::{Frame, PixelBuffer, PixelFormat};
use crate::geometry::{CaptureTarget, Rect, SourceGeometry};

/// Read and decode an image file into a raw frame.
///
/// The format is determined from the file's contents rather than its extension,
/// so a mislabelled file still decodes correctly. The resulting frame describes
/// its origin as coming from `path` and uses a top-left origin at `(0, 0)`, sized
/// to the decoded image.
pub fn load_frame(path: &Path) -> Result<Frame, Error> {
    let bytes = std::fs::read(path)
        .map_err(|e| Error::image_load_failed(format!("could not read {}: {e}", path.display())))?;

    if bytes.is_empty() {
        return Err(Error::image_load_failed(format!(
            "{} is empty",
            path.display()
        )));
    }

    let format = image::guess_format(&bytes).map_err(|e| {
        Error::image_load_failed(format!(
            "could not determine the image format of {}: {e}",
            path.display()
        ))
    })?;

    // Only the formats `eensh` can actually write are accepted, so that a diff
    // cannot silently depend on a codec that is not in the build.
    match format {
        image::ImageFormat::Png => decode_png(&bytes, path),
        image::ImageFormat::Jpeg => decode_via_image_crate(&bytes, format, path),
        other => Err(Error::image_load_failed(format!(
            "{} is a {} image; only PNG and JPEG are supported",
            path.display(),
            format_name(other)
        ))),
    }
}

/// Decode a JPEG using the `image` crate.
fn decode_via_image_crate(
    bytes: &[u8],
    format: image::ImageFormat,
    path: &Path,
) -> Result<Frame, Error> {
    let decoded = image::load_from_memory_with_format(bytes, format).map_err(|e| {
        Error::image_load_failed(format!("could not decode {}: {e}", path.display()))
    })?;

    let rgb = decoded.to_rgb8();
    let (width, height) = (rgb.width(), rgb.height());

    if width == 0 || height == 0 {
        return Err(Error::image_load_failed(format!(
            "{} decoded to a zero-sized image",
            path.display()
        )));
    }

    let pixels = PixelBuffer::new(width, height, PixelFormat::Rgb8, rgb.into_raw())?;

    let source_geometry = SourceGeometry {
        target: CaptureTarget::File {
            path: path.display().to_string(),
        },
        display: None,
        x: 0,
        y: 0,
        width,
        height,
    };

    Ok(Frame::new(source_geometry, pixels, Instant::now()))
}

/// Decode a PNG using the `png` crate.
///
/// This mirrors how [`crate::encode::png`] writes: the same crate, with the same
/// transformations, so a PNG written by `eensh` is read back byte for byte
/// whatever its colour type or bit depth. The `image` crate cannot be used here
/// because its PNG codec is not enabled in this build.
fn decode_png(bytes: &[u8], path: &Path) -> Result<Frame, Error> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Normalise palette, grayscale, 16-bit, and stripped-alpha inputs to plain
    // 8-bit RGB, which is the one pixel format the comparison engine handles.
    decoder.set_transformations(png::Transformations::normalize_to_color8());

    let mut reader = decoder.read_info().map_err(|e| {
        Error::image_load_failed(format!("could not read {} as PNG: {e}", path.display()))
    })?;

    let output_size = reader.output_buffer_size().ok_or_else(|| {
        Error::image_load_failed(format!("{} is too large to decode", path.display()))
    })?;

    let mut buffer = vec![0u8; output_size];
    let info = reader.next_frame(&mut buffer).map_err(|e| {
        Error::image_load_failed(format!("could not decode {}: {e}", path.display()))
    })?;
    buffer.truncate(info.buffer_size());

    let (width, height) = (info.width, info.height);
    if width == 0 || height == 0 {
        return Err(Error::image_load_failed(format!(
            "{} decoded to a zero-sized image",
            path.display()
        )));
    }

    let rgb = normalise_to_rgb8(&buffer, info.color_type, width, height).ok_or_else(|| {
        Error::image_load_failed(format!(
            "{} decoded to an unsupported colour type ({:?})",
            path.display(),
            info.color_type
        ))
    })?;

    let pixels = PixelBuffer::new(width, height, PixelFormat::Rgb8, rgb)?;

    let source_geometry = SourceGeometry {
        target: CaptureTarget::File {
            path: path.display().to_string(),
        },
        display: None,
        x: 0,
        y: 0,
        width,
        height,
    };

    Ok(Frame::new(source_geometry, pixels, Instant::now()))
}

/// Convert decoded PNG samples to tightly packed 8-bit RGB.
///
/// `normalize_to_color8` gives us grayscale, grayscale+alpha, RGB, or RGBA at
/// 8 bits per sample, so this is a widening step with no scaling arithmetic.
fn normalise_to_rgb8(
    samples: &[u8],
    color_type: png::ColorType,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    let pixel_count = width as usize * height as usize;
    let mut out = Vec::with_capacity(pixel_count * 3);

    match color_type {
        png::ColorType::Rgb => {
            if samples.len() < pixel_count * 3 {
                return None;
            }
            out.extend_from_slice(&samples[..pixel_count * 3]);
        }
        png::ColorType::Rgba => {
            if samples.len() < pixel_count * 4 {
                return None;
            }
            for pixel in samples.chunks_exact(4).take(pixel_count) {
                out.extend_from_slice(&pixel[..3]);
            }
        }
        png::ColorType::Grayscale => {
            if samples.len() < pixel_count {
                return None;
            }
            for &value in samples.iter().take(pixel_count) {
                out.extend_from_slice(&[value, value, value]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            if samples.len() < pixel_count * 2 {
                return None;
            }
            for pixel in samples.chunks_exact(2).take(pixel_count) {
                out.extend_from_slice(&[pixel[0], pixel[0], pixel[0]]);
            }
        }
        png::ColorType::Indexed => return None,
    }

    Some(out)
}

/// Build a frame covering an already-decoded region, for tests and for callers
/// that hold raw pixels rather than a file.
pub fn frame_from_rgb8(width: u32, height: u32, rgb: Vec<u8>) -> Result<Frame, Error> {
    let pixels = PixelBuffer::new(width, height, PixelFormat::Rgb8, rgb)?;
    let geometry = SourceGeometry {
        target: CaptureTarget::Region,
        display: None,
        x: 0,
        y: 0,
        width,
        height,
    };
    Ok(Frame::new(geometry, pixels, Instant::now()))
}

/// A readable name for an image format, for error messages.
fn format_name(format: image::ImageFormat) -> String {
    format!("{:?}", format).to_ascii_lowercase()
}

/// The frame-local rectangle covering a whole frame.
pub fn frame_rect(frame: &Frame) -> Result<Rect, Error> {
    Rect::new(0, 0, frame.width(), frame.height())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("eensh-input-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn loading_a_missing_file_reports_image_load_failed() {
        let error = load_frame(Path::new("/no/such/image.png")).unwrap_err();
        assert_eq!(error.code(), "image_load_failed");
        assert!(error.message().contains("could not read"));
    }

    #[test]
    fn loading_an_empty_file_reports_image_load_failed() {
        let path = temp_dir("empty").join("empty.png");
        std::fs::write(&path, b"").unwrap();
        let error = load_frame(&path).unwrap_err();
        assert_eq!(error.code(), "image_load_failed");
        assert!(error.message().contains("empty"));
    }

    #[test]
    fn loading_a_non_image_reports_image_load_failed() {
        let path = temp_dir("garbage").join("garbage.png");
        std::fs::write(&path, b"this is definitely not an image").unwrap();
        let error = load_frame(&path).unwrap_err();
        assert_eq!(error.code(), "image_load_failed");
    }

    #[test]
    fn decoding_rejects_unsupported_formats() {
        // A minimal GIF header: recognised, but not a format eensh writes.
        let path = temp_dir("gif").join("little.gif");
        std::fs::write(&path, b"GIF89a\x01\x00\x01\x00\x00\x00\x00;").unwrap();
        let error = load_frame(&path).unwrap_err();
        assert_eq!(error.code(), "image_load_failed");
        assert!(
            error.message().contains("only PNG and JPEG"),
            "message was: {}",
            error.message()
        );
    }

    #[test]
    fn file_frames_are_labelled_with_their_path() {
        // Round-trip through a real PNG produced by the encoder.
        let source = frame_from_rgb8(
            3,
            2,
            vec![
                10, 20, 30, 40, 50, 60, 70, 80, 90, 1, 2, 3, 4, 5, 6, 7, 8, 9,
            ],
        )
        .unwrap();
        let encoded = crate::encode::png::encode(&source, crate::encode::PngEffort::Fast).unwrap();

        let path = temp_dir("roundtrip").join("image.png");
        std::fs::write(&path, &encoded).unwrap();

        let loaded = load_frame(&path).unwrap();
        assert_eq!((loaded.width(), loaded.height()), (3, 2));
        assert_eq!(loaded.pixels.data(), source.pixels.data());
        assert_eq!(
            loaded.source_geometry.target,
            CaptureTarget::File {
                path: path.display().to_string()
            }
        );
    }

    #[test]
    fn format_is_detected_from_content_not_extension() {
        let source = frame_from_rgb8(2, 2, vec![0; 12]).unwrap();
        let encoded = crate::encode::png::encode(&source, crate::encode::PngEffort::Fast).unwrap();

        // A PNG with a misleading .jpg extension must still decode.
        let path = temp_dir("misnamed").join("actually-a-png.jpg");
        std::fs::write(&path, &encoded).unwrap();

        let loaded = load_frame(&path).unwrap();
        assert_eq!((loaded.width(), loaded.height()), (2, 2));
    }

    #[test]
    fn frame_rect_covers_the_whole_frame() {
        let frame = frame_from_rgb8(5, 7, vec![0; 5 * 7 * 3]).unwrap();
        assert_eq!(frame_rect(&frame).unwrap(), Rect::new(0, 0, 5, 7).unwrap());
    }
}
