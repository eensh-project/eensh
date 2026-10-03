//! Phase 2 CLI integration tests: `eensh diff` on image files.
//!
//! These exercise the whole command: decoding at the input boundary, comparing
//! raw frames, the JSON response, the changed crop, threshold parsing, and exit
//! statuses. Fixtures are generated with `eensh capture` itself rather than
//! checked in, so the tests cannot drift from the encoder.

mod common;

use common::*;
use std::process::Command;

/// Write a solid-colour image to a temporary file, using the real encoder.
fn write_solid_image(path: &std::path::Path, width: u32, height: u32, rgb: [u8; 3]) {
    let data: Vec<u8> = rgb
        .iter()
        .copied()
        .cycle()
        .take((width * height * 3) as usize)
        .collect();
    let frame = eensh::input::frame_from_rgb8(width, height, data).unwrap();
    let format = if path.extension().and_then(|e| e.to_str()) == Some("jpg")
        || path.extension().and_then(|e| e.to_str()) == Some("jpeg")
    {
        eensh::encode::ImageFormat::Jpeg
    } else {
        eensh::encode::ImageFormat::Png
    };
    let bytes = eensh::diff::encode_crop(&frame, format).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Write a solid-colour image with one differently-coloured rectangle.
fn write_image_with_patch(
    path: &std::path::Path,
    width: u32,
    height: u32,
    background: [u8; 3],
    patch: (u32, u32, u32, u32),
    patch_colour: [u8; 3],
) {
    let mut data = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let inside =
                x >= patch.0 && x < patch.0 + patch.2 && y >= patch.1 && y < patch.1 + patch.3;
            data.extend_from_slice(if inside { &patch_colour } else { &background });
        }
    }
    let frame = eensh::input::frame_from_rgb8(width, height, data).unwrap();
    let bytes = eensh::diff::encode_crop(&frame, eensh::encode::ImageFormat::Png).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn diffing_a_png_against_itself_reports_no_change() {
    let path = temp_file("diff-same", "same.png");
    write_solid_image(&path, 64, 48, [10, 20, 30]);

    let (code, stdout, stderr) = run_eensh_text(&[
        "diff",
        path.to_str().unwrap(),
        path.to_str().unwrap(),
        "--json",
    ]);

    assert_eq!(code, 0, "a successful comparison must exit zero: {stderr}");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let comparison = &response["comparison"];

    assert_eq!(comparison["changed"], false);
    assert_eq!(comparison["changed_pixels"], 0);
    assert_eq!(comparison["total_pixels"], 64 * 48);
    assert_eq!(comparison["changed_fraction"], 0.0);
    assert!(comparison["bounding_box"].is_null());
    assert!(response["timing"]["compare_us"].is_u64());
}

#[test]
fn diffing_two_pngs_reports_the_changed_rectangle() {
    let before = temp_file("diff-patch", "before.png");
    let after = temp_file("diff-patch", "after.png");

    write_solid_image(&before, 100, 80, [0, 0, 0]);
    write_image_with_patch(&after, 100, 80, [0, 0, 0], (20, 30, 10, 5), [255, 255, 255]);

    let (code, stdout, stderr) = run_eensh_text(&[
        "diff",
        before.to_str().unwrap(),
        after.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code, 0, "comparison failed: {stderr}");

    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let comparison = &response["comparison"];

    assert_eq!(comparison["changed"], true);
    assert_eq!(comparison["changed_pixels"], 50);
    assert_eq!(comparison["total_pixels"], 100 * 80);
    assert_eq!(comparison["bounding_box"]["x"], 20);
    assert_eq!(comparison["bounding_box"]["y"], 30);
    assert_eq!(comparison["bounding_box"]["width"], 10);
    assert_eq!(comparison["bounding_box"]["height"], 5);
}

#[test]
fn a_visual_difference_is_not_an_error_exit_status() {
    let before = temp_file("diff-exit", "before.png");
    let after = temp_file("diff-exit", "after.png");
    write_solid_image(&before, 32, 32, [0, 0, 0]);
    write_solid_image(&after, 32, 32, [255, 255, 255]);

    // Changed or unchanged, a successful comparison exits 0.
    let (changed_code, _, _) = run_eensh_text(&[
        "diff",
        before.to_str().unwrap(),
        after.to_str().unwrap(),
        "--json",
    ]);
    let (unchanged_code, _, _) = run_eensh_text(&[
        "diff",
        before.to_str().unwrap(),
        before.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(changed_code, 0);
    assert_eq!(unchanged_code, 0);
}

#[test]
fn mismatched_dimensions_are_reported_as_incompatible_frames() {
    let big = temp_file("diff-size", "big.png");
    let small = temp_file("diff-size", "small.png");
    write_solid_image(&big, 1920, 1080, [0, 0, 0]);
    write_solid_image(&small, 1280, 720, [0, 0, 0]);

    let (code, stdout, stderr) = run_eensh_text(&[
        "diff",
        big.to_str().unwrap(),
        small.to_str().unwrap(),
        "--json",
    ]);

    assert_eq!(code, 10, "expected the incompatible_frames exit status");
    assert!(stdout.trim().is_empty());
    let error: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(error["error"]["code"], "incompatible_frames");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("1920x1080"));
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("1280x720"));
}

#[test]
fn a_missing_input_file_is_reported_as_image_load_failed() {
    let real = temp_file("diff-missing", "real.png");
    write_solid_image(&real, 16, 16, [0, 0, 0]);

    let (code, stdout, stderr) = run_eensh_text(&[
        "diff",
        real.to_str().unwrap(),
        "/no/such/file.png",
        "--json",
    ]);

    assert_eq!(code, 12, "expected the image_load_failed exit status");
    assert!(stdout.trim().is_empty());
    let error: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(error["error"]["code"], "image_load_failed");
}

#[test]
fn a_malformed_input_file_is_reported_as_image_load_failed() {
    let real = temp_file("diff-garbage", "real.png");
    let garbage = temp_file("diff-garbage", "garbage.png");
    write_solid_image(&real, 16, 16, [0, 0, 0]);
    std::fs::write(&garbage, b"not an image at all").unwrap();

    let (code, _, stderr) = run_eensh_text(&[
        "diff",
        real.to_str().unwrap(),
        garbage.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code, 12);
    let error: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(error["error"]["code"], "image_load_failed");
}

#[test]
fn jpeg_input_is_decoded_and_compared() {
    let before = temp_file("diff-jpeg", "before.jpg");
    let after = temp_file("diff-jpeg", "after.jpg");

    // Use flat colours so that JPEG's lossiness does not move any pixel far
    // enough to matter at a threshold of 12.
    write_solid_image(&before, 64, 64, [0, 0, 0]);
    write_solid_image(&after, 64, 64, [255, 255, 255]);

    let (code, stdout, stderr) = run_eensh_text(&[
        "diff",
        before.to_str().unwrap(),
        after.to_str().unwrap(),
        "--mode",
        "rgb",
        "--pixel-threshold",
        "12",
        "--json",
    ]);
    assert_eq!(code, 0, "jpeg comparison failed: {stderr}");

    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let comparison = &response["comparison"];
    assert_eq!(comparison["changed"], true);
    // Fully changed apart from the rounding JPEG introduces, so assert a strong
    // majority rather than an exact count.
    let changed = comparison["changed_pixels"].as_u64().unwrap();
    assert!(
        changed >= (64 * 64) - 64,
        "expected nearly every pixel to change, got {changed}"
    );
    assert_eq!(
        comparison["bounding_box"],
        serde_json::json!({"x": 0, "y": 0, "width": 64, "height": 64})
    );
}

#[test]
fn the_pixel_threshold_is_honoured_from_the_command_line() {
    let before = temp_file("diff-threshold", "before.png");
    let after = temp_file("diff-threshold", "after.png");
    write_solid_image(&before, 32, 32, [100, 100, 100]);
    write_solid_image(&after, 32, 32, [105, 105, 105]);

    // Difference of 5: below a threshold of 12, above a threshold of 4.
    let (_, above, _) = run_eensh_text(&[
        "diff",
        before.to_str().unwrap(),
        after.to_str().unwrap(),
        "--mode",
        "rgb",
        "--pixel-threshold",
        "12",
        "--json",
    ]);
    let response: serde_json::Value = serde_json::from_str(above.trim()).unwrap();
    assert_eq!(response["comparison"]["changed_pixels"], 0);

    let (_, below, _) = run_eensh_text(&[
        "diff",
        before.to_str().unwrap(),
        after.to_str().unwrap(),
        "--mode",
        "rgb",
        "--pixel-threshold",
        "4",
        "--json",
    ]);
    let response: serde_json::Value = serde_json::from_str(below.trim()).unwrap();
    assert_eq!(response["comparison"]["changed_pixels"], 32 * 32);
}

#[test]
fn the_area_threshold_is_honoured_from_the_command_line() {
    let before = temp_file("diff-area", "before.png");
    let after = temp_file("diff-area", "after.png");
    write_solid_image(&before, 100, 100, [0, 0, 0]);
    write_image_with_patch(&after, 100, 100, [0, 0, 0], (0, 0, 5, 1), [255, 255, 255]);

    // 5 pixels in 10000 is 0.0005.
    let (_, strict, _) = run_eensh_text(&[
        "diff",
        before.to_str().unwrap(),
        after.to_str().unwrap(),
        "--area-threshold",
        "0.01",
        "--json",
    ]);
    let response: serde_json::Value = serde_json::from_str(strict.trim()).unwrap();
    assert_eq!(response["comparison"]["changed"], false);
    assert_eq!(response["comparison"]["changed_pixels"], 5);
    // The bounding box survives even though the frame is not meaningfully changed.
    assert_eq!(
        response["comparison"]["bounding_box"],
        serde_json::json!({"x": 0, "y": 0, "width": 5, "height": 1})
    );

    let (_, lax, _) = run_eensh_text(&[
        "diff",
        before.to_str().unwrap(),
        after.to_str().unwrap(),
        "--area-threshold",
        "0.0005",
        "--json",
    ]);
    let response: serde_json::Value = serde_json::from_str(lax.trim()).unwrap();
    assert_eq!(response["comparison"]["changed"], true);
}

#[test]
fn the_summary_names_the_changed_rectangle_without_json() {
    let before = temp_file("diff-summary", "before.png");
    let after = temp_file("diff-summary", "after.png");
    write_solid_image(&before, 100, 100, [0, 0, 0]);
    write_image_with_patch(&after, 100, 100, [0, 0, 0], (7, 8, 10, 4), [255, 255, 255]);

    let (code, stdout, stderr) =
        run_eensh_text(&["diff", before.to_str().unwrap(), after.to_str().unwrap()]);

    assert_eq!(code, 0);
    assert!(stdout.trim().is_empty(), "the summary is not JSON");
    assert!(
        stderr.contains("40/10000") && stderr.contains("10x4+7+8"),
        "summary was: {stderr:?}"
    );
}

#[test]
fn an_unchanged_summary_says_so() {
    let path = temp_file("diff-summary-same", "same.png");
    write_solid_image(&path, 40, 40, [1, 2, 3]);

    let (code, _stdout, stderr) =
        run_eensh_text(&["diff", path.to_str().unwrap(), path.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(
        stderr.contains("no pixels changed"),
        "summary was: {stderr:?}"
    );
}

#[test]
fn the_changed_crop_is_written_from_the_second_image() {
    let before = temp_file("diff-crop", "before.png");
    let after = temp_file("diff-crop", "after.png");
    let crop = temp_file("diff-crop", "changed.png");

    write_solid_image(&before, 60, 60, [0, 0, 0]);
    write_image_with_patch(&after, 60, 60, [0, 0, 0], (10, 20, 8, 6), [255, 0, 0]);

    let (code, stdout, stderr) = run_eensh_text(&[
        "diff",
        before.to_str().unwrap(),
        after.to_str().unwrap(),
        "--changed-crop",
        crop.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code, 0, "diff failed: {stderr}");

    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["changed_crop"]["region"]["x"], 10);
    assert_eq!(response["changed_crop"]["region"]["y"], 20);
    assert_eq!(response["changed_crop"]["region"]["width"], 8);
    assert_eq!(response["changed_crop"]["region"]["height"], 6);
    assert_eq!(response["changed_crop"]["format"], "png");

    // The written crop is an 8x6 image of the changed colour.
    assert!(crop.exists(), "the crop should have been written");
    let bytes = std::fs::read(&crop).unwrap();
    let image = decode_png(&bytes);
    assert_eq!((image.width, image.height), (8, 6));
    image.expect_pixel(0, 0, [255, 0, 0], 0);
    image.expect_pixel(7, 5, [255, 0, 0], 0);
}

#[test]
fn the_changed_crop_format_follows_the_extension() {
    let before = temp_file("diff-crop-ext", "before.png");
    let after = temp_file("diff-crop-ext", "after.png");
    let crop = temp_file("diff-crop-ext", "changed.jpg");

    write_solid_image(&before, 32, 32, [0, 0, 0]);
    write_image_with_patch(&after, 32, 32, [0, 0, 0], (4, 4, 8, 8), [255, 255, 255]);

    let (code, stdout, stderr) = run_eensh_text(&[
        "diff",
        before.to_str().unwrap(),
        after.to_str().unwrap(),
        "--changed-crop",
        crop.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code, 0, "diff failed: {stderr}");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["changed_crop"]["format"], "jpeg");

    let bytes = std::fs::read(&crop).unwrap();
    assert_eq!(&bytes[..2], &[0xff, 0xd8], "the crop should be a JPEG");
    let image = decode_jpeg(&bytes);
    assert_eq!((image.width, image.height), (8, 8));
}

#[test]
fn no_crop_is_written_when_nothing_changed() {
    let path = temp_file("diff-crop-none", "same.png");
    let crop = temp_file("diff-crop-none", "changed.png");
    write_solid_image(&path, 32, 32, [5, 5, 5]);

    let (code, stdout, stderr) = run_eensh_text(&[
        "diff",
        path.to_str().unwrap(),
        path.to_str().unwrap(),
        "--changed-crop",
        crop.to_str().unwrap(),
        "--json",
    ]);

    assert_eq!(code, 0, "diff failed: {stderr}");
    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(
        response.get("changed_crop").is_none() || response["changed_crop"].is_null(),
        "no crop should be reported"
    );
    assert!(
        !crop.exists(),
        "no file should be created rather than an arbitrary placeholder image"
    );
}

#[test]
fn a_crop_is_written_for_a_bounding_box_that_failed_the_area_threshold() {
    // The crop follows the *bounding box*, not the `changed` flag. The area
    // threshold expresses a policy judgement about significance; the bounding box
    // is a factual statement about where differences were found. When a caller
    // asks for a crop of the changed region and a changed region was located, it
    // is written, and the JSON still reports `changed: false`.
    let before = temp_file("diff-crop-small", "before.png");
    let after = temp_file("diff-crop-small", "after.png");
    let crop = temp_file("diff-crop-small", "changed.png");
    write_solid_image(&before, 100, 100, [0, 0, 0]);
    write_image_with_patch(&after, 100, 100, [0, 0, 0], (3, 4, 1, 1), [255, 255, 255]);

    let (code, stdout, stderr) = run_eensh_text(&[
        "diff",
        before.to_str().unwrap(),
        after.to_str().unwrap(),
        "--area-threshold",
        "0.5",
        "--changed-crop",
        crop.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code, 0, "diff failed: {stderr}");

    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["comparison"]["changed"], false);
    assert_eq!(
        response["comparison"]["bounding_box"],
        serde_json::json!({"x": 3, "y": 4, "width": 1, "height": 1})
    );

    // The crop is the real changed region, not an invented placeholder.
    assert!(crop.exists());
    let image = decode_png(&std::fs::read(&crop).unwrap());
    assert_eq!((image.width, image.height), (1, 1));
    image.expect_pixel(0, 0, [255, 255, 255], 0);
}

#[test]
fn an_invalid_area_threshold_is_rejected_before_any_work_happens() {
    let path = temp_file("diff-bad-area", "same.png");
    write_solid_image(&path, 8, 8, [0, 0, 0]);

    let (code, stdout, _) = run_eensh_text(&[
        "diff",
        path.to_str().unwrap(),
        path.to_str().unwrap(),
        "--area-threshold",
        "2.0",
    ]);
    assert_ne!(code, 0, "an out-of-range threshold must be rejected");
    assert!(stdout.trim().is_empty());
}

#[test]
fn the_diff_help_lists_its_options() {
    let (code, stdout, _) = run_eensh_text(&["diff", "--help"]);
    assert_eq!(code, 0);
    for option in [
        "--mode",
        "--pixel-threshold",
        "--area-threshold",
        "--changed-crop",
        "--json",
    ] {
        assert!(stdout.contains(option), "diff help is missing {option}");
    }
}

#[test]
fn timing_can_be_printed_for_a_comparison() {
    let path = temp_file("diff-timing", "same.png");
    write_solid_image(&path, 128, 128, [0, 0, 0]);

    let (code, _stdout, stderr) = run_eensh_text(&[
        "diff",
        path.to_str().unwrap(),
        path.to_str().unwrap(),
        "--time",
    ]);
    assert_eq!(code, 0);
    assert!(
        stderr.contains("eensh diff timing:") && stderr.contains("compare="),
        "timings should be printed, got: {stderr:?}"
    );
}

#[test]
fn the_comparison_does_not_require_an_encode_step() {
    // A library-level guard for the Phase 2 architecture requirement: comparing
    // two frames works on frames that were never encoded, and the CLI's decode
    // step is confined to input handling.
    let data: Vec<u8> = (0..(64 * 64 * 3)).map(|i| (i % 251) as u8).collect();
    let before = eensh::input::frame_from_rgb8(64, 64, data.clone()).unwrap();
    let after = eensh::input::frame_from_rgb8(64, 64, data).unwrap();

    let comparison =
        eensh::compare::compare_frames(&before, &after, &eensh::compare::CompareOptions::default())
            .unwrap();
    assert_eq!(comparison.changed_pixels, 0);
}

#[test]
fn the_exit_status_is_stable_for_an_unknown_subcommand_option() {
    let output = Command::new(eensh_binary())
        .args(["diff", "a.png", "b.png", "--not-a-flag"])
        .output()
        .expect("failed to run eensh");
    assert_ne!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
}
