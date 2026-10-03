//! Command line behaviour that must hold regardless of whether a display exists:
//! argument validation, exit statuses, and error reporting.

mod common;

use common::run_eensh_text;

#[test]
fn help_lists_the_capture_command_and_its_options() {
    let (code, stdout, _stderr) = run_eensh_text(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("capture"));
    assert!(stdout.contains("Capture an image"));
}

#[test]
fn version_is_reported() {
    let (code, stdout, _stderr) = run_eensh_text(&["--version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("eensh"));
}

#[test]
fn capture_help_documents_every_phase_1_option() {
    let (code, stdout, _stderr) = run_eensh_text(&["capture", "--help"]);
    assert_eq!(code, 0);
    for option in [
        "--display",
        "--region",
        "--window",
        "--format",
        "--quality",
        "--width",
        "--height",
        "--scale",
        "--base64",
        "--json",
    ] {
        assert!(stdout.contains(option), "help is missing {option}");
    }
}

#[test]
fn no_arguments_prints_usage_and_fails() {
    let (code, _stdout, stderr) = run_eensh_text(&[]);
    assert_ne!(code, 0);
    assert!(stderr.contains("Usage") || stderr.contains("usage"));
}

#[test]
fn capture_without_any_configuration_fails_rather_than_writing_an_empty_image() {
    // With no DISPLAY in the environment and no --display, resolution must fail
    // before anything is written to stdout.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_eensh"))
        .args(["capture"])
        .env_remove("DISPLAY")
        .output()
        .expect("failed to run eensh");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty(), "no image should be written");
}

#[test]
fn a_missing_display_is_reported_as_an_argument_error_when_nothing_is_configured() {
    // Run with DISPLAY explicitly cleared so the environment cannot supply one.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_eensh"))
        .args(["capture", "--json"])
        .env_remove("DISPLAY")
        .output()
        .expect("failed to run eensh");

    assert_eq!(output.status.code(), Some(2), "expected invalid_arguments");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let error: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("stderr should be JSON");
    assert_eq!(error["error"]["code"], "invalid_arguments");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("DISPLAY"));
}

#[test]
fn an_empty_display_value_is_rejected() {
    let (code, _stdout, stderr) = run_eensh_text(&["capture", "--display", "", "--json"]);
    assert_eq!(code, 2, "expected invalid_arguments");
    let error: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(error["error"]["code"], "invalid_arguments");
}

#[test]
fn mutually_exclusive_targets_are_rejected_by_the_parser() {
    let (code, stdout, _stderr) =
        run_eensh_text(&["capture", "--region", "0,0,10,10", "--window", "0x1"]);
    assert_ne!(code, 0);
    assert!(stdout.is_empty());
}

#[test]
fn conflicting_resize_options_are_rejected() {
    let (code, _, _) = run_eensh_text(&["capture", "--width", "100", "--scale", "0.5"]);
    assert_ne!(code, 0);

    let (code, _, _) = run_eensh_text(&["capture", "--scale", "0.5", "--height", "100"]);
    assert_ne!(code, 0);
}

#[test]
fn width_and_height_together_are_rejected_with_a_structured_error() {
    let (code, _, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        ":54321",
        "--width",
        "100",
        "--height",
        "50",
        "--json",
    ]);
    assert_eq!(code, 2, "expected invalid_arguments");
    let error: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(error["error"]["code"], "invalid_arguments");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("proportional"));
}

#[test]
fn base64_without_json_is_rejected_with_a_helpful_message() {
    let (code, _, _stderr) =
        run_eensh_text(&["capture", "--display", ":54321", "--base64", "--json"]);
    // Sanity check that this combination is actually accepted when --json is set.
    assert_ne!(code, 2, "base64 plus json should be a valid combination");

    // Without --json the failure is reported as plain text, since JSON was not
    // requested.
    let (code, stdout, stderr) = run_eensh_text(&["capture", "--display", ":54321", "--base64"]);
    assert_eq!(code, 2, "base64 without json must be rejected");
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("--json"),
        "the message should explain what is missing, got: {stderr:?}"
    );
}

#[test]
fn quality_with_png_output_is_rejected() {
    let (code, _, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        ":54321",
        "--quality",
        "75",
        "--json",
    ]);
    assert_eq!(code, 2, "expected invalid_arguments");
    let error: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert!(error["error"]["message"].as_str().unwrap().contains("JPEG"));
}

#[test]
fn compression_with_jpeg_output_is_rejected() {
    let (code, _, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        ":54321",
        "--format",
        "jpeg",
        "--compression",
        "best",
        "--json",
    ]);
    assert_eq!(code, 2);
    let error: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert!(error["error"]["message"].as_str().unwrap().contains("PNG"));
}

#[test]
fn an_unknown_format_is_rejected() {
    let (code, _, _) = run_eensh_text(&["capture", "--display", ":54321", "--format", "webp"]);
    assert_ne!(code, 0);
}

#[test]
fn quality_outside_the_valid_range_is_rejected() {
    for bad in ["0", "101", "-5"] {
        let (code, _, _) = run_eensh_text(&[
            "capture",
            "--display",
            ":54321",
            "--format",
            "jpeg",
            "--quality",
            bad,
        ]);
        assert_ne!(code, 0, "--quality {bad} should be rejected");
    }
}

#[test]
fn malformed_regions_are_rejected() {
    for bad in ["1,2,3", "1,2,3,4,5", "a,b,c,d", "1,2,0,4", "1,2,4,0"] {
        let (code, stdout, _) = run_eensh_text(&["capture", "--region", bad]);
        assert_ne!(code, 0, "--region {bad} should be rejected");
        assert!(stdout.is_empty());
    }
}

#[test]
fn malformed_window_ids_are_rejected() {
    for bad in ["0x", "zzz", "0"] {
        let (code, stdout, _) = run_eensh_text(&["capture", "--window", bad]);
        assert_ne!(code, 0, "--window {bad} should be rejected");
        assert!(stdout.is_empty());
    }
}

#[test]
fn errors_without_json_still_produce_a_readable_message() {
    let (code, stdout, stderr) = run_eensh_text(&["capture", "--display", ":54321"]);
    assert_eq!(code, 3);
    assert!(stdout.is_empty());
    assert!(!stderr.trim().is_empty());
    assert!(
        !stderr.trim_start().starts_with('{'),
        "without --json the error should be plain text, got: {stderr:?}"
    );
    assert!(stderr.contains(":54321"));
}

#[test]
fn display_selection_falls_back_to_the_environment() {
    // A display that cannot exist, supplied through the environment, must be
    // picked up and reported as unavailable.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_eensh"))
        .args(["capture", "--json"])
        .env("DISPLAY", ":54321")
        .output()
        .expect("failed to run eensh");

    assert_eq!(
        output.status.code(),
        Some(3),
        "expected display_unavailable"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let error: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(error["error"]["code"], "display_unavailable");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains(":54321"));
}

#[test]
fn the_display_flag_overrides_the_environment() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_eensh"))
        .args(["capture", "--display", ":54322", "--json"])
        .env("DISPLAY", ":54321")
        .output()
        .expect("failed to run eensh");

    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    let error: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    // The error must mention the display from the flag, not the environment.
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains(":54322"));
}
