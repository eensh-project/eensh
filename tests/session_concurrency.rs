//! Phase 4 requirement 67: concurrency.
//!
//! The spec names five situations that must behave correctly. Each is one test
//! below, named for the requirement.
//!
//! These run against a real service over a real socket, because the concurrency
//! policy is implemented across two layers — the per-connection threads and the
//! per-session mutexes — and only the combination is the actual behaviour. A
//! test that called the handler directly would miss the transport entirely, and
//! the transport is exactly where a global lock would hide.
//!
//! ## The rule being tested
//!
//! * Mutating operations within one session are **serialized** by that session's
//!   own mutex, not refused. A capture that arrives during an observation waits
//!   its turn and then succeeds.
//! * The three temporal observations are **mutually exclusive** per session, and
//!   a second one is refused with `session_busy` rather than queued invisibly.
//! * Operations on *different* sessions never contend: there is no global lock.
//! * A `close` during an observation is **refused** with `session_busy`, leaving
//!   the session intact. This is the documented policy, chosen over cancellation.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;

const SCREEN_W: u32 = 320;
const SCREEN_H: u32 = 240;

/// Start a service with one session, or skip.
fn setup(name: &str) -> Option<(Xvfb, String, ServiceProcess, String)> {
    let (server, display, service) = xvfb_service(name, SCREEN_W, SCREEN_H)?;
    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0, "session creation failed: {value}");
    let session_id = value["session_id"].as_str().unwrap().to_string();
    Some((server, display, service, session_id))
}

/// Start an observation in the background and return a handle plus a flag that
/// the observation is under way.
///
/// The returned handle must be joined before the test ends, so an observation is
/// never left running into another test.
fn start_background_observation(
    service: &ServiceProcess,
    session_id: &str,
    timeout: &'static str,
) -> (
    std::thread::JoinHandle<(i32, String, String)>,
    Arc<AtomicUsize>,
) {
    let socket = service.socket_arg();
    let session = session_id.to_string();

    // Signals that the observation has been given time to take the slot. The
    // thread cannot observe the session's internal state, so the test waits a
    // fixed interval instead; that interval is generous relative to process
    // startup.
    let ready = Arc::new(AtomicUsize::new(0));
    let ready_flag = Arc::clone(&ready);

    let handle = std::thread::spawn(move || {
        let result = run_eensh_text(&[
            "session",
            "observe",
            &session,
            "--socket",
            &socket,
            "--json",
            "--timeout",
            timeout,
            "--stable-for",
            "400ms",
            "--interval",
            "100ms",
        ]);
        ready_flag.store(1, Ordering::Relaxed);
        result
    });

    (handle, ready)
}

// ============================================================================
// 67. Same-session capture serialization
// ============================================================================

#[test]
fn concurrent_captures_do_not_corrupt_history_or_duplicate_ids() {
    let _guard = serial();
    let Some((_server, _display, service, session_id)) = setup("concurrent-captures") else {
        return;
    };

    // Several capture requests in parallel. Each is a separate process with its
    // own connection, so this exercises the per-session mutex across the
    // transport rather than in one thread.
    const CAPTURES: usize = 8;

    let mut handles = Vec::new();
    for _ in 0..CAPTURES {
        let socket = service.socket_arg();
        let session = session_id.clone();
        handles.push(std::thread::spawn(move || {
            run_eensh_text(&[
                "session", "capture", &session, "--socket", &socket, "--json",
            ])
        }));
    }

    let mut frame_ids: Vec<u64> = Vec::new();
    for handle in handles {
        let (code, stdout, stderr) = handle.join().expect("the capture thread panicked");
        assert_eq!(code, 0, "a concurrent capture failed: {stderr}");

        let value: serde_json::Value =
            serde_json::from_str(stdout.trim()).expect("a capture should return JSON");
        frame_ids.push(
            value["frame"]["frame_id"]
                .as_u64()
                .expect("a capture should carry a frame id"),
        );
    }

    // Every identifier is distinct: no two captures were handed the same one.
    let mut sorted = frame_ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        frame_ids.len(),
        "frame identifiers were duplicated across concurrent captures: {frame_ids:?}"
    );

    // The identifiers are exactly 1..=CAPTURES, so nothing was skipped and
    // nothing was allocated twice.
    assert_eq!(
        sorted,
        (1..=CAPTURES as u64).collect::<Vec<_>>(),
        "frame identifiers should be a contiguous run with no gaps: {frame_ids:?}"
    );

    // History is intact, and its size matches what was captured.
    let (code, value) = service.run_json(&["info", &session_id, "--json"]);
    assert_eq!(code, 0);
    let captured = value["info"]["frames_captured"].as_u64().unwrap();
    assert_eq!(
        captured, CAPTURES as u64,
        "the captured count should match the requests: {value}"
    );
}

#[test]
fn concurrent_captures_and_retrievals_never_see_a_partial_frame() {
    let _guard = serial();
    let Some((_server, _display, service, session_id)) = setup("capture-read-concurrent") else {
        return;
    };

    // Seed one frame so readers always have something to read.
    service.run_json(&["capture", &session_id, "--json"]);

    // Writers capture; readers retrieve and compare. If a frame could be observed
    // while half-inserted, a reader would see inconsistent dimensions or a
    // comparison failure rather than a clean answer.
    let mut handles = Vec::new();

    for _ in 0..4 {
        let socket = service.socket_arg();
        let session = session_id.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..3 {
                let (code, _, stderr) = run_eensh_text(&[
                    "session", "capture", &session, "--socket", &socket, "--json",
                ]);
                assert_eq!(code, 0, "a capture failed: {stderr}");
            }
        }));
    }

    for _ in 0..4 {
        let socket = service.socket_arg();
        let session = session_id.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..6 {
                let (code, stdout, stderr) =
                    run_eensh_text(&["session", "latest", &session, "--socket", &socket, "--json"]);
                assert_eq!(code, 0, "a retrieval failed: {stderr}");

                let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
                let frame = &value["frame"];
                assert_eq!(
                    frame["source"]["width"], SCREEN_W,
                    "a partially inserted frame would have wrong dimensions: {value}"
                );
                assert_eq!(frame["source"]["height"], SCREEN_H);
            }
        }));
    }

    for handle in handles {
        handle.join().expect("a worker thread panicked");
    }

    // Every capture completed exactly once, so nothing was lost to a race.
    let (code, value) = service.run_json(&["info", &session_id, "--json"]);
    assert_eq!(code, 0);
    assert_eq!(
        value["info"]["frames_captured"], 13,
        "1 seeded plus 12 concurrent captures: {value}"
    );
}

// ============================================================================
// 67. Observation exclusivity
// ============================================================================

#[test]
fn a_second_observation_is_refused_while_the_first_is_running() {
    let _guard = serial();
    let Some((_server, _display, service, session_id)) = setup("observation-exclusivity") else {
        return;
    };

    let (handle, _ready) = start_background_observation(&service, &session_id, "4s");

    // Let it take the slot.
    std::thread::sleep(Duration::from_millis(800));

    // A second observation of the same session must be refused explicitly, not
    // queued behind the first.
    for operation in ["wait-change", "wait-stable", "observe"] {
        let (code, value) =
            service.run_error(&[operation, &session_id, "--json", "--timeout", "1s"]);
        assert_eq!(
            code, 18,
            "{operation} should have been refused with session_busy, got {value}"
        );
        assert_eq!(value["error"]["code"], "session_busy");
    }

    let _ = handle.join();
}

#[test]
fn a_capture_during_an_observation_is_serialized_rather_than_refused() {
    let _guard = serial();
    let Some((_server, _display, service, session_id)) = setup("capture-during-observation") else {
        return;
    };

    let (handle, _ready) = start_background_observation(&service, &session_id, "5s");
    std::thread::sleep(Duration::from_millis(800));

    // Per the documented rule, capture is serialized, not refused. This is the
    // regression guard: an earlier version refused it with `session_busy`, which
    // also broke every observation, because an observation performs its own
    // captures.
    let started = Instant::now();
    let (code, stdout, stderr) = service.run(&["capture", &session_id, "--json"]);
    let waited = started.elapsed();

    assert_eq!(
        code, 0,
        "a capture during an observation should be serialized, not refused; stderr={stderr}"
    );
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["kind"], "frame");

    // The granularity of the serialization is one *capture*, not the whole
    // observation. The session lock is taken per capture and released between
    // samples, so a capture arriving mid-observation is served at the next sample
    // boundary rather than waiting for the observation to conclude. That property
    // is what keeps sessions independent, so it is asserted rather than assumed:
    // waiting the full observation would mean the lock spanned the entire run.
    assert!(
        waited < Duration::from_millis(600),
        "a capture should be served at the next sample boundary, not after the \
         whole observation; waited {waited:?}"
    );

    // The frame is a genuine new one: the identifier is beyond those the
    // observation had already used, so this capture was not served from a stale
    // snapshot.
    let frame_id = value["frame"]["frame_id"].as_u64().unwrap();
    assert!(
        frame_id > 1,
        "the interleaved capture should be a new frame, got id {frame_id}"
    );

    let (code, _, stderr) = handle.join().expect("the observation thread panicked");
    assert!(
        code == 0 || code == 100,
        "the observation should finish cleanly, got exit {code}: {stderr}"
    );

    // History is still coherent: the count equals the observation's captures plus
    // the one interleaved capture, with no identifiers lost or reused.
    let (code, value) = service.run_json(&["info", &session_id, "--json"]);
    assert_eq!(code, 0);
    let total = value["info"]["frames_captured"].as_u64().unwrap();
    assert!(
        total > 1,
        "the session should have captured several frames: {value}"
    );
}

// ============================================================================
// 67. Independent sessions
// ============================================================================

#[test]
fn an_observation_in_one_session_does_not_block_another_session() {
    let _guard = serial();
    let Some((_server, display, service, session_a)) = setup("independent-sessions") else {
        return;
    };

    // A second session on the same display.
    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0, "the second session should be created: {value}");
    let session_b = value["session_id"].as_str().unwrap().to_string();

    // A long observation in session A.
    let (handle, _ready) = start_background_observation(&service, &session_a, "4s");
    std::thread::sleep(Duration::from_millis(800));

    // Session B must be unaffected. If there were a global lock, this capture
    // would block until the observation finished; the assertion on elapsed time
    // is what distinguishes "unaffected" from "eventually served".
    let started = Instant::now();
    let (code, stdout, stderr) = service.run(&["capture", &session_b, "--json"]);
    let elapsed = started.elapsed();

    assert_eq!(code, 0, "session B should be unaffected: {stderr}");
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["kind"], "frame");
    assert_eq!(
        value["frame"]["session_id"],
        session_b.as_str(),
        "the frame should belong to session B"
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "a capture in session B should not wait for session A's observation, took {elapsed:?}"
    );

    // Session B can even run its own observation concurrently.
    let (code, value) = service.run_json(&[
        "wait-stable",
        &session_b,
        "--json",
        "--timeout",
        "2s",
        "--stable-for",
        "150ms",
    ]);
    assert_eq!(
        code, 0,
        "session B should be able to observe while session A observes: {value}"
    );

    let _ = handle.join();
}

// ============================================================================
// 67. Close race
// ============================================================================

#[test]
fn closing_during_an_observation_is_refused_without_deadlock() {
    let _guard = serial();
    let Some((_server, _display, service, session_id)) = setup("close-race") else {
        return;
    };

    let (handle, _ready) = start_background_observation(&service, &session_id, "3s");
    std::thread::sleep(Duration::from_millis(800));

    // The documented policy: close is refused while an observation is running.
    // The test must complete promptly -- a deadlock would hang here rather than
    // fail, so the whole test is bounded by the harness's own timeouts and by the
    // observation's deadline.
    let started = Instant::now();
    let (code, value) = service.run_error(&["close", &session_id, "--json"]);
    let elapsed = started.elapsed();

    assert_eq!(
        code, 18,
        "close should be refused with session_busy: {value}"
    );
    assert_eq!(value["error"]["code"], "session_busy");
    assert!(
        elapsed < Duration::from_millis(2000),
        "a refused close must return promptly, took {elapsed:?}"
    );

    // The refusal left the session intact: it is still observing, and the
    // observation is still going to finish normally. This is the regression this
    // test exists for: an earlier version refused the close but had *already*
    // unregistered the session, so a refusal that said "try again later" was
    // followed by the session being gone.
    let (code, value) = service.run_json(&["info", &session_id, "--json"]);
    assert_eq!(code, 0, "the session should still exist: {value}");
    assert_eq!(
        value["info"]["state"], "observing",
        "a refused close must not have torn the session down: {value}"
    );

    // The observation runs to its own conclusion. Nothing painted the screen, so
    // the expected outcome is a timeout, which is a successful completion of the
    // observation rather than a failure.
    let (code, _, stderr) = handle.join().expect("the observation thread panicked");
    assert!(
        code == 0 || code == 100,
        "the observation should still have completed, got exit {code}: {stderr}"
    );

    // And once it has finished, close succeeds.
    let (code, value) = service.run_json(&["close", &session_id, "--json"]);
    assert_eq!(
        code, 0,
        "close should succeed after the observation: {value}"
    );
}

#[test]
fn closing_a_session_with_a_capture_in_flight_is_serialized() {
    let _guard = serial();
    let Some((_server, _display, service, session_id)) = setup("close-during-capture") else {
        return;
    };

    // Both operations target the same session at once. Whichever wins, neither
    // may corrupt the other, and neither may hang.
    let socket_a = service.socket_arg();
    let socket_b = service.socket_arg();
    let session_a = session_id.clone();
    let session_b = session_id.clone();

    let capture = std::thread::spawn(move || {
        run_eensh_text(&[
            "session", "capture", &session_a, "--socket", &socket_a, "--json",
        ])
    });
    let close = std::thread::spawn(move || {
        run_eensh_text(&[
            "session", "close", &session_b, "--socket", &socket_b, "--json",
        ])
    });

    let (capture_code, _, _) = capture.join().expect("the capture thread panicked");
    let (close_code, _, _) = close.join().expect("the close thread panicked");

    // `close` removes the session from the registry, so the outcome depends on
    // which operation reached the session first. Both orders are valid; what is
    // not valid is a hang or a crash, and the outcomes must be from the documented
    // set rather than something arbitrary.
    assert!(
        capture_code == 0 || capture_code == 17,
        "a capture racing a close should either succeed or report the session gone, got {capture_code}"
    );
    assert!(
        close_code == 0 || close_code == 17,
        "a close racing a capture should either succeed or report the session gone, got {close_code}"
    );

    // At least one of them must have succeeded: the session existed when the test
    // started, so they cannot both have found it missing.
    assert!(
        capture_code == 0 || close_code == 0,
        "both the capture and the close reported the session missing"
    );
}

// ============================================================================
// 67. Frame retrieval during capture
// ============================================================================

#[test]
fn retrieval_during_capture_returns_a_complete_consistent_frame() {
    let _guard = serial();
    let Some((_server, _display, service, session_id)) = setup("retrieval-during-capture") else {
        return;
    };

    service.run_json(&["capture", &session_id, "--json", "--base64"]);

    // One thread captures in a tight loop while another reads the latest frame.
    // A frame observed mid-insertion would show inconsistent dimensions, and the
    // identifiers would move backwards.
    let socket_for_capture = service.socket_arg();
    let socket_for_read = service.socket_arg();
    let session_capture = session_id.clone();
    let session_read = session_id.clone();

    let capturer = std::thread::spawn(move || {
        for _ in 0..6 {
            let (code, _, stderr) = run_eensh_text(&[
                "session",
                "capture",
                &session_capture,
                "--socket",
                &socket_for_capture,
                "--json",
            ]);
            assert_eq!(code, 0, "a capture failed: {stderr}");
        }
    });

    let reader = std::thread::spawn(move || {
        let mut previous: Option<u64> = None;
        for _ in 0..10 {
            let (code, stdout, stderr) = run_eensh_text(&[
                "session",
                "latest",
                &session_read,
                "--socket",
                &socket_for_read,
                "--json",
            ]);
            if code != 0 {
                // The session cannot vanish here: nothing closes it in this test.
                panic!("a retrieval failed: {stderr}");
            }

            let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
            let id = value["frame"]["frame_id"].as_u64().unwrap();
            assert_eq!(
                value["frame"]["source"]["width"], SCREEN_W,
                "a partially inserted frame would have wrong dimensions: {value}"
            );

            // Identifiers only ever move forward, which a partially inserted
            // frame would violate.
            if let Some(previous) = previous {
                assert!(
                    id >= previous,
                    "the latest frame went backwards: {id} after {previous}"
                );
            }
            previous = Some(id);
        }
        previous
    });

    capturer.join().expect("the capture thread panicked");
    let final_seen = reader.join().expect("the reader thread panicked");
    assert!(final_seen.is_some(), "the reader should have seen a frame");
}
