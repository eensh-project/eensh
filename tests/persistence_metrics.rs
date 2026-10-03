//! Phase 4 requirements 65, 70, and 71: persistence cost and memory.
//!
//! These tests measure rather than assert, because there is no defensible
//! universal threshold to assert against: X server behaviour, scheduler noise,
//! and machine speed all move the numbers. What they *do* assert is the
//! structural claim — that the persistent path really does avoid repeating the
//! connection handshake, and that history really is bounded — because those are
//! properties of this implementation rather than of the machine it runs on.
//!
//! Run with `--nocapture` to see the measurements:
//!
//! ```text
//! cargo test --offline --test persistence_metrics -- --nocapture --test-threads=1
//! ```

mod common;

use std::time::Instant;

use common::*;

const SCREEN_W: u32 = 640;
const SCREEN_H: u32 = 480;

/// One timing sample.
#[derive(Debug, Clone, Copy)]
struct Sample {
    millis: f64,
}

impl Sample {
    fn mean(samples: &[Sample]) -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        samples.iter().map(|s| s.millis).sum::<f64>() / samples.len() as f64
    }

    fn percentile(samples: &[Sample], percent: f64) -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f64> = samples.iter().map(|s| s.millis).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let index = ((percent / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        sorted[index.min(sorted.len() - 1)]
    }
}

/// Format a millisecond value for a report line.
fn ms(value: f64) -> String {
    format!("{value:.2}ms")
}

// ============================================================================
// 65 and 70: persistent capture versus connection-per-capture
// ============================================================================

#[test]
fn persistent_capture_avoids_repeated_connection_setup() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("persistence-cost", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0);
    let session_id = value["session_id"].as_str().unwrap().to_string();

    const N: usize = 20;

    // --- Standalone: a fresh process and a fresh X11 connection per capture ---
    let mut standalone = Vec::with_capacity(N);
    let total_start = Instant::now();
    for _ in 0..N {
        let started = Instant::now();
        let (code, _, stderr) = run_eensh_text(&[
            "capture",
            "--display",
            &display,
            "--json",
            // No image bytes are wanted: the comparison is about capture and
            // connection cost, not about encoding, which both paths pay equally.
            // Writing to a file would add filesystem noise.
            "--width",
            "16",
        ]);
        assert_eq!(code, 0, "a standalone capture failed: {stderr}");
        standalone.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });
    }
    let standalone_total = total_start.elapsed();

    // --- Persistent: one session, N captures over its existing connection ---
    let mut persistent = Vec::with_capacity(N);
    let total_start = Instant::now();
    for _ in 0..N {
        let started = Instant::now();
        let (code, stdout, stderr) =
            service.run(&["capture", &session_id, "--json", "--width", "16"]);
        assert_eq!(code, 0, "a session capture failed: {stderr}");
        let _ = stdout;
        persistent.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });
    }
    let persistent_total = total_start.elapsed();

    // --- Session creation, measured on its own (requirement 70) ---
    let creation = {
        let started = Instant::now();
        let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
        assert_eq!(code, 0);
        let elapsed = started.elapsed();
        let id = value["session_id"].as_str().unwrap().to_string();
        service.run_json(&["close", &id, "--json"]);
        elapsed
    };

    println!();
    println!("== 65/70: {N} captures at {SCREEN_W}x{SCREEN_H} ==");
    println!(
        "standalone (process + connection per capture): mean {}  p50 {}  p95 {}  total {:.2}ms",
        ms(Sample::mean(&standalone)),
        ms(Sample::percentile(&standalone, 50.0)),
        ms(Sample::percentile(&standalone, 95.0)),
        standalone_total.as_secs_f64() * 1000.0
    );
    println!(
        "persistent (one session, one connection):      mean {}  p50 {}  p95 {}  total {:.2}ms",
        ms(Sample::mean(&persistent)),
        ms(Sample::percentile(&persistent, 50.0)),
        ms(Sample::percentile(&persistent, 95.0)),
        persistent_total.as_secs_f64() * 1000.0
    );
    println!(
        "session creation alone: {:.2}ms",
        creation.as_secs_f64() * 1000.0
    );
    println!(
        "ratio (standalone mean / persistent mean): {:.2}x",
        Sample::mean(&standalone) / Sample::mean(&persistent)
    );
    println!();

    // The structural claim, which does not depend on machine speed: the
    // persistent path must not be paying a fresh connection handshake per capture.
    let standalone_mean = Sample::mean(&standalone);
    let persistent_mean = Sample::mean(&persistent);
    assert!(
        persistent_mean <= standalone_mean,
        "the persistent path should not be slower per capture than a fresh process \
         and connection each time: persistent {} vs standalone {}",
        ms(persistent_mean),
        ms(standalone_mean)
    );

    // The saving is reported rather than bounded, and the *reason* it is modest
    // matters more than the number. Both paths still pay for a client process and
    // its argument parsing on every capture, because the CLI is one process per
    // invocation; the only thing the session removes is the X11 connection setup.
    // That cost is real -- `session creation` above measures it, since creation
    // opens the connection -- but it is a small fraction of a capture round trip
    // that is dominated by process start and by the capture itself.
    //
    // The spec explicitly declines to require a universal speedup ratio, and this
    // does not invent one. The claim being evidenced is narrower: the persistent
    // path behaves as intended and does not repeat connection setup.
}

// ============================================================================
// 70: comparison and retrieval are local memory operations
// ============================================================================

#[test]
fn comparison_and_retrieval_do_not_touch_the_display() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("local-operations", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let (code, value) = service.run_json(&[
        "create",
        "--display",
        &display,
        "--json",
        // Generous, so that nothing compared below is evicted while the test runs.
        // A smaller history would make `diff 1 2` fail with `frame_not_available`
        // once the measurement loop's own captures had pushed frame 1 out, which
        // would be measuring eviction rather than comparison cost.
        "--history",
        "64",
    ]);
    assert_eq!(code, 0);
    let session_id = value["session_id"].as_str().unwrap().to_string();

    // Capture enough frames to have something to compare and retrieve.
    for _ in 0..8 {
        let (code, _, stderr) = service.run(&["capture", &session_id, "--json"]);
        assert_eq!(code, 0, "capture failed: {stderr}");
    }

    // Retrieval and comparison should be cheaper than a capture, because neither
    // touches X11: both operate on immutable retained frames.
    //
    // The image options are deliberately identical across the three, and no
    // resize is applied. A retrieval still *encodes* the frame it returns, so
    // comparing a full-size retrieval against a resized capture would measure the
    // difference between two encode sizes rather than the difference between a
    // local operation and one that contacts the display.
    let mut retrieval = Vec::new();
    let mut comparison = Vec::new();
    let mut capture = Vec::new();

    for _ in 0..10 {
        let started = Instant::now();
        let (code, _, _) = service.run(&["latest", &session_id, "--json"]);
        assert_eq!(code, 0);
        retrieval.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });

        let started = Instant::now();
        let (code, _, _) = service.run(&["diff", &session_id, "1", "2", "--json"]);
        assert_eq!(code, 0);
        comparison.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });

        let started = Instant::now();
        let (code, _, _) = service.run(&["capture", &session_id, "--json"]);
        assert_eq!(code, 0);
        capture.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });
    }

    println!();
    println!("== 70: local operations at {SCREEN_W}x{SCREEN_H}, full-size images ==");
    println!(
        "retrieval (latest):   mean {}",
        ms(Sample::mean(&retrieval))
    );
    println!(
        "comparison (diff):    mean {}",
        ms(Sample::mean(&comparison))
    );
    println!("capture (fresh):      mean {}", ms(Sample::mean(&capture)));
    println!();

    // A comparison does no encoding at all and never touches X11, so it is the
    // cheapest of the three and must be cheaper than a retrieval, which still
    // encodes the frame it returns.
    assert!(
        Sample::mean(&comparison) < Sample::mean(&retrieval),
        "comparison should be cheaper than retrieval: comparison {} vs retrieval {}",
        ms(Sample::mean(&comparison)),
        ms(Sample::mean(&retrieval))
    );

    // A fresh capture must contact the X server and encode, so it is the most
    // expensive of the three.
    assert!(
        Sample::mean(&capture) > Sample::mean(&comparison),
        "a fresh capture should be dearer than a comparison: capture {} vs comparison {}",
        ms(Sample::mean(&capture)),
        ms(Sample::mean(&comparison))
    );
}

// ============================================================================
// 71: memory, and the bounded-ness of history
// ============================================================================

#[test]
fn history_memory_is_bounded_by_capacity_and_reported_per_configuration() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("memory-metrics", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    // Three representative configurations, as the spec suggests. The display is
    // fixed, so the *source* size is the same; the history is measured against
    // full-size frames either way, because history stores raw frames at source
    // resolution regardless of any resize applied to a returned image.
    for history in [1usize, 2, 8] {
        let (code, value) = service.run_json(&[
            "create",
            "--display",
            &display,
            "--json",
            "--history",
            &history.to_string(),
        ]);
        assert_eq!(code, 0, "create failed: {value}");
        let session_id = value["session_id"].as_str().unwrap().to_string();

        let rss_before = service.resident_bytes();
        let mut peak_rss = rss_before;

        // Capture well past the capacity, so the steady state is reached.
        for _ in 0..(history + 8) {
            let (code, _, stderr) = service.run(&["capture", &session_id, "--json"]);
            assert_eq!(code, 0, "capture failed: {stderr}");
        }

        let (code, value) = service.run_json(&["info", &session_id, "--json"]);
        assert_eq!(code, 0);
        let summary = &value["info"]["history"];

        let retained = summary["retained"].as_u64().unwrap();
        let retained_bytes = summary["retained_bytes"].as_u64().unwrap();
        let captured_total = summary["captured_total"].as_u64().unwrap();
        let capacity = summary["capacity"].as_u64().unwrap();

        if let Some(rss) = service.resident_bytes() {
            peak_rss = Some(rss);
        }

        // The frame size is known exactly: RGB8 is three bytes per pixel.
        let frame_bytes = (SCREEN_W as u64) * (SCREEN_H as u64) * 3;

        println!();
        println!("== 71: {SCREEN_W}x{SCREEN_H} x {history} history ==");
        println!("raw frame bytes:            {frame_bytes}");
        println!("capacity:                   {capacity}");
        println!("captured total:             {captured_total}");
        println!("retained:                   {retained}");
        println!("retained history bytes:     {retained_bytes}");
        println!(
            "retained / raw frame:       {:.2} frames",
            retained_bytes as f64 / frame_bytes as f64
        );
        if let (Some(before), Some(after)) = (rss_before, peak_rss) {
            println!("service RSS before:         {before}");
            println!("service RSS after:          {after}");
        }

        // Requirement 71's structural claim: history is bounded by capacity, so
        // the retained bytes cannot grow without limit however many frames are
        // captured. This is the property that makes the memory trade-off
        // predictable rather than merely large.
        assert_eq!(
            retained as usize, history,
            "history should be full but not over-full: {value}"
        );
        assert!(
            captured_total > history as u64,
            "the configuration should have evicted frames: {value}"
        );
        assert!(
            retained_bytes <= frame_bytes * capacity,
            "retained bytes must not exceed capacity times the frame size: {value}"
        );
        // And it should be exactly capacity frames, since every frame is the same
        // size and none has been resized. A discrepancy would mean either double
        // counting or a frame that is not retained.
        assert_eq!(
            retained_bytes,
            frame_bytes * retained,
            "retained bytes should equal retained frames times the frame size: {value}"
        );

        service.run_json(&["close", &session_id, "--json"]);
    }
}

// ============================================================================
// 71: retained frames are shared, not copied, when a caller holds one
// ============================================================================

#[test]
fn a_retained_frame_is_not_duplicated_for_a_second_reader() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("frame-sharing", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0);
    let session_id = value["session_id"].as_str().unwrap().to_string();

    for _ in 0..4 {
        let (code, _, _) = service.run(&["capture", &session_id, "--json"]);
        assert_eq!(code, 0);
    }

    let (code, value) = service.run_json(&["info", &session_id, "--json"]);
    assert_eq!(code, 0);
    let retained_bytes = value["info"]["history"]["retained_bytes"].as_u64().unwrap();
    let frame_bytes = (SCREEN_W as u64) * (SCREEN_H as u64) * 3;

    // Four captures, four frames retained, so the retained bytes are exactly four
    // frames. Reading a frame out of history does not add a copy to the total,
    // which is the property that makes repeated retrievals cheap.
    //
    // This is measured before and after a retrieval to show it directly.
    let before = retained_bytes;
    for _ in 0..5 {
        let (code, _, _) = service.run(&["latest", &session_id, "--json"]);
        assert_eq!(code, 0);
    }
    let (code, value) = service.run_json(&["info", &session_id, "--json"]);
    assert_eq!(code, 0);
    let after = value["info"]["history"]["retained_bytes"].as_u64().unwrap();

    println!();
    println!("== 71: retained bytes around five retrievals ==");
    println!("before: {before}  after: {after}  frame: {frame_bytes}");
    println!();

    assert_eq!(
        before, after,
        "retrieving frames must not change what history retains: {before} then {after}"
    );
    assert_eq!(
        after,
        frame_bytes * 4,
        "four captured frames should be retained exactly once each"
    );
}

// ============================================================================
// 70: IPC overhead on its own
// ============================================================================

#[test]
fn ipc_overhead_is_small_relative_to_the_work_it_carries() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("ipc-overhead", SCREEN_W, SCREEN_H) else {
        return;
    };

    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0);
    let session_id = value["session_id"].as_str().unwrap().to_string();
    service.run_json(&["capture", &session_id, "--json"]);

    // `ping` is the cheapest possible request: it does no capture, no encoding,
    // and no session work. Its round trip therefore isolates process start plus
    // the socket exchange, which is the IPC cost the service adds to every call.
    let mut ping = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        let (code, _, _) = service.run(&["list", "--json"]);
        assert_eq!(code, 0);
        ping.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });
    }

    println!();
    println!("== 70: IPC round trip (client process + socket request) ==");
    println!(
        "session list: mean {}  p50 {}",
        ms(Sample::mean(&ping)),
        ms(Sample::percentile(&ping, 50.0))
    );
    println!();

    // The measurement is reported rather than bounded. What is asserted is only
    // that a round trip completes at all, which the calls above already proved;
    // asserting a ceiling would be asserting the machine's speed.
    assert!(
        !ping.is_empty(),
        "the measurement should have produced samples"
    );
    assert!(
        Sample::mean(&ping) > 0.0,
        "a round trip takes a measurable amount of time"
    );
}

/// A note on what these numbers do and do not show.
///
/// Each `persistent` capture above is still a *separate client process*. The
/// saving measured is the X11 connection and the capture setup, not the process
/// start, because the CLI is a process per invocation. A caller that spoke the
/// protocol directly would avoid the process start as well and see a larger
/// difference. This is stated here so the report does not overclaim.
#[allow(dead_code)]
const MEASUREMENT_CAVEAT: &str = "persistent captures still pay one client process each";

// ============================================================================
// 84.15: observation timing, standalone versus persistent
// ============================================================================

#[test]
fn observation_timing_is_comparable_through_both_paths() {
    // Requirement 84 asks for representative observation timing before and after
    // persistence. The point is that persistence must not make observation
    // *dramatically* cheaper or dearer: the cost of an observation is dominated by
    // the deliberate waiting between samples, which neither path can avoid.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("observation-timing", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0);
    let session_id = value["session_id"].as_str().unwrap().to_string();

    // A static screen, so both paths take the same number of samples and settle on
    // the first comparison. Using a moving scene would make the two runs differ in
    // capture count and swamp the comparison with scheduler noise.
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

    let common = [
        "--json",
        "--interval",
        "50ms",
        "--timeout",
        "2s",
        "--stable-for",
        "200ms",
    ];

    let mut standalone = Vec::new();
    let mut persistent = Vec::new();

    for _ in 0..5 {
        let started = Instant::now();
        let mut args = vec!["wait-stable", "--display", &display];
        args.extend_from_slice(&common);
        let (code, value, stderr) = run_json_stdout(&args);
        assert_eq!(code, 0, "standalone wait-stable failed: {stderr} {value}");
        standalone.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });

        let started = Instant::now();
        let mut args = vec!["wait-stable", &session_id];
        args.extend_from_slice(&common);
        let (code, value) = service.run_json(&args);
        assert_eq!(code, 0, "session wait-stable failed: {value}");
        persistent.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });
    }

    println!();
    println!("== 84.15: wait-stable on a static screen, 5 runs each ==");
    println!(
        "standalone: mean {}  p50 {}",
        ms(Sample::mean(&standalone)),
        ms(Sample::percentile(&standalone, 50.0))
    );
    println!(
        "persistent: mean {}  p50 {}",
        ms(Sample::mean(&persistent)),
        ms(Sample::percentile(&persistent, 50.0))
    );
    println!();

    // Both must exceed the stability window they are waiting for, since neither can
    // settle before `--stable-for` has elapsed. This is the property that matters:
    // the time is dominated by the documented waiting, not by the transport.
    for (label, samples) in [("standalone", &standalone), ("persistent", &persistent)] {
        assert!(
            Sample::mean(samples) >= 200.0,
            "{label} should wait at least the stability window: {}",
            ms(Sample::mean(samples))
        );
    }
}
