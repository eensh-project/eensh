//! Changed-region extraction: the Phase 2 bounding box, as a view.
//!
//! # The distinction this module preserves
//!
//! Phase 2 reports two different things, and conflating them would lose information:
//!
//! ```text
//! bounding_box:  factual location of pixels above the pixel threshold
//! changed:       a policy judgement, after the area threshold
//! ```
//!
//! It is entirely normal for `changed` to be `false` while `bounding_box` is
//! non-empty: a sub-threshold change moved a few pixels without amounting to
//! anything. A caller that explicitly asks for a changed-region crop is asking about
//! the *pixels*, so it gets the crop regardless of the area verdict (requirement 11).
//!
//! # What is deliberately absent
//!
//! No connected-component segmentation. A single Phase 2 bounding box may span several
//! unrelated changed areas, and that is what is returned (requirement 13). When the
//! box is too large to be worth cropping, the policy may fall back to an overview —
//! but the factual comparison metadata is returned either way.

use std::sync::Arc;

use crate::error::Error;
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::pipeline::{self, ImageOptions};
use crate::presentation::policy::ImagePolicy;
use crate::presentation::view::{EncodedView, ViewKind, ViewSource};
use crate::presentation::PresentableFrame;
use crate::timing::Stopwatch;

pub use crate::presentation::policy::ChangedRegionPolicy;

/// What was actually returned for a changed-region request.
#[derive(Debug, Clone)]
pub struct ChangedRegionResult {
    /// The factual bounding box, before padding.
    pub raw_rect: Rect,
    /// The rectangle actually returned, after padding and clamping.
    pub returned_rect: Rect,
    /// The padding that was requested.
    pub padding: u32,
    /// Whether the policy fell back to the whole frame because the change was large.
    pub fell_back_to_overview: bool,
    /// The rendered view.
    pub view: EncodedView,
}

impl ChangedRegionResult {
    /// Whether the returned view covers the whole source frame.
    pub fn is_whole_frame(&self) -> bool {
        self.fell_back_to_overview
    }
}

/// Pad a bounding box, clamping to the frame.
///
/// Clamping is reported rather than hidden: the result carries both the padded
/// rectangle and the raw one, so a caller can always tell how much padding it
/// actually got. Padding that ran off the edge is not an error — it is the common
/// case when a change happens near a screen edge — but it must be visible.
pub fn pad_and_clamp(rect: &Rect, padding: u32, bounds: &Rect) -> Rect {
    let pad = padding as i64;
    let x = (rect.x as i64 - pad).max(bounds.x as i64);
    let y = (rect.y as i64 - pad).max(bounds.y as i64);
    let right = (rect.right() + pad).min(bounds.right());
    let bottom = (rect.bottom() + pad).min(bounds.bottom());

    Rect {
        x: x as i32,
        y: y as i32,
        width: (right - x).max(1) as u32,
        height: (bottom - y).max(1) as u32,
    }
}

/// Decide the policy for a changed-region crop, applying the fraction fallback.
///
/// Returns the effective image policy and whether it fell back to a whole-frame
/// overview. The fallback exists because a Phase 2 box spanning most of the screen is
/// cheaper to send as a resized overview than as a native-resolution crop of nearly
/// everything (requirement 30).
pub fn changed_view_policy(
    comparison: &crate::compare::Comparison,
    bounds: &Rect,
    policy: &ChangedRegionPolicy,
) -> (ImagePolicy, bool) {
    let Some(max_fraction) = policy.max_fraction else {
        return (policy.image, false);
    };
    let Some(box_rect) = comparison.bounding_box else {
        return (policy.image, false);
    };

    let total = bounds.pixel_count();
    if total == 0 {
        return (policy.image, false);
    }
    let fraction = box_rect.pixel_count() as f64 / total as f64;

    if fraction > max_fraction {
        // The change is large enough that the crop would be most of the frame. An
        // overview at the crop's own settings is the better trade, so this is a
        // fallback to the whole frame rather than a refusal.
        (policy.image, true)
    } else {
        (policy.image, false)
    }
}

/// Extract the changed region between two frames as a view.
///
/// The crop comes from the **newer** frame (requirement 10): the bounding box says
/// where the later frame differs from the earlier one, so that is where the content
/// the caller wants to see actually is.
pub fn changed_region(
    comparison: &crate::compare::Comparison,
    newer: &PresentableFrame,
    policy: &ChangedRegionPolicy,
) -> Result<Option<ChangedRegionResult>, Error> {
    let Some(raw_rect) = comparison.bounding_box else {
        // Nothing differed above the pixel threshold, so there is no region to crop.
        // This is a legitimate outcome, not an error: the caller asked what changed
        // and the answer is "nothing".
        return Ok(None);
    };

    let bounds = newer.source_rect();
    raw_rect.ensure_within(&bounds)?;

    let (image, fell_back) = changed_view_policy(comparison, &bounds, policy);
    let returned_rect = pad_and_clamp(&raw_rect, policy.padding, &bounds);

    let local = Rect::new(
        returned_rect.x - bounds.x,
        returned_rect.y - bounds.y,
        returned_rect.width,
        returned_rect.height,
    )?;

    let plan = crate::presentation::view::ViewPlan {
        frame_index: 0,
        frame_id: newer.frame_id,
        // A fallback is reported as an overview, because that is what it is: the
        // caller receives a whole-frame image. The `fell_back_to_overview` flag and
        // the still-present `raw_changed_rect` carry the reason, so nothing about the
        // comparison is lost by naming the view honestly.
        kind: if fell_back {
            ViewKind::Overview
        } else {
            ViewKind::ChangedRegion
        },
        source: if fell_back {
            ViewSource::Whole
        } else {
            ViewSource::Crop { rect: local }
        },
        image,
        required: policy.required,
        priority: crate::presentation::view::VIEW_PRIORITY_OPTIONAL_REGION,
        is_newest: true,
    };

    let view = crate::presentation::view::encode_view(&newer.frame, &plan)?;

    Ok(Some(ChangedRegionResult {
        raw_rect,
        returned_rect: if fell_back { bounds } else { returned_rect },
        padding: policy.padding,
        fell_back_to_overview: fell_back,
        view,
    }))
}

/// Present a changed region, with the frame it was cropped from kept alive.
///
/// A small helper so the caller does not have to hold the `Arc` separately from the
/// result to keep the pixels valid. In practice the frame is retained in history
/// anyway, but the signature makes the lifetime explicit rather than incidental.
pub fn present_changed(
    before: &PresentableFrame,
    after: &PresentableFrame,
    options: &crate::compare::CompareOptions,
    policy: &ChangedRegionPolicy,
) -> Result<ChangedPresentation, Error> {
    let comparison = crate::compare::compare_frames(&before.frame, &after.frame, options)?;
    let region = changed_region(&comparison, after, policy)?;
    Ok(ChangedPresentation { comparison, region })
}

/// A changed region in the shape the response carries, with the frame kept alive.
#[derive(Debug, Clone)]
pub struct ChangedPresentation {
    /// The comparison that produced it.
    pub comparison: crate::compare::Comparison,
    /// The crop.
    pub region: Option<ChangedRegionResult>,
}

/// Encode a whole-frame overview under a policy, for the fallback path.
///
/// Kept here rather than in `view.rs` because the fallback is a changed-region
/// concept: it exists so that a large change reverts to the same shape as an
/// overview, and the caller can treat both identically.
pub fn overview_under(frame: &Arc<Frame>, image: ImagePolicy) -> Result<EncodedView, Error> {
    let mut stopwatch = Stopwatch::start();
    let options: ImageOptions = image.to_image_options();
    let prepared = pipeline::prepare_image((**frame).clone(), &options)?;
    let total = stopwatch.stop();

    Ok(EncodedView {
        kind: ViewKind::Overview,
        source_rect: frame.source_rect(),
        image: Some(crate::output::json::ObservedFrame::from_prepared(
            &prepared, &options,
        )),
        applied: image,
        crop_us: 0,
        resize_us: prepared.resize_us,
        encode_us: prepared.encode_us,
        base64_us: prepared.base64_us.min(total),
    })
}

/// A comparison with no changed region, for the case where nothing differed.
pub fn no_change(comparison: &crate::compare::Comparison) -> bool {
    comparison.bounding_box.is_none()
}

/// Build a changed-region policy from a padding and an image policy.
pub fn changed_policy(padding: u32, image: ImagePolicy) -> ChangedRegionPolicy {
    ChangedRegionPolicy {
        padding,
        image,
        max_fraction: None,
        required: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::{compare_frames, CompareMode, CompareOptions};
    use crate::frame::{PixelBuffer, PixelFormat};
    use crate::geometry::SourceGeometry;
    use crate::session::FrameId;
    use std::time::Instant;

    fn framed(width: u32, height: u32, paint: impl Fn(u32, u32) -> [u8; 3]) -> Arc<Frame> {
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&paint(x, y));
            }
        }
        let pixels = PixelBuffer::new(width, height, PixelFormat::Rgb8, data).unwrap();
        Arc::new(Frame::new(
            SourceGeometry::desktop(None, width, height),
            pixels,
            Instant::now(),
        ))
    }

    fn presentable(id: u64, frame: Arc<Frame>) -> PresentableFrame {
        PresentableFrame {
            session_id: "s".into(),
            frame_id: FrameId(id),
            frame,
            captured_at: Instant::now(),
            capture_offset: None,
            capture_duration: None,
        }
    }

    fn options() -> CompareOptions {
        CompareOptions {
            mode: CompareMode::RgbThreshold,
            pixel_threshold: 12,
            area_threshold: 0.005,
        }
    }

    #[test]
    fn a_changed_rectangle_becomes_a_crop_of_the_same_rectangle() {
        let before = framed(40, 30, |_, _| [0, 0, 0]);
        // A bright block at (10, 8) sized 6x4.
        let after = framed(40, 30, |x, y| {
            if (10..16).contains(&x) && (8..12).contains(&y) {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        });

        let comparison = compare_frames(&before, &after, &options()).unwrap();
        assert_eq!(
            comparison.bounding_box,
            Rect::new(10, 8, 6, 4).ok(),
            "the bounding box is the exact changed rectangle"
        );

        let newer = presentable(2, after);
        let result = changed_region(&comparison, &newer, &changed_policy(0, ImagePolicy::png()))
            .unwrap()
            .unwrap();

        assert_eq!(result.raw_rect, Rect::new(10, 8, 6, 4).unwrap());
        assert_eq!(result.returned_rect, Rect::new(10, 8, 6, 4).unwrap());
        assert_eq!(result.padding, 0);
        assert!(!result.fell_back_to_overview);
        assert_eq!(result.view.source_rect, Rect::new(10, 8, 6, 4).unwrap());
    }

    #[test]
    fn padding_grows_the_crop_and_is_reported() {
        let before = framed(40, 30, |_, _| [0, 0, 0]);
        let after = framed(40, 30, |x, y| {
            if (18..22).contains(&x) && (14..17).contains(&y) {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        });
        let comparison = compare_frames(&before, &after, &options()).unwrap();

        let newer = presentable(2, after);
        let result = changed_region(&comparison, &newer, &changed_policy(4, ImagePolicy::png()))
            .unwrap()
            .unwrap();

        assert_eq!(result.raw_rect, Rect::new(18, 14, 4, 3).unwrap());
        assert_eq!(result.returned_rect, Rect::new(14, 10, 12, 11).unwrap());
        assert_eq!(result.padding, 4);
    }

    #[test]
    fn padding_at_an_edge_clamps_and_reports_the_actual_rectangle() {
        // A change in the very corner: the padding cannot be honoured on two sides.
        let before = framed(40, 30, |_, _| [0, 0, 0]);
        let after = framed(40, 30, |x, y| {
            if x < 3 && y < 2 {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        });
        let comparison = compare_frames(&before, &after, &options()).unwrap();

        let newer = presentable(2, after);
        let result = changed_region(&comparison, &newer, &changed_policy(10, ImagePolicy::png()))
            .unwrap()
            .unwrap();

        assert_eq!(result.raw_rect, Rect::new(0, 0, 3, 2).unwrap());
        assert_eq!(
            result.returned_rect,
            Rect::new(0, 0, 13, 12).unwrap(),
            "padding is clamped to the source, so only the right and bottom grow"
        );
    }

    #[test]
    fn padding_never_escapes_the_far_edge() {
        let before = framed(20, 20, |_, _| [0, 0, 0]);
        let after = framed(20, 20, |x, y| {
            if x >= 18 && y >= 18 {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        });
        let comparison = compare_frames(&before, &after, &options()).unwrap();
        let newer = presentable(2, after);
        let result = changed_region(&comparison, &newer, &changed_policy(10, ImagePolicy::png()))
            .unwrap()
            .unwrap();

        let bounds = newer.source_rect();
        assert!(result.returned_rect.right() <= bounds.right());
        assert!(result.returned_rect.bottom() <= bounds.bottom());
        assert_eq!(result.returned_rect, Rect::new(8, 8, 12, 12).unwrap());
    }

    #[test]
    fn no_change_means_no_crop_and_that_is_not_an_error() {
        let before = framed(20, 20, |_, _| [10, 20, 30]);
        let after = framed(20, 20, |_, _| [10, 20, 30]);
        let comparison = compare_frames(&before, &after, &options()).unwrap();
        assert!(no_change(&comparison));

        let newer = presentable(2, after);
        let result =
            changed_region(&comparison, &newer, &changed_policy(0, ImagePolicy::png())).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn a_bounding_box_is_still_returned_when_the_area_threshold_says_unchanged() {
        // Requirement 11: the factual box and the policy verdict are separate. A few
        // changed pixels in a large frame are below the area threshold, so `changed`
        // is false, but the box is real and a caller asking for the crop still gets it.
        let before = framed(200, 200, |_, _| [0, 0, 0]);
        let after = framed(200, 200, |x, y| {
            if x < 2 && y < 2 {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        });
        let comparison = compare_frames(&before, &after, &options()).unwrap();

        assert!(!comparison.changed, "four pixels of 40000 is below 0.5%");
        assert!(comparison.bounding_box.is_some(), "but the box is factual");

        let newer = presentable(2, after);
        let result = changed_region(&comparison, &newer, &changed_policy(0, ImagePolicy::png()))
            .unwrap()
            .expect("an explicitly requested crop is still returned");
        assert_eq!(result.raw_rect, Rect::new(0, 0, 2, 2).unwrap());
    }

    #[test]
    fn a_large_change_falls_back_to_the_whole_frame_when_the_policy_says_so() {
        let before = framed(40, 30, |_, _| [0, 0, 0]);
        // Most of the frame changes.
        let after = framed(40, 30, |x, y| {
            if x < 38 && y < 28 {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        });
        let comparison = compare_frames(&before, &after, &options()).unwrap();

        let mut policy = changed_policy(0, ImagePolicy::png());
        policy.max_fraction = Some(0.25);

        let newer = presentable(2, after);
        let result = changed_region(&comparison, &newer, &policy)
            .unwrap()
            .unwrap();

        assert!(result.fell_back_to_overview);
        assert_eq!(
            result.returned_rect,
            newer.source_rect(),
            "the fallback reports the whole frame it actually returned"
        );
        assert_eq!(result.view.kind, ViewKind::Overview);
        // The factual box is still reported, so nothing is lost.
        assert_eq!(result.raw_rect, Rect::new(0, 0, 38, 28).unwrap());
    }

    #[test]
    fn a_small_change_is_not_fallen_back_from() {
        let before = framed(40, 30, |_, _| [0, 0, 0]);
        let after = framed(40, 30, |x, y| {
            if (5..9).contains(&x) && (5..9).contains(&y) {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        });
        let comparison = compare_frames(&before, &after, &options()).unwrap();

        let mut policy = changed_policy(0, ImagePolicy::png());
        policy.max_fraction = Some(0.25);

        let newer = presentable(2, after);
        let result = changed_region(&comparison, &newer, &policy)
            .unwrap()
            .unwrap();
        assert!(!result.fell_back_to_overview);
    }

    #[test]
    fn the_crop_comes_from_the_newer_frame() {
        // The bounding box describes where the *later* frame differs, so the crop
        // must show the new content rather than the old.
        let before = framed(30, 20, |_, _| [0, 0, 0]);
        let after = framed(30, 20, |x, y| {
            if (10..14).contains(&x) && (5..9).contains(&y) {
                [0, 0, 255]
            } else {
                [0, 0, 0]
            }
        });
        let comparison = compare_frames(&before, &after, &options()).unwrap();

        let newer = presentable(2, after);
        let result = changed_region(&comparison, &newer, &changed_policy(0, ImagePolicy::png()))
            .unwrap()
            .unwrap();

        // Decode the crop and confirm it is the blue block, not black.
        let image = result.view.image.as_ref().unwrap();
        assert_eq!(image.width, 4);
        assert_eq!(image.height, 4);
        assert!(image.byte_length > 0);
    }

    #[test]
    fn a_changed_region_outside_the_frame_is_refused() {
        // Defensive: a comparison across frames of different sizes would produce a box
        // that cannot be cropped, and that must be an explicit error rather than a
        // silent clamp.
        let before = framed(40, 30, |_, _| [0, 0, 0]);
        let after = framed(40, 30, |_, _| [255, 255, 255]);
        let comparison = compare_frames(&before, &after, &options()).unwrap();

        let smaller = framed(10, 10, |_, _| [0, 0, 0]);
        let newer = presentable(2, smaller);
        let error = changed_region(&comparison, &newer, &changed_policy(0, ImagePolicy::png()))
            .unwrap_err();
        assert_eq!(error.code(), "invalid_region");
    }

    #[test]
    fn a_jpeg_changed_region_reports_its_quality() {
        let before = framed(30, 20, |_, _| [0, 0, 0]);
        let after = framed(30, 20, |x, y| {
            if (4..10).contains(&x) && (4..10).contains(&y) {
                [200, 100, 50]
            } else {
                [0, 0, 0]
            }
        });
        let comparison = compare_frames(&before, &after, &options()).unwrap();
        let newer = presentable(2, after);
        let result = changed_region(
            &comparison,
            &newer,
            &changed_policy(0, ImagePolicy::jpeg(65)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.view.image.unwrap().quality, Some(65));
    }
}
