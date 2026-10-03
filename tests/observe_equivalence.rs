//! Phase 4 requirement 64: observation equivalence.
//!
//! The same scripted scene is observed twice: once through the standalone,
//! connection-per-capture path from Phase 3, and once through a persistent
//! session. The two must reach the *same conclusion*.
//!
//! This is the strongest protection against Phase 4 quietly changing behaviour
//! while optimising it. The state machines are supposed to be shared verbatim
//! between the two paths; the only intended difference is where frames come from.
//! If that sharing is ever broken — a different default, a different comparison,
//! a subtly different rule about which frame counts as the settled one — these
//! tests fail.
//!
//! ## What is compared, and what is not
//!
//! **Frame IDs are allowed to differ and are not compared.** A session numbers
//! frames monotonically across its lifetime, so the same scene yields different
//! identifiers on the two paths. Comparing them would test bookkeeping rather
//! than semantics.
//!
//! **Capture counts are not compared either.** They depend on how many samples
//! happened to land inside an interval, which is a property of the scheduler, not
//! of the observation rules.
//!
//! What is compared is the semantic conclusion: the `result` the operation
//! reports and the exit status that goes with it. Those are the contract an agent
//! actually consumes.
//!
//! ## Why the scenarios are chosen for robustness
//!
//! Each scenario's conclusion is determined by the script alone and does not
//! depend on exactly when a sample lands. "Paint nothing forever" times out on
//! any schedule. "Paint once and hold" is detected as a change on any schedule
//! that samples after the paint. Scenarios whose outcome *did* depend on sample
//! timing would produce a flaky test that proves nothing, so they are not used.

mod common;

use std::time::Duration;

use common::*;

const SCREEN_W: u32 = 400;
const SCREEN_H: u32 = 300;

/// How the scene behaves over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    /// The scene never changes.
    Static,
    /// The scene changes once and holds.
    SingleChange,
    /// The scene changes, then returns to its original appearance.
    ReturnToBaseline,
    /// The scene changes in small increments that accumulate.
    GradualDrift,
    /// The scene changes, settles, changes again, and settles again.
    RepeatedSettling,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Scenario::Static => "static",
            Scenario::SingleChange => "single-change",
            Scenario::ReturnToBaseline => "return-to-baseline",
            Scenario::GradualDrift => "gradual-drift",
            Scenario::RepeatedSettling => "repeated-settling",
        }
    }

    /// The paint schedule, as `(delay_from_start, paints)`.
    ///
    /// Delays are generous relative to process startup so that a slow machine
    /// shifts *when* a sample lands without changing *what* was on screen when it
    /// did.
    fn script(self) -> Vec<(Duration, Vec<PaintOp>)> {
        let ms = Duration::from_millis;
        match self {
            // Nothing is ever painted on the root window, so the scene is a
            // constant black. This is the timeout-before-change case.
            Scenario::Static => vec![],

            // A large red block appears well after the baseline is likely taken,
            // so a change must be detected regardless of startup jitter.
            Scenario::SingleChange => vec![(ms(300), vec![paint_rect(50, 40, [255, 0, 0])])],

            // The block appears, then is painted back to black. The frame differs
            // from the baseline while it exists, and matches it again afterwards.
            Scenario::ReturnToBaseline => {
                vec![
                    (ms(300), vec![paint_rect(50, 40, [255, 0, 0])]),
                    (ms(700), vec![paint_rect(50, 40, [0, 0, 0])]),
                ]
            }

            // Successive small differences. Each step is large enough to clear the
            // default thresholds on its own, so a change is unambiguously detected
            // at the first step and the scene keeps moving afterwards.
            Scenario::GradualDrift => vec![
                (ms(200), vec![paint_rect(20, 20, [80, 0, 0])]),
                (ms(450), vec![paint_rect(140, 20, [0, 80, 0])]),
                (ms(700), vec![paint_rect(260, 20, [0, 0, 80])]),
                (ms(950), vec![paint_rect(20, 200, [40, 40, 0])]),
            ],

            // Change, hold long enough to settle, change again, hold again. This
            // exercises the reset path: a settling timer that has already elapsed
            // must be discarded when the scene moves again.
            Scenario::RepeatedSettling => vec![
                (ms(250), vec![paint_rect(50, 40, [255, 0, 0])]),
                (ms(900), vec![paint_rect(240, 40, [0, 255, 0])]),
                (ms(1500), vec![paint_rect(50, 200, [0, 0, 255])]),
            ],
        }
    }
}

/// Which temporal operation is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    WaitChange,
    WaitStable,
    Observe,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Operation::WaitChange => "wait-change",
            Operation::WaitStable => "wait-stable",
            Operation::Observe => "observe",
        }
    }
}

/// The conclusion of one observation, as an agent would consume it.
#[derive(Debug, PartialEq, Eq)]
struct Conclusion {
    exit: i32,
    result: String,
}

/// Locate the observation summary inside a response, whichever shape it has.
///
/// The two paths intentionally have different *envelopes*:
///
/// * the standalone commands emit the Phase 3 payload directly, so the summary is
///   at `observation`;
/// * the service wraps every response body in a `{ "kind": ..., "<type>": ... }`
///   envelope, uniformly for frames, diffs, and observations alike, so a session
///   observation's payload is one key deeper.
///
/// The envelope is a transport detail; the *payload* is the same type produced by
/// the same code on both paths. This helper therefore finds the payload rather
/// than assuming a depth, so the assertion is about semantics — which is what the
/// spec requires to be equivalent — and not about nesting.
fn observation_summary(value: &serde_json::Value) -> &serde_json::Value {
    let candidates = [&value["observation"], &value["observation"]["observation"]];

    for candidate in candidates {
        if candidate["result"].is_string() {
            return candidate;
        }
    }

    if value["result"].is_string() {
        return value;
    }

    panic!("no observation summary was found in the response: {value}");
}

/// Read the semantic conclusion out of an observation response.
fn conclusion(exit: i32, value: &serde_json::Value) -> Conclusion {
    let result = observation_summary(value)["result"]
        .as_str()
        .expect("an observation summary should carry a result")
        .to_string();
    Conclusion { exit, result }
}

/// Timings shared by both paths so the comparison is meaningful.
///
/// The timeout is long enough for every scenario to reach its decision, and the
/// interval is short enough that a change is noticed promptly after it is painted.
fn temporal_args() -> Vec<&'static str> {
    vec![
        "--interval",
        "50ms",
        "--timeout",
        "3s",
        "--stable-for",
        "250ms",
    ]
}

/// Run an observation through the standalone Phase 3 path.
fn run_standalone(display: &str, operation: Operation) -> Conclusion {
    let mut args = vec![operation.name(), "--display", display, "--json"];
    args.extend(temporal_args());

    let (exit, value, stderr) = run_json_stdout(&args);
    assert!(
        !value.is_null(),
        "standalone {} produced no JSON: {stderr}",
        operation.name()
    );
    conclusion(exit, &value)
}

/// Run an observation through a persistent Phase 4 session.
fn run_session(service: &ServiceProcess, session_id: &str, operation: Operation) -> Conclusion {
    let mut args = vec![operation.name(), session_id, "--json"];
    args.extend(temporal_args());
    args.push("--base64");

    let (exit, value) = service.run_json(&args);
    conclusion(exit, &value)
}

/// Compare the two paths for one operation and scenario.
///
/// Both paths observe the *same script*, but at different wall-clock moments, so
/// the script is replayed for each. `keepalive` holds a connection for the whole
/// test, which is what stops Xvfb from resetting the root window between runs —
/// without it, a replay would start from a cleared screen and the scenario would
/// be observing something different from the first time.
fn compare_paths(
    keepalive: &Screen,
    display: &str,
    service: &ServiceProcess,
    session_id: &str,
    operation: Operation,
    scenario: Scenario,
) {
    let label = format!("{} / {}", operation.name(), scenario.name());

    // Standalone first.
    let standalone = {
        clear(keepalive);
        let _painter = Painter::start(display.to_string(), scenario.script());
        run_standalone(display, operation)
    };

    // Then the session path, replaying the same script from the same starting
    // appearance.
    let session = {
        clear(keepalive);
        let _painter = Painter::start(display.to_string(), scenario.script());
        run_session(service, session_id, operation)
    };

    assert_eq!(
        standalone, session,
        "the two observation paths disagreed for {label}: \
         standalone reported {standalone:?}, the session reported {session:?}"
    );

    // A scenario must also be reproducible across two runs of the *same* path,
    // otherwise the comparison above could pass by both being wrong in the same
    // way rather than by being correct.
    let repeat = {
        clear(keepalive);
        let _painter = Painter::start(display.to_string(), scenario.script());
        run_session(service, session_id, operation)
    };
    assert_eq!(
        session, repeat,
        "the session path was not reproducible for {label}: {session:?} then {repeat:?}"
    );
}

/// Restore the screen to the scene's starting appearance.
///
/// Painted through the keep-alive connection, not a fresh one, so the root window
/// is never reset by a disconnection.
fn clear(screen: &Screen) {
    let masks = screen.visual_masks();
    screen.fill(
        screen.root(),
        0,
        0,
        SCREEN_W,
        SCREEN_H,
        rgb_to_pixel(masks, [0, 0, 0]),
    );
}

/// Set up the display and a service with one session, or skip.
///
/// Returns a `Screen` that the caller must keep alive for the whole test.
fn setup(name: &str) -> Option<(Xvfb, Screen, String, ServiceProcess, String)> {
    let (server, display, service) = xvfb_service(name, SCREEN_W, SCREEN_H)?;

    // Held open for the whole test: Xvfb resets the root window when its last
    // client disconnects, which would clear the scene between scenarios.
    let keepalive = Screen::open(&display);

    let (code, value) = service.run_json(&["create", "--display", &display, "--json"]);
    assert_eq!(code, 0, "session creation failed: {value}");
    let session_id = value["session_id"]
        .as_str()
        .expect("a created session should carry an id")
        .to_string();

    Some((server, keepalive, display, service, session_id))
}

// ============================================================================
// One test per operation, each covering every scenario
// ============================================================================

#[test]
fn wait_change_is_equivalent_through_both_paths() {
    let _guard = serial();
    let Some((_server, screen, display, service, session_id)) = setup("equiv-wait-change") else {
        return;
    };

    for scenario in [
        Scenario::Static,
        Scenario::SingleChange,
        Scenario::ReturnToBaseline,
        Scenario::GradualDrift,
        Scenario::RepeatedSettling,
    ] {
        compare_paths(
            &screen,
            &display,
            &service,
            &session_id,
            Operation::WaitChange,
            scenario,
        );
    }
}

#[test]
fn wait_stable_is_equivalent_through_both_paths() {
    let _guard = serial();
    let Some((_server, screen, display, service, session_id)) = setup("equiv-wait-stable") else {
        return;
    };

    for scenario in [
        Scenario::Static,
        Scenario::SingleChange,
        Scenario::GradualDrift,
        Scenario::RepeatedSettling,
    ] {
        compare_paths(
            &screen,
            &display,
            &service,
            &session_id,
            Operation::WaitStable,
            scenario,
        );
    }
}

#[test]
fn observe_is_equivalent_through_both_paths() {
    let _guard = serial();
    let Some((_server, screen, display, service, session_id)) = setup("equiv-observe") else {
        return;
    };

    for scenario in [
        Scenario::Static,
        Scenario::SingleChange,
        Scenario::ReturnToBaseline,
        Scenario::GradualDrift,
        Scenario::RepeatedSettling,
    ] {
        compare_paths(
            &screen,
            &display,
            &service,
            &session_id,
            Operation::Observe,
            scenario,
        );
    }
}

// ============================================================================
// The individual scenarios the spec names, asserted explicitly
// ============================================================================

#[test]
fn a_timeout_before_any_change_agrees_on_both_paths() {
    let _guard = serial();
    let Some((_server, screen, display, service, session_id)) = setup("equiv-timeout-before")
    else {
        return;
    };

    let standalone = {
        clear(&screen);
        let _painter = Painter::start(display.clone(), Scenario::Static.script());
        run_standalone(&display, Operation::Observe)
    };
    let session = {
        clear(&screen);
        let _painter = Painter::start(display.clone(), Scenario::Static.script());
        run_session(&service, &session_id, Operation::Observe)
    };

    // The scene never changes, so neither path may claim a transition, and both
    // must use the dedicated timeout status rather than an error.
    assert_eq!(standalone.result, "timeout", "standalone: {standalone:?}");
    assert_eq!(session.result, "timeout", "session: {session:?}");
    assert_eq!(standalone.exit, 100, "standalone: {standalone:?}");
    assert_eq!(session.exit, 100, "session: {session:?}");
    assert_eq!(standalone, session);
}

#[test]
fn a_change_that_stays_changed_agrees_on_both_paths() {
    let _guard = serial();
    let Some((_server, screen, display, service, session_id)) = setup("equiv-single-change") else {
        return;
    };

    let standalone = {
        clear(&screen);
        let _painter = Painter::start(display.clone(), Scenario::SingleChange.script());
        run_standalone(&display, Operation::WaitChange)
    };
    let session = {
        clear(&screen);
        let _painter = Painter::start(display.clone(), Scenario::SingleChange.script());
        run_session(&service, &session_id, Operation::WaitChange)
    };

    assert_eq!(standalone.result, "changed", "standalone: {standalone:?}");
    assert_eq!(session.result, "changed", "session: {session:?}");
    assert_eq!(standalone.exit, 0);
    assert_eq!(session.exit, 0);
}

#[test]
fn a_static_scene_settles_immediately_on_both_paths() {
    let _guard = serial();
    let Some((_server, screen, display, service, session_id)) = setup("equiv-static-stable") else {
        return;
    };

    // A screen that is already still has satisfied the stability requirement from
    // the first sample, so this must return promptly rather than waiting out the
    // deadline.
    let standalone = {
        clear(&screen);
        let _painter = Painter::start(display.clone(), Scenario::Static.script());
        run_standalone(&display, Operation::WaitStable)
    };
    let session = {
        clear(&screen);
        let _painter = Painter::start(display.clone(), Scenario::Static.script());
        run_session(&service, &session_id, Operation::WaitStable)
    };

    assert_eq!(standalone.result, "stable", "standalone: {standalone:?}");
    assert_eq!(session.result, "stable", "session: {session:?}");
    assert_eq!(standalone, session);
}
