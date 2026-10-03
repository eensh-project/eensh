//! Raw frame comparison.
//!
//! This module answers one question: given two raw [`Frame`]s, what changed and
//! where? It operates directly on pixel buffers, never on encoded images, and it
//! knows nothing about X11, JSON, base64, the filesystem, or the CLI. Those are
//! all outside its remit by design, because this is the primitive that later
//! phases will call repeatedly inside an observation loop.
//!
//! # The comparison rule
//!
//! For each pixel, the difference is measured one way only:
//!
//! ```text
//! difference = max(|r1 - r2|, |g1 - g2|, |b1 - b2|)
//! ```
//!
//! A pixel is **changed** when `difference > pixel_threshold`. The comparison is
//! strict, so `pixel_threshold = 0` means "any difference at all", which is
//! exactly the behaviour of [`CompareMode::Exact`]. An `alpha` channel, if a
//! future pixel format carries one, is ignored: Phase 1 frames are opaque.
//!
//! A frame is **meaningfully changed** when it has at least one changed pixel and
//! `changed_fraction >= area_threshold`. That comparison is inclusive, so
//! `area_threshold = 1.0` means "only a wholly changed frame matters" and a
//! threshold of `0.0` means "any changed pixel matters".
//!
//! A frame in which nothing changed is never reported as changed, whatever the
//! area threshold is, because there is no change to report. Without that
//! qualification the degenerate threshold `0.0` would make two identical frames
//! compare as changed, since `0.0 >= 0.0`.
//!
//! All three boundary behaviours — the strict pixel comparison, the inclusive
//! area comparison, and the zero-change rule — are deliberate and are covered by
//! tests.

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::frame::{Frame, PixelFormat};
use crate::geometry::Rect;

/// The largest accepted `pixel_threshold`, matching the range of an 8-bit channel.
pub const MAX_PIXEL_THRESHOLD: u8 = 255;

/// How pixel differences are measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareMode {
    /// Any difference in any channel counts.
    ///
    /// Identical to [`CompareMode::RgbThreshold`] with a threshold of zero; it
    /// exists as a separate name because that is how callers think about it.
    Exact,
    /// A pixel counts only when its largest channel difference exceeds
    /// [`CompareOptions::pixel_threshold`].
    RgbThreshold,
}

impl CompareMode {
    /// Parse a mode name as accepted by `--mode`.
    pub fn from_name(name: &str) -> Result<Self, Error> {
        match name.to_ascii_lowercase().as_str() {
            "exact" => Ok(CompareMode::Exact),
            "rgb" | "rgb_threshold" | "threshold" => Ok(CompareMode::RgbThreshold),
            other => Err(Error::invalid_arguments(format!(
                "unsupported compare mode {other:?}; expected one of: exact, rgb"
            ))),
        }
    }

    /// Canonical name used in output.
    pub fn name(self) -> &'static str {
        match self {
            CompareMode::Exact => "exact",
            CompareMode::RgbThreshold => "rgb_threshold",
        }
    }
}

impl std::fmt::Display for CompareMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// How to compare two frames.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompareOptions {
    /// How pixel differences are measured.
    pub mode: CompareMode,
    /// Largest channel difference still considered unchanged, in `0..=255`.
    pub pixel_threshold: u8,
    /// Smallest changed fraction still considered meaningful, in `0.0..=1.0`.
    pub area_threshold: f64,
}

impl Default for CompareOptions {
    /// Report every differing pixel.
    ///
    /// The default is deliberately the strictest useful setting: `Exact`
    /// comparison with no area threshold. A `diff` invocation with no options
    /// therefore answers "did anything at all differ", and tolerance is opt-in.
    fn default() -> Self {
        CompareOptions {
            mode: CompareMode::Exact,
            pixel_threshold: 0,
            area_threshold: 0.0,
        }
    }
}

impl CompareOptions {
    /// Validate the options, rejecting values that would make the comparison
    /// meaningless.
    pub fn validate(&self) -> Result<(), Error> {
        if !self.area_threshold.is_finite() || !(0.0..=1.0).contains(&self.area_threshold) {
            return Err(Error::invalid_arguments(format!(
                "area threshold must be between 0.0 and 1.0, got {}",
                self.area_threshold
            )));
        }
        Ok(())
    }

    /// The effective per-pixel threshold, treating exact mode as a threshold of
    /// zero.
    fn effective_pixel_threshold(&self) -> u8 {
        match self.mode {
            CompareMode::Exact => 0,
            CompareMode::RgbThreshold => self.pixel_threshold,
        }
    }
}

/// The result of comparing two frames.
///
/// This is deliberately a handful of integers plus one optional rectangle: a
/// later observation loop may evaluate it dozens of times per second, so it must
/// stay cheap to produce and cheap to hold.
///
/// The options that produced the result are echoed back so that a serialized
/// result is self-describing and does not have to be interpreted alongside
/// whatever flags happened to be used.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// The mode the comparison actually used.
    pub mode: CompareMode,
    /// The per-pixel threshold the comparison actually used.
    pub pixel_threshold: u8,
    /// The area threshold the comparison actually used.
    pub area_threshold: f64,
    /// Whether the changed area reached `area_threshold`.
    ///
    /// Always `false` when `changed_pixels` is zero.
    pub changed: bool,
    /// Number of pixels whose difference exceeded `pixel_threshold`.
    pub changed_pixels: u64,
    /// Number of pixels compared.
    pub total_pixels: u64,
    /// `changed_pixels / total_pixels`, in `0.0..=1.0`.
    pub changed_fraction: f64,
    /// Smallest rectangle containing every changed pixel, in frame-local
    /// coordinates, or `None` when nothing changed.
    ///
    /// This is reported whenever *any* pixel changed, including when the area
    /// threshold then decided the frame was not meaningfully changed. That
    /// distinction is the point: a caller can see that something moved even when
    /// the change is too small to act on.
    pub bounding_box: Option<Rect>,
}

impl Comparison {
    /// Assemble a comparison from scan counts.
    ///
    /// Kept separate from the scan so that the arithmetic — including the
    /// threshold decisions and the fraction — can be tested directly for very
    /// large frames that could never be allocated in a test.
    pub(crate) fn from_counts(
        options: &CompareOptions,
        changed_pixels: u64,
        total_pixels: u64,
        bounds: Option<(u32, u32, u32, u32)>,
    ) -> Result<Comparison, Error> {
        if total_pixels == 0 {
            return Err(Error::comparison_failed(
                "cannot compare a frame with no pixels".to_string(),
            ));
        }
        if changed_pixels > total_pixels {
            return Err(Error::comparison_failed(format!(
                "changed pixel count {changed_pixels} exceeds the total of {total_pixels}"
            )));
        }

        let changed_fraction = changed_pixels as f64 / total_pixels as f64;
        // A frame with no changed pixels is never meaningful change. Without this
        // guard a zero area threshold would make identical frames compare as
        // changed, because `0.0 >= 0.0`.
        let changed = changed_pixels > 0 && changed_fraction >= options.area_threshold;

        let bounding_box = match bounds {
            Some((min_x, min_y, max_x, max_y)) => {
                // Computed in 64-bit so that a frame wider than `u32::MAX / 2`
                // cannot wrap. Both bounds are below the frame dimensions, so the
                // result always fits.
                let width = max_x as u64 - min_x as u64 + 1;
                let height = max_y as u64 - min_y as u64 + 1;
                let rect = Rect::new(
                    min_x as i32,
                    min_y as i32,
                    u32::try_from(width).map_err(|_| {
                        Error::comparison_failed("changed region width is out of range".to_string())
                    })?,
                    u32::try_from(height).map_err(|_| {
                        Error::comparison_failed(
                            "changed region height is out of range".to_string(),
                        )
                    })?,
                )?;
                Some(rect)
            }
            None => None,
        };

        // A bounding box exists exactly when at least one pixel changed.
        debug_assert_eq!(bounding_box.is_some(), changed_pixels > 0);

        Ok(Comparison {
            mode: options.mode,
            pixel_threshold: options.effective_pixel_threshold(),
            area_threshold: options.area_threshold,
            changed,
            changed_pixels,
            total_pixels,
            changed_fraction,
            bounding_box,
        })
    }
}

/// Compare two raw frames.
///
/// Comparison is like-for-like: the frames must have the same pixel dimensions
/// and the same pixel format. A mismatch is an [`Error::IncompatibleFrames`]
/// rather than an implicit resize or an overlap comparison, because either of
/// those would make `changed_fraction` mean something the caller did not ask
/// for.
///
/// The scan always covers the whole frame. It does not stop early once
/// `area_threshold` is satisfied, because `changed_pixels`, `changed_fraction`,
/// and `bounding_box` must be exact regardless.
pub fn compare_frames(
    before: &Frame,
    after: &Frame,
    options: &CompareOptions,
) -> Result<Comparison, Error> {
    options.validate()?;

    if before.width() != after.width() || before.height() != after.height() {
        return Err(Error::incompatible_frames(format!(
            "frame dimensions differ: {}x{} vs {}x{}",
            before.width(),
            before.height(),
            after.width(),
            after.height()
        )));
    }

    if before.pixel_format != after.pixel_format {
        return Err(Error::incompatible_frames(format!(
            "pixel formats differ: {:?} vs {:?}",
            before.pixel_format, after.pixel_format
        )));
    }

    match before.pixel_format {
        PixelFormat::Rgb8 => {}
    }

    let width = before.width() as usize;
    let height = before.height() as usize;
    let stride = width * PixelFormat::Rgb8.bytes_per_pixel();

    if before.pixels.data().len() < stride * height || after.pixels.data().len() < stride * height {
        return Err(Error::comparison_failed(
            "a frame's pixel buffer is smaller than its declared dimensions".to_string(),
        ));
    }

    let scan = match options.mode {
        CompareMode::Exact => scan(
            before.pixels.data(),
            after.pixels.data(),
            width,
            height,
            stride,
            |a, b| a[0] != b[0] || a[1] != b[1] || a[2] != b[2],
        ),
        CompareMode::RgbThreshold => {
            let threshold = options.pixel_threshold as i32;
            scan(
                before.pixels.data(),
                after.pixels.data(),
                width,
                height,
                stride,
                move |a, b| {
                    // Most pixels are identical in a typical observation loop, and
                    // comparing three bytes at once is far cheaper than widening to
                    // 32-bit and taking absolute differences. This is a pure fast
                    // path: it cannot change the result, because equal channels have
                    // a difference of zero, which never exceeds the threshold.
                    if a == b {
                        return false;
                    }
                    // Threshold zero is exactly "any difference", so the fast path
                    // above has already decided all the interesting cases and the
                    // remaining test is a formality.
                    if threshold == 0 {
                        return true;
                    }
                    let dr = (a[0] as i32 - b[0] as i32).abs();
                    let dg = (a[1] as i32 - b[1] as i32).abs();
                    let db = (a[2] as i32 - b[2] as i32).abs();
                    dr.max(dg).max(db) > threshold
                },
            )
        }
    };

    let total_pixels = width as u64 * height as u64;
    Comparison::from_counts(options, scan.changed_pixels, total_pixels, scan.bounds)
}

/// Raw output of a single pass over the pixels.
struct ScanResult {
    changed_pixels: u64,
    /// Inclusive bounds of changed pixels as `(min_x, min_y, max_x, max_y)`.
    bounds: Option<(u32, u32, u32, u32)>,
}

/// Walk every pixel once, counting changes and accumulating their bounds.
///
/// `is_changed` is a closure rather than a flag so that the mode check happens
/// once, at monomorphisation, instead of once per pixel. The loop is written to
/// keep the per-pixel work to three subtractions and a comparison, and it
/// allocates nothing.
#[inline]
fn scan<F>(
    before: &[u8],
    after: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    is_changed: F,
) -> ScanResult
where
    F: Fn(&[u8], &[u8]) -> bool,
{
    let mut changed_pixels: u64 = 0;
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;

    for y in 0..height {
        let row_start = y * stride;
        let row_a = &before[row_start..row_start + stride];
        let row_b = &after[row_start..row_start + stride];

        // Track the row's own extent and merge it once per row. During a
        // widespread change this replaces three comparisons per pixel with three
        // per row.
        let mut row_min = u32::MAX;
        let mut row_max = 0u32;
        let mut row_changed = 0u64;

        for x in 0..width {
            let offset = x * 3;
            let a = &row_a[offset..offset + 3];
            let b = &row_b[offset..offset + 3];
            if is_changed(a, b) {
                row_changed += 1;
                let x = x as u32;
                if x < row_min {
                    row_min = x;
                }
                if x > row_max {
                    row_max = x;
                }
            }
        }

        if row_changed > 0 {
            changed_pixels += row_changed;
            let y = y as u32;
            if row_min < min_x {
                min_x = row_min;
            }
            if row_max > max_x {
                max_x = row_max;
            }
            if y < min_y {
                min_y = y;
            }
            if y > max_y {
                max_y = y;
            }
        }
    }

    let bounds = if changed_pixels > 0 {
        Some((min_x, min_y, max_x, max_y))
    } else {
        None
    };

    ScanResult {
        changed_pixels,
        bounds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{CaptureTarget, SourceGeometry};
    use std::time::Instant;

    /// Build a frame from a per-pixel function.
    fn frame(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Frame {
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&pixel(x, y));
            }
        }
        let pixels =
            crate::frame::PixelBuffer::new(width, height, PixelFormat::Rgb8, data).unwrap();
        Frame::new(
            SourceGeometry {
                target: CaptureTarget::Desktop,
                display: None,
                x: 0,
                y: 0,
                width,
                height,
            },
            pixels,
            Instant::now(),
        )
    }

    fn uniform(width: u32, height: u32, rgb: [u8; 3]) -> Frame {
        frame(width, height, |_, _| rgb)
    }

    /// Options with a given pixel threshold and no area threshold.
    fn with_pixel_threshold(mode: CompareMode, threshold: u8) -> CompareOptions {
        CompareOptions {
            mode,
            pixel_threshold: threshold,
            area_threshold: 0.0,
        }
    }

    // --- 22.1 identical frames -------------------------------------------------

    #[test]
    fn identical_frames_report_no_change() {
        let a = uniform(64, 48, [10, 20, 30]);
        let b = uniform(64, 48, [10, 20, 30]);
        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();

        assert!(!result.changed);
        assert_eq!(result.changed_pixels, 0);
        assert_eq!(result.total_pixels, 64 * 48);
        assert_eq!(result.changed_fraction, 0.0);
        assert_eq!(result.bounding_box, None);
    }

    #[test]
    fn identical_frames_report_no_change_in_threshold_mode_too() {
        let a = uniform(32, 32, [200, 100, 50]);
        let b = uniform(32, 32, [200, 100, 50]);
        let result =
            compare_frames(&a, &b, &with_pixel_threshold(CompareMode::RgbThreshold, 12)).unwrap();
        assert_eq!(result.changed_pixels, 0);
        assert_eq!(result.bounding_box, None);
    }

    // --- 22.2 one changed pixel -----------------------------------------------

    #[test]
    fn a_single_changed_pixel_is_counted_and_bounded_exactly() {
        let a = uniform(10, 10, [0, 0, 0]);
        let b = frame(10, 10, |x, y| {
            if x == 3 && y == 7 {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        });

        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(result.changed_pixels, 1);
        assert_eq!(result.total_pixels, 100);
        assert_eq!(result.changed_fraction, 0.01);
        assert_eq!(result.bounding_box, Some(Rect::new(3, 7, 1, 1).unwrap()));
        assert!(
            result.changed,
            "one pixel out of 100 is 1%, above a zero threshold"
        );
    }

    #[test]
    fn the_area_threshold_decides_changed_independently_of_the_count() {
        let a = uniform(100, 100, [0, 0, 0]);
        let b = frame(100, 100, |x, y| {
            if x == 0 && y == 0 {
                [1, 1, 1]
            } else {
                [0, 0, 0]
            }
        });

        // 1 pixel in 10000 is 0.0001.
        let strict = CompareOptions {
            area_threshold: 0.5,
            ..CompareOptions::default()
        };
        let result = compare_frames(&a, &b, &strict).unwrap();
        assert!(!result.changed, "0.0001 must not clear a 0.5 threshold");
        // The raw metrics and the bounding box survive the decision.
        assert_eq!(result.changed_pixels, 1);
        assert_eq!(result.bounding_box, Some(Rect::new(0, 0, 1, 1).unwrap()));

        let lax = CompareOptions {
            area_threshold: 0.0001,
            ..CompareOptions::default()
        };
        assert!(compare_frames(&a, &b, &lax).unwrap().changed);
    }

    // --- 22.3 several changed pixels ------------------------------------------

    #[test]
    fn changed_pixels_at_known_corners_produce_the_exact_bounding_box() {
        // A 20x10 frame where the changed pixels are exactly the corners.
        let a = uniform(20, 10, [0, 0, 0]);
        let b = frame(20, 10, |x, y| {
            let corner_x = x == 2 || x == 17;
            let corner_y = y == 1 || y == 8;
            if corner_x && corner_y {
                [255, 0, 0]
            } else {
                [0, 0, 0]
            }
        });

        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(result.changed_pixels, 4);
        // x spans 2..=17 (16 wide), y spans 1..=8 (8 tall).
        assert_eq!(result.bounding_box, Some(Rect::new(2, 1, 16, 8).unwrap()));
    }

    // --- 22.4 exact mode ------------------------------------------------------

    #[test]
    fn exact_mode_counts_any_channel_difference() {
        let a = uniform(4, 4, [100, 100, 100]);
        // Differ by one in a single channel.
        let b = frame(4, 4, |x, y| {
            if x == 1 && y == 1 {
                [101, 100, 100]
            } else {
                [100, 100, 100]
            }
        });
        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(result.changed_pixels, 1);

        // And in each other channel independently.
        for channel in 0..3 {
            let mut rgb = [100, 100, 100];
            rgb[channel] = 101;
            let b = frame(4, 4, |x, y| {
                if x == 1 && y == 1 {
                    rgb
                } else {
                    [100, 100, 100]
                }
            });
            assert_eq!(
                compare_frames(&a, &b, &CompareOptions::default())
                    .unwrap()
                    .changed_pixels,
                1,
                "channel {channel} should count"
            );
        }
    }

    // --- 22.5 threshold boundary ----------------------------------------------

    #[test]
    fn the_pixel_threshold_boundary_is_strictly_greater_than() {
        // Every pixel differs by 10 in the red channel.
        let a = uniform(4, 4, [100, 100, 100]);
        let b = uniform(4, 4, [110, 100, 100]);
        let all = 16;

        let below =
            compare_frames(&a, &b, &with_pixel_threshold(CompareMode::RgbThreshold, 9)).unwrap();
        assert_eq!(below.changed_pixels, all, "10 > 9, so every pixel changed");

        let equal =
            compare_frames(&a, &b, &with_pixel_threshold(CompareMode::RgbThreshold, 10)).unwrap();
        assert_eq!(
            equal.changed_pixels, 0,
            "10 > 10 is false, so nothing changed"
        );

        let above =
            compare_frames(&a, &b, &with_pixel_threshold(CompareMode::RgbThreshold, 11)).unwrap();
        assert_eq!(above.changed_pixels, 0, "10 > 11 is false");
    }

    #[test]
    fn a_single_pixel_at_the_boundary_is_handled_per_pixel() {
        // Only one pixel differs, and it differs by exactly the threshold.
        let a = uniform(4, 4, [100, 100, 100]);
        let b = frame(4, 4, |x, y| {
            if x == 2 && y == 2 {
                [112, 100, 100]
            } else {
                [100, 100, 100]
            }
        });

        let at =
            compare_frames(&a, &b, &with_pixel_threshold(CompareMode::RgbThreshold, 12)).unwrap();
        assert_eq!(at.changed_pixels, 0, "12 > 12 is false");
        assert_eq!(at.bounding_box, None);

        let below =
            compare_frames(&a, &b, &with_pixel_threshold(CompareMode::RgbThreshold, 11)).unwrap();
        assert_eq!(below.changed_pixels, 1, "12 > 11 is true");
        assert_eq!(below.bounding_box, Some(Rect::new(2, 2, 1, 1).unwrap()));
    }

    #[test]
    fn threshold_zero_is_equivalent_to_exact_mode() {
        let a = frame(16, 16, |x, y| {
            [(x * 7 % 256) as u8, (y * 11 % 256) as u8, 40]
        });
        let b = frame(16, 16, |x, y| {
            [(x * 7 % 256) as u8, ((y * 11 + 1) % 256) as u8, 40]
        });

        let exact = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        let threshold_zero =
            compare_frames(&a, &b, &with_pixel_threshold(CompareMode::RgbThreshold, 0)).unwrap();

        assert_eq!(exact.changed_pixels, threshold_zero.changed_pixels);
        assert_eq!(exact.bounding_box, threshold_zero.bounding_box);
        assert_eq!(exact.total_pixels, threshold_zero.total_pixels);
    }

    #[test]
    fn the_threshold_uses_the_largest_channel_difference() {
        // Each channel differs by 5, so the largest difference is 5.
        let a = uniform(2, 2, [100, 100, 100]);
        let b = uniform(2, 2, [105, 105, 105]);

        assert_eq!(
            compare_frames(&a, &b, &with_pixel_threshold(CompareMode::RgbThreshold, 4))
                .unwrap()
                .changed_pixels,
            4,
            "5 > 4, so every pixel changed"
        );
        assert_eq!(
            compare_frames(&a, &b, &with_pixel_threshold(CompareMode::RgbThreshold, 5))
                .unwrap()
                .changed_pixels,
            0,
            "5 > 5 is false"
        );
    }

    #[test]
    fn a_negative_signed_difference_is_measured_by_magnitude() {
        // Darker in one direction must behave the same as lighter in the other.
        let bright = uniform(2, 2, [200, 200, 200]);
        let dark = uniform(2, 2, [190, 190, 190]);
        let options = with_pixel_threshold(CompareMode::RgbThreshold, 9);

        let down = compare_frames(&bright, &dark, &options).unwrap();
        let up = compare_frames(&dark, &bright, &options).unwrap();
        assert_eq!(down.changed_pixels, 4);
        assert_eq!(up.changed_pixels, 4);
    }

    // --- 22.6 area threshold boundary -----------------------------------------

    #[test]
    fn the_area_threshold_boundary_is_inclusive() {
        // 10x10 = 100 pixels; make exactly 5 change, so the fraction is 0.05.
        let a = uniform(10, 10, [0, 0, 0]);
        let b = frame(10, 10, |x, y| {
            if y == 0 && x < 5 {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        });

        let base = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(base.changed_pixels, 5);
        assert!((base.changed_fraction - 0.05).abs() < 1e-12);

        let below = CompareOptions {
            area_threshold: 0.0500001,
            ..CompareOptions::default()
        };
        assert!(!compare_frames(&a, &b, &below).unwrap().changed, "below");

        let equal = CompareOptions {
            area_threshold: 0.05,
            ..CompareOptions::default()
        };
        assert!(
            compare_frames(&a, &b, &equal).unwrap().changed,
            "equal must count, because the comparison is >="
        );

        let above = CompareOptions {
            area_threshold: 0.0499999,
            ..CompareOptions::default()
        };
        assert!(compare_frames(&a, &b, &above).unwrap().changed, "above");
    }

    #[test]
    fn an_area_threshold_of_zero_means_any_change_counts() {
        let a = uniform(8, 8, [5, 5, 5]);
        let b = frame(8, 8, |x, y| {
            if x == 7 && y == 7 {
                [6, 5, 5]
            } else {
                [5, 5, 5]
            }
        });
        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert!(result.changed);
    }

    #[test]
    fn identical_frames_are_not_changed_even_with_a_zero_area_threshold() {
        // The degenerate case: `0.0 >= 0.0` would otherwise report a change.
        let a = uniform(8, 8, [5, 5, 5]);
        let options = CompareOptions {
            area_threshold: 0.0,
            ..CompareOptions::default()
        };
        let result = compare_frames(&a, &a, &options).unwrap();
        assert_eq!(result.changed_pixels, 0);
        assert!(
            !result.changed,
            "no pixels changed, so nothing is meaningfully changed"
        );
    }

    #[test]
    fn an_area_threshold_of_one_requires_a_wholly_changed_frame() {
        let a = uniform(10, 10, [0, 0, 0]);
        let all = uniform(10, 10, [1, 1, 1]);
        let almost = frame(10, 10, |x, y| {
            if x == 9 && y == 9 {
                [0, 0, 0]
            } else {
                [1, 1, 1]
            }
        });

        let options = CompareOptions {
            area_threshold: 1.0,
            ..CompareOptions::default()
        };
        assert!(compare_frames(&a, &all, &options).unwrap().changed);
        assert!(!compare_frames(&a, &almost, &options).unwrap().changed);
    }

    // --- 22.7 geometry mismatch ----------------------------------------------

    #[test]
    fn mismatched_width_is_an_incompatible_frames_error() {
        let a = uniform(1920, 1080, [0, 0, 0]);
        let b = uniform(1280, 1080, [0, 0, 0]);
        let error = compare_frames(&a, &b, &CompareOptions::default()).unwrap_err();
        assert_eq!(error.code(), "incompatible_frames");
        assert!(error.message().contains("1920x1080"));
        assert!(error.message().contains("1280x1080"));
    }

    #[test]
    fn mismatched_height_is_an_incompatible_frames_error() {
        let a = uniform(640, 480, [0, 0, 0]);
        let b = uniform(640, 481, [0, 0, 0]);
        let error = compare_frames(&a, &b, &CompareOptions::default()).unwrap_err();
        assert_eq!(error.code(), "incompatible_frames");
    }

    #[test]
    fn mismatched_dimensions_are_never_silently_resized_or_overlapped() {
        // A 10x10 frame painted entirely black against a 20x20 frame painted
        // entirely white would report "changed" under an overlap comparison. It
        // must instead be rejected.
        let a = uniform(10, 10, [0, 0, 0]);
        let b = uniform(20, 20, [255, 255, 255]);
        assert!(compare_frames(&a, &b, &CompareOptions::default()).is_err());
    }

    // --- 22.8 large dimensions ------------------------------------------------

    #[test]
    fn the_arithmetic_survives_full_hd_and_beyond() {
        // 1920x1080: every pixel changed.
        let total = 1920u64 * 1080;
        let result = Comparison::from_counts(
            &CompareOptions::default(),
            total,
            total,
            Some((0, 0, 1919, 1079)),
        )
        .unwrap();
        assert_eq!(result.changed_fraction, 1.0);
        assert!(result.changed);
        assert_eq!(
            result.bounding_box,
            Some(Rect::new(0, 0, 1920, 1080).unwrap())
        );

        // A single changed pixel in a 1920x1080 frame.
        let result = Comparison::from_counts(
            &CompareOptions::default(),
            1,
            total,
            Some((1919, 1079, 1919, 1079)),
        )
        .unwrap();
        assert_eq!(result.changed_pixels, 1);
        assert!(result.changed_fraction > 0.0 && result.changed_fraction < 1e-6);
        assert_eq!(
            result.bounding_box,
            Some(Rect::new(1919, 1079, 1, 1).unwrap())
        );
    }

    #[test]
    fn the_arithmetic_survives_dimensions_at_the_limit_of_the_type() {
        // The largest frame the pixel-count arithmetic can describe without
        // overflowing: u32::MAX x u32::MAX pixels.
        let side = u32::MAX as u64;
        let total = side * side;

        let result = Comparison::from_counts(
            &CompareOptions::default(),
            total,
            total,
            Some((0, 0, u32::MAX - 1, u32::MAX - 1)),
        )
        .unwrap();
        assert_eq!(result.total_pixels, total);
        assert_eq!(result.changed_fraction, 1.0);
        assert_eq!(
            result.bounding_box,
            Some(Rect::new(0, 0, u32::MAX, u32::MAX).unwrap())
        );
    }

    #[test]
    fn impossible_counts_are_rejected_rather_than_producing_a_nonsense_result() {
        let total = 100u64;
        assert!(Comparison::from_counts(&CompareOptions::default(), 0, 0, None).is_err());
        assert!(Comparison::from_counts(&CompareOptions::default(), 101, total, None).is_err());
    }

    #[test]
    fn a_real_1920x1080_scan_counts_every_pixel() {
        // Exercises the scan itself, not just the arithmetic, at a realistic size.
        let a = uniform(1920, 1080, [0, 0, 0]);
        let b = uniform(1920, 1080, [255, 255, 255]);
        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(result.total_pixels, 1920 * 1080);
        assert_eq!(result.changed_pixels, 1920 * 1080);
        assert_eq!(result.changed_fraction, 1.0);
        assert_eq!(
            result.bounding_box,
            Some(Rect::new(0, 0, 1920, 1080).unwrap())
        );
    }

    // --- 22.9 bounding-box correctness ---------------------------------------

    #[test]
    fn bounding_box_handles_a_change_at_the_top_left() {
        let a = uniform(8, 8, [0, 0, 0]);
        let b = frame(8, 8, |x, y| {
            if x == 0 && y == 0 {
                [1, 1, 1]
            } else {
                [0, 0, 0]
            }
        });
        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(result.bounding_box, Some(Rect::new(0, 0, 1, 1).unwrap()));
    }

    #[test]
    fn bounding_box_handles_a_change_at_the_bottom_right() {
        let a = uniform(8, 8, [0, 0, 0]);
        let b = frame(8, 8, |x, y| {
            if x == 7 && y == 7 {
                [1, 1, 1]
            } else {
                [0, 0, 0]
            }
        });
        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(result.bounding_box, Some(Rect::new(7, 7, 1, 1).unwrap()));
    }

    #[test]
    fn bounding_box_spans_sparse_distant_changes() {
        let a = uniform(100, 50, [0, 0, 0]);
        let b = frame(100, 50, |x, y| {
            let changed = (x == 1 && y == 2) || (x == 98 && y == 47);
            if changed {
                [9, 9, 9]
            } else {
                [0, 0, 0]
            }
        });
        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(result.changed_pixels, 2);
        // x: 1..=98 (98 wide), y: 2..=47 (46 tall).
        assert_eq!(result.bounding_box, Some(Rect::new(1, 2, 98, 46).unwrap()));
    }

    #[test]
    fn bounding_box_handles_a_single_row() {
        let a = uniform(10, 10, [0, 0, 0]);
        let b = frame(10, 10, |_, y| if y == 4 { [3, 3, 3] } else { [0, 0, 0] });
        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(result.bounding_box, Some(Rect::new(0, 4, 10, 1).unwrap()));
    }

    #[test]
    fn bounding_box_handles_a_single_column() {
        let a = uniform(10, 10, [0, 0, 0]);
        let b = frame(10, 10, |x, _| if x == 6 { [3, 3, 3] } else { [0, 0, 0] });
        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(result.bounding_box, Some(Rect::new(6, 0, 1, 10).unwrap()));
    }

    #[test]
    fn bounding_box_covers_the_whole_frame_when_everything_changes() {
        let a = uniform(6, 4, [0, 0, 0]);
        let b = uniform(6, 4, [255, 255, 255]);
        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        assert_eq!(result.bounding_box, Some(Rect::new(0, 0, 6, 4).unwrap()));
    }

    // --- 22.10 determinism ---------------------------------------------------

    #[test]
    fn repeated_comparisons_produce_identical_results() {
        let a = frame(64, 64, |x, y| {
            [(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8]
        });
        let b = frame(64, 64, |x, y| {
            if (x / 8 + y / 8) % 3 == 0 {
                [((x * 3) % 256) as u8, 0, 255]
            } else {
                [(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8]
            }
        });
        let options = CompareOptions {
            mode: CompareMode::RgbThreshold,
            pixel_threshold: 7,
            area_threshold: 0.01,
        };

        let first = compare_frames(&a, &b, &options).unwrap();
        for _ in 0..8 {
            assert_eq!(compare_frames(&a, &b, &options).unwrap(), first);
        }
    }

    #[test]
    fn results_do_not_depend_on_argument_order_in_a_meaningful_way() {
        // The metrics are symmetric: swapping the frames changes nothing about
        // the counts or the bounding box.
        let a = frame(32, 32, |x, y| [(x % 256) as u8, (y % 256) as u8, 0]);
        let b = frame(32, 32, |x, y| {
            if x < 8 {
                [255, 255, 255]
            } else {
                [(x % 256) as u8, (y % 256) as u8, 0]
            }
        });
        let forward = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        let backward = compare_frames(&b, &a, &CompareOptions::default()).unwrap();
        assert_eq!(forward.changed_pixels, backward.changed_pixels);
        assert_eq!(forward.bounding_box, backward.bounding_box);
        assert_eq!(forward.changed_fraction, backward.changed_fraction);
    }

    // --- options validation --------------------------------------------------

    #[test]
    fn the_options_are_echoed_back_in_the_result() {
        let a = uniform(4, 4, [0, 0, 0]);
        let b = uniform(4, 4, [0, 0, 0]);
        let options = CompareOptions {
            mode: CompareMode::RgbThreshold,
            pixel_threshold: 12,
            area_threshold: 0.005,
        };
        let result = compare_frames(&a, &b, &options).unwrap();
        assert_eq!(result.mode, CompareMode::RgbThreshold);
        assert_eq!(result.pixel_threshold, 12);
        assert_eq!(result.area_threshold, 0.005);
    }

    #[test]
    fn exact_mode_is_reported_with_a_threshold_of_zero() {
        let a = uniform(4, 4, [0, 0, 0]);
        let options = CompareOptions {
            pixel_threshold: 99,
            ..CompareOptions::default()
        };
        let result = compare_frames(&a, &a, &options).unwrap();
        assert_eq!(result.mode, CompareMode::Exact);
        assert_eq!(
            result.pixel_threshold, 0,
            "exact mode ignores the threshold field"
        );
    }

    #[test]
    fn out_of_range_area_thresholds_are_rejected() {
        let a = uniform(4, 4, [0, 0, 0]);
        for bad in [-0.1, 1.0001, f64::NAN, f64::INFINITY] {
            let options = CompareOptions {
                area_threshold: bad,
                ..CompareOptions::default()
            };
            let error = compare_frames(&a, &a, &options).unwrap_err();
            assert_eq!(
                error.code(),
                "invalid_arguments",
                "threshold {bad} should fail"
            );
        }
        for good in [0.0, 0.5, 1.0] {
            let options = CompareOptions {
                area_threshold: good,
                ..CompareOptions::default()
            };
            assert!(compare_frames(&a, &a, &options).is_ok());
        }
    }

    #[test]
    fn mode_names_parse_and_render() {
        assert_eq!(CompareMode::from_name("exact").unwrap(), CompareMode::Exact);
        assert_eq!(
            CompareMode::from_name("rgb").unwrap(),
            CompareMode::RgbThreshold
        );
        assert_eq!(
            CompareMode::from_name("RGB_THRESHOLD").unwrap(),
            CompareMode::RgbThreshold
        );
        assert!(CompareMode::from_name("perceptual").is_err());
        assert_eq!(CompareMode::Exact.name(), "exact");
        assert_eq!(CompareMode::RgbThreshold.name(), "rgb_threshold");
    }

    #[test]
    fn the_default_options_report_every_difference() {
        let options = CompareOptions::default();
        assert_eq!(options.mode, CompareMode::Exact);
        assert_eq!(options.pixel_threshold, 0);
        assert_eq!(options.area_threshold, 0.0);
        assert_eq!(options.effective_pixel_threshold(), 0);
    }

    #[test]
    fn the_result_serializes_with_the_documented_field_names() {
        let a = uniform(100, 10, [0, 0, 0]);
        let b = frame(100, 10, |x, y| {
            if x == 5 && y == 2 {
                [255, 0, 0]
            } else {
                [0, 0, 0]
            }
        });
        let result = compare_frames(&a, &b, &CompareOptions::default()).unwrap();
        let value = serde_json::to_value(result).unwrap();

        assert_eq!(value["mode"], "exact");
        assert_eq!(value["changed"], true);
        assert_eq!(value["changed_pixels"], 1);
        assert_eq!(value["total_pixels"], 1000);
        assert_eq!(value["bounding_box"]["x"], 5);
        assert_eq!(value["bounding_box"]["y"], 2);
        assert_eq!(value["bounding_box"]["width"], 1);
        assert_eq!(value["bounding_box"]["height"], 1);
        assert!(value["changed_fraction"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn a_null_bounding_box_is_serialized_as_null() {
        let value = serde_json::to_value(
            compare_frames(
                &uniform(4, 4, [1, 2, 3]),
                &uniform(4, 4, [1, 2, 3]),
                &CompareOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(value["bounding_box"].is_null());
    }
}
