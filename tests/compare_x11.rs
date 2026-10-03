//! Phase 2 integration tests: the full `X11 → Frame → compare` path.
//!
//! These tests prove the acceptance scenario end to end. They start a real
//! `Xvfb`, paint a known scene, capture it, modify a known rectangle, capture
//! again, and compare the two raw frames. No PNG or JPEG is involved anywhere,
//! which is the point: the comparison runs on the frames the capture backend
//! produces.
//!
//! The comparison is performed through the library API rather than a CLI
//! invocation, precisely so the test cannot accidentally depend on an encode step.

mod common;

use common::*;
use eensh::capture::{x11, CaptureRequest, Display};
use eensh::compare::{compare_frames, CompareMode, CompareOptions};

const SCREEN_W: u32 = 800;
const SCREEN_H: u32 = 600;

fn skip_if_no_xvfb() -> bool {
    if find_xvfb().is_none() {
        assert!(!xvfb_required(), "Xvfb is required but missing");
        return true;
    }
    false
}

/// Capture the whole desktop at native size, with no resize or encoding.
fn capture_desktop(display: &str) -> eensh::frame::Frame {
    let handle = Display::open(display).expect("could not open the test display");
    x11::capture(&handle, &CaptureRequest::Desktop).expect("capture should succeed")
}

/// Paint a solid rectangle on the root window.
fn paint(screen: &Screen, x: i32, y: i32, width: u32, height: u32, rgb: [u8; 3]) {
    let masks = screen.visual_masks();
    screen.fill(screen.root(), x, y, width, height, rgb_to_pixel(masks, rgb));
}

#[test]
fn the_acceptance_scenario_identifies_the_changed_rectangle() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let black = [0u8, 0, 0];
    let white = [255u8, 255, 255];

    // 1. Create a known scene: an entirely black screen.
    paint(&screen, 0, 0, SCREEN_W, SCREEN_H, black);

    // 2. Capture Frame A.
    let before = capture_desktop(&display);

    // 3. Modify a known rectangle: 100x50 at (200, 150).
    paint(&screen, 200, 150, 100, 50, white);

    // 4. Capture Frame B.
    let after = capture_desktop(&display);

    // 5. Compare.
    let options = CompareOptions::default();
    let comparison = compare_frames(&before, &after, &options).expect("comparison should succeed");

    // 6. Assert the changed geometry is exactly the painted rectangle.
    assert_eq!(comparison.total_pixels, (SCREEN_W * SCREEN_H) as u64);
    assert_eq!(
        comparison.changed_pixels,
        (100 * 50) as u64,
        "exactly the painted rectangle should differ"
    );
    assert_eq!(
        comparison.bounding_box,
        Some(eensh::geometry::Rect::new(200, 150, 100, 50).unwrap()),
        "the bounding box should be the painted rectangle"
    );
    assert!(comparison.changed);
}

#[test]
fn an_unmodified_screen_produces_zero_change() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    // Paint a non-uniform scene so that "identical" is a meaningful claim.
    let masks = screen.visual_masks();
    let red = [255u8, 0, 0];
    let blue = [0u8, 0, 255];
    screen.fill(screen.root(), 0, 0, 400, 300, rgb_to_pixel(masks, red));
    screen.fill(screen.root(), 400, 300, 400, 300, rgb_to_pixel(masks, blue));

    let before = capture_desktop(&display);
    let after = capture_desktop(&display);

    let comparison = compare_frames(&before, &after, &CompareOptions::default()).unwrap();
    assert_eq!(
        comparison.changed_pixels, 0,
        "two captures of an unchanged screen must be identical"
    );
    assert_eq!(comparison.changed_fraction, 0.0);
    assert_eq!(comparison.bounding_box, None);
    assert!(!comparison.changed);
}

#[test]
fn a_change_smaller_than_the_area_threshold_is_not_meaningfully_changed() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let black = [0u8, 0, 0];
    let white = [255u8, 255, 255];

    paint(&screen, 0, 0, SCREEN_W, SCREEN_H, black);
    let before = capture_desktop(&display);

    // A single white pixel: 1 in 480000, or about 0.0002%.
    paint(&screen, 10, 10, 1, 1, white);
    let after = capture_desktop(&display);

    let options = CompareOptions {
        area_threshold: 0.01,
        ..CompareOptions::default()
    };
    let comparison = compare_frames(&before, &after, &options).unwrap();

    assert!(
        !comparison.changed,
        "one pixel must not clear a 1% threshold"
    );
    // The raw metrics and bounding box are still reported.
    assert_eq!(comparison.changed_pixels, 1);
    assert_eq!(
        comparison.bounding_box,
        Some(eensh::geometry::Rect::new(10, 10, 1, 1).unwrap()),
        "the bounding box must survive the area-threshold decision"
    );
}

#[test]
fn a_colour_change_below_the_pixel_threshold_is_ignored() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    // Two greys 5 apart, which is a plausible rendering drift but not a real change.
    let dark = [100u8, 100, 100];
    let slightly_lighter = [105u8, 105, 105];

    paint(&screen, 0, 0, SCREEN_W, SCREEN_H, dark);
    let before = capture_desktop(&display);

    paint(&screen, 100, 100, 200, 200, slightly_lighter);
    let after = capture_desktop(&display);

    // A threshold of 12 must ignore a difference of 5.
    let options = CompareOptions {
        mode: CompareMode::RgbThreshold,
        pixel_threshold: 12,
        area_threshold: 0.0,
    };
    let comparison = compare_frames(&before, &after, &options).unwrap();
    assert_eq!(
        comparison.changed_pixels, 0,
        "a difference of 5 must not clear a threshold of 12"
    );
    assert!(!comparison.changed);

    // The same change does register under exact comparison.
    let exact = compare_frames(&before, &after, &CompareOptions::default()).unwrap();
    assert_eq!(exact.changed_pixels, (200 * 200) as u64);
    assert_eq!(
        exact.bounding_box,
        Some(eensh::geometry::Rect::new(100, 100, 200, 200).unwrap())
    );
}

#[test]
fn a_colour_change_above_the_pixel_threshold_is_detected() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let dark = [100u8, 100, 100];
    let much_lighter = [140u8, 100, 100];

    paint(&screen, 0, 0, SCREEN_W, SCREEN_H, dark);
    let before = capture_desktop(&display);

    paint(&screen, 50, 60, 80, 40, much_lighter);
    let after = capture_desktop(&display);

    let options = CompareOptions {
        mode: CompareMode::RgbThreshold,
        pixel_threshold: 12,
        area_threshold: 0.0,
    };
    let comparison = compare_frames(&before, &after, &options).unwrap();
    assert_eq!(
        comparison.changed_pixels,
        (80 * 40) as u64,
        "a difference of 40 must clear a threshold of 12"
    );
    assert_eq!(
        comparison.bounding_box,
        Some(eensh::geometry::Rect::new(50, 60, 80, 40).unwrap())
    );
}

#[test]
fn the_changed_crop_of_a_live_capture_contains_the_changed_region() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };

    let screen = Screen::open(&display);
    let black = [0u8, 0, 0];
    let green = [0u8, 255, 0];

    paint(&screen, 0, 0, SCREEN_W, SCREEN_H, black);
    let before = capture_desktop(&display);
    paint(&screen, 300, 200, 60, 30, green);
    let after = capture_desktop(&display);

    let comparison = compare_frames(&before, &after, &CompareOptions::default()).unwrap();
    let region = comparison.bounding_box.expect("something changed");

    // Crop the *after* frame, which is what the CLI does.
    let cropped = after.crop(&region).expect("crop should succeed");
    assert_eq!((cropped.width(), cropped.height()), (60, 30));

    // Every pixel of the crop should be the painted colour.
    for y in 0..30u32 {
        for x in 0..60u32 {
            let index = ((y * 60 + x) * 3) as usize;
            let pixel = &cropped.pixels.data()[index..index + 3];
            assert_eq!(pixel, green, "crop pixel ({x}, {y}) was {pixel:?}");
        }
    }

    // The crop's source geometry is shifted by the region origin, so a transform
    // computed from it still points at the right place on the desktop.
    assert_eq!(cropped.source_geometry.x, 300);
    assert_eq!(cropped.source_geometry.y, 200);
}

#[test]
fn comparing_frames_from_two_different_display_sizes_fails_explicitly() {
    let _guard = serial();
    if skip_if_no_xvfb() {
        return;
    }
    let Some((_server_a, display_a)) = xvfb(SCREEN_W, SCREEN_H) else {
        return;
    };
    let Some((_server_b, display_b)) = xvfb(320, 240) else {
        return;
    };

    let a = capture_desktop(&display_a);
    let b = capture_desktop(&display_b);

    let error = compare_frames(&a, &b, &CompareOptions::default()).unwrap_err();
    assert_eq!(error.code(), "incompatible_frames");
    assert!(
        error.message().contains("800x600") && error.message().contains("320x240"),
        "message should name both sizes, was: {}",
        error.message()
    );
}
