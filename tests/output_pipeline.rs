//! End-to-end tests for the agent-facing output contract: JSON shape, base64
//! payloads, file writing, and the stdout/stderr split.

mod common;

use common::*;

const SCREEN_W: u32 = 320;
const SCREEN_H: u32 = 240;

fn skip_if_no_xvfb() -> bool {
    if find_xvfb().is_none() {
        assert!(!xvfb_required(), "Xvfb is required but missing");
        return true;
    }
    false
}

#[test]
fn the_agent_facing_invocation_returns_a_decodable_jpeg_with_full_metadata() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let (code, stdout, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--width",
        "160",
        "--format",
        "jpeg",
        "--quality",
        "75",
        "--base64",
        "--json",
    ]);
    assert_eq!(code, 0, "capture failed: {stderr}");

    let response: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout should be JSON");

    // Source geometry.
    assert_eq!(response["source"]["width"], SCREEN_W);
    assert_eq!(response["source"]["height"], SCREEN_H);

    // Returned image geometry, distinct from the source.
    assert_eq!(response["image"]["width"], 160);
    assert_eq!(response["image"]["height"], 120);
    assert_eq!(response["image"]["media_type"], "image/jpeg");
    assert_eq!(response["image"]["encoding"], "base64");
    assert_eq!(response["image"]["quality"], 75);

    // Coordinate mapping.
    assert_eq!(response["transform"]["origin"], "top-left");
    assert_eq!(response["transform"]["scale_x"], 2.0);
    assert_eq!(response["transform"]["scale_y"], 2.0);

    // Timing for every stage.
    for field in [
        "capture_us",
        "resize_us",
        "encode_us",
        "base64_us",
        "total_us",
    ] {
        assert!(
            response["timing"][field].is_u64(),
            "timing.{field} should be present"
        );
    }

    // The base64 payload must decode to a real JPEG of the declared size.
    let data = response["image"]["data"].as_str().unwrap();
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
        .expect("the payload must be valid base64");

    assert_eq!(&bytes[..2], &[0xff, 0xd8], "payload should be a JPEG");
    assert_eq!(
        bytes.len(),
        response["image"]["byte_length"].as_u64().unwrap() as usize,
        "declared byte length must match the payload"
    );

    let decoded = decode_jpeg(&bytes);
    assert_eq!((decoded.width, decoded.height), (160, 120));
}

#[test]
fn base64_payload_decodes_to_the_same_bytes_that_are_written_to_a_file() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let path = temp_file("base64-vs-file", "file.png");

    // First, write the image to a file.
    let (code, _, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--format",
        "png",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "capture failed: {stderr}");
    let file_bytes = std::fs::read(&path).unwrap();

    let (code, stdout, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--format",
        "png",
        "--base64",
        "--json",
    ]);
    assert_eq!(code, 0, "capture failed: {stderr}");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let payload = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        response["image"]["data"].as_str().unwrap(),
    )
    .unwrap();

    // A static screen must encode identically, so the two byte streams match.
    assert_eq!(
        payload, file_bytes,
        "base64 payload should be the encoded image, byte for byte"
    );
}

#[test]
fn writing_to_a_file_keeps_stdout_empty() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let path = temp_file("file-only", "shot.png");
    let (code, stdout, stderr) =
        run_eensh_text(&["capture", "--display", &display, path.to_str().unwrap()]);

    assert_eq!(code, 0, "capture failed: {stderr}");
    assert!(stdout.is_empty(), "stdout should be empty: {stdout:?}");
    assert!(stderr.is_empty(), "stderr should be empty: {stderr:?}");
    assert!(path.exists(), "the output file should exist");

    let image = decode_png(&std::fs::read(&path).unwrap());
    assert_eq!((image.width, image.height), (SCREEN_W, SCREEN_H));
}

#[test]
fn json_with_a_file_output_reports_metadata_on_stdout() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let path = temp_file("json-with-file", "shot.jpg");
    let (code, stdout, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--format",
        "jpeg",
        "--json",
        path.to_str().unwrap(),
    ]);

    assert_eq!(code, 0, "capture failed: {stderr}");
    assert!(path.exists());
    // With the image in a file, stdout carries the JSON and nothing else.
    let response: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout should be JSON");
    assert_eq!(response["image"]["media_type"], "image/jpeg");
    assert!(
        response["image"].get("data").is_none(),
        "no --base64 was requested"
    );
}

#[test]
fn json_with_binary_stdout_moves_metadata_to_stderr_and_keeps_stdout_clean() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let output = run_eensh(&[
        "capture",
        "--display",
        &display,
        "--format",
        "png",
        "--json",
        "-",
    ]);
    assert!(output.status.success());

    // stdout is a clean image stream, with no JSON mixed in.
    assert_eq!(&output.stdout[..8], b"\x89PNG\r\n\x1a\n");
    let image = decode_png(&output.stdout);
    assert_eq!((image.width, image.height), (SCREEN_W, SCREEN_H));

    // The metadata went to stderr instead.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let response: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("stderr should be JSON");
    assert_eq!(response["image"]["width"], SCREEN_W);
}

#[test]
fn the_format_follows_the_output_file_extension() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let jpeg_path = temp_file("extension", "inferred.jpg");
    let (code, _, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        jpeg_path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "capture failed: {stderr}");

    let bytes = std::fs::read(&jpeg_path).unwrap();
    assert_eq!(&bytes[..2], &[0xff, 0xd8], "the file should be a JPEG");
    let image = decode_jpeg(&bytes);
    assert_eq!((image.width, image.height), (SCREEN_W, SCREEN_H));

    let png_path = temp_file("extension", "inferred.png");
    let (code, _, stderr) =
        run_eensh_text(&["capture", "--display", &display, png_path.to_str().unwrap()]);
    assert_eq!(code, 0, "capture failed: {stderr}");

    let bytes = std::fs::read(&png_path).unwrap();
    assert_eq!(
        &bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "the file should be a PNG"
    );
}

#[test]
fn writing_to_an_unwritable_path_is_an_output_error() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let (code, stdout, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--format",
        "png",
        "--json",
        "/no/such/directory/shot.png",
    ]);

    assert_eq!(code, 9, "expected the output_failed exit status");
    assert!(stdout.trim().is_empty());

    // With --json, the failure is structured.
    let error: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("stderr should be JSON");
    assert_eq!(error["error"]["code"], "output_failed");
}

#[test]
fn a_partial_write_never_replaces_an_existing_valid_file() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let path = temp_file("atomic", "shot.png");
    std::fs::write(&path, b"previous contents that must survive a failure").unwrap();
    let before = std::fs::read(&path).unwrap();

    // A region that is out of bounds fails before anything is written.
    let (code, _, _) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--region",
        "10000,10000,10,10",
        path.to_str().unwrap(),
    ]);
    assert_ne!(code, 0);

    let after = std::fs::read(&path).unwrap();
    assert_eq!(
        before, after,
        "a failed capture must not disturb the old file"
    );
}

#[test]
fn timing_metadata_can_be_printed_without_json() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let path = temp_file("timing", "shot.png");
    let (code, stdout, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--time",
        path.to_str().unwrap(),
    ]);

    assert_eq!(code, 0, "capture failed: {stderr}");
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("eensh timing:") && stderr.contains("encode="),
        "timings should be printed to stderr, got: {stderr:?}"
    );
}
