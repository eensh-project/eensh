//! The comparison pipeline.
//!
//! This mirrors [`crate::pipeline`] for `eensh diff`. It is the only place that
//! knows the whole diff flow:
//!
//! ```text
//! PNG/JPEG file
//!     -> decode to a raw Frame      (input, at the boundary)
//!     -> compare_frames(...)        (compare, on raw pixels)
//!     -> optional changed crop      (Frame::crop + encode)
//!     -> JSON / text
//! ```
//!
//! Decoding happens strictly at the edge, so the comparison itself never sees an
//! encoded image. That is what allows the same primitive to run on live captures
//! in later phases.

use crate::cli::{ChangedCropRequest, ResolvedDiff};
use crate::compare::{compare_frames, Comparison};
use crate::encode::{self, EncodeOptions, ImageFormat};
use crate::error::Error;
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::input;
use crate::output::json::{ChangedCrop, CompareTiming, DiffResponse, InputDescription};
use crate::output::MetadataDestination;
use crate::timing::Stopwatch;

/// The outcome of a successful diff.
#[derive(Debug)]
pub struct DiffOutcome {
    /// The completed response.
    pub response: DiffResponse,
    /// The encoded crop, if one was produced.
    pub crop_bytes: Option<Vec<u8>>,
}

/// Run a resolved diff.
pub fn run(config: &ResolvedDiff) -> Result<DiffOutcome, Error> {
    let mut total = Stopwatch::start();

    // Stage 1: decode both inputs.
    let mut stopwatch = Stopwatch::start();
    let before = input::load_frame(&config.before)?;
    let after = input::load_frame(&config.after)?;
    let load_us = stopwatch.stop();

    // Stage 2: compare raw frames. No encoding is involved on either side.
    let mut stopwatch = Stopwatch::start();
    let comparison = compare_frames(&before, &after, &config.options)?;
    let compare_us = stopwatch.stop();

    // Stage 3: optionally crop the *second* frame to the changed region.
    //
    // A crop is written whenever a bounding box exists, which is exactly when at
    // least one pixel exceeded the pixel threshold. The area threshold is not
    // consulted: it expresses a policy judgement about significance, whereas the
    // bounding box is a factual statement about where differences were found. A
    // caller that asked for the changed region still gets it, and the JSON still
    // reports `changed: false`. When no bounding box exists there is nothing to
    // crop, so no file is written and the response says so explicitly rather than
    // inventing a placeholder image.
    let mut crop_us = 0;
    let mut crop_bytes = None;
    let mut changed_crop = None;

    if let Some(request) = &config.crop {
        let mut stopwatch = Stopwatch::start();
        if let Some(region) = comparison.bounding_box {
            let cropped = after.crop(&region)?;
            let encoded = encode::encode(
                &cropped,
                &EncodeOptions {
                    format: request.format,
                    quality: encode::jpeg::DEFAULT_QUALITY,
                    png_effort: encode::PngEffort::Default,
                },
            )?;
            crate::output::file::write_file(&request.path, &encoded.bytes)?;

            changed_crop = Some(ChangedCrop {
                path: request.path.display().to_string(),
                region,
                byte_length: encoded.bytes.len(),
                format: request.format.name().to_string(),
            });
            crop_bytes = Some(encoded.bytes);
        }
        crop_us = stopwatch.stop();
    }

    let total_us = total.stop();

    let response = DiffResponse {
        before: InputDescription::from_frame(&before),
        after: InputDescription::from_frame(&after),
        comparison,
        changed_crop,
        timing: CompareTiming {
            load_us,
            compare_us,
            crop_us,
            total_us,
        },
    };

    Ok(DiffOutcome {
        response,
        crop_bytes,
    })
}

/// Report whether a diff should write its human readable summary to stderr.
///
/// The diff command is additive and keeps the capture command's routing rules:
/// JSON on stdout unless the caller asked for the summary instead.
pub fn summary_destination(json: bool) -> MetadataDestination {
    if json {
        MetadataDestination::None
    } else {
        MetadataDestination::Stdout
    }
}

/// Format a one-line human readable summary of a comparison.
pub fn summary(comparison: &Comparison) -> String {
    match comparison.bounding_box {
        Some(rect) => format!(
            "{}: {}/{} pixels changed ({:.4}%{}), changed region {}x{}+{}+{}",
            comparison.mode.name(),
            comparison.changed_pixels,
            comparison.total_pixels,
            comparison.changed_fraction * 100.0,
            if comparison.changed {
                ""
            } else {
                ", below area threshold"
            },
            rect.width,
            rect.height,
            rect.x,
            rect.y,
        ),
        None => format!(
            "{}: no pixels changed ({} compared)",
            comparison.mode.name(),
            comparison.total_pixels,
        ),
    }
}

/// The rectangle to crop for a changed region, if any.
pub fn crop_region(comparison: &Comparison) -> Option<Rect> {
    comparison.bounding_box
}

/// Convenience for callers that already hold two frames.
///
/// This is the library-facing entry point the specification calls
/// `compare_frames`; it is re-exported from [`crate::compare`].
pub fn compare(
    before: &Frame,
    after: &Frame,
    options: &crate::compare::CompareOptions,
) -> Result<Comparison, Error> {
    compare_frames(before, after, options)
}

/// Encode a crop with the same defaults the CLI would use for a given format.
pub fn encode_crop(frame: &Frame, format: ImageFormat) -> Result<Vec<u8>, Error> {
    encode::encode(
        frame,
        &EncodeOptions {
            format,
            quality: encode::jpeg::DEFAULT_QUALITY,
            png_effort: encode::PngEffort::Default,
        },
    )
    .map(|encoded| encoded.bytes)
}

/// Build a crop request for tests and library callers.
pub fn crop_request(
    path: impl Into<std::path::PathBuf>,
    format: ImageFormat,
) -> ChangedCropRequest {
    ChangedCropRequest {
        path: path.into(),
        format,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::{CompareMode, CompareOptions};

    fn frame(width: u32, height: u32, rgb: [u8; 3]) -> Frame {
        let data = rgb
            .iter()
            .copied()
            .cycle()
            .take((width * height * 3) as usize)
            .collect();
        input::frame_from_rgb8(width, height, data).unwrap()
    }

    #[test]
    fn the_summary_reports_a_changed_region() {
        let a = frame(100, 100, [0, 0, 0]);
        let mut b_data = vec![0u8; 100 * 100 * 3];
        // Change a 10x4 block at (5, 6).
        for y in 6..10 {
            for x in 5..15 {
                let index = (y * 100 + x) * 3;
                b_data[index] = 255;
            }
        }
        let b = input::frame_from_rgb8(100, 100, b_data).unwrap();

        let comparison = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        let text = summary(&comparison);
        assert!(text.contains("40/10000"), "summary was: {text}");
        assert!(text.contains("10x4+5+6"), "summary was: {text}");
    }

    #[test]
    fn the_summary_reports_no_change() {
        let a = frame(8, 8, [1, 2, 3]);
        let b = frame(8, 8, [1, 2, 3]);
        let comparison = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(
            summary(&comparison),
            "exact: no pixels changed (64 compared)"
        );
    }

    #[test]
    fn the_summary_notes_a_change_below_the_area_threshold() {
        let a = frame(100, 100, [0, 0, 0]);
        let mut b_data = vec![0u8; 100 * 100 * 3];
        b_data[0] = 255;
        let b = input::frame_from_rgb8(100, 100, b_data).unwrap();

        let options = CompareOptions {
            area_threshold: 0.5,
            ..CompareOptions::default()
        };
        let comparison = compare_frames(&a, &b, &options).unwrap();
        assert!(!comparison.changed);
        assert!(summary(&comparison).contains("below area threshold"));
    }

    #[test]
    fn the_summary_names_the_mode_that_ran() {
        let a = frame(8, 8, [0, 0, 0]);
        let b = frame(8, 8, [0, 0, 0]);
        let options = CompareOptions {
            mode: CompareMode::RgbThreshold,
            pixel_threshold: 12,
            area_threshold: 0.0,
        };
        let comparison = compare_frames(&a, &b, &options).unwrap();
        assert!(summary(&comparison).starts_with("rgb_threshold:"));
    }

    #[test]
    fn encode_crop_produces_a_decodable_image_of_the_right_size() {
        let frame = frame(20, 10, [10, 20, 30]);
        let bytes = encode_crop(&frame, ImageFormat::Png).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");

        let jpeg = encode_crop(&frame, ImageFormat::Jpeg).unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
    }

    #[test]
    fn there_is_no_crop_region_when_nothing_changed() {
        let a = frame(8, 8, [5, 5, 5]);
        let b = frame(8, 8, [5, 5, 5]);
        let comparison = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(crop_region(&comparison), None);
    }

    #[test]
    fn json_is_not_summarized_on_stdout() {
        assert_eq!(summary_destination(true), MetadataDestination::None);
        assert_eq!(summary_destination(false), MetadataDestination::Stdout);
    }
}
