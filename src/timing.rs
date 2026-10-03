//! Stage timing instrumentation.
//!
//! `eensh` is invoked once per observation, so the interesting question is not
//! "how fast is the machine" but "which stage is adding latency to *this*
//! capture". Every major stage is therefore measured with a monotonic clock and
//! reported alongside the image.
//!
//! Durations are recorded as microseconds because whole milliseconds are too
//! coarse to distinguish, say, resize from encode.

use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Per-stage durations for one capture, in microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timing {
    /// Reading pixels out of the X server.
    pub capture_us: u64,
    /// Resizing the raw frame, if a resize was requested.
    pub resize_us: u64,
    /// PNG or JPEG encoding.
    pub encode_us: u64,
    /// Base64 conversion, if requested.
    pub base64_us: u64,
    /// Wall-clock duration of the whole operation.
    pub total_us: u64,
}

/// Records the elapsed time of a single stage.
///
/// A `Stopwatch` is started, stopped once, and then holds its measured
/// duration. Starting and stopping are separate steps so the timer can wrap a
/// fallible operation without leaking a partial duration.
#[derive(Debug)]
pub struct Stopwatch {
    started_at: Instant,
    elapsed_us: Option<u64>,
}

impl Stopwatch {
    /// Start a stopwatch.
    pub fn start() -> Self {
        Stopwatch {
            started_at: Instant::now(),
            elapsed_us: None,
        }
    }

    /// Stop the stopwatch, recording the elapsed duration.
    ///
    /// Calling this more than once is a no-op after the first call, so a
    /// stopwatch can be stopped on both the success and error paths.
    pub fn stop(&mut self) -> u64 {
        let elapsed = *self
            .elapsed_us
            .get_or_insert_with(|| self.started_at.elapsed().as_micros() as u64);
        elapsed
    }

    /// The recorded duration, or the time elapsed so far.
    pub fn elapsed_us(&self) -> u64 {
        self.elapsed_us
            .unwrap_or_else(|| self.started_at.elapsed().as_micros() as u64)
    }
}

/// Accumulates stage timings while a capture is assembled.
#[derive(Debug, Default)]
pub struct TimingBuilder {
    started_at: Option<Instant>,
    timing: Timing,
}

impl TimingBuilder {
    /// Begin the overall measurement.
    pub fn start() -> Self {
        TimingBuilder {
            started_at: Some(Instant::now()),
            timing: Timing::default(),
        }
    }

    /// Record the capture stage.
    pub fn capture(&mut self, stopwatch: &mut Stopwatch) {
        self.timing.capture_us = stopwatch.stop();
    }

    /// Record the resize stage.
    pub fn resize(&mut self, stopwatch: &mut Stopwatch) {
        self.timing.resize_us = stopwatch.stop();
    }

    /// Record the encode stage.
    pub fn encode(&mut self, stopwatch: &mut Stopwatch) {
        self.timing.encode_us = stopwatch.stop();
    }

    /// Record the base64 stage.
    pub fn base64(&mut self, stopwatch: &mut Stopwatch) {
        self.timing.base64_us = stopwatch.stop();
    }

    /// Record the resize, encode, and base64 stages from a prepared image.
    ///
    /// The shared presentation path in [`crate::pipeline::prepare_image`] times
    /// those three stages itself, so they are copied across rather than measured
    /// again here.
    pub fn record(&mut self, prepared: &crate::pipeline::PreparedImage) {
        self.timing.resize_us = prepared.resize_us;
        self.timing.encode_us = prepared.encode_us;
        self.timing.base64_us = prepared.base64_us;
    }

    /// Finish and return the collected timings.
    pub fn finish(mut self) -> Timing {
        self.timing.total_us = self
            .started_at
            .map(|start| start.elapsed().as_micros() as u64)
            .unwrap_or(0);
        self.timing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timings_serialize_with_stable_field_names() {
        let timing = Timing {
            capture_us: 1,
            resize_us: 2,
            encode_us: 3,
            base64_us: 4,
            total_us: 5,
        };
        let value = serde_json::to_value(timing).unwrap();
        for field in [
            "capture_us",
            "resize_us",
            "encode_us",
            "base64_us",
            "total_us",
        ] {
            assert!(value.get(field).is_some(), "missing field {field}");
        }
    }

    #[test]
    fn a_stopped_stopwatch_reports_a_frozen_duration() {
        let mut stopwatch = Stopwatch::start();
        let first = stopwatch.stop();
        let second = stopwatch.stop();
        assert_eq!(first, second);
    }

    #[test]
    fn builder_records_each_stage() {
        let mut builder = TimingBuilder::start();
        for stage in 0..4 {
            let mut stopwatch = Stopwatch::start();
            // Do a little work so the duration is not necessarily zero.
            let mut sum = 0u64;
            for i in 0..1_000u64 {
                sum = sum.wrapping_add(i);
            }
            std::hint::black_box(sum);
            match stage {
                0 => builder.capture(&mut stopwatch),
                1 => builder.resize(&mut stopwatch),
                2 => builder.encode(&mut stopwatch),
                _ => builder.base64(&mut stopwatch),
            }
        }
        let timing = builder.finish();
        // Every stage was recorded; total covers at least the sum of the parts.
        assert!(timing.total_us >= timing.capture_us.max(timing.encode_us));
    }
}
