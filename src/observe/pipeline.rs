//! The `eensh wait-change | wait-stable | observe` orchestration.
//!
//! This mirrors [`crate::pipeline`] and [`crate::diff`] for temporal
//! observation. It is the only place that knows the whole flow:
//!
//! ```text
//! X11 / Xvfb
//!     -> FrameSource::capture            raw frames, repeatedly
//!     -> compare_frames                  raw frames only
//!     -> temporal state machine          observe/
//!     -> prepare and encode ONE frame    pipeline::prepare_image
//!     -> JSON / file / stdout
//! ```
//!
//! Two orderings here are load-bearing:
//!
//! * **Comparison runs at native captured resolution.** Comparing a resized
//!   agent-output view would make change detection depend on the output format,
//!   so `--width 960` changes what is returned, not what is compared.
//! * **Encoding runs once, on the frame the operation ended on.** Encoding every
//!   sample would destroy the latency the raw-frame design exists to protect.

use crate::capture::{x11, CaptureRequest, Display};
use crate::cli::{ObservationKind, ResolvedObserve, ResolvedWaitChange, ResolvedWaitStable};
use crate::error::Error;
use crate::frame::Frame;
use crate::geometry::SourceGeometry;
use crate::observe::{
    self, FrameSource, ObserveOptions, Outcome, WaitChangeOptions, WaitStableOptions,
};
use crate::output::json::{
    ObservationOutputTiming, ObservationResponse, ObservationSection, ObservationTimingSection,
    ObservedFrame, TransitionComparison,
};
use crate::pipeline;

/// A [`FrameSource`] that captures from an X11 display.
///
/// Each sample opens and closes its own connection. That is deliberately
/// unoptimised: Phase 3 is about getting the temporal semantics right, and Phase 4
/// exists to amortise this connection cost across samples without changing what
/// any of it means. Hiding the cost here would also make Phase 4's improvement
/// impossible to measure.
pub struct X11FrameSource {
    display_name: String,
    request: CaptureRequest,
    /// The target that was actually captured, recorded from the first sample.
    target: Option<SourceGeometry>,
}

impl X11FrameSource {
    /// Build a source for a capture request on a display.
    pub fn new(display_name: String, request: CaptureRequest) -> Self {
        X11FrameSource {
            display_name,
            request,
            target: None,
        }
    }

    /// The target that was captured, once a sample has been taken.
    pub fn target(&self) -> Option<&SourceGeometry> {
        self.target.as_ref()
    }
}

impl FrameSource for X11FrameSource {
    fn capture(&mut self) -> Result<Frame, Error> {
        let display = Display::open(&self.display_name)?;

        let frame = match &self.request {
            CaptureRequest::Desktop => x11::capture_desktop(&display),
            CaptureRequest::Region(rect) => x11::capture_region(&display, rect),
            CaptureRequest::Window(id) => x11::capture_window(&display, *id),
        }?;

        if self.target.is_none() {
            self.target = Some(frame.source_geometry.clone());
        }

        Ok(frame)
    }
}

/// The outcome of a temporal observation, with the final frame prepared for
/// output.
#[derive(Debug)]
pub struct ObservationOutcome {
    /// The completed response, whether or not it was written.
    pub response: ObservationResponse,
    /// The encoded final frame.
    pub encoded: Vec<u8>,
    /// Whether the encoded frame was written to the configured destination.
    pub image_written: bool,
    /// Whether the JSON response was written to a stream.
    pub metadata_written: bool,
}

/// Run a resolved `wait-change`.
pub fn run_wait_change(
    config: &ResolvedWaitChange,
    clock: &dyn observe::Clock,
) -> Result<ObservationOutcome, Error> {
    let mut source =
        X11FrameSource::new(config.target.display.clone(), config.target.request.clone());

    let result = observe::wait_for_change(
        &mut source,
        clock,
        &WaitChangeOptions {
            temporal: config.temporal,
        },
    )?;

    let section = ObservationSection {
        kind: ObservationKind::WaitChange,
        result: result.outcome,
        elapsed_ms: millis(result.elapsed),
        captures: result.captures,
        comparisons: result.comparisons,
        stable_for_ms: None,
        stable_duration_ms: None,
        change_detected_ms: None,
    };

    finish(
        config,
        section,
        result.frame,
        Some(result.comparison),
        None,
        result.timing,
    )
}

/// Run a resolved `wait-stable`.
pub fn run_wait_stable(
    config: &ResolvedWaitStable,
    clock: &dyn observe::Clock,
) -> Result<ObservationOutcome, Error> {
    let mut source =
        X11FrameSource::new(config.target.display.clone(), config.target.request.clone());

    let result = observe::wait_for_stable(
        &mut source,
        clock,
        &WaitStableOptions {
            temporal: config.temporal,
            stable_for: config.stable_for,
        },
    )?;

    let section = ObservationSection {
        kind: ObservationKind::WaitStable,
        result: result.outcome,
        elapsed_ms: millis(result.elapsed),
        captures: result.captures,
        comparisons: result.comparisons,
        stable_for_ms: Some(millis(config.stable_for)),
        stable_duration_ms: Some(millis(result.stable_duration)),
        change_detected_ms: None,
    };

    finish(
        config,
        section,
        result.frame,
        Some(result.last_comparison),
        None,
        result.timing,
    )
}

/// Run a resolved `observe`.
pub fn run_observe(
    config: &ResolvedObserve,
    clock: &dyn observe::Clock,
) -> Result<ObservationOutcome, Error> {
    let mut source =
        X11FrameSource::new(config.target.display.clone(), config.target.request.clone());

    let result = observe::observe(
        &mut source,
        clock,
        &ObserveOptions {
            temporal: config.temporal,
            stable_for: config.stable_for,
        },
    )?;

    let section = ObservationSection {
        kind: ObservationKind::Observe,
        result: result.outcome,
        elapsed_ms: millis(result.elapsed),
        captures: result.captures,
        comparisons: result.comparisons,
        stable_for_ms: Some(millis(config.stable_for)),
        stable_duration_ms: None,
        change_detected_ms: result.change_detected_at.map(millis),
    };

    finish(
        config,
        section,
        result.frame,
        Some(result.last_comparison),
        result.first_change,
        result.timing,
    )
}

fn millis(duration: std::time::Duration) -> u64 {
    duration.as_millis() as u64
}

/// Prepare the final frame and assemble the response.
///
/// The single encode for the whole operation happens here, after the state
/// machine has finished.
fn finish<C: ObservationConfig>(
    config: &C,
    section: ObservationSection,
    frame: Frame,
    comparison: Option<crate::compare::Comparison>,
    first_change: Option<crate::compare::Comparison>,
    observation_timing: observe::ObservationTiming,
) -> Result<ObservationOutcome, Error> {
    let options = config.image_options();
    let prepared = pipeline::prepare_image(frame, &options)?;

    let response = ObservationResponse {
        session_id: None,
        frames: None,
        observation: section,
        source: prepared.source_geometry.clone(),
        transform: prepared.transform,
        image: ObservedFrame::from_prepared(&prepared, &options),
        comparison: comparison.map(TransitionComparison::from_comparison),
        first_change: first_change.map(TransitionComparison::from_comparison),
        timing: ObservationTimingSection {
            captures: observation_timing.captures,
            comparisons: observation_timing.comparisons,
            capture_us_total: observation_timing.capture_us_total,
            compare_us_total: observation_timing.compare_us_total,
            sleep_us_total: observation_timing.sleep_us_total,
            encode: ObservationOutputTiming {
                encode_us: prepared.encode_us,
                resize_us: prepared.resize_us,
                base64_us: prepared.base64_us,
            },
        },
    };

    let plan = config.output();

    let mut image_written = false;
    if plan.write_image_bytes {
        plan.destination.write_image(&prepared.encoded.bytes)?;
        image_written = true;
    }

    let mut metadata_written = false;
    if !matches!(plan.metadata, crate::output::MetadataDestination::None) {
        let text = response.to_json_string()?;
        plan.metadata.write(&text)?;
        metadata_written = true;
    }

    Ok(ObservationOutcome {
        response,
        encoded: prepared.encoded.bytes,
        image_written,
        metadata_written,
    })
}

/// What the three observation commands have in common.
pub trait ObservationConfig {
    /// Image settings for the final returned frame.
    fn image_options(&self) -> pipeline::ImageOptions;
    /// Where the result goes.
    fn output(&self) -> &crate::output::OutputPlan;
}

impl ObservationConfig for ResolvedWaitChange {
    fn image_options(&self) -> pipeline::ImageOptions {
        self.image
    }
    fn output(&self) -> &crate::output::OutputPlan {
        &self.output
    }
}

impl ObservationConfig for ResolvedWaitStable {
    fn image_options(&self) -> pipeline::ImageOptions {
        self.image
    }
    fn output(&self) -> &crate::output::OutputPlan {
        &self.output
    }
}

impl ObservationConfig for ResolvedObserve {
    fn image_options(&self) -> pipeline::ImageOptions {
        self.image
    }
    fn output(&self) -> &crate::output::OutputPlan {
        &self.output
    }
}

/// Unused import guard: `Outcome` is part of the public vocabulary of results.
const _: Option<Outcome> = None;
