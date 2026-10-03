//! View extraction: turning a raw frame plus a policy into a list of concrete views.
//!
//! This module answers *what* to render. The budget module answers *how much* it may
//! cost. Keeping the two apart is what makes the fitting loop in `budget.rs`
//! comprehensible: it re-plans dimensions and quality, and never has to reason about
//! what a region is or where a crop came from.

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::output::json::ObservedFrame;
use crate::pipeline::{self, ImageOptions};
use crate::presentation::policy::{ImagePolicy, ObservationPolicy, RegionScope};
use crate::presentation::PresentableFrame;
use crate::timing::Stopwatch;

/// Priority assigned to a required region.
pub const VIEW_PRIORITY_REQUIRED_REGION: u8 = 200;
/// Priority assigned to the newest overview.
pub const VIEW_PRIORITY_NEWEST: u8 = 220;
/// Priority assigned to an optional region on the newest frame.
pub const VIEW_PRIORITY_OPTIONAL_REGION: u8 = 120;
/// Priority assigned to the first older temporal frame.
pub const VIEW_PRIORITY_OLD: u8 = 60;
/// Priority assigned to the oldest temporal frames.
pub const VIEW_PRIORITY_OLDEST: u8 = 40;

/// What a view is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ViewKind {
    /// The whole source frame, transformed.
    Overview,
    /// A named source-space region.
    Region {
        /// The region's caller-supplied name.
        name: String,
    },
    /// The Phase 2 bounding box of a change, optionally padded.
    ChangedRegion,
}

impl ViewKind {
    /// The name used in a response.
    pub fn name(&self) -> &str {
        match self {
            ViewKind::Overview => "overview",
            ViewKind::Region { name } => name,
            ViewKind::ChangedRegion => "changed",
        }
    }
}

/// What a view is rendered from.
///
/// A view is either the whole frame or a crop of it, expressed in *frame-local* pixel
/// coordinates. The conversion from source coordinates happens at planning time, so
/// the encoder never has to know where the frame came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewSource {
    /// The entire frame.
    Whole,
    /// A crop, in frame-local pixels.
    Crop {
        /// The rectangle, relative to the frame's own top-left corner.
        rect: Rect,
    },
}

impl ViewSource {
    /// The source-space rectangle this view covers, given the frame it came from.
    ///
    /// This is what a response reports, and it is what a transform is derived from:
    /// the returned image's pixels map back through *this* rectangle, not through the
    /// frame's, so a crop reports where it actually came from.
    pub fn source_rect(&self, frame: &PresentableFrame) -> Rect {
        match self {
            ViewSource::Whole => frame.source_rect(),
            ViewSource::Crop { rect } => Rect {
                x: frame.source_rect().x + rect.x,
                y: frame.source_rect().y + rect.y,
                width: rect.width,
                height: rect.height,
            },
        }
    }
}

/// One view to render: which frame, which pixels, and how.
#[derive(Debug, Clone)]
pub struct ViewPlan {
    /// Which frame of the presentation this view comes from, as an index into the
    /// frames passed to the planner.
    pub frame_index: usize,
    /// The frame's identity, carried so the fitting loop can report adjustments by
    /// frame without re-deriving the index.
    pub frame_id: crate::session::FrameId,
    /// What kind of view this is.
    pub kind: ViewKind,
    /// Which pixels of the frame it covers.
    pub source: ViewSource,
    /// How to render it.
    pub image: ImagePolicy,
    /// Whether a budget fitter may drop the view.
    pub required: bool,
    /// How long the view is protected during fitting.
    pub priority: u8,
    /// Whether the frame this view belongs to is the newest one.
    pub is_newest: bool,
}

impl ViewPlan {
    /// The source-space rectangle, given the frame it was planned against.
    pub fn source_rect(&self, frames: &[PresentableFrame]) -> Rect {
        self.source.source_rect(&frames[self.frame_index])
    }

    /// Whether this view carries an image at all.
    pub fn has_image(&self) -> bool {
        !self.image.is_metadata_only()
    }
}

/// A complete rendering plan: every view to produce, in emission order.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The views, grouped by frame in the order the frames were supplied.
    pub views: Vec<ViewPlan>,
}

impl Plan {
    /// Views belonging to `frame_index`.
    pub fn views_for(&self, frame_index: usize) -> impl Iterator<Item = &ViewPlan> {
        self.views
            .iter()
            .filter(move |v| v.frame_index == frame_index)
    }

    /// Views that carry an image, in plan order.
    pub fn image_views(&self) -> impl Iterator<Item = &ViewPlan> {
        self.views.iter().filter(|v| v.has_image())
    }

    /// Total views.
    pub fn len(&self) -> usize {
        self.views.len()
    }

    /// Whether the plan is empty.
    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    /// The priority of the most protected *required* view.
    pub fn highest_required_priority(&self) -> u8 {
        self.views
            .iter()
            .filter(|v| v.required)
            .map(|v| v.priority)
            .max()
            .unwrap_or(0)
    }
}

/// Plan a single frame's presentation.
pub fn plan_for_frame(frame: &PresentableFrame, policy: &ObservationPolicy) -> Result<Plan, Error> {
    plan(frame_indexed(std::slice::from_ref(frame)), policy)
}

/// Plan a temporal stack's presentation, oldest first.
///
/// Temporal *order* is the frame's position in the slice, never its identifier: a
/// stack may be interleaved with independent captures and so have non-contiguous IDs
/// (requirement 49). Ordering by ID would then silently mis-assign "older" and
/// "newest", which is the one distinction a `NewestDetailed` policy depends on.
pub fn plan_for_stack(
    frames: &[PresentableFrame],
    policy: &ObservationPolicy,
) -> Result<Plan, Error> {
    plan(frames.iter().enumerate().collect::<Vec<_>>(), policy)
}

/// A frame paired with its index in the presentation.
///
/// A single-frame presentation passes index 0, which is also the index of the newest
/// frame, so a temporal policy degenerates correctly rather than needing a special
/// case at every call site.
fn frame_indexed(frames: &[PresentableFrame]) -> Vec<(usize, &PresentableFrame)> {
    frames.iter().enumerate().collect()
}

fn plan(
    frames: Vec<(usize, &PresentableFrame)>,
    policy: &ObservationPolicy,
) -> Result<Plan, Error> {
    policy.validate()?;
    let total = frames.len();
    let mut views = Vec::new();

    for (index, frame) in &frames {
        let index = *index;
        let frame = *frame;
        let is_newest = index + 1 == total;

        // The overview, when the policy asks for one. Its image policy comes from the
        // temporal policy if there is one, so `NewestDetailed` controls the overview
        // without the caller having to describe the overview twice.
        if let Some(overview) = policy.overview {
            let image = match &policy.temporal {
                Some(temporal) => temporal.image_for(index, total),
                None => overview,
            };
            // Every frame's overview is *required*. A temporal policy deliberately
            // lowers what the older frames are rendered at; it does not say they are
            // expendable, and a caller that asked for three frames asked for three.
            // Marking them optional would let the budget fitter silently return a
            // one-frame stack, which would look like a complete answer. Older frames
            // can still be reduced all the way to their floors, and a caller who
            // genuinely wants them droppable can ask for `newest-only` or
            // `metadata-older`, which say so explicitly.
            views.push(ViewPlan {
                frame_index: index,
                frame_id: frame.frame_id,
                kind: ViewKind::Overview,
                source: ViewSource::Whole,
                image,
                required: true,
                priority: if is_newest {
                    VIEW_PRIORITY_NEWEST
                } else {
                    older_priority(index, total)
                },
                is_newest,
            });
        }

        // Named regions, cropped from this frame at the same source coordinates for
        // every frame they apply to.
        for region in &policy.regions {
            let applies = match region.scope {
                RegionScope::All => true,
                RegionScope::Newest => is_newest,
                RegionScope::Older => !is_newest,
            };
            if !applies {
                continue;
            }

            let local = to_local(frame, &region.rect)?;
            views.push(ViewPlan {
                frame_index: index,
                frame_id: frame.frame_id,
                kind: ViewKind::Region {
                    name: region.name.clone(),
                },
                source: ViewSource::Crop { rect: local },
                image: region.image,
                required: region.required,
                // The declared priority is honoured, so a caller can order its own
                // regions. A *required* region is floored at the required-region
                // priority, so a caller cannot accidentally let one be degraded below
                // an optional one — the requirement is the stronger statement of the
                // two, and it wins.
                priority: if region.required {
                    region.priority.max(VIEW_PRIORITY_REQUIRED_REGION)
                } else {
                    region.priority
                },
                is_newest,
            });
        }
    }

    // The changed view is *not* planned here. It needs a Phase 2 comparison, and
    // this module deliberately knows nothing about comparisons: keeping it out means
    // the planner stays a pure function of a policy and a frame, and the changed
    // path enters through `changed.rs` with the comparison it actually needs.
    Ok(Plan { views })
}

fn older_priority(index: usize, total: usize) -> u8 {
    // The frame immediately before the newest is the most valuable context, the
    // oldest the least. Expressed as a distance so a long stack degrades gracefully.
    let distance = (total - 1).saturating_sub(index + 1);
    match distance {
        0 => VIEW_PRIORITY_OLD,
        _ => VIEW_PRIORITY_OLDEST,
    }
}

/// Convert a source-space rectangle into frame-local pixels.
///
/// Regions are declared in source coordinates (requirement 33) and cropped from
/// frames that may themselves have a non-zero source origin, so the conversion is a
/// subtraction rather than an identity. It also validates that the region lies inside
/// the frame, which is where an out-of-bounds region is caught: refusing it here
/// means no capture is wasted and the error names the policy, not the pixel buffer.
fn to_local(frame: &PresentableFrame, rect: &Rect) -> Result<Rect, Error> {
    let frame_rect = frame.source_rect();
    rect.ensure_within(&frame_rect)?;
    Rect::new(
        rect.x - frame_rect.x,
        rect.y - frame_rect.y,
        rect.width,
        rect.height,
    )
}

/// A view that has been cropped, resized, encoded, and optionally base64 encoded.
#[derive(Debug, Clone)]
pub struct EncodedView {
    /// What kind of view this is.
    pub kind: ViewKind,
    /// The source-space rectangle it came from.
    pub source_rect: Rect,
    /// The encoded image, or `None` for a metadata-only view.
    pub image: Option<ObservedFrame>,
    /// The policy actually applied.
    pub applied: ImagePolicy,
    /// Crop duration in microseconds.
    pub crop_us: u64,
    /// Resize duration in microseconds.
    pub resize_us: u64,
    /// Encode duration in microseconds.
    pub encode_us: u64,
    /// Base64 duration in microseconds.
    pub base64_us: u64,
}

impl EncodedView {
    /// Convert into the shape the response carries.
    pub fn into_presented(self) -> crate::presentation::PresentedView {
        crate::presentation::PresentedView {
            kind: self.kind,
            source_rect: self.source_rect,
            image: self.image,
            applied: self.applied,
        }
    }
}

/// Render one planned view: crop, resize, encode, and optionally base64 encode.
///
/// A metadata-only view short-circuits before any pixels are touched, which is what
/// makes omitting an older frame's image actually free rather than merely cheap.
pub fn encode_view(frame: &Frame, plan: &ViewPlan) -> Result<EncodedView, Error> {
    let local = match &plan.source {
        ViewSource::Whole => None,
        ViewSource::Crop { rect } => Some(*rect),
    };

    // The source rectangle is computed from the frame's own geometry so the response
    // reports where the crop came from rather than where it landed.
    let source_rect = match local {
        None => frame.source_rect(),
        Some(rect) => Rect {
            x: frame.source_geometry.x + rect.x,
            y: frame.source_geometry.y + rect.y,
            width: rect.width,
            height: rect.height,
        },
    };

    if plan.image.is_metadata_only() {
        return Ok(EncodedView {
            kind: plan.kind.clone(),
            source_rect,
            image: None,
            applied: plan.image,
            crop_us: 0,
            resize_us: 0,
            encode_us: 0,
            base64_us: 0,
        });
    }

    let mut stopwatch = Stopwatch::start();
    let cropped = match local {
        None => frame.clone(),
        Some(rect) => frame.crop(&rect)?,
    };
    let crop_us = stopwatch.stop();

    let options: ImageOptions = plan.image.to_image_options();
    let prepared = pipeline::prepare_image(cropped, &options)?;

    Ok(EncodedView {
        kind: plan.kind.clone(),
        source_rect,
        image: Some(ObservedFrame::from_prepared(&prepared, &options)),
        applied: plan.image,
        crop_us,
        resize_us: prepared.resize_us,
        encode_us: prepared.encode_us,
        base64_us: prepared.base64_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::ImageFormat;
    use crate::frame::{PixelBuffer, PixelFormat};
    use crate::geometry::SourceGeometry;
    use crate::presentation::policy::{ImageFloors, RegionPolicy, TemporalFramePolicy};
    use crate::session::FrameId;
    use std::sync::Arc;
    use std::time::Instant;

    fn frame(width: u32, height: u32, origin: (i32, i32)) -> Arc<Frame> {
        let data = vec![90u8; (width * height * 3) as usize];
        let pixels = PixelBuffer::new(width, height, PixelFormat::Rgb8, data).unwrap();
        Arc::new(Frame::new(
            SourceGeometry::region(Some(":99".into()), origin.0, origin.1, width, height),
            pixels,
            Instant::now(),
        ))
    }

    fn presentable(id: u64, origin: (i32, i32)) -> PresentableFrame {
        PresentableFrame {
            session_id: "s".into(),
            frame_id: FrameId(id),
            frame: frame(40, 30, origin),
            captured_at: Instant::now(),
            capture_offset: None,
            capture_duration: None,
        }
    }

    #[test]
    fn a_whole_frame_view_reports_the_frames_own_rectangle() {
        let policy = ObservationPolicy::phase5_default(ImagePolicy::png());
        let plan = plan_for_frame(&presentable(1, (0, 0)), &policy).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.views[0].source, ViewSource::Whole);
        assert!(plan.views[0].required);
    }

    #[test]
    fn a_region_on_a_shifted_frame_converts_source_coordinates_to_frame_local_ones() {
        // The frame's own origin is (100, 200), so a region declared at source
        // (110, 220) is 10,20 inside the frame.
        let source = presentable(1, (100, 200));
        let policy = ObservationPolicy {
            overview: None,
            regions: vec![RegionPolicy::required(
                "hud",
                Rect::new(110, 220, 10, 5).unwrap(),
                ImagePolicy::png(),
            )],
            temporal: None,
            payload_budget: None,
            changed: None,
        };

        let plan = plan_for_frame(&source, &policy).unwrap();
        assert_eq!(
            plan.views[0].source,
            ViewSource::Crop {
                rect: Rect::new(10, 20, 10, 5).unwrap()
            }
        );
        // And the reported source rectangle is in original source space.
        assert_eq!(
            plan.views[0].source_rect(std::slice::from_ref(&source)),
            Rect::new(110, 220, 10, 5).unwrap()
        );
    }

    #[test]
    fn a_region_outside_the_frame_is_refused_at_planning_time() {
        let source = presentable(1, (0, 0));
        let policy = ObservationPolicy {
            overview: None,
            regions: vec![RegionPolicy::required(
                "too-big",
                Rect::new(0, 0, 100, 100).unwrap(),
                ImagePolicy::png(),
            )],
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        let error = plan_for_frame(&source, &policy).unwrap_err();
        assert_eq!(error.code(), "invalid_region");
    }

    #[test]
    fn a_newest_scoped_region_is_planned_only_for_the_newest_frame() {
        let frames = vec![presentable(1, (0, 0)), presentable(2, (0, 0))];
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::png()),
            regions: vec![RegionPolicy::required(
                "hud",
                Rect::new(0, 0, 10, 10).unwrap(),
                ImagePolicy::png(),
            )
            .with_scope(RegionScope::Newest)],
            temporal: None,
            payload_budget: None,
            changed: None,
        };

        let plan = plan_for_stack(&frames, &policy).unwrap();
        assert_eq!(plan.views_for(0).count(), 1, "older frame: overview only");
        assert_eq!(plan.views_for(1).count(), 2, "newest: overview and region");
    }

    #[test]
    fn an_older_scoped_region_is_planned_only_for_the_older_frames() {
        let frames = vec![
            presentable(1, (0, 0)),
            presentable(2, (0, 0)),
            presentable(3, (0, 0)),
        ];
        let policy = ObservationPolicy {
            overview: None,
            regions: vec![RegionPolicy::required(
                "history",
                Rect::new(0, 0, 10, 10).unwrap(),
                ImagePolicy::png(),
            )
            .with_scope(RegionScope::Older)],
            temporal: Some(TemporalFramePolicy::AllSame {
                image: ImagePolicy::png(),
            }),
            payload_budget: None,
            changed: None,
        };

        let plan = plan_for_stack(&frames, &policy).unwrap();
        assert_eq!(plan.views_for(0).count(), 1);
        assert_eq!(plan.views_for(1).count(), 1);
        assert_eq!(plan.views_for(2).count(), 0, "the newest is excluded");
    }

    #[test]
    fn plan_order_is_oldest_first_so_the_response_can_emit_it_directly() {
        let frames = vec![
            presentable(1, (0, 0)),
            presentable(2, (0, 0)),
            presentable(3, (0, 0)),
        ];
        let policy = ObservationPolicy::phase5_default(ImagePolicy::png());
        let plan = plan_for_stack(&frames, &policy).unwrap();

        let order: Vec<u64> = plan.views.iter().map(|v| v.frame_id.get()).collect();
        assert_eq!(order, vec![1, 2, 3]);
    }

    #[test]
    fn the_newest_frame_is_identified_by_position_not_by_identifier() {
        // Requirements 14 and 49: a stack interleaved with independent captures has
        // non-contiguous identifiers, so "newest" has to mean "last in the stack".
        let frames = vec![
            presentable(300, (0, 0)),
            presentable(7, (0, 0)),
            presentable(12, (0, 0)),
        ];
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(10, 40)),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::NewestDetailed {
                older: ImagePolicy::jpeg_width(10, 40),
                newest: ImagePolicy::jpeg_width(40, 90),
            }),
            payload_budget: None,
            changed: None,
        };

        let plan = plan_for_stack(&frames, &policy).unwrap();
        let newest: Vec<u64> = plan
            .views
            .iter()
            .filter(|v| v.is_newest)
            .map(|v| v.frame_id.get())
            .collect();
        assert_eq!(
            newest,
            vec![12],
            "the last frame is the newest, not the largest id"
        );
    }

    #[test]
    fn encoding_a_metadata_only_view_touches_no_pixels() {
        let source = presentable(1, (0, 0));
        let plan = ViewPlan {
            frame_index: 0,
            frame_id: FrameId(1),
            kind: ViewKind::Overview,
            source: ViewSource::Whole,
            image: ImagePolicy::metadata_only(),
            required: false,
            priority: 0,
            is_newest: true,
        };

        let encoded = encode_view(&source.frame, &plan).unwrap();
        assert!(encoded.image.is_none());
        assert_eq!(encoded.encode_us, 0);
        assert_eq!(encoded.resize_us, 0);
    }

    #[test]
    fn encoding_a_region_view_reports_the_source_rectangle_not_the_image_size() {
        let source = presentable(1, (0, 0));
        let plan = ViewPlan {
            frame_index: 0,
            frame_id: FrameId(1),
            kind: ViewKind::Region { name: "hud".into() },
            source: ViewSource::Crop {
                rect: Rect::new(4, 6, 20, 10).unwrap(),
            },
            image: ImagePolicy::jpeg_width(10, 70),
            required: true,
            priority: VIEW_PRIORITY_REQUIRED_REGION,
            is_newest: true,
        };

        let encoded = encode_view(&source.frame, &plan).unwrap();
        assert_eq!(encoded.source_rect, Rect::new(4, 6, 20, 10).unwrap());
        let image = encoded.image.unwrap();
        assert_eq!((image.width, image.height), (10, 5));
        assert_eq!(image.format, "jpeg");
    }

    #[test]
    fn the_default_priority_ladder_protects_newest_then_required_then_optional() {
        let frames = vec![presentable(1, (0, 0)), presentable(2, (0, 0))];
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(20, 60)),
            regions: vec![RegionPolicy::optional(
                "extra",
                Rect::new(0, 0, 10, 10).unwrap(),
                ImagePolicy::png(),
            )],
            temporal: Some(TemporalFramePolicy::NewestDetailed {
                older: ImagePolicy::jpeg_width(10, 40),
                newest: ImagePolicy::jpeg_width(20, 60),
            }),
            payload_budget: None,
            changed: None,
        };

        let plan = plan_for_stack(&frames, &policy).unwrap();
        let newest_overview = plan
            .views
            .iter()
            .find(|v| v.kind == ViewKind::Overview && v.is_newest)
            .unwrap();
        let older_overview = plan
            .views
            .iter()
            .find(|v| v.kind == ViewKind::Overview && !v.is_newest)
            .unwrap();
        let optional = plan.views.iter().find(|v| !v.required).unwrap();

        assert!(newest_overview.priority > older_overview.priority);
        assert!(newest_overview.priority > optional.priority);
    }

    #[test]
    fn floors_attached_to_a_policy_survive_planning() {
        let mut image = ImagePolicy::jpeg_width(480, 55);
        image.floors = Some(ImageFloors::older());
        let policy = ObservationPolicy::phase5_default(image);
        let plan = plan_for_frame(&presentable(1, (0, 0)), &policy).unwrap();
        assert_eq!(plan.views[0].image.floors, Some(ImageFloors::older()));
    }

    #[test]
    fn a_jpeg_and_a_png_region_are_encoded_independently() {
        let source = presentable(1, (0, 0));
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::png()),
            regions: vec![RegionPolicy::required(
                "lossy",
                Rect::new(0, 0, 10, 10).unwrap(),
                ImagePolicy::jpeg_width(10, 50),
            )],
            temporal: None,
            payload_budget: None,
            changed: None,
        };

        let plan = plan_for_frame(&source, &policy).unwrap();
        let encoded: Vec<_> = plan
            .views
            .iter()
            .map(|v| encode_view(&source.frame, v).unwrap().image.unwrap())
            .collect();

        assert_eq!(encoded[0].format, ImageFormat::Png.name());
        assert_eq!(encoded[1].format, ImageFormat::Jpeg.name());
        assert_eq!(encoded[1].quality, Some(50));
        assert_eq!(encoded[0].quality, None, "PNG has no quality");
    }
}
