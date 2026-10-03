//! Phase 5: real-time sampling against a live Xvfb display, and its backpressure.
//!
//! The synthetic scheduler tests in `src/realtime.rs` are authoritative for exact skip
//! counts, because live capture duration is not something a test can pin down. What
//! these tests establish is the other half: that the scheduler is wired to a real
//! session correctly — frames are physically captured, contents follow temporal order,
//! every sample enters history, and the concurrency rules hold.
//!
//! Requirement 58 warns against flaky assumptions about live capture duration, so no
//! assertion here depends on how long a capture took. They assert structure and ordering
//! instead.
//!
//! Observations run in a background thread while the test body holds `serial()`, so the
//! single-threaded X connection in the body is never contended.

mod common;

use std::time::Duration;

use common::*;

const SCREEN_W: u32 = 400;
const SCREEN_H: u32 = 300;

/// A background command whose completion can be harvested later.
struct Background {
    handle: Option<std::thread::JoinHandle<(i32, String, String)>>,
}

impl Background {
    fn start(args: Vec<String>) -> Background {
        let handle = std::thread::spawn(move || {
            let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
            run_eensh_text(&borrowed)
        });
        Background {
            handle: Some(handle),
        }
    }

    /// Wait for the command and return `(exit, stdout, stderr)`.
    fn wait(mut self) -> (i32, String, String) {
        self.handle
            .take()
            .expect("a handle")
            .join()
            .unwrap_or_else(|_| panic!("the background command panicked"))
    }
}

/// Start a long real-time observation against a session, in the background.
fn start_realtime(service: &ServiceProcess, session: &str) -> Background {
    Background::start(vec![
        "session".to_string(),
        "realtime".to_string(),
        session.to_string(),
        "--socket".to_string(),
        service.socket_arg(),
        "--json".to_string(),
        "--frames".to_string(),
        "8".to_string(),
        "--interval".to_string(),
        "200ms".to_string(),
        "--timeout".to_string(),
        "3s".to_string(),
    ])
}

/// Run `session realtime` and parse the JSON result.
///
/// Real-time output is a result rather than a diagnostic, and it is printed on stdout
/// like the other observation commands.
#[allow(clippy::type_complexity)]
fn realtime(service: &ServiceProcess, args: &[&str]) -> (i32, serde_json::Value, String) {
    let mut full = vec!["realtime"];
    full.extend_from_slice(args);
    full.push("--json");
    let (code, stdout, stderr) = service.run(&full);

    // Success is a result on stdout; failure is a diagnostic on stderr, as it is for
    // every command other than the observation ones.
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

/// Create a session and return its identifier.
fn create_session(service: &ServiceProcess, display: &str, extra: &[&str]) -> String {
    let mut args = vec!["create", "--display", display, "--json"];
    args.extend_from_slice(extra);
    let (code, value) = service.run_json(&args);
    assert_eq!(code, 0, "session creation failed: {value}");
    value["session_id"].as_str().unwrap().to_string()
}

/// The real-time body of a tagged response.
///
/// A response is `{"kind": "realtime", "realtime": { ... }}`, so every field of
/// interest lives one level down.
fn body(value: &serde_json::Value) -> &serde_json::Value {
    &value["realtime"]
}

/// The summary section, which is itself called `realtime` inside the body.
fn section(value: &serde_json::Value) -> &serde_json::Value {
    &body(value)["realtime"]
}

fn frames_of(value: &serde_json::Value) -> &Vec<serde_json::Value> {
    body(value)["frames"].as_array().expect("a frame stack")
}

fn frame_ids(value: &serde_json::Value) -> Vec<u64> {
    frames_of(value)
        .iter()
        .map(|frame| frame["frame_id"].as_u64().unwrap())
        .collect()
}

fn offsets_of(value: &serde_json::Value) -> Vec<u64> {
    frames_of(value)
        .iter()
        .map(|frame| frame["capture_offset_us"].as_u64().unwrap())
        .collect()
}

/// Paint the whole screen a flat colour and hold the connection open.
///
/// Xvfb resets its root window when the last client disconnects, so a test that samples
/// the root window has to keep a connection alive for as long as it samples. A `Painter`
/// holds one, but the static tests have nothing to paint and so need this instead.
fn keep_alive(display: &str) -> Screen {
    let screen = Screen::open(display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [30, 30, 30]),
    );
    screen
}

// ============================================================================
// 58: a moving scene
// ============================================================================

#[test]
fn a_moving_scene_appears_in_temporal_order() {
    // The canonical use case: something moves while the stack is being captured, and the
    // frames should show it at successive positions rather than all at one.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-moving", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);

    // Paint three separated blocks over the sampling window. Each is large enough to
    // clear the thresholds by a wide margin, so a frame that caught the scene at a given
    // state is unambiguous.
    let painter = Painter::start(
        display.clone(),
        vec![
            (
                Duration::from_millis(20),
                vec![paint_rect(0, 0, [255, 0, 0])],
            ),
            (
                Duration::from_millis(90),
                vec![paint_rect(150, 0, [0, 255, 0])],
            ),
            (
                Duration::from_millis(160),
                vec![paint_rect(280, 0, [0, 0, 255])],
            ),
        ],
    );

    let (code, value, stderr) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "3",
            "--interval",
            "80ms",
            "--timeout",
            "500ms",
            "--base64",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");
    drop(painter);

    let frames = frames_of(&value);
    assert_eq!(
        frames.len(),
        3,
        "three frames were requested and should all be returned: {value}"
    );

    // Oldest to newest: offsets must strictly increase.
    let offsets = offsets_of(&value);
    assert!(
        offsets.windows(2).all(|pair| pair[0] < pair[1]),
        "offsets must strictly increase, oldest first: {offsets:?}"
    );

    // Every frame is a decodable image of the full screen.
    for frame in frames {
        let data = frame["image"]["data"].as_str().expect("inline base64");
        let decoded = decode_png(&base64_decode(data));
        assert_eq!((decoded.width, decoded.height), (SCREEN_W, SCREEN_H));
    }

    // The stack is genuinely a stack of *different* moments: the oldest frame does not
    // already contain everything the newest one does.
    let oldest = decode_png(&base64_decode(frames[0]["image"]["data"].as_str().unwrap()));
    let newest = decode_png(&base64_decode(frames[2]["image"]["data"].as_str().unwrap()));
    assert_ne!(
        oldest.rgb, newest.rgb,
        "the oldest and newest frames should differ, since the scene moved"
    );

    // The newest frame is named explicitly and is the last in the stack.
    let newest_id = body(&value)["newest_frame_id"].as_u64().unwrap();
    assert_eq!(
        newest_id,
        frames.last().unwrap()["frame_id"].as_u64().unwrap()
    );
    assert!(
        body(&value)["newest_frame_age_us"].as_u64().unwrap() > 0,
        "the newest frame already has an age when it is reported: {value}"
    );
}

#[test]
fn a_static_scene_still_returns_every_requested_frame() {
    // Requirement 44: no implicit deduplication. Three identical captures are three
    // frames, and the fact that nothing moved is itself information.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-static", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    let (code, value, stderr) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "3",
            "--interval",
            "40ms",
            "--timeout",
            "500ms",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");

    assert_eq!(
        section(&value)["captured_frames"],
        3,
        "a still scene must still yield three physical captures: {value}"
    );
    assert_eq!(section(&value)["requested_frames"], 3);
    assert_eq!(section(&value)["result"], "complete");

    // Three distinct identities, one per physical capture.
    let ids = frame_ids(&value);
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        3,
        "each capture gets its own identity: {ids:?}"
    );
}

// ============================================================================
// 54: identity, history, and native resolution
// ============================================================================

#[test]
fn every_realtime_sample_enters_session_history() {
    // Requirement 54: every real-time frame enters history, so it can be retrieved
    // afterwards through the ordinary Phase 4 path.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-history", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    let (code, value, stderr) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "3",
            "--interval",
            "30ms",
            "--timeout",
            "500ms",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");

    let ids = frame_ids(&value);
    assert_eq!(ids.len(), 3);

    for id in &ids {
        let (code, value) = service.run_json(&["frame", &session, &id.to_string(), "--json"]);
        assert_eq!(code, 0, "frame {id} should be retained: {value}");
        assert_eq!(value["frame"]["frame_id"], *id);
    }

    let (code, info) = service.run_json(&["info", &session, "--json"]);
    assert_eq!(code, 0);
    assert_eq!(
        info["info"]["frames_captured"], 3,
        "history should account for all three samples: {info}"
    );
    assert_eq!(
        info["info"]["history"]["newest_frame_id"],
        *ids.last().unwrap()
    );
}

#[test]
fn output_resizing_does_not_affect_the_native_capture_or_history() {
    // Requirement 20: a `--width` request applies only after sampling. The stored frame
    // stays at native resolution.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-native", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    let (code, value, stderr) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "2",
            "--interval",
            "30ms",
            "--timeout",
            "500ms",
            "--width",
            "80",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");

    // The returned images are resized...
    for frame in frames_of(&value) {
        assert_eq!(frame["image"]["width"], 80, "the returned image is resized");
        assert_eq!(
            frame["image"]["height"], 60,
            "the aspect ratio is preserved"
        );
    }

    // ...but the source geometry the stack reports is native, and the transform says how
    // to get back to it.
    assert_eq!(body(&value)["source"]["width"], SCREEN_W);
    assert_eq!(body(&value)["source"]["height"], SCREEN_H);
    assert_eq!(body(&value)["transform"]["scale_x"], 5.0);

    // Retrieving one of those frames with no resize returns full size, which shows the
    // stored frame was never shrunk.
    let id = frames_of(&value)[0]["frame_id"].as_u64().unwrap();
    let (code, retrieved) = service.run_json(&["frame", &session, &id.to_string(), "--json"]);
    assert_eq!(code, 0);
    assert_eq!(retrieved["frame"]["source"]["width"], SCREEN_W);
    assert_eq!(retrieved["frame"]["image"]["width"], SCREEN_W);
}

#[test]
fn an_external_capture_interleaves_without_corrupting_the_stack() {
    // Requirement 14: external captures may interleave, so stack identities are not
    // assumed contiguous. The stack must contain only its own frames, in order.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-interleave", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    // Two external captures first, so the session is already past its first frames.
    for _ in 0..2 {
        let (code, _, stderr) = service.run(&["capture", &session, "--json"]);
        assert_eq!(code, 0, "external capture failed: {stderr}");
    }

    let (code, value, stderr) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "2",
            "--interval",
            "30ms",
            "--timeout",
            "500ms",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");

    let ids = frame_ids(&value);
    assert_eq!(ids.len(), 2);
    assert!(
        ids.iter().all(|id| *id > 2),
        "stack identities should follow the external captures: {ids:?}"
    );
    assert!(
        ids[0] < ids[1],
        "the stack is still oldest to newest: {ids:?}"
    );

    // The stack reported exactly what it captured, no more and no less.
    assert_eq!(section(&value)["captured_frames"], 2);
    assert_eq!(section(&value)["requested_frames"], 2);

    let (code, info) = service.run_json(&["info", &session, "--json"]);
    assert_eq!(code, 0);
    assert_eq!(
        info["info"]["frames_captured"], 4,
        "four captures happened in total: {info}"
    );
}

// ============================================================================
// 54/15: operation-owned references survive eviction
// ============================================================================

#[test]
fn a_realtime_stack_survives_a_history_that_cannot_hold_it() {
    // Requirements 15 and 54: the operation owns its frames, so a stack larger than the
    // public history still returns every frame. Capacity is not increased.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-eviction", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    // Capacity 2 with four frames requested, so the early samples are certainly evicted
    // from public history before the response is assembled.
    let session = create_session(&service, &display, &["--history", "2"]);
    let _keep = keep_alive(&display);

    let (code, value, stderr) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "4",
            "--interval",
            "20ms",
            "--timeout",
            "800ms",
            "--width",
            "80",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");

    assert_eq!(
        section(&value)["captured_frames"],
        4,
        "a stack larger than history must still be returned whole: {value}"
    );

    let ids = frame_ids(&value);
    assert_eq!(ids.len(), 4);

    // The early frames are gone from public history, and asking for them says so
    // explicitly rather than substituting a different frame.
    let (code, value) = service.run_error(&["frame", &session, &ids[0].to_string(), "--json"]);
    assert_eq!(
        code, 20,
        "an evicted frame must be explicitly unavailable: {value}"
    );
    assert_eq!(value["error"]["code"], "frame_not_available");

    // Capacity was not silently raised to accommodate the stack.
    let (code, info) = service.run_json(&["info", &session, "--json"]);
    assert_eq!(code, 0);
    assert_eq!(
        info["info"]["history"]["capacity"], 2,
        "public history capacity must not change: {info}"
    );
    assert!(
        info["info"]["history"]["retained"].as_u64().unwrap() <= 2,
        "history must stay bounded: {info}"
    );
}

// ============================================================================
// 57: backpressure
// ============================================================================

#[test]
fn a_second_realtime_is_refused_while_the_first_is_running() {
    // Requirements 28 and 57: only one temporal operation per session. A queued real-time
    // observation would be stale before it started, so it is refused.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-exclusive", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    let running = start_realtime(&service, &session);

    // Let it take the temporal slot.
    std::thread::sleep(Duration::from_millis(700));

    // A second real-time request must be refused, and refused promptly.
    let started = std::time::Instant::now();
    let (code, value, _) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "2",
            "--interval",
            "20ms",
            "--timeout",
            "500ms",
        ],
    );
    let elapsed = started.elapsed();
    assert_eq!(
        code, 18,
        "a second realtime should be refused with session_busy: {value}"
    );
    assert!(
        elapsed < Duration::from_millis(1000),
        "the refusal should be immediate rather than queued, took {elapsed:?}"
    );

    // So must every other temporal operation, which is the point of the rule.
    for operation in ["wait-change", "wait-stable", "observe"] {
        let (code, value) =
            service.run_error(&[operation, &session, "--json", "--timeout", "800ms"]);
        assert_eq!(
            code, 18,
            "{operation} should be refused while realtime runs: {value}"
        );
        assert_eq!(value["error"]["code"], "session_busy");
    }

    let (code, _, stderr) = running.wait();
    assert_eq!(code, 0, "the first realtime should still succeed: {stderr}");
}

#[test]
fn a_capture_during_realtime_is_serialized_rather_than_refused() {
    // Requirement 29: a one-shot capture may interleave at a physical capture boundary.
    // It is served rather than refused, which is what keeps real-time observation from
    // blocking ordinary use of the same session.
    let _guard = serial();
    let Some((_server, display, service)) =
        xvfb_service("realtime-capture-interleave", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    let running = start_realtime(&service, &session);
    std::thread::sleep(Duration::from_millis(600));

    // The capture must succeed. It may wait for a sample boundary, but it must not be
    // refused with session_busy.
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = service.run(&["capture", &session, "--json"]);
    let elapsed = started.elapsed();
    assert_eq!(
        code, 0,
        "a capture during realtime should be served, not refused: {stderr}"
    );
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["kind"], "frame");
    assert!(
        elapsed < Duration::from_millis(3000),
        "the capture should not be parked for the whole observation, took {elapsed:?}"
    );

    let (code, _, stderr) = running.wait();
    assert_eq!(
        code, 0,
        "the realtime request should have completed, got {code}: {stderr}"
    );
}

#[test]
fn realtime_in_one_session_does_not_block_another() {
    // Requirement 57: no global lock. A real-time observation in session A must leave
    // session B free, which is only true because the locking is per-session.
    let _guard = serial();
    let Some((_server, display, service)) =
        xvfb_service("realtime-independent", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session_a = create_session(&service, &display, &[]);
    let session_b = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    let running = start_realtime(&service, &session_a);
    std::thread::sleep(Duration::from_millis(600));

    // B must be free: a real-time request of its own, which would be refused if the
    // sessions shared any state.
    let started = std::time::Instant::now();
    let (code, value, stderr) = realtime(
        &service,
        &[
            &session_b,
            "--frames",
            "2",
            "--interval",
            "20ms",
            "--timeout",
            "800ms",
        ],
    );
    let elapsed = started.elapsed();

    assert_eq!(code, 0, "session B should be unaffected: {stderr} {value}");
    assert_eq!(
        section(&value)["captured_frames"],
        2,
        "session B should have sampled fully: {value}"
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "session B should not wait for session A's realtime, took {elapsed:?}"
    );

    let (code, _, stderr) = running.wait();
    assert_eq!(code, 0, "session A's realtime should succeed: {stderr}");
}

// ============================================================================
// 58: target loss during sampling
// ============================================================================

#[test]
fn destroying_the_target_during_realtime_reports_target_lost() {
    // Requirements 27 and 58: a lost window is a structured terminal error rather than a
    // partial result, and the session is marked failed.
    let _guard = serial();
    let Some((_server, display, service)) =
        xvfb_service("realtime-target-lost", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let screen = Screen::open(&display);
    let window = screen.create_window(120, 90, 20, 20);
    screen.map(window);

    let (code, value) = service.run_json(&[
        "create",
        "--display",
        &display,
        "--window",
        &format!("0x{window:x}"),
        "--json",
    ]);
    assert_eq!(code, 0, "session creation should succeed: {value}");
    let session = value["session_id"].as_str().unwrap().to_string();

    // Destroy the window before the observation, so the very first sample fails.
    screen.destroy(window);
    std::thread::sleep(Duration::from_millis(150));

    let (code, value, _) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "3",
            "--interval",
            "30ms",
            "--timeout",
            "500ms",
        ],
    );

    assert_eq!(
        code, 14,
        "a destroyed target during realtime should report target_lost: {value}"
    );
    assert_eq!(value["error"]["code"], "target_lost");

    // And the session is failed rather than left claiming to be usable.
    let (code, info) = service.run_json(&["info", &session, "--json"]);
    assert_eq!(code, 0);
    assert_eq!(info["info"]["state"], "failed");
}

// ============================================================================
// 19: encoding does not happen between samples
// ============================================================================

#[test]
fn sampling_gaps_track_the_cadence_rather_than_an_encode_cost() {
    // Requirement 19 is mandatory and its effect is measurable: if frames were encoded
    // inside the sampling window, the reported capture offsets would be pushed apart by
    // the encode time. With a wide interval and a fast capture they should instead sit
    // close to their cadence slots.
    //
    // This does not assert exact offsets — live capture duration varies — only that the
    // gaps track the requested cadence rather than the cost of two full PNG encodes.
    let _guard = serial();
    let Some((_server, display, service)) =
        xvfb_service("realtime-encoding-order", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    let (code, value, stderr) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "3",
            "--interval",
            "200ms",
            "--timeout",
            "2s",
            "--base64",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");

    let offsets = offsets_of(&value);

    // The first sample is immediate.
    assert!(
        offsets[0] < 50_000,
        "the first sample should be immediate: {offsets:?}"
    );

    // The later ones are near their slots, not a full encode later.
    for pair in offsets.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            (120_000..400_000).contains(&gap),
            "the gap should track the 200ms cadence rather than an encode cost: {offsets:?}"
        );
    }

    // The timing block reports the two phases separately, which is what makes the claim
    // above checkable rather than merely asserted: sampling and encoding are distinct
    // quantities, so encoding cannot secretly be inside sampling.
    let timing = &body(&value)["timing"];
    let sampling_us = timing["sampling_us"].as_u64().unwrap();
    let encode_us_total = timing["encode_us_total"].as_u64().unwrap();
    let base64_us_total = timing["base64_us_total"].as_u64().unwrap();
    assert!(sampling_us > 0 && encode_us_total > 0 && base64_us_total > 0);

    // Sampling decomposes into capture plus sleep. The two account for the window
    // almost exactly; the small remainder is loop bookkeeping, and the point is that
    // there is no *room* in there for an encode, which the encode total above shows
    // happened separately.
    let capture_us_total = timing["capture_us_total"].as_u64().unwrap();
    let sleep_us_total = timing["sleep_us_total"].as_u64().unwrap();
    let accounted = capture_us_total + sleep_us_total;
    assert!(
        accounted <= sampling_us,
        "capture plus sleep cannot exceed the sampling window: {timing}"
    );
    assert!(
        sampling_us - accounted < 1_000,
        "the window should be accounted for by capture and sleep, not by encoding: {timing}"
    );

    // Encoding is reported outside the window and is a comparable magnitude to it,
    // which is what makes its exclusion from the offsets observable at all.
    assert!(
        encode_us_total > 1_000,
        "three PNG encodes should be visible in the timing: {timing}"
    );
}

#[test]
fn each_frame_is_independently_decodable() {
    // Requirement 40: base64 is first class per frame, not one concatenated blob.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-base64", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    let (code, value, stderr) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "3",
            "--interval",
            "20ms",
            "--timeout",
            "500ms",
            "--base64",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");

    let frames = frames_of(&value);
    assert_eq!(frames.len(), 3);

    for frame in frames {
        assert_eq!(frame["image"]["media_type"], "image/png");
        assert_eq!(frame["image"]["encoding"], "base64");
        let data = frame["image"]["data"].as_str().expect("inline base64");
        let bytes = base64_decode(data);

        // Each decodes on its own into a complete image.
        let decoded = decode_png(&bytes);
        assert_eq!(
            (decoded.width, decoded.height),
            (SCREEN_W, SCREEN_H),
            "each payload should be a whole image"
        );
        assert_eq!(
            bytes.len() as u64,
            frame["image"]["byte_length"].as_u64().unwrap(),
            "the declared length should be the decoded length"
        );
        assert_eq!(
            &bytes[..8],
            b"\x89PNG\r\n\x1a\n",
            "each frame should be a standalone PNG"
        );
    }
}

#[test]
fn metadata_only_mode_returns_identities_and_timing_without_images() {
    // Requirement 43: an orchestrator can read the timing and decide what to fetch.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-metadata", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    let (code, value, stderr) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "3",
            "--interval",
            "20ms",
            "--timeout",
            "500ms",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {stderr} {value}");

    for frame in frames_of(&value) {
        assert!(
            frame["image"]["data"].is_null(),
            "without base64 no inline data should be present: {frame}"
        );
        // The timing metadata is still complete.
        assert!(frame["frame_id"].is_number());
        assert!(frame["capture_offset_us"].is_number());
        assert!(frame["capture_duration_us"].is_number());
        assert!(frame["age_us"].is_number());
        assert!(frame["image"]["width"].is_number());
        assert!(frame["image"]["byte_length"].is_number());
    }

    // The identities are enough to fetch the frames afterwards.
    let id = frames_of(&value)[0]["frame_id"].as_u64().unwrap();
    let (code, _) = service.run_json(&["frame", &session, &id.to_string(), "--json"]);
    assert_eq!(code, 0, "the frame should be retrievable by identity");
}

// ============================================================================
// 57 (reverse): temporal exclusivity is symmetric
// ============================================================================

#[test]
fn realtime_arriving_during_an_observation_is_refused_the_same_way() {
    // The previous test shows realtime refusing a second temporal operation. It would be
    // easy for this direction to work by accident while the other direction were left
    // lenient, so the rule is checked both ways.
    //
    // The rule lives in the manager's `try_get` rather than in the real-time code, so
    // every temporal operation is covered by one check. That is also what makes the
    // refusal *specific*: an observation refuses a real-time request by name, rather
    // than realtime being a special case with its own message.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-waits", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    // An observation that will run for a while: it waits for a change that never comes.
    let observing = Background::start(vec![
        "session".to_string(),
        "wait-change".to_string(),
        session.clone(),
        "--socket".to_string(),
        service.socket_arg(),
        "--json".to_string(),
        "--timeout".to_string(),
        "2s".to_string(),
    ]);

    // Let it take the temporal slot.
    std::thread::sleep(Duration::from_millis(600));

    // A real-time request must be refused, promptly and by name.
    let started = std::time::Instant::now();
    let (code, value, _) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "2",
            "--interval",
            "20ms",
            "--timeout",
            "1s",
        ],
    );
    let elapsed = started.elapsed();

    assert_eq!(
        code, 18,
        "realtime during an observation should be refused: {value}"
    );
    assert_eq!(value["error"]["code"], "session_busy");
    assert!(
        elapsed < Duration::from_millis(500),
        "the refusal should be immediate rather than queued behind the observation, \
         took {elapsed:?}"
    );

    // The observation ran to completion rather than being disturbed. Nothing moved, so
    // it ends in the timeout outcome, which is its correct result rather than a fault.
    let (code, stdout, stderr) = observing.wait();
    assert_eq!(
        code, 100,
        "the observation should run out its own timeout undisturbed: {stdout} {stderr}"
    );

    // And the session is usable again once it has: the refusal cost nothing.
    let (code, info) = service.run_json(&["info", &session, "--json"]);
    assert_eq!(code, 0);
    assert_eq!(
        info["info"]["state"], "ready",
        "the session is usable again: {info}"
    );

    // The refused request left no trace: only the observation's frames are retained.
    let (code, value, stderr) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "2",
            "--interval",
            "20ms",
            "--timeout",
            "1s",
        ],
    );
    assert_eq!(code, 0, "realtime should work now: {stderr} {value}");
    assert_eq!(section(&value)["captured_frames"], 2);
    assert_eq!(
        section(&value)["skipped_opportunities"],
        0,
        "the second attempt starts clean rather than resuming the refused one: {value}"
    );
}

// ============================================================================
// 26: a partial stack is returned, not withheld
// ============================================================================

#[test]
fn a_stack_that_cannot_fill_in_time_is_returned_partially() {
    // Requirement 26: when the deadline cannot accommodate the whole request, the frames
    // that were obtained are returned with an explicit outcome, rather than the request
    // failing and discarding work that was already done.
    //
    // The configuration below asks for eight frames a second apart but allows only a
    // quarter second, so at most the first sample can fit. This is deliberately
    // impossible-looking, and requirement 10 says it is accepted rather than rejected:
    // the caller's scheduling mistake is reported as a partial result.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-partial", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    let (code, value, stderr) = realtime(
        &service,
        &[
            &session,
            "--frames",
            "8",
            "--interval",
            "1s",
            "--timeout",
            "250ms",
        ],
    );
    assert_eq!(
        code, 0,
        "an impossible schedule is a partial result, not an error: {stderr} {value}"
    );

    assert_eq!(section(&value)["result"], "partial");
    let captured = section(&value)["captured_frames"].as_u64().unwrap();
    assert_eq!(
        captured, 1,
        "only the first sample can fit in the deadline: {value}"
    );
    assert_eq!(section(&value)["requested_frames"], 8);
    assert_eq!(
        section(&value)["skipped_opportunities"],
        0,
        "no opportunity was *skipped*; the schedule simply ran out of time: {value}"
    );

    // The one frame that was obtained is real, complete, and retrievable.
    let frames = frames_of(&value);
    assert_eq!(frames.len(), 1);
    let id = frames[0]["frame_id"].as_u64().unwrap();
    assert_eq!(body(&value)["newest_frame_id"], id);

    let (code, retrieved) = service.run_json(&["frame", &session, &id.to_string(), "--json"]);
    assert_eq!(code, 0, "the partial frame is retained: {retrieved}");

    // The session is still usable: a partial result is not a failure.
    let (code, info) = service.run_json(&["info", &session, "--json"]);
    assert_eq!(code, 0);
    assert_eq!(info["info"]["state"], "ready");
}

// ============================================================================
// Framing: the options actually mean something
// ============================================================================
#[test]
fn a_frame_count_outside_the_supported_range_is_rejected() {
    // Requirement 11: the bound is enforced before anything reaches the wire.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-bound", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);

    let (code, _, _) = realtime(&service, &[&session, "--frames", "9"]);
    assert_eq!(code, 2, "more than the maximum should be invalid");

    let (code, _, _) = realtime(&service, &[&session, "--frames", "0"]);
    assert_eq!(code, 2, "zero frames should be invalid");

    // And the session is untouched by either refusal.
    let (code, info) = service.run_json(&["info", &session, "--json"]);
    assert_eq!(code, 0);
    assert_eq!(info["info"]["state"], "ready");
}

#[test]
fn a_single_frame_stack_needs_no_interval() {
    // Requirement 9: one frame is a degenerate but legitimate stack, and it should not be
    // forced to carry timing that has no meaning for it.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("realtime-single", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display, &[]);
    let _keep = keep_alive(&display);

    let (code, value, stderr) = realtime(&service, &[&session, "--frames", "1"]);
    assert_eq!(code, 0, "a single frame should succeed: {stderr} {value}");

    assert_eq!(section(&value)["result"], "complete");
    assert_eq!(section(&value)["captured_frames"], 1);
    assert_eq!(section(&value)["skipped_opportunities"], 0);
    assert_eq!(frames_of(&value).len(), 1);

    // The offset is recorded when the sample completes, so it carries that sample's own
    // capture time rather than being exactly zero. What matters is that there was no
    // wait: the only sample is taken at the start of the request.
    let offset = offsets_of(&value)[0];
    assert!(
        offset < 50_000,
        "the single sample should be taken immediately, not after a delay: {offset}us"
    );

    // With one frame there is no cadence, so nothing sleeps between samples.
    assert!(
        body(&value)["timing"]["sleep_us_total"].as_u64().unwrap() < 50_000,
        "a single frame should not sleep between samples: {value}"
    );
}
