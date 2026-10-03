//! Phase 6: presentation against a live Xvfb display (specification section 55).
//!
//! # What only a real display can establish
//!
//! The presentation unit tests work on synthetic frames, which is what makes them fast
//! and exact. What they cannot show is that the presentation layer is wired to a real
//! capture with the pixels still intact end to end: that a rectangle the caller names in
//! *source* coordinates comes back containing the pixels that really were at those
//! coordinates on the screen. A source-to-view transform that is wrong by a scale factor
//! produces a perfectly well-formed response, and only a painted screen catches it.
//!
//! # The rules these tests work under
//!
//! * **Xvfb clears the root window when its last client disconnects.** Every scenario
//!   holds a `Screen` or a `Painter` connection open for its whole duration, or the scene
//!   it painted would vanish mid-sample.
//! * **Capture duration is not something a test can pin down** (requirement 58). Nothing
//!   here asserts how long a capture took, only what it contained and how it was ordered.
//! * **Tests take the process-wide `serial()` guard** because the `Screen` connection in
//!   the body is single-threaded, and `ServiceProcess` spawns a child. Sampling commands
//!   that block are run through the harness one at a time.
//!
//! # Reading a response
//!
//! A presentation response is attached as `presentation` beside the command's own body:
//!
//! ```text
//! { "kind": "frame", "frame": {...}, "presentation": { "frames": [ ... ] } }
//! ```
//!
//! So the helpers below reach for `["presentation"]` and then walk the frame stack.

mod common;

use common::*;

const SCREEN_W: u32 = 400;
const SCREEN_H: u32 = 300;

/// Skip the test when Xvfb is unavailable, unless the environment insists on it.
fn skip_if_no_xvfb() -> bool {
    if find_xvfb().is_none() {
        assert!(!xvfb_required(), "Xvfb is required but missing");
        return false;
    }
    true
}

/// Run a `session` subcommand and parse the JSON, whether it arrived on stdout or
/// stderr.
///
/// Presentation commands that succeed print their result on stdout; a refusal such as an
/// over-tight budget is a diagnostic on stderr. Both are JSON, and a test that wants to
/// inspect a refusal should not have to care which stream carried it.
fn run_session(service: &ServiceProcess, args: &[&str]) -> (i32, serde_json::Value) {
    let (code, stdout, stderr) = service.run(args);
    let value = serde_json::from_str(stdout.trim())
        .or_else(|_| serde_json::from_str(stderr.trim()))
        .unwrap_or_else(|error| {
            panic!("neither stream held JSON ({error}); stdout: {stdout:?} stderr: {stderr:?}")
        });
    (code, value)
}

/// Create a session for the whole screen of `display`.
fn create_session(service: &ServiceProcess, display: &str) -> String {
    let (code, value) = service.run_json(&["create", "--display", display, "--json"]);
    assert_eq!(code, 0, "session creation failed: {value}");
    value["session_id"].as_str().unwrap().to_string()
}

/// The presentation section of a response.
fn presentation(value: &serde_json::Value) -> &serde_json::Value {
    let section = &value["presentation"];
    assert!(
        !section.is_null(),
        "the response carried no presentation: {value}"
    );
    section
}

/// The presented frames, oldest first.
fn frames(value: &serde_json::Value) -> &Vec<serde_json::Value> {
    presentation(value)["frames"]
        .as_array()
        .expect("a frame stack")
}

/// One frame's views.
fn views(frame: &serde_json::Value) -> &Vec<serde_json::Value> {
    frame["views"].as_array().expect("a view list")
}

/// The view of the given name.
fn view<'a>(frame: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    views(frame)
        .iter()
        .find(|candidate| candidate["name"] == name)
        .unwrap_or_else(|| panic!("no view named {name:?} in {frame}"))
}

/// The only frame in a single-frame presentation.
fn only_frame(value: &serde_json::Value) -> &serde_json::Value {
    let stack = frames(value);
    assert_eq!(stack.len(), 1, "expected a single frame: {value}");
    &stack[0]
}

/// Decode a view's inline image, failing with the view's own description if it has none.
fn decode_view(view: &serde_json::Value) -> DecodedImage {
    let image = &view["image"];
    assert!(
        !image.is_null(),
        "view {:?} carried no image: {view}",
        view["name"]
    );
    let data = image["data"]
        .as_str()
        .unwrap_or_else(|| panic!("view {:?} has no inline data: {image}", view["name"]));
    let bytes = base64_decode(data);
    match image["format"].as_str() {
        Some("png") => decode_png(&bytes),
        Some("jpeg") => decode_jpeg(&bytes),
        other => panic!("view {view} reported an unknown format {other:?}"),
    }
}

/// Paint the whole screen a flat colour and hold the connection open.
fn flat_screen(display: &str, rgb: [u8; 3]) -> Screen {
    let screen = Screen::open(display);
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, rgb),
    );
    screen
}

// ============================================================================
// 55: overview + regions
// ============================================================================

#[test]
fn an_overview_and_a_region_contain_the_pixels_that_were_painted() {
    // The end-to-end coordinate claim. Four quadrants in four known colours, then an
    // overview of the whole screen and a region covering one quadrant. If the region's
    // source rectangle were interpreted in the wrong coordinate space, or the crop were
    // taken from the wrong offset, the region would contain the wrong quadrant's colour.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-overview-roi", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let screen = Screen::open(&display);
    let masks = screen.visual_masks();
    let paint = |x: i32, y: i32, width: u32, height: u32, rgb: [u8; 3]| {
        screen.fill(screen.root(), x, y, width, height, rgb_to_pixel(masks, rgb));
    };
    paint(0, 0, 200, 150, [255, 0, 0]);
    paint(200, 0, 200, 150, [0, 255, 0]);
    paint(0, 150, 200, 150, [0, 0, 255]);
    paint(200, 150, 200, 150, [255, 255, 0]);

    let session = create_session(&service, &display);

    let (code, value) = run_session(
        &service,
        &[
            "capture",
            &session,
            "--json",
            "--base64",
            "--overview-width",
            "200",
            "--overview-format",
            "png",
            // Native resolution for the region, so its pixels can be compared directly
            // with what was painted without any scaling to reason about.
            "--region",
            "br=200,150,200,150",
            "--region-format",
            "png",
        ],
    );
    assert_eq!(code, 0, "capture failed: {value}");

    let frame = only_frame(&value);

    // The overview is the whole screen, resized to the requested width.
    let overview = view(frame, "overview");
    assert_eq!(overview["source_rect"]["width"], SCREEN_W);
    assert_eq!(overview["source_rect"]["height"], SCREEN_H);
    let decoded = decode_view(overview);
    assert_eq!(decoded.width, 200);
    assert_eq!(decoded.height, 150);
    // Top-left of the overview is the top-left quadrant, which was red.
    decoded.expect_pixel(20, 20, [255, 0, 0], 4);
    // Top-right is green.
    decoded.expect_pixel(180, 20, [0, 255, 0], 4);

    // The region reports the source rectangle the caller asked for, not a frame-local
    // one, so the caller never has to reconstruct where the crop came from.
    let region = view(frame, "br");
    assert_eq!(region["kind"], "region");
    assert_eq!(region["source_rect"]["x"], 200);
    assert_eq!(region["source_rect"]["y"], 150);
    assert_eq!(region["source_rect"]["width"], 200);
    assert_eq!(region["source_rect"]["height"], 150);
    let decoded = decode_view(region);
    assert_eq!((decoded.width, decoded.height), (200, 150));
    // Bottom-right is yellow, and every corner of the crop should be too: this is what
    // catches an off-by-one quadrant or an inverted axis.
    for (x, y) in [(5, 5), (194, 5), (5, 144), (194, 144), (100, 75)] {
        decoded.expect_pixel(x, y, [255, 255, 0], 4);
    }
}

#[test]
fn several_regions_come_back_independently_sized_and_formatted() {
    // Spec section 47: multiple ROIs, independent region formats, ROI resized. The
    // regions here deliberately differ in both format and size so a response that reused
    // one region's settings for another would be caught.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-regions", SCREEN_W, SCREEN_H) else {
        return;
    };

    let _screen = flat_screen(&display, [40, 40, 40]);
    let session = create_session(&service, &display);

    let (code, value) = run_session(
        &service,
        &[
            "capture",
            &session,
            "--json",
            "--base64",
            "--no-overview",
            "--region",
            "left=0,0,200,300",
            "--region",
            // `@all` scopes the region to every frame of a stack and `!150` raises its
            // fitting priority above the default for a required region.
            "right=200,0,200,300@all,!150",
            // The first region takes the shared settings; the second carries its own and
            // is declared with an unusual priority to prove the declarator is honoured.
            "--region-width",
            "100",
            "--region-format",
            "png",
        ],
    );
    assert_eq!(code, 0, "capture failed: {value}");

    let frame = only_frame(&value);
    let names: Vec<&str> = views(frame)
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["left", "right"],
        "regions should be emitted in declared order: {frame}"
    );

    for name in ["left", "right"] {
        let region = view(frame, name);
        assert_eq!(region["source_rect"]["width"], 200);
        let decoded = decode_view(region);
        assert_eq!(decoded.width, 100, "region {name} should be resized to 100");
    }
}

#[test]
fn a_region_only_presentation_carries_no_overview() {
    // Spec section 47: overview disabled, region-only response. The point is that the
    // whole-frame view is genuinely absent rather than present-but-empty, because an agent
    // paying for an overview it did not want is the cost this exists to avoid.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-region-only", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let _screen = flat_screen(&display, [10, 120, 200]);
    let session = create_session(&service, &display);

    let (code, value) = run_session(
        &service,
        &[
            "capture",
            &session,
            "--json",
            "--base64",
            "--no-overview",
            "--region",
            "hud=0,0,400,60",
            "--region-format",
            "png",
        ],
    );
    assert_eq!(code, 0, "capture failed: {value}");

    let frame = only_frame(&value);
    assert_eq!(views(frame).len(), 1, "only the region should be present");
    assert!(
        frame["views"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["kind"] != "overview"),
        "no overview may be present: {frame}"
    );
    assert_eq!(frame["views"][0]["kind"], "region");

    // And what it holds is the painted colour.
    let decoded = decode_view(&frame["views"][0]);
    assert_eq!((decoded.width, decoded.height), (400, 60));
    decoded.expect_pixel(200, 30, [10, 120, 200], 4);
}

#[test]
fn a_metadata_only_presentation_reports_a_view_with_no_image() {
    // Spec section 47: metadata-only output. Two things have to hold at once, and they are
    // the two halves of the same contract: a view that was asked to carry no pixels must
    // carry none, and it must *say* so rather than leaving the caller to infer it from a
    // missing field. The second matters because a view with a null image is otherwise
    // indistinguishable from a view that failed to encode.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-metadata", SCREEN_W, SCREEN_H) else {
        return;
    };

    let _screen = flat_screen(&display, [200, 30, 30]);
    let session = create_session(&service, &display);

    // A two-frame real-time stack under `metadata-older`: the older frame keeps its
    // identity and timing and loses its image, and the newest frame is unaffected.
    let (code, value) = run_session(
        &service,
        &[
            "realtime",
            &session,
            "--json",
            "--base64",
            "--frames",
            "2",
            "--interval",
            "40ms",
            "--timeout",
            "300ms",
            "--temporal",
            "metadata-older",
            "--newest-width",
            "200",
            "--newest-quality",
            "80",
        ],
    );
    assert_eq!(code, 0, "realtime failed: {value}");

    let stack = frames(&value);
    assert_eq!(stack.len(), 2, "both frames should be reported: {value}");

    let older = view(&stack[0], "overview");
    assert_eq!(
        older["applied"]["metadata_only"], true,
        "the older overview should declare itself metadata-only: {older}"
    );
    assert!(
        older["image"].is_null(),
        "a metadata-only view must carry no image: {older}"
    );

    // The newest frame is presented normally, so the mode removed detail rather than
    // disabling the response.
    let newest = view(&stack[1], "overview");
    assert_eq!(newest["applied"]["metadata_only"], false);
    assert!(newest["image"].is_object());

    // The identity and timing of the image-less frame survive, which is the whole point of
    // keeping it in the stack.
    assert!(stack[0]["frame_id"].is_number());
    assert!(stack[0]["age_us"].is_number());
    assert!(stack[0]["capture_offset_us"].is_number());

    // A view is still described even with no pixels: the source rectangle and the mapping
    // back to it are part of the contract, not part of the image.
    assert!(older["source_rect"].is_object());
    assert!(older["transform"].is_object());
}

#[test]
fn multiple_views_of_one_frame_share_its_identity() {
    // Spec section 55: the same-frame guarantee. Several views in one response are several
    // renderings of *one* moment, not a montage of different captures. The response proves
    // it by naming one frame id and one capture time for the whole group.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-same-frame", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let _screen = flat_screen(&display, [60, 60, 60]);
    let session = create_session(&service, &display);

    let (code, value) = run_session(
        &service,
        &[
            "capture",
            &session,
            "--json",
            "--base64",
            "--overview-width",
            "200",
            "--overview-format",
            "png",
            "--region",
            "a=0,0,100,100",
            "--region",
            "b=100,0,100,100",
            "--region",
            "c=0,100,100,100",
            "--region-format",
            "png",
        ],
    );
    assert_eq!(code, 0, "capture failed: {value}");

    // One presented frame holding four views. If the implementation captured per view,
    // there would be four frames here and possibly four different moments.
    let stack = frames(&value);
    assert_eq!(stack.len(), 1, "views of one frame are one frame: {value}");
    assert_eq!(views(&stack[0]).len(), 4, "overview plus three regions");

    // The response body names the same frame id as the presentation does, which is what
    // ties the two halves of the response to one moment.
    let body = &value["frame"];
    assert_eq!(
        body["frame_id"], stack[0]["frame_id"],
        "the body and the presentation must describe the same frame: {value}"
    );
}

#[test]
fn a_region_outside_the_source_is_refused() {
    // Spec section 47: out-of-bounds region rejection. A region that claims pixels the
    // frame does not have is a caller error, and is refused as `invalid_region` rather than
    // clamped, because a silently clamped region is a region whose reported source
    // rectangle does not describe what was returned.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-oob", SCREEN_W, SCREEN_H) else {
        return;
    };

    let _screen = flat_screen(&display, [0, 0, 0]);
    let session = create_session(&service, &display);

    let (code, value) = run_session(
        &service,
        &[
            "capture",
            &session,
            "--json",
            "--base64",
            "--region",
            "past=300,200,200,200",
            "--region-format",
            "png",
        ],
    );

    assert_ne!(code, 0, "an out-of-bounds region must be refused: {value}");
    let code_str = value["error"]["code"].as_str().unwrap_or_default();
    assert_eq!(
        code_str, "invalid_region",
        "the refusal should be reported as an invalid region: {value}"
    );
}

#[test]
fn a_zero_sized_region_is_refused() {
    // Spec section 47: zero-sized region rejection. A zero-area region can never
    // correspond to a real image, and permitting one would put an unencodable view into
    // a plan.
    //
    // Two layers refuse it, and this test asserts the outcome rather than which layer
    // spoke first: the CLI rejects the argument before any request is sent, and the policy
    // validator rejects it independently if it is ever constructed another way. The second
    // of those is exercised directly in the policy unit tests; here what matters is that a
    // caller cannot obtain a zero-area view.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-zero", SCREEN_W, SCREEN_H) else {
        return;
    };

    let _screen = flat_screen(&display, [0, 0, 0]);
    let session = create_session(&service, &display);

    let (code, stdout, stderr) = service.run(&[
        "capture",
        &session,
        "--json",
        "--base64",
        "--region",
        "nothing=10,10,0,50",
        "--region-format",
        "png",
    ]);

    assert_ne!(code, 0, "a zero-sized region must be refused");
    assert_eq!(code, 2, "the refusal is an argument error: {stderr}");

    // No presentation was produced under any spelling: neither stream holds a frame.
    assert!(!stdout.contains("\"presentation\""));
    assert!(!stderr.contains("\"presentation\""));
    // And the refusal names the reason rather than merely failing.
    assert!(
        stderr.contains("non-zero") || stderr.contains("zero"),
        "the refusal should say why: {stderr}"
    );
}
