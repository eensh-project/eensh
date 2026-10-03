//! Integration tests for capturing a specific X11 window by ID.
//!
//! Two properties matter here and are tested separately:
//!
//! * a window capture returns the pixels and geometry of that window; and
//! * a window capture can be distinguished from a failed one, so an agent never
//!   mistakes an error for a screenshot.

mod common;

use common::*;

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
fn captures_a_window_by_id_at_its_own_geometry() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();

    let window = screen.create_window(120, 80, 40, 60);
    screen.map(window);
    let (window_x, window_y, window_w, window_h) = screen.geometry(window);

    // Paint the whole window a distinctive blue.
    let blue = [0u8, 0, 255];
    screen.fill(window, 0, 0, window_w, window_h, rgb_to_pixel(masks, blue));

    let path = temp_file("window", "window.png");
    let (code, _, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--window",
        &format!("0x{window:x}"),
        "--format",
        "png",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "window capture failed: {stderr}");

    let image = decode_png(&std::fs::read(&path).unwrap());
    assert_eq!(
        (image.width, image.height),
        (window_w, window_h),
        "window capture must return the window's own size"
    );
    // The window is fully visible on a blank desktop, so every pixel is blue.
    image.expect_pixel(0, 0, blue, 0);
    image.expect_pixel(image.width - 1, image.height - 1, blue, 0);
    let _ = (window_x, window_y);
}

#[test]
fn window_capture_reports_the_window_origin_in_the_transform() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let window = screen.create_window(100, 50, 200, 150);
    screen.map(window);
    let (window_x, window_y, _, _) = screen.geometry(window);

    let path = temp_file("window-origin", "window.png");
    let (code, stdout, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--window",
        &format!("{window}"),
        "--format",
        "png",
        "--json",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "window capture failed: {stderr}");

    let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["source"]["kind"], "window");
    assert_eq!(response["source"]["id"], format!("0x{window:x}"));
    assert_eq!(response["source"]["x"], window_x);
    assert_eq!(response["source"]["y"], window_y);
    assert_eq!(response["source"]["width"], 100);
    assert_eq!(response["source"]["height"], 50);
    assert_eq!(response["transform"]["offset_x"], window_x);
    assert_eq!(response["transform"]["offset_y"], window_y);
}

#[test]
fn window_capture_uses_the_visible_desktop_pixels() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();

    // A large red window, with a smaller green window mapped on top of it.
    let lower = screen.create_window(200, 200, 100, 100);
    screen.map(lower);
    let red = [255u8, 0, 0];
    screen.fill(lower, 0, 0, 200, 200, rgb_to_pixel(masks, red));

    let upper = screen.create_window(80, 80, 120, 120);
    screen.map(upper);
    let green = [0u8, 255, 0];
    screen.fill(upper, 0, 0, 80, 80, rgb_to_pixel(masks, green));

    // Capture the lower window. Where the upper window covers it, the pixels are
    // green: this documents that window capture returns what is visible.
    let path = temp_file("window-occlusion", "lower.png");
    let (code, _, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--window",
        &format!("0x{lower:x}"),
        "--format",
        "png",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "window capture failed: {stderr}");

    let image = decode_png(&std::fs::read(&path).unwrap());
    assert_eq!((image.width, image.height), (200, 200));
    // Top-left corner is not covered by the upper window.
    image.expect_pixel(0, 0, red, 0);
    // A point comfortably inside the upper window's rectangle, expressed in the
    // lower window's own coordinates.
    image.expect_pixel(50, 50, green, 0);
    // Bottom-right corner is uncovered again.
    image.expect_pixel(199, 199, red, 0);
}

#[test]
fn a_destroyed_window_reports_window_not_found() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let window = screen.create_window(50, 50, 0, 0);
    screen.map(window);
    screen.destroy(window);

    let (code, stdout, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--window",
        &format!("0x{window:x}"),
        "--json",
        "-",
    ]);

    assert_ne!(code, 0, "capturing a destroyed window must fail");
    assert_eq!(code, 5, "expected the window_not_found exit status");
    assert!(stdout.trim().is_empty(), "no image should be produced");
    let error: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("stderr should be JSON");
    assert_eq!(error["error"]["code"], "window_not_found");
}

#[test]
fn a_malformed_window_id_is_an_argument_error_not_a_capture_error() {
    // No display is needed: parsing fails before any X11 call.
    let (code, stdout, _stderr) = run_eensh_text(&["capture", "--window", "not-a-window"]);

    // Clap reports usage errors with its own status; the important part is that
    // this is a non-zero exit with no image.
    assert_ne!(code, 0);
    assert!(stdout.trim().is_empty());
}

#[test]
fn an_unknown_but_well_formed_window_id_is_reported_as_not_found() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    // A window ID that was never created on this display.
    let (code, _, stderr) = run_eensh_text(&[
        "capture",
        "--display",
        &display,
        "--window",
        "0x7fffffff",
        "--json",
        "-",
    ]);

    assert_eq!(code, 5, "expected the window_not_found exit status");
    let error: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(error["error"]["code"], "window_not_found");
}
