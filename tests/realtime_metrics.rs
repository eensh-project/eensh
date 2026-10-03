//! Phase 5 requirements 35, 36, and 61: real-time cost and memory.
//!
//! These tests measure rather than assert, for the same reason as the Phase 4 metrics:
//! there is no defensible universal threshold for a capture, an encode, or a frame age.
//! What they *do* assert is the structural property that holds on any machine — that the
//! sampling window contains capture and sleep but not encoding, that a stack is returned
//! whole regardless of what history can hold, and that skipping is governed by the
//! interval rather than by luck.
//!
//! Run with `--nocapture` to see the measurements:
//!
//! ```text
//! cargo test --offline --test realtime_metrics -- --nocapture --test-threads=1
//! ```

mod common;

use std::time::{Duration, Instant};

use common::*;
use eensh::client::{EenshClient, ImageSpec, SessionSpec};
use eensh::realtime::RealtimeOptions;

const SCREEN_W: u32 = 640;
const SCREEN_H: u32 = 480;

/// One timing sample, in milliseconds.
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

    fn total(samples: &[Sample]) -> f64 {
        samples.iter().map(|s| s.millis).sum()
    }
}

fn ms(value: f64) -> String {
    format!("{value:.2}ms")
}

fn us(value: u64) -> String {
    format!("{value}us")
}

/// The real-time body of a tagged response: `{"kind": "realtime", "realtime": {..}}`.
fn body(value: &serde_json::Value) -> &serde_json::Value {
    &value["realtime"]
}

/// The summary section, which is itself named `realtime` inside the body.
fn section(value: &serde_json::Value) -> &serde_json::Value {
    &body(value)["realtime"]
}

/// The timing block.
fn timing(value: &serde_json::Value) -> &serde_json::Value {
    &body(value)["timing"]
}

/// The frame stack.
fn frames(value: &serde_json::Value) -> &Vec<serde_json::Value> {
    body(value)["frames"].as_array().expect("a frame stack")
}

/// Run `session realtime` and parse the tagged response.
#[allow(clippy::type_complexity)]
fn realtime_json(service: &ServiceProcess, args: &[&str]) -> (i32, serde_json::Value, String) {
    let mut full = vec!["realtime"];
    full.extend_from_slice(args);
    full.push("--json");
    let (code, stdout, stderr) = service.run(&full);

    let parsed = serde_json::from_str(stdout.trim())
        .or_else(|_| serde_json::from_str(stderr.trim()))
        .ok();
    match parsed {
        Some(value) => (code, value, stderr),
        None => (
            code,
            serde_json::Value::Null,
            format!("neither stream held JSON; stdout: {stdout:?} stderr: {stderr:?}"),
        ),
    }
}

/// A painter that never paints, holding a connection open so the root window persists.
fn keep_alive(display: &str) -> Screen {
    let screen = Screen::open(display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [40, 40, 40]),
    );
    screen
}

// ============================================================================
// 35: the four ways to ask for a frame
// ============================================================================

#[test]
fn the_four_paths_to_a_frame_are_measured_side_by_side() {
    // Requirement 35 asks for a like-for-like comparison of the four ways a caller can
    // obtain a frame: a standalone process per capture, the session CLI, a direct client
    // over one connection, and a direct-client real-time stack.
    //
    // The CLI paths pay for a process per operation and the direct paths do not, so the
    // comparison is not between equals in *architecture*. It is between equals in
    // *outcome*: each one ends with a frame in hand. Reporting them together is what
    // makes the cost of the process-per-operation habit visible.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-benchmark", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let _keep = keep_alive(&display);

    const N: usize = 12;

    // --- 1. Standalone: a fresh process and connection per capture ---
    let mut standalone = Vec::with_capacity(N);
    for _ in 0..N {
        let started = Instant::now();
        // The same presentation as the direct paths below: PNG at the same width, no
        // inline payload. Comparing across a format change would measure the codec
        // rather than the architecture.
        let (code, _, stderr) = run_eensh_text(&[
            "capture",
            "--display",
            &display,
            "--json",
            "--width",
            "160",
            "--format",
            "png",
        ]);
        assert_eq!(code, 0, "a standalone capture failed: {stderr}");
        standalone.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });
    }

    // --- 2. Session CLI: one session, a process per capture ---
    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0);
    let session_id = value["session_id"].as_str().unwrap().to_string();

    let mut session_cli = Vec::with_capacity(N);
    for _ in 0..N {
        let started = Instant::now();
        let (code, _, stderr) = service.run(&[
            "capture",
            &session_id,
            "--json",
            "--width",
            "160",
            "--format",
            "png",
        ]);
        assert_eq!(code, 0, "a session capture failed: {stderr}");
        session_cli.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });
    }

    // --- 3. Direct client: one connection, no process per capture ---
    let mut client = EenshClient::connect(service.socket()).expect("the client should connect");
    let direct_session = client
        .create_session(&display, &SessionSpec::desktop())
        .expect("a session should be creatable over the client");
    let direct_id = direct_session.id().to_string();

    // One untimed call first, so no lazy one-off costs land in the measurement.
    let presentation = ImageSpec {
        width: Some(160),
        base64: true,
        ..ImageSpec::default()
    };
    client
        .capture(&direct_id, presentation)
        .expect("a warm-up capture should succeed");

    let mut direct = Vec::with_capacity(N);
    for _ in 0..N {
        let started = Instant::now();
        let frame = client
            .capture(&direct_id, presentation)
            .unwrap_or_else(|e| panic!("a direct capture failed: {}", e.message()));
        // The frame really is in hand, so this is a fair comparison.
        assert!(frame.image.byte_length > 0);
        direct.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });
    }

    // --- 4. Direct-client real-time: three fresh frames over one call ---
    let mut realtime = Vec::with_capacity(N);
    let options = RealtimeOptions::default();
    for _ in 0..N {
        let started = Instant::now();
        // A realtime stack presents every frame the same way, so the per-frame cost is
        // comparable with the single-capture paths above.
        let stack = client
            .realtime(&direct_id, &options, presentation)
            .unwrap_or_else(|e| panic!("a direct realtime failed: {}", e.message()));
        assert_eq!(
            stack.captured_frames(),
            options.frames,
            "the stack should be whole"
        );
        realtime.push(Sample {
            millis: started.elapsed().as_secs_f64() * 1000.0,
        });
    }

    println!();
    println!("== 35: {N} operations at {SCREEN_W}x{SCREEN_H} ==");
    let report = |label: &str, samples: &[Sample], per_frame: f64| {
        println!(
            "{label:<34} mean {:>9}  p50 {:>9}  p95 {:>9}  total {:>10}",
            ms(Sample::mean(samples)),
            ms(Sample::percentile(samples, 50.0)),
            ms(Sample::percentile(samples, 95.0)),
            ms(Sample::total(samples)),
        );
        println!(
            "{:<34} per frame: {}",
            "",
            ms(Sample::mean(samples) / per_frame)
        );
    };
    report("standalone CLI (1 frame)", &standalone, 1.0);
    report("session CLI (1 frame)", &session_cli, 1.0);
    report("direct client (1 frame)", &direct, 1.0);
    report("direct client realtime (3 frames)", &realtime, 3.0);
    println!();

    // The structural claims.
    //
    // The first is that a stack costs at least the interval it was asked for. This is
    // the single most important honest statement about real-time cost, and it is the
    // opposite of a throughput claim: the primitive is *not* a way to get three frames
    // more cheaply than three round trips. Its entire purpose is that the frames are
    // spaced, so waiting is the feature. Three separate captures can genuinely return
    // sooner; they will simply all show nearly the same moment.
    let realtime_mean = Sample::mean(&realtime);
    let minimum_window = (options.frames - 1) as f64 * options.interval.as_secs_f64() * 1000.0;
    assert!(
        realtime_mean >= minimum_window,
        "a stack of {} frames at {}ms cannot complete faster than the schedule it asked \
         for ({minimum_window}ms): {realtime_mean}ms",
        options.frames,
        options.interval.as_millis()
    );

    // The second is that a process per operation is what the direct paths avoid. With
    // presentation held identical, a direct capture over an existing connection must not
    // be slower than the same capture driven through a fresh process.
    let direct_mean = Sample::mean(&direct);
    let session_cli_mean = Sample::mean(&session_cli);
    let standalone_mean = Sample::mean(&standalone);
    assert!(
        direct_mean <= session_cli_mean,
        "a direct capture should beat a process per capture: direct {} vs session CLI {}",
        ms(direct_mean),
        ms(session_cli_mean)
    );
    assert!(
        session_cli_mean <= standalone_mean,
        "a session should beat a fresh process and connection: session CLI {} vs standalone {}",
        ms(session_cli_mean),
        ms(standalone_mean)
    );

    // The third is about the shape of the real-time cost. The total is bounded below by
    // the schedule, which the assertion above already established, and bounded above by
    // that same schedule plus what it costs to capture and present the frames. The point
    // of stating both is that the wait is the dominant term: a stack is mostly waiting,
    // not working, and a machine roughly twice as slow would still not change its cost.
    let realtime_mean = Sample::mean(&realtime);
    let presentation_allowance = options.frames as f64 * direct_mean * 2.0;
    assert!(
        realtime_mean < minimum_window + presentation_allowance,
        "a stack should cost about its schedule plus its captures, not the interval per \
         frame: {realtime_mean}ms vs {minimum_window}ms schedule + {presentation_allowance}ms \
         of captures"
    );

    // The per-frame average is *below* the interval when more than two frames are taken,
    // because the first sample is immediate. Reporting it here is what makes the cost
    // shape legible; asserting it would only restate the arithmetic.
    println!(
        "schedule floor: {:.2}ms; measured total {:.2}ms; per frame {:.2}ms against a \
         {:.0}ms cadence",
        minimum_window,
        realtime_mean,
        realtime_mean / options.frames as f64,
        options.interval.as_secs_f64() * 1000.0
    );

    // And every path really produced a frame, so the comparison above is between like
    // outcomes even where it is not between like architectures.
    assert_eq!(standalone.len(), N);
    assert_eq!(session_cli.len(), N);
    assert_eq!(direct.len(), N);
    assert_eq!(realtime.len(), N);

    client
        .close_session(&direct_id)
        .expect("the session should close");
}

// ============================================================================
// 36: sampling measurements
// ============================================================================

#[test]
fn sampling_timing_is_measured_for_two_cadences() {
    // Requirement 36: report offsets, capture durations, the sampling window, encode
    // duration, skip counts, and the newest frame's age, for more than one cadence.
    //
    // Two configurations are measured because they probe different regimes. Three frames
    // at 50 ms is the default and should routinely complete. Four frames at 25 ms is a
    // tighter cadence, where a slow capture is more likely to push an opportunity out and
    // produce a skip — which is exactly the behaviour worth reporting rather than hiding.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-sampling", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let _keep = keep_alive(&display);
    let mut client = EenshClient::connect(service.socket()).expect("the client should connect");
    let session = client
        .create_session(&display, &SessionSpec::desktop())
        .expect("a session should be creatable");
    let id = session.id().to_string();

    let configurations = [
        ("default", RealtimeOptions::default()),
        (
            "tight",
            RealtimeOptions {
                frames: 4,
                interval: Duration::from_millis(25),
                timeout: Duration::from_millis(500),
            },
        ),
    ];

    println!();
    println!("== 36: real-time sampling ==");

    for (label, options) in configurations {
        const N: usize = 8;
        let mut windows = Vec::with_capacity(N);
        let mut encode_totals = Vec::with_capacity(N);
        let mut newest_ages = Vec::with_capacity(N);
        let mut skip_counts = Vec::new();
        let mut last_offsets = Vec::new();
        let mut last_durations = Vec::new();
        let mut complete = 0usize;

        for _ in 0..N {
            let started = Instant::now();
            let stack = client
                .realtime(&id, &options, ImageSpec::metadata_only())
                .unwrap_or_else(|e| panic!("realtime failed: {}", e.message()));
            windows.push(Sample {
                millis: started.elapsed().as_secs_f64() * 1000.0,
            });

            if stack.is_complete() {
                complete += 1;
            }
            skip_counts.push(stack.skipped_opportunities());
            newest_ages.push(Sample {
                millis: stack.newest_frame_age_us() as f64 / 1000.0,
            });

            let body = stack.frames();
            last_offsets = body
                .iter()
                .map(|frame| frame.capture_offset_us)
                .collect::<Vec<_>>();
            last_durations = body
                .iter()
                .map(|frame| frame.capture_duration_us)
                .collect::<Vec<_>>();

            // The encode totals are only visible on the wire, so ask for them the
            // honest way: through the CLI, once, after the timing loop.
        }

        // One CLI call to read the timing block, which the typed client exposes through
        // the response but which is not part of the ergonomic surface.
        let (code, value, stderr) = realtime_json(
            &service,
            &[
                &id,
                "--frames",
                &options.frames.to_string(),
                "--interval",
                &format!("{}ms", options.interval.as_millis()),
                "--timeout",
                &format!("{}ms", options.timeout.as_millis()),
            ],
        );
        assert_eq!(code, 0, "the timing probe failed: {stderr} {value}");
        let timing = timing(&value);
        encode_totals.push(Sample {
            millis: timing["encode_us_total"].as_u64().unwrap() as f64 / 1000.0,
        });

        println!();
        println!(
            "-- {label}: frames={} interval={}ms timeout={}ms --",
            options.frames,
            options.interval.as_millis(),
            options.timeout.as_millis()
        );
        println!(
            "round trip:      mean {:>8}  p50 {:>8}  p95 {:>8}",
            ms(Sample::mean(&windows)),
            ms(Sample::percentile(&windows, 50.0)),
            ms(Sample::percentile(&windows, 95.0))
        );
        println!(
            "complete:        {complete}/{N} ({:.0}%)",
            complete as f64 / N as f64 * 100.0
        );
        println!(
            "skipped slots:   min {}  max {}",
            skip_counts.iter().min().unwrap(),
            skip_counts.iter().max().unwrap()
        );
        println!(
            "newest age:      mean {:>8}  p95 {:>8}",
            ms(Sample::mean(&newest_ages)),
            ms(Sample::percentile(&newest_ages, 95.0))
        );
        println!(
            "last offsets:    {}",
            last_offsets
                .iter()
                .map(|value| us(*value))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "last durations:  {}",
            last_durations
                .iter()
                .map(|value| us(*value))
                .collect::<Vec<_>>()
                .join(", ")
        );
        // The timing probe's own cadence differs from the loop's, so it is reported as a
        // representative encode cost rather than folded into the averages above.
        println!(
            "encode (probe):  {} for {} frames",
            ms(encode_totals[0].millis),
            options.frames
        );

        // Structural claims, independent of the machine.
        assert_eq!(
            last_offsets.len(),
            options.frames.max(1),
            "the stack should report one offset per sampled frame"
        );
        assert!(
            last_offsets.windows(2).all(|pair| pair[0] <= pair[1]),
            "offsets are oldest first: {last_offsets:?}"
        );
        assert!(
            Sample::mean(&newest_ages) >= 0.0,
            "a newest frame always has an age"
        );
    }
    println!();

    client.close_session(&id).expect("the session should close");
}

#[test]
fn the_timing_block_separates_sampling_from_presentation() {
    // Requirement 36 and 19 together: the numbers that prove encoding happens outside the
    // sampling window have to actually be reported, and they have to be consistent.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-timing", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let _keep = keep_alive(&display);
    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0);
    let id = value["session_id"].as_str().unwrap().to_string();

    let (code, value, stderr) = realtime_json(
        &service,
        &[
            &id,
            "--frames",
            "3",
            "--interval",
            "100ms",
            "--timeout",
            "1s",
            "--base64",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");

    let timing = timing(&value);
    let sampling = timing["sampling_us"].as_u64().unwrap();
    let capture = timing["capture_us_total"].as_u64().unwrap();
    let sleep = timing["sleep_us_total"].as_u64().unwrap();
    let encode = timing["encode_us_total"].as_u64().unwrap();
    let base64 = timing["base64_us_total"].as_u64().unwrap();

    println!();
    println!("== timing decomposition ==");
    println!("sampling window:  {}", us(sampling));
    println!("  capture:        {}", us(capture));
    println!("  sleep:          {}", us(sleep));
    println!("  unaccounted:    {}", us(sampling - capture - sleep));
    println!("presentation:");
    println!("  encode:         {}", us(encode));
    println!("  base64:         {}", us(base64));
    println!();

    // The sampling window is entirely capture and sleep, so encoding is not inside it.
    assert!(
        capture + sleep <= sampling,
        "capture plus sleep cannot exceed the window: capture {capture}, sleep {sleep}, \
         window {sampling}"
    );
    assert!(
        sampling - capture - sleep < 5_000,
        "the window should be accounted for without room for an encode: \
         unaccounted {}us",
        sampling - capture - sleep
    );

    // The window is at least the interval between the first and last scheduled slots,
    // which is what bounds it from below.
    assert!(
        sampling >= 200_000,
        "three frames at 100ms cannot have completed in less than two intervals: {sampling}us"
    );

    // Presentation work is real and is reported, not absorbed.
    assert!(encode > 0, "three PNG encoding passes cost something");
    assert!(
        encode + base64 > 0,
        "presentation is measured separately from sampling"
    );
}

// ============================================================================
// 61: memory for active temporal stacks
// ============================================================================

#[test]
fn an_active_stack_s_retained_bytes_are_reported_and_bounded() {
    // Requirement 61: report memory for an active temporal stack. The structural claim is
    // that retained bytes track the frames the session holds rather than the frames the
    // stack returned, because the operation's own frames are released when it finishes.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-memory", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let _keep = keep_alive(&display);

    let frame_bytes = (SCREEN_W * SCREEN_H * 3) as u64;

    let (code, value) =
        service.run_json(&["create", "--display", &display, "--history", "4", "--json"]);
    assert_eq!(code, 0);
    let id = value["session_id"].as_str().unwrap().to_string();

    let baseline = service.resident_bytes();

    let (code, info) = service.run_json(&["info", &id, "--json"]);
    assert_eq!(code, 0);
    assert_eq!(info["info"]["history"]["retained_bytes"], 0);
    assert_eq!(info["info"]["history"]["retained"], 0);

    // An eight-frame stack against a history of four: the operation holds eight, the
    // session keeps four, and the difference must be released.
    const STACK: usize = 8;
    let (code, value, stderr) = realtime_json(
        &service,
        &[
            &id,
            "--frames",
            &STACK.to_string(),
            "--interval",
            "40ms",
            "--timeout",
            "2s",
            "--width",
            "160",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");
    assert_eq!(
        section(&value)["captured_frames"],
        STACK,
        "the whole stack should be returned: {value}"
    );

    // The returned images were resized, but the *stored* frames are native.
    let (code, info) = service.run_json(&["info", &id, "--json"]);
    assert_eq!(code, 0);
    let retained = info["info"]["history"]["retained"].as_u64().unwrap();
    let retained_bytes = info["info"]["history"]["retained_bytes"].as_u64().unwrap();

    println!();
    println!("== 61: memory for an active temporal stack ==");
    println!("screen:                 {SCREEN_W}x{SCREEN_H}");
    println!("one native frame:       {} bytes", frame_bytes);
    println!("stack requested:        {STACK} frames");
    println!(
        "history capacity:       {}",
        info["info"]["history"]["capacity"]
    );
    println!("history retained:       {retained} frames");
    println!("history retained bytes: {retained_bytes}");
    println!(
        "frames captured total:  {}",
        info["info"]["frames_captured"]
    );
    match (baseline, service.resident_bytes()) {
        (Some(before), Some(after)) => {
            println!(
                "service RSS:            {before} -> {after} bytes ({:+} bytes)",
                after as i64 - before as i64
            );
        }
        _ => println!("service RSS:            unavailable on this system"),
    }
    println!();

    // The structural claims: history is bounded by its capacity, not by the size of the
    // stack that just ran, and retained bytes are consistent with that count.
    assert_eq!(
        retained,
        info["info"]["history"]["capacity"].as_u64().unwrap(),
        "history should be full but not overfull: {info}"
    );
    assert!(
        retained < STACK as u64,
        "history must not be inflated to hold the stack: retained {retained} of {STACK}"
    );
    assert_eq!(
        retained_bytes,
        retained * frame_bytes,
        "retained bytes should match the retained frames at native size: {info}"
    );

    // The stack's frames beyond history were released, not leaked: the service is not
    // still holding eight screens' worth after the operation finished.
    assert!(
        retained_bytes < STACK as u64 * frame_bytes,
        "the operation's own frames should have been released: {retained_bytes} bytes \
         for a {STACK}-frame stack"
    );
}

#[test]
fn concurrent_stacks_each_own_their_frames_independently() {
    // Requirement 61 in the concurrent case: two sessions sampling at once hold their own
    // frames, so the memory is two stacks' worth and neither corrupts the other.
    let _guard = serial();
    let Some((_server, display, service)) =
        xvfb_service("realtime-memory-pair", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let _keep = keep_alive(&display);

    let mut ids = Vec::new();
    for _ in 0..2 {
        let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
        assert_eq!(code, 0);
        ids.push(value["session_id"].as_str().unwrap().to_string());
    }

    // Sample both sessions at once, each over its own connection.
    let socket_a = service.socket_arg();
    let session_a = ids[0].clone();
    let a = std::thread::spawn(move || {
        run_eensh_text(&[
            "session",
            "realtime",
            &session_a,
            "--socket",
            &socket_a,
            "--json",
            "--frames",
            "6",
            "--interval",
            "40ms",
            "--timeout",
            "2s",
        ])
    });

    let socket_b = service.socket_arg();
    let session_b = ids[1].clone();
    let b = std::thread::spawn(move || {
        run_eensh_text(&[
            "session",
            "realtime",
            &session_b,
            "--socket",
            &socket_b,
            "--json",
            "--frames",
            "6",
            "--interval",
            "40ms",
            "--timeout",
            "2s",
        ])
    });

    let (code_a, stdout_a, stderr_a) = a.join().expect("thread A");
    let (code_b, stdout_b, stderr_b) = b.join().expect("thread B");
    assert_eq!(code_a, 0, "session A failed: {stderr_a}");
    assert_eq!(code_b, 0, "session B failed: {stderr_b}");

    let value_a: serde_json::Value = serde_json::from_str(stdout_a.trim()).unwrap();
    let value_b: serde_json::Value = serde_json::from_str(stdout_b.trim()).unwrap();

    let ids_a: Vec<u64> = frames(&value_a)
        .iter()
        .map(|frame| frame["frame_id"].as_u64().unwrap())
        .collect();
    let ids_b: Vec<u64> = frames(&value_b)
        .iter()
        .map(|frame| frame["frame_id"].as_u64().unwrap())
        .collect();

    println!();
    println!("== 61: two concurrent six-frame stacks ==");
    println!("session A frames: {ids_a:?}");
    println!("session B frames: {ids_b:?}");
    for id in &ids {
        let (_, info) = service.run_json(&["info", id, "--json"]);
        println!(
            "{id}: captured={} retained={} bytes={}",
            info["info"]["frames_captured"],
            info["info"]["history"]["retained"],
            info["info"]["history"]["retained_bytes"]
        );
    }
    match service.resident_bytes() {
        Some(bytes) => println!("service RSS:            {bytes} bytes"),
        None => println!("service RSS:            unavailable on this system"),
    }
    println!();

    // Each session's identities are its own and sequential, which shows the stacks did
    // not share state.
    assert_eq!(ids_a.len(), 6);
    assert_eq!(ids_b.len(), 6);
    assert_eq!(ids_a[0], 1, "session A starts its own numbering");
    assert_eq!(ids_b[0], 1, "session B starts its own numbering");

    // Both sessions are usable again, so neither stack stranded the other.
    for id in &ids {
        let (code, info) = service.run_json(&["info", id, "--json"]);
        assert_eq!(code, 0);
        assert_eq!(
            info["info"]["state"], "ready",
            "{id} should be ready: {info}"
        );
        assert_eq!(info["info"]["frames_captured"], 6);
    }
}

// ============================================================================
// 36: skipping is governed by the interval
// ============================================================================

#[test]
fn a_cadence_faster_than_capture_degrades_into_skips_rather_than_slow_capture() {
    // Requirement 33: when the cadence is faster than capture, the operation must skip
    // opportunities rather than let each capture drag the schedule. The observable
    // difference is that the *last* offset stays close to the nominal end of the schedule
    // — because skipped slots are not replayed, the stack finishes on time even though it
    // could not fill.
    //
    // A one-millisecond interval is certainly faster than a full-screen capture, so the
    // middle samples must be skipped.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-skipping", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let _keep = keep_alive(&display);

    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0);
    let id = value["session_id"].as_str().unwrap().to_string();

    let (code, value, stderr) = realtime_json(
        &service,
        &[&id, "--frames", "5", "--interval", "1ms", "--timeout", "2s"],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");

    let section = section(&value);
    let skipped = section["skipped_opportunities"].as_u64().unwrap();
    let scheduled = section["scheduled_opportunities"].as_u64().unwrap();
    let captured = section["captured_frames"].as_u64().unwrap();
    let elapsed = section["elapsed_ms"].as_u64().unwrap();

    println!();
    println!("== 36: skipping under an impossible cadence ==");
    println!("requested:  5 frames at 1ms");
    println!("scheduled:  {scheduled} opportunities");
    println!("captured:   {captured} frames");
    println!("skipped:    {skipped} opportunities");
    println!("elapsed:    {elapsed}ms");
    println!(
        "offsets:    {:?}",
        frames(&value)
            .iter()
            .map(|f| f["capture_offset_us"].as_u64().unwrap())
            .collect::<Vec<_>>()
    );
    println!();

    // The whole point: opportunities were skipped rather than replayed, so the elapsed
    // time is governed by the cadence and not by five sequential full-screen captures
    // stretched out with their own intervals.
    assert!(
        skipped > 0,
        "a 1ms cadence on full-screen captures must skip opportunities: {value}"
    );

    // Every requested frame was still captured, because skipping a *slot* does not
    // discard a *frame*: the next opportunity is taken instead.
    assert_eq!(
        captured, 5,
        "skipping slots should not cost frames, since the deadline was generous: {value}"
    );

    // And the elapsed window stays near the nominal schedule, which is what "no replay"
    // means in practice: the final slot is at four milliseconds, not four captures later.
    assert!(
        elapsed < 1_000,
        "the schedule should not be stretched by the missing slots: {elapsed}ms"
    );
}
