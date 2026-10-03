//! Phase 3 Xvfb integration tests: live temporal observation.
//!
//! These drive the real `eensh wait-change`, `wait-stable`, and `observe`
//! commands against a live Xvfb display while a background thread paints known
//! rectangles onto it.
//!
//! Two environment facts shape the design of these tests:
//!
//! * **Xvfb clears the root window when its last client disconnects.** The
//!   harness holds a `Screen` connection open for the whole scenario, exactly as
//!   the Phase 2 verification established.
//! * **The paint thread and the observer run concurrently.** That is the point:
//!   temporal observation only means anything if the scene changes *while* it is
//!   being watched. The painter holds its own X connection so it never races the
//!   observer's.
//!
//! The painter uses non-blocking sleeps and paints on a schedule, so a scenario
//! is expressed as "wait 150ms, then paint this" and the assertion is on what the
//! observation returned.

mod common;

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;

const SCREEN_W: u32 = 400;
const SCREEN_H: u32 = 300;

fn skip_if_no_xvfb() -> bool {
    if find_xvfb().is_none() {
        assert!(!xvfb_required(), "Xvfb is required but missing");
        return false;
    }
    true
}

/// Paint on a schedule from a background thread, holding its own X connection.
///
/// A scripted painter is what makes these tests deterministic rather than racy:
/// the paints happen at known offsets from the start of the observation, and the
/// observer is expected to end up in the corresponding state.
struct Painter {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Painter {
    /// Start painting according to `script`, as `(delay_from_start, paints)`.
    fn start(display: String, script: Vec<(Duration, Vec<PaintOp>)>) -> Painter {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = std::thread::spawn(move || {
            let screen = Screen::open(&display);
            let masks = screen.visual_masks();

            let started = std::time::Instant::now();
            for (delay, ops) in script {
                // Sleep in small slices so the stop flag is honoured promptly.
                while started.elapsed() < delay {
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    let remaining = delay.saturating_sub(started.elapsed());
                    std::thread::sleep(remaining.min(Duration::from_millis(5)));
                }
                if stop_flag.load(Ordering::Relaxed) {
                    return;
                }
                for op in &ops {
                    screen.fill(
                        screen.root(),
                        op.x,
                        op.y,
                        op.width,
                        op.height,
                        rgb_to_pixel(masks, op.rgb),
                    );
                }
            }

            // Hold the connection open until told to stop, so Xvfb does not reset
            // the root window while the observer is still capturing.
            while !stop_flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        Painter {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// One rectangle to paint.
#[derive(Debug, Clone, Copy)]
struct PaintOp {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    rgb: [u8; 3],
}

/// A rectangle covering a fraction of the screen.
///
/// With area thresholds expressed as fractions, it is convenient to paint a
/// region whose area is an exact percentage of the 400x300 screen. A 100x60
/// rectangle is 6% of it, which clears the default 0.5% threshold comfortably.
fn rect(x: i32, y: i32, rgb: [u8; 3]) -> PaintOp {
    PaintOp {
        x,
        y,
        width: 100,
        height: 60,
        rgb,
    }
}

/// Run an observation command and parse its JSON **stdout** response.
///
/// Returns `(exit_code, response, stderr)`. When stdout is not JSON the response
/// is `Null` and the third element explains what both streams contained, so a
/// failure message is useful rather than just "expected value".
fn run_observation(args: &[&str]) -> (i32, serde_json::Value, String) {
    let output = Command::new(eensh_binary())
        .args(args)
        .output()
        .expect("failed to run eensh");

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    match serde_json::from_str(stdout.trim()) {
        Ok(value) => (code, value, stderr),
        Err(error) => (
            code,
            serde_json::Value::Null,
            format!("stdout is not JSON ({error}); stdout: {stdout:?} stderr: {stderr:?}"),
        ),
    }
}

/// Run an observation command and parse its JSON **stderr** response.
///
/// Failures are reported on stderr, as they are for every other `eensh` command.
fn run_observation_error(args: &[&str]) -> (i32, serde_json::Value) {
    let output = Command::new(eensh_binary())
        .args(args)
        .output()
        .expect("failed to run eensh");

    let code = output.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|error| panic!("stderr is not JSON ({error}): {stderr:?}"));

    (code, value)
}

// ============================================================================
// 34.1 wait-change
// ============================================================================

#[test]
fn wait_change_returns_when_the_scene_changes() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    // Hold a connection for the scenario, and paint a white screen immediately.
    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [255, 255, 255]),
    );

    // Paint a red rectangle 150ms in, well after the baseline is taken.
    let painter = Painter::start(
        display.clone(),
        vec![(Duration::from_millis(150), vec![rect(100, 80, [255, 0, 0])])],
    );

    let (code, response, stderr) = run_observation(&[
        "wait-change",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "5s",
        "--json",
    ]);

    assert_eq!(code, 0, "expected success, stderr: {stderr}");

    let observation = &response["observation"];
    assert_eq!(observation["kind"], "wait_change");
    assert_eq!(observation["result"], "changed");

    // The changed rectangle should be identified.
    let bounding = &response["comparison"]["bounding_box"];
    assert_eq!(bounding["x"], 100);
    assert_eq!(bounding["y"], 80);
    assert_eq!(bounding["width"], 100);
    assert_eq!(bounding["height"], 60);

    // 100x60 of 400x300.
    assert_eq!(response["comparison"]["changed_pixels"], 6000);
    assert_eq!(response["comparison"]["total_pixels"], 120_000);
    assert_eq!(response["comparison"]["changed"], true);

    // The returned frame is at native resolution, not resized.
    assert_eq!(response["image"]["width"], SCREEN_W);
    assert_eq!(response["image"]["height"], SCREEN_H);
    assert_eq!(response["source"]["width"], SCREEN_W);

    drop(painter);
}

#[test]
fn wait_change_times_out_and_reports_the_latest_state_when_nothing_happens() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [10, 20, 30]),
    );

    let (code, response, stderr) = run_observation(&[
        "wait-change",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "400ms",
        "--json",
    ]);

    // A timeout is its own exit status, distinct from every error code.
    assert_eq!(code, 100, "expected the timeout status, stderr: {stderr}");

    let observation = &response["observation"];
    assert_eq!(observation["result"], "timeout");
    assert_eq!(response["comparison"]["changed"], false);

    // A timeout still returns the latest frame and the counts.
    assert_eq!(response["image"]["width"], SCREEN_W);
    assert!(observation["captures"].as_u64().unwrap() > 1);
    assert!(observation["elapsed_ms"].as_u64().unwrap() >= 400);
}

#[test]
fn wait_change_ignores_a_change_below_the_pixel_threshold() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [100, 100, 100]),
    );

    // Shade a rectangle by only 5 per channel, well under the default threshold
    // of 12.
    let painter = Painter::start(
        display.clone(),
        vec![(
            Duration::from_millis(100),
            vec![rect(50, 50, [105, 105, 105])],
        )],
    );

    let (code, response, _stderr) = run_observation(&[
        "wait-change",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "500ms",
        "--json",
    ]);

    assert_eq!(
        code, 100,
        "a sub-threshold shading must not count as change"
    );
    assert_eq!(response["observation"]["result"], "timeout");
    assert_eq!(response["comparison"]["changed_pixels"], 0);

    drop(painter);
}

#[test]
fn wait_change_requires_a_meaningful_area_not_just_a_few_pixels() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [0, 0, 0]),
    );

    // A single pixel changes: 1 in 120000, far below the default 0.5%.
    let painter = Painter::start(
        display.clone(),
        vec![(
            Duration::from_millis(100),
            vec![PaintOp {
                x: 200,
                y: 150,
                width: 1,
                height: 1,
                rgb: [255, 255, 255],
            }],
        )],
    );

    let (code, response, _stderr) = run_observation(&[
        "wait-change",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "500ms",
        "--json",
    ]);

    assert_eq!(code, 100, "one pixel must not clear the area threshold");
    assert_eq!(response["observation"]["result"], "timeout");
    // The pixel difference is still reported, even though it was not meaningful.
    assert_eq!(response["comparison"]["changed_pixels"], 1);
    assert!(!response["comparison"]["bounding_box"].is_null());

    drop(painter);
}

// ============================================================================
// 34.2 wait-stable
// ============================================================================

#[test]
fn wait_stable_does_not_return_while_painting_continues_then_returns_after_it_stops() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [0, 0, 0]),
    );

    // Four paints, 100ms apart, then silence. The observer is asked for 400ms of
    // stability, so it cannot complete before the last paint plus 400ms.
    let start = std::time::Instant::now();
    let painter = Painter::start(
        display.clone(),
        vec![
            (Duration::from_millis(100), vec![rect(0, 0, [255, 0, 0])]),
            (Duration::from_millis(200), vec![rect(100, 0, [0, 255, 0])]),
            (Duration::from_millis(300), vec![rect(200, 0, [0, 0, 255])]),
            (
                Duration::from_millis(400),
                vec![rect(300, 0, [255, 255, 0])],
            ),
        ],
    );

    let (code, response, stderr) = run_observation(&[
        "wait-stable",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "10s",
        "--stable-for",
        "400ms",
        "--json",
    ]);

    assert_eq!(code, 0, "expected stability, stderr: {stderr}");

    let observation = &response["observation"];
    assert_eq!(observation["kind"], "wait_stable");
    assert_eq!(observation["result"], "stable");
    assert_eq!(observation["stable_for_ms"], 400);

    // It must not have declared stability before the last paint plus 400ms.
    let elapsed = observation["elapsed_ms"].as_u64().unwrap();
    let wall = start.elapsed().as_millis() as u64;
    assert!(
        wall >= 800,
        "the observer returned at {wall}ms, before the last paint at 400ms plus \
         400ms of required stability"
    );
    // And it must have actually reported a meaningful stable duration.
    assert!(observation["stable_duration_ms"].as_u64().unwrap() >= 400);
    let _ = elapsed;

    drop(painter);
}

#[test]
fn wait_stable_returns_promptly_for_an_already_static_scene() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [40, 40, 40]),
    );

    let started = std::time::Instant::now();
    let (code, response, stderr) = run_observation(&[
        "wait-stable",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "5s",
        "--stable-for",
        "200ms",
        "--json",
    ]);
    let elapsed = started.elapsed();

    assert_eq!(code, 0, "expected stability, stderr: {stderr}");
    assert_eq!(response["observation"]["result"], "stable");
    // An already-static screen should settle in roughly stable_for, not in the
    // full timeout. The margin is generous because the connection handshake is
    // slow on some Xvfb builds.
    assert!(
        elapsed < Duration::from_secs(3),
        "an already-stable scene should not take the full timeout; took {elapsed:?}"
    );
}

#[test]
fn wait_stable_times_out_when_the_scene_never_stops_moving() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [0, 0, 0]),
    );

    // A different rectangle every 40ms for longer than the timeout.
    let script: Vec<(Duration, Vec<PaintOp>)> = (0..30)
        .map(|i| {
            let shade = (20 * i) as u8;
            (
                Duration::from_millis(40 * i as u64),
                vec![PaintOp {
                    x: (10 * i) % 300,
                    y: (7 * i) % 200,
                    width: 80,
                    height: 60,
                    rgb: [shade, 255 - shade, shade],
                }],
            )
        })
        .collect();
    let painter = Painter::start(display.clone(), script);

    let (code, response, _stderr) = run_observation(&[
        "wait-stable",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "600ms",
        "--stable-for",
        "400ms",
        "--json",
    ]);

    assert_eq!(code, 100, "a continuously changing scene must time out");
    assert_eq!(response["observation"]["result"], "timeout");

    drop(painter);
}

// ============================================================================
// 34.3 observe
// ============================================================================

#[test]
fn observe_detects_a_transition_and_returns_the_settled_frame() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    let white = [255u8, 255, 255];
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, white),
    );

    // A transition that develops over several steps and then settles: the final
    // state is a red rectangle in the bottom right, and the intermediate greens
    // and blues are the "partial render" that observe must not return.
    let final_rgb = [255u8, 0, 0];
    let painter = Painter::start(
        display.clone(),
        vec![
            (Duration::from_millis(120), vec![rect(0, 0, [0, 255, 0])]),
            (Duration::from_millis(200), vec![rect(100, 0, [0, 0, 255])]),
            (Duration::from_millis(280), vec![rect(300, 240, final_rgb)]),
        ],
    );

    let (code, response, stderr) = run_observation(&[
        "observe",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "10s",
        "--stable-for",
        "300ms",
        "--json",
    ]);

    assert_eq!(code, 0, "expected a settled observation, stderr: {stderr}");

    let observation = &response["observation"];
    assert_eq!(observation["kind"], "observe");
    assert_eq!(observation["result"], "observed");

    // A change was detected, and when.
    assert!(
        response["first_change"]["changed"].as_bool().unwrap(),
        "a first change should have been recorded"
    );
    assert!(observation["change_detected_ms"].as_u64().is_some());

    // The final frame must be the settled state, at native resolution so the
    // comparison could run on it.
    assert_eq!(response["image"]["width"], SCREEN_W);
    assert_eq!(response["image"]["height"], SCREEN_H);

    // The last comparison should be settled.
    assert_eq!(response["comparison"]["changed"], false);

    drop(painter);
}

#[test]
fn observe_times_out_when_nothing_ever_changes() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [7, 7, 7]),
    );

    let (code, response, _stderr) = run_observation(&[
        "observe",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "400ms",
        "--stable-for",
        "200ms",
        "--json",
    ]);

    assert_eq!(code, 100);
    assert_eq!(response["observation"]["result"], "timeout");
    // No change was ever detected, and the response says so by omission.
    assert!(response.get("first_change").is_none() || response["first_change"].is_null());
    assert!(response["observation"].get("change_detected_ms").is_none());
}

#[test]
fn observe_records_a_change_that_never_settles() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [0, 0, 0]),
    );

    let script: Vec<(Duration, Vec<PaintOp>)> = (0..30)
        .map(|i| {
            (
                Duration::from_millis(30 * i as u64),
                vec![PaintOp {
                    x: (9 * i) % 300,
                    y: (5 * i) % 200,
                    width: 80,
                    height: 60,
                    rgb: [(31 * i) as u8, (17 * i) as u8, (7 * i) as u8],
                }],
            )
        })
        .collect();
    let painter = Painter::start(display.clone(), script);

    let (code, response, _stderr) = run_observation(&[
        "observe",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "700ms",
        "--stable-for",
        "400ms",
        "--json",
    ]);

    assert_eq!(code, 100, "a scene that never settles must time out");
    assert_eq!(response["observation"]["result"], "timeout");
    // Crucially, the caller can still tell that a change did happen.
    assert!(
        response["first_change"]["changed"].as_bool().unwrap(),
        "the detected change must be reported even though the operation timed out"
    );
    assert!(response["observation"]["change_detected_ms"]
        .as_u64()
        .is_some());

    drop(painter);
}

#[test]
fn observe_completes_when_the_scene_returns_to_its_baseline() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    let baseline = [30u8, 30, 30];
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, baseline),
    );

    // A transient acknowledgement that reverts: the transition happens, then the
    // scene returns to exactly where it started. This is a real transition that
    // settled, so observe must complete.
    let painter = Painter::start(
        display.clone(),
        vec![
            (
                Duration::from_millis(120),
                vec![rect(50, 50, [255, 255, 255])],
            ),
            (Duration::from_millis(250), vec![rect(50, 50, baseline)]),
        ],
    );

    let (code, response, stderr) = run_observation(&[
        "observe",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "10s",
        "--stable-for",
        "300ms",
        "--json",
    ]);

    assert_eq!(
        code, 0,
        "settling back onto the baseline is a successful observation, stderr: {stderr}"
    );
    assert_eq!(response["observation"]["result"], "observed");
    assert!(response["first_change"]["changed"].as_bool().unwrap());

    drop(painter);
}

// ============================================================================
// output behaviour
// ============================================================================

#[test]
fn observation_comparison_runs_at_native_resolution_not_the_output_size() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [255, 255, 255]),
    );

    let painter = Painter::start(
        display.clone(),
        vec![(Duration::from_millis(120), vec![rect(100, 80, [255, 0, 0])])],
    );

    // Ask for a downscaled JPEG output. The comparison must still report the
    // native pixel counts, because change detection must not depend on the
    // output format.
    let (code, response, stderr) = run_observation(&[
        "wait-change",
        "--display",
        &display,
        "--width",
        "200",
        "--format",
        "jpeg",
        "--quality",
        "75",
        "--base64",
        "--interval",
        "50ms",
        "--timeout",
        "5s",
        "--json",
    ]);

    assert_eq!(code, 0, "stderr: {stderr}");

    // The comparison is at native resolution.
    assert_eq!(response["comparison"]["total_pixels"], 120_000);
    assert_eq!(response["comparison"]["changed_pixels"], 6000);
    assert_eq!(response["comparison"]["bounding_box"]["width"], 100);

    // But the returned image is the requested size, with a correct transform.
    assert_eq!(response["image"]["width"], 200);
    assert_eq!(response["image"]["height"], 150);
    assert_eq!(response["image"]["media_type"], "image/jpeg");
    assert_eq!(response["image"]["quality"], 75);
    assert_eq!(response["image"]["encoding"], "base64");
    assert_eq!(response["transform"]["scale_x"], 2.0);
    assert_eq!(response["transform"]["scale_y"], 2.0);

    // The base64 payload decodes to a real JPEG of the declared size.
    let data = response["image"]["data"].as_str().unwrap();
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data).unwrap();
    assert_eq!(&bytes[..2], &[0xff, 0xd8]);
    let decoded = decode_jpeg(&bytes);
    assert_eq!((decoded.width, decoded.height), (200, 150));

    drop(painter);
}

#[test]
fn observation_reports_stage_timings_separately() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [0, 0, 0]),
    );

    let (code, response, _stderr) = run_observation(&[
        "wait-change",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "300ms",
        "--json",
    ]);
    assert_eq!(code, 100);

    let timing = &response["timing"];
    // The three questions these numbers answer: is capture slow, is comparison
    // slow, or are we mostly sleeping?
    for field in [
        "captures",
        "comparisons",
        "capture_us_total",
        "compare_us_total",
        "sleep_us_total",
    ] {
        assert!(timing[field].is_u64(), "timing.{field} should be present");
    }
    assert!(timing["encode"]["encode_us"].is_u64());

    let captures = timing["captures"].as_u64().unwrap();
    let comparisons = timing["comparisons"].as_u64().unwrap();
    assert_eq!(
        comparisons,
        captures - 1,
        "comparisons should be one fewer than captures"
    );
    // Capture is expected to dominate on this Xvfb build; the point is that the
    // numbers make that visible rather than hidden.
    assert!(timing["capture_us_total"].as_u64().unwrap() > 0);
}

#[test]
fn observation_does_not_write_an_image_unless_the_binary_destination_wants_one() {
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [5, 5, 5]),
    );

    let path = temp_file("observe-file", "settled.png");
    let (code, _response, stderr) = run_observation(&[
        "observe",
        "--display",
        &display,
        "--interval",
        "50ms",
        "--timeout",
        "400ms",
        "--stable-for",
        "150ms",
        "--json",
        path.to_str().unwrap(),
    ]);

    // The scene never changes, so observe times out, but it still writes the
    // final frame to the requested path.
    assert_eq!(code, 100, "stderr: {stderr}");
    assert!(
        path.exists(),
        "the final frame should be written to the path"
    );
    let image = decode_png(&std::fs::read(&path).unwrap());
    assert_eq!((image.width, image.height), (SCREEN_W, SCREEN_H));
}

#[test]
fn an_unavailable_display_fails_with_a_structured_error() {
    let (code, error) = run_observation_error(&[
        "wait-change",
        "--display",
        ":54321",
        "--json",
        "--timeout",
        "1s",
    ]);

    assert_eq!(code, 3, "expected display_unavailable");
    assert_eq!(error["error"]["code"], "display_unavailable");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains(":54321"));
}

#[test]
fn an_invalid_duration_is_rejected_before_any_display_is_opened() {
    // No Xvfb needed: validation happens during argument resolution, so a bad
    // setting never reaches capture.
    let (code, error) = run_observation_error(&[
        "observe",
        "--display",
        ":54321",
        "--interval",
        "0ms",
        "--json",
    ]);
    assert_eq!(code, 15, "expected invalid_duration");
    assert_eq!(error["error"]["code"], "invalid_duration");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("interval"));
}

#[test]
fn observation_help_lists_the_temporal_options() {
    let output = Command::new(eensh_binary())
        .args(["observe", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for option in [
        "--interval",
        "--timeout",
        "--stable-for",
        "--pixel-threshold",
        "--area-threshold",
        "--json",
    ] {
        assert!(help.contains(option), "help is missing {option}");
    }
}

#[test]
fn the_three_observation_commands_are_all_present() {
    let output = Command::new(eensh_binary()).arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&output.stdout);
    for command in ["capture", "diff", "wait-change", "wait-stable", "observe"] {
        assert!(
            help.contains(command),
            "help is missing the {command} command"
        );
    }
}
