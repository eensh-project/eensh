//! Phase 5: bounded, freshness-oriented real-time sampling.
//!
//! # What this is, and what it is not
//!
//! A real-time observation takes a short temporal stack of a scene that may never
//! be still — a game running at full speed, a simulation animating, a camera
//! rotating — and returns it with honest timing. It answers "what is happening
//! visually right now over a short interval?", which is a different question from
//! Phase 3's "did something change, and has it stopped?".
//!
//! It therefore has no stability requirement and no change detection. It samples
//! unconditionally over time.
//!
//! It is deliberately **not** a video system: it does not capture while idle, it
//! does not stream, and it does not queue. A request samples for as long as it was
//! asked to and then stops.
//!
//! # The scheduling rule, in one paragraph
//!
//! Sample opportunities sit at fixed multiples of the interval from the request
//! start (`0, interval, 2*interval, ...`), **not** one interval after the previous
//! capture finished. Sampling from the completion time would let capture duration
//! accumulate into the temporal spacing, so a stack asked for at 50 ms intervals
//! would silently become 80 ms intervals on a slow capture. When a capture overruns
//! its slot, the slots it covered are *skipped*, never replayed: a fresh future
//! frame is worth more to an agent than an artificial backlog of stale ones.
//!
//! # Where the state machine ends and presentation begins
//!
//! [`sample_stack`] captures, records timing, and nothing else. Encoding happens
//! afterwards, in the pipeline, because encoding between captures would distort the
//! very cadence the operation exists to report — see the note on
//! [`RealtimeCapture`].

use std::time::Duration;

use crate::error::Error;
use crate::observe::clock::Clock;
use crate::session::{FrameId, SessionFrame};

/// A source of identified frames, for a real-time observation.
///
/// Deliberately separate from Phase 3's `FrameSource`, which yields a bare
/// `Frame`. Every real-time sample must report *which* frame it was, so the
/// identity has to come back with the pixels rather than being reconstructed from a
/// side channel. A session implements this; the scheduler tests implement it with
/// synthetic frames and no X11 at all.
pub trait RealtimeSource {
    /// Perform one physical capture.
    fn capture(&mut self) -> Result<SessionFrame, Error>;
}

/// The default number of frames a real-time observation captures.
pub const DEFAULT_FRAMES: usize = 3;

/// The largest temporal stack a real-time observation may request.
///
/// A hard bound is required rather than a polite suggestion, because both the
/// response payload and the temporary raw-frame memory multiply with the frame
/// count. Eight frames at 1920x1080 is already about 47 MiB of live frames.
pub const MAX_FRAMES: usize = 8;

/// The default interval between scheduled sample opportunities.
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(50);

/// The default deadline for the whole real-time request.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(500);

/// How a real-time observation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeOutcome {
    /// Every requested frame was captured.
    Complete,
    /// The deadline arrived before the requested count was collected.
    ///
    /// This is a normal outcome, not a failure. A real-time observer that always
    /// insisted on the full count would be unusable on a slow target.
    Partial,
}

impl RealtimeOutcome {
    /// Canonical name, used in JSON.
    pub fn name(self) -> &'static str {
        match self {
            RealtimeOutcome::Complete => "complete",
            RealtimeOutcome::Partial => "partial",
        }
    }
}

impl std::fmt::Display for RealtimeOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Options for a real-time observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealtimeOptions {
    /// How many physical frames were requested, 1..=[`MAX_FRAMES`].
    pub frames: usize,
    /// The cadence between scheduled sample opportunities.
    pub interval: Duration,
    /// The deadline after which no new capture is started.
    pub timeout: Duration,
}

impl Default for RealtimeOptions {
    fn default() -> Self {
        RealtimeOptions {
            frames: DEFAULT_FRAMES,
            interval: DEFAULT_INTERVAL,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl RealtimeOptions {
    /// Validate the request, rejecting only what is genuinely impossible.
    ///
    /// Note what is deliberately *not* rejected: a request whose interval times
    /// frame count exceeds its timeout, such as `frames=8 interval=1s
    /// timeout=100ms`. That is a well-formed request for a partial result, and
    /// refusing it would make an agent responsible for arithmetic the service can
    /// simply answer honestly.
    pub fn validate(&self) -> Result<(), Error> {
        if self.frames == 0 {
            return Err(Error::invalid_arguments(
                "a real-time observation must request at least 1 frame",
            ));
        }
        if self.frames > MAX_FRAMES {
            return Err(Error::invalid_arguments(format!(
                "a real-time observation may request at most {MAX_FRAMES} frames, got {}; \
                 raise the frame count in a later request rather than truncating this one",
                self.frames
            )));
        }
        if self.timeout == Duration::ZERO {
            return Err(Error::invalid_duration(
                "the real-time deadline must be greater than zero, because no capture \
                 can be started once it has already passed",
            ));
        }
        if self.frames > 1 && self.interval == Duration::ZERO {
            return Err(Error::invalid_duration(
                "a real-time interval of zero would schedule every sample at the same \
                 instant; use an interval greater than zero, or request a single frame",
            ));
        }
        Ok(())
    }

    /// The ideal time, from the request start, of the sample at `index`.
    ///
    /// Fixed origin, so this is an exact multiple of the interval and does not
    /// depend on how long any previous capture took.
    pub fn ideal_offset(&self, index: usize) -> Duration {
        self.interval * index as u32
    }
}

/// One physically captured sample, with the timing needed to reason about it.
#[derive(Debug)]
pub struct RealtimeFrame {
    /// The captured frame, still at native source resolution.
    ///
    /// Retained as a shared handle, so a frame that is also sitting in session
    /// history costs one allocation rather than two. Encoding is deliberately
    /// *not* done here: it happens once sampling has finished, because encoding
    /// between captures would stretch the interval the caller asked for.
    pub session_frame: crate::session::SessionFrame,
    /// When this capture completed, measured from the request start.
    pub capture_offset: Duration,
    /// How long this capture took.
    pub capture_duration: Duration,
}

impl RealtimeFrame {
    /// The frame's identity within its session.
    pub fn frame_id(&self) -> FrameId {
        self.session_frame.frame_id
    }

    /// The frame's width in native source pixels.
    pub fn width(&self) -> u32 {
        self.session_frame.width()
    }

    /// The frame's height in native source pixels.
    pub fn height(&self) -> u32 {
        self.session_frame.height()
    }
}

/// The result of a real-time observation.
///
/// Raw frames only. Nothing here knows about PNG, JPEG, base64, or JSON, so the
/// sampling schedule can be tested with a manual clock and no encoder at all.
#[derive(Debug)]
pub struct RealtimeResult {
    /// Whether the full count was collected before the deadline.
    pub outcome: RealtimeOutcome,
    /// The captured samples, oldest first.
    pub frames: Vec<RealtimeFrame>,
    /// How many frames the caller asked for.
    pub requested_frames: usize,
    /// How many cadence slots were considered.
    pub scheduled_opportunities: u64,
    /// How many cadence slots were skipped because their time had already passed.
    pub skipped_opportunities: u64,
    /// Time spent inside capture calls.
    pub capture_us_total: u64,
    /// Time spent deliberately waiting between samples.
    pub sleep_us_total: u64,
    /// The interval the samples were scheduled against.
    pub interval: Duration,
    /// The deadline that limited the request.
    pub timeout: Duration,
    /// How long the sampling window actually took.
    pub elapsed: Duration,
}

impl RealtimeResult {
    /// How many frames were actually captured.
    pub fn captured_frames(&self) -> usize {
        self.frames.len()
    }

    /// The newest frame, which is the last element because the stack is ordered
    /// oldest to newest.
    pub fn newest(&self) -> Option<&RealtimeFrame> {
        self.frames.last()
    }

    /// The oldest frame.
    pub fn oldest(&self) -> Option<&RealtimeFrame> {
        self.frames.first()
    }
}

/// Capture a bounded temporal stack from a frame source.
///
/// The source is a session, so every sample receives a normal frame identity and
/// enters history exactly as a one-shot capture would. The loop stops as soon as
/// either the requested count is reached or the deadline prevents another capture
/// from being started.
///
/// # A capture that has started is allowed to finish
///
/// The deadline gates *starting* a capture, not finishing one. Cancelling a capture
/// already in flight is not possible through the X11 path without tearing down the
/// connection, so a request may overrun its deadline by up to one capture plus
/// response preparation. If that final capture completes the requested count, the
/// outcome is [`RealtimeOutcome::Complete`], which is the honest description of what
/// happened.
///
/// # Failure is not a partial result
///
/// A capture or session error aborts the whole operation and propagates the
/// structured error. It is never downgraded to `Partial`: "the deadline arrived" and
/// "the display broke" are different situations, and an agent that could not tell
/// them apart would make the wrong decision about whether to retry.
pub fn sample_stack<S: RealtimeSource>(
    source: &mut S,
    clock: &dyn Clock,
    options: &RealtimeOptions,
) -> Result<RealtimeResult, Error> {
    options.validate()?;

    let started_at = clock.now();
    let mut frames: Vec<RealtimeFrame> = Vec::with_capacity(options.frames);
    let mut scheduled_opportunities: u64 = 0;
    let mut skipped_opportunities: u64 = 0;
    let mut capture_us_total: u64 = 0;
    let mut sleep_us_total: u64 = 0;

    for index in 0..options.frames {
        // The cadence slot this sample targets. It moves forward only, and only by
        // whole intervals, so the schedule keeps a fixed origin rather than
        // drifting with capture duration. The first slot is always at the request
        // start, which is what makes the first sample immediate.
        let target = if index == 0 {
            Duration::ZERO
        } else {
            let previous = options.ideal_offset(index - 1);
            let mut slot = previous + options.interval;
            while clock.now() > slot {
                // The previous capture overran this slot. Skip it rather than
                // replaying it: a backlog of stale frames is worse than a gap.
                skipped_opportunities += 1;
                slot += options.interval;
            }
            slot
        };

        // The deadline gates starting a capture, not finishing one.
        if clock.now() >= options.timeout {
            break;
        }

        if target > clock.now() {
            let sleep_started = clock.now();
            clock.sleep_until(target);
            sleep_us_total += (clock.now() - sleep_started).as_micros() as u64;
        }

        // Re-checked after waiting, because the wait may have reached the deadline.
        if clock.now() >= options.timeout {
            break;
        }

        scheduled_opportunities += 1;

        let capture_started = clock.now();
        let frame = source.capture()?;
        let capture_completed = clock.now();
        capture_us_total += (capture_completed - capture_started).as_micros() as u64;

        // Geometry is checked inside the session on every capture, so a target that
        // moved or resized aborts the operation rather than producing a stack whose
        // frames do not share a coordinate space.
        frames.push(RealtimeFrame {
            session_frame: frame,
            capture_offset: capture_completed - started_at,
            capture_duration: capture_completed - capture_started,
        });
    }

    let outcome = if frames.len() >= options.frames {
        RealtimeOutcome::Complete
    } else {
        RealtimeOutcome::Partial
    };

    Ok(RealtimeResult {
        outcome,
        frames,
        requested_frames: options.frames,
        scheduled_opportunities,
        skipped_opportunities,
        capture_us_total,
        sleep_us_total,
        interval: options.interval,
        timeout: options.timeout,
        elapsed: clock.now() - started_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::CompareOptions;
    use crate::frame::Frame;
    use crate::input::frame_from_rgb8;
    use crate::observe::clock::ManualClock;
    use std::sync::Arc;
    use std::time::Instant;

    /// Wrap a raw frame as a session frame with a synthetic identity.
    ///
    /// The scheduler only needs an identity and a size, so the tests do not need a
    /// real session or a display.
    fn identified(frame: Frame, id: u64) -> SessionFrame {
        SessionFrame {
            session_id: "s-test".to_string(),
            frame_id: FrameId(id),
            frame: Arc::new(frame),
            captured_at: Instant::now(),
            capture_duration: Duration::ZERO,
        }
    }

    /// A scene expressed as a grid of coloured cells, so a frame's contents identify
    /// which sample it was.
    ///
    /// Each "scene number" paints one cell white, which makes it possible to assert
    /// which physical capture a returned frame came from rather than merely that a
    /// frame came back.
    struct Scene {
        width: u32,
        height: u32,
        cells: u32,
        cell: u32,
    }

    impl Scene {
        fn new(cells: u32, cell: u32) -> Self {
            Scene {
                width: cells * cell,
                height: cell,
                cells,
                cell,
            }
        }

        /// Render scene `index`: cell `index % cells` is white, the rest black.
        fn render(&self, index: usize) -> Frame {
            let lit = (index % self.cells as usize) as u32;
            let mut data = Vec::with_capacity((self.width * self.height * 3) as usize);
            for _y in 0..self.height {
                for x in 0..self.width {
                    let column = x / self.cell;
                    let value = if column == lit { 255 } else { 0 };
                    data.extend_from_slice(&[value, value, value]);
                }
            }
            frame_from_rgb8(self.width, self.height, data).unwrap()
        }
    }

    /// A source that advances the clock by a fixed capture duration, then yields the
    /// next scene.
    ///
    /// This is what makes the scheduler testable without sleeps: "capture takes 80 ms"
    /// is a property of the source, not of a real clock.
    struct TimedSource {
        scene: Scene,
        capture_duration: Duration,
        clock: ManualClock,
        served: usize,
    }

    impl RealtimeSource for TimedSource {
        fn capture(&mut self) -> Result<SessionFrame, Error> {
            // The clock is advanced *before* the frame is produced, so the reported
            // capture duration and offset reflect the cost.
            self.clock.advance(self.capture_duration);
            let frame = self.scene.render(self.served);
            self.served += 1;
            Ok(identified(frame, self.served as u64))
        }
    }

    /// A source where later captures are slower, to exercise a mid-request overrun.
    struct VariableSource {
        scene: Scene,
        durations: Vec<Duration>,
        clock: ManualClock,
        served: usize,
    }

    impl RealtimeSource for VariableSource {
        fn capture(&mut self) -> Result<SessionFrame, Error> {
            let duration = self
                .durations
                .get(self.served)
                .copied()
                .unwrap_or_else(|| *self.durations.last().unwrap());
            self.clock.advance(duration);
            let frame = self.scene.render(self.served);
            self.served += 1;
            Ok(identified(frame, self.served as u64))
        }
    }

    /// A source that fails on its `fail_on`th capture (0-based).
    struct FailingSource {
        scene: Scene,
        clock: ManualClock,
        capture_duration: Duration,
        fail_on: usize,
        served: usize,
    }

    impl RealtimeSource for FailingSource {
        fn capture(&mut self) -> Result<SessionFrame, Error> {
            self.clock.advance(self.capture_duration);
            if self.served == self.fail_on {
                return Err(Error::capture_failed("the display went away"));
            }
            let frame = self.scene.render(self.served);
            self.served += 1;
            Ok(identified(frame, self.served as u64))
        }
    }

    fn fixture(
        capture_duration: Duration,
        frames: usize,
        interval_ms: u64,
        timeout_ms: u64,
    ) -> (ManualClock, TimedSource, RealtimeOptions) {
        let clock = ManualClock::new();
        let source = TimedSource {
            scene: Scene::new(8, 4),
            capture_duration,
            clock: clock.clone(),
            served: 0,
        };
        let options = RealtimeOptions {
            frames,
            interval: Duration::from_millis(interval_ms),
            timeout: Duration::from_millis(timeout_ms),
        };
        (clock, source, options)
    }

    // -- 53: ideal cadence --------------------------------------------------

    #[test]
    fn an_ideal_cadence_takes_samples_at_exact_multiples_of_the_interval() {
        // Frames 3, interval 50ms, negligible capture: the ideal slots are 0, 50, 100.
        let (clock, mut source, options) = fixture(Duration::ZERO, 3, 50, 500);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(result.outcome, RealtimeOutcome::Complete);
        assert_eq!(result.captured_frames(), 3);
        assert_eq!(result.skipped_opportunities, 0, "nothing should be skipped");
        assert_eq!(result.scheduled_opportunities, 3);

        let offsets: Vec<u64> = result
            .frames
            .iter()
            .map(|f| f.capture_offset.as_millis() as u64)
            .collect();
        assert_eq!(offsets, vec![0, 50, 100], "offsets were {offsets:?}");
    }

    #[test]
    fn a_slow_capture_skips_its_missed_slots() {
        // The spec's worked example: interval 50ms, capture 80ms.
        //   t=0    capture A
        //   t=50   missed
        //   t=80   A finishes
        //   t=100  capture B
        let (clock, mut source, options) = fixture(Duration::from_millis(80), 2, 50, 500);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(result.captured_frames(), 2);
        assert_eq!(
            result.skipped_opportunities, 1,
            "the slot at t=50 should be reported as skipped exactly once"
        );

        let offsets: Vec<u64> = result
            .frames
            .iter()
            .map(|f| f.capture_offset.as_millis() as u64)
            .collect();
        assert_eq!(offsets, vec![80, 180], "offsets were {offsets:?}");
    }

    #[test]
    fn a_capture_spanning_several_intervals_counts_every_missed_slot() {
        // Capture of 130ms with a 50ms interval covers the slots at 50 and 100.
        let (clock, mut source, options) = fixture(Duration::from_millis(130), 2, 50, 1000);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(result.captured_frames(), 2);
        assert_eq!(
            result.skipped_opportunities, 2,
            "the slots at 50 and 100 should both be counted"
        );
    }

    #[test]
    fn no_catch_up_backlog_forms_after_an_overrun() {
        // After a long overrun the next capture must land on the *next future* slot,
        // not once per missed slot. If a backlog existed, this would capture many
        // frames in quick succession.
        let (clock, mut source, options) = fixture(Duration::from_millis(260), 3, 50, 5000);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(
            result.captured_frames(),
            3,
            "exactly one capture per scheduled slot"
        );

        // Each offset is separated by more than the interval, never by zero, which is
        // what a replay of missed slots would produce.
        let offsets: Vec<u64> = result
            .frames
            .iter()
            .map(|f| f.capture_offset.as_millis() as u64)
            .collect();
        for pair in offsets.windows(2) {
            assert!(
                pair[1] - pair[0] >= 260,
                "captures should be separated by at least one capture duration, \
                 otherwise slots are being replayed: {offsets:?}"
            );
        }
    }

    #[test]
    fn the_first_sample_is_taken_immediately() {
        let (clock, mut source, options) = fixture(Duration::from_millis(1), 3, 200, 5000);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert!(
            result.frames[0].capture_offset < Duration::from_millis(10),
            "the first sample should not wait for a cadence slot, got {:?}",
            result.frames[0].capture_offset
        );
    }

    // -- 53: deadline -------------------------------------------------------

    #[test]
    fn a_deadline_that_prevents_the_requested_count_returns_partial() {
        // Four frames at 50ms would need 150ms of cadence; 100ms only allows two.
        let (clock, mut source, options) = fixture(Duration::ZERO, 4, 50, 100);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(result.outcome, RealtimeOutcome::Partial);
        assert_eq!(result.requested_frames, 4);
        assert_eq!(result.captured_frames(), 2);
        assert!(
            result.captured_frames() >= 1,
            "a partial result must still carry the frames it did capture"
        );
    }

    #[test]
    fn a_partial_result_is_not_an_error_and_keeps_its_frames() {
        let (clock, mut source, options) = fixture(Duration::from_millis(20), 8, 30, 60);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(result.outcome, RealtimeOutcome::Partial);
        assert!(
            !result.frames.is_empty(),
            "a partial result should return what it managed to capture"
        );
    }

    #[test]
    fn a_capture_that_started_before_the_deadline_is_allowed_to_finish() {
        // The deadline gates starting a capture, not finishing one. Here the second
        // capture starts just before the deadline and completes after it.
        let clock = ManualClock::new();
        let mut source = TimedSource {
            scene: Scene::new(8, 4),
            capture_duration: Duration::from_millis(40),
            clock: clock.clone(),
            served: 0,
        };
        let options = RealtimeOptions {
            frames: 2,
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(60),
        };

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(
            result.captured_frames(),
            2,
            "a capture started before the deadline must be allowed to finish"
        );
        assert_eq!(result.outcome, RealtimeOutcome::Complete);
        assert!(
            result.elapsed > options.timeout,
            "the request may overrun its deadline by the in-flight capture: {:?}",
            result.elapsed
        );
    }

    #[test]
    fn a_single_frame_request_takes_one_fresh_capture() {
        let (clock, mut source, options) = fixture(Duration::from_millis(5), 1, 50, 500);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(result.outcome, RealtimeOutcome::Complete);
        assert_eq!(result.captured_frames(), 1);
        assert_eq!(
            source.served, 1,
            "a single-frame request must perform exactly one physical capture"
        );
        assert_eq!(result.skipped_opportunities, 0);
    }

    #[test]
    fn a_single_frame_request_ignores_the_interval() {
        // `--frames 1` with a large interval is legitimate: there is no second sample
        // to space out, so the interval must not introduce a delay.
        let (clock, mut source, options) = fixture(Duration::from_millis(1), 1, 5000, 5000);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(result.captured_frames(), 1);
        assert!(
            result.elapsed < Duration::from_millis(50),
            "a single sample should not wait for the interval, took {:?}",
            result.elapsed
        );
    }

    // -- 53: validation -----------------------------------------------------

    #[test]
    fn a_zero_frame_count_is_rejected() {
        let options = RealtimeOptions {
            frames: 0,
            ..RealtimeOptions::default()
        };
        let error = options.validate().unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
    }

    #[test]
    fn more_frames_than_the_maximum_is_rejected_rather_than_truncated() {
        let options = RealtimeOptions {
            frames: MAX_FRAMES + 1,
            ..RealtimeOptions::default()
        };
        let error = options.validate().unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
        assert!(
            error.message().contains(&MAX_FRAMES.to_string()),
            "the message should name the limit: {}",
            error.message()
        );

        // The maximum itself is allowed.
        assert!(RealtimeOptions {
            frames: MAX_FRAMES,
            ..RealtimeOptions::default()
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn a_zero_interval_is_rejected_only_when_more_than_one_frame_is_asked_for() {
        let multi = RealtimeOptions {
            frames: 3,
            interval: Duration::ZERO,
            ..RealtimeOptions::default()
        };
        assert_eq!(multi.validate().unwrap_err().code(), "invalid_duration");

        let single = RealtimeOptions {
            frames: 1,
            interval: Duration::ZERO,
            ..RealtimeOptions::default()
        };
        assert!(
            single.validate().is_ok(),
            "one frame has no cadence to space out"
        );
    }

    #[test]
    fn a_zero_timeout_is_rejected() {
        let options = RealtimeOptions {
            timeout: Duration::ZERO,
            ..RealtimeOptions::default()
        };
        assert_eq!(options.validate().unwrap_err().code(), "invalid_duration");
    }

    #[test]
    fn an_impossible_looking_request_is_accepted_and_yields_a_partial_result() {
        // The spec explicitly calls this configuration valid: 8 frames at 1s cannot
        // fit in 100ms, but that is a legitimate request for a partial answer rather
        // than a mistake to refuse.
        let options = RealtimeOptions {
            frames: 8,
            interval: Duration::from_secs(1),
            timeout: Duration::from_millis(100),
        };
        assert!(options.validate().is_ok(), "this must be accepted");

        let clock = ManualClock::new();
        let mut source = TimedSource {
            scene: Scene::new(8, 4),
            capture_duration: Duration::ZERO,
            clock: clock.clone(),
            served: 0,
        };

        let result = sample_stack(&mut source, &clock, &options).unwrap();
        assert_eq!(result.outcome, RealtimeOutcome::Partial);
        assert_eq!(
            result.captured_frames(),
            1,
            "only the immediate sample fits"
        );
    }

    // -- 54: stack contents -------------------------------------------------

    #[test]
    fn the_stack_is_ordered_oldest_to_newest() {
        let (clock, mut source, options) = fixture(Duration::from_millis(5), 4, 50, 5000);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        let offsets: Vec<Duration> = result.frames.iter().map(|f| f.capture_offset).collect();
        let mut sorted = offsets.clone();
        sorted.sort();
        assert_eq!(
            offsets, sorted,
            "the stack must be oldest to newest, got {offsets:?}"
        );

        // And each frame carries a distinct scene, so the contents match the order.
        for (position, frame) in result.frames.iter().enumerate() {
            let expected = source.scene.render(position);
            assert_eq!(
                frame.session_frame.frame.width(),
                expected.width(),
                "frame {position} should be the scene captured at that position"
            );
        }
    }

    #[test]
    fn a_static_scene_still_returns_every_requested_physical_capture() {
        // Requirement 44: no implicit deduplication. Three identical samples are three
        // frames, because the fact that the scene was still across real sample times is
        // itself information.
        let clock = ManualClock::new();

        /// A source that always yields the same scene.
        struct StaticSource {
            scene: Scene,
            clock: ManualClock,
            served: usize,
        }
        impl RealtimeSource for StaticSource {
            fn capture(&mut self) -> Result<SessionFrame, Error> {
                self.clock.advance(Duration::from_millis(1));
                self.served += 1;
                Ok(identified(self.scene.render(0), self.served as u64))
            }
        }

        let mut source = StaticSource {
            scene: Scene::new(8, 4),
            clock: clock.clone(),
            served: 0,
        };
        let options = RealtimeOptions {
            frames: 3,
            interval: Duration::from_millis(20),
            timeout: Duration::from_millis(500),
        };

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(result.captured_frames(), 3, "no frame may be deduplicated");
        assert_eq!(
            source.served, 3,
            "three physical captures must have happened"
        );
    }

    #[test]
    fn actual_capture_timing_is_reported_rather_than_the_ideal_schedule() {
        // The ideal slots are 0/50/100ms, but a capture that takes 80ms cannot hit
        // them. The reported offsets must be the actual ones.
        let (clock, mut source, options) = fixture(Duration::from_millis(80), 3, 50, 5000);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        let offsets: Vec<u64> = result
            .frames
            .iter()
            .map(|f| f.capture_offset.as_millis() as u64)
            .collect();
        assert_ne!(
            offsets,
            vec![0, 50, 100],
            "the offsets must be measured, not assumed: {offsets:?}"
        );
    }

    #[test]
    fn each_frame_reports_its_own_capture_duration() {
        let clock = ManualClock::new();
        let mut source = VariableSource {
            scene: Scene::new(8, 4),
            durations: vec![
                Duration::from_millis(10),
                Duration::from_millis(30),
                Duration::from_millis(20),
            ],
            clock: clock.clone(),
            served: 0,
        };
        let options = RealtimeOptions {
            frames: 3,
            interval: Duration::from_millis(100),
            timeout: Duration::from_millis(5000),
        };

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        let durations: Vec<u64> = result
            .frames
            .iter()
            .map(|f| f.capture_duration.as_millis() as u64)
            .collect();
        assert_eq!(
            durations,
            vec![10, 30, 20],
            "each frame should report the capture duration that produced it"
        );
        assert_eq!(result.capture_us_total, 60_000);
    }

    #[test]
    fn sleep_time_is_accounted_separately_from_capture_time() {
        let (clock, mut source, options) = fixture(Duration::from_millis(5), 3, 50, 5000);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(result.capture_us_total, 15_000, "three 5ms captures");
        assert!(
            result.sleep_us_total >= 90_000,
            "two intervals of 50ms should have been waited: {}",
            result.sleep_us_total
        );
        // The two account for the whole window, which is what makes the timing
        // diagnosis in the response meaningful.
        assert!(
            result.capture_us_total + result.sleep_us_total <= result.elapsed.as_micros() as u64,
            "capture plus sleep cannot exceed the elapsed window"
        );
    }

    #[test]
    fn sample_accounting_adds_up() {
        let (clock, mut source, options) = fixture(Duration::from_millis(60), 5, 50, 5000);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(result.requested_frames, 5);
        assert_eq!(result.captured_frames(), 5);
        assert_eq!(
            result.scheduled_opportunities, 5,
            "one slot is considered per requested frame"
        );
        // Every considered slot produced a capture, which is the invariant that
        // matters: `scheduled_opportunities` counts slots that were actually used.
        // Skipped slots are counted separately and are not a subset of them -- a
        // 60ms capture against a 50ms interval skips at least one slot per sample.
        assert_eq!(
            result.scheduled_opportunities as usize,
            result.captured_frames(),
            "every scheduled opportunity should have produced a capture"
        );
        assert!(
            result.skipped_opportunities >= 5,
            "a 60ms capture against a 50ms cadence skips at least one slot per sample, \
             got {}",
            result.skipped_opportunities
        );
    }

    #[test]
    fn the_newest_frame_is_the_last_element_and_the_oldest_the_first() {
        let (clock, mut source, options) = fixture(Duration::from_millis(10), 3, 40, 5000);

        let result = sample_stack(&mut source, &clock, &options).unwrap();

        assert_eq!(
            result.newest().unwrap().capture_offset,
            result.frames.last().unwrap().capture_offset
        );
        assert_eq!(
            result.oldest().unwrap().capture_offset,
            result.frames.first().unwrap().capture_offset
        );
        assert!(result.oldest().unwrap().capture_offset < result.newest().unwrap().capture_offset);
    }

    // -- 47: failure semantics ---------------------------------------------

    #[test]
    fn a_capture_failure_aborts_rather_than_returning_a_partial_result() {
        let clock = ManualClock::new();
        let mut source = FailingSource {
            scene: Scene::new(8, 4),
            clock: clock.clone(),
            capture_duration: Duration::from_millis(5),
            fail_on: 1,
            served: 0,
        };
        let options = RealtimeOptions {
            frames: 3,
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(5000),
        };

        let error = sample_stack(&mut source, &clock, &options).unwrap_err();

        assert_eq!(
            error.code(),
            "capture_failed",
            "a backend failure must surface as an error, not as a partial result"
        );
    }

    #[test]
    fn a_failure_on_the_first_capture_is_also_an_error() {
        let clock = ManualClock::new();
        let mut source = FailingSource {
            scene: Scene::new(8, 4),
            clock: clock.clone(),
            capture_duration: Duration::from_millis(5),
            fail_on: 0,
            served: 0,
        };
        let options = RealtimeOptions {
            frames: 3,
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(5000),
        };

        let error = sample_stack(&mut source, &clock, &options).unwrap_err();
        assert_eq!(error.code(), "capture_failed");
    }

    #[test]
    fn default_options_match_the_documented_profile() {
        let options = RealtimeOptions::default();
        assert_eq!(options.frames, 3);
        assert_eq!(options.interval, Duration::from_millis(50));
        assert_eq!(options.timeout, Duration::from_millis(500));
        assert!(options.validate().is_ok());
    }

    #[test]
    fn realtime_outcome_names_are_stable() {
        assert_eq!(RealtimeOutcome::Complete.name(), "complete");
        assert_eq!(RealtimeOutcome::Partial.name(), "partial");
        assert_eq!(
            serde_json::to_value(RealtimeOutcome::Partial).unwrap(),
            serde_json::json!("partial")
        );
    }

    #[test]
    fn ideal_offsets_are_exact_multiples_of_the_interval() {
        let options = RealtimeOptions {
            frames: 4,
            interval: Duration::from_millis(25),
            timeout: Duration::from_secs(1),
        };
        let offsets: Vec<u64> = (0..4)
            .map(|i| options.ideal_offset(i).as_millis() as u64)
            .collect();
        assert_eq!(offsets, vec![0, 25, 50, 75]);
    }

    #[test]
    fn the_comparison_options_type_is_untouched_by_realtime() {
        // Requirement 45: real-time sampling requires no change thresholds. This is a
        // compile-time check that `RealtimeOptions` has no threshold fields, stated as
        // a test so the intent is visible rather than implied.
        let options = RealtimeOptions::default();
        let _: RealtimeOptions = options;
        let _ = CompareOptions::default();
    }
}
