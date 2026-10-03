//! Efficient visual delivery for software agents.
//!
//! # Where this sits
//!
//! Phase 6 belongs **after** raw observation, and the boundary is the whole point:
//!
//! ```text
//! capture / observe / realtime
//!     -> raw Frame or raw Frame stack        (semantics: when pixels were taken)
//!     -> presentation policy                 (policy: what to send)
//!     -> View(s)                             (extraction, still raw where possible)
//!     -> encode                              (presentation work)
//!     -> base64 / protocol response
//! ```
//!
//! Nothing in this module captures, compares, schedules, or touches history. It is
//! handed frames that already exist and decides how to render them. That is what
//! lets the *same* raw observation produce several alternative presentations, and it
//! is what keeps Phase 6 from quietly changing Phase 1–5 semantics.
//!
//! # The one rule
//!
//! > What gets encoded is a policy decision over already captured raw frames.
//!
//! There is no implicit policy. If a caller supplies no policy, the Phase 5 output
//! is reproduced exactly — see [`ObservationPolicy::phase5_default`].

pub mod budget;
pub mod changed;
pub mod policy;
pub mod view;

pub use budget::{fit_plan, measure_plan, PayloadAdjustment, PayloadFit, PayloadFitState};
pub use changed::{
    changed_policy, changed_region, changed_view_policy, ChangedRegionPolicy, ChangedRegionResult,
};
pub use policy::{
    ImagePolicy, ObservationPolicy, PayloadBudget, RegionPolicy, RegionScope, ResizePolicy,
    TemporalFramePolicy,
};
pub use view::{
    encode_view, EncodedView, ViewKind, ViewPlan, ViewSource, VIEW_PRIORITY_NEWEST,
    VIEW_PRIORITY_OLD,
};

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::session::FrameId;

/// A raw frame offered for presentation, with the identity it must keep.
///
/// The frame is an [`Arc`] because a view is a *different rendering*, never a
/// different copy of the pixels: several views over one frame must share the one
/// allocation (requirement 43). Every caller already holds frames this way, so
/// building one of these costs a pointer bump.
#[derive(Debug, Clone)]
pub struct PresentableFrame {
    /// Which session captured it.
    pub session_id: String,
    /// The frame's identity within that session.
    pub frame_id: FrameId,
    /// The raw pixels, shared rather than copied.
    pub frame: Arc<Frame>,
    /// When the capture completed.
    pub captured_at: Instant,
    /// Where this frame sat in a sampling window, for a temporal stack.
    pub capture_offset: Option<Duration>,
    /// How long the capture itself took, for a temporal stack.
    pub capture_duration: Option<Duration>,
}

impl PresentableFrame {
    /// A frame outside any sampling window: a capture, a retrieval, a final frame.
    pub fn single(session_id: impl Into<String>, frame: Arc<Frame>) -> Self {
        PresentableFrame {
            session_id: session_id.into(),
            frame_id: FrameId::FIRST,
            captured_at: frame.captured_at,
            frame,
            capture_offset: None,
            capture_duration: None,
        }
    }

    /// The frame's age, as of `now`.
    pub fn age_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.captured_at)
    }

    /// The frame's width in source pixels.
    pub fn width(&self) -> u32 {
        self.frame.width()
    }

    /// The frame's height in source pixels.
    pub fn height(&self) -> u32 {
        self.frame.height()
    }

    /// The whole frame as a source-space rectangle.
    pub fn source_rect(&self) -> Rect {
        self.frame.source_rect()
    }
}

/// A view that has been cropped, resized, encoded, and optionally base64 encoded.
///
/// The source rectangle is carried separately from the returned image, exactly as
/// `PreparedImage` does for a whole frame, because a crop or a resize means those
/// are two coordinate spaces and the response must not conflate them.
#[derive(Debug, Clone)]
pub struct PresentedView {
    /// What kind of view this is.
    pub kind: ViewKind,
    /// Where this view came from, in source coordinates.
    pub source_rect: Rect,
    /// The encoded image, or `None` for a metadata-only view.
    pub image: Option<crate::output::json::ObservedFrame>,
    /// Presentation settings actually applied, after any budget fitting.
    pub applied: ImagePolicy,
}

/// A whole presentation: one or more views per frame, in frame order.
#[derive(Debug, Clone)]
pub struct Presentation {
    /// One entry per frame, oldest first.
    pub frames: Vec<PresentedFrame>,
    /// What the budget fitter did, when a budget was set.
    pub payload: Option<PayloadFit>,
    /// Where the presentation time went.
    pub timing: PresentationTiming,
}

impl Presentation {
    /// The newest frame's presentation.
    pub fn newest(&self) -> Option<&PresentedFrame> {
        self.frames.last()
    }

    /// Total encoded bytes across every view.
    pub fn total_encoded_bytes(&self) -> usize {
        self.frames
            .iter()
            .flat_map(|frame| frame.views.iter())
            .filter_map(|view| view.image.as_ref())
            .map(|image| image.byte_length)
            .sum()
    }

    /// Total base64 bytes across every view.
    ///
    /// Measured, not derived: the base64 string length is what actually travelled,
    /// including its padding.
    pub fn total_base64_bytes(&self) -> usize {
        self.frames
            .iter()
            .flat_map(|frame| frame.views.iter())
            .filter_map(|view| view.image.as_ref())
            .filter_map(|image| image.data.as_ref())
            .map(String::len)
            .sum()
    }
}

/// One frame's views.
#[derive(Debug, Clone)]
pub struct PresentedFrame {
    /// The frame's identity.
    pub frame_id: FrameId,
    /// When the capture completed, measured from the request start.
    pub capture_offset: Option<Duration>,
    /// How long the capture itself took.
    pub capture_duration: Option<Duration>,
    /// How long ago the frame was captured, when the response was assembled.
    pub age: Duration,
    /// The views, overview first, then regions in declared order.
    pub views: Vec<PresentedView>,
}

impl PresentedFrame {
    /// The overview view, if one was requested and survived.
    pub fn overview(&self) -> Option<&PresentedView> {
        self.views
            .iter()
            .find(|view| matches!(view.kind, ViewKind::Overview))
    }

    /// A named region view.
    pub fn region(&self, name: &str) -> Option<&PresentedView> {
        self.views.iter().find(|view| match &view.kind {
            ViewKind::Region { name: n } => n == name,
            _ => false,
        })
    }

    /// Whether this frame carries any image bytes at all.
    pub fn has_images(&self) -> bool {
        self.views.iter().any(|view| view.image.is_some())
    }
}

/// Where the presentation time went.
///
/// Kept separate from any sampling timing so a caller can distinguish *captured
/// late* from *captured on time, delivered late* (requirement 35). This is the
/// second of those two questions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PresentationTiming {
    /// Total time in the presentation phase.
    pub presentation_us: u64,
    /// Time spent cropping regions out of raw frames.
    pub crop_us_total: u64,
    /// Time spent resizing.
    pub resize_us_total: u64,
    /// Time spent encoding.
    pub encode_us_total: u64,
    /// Time spent base64 encoding.
    pub base64_us_total: u64,
    /// Time spent in the budget fitting loop, including re-encodes.
    pub budget_fit_us: u64,
}

impl PresentationTiming {
    /// A copy of this timing as JSON.
    pub fn to_json(self) -> crate::output::json::PresentationTimingSection {
        crate::output::json::PresentationTimingSection {
            presentation_us: self.presentation_us,
            crop_us_total: self.crop_us_total,
            resize_us_total: self.resize_us_total,
            encode_us_total: self.encode_us_total,
            base64_us_total: self.base64_us_total,
            budget_fit_us: self.budget_fit_us,
        }
    }
}

/// Reject a policy that cannot mean anything.
///
/// Checked before any frame is touched, so a malformed policy is refused without
/// doing work — and refused as `invalid_presentation_policy` rather than being
/// silently repaired, because a silently repaired policy is a policy the caller
/// cannot reason about.
pub fn validate_policy(policy: &ObservationPolicy) -> Result<(), Error> {
    policy.validate()
}

/// Present one raw frame under a policy.
///
/// This is the multi-view path used by `capture`, `latest`, `frame`, the final frame
/// of an observation, and the changed-region crop. It never captures: it renders a
/// frame that already exists, which is why re-presenting a retained frame allocates
/// no new frame ID (requirement 42).
pub fn present_frame(
    frame: &PresentableFrame,
    policy: &ObservationPolicy,
) -> Result<Presentation, Error> {
    policy.validate()?;
    let started = Instant::now();

    let plan = view::plan_for_frame(frame, policy)?;
    let (plan, fit) = match &policy.payload_budget {
        Some(budget) => {
            let fit_started = Instant::now();
            let (plan, fit) = budget::fit_plan(plan, std::slice::from_ref(frame), budget)?;
            let fit_us = fit_started.elapsed().as_micros() as u64;
            (plan, Some(fit.with_fit_time(fit_us)))
        }
        None => (plan, None),
    };

    // The moment ages are measured. Taken once, after the (possibly fitted) encode
    // pass, so every frame's age describes the same instant.
    let finalized_at = Instant::now();

    let mut crop_us_total = 0u64;
    let mut resize_us_total = 0u64;
    let mut encode_us_total = 0u64;
    let mut base64_us_total = 0u64;

    let mut views: Vec<PresentedView> = Vec::new();
    for view_plan in &plan.views {
        let encoded = view::encode_view(&frame.frame, view_plan)?;
        crop_us_total += encoded.crop_us;
        resize_us_total += encoded.resize_us;
        encode_us_total += encoded.encode_us;
        base64_us_total += encoded.base64_us;
        views.push(encoded.into_presented());
    }

    let presented = PresentedFrame {
        frame_id: frame.frame_id,
        capture_offset: frame.capture_offset,
        capture_duration: frame.capture_duration,
        age: frame.age_at(finalized_at),
        views,
    };

    let timing = PresentationTiming {
        presentation_us: started.elapsed().as_micros() as u64,
        crop_us_total,
        resize_us_total,
        encode_us_total,
        base64_us_total,
        budget_fit_us: fit.as_ref().map(|f| f.fit_us).unwrap_or(0),
    };

    Ok(Presentation {
        frames: vec![presented],
        payload: fit,
        timing,
    })
}

/// Present a temporal stack under a policy, oldest first.
///
/// Every frame shares one policy decision table, so the *same* named region is
/// cropped from each frame at the same source coordinates (requirement 32), and the
/// newest frame can differ from the older ones (requirement 15).
pub fn present_stack(
    frames: &[PresentableFrame],
    policy: &ObservationPolicy,
) -> Result<Presentation, Error> {
    policy.validate()?;
    if frames.is_empty() {
        return Err(Error::observation_failed(
            "a presentation needs at least one frame",
        ));
    }
    let started = Instant::now();

    let plan = view::plan_for_stack(frames, policy)?;
    let (plan, fit) = match &policy.payload_budget {
        Some(budget) => {
            let fit_started = Instant::now();
            let (plan, fit) = budget::fit_plan(plan, frames, budget)?;
            let fit_us = fit_started.elapsed().as_micros() as u64;
            (plan, Some(fit.with_fit_time(fit_us)))
        }
        None => (plan, None),
    };

    let finalized_at = Instant::now();

    let mut crop_us_total = 0u64;
    let mut resize_us_total = 0u64;
    let mut encode_us_total = 0u64;
    let mut base64_us_total = 0u64;

    // Views are grouped by the frame they were planned against, so the response can
    // emit frames oldest to newest while the fitting order stays independent of the
    // emission order (requirement 19).
    let mut per_frame: Vec<Vec<PresentedView>> = vec![Vec::new(); frames.len()];
    for view_plan in &plan.views {
        let source = &frames[view_plan.frame_index];
        let encoded = view::encode_view(&source.frame, view_plan)?;
        crop_us_total += encoded.crop_us;
        resize_us_total += encoded.resize_us;
        encode_us_total += encoded.encode_us;
        base64_us_total += encoded.base64_us;
        per_frame[view_plan.frame_index].push(encoded.into_presented());
    }

    let presented: Vec<PresentedFrame> = frames
        .iter()
        .zip(per_frame)
        .map(|(frame, views)| PresentedFrame {
            frame_id: frame.frame_id,
            capture_offset: frame.capture_offset,
            capture_duration: frame.capture_duration,
            age: frame.age_at(finalized_at),
            views,
        })
        .collect();

    let timing = PresentationTiming {
        presentation_us: started.elapsed().as_micros() as u64,
        crop_us_total,
        resize_us_total,
        encode_us_total,
        base64_us_total,
        budget_fit_us: fit.as_ref().map(|f| f.fit_us).unwrap_or(0),
    };

    Ok(Presentation {
        frames: presented,
        payload: fit,
        timing,
    })
}

/// Convert a presentation into its response shape.
///
/// Transforms are derived here rather than stored on each view, because a view's
/// transform is a pure function of its source rectangle and its returned image size.
/// Storing it would create a second place for the two to disagree.
pub fn to_response(
    presentation: &Presentation,
    temporal_mode: Option<String>,
) -> crate::output::json::PresentationResponse {
    use crate::output::json::{
        PayloadSection, PresentationResponse, PresentationTimingSection, PresentedFrameResponse,
        ViewResponse,
    };

    let frames = presentation
        .frames
        .iter()
        .map(|frame| {
            let views: Vec<ViewResponse> = frame.views.iter().map(view_response).collect();
            let encoded_bytes = views
                .iter()
                .filter_map(|view| view.image.as_ref())
                .map(|image| image.byte_length)
                .sum();
            let base64_bytes = views
                .iter()
                .filter_map(|view| view.image.as_ref())
                .filter_map(|image| image.data.as_ref())
                .map(String::len)
                .sum();
            PresentedFrameResponse {
                frame_id: frame.frame_id,
                capture_offset_us: frame.capture_offset.map(|d| d.as_micros() as u64),
                capture_duration_us: frame.capture_duration.map(|d| d.as_micros() as u64),
                age_us: frame.age.as_micros() as u64,
                views,
                encoded_bytes,
                base64_bytes,
            }
        })
        .collect();

    let payload = presentation.payload.as_ref().map(|fit| PayloadSection {
        budget_base64_bytes: fit.budget_base64_bytes,
        actual_base64_bytes: fit.actual_base64_bytes,
        fit: fit.fit.name().to_string(),
        adjustments: fit.adjustments.clone(),
    });

    PresentationResponse {
        frames,
        payload,
        timing: PresentationTimingSection {
            presentation_us: presentation.timing.presentation_us,
            crop_us_total: presentation.timing.crop_us_total,
            resize_us_total: presentation.timing.resize_us_total,
            encode_us_total: presentation.timing.encode_us_total,
            base64_us_total: presentation.timing.base64_us_total,
            budget_fit_us: presentation.timing.budget_fit_us,
        },
        total_encoded_bytes: presentation.total_encoded_bytes(),
        total_base64_bytes: presentation.total_base64_bytes(),
        temporal_mode,
    }
}

/// Render one view as a response.
pub fn view_response(view: &PresentedView) -> crate::output::json::ViewResponse {
    use crate::geometry::Transform;
    use crate::output::json::{AppliedImagePolicy, ViewResponse};

    // The transform maps the returned image's pixels back to the source rectangle
    // this view was cut from. For a metadata-only view there is no image to map, so
    // the transform describes a one-to-one mapping over the source rectangle
    // instead: it is the only mapping that is true when there are no pixels.
    let transform = view
        .image
        .as_ref()
        .and_then(|image| Transform::new(&view.source_rect, image.width, image.height).ok())
        .unwrap_or_else(|| {
            Transform::new(
                &view.source_rect,
                view.source_rect.width,
                view.source_rect.height,
            )
            .unwrap_or_else(|_| {
                Transform::new(&view.source_rect, 1, 1).expect("a 1x1 transform is valid")
            })
        });

    ViewResponse {
        name: view.kind.name().to_string(),
        kind: view_kind_name(&view.kind).to_string(),
        source_rect: view.source_rect,
        transform,
        image: view.image.clone(),
        applied: AppliedImagePolicy {
            format: view.applied.format.name().to_string(),
            quality: (view.applied.format == crate::encode::ImageFormat::Jpeg)
                .then_some(view.applied.quality),
            width: view.applied.resize.width(),
            base64: view.applied.base64,
            metadata_only: view.applied.is_metadata_only(),
        },
    }
}

/// The response-level kind name for a view.
fn view_kind_name(kind: &ViewKind) -> &'static str {
    match kind {
        ViewKind::Overview => "overview",
        ViewKind::Region { .. } => "region",
        ViewKind::ChangedRegion => "changed_region",
    }
}

/// Convert a changed-region result into its response shape.
///
/// Takes the result by value because the view it carries is converted rather than
/// cloned: the crop is the only copy of those encoded bytes, so there is nothing to
/// share it with.
pub fn changed_to_response(
    region: ChangedRegionResult,
) -> crate::output::json::ChangedRegionResponse {
    crate::output::json::ChangedRegionResponse {
        raw_changed_rect: region.raw_rect,
        returned_rect: region.returned_rect,
        padding: region.padding,
        fell_back_to_overview: region.fell_back_to_overview,
        view: view_response(&region.view.into_presented()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{PixelBuffer, PixelFormat};
    use crate::geometry::SourceGeometry;

    fn frame(width: u32, height: u32, shade: u8) -> Arc<Frame> {
        let data = vec![shade; (width * height * 3) as usize];
        let pixels = PixelBuffer::new(width, height, PixelFormat::Rgb8, data).unwrap();
        Arc::new(Frame::new(
            SourceGeometry::desktop(Some(":99".into()), width, height),
            pixels,
            Instant::now(),
        ))
    }

    fn presentable(id: u64, width: u32, height: u32) -> PresentableFrame {
        PresentableFrame {
            session_id: "s".into(),
            frame_id: FrameId(id),
            frame: frame(width, height, 40),
            captured_at: Instant::now(),
            capture_offset: None,
            capture_duration: None,
        }
    }

    #[test]
    fn the_phase5_default_reproduces_one_full_size_view() {
        // No policy supplied must mean the Phase 5 behaviour, which is one image of
        // the whole frame in the requested format.
        let policy = ObservationPolicy::phase5_default(ImagePolicy::png());
        let source = presentable(1, 40, 30);
        let presentation = present_frame(&source, &policy).unwrap();

        assert_eq!(presentation.frames.len(), 1);
        assert_eq!(presentation.frames[0].views.len(), 1);
        let view = &presentation.frames[0].views[0];
        assert_eq!(view.kind, ViewKind::Overview);
        assert_eq!(view.source_rect.width, 40);
        assert_eq!(view.image.as_ref().unwrap().width, 40);
    }

    #[test]
    fn several_regions_share_one_frame_and_one_allocation() {
        // Requirement 43: a view is a different rendering, never a different copy.
        // The source `Arc` is untouched by planning, so the pixel buffer is shared.
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(20, 60)),
            regions: vec![
                RegionPolicy::required("a", Rect::new(0, 0, 10, 10).unwrap(), ImagePolicy::png()),
                RegionPolicy::required("b", Rect::new(20, 0, 10, 10).unwrap(), ImagePolicy::png()),
            ],
            temporal: None,
            payload_budget: None,
            changed: None,
        };

        let source = presentable(7, 40, 30);
        let before = Arc::as_ptr(&source.frame);
        let presentation = present_frame(&source, &policy).unwrap();

        assert_eq!(
            Arc::as_ptr(&source.frame),
            before,
            "the frame is not cloned"
        );
        assert_eq!(presentation.frames[0].views.len(), 3);
        assert!(presentation.frames[0].overview().is_some());
        assert!(presentation.frames[0].region("a").is_some());
        assert!(presentation.frames[0].region("b").is_some());
    }

    #[test]
    fn a_region_crop_reports_its_source_rectangle_not_its_image_size() {
        let policy = ObservationPolicy {
            overview: None,
            regions: vec![RegionPolicy::required(
                "hud",
                Rect::new(4, 6, 20, 10).unwrap(),
                ImagePolicy::png_width(10),
            )],
            temporal: None,
            payload_budget: None,
            changed: None,
        };

        let presentation = present_frame(&presentable(1, 40, 30), &policy).unwrap();
        let view = presentation.frames[0].region("hud").unwrap();

        // The crop is 20x10 in source space and resized to 10x5 for delivery.
        assert_eq!(view.source_rect.x, 4);
        assert_eq!(view.source_rect.y, 6);
        assert_eq!(view.source_rect.width, 20);
        let image = view.image.as_ref().unwrap();
        assert_eq!((image.width, image.height), (10, 5));
    }

    #[test]
    fn a_metadata_only_view_carries_identity_without_bytes() {
        // A metadata-only region alongside a real overview: the region keeps its
        // place, its source rectangle, and its transform, and contributes nothing to
        // the payload.
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(20, 60)),
            regions: vec![RegionPolicy::optional(
                "note",
                Rect::new(0, 0, 4, 4).unwrap(),
                ImagePolicy::metadata_only(),
            )],
            temporal: None,
            payload_budget: None,
            changed: None,
        };

        let presentation = present_frame(&presentable(1, 40, 30), &policy).unwrap();
        let view = presentation.frames[0].region("note").unwrap();
        assert!(view.image.is_none(), "metadata only means no image");
        assert_eq!(view.source_rect.width, 4);
        let overhead = presentation.total_base64_bytes();
        assert!(overhead > 0, "the overview still carries bytes");
    }

    #[test]
    fn a_temporal_policy_applies_the_right_settings_to_the_right_frame() {
        // Requirement 15: older frames reduced, newest preserved.
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(40, 90)),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::NewestDetailed {
                older: ImagePolicy::jpeg_width(10, 40),
                newest: ImagePolicy::jpeg_width(40, 90),
            }),
            payload_budget: None,
            changed: None,
        };

        let frames = vec![
            presentable(1, 40, 30),
            presentable(2, 40, 30),
            presentable(3, 40, 30),
        ];
        let presentation = present_stack(&frames, &policy).unwrap();

        assert_eq!(presentation.frames.len(), 3);
        // Oldest to newest, and the newest is the detailed one.
        assert_eq!(presentation.frames[0].frame_id, FrameId(1));
        assert_eq!(presentation.frames[2].frame_id, FrameId(3));
        assert_eq!(
            presentation.frames[0]
                .overview()
                .unwrap()
                .image
                .as_ref()
                .unwrap()
                .width,
            10
        );
        assert_eq!(
            presentation.frames[2]
                .overview()
                .unwrap()
                .image
                .as_ref()
                .unwrap()
                .width,
            40
        );
        assert_eq!(
            presentation.frames[2]
                .overview()
                .unwrap()
                .image
                .as_ref()
                .unwrap()
                .quality,
            Some(90)
        );
    }

    #[test]
    fn a_newest_only_policy_leaves_older_frames_with_identity_but_no_image() {
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(40, 75)),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::NewestOnly {
                image: ImagePolicy::jpeg_width(40, 75),
            }),
            payload_budget: None,
            changed: None,
        };

        let frames = vec![presentable(1, 40, 30), presentable(2, 40, 30)];
        let presentation = present_stack(&frames, &policy).unwrap();

        assert!(!presentation.frames[0].has_images());
        assert!(presentation.frames[1].has_images());
        // The older frame still reports its identity and timing.
        assert_eq!(presentation.frames[0].frame_id, FrameId(1));
    }

    #[test]
    fn an_invalid_policy_is_refused_before_any_frame_is_touched() {
        let policy = ObservationPolicy {
            overview: None,
            regions: Vec::new(),
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        // A policy that asks for nothing at all cannot produce a response. Refused as
        // an invalid *policy* rather than as an empty result, because an empty
        // response would leave a caller unable to tell a mistake from a still scene.
        let error = present_frame(&presentable(1, 4, 4), &policy).unwrap_err();
        assert_eq!(error.code(), "invalid_presentation_policy");
    }

    #[test]
    fn a_zero_sized_region_is_refused_as_an_invalid_policy() {
        let policy = ObservationPolicy {
            overview: None,
            regions: vec![RegionPolicy::required(
                "empty",
                Rect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 10,
                },
                ImagePolicy::png(),
            )],
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        let error = present_frame(&presentable(1, 40, 30), &policy).unwrap_err();
        assert_eq!(error.code(), "invalid_presentation_policy");
    }

    #[test]
    fn an_out_of_bounds_region_is_refused_rather_than_clipped() {
        let policy = ObservationPolicy {
            overview: None,
            regions: vec![RegionPolicy::required(
                "past-the-edge",
                Rect::new(0, 0, 100, 100).unwrap(),
                ImagePolicy::png(),
            )],
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        // An out-of-bounds region is caught when it is mapped into frame pixels,
        // which happens at planning time — before any capture or encode — so the
        // error is `invalid_region`, the same code an out-of-bounds capture region
        // reports. Either code would be defensible; this reuses the existing one
        // rather than inventing a second name for the same fault.
        let error = present_frame(&presentable(1, 40, 30), &policy).unwrap_err();
        assert_eq!(error.code(), "invalid_region");
        assert!(!error.message().contains("past-the-edge"));
    }

    #[test]
    fn duplicate_region_names_are_refused_because_a_response_could_not_name_them() {
        let policy = ObservationPolicy {
            overview: None,
            regions: vec![
                RegionPolicy::required("hud", Rect::new(0, 0, 4, 4).unwrap(), ImagePolicy::png()),
                RegionPolicy::required("hud", Rect::new(4, 4, 4, 4).unwrap(), ImagePolicy::png()),
            ],
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        let error = present_frame(&presentable(1, 40, 30), &policy).unwrap_err();
        assert_eq!(error.code(), "invalid_presentation_policy");
        assert!(error.message().contains("hud"), "{}", error.message());
    }

    #[test]
    fn a_region_named_like_the_overview_is_refused_so_the_response_stays_unambiguous() {
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::png()),
            regions: vec![RegionPolicy::required(
                "overview",
                Rect::new(0, 0, 4, 4).unwrap(),
                ImagePolicy::png(),
            )],
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        let error = present_frame(&presentable(1, 40, 30), &policy).unwrap_err();
        assert_eq!(error.code(), "invalid_presentation_policy");
    }

    #[test]
    fn a_region_with_a_newest_only_scope_is_cropped_from_only_the_newest_frame() {
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(20, 60)),
            regions: vec![RegionPolicy {
                scope: RegionScope::Newest,
                ..RegionPolicy::required(
                    "hud",
                    Rect::new(0, 0, 10, 10).unwrap(),
                    ImagePolicy::png(),
                )
            }],
            temporal: Some(TemporalFramePolicy::AllSame {
                image: ImagePolicy::jpeg_width(20, 60),
            }),
            payload_budget: None,
            changed: None,
        };

        let frames = vec![
            presentable(1, 40, 30),
            presentable(2, 40, 30),
            presentable(3, 40, 30),
        ];
        let presentation = present_stack(&frames, &policy).unwrap();

        // Every frame has the overview; only the newest has the region.
        for frame in &presentation.frames {
            assert!(frame.overview().is_some(), "overview covers all frames");
        }
        assert!(presentation.frames[0].region("hud").is_none());
        assert!(presentation.frames[2].region("hud").is_some());
    }

    #[test]
    fn view_priority_ranks_the_newest_above_older_frames_and_required_above_optional() {
        // Requirement 28's default priority ladder, expressed by the planner.
        //
        // The claim is a *ranking*, not a claim that every required view outranks every
        // optional one: an older frame's overview is required — it was asked for — yet
        // sits below an optional region on the newest frame, because the newest frame
        // is what the caller is looking at. Both orderings are deliberate, so both are
        // asserted rather than one being left implicit.
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(20, 60)),
            regions: vec![
                RegionPolicy::required("hud", Rect::new(0, 0, 10, 10).unwrap(), ImagePolicy::png()),
                RegionPolicy::optional(
                    "extra",
                    Rect::new(20, 0, 10, 10).unwrap(),
                    ImagePolicy::png(),
                ),
            ],
            temporal: Some(TemporalFramePolicy::NewestDetailed {
                older: ImagePolicy::jpeg_width(10, 40),
                newest: ImagePolicy::jpeg_width(20, 60),
            }),
            payload_budget: None,
            changed: None,
        };

        let frames = vec![presentable(1, 40, 30), presentable(2, 40, 30)];
        let plan = view::plan_for_stack(&frames, &policy).unwrap();

        let newest_overview = plan
            .views
            .iter()
            .find(|v| matches!(v.kind, ViewKind::Overview) && v.is_newest)
            .unwrap();
        let older_overview = plan
            .views
            .iter()
            .find(|v| matches!(v.kind, ViewKind::Overview) && !v.is_newest)
            .unwrap();
        let required_region = plan
            .views
            .iter()
            .find(|v| matches!(v.kind, ViewKind::Region { .. }) && v.required)
            .unwrap();

        assert!(
            newest_overview.priority > older_overview.priority,
            "the newest frame's overview must outrank an older frame's"
        );
        assert!(
            required_region.priority > older_overview.priority,
            "a required region must outrank an older frame's overview"
        );

        // Every frame carries an overview, and every one is required: asking for a stack
        // of two frames means two frames, so fitting may reduce them but never silently
        // drop one.
        let overviews: Vec<_> = plan
            .views
            .iter()
            .filter(|v| matches!(v.kind, ViewKind::Overview))
            .collect();
        assert_eq!(overviews.len(), 2, "one overview per frame");
        assert!(
            overviews.iter().all(|v| v.required),
            "no frame's overview may be dropped by fitting"
        );
    }
}
