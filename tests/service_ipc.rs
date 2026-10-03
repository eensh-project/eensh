//! Phase 4 service integration tests: the socket, the protocol, and the sessions
//! behind them.
//!
//! These drive the real `eensh serve` process over a real Unix socket. That is
//! deliberate: the transport, the framing, and the error mapping are part of the
//! contract an agent depends on, and none of them would be exercised by calling
//! the handler in-process.
//!
//! Requirements 62, 63, 66, 67, 68, and 69 are covered across this file and the
//! others in the suite; the mapping is stated at each test rather than repeated.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::*;

const SCREEN_W: u32 = 320;
const SCREEN_H: u32 = 240;

/// Create a session for a display and return its identifier.
fn create_session(service: &ServiceProcess, display: &str, extra: &[&str]) -> String {
    let mut args = vec!["create", "--display", display, "--json"];
    args.extend_from_slice(extra);

    let (code, value) = service.run_json(&args);
    assert_eq!(code, 0, "session creation failed: {value}");

    value["session_id"]
        .as_str()
        .expect("a created session should carry an id")
        .to_string()
}

// ============================================================================
// 68. Service startup, socket, and health
// ============================================================================

#[test]
fn the_service_creates_an_owner_only_socket_and_answers_ping() {
    let service = ServiceProcess::start("startup");

    // 68: Unix socket creation.
    assert!(
        service.socket().exists(),
        "the service should create its socket"
    );

    // 69: safe permissions. The service returns raw desktop pixels, so the socket
    // must never be reachable by another user.
    let mode = std::fs::metadata(service.socket())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the socket must be owner-only, was {mode:o}");

    // 68: ping/status. A successful ping reports its status on stdout.
    let (code, value, stderr) =
        run_json_stdout(&["ping", "--socket", &service.socket_arg(), "--json"]);
    assert!(!value.is_null(), "ping produced no JSON: {stderr}");
    assert_eq!(code, 0, "ping should succeed: {value}");
    assert_eq!(
        value["kind"], "status",
        "ping should report a status body, got {value}"
    );
    assert!(value["status"]["protocol_version"].is_number());
    assert_eq!(
        value["status"]["sessions"], 0,
        "a fresh service holds no sessions"
    );
}

#[test]
fn the_service_removes_its_socket_on_shutdown() {
    // 68: service shutdown and socket cleanup. 69: no leaked sockets.
    let socket = {
        let service = ServiceProcess::start("shutdown-cleanup");
        let socket = service.socket().to_path_buf();
        assert!(socket.exists());

        // Dropping asks the service to stop with SIGTERM and waits for it.
        drop(service);
        socket
    };

    assert!(
        !socket.exists(),
        "the socket at {} was left behind after shutdown",
        socket.display()
    );
}

#[test]
fn a_stale_socket_from_a_crashed_service_is_reclaimed() {
    // 49: service restart. A killed service cannot clean up, so the next start
    // must not be blocked by its leftovers.
    let directory = temp_dir("stale-socket");
    let socket = directory.join("eensh.sock");
    let _ = std::fs::remove_file(&socket);

    // Simulate a crash: bind a listener and drop it without removing the file.
    {
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    }
    assert!(socket.exists(), "the socket file should survive a crash");

    let service = ServiceProcess::start_at(&socket);
    let (code, value, stderr) =
        run_json_stdout(&["ping", "--socket", &service.socket_arg(), "--json"]);
    assert!(!value.is_null(), "ping produced no JSON: {stderr}");
    assert_eq!(code, 0, "the restarted service should answer: {value}");
}

// ============================================================================
// 63. Session lifecycle
// ============================================================================

#[test]
fn a_session_can_be_created_listed_described_and_closed() {
    let _guard = serial();
    let Some((_server, _display, service)) = xvfb_service("lifecycle", SCREEN_W, SCREEN_H) else {
        return;
    };
    let display = _display;

    // Create: creation resolves the target geometry eagerly, so the info is
    // already populated before any capture.
    let session_id = create_session(&service, &display, &[]);

    // List.
    let (code, value) = service.run_json(&["list", "--json"]);
    assert_eq!(code, 0);
    let sessions = value["sessions"].as_array().expect("a list of sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"], session_id.as_str());

    // Describe. The payload is a `session_info` body, so the summary is nested
    // under `info` alongside its `kind` tag.
    let (code, value) = service.run_json(&["info", &session_id, "--json"]);
    assert_eq!(code, 0);
    assert_eq!(value["kind"], "session_info");
    let info = &value["info"];
    assert_eq!(info["session_id"], session_id.as_str());
    assert_eq!(info["state"], "ready");
    assert_eq!(info["frames_captured"], 0);
    assert_eq!(
        info["geometry"]["width"], SCREEN_W,
        "creation should resolve geometry eagerly"
    );

    // Close.
    let (code, value) = service.run_json(&["close", &session_id, "--json"]);
    assert_eq!(code, 0, "close should succeed: {value}");

    // The closed session is gone. A failure is reported on stderr.
    let (code, _) = service.run_error(&["info", &session_id, "--json"]);
    assert_eq!(code, 17, "a closed session should report session_not_found");
}

#[test]
fn closing_an_unregistered_session_is_reported_as_not_found() {
    let service = ServiceProcess::start("close-unknown");

    let (code, value) = service.run_error(&["close", "s-does-not-exist", "--json"]);
    assert_eq!(code, 17, "expected session_not_found, got {value}");
    assert_eq!(value["error"]["code"], "session_not_found");
}

#[test]
fn sessions_start_with_no_frames_and_report_frame_not_available() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("empty-session", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session_id = create_session(&service, &display, &[]);

    // `latest` on a session that has not captured yet is `no_frame_available`,
    // which is distinct from asking for an identifier that does not exist.
    // Failures are reported on stderr, as for every non-observation command.
    let (code, value) = service.run_error(&["latest", &session_id, "--json"]);
    assert_eq!(code, 21, "expected no_frame_available, got {value}");
    assert_eq!(value["error"]["code"], "no_frame_available");

    // Asking for a specific frame number is `frame_not_available`.
    let (code, value) = service.run_error(&["frame", &session_id, "1", "--json"]);
    assert_eq!(code, 20, "expected frame_not_available, got {value}");
    assert_eq!(value["error"]["code"], "frame_not_available");
}

// ============================================================================
// 68. Capture, retrieval, and diff
// ============================================================================

#[test]
fn capture_assigns_monotonic_frame_ids_and_retrieval_returns_them() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("capture-ids", SCREEN_W, SCREEN_H) else {
        return;
    };

    let session_id = create_session(&service, &display, &[]);

    // Two captures, with identifiers 1 and 2. The response body is a `frame`
    // variant, so the payload is nested under that key along with a `kind` tag.
    for expected in [1, 2] {
        let (code, value) = service.run_json(&["capture", &session_id, "--json"]);
        assert_eq!(code, 0, "capture {expected} failed: {value}");
        assert_eq!(value["kind"], "frame");
        assert_eq!(
            value["frame"]["frame_id"], expected,
            "frame identifiers should be monotonic from 1"
        );
        assert_eq!(
            value["frame"]["fresh_capture"], true,
            "a capture request should report a fresh capture"
        );
    }

    // Retrieval does not capture, and says so.
    let (code, value) = service.run_json(&["latest", &session_id, "--json"]);
    assert_eq!(code, 0);
    assert_eq!(value["frame"]["frame_id"], 2);
    assert_eq!(
        value["frame"]["fresh_capture"], false,
        "a retrieval must not report a fresh capture"
    );

    // A specific retained frame is retrievable and reports its age.
    let (code, value) = service.run_json(&["frame", &session_id, "1", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(value["frame"]["frame_id"], 1);
    assert!(
        value["frame"]["frame_age_us"].as_u64().unwrap() > 0,
        "a retained frame should have a positive age"
    );
}

#[test]
fn a_capture_can_be_returned_inline_as_base64_and_decoded() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("capture-base64", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session_id = create_session(&service, &display, &[]);

    let (code, value) = service.run_json(&["capture", &session_id, "--json", "--base64"]);
    assert_eq!(code, 0, "capture failed: {value}");

    let image = &value["frame"]["image"];
    assert_eq!(image["encoding"], "base64");
    assert_eq!(image["media_type"], "image/png");

    // Decode it, so this asserts the bytes are a real image rather than merely
    // that a string was present.
    let data = image["data"].as_str().expect("inline base64 data");
    let bytes = base64_decode(data);
    let decoded = decode_png(&bytes);
    assert_eq!(decoded.width, SCREEN_W);
    assert_eq!(decoded.height, SCREEN_H);
}

#[test]
fn a_resize_is_reflected_in_the_returned_frame() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("capture-resize", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session_id = create_session(&service, &display, &[]);

    let (code, value) = service.run_json(&[
        "capture",
        &session_id,
        "--width",
        "80",
        "--json",
        "--base64",
    ]);
    assert_eq!(code, 0, "resized capture failed: {value}");
    assert_eq!(value["frame"]["image"]["width"], 80);
    assert_eq!(
        value["frame"]["image"]["height"], 60,
        "the aspect ratio should be preserved"
    );

    // The transform must describe how the returned image maps back to the source,
    // which is what lets an agent turn a pixel in the small image into a
    // coordinate in the real one.
    assert_eq!(value["frame"]["transform"]["scale_x"], 4.0);
    assert_eq!(value["frame"]["source"]["width"], SCREEN_W);
}

#[test]
fn two_retained_frames_can_be_compared() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("session-diff", SCREEN_W, SCREEN_H) else {
        return;
    };

    let session_id = create_session(&service, &display, &[]);

    // A black screen, captured twice with a paint in between.
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

    service.run_json(&["capture", &session_id, "--json"]);
    screen.fill(
        screen.root(),
        10,
        10,
        100,
        60,
        rgb_to_pixel(masks, [255, 255, 255]),
    );
    service.run_json(&["capture", &session_id, "--json"]);

    let (code, value) = service.run_json(&["diff", &session_id, "1", "2", "--json"]);
    assert_eq!(code, 0, "diff failed: {value}");

    let comparison = &value["diff"]["comparison"];
    assert_eq!(comparison["changed"], true, "the paint should be detected");
    assert_eq!(
        comparison["changed_pixels"], 6000,
        "a 100x60 block is exactly 6000 pixels"
    );

    // The bounding box should be the painted rectangle, in source coordinates.
    assert_eq!(comparison["bounding_box"]["x"], 10);
    assert_eq!(comparison["bounding_box"]["y"], 10);
    assert_eq!(comparison["bounding_box"]["width"], 100);
    assert_eq!(comparison["bounding_box"]["height"], 60);
}

#[test]
fn comparing_with_an_unretained_frame_fails_explicitly() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("diff-missing", SCREEN_W, SCREEN_H) else {
        return;
    };

    let session_id = create_session(&service, &display, &[]);
    service.run_json(&["capture", &session_id, "--json"]);

    let (code, value) = service.run_error(&["diff", &session_id, "1", "99", "--json"]);
    assert_eq!(code, 20, "expected frame_not_available, got {value}");
    assert_eq!(value["error"]["code"], "frame_not_available");
}

// ============================================================================
// 68. Observe over the socket, and 67. Observation exclusivity
// ============================================================================

#[test]
fn an_observation_reports_the_frames_that_mattered() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("observe-frames", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session_id = create_session(&service, &display, &[]);

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

    // Paint a change well after the observation starts, then leave it.
    let painter = Painter::start(
        display.clone(),
        vec![(
            Duration::from_millis(300),
            vec![paint_rect(40, 30, [255, 0, 0])],
        )],
    );

    let (code, value) = service.run_json(&[
        "observe",
        &session_id,
        "--json",
        "--timeout",
        "3s",
        "--stable-for",
        "200ms",
        "--interval",
        "50ms",
    ]);
    assert_eq!(code, 0, "observe failed: {value}");
    drop(painter);

    let summary = &value["observation"]["observation"];
    assert_eq!(
        summary["result"], "observed",
        "expected a transition: {value}"
    );
    assert_eq!(summary["kind"], "observe");

    // The frame identifiers must be present and coherent: the baseline is the
    // first frame, the change comes after it, and the final frame is the last.
    let frames = &value["observation"]["frames"];
    let baseline = frames["baseline"].as_u64().expect("a baseline frame id");
    let first_change = frames["first_change"]
        .as_u64()
        .expect("a first-change frame id");
    let final_frame = frames["final_frame"].as_u64().expect("a final frame id");

    assert_eq!(baseline, 1, "the baseline is the first frame captured");
    assert!(
        first_change > baseline,
        "the first change must come after the baseline: {first_change} vs {baseline}"
    );
    assert!(
        final_frame >= first_change,
        "the final frame cannot precede the change: {final_frame} vs {first_change}"
    );
}

#[test]
fn a_second_observation_is_refused_while_one_is_running() {
    // 67: observation exclusivity. The spec prefers an explicit refusal over
    // invisible queueing, so this asserts the specific error rather than merely
    // that something went wrong.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("observe-exclusive", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session_id = create_session(&service, &display, &[]);

    // A long-running observation in a background thread.
    let socket = service.socket_arg();
    let session_for_thread = session_id.clone();
    let handle = std::thread::spawn(move || {
        run_eensh_text(&[
            "session",
            "observe",
            &session_for_thread,
            "--socket",
            &socket,
            "--json",
            "--timeout",
            "3s",
            "--stable-for",
            "300ms",
            "--interval",
            "100ms",
        ])
    });

    // Give the observation time to take the exclusivity slot.
    std::thread::sleep(Duration::from_millis(700));

    let (code, value) =
        service.run_error(&["wait-change", &session_id, "--json", "--timeout", "500ms"]);
    assert_eq!(code, 18, "expected session_busy, got {value}");
    assert_eq!(value["error"]["code"], "session_busy");

    let _ = handle.join();
}

// ============================================================================
// 68. Malformed requests, protocol version, and unknown sessions
// ============================================================================

#[test]
fn a_malformed_request_is_answered_with_an_error_rather_than_a_hang() {
    // 68: malformed request. A client that sends nonsense must get a reply, not a
    // dropped connection, because a hang is indistinguishable from a slow capture.
    let service = ServiceProcess::start("malformed");

    let reply = service
        .send_raw("this is not json")
        .expect("the service should reply rather than close the connection");

    let value: serde_json::Value =
        serde_json::from_str(&reply).expect("the reply should itself be JSON");
    assert_eq!(value["ok"], false, "a malformed request must fail: {value}");
    assert!(
        value["error"]["code"].is_string(),
        "the failure should carry a stable code: {value}"
    );
}

#[test]
fn a_valid_request_missing_a_required_field_is_rejected_cleanly() {
    let service = ServiceProcess::start("missing-field");

    // Correct JSON, but no `method`, so it cannot be dispatched.
    let reply = service
        .send_raw(r#"{"request_id":"r-1"}"#)
        .expect("the service should reply");

    let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(
        value["ok"], false,
        "a request without a method must fail: {value}"
    );
    assert_eq!(
        value["request_id"], "r-1",
        "the request id should be echoed back"
    );
}

#[test]
fn an_unsupported_protocol_version_is_reported() {
    // 59/68: protocol versioning. The version is carried so that a mismatched
    // client is told, rather than being given a confusing parse error.
    let service = ServiceProcess::start("protocol-version");

    let reply = service.send_raw(r#"{"request_id":"r-1","method":"ping","protocol_version":9999}"#);

    match reply {
        Ok(reply) => {
            let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
            assert_eq!(
                value["ok"], false,
                "an unsupported version must not be accepted: {value}"
            );
        }
        Err(_) => {
            // Refusing by closing the connection is also a defensible answer for
            // an incompatible peer, so this is accepted as long as it is prompt.
        }
    }
}

#[test]
fn a_request_for_an_unknown_session_reports_not_found_on_every_method() {
    let service = ServiceProcess::start("unknown-session");

    // Every session-scoped method must give the same honest answer.
    for args in [
        vec!["info", "s-missing", "--json"],
        vec!["capture", "s-missing", "--json"],
        vec!["latest", "s-missing", "--json"],
        vec!["frame", "s-missing", "1", "--json"],
        vec!["diff", "s-missing", "1", "2", "--json"],
        vec!["close", "s-missing", "--json"],
    ] {
        let (code, value) = service.run_error(&args);
        assert_eq!(
            code, 17,
            "{args:?} should report session_not_found, got {value}"
        );
        assert_eq!(value["error"]["code"], "session_not_found");
    }
}

#[test]
fn an_unreachable_service_is_reported_as_unavailable() {
    // 68: the client must distinguish "no service" from "the service said no".
    let (code, value) = run_json_stderr(&[
        "ping",
        "--socket",
        "/nonexistent/eensh-does-not-exist.sock",
        "--json",
    ]);
    assert_eq!(code, 22, "expected service_unavailable, got {value}");
    assert_eq!(value["error"]["code"], "service_unavailable");
}

#[test]
fn invalid_arguments_are_rejected_before_the_service_is_contacted() {
    let service = ServiceProcess::start("invalid-args");

    // A frame identifier must be at least 1. This is rejected by argument
    // parsing, so it never becomes a request at all.
    let (code, stdout, stderr) = service.run(&["frame", "s-1", "0", "--json"]);
    assert_eq!(
        code, 2,
        "expected invalid_arguments; stdout={stdout} stderr={stderr}"
    );

    // An invalid duration is likewise refused locally.
    let (code, _, _) = service.run(&["wait-change", "s-1", "--json", "--timeout", "5"]);
    assert_eq!(code, 2, "a bare duration should be refused");
}

// ============================================================================
// 66. History + observation: an observation longer than the history
// ============================================================================

#[test]
fn an_observation_outlives_a_history_that_evicts_its_baseline() {
    // 66: this is the distinction between an operation-owned frame reference and
    // public bounded history. A long observation captures far more frames than
    // the history retains; the operation must still succeed, its working baseline
    // must stay valid internally, and the evicted frames must become explicitly
    // unavailable rather than being silently substituted.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("observe-eviction", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    // Capacity 2, so the observation will certainly evict its own baseline.
    let session_id = create_session(&service, &display, &["--history", "2"]);

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

    // Paint after the observation begins and keep painting, so many frames are
    // captured and nothing settles until the end.
    let painter = Painter::start(
        display.clone(),
        vec![
            (
                Duration::from_millis(300),
                vec![paint_rect(20, 20, [255, 0, 0])],
            ),
            (
                Duration::from_millis(700),
                vec![paint_rect(140, 20, [0, 255, 0])],
            ),
            (
                Duration::from_millis(1100),
                vec![paint_rect(20, 160, [0, 0, 255])],
            ),
        ],
    );

    let (code, value) = service.run_json(&[
        "observe",
        &session_id,
        "--json",
        "--timeout",
        "4s",
        "--stable-for",
        "200ms",
        "--interval",
        "40ms",
    ]);
    assert_eq!(code, 0, "a long observation should still succeed: {value}");
    drop(painter);

    let summary = &value["observation"]["observation"];
    assert_eq!(
        summary["result"], "observed",
        "expected a transition: {value}"
    );
    assert!(
        summary["captures"].as_u64().unwrap() > 2,
        "the observation must have captured more frames than the history retains"
    );

    // The baseline is reported by identifier even though it is no longer retained.
    let baseline = value["observation"]["frames"]["baseline"]
        .as_u64()
        .expect("the baseline identifier should be reported");

    // Public history evicted it, and retrieval says so explicitly rather than
    // returning a different frame.
    let (code, value) = service.run_error(&["frame", &session_id, &baseline.to_string(), "--json"]);
    assert_eq!(
        code, 20,
        "an evicted frame must be explicitly unavailable, got {value}"
    );
    assert_eq!(value["error"]["code"], "frame_not_available");

    let (code, value) = service.run_json(&["info", &session_id, "--json"]);
    assert_eq!(code, 0);
    // `SessionInfo.history` *is* the summary, so the path is `info.history`.
    let history = &value["info"]["history"];
    assert!(
        history["retained"].as_u64().unwrap() <= 2,
        "public history must stay bounded: {value}"
    );
    assert!(
        history["captured_total"].as_u64().unwrap() > 2,
        "the total captured count should exceed what is retained: {value}"
    );
}
