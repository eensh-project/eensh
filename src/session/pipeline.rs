//! Session responses and the persistent-session observation path.
//!
//! This is where Phase 4 stops being plumbing and starts being visible: a capture
//! that carries a frame identity and an age, a retained frame that can be
//! re-encoded without recapturing, and an observation that reports which frames
//! its semantic points corresponded to.
//!
//! The observation path is deliberately thin. It builds a [`SessionFrameSource`]
//! and calls the Phase 3 state machine unchanged. If this file ever grows a second
//! opinion about what "changed" or "stable" means, the equivalence tests in
//! `tests/session_x11.rs` exist to fail.

use serde::{Deserialize, Serialize};

use crate::compare::Comparison;
use crate::error::Error;
use crate::geometry::Transform;
use crate::observe::{self, ObserveOptions, WaitChangeOptions, WaitStableOptions};
use crate::output::json::{
    ObservationFrameIds, ObservationOutputTiming, ObservationResponse, ObservationSection,
    ObservationTimingSection, ObservedFrame, TransitionComparison,
};
use crate::pipeline::{self, ImageOptions};
use crate::session::manager::{SessionFrameSource, SharedSession};
use crate::session::{FrameId, SessionFrame, SessionInfo};
use crate::timing::Stopwatch;

/// A capture or retrieval result for one frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionFrameResponse {
    /// The session that owns the frame.
    pub session_id: String,
    /// The frame's identity.
    pub frame_id: FrameId,
    /// How long ago the frame was captured, when the request was answered.
    pub frame_age_us: u64,
    /// Whether this request performed a physical capture.
    ///
    /// `latest` and `frame` return `false`; `capture` returns `true`. The
    /// distinction is the whole difference between "show me what you have" and
    /// "look now", so it is stated explicitly rather than inferred.
    pub fresh_capture: bool,
    /// Native source geometry of the frame.
    pub source: crate::geometry::SourceGeometry,
    /// Image-to-source mapping for the returned image.
    pub transform: Transform,
    /// The returned image.
    pub image: ObservedFrame,
    /// Timings for this request.
    pub timing: SessionCaptureTiming,
}

/// Per-request timings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCaptureTiming {
    /// The physical capture, when one happened.
    pub capture_us: u64,
    /// Resize, when the returned image was transformed.
    pub resize_us: u64,
    /// Encode.
    pub encode_us: u64,
    /// Base64.
    pub base64_us: u64,
}

/// What was asked of a session frame, which decides whether it was captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRequest {
    /// Perform a new capture.
    Capture,
    /// Return the newest retained frame.
    Latest,
    /// Return a specific retained frame.
    ById(FrameId),
}

impl FrameRequest {
    /// Whether this request performs a physical capture.
    pub fn is_capture(&self) -> bool {
        matches!(self, FrameRequest::Capture)
    }
}

/// Capture or retrieve a frame from a session and prepare it for output.
///
/// This is the single entry point behind `session capture`, `session latest`, and
/// `session frame`, because all three differ only in where the frame comes from.
pub fn session_frame(
    handle: &SharedSession,
    request: FrameRequest,
    options: &ImageOptions,
) -> Result<SessionFrameResponse, Error> {
    let mut ledger = SessionCaptureTiming::default();

    let session_frame = {
        let mut session = handle.lock().expect("session poisoned");
        match request {
            FrameRequest::Capture => {
                let stopwatch = Stopwatch::start();
                let frame = session.capture()?;
                ledger.capture_us = stopwatch.elapsed_us();
                frame
            }
            FrameRequest::Latest => session.latest()?,
            FrameRequest::ById(id) => session.frame(id)?,
        }
    };

    build_frame_response(session_frame, request, options, ledger)
}

/// Prepare a captured or retrieved frame for output.
fn build_frame_response(
    session_frame: SessionFrame,
    request: FrameRequest,
    options: &ImageOptions,
    mut ledger: SessionCaptureTiming,
) -> Result<SessionFrameResponse, Error> {
    let session_id = session_frame.session_id.clone();
    let frame_id = session_frame.frame_id;
    let frame_age_us = session_frame.age_at(std::time::Instant::now()).as_micros() as u64;

    // The frame is shared with history, so this clone is a handle copy rather
    // than a pixel copy.
    let frame = (*session_frame.frame).clone();
    let prepared = pipeline::prepare_image(frame, options)?;

    ledger.resize_us = prepared.resize_us;
    ledger.encode_us = prepared.encode_us;
    ledger.base64_us = prepared.base64_us;

    Ok(SessionFrameResponse {
        session_id,
        frame_id,
        frame_age_us,
        fresh_capture: request.is_capture(),
        source: prepared.source_geometry.clone(),
        transform: prepared.transform,
        image: ObservedFrame::from_prepared(&prepared, options),
        timing: ledger,
    })
}

/// A session-scoped observation result.
///
/// Phase 3's response shape is preserved and session identity is added
/// additively, with the frame identifiers that mark the semantic points.
#[derive(Debug)]
pub struct SessionObservationOutcome {
    /// The Phase 3 response, extended with session and frame identity.
    pub response: ObservationResponse,
    /// The encoded final frame.
    pub encoded: Vec<u8>,
}

/// The frame identifiers observed during a session observation.
///
/// Collected from the source, which is the only component that sees every frame
/// arrive. The state machine reports outcomes, not identities, so identities have
/// to be recorded where the frames actually are.
#[derive(Debug, Default)]
struct ObservedFrames {
    baseline: Option<FrameId>,
    first_change: Option<FrameId>,
    final_frame: Option<FrameId>,
}

impl ObservedFrames {
    /// Read the identities the source recorded.
    fn from_source(source: &SessionFrameSource) -> Self {
        ObservedFrames {
            baseline: source.baseline_frame_id(),
            first_change: source.first_change_frame_id(),
            final_frame: source.last_frame_id(),
        }
    }
}

/// Run `wait-change` inside a session.
pub fn session_wait_change(
    handle: &SharedSession,
    options: &WaitChangeOptions,
    image: &ImageOptions,
    clock: &dyn observe::Clock,
) -> Result<SessionObservationOutcome, Error> {
    let mut source = SessionFrameSource::new(std::sync::Arc::clone(handle))?;

    let result = observe::wait_for_change(&mut source, clock, options)?;

    let observed = ObservedFrames::from_source(&source);

    let section = ObservationSection {
        kind: crate::cli::ObservationKind::WaitChange,
        result: result.outcome,
        elapsed_ms: result.elapsed.as_millis() as u64,
        captures: result.captures,
        comparisons: result.comparisons,
        stable_for_ms: None,
        stable_duration_ms: None,
        change_detected_ms: None,
    };

    finish(
        &source,
        handle,
        section,
        result.frame,
        Some(result.comparison),
        None,
        observed,
        result.timing,
        image,
    )
}

/// Run `wait-stable` inside a session.
pub fn session_wait_stable(
    handle: &SharedSession,
    options: &WaitStableOptions,
    image: &ImageOptions,
    clock: &dyn observe::Clock,
) -> Result<SessionObservationOutcome, Error> {
    let mut source = SessionFrameSource::new(std::sync::Arc::clone(handle))?;

    let result = observe::wait_for_stable(&mut source, clock, options)?;

    let observed = ObservedFrames::from_source(&source);

    let section = ObservationSection {
        kind: crate::cli::ObservationKind::WaitStable,
        result: result.outcome,
        elapsed_ms: result.elapsed.as_millis() as u64,
        captures: result.captures,
        comparisons: result.comparisons,
        stable_for_ms: Some(options.stable_for.as_millis() as u64),
        stable_duration_ms: Some(result.stable_duration.as_millis() as u64),
        change_detected_ms: None,
    };

    finish(
        &source,
        handle,
        section,
        result.frame,
        Some(result.last_comparison),
        None,
        observed,
        result.timing,
        image,
    )
}

/// Run `observe` inside a session.
pub fn session_observe(
    handle: &SharedSession,
    options: &ObserveOptions,
    image: &ImageOptions,
    clock: &dyn observe::Clock,
) -> Result<SessionObservationOutcome, Error> {
    // `observe` is the only operation with a first change to report, so it is the
    // only one that asks the source to track which frame showed it. The tracking
    // uses the same comparison options the state machine uses, so it reaches the
    // same conclusion by construction rather than by a second opinion.
    let mut source = SessionFrameSource::tracking_change(
        std::sync::Arc::clone(handle),
        options.temporal.compare,
    )?;

    let result = observe::observe(&mut source, clock, options)?;

    let observed = ObservedFrames::from_source(&source);

    let section = ObservationSection {
        kind: crate::cli::ObservationKind::Observe,
        result: result.outcome,
        elapsed_ms: result.elapsed.as_millis() as u64,
        captures: result.captures,
        comparisons: result.comparisons,
        stable_for_ms: Some(options.stable_for.as_millis() as u64),
        stable_duration_ms: None,
        change_detected_ms: result.change_detected_at.map(|d| d.as_millis() as u64),
    };

    finish(
        &source,
        handle,
        section,
        result.frame,
        Some(result.last_comparison),
        result.first_change,
        observed,
        result.timing,
        image,
    )
}

/// Prepare the final frame and assemble the response.
#[allow(clippy::too_many_arguments)]
fn finish(
    source: &SessionFrameSource,
    handle: &SharedSession,
    section: ObservationSection,
    frame: crate::frame::Frame,
    comparison: Option<Comparison>,
    first_change: Option<Comparison>,
    observed: ObservedFrames,
    observation_timing: observe::ObservationTiming,
    options: &ImageOptions,
) -> Result<SessionObservationOutcome, Error> {
    let prepared = pipeline::prepare_image(frame, options)?;

    let session_id = source.session_id().to_string();
    let final_frame_id = observed
        .final_frame
        .or_else(|| {
            handle
                .lock()
                .expect("session poisoned")
                .history()
                .latest_frame_id()
        })
        .ok_or_else(|| Error::no_frame_available("the observation captured no frame"))?;

    let frames = ObservationFrameIds {
        baseline: observed.baseline,
        first_change: observed.first_change,
        final_frame: final_frame_id,
    };

    let response = ObservationResponse {
        session_id: Some(session_id),
        frames: Some(frames),
        observation: section,
        source: prepared.source_geometry.clone(),
        transform: prepared.transform,
        image: ObservedFrame::from_prepared(&prepared, options),
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

    Ok(SessionObservationOutcome {
        response,
        encoded: prepared.encoded.bytes,
    })
}

/// The result of closing a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClosed {
    /// The closed session.
    pub session_id: String,
    /// Always `closed`; present so a caller can assert on the state.
    pub state: String,
}

/// A service health report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    /// The protocol version the service speaks.
    pub protocol_version: u32,
    /// The service's package version.
    pub version: String,
    /// Registered sessions.
    pub sessions: usize,
    /// How long the service has been running.
    pub uptime_ms: u64,
}

/// A listing of registered sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionList {
    /// One entry per session.
    pub sessions: Vec<SessionInfo>,
}

/// The outcome of a comparison between two retained frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionDiffResponse {
    /// The session.
    pub session_id: String,
    /// The earlier frame.
    pub before: FrameId,
    /// The later frame.
    pub after: FrameId,
    /// The comparison, using the Phase 2 engine unchanged.
    pub comparison: Comparison,
    /// How long the comparison took.
    pub compare_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_requests_distinguish_capture_from_retrieval() {
        assert!(FrameRequest::Capture.is_capture());
        assert!(!FrameRequest::Latest.is_capture());
        assert!(!FrameRequest::ById(FrameId(3)).is_capture());
    }

    #[test]
    fn a_frame_response_serializes_its_identity_and_freshness() {
        let response = SessionFrameResponse {
            session_id: "s-1".to_string(),
            frame_id: FrameId(1842),
            frame_age_us: 1850,
            fresh_capture: true,
            source: crate::geometry::SourceGeometry::desktop(Some(":99".into()), 100, 50),
            transform: Transform {
                origin: crate::geometry::TransformOrigin::TopLeft,
                offset_x: 0,
                offset_y: 0,
                scale_x: 1.0,
                scale_y: 1.0,
            },
            image: ObservedFrame {
                width: 100,
                height: 50,
                media_type: "image/png".to_string(),
                format: "png".to_string(),
                byte_length: 12,
                encoding: None,
                data: None,
                quality: None,
            },
            timing: SessionCaptureTiming::default(),
        };

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["session_id"], "s-1");
        assert_eq!(value["frame_id"], 1842);
        assert_eq!(value["frame_age_us"], 1850);
        assert_eq!(value["fresh_capture"], true);
    }

    #[test]
    fn a_retrieval_response_reports_that_it_did_not_capture() {
        let timing = SessionCaptureTiming {
            capture_us: 0,
            ..SessionCaptureTiming::default()
        };
        assert!(!FrameRequest::Latest.is_capture());
        assert_eq!(timing.capture_us, 0, "a retrieval performs no capture");
    }

    #[test]
    fn a_closed_response_names_the_session() {
        let closed = SessionClosed {
            session_id: "s-1".to_string(),
            state: "closed".to_string(),
        };
        let value = serde_json::to_value(&closed).unwrap();
        assert_eq!(value["session_id"], "s-1");
        assert_eq!(value["state"], "closed");
    }
}
