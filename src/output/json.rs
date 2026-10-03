//! The stable JSON response shapes.
//!
//! There are two documents: the capture response and the diff response. The
//! capture response deliberately keeps three groups of numbers apart, because
//! conflating them is the classic way to make an agent-facing screenshot tool
//! ambiguous:
//!
//! * `source` — where the pixels came from, in source-desktop pixels;
//! * `image` — the returned image, in its own pixels;
//! * `transform` — the explicit mapping between the two.
//!
//! Nothing in the response asks the caller to guess which coordinate space a
//! width belongs to.
//!
//! The diff response is additive: it documents a comparison without changing
//! anything about the capture document.

use serde::{Deserialize, Serialize};

use crate::compare::Comparison;
use crate::encode::EncodedImage;
use crate::geometry::{SourceGeometry, Transform};
use crate::timing::Timing;

/// The returned image, independent of where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSection {
    /// Width of the returned image, in image pixels.
    pub width: u32,
    /// Height of the returned image, in image pixels.
    pub height: u32,
    /// IANA media type, for example `image/jpeg`.
    pub media_type: String,
    /// Canonical format name, for example `jpeg`.
    pub format: String,
    /// Size of the encoded image in bytes.
    pub byte_length: usize,
    /// Encoding of [`ImageSection::data`], when inline data is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// Inline base64 image data, when `--base64` was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// JPEG quality, when the image is a JPEG.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
}

/// A complete capture response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureResponse {
    /// Source geometry, in source-desktop pixels.
    pub source: SourceGeometry,
    /// The returned image.
    pub image: ImageSection,
    /// Mapping from image coordinates back to source coordinates.
    pub transform: Transform,
    /// Per-stage timings in microseconds.
    pub timing: Timing,
}

impl CaptureResponse {
    /// Assemble a response from its parts.
    pub fn new(
        source: SourceGeometry,
        encoded: &EncodedImage,
        transform: Transform,
        timing: Timing,
        base64_data: Option<String>,
        quality: Option<u8>,
    ) -> Self {
        CaptureResponse {
            source,
            image: ImageSection {
                width: encoded.width,
                height: encoded.height,
                media_type: encoded.media_type().to_string(),
                format: encoded.format.name().to_string(),
                byte_length: encoded.bytes.len(),
                encoding: base64_data.as_ref().map(|_| "base64".to_string()),
                data: base64_data,
                quality,
            },
            transform,
            timing,
        }
    }

    /// Serialize to compact JSON followed by a newline.
    pub fn to_json_string(&self) -> Result<String, crate::error::Error> {
        serde_json::to_string(self)
            .map(|mut text| {
                text.push('\n');
                text
            })
            .map_err(|e| {
                crate::error::Error::Internal(format!(
                    "capture response could not be serialized: {e}"
                ))
            })
    }
}

/// Timings for a comparison, in microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareTiming {
    /// Time spent reading and decoding the input images, or zero when the
    /// comparison ran on in-memory frames.
    pub load_us: u64,
    /// Time spent comparing the two frames.
    pub compare_us: u64,
    /// Time spent writing the changed crop, if one was requested.
    pub crop_us: u64,
    /// Wall-clock duration of the whole operation.
    pub total_us: u64,
}

/// A short description of one comparison input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDescription {
    /// Where the input came from.
    pub source: SourceGeometry,
    /// Width of the input in pixels.
    pub width: u32,
    /// Height of the input in pixels.
    pub height: u32,
}

impl InputDescription {
    /// Describe a frame.
    pub fn from_frame(frame: &crate::frame::Frame) -> Self {
        InputDescription {
            source: frame.source_geometry.clone(),
            width: frame.width(),
            height: frame.height(),
        }
    }
}

// ============================================================================
// Phase 6: efficient agent observation
// ============================================================================
//
// These types are additive. Nothing above changed shape, and a Phase 1-5 caller
// that never mentions `views` or `presentation` sees exactly the response it saw
// before (requirement 37).

/// One rendered view of a frame: an overview, a named region, or a changed crop.
///
/// The `source_rect` is in **source coordinates**, and it is what the returned
/// image's pixels map back through. A crop of a window at source `(100, 200)`
/// reports the source rectangle, not the frame-local one, so a caller never has to
/// reconstruct where a region came from (requirement 7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewResponse {
    /// The view's name: `overview`, a caller-supplied region name, or `changed`.
    pub name: String,
    /// What kind of view this is.
    pub kind: String,
    /// The source-space rectangle this view was rendered from.
    pub source_rect: crate::geometry::Rect,
    /// Mapping from view pixels back to source pixels.
    pub transform: Transform,
    /// The image, or `None` for a metadata-only view.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ObservedFrame>,
    /// The presentation settings actually applied, after any budget fitting.
    pub applied: AppliedImagePolicy,
}

/// The presentation settings actually used for a view.
///
/// Reported separately from what was requested so that a caller can always tell
/// whether it got what it asked for, and so a budget adjustment is visible in the
/// view itself rather than only in the fit report (requirement 23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedImagePolicy {
    /// Output format actually used.
    pub format: String,
    /// JPEG quality actually used, when the format has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    /// Width actually used, when the view was resized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Whether the image was embedded inline.
    pub base64: bool,
    /// Whether the view carries no image at all.
    pub metadata_only: bool,
}

/// One frame's presentation inside a multi-view response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresentedFrameResponse {
    /// The frame's session identity.
    pub frame_id: crate::session::FrameId,
    /// When the sample was taken, from the request start. Temporal only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_offset_us: Option<u64>,
    /// How long the capture took. Temporal only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_duration_us: Option<u64>,
    /// How long ago the frame was captured, when the response was assembled.
    pub age_us: u64,
    /// The views, overview first, then regions in declared order.
    pub views: Vec<ViewResponse>,
    /// Total encoded bytes across this frame's views.
    pub encoded_bytes: usize,
    /// Total base64 bytes across this frame's views.
    pub base64_bytes: usize,
}

impl PresentedFrameResponse {
    /// The overview view, if there is one.
    pub fn overview(&self) -> Option<&ViewResponse> {
        self.views.iter().find(|view| view.kind == "overview")
    }

    /// A named region view.
    pub fn region(&self, name: &str) -> Option<&ViewResponse> {
        self.views
            .iter()
            .find(|view| view.kind == "region" && view.name == name)
    }

    /// Whether any view on this frame carries an image.
    pub fn has_images(&self) -> bool {
        self.views.iter().any(|view| view.image.is_some())
    }
}

/// What the payload budget fitter did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PayloadSection {
    /// The budget the caller set, in base64 bytes.
    pub budget_base64_bytes: usize,
    /// What the visual payload actually came to, in base64 bytes.
    pub actual_base64_bytes: usize,
    /// `exact` or `adjusted`.
    pub fit: String,
    /// Every change the fitter made.
    pub adjustments: Vec<crate::presentation::PayloadAdjustment>,
}

/// Where the presentation time went.
///
/// Kept separate from the sampling timing so a caller can distinguish *captured
/// late* from *captured on time, delivered late* (requirement 35).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationTimingSection {
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

/// A multi-view presentation of one or more frames.
///
/// This is the Phase 6 addition to the capture and retrieval responses. It is
/// attached as `presentation` and is absent entirely when no policy was supplied,
/// which is what keeps the Phase 5 output byte-for-byte unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresentationResponse {
    /// One entry per frame, oldest first.
    pub frames: Vec<PresentedFrameResponse>,
    /// What the budget fitter did, when a budget was set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<PayloadSection>,
    /// Where the presentation time went.
    pub timing: PresentationTimingSection,
    /// Total encoded bytes across every view.
    pub total_encoded_bytes: usize,
    /// Total base64 bytes across every view.
    pub total_base64_bytes: usize,
    /// The temporal policy mode that was applied, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporal_mode: Option<String>,
}

impl PresentationResponse {
    /// The newest frame's presentation.
    pub fn newest(&self) -> Option<&PresentedFrameResponse> {
        self.frames.last()
    }

    /// The single frame's presentation, for a non-temporal request.
    pub fn frame(&self) -> Option<&PresentedFrameResponse> {
        self.frames.first()
    }
}

/// A changed-region view and the comparison that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangedRegionResponse {
    /// The factual Phase 2 bounding box, before padding.
    pub raw_changed_rect: crate::geometry::Rect,
    /// The rectangle actually returned, after padding and clamping.
    pub returned_rect: crate::geometry::Rect,
    /// The padding that was requested.
    pub padding: u32,
    /// Whether the change was large enough that the policy returned the whole frame.
    pub fell_back_to_overview: bool,
    /// The rendered view.
    pub view: ViewResponse,
}

/// Describes a crop of the second frame that was written to disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedCrop {
    /// Where the crop was written.
    pub path: String,
    /// The region of the *after* frame that was cropped, in frame-local pixels.
    pub region: crate::geometry::Rect,
    /// Size of the written image in bytes.
    pub byte_length: usize,
    /// Image format of the written crop.
    pub format: String,
}

/// A complete diff response.
///
/// The comparison is nested under `comparison` rather than flattened, so the
/// fields that describe *what changed* stay visibly separate from the fields
/// that describe the two inputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffResponse {
    /// The first input.
    pub before: InputDescription,
    /// The second input.
    pub after: InputDescription,
    /// The comparison metrics.
    pub comparison: Comparison,
    /// The changed crop, if one was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_crop: Option<ChangedCrop>,
    /// Per-stage timings.
    pub timing: CompareTiming,
}

impl DiffResponse {
    /// Serialize to compact JSON followed by a newline.
    pub fn to_json_string(&self) -> Result<String, crate::error::Error> {
        serde_json::to_string(self)
            .map(|mut text| {
                text.push('\n');
                text
            })
            .map_err(|e| {
                crate::error::Error::Internal(format!("diff response could not be serialized: {e}"))
            })
    }
}

// ---------------------------------------------------------------------------
// Temporal observation responses
// ---------------------------------------------------------------------------

/// Timings for preparing the single returned frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationOutputTiming {
    /// Resize, in microseconds.
    pub resize_us: u64,
    /// Encode, in microseconds.
    pub encode_us: u64,
    /// Base64, in microseconds.
    pub base64_us: u64,
}

/// Timings for a temporal observation.
///
/// The point of these numbers is to answer three questions without a profiler:
/// are we slow because capture is slow, because comparison is slow, or because
/// we are mostly sleeping between polls?
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationTimingSection {
    /// Frames captured.
    pub captures: u64,
    /// Frame pairs compared.
    pub comparisons: u64,
    /// Total time inside capture, in microseconds.
    pub capture_us_total: u64,
    /// Total time inside comparison, in microseconds.
    pub compare_us_total: u64,
    /// Total time deliberately waiting between samples, in microseconds.
    pub sleep_us_total: u64,
    /// Timings for preparing the one returned frame.
    pub encode: ObservationOutputTiming,
}

/// The observation summary.
///
/// `result` is the authoritative statement of what happened. A timeout is
/// reported here rather than as an error, so that "nothing happened" is never
/// confused with "capture broke".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationSection {
    /// Which operation ran.
    pub kind: crate::cli::ObservationKind,
    /// `changed`, `stable`, `observed`, or `timeout`.
    pub result: crate::observe::Outcome,
    /// Total elapsed time in milliseconds.
    pub elapsed_ms: u64,
    /// Frames captured.
    pub captures: u64,
    /// Frame pairs compared.
    pub comparisons: u64,
    /// Required stability duration, for the operations that have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_for_ms: Option<u64>,
    /// How long the scene had been still when it completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_duration_ms: Option<u64>,
    /// When the first change was detected, for `observe`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_detected_ms: Option<u64>,
}

/// A comparison summarized inside a temporal result.
///
/// It carries the fields that describe the transition without repeating the
/// option values, which appear on the frame's own `transform`/`image` pair and
/// in the observation summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionComparison {
    /// Whether this comparison crossed the thresholds.
    pub changed: bool,
    /// Pixels that differed.
    pub changed_pixels: u64,
    /// Pixels compared.
    pub total_pixels: u64,
    /// Fraction that differed.
    pub changed_fraction: f64,
    /// Where the differences were.
    pub bounding_box: Option<crate::geometry::Rect>,
}

impl TransitionComparison {
    /// Summarize a Phase 2 comparison.
    pub fn from_comparison(comparison: crate::compare::Comparison) -> Self {
        TransitionComparison {
            changed: comparison.changed,
            changed_pixels: comparison.changed_pixels,
            total_pixels: comparison.total_pixels,
            changed_fraction: comparison.changed_fraction,
            bounding_box: comparison.bounding_box,
        }
    }
}

/// The returned final frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedFrame {
    /// Width of the returned image, in image pixels.
    pub width: u32,
    /// Height of the returned image, in image pixels.
    pub height: u32,
    /// IANA media type.
    pub media_type: String,
    /// Canonical format name.
    pub format: String,
    /// Size of the encoded image in bytes.
    pub byte_length: usize,
    /// Encoding of the inline data, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// Inline base64 image data, when requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// JPEG quality, when the image is a JPEG.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
}

impl ObservedFrame {
    /// Build from a prepared image and the options that produced it.
    pub fn from_prepared(
        prepared: &crate::pipeline::PreparedImage,
        options: &crate::pipeline::ImageOptions,
    ) -> Self {
        ObservedFrame {
            width: prepared.encoded.width,
            height: prepared.encoded.height,
            media_type: prepared.encoded.media_type().to_string(),
            format: prepared.encoded.format.name().to_string(),
            byte_length: prepared.encoded.bytes.len(),
            encoding: prepared.data.as_ref().map(|_| "base64".to_string()),
            data: prepared.data.clone(),
            quality: prepared.quality(options),
        }
    }
}

/// A complete temporal observation response.
///
/// The structure mirrors the capture response deliberately: `source` is native
/// source geometry, `image` is the returned image in its own pixels, and
/// `transform` maps between them. An observation adds `observation` to describe
/// the temporal outcome, plus the comparisons that explain it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationResponse {
    /// The session this observation ran in, for `eensh session observe`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The frame identifiers that mark the semantic points of the observation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<ObservationFrameIds>,
    /// What happened, temporally.
    pub observation: ObservationSection,
    /// Native source geometry of the observed target.
    pub source: SourceGeometry,
    /// Image-to-source coordinate mapping for the returned frame.
    pub transform: Transform,
    /// The returned frame.
    pub image: ObservedFrame,
    /// The relevant comparison: baseline-to-final for `wait-change`,
    /// consecutive for `wait-stable`, and the final settling comparison for
    /// `observe`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<TransitionComparison>,
    /// The comparison that first detected a transition, for `observe`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_change: Option<TransitionComparison>,
    /// Where the time went.
    pub timing: ObservationTimingSection,
}

/// The frame identifiers marking an observation's semantic points.
///
/// A caller should never have to infer which frames mattered from a count. These
/// are the true identifiers, and an older one may already have been evicted from
/// public history by the time the observation returns; retrieval of an evicted
/// frame reports `frame_not_available` rather than quietly substituting another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationFrameIds {
    /// The fixed baseline frame, for the operations that keep one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<crate::session::FrameId>,
    /// The frame that first showed a meaningful change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_change: Option<crate::session::FrameId>,
    /// The frame the operation returned.
    pub final_frame: crate::session::FrameId,
}

impl ObservationResponse {
    /// Serialize to compact JSON followed by a newline.
    pub fn to_json_string(&self) -> Result<String, crate::error::Error> {
        serde_json::to_string(self)
            .map(|mut text| {
                text.push('\n');
                text
            })
            .map_err(|e| {
                crate::error::Error::Internal(format!(
                    "observation response could not be serialized: {e}"
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{ImageFormat, PngEffort};
    use crate::geometry::CaptureTarget;
    use serde_json::{json, Value};

    fn encoded(width: u32, height: u32) -> EncodedImage {
        EncodedImage {
            bytes: vec![0u8; 128],
            format: ImageFormat::Jpeg,
            width,
            height,
        }
    }

    fn timing() -> Timing {
        Timing {
            capture_us: 910,
            resize_us: 620,
            encode_us: 3870,
            base64_us: 330,
            total_us: 5730,
        }
    }

    fn desktop_source() -> SourceGeometry {
        SourceGeometry::desktop(Some(":99".into()), 1920, 1080)
    }

    #[test]
    fn desktop_response_distinguishes_source_and_image_dimensions() {
        let source = desktop_source();
        let transform = Transform::new(&source.rect(), 960, 540).unwrap();
        let response = CaptureResponse::new(
            source,
            &encoded(960, 540),
            transform,
            timing(),
            Some("AAAA".into()),
            Some(75),
        );
        let value: Value = serde_json::from_str(&response.to_json_string().unwrap()).unwrap();

        // Source geometry is in source pixels.
        assert_eq!(value["source"]["kind"], "desktop");
        assert_eq!(value["source"]["display"], ":99");
        assert_eq!(value["source"]["width"], 1920);
        assert_eq!(value["source"]["height"], 1080);

        // Image geometry is in image pixels.
        assert_eq!(value["image"]["width"], 960);
        assert_eq!(value["image"]["height"], 540);
        assert_eq!(value["image"]["media_type"], "image/jpeg");
        assert_eq!(value["image"]["format"], "jpeg");
        assert_eq!(value["image"]["encoding"], "base64");
        assert_eq!(value["image"]["data"], "AAAA");
        assert_eq!(value["image"]["quality"], 75);

        // The transform is explicit.
        assert_eq!(value["transform"]["origin"], "top-left");
        assert_eq!(value["transform"]["scale_x"], 2.0);
        assert_eq!(value["transform"]["scale_y"], 2.0);
    }

    #[test]
    fn region_response_reports_the_region_origin_in_the_transform() {
        let source = SourceGeometry::region(Some(":99".into()), 100, 200, 800, 600);
        let transform = Transform::new(&source.rect(), 400, 300).unwrap();
        let response =
            CaptureResponse::new(source, &encoded(400, 300), transform, timing(), None, None);
        let value: Value = serde_json::from_str(&response.to_json_string().unwrap()).unwrap();

        assert_eq!(value["source"]["kind"], "region");
        assert_eq!(value["source"]["x"], 100);
        assert_eq!(value["source"]["y"], 200);
        assert_eq!(value["transform"]["offset_x"], 100);
        assert_eq!(value["transform"]["offset_y"], 200);
        assert_eq!(value["transform"]["scale_x"], 2.0);
        // Without --base64 there is no encoding or data.
        assert!(value["image"].get("encoding").is_none());
        assert!(value["image"].get("data").is_none());
    }

    #[test]
    fn unresized_capture_has_a_unit_transform() {
        let source =
            SourceGeometry::window(Some(":99".into()), "0x4600007".into(), 10, 20, 640, 480);
        let transform = Transform::new(&source.rect(), 640, 480).unwrap();
        let response =
            CaptureResponse::new(source, &encoded(640, 480), transform, timing(), None, None);
        let value: Value = serde_json::from_str(&response.to_json_string().unwrap()).unwrap();
        assert_eq!(value["source"]["kind"], "window");
        assert_eq!(value["source"]["id"], "0x4600007");
        assert_eq!(value["transform"]["scale_x"], 1.0);
        assert_eq!(value["transform"]["scale_y"], 1.0);
        assert_eq!(value["transform"]["offset_x"], 10);
        assert_eq!(value["transform"]["offset_y"], 20);
    }

    #[test]
    fn timings_are_present_for_every_stage() {
        let source = desktop_source();
        let transform = Transform::new(&source.rect(), 1920, 1080).unwrap();
        let response = CaptureResponse::new(
            source,
            &encoded(1920, 1080),
            transform,
            timing(),
            None,
            None,
        );
        let value: Value = serde_json::from_str(&response.to_json_string().unwrap()).unwrap();
        for field in [
            "capture_us",
            "resize_us",
            "encode_us",
            "base64_us",
            "total_us",
        ] {
            assert!(
                value["timing"][field].is_u64(),
                "timing.{field} missing or not an integer"
            );
        }
    }

    #[test]
    fn response_is_stable_across_serializations() {
        let source = desktop_source();
        let transform = Transform::new(&source.rect(), 960, 540).unwrap();
        let response = CaptureResponse::new(
            source,
            &encoded(960, 540),
            transform,
            timing(),
            Some("AAAA".into()),
            Some(80),
        );
        assert_eq!(
            response.to_json_string().unwrap(),
            response.to_json_string().unwrap()
        );
    }

    #[test]
    fn error_response_has_the_documented_shape() {
        let error = crate::error::Error::WindowNotFound {
            window_id: "0x1".into(),
        };
        assert_eq!(
            error.to_json(),
            json!({
                "error": {
                    "code": "window_not_found",
                    "message": "X11 window not found: 0x1",
                }
            })
        );
    }

    #[test]
    fn png_media_type_is_reported_for_png_output() {
        let source = desktop_source();
        let transform = Transform::new(&source.rect(), 100, 100).unwrap();
        let encoded = EncodedImage {
            bytes: vec![0u8; 10],
            format: ImageFormat::Png,
            width: 100,
            height: 100,
        };
        let response = CaptureResponse::new(source, &encoded, transform, timing(), None, None);
        let value: Value = serde_json::from_str(&response.to_json_string().unwrap()).unwrap();
        assert_eq!(value["image"]["media_type"], "image/png");
        assert_eq!(value["image"]["format"], "png");
        assert!(value["image"].get("quality").is_none());
        let _ = PngEffort::Default;
    }

    #[test]
    fn source_target_round_trips_through_json() {
        let cases = [
            CaptureTarget::Desktop,
            CaptureTarget::Region,
            CaptureTarget::Window { id: "0x2".into() },
        ];
        for target in cases {
            let source = SourceGeometry {
                target: target.clone(),
                display: Some(":99".into()),
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            };
            let text = serde_json::to_string(&source).unwrap();
            let back: SourceGeometry = serde_json::from_str(&text).unwrap();
            assert_eq!(back, source, "round trip failed for {target:?}");
        }
    }
}
