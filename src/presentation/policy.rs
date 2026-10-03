//! Presentation policy: what to render, in what form, and within what budget.
//!
//! These types are the *only* place a caller expresses a presentation decision. They
//! are deliberately structured rather than an opaque profile string: a policy that
//! resolves to `"agent-efficient"` inside the service is a policy a client cannot
//! reason about, inspect, or predict (requirement 38).
//!
//! Every type here is serializable, because the same policy has to travel over the
//! protocol, through the client, and onto the command line without three separate
//! definitions of what it means.

use serde::{Deserialize, Serialize};

use crate::cli::ResizeRequest;
use crate::encode::{ImageFormat, PngEffort};
use crate::error::Error;
use crate::geometry::Rect;

/// The name a budget fitting loop uses for quality steps.
///
/// Explicit ladders are what make fitting reproducible: a search that derived its
/// steps from the requested value would produce a different answer for two callers
/// who meant the same thing (requirement 67).
pub const QUALITY_LADDER: [u8; 6] = [85, 75, 65, 55, 45, 35];

/// The width ladder used by budget fitting, largest first.
pub const WIDTH_LADDER: [u32; 6] = [1920, 1280, 960, 640, 480, 320];

/// How a view should be resized, if at all.
///
/// Deliberately mirrors [`ResizeRequest`] rather than reusing it, so the policy can
/// be validated and serialized without depending on CLI argument types. The
/// conversion is in [`ResizePolicy::to_request`].
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResizePolicy {
    /// Native resolution.
    #[default]
    None,
    /// Scale to this width, preserving the aspect ratio.
    Width {
        /// Target width in image pixels.
        width: u32,
    },
    /// Scale to this height, preserving the aspect ratio.
    Height {
        /// Target height in image pixels.
        height: u32,
    },
    /// Scale by this factor.
    Scale {
        /// The factor.
        factor: f64,
    },
}

impl ResizePolicy {
    /// The pipeline's resize request.
    pub fn to_request(self) -> ResizeRequest {
        match self {
            ResizePolicy::None => ResizeRequest::None,
            ResizePolicy::Width { width } => ResizeRequest::Width(width),
            ResizePolicy::Height { height } => ResizeRequest::Height(height),
            ResizePolicy::Scale { factor } => ResizeRequest::Scale(factor),
        }
    }

    /// The requested width, when the policy names one.
    pub fn width(self) -> Option<u32> {
        match self {
            ResizePolicy::Width { width } => Some(width),
            _ => None,
        }
    }

    /// Validate the policy on its own terms.
    pub fn validate(self) -> Result<(), Error> {
        match self {
            ResizePolicy::None => Ok(()),
            ResizePolicy::Width { width: 0 } | ResizePolicy::Height { height: 0 } => Err(
                Error::invalid_presentation_policy("resize dimensions must be greater than zero"),
            ),
            ResizePolicy::Width { .. } | ResizePolicy::Height { .. } => Ok(()),
            ResizePolicy::Scale { factor } if !factor.is_finite() || factor <= 0.0 => Err(
                Error::invalid_presentation_policy("scale must be a positive, finite number"),
            ),
            ResizePolicy::Scale { .. } => Ok(()),
        }
    }
}

/// How one image should be presented.
///
/// This is the unit a budget fitter adjusts: everything about a view's output
/// lives here, so "degrade this view" is a matter of returning a different
/// `ImagePolicy` rather than mutating several fields in step.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ImagePolicy {
    /// Whether the view carries no image at all.
    ///
    /// A metadata-only view is how an older temporal frame keeps its place in the
    /// stack without paying for an encode (requirement 16). It is not "an empty
    /// image"; it is explicitly the absence of one, so it is represented explicitly
    /// rather than inferred from a combination of other fields.
    #[serde(default)]
    pub metadata_only: bool,
    /// Output format.
    pub format: ImageFormat,
    /// JPEG quality, ignored for PNG.
    pub quality: u8,
    /// PNG compression effort, ignored for JPEG.
    pub png_effort: PngEffort,
    /// Resize.
    pub resize: ResizePolicy,
    /// Whether to embed the image inline as base64.
    pub base64: bool,
    /// Floors a budget fitter may not go below.
    ///
    /// `None` means the fitter may reduce this view as far as it likes, including
    /// omitting it entirely when it is optional.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floors: Option<ImageFloors>,
}

impl Default for ImagePolicy {
    fn default() -> Self {
        ImagePolicy::png()
    }
}

impl ImagePolicy {
    /// Native-resolution PNG, inline.
    pub fn png() -> Self {
        ImagePolicy {
            metadata_only: false,
            format: ImageFormat::Png,
            quality: crate::encode::jpeg::DEFAULT_QUALITY,
            png_effort: PngEffort::Default,
            resize: ResizePolicy::None,
            base64: true,
            floors: None,
        }
    }

    /// PNG resized to a width.
    pub fn png_width(width: u32) -> Self {
        ImagePolicy {
            resize: ResizePolicy::Width { width },
            ..ImagePolicy::png()
        }
    }

    /// JPEG at native resolution.
    pub fn jpeg(quality: u8) -> Self {
        ImagePolicy {
            format: ImageFormat::Jpeg,
            quality,
            ..ImagePolicy::png()
        }
    }

    /// JPEG resized to a width.
    pub fn jpeg_width(width: u32, quality: u8) -> Self {
        ImagePolicy {
            format: ImageFormat::Jpeg,
            quality,
            resize: ResizePolicy::Width { width },
            ..ImagePolicy::png()
        }
    }

    /// No image at all: identity and timing only.
    pub fn metadata_only() -> Self {
        ImagePolicy {
            metadata_only: true,
            base64: false,
            ..ImagePolicy::png()
        }
    }

    /// Whether this policy produces no image.
    pub fn is_metadata_only(&self) -> bool {
        self.metadata_only
    }

    /// Validate the policy on its own terms.
    pub fn validate(&self, context: &str) -> Result<(), Error> {
        self.resize.validate().map_err(|error| {
            Error::invalid_presentation_policy(format!("{context}: {}", error.message()))
        })?;
        // A metadata-only view *describes* rather than *delivers*, so embedding contradicts it.
        // The two flags are mutually exclusive by construction in the builder methods, so this
        // fires only for a policy assembled by hand with both set — which is exactly the case
        // worth refusing, because the caller's intent could not be guessed and guessing it would
        // mean either silently sending pixels the caller said not to send, or silently sending
        // none where pixels were asked for.
        if self.metadata_only && self.base64 {
            return Err(Error::invalid_presentation_policy(format!(
                "{context}: `metadata_only` and `base64` contradict each other; a view cannot both 
                 describe itself and embed its image"
            )));
        }
        Ok(())
    }

    /// The pipeline image options this policy resolves to.
    pub fn to_image_options(&self) -> crate::pipeline::ImageOptions {
        crate::pipeline::ImageOptions {
            resize: self.resize.to_request(),
            format: self.format,
            quality: self.quality,
            png_effort: self.png_effort,
            base64: self.base64,
        }
    }

    /// A copy with a different quality.
    pub fn with_quality(mut self, quality: u8) -> Self {
        self.quality = quality;
        self
    }

    /// A copy with a different resize.
    pub fn with_resize(mut self, resize: ResizePolicy) -> Self {
        self.resize = resize;
        self
    }
}

/// The lowest settings a budget fitter may reduce a view to.
///
/// Floors are what make "protect the newest frame" enforceable rather than
/// aspirational: without them, a small budget would simply degrade the newest view
/// until it fit, which is exactly the outcome requirement 21 forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageFloors {
    /// Smallest width the fitter may reduce to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_width: Option<u32>,
    /// Lowest quality the fitter may reduce to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_quality: Option<u8>,
}

impl ImageFloors {
    /// Floors for an older temporal frame: it may be reduced aggressively.
    pub fn older() -> Self {
        ImageFloors {
            min_width: Some(320),
            min_quality: Some(40),
        }
    }

    /// Floors for the newest frame: it may be reduced, but not much.
    pub fn newest() -> Self {
        ImageFloors {
            min_width: Some(640),
            min_quality: Some(55),
        }
    }

    /// No floors: the fitter may reduce the view freely.
    pub fn none() -> Self {
        ImageFloors {
            min_width: None,
            min_quality: None,
        }
    }
}

/// Where a named region applies within a temporal stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegionScope {
    /// Every frame in the stack.
    #[default]
    All,
    /// Only the newest frame.
    Newest,
    /// Only the older frames, never the newest.
    Older,
}

/// A named region of the source frame, with its own output settings.
///
/// Region coordinates are **always native source coordinates**, never coordinates in
/// a resized overview (requirement 33). Keeping them in source space is what makes
/// the returned transform able to map a region back to the desktop, which is what a
/// future input-driving phase would need.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegionPolicy {
    /// The name this region is returned under.
    pub name: String,
    /// The source-space rectangle.
    pub rect: Rect,
    /// How to render it.
    pub image: ImagePolicy,
    /// Where in the stack it applies.
    #[serde(default)]
    pub scope: RegionScope,
    /// Whether a budget fitter may drop it.
    ///
    /// A required region may still be *degraded*, within its floors; it is only
    /// dropped when even its floors do not fit, and then the whole request fails
    /// rather than silently losing content the caller said it needed.
    pub required: bool,
    /// Relative priority when a budget forces a choice. Higher is kept longer.
    #[serde(default)]
    pub priority: u8,
}

impl RegionPolicy {
    /// A region that must survive, or the request fails.
    pub fn required(name: impl Into<String>, rect: Rect, image: ImagePolicy) -> Self {
        RegionPolicy {
            name: name.into(),
            rect,
            image,
            scope: RegionScope::All,
            required: true,
            priority: 200,
        }
    }

    /// A region a budget fitter may drop.
    pub fn optional(name: impl Into<String>, rect: Rect, image: ImagePolicy) -> Self {
        RegionPolicy {
            name: name.into(),
            rect,
            image,
            scope: RegionScope::All,
            required: false,
            priority: 120,
        }
    }

    /// A copy restricted to a different scope.
    pub fn with_scope(mut self, scope: RegionScope) -> Self {
        self.scope = scope;
        self
    }

    /// A copy with a different priority.
    pub fn with_priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }
}

/// How a temporal stack's frames should be rendered.
///
/// The variants are deliberately a small closed set rather than a general selector
/// language (requirement 18): an arbitrary predicate over frame indices would be
/// impossible to fit deterministically against a budget, and no agent needs one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TemporalFramePolicy {
    /// Every frame gets the same treatment.
    AllSame {
        /// The shared settings.
        image: ImagePolicy,
    },
    /// Older frames reduced, the newest preserved.
    ///
    /// This is the policy the Phase 5 measurements argue for: newest-frame freshness
    /// is dominated by encoding the older frames, which an agent usually needs only
    /// for motion context.
    NewestDetailed {
        /// Settings for every frame except the newest.
        older: ImagePolicy,
        /// Settings for the newest frame.
        newest: ImagePolicy,
    },
    /// Only the newest frame carries an image; the rest are metadata.
    NewestOnly {
        /// Settings for the newest frame.
        image: ImagePolicy,
    },
    /// Older frames are metadata; the newest carries an image.
    ///
    /// Equivalent in output to [`TemporalFramePolicy::NewestOnly`], but named
    /// separately because it says something different about intent: the older frames
    /// are *context*, not merely frames that happened not to be encoded. Kept because
    /// the two differ once regions are involved — a `NewestOnly` policy has no
    /// natural place for an older-scoped region, while `MetadataOlder` does.
    MetadataOlder {
        /// Settings for the newest frame.
        newest: ImagePolicy,
    },
}

impl TemporalFramePolicy {
    /// The image policy for a frame at `index` of `total` frames, oldest first.
    pub fn image_for(&self, index: usize, total: usize) -> ImagePolicy {
        let is_newest = total == 0 || index + 1 == total;
        match self {
            TemporalFramePolicy::AllSame { image } => *image,
            TemporalFramePolicy::NewestDetailed { older, newest } => {
                if is_newest {
                    *newest
                } else {
                    *older
                }
            }
            TemporalFramePolicy::NewestOnly { image } => {
                if is_newest {
                    *image
                } else {
                    ImagePolicy::metadata_only()
                }
            }
            TemporalFramePolicy::MetadataOlder { newest } => {
                if is_newest {
                    *newest
                } else {
                    ImagePolicy::metadata_only()
                }
            }
        }
    }

    /// Whether the frame at `index` of `total` carries an image at all.
    pub fn has_image_at(&self, index: usize, total: usize) -> bool {
        match self {
            TemporalFramePolicy::NewestOnly { .. } | TemporalFramePolicy::MetadataOlder { .. } => {
                total == 0 || index + 1 == total
            }
            _ => true,
        }
    }

    /// A name for the mode, used in the response's policy report.
    pub fn mode_name(&self) -> &'static str {
        match self {
            TemporalFramePolicy::AllSame { .. } => "all_same",
            TemporalFramePolicy::NewestDetailed { .. } => "newest_detailed",
            TemporalFramePolicy::NewestOnly { .. } => "newest_only",
            TemporalFramePolicy::MetadataOlder { .. } => "metadata_older",
        }
    }

    /// The settings used for the frames that are not the newest.
    pub fn older_image(&self) -> ImagePolicy {
        match self {
            TemporalFramePolicy::AllSame { image } => *image,
            TemporalFramePolicy::NewestDetailed { older, .. } => *older,
            TemporalFramePolicy::NewestOnly { .. } | TemporalFramePolicy::MetadataOlder { .. } => {
                ImagePolicy::metadata_only()
            }
        }
    }

    /// The settings used for the newest frame.
    pub fn newest_image(&self) -> ImagePolicy {
        match self {
            TemporalFramePolicy::AllSame { image } => *image,
            TemporalFramePolicy::NewestDetailed { newest, .. } => *newest,
            TemporalFramePolicy::NewestOnly { image } => *image,
            TemporalFramePolicy::MetadataOlder { newest } => *newest,
        }
    }
}

/// A visual payload budget, in base64 bytes.
///
/// One canonical unit, deliberately: a budget expressed in encoded bytes and one
/// expressed in base64 bytes would differ by about a third, and a caller reasoning
/// about what will fit in a message would be reasoning about the wrong number. The
/// base64 length is what actually travels, so that is what is bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadBudget {
    /// The most base64 bytes the visual payload may occupy.
    pub max_base64_bytes: usize,
}

impl PayloadBudget {
    /// A budget in base64 bytes.
    pub fn new(max_base64_bytes: usize) -> Self {
        PayloadBudget { max_base64_bytes }
    }
}

/// How a changed-region crop is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ChangedRegionPolicy {
    /// How much padding to add around the factual bounding box, in source pixels.
    ///
    /// Padding is clamped to the source bounds, and the returned rectangle always
    /// reports what was actually returned (requirement 12).
    pub padding: u32,
    /// How to render the crop.
    pub image: ImagePolicy,
    /// Whether to fall back to an overview when the changed area is large.
    ///
    /// A single Phase 2 bounding box can span several unrelated changes, and a box
    /// covering most of the screen is cheaper to send as an overview than as a crop
    /// at its own resolution (requirement 30).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fraction: Option<f64>,
    /// Whether the changed view must survive budget fitting.
    pub required: bool,
}

impl Default for ChangedRegionPolicy {
    fn default() -> Self {
        ChangedRegionPolicy {
            padding: 0,
            image: ImagePolicy::png(),
            max_fraction: None,
            required: true,
        }
    }
}

/// A complete presentation policy for one observation.
///
/// The overview, the regions, the temporal treatment, and the budget are one thing
/// because they compete for the same budget: fitting an overview down while leaving
/// a region untouched is only coherent if both are visible to the same decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationPolicy {
    /// The whole-frame view, when one is wanted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<ImagePolicy>,
    /// Named source-space regions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<RegionPolicy>,
    /// How a temporal stack's frames differ from one another.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporal: Option<TemporalFramePolicy>,
    /// The visual payload budget, when the caller set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_budget: Option<PayloadBudget>,
    /// How a changed-region crop is delivered, for `diff`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<ChangedRegionPolicy>,
}

impl Default for ObservationPolicy {
    fn default() -> Self {
        ObservationPolicy::phase5_default(ImagePolicy::png())
    }
}

impl ObservationPolicy {
    /// The Phase 5 behaviour: one whole-frame image, nothing else.
    ///
    /// This is what an omitted policy resolves to, which is what makes Phase 6
    /// strictly additive (requirement 37): a caller that knows nothing about
    /// presentation gets exactly what it got before.
    pub fn phase5_default(image: ImagePolicy) -> Self {
        ObservationPolicy {
            overview: Some(image),
            regions: Vec::new(),
            temporal: None,
            payload_budget: None,
            changed: None,
        }
    }

    /// Only a whole-frame view, with the given resize.
    pub fn overview_only(image: ImagePolicy) -> Self {
        ObservationPolicy::phase5_default(image)
    }

    /// A copy with regions attached.
    pub fn with_regions(mut self, regions: Vec<RegionPolicy>) -> Self {
        self.regions = regions;
        self
    }

    /// A copy with a budget attached.
    pub fn with_budget(mut self, budget: PayloadBudget) -> Self {
        self.payload_budget = Some(budget);
        self
    }

    /// A copy with a temporal treatment attached.
    pub fn with_temporal(mut self, temporal: TemporalFramePolicy) -> Self {
        self.temporal = Some(temporal);
        self
    }

    /// Whether this policy asks for any image at all.
    ///
    /// A metadata-only view still *counts* here. A caller may legitimately ask for
    /// identity and timing with no pixels, and refusing that would remove a documented
    /// behaviour; what this predicate rules out is a policy that asks for nothing at
    /// all, which could not produce a meaningful response.
    pub fn requests_any_image(&self) -> bool {
        self.overview.is_some()
            || !self.regions.is_empty()
            || self.temporal.is_some()
            || self.changed.is_some()
    }

    /// Validate the policy as a whole.
    ///
    /// Rejected here rather than at use, so a malformed policy costs no capture and
    /// is reported as `invalid_presentation_policy` naming the offending part.
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(overview) = &self.overview {
            overview.validate("overview")?;
        }
        if let Some(temporal) = &self.temporal {
            temporal.older_image().validate("temporal.older")?;
            temporal.newest_image().validate("temporal.newest")?;
        }
        if let Some(changed) = &self.changed {
            changed.image.validate("changed")?;
            if let Some(fraction) = changed.max_fraction {
                if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
                    return Err(Error::invalid_presentation_policy(format!(
                        "changed.max_fraction must be between 0.0 and 1.0, got {fraction}"
                    )));
                }
            }
        }

        let mut names: Vec<&str> = Vec::with_capacity(self.regions.len());
        for region in &self.regions {
            if region.name.is_empty() {
                return Err(Error::invalid_presentation_policy(
                    "a region must have a name; an unnamed region could not be referred to \
                     in a response",
                ));
            }
            if region.name == "overview" {
                return Err(Error::invalid_presentation_policy(
                    "a region may not be named \"overview\": that name is reserved for the \
                     whole-frame view, and a response naming both would be ambiguous",
                ));
            }
            if names.contains(&region.name.as_str()) {
                return Err(Error::invalid_presentation_policy(format!(
                    "region {:?} is declared more than once; a response could not say which \
                     is which",
                    region.name
                )));
            }
            if region.rect.width == 0 || region.rect.height == 0 {
                return Err(Error::invalid_presentation_policy(format!(
                    "region {:?} has zero area ({}x{}); a zero-sized region can never \
                     correspond to a real image",
                    region.name, region.rect.width, region.rect.height
                )));
            }
            region
                .image
                .validate(&format!("region {:?}", region.name))?;
            names.push(&region.name);
        }

        if let Some(budget) = &self.payload_budget {
            if budget.max_base64_bytes == 0 {
                return Err(Error::invalid_presentation_policy(
                    "a payload budget of zero can never be met; omit the budget to disable \
                     fitting",
                ));
            }
        }

        if !self.requests_any_image() {
            return Err(Error::invalid_presentation_policy(
                "the policy requests no image at all: enable an overview, a region, a \
                 temporal image, or a changed view",
            ));
        }

        Ok(())
    }

    /// Whether a budget fitter is needed.
    pub fn has_budget(&self) -> bool {
        self.payload_budget.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_phase5_behaviour() {
        let policy = ObservationPolicy::default();
        assert!(policy.overview.is_some());
        assert!(policy.regions.is_empty());
        assert!(policy.temporal.is_none());
        assert!(policy.payload_budget.is_none());
        assert!(!policy.has_budget());
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn temporal_newest_detailed_distinguishes_the_newest_from_the_rest() {
        let policy = TemporalFramePolicy::NewestDetailed {
            older: ImagePolicy::jpeg_width(480, 55),
            newest: ImagePolicy::jpeg_width(960, 75),
        };

        assert_eq!(policy.image_for(0, 3), ImagePolicy::jpeg_width(480, 55));
        assert_eq!(policy.image_for(1, 3), ImagePolicy::jpeg_width(480, 55));
        assert_eq!(policy.image_for(2, 3), ImagePolicy::jpeg_width(960, 75));
        assert_eq!(policy.mode_name(), "newest_detailed");
    }

    #[test]
    fn a_single_frame_stack_treats_frame_zero_as_the_newest() {
        let policy = TemporalFramePolicy::NewestDetailed {
            older: ImagePolicy::jpeg_width(480, 55),
            newest: ImagePolicy::jpeg_width(960, 75),
        };
        assert_eq!(
            policy.image_for(0, 1),
            ImagePolicy::jpeg_width(960, 75),
            "with one frame, the only frame is the newest"
        );
    }

    #[test]
    fn newest_only_marks_older_frames_as_metadata() {
        let policy = TemporalFramePolicy::NewestOnly {
            image: ImagePolicy::jpeg_width(960, 75),
        };
        assert!(!policy.has_image_at(0, 3));
        assert!(!policy.has_image_at(1, 3));
        assert!(policy.has_image_at(2, 3));
        assert!(policy.image_for(0, 3).is_metadata_only());
    }

    #[test]
    fn metadata_older_behaves_like_newest_only_in_its_images_but_names_itself_differently() {
        let a = TemporalFramePolicy::NewestOnly {
            image: ImagePolicy::jpeg_width(960, 75),
        };
        let b = TemporalFramePolicy::MetadataOlder {
            newest: ImagePolicy::jpeg_width(960, 75),
        };
        assert_eq!(a.newest_image(), b.newest_image());
        assert_eq!(a.older_image(), b.older_image());
        assert_ne!(a.mode_name(), b.mode_name());
    }

    #[test]
    fn a_duplicate_region_name_is_rejected() {
        let policy = ObservationPolicy {
            overview: None,
            regions: vec![
                RegionPolicy::required("a", Rect::new(0, 0, 4, 4).unwrap(), ImagePolicy::png()),
                RegionPolicy::required("a", Rect::new(0, 0, 4, 4).unwrap(), ImagePolicy::png()),
            ],
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        let error = policy.validate().unwrap_err();
        assert_eq!(error.code(), "invalid_presentation_policy");
    }

    #[test]
    fn a_reserved_region_name_is_rejected() {
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
        assert!(policy.validate().is_err());
    }

    #[test]
    fn a_zero_budget_is_rejected_because_it_can_never_be_met() {
        let policy = ObservationPolicy::default().with_budget(PayloadBudget::new(0));
        let error = policy.validate().unwrap_err();
        assert!(
            error.message().contains("budget of zero"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn a_policy_asking_for_nothing_is_rejected() {
        // No overview, no regions, no temporal treatment, no changed view: there is
        // nothing here to present, and a response built from it would be a frame with
        // no views. Refused as a malformed policy rather than silently returning an
        // empty presentation.
        let policy = ObservationPolicy {
            overview: None,
            regions: Vec::new(),
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        let error = policy.validate().unwrap_err();
        assert_eq!(error.code(), "invalid_presentation_policy");
        assert!(
            error.message().contains("requests no image"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn a_metadata_only_policy_is_accepted_and_reports_itself_as_such() {
        // The other side of the same predicate, and the reason it is written the way it
        // is: asking for identity and timing with no pixels is a real request, not a
        // vacuous one. Spec section 47 lists metadata-only output as a required
        // behaviour, so a policy that asks for it must validate and must say so.
        let image = ImagePolicy::metadata_only();
        let policy = ObservationPolicy {
            overview: None,
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::NewestOnly { image }),
            payload_budget: None,
            changed: None,
        };
        // Accepted: a temporal treatment is present, so the policy asks for something.
        assert!(
            policy.validate().is_ok(),
            "{}",
            policy.validate().unwrap_err()
        );

        // And the request is visible in the policy rather than having to be inferred.
        assert!(policy
            .temporal
            .as_ref()
            .expect("a temporal policy")
            .newest_image()
            .is_metadata_only());
    }

    #[test]
    fn changed_fraction_bounds_are_enforced() {
        for fraction in [-0.1, 1.5, f64::NAN] {
            let policy = ObservationPolicy {
                overview: Some(ImagePolicy::png()),
                regions: Vec::new(),
                temporal: None,
                payload_budget: None,
                changed: Some(ChangedRegionPolicy {
                    max_fraction: Some(fraction),
                    ..ChangedRegionPolicy::default()
                }),
            };
            assert!(
                policy.validate().is_err(),
                "fraction {fraction} should be rejected"
            );
        }
    }

    #[test]
    fn resize_policies_reject_degenerate_values() {
        assert!(ResizePolicy::Width { width: 0 }.validate().is_err());
        assert!(ResizePolicy::Height { height: 0 }.validate().is_err());
        assert!(ResizePolicy::Scale { factor: 0.0 }.validate().is_err());
        assert!(ResizePolicy::Scale { factor: -1.0 }.validate().is_err());
        assert!(ResizePolicy::Scale {
            factor: f64::INFINITY
        }
        .validate()
        .is_err());
        assert!(ResizePolicy::Width { width: 10 }.validate().is_ok());
        assert!(ResizePolicy::None.validate().is_ok());
    }

    #[test]
    fn a_policy_round_trips_through_json() {
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(960, 75)),
            regions: vec![RegionPolicy::required(
                "hud",
                Rect::new(0, 900, 1920, 180).unwrap(),
                ImagePolicy::png(),
            )
            .with_scope(RegionScope::Newest)],
            temporal: Some(TemporalFramePolicy::NewestDetailed {
                older: ImagePolicy::jpeg_width(480, 55),
                newest: ImagePolicy::jpeg_width(960, 75),
            }),
            payload_budget: Some(PayloadBudget::new(400_000)),
            changed: None,
        };

        let text = serde_json::to_string(&policy).unwrap();
        let back: ObservationPolicy = serde_json::from_str(&text).unwrap();
        assert_eq!(policy, back);
    }

    #[test]
    fn floors_are_directional_so_the_newest_is_protected_more_than_the_old() {
        let older = ImageFloors::older();
        let newest = ImageFloors::newest();
        assert!(
            newest.min_width.unwrap() > older.min_width.unwrap(),
            "the newest frame must have a higher resolution floor"
        );
        assert!(
            newest.min_quality.unwrap() > older.min_quality.unwrap(),
            "the newest frame must have a higher quality floor"
        );
    }
}
