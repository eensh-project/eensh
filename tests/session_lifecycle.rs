//! Phase 4 requirements 47, 48, 49, 50, and 69: target loss, restart, and
//! cleanup.
//!
//! These are the failure paths, which are easy to leave untested because they
//! require deliberately breaking something. Each test here destroys a window,
//! restarts a service, or checks that nothing was left behind, because those are
//! the situations where a session's lifecycle rules actually matter.

mod common;

use std::time::Duration;

use common::*;

const SCREEN_W: u32 = 320;
const SCREEN_H: u32 = 240;

// ============================================================================
// 48. Window destruction: target_lost, not window_not_found
// ============================================================================

#[test]
fn a_window_destroyed_after_session_creation_reports_target_lost() {
    // The distinction Phase 4 can make and Phase 3 could not: a window that
    // existed when the session was created and has since disappeared is
    // `target_lost`, whereas a window that never existed is `window_not_found`.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("window-lost", SCREEN_W, SCREEN_H) else {
        return;
    };

    // Create a real window and keep the connection that owns it.
    let screen = Screen::open(&display);
    let window = screen.create_window(120, 90, 20, 20);
    screen.map(window);

    // A session bound to that window resolves its geometry at creation, which is
    // what makes the later disappearance distinguishable from a bad ID.
    let (code, value) = service.run_json(&[
        "create",
        "--display",
        &display,
        "--window",
        &format!("0x{window:x}"),
        "--json",
    ]);
    assert_eq!(code, 0, "session creation should succeed: {value}");
    let session_id = value["session_id"].as_str().unwrap().to_string();
    assert_eq!(
        value["info"]["target"]["kind"], "window",
        "the session should be bound to the window: {value}"
    );

    // A capture before destruction works, proving the session is genuinely bound.
    let (code, value) = service.run_json(&["capture", &session_id, "--json"]);
    assert_eq!(code, 0, "a capture before destruction should work: {value}");

    // Destroy the window, then capture again.
    screen.destroy(window);
    std::thread::sleep(Duration::from_millis(100));

    let (code, value) = service.run_error(&["capture", &session_id, "--json"]);
    assert_eq!(
        code, 14,
        "a destroyed window target should report target_lost, got {value}"
    );
    assert_eq!(
        value["error"]["code"], "target_lost",
        "the richer error is available because the session knew the window"
    );

    // Requirement 47/48: the session is failed and stays failed rather than
    // silently recovering, and it is not transparently re-pointed at anything.
    //
    // `info` still succeeds: it is read-only, and being able to ask why a session
    // failed is the reason the reason is retained at all.
    let (code, value) = service.run_json(&["info", &session_id, "--json"]);
    assert_eq!(code, 0, "the session should still be describable: {value}");
    assert_eq!(
        value["info"]["state"], "failed",
        "a lost target should fail the session: {value}"
    );
    assert!(
        value["info"]["failure"].is_string(),
        "the failure should be retained for diagnosis: {value}"
    );

    // Future operations are refused with the same terminal reason.
    let (code, value) = service.run_error(&["capture", &session_id, "--json"]);
    assert_eq!(code, 14, "a failed session should keep refusing: {value}");
}

#[test]
fn a_window_that_never_existed_reports_window_not_found_at_creation() {
    // The other half of the distinction. Creation resolves the target eagerly, so
    // a window that does not exist fails there, as `window_not_found`.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("window-absent", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let (code, value) =
        service.run_error(&["create", "--display", &display, "--window", "0x1", "--json"]);
    assert_eq!(
        code, 5,
        "an absent window should be window_not_found: {value}"
    );
    assert_eq!(value["error"]["code"], "window_not_found");

    // Nothing was registered, so nothing leaked from the failed creation.
    let (code, value) = service.run_json(&["list", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(
        value["sessions"].as_array().unwrap().len(),
        0,
        "a failed creation must not register a session: {value}"
    );
}

// ============================================================================
// 47. Display loss fails the session and requires recreation
// ============================================================================

#[test]
fn losing_the_display_fails_the_session_and_requires_recreation() {
    let _guard = serial();
    if find_xvfb().is_none() {
        assert!(!xvfb_required(), "Xvfb is required but missing");
        return;
    }

    let service = ServiceProcess::start("display-loss");

    // A display that exists when the session is created.
    let (server, display) = xvfb(SCREEN_W, SCREEN_H).unwrap();

    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0, "session creation should succeed: {value}");
    let session_id = value["session_id"].as_str().unwrap().to_string();

    let (code, value) = service.run_json(&["capture", &session_id, "--json"]);
    assert_eq!(
        code, 0,
        "a capture should work while the display lives: {value}"
    );

    // Take the display away.
    drop(server);
    std::thread::sleep(Duration::from_millis(300));

    // Phase 4 must not silently reconnect. The next operation reports a
    // structured, terminal error.
    let (code, value) = service.run_error(&["capture", &session_id, "--json"]);
    assert_eq!(
        code, 14,
        "a lost display should report target_lost, got {value}"
    );
    assert_eq!(value["error"]["code"], "target_lost");

    // Requirement 47: the session is marked failed, the reason is retained, and
    // future operations are refused rather than retried at the real server.
    //
    // `info` is read-only and is deliberately *not* gated on the session being
    // usable: the whole point of retaining the failure reason is that a caller can
    // ask why a session died. It therefore still succeeds, on stdout.
    let (code, value) = service.run_json(&["info", &session_id, "--json"]);
    assert_eq!(
        code, 0,
        "the failed session should still be describable: {value}"
    );
    assert_eq!(
        value["info"]["state"], "failed",
        "a lost display should fail the session: {value}"
    );
    assert!(
        value["info"]["failure"].is_string(),
        "the reason should be retained for diagnosis: {value}"
    );

    let (code, value) = service.run_error(&["capture", &session_id, "--json"]);
    assert_eq!(
        code, 14,
        "a failed session should keep refusing rather than recover: {value}"
    );

    // Recreation is the documented remedy, and it must be possible.
    let (server, display) = xvfb(SCREEN_W, SCREEN_H).unwrap();
    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(
        code, 0,
        "a new session on a fresh display should work: {value}"
    );
    let replacement = value["session_id"].as_str().unwrap().to_string();
    assert_ne!(
        replacement, session_id,
        "the replacement must be a new session, not the old one revived"
    );

    let (code, value) = service.run_json(&["capture", &replacement, "--json"]);
    assert_eq!(code, 0, "the replacement session should capture: {value}");
    drop(server);
}

// ============================================================================
// 49. Service restart invalidates session IDs
// ============================================================================

#[test]
fn a_service_restart_invalidates_previous_session_ids() {
    // 49: session IDs are process-lifetime objects, and no attempt is made to
    // restore them.
    let _guard = serial();
    let Some((_server, display, _service)) = xvfb_service("restart", SCREEN_W, SCREEN_H) else {
        return;
    };

    // Use a dedicated service on a known path so it can be restarted there.
    let directory = temp_dir("restart-socket");
    let socket = directory.join("eensh.sock");
    let _ = std::fs::remove_file(&socket);

    let first = ServiceProcess::start_at(&socket);
    let (code, value) = first.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0);
    let session_id = value["session_id"].as_str().unwrap().to_string();

    let (code, _) = first.run_json(&["capture", &session_id, "--json"]);
    assert_eq!(code, 0);

    // Stop the service, which also removes its socket.
    drop(first);
    assert!(
        !socket.exists(),
        "the first service should have removed its socket"
    );

    // Start a fresh service on the same path.
    let second = ServiceProcess::start_at(&socket);

    // The old identifier must be gone, and the answer must be the documented one
    // so a client knows to create a new session.
    let (code, value) = second.run_error(&["info", &session_id, "--json"]);
    assert_eq!(
        code, 17,
        "a session ID should not survive a service restart: {value}"
    );
    assert_eq!(value["error"]["code"], "session_not_found");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("restarted"),
        "the message should hint at the restart case: {value}"
    );

    // The restarted service holds nothing, which also confirms nothing was
    // persisted to disk.
    let (code, value) = second.run_json(&["list", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(
        value["sessions"].as_array().unwrap().len(),
        0,
        "a restarted service should start empty: {value}"
    );
}

// ============================================================================
// 50. No disk persistence
// ============================================================================

#[test]
fn nothing_is_written_to_disk_for_history() {
    // 50: history is in memory only. This checks the session's own temporary
    // directory, which is the natural place a careless implementation would have
    // put frames.
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("no-disk", SCREEN_W, SCREEN_H) else {
        return;
    };

    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0);
    let session_id = value["session_id"].as_str().unwrap().to_string();

    for _ in 0..4 {
        let (code, _) = service.run_json(&["capture", &session_id, "--json"]);
        assert_eq!(code, 0);
    }

    // The socket directory is created by the harness specifically for this
    // service, so anything other than the socket in it would be a frame on disk.
    let directory = service.socket().parent().unwrap();
    let entries: Vec<String> = std::fs::read_dir(directory)
        .expect("the socket directory should be readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();

    let unexpected: Vec<&String> = entries
        .iter()
        .filter(|name| *name != "eensh.sock")
        .collect();

    assert!(
        unexpected.is_empty(),
        "history must not be written to disk, found: {unexpected:?}"
    );
}

// ============================================================================
// 69. Cleanup: no leaked processes, sockets, or locks
// ============================================================================

#[test]
fn the_service_leaves_no_socket_and_no_session_behind() {
    let _guard = serial();
    let Some((_server, display, _service)) = xvfb_service("cleanup", SCREEN_W, SCREEN_H) else {
        return;
    };

    let directory = temp_dir("cleanup-socket");
    let socket = directory.join("eensh.sock");
    let _ = std::fs::remove_file(&socket);

    {
        let service = ServiceProcess::start_at(&socket);
        let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
        assert_eq!(code, 0);
        let session_id = value["session_id"].as_str().unwrap().to_string();
        service.run_json(&["capture", &session_id, "--json"]);

        // Dropping asks the service to stop with SIGTERM and waits for it.
    }

    assert!(
        !socket.exists(),
        "the socket should be removed when the service stops"
    );
}

#[test]
fn a_session_closed_explicitly_releases_its_display() {
    let _guard = serial();
    let Some((_server, display, service)) = xvfb_service("close-releases", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0);
    let session_id = value["session_id"].as_str().unwrap().to_string();
    assert_eq!(value["info"]["state"], "ready");

    let (code, _) = service.run_json(&["close", &session_id, "--json"]);
    assert_eq!(code, 0);

    // The session is gone from the registry, so its display and its history went
    // with it rather than being retained against a name nobody can use.
    let (code, value) = service.run_error(&["info", &session_id, "--json"]);
    assert_eq!(code, 17, "a closed session should be unregistered: {value}");

    let (code, value) = service.run_json(&["list", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(
        value["sessions"].as_array().unwrap().len(),
        0,
        "no session should remain after the only one was closed: {value}"
    );
}
