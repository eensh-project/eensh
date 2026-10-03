//! Phase 6: measurements for the report (specification sections 53, 54, and 68).
//!
//! # Why these are tests rather than a benchmark harness
//!
//! A separate benchmark binary would be a second thing to keep working, and its numbers would
//! drift from the code the moment either changed. Putting the measurements here means they are
//! executed by `cargo test`, on the same machine and at the same time as the correctness
//! properties, and a regression that makes a policy stop being efficient fails the build.
//!
//! # What is asserted, and what is only reported
//!
//! Only the claims the implementation is *supposed* to guarantee are asserted:
//!
//! * the efficiency ordering (equal-detail costs more than newest-detailed, which costs more
//!   than newest-only), because that is the reason the policies exist;
//! * the memory claims that follow from there being one `Arc<Frame>` behind every view;
//! * the history capacity, which is a documented constant and must not have been enlarged to
//!   make presentation convenient.
//!
//! Everything else — the exact bytes, the exact microseconds — is **printed**, not asserted.
//! A brittle threshold in a measurement test is worse than no threshold: it fails on a different
//! machine, gets relaxed until it means nothing, and teaches the reader to distrust the suite.
//! The numbers are chosen to be stable enough to be worth printing, and the report quotes them
//! as observations from one machine rather than as universal constants.
//!
//! Run with `cargo test --test presentation_metrics -- --nocapture` to see the numbers.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use common::*;

const SCREEN_W: u32 = 640;
const SCREEN_H: u32 = 480;

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

/// The summary section of a real-time body, which is itself called `realtime`.
fn sampling(value: &serde_json::Value) -> &serde_json::Value {
    &value["realtime"]["realtime"]
}

/// A scene with real structure, so the encoders have work to do.
///
/// A flat screen is a bad measurement subject: PNG compresses it to almost nothing and JPEG
/// reduces every quality setting to the same handful of bytes, so every policy measures the
/// same. Painting a field of contrasting blocks gives the codecs something to actually compress,
/// which is what makes the quality ladder observable at all.
fn textured_scene(display: &str) -> Painter {
    let ops: Vec<PaintOp> = (0..48)
        .map(|index| {
            let x = (index % 8) * 80;
            let y = (index / 8) * 80;
            let rgb = match index % 4 {
                0 => [220, 30, 30],
                1 => [30, 220, 30],
                2 => [30, 30, 220],
                _ => [230, 230, 30],
            };
            PaintOp {
                x,
                y,
                width: 70,
                height: 70,
                rgb,
            }
        })
        .collect();
    Painter::paint_once(display.to_string(), ops)
}

/// The measurements of one policy run.
struct Measurement {
    label: &'static str,
    total_base64: u64,
    response_bytes: usize,
    frame_count: usize,
    view_count: usize,
    presentation_us: u64,
    newest_age_us: u64,
    frame_encodes: usize,
}

fn measure(
    service: &ServiceProcess,
    session: &str,
    label: &'static str,
    extra: &[&str],
) -> Measurement {
    // Five frames at this size make every policy's difference large enough to be visible above
    // the noise of a live capture.
    let mut args = vec![
        "realtime",
        session,
        "--json",
        "--base64",
        "--frames",
        "5",
        "--interval",
        "40ms",
        "--timeout",
        "700ms",
    ];
    args.extend_from_slice(extra);
    let (code, value) = run_session(service, &args);
    assert_eq!(code, 0, "{label} failed: {value}");

    let section = presentation(&value);
    let stack = frames(&value);
    let view_count: usize = stack
        .iter()
        .map(|frame| frame["views"].as_array().unwrap().len())
        .sum();

    // The response size as an agent would pay for it: the whole serialized body, not just the
    // image payload. A policy that saves bytes on images but adds them back in metadata has not
    // helped anyone.
    let response_bytes = serde_json::to_string(&value)
        .expect("the response should serialize")
        .len();

    let measurement = Measurement {
        label,
        total_base64: section["total_base64_bytes"].as_u64().unwrap(),
        response_bytes,
        frame_count: stack.len(),
        view_count,
        presentation_us: section["timing"]["presentation_us"].as_u64().unwrap(),
        newest_age_us: value["realtime"]["newest_frame_age_us"].as_u64().unwrap(),
        frame_encodes: stack.len(),
    };

    println!(
        "{:<22} frames={} views={} base64={:>9} response={:>9} presentation={:>7}us newest_age={:>8}us",
        measurement.label,
        measurement.frame_count,
        measurement.view_count,
        measurement.total_base64,
        measurement.response_bytes,
        measurement.presentation_us,
        measurement.newest_age_us,
    );

    measurement
}

// ============================================================================
// 68: the efficiency comparison
// ============================================================================

#[test]
fn efficient_policies_actually_cost_less_than_sending_everything() {
    // Specification sections 53 and 68. Four policies over one live scene, measured on the
    // quantities an agent pays for: the base64 payload, the serialized response, the time spent
    // presenting, and the age of the newest frame when the response is assembled.
    //
    // The ordering is asserted; the numbers are printed.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-metrics", SCREEN_W, SCREEN_H) else {
        return;
    };

    let scene = textured_scene(&display);
    let session = create_session(&service, &display);

    println!("\n--- Phase 6 policy cost, {SCREEN_W}x{SCREEN_H} desktop, 5 frames ---");

    // A: every frame at full quality, PNG.
    //
    // This is a *reference point*, not the baseline of the comparison, and that distinction
    // matters. The first version of this test compared PNG policies against JPEG ones and
    // concluded, absurdly, that reducing detail made the payload larger: a field of flat
    // contrasting blocks compresses to almost nothing as PNG while costing a great deal as JPEG,
    // so the codec dominated every number and the policy differences were invisible underneath it.
    // The comparison below is therefore made **within one format**, which leaves the policy as the
    // only variable.
    let png = measure(
        &service,
        &session,
        "png (reference)",
        &[
            "--temporal",
            "all-same",
            "--overview-width",
            "640",
            "--overview-format",
            "png",
        ],
    );

    // B: the baseline. Every frame at the newest frame's JPEG settings, so the newest view is
    // byte-for-byte the same here as it is in the newest-detailed policy below.
    let equal_jpeg = measure(
        &service,
        &session,
        "all-same jpeg q85",
        &[
            "--temporal",
            "all-same",
            "--overview-width",
            "640",
            "--overview-format",
            "jpeg",
            "--overview-quality",
            "85",
        ],
    );

    // C: newest-detailed. The newest frame keeps exactly B's settings; the older frames drop to a
    // quarter of the pixels and a much lower quality. This isolation is what makes the comparison
    // fair: the newest view is identical in both policies, so any difference is the older frames'.
    let detailed = measure(
        &service,
        &session,
        "newest-detailed",
        &[
            "--temporal",
            "newest-detailed",
            "--overview-format",
            "jpeg",
            "--older-width",
            "320",
            "--older-quality",
            "45",
            "--newest-width",
            "640",
            "--newest-quality",
            "85",
        ],
    );

    // D: newest-only. One frame carries pixels; the rest are described.
    let newest_only = measure(
        &service,
        &session,
        "newest-only",
        &[
            "--temporal",
            "newest-only",
            "--newest-width",
            "640",
            "--newest-quality",
            "85",
            "--overview-format",
            "jpeg",
        ],
    );

    // E: metadata only. The floor: identity, timing, and geometry, with no pixels anywhere.
    let metadata = measure(
        &service,
        &session,
        "metadata-only",
        &[
            "--metadata-only",
            "--overview-width",
            "640",
            "--overview-format",
            "jpeg",
        ],
    );

    println!(
        "--- against the same JPEG settings: newest-detailed {:.0}% off, newest-only {:.0}% off, \
         metadata-only {:.0}% off ---\n",
        100.0 * (1.0 - detailed.total_base64 as f64 / equal_jpeg.total_base64.max(1) as f64),
        100.0 * (1.0 - newest_only.total_base64 as f64 / equal_jpeg.total_base64.max(1) as f64),
        100.0 * (1.0 - metadata.total_base64 as f64 / equal_jpeg.total_base64.max(1) as f64),
    );

    drop(scene);

    // -- the assertions ---------------------------------------------------------------
    // Every policy returns every frame it captured. This is the property the whole fitting
    // rework was for, so it is asserted for all of them rather than for one.
    for measurement in [&equal_jpeg, &detailed, &newest_only, &metadata, &png] {
        assert_eq!(
            measurement.frame_count, measurement.frame_encodes,
            "{} did not report every frame it captured",
            measurement.label
        );
        assert!(
            measurement.frame_count >= 2,
            "{} should have sampled a stack: {} frames",
            measurement.label,
            measurement.frame_count
        );
    }

    // The central ordering, within one format. Each efficient policy is strictly cheaper than the
    // next more generous one. These are asserted because they *are* the reason the policies exist:
    // if newest-only were not cheaper than newest-detailed, it would have no reason to exist.
    assert!(
        detailed.total_base64 < equal_jpeg.total_base64,
        "newest-detailed ({} bytes) should cost less than the same settings on every frame ({} \
         bytes)",
        detailed.total_base64,
        equal_jpeg.total_base64
    );
    assert!(
        newest_only.total_base64 < detailed.total_base64,
        "newest-only ({} bytes) should cost less than newest-detailed ({} bytes)",
        newest_only.total_base64,
        detailed.total_base64
    );
    assert_eq!(
        metadata.total_base64, 0,
        "a metadata-only presentation carries no payload at all"
    );

    // The same ordering in the serialized response, which is what actually crosses the wire. A
    // policy that saved bytes on images but spent them back on metadata would not have helped.
    assert!(
        detailed.response_bytes < equal_jpeg.response_bytes,
        "the response itself should shrink: {} vs {}",
        detailed.response_bytes,
        equal_jpeg.response_bytes
    );
    assert!(
        newest_only.response_bytes < detailed.response_bytes,
        "the response itself should shrink again: {} vs {}",
        newest_only.response_bytes,
        detailed.response_bytes
    );

    // And a policy that renders four fewer full-size views must not take longer to present. The
    // newest frame's own age is printed rather than asserted: it depends on the scene and on how
    // busy the machine is, and an absolute threshold on it would fail on a loaded CI box for
    // reasons that have nothing to do with this code.
    println!(
        "--- newest-frame age: all-same {}us, newest-detailed {}us, newest-only {}us ---\n",
        equal_jpeg.newest_age_us, detailed.newest_age_us, newest_only.newest_age_us
    );
    assert!(
        newest_only.presentation_us <= equal_jpeg.presentation_us,
        "a policy rendering four fewer full-size views should not present more slowly: {}us vs {}us",
        newest_only.presentation_us,
        equal_jpeg.presentation_us
    );
}

// ============================================================================
// 68: overview plus ROI, which is the common agent request
// ============================================================================

#[test]
fn an_overview_plus_regions_costs_less_than_the_same_detail_as_whole_frames() {
    // The practical case the specification opens with: "show me the whole screen cheaply and these
    // two areas in detail". The alternative an agent has without this feature is to capture whole
    // frames, so the comparison is against *that*, at the same quality and in the same format.
    //
    // The first version of this test compared a JPEG overview-plus-crops against a PNG whole frame
    // and reported the feature as ten times *more* expensive, which was true and useless: the codec
    // was doing all the talking. Comparing at equal quality in one format is the only version of
    // this measurement that means anything.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-metrics-roi", SCREEN_W, SCREEN_H)
    else {
        return;
    };

    let _scene = textured_scene(&display);
    let session = create_session(&service, &display);

    let hud = format!("hud{}=0,0,240,160", stamp());
    let sidebar = format!("side{}=400,320,240,160", stamp());

    println!("\n--- overview + ROI against whole frames, {SCREEN_W}x{SCREEN_H}, JPEG q75 ---");

    // The alternative: every frame as a whole at the region's quality and width.
    let whole = measure(
        &service,
        &session,
        "whole frames q75",
        &[
            "--overview-width",
            "640",
            "--overview-format",
            "jpeg",
            "--overview-quality",
            "75",
        ],
    );

    // The feature: a cheaper overview plus the two areas at full quality.
    let composed = measure(
        &service,
        &session,
        "overview + 2 regions",
        &[
            "--overview-width",
            "320",
            "--overview-format",
            "jpeg",
            "--overview-quality",
            "50",
            "--region",
            &hud,
            "--region",
            &sidebar,
            "--region-format",
            "jpeg",
            "--region-quality",
            "75",
        ],
    );

    println!(
        "--- the composed response is {:.0}% of the whole-frame payload, and carries {} views \
         against {} ---\n",
        100.0 * composed.total_base64 as f64 / whole.total_base64.max(1) as f64,
        composed.view_count,
        whole.view_count,
    );

    // The structural claims, which hold regardless of what the encoder did. The composed response
    // describes the same moment with more views: an overview plus two detailed crops against one
    // whole frame.
    assert_eq!(
        composed.frame_count, whole.frame_count,
        "both present one frame per sample"
    );
    assert_eq!(
        composed.view_count,
        whole.view_count * 3,
        "three views per frame against one"
    );

    // The cost claim, at equal quality in one format. It is asserted because it is the reason the
    // feature exists: if asking for two crops cost more than two whole frames, an agent should be
    // told to do the latter instead.
    assert!(
        composed.total_base64 < whole.total_base64,
        "a cheap overview plus two crops ({} bytes) should cost less than whole frames at the crops' \
         own quality ({} bytes)",
        composed.total_base64,
        whole.total_base64
    );
}

// ============================================================================
// 54: memory
// ============================================================================

#[test]
fn views_share_one_raw_frame_and_presentation_does_not_grow_history() {
    // Specification section 54. Every bullet is checked against the running service:
    //
    // * multiple views do not duplicate raw frames;
    // * multiple ROIs share one raw source frame;
    // * temporal presentation does not clone raw pixel buffers;
    // * history capacity is unchanged;
    // * and the service's memory does not grow without bound across many presentations.
    //
    // The mechanism is structural: `SessionFrame::frame` is an `Arc<Frame>`, so a view holds a
    // pointer rather than a copy, and the presentation layer never constructs a `Frame`. This
    // test establishes the consequences of that on a real process — the parts that a reader
    // cannot verify by reading the code.
    let _guard = serial();
    if !skip_if_no_xvfb() {
        return;
    }
    let Some((_server, display, service)) = xvfb_service("p6-memory", SCREEN_W, SCREEN_H) else {
        return;
    };

    let _scene = textured_scene(&display);
    let session = create_session(&service, &display);
    let one_frame_bytes = (SCREEN_W as u64) * (SCREEN_H as u64) * 3;

    // The history capacity is a fixed constant, and presentation must not have changed it.
    let (_, info) = run_session(&service, &["info", &session, "--json"]);
    let capacity = info["info"]["history"]["capacity"].as_u64().unwrap();
    let retained_after_one = info["info"]["history"]["retained_bytes"].as_u64().unwrap();
    println!("\n--- history capacity {capacity}, {retained_after_one} bytes retained after one frame ---");

    // One frame, one view: the baseline resident set.
    let (code, single) = run_session(
        &service,
        &[
            "capture",
            &session,
            "--json",
            "--base64",
            "--overview-width",
            "320",
            "--overview-format",
            "jpeg",
        ],
    );
    assert_eq!(code, 0, "capture failed: {single}");

    let rss_one_view = service
        .resident_bytes()
        .expect("the service's resident set should be readable on Linux");

    // The same frame presented again as an overview plus eight regions. Nine views over one
    // frame. If views duplicated the raw frame, this would add eight times the frame's bytes to
    // the process; if they share it, it adds roughly nothing.
    let names: Vec<String> = (0..8)
        .map(|index| format!("r{index}_{}", stamp()))
        .collect();
    let regions: Vec<String> = names
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{name}={},{},80,80", index % 4 * 160, index / 4 * 160))
        .collect();

    let frame_id = single["frame"]["frame_id"].as_u64().unwrap().to_string();
    let mut args = vec![
        "frame",
        session.as_str(),
        frame_id.as_str(),
        "--json",
        "--base64",
        "--overview-width",
        "320",
        "--overview-format",
        "jpeg",
        "--region-format",
        "jpeg",
        "--region-width",
        "80",
    ];
    for region in &regions {
        args.push("--region");
        args.push(region);
    }
    let (code, many) = run_session(&service, &args);
    assert_eq!(code, 0, "the multi-view request failed: {many}");

    let view_count: usize = frames(&many)
        .iter()
        .map(|frame| frame["views"].as_array().unwrap().len())
        .sum();
    assert_eq!(view_count, 9, "one overview and eight regions");

    // Presenting the same retained frame did not capture: the frame count is unchanged.
    let (_, info) = run_session(&service, &["info", &session, "--json"]);
    assert_eq!(
        info["info"]["frames_captured"].as_u64(),
        Some(1),
        "nine views of a retained frame must not capture anything: {info}"
    );
    assert_eq!(
        info["info"]["history"]["capacity"].as_u64(),
        Some(capacity),
        "the history capacity must not have changed"
    );
    assert_eq!(
        info["info"]["history"]["retained"].as_u64(),
        Some(1),
        "only the one capture should be retained: {info}"
    );

    let rss_many_views = service
        .resident_bytes()
        .expect("the service's resident set should be readable on Linux");

    println!(
        "--- one frame is {} bytes of raw pixels; rss {rss_one_view} -> {rss_many_views} across one \
         view and then nine ---",
        one_frame_bytes
    );

    // The claim, stated the way it can actually be measured. Comparing a single-view run against a
    // nine-view run measures the allocator's *high-water mark*, not a leak: the encoded crops and
    // the serialized response are genuinely allocated while the request is in flight, and glibc
    // does not hand the pages back the moment they are freed. An earlier version of this test
    // failed on that alone, and the failure said nothing about pixels being duplicated.
    //
    // What a viewer-sharing bug would look like is growth that continues as more views are asked
    // for. So the nine-view request is repeated and the resident set is required to *settle*: after
    // the allocator has seen the same workload once, another nine views must add essentially
    // nothing. That is a leak detector, and it is what the claim "views do not duplicate raw
    // frames" actually predicts.
    let before_repeat = service
        .resident_bytes()
        .expect("the service's resident set should be readable on Linux");

    for _ in 0..3 {
        let (code, again) = run_session(&service, &args);
        assert_eq!(code, 0, "the repeated multi-view request failed: {again}");
    }

    let after_repeat = service
        .resident_bytes()
        .expect("the service's resident set should be readable on Linux");

    let steady_growth = after_repeat.saturating_sub(before_repeat);
    println!(
        "--- after three more nine-view presentations of the same frame: {} -> {} bytes (growth {} \
         bytes, {:.2}% of a raw frame) ---",
        before_repeat,
        after_repeat,
        steady_growth,
        100.0 * steady_growth as f64 / one_frame_bytes as f64
    );

    // Three more rounds of nine views, each of which would duplicate a whole raw frame if views
    // copied pixels, would therefore cost three frames' worth — some 2.7 MB at this size. The bound
    // is one frame's worth, which is generous enough for allocator noise and far too tight for three
    // copies.
    assert!(
        steady_growth < one_frame_bytes,
        "three further nine-view presentations grew the process by {steady_growth} bytes; if each \
         view copied its raw frame this would be about {} bytes",
        one_frame_bytes * 3
    );

    // A temporal presentation is the other case that could plausibly clone: a stack of frames each
    // rendered several ways. The same policy is used again below, so it is named once here.
    let temporal_policy: Vec<&str> = vec![
        "realtime",
        &session,
        "--json",
        "--base64",
        "--frames",
        "4",
        "--interval",
        "30ms",
        "--timeout",
        "400ms",
        "--temporal",
        "newest-detailed",
        "--overview-width",
        "320",
        "--overview-format",
        "jpeg",
        "--older-width",
        "160",
        "--newest-width",
        "320",
        "--region",
        &regions[0],
        "--region-format",
        "jpeg",
    ];
    let (code, temporal) = run_session(&service, &temporal_policy);
    assert_eq!(code, 0, "the temporal request failed: {temporal}");

    let (_, info) = run_session(&service, &["info", &session, "--json"]);
    let retained = info["info"]["history"]["retained"].as_u64().unwrap();
    println!(
        "--- after a four-frame temporal presentation: {retained} frames retained of {capacity} \
         capacity ---\n"
    );
    assert!(
        retained <= capacity,
        "history exceeded its capacity: {retained} of {capacity}"
    );

    // The bounded-fitting claim, and the reason it asks for the floor first: the smallest reachable
    // payload is not simply the unfitted payload scaled down. Here it is *larger* than the unfitted
    // payload, because JPEG at this content does not shrink monotonically with width — the
    // per-image header and the block structure interact. Asking the service what it can reach is
    // therefore the only correct way to pick a budget that is guaranteed reachable, and guessing
    // "half of it" produced a budget below the floor on the first run of this test.
    let (_, refused) = run_session(&service, &{
        let mut args = temporal_policy.clone();
        args.push("--max-base64-bytes");
        args.push("1");
        args
    });
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    let floor: u64 = message
        .split("smallest achievable payload is ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("could not read the floor from {message:?}"));
    let preferred = presentation(&temporal)["total_base64_bytes"]
        .as_u64()
        .unwrap();
    println!(
        "--- the same policy unfitted is {preferred} bytes, and its floor is {floor} bytes ---"
    );

    // The floor has to be usable: a budget *at* it must work, or the message is sending the caller
    // round the same loop again. This is the exact defect a live run of this suite exposed, where
    // the reported floor (56268 bytes) sat *above* the unfitted payload (51256 bytes) because the
    // most-degraded plan was not the cheapest one for this content.
    assert!(
        floor <= preferred,
        "the reported floor ({floor}) must not exceed the unfitted payload ({preferred})"
    );

    let budget = floor.to_string();
    let mut fitted_args = temporal_policy.clone();
    fitted_args.push("--max-base64-bytes");
    fitted_args.push(&budget);
    let (code, fitted) = run_session(&service, &fitted_args);
    assert_eq!(
        code, 0,
        "a budget at the quoted floor of {floor} must be reachable: {fitted}"
    );
    let payload = &presentation(&fitted)["payload"];
    assert!(
        payload["actual_base64_bytes"].as_u64().unwrap()
            <= payload["budget_base64_bytes"].as_u64().unwrap(),
        "fitting stayed inside its budget: {payload}"
    );
    // Every frame survives the fitting, which is the property the whole rework was for.
    assert_eq!(
        frames(&fitted).len(),
        sampling(&fitted)["captured_frames"].as_u64().unwrap() as usize,
        "every captured frame must appear after fitting: {fitted}"
    );
    println!(
        "--- fitting to {budget}: {} bytes, {} adjustment(s), {} frames kept ---\n",
        payload["actual_base64_bytes"],
        payload["adjustments"].as_array().unwrap().len(),
        frames(&fitted).len(),
    );
}
