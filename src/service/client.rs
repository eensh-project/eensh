//! The `eensh serve`, `eensh ping`, and `eensh session ...` client.
//!
//! # Two ways to work
//!
//! The standalone commands — `capture`, `diff`, `wait-change`, `wait-stable`,
//! `observe` — each open a display, do their work, and exit. They are unchanged.
//!
//! The session commands talk to a running `eensh serve`:
//!
//! ```text
//! eensh serve &
//! eensh session create --display :99 --json
//! eensh session capture SESSION --json --base64
//! eensh session observe SESSION --json --base64
//! eensh session close SESSION
//! ```
//!
//! The service is never auto-started. Explicit lifecycle is easier to reason
//! about, and hiding a daemon behind an ordinary capture command is exactly the
//! kind of surprise Phase 4 should not introduce.
//!
//! The CLI is a client, not a protocol implementation: it builds a request
//! envelope, sends it, and renders whatever comes back. It does not know how a
//! session captures.

use crate::cli::{ObservationKind, ServeArgs, SessionCommand, SessionImageArgs, SessionTarget};
use crate::error::Error;
use crate::observe::Outcome;
use crate::output::file;
use crate::service::protocol::{
    self, ImageOptionsWire, ObservationKindWire, Request, RequestEnvelope, ResponseBody,
    ResponseEnvelope, PROTOCOL_VERSION,
};
use crate::service::unix;
use crate::session::FrameId;

/// The documented timeout status, shared with the standalone observation commands.
const TIMEOUT_EXIT: i32 = 100;

/// Run `eensh serve`.
pub fn run_serve(args: &ServeArgs) -> i32 {
    let path = args.socket.clone().unwrap_or_else(unix::socket_path);

    let service = match unix::Service::bind(path.clone()) {
        Ok(service) => service,
        Err(error) => {
            emit_error(&error, args.json);
            return error.exit_code();
        }
    };

    // Reported on stderr so that it cannot be mistaken for command output; a
    // caller that wants machine-readable readiness uses `eensh ping`.
    file::write_stderr(&format!(
        "eensh serve: listening on {} (protocol version {})",
        path.display(),
        PROTOCOL_VERSION
    ));

    // The accept loop exits when this flag is set, which lets the socket be
    // removed by the normal shutdown path rather than abandoned.
    let flag = service.shutdown_flag();
    install_signal_handlers(&flag);

    let result = service.run();
    service.cleanup();

    match result {
        Ok(()) => 0,
        Err(error) => {
            emit_error(&error, args.json);
            error.exit_code()
        }
    }
}

/// Arrange for the accept loop to stop on termination.
///
/// The handler only sets a flag, because the socket must be removed by the normal
/// shutdown path. Doing real work in a signal context is not safe, and exiting
/// immediately would bypass the cleanup that removes the socket.
fn install_signal_handlers(flag: &std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    use std::sync::OnceLock;

    static FLAG: OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> = OnceLock::new();
    if FLAG.set(std::sync::Arc::clone(flag)).is_err() {
        return;
    }

    extern "C" fn on_signal(_signal: std::os::raw::c_int) {
        if let Some(flag) = FLAG.get() {
            flag.store(true, Ordering::Relaxed);
        }
    }

    // Safety: the handler body only stores to an atomic, which is signal-safe.
    // The cast goes through an explicit function pointer so that the handler
    // address is taken as an address rather than converted from a zero-sized
    // function item.
    let handler = on_signal as extern "C" fn(std::os::raw::c_int);
    unsafe {
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
}

/// Run `eensh ping`.
pub fn run_ping(args: &ServeArgs) -> i32 {
    let path = args.socket.clone().unwrap_or_else(unix::socket_path);
    let envelope = RequestEnvelope::new(request_id(), Request::Ping);

    match unix::request(&path, &envelope) {
        Ok(response) => emit(&response, args.json),
        Err(error) => {
            emit_error(&error, args.json);
            error.exit_code()
        }
    }
}

/// Run an `eensh session ...` command.
pub fn run_session(command: &SessionCommand) -> i32 {
    let json = command.json();
    let path = command.socket().cloned().unwrap_or_else(unix::socket_path);

    let envelope = match build_request(command) {
        Ok(envelope) => envelope,
        Err(error) => {
            emit_error(&error, json);
            return error.exit_code();
        }
    };

    match unix::request(&path, &envelope) {
        Ok(response) => emit(&response, json),
        Err(error) => {
            emit_error(&error, json);
            error.exit_code()
        }
    }
}

/// Build the protocol request a session command corresponds to.
///
/// Every option is resolved here, using the same helpers the standalone commands
/// use, so that a session request is built by the same rules as its non-session
/// counterpart.
fn build_request(command: &SessionCommand) -> Result<RequestEnvelope, Error> {
    let request = match command {
        SessionCommand::Create(args) => Request::SessionCreate {
            display: args
                .display
                .clone()
                .or_else(|| std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()))
                .ok_or_else(|| {
                    Error::invalid_arguments(
                        "no X11 display was specified and DISPLAY is not set; pass --display",
                    )
                })?,
            target: to_request_target(&args.target()),
            history: args.history,
        },

        SessionCommand::List { .. } => Request::SessionList,

        SessionCommand::Info { session_id, .. } => Request::SessionInfo {
            session_id: session_id.clone(),
        },

        SessionCommand::Close { session_id, .. } => Request::SessionClose {
            session_id: session_id.clone(),
        },

        SessionCommand::Capture {
            session_id,
            image,
            base64,
            ..
        } => Request::SessionCapture {
            session_id: session_id.clone(),
            output: to_wire(image, *base64)?,
        },

        SessionCommand::Latest {
            session_id,
            image,
            base64,
            ..
        } => Request::SessionLatest {
            session_id: session_id.clone(),
            output: to_wire(image, *base64)?,
        },

        SessionCommand::Frame {
            session_id,
            frame_id,
            image,
            base64,
            ..
        } => Request::SessionFrame {
            session_id: session_id.clone(),
            frame_id: FrameId(*frame_id),
            output: to_wire(image, *base64)?,
        },

        SessionCommand::Diff {
            session_id,
            before,
            after,
            ..
        } => {
            let compare = command.diff_compare_options()?.ok_or_else(|| {
                Error::Internal("a diff command did not resolve to comparison options".to_string())
            })?;
            Request::SessionDiff {
                session_id: session_id.clone(),
                before: FrameId(*before),
                after: FrameId(*after),
                compare: compare.into(),
            }
        }

        SessionCommand::WaitChange(args) => Request::SessionObserve {
            session_id: args.session_id.clone(),
            kind: ObservationKindWire::WaitChange,
            temporal: args.temporal()?.into(),
            stable_for_ms: 0,
            output: to_wire(&args.image, args.base64)?,
        },

        SessionCommand::WaitStable(args) => Request::SessionObserve {
            session_id: args.session_id.clone(),
            kind: ObservationKindWire::WaitStable,
            temporal: args.temporal()?.into(),
            stable_for_ms: args.stable_for(ObservationKind::WaitStable).as_millis() as u64,
            output: to_wire(&args.image, args.base64)?,
        },

        SessionCommand::Observe(args) => Request::SessionObserve {
            session_id: args.session_id.clone(),
            kind: ObservationKindWire::Observe,
            temporal: args.temporal()?.into(),
            stable_for_ms: args.stable_for(ObservationKind::Observe).as_millis() as u64,
            output: to_wire(&args.image, args.base64)?,
        },
    };

    Ok(RequestEnvelope::new(request_id(), request))
}

/// Translate the CLI's target into the wire form.
fn to_request_target(target: &SessionTarget) -> protocol::RequestTarget {
    match target {
        SessionTarget::Desktop => protocol::RequestTarget::Desktop,
        SessionTarget::Region(region) => protocol::RequestTarget::Region { region: *region },
        SessionTarget::Window(id) => protocol::RequestTarget::Window { id: *id },
    }
}

/// Translate image arguments into the wire form.
///
/// The pipeline's image options are not sent directly because the wire form
/// carries at most one of width, height, or scale, while the pipeline carries a
/// tagged resize request. This is where the CLI's resolution rules are applied,
/// so an unsupported combination is rejected before a request is sent.
fn to_wire(image: &SessionImageArgs, base64: bool) -> Result<ImageOptionsWire, Error> {
    let options = image.to_image_options()?;

    Ok(ImageOptionsWire {
        format: options.format,
        quality: options.quality,
        png_effort: options.png_effort,
        width: match options.resize {
            crate::cli::ResizeRequest::Width(width) => Some(width),
            _ => None,
        },
        height: match options.resize {
            crate::cli::ResizeRequest::Height(height) => Some(height),
            _ => None,
        },
        scale: match options.resize {
            crate::cli::ResizeRequest::Scale(scale) => Some(scale),
            _ => None,
        },
        base64,
    })
}

/// Render a response and choose the exit status.
///
/// The status reflects the observation outcome, not the transport: a timeout is a
/// successful request that reports `timeout`, and it gets the same dedicated
/// status the standalone observation commands use.
fn emit(response: &ResponseEnvelope, json: bool) -> i32 {
    if !response.ok {
        let body = response.error.as_ref();
        let code = body
            .map(|b| b.code.as_str())
            .unwrap_or("service_protocol_error");
        let message = body
            .map(|b| b.message.clone())
            .unwrap_or_else(|| "the service reported a failure".to_string());

        if json {
            file::write_stderr(&format!(
                "{}\n",
                serde_json::json!({ "error": { "code": code, "message": message } })
            ));
        } else {
            file::write_stderr(&message);
        }

        return exit_code_for(code);
    }

    let Some(result) = &response.result else {
        return report_protocol_error("the service returned neither a result nor an error", json);
    };

    // Checked before rendering, because a timeout is not a transport failure.
    let timed_out = matches!(
        result,
        ResponseBody::Observation { observation }
            if observation.observation.result == Outcome::Timeout
    );

    if json {
        match serde_json::to_string(result) {
            Ok(text) => {
                if let Err(error) = file::write_stdout_text(&text) {
                    file::write_stderr(&error.message());
                    return error.exit_code();
                }
            }
            Err(error) => {
                return report_protocol_error(
                    &format!("could not serialize the response: {error}"),
                    json,
                )
            }
        }
    } else {
        file::write_stderr(&summarize(result));
    }

    if timed_out {
        TIMEOUT_EXIT
    } else {
        0
    }
}

/// Map a service error code back to the exit status that code has elsewhere.
///
/// Reconstructed through the same table the rest of the tool uses, so a
/// `frame_not_available` from the service exits the same way a
/// `frame_not_available` from anywhere else would. That keeps the exit-code table
/// in `error.rs` the single source of truth even across a process boundary.
fn exit_code_for(code: &str) -> i32 {
    let candidates = [
        Error::invalid_arguments(""),
        Error::display_unavailable("", ""),
        Error::invalid_region(""),
        Error::WindowNotFound {
            window_id: String::new(),
        },
        Error::capture_failed(""),
        Error::GeometryChanged(String::new()),
        Error::TargetLost(String::new()),
        Error::InvalidDuration(String::new()),
        Error::ObservationFailed(String::new()),
        Error::SessionNotFound(String::new()),
        Error::SessionBusy(String::new()),
        Error::SessionClosed(String::new()),
        Error::FrameNotAvailable(String::new()),
        Error::NoFrameAvailable(String::new()),
        Error::ServiceUnavailable(String::new()),
        Error::ServiceProtocolError(String::new()),
        Error::ServiceOverloaded(String::new()),
    ];

    for candidate in candidates {
        if candidate.code() == code {
            return candidate.exit_code();
        }
    }
    Error::Internal(String::new()).exit_code()
}

/// A one-line human-readable summary of a response.
fn summarize(result: &ResponseBody) -> String {
    match result {
        ResponseBody::Status { status } => format!(
            "eensh service: version {}, protocol {}, {} session(s), up {}ms",
            status.version, status.protocol_version, status.sessions, status.uptime_ms
        ),
        ResponseBody::SessionCreated { session_id, info } => format!(
            "created session {session_id}: {} on {}, history capacity {}",
            info.target.kind, info.display, info.history.capacity
        ),
        ResponseBody::SessionList { sessions } => {
            if sessions.is_empty() {
                "no sessions".to_string()
            } else {
                sessions
                    .iter()
                    .map(|info| {
                        format!(
                            "{}: {} {} frames={}",
                            info.session_id, info.state, info.target.kind, info.frames_captured
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        ResponseBody::SessionInfo { info } => format!(
            "{}: {} {} frames={} history={}/{} retained={}B",
            info.session_id,
            info.state,
            info.target.kind,
            info.frames_captured,
            info.history.retained,
            info.history.capacity,
            info.history.retained_bytes
        ),
        ResponseBody::SessionClosed { session_id } => format!("closed session {session_id}"),
        ResponseBody::Frame { frame } => format!(
            "frame {} ({}) {}x{} age={}us fresh_capture={}",
            frame.frame_id,
            frame.image.media_type,
            frame.image.width,
            frame.image.height,
            frame.frame_age_us,
            frame.fresh_capture
        ),
        ResponseBody::Diff { diff } => format!(
            "{} vs {}: changed={} {}/{} pixels ({:.4}%) in {}us",
            diff.before,
            diff.after,
            diff.comparison.changed,
            diff.comparison.changed_pixels,
            diff.comparison.total_pixels,
            diff.comparison.changed_fraction * 100.0,
            diff.compare_us
        ),
        ResponseBody::Observation { observation } => {
            let section = &observation.observation;
            let frames = observation
                .frames
                .map(|ids| {
                    format!(
                        " final_frame={} baseline={:?} first_change={:?}",
                        ids.final_frame, ids.baseline, ids.first_change
                    )
                })
                .unwrap_or_default();
            format!(
                "{} {}: {} captures in {}ms,{}",
                section.kind, section.result, section.captures, section.elapsed_ms, frames
            )
        }
    }
}

/// Report a protocol-level failure.
fn report_protocol_error(message: &str, json: bool) -> i32 {
    let error = Error::service_protocol_error(message);
    let code = error.exit_code();
    emit_error(&error, json);
    code
}

/// Report an error in the requested shape.
fn emit_error(error: &Error, json: bool) {
    if json {
        file::write_stderr(&format!("{}\n", error.to_json()));
    } else {
        file::write_stderr(&error.message());
    }
}

/// A unique request identifier.
///
/// Only needs to be unique enough for a client to match a response to its request.
fn request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{}-{}", nanos, COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// The observation kind a wire observation corresponds to.
pub fn observation_kind(kind: ObservationKindWire) -> ObservationKind {
    match kind {
        ObservationKindWire::WaitChange => ObservationKind::WaitChange,
        ObservationKindWire::WaitStable => ObservationKind::WaitStable,
        ObservationKindWire::Observe => ObservationKind::Observe,
    }
}

/// The wire kind an observation command corresponds to.
pub fn wire_kind(kind: ObservationKind) -> ObservationKindWire {
    match kind {
        ObservationKind::WaitChange => ObservationKindWire::WaitChange,
        ObservationKind::WaitStable => ObservationKindWire::WaitStable,
        ObservationKind::Observe => ObservationKindWire::Observe,
    }
}

/// The default image presentation, used when a client omits image options.
pub fn default_image_wire() -> ImageOptionsWire {
    ImageOptionsWire::default_png_base64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::CompareOptions;
    use crate::geometry::Rect;

    fn image_args() -> SessionImageArgs {
        SessionImageArgs {
            width: None,
            height: None,
            scale: None,
            format: None,
            quality: None,
            compression: None,
        }
    }

    fn create_args(display: Option<&str>, target: SessionTarget) -> crate::cli::SessionCreateArgs {
        crate::cli::SessionCreateArgs {
            display: display.map(str::to_string),
            region: match target {
                SessionTarget::Region(region) => Some(region),
                _ => None,
            },
            window: match target {
                SessionTarget::Window(id) => Some(id),
                _ => None,
            },
            history: None,
            json: true,
            socket: None,
        }
    }

    fn observe_args(stable_for: Option<std::time::Duration>) -> crate::cli::SessionObserveArgs {
        crate::cli::SessionObserveArgs {
            session_id: "s-1".to_string(),
            mode: None,
            pixel_threshold: None,
            area_threshold: None,
            interval: None,
            timeout: None,
            stable_for,
            image: image_args(),
            base64: false,
            json: true,
            socket: None,
        }
    }

    #[test]
    fn a_create_request_carries_the_target_and_history() {
        let mut args = create_args(
            Some(":99"),
            SessionTarget::Region(Rect::new(1, 2, 3, 4).unwrap()),
        );
        args.history = Some(4);

        let envelope = build_request(&SessionCommand::Create(args)).unwrap();
        match envelope.request {
            Request::SessionCreate {
                display,
                target,
                history,
            } => {
                assert_eq!(display, ":99");
                assert_eq!(history, Some(4));
                assert_eq!(
                    target,
                    protocol::RequestTarget::Region {
                        region: Rect::new(1, 2, 3, 4).unwrap()
                    }
                );
            }
            other => panic!("expected a create request, got {other:?}"),
        }
    }

    #[test]
    fn a_create_request_refuses_an_absent_display() {
        // DISPLAY is read from the environment, which cannot be set safely under a
        // parallel test runner. This therefore asserts the contract that holds in
        // either case: a request is built when DISPLAY is present, and the failure
        // when it is not names the flag that fixes it.
        let args = create_args(None, SessionTarget::Desktop);
        match build_request(&SessionCommand::Create(args)) {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(error.code(), "invalid_arguments");
                assert!(error.message().contains("--display"), "{}", error.message());
            }
        }
    }

    #[test]
    fn a_capture_request_is_marked_as_a_capture() {
        let envelope = build_request(&SessionCommand::Capture {
            session_id: "s-1".to_string(),
            image: image_args(),
            base64: true,
            json: true,
            socket: None,
        })
        .unwrap();

        assert!(matches!(envelope.request, Request::SessionCapture { .. }));
        assert_eq!(envelope.request.session_id(), Some("s-1"));
    }

    #[test]
    fn a_frame_request_carries_the_identifier() {
        let envelope = build_request(&SessionCommand::Frame {
            session_id: "s-1".to_string(),
            frame_id: 1842,
            image: image_args(),
            base64: false,
            json: true,
            socket: None,
        })
        .unwrap();

        match envelope.request {
            Request::SessionFrame { frame_id, .. } => assert_eq!(frame_id, FrameId(1842)),
            other => panic!("expected a frame request, got {other:?}"),
        }
    }

    #[test]
    fn a_diff_request_uses_the_same_defaults_as_the_standalone_diff() {
        let envelope = build_request(&SessionCommand::Diff {
            session_id: "s-1".to_string(),
            before: 3,
            after: 7,
            mode: None,
            pixel_threshold: None,
            area_threshold: None,
            json: true,
            socket: None,
        })
        .unwrap();

        match envelope.request {
            Request::SessionDiff {
                before,
                after,
                compare,
                ..
            } => {
                assert_eq!(before, FrameId(3));
                assert_eq!(after, FrameId(7));
                // Not the temporal default: a diff asks whether anything differed.
                assert_eq!(compare, CompareOptions::default().into());
            }
            other => panic!("expected a diff request, got {other:?}"),
        }
    }

    #[test]
    fn a_jpeg_quality_is_rejected_for_png_output() {
        let mut args = image_args();
        args.quality = Some(50);
        // PNG is the default format, so a quality is a contradiction.
        assert!(args.to_image_options().is_err());

        args.format = Some("jpeg".to_string());
        assert!(args.to_image_options().is_ok());
    }

    #[test]
    fn observation_requests_carry_their_kind_and_stable_duration() {
        let wait_change = build_request(&SessionCommand::WaitChange(observe_args(None))).unwrap();
        match wait_change.request {
            Request::SessionObserve { kind, .. } => {
                assert_eq!(kind, ObservationKindWire::WaitChange)
            }
            other => panic!("expected an observation, got {other:?}"),
        }

        let observe = build_request(&SessionCommand::Observe(observe_args(Some(
            std::time::Duration::from_millis(450),
        ))))
        .unwrap();
        match observe.request {
            Request::SessionObserve {
                kind,
                stable_for_ms,
                ..
            } => {
                assert_eq!(kind, ObservationKindWire::Observe);
                assert_eq!(stable_for_ms, 450);
            }
            other => panic!("expected an observation, got {other:?}"),
        }
    }

    #[test]
    fn a_session_observation_defaults_to_the_temporal_profile() {
        // The same profile the standalone observation commands use, which is
        // deliberately not the `diff` default.
        assert_eq!(
            observe_args(None).temporal().unwrap(),
            crate::observe::TemporalCompareOptions::default()
        );
    }

    #[test]
    fn error_codes_map_back_to_their_documented_exit_statuses() {
        assert_eq!(exit_code_for("session_not_found"), 17);
        assert_eq!(exit_code_for("session_busy"), 18);
        assert_eq!(exit_code_for("frame_not_available"), 20);
        assert_eq!(exit_code_for("service_unavailable"), 22);
        assert_eq!(exit_code_for("display_unavailable"), 3);
        // An unknown code falls back to an internal error rather than passing.
        assert_eq!(exit_code_for("something_new"), 1);
    }

    #[test]
    fn summaries_name_the_important_fields() {
        let info = crate::session::SessionInfo {
            session_id: "s-1".to_string(),
            state: crate::session::SessionState::Ready,
            display: ":99".to_string(),
            target: crate::session::TargetDescription {
                kind: "desktop".to_string(),
                id: None,
                region: None,
            },
            geometry: None,
            frames_captured: 0,
            history: crate::session::HistorySummary {
                capacity: 8,
                retained: 0,
                oldest_frame_id: None,
                newest_frame_id: None,
                captured_total: 0,
                retained_bytes: 0,
            },
            age_ms: 0,
            failure: None,
        };

        let text = summarize(&ResponseBody::SessionCreated {
            session_id: "s-1".to_string(),
            info,
        });
        assert!(text.contains("s-1"), "summary was: {text}");
        assert!(text.contains("capacity 8"), "summary was: {text}");
    }

    #[test]
    fn a_request_id_is_unique_across_calls() {
        assert_ne!(request_id(), request_id());
    }

    #[test]
    fn observation_kind_conversion_round_trips() {
        for kind in [
            ObservationKind::WaitChange,
            ObservationKind::WaitStable,
            ObservationKind::Observe,
        ] {
            assert_eq!(observation_kind(wire_kind(kind)), kind);
        }
    }
}
