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
