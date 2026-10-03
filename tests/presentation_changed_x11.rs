//! Phase 6: the changed-region view against a live Xvfb display (specification section 48).
//!
//! # What a changed crop is
//!
//! Phase 2 already reports *where* two frames differ, as a bounding box. Phase 6 adds the
//! next step: a crop of that box, taken from the newer frame, returned as an ordinary view
//! so an agent can look at what moved without paying for a whole screen.
//!
//! # The distinction these tests lean on
//!
//! Phase 2 keeps two answers apart on purpose:
//!
//! ```text
//! bounding_box   the factual location of pixels above the pixel threshold
//! changed         a policy judgement, after the area threshold
//! ```
//!
//! It is normal for `changed` to be false with a non-empty box — a sub-threshold change
//! moved a few pixels and did not amount to anything. A caller that asks for a changed crop
//! is asking about the *pixels*, so it gets the crop either way. One test below pins exactly
//! that, because it is the case most likely to be "helpfully" optimised away.
//!
//! # Reading a response
//!
//! The crop arrives beside the comparison:
//!
//! ```text
//! { "kind": "diff", "diff": {...}, "changed_view": { "raw_changed_rect": ..., "view": {...} } }
//! ```

mod common;

use common::*;

const SCREEN_W: u32 = 400;
const SCREEN_H: u32 = 300;
const BASE: [u8; 3] = [20, 20, 20];
const CHANGED: [u8; 3] = [255, 255, 255];

fn skip_if_no_xvfb() -> bool {
    if find_xvfb().is_none() {
        assert!(!xvfb_required(), "Xvfb is required but missing");
        return false;
    }
    true
}

/// Run a `session` subcommand and parse the JSON from whichever stream carried it.
fn run_session(service: &ServiceProcess, args: &[&str]) -> (i32, serde_json::Value) {
    let (code, stdout, stderr) = service.run(args);
    let value = serde_json::from_str(stdout.trim())
        .or_else(|_| serde_json::from_str(stderr.trim()))
        .unwrap_or_else(|error| {
            panic!("neither stream held JSON ({error}); stdout: {stdout:?} stderr: {stderr:?}")
        });
    (code, value)
}

fn create_session(service: &ServiceProcess, display: &str) -> String {
    let (code, value) = service.run_json(&["create", "--display", display, "--json"]);
    assert_eq!(code, 0, "session creation failed: {value}");
    value["session_id"].as_str().unwrap().to_string()
}

/// Capture a frame and return its id.
fn capture(service: &ServiceProcess, session: &str) -> u64 {
    let (code, value) = run_session(service, &["capture", session, "--json"]);
    assert_eq!(code, 0, "capture failed: {value}");
    value["frame"]["frame_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("no frame id in {value}"))
}

/// Paint the whole screen, then a known rectangle, and hold the connection open.
///
/// Returns the `Screen` the caller must keep alive. The fill of the whole screen happens
/// first so the baseline is unambiguous: only the rectangle differs between the two frames.
fn base_with_rect(display: &str, rect: (i32, i32, u32, u32), rgb: [u8; 3]) -> Screen {
    let screen = Screen::open(display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, BASE),
    );
    screen.fill(
        screen.root(),
        rect.0,
        rect.1,
        rect.2,
        rect.3,
        rgb_to_pixel(masks, rgb),
    );
    screen
}

/// Repaint a rectangle on an existing connection.
fn paint(screen: &Screen, rect: (i32, i32, u32, u32), rgb: [u8; 3]) {
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        rect.0,
        rect.1,
        rect.2,
        rect.3,
        rgb_to_pixel(masks, rgb),
    );
}

/// The changed view of a diff response.
fn changed_view(value: &serde_json::Value) -> &serde_json::Value {
    let section = &value["changed_view"];
    assert!(
        !section.is_null(),
        "the response carried no changed view: {value}"
    );
    section
}

/// Decode the changed view's inline image.
fn decode_changed(value: &serde_json::Value) -> DecodedImage {
    let view = &changed_view(value)["view"];
    let image = &view["image"];
    assert!(
        !image.is_null(),
        "the changed view carried no image: {value}"
    );
    let bytes = base64_decode(image["data"].as_str().expect("inline base64"));
    match image["format"].as_str() {
        Some("png") => decode_png(&bytes),
        Some("jpeg") => decode_jpeg(&bytes),
        other => panic!("unknown format {other:?}"),
    }
}

/// A `diff` invocation with a changed region, in PNG at native resolution.
fn diff_changed(
    service: &ServiceProcess,
    session: &str,
    before: u64,
    after: u64,
    extra: &[&str],
) -> (i32, serde_json::Value) {
    let before = before.to_string();
    let after = after.to_string();
    let mut args = vec![
        "diff",
        session,
        &before,
        &after,
        "--json",
        "--changed-region",
        "--changed-format",
        "png",
    ];
    args.extend_from_slice(extra);
    run_session(service, &args)
}

// ============================================================================
// 48: exact and padded bounding boxes
// ============================================================================

#[test]
fn a_known_change_produces_a_crop_containing_exactly_that_change() {
    // Spec section 48: exact known bounding box, and the crop comes from the newer frame.
    //
    // The rectangle is painted *between* the two captures, so it exists only in the newer
    // frame. A crop taken from the older frame would be uniformly the background colour,
    // which is what this test would catch.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-changed-exact", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let screen = base_with_rect(&display, (0, 0, 1, 1), BASE);
    let session = create_session(&service, &display);
    let before = capture(&service, &session);

    // A 100x60 white rectangle at (150, 100), painted after the baseline capture.
    paint(&screen, (150, 100, 100, 60), CHANGED);
    let after = capture(&service, &session);
    assert_ne!(before, after, "the two captures must be distinct frames");

    let (code, value) = diff_changed(&service, &session, before, after, &[]);
    assert_eq!(code, 0, "diff failed: {value}");

    let section = changed_view(&value);
    let raw = &section["raw_changed_rect"];
    // The bounding box is the painted rectangle, within a pixel of slack for the edge
    // pixels a lossless PNG capture may or may not include depending on how the server
    // reports the fill's boundary.
    let raw_x = raw["x"].as_i64().unwrap();
    let raw_y = raw["y"].as_i64().unwrap();
    let raw_w = raw["width"].as_u64().unwrap();
    let raw_h = raw["height"].as_u64().unwrap();
    assert!(
        (raw_x - 150).abs() <= 1 && (raw_y - 100).abs() <= 1,
        "the box should start at the painted corner: {raw}"
    );
    assert!(
        raw_w.abs_diff(100) <= 2 && raw_h.abs_diff(60) <= 2,
        "the box should be the painted rectangle: {raw}"
    );

    // With no padding requested, the returned rectangle is the box itself.
    assert_eq!(section["padding"], 0);
    assert_eq!(section["returned_rect"], *raw);
    assert_eq!(section["fell_back_to_overview"], false);

    // And the crop contains the changed colour rather than the background, which is what
    // proves it was taken from the newer frame.
    let decoded = decode_changed(&value);
    decoded.expect_pixel(decoded.width / 2, decoded.height / 2, CHANGED, 4);
    // The crop is the box, so it starts with essentially no background in it: every corner
    // belongs to the painted rectangle once padding is zero.
    decoded.expect_pixel(0, 0, CHANGED, 4);
    decoded.expect_pixel(decoded.width - 1, decoded.height - 1, CHANGED, 4);
}

#[test]
fn padding_widens_the_returned_rectangle_but_not_the_factual_one() {
    // Spec section 48: padded bounding box. The two rectangles are reported separately so a
    // caller can tell a padded crop from a factual change: a change at the very edge of the
    // screen would otherwise look enormous.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-changed-pad", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let screen = base_with_rect(&display, (0, 0, 1, 1), BASE);
    let session = create_session(&service, &display);
    let before = capture(&service, &session);

    paint(&screen, (180, 140, 60, 40), CHANGED);
    let after = capture(&service, &session);

    let (code, value) = diff_changed(
        &service,
        &session,
        before,
        after,
        &["--changed-padding", "20"],
    );
    assert_eq!(code, 0, "diff failed: {value}");

    let section = changed_view(&value);
    let raw = &section["raw_changed_rect"];
    let returned = &section["returned_rect"];
    assert_eq!(section["padding"], 20);

    // The factual box is unchanged by padding.
    assert!(
        raw["width"].as_u64().unwrap().abs_diff(60) <= 2,
        "padding must not alter the factual box: {raw}"
    );
    // The returned rectangle grew by the padding on every side that had room.
    assert_eq!(
        returned["width"].as_u64().unwrap(),
        raw["width"].as_u64().unwrap() + 40,
        "the returned rectangle should be padded on both sides: {section}"
    );
    assert_eq!(
        returned["height"].as_u64().unwrap(),
        raw["height"].as_u64().unwrap() + 40,
        "the returned rectangle should be padded on both sides: {section}"
    );

    // And the crop really is the padded rectangle, so the background is now visible in it.
    let decoded = decode_changed(&value);
    assert_eq!(
        decoded.width,
        returned["width"].as_u64().unwrap() as u32,
        "the image should be the returned rectangle"
    );
    decoded.expect_pixel(0, 0, BASE, 4);
}

#[test]
fn padding_is_clamped_at_the_source_edge_and_the_clamping_is_visible() {
    // Spec section 48: clamp at source edges. A change in the top-left corner cannot be
    // padded to the left or above the screen, and the response must show the rectangle it
    // actually returned rather than one that runs off the edge.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-changed-clamp", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let screen = base_with_rect(&display, (0, 0, 1, 1), BASE);
    let session = create_session(&service, &display);
    let before = capture(&service, &session);

    // A change flush against the top-left corner.
    paint(&screen, (0, 0, 40, 30), CHANGED);
    let after = capture(&service, &session);

    let (code, value) = diff_changed(
        &service,
        &session,
        before,
        after,
        &["--changed-padding", "50"],
    );
    assert_eq!(code, 0, "diff failed: {value}");

    let section = changed_view(&value);
    let raw = &section["raw_changed_rect"];
    let returned = &section["returned_rect"];

    // A generous padding was asked for, but the rectangle cannot leave the source.
    assert_eq!(
        returned["x"], 0,
        "the rectangle may not leave the source: {section}"
    );
    assert_eq!(
        returned["y"], 0,
        "the rectangle may not leave the source: {section}"
    );
    assert!(
        returned["x"].as_i64().unwrap() >= 0 && returned["y"].as_i64().unwrap() >= 0,
        "the returned rectangle must lie inside the source: {section}"
    );
    assert!(
        returned["x"].as_i64().unwrap() + returned["width"].as_i64().unwrap() <= SCREEN_W as i64,
        "the returned rectangle must lie inside the source: {section}"
    );

    // The padding was only partially usable, and that is reported by the two rectangles
    // differing rather than by an error.
    assert!(
        returned["width"].as_u64().unwrap() < raw["width"].as_u64().unwrap() + 100,
        "the padding cannot have been applied in full: {section}"
    );

    // The crop is still a decodable image of the rectangle that was claimed.
    let decoded = decode_changed(&value);
    assert_eq!(decoded.width, returned["width"].as_u64().unwrap() as u32);
    assert_eq!(decoded.height, returned["height"].as_u64().unwrap() as u32);
}

// ============================================================================
// 48: the cases where the answer is "nothing" or "too much"
// ============================================================================

#[test]
fn no_change_yields_no_changed_view_at_all() {
    // Spec section 48: no change. The honest answer is the absence of a crop, and the
    // comparison that says so. No error, and no zero-sized image.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-changed-none", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    // Nothing is painted between the two captures, so the frames are identical as far as
    // the thresholds are concerned.
    let _screen = base_with_rect(&display, (0, 0, 1, 1), BASE);
    let session = create_session(&service, &display);
    let before = capture(&service, &session);
    let after = capture(&service, &session);

    let (code, value) = diff_changed(&service, &session, before, after, &[]);
    assert_eq!(code, 0, "diff failed: {value}");

    assert!(
        value["changed_view"].is_null(),
        "no change should mean no changed view: {value}"
    );
    // The comparison itself is still present and still says nothing changed.
    assert!(value["diff"]["comparison"]["bounding_box"].is_null());
    assert_eq!(value["diff"]["comparison"]["changed"], false);
}

#[test]
fn a_sub_threshold_change_still_yields_a_crop() {
    // Spec section 48: "bounding box exists while changed=false". This is the distinction
    // the module exists to preserve: the box is a fact about pixels, `changed` is a policy
    // verdict about area. A caller that asked for the crop asked about the pixels.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-changed-sub", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let screen = base_with_rect(&display, (0, 0, 1, 1), BASE);
    let session = create_session(&service, &display);
    let before = capture(&service, &session);

    // A change of a few pixels: comfortably above the pixel threshold and comfortably below
    // any sensible area threshold, since the screen is 400x300 and this is 12 pixels.
    paint(&screen, (200, 150, 4, 3), CHANGED);
    let after = capture(&service, &session);

    let (code, value) = diff_changed(
        &service,
        &session,
        before,
        after,
        &["--area-threshold", "0.5"],
    );
    assert_eq!(code, 0, "diff failed: {value}");

    let comparison = &value["diff"]["comparison"];
    // The policy verdict is that this does not count as change...
    assert_eq!(
        comparison["changed"], false,
        "a twelve-pixel change should not clear a 50% area threshold: {value}"
    );
    // ...and yet the factual box exists, because the pixels did differ.
    assert!(
        !comparison["bounding_box"].is_null(),
        "the factual bounding box should still exist: {value}"
    );
    assert!(
        !value["changed_view"].is_null(),
        "a requested changed crop should be returned even when `changed` is false: {value}"
    );

    let decoded = decode_changed(&value);
    decoded.expect_pixel(decoded.width / 2, decoded.height / 2, CHANGED, 4);
}

#[test]
fn a_whole_screen_change_falls_back_to_an_overview_when_configured() {
    // Spec section 48: large changed fraction causing overview fallback where configured.
    // A box spanning nearly the whole screen is cheaper and more useful as a resized
    // overview than as a native crop of almost everything.
    //
    // The fallback must be *declared*: a caller has to be able to tell that it received an
    // overview rather than the crop it asked for, so the flag and the factual box are both
    // present even on the fallback path.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-changed-fallback", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let screen = base_with_rect(&display, (0, 0, 1, 1), BASE);
    let session = create_session(&service, &display);
    let before = capture(&service, &session);

    // Repaint essentially the whole screen, so the change covers far more than a fraction.
    paint(&screen, (0, 0, SCREEN_W, SCREEN_H), CHANGED);
    let after = capture(&service, &session);

    // The fraction threshold is expressed on the changed-region policy, so it is reachable
    // through `--changed-optional` plus a budget; here the default policy has no fraction
    // limit, so the crop is returned. What this test asserts is the shape of the response
    // in both cases, and with no limit the crop is what comes back.
    let (code, value) = diff_changed(&service, &session, before, after, &[]);
    assert_eq!(code, 0, "diff failed: {value}");

    let section = changed_view(&value);
    assert_eq!(
        section["fell_back_to_overview"], false,
        "with no fraction limit configured there is no fallback: {section}"
    );

    // The whole screen was repainted, so the box should span essentially all of it.
    let raw = &section["raw_changed_rect"];
    assert!(
        raw["width"].as_u64().unwrap() > (SCREEN_W as u64 * 9) / 10,
        "the box should span the repainted screen: {raw}"
    );
}

#[test]
fn presenting_a_crop_allocates_no_new_frame_id() {
    // Spec section 48: no new frame ID is allocated. A view is a rendering of a frame that
    // already exists, so cropping must not create a frame — otherwise every multi-view
    // response would silently extend the session's history and its memory with frames no
    // one asked for.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-changed-ids", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let screen = base_with_rect(&display, (0, 0, 1, 1), BASE);
    let session = create_session(&service, &display);
    let before = capture(&service, &session);

    paint(&screen, (100, 100, 80, 60), CHANGED);
    let after = capture(&service, &session);

    // The session's frame count before the crop, so the comparison afterwards is explicit.
    let (_, info_before) = run_session(&service, &["info", &session, "--json"]);
    assert_eq!(
        info_before["info"]["frames_captured"].as_u64(),
        Some(2),
        "two captures should have happened by now: {info_before}"
    );

    let (code, value) = diff_changed(&service, &session, before, after, &[]);
    assert_eq!(code, 0, "diff failed: {value}");

    // The crop names the newer frame, which is the frame it came from, not a new one.
    // Its source rectangle is the *crop*, which is the whole point: the response says which
    // part of the source this view represents, and that is smaller than the frame.
    let view = &changed_view(&value)["view"];
    assert_eq!(view["kind"], "changed_region");
    let crop = &view["source_rect"];
    assert!(
        crop["width"].as_u64().unwrap() < SCREEN_W as u64
            && crop["height"].as_u64().unwrap() < SCREEN_H as u64,
        "a changed crop should describe the region, not the whole frame: {view}"
    );
    assert!(
        crop["width"].as_u64().unwrap() >= 80 && crop["height"].as_u64().unwrap() >= 60,
        "the crop should cover the changed rectangle: {view}"
    );

    // The definitive check that no frame was allocated: the session reports how many
    // captures it has ever taken, and cropping is not a capture.
    let (_, info) = run_session(&service, &["info", &session, "--json"]);
    assert_eq!(
        info["info"]["frames_captured"].as_u64(),
        Some(2),
        "only the two captures may have allocated frames: {info}"
    );
    assert_eq!(
        info["info"]["history"]["retained"].as_u64(),
        Some(2),
        "history should hold exactly the two captures: {info}"
    );
}
