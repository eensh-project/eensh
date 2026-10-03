//! The capture pipeline.
//!
//! This module is where the stages are stitched together, and it is the only
//! place that knows about all of them. The ordering is fixed:
//!
//! ```text
//! X11 / Xvfb
//!     -> capture backend      (raw Frame, source geometry)
//!     -> resize               (still a raw Frame)
//!     -> image encoder        (encoded bytes, image geometry)
//!     -> optional base64      (encoded bytes only, never raw pixels)
//!     -> JSON / file / stdout
//! ```
//!
//! Each step is independently timed so that a slow observation can be
//! attributed to a specific stage. Failures stop the pipeline without emitting a
//! partial image.

use crate::capture::{x11, CaptureRequest, Display};
use crate::cli::{ResizeRequest, ResolvedCapture};
use crate::encode::{self, EncodeOptions};
use crate::error::Error;
use crate::frame::Frame;
use crate::geometry::{resize_by_scale, resize_to_height, resize_to_width, Rect, Transform};
use crate::output::{base64, json::CaptureResponse};
use crate::resize;
use crate::timing::{Stopwatch, TimingBuilder};

/// The outcome of a successful capture.
#[derive(Debug)]
pub struct CaptureOutcome {
    /// The completed JSON response, whether or not it was written to stdout.
    pub response: CaptureResponse,
    /// The encoded image bytes.
    pub encoded: Vec<u8>,
    /// Whether those bytes were written to the configured image destination.
    pub image_written: bool,
    /// Whether the JSON response was written to a stream.
    pub metadata_written: bool,
}

/// Run a fully resolved capture.
///
/// The returned outcome reports what was written where, so the CLI can decide
/// whether to print a diagnostic without re-deriving the routing rules.
pub fn run(config: &ResolvedCapture) -> Result<CaptureOutcome, Error> {
    let mut timing = TimingBuilder::start();

    // Stage 1: capture. The display is opened here and closed when this
    // function returns; nothing downstream can hold on to it.
    let mut stopwatch = Stopwatch::start();
    let frame = capture_frame(config)?;
    timing.capture(&mut stopwatch);

    // Stage 2: resize, on raw pixels.
    let mut stopwatch = Stopwatch::start();
    let frame = apply_resize(frame, config.resize)?;
    timing.resize(&mut stopwatch);

    let image_rect = Rect::new(
        frame.source_geometry.x,
        frame.source_geometry.y,
        frame.source_geometry.width,
        frame.source_geometry.height,
    )?;

    // Stage 3: encode.
    let mut stopwatch = Stopwatch::start();
    let options = EncodeOptions {
        format: config.format,
        quality: config.quality,
        png_effort: config.png_effort,
    };
    let encoded = encode::encode(&frame, &options)?;
    timing.encode(&mut stopwatch);

    // Stage 4: base64, applied only to the encoded bytes.
    let mut stopwatch = Stopwatch::start();
    let data = if config.base64 {
        Some(base64::encode(&encoded.bytes))
    } else {
        None
    };
    timing.base64(&mut stopwatch);

    let timing = timing.finish();

    // The transform maps image pixels back to source pixels, and is derived
    // from the *source* rectangle and the *encoded* size. Deriving it from the
    // resized frame instead would quietly claim a source size that never
    // existed.
    let transform = Transform::new(&image_rect, encoded.width, encoded.height)?;

    let quality = match config.format {
        encode::ImageFormat::Jpeg => Some(config.quality),
        encode::ImageFormat::Png => None,
    };

    let response = CaptureResponse::new(
        frame.source_geometry.clone(),
        &encoded,
        transform,
        timing,
        data,
        quality,
    );

    // Stage 5: output.
    let mut image_written = false;
    if config.output.write_image_bytes {
        config.output.destination.write_image(&encoded.bytes)?;
        image_written = true;
    }

    let mut metadata_written = false;
    if !matches!(
        config.output.metadata,
        crate::output::MetadataDestination::None
    ) {
        let text = response.to_json_string()?;
        config.output.metadata.write(&text)?;
        metadata_written = true;
    }

    Ok(CaptureOutcome {
        response,
        encoded: encoded.bytes,
        image_written,
        metadata_written,
    })
}

/// Open the display and capture the requested target.
fn capture_frame(config: &ResolvedCapture) -> Result<Frame, Error> {
    let display = Display::open(&config.display)?;

    // Resolve the request against the live display so that geometry mistakes
    // are reported before any pixels are read.
    match &config.request {
        CaptureRequest::Desktop => x11::capture_desktop(&display),
        CaptureRequest::Region(rect) => x11::capture_region(&display, rect),
        CaptureRequest::Window(id) => x11::capture_window(&display, *id),
    }
}

/// Apply the requested resize to a raw frame.
fn apply_resize(frame: Frame, request: ResizeRequest) -> Result<Frame, Error> {
    let (width, height) = match request {
        ResizeRequest::None => return Ok(frame),
        ResizeRequest::Width(target) => {
            let (w, h) = resize_to_width(frame.width(), frame.height(), target);
            (w, h)
        }
        ResizeRequest::Height(target) => {
            let (w, h) = resize_to_height(frame.width(), frame.height(), target);
            (w, h)
        }
        ResizeRequest::Scale(factor) => {
            let (w, h) = resize_by_scale(frame.width(), frame.height(), factor);
            (w, h)
        }
    };

    resize::resize(&frame, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{PixelBuffer, PixelFormat};
    use crate::geometry::{CaptureTarget, SourceGeometry};
    use std::time::Instant;

    fn frame(width: u32, height: u32) -> Frame {
        let data = vec![128u8; (width * height * 3) as usize];
        let pixels = PixelBuffer::new(width, height, PixelFormat::Rgb8, data).unwrap();
        Frame::new(
            SourceGeometry {
                target: CaptureTarget::Desktop,
                display: Some(":99".into()),
                x: 0,
                y: 0,
                width,
                height,
            },
            pixels,
            Instant::now(),
        )
    }

    #[test]
    fn resize_request_none_returns_the_frame_untouched() {
        let original = frame(4, 4);
        let out = apply_resize(original.clone(), ResizeRequest::None).unwrap();
        assert_eq!(out.width(), original.width());
        assert_eq!(out.pixels.data(), original.pixels.data());
    }

    #[test]
    fn resize_requests_produce_the_expected_dimensions() {
        let out = apply_resize(frame(1920, 1080), ResizeRequest::Width(960)).unwrap();
        assert_eq!((out.width(), out.height()), (960, 540));

        let out = apply_resize(frame(1920, 1080), ResizeRequest::Height(540)).unwrap();
        assert_eq!((out.width(), out.height()), (960, 540));

        let out = apply_resize(frame(1920, 1080), ResizeRequest::Scale(0.25)).unwrap();
        assert_eq!((out.width(), out.height()), (480, 270));
    }

    #[test]
    fn resize_preserves_the_reported_source_geometry() {
        let out = apply_resize(frame(1920, 1080), ResizeRequest::Width(320)).unwrap();
        assert_eq!(out.source_geometry.width, 1920);
        assert_eq!(out.source_geometry.height, 1080);
        assert_eq!(out.width(), 320);
    }
}
