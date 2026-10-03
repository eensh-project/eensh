//! Temporal observation over raw frames.
//!
//! This module is a state machine over repeated captures and Phase 2
//! comparisons. It answers three questions:
//!
//! * *Has the visible state changed?* — [`wait_for_change`]
//! * *Has the visible state stopped changing?* — [`wait_for_stable`]
//! * *What is the resulting settled state after a transition?* — [`observe`]
//!
//! # The one distinction that matters
//!
//! The three operations differ in exactly one way, and everything else is
//! shared:
//!
//! ```text
//! wait-change : compare every frame against a FIXED BASELINE
//! wait-stable : compare CONSECUTIVE frames
//! observe     : fixed baseline until a change, then consecutive frames
//! ```
//!
//! A fixed baseline is what makes `wait-change` able to see a gradual transition
//! whose every individual step is below the area threshold. Consecutive
//! comparison is what makes `wait-stable` able to tell whether the scene is
//! *currently still moving*. Using the wrong one for either job gives a plausible
//! but incorrect answer, so the choice is explicit in the code rather than
//! incidental.
//!
//! # Layers
//!
//! ```text
//! frame acquisition   FrameSource (capture/ or a test fake)
//!     !=
//! frame comparison    compare.rs
//!     !=
//! temporal policy     this module
//!     !=
//! output              output/, applied only to the final frame
//! ```
//!
//! Sampled frames are never encoded. Only the frame the operation ends on is
//! ever handed to an encoder.

pub mod clock;
pub mod pipeline;

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::compare::{compare_frames, CompareMode, CompareOptions, Comparison};
use crate::error::Error;
use crate::frame::Frame;
use crate::geometry::SourceGeometry;

pub use clock::{Clock, ManualClock, SystemClock};

/// Something that can produce a frame on demand.
///
/// This is the seam that keeps temporal policy independent of X11. The CLI
/// supplies a capture-backed implementation; the tests supply a scripted one.
/// It is deliberately one method: it exists to separate *where frames come from*
/// from *what is done with them*, not to model capture in general.
pub trait FrameSource {
    /// Produce the next frame.
    fn capture(&mut self) -> Result<Frame, Error>;
}

/// Comparison settings shared by every temporal operation.
///
/// The defaults are *not* the same as `eensh diff`, and that is deliberate:
/// `diff` asks "did anything differ?" while temporal observation asks "did
/// anything meaningfully change?". Observation is tolerant of rendering noise by
/// default; a diff is not.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TemporalCompareOptions {
    /// Phase 2 comparison settings.
    pub compare: CompareOptions,
    /// Target cadence between samples.
    pub interval: Duration,
    /// Total deadline for the whole operation.
    pub timeout: Duration,
}

/// Default `pixel_threshold` for temporal observation.
pub const DEFAULT_PIXEL_THRESHOLD: u8 = 12;
/// Default `area_threshold` for temporal observation: 0.5% of pixels.
pub const DEFAULT_AREA_THRESHOLD: f64 = 0.005;
/// Default polling interval.
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);
/// Default total timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
/// Default required stability duration.
pub const DEFAULT_STABLE_FOR: Duration = Duration::from_millis(300);

impl Default for TemporalCompareOptions {
    fn default() -> Self {
        TemporalCompareOptions {
            compare: CompareOptions {
                mode: CompareMode::RgbThreshold,
                pixel_threshold: DEFAULT_PIXEL_THRESHOLD,
                area_threshold: DEFAULT_AREA_THRESHOLD,
            },
            interval: DEFAULT_INTERVAL,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl TemporalCompareOptions {
    /// Validate the interval, timeout, and comparison thresholds together.
    pub fn validate(&self) -> Result<(), Error> {
        if self.interval.is_zero() {
            return Err(Error::invalid_duration(
                "the polling interval must be greater than zero",
            ));
        }
        if self.timeout.is_zero() {
            return Err(Error::invalid_duration(
                "the timeout must be greater than zero",
            ));
        }
        self.compare.validate()
    }
}

/// Options for [`wait_for_change`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WaitChangeOptions {
    /// Comparison settings, cadence, and deadline.
    pub temporal: TemporalCompareOptions,
}

/// Options for [`wait_for_stable`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaitStableOptions {
    /// Comparison settings, cadence, and deadline.
    pub temporal: TemporalCompareOptions,
    /// How long the scene must remain unchanged to count as stable.
    pub stable_for: Duration,
}

impl Default for WaitStableOptions {
    fn default() -> Self {
        WaitStableOptions {
            temporal: TemporalCompareOptions::default(),
            stable_for: DEFAULT_STABLE_FOR,
        }
    }
}

impl WaitStableOptions {
    /// Validate every setting.
    pub fn validate(&self) -> Result<(), Error> {
        self.temporal.validate()?;
        if self.stable_for.is_zero() {
            return Err(Error::invalid_duration(
                "the stable duration must be greater than zero; zero would mean \
                 'instantly stable', which is almost never what was intended",
            ));
        }
        Ok(())
    }
}

/// Options for [`observe`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ObserveOptions {
    /// Comparison settings, cadence, and deadline.
    pub temporal: TemporalCompareOptions,
    /// How long the scene must remain unchanged after the transition.
    pub stable_for: Duration,
}

impl ObserveOptions {
    /// Build options with the documented defaults.
    pub fn with_defaults() -> Self {
        ObserveOptions {
            temporal: TemporalCompareOptions::default(),
            stable_for: DEFAULT_STABLE_FOR,
        }
    }

    /// Validate every setting.
    pub fn validate(&self) -> Result<(), Error> {
        self.temporal.validate()?;
        if self.stable_for.is_zero() {
            return Err(Error::invalid_duration(
                "the stable duration must be greater than zero; zero would mean \
                 'instantly stable', which is almost never what you want here",
            ));
        }
        Ok(())
    }
}

/// How an observation ended.
///
/// A timeout is an observation *outcome*, not an error: the operation worked,
/// the visual condition simply did not occur in time. Keeping it in this enum
/// rather than in [`Error`] is what lets an agent tell "nothing happened" apart
/// from "capture broke".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The target changed meaningfully from the baseline.
    Changed,
    /// The target held still for the required duration.
    Stable,
    /// A change was observed and the resulting state settled.
    Observed,
    /// The deadline passed before the requested condition occurred.
    Timeout,
}

impl Outcome {
    /// Canonical name used in JSON.
    pub fn name(self) -> &'static str {
        match self {
            Outcome::Changed => "changed",
            Outcome::Stable => "stable",
            Outcome::Observed => "observed",
            Outcome::Timeout => "timeout",
        }
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Where latency went during an observation.
///
/// The point is to answer "are we slow because capture is slow, because
/// comparison is slow, or because we are mostly sleeping between polls?"
/// without a profiler.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationTiming {
    /// Total wall time of the operation, in microseconds.
    pub elapsed_us: u64,
    /// Frames captured.
    pub captures: u64,
    /// Frame pairs compared. Normally `captures - 1`.
    pub comparisons: u64,
    /// Time spent inside `FrameSource::capture`, in microseconds.
    pub capture_us_total: u64,
    /// Time spent inside `compare_frames`, in microseconds.
    pub compare_us_total: u64,
    /// Time spent deliberately waiting between samples, in microseconds.
    pub sleep_us_total: u64,
}

/// The result of [`wait_for_change`].
#[derive(Debug)]
pub struct WaitChangeResult {
    /// How the operation ended: `Changed` or `Timeout`.
    pub outcome: Outcome,
    /// The frame the operation ended on. On timeout this is the latest sample.
    pub frame: Frame,
    /// Comparison of the baseline against [`WaitChangeResult::frame`].
    pub comparison: Comparison,
    /// Frames captured.
    pub captures: u64,
    /// Frame pairs compared.
    pub comparisons: u64,
    /// Total elapsed time.
    pub elapsed: Duration,
    /// Where the time went.
    pub timing: ObservationTiming,
}

/// The result of [`wait_for_stable`].
#[derive(Debug)]
pub struct WaitStableResult {
    /// How the operation ended: `Stable` or `Timeout`.
    pub outcome: Outcome,
    /// The final frame. On timeout this is the latest sample.
    pub frame: Frame,
    /// The most recent consecutive comparison.
    pub last_comparison: Comparison,
    /// Frames captured.
    pub captures: u64,
    /// Frame pairs compared.
    pub comparisons: u64,
    /// Total elapsed time.
    pub elapsed: Duration,
    /// How long the scene had been continuously unchanged when it completed.
    pub stable_duration: Duration,
    /// Where the time went.
    pub timing: ObservationTiming,
}

/// The result of [`observe`].
#[derive(Debug)]
pub struct ObserveResult {
    /// How the operation ended: `Observed` or `Timeout`.
    pub outcome: Outcome,
    /// The final settled frame. On timeout this is the latest sample.
    pub frame: Frame,
    /// The comparison that first detected the transition, if one was detected.
    pub first_change: Option<Comparison>,
    /// The most recent comparison.
    pub last_comparison: Comparison,
    /// When the first change was detected, measured from the start.
    pub change_detected_at: Option<Duration>,
    /// Frames captured.
    pub captures: u64,
    /// Frame pairs compared.
    pub comparisons: u64,
    /// Total elapsed time.
    pub elapsed: Duration,
    /// Where the time went.
    pub timing: ObservationTiming,
}

/// Accumulates timing across a run without clutter at every call site.
struct Ledger {
    captures: u64,
    comparisons: u64,
    capture_us_total: u64,
    compare_us_total: u64,
    slept: Duration,
    started_at: Duration,
}

impl Ledger {
    fn new(started_at: Duration) -> Self {
        Ledger {
            captures: 0,
            comparisons: 0,
            capture_us_total: 0,
            compare_us_total: 0,
            slept: Duration::ZERO,
            started_at,
        }
    }

    fn finish(&self, clock: &dyn Clock) -> ObservationTiming {
        ObservationTiming {
            elapsed_us: (clock.now() - self.started_at).as_micros() as u64,
            captures: self.captures,
            comparisons: self.comparisons,
            capture_us_total: self.capture_us_total,
            compare_us_total: self.compare_us_total,
            sleep_us_total: self.slept.as_micros() as u64,
        }
    }
}

/// Verify that a frame is compatible with the observation baseline.
///
/// Phase 2 only requires matching pixel grids. A temporal observer is stricter:
/// it is watching *the same target over time*, so the effective source geometry
/// must not move or resize underneath it. A window that is resized, a game that
/// switches fullscreen mode, or a monitor-layout change would otherwise silently
/// compare pixel grids that no longer represent the same visual coordinates.
fn check_consistency(baseline: &SourceGeometry, current: &Frame) -> Result<(), Error> {
    if baseline.width != current.width() || baseline.height != current.height() {
        return Err(Error::geometry_changed(format!(
            "observed target changed from {}x{} at ({},{}) to {}x{} at ({},{})",
            baseline.width,
            baseline.height,
            baseline.x,
            baseline.y,
            current.width(),
            current.height(),
            current.source_geometry.x,
            current.source_geometry.y,
        )));
    }

    if baseline.x != current.source_geometry.x || baseline.y != current.source_geometry.y {
        return Err(Error::geometry_changed(format!(
            "observed target moved from ({},{}) to ({},{}); its size stayed {}x{}",
            baseline.x,
            baseline.y,
            current.source_geometry.x,
            current.source_geometry.y,
            baseline.width,
            baseline.height,
        )));
    }

    Ok(())
}

/// Capture one frame, recording the time spent in the source.
///
/// A capture failure aborts the whole observation with the underlying structured
/// error. Phase 3 does not retry: a transient-failure policy belongs with the
/// persistent session in Phase 4, and guessing at one now would make failures
/// harder to diagnose.
fn capture_sample<S: FrameSource>(
    source: &mut S,
    clock: &dyn Clock,
    ledger: &mut Ledger,
) -> Result<Frame, Error> {
    let started = clock.now();
    let frame = source.capture()?;
    ledger.capture_us_total += (clock.now() - started).as_micros() as u64;
    ledger.captures += 1;
    Ok(frame)
}

/// Compare two frames, recording the time spent comparing.
fn compare_sample(
    before: &Frame,
    after: &Frame,
    options: &CompareOptions,
    clock: &dyn Clock,
    ledger: &mut Ledger,
) -> Result<Comparison, Error> {
    let started = clock.now();
    let comparison = compare_frames(before, after, options)?;
    ledger.compare_us_total += (clock.now() - started).as_micros() as u64;
    ledger.comparisons += 1;
    Ok(comparison)
}

/// A well-formed zero comparison of a frame against itself.
///
/// Every operation needs a `Comparison` to return even when no two distinct
/// samples were ever captured, so the baseline is compared with itself to give a
/// truthful "nothing changed, nothing is known to have changed" value.
///
/// This is **not** counted in the ledger. It compares no two distinct samples, so
/// counting it would make `comparisons` drift to `captures` instead of the
/// documented `captures - 1`.
fn initial_comparison(frame: &Frame, options: &CompareOptions) -> Result<Comparison, Error> {
    compare_frames(frame, frame, options)
}

/// Wait until the target differs meaningfully from the baseline.
///
/// Every sample is compared against the **fixed** baseline captured first, not
/// against the previous sample. That is what allows a gradual transition to be
/// detected: if each individual step is smaller than the area threshold, a
/// consecutive comparison would never fire, while the accumulated difference from
/// the original scene is large.
///
/// Returns [`Outcome::Changed`] when a meaningful difference is found, and
/// [`Outcome::Timeout`] when the deadline passes first. On timeout the latest
/// frame and its comparison against the baseline are still returned, because an
/// agent usually wants to inspect the current state after a transition that did
/// not happen.
pub fn wait_for_change<S: FrameSource>(
    source: &mut S,
    clock: &dyn Clock,
    options: &WaitChangeOptions,
) -> Result<WaitChangeResult, Error> {
    options.temporal.validate()?;

    let started_at = clock.now();
    let mut ledger = Ledger::new(started_at);
    let schedule = clock::Schedule {
        interval: options.temporal.interval,
        timeout: options.temporal.timeout,
    };

    let baseline = capture_sample(source, clock, &mut ledger)?;
    let baseline_geometry = baseline.source_geometry.clone();

    // A well-formed zero comparison, so the return value is always meaningful
    // even if the deadline expires before a second sample is taken.
    let mut last_comparison = initial_comparison(&baseline, &options.temporal.compare)?;
    let mut latest = baseline.clone();

    let mut index: u32 = 1;
    loop {
        let deadline = schedule.next_deadline(index, clock.now());
        if schedule.timeout <= clock.now() {
            break;
        }

        let sleep_started = clock.now();
        clock.sleep_until(deadline.min(schedule.timeout));
        ledger.slept += clock.now() - sleep_started;

        if clock.now() >= schedule.timeout {
            break;
        }

        let current = capture_sample(source, clock, &mut ledger)?;
        check_consistency(&baseline_geometry, &current)?;

        last_comparison = compare_sample(
            &baseline,
            &current,
            &options.temporal.compare,
            clock,
            &mut ledger,
        )?;
        latest = current;

        if last_comparison.changed {
            let elapsed = clock.now() - started_at;
            return Ok(WaitChangeResult {
                outcome: Outcome::Changed,
                frame: latest,
                comparison: last_comparison,
                captures: ledger.captures,
                comparisons: ledger.comparisons,
                elapsed,
                timing: ledger.finish(clock),
            });
        }

        index += 1;
    }

    let elapsed = clock.now() - started_at;
    Ok(WaitChangeResult {
        outcome: Outcome::Timeout,
        frame: latest,
        comparison: last_comparison,
        captures: ledger.captures,
        comparisons: ledger.comparisons,
        elapsed,
        timing: ledger.finish(clock),
    })
}

/// Wait until the target stops changing for a required duration.
///
/// Unlike [`wait_for_change`], this compares **consecutive** samples, because it
/// asks whether the scene is still moving right now.
///
/// The stability interval starts when the first sample is taken. A scene that is
/// already still therefore satisfies the operation after `stable_for`, which is
/// the behaviour callers want: "wait until it has settled" should return
/// immediately if it has already settled.
pub fn wait_for_stable<S: FrameSource>(
    source: &mut S,
    clock: &dyn Clock,
    options: &WaitStableOptions,
) -> Result<WaitStableResult, Error> {
    options.validate()?;

    let started_at = clock.now();
    let mut ledger = Ledger::new(started_at);
    let schedule = clock::Schedule {
        interval: options.temporal.interval,
        timeout: options.temporal.timeout,
    };

    let mut previous = capture_sample(source, clock, &mut ledger)?;
    let geometry = previous.source_geometry.clone();
    let mut last_comparison = initial_comparison(&previous, &options.temporal.compare)?;
    let mut last_change_at = started_at;

    let mut index: u32 = 1;
    loop {
        let deadline = schedule.next_deadline(index, clock.now());
        if schedule.timeout <= clock.now() {
            break;
        }

        let sleep_started = clock.now();
        clock.sleep_until(deadline.min(schedule.timeout));
        ledger.slept += clock.now() - sleep_started;

        if clock.now() >= schedule.timeout {
            break;
        }

        let current = capture_sample(source, clock, &mut ledger)?;
        check_consistency(&geometry, &current)?;

        last_comparison = compare_sample(
            &previous,
            &current,
            &options.temporal.compare,
            clock,
            &mut ledger,
        )?;

        if last_comparison.changed {
            // Any meaningful movement restarts the clock, even if the changed
            // state survives for only a single sample.
            last_change_at = clock.now();
        } else if clock.now() - last_change_at >= options.stable_for {
            let elapsed = clock.now() - started_at;
            return Ok(WaitStableResult {
                outcome: Outcome::Stable,
                frame: current,
                last_comparison,
                captures: ledger.captures,
                comparisons: ledger.comparisons,
                elapsed,
                stable_duration: clock.now() - last_change_at,
                timing: ledger.finish(clock),
            });
        }

        previous = current;
        index += 1;
    }

    let elapsed = clock.now() - started_at;
    Ok(WaitStableResult {
        outcome: Outcome::Timeout,
        frame: previous,
        last_comparison,
        captures: ledger.captures,
        comparisons: ledger.comparisons,
        elapsed,
        stable_duration: clock.now().saturating_sub(last_change_at),
        timing: ledger.finish(clock),
    })
}

/// Wait for a visual transition, then for the resulting state to settle.
///
/// This is the operation most agent workflows want:
///
/// ```text
/// capture baseline
///     -> compare each sample against the baseline until it changes
///     -> then compare consecutive samples until they stop differing
///     -> return the settled frame
/// ```
///
/// It deliberately does not return on the first changed frame. That frame is
/// typically a half-drawn menu, an animation frame, or an incomplete layout. The
/// point of `observe` is the state the transition *settles into*.
///
/// Two behaviours are easy to get wrong and are handled explicitly:
///
/// * **Further changes while settling do not restart the search.** Once the
///   target has departed from the baseline, the operation is watching that
///   transition; it never returns to waiting for a change. Each further change
///   just resets the stability timer.
/// * **The scene returning to the baseline still completes.** A popup that opens
///   and closes, or a button flash, is a real transition that settled. The final
///   frame is not required to differ from the baseline.
pub fn observe<S: FrameSource>(
    source: &mut S,
    clock: &dyn Clock,
    options: &ObserveOptions,
) -> Result<ObserveResult, Error> {
    options.validate()?;

    let started_at = clock.now();
    let mut ledger = Ledger::new(started_at);
    let schedule = clock::Schedule {
        interval: options.temporal.interval,
        timeout: options.temporal.timeout,
    };

    let baseline = capture_sample(source, clock, &mut ledger)?;
    let baseline_geometry = baseline.source_geometry.clone();
    let mut last_comparison = initial_comparison(&baseline, &options.temporal.compare)?;

    let mut latest = baseline.clone();
    let mut first_change: Option<Comparison> = None;
    let mut change_detected_at: Option<Duration> = None;
    let mut last_change_at = started_at;

    let mut index: u32 = 1;
    loop {
        let deadline = schedule.next_deadline(index, clock.now());
        if schedule.timeout <= clock.now() {
            break;
        }

        let sleep_started = clock.now();
        clock.sleep_until(deadline.min(schedule.timeout));
        ledger.slept += clock.now() - sleep_started;

        if clock.now() >= schedule.timeout {
            break;
        }

        let current = capture_sample(source, clock, &mut ledger)?;
        check_consistency(&baseline_geometry, &current)?;

        match first_change {
            // Still waiting for the transition: compare against the baseline.
            None => {
                let comparison = compare_sample(
                    &baseline,
                    &current,
                    &options.temporal.compare,
                    clock,
                    &mut ledger,
                )?;
                last_comparison = comparison;
                if comparison.changed {
                    first_change = Some(comparison);
                    change_detected_at = Some(clock.now() - started_at);
                    last_change_at = clock.now();
                }
            }
            // Settling: compare consecutive frames.
            Some(_) => {
                let comparison = compare_sample(
                    &latest,
                    &current,
                    &options.temporal.compare,
                    clock,
                    &mut ledger,
                )?;
                last_comparison = comparison;
                if comparison.changed {
                    last_change_at = clock.now();
                } else if clock.now() - last_change_at >= options.stable_for {
                    let elapsed = clock.now() - started_at;
                    return Ok(ObserveResult {
                        outcome: Outcome::Observed,
                        frame: current,
                        first_change,
                        last_comparison,
                        change_detected_at,
                        captures: ledger.captures,
                        comparisons: ledger.comparisons,
                        elapsed,
                        timing: ledger.finish(clock),
                    });
                }
            }
        }

        latest = current;
        index += 1;
    }

    let elapsed = clock.now() - started_at;
    Ok(ObserveResult {
        outcome: Outcome::Timeout,
        frame: latest,
        first_change,
        last_comparison,
        change_detected_at,
        captures: ledger.captures,
        comparisons: ledger.comparisons,
        elapsed,
        timing: ledger.finish(clock),
    })
}
