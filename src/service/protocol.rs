//! The service wire protocol.
//!
//! Requests and responses are JSON documents, length-delimited by a four-byte
//! big-endian prefix. That is deliberately the simplest thing that works: the
//! control plane is small and human-readable, and the only bulky field is a
//! base64 image, which is already what the agent-facing JSON carries.
//!
//! The protocol is described in terms of concepts rather than command lines. A
//! request says `capture` and carries an output description; it does not carry
//! `--width 960`. That keeps the service usable by a client that is not a shell,
//! and it keeps CLI concerns out of the daemon.
//!
//! Image bytes travel base64-encoded inside the JSON. A binary side channel would
//! save about a quarter of the bytes and cost a second framing path to get wrong;
//! for a local socket carrying a few hundred kilobytes the trade is not worth it.

use std::io::{BufRead, BufReader, Read, Write};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::observe::{ObserveOptions, WaitChangeOptions, WaitStableOptions};
use crate::pipeline::ImageOptions;
use crate::session::history::FrameId;
use crate::session::SessionConfig;

/// The protocol version this build speaks.
///
/// A client and service that disagree are rejected explicitly rather than
/// allowed to misinterpret each other's fields.
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest request or response frame the service will accept, in bytes.
///
/// A capture response can legitimately be several megabytes once a large image is
/// base64 encoded, so the limit is generous. It exists to bound a malformed or
/// hostile length prefix rather than to constrain normal use.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// What the client is asking for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    /// Report service health and protocol version.
    Ping,

    /// Create a session.
    SessionCreate {
        /// The display and target to observe.
        display: String,
        /// `desktop`, or a region, or a window.
        target: RequestTarget,
        /// How many recent frames to retain.
        #[serde(skip_serializing_if = "Option::is_none")]
        history: Option<usize>,
    },

    /// List registered sessions.
    SessionList,

    /// Report one session's status.
    SessionInfo {
        /// The session to describe.
        session_id: String,
    },

    /// Close a session.
    SessionClose {
        /// The session to close.
        session_id: String,
    },

    /// Perform a fresh capture.
    ///
    /// Distinct from [`Request::SessionLatest`] on purpose: this looks at the
    /// screen now, that one reports what is already held.
    SessionCapture {
        /// The session to capture from.
        session_id: String,
        /// How to present the returned frame.
        output: ImageOptionsWire,
        /// The Phase 6 presentation policy, when one was supplied.
        ///
        /// Absent means "Phase 5 behaviour": one whole-frame image, exactly as
        /// before. That is what makes Phase 6 additive — a caller that knows nothing
        /// about presentation sends nothing and gets what it always got.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<Box<crate::presentation::ObservationPolicy>>,
    },

    /// Return the newest retained frame without capturing.
    SessionLatest {
        /// The session to read.
        session_id: String,
        /// How to present the returned frame.
        output: ImageOptionsWire,
        /// The Phase 6 presentation policy, when one was supplied.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<Box<crate::presentation::ObservationPolicy>>,
    },

    /// Return a specific retained frame.
    SessionFrame {
        /// The session to read.
        session_id: String,
        /// Which frame.
        frame_id: FrameId,
        /// How to present the returned frame.
        output: ImageOptionsWire,
        /// The Phase 6 presentation policy, when one was supplied.
        ///
        /// This is the path that makes re-presentation possible: any retained frame
        /// can be rendered again under a different policy without allocating a new
        /// frame identity (requirement 42).
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<Box<crate::presentation::ObservationPolicy>>,
    },

    /// Compare two retained frames.
    SessionDiff {
        /// The session.
        session_id: String,
        /// The earlier frame.
        before: FrameId,
        /// The later frame.
        after: FrameId,
        /// Comparison settings.
        compare: CompareOptionsWire,
        /// How to deliver the changed region, when one was requested.
        #[serde(skip_serializing_if = "Option::is_none")]
        changed: Option<crate::presentation::ChangedRegionPolicy>,
    },

    /// Run a temporal observation inside a session.
    SessionObserve {
        /// The session to observe.
        session_id: String,
        /// Which observation.
        kind: ObservationKindWire,
        /// Comparison, cadence, and deadline.
        temporal: TemporalWire,
        /// Required stillness, for the operations that have it.
        stable_for_ms: u64,
        /// How to present the final frame.
        output: ImageOptionsWire,
        /// The Phase 6 presentation policy, when one was supplied.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<Box<crate::presentation::ObservationPolicy>>,
    },

    /// Capture a bounded real-time temporal stack from a session.
    ///
    /// Distinct from [`Request::SessionObserve`] in the same way the two operations
    /// are: an observation waits for a condition, a real-time sample does not wait for
    /// anything. It samples the current visual state over a short interval and
    /// returns what it captured.
    SessionRealtime {
        /// The session to sample.
        session_id: String,
        /// How many frames, how far apart, and by when.
        realtime: RealtimeWire,
        /// How to present every returned frame.
        output: ImageOptionsWire,
        /// The Phase 6 presentation policy, when one was supplied.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<Box<crate::presentation::ObservationPolicy>>,
    },
}

/// Real-time sampling parameters over the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeWire {
    /// How many physical frames to capture, 1..=[`crate::realtime::MAX_FRAMES`].
    pub frames: usize,
    /// The cadence between scheduled sample opportunities.
    pub interval_ms: u64,
    /// The deadline after which no new capture is started.
    pub timeout_ms: u64,
}

impl From<RealtimeWire> for crate::realtime::RealtimeOptions {
    fn from(wire: RealtimeWire) -> Self {
        crate::realtime::RealtimeOptions {
            frames: wire.frames,
            interval: std::time::Duration::from_millis(wire.interval_ms),
            timeout: std::time::Duration::from_millis(wire.timeout_ms),
        }
    }
}

impl From<crate::realtime::RealtimeOptions> for RealtimeWire {
    fn from(options: crate::realtime::RealtimeOptions) -> Self {
        RealtimeWire {
            frames: options.frames,
            interval_ms: options.interval.as_millis() as u64,
            timeout_ms: options.timeout.as_millis() as u64,
        }
    }
}

/// The target part of a session-create request.
///
/// This mirrors [`crate::capture::CaptureRequest`] but is serializable, which the
/// capture enum is not: a window identifier and a rectangle need different shapes
/// and cannot share one tagged representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestTarget {
    /// The whole root window.
    Desktop,
    /// A rectangle in root-window coordinates.
    Region {
        /// The rectangle.
        region: crate::geometry::Rect,
    },
    /// A specific X11 window.
    Window {
        /// The window identifier.
        id: u64,
    },
}

impl RequestTarget {
    /// Convert to the capture backend's request.
    pub fn to_capture_request(&self) -> crate::capture::CaptureRequest {
        match self {
            RequestTarget::Desktop => crate::capture::CaptureRequest::Desktop,
            RequestTarget::Region { region } => crate::capture::CaptureRequest::Region(*region),
            RequestTarget::Window { id } => crate::capture::CaptureRequest::Window(*id),
        }
    }
}

/// Which temporal observation to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKindWire {
    /// `wait-change`.
    WaitChange,
    /// `wait-stable`.
    WaitStable,
    /// `observe`.
    Observe,
}

/// Comparison settings over the wire.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompareOptionsWire {
    /// `exact` or `rgb_threshold`.
    pub mode: crate::compare::CompareMode,
    /// Per-pixel threshold.
    pub pixel_threshold: u8,
    /// Area threshold.
    pub area_threshold: f64,
}

impl From<CompareOptionsWire> for crate::compare::CompareOptions {
    fn from(wire: CompareOptionsWire) -> Self {
        crate::compare::CompareOptions {
            mode: wire.mode,
            pixel_threshold: wire.pixel_threshold,
            area_threshold: wire.area_threshold,
        }
    }
}

impl From<crate::compare::CompareOptions> for CompareOptionsWire {
    fn from(options: crate::compare::CompareOptions) -> Self {
        CompareOptionsWire {
            mode: options.mode,
            pixel_threshold: options.pixel_threshold,
            area_threshold: options.area_threshold,
        }
    }
}

/// Temporal settings over the wire.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TemporalWire {
    /// Comparison settings.
    pub compare: CompareOptionsWire,
    /// Cadence between samples.
    pub interval_ms: u64,
    /// Total deadline.
    pub timeout_ms: u64,
}

impl From<TemporalWire> for crate::observe::TemporalCompareOptions {
    fn from(wire: TemporalWire) -> Self {
        crate::observe::TemporalCompareOptions {
            compare: wire.compare.into(),
            interval: std::time::Duration::from_millis(wire.interval_ms),
            timeout: std::time::Duration::from_millis(wire.timeout_ms),
        }
    }
}

impl From<crate::observe::TemporalCompareOptions> for TemporalWire {
    fn from(temporal: crate::observe::TemporalCompareOptions) -> Self {
        TemporalWire {
            compare: temporal.compare.into(),
            interval_ms: temporal.interval.as_millis() as u64,
            timeout_ms: temporal.timeout.as_millis() as u64,
        }
    }
}

impl TemporalWire {
    /// Build `wait-change` options from the wire form.
    pub fn to_wait_change(self) -> WaitChangeOptions {
        WaitChangeOptions {
            temporal: self.into(),
        }
    }

    /// Build `wait-stable` options from the wire form.
    pub fn to_wait_stable(self, stable_for_ms: u64) -> WaitStableOptions {
        WaitStableOptions {
            temporal: self.into(),
            stable_for: std::time::Duration::from_millis(stable_for_ms),
        }
    }

    /// Build `observe` options from the wire form.
    pub fn to_observe(self, stable_for_ms: u64) -> ObserveOptions {
        ObserveOptions {
            temporal: self.into(),
            stable_for: std::time::Duration::from_millis(stable_for_ms),
        }
    }
}

/// Image presentation settings over the wire.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ImageOptionsWire {
    /// Output format.
    pub format: crate::encode::ImageFormat,
    /// JPEG quality.
    pub quality: u8,
    /// PNG effort.
    pub png_effort: crate::encode::PngEffort,
    /// Target width, preserving the aspect ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Target height, preserving the aspect ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Uniform scale factor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// Whether to embed the image as base64.
    pub base64: bool,
}

impl ImageOptionsWire {
    /// The default presentation: PNG, no resize, inline as base64.
    pub fn default_png_base64() -> Self {
        ImageOptionsWire {
            format: crate::encode::ImageFormat::Png,
            quality: crate::encode::jpeg::DEFAULT_QUALITY,
            png_effort: crate::encode::PngEffort::Default,
            width: None,
            height: None,
            scale: None,
            base64: true,
        }
    }

    /// Convert to the pipeline's image options.
    pub fn to_image_options(&self) -> Result<ImageOptions, Error> {
        validate_resize(self.width, self.height, self.scale)?;

        Ok(ImageOptions {
            resize: match (self.width, self.height, self.scale) {
                (Some(width), _, _) => crate::cli::ResizeRequest::Width(width),
                (_, Some(height), _) => crate::cli::ResizeRequest::Height(height),
                (_, _, Some(scale)) => crate::cli::ResizeRequest::Scale(scale),
                _ => crate::cli::ResizeRequest::None,
            },
            format: self.format,
            quality: self.quality,
            png_effort: self.png_effort,
            base64: self.base64,
        })
    }
}

/// Reject resize combinations the pipeline does not support.
///
/// Checked here, at the protocol boundary, so a malformed request is refused
/// before a display is touched.
fn validate_resize(
    width: Option<u32>,
    height: Option<u32>,
    scale: Option<f64>,
) -> Result<(), Error> {
    let specified = [width.is_some(), height.is_some(), scale.is_some()]
        .iter()
        .filter(|set| **set)
        .count();
    if specified > 1 {
        return Err(Error::invalid_arguments(
            "specify at most one of width, height, or scale; Phase 1 only supports \
             proportional resizing",
        ));
    }
    if width == Some(0) || height == Some(0) {
        return Err(Error::invalid_arguments(
            "resize dimensions must be greater than zero",
        ));
    }
    if let Some(scale) = scale {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(Error::invalid_arguments(
                "scale must be a positive, finite number",
            ));
        }
    }
    Ok(())
}

/// A request envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// Echoed back in the response, so a client can match them up.
    pub request_id: String,
    /// The protocol version the client is speaking.
    ///
    /// Checked by the service, so that a binary mismatch is reported in terms a
    /// person can act on rather than surfacing as a confusing parse error on a
    /// field that happens to have changed shape. Optional so that a hand-written
    /// client that omits it is still served; a client that *declares* an
    /// incompatible version is refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
    /// The request itself.
    #[serde(flatten)]
    pub request: Request,
}

impl RequestEnvelope {
    /// A request carrying this build's protocol version.
    pub fn new(request_id: impl Into<String>, request: Request) -> Self {
        RequestEnvelope {
            request_id: request_id.into(),
            protocol_version: Some(PROTOCOL_VERSION),
            request,
        }
    }

    /// Check the declared version against the one this service speaks.
    ///
    /// The rule is exact equality, which the spec allows as the initial policy.
    /// There is no negotiation: a client either speaks this version or is told it
    /// does not, because a partial compatibility matrix would be more code than
    /// the problem currently justifies.
    pub fn check_version(&self) -> Result<(), Error> {
        match self.protocol_version {
            None => Ok(()),
            Some(version) if version == PROTOCOL_VERSION => Ok(()),
            Some(version) => Err(Error::service_protocol_error(format!(
                "protocol version {version} is not supported; this service speaks version \
                 {PROTOCOL_VERSION}. The client and the service are probably different builds."
            ))),
        }
    }
}

/// A response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    /// The request this answers.
    pub request_id: String,
    /// Whether the request succeeded.
    pub ok: bool,
    /// The result, when it succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ResponseBody>,
    /// The structured error, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

/// The structured error shape on the wire.
///
/// The `code` is what a client should branch on; the message is for a human
/// reading a log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Stable machine-readable code.
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

impl ErrorBody {
    /// Convert a service error into its wire form.
    pub fn from_error(error: &Error) -> Self {
        ErrorBody {
            code: error.code().to_string(),
            message: error.message(),
        }
    }

    /// Convert back into a service error.
    ///
    /// Client-side errors are reconstructed as [`Error::Internal`] with the
    /// original code preserved in the message, because the service's error
    /// taxonomy is not meaningful for a client's own control flow.
    pub fn to_error(&self) -> Error {
        Error::service_protocol_error(format!("{}: {}", self.code, self.message))
    }
}

/// The successful response body, one variant per method.
///
/// Several variants carry a `Box`ed payload. That is not incidental: a response
/// carrying a multi-view presentation is an order of magnitude larger than a status
/// reply, and without the indirection every `ResponseBody` — including a one-field
/// ping answer — would be sized for the largest one. Boxing keeps the common case
/// cheap and makes the size of the rare case explicit at the point it is built.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseBody {
    /// Answer to [`Request::Ping`].
    Status {
        /// The service's health report.
        status: crate::session::pipeline::ServiceStatus,
    },
    /// Answer to [`Request::SessionCreate`].
    SessionCreated {
        /// The new session's identifier.
        session_id: String,
        /// Its initial status.
        info: crate::session::SessionInfo,
    },
    /// Answer to [`Request::SessionList`].
    SessionList {
        /// One entry per session.
        sessions: Vec<crate::session::SessionInfo>,
    },
    /// Answer to [`Request::SessionInfo`].
    SessionInfo {
        /// The requested status.
        info: crate::session::SessionInfo,
    },
    /// Answer to [`Request::SessionClose`].
    SessionClosed {
        /// The closed session.
        session_id: String,
    },
    /// Answer to a frame request.
    Frame {
        /// The frame result.
        frame: Box<crate::session::pipeline::SessionFrameResponse>,
        /// The Phase 6 multi-view presentation, when a policy was supplied.
        ///
        /// Absent entirely when no policy was supplied, which is what keeps the
        /// Phase 5 response shape unchanged rather than merely equivalent.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<Box<crate::output::json::PresentationResponse>>,
    },
    /// Answer to [`Request::SessionDiff`].
    Diff {
        /// The comparison result.
        diff: crate::session::pipeline::SessionDiffResponse,
        /// The changed-region view, when one was requested.
        #[serde(skip_serializing_if = "Option::is_none")]
        changed_view: Option<crate::output::json::ChangedRegionResponse>,
    },
    /// Answer to [`Request::SessionObserve`].
    Observation {
        /// The observation result.
        observation: Box<crate::output::json::ObservationResponse>,
        /// The Phase 6 multi-view presentation of the final frame, when a policy was
        /// supplied.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<Box<crate::output::json::PresentationResponse>>,
    },
    /// Answer to [`Request::SessionRealtime`].
    Realtime {
        /// The temporal stack and its timing.
        realtime: Box<crate::session::realtime::RealtimeResponse>,
        /// The Phase 6 multi-view presentation, when a policy was supplied.
        ///
        /// Carried *alongside* the Phase 5 response rather than replacing it, so the
        /// sampling metadata — offsets, skips, ages, sampling timing — stays exactly
        /// where a Phase 5 caller looks for it.
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<Box<crate::output::json::PresentationResponse>>,
    },
}

/// Write a length-delimited JSON frame.
///
/// The length prefix is four bytes, big-endian, and counts only the JSON body.
pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<(), Error> {
    let body = serde_json::to_vec(value)
        .map_err(|e| Error::service_protocol_error(format!("could not encode a request: {e}")))?;

    if body.len() > MAX_FRAME_BYTES {
        return Err(Error::service_protocol_error(format!(
            "message of {} bytes exceeds the {} byte limit",
            body.len(),
            MAX_FRAME_BYTES
        )));
    }

    let length = (body.len() as u32).to_be_bytes();
    writer
        .write_all(&length)
        .and_then(|_| writer.write_all(&body))
        .and_then(|_| writer.flush())
        .map_err(|e| Error::service_unavailable(format!("could not send a message: {e}")))
}

/// Read a length-delimited JSON frame.
///
/// Returns `Ok(None)` at a clean end of stream, so a client disconnecting is not
/// an error condition for the service.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(
    reader: &mut BufReader<R>,
) -> Result<Option<T>, Error> {
    let Some(body) = read_body(reader)? else {
        return Ok(None);
    };

    let value = serde_json::from_slice(&body)
        .map_err(|e| Error::service_protocol_error(format!("could not decode a message: {e}")))?;

    Ok(Some(value))
}

/// Read one length-delimited message body.
///
/// Returns `Ok(None)` at a clean end of stream. Shared between [`read_frame`] and
/// [`read_incoming`] so the framing rules live in one place.
fn read_body<R: Read>(reader: &mut BufReader<R>) -> Result<Option<Vec<u8>>, Error> {
    let mut length_bytes = [0u8; 4];
    match reader.read_exact(&mut length_bytes) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => {
            return Err(Error::service_protocol_error(format!(
                "could not read a message length: {e}"
            )))
        }
    }

    let length = u32::from_be_bytes(length_bytes) as usize;
    if length == 0 {
        return Err(Error::service_protocol_error("a message length of zero"));
    }
    if length > MAX_FRAME_BYTES {
        // Refuse before allocating, so a bogus prefix cannot make the service
        // reserve gigabytes.
        return Err(Error::service_protocol_error(format!(
            "message of {length} bytes exceeds the {MAX_FRAME_BYTES} byte limit"
        )));
    }

    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).map_err(|e| {
        Error::service_protocol_error(format!("could not read a message body: {e}"))
    })?;

    Ok(Some(body))
}

/// An incoming request, or a body that could not be interpreted as one.
#[derive(Debug)]
pub enum IncomingRequest {
    /// A request that decoded.
    Request(RequestEnvelope),
    /// A body that arrived intact but could not be decoded as a request.
    ///
    /// Carries the best request id that could be salvaged, so the service can tell
    /// the client *which* of its requests was rejected. Without that, a client
    /// with more than one request in flight cannot tell them apart.
    Malformed {
        /// The recovered request id, or an empty string if none was found.
        request_id: String,
        /// Why the body was rejected.
        error: Error,
    },
}

/// Read a request, distinguishing a framing failure from an undecodable body.
///
/// A framing failure is fatal to the connection, because the length prefixes can
/// no longer be trusted. An undecodable body is not: the framing is intact, so the
/// connection can continue and the client is told what was wrong.
pub fn read_incoming<R: Read>(reader: &mut BufReader<R>) -> Result<Option<IncomingRequest>, Error> {
    let Some(body) = read_body(reader)? else {
        return Ok(None);
    };

    match serde_json::from_slice::<RequestEnvelope>(&body) {
        Ok(envelope) => Ok(Some(IncomingRequest::Request(envelope))),
        Err(error) => Ok(Some(IncomingRequest::Malformed {
            request_id: salvage_request_id(&body),
            error: Error::service_protocol_error(format!("could not decode a message: {error}")),
        })),
    }
}

/// Recover a `request_id` from a body that failed to decode as a request.
///
/// Deliberately forgiving: the body may be syntactically valid JSON with the wrong
/// shape, or partly malformed. Only a string `request_id` is taken, and anything
/// else yields an empty string rather than an error, because this is a best-effort
/// aid to the client and must never itself fail.
fn salvage_request_id(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("request_id")?.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Read a frame from a plain reader, buffering it.
///
/// Convenience for tests and for clients that hold a plain stream.
pub fn read_frame_from<R: Read, T: for<'de> Deserialize<'de>>(
    reader: R,
) -> Result<Option<T>, Error> {
    let mut buffered = BufReader::new(reader);
    read_frame(&mut buffered)
}

/// Build a session configuration from a create request.
pub fn session_config_from(
    display: String,
    target: &RequestTarget,
    history: Option<usize>,
) -> Result<SessionConfig, Error> {
    let target = crate::session::TargetSpec {
        display,
        request: target.to_capture_request(),
    };
    let config = SessionConfig::new(target);
    Ok(match history {
        Some(capacity) => config.with_capacity(capacity),
        None => config,
    })
}

/// Convenience: whether a request needs a session lock.
impl Request {
    /// The session this request refers to, if any.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Request::Ping | Request::SessionCreate { .. } | Request::SessionList => None,
            Request::SessionInfo { session_id }
            | Request::SessionClose { session_id }
            | Request::SessionCapture { session_id, .. }
            | Request::SessionLatest { session_id, .. }
            | Request::SessionFrame { session_id, .. }
            | Request::SessionDiff { session_id, .. }
            | Request::SessionObserve { session_id, .. }
            | Request::SessionRealtime { session_id, .. } => Some(session_id),
        }
    }

    /// A short name for diagnostics.
    pub fn method(&self) -> &'static str {
        match self {
            Request::Ping => "ping",
            Request::SessionCreate { .. } => "session_create",
            Request::SessionList => "session_list",
            Request::SessionInfo { .. } => "session_info",
            Request::SessionClose { .. } => "session_close",
            Request::SessionCapture { .. } => "session_capture",
            Request::SessionLatest { .. } => "session_latest",
            Request::SessionFrame { .. } => "session_frame",
            Request::SessionDiff { .. } => "session_diff",
            Request::SessionObserve { .. } => "session_observe",
            Request::SessionRealtime { .. } => "session_realtime",
        }
    }
}

/// A line-delimited text helper, used only by the service's smoke test path.
pub fn read_line_trimmed<R: BufRead>(reader: &mut R) -> Result<Option<String>, Error> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(line.trim().to_string())),
        Err(e) => Err(Error::service_protocol_error(format!(
            "could not read a line: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn a_request_round_trips_through_a_frame() {
        let envelope = RequestEnvelope::new(
            "abc123",
            Request::SessionCapture {
                session_id: "s-1".to_string(),
                output: ImageOptionsWire::default_png_base64(),
                presentation: None,
            },
        );

        let mut buffer = Vec::new();
        write_frame(&mut buffer, &envelope).unwrap();

        let decoded: Option<RequestEnvelope> = read_frame_from(Cursor::new(buffer)).unwrap();
        assert_eq!(decoded.unwrap(), envelope);
    }

    #[test]
    fn the_length_prefix_is_big_endian_and_counts_only_the_body() {
        let envelope = RequestEnvelope::new("x", Request::Ping);
        let body = serde_json::to_vec(&envelope).unwrap();

        let mut buffer = Vec::new();
        write_frame(&mut buffer, &envelope).unwrap();

        assert_eq!(&buffer[..4], &(body.len() as u32).to_be_bytes());
        assert_eq!(&buffer[4..], &body[..]);
    }

    #[test]
    fn a_clean_end_of_stream_is_not_an_error() {
        let result: Option<RequestEnvelope> = read_frame_from(Cursor::new(Vec::new())).unwrap();
        assert!(result.is_none(), "a client disconnecting is normal");
    }

    #[test]
    fn a_truncated_body_is_a_protocol_error() {
        // Claims 100 bytes, supplies 3.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&100u32.to_be_bytes());
        bytes.extend_from_slice(b"abc");

        let error = read_frame_from::<_, RequestEnvelope>(Cursor::new(bytes)).unwrap_err();
        assert_eq!(error.code(), "service_protocol_error");
    }

    #[test]
    fn an_absurd_length_is_refused_before_allocating() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());

        let error = read_frame_from::<_, RequestEnvelope>(Cursor::new(bytes)).unwrap_err();
        assert_eq!(error.code(), "service_protocol_error");
        assert!(error.message().contains("exceeds"));
    }

    #[test]
    fn a_zero_length_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u32.to_be_bytes());

        let error = read_frame_from::<_, RequestEnvelope>(Cursor::new(bytes)).unwrap_err();
        assert_eq!(error.code(), "service_protocol_error");
    }

    #[test]
    fn malformed_json_is_a_protocol_error() {
        let body = b"{ this is not json";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
        bytes.extend_from_slice(body);

        let error = read_frame_from::<_, RequestEnvelope>(Cursor::new(bytes)).unwrap_err();
        assert_eq!(error.code(), "service_protocol_error");
    }

    #[test]
    fn requests_report_their_method_and_session() {
        let ping = Request::Ping;
        assert_eq!(ping.method(), "ping");
        assert_eq!(ping.session_id(), None);

        let capture = Request::SessionCapture {
            session_id: "s-9".to_string(),
            output: ImageOptionsWire::default_png_base64(),
            presentation: None,
        };
        assert_eq!(capture.method(), "session_capture");
        assert_eq!(capture.session_id(), Some("s-9"));

        let create = Request::SessionCreate {
            display: ":99".to_string(),
            target: RequestTarget::Desktop,
            history: None,
        };
        assert_eq!(create.session_id(), None, "creation has no session yet");
    }

    #[test]
    fn target_conversion_matches_the_capture_backend() {
        assert_eq!(
            RequestTarget::Desktop.to_capture_request(),
            crate::capture::CaptureRequest::Desktop
        );

        let rect = crate::geometry::Rect::new(1, 2, 3, 4).unwrap();
        assert_eq!(
            RequestTarget::Region { region: rect }.to_capture_request(),
            crate::capture::CaptureRequest::Region(rect)
        );

        assert_eq!(
            RequestTarget::Window { id: 0x4600007 }.to_capture_request(),
            crate::capture::CaptureRequest::Window(0x4600007)
        );
    }

    #[test]
    fn image_options_reject_conflicting_resizes() {
        let mut options = ImageOptionsWire::default_png_base64();
        options.width = Some(100);
        options.height = Some(50);
        assert_eq!(
            options.to_image_options().unwrap_err().code(),
            "invalid_arguments"
        );

        options = ImageOptionsWire::default_png_base64();
        options.scale = Some(2.0);
        options.width = Some(10);
        assert!(options.to_image_options().is_err());
    }

    #[test]
    fn image_options_reject_nonsense_resizes() {
        let mut options = ImageOptionsWire::default_png_base64();
        options.width = Some(0);
        assert!(options.to_image_options().is_err());

        options = ImageOptionsWire::default_png_base64();
        options.scale = Some(0.0);
        assert!(options.to_image_options().is_err());

        options = ImageOptionsWire::default_png_base64();
        options.scale = Some(f64::NAN);
        assert!(options.to_image_options().is_err());
    }

    #[test]
    fn a_single_resize_option_is_accepted() {
        let mut options = ImageOptionsWire::default_png_base64();
        options.width = Some(960);
        let converted = options.to_image_options().unwrap();
        assert_eq!(converted.resize, crate::cli::ResizeRequest::Width(960));
    }

    #[test]
    fn temporal_options_round_trip() {
        let temporal = crate::observe::TemporalCompareOptions {
            compare: crate::compare::CompareOptions {
                mode: crate::compare::CompareMode::RgbThreshold,
                pixel_threshold: 12,
                area_threshold: 0.005,
            },
            interval: std::time::Duration::from_millis(100),
            timeout: std::time::Duration::from_secs(5),
        };

        let wire: TemporalWire = temporal.into();
        assert_eq!(wire.interval_ms, 100);
        assert_eq!(wire.timeout_ms, 5000);

        let back: crate::observe::TemporalCompareOptions = wire.into();
        assert_eq!(back, temporal);
    }

    #[test]
    fn temporal_wire_builds_the_right_option_types() {
        let wire = TemporalWire {
            compare: crate::compare::CompareOptions::default().into(),
            interval_ms: 50,
            timeout_ms: 1000,
        };

        assert_eq!(
            wire.to_wait_change().temporal.interval,
            std::time::Duration::from_millis(50)
        );
        assert_eq!(
            wire.to_wait_stable(300).stable_for,
            std::time::Duration::from_millis(300)
        );
        assert_eq!(
            wire.to_observe(400).stable_for,
            std::time::Duration::from_millis(400)
        );
    }

    #[test]
    fn compare_options_round_trip() {
        let options = crate::compare::CompareOptions {
            mode: crate::compare::CompareMode::RgbThreshold,
            pixel_threshold: 12,
            area_threshold: 0.005,
        };
        let wire: CompareOptionsWire = options.into();
        let back: crate::compare::CompareOptions = wire.into();
        assert_eq!(back, options);
    }

    #[test]
    fn a_response_round_trips_and_carries_either_a_result_or_an_error() {
        let response = ResponseEnvelope {
            request_id: "abc".to_string(),
            ok: false,
            result: None,
            error: Some(ErrorBody::from_error(&Error::session_not_found("s-1"))),
        };

        let mut buffer = Vec::new();
        write_frame(&mut buffer, &response).unwrap();

        let decoded: Option<ResponseEnvelope> = read_frame_from(Cursor::new(buffer)).unwrap();
        let decoded = decoded.unwrap();
        assert!(!decoded.ok);
        assert_eq!(decoded.error.unwrap().code, "session_not_found");
    }

    #[test]
    fn an_error_body_preserves_the_code() {
        let error = Error::session_busy("session s-1 is running an observation");
        let body = ErrorBody::from_error(&error);
        assert_eq!(body.code, "session_busy");
        assert!(body.message.contains("s-1"));
    }

    #[test]
    fn a_create_request_builds_a_session_config() {
        let config = session_config_from(
            ":99".to_string(),
            &RequestTarget::Region {
                region: crate::geometry::Rect::new(0, 0, 100, 50).unwrap(),
            },
            Some(3),
        )
        .unwrap();

        assert_eq!(config.target.display, ":99");
        assert_eq!(config.history_capacity, 3);
        assert_eq!(
            config.target.request,
            crate::capture::CaptureRequest::Region(
                crate::geometry::Rect::new(0, 0, 100, 50).unwrap()
            )
        );
    }

    #[test]
    fn a_create_request_without_a_history_uses_the_default() {
        let config = session_config_from(":99".to_string(), &RequestTarget::Desktop, None).unwrap();
        assert_eq!(config.history_capacity, crate::session::DEFAULT_CAPACITY);
    }

    #[test]
    fn request_json_uses_the_documented_tag() {
        let value = serde_json::to_value(Request::Ping).unwrap();
        assert_eq!(value["method"], "ping");

        let value = serde_json::to_value(RequestTarget::Window { id: 7 }).unwrap();
        assert_eq!(value["kind"], "window");
        assert_eq!(value["id"], 7);
    }
}
