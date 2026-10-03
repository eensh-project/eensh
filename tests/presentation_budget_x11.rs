//! Phase 6: payload budgets, temporal policies, and timing isolation (specification
//! sections 49 to 53).
//!
//! # Why these live against a real display
//!
//! The fitting ladder, its ordering, and its determinism are unit-tested against synthetic
//! frames, where a byte count is reproducible and an assertion can be exact. What those tests
//! cannot establish is the two claims that are about the *system* rather than the algorithm:
//!
//! 1. **Requirement 34's ordering.** Presentation happens after sampling and cannot influence
//!    it. A real-time request under a metadata-only policy and the same request under a heavy
//!    multi-view policy must sample *identically*: same frame count, same cadence, same skips,
//!    same sampling time. If presentation crept inside the sampling window — encoding a frame
//!    before the next slot was checked, say — the heavy policy would sample later or skip more.
//! 2. **Requirement 21's protection of the newest frame**, measured rather than asserted. The
//!    efficient policies must actually cost less, on a real scene, than sending every frame at
//!    full quality.
//!
//! # Reading a response
//!
//! ```text
//! { "kind": "realtime", "realtime": {...}, "presentation": { "frames": [...], "payload": {...} } }
//! ```

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;

const SCREEN_W: u32 = 400;
const SCREEN_H: u32 = 300;

/// A monotonically increasing stamp used to build region names unique to each test run.
///
/// The fitting order is a *total* key whose final tie-break is the view's name, so a test that
/// wants to observe determinism must not accidentally let a stale directory or a leftover
/// process change what the names are. Stamping them keeps every run self-contained.
static STAMP: AtomicU64 = AtomicU64::new(0);

fn stamp() -> u64 {
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    base ^ (STAMP.fetch_add(1, Ordering::Relaxed) << 32)
}

fn skip_if_no_xvfb() -> bool {
    if find_xvfb().is_none() {
        assert!(!xvfb_required(), "Xvfb is required but missing");
        return false;
    }
    true
}

/// Run a `session` subcommand and parse JSON from either stream.
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

fn presentation(value: &serde_json::Value) -> &serde_json::Value {
    let section = &value["presentation"];
    assert!(!section.is_null(), "no presentation in {value}");
    section
}

fn frames(value: &serde_json::Value) -> &Vec<serde_json::Value> {
    presentation(value)["frames"]
        .as_array()
        .expect("a frame stack")
}

/// Whether any of a frame's views carries pixels.
///
/// Computed from the views rather than read from a field, because the response does not carry a
/// per-frame summary of it: the frame's declared byte counts are the closest thing, and they are
/// not the same statement — a view can carry an image of zero bytes in principle, and a caller
/// reading a summary would have to trust it. Asking the views directly is the honest test.
fn frame_has_image(frame: &serde_json::Value) -> bool {
    frame["views"]
        .as_array()
        .expect("a view list")
        .iter()
        .any(|view| view["image"].is_object())
}

/// The summary section of a real-time body, which is itself called `realtime`.
fn sampling(value: &serde_json::Value) -> &serde_json::Value {
    &value["realtime"]["realtime"]
}

/// A real-time request with a given presentation policy, as extra CLI flags.
///
/// The sampling parameters are fixed, so the only thing that varies between two calls is the
/// presentation policy. That is the whole structure of the isolation test.
const SAMPLING: [&str; 6] = ["--frames", "4", "--interval", "40ms", "--timeout", "600ms"];

fn realtime(service: &ServiceProcess, session: &str, extra: &[&str]) -> (i32, serde_json::Value) {
    let mut args = vec!["realtime", session, "--json", "--base64"];
    args.extend_from_slice(&SAMPLING);
    args.extend_from_slice(extra);
    run_session(service, &args)
}

/// A static scene with real structure, held open for the test's duration.
///
/// Used wherever a budget is *derived* from one request and then applied to another. A moving scene
/// would make that comparison meaningless: the second request captures different pixels, so the
/// floor it is measured against is not the floor it is held to. An earlier version of
/// `every_frame_survives_a_budget_that_is_only_just_reachable` failed for exactly that reason,
/// reporting a floor of 19564 bytes for a budget derived from a different sample's floor of about
/// 13000.
fn static_scene(display: &str) -> Painter {
    let ops = (0..24)
        .map(|index| {
            let x = (index % 6) * 60;
            let y = (index / 6) * 60;
            let rgb = [
                ((index * 37) % 200 + 30) as u8,
                ((index * 71) % 200 + 30) as u8,
                ((index * 113) % 200 + 30) as u8,
            ];
            PaintOp {
                x,
                y,
                width: 50,
                height: 50,
                rgb,
            }
        })
        .collect();
    Painter::paint_once(display.to_string(), ops)
}

/// A scene that never holds still, so every frame differs and the scene is not degenerate.
///
/// A painter that keeps repainting a shifting pattern has two useful properties for these
/// tests: the frames genuinely differ (so a stack is a stack rather than one frame repeated),
/// and the JPEG encoder has real work to do, so a quality reduction actually changes the byte
/// count rather than hitting a constant.
///
/// The scene holds its `Painter` for its lifetime, which is the point: the painter holds an X
/// connection open, and Xvfb clears the root window when its last client disconnects. A scene
/// that let its painter go would vanish out from under the samples taken from it.
struct MovingScene {
    _painter: Painter,
}

impl MovingScene {
    fn start(display: &str) -> MovingScene {
        // Seven repaints across the sampling window, in different colours and positions, so a
        // frame is unlikely to coincide with another and the encoder cannot collapse them.
        let script = (0..7)
            .map(|index| {
                let x = (index * 52) % 300;
                let y = (index * 37) % 200;
                let rgb = [
                    ((index * 53) % 200 + 40) as u8,
                    ((index * 97) % 200 + 40) as u8,
                    ((index * 149) % 200 + 40) as u8,
                ];
                (
                    Duration::from_millis(20 + index as u64 * 35),
                    vec![PaintOp {
                        x,
                        y,
                        width: 90,
                        height: 70,
                        rgb,
                    }],
                )
            })
            .collect();
        MovingScene {
            _painter: Painter::start(display.to_string(), script),
        }
    }
}

impl Drop for MovingScene {
    fn drop(&mut self) {
        // The painter is stopped by its own `Drop`; nothing to do beyond making the intent
        // explicit that the scene must outlive every sample taken from it.
    }
}

// ============================================================================
// 52: timing isolation
// ============================================================================

#[test]
fn presentation_policy_cannot_influence_sampling() {
    // Specification section 52, and requirement 34. One raw Phase 5 real-time request is made
    // twice: once presenting nothing but metadata, and once presenting several images with a
    // budget that forces the fitter to re-encode. The sampling must be identical.
    //
    // A synthetic clock is not available here because the real-time path is driven by the
    // service's `SystemClock`; what replaces it is the *comparison*. Both requests are run
    // against the same live scene with the same cadence, so the honest claim is the one the
    // specification actually makes: presentation cannot influence the number of frames, the
    // reported cadence, the skip count, or the sampling window's shape. Wall-clock milliseconds
    // are compared only loosely for that reason, and everything discrete is compared exactly.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-isolation", SCREEN_W, SCREEN_H) else {
        return;
    };

    let _screen = Screen::open(&display);
    let session = create_session(&service, &display);

    // A: metadata only. This is the cheapest presentation the system can produce, so its
    // sampling is as close to a bare Phase 5 request as the CLI can express.
    let (code, light) = realtime(
        &service,
        &session,
        &["--temporal", "metadata-older", "--newest-width", "320"],
    );
    assert_eq!(code, 0, "the metadata-only request failed: {light}");

    // B: everything at once — an overview, three named regions, a changed-optional view, and a
    // budget tight enough to make the fitter work. This is the heaviest presentation in the
    // tests, deliberately chosen to maximise any leakage.
    let name_a = format!("alpha{}", stamp());
    let name_b = format!("beta{}", stamp());
    let name_c = format!("gamma{}", stamp());
    // A region's scope and priority are suffixes introduced by `@`, comma separated. Writing
    // them without the `@` makes the whole thing part of the coordinates, which is what the
    // first run of this test caught.
    let region_a = format!("{name_a}=0,0,200,150");
    let region_b = format!("{name_b}=200,0,200,150@all,!140");
    let region_c = format!("{name_c}=0,150,200,150");
    let (code, heavy) = realtime(
        &service,
        &session,
        &[
            "--temporal",
            "newest-detailed",
            "--overview-width",
            "320",
            "--region",
            &region_a,
            "--region",
            &region_b,
            "--region",
            &region_c,
            "--older-width",
            "160",
            "--older-quality",
            "60",
            "--newest-width",
            "320",
            "--newest-quality",
            "80",
            "--max-base64-bytes",
            "40000",
        ],
    );
    assert_eq!(code, 0, "the heavy request failed: {heavy}");

    // The two presentations really were different, or the comparison would be vacuous.
    let light_frames = frames(&light);
    let heavy_views: usize = frames(&heavy)
        .iter()
        .map(|f| f["views"].as_array().unwrap().len())
        .sum();
    let light_views: usize = light_frames
        .iter()
        .map(|f| f["views"].as_array().unwrap().len())
        .sum();
    assert!(
        heavy_views > light_views,
        "the heavy policy should produce more views than the metadata-only one: {heavy_views} vs \
         {light_views}"
    );
    assert!(
        presentation(&heavy)["total_base64_bytes"].as_u64().unwrap()
            > presentation(&light)["total_base64_bytes"].as_u64().unwrap(),
        "the heavy policy should produce more bytes"
    );

    // -- the sampling claims, compared exactly ------------------------------------------
    let light_sampling = sampling(&light);
    let heavy_sampling = sampling(&heavy);

    assert_eq!(
        light_sampling["result"], heavy_sampling["result"],
        "the outcome must not depend on the presentation policy"
    );
    assert_eq!(
        light_sampling["requested_frames"], heavy_sampling["requested_frames"],
        "the requested count is the caller's, not the policy's"
    );
    assert_eq!(
        light_sampling["captured_frames"], heavy_sampling["captured_frames"],
        "the same number of frames must be captured under either policy: {light_sampling} vs \
         {heavy_sampling}"
    );
    assert_eq!(
        light_sampling["scheduled_opportunities"], heavy_sampling["scheduled_opportunities"],
        "the cadence slots must be the same"
    );
    assert_eq!(
        light_sampling["skipped_opportunities"], heavy_sampling["skipped_opportunities"],
        "presentation must not cause a slot to be skipped: {light_sampling} vs {heavy_sampling}"
    );
    assert_eq!(
        light_sampling["interval_ms"], heavy_sampling["interval_ms"],
        "the interval is the one the caller asked for"
    );
    assert_eq!(
        light_sampling["timeout_ms"], heavy_sampling["timeout_ms"],
        "the deadline is the one the caller asked for"
    );

    // The sampling window's shape, as distinct from its wall-clock length. The offsets are what
    // make a stack temporal, and they must be at multiples of the interval under both.
    let offsets = |value: &serde_json::Value| -> Vec<u64> {
        frames(value)
            .iter()
            .filter_map(|f| f["capture_offset_us"].as_u64())
            .collect()
    };
    let light_offsets = offsets(&light);
    let heavy_offsets = offsets(&heavy);
    assert_eq!(
        light_offsets.len(),
        heavy_offsets.len(),
        "the two stacks should have the same depth"
    );
    // Offsets are not compared value-by-value: the two requests ran at different wall-clock
    // moments against a live scene, and the first slot's offset depends on how long the session
    // lock and the first capture took. What is compared is the *cadence*: consecutive gaps are
    // whole multiples of the interval, which is the scheduling rule rather than a coincidence.
    for (label, stack) in [("light", &light_offsets), ("heavy", &heavy_offsets)] {
        for pair in stack.windows(2) {
            let gap = pair[1] - pair[0];
            // A gap is one or more whole intervals. The tolerance is generous at the low end
            // because the scheduler compares its own `Duration`s while the response reports
            // whole microseconds, so a nominal 40ms slot can round to 39,999 or 40,001; an
            // assertion demanding exactly 40,000 would be asserting the rounding rather than
            // the schedule.
            assert!(
                gap >= 39_000,
                "a {label} gap of {gap}us is shorter than the requested 40ms interval: {stack:?}"
            );
            let remainder = gap % 40_000;
            assert!(
                !(2_000..=38_000).contains(&remainder),
                "a {label} gap of {gap}us is not a whole number of 40ms intervals: {stack:?}"
            );
        }
    }

    // -- and the timing must be allowed to differ --------------------------------------
    // Requirement 35: presentation time is reported separately from sampling time precisely
    // because the two are different quantities. The heavy policy must have spent more on
    // presentation; it is not required to have spent more on sampling, and asserting that it
    // did would be asserting a bug.
    assert!(
        presentation(&heavy)["timing"]["presentation_us"]
            .as_u64()
            .unwrap()
            >= presentation(&light)["timing"]["presentation_us"]
                .as_u64()
                .unwrap(),
        "the heavier presentation should not have taken less time: {heavy} vs {light}"
    );
}

// ============================================================================
// 49: temporal policies
// ============================================================================

#[test]
fn each_temporal_mode_renders_the_stack_the_way_it_says_it_does() {
    // Specification section 49. The four modes are compared on one live scene so that the
    // difference between them is a difference in the *policy* rather than in what was captured.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-temporal", SCREEN_W, SCREEN_H) else {
        return;
    };

    let session = create_session(&service, &display);
    let _scene = MovingScene::start(&display);

    // -- all-same: every frame identical settings --------------------------------------
    let (code, value) = realtime(
        &service,
        &session,
        &[
            "--temporal",
            "all-same",
            "--overview-width",
            "160",
            "--overview-format",
            "png",
        ],
    );
    assert_eq!(code, 0, "all-same failed: {value}");
    assert_eq!(
        presentation(&value)["temporal_mode"],
        "all_same",
        "the mode should be reported"
    );

    let stack = frames(&value);
    assert!(stack.len() >= 2, "a stack of several frames: {value}");
    let settings: Vec<(Option<u64>, Option<u64>)> = stack
        .iter()
        .map(|frame| {
            let overview = frame["views"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["name"] == "overview")
                .expect("an overview on every frame");
            (
                overview["applied"]["width"].as_u64(),
                overview["applied"]["quality"].as_u64(),
            )
        })
        .collect();
    assert!(
        settings.windows(2).all(|pair| pair[0] == pair[1]),
        "all-same should render every frame identically: {settings:?}"
    );

    // -- newest-detailed: the newest differs from the older frames ---------------------
    let (code, value) = realtime(
        &service,
        &session,
        &[
            "--temporal",
            "newest-detailed",
            "--older-width",
            "80",
            "--older-quality",
            "45",
            "--newest-width",
            "240",
            "--newest-quality",
            "85",
            "--overview-format",
            "jpeg",
        ],
    );
    assert_eq!(code, 0, "newest-detailed failed: {value}");
    assert_eq!(presentation(&value)["temporal_mode"], "newest_detailed");

    let stack = frames(&value);
    assert!(stack.len() >= 2, "a stack of several frames: {value}");
    let widths: Vec<Option<u64>> = stack
        .iter()
        .map(|frame| frame["views"][0]["applied"]["width"].as_u64())
        .collect();
    let newest_width = widths.last().copied().flatten();
    assert_eq!(newest_width, Some(240), "the newest frame keeps its detail");
    for (index, width) in widths[..widths.len() - 1].iter().enumerate() {
        assert_eq!(
            *width,
            Some(80),
            "older frame {index} should be reduced: {widths:?}"
        );
    }
    // The reduction is a detail decision, not an existential one: every frame is still
    // present, which is the regression the fitting work fixed.
    assert_eq!(
        stack.len(),
        sampling(&value)["captured_frames"].as_u64().unwrap() as usize,
        "every captured frame must appear: {value}"
    );

    // -- newest-only: only the newest carries an image ----------------------------------
    let (code, value) = realtime(
        &service,
        &session,
        &[
            "--temporal",
            "newest-only",
            "--newest-width",
            "200",
            "--newest-quality",
            "80",
            "--overview-format",
            "jpeg",
        ],
    );
    assert_eq!(code, 0, "newest-only failed: {value}");
    assert_eq!(presentation(&value)["temporal_mode"], "newest_only");

    let stack = frames(&value);
    let with_image: Vec<bool> = stack.iter().map(frame_has_image).collect();
    assert_eq!(
        with_image.last(),
        Some(&true),
        "the newest frame carries the image: {with_image:?}"
    );
    assert!(
        with_image[..with_image.len() - 1].iter().all(|has| !has),
        "only the newest frame should carry an image: {with_image:?}"
    );
    // And the frames are still *there*, which is the difference between newest-only and
    // returning one frame: the earlier moments are described, not discarded.
    assert!(
        stack.len() >= 2,
        "the earlier frames are still reported: {value}"
    );

    // -- metadata-older: the same shape, arrived at a different way --------------------
    let (code, value) = realtime(
        &service,
        &session,
        &[
            "--temporal",
            "metadata-older",
            "--newest-width",
            "200",
            "--newest-quality",
            "80",
            "--overview-format",
            "jpeg",
        ],
    );
    assert_eq!(code, 0, "metadata-older failed: {value}");
    assert_eq!(presentation(&value)["temporal_mode"], "metadata_older");

    let stack = frames(&value);
    for (index, frame) in stack[..stack.len() - 1].iter().enumerate() {
        let overview = frame["views"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "overview")
            .expect("an overview description on every frame");
        assert_eq!(
            overview["applied"]["metadata_only"], true,
            "older frame {index} should say it is metadata only: {overview}"
        );
        assert!(
            overview["image"].is_null(),
            "older frame {index} should carry no pixels"
        );
        // Identity and timing survive, which is the point of keeping the frame at all.
        assert!(frame["frame_id"].is_number() && frame["capture_offset_us"].is_number());
    }
    let newest = stack.last().unwrap();
    assert!(
        frame_has_image(newest),
        "the newest keeps its image: {newest}"
    );
}

#[test]
fn a_metadata_only_policy_produces_a_response_with_no_pixels_anywhere() {
    // The `--metadata-only` flag makes a claim about the whole response, not about one view, so
    // it is tested that way: no view anywhere, under any temporal mode, carries an image — and
    // every view still describes itself.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-metadata-all", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display);
    let _scene = MovingScene::start(&display);

    let name = format!("roi{}", stamp());
    let region = format!("{name}=50,50,150,100");
    let (code, value) = realtime(
        &service,
        &session,
        &[
            "--metadata-only",
            "--overview-width",
            "200",
            "--region",
            &region,
        ],
    );
    assert_eq!(code, 0, "the metadata-only request failed: {value}");

    let stack = frames(&value);
    assert!(
        !stack.is_empty(),
        "frames should still be reported: {value}"
    );
    let mut described = 0usize;
    for frame in stack {
        for view in frame["views"].as_array().unwrap() {
            assert!(
                view["image"].is_null(),
                "a metadata-only response carried an image: {view}"
            );
            assert_eq!(
                view["applied"]["metadata_only"], true,
                "the view should say it is metadata only: {view}"
            );
            assert!(view["source_rect"].is_object());
            assert!(view["transform"].is_object());
            described += 1;
        }
    }
    assert!(
        described >= 2,
        "the views should still be described: {value}"
    );

    // And the response says so in its own totals, which is the cheapest check an agent can
    // make before deciding whether it needs to ask for anything heavier.
    assert_eq!(
        presentation(&value)["total_base64_bytes"],
        0,
        "a metadata-only response should carry no base64 payload: {value}"
    );
    assert_eq!(presentation(&value)["total_encoded_bytes"], 0);
}

// ============================================================================
// 49: static stack and non-contiguous IDs
// ============================================================================

#[test]
fn a_temporal_policy_works_on_a_stack_whose_frames_are_identical() {
    // Specification section 49: static stack. Nothing moves, so every frame is the same pixels,
    // and the policy must still apply per position in the stack rather than collapsing the
    // identical frames into one.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-temporal-static", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    // Nothing paints at all, so the screen is whatever Xvfb starts with and stays that way.
    let _screen = Screen::open(&display);
    let session = create_session(&service, &display);

    let (code, value) = realtime(
        &service,
        &session,
        &[
            "--temporal",
            "newest-detailed",
            "--older-width",
            "100",
            "--newest-width",
            "300",
            "--overview-format",
            "jpeg",
        ],
    );
    assert_eq!(code, 0, "the static request failed: {value}");

    let stack = frames(&value);
    assert!(
        stack.len() >= 2,
        "a static scene must still produce the requested frames: {value}"
    );
    // Identical pixels must not be deduplicated: the fact that nothing moved is information, and
    // the policy's per-position treatment must be visible.
    let widths: Vec<Option<u64>> = stack
        .iter()
        .map(|frame| frame["views"][0]["applied"]["width"].as_u64())
        .collect();
    assert_eq!(widths.last().copied().flatten(), Some(300));
    for width in &widths[..widths.len() - 1] {
        assert_eq!(*width, Some(100));
    }
    // Frame identities are distinct, so the stack is a stack rather than one frame repeated.
    let ids: Vec<u64> = stack
        .iter()
        .map(|f| f["frame_id"].as_u64().unwrap())
        .collect();
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "each sample is its own frame: {ids:?}"
    );
}

#[test]
fn temporal_order_follows_the_stack_rather_than_numeric_frame_ids() {
    // Specification section 49: non-contiguous IDs. "Newest" means last in the sampling order,
    // not largest frame id — and the two differ whenever other captures have happened in
    // between, which is the normal case for a session that is being used for anything else.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-temporal-gap", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let _screen = Screen::open(&display);
    let session = create_session(&service, &display);

    // Interleave single captures with the stack, so the stack's frame ids are not contiguous.
    let (_, first) = run_session(&service, &["capture", &session, "--json"]);
    assert!(first["frame"]["frame_id"].is_number());

    let (code, value) = realtime(
        &service,
        &session,
        &[
            "--temporal",
            "newest-detailed",
            "--older-width",
            "100",
            "--newest-width",
            "300",
            "--overview-format",
            "jpeg",
        ],
    );
    assert_eq!(code, 0, "the request failed: {value}");

    let stack = frames(&value);
    let ids: Vec<u64> = stack
        .iter()
        .map(|f| f["frame_id"].as_u64().unwrap())
        .collect();
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "the stack should be in capture order: {ids:?}"
    );
    // The stack does not start at 1, because a capture happened first. This is what makes the
    // test meaningful: the newest frame is the last *sample*, not the largest possible id.
    assert!(
        ids[0] > 1,
        "the stack should not start at the first frame of the session: {ids:?}"
    );

    // The reduced frames are the ones that are not last in the stack, which is the claim that
    // matters: had the policy keyed off frame id arithmetic, the interleaving would show up as
    // the wrong frame keeping its detail.
    let widths: Vec<Option<u64>> = stack
        .iter()
        .map(|f| f["views"][0]["applied"]["width"].as_u64())
        .collect();
    assert_eq!(
        widths.last().copied().flatten(),
        Some(300),
        "the last sample keeps its detail: {widths:?}"
    );
    assert!(
        widths[..widths.len() - 1].iter().all(|w| *w == Some(100)),
        "every earlier sample is reduced, whatever its id: {widths:?}"
    );

    // The session's own report agrees that the newest sample is the stack's last frame.
    assert_eq!(
        value["realtime"]["newest_frame_id"].as_u64(),
        ids.last().copied(),
        "the reported newest frame is the last in the stack: {value}"
    );
}

// ============================================================================
// 50: payload budget against a real scene
// ============================================================================

#[test]
fn a_budget_below_the_floor_is_refused_with_the_numbers_that_explain_it() {
    // Specification section 50, final scenario. A budget that cannot hold even the required
    // views must fail with `payload_budget_exceeded` — and must not be served by quietly
    // dropping one of the frames the caller asked for, which is the defect this behaviour was
    // strengthened to prevent.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-budget-floor", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display);
    let _scene = MovingScene::start(&display);

    let (code, value) = realtime(
        &service,
        &session,
        &[
            "--temporal",
            "newest-detailed",
            "--overview-width",
            "320",
            "--overview-format",
            "png",
            "--newest-width",
            "320",
            "--max-base64-bytes",
            "64",
        ],
    );

    assert_ne!(code, 0, "an impossible budget must be refused: {value}");
    assert_eq!(
        value["error"]["code"].as_str().unwrap_or_default(),
        "payload_budget_exceeded",
        "the refusal should name the budget: {value}"
    );

    let message = value["error"]["message"].as_str().unwrap_or_default();
    // The message has to be actionable: the caller can only choose a new budget if it is told
    // what the smallest achievable one is.
    assert!(
        message.contains("smallest achievable payload"),
        "the refusal should quantify the shortfall: {message}"
    );
    assert!(
        message.contains("64"),
        "the refusal should quote the budget it was given: {message}"
    );
}

#[test]
fn every_frame_survives_a_budget_that_is_only_just_reachable() {
    // The regression, established against a live display rather than on synthetic frames. With
    // a budget that requires the whole ladder to be spent, the response must still contain every
    // frame the caller asked for. A caller that asks for four frames and receives one has been
    // silently lied to, because what came back looks complete.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-budget-frames", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display);
    // A static scene, because this test derives its budget from one request's measured floor and
    // then holds a second request to it. With moving pixels the two requests see different content
    // and the comparison is not the one the test claims to be making.
    let _scene = static_scene(&display);

    // Find the floor by asking for an impossible budget and reading the answer.
    let name = format!("roi{}", stamp());
    let region = format!("{name}=0,0,120,90");
    let (_, refused) = realtime(
        &service,
        &session,
        &[
            "--temporal",
            "newest-detailed",
            "--overview-width",
            "200",
            "--overview-format",
            "jpeg",
            "--region",
            &region,
            "--region-format",
            "jpeg",
            "--older-width",
            "100",
            "--older-quality",
            "50",
            "--newest-width",
            "200",
            "--newest-quality",
            "80",
            "--max-base64-bytes",
            "1",
        ],
    );
    let message = refused["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let floor: usize = message
        .split("smallest achievable payload is ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("could not read the floor from {message:?}"));

    // A budget just above the floor: reachable, but only with everything spent.
    let budget = (floor + 40).to_string();
    let (code, value) = realtime(
        &service,
        &session,
        &[
            "--temporal",
            "newest-detailed",
            "--overview-width",
            "200",
            "--overview-format",
            "jpeg",
            "--region",
            &region,
            "--region-format",
            "jpeg",
            "--older-width",
            "100",
            "--older-quality",
            "50",
            "--newest-width",
            "200",
            "--newest-quality",
            "80",
            "--max-base64-bytes",
            &budget,
        ],
    );
    assert_eq!(code, 0, "a budget of {budget} should be reachable: {value}");

    let captured = sampling(&value)["captured_frames"].as_u64().unwrap() as usize;
    let stack = frames(&value);
    assert_eq!(
        stack.len(),
        captured,
        "every captured frame must appear in the presentation: {value}"
    );

    // Each one kept its overview, which is the specific thing that was being dropped.
    for (index, frame) in stack.iter().enumerate() {
        assert!(
            frame["views"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == "overview"),
            "frame {index} lost its overview: {frame}"
        );
    }

    // And the fitter stayed inside its budget, which is the other half of the contract.
    let payload = &presentation(&value)["payload"];
    assert!(
        payload["actual_base64_bytes"].as_u64().unwrap()
            <= payload["budget_base64_bytes"].as_u64().unwrap(),
        "the fitted payload exceeded its budget: {payload}"
    );
    assert_eq!(
        payload["actual_base64_bytes"].as_u64().unwrap(),
        presentation(&value)["total_base64_bytes"].as_u64().unwrap(),
        "the fit report and the response totals must agree: {payload}"
    );
}

#[test]
fn a_generous_budget_leaves_the_presentation_untouched() {
    // Specification section 50, first scenario, and requirement 24: adaptation is opt-in. Given
    // room to spare, the fitter must not take any of it — an opportunistically degraded response
    // is a response the caller cannot reproduce.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-budget-exact", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display);
    let _scene = MovingScene::start(&display);

    let (code, value) = realtime(
        &service,
        &session,
        &[
            "--temporal",
            "newest-detailed",
            "--overview-width",
            "200",
            "--overview-format",
            "jpeg",
            "--older-width",
            "100",
            "--older-quality",
            "60",
            "--newest-width",
            "200",
            "--newest-quality",
            "80",
            "--max-base64-bytes",
            "10000000",
        ],
    );
    assert_eq!(code, 0, "the request failed: {value}");

    let payload = &presentation(&value)["payload"];
    assert_eq!(
        payload["fit"], "exact",
        "a generous budget must not cause any adjustment: {payload}"
    );
    assert_eq!(
        payload["adjustments"].as_array().unwrap().len(),
        0,
        "no adjustment should be recorded: {payload}"
    );

    // And the settings are exactly the ones asked for.
    let stack = frames(&value);
    for frame in &stack[..stack.len() - 1] {
        assert_eq!(frame["views"][0]["applied"]["width"], 100);
        assert_eq!(frame["views"][0]["applied"]["quality"], 60);
    }
    let newest = stack.last().unwrap();
    assert_eq!(newest["views"][0]["applied"]["width"], 200);
    assert_eq!(newest["views"][0]["applied"]["quality"], 80);
}

#[test]
fn a_tighter_budget_keeps_every_frame_and_never_touches_the_newest_first() {
    // Specification section 50, second scenario, and requirement 21 against a live scene.
    //
    // The budget is derived from a measurement of *the same policy*, which is the only way the
    // comparison means anything: an earlier version of this test measured a cheap policy and
    // budgeted a heavy one, and the two were not comparable at all. What the assertions then
    // claim is deliberately weaker than "the newest is untouched", because Xvfb's screen is
    // nearly uniform and JPEG headers dominate a payload that small: the ladder frequently has
    // almost nothing to gain, and a test that demanded a saving would be asserting something
    // about the test pattern rather than about the fitter. The invariants that do hold are
    // asserted instead, and the ladder's *ordering* is established exactly by the unit tests in
    // `src/presentation/budget.rs`, where the frames are textured and a byte count is a fact.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-budget-older", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display);
    // A static scene with structure, for the same reason as the floor test below: the budget is
    // derived from a measured payload and then applied to a second request, so the two have to see
    // the same content for the comparison to mean anything.
    let _scene = static_scene(&display);

    let policy: Vec<&str> = vec![
        "--temporal",
        "newest-detailed",
        "--overview-width",
        "320",
        "--overview-format",
        "jpeg",
        "--older-width",
        "320",
        "--older-quality",
        "85",
        "--newest-width",
        "320",
        "--newest-quality",
        "85",
    ];

    // First, what this exact stack costs unfitted.
    let (code, reference) = realtime(&service, &session, &policy);
    assert_eq!(code, 0, "the reference request failed: {reference}");
    let full = presentation(&reference)["total_base64_bytes"]
        .as_u64()
        .unwrap();
    assert!(
        full > 1_000,
        "the reference payload should be substantial: {full}"
    );

    // Now the same request under a budget most of the way to nothing. Whether it is reachable
    // depends on how much the older frames can shed, so a refusal is accepted — but a *success*
    // must satisfy every promise the fitting makes.
    let budget = (full / 2).to_string();
    let mut fitted_flags = policy.clone();
    fitted_flags.push("--max-base64-bytes");
    fitted_flags.push(&budget);
    let (code, value) = realtime(&service, &session, &fitted_flags);

    if code != 0 {
        // A refusal is allowed, and must be the documented one rather than a short stack.
        assert_eq!(
            value["error"]["code"].as_str().unwrap_or_default(),
            "payload_budget_exceeded",
            "a too-small budget should be refused as a budget failure: {value}"
        );
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("smallest achievable payload"),
            "the refusal should quantify the shortfall: {value}"
        );
        return;
    }

    let payload = &presentation(&value)["payload"];
    assert_eq!(
        payload["fit"], "adjusted",
        "a budget of half the unfitted payload should have required fitting: {payload}"
    );
    assert!(
        payload["actual_base64_bytes"].as_u64().unwrap()
            <= payload["budget_base64_bytes"].as_u64().unwrap(),
        "the fitted payload exceeded its budget: {payload}"
    );
    // The two independent statements of the payload size must agree, or one of them is lying.
    assert_eq!(
        payload["actual_base64_bytes"].as_u64().unwrap(),
        presentation(&value)["total_base64_bytes"].as_u64().unwrap(),
        "the fit report and the response totals must agree: {payload}"
    );

    // Every captured frame survives, which is the regression this whole area was reworked for.
    let stack = frames(&value);
    assert_eq!(
        stack.len(),
        sampling(&value)["captured_frames"].as_u64().unwrap() as usize,
        "every captured frame must appear: {value}"
    );
    for (index, frame) in stack.iter().enumerate() {
        assert!(
            frame["views"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == "overview"),
            "frame {index} lost its overview: {frame}"
        );
    }

    // And if the newest frame was touched at all, every older view must be at the end of its
    // ladder. That is the protection requirement 21 describes, and it is checkable without
    // knowing what the scene compressed to.
    let newest_id = value["realtime"]["newest_frame_id"].as_u64().unwrap();
    let adjustments = payload["adjustments"].as_array().unwrap();
    let newest_touched = adjustments
        .iter()
        .any(|adjustment| adjustment["frame_id"].as_u64() == Some(newest_id));
    if newest_touched {
        // No older view may still be at its requested quality, since quality is the first lever
        // and every older frame has one.
        let requested_quality = 85u64;
        for (index, frame) in stack[..stack.len() - 1].iter().enumerate() {
            let overview = frame["views"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["name"] == "overview")
                .expect("an overview");
            if overview["applied"]["format"] == "jpeg" {
                assert_ne!(
                    overview["applied"]["quality"].as_u64(),
                    Some(requested_quality),
                    "the newest was degraded while older frame {index} kept its quality: {payload}"
                );
            }
        }
    }
}

#[test]
fn an_optional_region_is_dropped_before_a_required_one_is_degraded() {
    // Specification section 50, "required ROI protected": optional content drops first.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-budget-optional", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display);
    let _scene = MovingScene::start(&display);

    let required = format!("needed{}=0,0,200,150", stamp());
    let required_name = required[..required.find('=').unwrap()].to_string();
    // The second region is declared expendable by giving it a priority far below the default
    // that a required region receives. It is dropped only once the fitter has nothing else left.
    let optional = format!("spare{}=0,150,200,150@!20", stamp());
    let optional_name = optional[..optional.find('=').unwrap()].to_string();

    let mut policy: Vec<&str> = vec![
        "--overview-width",
        "200",
        "--overview-format",
        "jpeg",
        "--region",
        &required,
        "--region",
        &optional,
        "--region-format",
        "jpeg",
        "--region-width",
        "200",
    ];

    // What the stack costs with nothing fitted.
    let (code, reference) = realtime(&service, &session, &policy);
    assert_eq!(code, 0, "the reference request failed: {reference}");
    let full = presentation(&reference)["total_base64_bytes"]
        .as_u64()
        .unwrap();
    assert!(
        full > 1_000,
        "the reference payload should be substantial: {full}"
    );

    // A budget with most of the room taken away, so the fitter has to spend whatever it can.
    let budget = ((full as f64 * 0.6) as u64).to_string();
    policy.push("--max-base64-bytes");
    policy.push(&budget);
    let (code, value) = realtime(&service, &session, &policy);

    if code != 0 {
        assert_eq!(
            value["error"]["code"].as_str().unwrap_or_default(),
            "payload_budget_exceeded",
            "a too-small budget should be refused as a budget failure: {value}"
        );
        return;
    }

    let payload = &presentation(&value)["payload"];
    assert!(
        payload["actual_base64_bytes"].as_u64().unwrap()
            <= payload["budget_base64_bytes"].as_u64().unwrap(),
        "the fitted payload exceeded its budget: {payload}"
    );

    // Whatever the ladder chose, the overview and the required region are still present and still
    // images. This is the guarantee: required content is degraded at most, never silently removed.
    for frame in frames(&value) {
        let views = frame["views"].as_array().unwrap();
        assert!(
            views
                .iter()
                .any(|v| v["name"] == "overview" && v["image"].is_object()),
            "the overview should survive: {frame}"
        );
        assert!(
            views
                .iter()
                .any(|v| v["name"] == required_name.as_str() && v["image"].is_object()),
            "the required region should survive: {frame}"
        );
    }

    // And no adjustment may record the required region as omitted. The optional one may be.
    let dropped_required =
        payload["adjustments"].as_array().unwrap().iter().any(|a| {
            a["change"] == "omitted" && a["view"].as_str() == Some(required_name.as_str())
        });
    assert!(
        !dropped_required,
        "a required region must never be omitted: {payload}"
    );
    let _ = optional_name;
}

// ============================================================================
// 51: determinism, on a real scene
// ============================================================================

#[test]
fn fitting_the_same_raw_frames_repeatedly_produces_the_same_payload() {
    // Specification section 51, against a live scene rather than synthetic frames. The frames are
    // captured once and re-presented through `frame`, which is what makes this a determinism test
    // rather than a sampling test: the *same* raw frames meet the *same* policy several times.
    //
    // The claim is that the fitted result depends on nothing outside its inputs — not on a
    // hash map's iteration order, not on the order the regions happened to be declared in beyond
    // the declared order itself.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-determinism", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let session = create_session(&service, &display);
    let _scene = MovingScene::start(&display);

    // Capture a frame and keep its id, so the same raw pixels are presented repeatedly.
    let (code, captured) = run_session(&service, &["capture", &session, "--json"]);
    assert_eq!(code, 0, "capture failed: {captured}");
    let frame_id = captured["frame"]["frame_id"].as_u64().unwrap();
    let frame_arg = frame_id.to_string();

    // Deliberately awkward: three regions declared in an order that does not match their names,
    // their priorities, or their positions, so an implementation that sorts by iteration order
    // rather than by a total key produces a different answer somewhere. A region's suffix is
    // introduced by `@` and may carry both a scope and a priority, comma separated.
    let zulu = format!("zulu{}=0,0,120,90", stamp());
    let alpha = format!("alpha{}=130,0,120,90@!210", stamp());
    let mike = format!("mike{}=0,100,120,90", stamp());
    let zulu_name = zulu[..zulu.find('=').unwrap()].to_string();
    let alpha_name = alpha[..alpha.find('=').unwrap()].to_string();
    let mike_name = mike[..mike.find('=').unwrap()].to_string();

    let mut signature: Option<Vec<String>> = None;
    for attempt in 0..5 {
        let (code, value) = run_session(
            &service,
            &[
                "frame",
                &session,
                &frame_arg,
                "--json",
                "--base64",
                "--overview-width",
                "200",
                "--overview-format",
                "jpeg",
                "--region",
                &zulu,
                "--region",
                &alpha,
                "--region",
                &mike,
                "--region-format",
                "jpeg",
                "--region-width",
                "120",
                "--max-base64-bytes",
                "8000",
            ],
        );
        assert_eq!(code, 0, "attempt {attempt} failed: {value}");

        // A signature covering everything the specification requires to be identical: the
        // selected views, their dimensions, formats, quality, whether they carry pixels, and the
        // payload total.
        let mut parts: Vec<String> = Vec::new();
        for frame in frames(&value) {
            parts.push(format!("frame={}", frame["frame_id"]));
            for view in frame["views"].as_array().unwrap() {
                parts.push(format!(
                    "view={}/{}/{:?}/{:?}/{:?}/image={}",
                    view["name"],
                    view["kind"],
                    view["applied"]["width"],
                    view["applied"]["quality"],
                    view["applied"]["format"],
                    view["image"].is_object(),
                ));
            }
        }
        parts.push(format!(
            "payload={}/{}",
            presentation(&value)["payload"]["fit"],
            presentation(&value)["payload"]["actual_base64_bytes"]
        ));

        match &signature {
            None => signature = Some(parts),
            Some(first) => assert_eq!(
                first, &parts,
                "attempt {attempt} produced a different presentation from the same frames and \
                 policy"
            ),
        }
    }

    // The names really were unusual, so the signature cannot have matched by being trivially
    // empty or by all three regions having been dropped.
    let parts = signature.expect("at least one attempt");
    for name in [&zulu_name, &alpha_name, &mike_name] {
        // The name is rendered with `{:?}` in the signature, so it appears quoted there.
        let needle = format!("view={name:?}/");
        assert!(
            parts.iter().any(|part| part.starts_with(&needle)),
            "region {name} should appear in the signature: {parts:?}"
        );
    }
}
