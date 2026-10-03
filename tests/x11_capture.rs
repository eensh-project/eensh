//! Xvfb integration tests for direct X11 capture.
//!
//! These tests start a private `Xvfb` display, draw *known* colours onto it, and
//! verify that `eensh` returns those exact pixels at the coordinates they were
//! drawn at. That is what makes them more than a smoke test: they check that the
//! geometry, the coordinate transform, and the pixel decode all agree with the
//! screen.
//!
//! Every test is skipped with a message when no `Xvfb` binary is available.
//! Set `EENSH_REQUIRE_XVFB=1` to make a missing `Xvfb` a failure instead.

mod common;

use common::*;

/// The virtual screen used throughout, large enough to place windows inside.
const SCREEN_W: u32 = 640;
const SCREEN_H: u32 = 480;

fn skip_if_no_xvfb() -> bool {
    if find_xvfb().is_none() {
        assert!(!xvfb_required(), "Xvfb is required but missing");
        return true;
    }
    false
}

#[test]
fn captures_the_full_desktop_at_the_screen_size() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    // Writing the image to a file leaves stdout free for the JSON metadata.
    let path = temp_file("desktop", "desktop.png");
    let (code, stdout, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--format",
        "png",
        "--json",
        path.to_str().unwrap(),
    ]);

    assert_eq!(code, 0, "capture failed: {stderr}");
    let response: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout should be JSON");

    assert_eq!(response["source"]["kind"], "desktop");
    assert_eq!(response["source"]["display"], display.as_str());
    assert_eq!(response["source"]["width"], SCREEN_W);
    assert_eq!(response["source"]["height"], SCREEN_H);
    assert_eq!(response["source"]["x"], 0);
    assert_eq!(response["source"]["y"], 0);

    // No resize was requested, so image dimensions equal source dimensions.
    assert_eq!(response["image"]["width"], SCREEN_W);
    assert_eq!(response["image"]["height"], SCREEN_H);
    assert_eq!(response["image"]["media_type"], "image/png");

    // The written file really is a PNG of that size.
    let image = decode_png(&std::fs::read(&path).unwrap());
    assert_eq!((image.width, image.height), (SCREEN_W, SCREEN_H));
}

#[test]
fn desktop_capture_matches_the_screen_size_without_json() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let output = run_eensh(&["capture", "--display", &display, "--format", "png", "-"]);
    assert!(output.status.success(), "capture failed");

    // stdout is a raw PNG, not JSON.
    assert_eq!(&output.stdout[..8], b"\x89PNG\r\n\x1a\n");
    let image = decode_png(&output.stdout);
    assert_eq!((image.width, image.height), (SCREEN_W, SCREEN_H));
}

#[test]
fn region_capture_returns_the_requested_rectangle_of_pixels() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();

    // Paint a 40x30 white square at (100, 200) on the root window.
    let red = [255u8, 0, 0];
    screen.fill(screen.root(), 100, 200, 40, 30, rgb_to_pixel(masks, red));

    let path = temp_file("region", "region.png");
    let (code, stdout, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--region",
        "100,200,40,30",
        "--format",
        "png",
        "--json",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "capture failed: {stderr}");

    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["source"]["kind"], "region");
    assert_eq!(response["source"]["x"], 100);
    assert_eq!(response["source"]["y"], 200);
    assert_eq!(response["source"]["width"], 40);
    assert_eq!(response["source"]["height"], 30);
    assert_eq!(response["image"]["width"], 40);
    assert_eq!(response["image"]["height"], 30);

    // The transform must identify the region origin and a unit scale.
    assert_eq!(response["transform"]["offset_x"], 100);
    assert_eq!(response["transform"]["offset_y"], 200);
    assert_eq!(response["transform"]["scale_x"], 1.0);
    assert_eq!(response["transform"]["scale_y"], 1.0);

    // The captured region should contain exactly the colour painted there.
    let image = decode_png(&std::fs::read(&path).unwrap());
    assert_eq!((image.width, image.height), (40, 30));
    for y in 0..30 {
        for x in 0..40 {
            image.expect_pixel(x, y, red, 0);
        }
    }
}

#[test]
fn region_outside_the_display_is_rejected_without_silent_clipping() {
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
        "--region",
        "600,400,200,200",
        "--json",
        "-",
    ]);

    assert_ne!(code, 0, "an out-of-bounds region must not succeed");
    assert_eq!(code, 4, "expected the invalid_region exit status");
    assert!(stdout.trim().is_empty(), "no image should be produced");

    let error: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("stderr should be JSON");
    assert_eq!(error["error"]["code"], "invalid_region");
}

#[test]
fn resizing_produces_the_transformed_geometry_and_coordinates() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    // SCREEN_W x SCREEN_H (640x480) halved is exactly 320x240.
    //
    // The image goes to a file so that stdout is free for the JSON. Writing
    // binary to stdout alongside `--json` puts the JSON on stderr instead, which
    // is the documented `capture` rule and would make this parse the wrong stream.
    let path = temp_file("resize", "half.png");
    let (code, stdout, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--width",
        "320",
        "--format",
        "png",
        "--json",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "capture failed: {stderr}");

    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["source"]["width"], SCREEN_W);
    assert_eq!(response["source"]["height"], SCREEN_H);
    assert_eq!(response["image"]["width"], 320);
    assert_eq!(response["image"]["height"], 240);
    assert_eq!(response["transform"]["scale_x"], 2.0);
    assert_eq!(response["transform"]["scale_y"], 2.0);
    assert_eq!(response["transform"]["offset_x"], 0);
    assert_eq!(response["transform"]["offset_y"], 0);

    // The resized file really is 320x240.
    let image = decode_png(&std::fs::read(&path).unwrap());
    assert_eq!((image.width, image.height), (320, 240));
}

#[test]
fn resized_region_keeps_the_source_offset_in_the_transform() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    // A 200x100 region at (50, 60), resized to half: 100x50.
    let (code, stdout, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--region",
        "50,60,200,100",
        "--width",
        "100",
        "--format",
        "jpeg",
        "--base64",
        "--json",
    ]);
    assert_eq!(code, 0, "capture failed: {stderr}");

    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["image"]["width"], 100);
    assert_eq!(response["image"]["height"], 50);
    assert_eq!(response["transform"]["offset_x"], 50);
    assert_eq!(response["transform"]["offset_y"], 60);
    assert_eq!(response["transform"]["scale_x"], 2.0);
    assert_eq!(response["transform"]["scale_y"], 2.0);

    // The documented mapping: image (100, 50) -> source (50 + 200, 60 + 100).
    let scale_x = response["transform"]["scale_x"].as_f64().unwrap();
    let scale_y = response["transform"]["scale_y"].as_f64().unwrap();
    let offset_x = response["transform"]["offset_x"].as_f64().unwrap();
    let offset_y = response["transform"]["offset_y"].as_f64().unwrap();
    assert_eq!(offset_x + 100.0 * scale_x, 250.0);
    assert_eq!(offset_y + 50.0 * scale_y, 160.0);
}

#[test]
fn unavailable_display_is_distinguishable_from_other_failures() {
    let _guard = serial();
    // This test deliberately does not need Xvfb, but keeping it serialised keeps
    // the failure output tidy when a display number collides with a live server.

    let (code, stdout, stderr) = run_eensh_text(&["capture", "--display", ":54321", "--json"]);

    assert_eq!(code, 3, "expected the display_unavailable exit status");
    assert!(stdout.trim().is_empty());
    let error: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("stderr should be JSON");
    assert_eq!(error["error"]["code"], "display_unavailable");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains(":54321"));
}

#[test]
fn capture_from_a_real_display_reflects_painted_pixels_after_resize() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();

    // Paint the top-left quarter of the screen green.
    let green = [0u8, 255, 0];
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W / 2,
        SCREEN_H / 2,
        rgb_to_pixel(masks, green),
    );

    // Capture at half size, so the green quarter becomes the top-left corner.
    let path = temp_file("resized-region", "half.png");
    let (code, _, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--width",
        &(SCREEN_W / 2).to_string(),
        "--format",
        "png",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "capture failed: {stderr}");

    let image = decode_png(&std::fs::read(&path).unwrap());
    assert_eq!((image.width, image.height), (SCREEN_W / 2, SCREEN_H / 2));
    // A point well inside the green quarter still maps to green.
    image.expect_pixel(10, 10, green, 0);
    image.expect_pixel(image.width / 2 - 1, 10, green, 0);
    // A point well inside the black half maps to black.
    image.expect_pixel(image.width - 1, image.height - 1, [0, 0, 0], 0);
}
