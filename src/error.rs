//! Structured, automation-friendly error model.
//!
//! Every failure surfaced by the CLI is mapped to one of a small, stable set
//! of error classes. Each class has:
//!
//! * a machine readable `code` used in JSON error responses, and
//! * a stable process exit status.
//!
//! ## Exit codes
//!
//! | code | error class          | meaning                                        |
//! |------|----------------------|------------------------------------------------|
//! | 0    | *none*               | success                                        |
//! | 1    | `internal_error`     | unexpected internal failure                    |
//! | 2    | `invalid_arguments`  | unparsable or contradictory CLI arguments      |
//! | 3    | `display_unavailable`| X11 display could not be opened                |
//! | 4    | `invalid_region`     | region is malformed, zero sized, or out of bounds |
//! | 5    | `window_not_found`   | the requested X11 window does not exist        |
//! | 6    | `capture_failed`     | the X11 backend failed to produce a frame      |
//! | 7    | `resize_failed`      | the requested resize could not be performed    |
//! | 8    | `encode_failed`      | PNG/JPEG encoding failed                       |
//! | 9    | `output_failed`      | writing the result to disk or stdout failed    |
//! | 10   | `incompatible_frames`| the two frames cannot be compared              |
//! | 11   | `comparison_failed`  | the comparison could not be performed          |
//! | 12   | `image_load_failed`  | an input image could not be read or decoded    |
//! | 13   | `geometry_changed`   | the observed target changed shape mid-observation |
//! | 14   | `target_lost`        | the observed target disappeared mid-observation |
//! | 15   | `invalid_duration`   | a duration argument was zero, negative, or unparsable |
//! | 16   | `observation_failed` | the observation could not be performed         |
//! | 17   | `session_not_found`  | the requested capture session does not exist   |
//! | 18   | `session_busy`       | the session is already running another operation |
//! | 19   | `session_closed`     | the session was closed while the request ran    |
//! | 20   | `frame_not_available`| the requested frame is not in the session's history |
//! | 21   | `service_unavailable`| the local service could not be reached         |
//! | 22   | `service_protocol_error` | the service sent an unusable response       |
//! | 23   | `service_overloaded` | the service refused the request as busy        |
//! | 24   | `no_frame_available` | the session has not captured a frame yet       |
//!
//! ## JSON shape
//!
//! ```json
//! { "error": { "code": "display_unavailable", "message": "..." } }
//! ```

use std::fmt;

/// The error type used throughout `eensh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Unparsable or mutually contradictory CLI arguments.
    InvalidArguments(String),
    /// The X11 display could not be opened or did not exist.
    DisplayUnavailable { display: String, detail: String },
    /// The requested source region was malformed or out of bounds.
    InvalidRegion(String),
    /// The requested X11 window does not exist.
    WindowNotFound { window_id: String },
    /// The capture backend failed to produce a frame.
    CaptureFailed(String),
    /// The image could not be resized.
    ResizeFailed(String),
    /// The image could not be encoded.
    EncodeFailed(String),
    /// The result could not be written to its destination.
    OutputFailed(String),
    /// The two frames cannot be compared, for example because their sizes differ.
    IncompatibleFrames(String),
    /// The comparison could not be performed.
    ComparisonFailed(String),
    /// An input image could not be read or decoded.
    ImageLoadFailed(String),
    /// The observed target changed geometry during an observation.
    GeometryChanged(String),
    /// The observed target disappeared during an observation.
    TargetLost(String),
    /// A duration argument was zero, negative, or unparsable.
    InvalidDuration(String),
    /// The observation could not be performed.
    ObservationFailed(String),
    /// The requested capture session does not exist.
    SessionNotFound(String),
    /// The session is already running another operation.
    SessionBusy(String),
    /// The session was closed while the request was running.
    SessionClosed(String),
    /// The requested frame is not in the session's history.
    FrameNotAvailable(String),
    /// The session has not captured a frame yet.
    NoFrameAvailable(String),
    /// The local service could not be reached.
    ServiceUnavailable(String),
    /// The service sent an unusable or malformed response.
    ServiceProtocolError(String),
    /// The service refused the request because it is overloaded.
    ServiceOverloaded(String),
    /// An unexpected internal failure.
    Internal(String),
}

impl Error {
    /// Convenience constructor for [`Error::InvalidArguments`].
    pub fn invalid_arguments(message: impl Into<String>) -> Self {
        Error::InvalidArguments(message.into())
    }

    /// Convenience constructor for [`Error::InvalidRegion`].
    pub fn invalid_region(message: impl Into<String>) -> Self {
        Error::InvalidRegion(message.into())
    }

    /// Convenience constructor for [`Error::DisplayUnavailable`].
    pub fn display_unavailable(display: impl Into<String>, detail: impl Into<String>) -> Self {
        Error::DisplayUnavailable {
            display: display.into(),
            detail: detail.into(),
        }
    }

    /// Convenience constructor for [`Error::CaptureFailed`].
    pub fn capture_failed(message: impl Into<String>) -> Self {
        Error::CaptureFailed(message.into())
    }

    /// Convenience constructor for [`Error::EncodeFailed`].
    pub fn encode_failed(message: impl Into<String>) -> Self {
        Error::EncodeFailed(message.into())
    }

    /// Convenience constructor for [`Error::OutputFailed`].
    pub fn output_failed(message: impl Into<String>) -> Self {
        Error::OutputFailed(message.into())
    }

    /// Convenience constructor for [`Error::IncompatibleFrames`].
    pub fn incompatible_frames(message: impl Into<String>) -> Self {
        Error::IncompatibleFrames(message.into())
    }

    /// Convenience constructor for [`Error::ComparisonFailed`].
    pub fn comparison_failed(message: impl Into<String>) -> Self {
        Error::ComparisonFailed(message.into())
    }

    /// Convenience constructor for [`Error::ImageLoadFailed`].
    pub fn image_load_failed(message: impl Into<String>) -> Self {
        Error::ImageLoadFailed(message.into())
    }

    /// Convenience constructor for [`Error::GeometryChanged`].
    pub fn geometry_changed(message: impl Into<String>) -> Self {
        Error::GeometryChanged(message.into())
    }

    /// Convenience constructor for [`Error::TargetLost`].
    pub fn target_lost(message: impl Into<String>) -> Self {
        Error::TargetLost(message.into())
    }

    /// Convenience constructor for [`Error::InvalidDuration`].
    pub fn invalid_duration(message: impl Into<String>) -> Self {
        Error::InvalidDuration(message.into())
    }

    /// Convenience constructor for [`Error::ObservationFailed`].
    pub fn observation_failed(message: impl Into<String>) -> Self {
        Error::ObservationFailed(message.into())
    }

    /// Convenience constructor for [`Error::SessionNotFound`].
    pub fn session_not_found(message: impl Into<String>) -> Self {
        Error::SessionNotFound(message.into())
    }

    /// Convenience constructor for [`Error::SessionBusy`].
    pub fn session_busy(message: impl Into<String>) -> Self {
        Error::SessionBusy(message.into())
    }

    /// Convenience constructor for [`Error::SessionClosed`].
    pub fn session_closed(message: impl Into<String>) -> Self {
        Error::SessionClosed(message.into())
    }

    /// Convenience constructor for [`Error::FrameNotAvailable`].
    pub fn frame_not_available(message: impl Into<String>) -> Self {
        Error::FrameNotAvailable(message.into())
    }

    /// Convenience constructor for [`Error::NoFrameAvailable`].
    pub fn no_frame_available(message: impl Into<String>) -> Self {
        Error::NoFrameAvailable(message.into())
    }

    /// Convenience constructor for [`Error::ServiceUnavailable`].
    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Error::ServiceUnavailable(message.into())
    }

    /// Convenience constructor for [`Error::ServiceProtocolError`].
    pub fn service_protocol_error(message: impl Into<String>) -> Self {
        Error::ServiceProtocolError(message.into())
    }

    /// Convenience constructor for [`Error::ServiceOverloaded`].
    pub fn service_overloaded(message: impl Into<String>) -> Self {
        Error::ServiceOverloaded(message.into())
    }

    /// Rebuild an error from a service's error code.
    ///
    /// A client receives a stable code and a human message across the socket. The
    /// code is the contract, so it is mapped back to the variant it names — which
    /// means a client can branch on `err.code()` exactly as it would in-process, and
    /// a `session_busy` from the service is still recognisably a `session_busy`.
    ///
    /// An unrecognised code becomes a protocol error carrying the original code in
    /// its message, rather than being silently coerced to something it is not. A
    /// client built against a newer service should be told it does not understand the
    /// answer.
    pub fn from_service_code(code: &str, message: &str) -> Self {
        match code {
            "invalid_arguments" => Error::InvalidArguments(message.to_string()),
            "display_unavailable" => Error::DisplayUnavailable {
                display: String::new(),
                detail: message.to_string(),
            },
            "invalid_region" => Error::InvalidRegion(message.to_string()),
            "window_not_found" => Error::WindowNotFound {
                window_id: message.to_string(),
            },
            "capture_failed" => Error::CaptureFailed(message.to_string()),
            "resize_failed" => Error::ResizeFailed(message.to_string()),
            "encode_failed" => Error::EncodeFailed(message.to_string()),
            "output_failed" => Error::OutputFailed(message.to_string()),
            "incompatible_frames" => Error::IncompatibleFrames(message.to_string()),
            "comparison_failed" => Error::ComparisonFailed(message.to_string()),
            "image_load_failed" => Error::ImageLoadFailed(message.to_string()),
            "geometry_changed" => Error::GeometryChanged(message.to_string()),
            "target_lost" => Error::TargetLost(message.to_string()),
            "invalid_duration" => Error::InvalidDuration(message.to_string()),
            "observation_failed" => Error::ObservationFailed(message.to_string()),
            "session_not_found" => Error::SessionNotFound(message.to_string()),
            "session_busy" => Error::SessionBusy(message.to_string()),
            "session_closed" => Error::SessionClosed(message.to_string()),
            "frame_not_available" => Error::FrameNotAvailable(message.to_string()),
            "no_frame_available" => Error::NoFrameAvailable(message.to_string()),
            "service_unavailable" => Error::ServiceUnavailable(message.to_string()),
            "service_overloaded" => Error::ServiceOverloaded(message.to_string()),
            "service_protocol_error" => Error::ServiceProtocolError(message.to_string()),
            other => Error::service_protocol_error(format!(
                "the service reported {other:?}, which this client does not recognise: \
                 {message}"
            )),
        }
    }

    /// Whether this error means the request should not be retried against the
    /// same session.
    ///
    /// Used by the service to decide whether a session is still usable.
    pub fn is_session_fatal(&self) -> bool {
        matches!(
            self,
            Error::SessionNotFound(_)
                | Error::SessionClosed(_)
                | Error::TargetLost(_)
                | Error::DisplayUnavailable { .. }
        )
    }

    /// Convenience constructor for [`Error::Internal`].
    pub fn internal(message: impl Into<String>) -> Self {
        Error::Internal(message.into())
    }

    /// Stable machine readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Error::InvalidArguments(_) => "invalid_arguments",
            Error::DisplayUnavailable { .. } => "display_unavailable",
            Error::InvalidRegion(_) => "invalid_region",
            Error::WindowNotFound { .. } => "window_not_found",
            Error::CaptureFailed(_) => "capture_failed",
            Error::ResizeFailed(_) => "resize_failed",
            Error::EncodeFailed(_) => "encode_failed",
            Error::OutputFailed(_) => "output_failed",
            Error::IncompatibleFrames(_) => "incompatible_frames",
            Error::ComparisonFailed(_) => "comparison_failed",
            Error::ImageLoadFailed(_) => "image_load_failed",
            Error::GeometryChanged(_) => "geometry_changed",
            Error::TargetLost(_) => "target_lost",
            Error::InvalidDuration(_) => "invalid_duration",
            Error::ObservationFailed(_) => "observation_failed",
            Error::SessionNotFound(_) => "session_not_found",
            Error::SessionBusy(_) => "session_busy",
            Error::SessionClosed(_) => "session_closed",
            Error::FrameNotAvailable(_) => "frame_not_available",
            Error::NoFrameAvailable(_) => "no_frame_available",
            Error::ServiceUnavailable(_) => "service_unavailable",
            Error::ServiceProtocolError(_) => "service_protocol_error",
            Error::ServiceOverloaded(_) => "service_overloaded",
            Error::Internal(_) => "internal_error",
        }
    }

    /// Stable process exit status for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Internal(_) => 1,
            Error::InvalidArguments(_) => 2,
            Error::DisplayUnavailable { .. } => 3,
            Error::InvalidRegion(_) => 4,
            Error::WindowNotFound { .. } => 5,
            Error::CaptureFailed(_) => 6,
            Error::ResizeFailed(_) => 7,
            Error::EncodeFailed(_) => 8,
            Error::OutputFailed(_) => 9,
            Error::IncompatibleFrames(_) => 10,
            Error::ComparisonFailed(_) => 11,
            Error::ImageLoadFailed(_) => 12,
            Error::GeometryChanged(_) => 13,
            Error::TargetLost(_) => 14,
            Error::InvalidDuration(_) => 15,
            Error::ObservationFailed(_) => 16,
            Error::SessionNotFound(_) => 17,
            Error::SessionBusy(_) => 18,
            Error::SessionClosed(_) => 19,
            Error::FrameNotAvailable(_) => 20,
            Error::NoFrameAvailable(_) => 21,
            Error::ServiceUnavailable(_) => 22,
            Error::ServiceProtocolError(_) => 23,
            Error::ServiceOverloaded(_) => 24,
        }
    }

    /// Concise human readable message.
    pub fn message(&self) -> String {
        match self {
            Error::InvalidArguments(m) => format!("invalid arguments: {m}"),
            Error::DisplayUnavailable { display, detail } => {
                format!("unable to connect to X11 display {display}: {detail}")
            }
            Error::InvalidRegion(m) => format!("invalid region: {m}"),
            Error::WindowNotFound { window_id } => {
                format!("X11 window not found: {window_id}")
            }
            Error::CaptureFailed(m) => format!("capture failed: {m}"),
            Error::ResizeFailed(m) => format!("resize failed: {m}"),
            Error::EncodeFailed(m) => format!("encode failed: {m}"),
            Error::OutputFailed(m) => format!("output failed: {m}"),
            Error::IncompatibleFrames(m) => format!("incompatible frames: {m}"),
            Error::ComparisonFailed(m) => format!("comparison failed: {m}"),
            Error::ImageLoadFailed(m) => format!("image load failed: {m}"),
            Error::GeometryChanged(m) => format!("geometry changed: {m}"),
            Error::TargetLost(m) => format!("target lost: {m}"),
            Error::InvalidDuration(m) => format!("invalid duration: {m}"),
            Error::ObservationFailed(m) => format!("observation failed: {m}"),
            Error::SessionNotFound(m) => format!("session not found: {m}"),
            Error::SessionBusy(m) => format!("session busy: {m}"),
            Error::SessionClosed(m) => format!("session closed: {m}"),
            Error::FrameNotAvailable(m) => format!("frame not available: {m}"),
            Error::NoFrameAvailable(m) => format!("no frame available: {m}"),
            Error::ServiceUnavailable(m) => format!("service unavailable: {m}"),
            Error::ServiceProtocolError(m) => format!("service protocol error: {m}"),
            Error::ServiceOverloaded(m) => format!("service overloaded: {m}"),
            Error::Internal(m) => format!("internal error: {m}"),
        }
    }

    /// Machine readable representation used by `--json` error responses.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": {
                "code": self.code(),
                "message": self.message(),
            }
        })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_exit_statuses_are_stable() {
        let cases = [
            (Error::InvalidArguments("x".into()), "invalid_arguments", 2),
            (
                Error::DisplayUnavailable {
                    display: ":99".into(),
                    detail: "boom".into(),
                },
                "display_unavailable",
                3,
            ),
            (Error::InvalidRegion("x".into()), "invalid_region", 4),
            (
                Error::WindowNotFound {
                    window_id: "0x1".into(),
                },
                "window_not_found",
                5,
            ),
            (Error::CaptureFailed("x".into()), "capture_failed", 6),
            (Error::ResizeFailed("x".into()), "resize_failed", 7),
            (Error::EncodeFailed("x".into()), "encode_failed", 8),
            (Error::OutputFailed("x".into()), "output_failed", 9),
            (
                Error::IncompatibleFrames("x".into()),
                "incompatible_frames",
                10,
            ),
            (Error::ComparisonFailed("x".into()), "comparison_failed", 11),
            (Error::ImageLoadFailed("x".into()), "image_load_failed", 12),
            (Error::GeometryChanged("x".into()), "geometry_changed", 13),
            (Error::TargetLost("x".into()), "target_lost", 14),
            (Error::InvalidDuration("x".into()), "invalid_duration", 15),
            (
                Error::ObservationFailed("x".into()),
                "observation_failed",
                16,
            ),
            (Error::SessionNotFound("x".into()), "session_not_found", 17),
            (Error::SessionBusy("x".into()), "session_busy", 18),
            (Error::SessionClosed("x".into()), "session_closed", 19),
            (
                Error::FrameNotAvailable("x".into()),
                "frame_not_available",
                20,
            ),
            (
                Error::NoFrameAvailable("x".into()),
                "no_frame_available",
                21,
            ),
            (
                Error::ServiceUnavailable("x".into()),
                "service_unavailable",
                22,
            ),
            (
                Error::ServiceProtocolError("x".into()),
                "service_protocol_error",
                23,
            ),
            (
                Error::ServiceOverloaded("x".into()),
                "service_overloaded",
                24,
            ),
            (Error::Internal("x".into()), "internal_error", 1),
        ];

        for (error, code, exit) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(error.exit_code(), exit);
            assert!(!error.message().is_empty());
        }
    }

    #[test]
    fn json_error_shape() {
        let error = Error::display_unavailable(":99", "Unable to connect");
        let value = error.to_json();
        assert_eq!(value["error"]["code"], "display_unavailable");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unable to connect to X11 display :99"));
    }
}
