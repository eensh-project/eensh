//! Synthetic state-machine tests for temporal observation.
//!
//! Every test here drives a scripted [`FrameSource`] and a [`ManualClock`], so
//! the scenarios run instantly and the timing assertions are exact rather than
//! approximate. Nothing in this file touches X11, and nothing sleeps.
//!
//! The frame script is the whole point: a scene is expressed as a sequence of
//! distinct frames (`A A B B C`), and the operation is asserted to end on the
//! frame the semantics require. That makes the three policies — fixed baseline,
//! consecutive frames, and the switch between them — directly observable instead
//! of merely asserted in prose.

use std::sync::Arc;
use std::time::Duration;

use eensh::compare::{CompareMode, CompareOptions};
use eensh::error::Error;
use eensh::frame::Frame;
use eensh::geometry::Rect;
use eensh::observe::clock::{ManualClock, SystemClock};
use eensh::observe::{
    observe, wait_for_change, wait_for_stable, FrameSource, ObserveOptions, Outcome,
    TemporalCompareOptions, WaitChangeOptions, WaitStableOptions,
};

// --- the scripted source -----------------------------------------------------

/// A scene expressed as a grid of coloured cells.
///
/// Cells are painted by index so that a scenario can say "change the middle
/// cell" and get a precisely-sized changed region, which lets the tests use the
/// same area-threshold arithmetic the real captures would.
///
/// With a 10x10 grid of 100 cells, one cell is exactly 1% of the scene, which
/// makes the area-threshold arithmetic easy to reason about: a scenario that
/// changes 6 cells has changed 6% of the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Scene {
    width: u32,
    height: u32,
    columns: u32,
    rows: u32,
    cell_width: u32,
    cell_height: u32,
}

impl Scene {
    /// A scene divided into `columns * rows` cells of `cell_size` pixels.
    fn new(columns: u32, rows: u32, cell_size: u32) -> Self {
        Scene {
            width: columns * cell_size,
            height: rows * cell_size,
            columns,
            rows,
            cell_width: cell_size,
            cell_height: cell_size,
        }
    }

    /// Render a scene in which the listed cell indices are "changed" cells.
    ///
    /// Changed cells are white and unchanged cells are black, so a scenario's
    /// changed-pixel count is exactly `indices.len()` cells' worth.
    fn render(&self, changed: &[usize]) -> Frame {
        let mut data = Vec::with_capacity((self.width * self.height * 3) as usize);

        for y in 0..self.height {
            for x in 0..self.width {
                let column = x / self.cell_width;
                let row = y / self.cell_height;
                let index = (row * self.columns + column) as usize;
                let value = if changed.contains(&index) { 255 } else { 0 };
                data.extend_from_slice(&[value, value, value]);
            }
        }

        eensh::input::frame_from_rgb8(self.width, self.height, data).unwrap()
    }
}

/// A `FrameSource` that plays a fixed script, then repeats its last frame.
struct ScriptedSource {
    /// One entry per capture: the indices of the changed cells for that frame.
    script: Vec<Vec<usize>>,
    scene: Scene,
    position: usize,
    /// Total captures served, including repeats of the final frame.
    served: Arc<std::sync::atomic::AtomicUsize>,
}

impl ScriptedSource {
    fn new(scene: Scene, script: Vec<Vec<usize>>) -> Self {
        ScriptedSource {
            script,
            scene,
            position: 0,
            served: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl FrameSource for ScriptedSource {
    fn capture(&mut self) -> Result<Frame, Error> {
        self.served
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let index = self.position.min(self.script.len().saturating_sub(1));
        let frame = self.scene.render(&self.script[index]);
        // Advance, then hold on the final entry forever. Holding matters: a
        // scenario that ends in a stable state must stay stable, and repeating
        // the last frame is exactly what a settled screen does.
        if self.position + 1 < self.script.len() {
            self.position += 1;
        }
        Ok(frame)
    }
}

/// A source that fails after `successes` captures.
struct FailingSource {
    script: ScriptedSource,
    successes: usize,
}

impl FrameSource for FailingSource {
    fn capture(&mut self) -> Result<Frame, Error> {
        if self.successes == 0 {
            return Err(Error::capture_failed("the display went away"));
        }
        self.successes -= 1;
        self.script.capture()
    }
}

/// A scene that changes size partway through, to exercise drift detection.
struct ResizingSource {
    served: usize,
    small: Scene,
    large: Scene,
    switch_after: usize,
}

impl FrameSource for ResizingSource {
    fn capture(&mut self) -> Result<Frame, Error> {
        self.served += 1;
        if self.served > self.switch_after {
            Ok(self.large.render(&[]))
        } else {
            Ok(self.small.render(&[]))
        }
    }
}

/// A scene that changes its reported origin partway through, to exercise
/// target-drift detection for a moved window.
struct MovingSource {
    served: usize,
    move_after: usize,
    frame_data: Vec<u8>,
}

impl FrameSource for MovingSource {
    fn capture(&mut self) -> Result<Frame, Error> {
        self.served += 1;
        let mut frame = eensh::input::frame_from_rgb8(20, 20, self.frame_data.clone()).unwrap();
        if self.served > self.move_after {
            frame.source_geometry.x += 5;
        }
        Ok(frame)
    }
}

// --- shared setup ------------------------------------------------------------

/// A 10x10 scene of 100 cells, one cell per 10x10 pixel block.
///
/// The arithmetic is convenient: one cell is exactly 1% of the scene.
fn scene() -> Scene {
    Scene::new(10, 10, 10)
}

/// Options tuned so that "meaningfully changed" means "at least 5% of the scene
/// changed", with a pixel threshold that ignores small shading differences.
fn temporal_options() -> TemporalCompareOptions {
    TemporalCompareOptions {
        compare: CompareOptions {
            mode: CompareMode::RgbThreshold,
            pixel_threshold: 12,
            // 5 cells out of 100.
            area_threshold: 0.05,
        },
        interval: Duration::from_millis(100),
        timeout: Duration::from_secs(10),
    }
}

/// Options whose area threshold effectively disables suppression, for scenarios
/// that need single-cell changes to count.
fn sensitive_options() -> TemporalCompareOptions {
    TemporalCompareOptions {
        compare: CompareOptions {
            mode: CompareMode::Exact,
            pixel_threshold: 0,
            area_threshold: 0.0,
        },
        ..temporal_options()
    }
}

// ============================================================================
// 31. wait-change
// ============================================================================

#[test]
fn wait_change_returns_the_changed_frame() {
    // A A A B
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(
        scene(),
        vec![vec![], vec![], vec![], vec![0, 1, 2, 3, 4, 5]],
    );

    let result = wait_for_change(
        &mut source,
        &clock,
        &WaitChangeOptions {
            temporal: temporal_options(),
        },
    )
    .unwrap();

    assert_eq!(result.outcome, Outcome::Changed);
    assert!(result.comparison.changed);
    // 6 cells of 100 changed.
    assert_eq!(result.comparison.total_pixels, 100 * 100);
    assert_eq!(result.comparison.changed_pixels, 600);
    assert!(result.comparison.changed_fraction >= 0.05);
    assert_eq!(
        result.comparison.bounding_box,
        Some(Rect::new(0, 0, 60, 10).unwrap()),
        "the first six cells form a 60x10 block"
    );
}

#[test]
fn wait_change_uses_a_fixed_baseline_so_gradual_drift_is_caught() {
    // Each step changes one more cell than the last, but every individual step
    // is below the 5% area threshold. Only comparison against the *original*
    // baseline accumulates enough difference to fire.
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(
        scene(),
        vec![
            vec![],
            vec![0],             // 1% cumulative
            vec![0, 1],          // 2%
            vec![0, 1, 2],       // 3%
            vec![0, 1, 2, 3],    // 4%
            vec![0, 1, 2, 3, 4], // 5% - crosses the threshold
        ],
    );

    let result = wait_for_change(
        &mut source,
        &clock,
        &WaitChangeOptions {
            temporal: temporal_options(),
        },
    )
    .unwrap();

    assert_eq!(result.outcome, Outcome::Changed);
    assert_eq!(
        result.comparison.changed_pixels, 500,
        "the baseline comparison must see all five accumulated cells"
    );
    assert!(result.comparison.changed_fraction >= 0.05);
    // Six captures: the baseline plus five samples.
    assert_eq!(result.captures, 6);
    assert_eq!(result.comparisons, 5);
}

#[test]
fn wait_change_that_never_happens_times_out() {
    // A A A A ...
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![], vec![], vec![]]);
    let options = WaitChangeOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_secs(1),
            ..temporal_options()
        },
    };

    let result = wait_for_change(&mut source, &clock, &options).unwrap();

    assert_eq!(result.outcome, Outcome::Timeout);
    assert!(!result.comparison.changed);
    assert!(result.captures > 1, "it should have kept polling");
    // A timeout still reports where it got to.
    assert_eq!(result.comparison.changed_pixels, 0);
    assert_eq!(result.comparison.bounding_box, None);
    assert!(result.elapsed < Duration::from_secs(2));
}

#[test]
fn wait_change_ignores_noise_below_the_pixel_threshold() {
    // Cell 0 is shaded by 5 in every channel, below the threshold of 12.
    let clock = ManualClock::new();
    let mut source = ShadedSource {
        served: 0,
        width: 100,
        height: 100,
    };

    let result = wait_for_change(
        &mut source,
        &clock,
        &WaitChangeOptions {
            temporal: TemporalCompareOptions {
                timeout: Duration::from_millis(500),
                ..temporal_options()
            },
        },
    )
    .unwrap();

    assert_eq!(
        result.outcome,
        Outcome::Timeout,
        "a sub-threshold shading difference must not count as change"
    );
    assert_eq!(result.comparison.changed_pixels, 0);
}

#[test]
fn wait_change_reports_a_geometry_change_rather_than_comparing_mismatched_grids() {
    let clock = ManualClock::new();
    let mut source = ResizingSource {
        served: 0,
        small: Scene::new(10, 10, 10),
        large: Scene::new(20, 20, 10),
        switch_after: 2,
    };

    let error = wait_for_change(
        &mut source,
        &clock,
        &WaitChangeOptions {
            temporal: temporal_options(),
        },
    )
    .unwrap_err();

    assert_eq!(error.code(), "geometry_changed");
    assert!(
        error.message().contains("100x100") && error.message().contains("200x200"),
        "message should name both geometries, was: {}",
        error.message()
    );
}

#[test]
fn wait_change_reports_a_target_that_moved() {
    let clock = ManualClock::new();
    let mut source = MovingSource {
        served: 0,
        move_after: 2,
        frame_data: vec![0u8; 20 * 20 * 3],
    };

    let error = wait_for_change(
        &mut source,
        &clock,
        &WaitChangeOptions {
            temporal: sensitive_options(),
        },
    )
    .unwrap_err();

    assert_eq!(error.code(), "geometry_changed");
    assert!(
        error.message().contains("moved"),
        "message should say it moved, was: {}",
        error.message()
    );
}

#[test]
fn wait_change_aborts_on_a_capture_failure() {
    let clock = ManualClock::new();
    let mut source = FailingSource {
        script: ScriptedSource::new(scene(), vec![vec![], vec![]]),
        successes: 2,
    };
    // The baseline and one sample succeed, the next capture fails.
    let options = WaitChangeOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_secs(5),
            ..temporal_options()
        },
    };

    let error = wait_for_change(&mut source, &clock, &options).unwrap_err();
    assert_eq!(error.code(), "capture_failed");
    assert!(error.message().contains("display went away"));
}

#[test]
fn wait_change_aborts_immediately_when_the_first_capture_fails() {
    let clock = ManualClock::new();
    let mut source = FailingSource {
        script: ScriptedSource::new(scene(), vec![vec![]]),
        successes: 0,
    };
    let error = wait_for_change(
        &mut source,
        &clock,
        &WaitChangeOptions {
            temporal: temporal_options(),
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "capture_failed");
}

/// A source that applies a fixed sub-threshold shade to every frame.
struct ShadedSource {
    served: usize,
    width: u32,
    height: u32,
}

impl FrameSource for ShadedSource {
    fn capture(&mut self) -> Result<Frame, Error> {
        self.served += 1;
        // Alternate between two shades 5 apart.
        let value = if self.served % 2 == 0 { 5u8 } else { 0u8 };
        let data = vec![value; (self.width * self.height * 3) as usize];
        eensh::input::frame_from_rgb8(self.width, self.height, data)
    }
}

// ============================================================================
// 32. wait-stable
// ============================================================================

#[test]
fn wait_stable_returns_immediately_for_an_already_stable_scene() {
    // A A A A - stable throughout, so it should complete after stable_for.
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![], vec![], vec![]]);
    let options = WaitStableOptions {
        temporal: sensitive_options(),
        stable_for: Duration::from_millis(300),
    };

    let result = wait_for_stable(&mut source, &clock, &options).unwrap();

    assert_eq!(result.outcome, Outcome::Stable);
    assert!(!result.last_comparison.changed);
    // The stability interval starts at the first frame, so 300ms of stillness is
    // reached on the fourth sample (0, 100, 200, 300ms).
    assert!(result.stable_duration >= Duration::from_millis(300));
    assert!(result.elapsed >= Duration::from_millis(300));
}

#[test]
fn wait_stable_times_out_when_the_scene_never_settles() {
    // A B C D E F ... a different cell each time.
    let clock = ManualClock::new();
    let script: Vec<Vec<usize>> = (0..40).map(|i| vec![i % 100]).collect();
    let mut source = ScriptedSource::new(scene(), script);
    let options = WaitStableOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_millis(700),
            ..sensitive_options()
        },
        stable_for: Duration::from_millis(300),
    };

    let result = wait_for_stable(&mut source, &clock, &options).unwrap();

    assert_eq!(result.outcome, Outcome::Timeout);
    assert!(
        result.last_comparison.changed,
        "the last sample still differed"
    );
    assert!(result.elapsed <= Duration::from_millis(900));
}

#[test]
fn wait_stable_settles_after_a_change_then_a_quiet_period() {
    // A B C C C C - settles on C.
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(
        scene(),
        vec![
            vec![],
            vec![0],
            vec![0, 1],
            vec![0, 1],
            vec![0, 1],
            vec![0, 1],
        ],
    );
    let options = WaitStableOptions {
        temporal: sensitive_options(),
        stable_for: Duration::from_millis(300),
    };

    let result = wait_for_stable(&mut source, &clock, &options).unwrap();

    assert_eq!(result.outcome, Outcome::Stable);
    assert!(!result.last_comparison.changed);
    // The final frame must be the settled C, not the first C it saw.
    assert_eq!(result.frame.source_geometry.width, 100);

    // Verify the returned frame really is C, not A or B.
    let expected_c = scene().render(&[0, 1]);
    assert_eq!(
        result.frame.pixels.data(),
        expected_c.pixels.data(),
        "the returned frame should be the settled scene"
    );
}

#[test]
fn a_brief_quiet_period_shorter_than_stable_for_does_not_complete_the_operation() {
    // A B B C C C C
    //   ^^^ two quiet samples = 200ms, which is less than stable_for = 300ms, so
    //   the operation must not complete there.
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(
        scene(),
        vec![
            vec![],     // A
            vec![0],    // B
            vec![0],    // B
            vec![0, 1], // C
            vec![0, 1],
            vec![0, 1],
            vec![0, 1],
        ],
    );
    let options = WaitStableOptions {
        temporal: sensitive_options(),
        stable_for: Duration::from_millis(300),
    };

    let result = wait_for_stable(&mut source, &clock, &options).unwrap();

    assert_eq!(result.outcome, Outcome::Stable);
    let expected_c = scene().render(&[0, 1]);
    assert_eq!(
        result.frame.pixels.data(),
        expected_c.pixels.data(),
        "it must settle on the final C, not on the earlier B plateau"
    );
}

#[test]
fn wait_stable_noise_below_the_threshold_does_not_reset_the_timer() {
    // The scene alternates by a sub-threshold shade every sample, which must be
    // treated as continuous stability.
    let clock = ManualClock::new();
    let mut source = ShadedSource {
        served: 0,
        width: 100,
        height: 100,
    };
    let options = WaitStableOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_secs(2),
            ..temporal_options()
        },
        stable_for: Duration::from_millis(300),
    };

    let result = wait_for_stable(&mut source, &clock, &options).unwrap();
    assert_eq!(result.outcome, Outcome::Stable);
    assert!(!result.last_comparison.changed);
}

#[test]
fn a_single_sample_of_meaningful_activity_resets_the_stability_timer() {
    // A A A B A A A A - the lone B must restart the clock even though the very
    // next frame returns to A. B changes 6 cells, which is 6% and so clears the
    // 5% area threshold; a smaller blip would correctly be ignored as noise.
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(
        scene(),
        vec![
            vec![],
            vec![],
            vec![],
            vec![0, 1, 2, 3, 4, 5], // a single blip
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        ],
    );
    let options = WaitStableOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_secs(5),
            ..temporal_options()
        },
        stable_for: Duration::from_millis(300),
    };

    let result = wait_for_stable(&mut source, &clock, &options).unwrap();
    assert_eq!(result.outcome, Outcome::Stable);

    // The blip arrives at t=300ms, and the return to A at t=400ms is itself a
    // change, so the stability window can only start at 400ms and completion
    // cannot come before 400 + 300 = 700ms. Without the reset it would have
    // completed at t=300ms, having never noticed the blip at all.
    assert!(
        result.elapsed >= Duration::from_millis(700),
        "the stability timer should have been reset by the blip; elapsed was {:?}",
        result.elapsed
    );
}

#[test]
fn wait_stable_rejects_a_zero_stable_duration() {
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![]]);
    let error = wait_for_stable(
        &mut source,
        &clock,
        &WaitStableOptions {
            temporal: sensitive_options(),
            stable_for: Duration::ZERO,
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "invalid_duration");
    assert!(error.message().contains("stable duration"));
}

// ============================================================================
// 33. observe
// ============================================================================

#[test]
fn observe_detects_a_transition_and_settles_on_the_result() {
    // A A B C D D D D - first change at B, settle on D.
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(
        scene(),
        vec![
            vec![],                                     // A baseline
            vec![],                                     // A
            vec![0, 1, 2, 3, 4, 5],                     // B: meaningful change (6 cells)
            vec![0, 1, 2, 3, 4, 5, 6],                  // C
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11], // D: settles here
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
        ],
    );
    let options = ObserveOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_secs(5),
            ..temporal_options()
        },
        stable_for: Duration::from_millis(300),
    };

    let result = observe(&mut source, &clock, &options).unwrap();

    assert_eq!(result.outcome, Outcome::Observed);
    assert!(
        result.first_change.is_some(),
        "a change should have been detected"
    );
    assert!(
        result.first_change.unwrap().changed,
        "the recorded first change must itself be a change"
    );
    assert!(result.change_detected_at.is_some());
    assert!(!result.last_comparison.changed, "it should end settled");

    // The final frame is the settled scene.
    let expected = scene().render(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    assert_eq!(result.frame.pixels.data(), expected.pixels.data());
}

#[test]
fn observe_times_out_when_nothing_ever_changes() {
    // A A A A ...
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![], vec![]]);
    let options = ObserveOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_millis(500),
            ..temporal_options()
        },
        stable_for: Duration::from_millis(300),
    };

    let result = observe(&mut source, &clock, &options).unwrap();

    assert_eq!(result.outcome, Outcome::Timeout);
    assert!(result.first_change.is_none(), "no change was ever detected");
    assert_eq!(result.change_detected_at, None);
}

#[test]
fn observe_times_out_after_detecting_a_change_that_never_settles() {
    // The transition starts but the scene keeps moving past the deadline.
    //
    // The script alternates between two disjoint six-cell groups, so every
    // consecutive pair differs by twelve cells (12%). A sliding window would have
    // been wrong here: adjacent frames would overlap and differ by only 2%, which
    // the 5% threshold would treat as stable even though the scene never stops
    // moving.
    let clock = ManualClock::new();
    let script: Vec<Vec<usize>> = (0..30)
        .map(|i| {
            if i % 2 == 0 {
                (0..6).collect()
            } else {
                (10..16).collect()
            }
        })
        .collect();
    let mut source = ScriptedSource::new(scene(), script);
    let options = ObserveOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_millis(800),
            ..temporal_options()
        },
        stable_for: Duration::from_millis(300),
    };

    let result = observe(&mut source, &clock, &options).unwrap();

    assert_eq!(result.outcome, Outcome::Timeout);
    assert!(
        result.first_change.is_some(),
        "the change should be recorded even though it never settled"
    );
    assert!(
        result.change_detected_at.is_some(),
        "the caller should be able to tell the change happened before the timeout"
    );
}

#[test]
fn observe_completes_when_the_scene_returns_to_the_baseline() {
    // A B A A A A - a transient acknowledgement that reverts.
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(
        scene(),
        vec![
            vec![],                 // A
            vec![0, 1, 2, 3, 4, 5], // B: change
            vec![],                 // back to A
            vec![],
            vec![],
            vec![],
            vec![],
        ],
    );
    let options = ObserveOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_secs(5),
            ..temporal_options()
        },
        stable_for: Duration::from_millis(300),
    };

    let result = observe(&mut source, &clock, &options).unwrap();

    assert_eq!(
        result.outcome,
        Outcome::Observed,
        "settling back onto the original scene is a successful observation"
    );
    // The final frame must be the baseline scene again.
    let expected = scene().render(&[]);
    assert_eq!(result.frame.pixels.data(), expected.pixels.data());
}

#[test]
fn observe_survives_several_bursts_of_activity_before_settling() {
    // A B C C D E E E E - the stability timer is reset by D and E.
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(
        scene(),
        vec![
            vec![],                                   // A baseline
            vec![0, 1, 2, 3, 4, 5],                   // B: change detected
            vec![0, 1, 2, 3, 4, 5, 6],                // C
            vec![0, 1, 2, 3, 4, 5, 6],                // C quiet
            vec![10, 11, 12, 13, 14, 15, 16, 17],     // D: another burst
            vec![20, 21, 22, 23, 24, 25, 26, 27, 28], // E
            vec![20, 21, 22, 23, 24, 25, 26, 27, 28], // E quiet
            vec![20, 21, 22, 23, 24, 25, 26, 27, 28],
            vec![20, 21, 22, 23, 24, 25, 26, 27, 28],
            vec![20, 21, 22, 23, 24, 25, 26, 27, 28],
        ],
    );
    let options = ObserveOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_secs(5),
            ..temporal_options()
        },
        stable_for: Duration::from_millis(300),
    };

    let result = observe(&mut source, &clock, &options).unwrap();
    assert_eq!(result.outcome, Outcome::Observed);

    let expected = scene().render(&[20, 21, 22, 23, 24, 25, 26, 27, 28]);
    assert_eq!(
        result.frame.pixels.data(),
        expected.pixels.data(),
        "it must settle on the final E, not on the earlier C plateau"
    );
}

#[test]
fn observe_does_not_return_on_the_first_changed_frame() {
    // If observe returned on the first change, it would stop at B. It must keep
    // going until the scene settles.
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(
        scene(),
        vec![
            vec![],                    // A
            vec![0, 1, 2, 3, 4, 5],    // B: first change
            vec![0, 1, 2, 3, 4, 5, 6], // C
            vec![7, 8, 9, 10, 11, 12], // D: settles
            vec![7, 8, 9, 10, 11, 12],
            vec![7, 8, 9, 10, 11, 12],
            vec![7, 8, 9, 10, 11, 12],
        ],
    );
    let options = ObserveOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_secs(5),
            ..temporal_options()
        },
        stable_for: Duration::from_millis(300),
    };

    let result = observe(&mut source, &clock, &options).unwrap();

    let b = scene().render(&[0, 1, 2, 3, 4, 5]);
    assert_ne!(
        result.frame.pixels.data(),
        b.pixels.data(),
        "returning the first changed frame would capture a half-drawn state"
    );
    let d = scene().render(&[7, 8, 9, 10, 11, 12]);
    assert_eq!(result.frame.pixels.data(), d.pixels.data());
}

#[test]
fn observe_reports_a_geometry_change_mid_observation() {
    let clock = ManualClock::new();
    let mut source = ResizingSource {
        served: 0,
        small: Scene::new(10, 10, 10),
        large: Scene::new(20, 20, 10),
        switch_after: 3,
    };

    let error = observe(&mut source, &clock, &ObserveOptions::with_defaults()).unwrap_err();
    assert_eq!(error.code(), "geometry_changed");
}

#[test]
fn observe_aborts_on_a_capture_failure_while_settling() {
    let clock = ManualClock::new();
    let mut source = FailingSource {
        script: ScriptedSource::new(
            scene(),
            vec![vec![], vec![0, 1, 2, 3, 4, 5], vec![0, 1, 2, 3, 4, 5]],
        ),
        successes: 4,
    };
    let options = ObserveOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_secs(5),
            ..temporal_options()
        },
        stable_for: Duration::from_millis(300),
    };

    let error = observe(&mut source, &clock, &options).unwrap_err();
    assert_eq!(error.code(), "capture_failed");
}

// ============================================================================
// timing, counting, and scheduling
// ============================================================================

#[test]
fn the_capture_and_comparison_counts_are_consistent() {
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![], vec![], vec![0, 1, 2, 3, 4, 5]]);

    let result = wait_for_change(
        &mut source,
        &clock,
        &WaitChangeOptions {
            temporal: temporal_options(),
        },
    )
    .unwrap();

    assert_eq!(
        result.comparisons,
        result.captures - 1,
        "comparisons should be one fewer than captures"
    );
    assert_eq!(result.timing.captures, result.captures);
    assert_eq!(result.timing.comparisons, result.comparisons);
}

#[test]
fn comparison_counts_hold_for_a_plain_timeout_too() {
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![]]);
    let options = WaitStableOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_millis(500),
            ..sensitive_options()
        },
        stable_for: Duration::from_millis(100),
    };

    let result = wait_for_stable(&mut source, &clock, &options).unwrap();
    assert_eq!(result.comparisons, result.captures - 1);
}

#[test]
fn observed_timing_accounts_for_capture_compare_and_sleep() {
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![], vec![], vec![0, 1, 2, 3, 4, 5]]);

    let result = wait_for_change(
        &mut source,
        &clock,
        &WaitChangeOptions {
            temporal: temporal_options(),
        },
    )
    .unwrap();

    let timing = result.timing;
    assert_eq!(timing.captures, 3);
    assert_eq!(timing.comparisons, 2);
    assert!(timing.elapsed_us > 0);
    // The manual clock advances through the requested sleeps, so the sleep total
    // is the interval times the number of polls that had to wait.
    assert_eq!(timing.sleep_us_total, 200_000, "two 100ms intervals");
    // The operation cannot have spent more wall time than it elapsed.
    assert!(timing.sleep_us_total <= timing.elapsed_us);
}

#[test]
fn polling_uses_a_fixed_cadence_without_accumulating_drift() {
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![]]);
    let options = WaitChangeOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_millis(500),
            interval: Duration::from_millis(100),
            ..sensitive_options()
        },
    };

    wait_for_change(&mut source, &clock, &options).unwrap();

    let deadlines = clock.requested_deadlines();
    assert!(!deadlines.is_empty());
    // Every requested deadline is an exact multiple of the interval, because the
    // schedule is anchored to the start rather than to the previous sample.
    for deadline in &deadlines {
        assert_eq!(
            deadline.as_millis() % 100,
            0,
            "deadline {deadline:?} is not on the 100ms cadence"
        );
    }
}

#[test]
fn a_timeout_stops_polling_at_the_deadline() {
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![]]);
    let options = WaitChangeOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_millis(350),
            interval: Duration::from_millis(100),
            ..sensitive_options()
        },
    };

    let result = wait_for_change(&mut source, &clock, &options).unwrap();

    assert_eq!(result.outcome, Outcome::Timeout);
    // The global deadline is never extended by activity, and never overrun.
    assert!(
        result.elapsed <= Duration::from_millis(400),
        "should stop at the 350ms deadline, elapsed was {:?}",
        result.elapsed
    );
}

#[test]
fn the_timeout_is_a_single_total_deadline_and_is_not_reset_by_activity() {
    // A scene that keeps changing must still stop at the deadline.
    let clock = ManualClock::new();
    let script: Vec<Vec<usize>> = (0..40).map(|i| vec![i % 100]).collect();
    let mut source = ScriptedSource::new(scene(), script);
    let options = ObserveOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_millis(500),
            ..sensitive_options()
        },
        stable_for: Duration::from_millis(300),
    };

    let result = observe(&mut source, &clock, &options).unwrap();
    assert_eq!(result.outcome, Outcome::Timeout);
    assert!(
        result.elapsed <= Duration::from_millis(550),
        "the deadline must not be extended by continued activity; elapsed was {:?}",
        result.elapsed
    );
}

// ============================================================================
// validation
// ============================================================================

#[test]
fn a_zero_interval_is_rejected() {
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![]]);
    let error = wait_for_change(
        &mut source,
        &clock,
        &WaitChangeOptions {
            temporal: TemporalCompareOptions {
                interval: Duration::ZERO,
                ..temporal_options()
            },
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "invalid_duration");
}

#[test]
fn a_zero_timeout_is_rejected() {
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![]]);
    let error = wait_for_change(
        &mut source,
        &clock,
        &WaitChangeOptions {
            temporal: TemporalCompareOptions {
                timeout: Duration::ZERO,
                ..temporal_options()
            },
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "invalid_duration");
}

#[test]
fn an_out_of_range_area_threshold_is_rejected() {
    let clock = ManualClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![]]);
    let error = wait_for_change(
        &mut source,
        &clock,
        &WaitChangeOptions {
            temporal: TemporalCompareOptions {
                compare: CompareOptions {
                    area_threshold: 1.5,
                    ..temporal_options().compare
                },
                ..temporal_options()
            },
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "invalid_arguments");
}

#[test]
fn the_defaults_are_the_documented_temporal_profile() {
    let temporal = TemporalCompareOptions::default();
    assert_eq!(temporal.compare.mode, CompareMode::RgbThreshold);
    assert_eq!(temporal.compare.pixel_threshold, 12);
    assert_eq!(temporal.compare.area_threshold, 0.005);
    assert_eq!(temporal.interval, Duration::from_millis(100));
    assert_eq!(temporal.timeout, Duration::from_secs(5));

    assert_eq!(
        ObserveOptions::with_defaults().stable_for,
        Duration::from_millis(300)
    );
    assert_eq!(
        WaitStableOptions::default().stable_for,
        Duration::from_millis(300)
    );
}

#[test]
fn the_temporal_defaults_differ_from_the_diff_defaults_on_purpose() {
    // `diff` asks "did anything differ?" and defaults to exact comparison.
    // Temporal observation asks "did anything meaningfully change?" and defaults
    // to a noise-tolerant threshold.
    let diff = CompareOptions::default();
    let temporal = TemporalCompareOptions::default().compare;

    assert_eq!(diff.mode, CompareMode::Exact);
    assert_eq!(diff.pixel_threshold, 0);
    assert_ne!(temporal.mode, diff.mode);
    assert_ne!(temporal.pixel_threshold, diff.pixel_threshold);
    assert_ne!(temporal.area_threshold, diff.area_threshold);
}

#[test]
fn the_outcome_names_are_stable() {
    assert_eq!(Outcome::Changed.name(), "changed");
    assert_eq!(Outcome::Stable.name(), "stable");
    assert_eq!(Outcome::Observed.name(), "observed");
    assert_eq!(Outcome::Timeout.name(), "timeout");
}

// ============================================================================
// a small number of real-time tests
// ============================================================================

#[test]
fn a_real_clock_observation_completes_against_a_live_source() {
    // One real-time test with generous margins, so that the system clock path and
    // the real sleep are exercised at least once rather than only the manual one.
    let clock = SystemClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![], vec![], vec![0, 1, 2, 3, 4, 5]]);
    let options = WaitChangeOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_secs(5),
            interval: Duration::from_millis(10),
            ..temporal_options()
        },
    };

    let result = wait_for_change(&mut source, &clock, &options).unwrap();
    assert_eq!(result.outcome, Outcome::Changed);
    assert!(result.elapsed < Duration::from_secs(2));
    assert!(result.elapsed >= Duration::from_millis(10), "it did poll");
}

#[test]
fn a_real_clock_stability_wait_completes_in_roughly_the_stable_duration() {
    let clock = SystemClock::new();
    let mut source = ScriptedSource::new(scene(), vec![vec![]]);
    let options = WaitStableOptions {
        temporal: TemporalCompareOptions {
            timeout: Duration::from_secs(5),
            interval: Duration::from_millis(10),
            ..sensitive_options()
        },
        stable_for: Duration::from_millis(50),
    };

    let result = wait_for_stable(&mut source, &clock, &options).unwrap();
    assert_eq!(result.outcome, Outcome::Stable);
    assert!(
        result.elapsed >= Duration::from_millis(50),
        "must have actually waited for stability; elapsed was {:?}",
        result.elapsed
    );
    assert!(
        result.elapsed < Duration::from_secs(2),
        "must not have waited far longer than asked; elapsed was {:?}",
        result.elapsed
    );
}
