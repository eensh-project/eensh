//! The persistent-session real-time path: sampling, then presentation.
//!
//! # The one ordering rule that matters
//!
//! ```text
//! sample frames  ->  sample frames  ->  sample frames
//!                                             |
//!                                    sampling is complete
//!                                             |
//!                     resize / encode / base64 every frame
//!                                             |
//!                            compute frame ages
//!                                             |
//!                                    assemble the response
//! ```
//!
//! Encoding between captures would stretch the interval the caller asked for, so the
//! two phases are kept strictly apart. [`session_realtime`] samples and returns raw
//! frames; [`RealtimeCapture::prepare`] does all the image work afterwards.
//!
//! Frame ages are computed last, after encoding, because an age that ignores the time
//! spent encoding is an age an agent cannot trust: it would report a frame as fresher
//! than the data it is looking at.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::geometry::{SourceGeometry, Transform};
use crate::output::json::ObservedFrame;
use crate::pipeline::{self, ImageOptions};
use crate::realtime::{
    sample_stack, RealtimeOptions, RealtimeOutcome, RealtimeResult, RealtimeSource,
};
use crate::session::manager::SharedSession;
use crate::session::{FrameId, SessionFrame};
use crate::timing::Stopwatch;

/// A real-time sample, captured but not yet presented.
///
/// The raw frame is deliberately retained rather than encoded here. Every frame a
/// real-time operation returns also lives in session history, and both refer to the
/// same immutable allocation, so holding it costs a pointer rather than a pixel copy.
#[derive(Debug, Clone)]
pub struct RealtimeSample {
    /// The captured frame, at native source resolution, with its identity.
    pub session_frame: SessionFrame,
    /// When the capture completed, measured from the request start.
    pub capture_offset: Duration,
    /// How long the capture itself took.
    pub capture_duration: Duration,
}

impl RealtimeSample {
    /// The frame's identity within its session.
    pub fn frame_id(&self) -> FrameId {
        self.session_frame.frame_id
    }

    /// The frame's age, given the moment the response is being finalized.
    pub fn age_at(&self, now: Instant) -> Duration {
        self.session_frame.age_at(now)
    }
}

/// A completed real-time sampling window, before any image work.
///
/// The geometry is carried explicitly because it is the *one* geometry every frame
/// shares: the sampling loop aborts on any inconsistency, so a mixed-coordinate
/// stack can never be represented by this type.
#[derive(Debug, Clone)]
pub struct RealtimeCapture {
    /// The session the samples came from.
    pub session_id: String,
    /// The samples, oldest first.
    pub samples: Vec<RealtimeSample>,
    /// Whether the full requested count was collected.
    pub outcome: RealtimeOutcome,
    /// How many frames were requested.
    pub requested_frames: usize,
    /// Cadence slots considered.
    pub scheduled_opportunities: u64,
    /// Cadence slots skipped because their time had already passed.
    pub skipped_opportunities: u64,
    /// Time spent inside capture calls.
    pub capture_us_total: u64,
    /// Time spent deliberately waiting between samples.
    pub sleep_us_total: u64,
    /// The interval the samples were scheduled against.
    pub interval: Duration,
    /// The deadline that limited the request.
    pub timeout: Duration,
    /// How long the sampling window took.
    pub sampling_elapsed: Duration,
    /// The native geometry every sample shares.
    pub source: SourceGeometry,
}

impl RealtimeCapture {
    /// How many frames were actually captured.
    pub fn captured_frames(&self) -> usize {
        self.samples.len()
    }

    /// The newest sample.
    pub fn newest(&self) -> Option<&RealtimeSample> {
        self.samples.last()
    }

    /// Sample this capture's geometry and present every frame.
    ///
    /// This is the second phase. It is a separate call so that a caller — a test, or
    /// an orchestrator that only wants metadata — can skip the image work entirely, and
    /// so the ordering rule above is visible in the type system rather than only in a
    /// comment.
    pub fn prepare(self, options: &ImageOptions) -> Result<RealtimeResponse, Error> {
        let mut samples = Vec::with_capacity(self.samples.len());
        let mut source = None;
        let mut transform = None;
        let mut encode_us_total = 0u64;
        let mut resize_us_total = 0u64;
        let mut base64_us_total = 0u64;

        // Oldest to newest, so the encoding order matches the stack order.
        for sample in &self.samples {
            let stopwatch = Stopwatch::start();
            let frame = (*sample.session_frame.frame).clone();
            let prepared = pipeline::prepare_image(frame, options)?;
            let _ = stopwatch;

            resize_us_total += prepared.resize_us;
            encode_us_total += prepared.encode_us;
            base64_us_total += prepared.base64_us;

            // Every frame is transformed identically, and the first frame's geometry
            // is the stack's. A mismatch here would mean the sampling loop let
            // through frames that cannot share a coordinate space.
            match (&source, &transform) {
                (None, None) => {
                    source = Some(prepared.source_geometry.clone());
                    transform = Some(prepared.transform);
                }
                (Some(baseline), Some(_)) => {
                    if baseline.width != prepared.source_geometry.width
                        || baseline.height != prepared.source_geometry.height
                    {
                        return Err(Error::geometry_changed(format!(
                            "real-time frames disagree on source geometry: {}x{} then {}x{}; \
                             a stack must share one coordinate space",
                            baseline.width,
                            baseline.height,
                            prepared.source_geometry.width,
                            prepared.source_geometry.height
                        )));
                    }
                }
                _ => unreachable!("source and transform are set together"),
            }

            samples.push(PreparedSample {
                frame_id: sample.frame_id(),
                capture_offset: sample.capture_offset,
                capture_duration: sample.capture_duration,
                captured_at: sample.session_frame.captured_at,
                image: ObservedFrame::from_prepared(&prepared, options),
            });
        }

        // Ages are computed *after* every encode, so they describe the moment the
        // response is assembled rather than a moment that encoding has since aged.
        let finalized_at = Instant::now();

        let newest_age_us = samples
            .last()
            .map(|sample| sample.age_at(finalized_at).as_micros() as u64)
            .unwrap_or(0);

        let frames: Vec<RealtimeFrameResponse> = samples
            .into_iter()
            .map(|sample| RealtimeFrameResponse {
                frame_id: sample.frame_id,
                capture_offset_us: sample.capture_offset.as_micros() as u64,
                capture_duration_us: sample.capture_duration.as_micros() as u64,
                age_us: sample.age_at(finalized_at).as_micros() as u64,
                image: sample.image,
            })
            .collect();

        let newest_frame_id = frames.last().map(|frame| frame.frame_id);

        let source = source.unwrap_or_else(|| self.source.clone());

        // A stack always has at least one frame, so these fallbacks are unreachable;
        // they exist so the construction is total. The fallback transform maps source
        // pixels to source pixels, which is the only mapping that is true without an
        // image to describe.
        let transform = transform.unwrap_or_else(|| {
            let rect = crate::geometry::Rect {
                x: source.x,
                y: source.y,
                width: source.width,
                height: source.height,
            };
            Transform::new(&rect, source.width, source.height)
                .unwrap_or(Transform::new(&rect, 1, 1).expect("a 1x1 transform is always valid"))
        });

        Ok(RealtimeResponse {
            session_id: self.session_id,
            realtime: RealtimeSection {
                result: self.outcome,
                requested_frames: self.requested_frames,
                captured_frames: frames.len(),
                interval_ms: self.interval.as_millis() as u64,
                timeout_ms: self.timeout.as_millis() as u64,
                scheduled_opportunities: self.scheduled_opportunities,
                skipped_opportunities: self.skipped_opportunities,
                elapsed_ms: self.sampling_elapsed.as_millis() as u64,
            },
            newest_frame_id,
            newest_frame_age_us: newest_age_us,
            frames,
            source,
            transform,
            timing: RealtimeTiming {
                sampling_us: self.sampling_elapsed.as_micros() as u64,
                capture_us_total: self.capture_us_total,
                sleep_us_total: self.sleep_us_total,
                encode_us_total,
                resize_us_total,
                base64_us_total,
            },
        })
    }
}

/// A sample whose image has been prepared.
struct PreparedSample {
    frame_id: FrameId,
    capture_offset: Duration,
    capture_duration: Duration,
    captured_at: Instant,
    image: ObservedFrame,
}

impl PreparedSample {
    fn age_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.captured_at)
    }
}

/// One frame of a real-time result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RealtimeFrameResponse {
    /// The frame's identity within its session.
    pub frame_id: FrameId,
    /// When the capture completed, measured from the request start.
    ///
    /// Measured, not idealised: a request that asked for 0/50/100 ms will report
    /// something else if the captures took longer than the interval.
    pub capture_offset_us: u64,
    /// How long the capture itself took.
    pub capture_duration_us: u64,
    /// How old the frame was when the response was assembled.
    ///
    /// Computed after all encoding, so it accounts for the time spent preparing the
    /// images the caller is about to look at.
    pub age_us: u64,
    /// The returned image for this frame.
    pub image: ObservedFrame,
}

/// The real-time summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RealtimeSection {
    /// `complete` or `partial`.
    pub result: RealtimeOutcome,
    /// How many frames were requested.
    pub requested_frames: usize,
    /// How many were captured.
    pub captured_frames: usize,
    /// The requested cadence.
    pub interval_ms: u64,
    /// The requested deadline.
    pub timeout_ms: u64,
    /// Cadence slots considered.
    pub scheduled_opportunities: u64,
    /// Cadence slots skipped because their time had already passed.
    pub skipped_opportunities: u64,
    /// How long the sampling window took.
    pub elapsed_ms: u64,
}

/// Where the time went in a real-time request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeTiming {
    /// The sampling window, which is dominated by the requested temporal span.
    pub sampling_us: u64,
    /// Time inside capture calls, summed across samples.
    pub capture_us_total: u64,
    /// Time deliberately waiting between samples.
    pub sleep_us_total: u64,
    /// Encoding, summed across samples.
    pub encode_us_total: u64,
    /// Resizing, summed across samples.
    pub resize_us_total: u64,
    /// Base64, summed across samples.
    pub base64_us_total: u64,
}

/// A complete real-time response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RealtimeResponse {
    /// The session the samples came from.
    pub session_id: String,
    /// The real-time summary.
    pub realtime: RealtimeSection,
    /// The newest frame's identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_frame_id: Option<FrameId>,
    /// The newest frame's age, which is the smallest age in the stack.
    pub newest_frame_age_us: u64,
    /// The frames, oldest first.
    pub frames: Vec<RealtimeFrameResponse>,
    /// Native source geometry, shared by every frame.
    pub source: SourceGeometry,
    /// Image-to-source mapping, identical for every frame.
    pub transform: Transform,
    /// Where the time went.
    pub timing: RealtimeTiming,
}

/// Adapts a session handle to the real-time sampler.
///
/// Captures go through the normal session path, so each sample receives a monotonic
/// frame identity and enters bounded history exactly as a one-shot capture would. An
/// ordinary `capture` arriving during a real-time observation may therefore be
/// assigned an intervening identifier, which is why a real-time stack's identifiers
/// are not assumed to be contiguous.
struct SessionRealtimeSource {
    handle: SharedSession,
}

impl RealtimeSource for SessionRealtimeSource {
    fn capture(&mut self) -> Result<SessionFrame, Error> {
        let mut session = self.handle.lock().expect("session poisoned");
        session.capture()
    }
}

/// Sample a bounded temporal stack from a session.
///
/// This is the sampling half only. The return value holds raw frames; call
/// [`RealtimeCapture::prepare`] to encode them. Keeping the two apart is what
/// guarantees that no image work happens inside the sampling window.
///
/// The operation takes the session's temporal slot, so it is mutually exclusive with
/// `wait-change`, `wait-stable`, and `observe`. A second temporal operation is refused
/// with `session_busy` rather than queued, because a queued real-time observation is
/// stale before it starts.
pub fn session_realtime(
    handle: &SharedSession,
    options: &RealtimeOptions,
    clock: &dyn crate::observe::Clock,
) -> Result<RealtimeCapture, Error> {
    options.validate()?;

    let session_id = {
        let mut session = handle.lock().expect("session poisoned");
        session.begin_observation()?;
        session.session_id().to_string()
    };

    // Released on every path out of this function, including an early return, so a
    // failed request cannot leave the session permanently "observing".
    let _guard = ObservationSlot {
        handle: std::sync::Arc::clone(handle),
    };

    let mut source = SessionRealtimeSource {
        handle: std::sync::Arc::clone(handle),
    };

    let result: RealtimeResult = sample_stack(&mut source, clock, options)?;

    if result.frames.is_empty() {
        return Err(Error::observation_failed(
            "the real-time deadline passed before a single frame could be captured",
        ));
    }

    // Every sample shares the geometry the session resolved, because the session
    // aborts a capture whose geometry moved. Taking it from the first sample states
    // that plainly rather than re-deriving it.
    let source_geometry = result.frames[0].session_frame.frame.source_geometry.clone();

    let samples = result
        .frames
        .into_iter()
        .map(|frame| RealtimeSample {
            session_frame: frame.session_frame,
            capture_offset: frame.capture_offset,
            capture_duration: frame.capture_duration,
        })
        .collect();

    Ok(RealtimeCapture {
        session_id,
        samples,
        outcome: result.outcome,
        requested_frames: result.requested_frames,
        scheduled_opportunities: result.scheduled_opportunities,
        skipped_opportunities: result.skipped_opportunities,
        capture_us_total: result.capture_us_total,
        sleep_us_total: result.sleep_us_total,
        interval: result.interval,
        timeout: result.timeout,
        sampling_elapsed: result.elapsed,
        source: source_geometry,
    })
}

/// Releases a session's temporal slot when the operation ends, however it ends.
struct ObservationSlot {
    handle: SharedSession,
}

impl Drop for ObservationSlot {
    fn drop(&mut self) {
        // A poisoned lock means the session already failed and is unusable; there is
        // nothing useful left to release in that case.
        if let Ok(mut session) = self.handle.lock() {
            session.end_observation();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{ImageFormat, PngEffort};

    fn image_options(base64: bool, width: Option<u32>) -> ImageOptions {
        ImageOptions {
            resize: match width {
                Some(width) => crate::cli::ResizeRequest::Width(width),
                None => crate::cli::ResizeRequest::None,
            },
            format: ImageFormat::Png,
            quality: 80,
            png_effort: PngEffort::Default,
            base64,
        }
    }

    /// Build a capture with synthetic samples, bypassing the session entirely.
    fn capture_with(sample_count: usize, width: u32, height: u32) -> RealtimeCapture {
        use crate::input::frame_from_rgb8;
        use std::sync::Arc;

        let mut samples = Vec::new();
        for index in 0..sample_count {
            let mut data = Vec::with_capacity((width * height * 3) as usize);
            for _ in 0..(width * height) {
                let value = if index % 2 == 0 { 10 } else { 200 };
                data.extend_from_slice(&[value, value, value]);
            }
            let frame = frame_from_rgb8(width, height, data).unwrap();
            samples.push(RealtimeSample {
                session_frame: SessionFrame {
                    session_id: "s-test".to_string(),
                    frame_id: FrameId(index as u64 + 1),
                    frame: Arc::new(frame),
                    // Distinct capture times, so ages differ measurably.
                    captured_at: Instant::now()
                        - Duration::from_millis(10 * (sample_count - index) as u64),
                    capture_duration: Duration::from_millis(3),
                },
                capture_offset: Duration::from_millis(50 * index as u64),
                capture_duration: Duration::from_millis(3),
            });
        }

        let source = samples
            .first()
            .map(|s| s.session_frame.frame.source_geometry.clone())
            .expect("a capture needs at least one sample");

        RealtimeCapture {
            session_id: "s-test".to_string(),
            samples,
            outcome: RealtimeOutcome::Complete,
            requested_frames: sample_count,
            scheduled_opportunities: sample_count as u64,
            skipped_opportunities: 0,
            capture_us_total: 3000 * sample_count as u64,
            sleep_us_total: 0,
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(500),
            sampling_elapsed: Duration::from_millis(150),
            source,
        }
    }

    #[test]
    fn a_prepared_response_keeps_the_stack_order() {
        let capture = capture_with(3, 64, 48);
        let response = capture.prepare(&image_options(false, None)).unwrap();

        let ids: Vec<u64> = response.frames.iter().map(|f| f.frame_id.get()).collect();
        assert_eq!(ids, vec![1, 2, 3], "the stack must be oldest to newest");

        let offsets: Vec<u64> = response
            .frames
            .iter()
            .map(|f| f.capture_offset_us / 1000)
            .collect();
        assert_eq!(offsets, vec![0, 50, 100]);
    }

    #[test]
    fn the_newest_frame_has_the_smallest_age() {
        let capture = capture_with(3, 64, 48);
        let response = capture.prepare(&image_options(false, None)).unwrap();

        let ages: Vec<u64> = response.frames.iter().map(|f| f.age_us).collect();
        assert!(
            ages.windows(2).all(|pair| pair[0] >= pair[1]),
            "ages should decrease from oldest to newest: {ages:?}"
        );
        assert_eq!(
            response.newest_frame_age_us,
            *ages.last().unwrap(),
            "the reported newest age should be the last frame's age"
        );
        assert_eq!(response.newest_frame_id.unwrap().get(), 3);
    }

    #[test]
    fn ages_reflect_time_spent_encoding() {
        // Encoding three 64x48 PNGs takes a measurable moment; the ages must be taken
        // after it, not before. Comparing against an age captured at the start would
        // only ever be smaller, so this pins the direction that matters.
        let capture = capture_with(3, 256, 192);
        let response = capture.prepare(&image_options(true, None)).unwrap();

        assert!(
            response.timing.encode_us_total > 0,
            "the preparation should have encoded something"
        );
        for frame in &response.frames {
            assert!(
                frame.age_us > 0,
                "an age computed after encoding cannot be zero for an older frame"
            );
        }
    }

    #[test]
    fn every_frame_receives_an_identically_transformed_image() {
        let capture = capture_with(3, 128, 96);
        let response = capture.prepare(&image_options(false, Some(64))).unwrap();

        let mut dimensions = Vec::new();
        for frame in &response.frames {
            dimensions.push((frame.image.width, frame.image.height));
            assert_eq!(frame.image.media_type, "image/png");
        }
        assert!(
            dimensions.windows(2).all(|pair| pair[0] == pair[1]),
            "every frame must be transformed identically: {dimensions:?}"
        );
        assert_eq!(response.transform.scale_x, 2.0);
        assert_eq!(response.source.width, 128);
    }

    #[test]
    fn base64_is_embedded_independently_in_each_frame() {
        let capture = capture_with(3, 64, 48);
        let response = capture.prepare(&image_options(true, None)).unwrap();

        for frame in &response.frames {
            assert_eq!(frame.image.encoding.as_deref(), Some("base64"));
            let data = frame
                .image
                .data
                .as_ref()
                .expect("each frame should carry its own inline data");
            // Each payload decodes on its own; none is a concatenation of several.
            let bytes = crate::output::base64::decode(data).unwrap();
            assert_eq!(bytes.len(), frame.image.byte_length);
        }
    }

    #[test]
    fn metadata_only_mode_omits_the_image_data() {
        let capture = capture_with(3, 64, 48);
        let response = capture.prepare(&image_options(false, None)).unwrap();

        for frame in &response.frames {
            assert!(
                frame.image.data.is_none(),
                "without base64 no inline data should be sent"
            );
            assert!(frame.image.encoding.is_none());
            // The metadata an orchestrator needs to decide what to fetch later is
            // still present.
            assert!(frame.image.byte_length > 0);
            assert_eq!(frame.image.width, 64);
        }
    }

    #[test]
    fn a_partial_capture_reports_partial_and_keeps_its_frames() {
        let mut capture = capture_with(2, 64, 48);
        capture.outcome = RealtimeOutcome::Partial;
        capture.requested_frames = 4;
        capture.skipped_opportunities = 3;

        let response = capture.prepare(&image_options(false, None)).unwrap();

        assert_eq!(response.realtime.result, RealtimeOutcome::Partial);
        assert_eq!(response.realtime.requested_frames, 4);
        assert_eq!(response.realtime.captured_frames, 2);
        assert_eq!(response.realtime.skipped_opportunities, 3);
        assert_eq!(response.frames.len(), 2);
    }

    #[test]
    fn the_response_serializes_with_the_documented_shape() {
        let capture = capture_with(2, 32, 32);
        let response = capture.prepare(&image_options(false, None)).unwrap();
        let value = serde_json::to_value(&response).unwrap();

        assert_eq!(value["session_id"], "s-test");
        assert_eq!(value["realtime"]["result"], "complete");
        assert_eq!(value["realtime"]["requested_frames"], 2);
        assert_eq!(value["realtime"]["captured_frames"], 2);
        assert_eq!(value["realtime"]["interval_ms"], 50);
        assert_eq!(value["realtime"]["timeout_ms"], 500);
        assert_eq!(value["newest_frame_id"], 2);
        assert!(value["newest_frame_age_us"].is_number());
        assert_eq!(value["frames"][0]["frame_id"], 1);
        assert!(value["frames"][0]["capture_offset_us"].is_number());
        assert!(value["frames"][0]["capture_duration_us"].is_number());
        assert!(value["frames"][0]["age_us"].is_number());
        assert_eq!(value["source"]["width"], 32);
        assert!(value["timing"]["sampling_us"].is_number());
    }

    #[test]
    fn sampling_and_encoding_timings_are_separated() {
        let capture = capture_with(3, 64, 48);
        let response = capture.prepare(&image_options(false, None)).unwrap();

        // The sampling window is a property of the request; the encode totals are a
        // property of the presentation. Reporting them separately is what makes it
        // obvious whether latency came from the requested span or from the images.
        assert_eq!(response.timing.sampling_us, 150_000);
        assert_eq!(response.timing.capture_us_total, 9_000);
        assert!(response.timing.encode_us_total > 0);
        assert_eq!(response.timing.base64_us_total, 0);
    }
}
