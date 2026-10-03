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
use crate::encode::{self, EncodeOptions, EncodedImage, ImageFormat, PngEffort};
use crate::error::Error;
use crate::frame::Frame;
use crate::geometry::{
    resize_by_scale, resize_to_height, resize_to_width, Rect, SourceGeometry, Transform,
};
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

/// How a returned frame should be resized and encoded.
///
/// Extracted so that `capture` and the Phase 3 observation commands share one
/// presentation path. Observation must not invent a second way to encode a
/// frame: it uses this, on the single frame the operation ends on.
///
/// Not `Eq`, because [`ResizeRequest::Scale`] carries an `f64`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageOptions {
    /// Requested resize.
    pub resize: ResizeRequest,
    /// Output format.
    pub format: ImageFormat,
    /// JPEG quality.
    pub quality: u8,
    /// PNG compression effort.
    pub png_effort: PngEffort,
    /// Whether to embed the image as base64 in the JSON response.
    pub base64: bool,
}

impl ImageOptions {
    /// Take the image settings from a resolved capture.
    pub fn from_capture(config: &ResolvedCapture) -> Self {
        ImageOptions {
            resize: config.resize,
            format: config.format,
            quality: config.quality,
            png_effort: config.png_effort,
            base64: config.base64,
        }
    }
}

/// A frame that has been resized, encoded, and optionally base64 encoded.
///
/// The native source geometry is carried separately from the encoded image,
/// because a resize means those are two different coordinate spaces and the
/// response must not conflate them.
#[derive(Debug)]
pub struct PreparedImage {
    /// Where the frame came from, in native source pixels.
    pub source_geometry: SourceGeometry,
    /// The encoded image.
    pub encoded: EncodedImage,
    /// Base64 of the encoded bytes, when requested.
    pub data: Option<String>,
    /// Image-to-source coordinate mapping.
    pub transform: Transform,
    /// Resize duration in microseconds.
    pub resize_us: u64,
    /// Encode duration in microseconds.
    pub encode_us: u64,
    /// Base64 duration in microseconds.
    pub base64_us: u64,
}

impl PreparedImage {
    /// JPEG quality to report, when the image is a JPEG.
    pub fn quality(&self, options: &ImageOptions) -> Option<u8> {
        match options.format {
            ImageFormat::Jpeg => Some(options.quality),
            ImageFormat::Png => None,
        }
    }
}

/// Resize, encode, and optionally base64 encode a frame.
///
/// This is the whole presentation path for a returned frame, and it runs exactly
/// once per operation — for a capture, on the captured frame; for an observation,
/// on the settled frame. Sampled frames during an observation are never encoded.
pub fn prepare_image(frame: Frame, options: &ImageOptions) -> Result<PreparedImage, Error> {
    // Resize is applied to raw pixels, before encoding.
    let mut stopwatch = Stopwatch::start();
    let frame = apply_resize(frame, options.resize)?;
    let resize_us = stopwatch.stop();

    let image_rect = Rect::new(
        frame.source_geometry.x,
        frame.source_geometry.y,
        frame.source_geometry.width,
        frame.source_geometry.height,
    )?;

    let mut stopwatch = Stopwatch::start();
    let encoded = encode::encode(
        &frame,
        &EncodeOptions {
            format: options.format,
            quality: options.quality,
            png_effort: options.png_effort,
        },
    )?;
    let encode_us = stopwatch.stop();

    let mut stopwatch = Stopwatch::start();
    let data = if options.base64 {
        Some(base64::encode(&encoded.bytes))
    } else {
        None
    };
    let base64_us = stopwatch.stop();

    // The transform maps image pixels back to source pixels, and is derived from
    // the *native source* rectangle and the *encoded* size. Deriving it from the
    // resized frame instead would quietly claim a source size that never existed.
    let transform = Transform::new(&image_rect, encoded.width, encoded.height)?;

    Ok(PreparedImage {
        source_geometry: frame.source_geometry,
        encoded,
        data,
        transform,
        resize_us,
        encode_us,
        base64_us,
    })
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

    // Stages 2-4: resize, encode, and base64, shared with observation.
    let options = ImageOptions::from_capture(config);
    let prepared = prepare_image(frame, &options)?;
    timing.record(&prepared);

    let timing = timing.finish();
    let quality = prepared.quality(&options);

    let response = CaptureResponse::new(
        prepared.source_geometry.clone(),
        &prepared.encoded,
        prepared.transform,
        timing,
        prepared.data.clone(),
        quality,
    );

    // Stage 5: output.
    let mut image_written = false;
    if config.output.write_image_bytes {
        config
            .output
            .destination
            .write_image(&prepared.encoded.bytes)?;
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
        encoded: prepared.encoded.bytes,
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
pub fn apply_resize(frame: Frame, request: ResizeRequest) -> Result<Frame, Error> {
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
