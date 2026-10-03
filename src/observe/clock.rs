//! Time, abstracted so that temporal logic can be tested without waiting.
//!
//! Phase 3 is a state machine over a clock. If that clock were `Instant::now()`
//! and `thread::sleep` directly, every state-machine test would have to spend
//! real wall-clock time, and the suite would be slow and flaky. Instead the
//! temporal layer talks to a [`Clock`], which has two implementations:
//!
//! * [`SystemClock`], backed by `Instant` and `thread::sleep`, used by the CLI;
//! * [`ManualClock`], where time only advances when a test says so.
//!
//! All durations are monotonic. Wall-clock time is never used for deadline
//! arithmetic, because a system clock adjustment must not be able to extend or
//! truncate an observation.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A monotonic time source that can also wait.
pub trait Clock {
    /// The elapsed time since this clock started.
    ///
    /// Only differences between two calls are meaningful; the absolute value is
    /// not a wall-clock timestamp.
    fn now(&self) -> Duration;

    /// Wait until the clock reaches `deadline`.
    ///
    /// Returns immediately when `deadline` has already passed, so a caller that
    /// has fallen behind never blocks.
    fn sleep_until(&self, deadline: Duration);
}

/// A clock backed by [`Instant`] and [`std::thread::sleep`].
#[derive(Debug)]
pub struct SystemClock {
    started_at: Instant,
}

impl SystemClock {
    /// Start a system clock, taking the current instant as its origin.
    pub fn new() -> Self {
        SystemClock {
            started_at: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn sleep_until(&self, deadline: Duration) {
        let now = self.now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }
    }
}

/// A clock whose time only advances when a test asks it to.
///
/// This is what makes the state-machine tests deterministic: a scenario that
/// would take ten seconds of real time (poll twenty times at 500 ms) runs
/// instantly, and the timing assertions are exact rather than approximate.
///
/// Sleeps do not block. They advance the clock to the requested deadline, and
/// record how much "sleep" time was consumed so that a test can verify the
/// scheduler is pacing itself correctly.
#[derive(Debug, Clone, Default)]
pub struct ManualClock {
    inner: Arc<Mutex<ManualClockState>>,
}

#[derive(Debug, Default)]
struct ManualClockState {
    now: Duration,
    /// Total time consumed by `sleep_until` calls.
    slept: Duration,
    /// Deadlines requested by `sleep_until`, oldest first. Tests can assert that
    /// the scheduler asked for the intervals it was supposed to.
    requested_deadlines: VecDeque<Duration>,
}

impl ManualClock {
    /// Create a manual clock positioned at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance the clock by `delta`.
    pub fn advance(&self, delta: Duration) {
        let mut state = self.inner.lock().expect("manual clock lock poisoned");
        state.now += delta;
    }

    /// Total time consumed by sleeps.
    pub fn slept(&self) -> Duration {
        self.inner.lock().expect("manual clock lock poisoned").slept
    }

    /// The deadlines passed to `sleep_until`, oldest first.
    pub fn requested_deadlines(&self) -> Vec<Duration> {
        self.inner
            .lock()
            .expect("manual clock lock poisoned")
            .requested_deadlines
            .iter()
            .copied()
            .collect()
    }

    /// How many times `sleep_until` was called.
    pub fn sleep_count(&self) -> usize {
        self.inner
            .lock()
            .expect("manual clock lock poisoned")
            .requested_deadlines
            .len()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.inner.lock().expect("manual clock lock poisoned").now
    }

    fn sleep_until(&self, deadline: Duration) {
        let mut state = self.inner.lock().expect("manual clock lock poisoned");
        state.requested_deadlines.push_back(deadline);
        let now = state.now;
        if deadline > now {
            state.slept += deadline - now;
            state.now = deadline;
        }
    }
}

/// Parsed cadence and deadline for an observation.
///
/// Kept separate from the public options structs so that a test can exercise
/// scheduling arithmetic without constructing a full configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Schedule {
    pub interval: Duration,
    pub timeout: Duration,
}

impl Schedule {
    /// The deadline a sample scheduled at `index` should aim for.
    ///
    /// Samples are scheduled against a fixed origin rather than against the end
    /// of the previous sample, so that a slow capture does not push every
    /// subsequent sample later and later. This is the "advance to the next
    /// sensible future sample" behaviour: the cadence does not accumulate drift.
    pub fn deadline_for(&self, index: u32) -> Duration {
        self.interval * index
    }

    /// The next sample deadline at or after `now`, given that `index` samples
    /// have already been taken.
    ///
    /// When capture has overrun several intervals, this skips the missed
    /// sample opportunities rather than replaying them, so the observer always
    /// works from fresh frames instead of an artificial backlog.
    pub fn next_deadline(&self, index: u32, now: Duration) -> Duration {
        let mut deadline = self.deadline_for(index);
        if deadline < now {
            let behind = now - deadline;
            let skipped = behind.as_nanos() / self.interval.as_nanos().max(1) + 1;
            deadline += self.interval * skipped as u32;
        }
        deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_clock_starts_at_zero_and_only_moves_when_told() {
        let clock = ManualClock::new();
        assert_eq!(clock.now(), Duration::ZERO);
        clock.advance(Duration::from_millis(5));
        assert_eq!(clock.now(), Duration::from_millis(5));
        // Reading does not advance it.
        assert_eq!(clock.now(), Duration::from_millis(5));
    }

    #[test]
    fn sleeping_advances_a_manual_clock_to_the_deadline() {
        let clock = ManualClock::new();
        clock.sleep_until(Duration::from_millis(100));
        assert_eq!(clock.now(), Duration::from_millis(100));
        assert_eq!(clock.slept(), Duration::from_millis(100));

        // A deadline already in the past does not move the clock backwards.
        clock.sleep_until(Duration::from_millis(40));
        assert_eq!(clock.now(), Duration::from_millis(100));
        assert_eq!(clock.slept(), Duration::from_millis(100));
        assert_eq!(clock.sleep_count(), 2);
    }

    #[test]
    fn a_manual_clock_records_the_deadlines_it_was_asked_for() {
        let clock = ManualClock::new();
        clock.sleep_until(Duration::from_millis(100));
        clock.sleep_until(Duration::from_millis(200));
        assert_eq!(
            clock.requested_deadlines(),
            vec![Duration::from_millis(100), Duration::from_millis(200)]
        );
    }

    #[test]
    fn manual_clocks_clone_into_a_shared_view() {
        let clock = ManualClock::new();
        let observer = clock.clone();
        clock.advance(Duration::from_millis(7));
        assert_eq!(observer.now(), Duration::from_millis(7));
    }

    #[test]
    fn a_system_clock_moves_forward() {
        let clock = SystemClock::new();
        let first = clock.now();
        std::thread::sleep(Duration::from_millis(2));
        assert!(clock.now() >= first);
    }

    #[test]
    fn a_system_clock_does_not_block_for_a_past_deadline() {
        let clock = SystemClock::new();
        clock.sleep_until(Duration::ZERO);
        assert!(clock.now() < Duration::from_millis(50));
    }

    #[test]
    fn sample_deadlines_do_not_accumulate_drift() {
        let schedule = Schedule {
            interval: Duration::from_millis(100),
            timeout: Duration::from_secs(10),
        };

        // With no overrun, deadlines are exact multiples of the interval.
        for index in 0..5 {
            assert_eq!(
                schedule.next_deadline(index, schedule.deadline_for(index)),
                Duration::from_millis(100 * index as u64)
            );
        }
    }

    #[test]
    fn a_slow_capture_skips_missed_sample_opportunities() {
        let schedule = Schedule {
            interval: Duration::from_millis(100),
            timeout: Duration::from_secs(10),
        };

        // Sample 3 was due at 300ms but we are only finishing at 355ms.
        let deadline = schedule.next_deadline(3, Duration::from_millis(355));
        assert_eq!(
            deadline,
            Duration::from_millis(400),
            "should advance to the next future sample, not replay 300ms"
        );

        // Badly behind: due at 100ms, now at 1000ms.
        let deadline = schedule.next_deadline(1, Duration::from_millis(1000));
        assert_eq!(deadline, Duration::from_millis(1100));
    }
}
