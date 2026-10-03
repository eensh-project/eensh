//! A reusable, long-lived client for the persistent service.
//!
//! # Why this exists
//!
//! Phase 4's service is persistent, but the CLI is not: `eensh session capture`
//! starts a process, connects, sends one request, and exits. For an agent sampling a
//! game at 20 Hz, that process start is paid on every observation — and measurement
//! puts it at the largest single part of the round trip, considerably larger than the
//! X11 connection the session was built to save.
//!
//! This client holds one `UnixStream` open and sends many requests over it. It speaks
//! the protocol directly rather than shelling out, so a caller talks in terms of
//! sessions and observations rather than argument vectors and exit codes.
//!
//! ```no_run
//! # use eensh::client::{EenshClient, ImageSpec, SessionSpec};
//! # use eensh::realtime::RealtimeOptions;
//! # fn main() -> Result<(), eensh::Error> {
//! let mut client = EenshClient::connect_default()?;
//! let session = client.create_session(":99", &SessionSpec::desktop())?;
//!
//! let stack = client.realtime(
//!     session.id(),
//!     &RealtimeOptions::default(),
//!     ImageSpec::jpeg_overview(960, 75),
//! )?;
//! println!("{} frames, newest age {}us", stack.captured_frames(), stack.newest_frame_age_us());
//! # Ok(())
//! # }
//! ```
//!
//! # One outstanding request, enforced by the type
//!
//! Every method takes `&mut self`. That is not incidental: it makes "send a request,
//! wait for its response, send the next" the only thing this type can do, so a caller
//! cannot accidentally interleave two requests on one connection and then read one
//! response as the answer to the other. A caller that wants concurrency opens a
//! second client.
//!
//! Responses are matched to their request by the echoed request id, and a mismatch is
//! a protocol error rather than being handed back as the answer to the wrong question.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Error;
use crate::presentation::{ChangedRegionPolicy, ObservationPolicy};
use crate::realtime::{RealtimeOptions, RealtimeOutcome};
use crate::service::protocol::{
    self, Request, RequestEnvelope, RequestTarget, ResponseBody, ResponseEnvelope, PROTOCOL_VERSION,
};
use crate::service::unix;
use crate::session::pipeline::ServiceStatus;
use crate::session::realtime::RealtimeResponse;
use crate::session::{FrameId, SessionInfo};

/// A session as the client sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSession {
    id: String,
    display: String,
}

impl ClientSession {
    /// The session's identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The display it is bound to.
    pub fn display(&self) -> &str {
        &self.display
    }
}

/// A real-time stack, with the sampling metadata an agent needs.
///
/// A thin wrapper rather than the raw response, so the questions an agent actually
/// asks — "was it complete?", "how stale is the newest frame?" — are answered without
/// reaching into a nested shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientRealtime {
    /// The full response, including per-frame images when base64 was requested.
    pub response: RealtimeResponse,
    /// The Phase 6 multi-view presentation, when a policy was supplied.
    ///
    /// `None` means the Phase 5 shape, which is what a caller sending no policy gets.
    pub presentation: Option<crate::output::json::PresentationResponse>,
}

impl ClientRealtime {
    /// Whether the full requested stack was captured.
    pub fn is_complete(&self) -> bool {
        self.response.realtime.result == RealtimeOutcome::Complete
    }

    /// How many frames were captured.
    pub fn captured_frames(&self) -> usize {
        self.response.realtime.captured_frames
    }

    /// How many cadence slots were skipped because they had already passed.
    pub fn skipped_opportunities(&self) -> u64 {
        self.response.realtime.skipped_opportunities
    }

    /// The newest frame's age, in microseconds.
    pub fn newest_frame_age_us(&self) -> u64 {
        self.response.newest_frame_age_us
    }

    /// The newest frame's identity.
    pub fn newest_frame_id(&self) -> Option<FrameId> {
        self.response.newest_frame_id
    }

    /// The frames, oldest first.
    pub fn frames(&self) -> &[crate::session::realtime::RealtimeFrameResponse] {
        &self.response.frames
    }

    /// The Phase 5 response shape, for a caller that only samples.
    pub fn phase5(&self) -> &RealtimeResponse {
        &self.response
    }
}

/// A frame response, with its Phase 6 presentation when one was requested.
///
/// The presentation is optional rather than defaulted, because its absence is what
/// tells a caller it received the Phase 5 shape — a fact worth being able to check
/// rather than infer.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientFrame {
    /// The Phase 4 frame result.
    pub frame: crate::session::pipeline::SessionFrameResponse,
    /// The Phase 6 multi-view presentation, when a policy was supplied.
    pub presentation: Option<crate::output::json::PresentationResponse>,
}

impl ClientFrame {
    /// Unwrap the wire form, which boxes both fields.
    ///
    /// The boxes exist on the wire so that a large presentation does not inflate every
    /// response variant; the client immediately unboxes them, because a caller of a
    /// typed client should not have to think about protocol sizing.
    fn from_wire(
        frame: Box<crate::session::pipeline::SessionFrameResponse>,
        presentation: Option<Box<crate::output::json::PresentationResponse>>,
    ) -> Self {
        ClientFrame {
            frame: *frame,
            presentation: presentation.map(|boxed| *boxed),
        }
    }

    /// The frame's identity.
    pub fn frame_id(&self) -> FrameId {
        self.frame.frame_id
    }

    /// The first view with this name, searching every frame in the presentation.
    pub fn view(&self, name: &str) -> Option<&crate::output::json::ViewResponse> {
        let presentation = self.presentation.as_ref()?;
        presentation
            .frames
            .iter()
            .flat_map(|frame| frame.views.iter())
            .find(|view| view.name == name)
    }

    /// The whole-frame overview, when one was requested.
    pub fn overview(&self) -> Option<&crate::output::json::ViewResponse> {
        self.view("overview")
    }
}

/// A comparison response, with its changed-region view when one was requested.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientDiff {
    /// The Phase 2 comparison, unchanged.
    pub comparison: crate::compare::Comparison,
    /// The changed-region view, when the request asked for one.
    pub changed_view: Option<crate::output::json::ChangedRegionResponse>,
}

impl ClientDiff {
    /// Whether the comparison crossed the thresholds.
    pub fn changed(&self) -> bool {
        self.comparison.changed
    }

    /// The factual bounding box, whether or not the comparison counted as a change.
    ///
    /// The two are deliberately separate: a sub-threshold change leaves this set while
    /// `changed` stays false (requirement 11).
    pub fn bounding_box(&self) -> Option<crate::geometry::Rect> {
        self.comparison.bounding_box
    }
}

/// What a session should observe.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionSpec {
    /// The rectangle to observe, if the session is bound to a region.
    pub region: Option<crate::geometry::Rect>,
    /// The window to observe, if the session is bound to one.
    pub window: Option<u64>,
    /// How many recent frames to retain.
    pub history: Option<usize>,
}

impl SessionSpec {
    /// A whole-desktop session.
    pub fn desktop() -> Self {
        Self::default()
    }

    /// A session bound to a region.
    pub fn region(region: crate::geometry::Rect) -> Self {
        SessionSpec {
            region: Some(region),
            ..Self::default()
        }
    }

    /// A session bound to a window.
    pub fn window(id: u64) -> Self {
        SessionSpec {
            window: Some(id),
            ..Self::default()
        }
    }

    /// Retain this many recent frames.
    pub fn with_history(mut self, capacity: usize) -> Self {
        self.history = Some(capacity);
        self
    }

    /// A session bound to a region, for the common case.
    pub fn with_region(mut self, region: crate::geometry::Rect) -> Self {
        self.region = Some(region);
        self.window = None;
        self
    }

    fn to_target(&self) -> RequestTarget {
        if let Some(window) = self.window {
            RequestTarget::Window { id: window }
        } else if let Some(region) = self.region {
            RequestTarget::Region { region }
        } else {
            RequestTarget::Desktop
        }
    }
}

/// How to present a returned frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageSpec {
    /// Output format.
    pub format: crate::encode::ImageFormat,
    /// JPEG quality.
    pub quality: u8,
    /// PNG effort.
    pub png_effort: crate::encode::PngEffort,
    /// Target width, preserving the aspect ratio.
    pub width: Option<u32>,
    /// Target height, preserving the aspect ratio.
    pub height: Option<u32>,
    /// Uniform scale factor.
    pub scale: Option<f64>,
    /// Whether to embed the image inline as base64.
    pub base64: bool,
}

impl Default for ImageSpec {
    fn default() -> Self {
        ImageSpec {
            format: crate::encode::ImageFormat::Png,
            quality: crate::encode::jpeg::DEFAULT_QUALITY,
            png_effort: crate::encode::PngEffort::Default,
            width: None,
            height: None,
            scale: None,
            base64: true,
        }
    }
}

impl ImageSpec {
    /// A small inline JPEG, which is the shape an agent usually wants: small enough
    /// to send, large enough to see motion in.
    pub fn jpeg_overview(width: u32, quality: u8) -> Self {
        ImageSpec {
            format: crate::encode::ImageFormat::Jpeg,
            quality,
            width: Some(width),
            base64: true,
            ..Self::default()
        }
    }

    /// Metadata only: no inline image data.
    ///
    /// Lets an orchestrator see the frame identities and timings first, and decide
    /// afterwards which retained frames are worth retrieving.
    pub fn metadata_only() -> Self {
        ImageSpec {
            base64: false,
            ..Self::default()
        }
    }

    fn to_wire(self) -> Result<protocol::ImageOptionsWire, Error> {
        let wire = protocol::ImageOptionsWire {
            format: self.format,
            quality: self.quality,
            png_effort: self.png_effort,
            width: self.width,
            height: self.height,
            scale: self.scale,
            base64: self.base64,
        };

        // Validated locally, so a contradictory combination is refused without a
        // round trip.
        wire.to_image_options()?;
        Ok(wire)
    }
}

/// A long-lived client over one connection.
pub struct EenshClient {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    path: PathBuf,
    serial: AtomicU64,
}

impl std::fmt::Debug for EenshClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EenshClient")
            .field("socket", &self.path)
            .finish()
    }
}

impl EenshClient {
    /// Connect to a service at an explicit socket path.
    pub fn connect(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        let stream = unix::connect(&path)?;

        // The reader holds a duplicate of the same socket, so a request can be
        // written and its reply read without the stream having to serve both roles.
        let reader_stream = stream.try_clone().map_err(|e| {
            Error::service_unavailable(format!("could not duplicate the socket: {e}"))
        })?;

        Ok(EenshClient {
            stream,
            reader: BufReader::new(reader_stream),
            path,
            serial: AtomicU64::new(0),
        })
    }

    /// Connect to a service at the default resolved socket path.
    pub fn connect_default() -> Result<Self, Error> {
        EenshClient::connect(unix::socket_path())
    }

    /// The socket this client is connected to.
    pub fn socket(&self) -> &Path {
        &self.path
    }

    /// Whether the connection is still usable, as far as can be told without I/O.
    ///
    /// A closed peer is reported as readable-with-nothing-to-read, so this peeks at
    /// the socket without consuming a byte.
    pub fn is_connected(&self) -> bool {
        use std::os::fd::AsRawFd;

        let fd = self.stream.as_raw_fd();
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };

        let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if ready <= 0 {
            // Nothing pending, so it is either healthy or merely idle.
            return true;
        }

        let mut byte = 0u8;
        let peeked = unsafe {
            libc::recv(
                fd,
                &mut byte as *mut u8 as *mut libc::c_void,
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        peeked != 0
    }

    /// Send one request and read its response.
    ///
    /// `&mut self` is what enforces one outstanding request: the borrow checker will
    /// not let a second call begin before this one returns.
    fn call(&mut self, request: Request) -> Result<ResponseBody, Error> {
        let serial = self.serial.fetch_add(1, Ordering::Relaxed);
        let request_id = format!("client-{}", serial + 1);

        let envelope = RequestEnvelope {
            request_id: request_id.clone(),
            protocol_version: Some(PROTOCOL_VERSION),
            request,
        };

        protocol::write_frame(&mut self.stream, &envelope)?;

        let response: Option<ResponseEnvelope> = protocol::read_frame(&mut self.reader)?;

        let Some(response) = response else {
            return Err(Error::service_protocol_error(
                "the service closed the connection without responding",
            ));
        };

        // A response that answers a different request is not this request's answer.
        // Returning it would attach the wrong result to the wrong question, which is
        // worse than failing: an agent acting on a stale frame it believes is fresh
        // makes a wrong decision with no indication that anything went wrong.
        if response.request_id != request_id {
            return Err(Error::service_protocol_error(format!(
                "the service answered request {:?} while {} was outstanding; the \
                 connection is out of step",
                response.request_id, request_id
            )));
        }

        if !response.ok {
            let body = response.error.unwrap_or(protocol::ErrorBody {
                code: "service_protocol_error".to_string(),
                message: "the service reported a failure with no detail".to_string(),
            });
            return Err(Error::from_service_code(&body.code, &body.message));
        }

        response.result.ok_or_else(|| {
            Error::service_protocol_error("the service reported success with no result")
        })
    }

    /// Report service health.
    pub fn ping(&mut self) -> Result<ServiceStatus, Error> {
        match self.call(Request::Ping)? {
            ResponseBody::Status { status } => Ok(status),
            other => Err(unexpected("ping", &other)),
        }
    }

    /// Create a session.
    pub fn create_session(
        &mut self,
        display: &str,
        spec: &SessionSpec,
    ) -> Result<ClientSession, Error> {
        match self.call(Request::SessionCreate {
            display: display.to_string(),
            target: spec.to_target(),
            history: spec.history,
        })? {
            ResponseBody::SessionCreated { session_id, .. } => Ok(ClientSession {
                id: session_id,
                display: display.to_string(),
            }),
            other => Err(unexpected("create_session", &other)),
        }
    }

    /// List sessions.
    pub fn list_sessions(&mut self) -> Result<Vec<SessionInfo>, Error> {
        match self.call(Request::SessionList)? {
            ResponseBody::SessionList { sessions } => Ok(sessions),
            other => Err(unexpected("list_sessions", &other)),
        }
    }

    /// Describe a session.
    pub fn session_info(&mut self, session_id: &str) -> Result<SessionInfo, Error> {
        match self.call(Request::SessionInfo {
            session_id: session_id.to_string(),
        })? {
            ResponseBody::SessionInfo { info } => Ok(info),
            other => Err(unexpected("session_info", &other)),
        }
    }

    /// Capture a fresh frame.
    ///
    /// Unchanged since Phase 4, so every existing caller keeps working
    /// (requirement 70). Use [`EenshClient::capture_presented`] to attach a Phase 6
    /// policy.
    pub fn capture(
        &mut self,
        session_id: &str,
        image: ImageSpec,
    ) -> Result<crate::session::pipeline::SessionFrameResponse, Error> {
        self.capture_presented(session_id, image, None)
            .map(|presented| presented.frame)
    }

    /// Capture a fresh frame under a presentation policy.
    ///
    /// `presentation` is the Phase 6 policy. Passing `None` reproduces the Phase 5
    /// behaviour exactly: one whole-frame image, described by `image`.
    pub fn capture_presented(
        &mut self,
        session_id: &str,
        image: ImageSpec,
        presentation: Option<&ObservationPolicy>,
    ) -> Result<ClientFrame, Error> {
        match self.call(Request::SessionCapture {
            session_id: session_id.to_string(),
            output: image.to_wire()?,
            presentation: presentation.cloned().map(Box::new),
        })? {
            ResponseBody::Frame {
                frame,
                presentation,
            } => Ok(ClientFrame::from_wire(frame, presentation)),
            other => Err(unexpected("capture", &other)),
        }
    }

    /// Return the newest retained frame without capturing.
    pub fn latest(
        &mut self,
        session_id: &str,
        image: ImageSpec,
    ) -> Result<crate::session::pipeline::SessionFrameResponse, Error> {
        self.latest_presented(session_id, image, None)
            .map(|presented| presented.frame)
    }

    /// Return the newest retained frame, presented under a policy.
    pub fn latest_presented(
        &mut self,
        session_id: &str,
        image: ImageSpec,
        presentation: Option<&ObservationPolicy>,
    ) -> Result<ClientFrame, Error> {
        match self.call(Request::SessionLatest {
            session_id: session_id.to_string(),
            output: image.to_wire()?,
            presentation: presentation.cloned().map(Box::new),
        })? {
            ResponseBody::Frame {
                frame,
                presentation,
            } => Ok(ClientFrame::from_wire(frame, presentation)),
            other => Err(unexpected("latest", &other)),
        }
    }

    /// Return a specific retained frame without capturing.
    pub fn frame(
        &mut self,
        session_id: &str,
        frame_id: FrameId,
        image: ImageSpec,
    ) -> Result<crate::session::pipeline::SessionFrameResponse, Error> {
        self.frame_presented(session_id, frame_id, image, None)
            .map(|presented| presented.frame)
    }

    /// Return a specific retained frame, presented under a policy.
    ///
    /// This is the re-presentation path: any retained frame can be rendered again
    /// under a different policy, and no new frame identity is allocated
    /// (requirement 42).
    pub fn frame_presented(
        &mut self,
        session_id: &str,
        frame_id: FrameId,
        image: ImageSpec,
        presentation: Option<&ObservationPolicy>,
    ) -> Result<ClientFrame, Error> {
        match self.call(Request::SessionFrame {
            session_id: session_id.to_string(),
            frame_id,
            output: image.to_wire()?,
            presentation: presentation.cloned().map(Box::new),
        })? {
            ResponseBody::Frame {
                frame,
                presentation,
            } => Ok(ClientFrame::from_wire(frame, presentation)),
            other => Err(unexpected("frame", &other)),
        }
    }

    /// Compare two retained frames.
    pub fn diff(
        &mut self,
        session_id: &str,
        before: FrameId,
        after: FrameId,
        options: &crate::compare::CompareOptions,
    ) -> Result<crate::compare::Comparison, Error> {
        self.diff_with_changed_region(session_id, before, after, options, None)
            .map(|diff| diff.comparison)
    }

    /// Compare two retained frames and optionally crop the changed region.
    pub fn diff_with_changed_region(
        &mut self,
        session_id: &str,
        before: FrameId,
        after: FrameId,
        options: &crate::compare::CompareOptions,
        changed: Option<&ChangedRegionPolicy>,
    ) -> Result<ClientDiff, Error> {
        match self.call(Request::SessionDiff {
            session_id: session_id.to_string(),
            before,
            after,
            compare: (*options).into(),
            changed: changed.copied(),
        })? {
            ResponseBody::Diff { diff, changed_view } => Ok(ClientDiff {
                comparison: diff.comparison,
                changed_view,
            }),
            other => Err(unexpected("diff", &other)),
        }
    }

    /// Capture a bounded real-time temporal stack.
    ///
    /// The operation Phase 5 exists for: several fresh frames across a short
    /// interval, returned oldest first with their timing, for an agent that needs to
    /// see motion rather than a single still.
    pub fn realtime(
        &mut self,
        session_id: &str,
        options: &RealtimeOptions,
        image: ImageSpec,
    ) -> Result<ClientRealtime, Error> {
        self.realtime_presented(session_id, options, image, None)
    }

    /// Capture a bounded real-time stack and present it under a policy.
    ///
    /// A [`TemporalFramePolicy`](crate::presentation::TemporalFramePolicy) is usually
    /// what a caller wants here, so that the older frames cost less than the newest.
    pub fn realtime_presented(
        &mut self,
        session_id: &str,
        options: &RealtimeOptions,
        image: ImageSpec,
        presentation: Option<&ObservationPolicy>,
    ) -> Result<ClientRealtime, Error> {
        match self.call(Request::SessionRealtime {
            session_id: session_id.to_string(),
            realtime: (*options).into(),
            output: image.to_wire()?,
            presentation: presentation.cloned().map(Box::new),
        })? {
            ResponseBody::Realtime {
                realtime,
                presentation,
            } => Ok(ClientRealtime {
                response: *realtime,
                presentation: presentation.map(|boxed| *boxed),
            }),
            other => Err(unexpected("realtime", &other)),
        }
    }

    /// Run a `wait-change` observation inside a session.
    pub fn wait_change(
        &mut self,
        session_id: &str,
        options: &crate::observe::WaitChangeOptions,
        image: ImageSpec,
    ) -> Result<crate::output::json::ObservationResponse, Error> {
        self.observe(
            session_id,
            protocol::ObservationKindWire::WaitChange,
            protocol::TemporalWire::from(options.temporal),
            0,
            image,
        )
    }

    /// Run a `wait-stable` observation inside a session.
    pub fn wait_stable(
        &mut self,
        session_id: &str,
        options: &crate::observe::WaitStableOptions,
        image: ImageSpec,
    ) -> Result<crate::output::json::ObservationResponse, Error> {
        self.observe(
            session_id,
            protocol::ObservationKindWire::WaitStable,
            protocol::TemporalWire::from(options.temporal),
            options.stable_for.as_millis() as u64,
            image,
        )
    }

    /// Observe a transition and then its settling.
    pub fn observe(
        &mut self,
        session_id: &str,
        kind: protocol::ObservationKindWire,
        temporal: protocol::TemporalWire,
        stable_for_ms: u64,
        image: ImageSpec,
    ) -> Result<crate::output::json::ObservationResponse, Error> {
        match self.call(Request::SessionObserve {
            session_id: session_id.to_string(),
            kind,
            temporal,
            stable_for_ms,
            output: image.to_wire()?,
            presentation: None,
        })? {
            ResponseBody::Observation { observation, .. } => Ok(*observation),
            other => Err(unexpected("observe", &other)),
        }
    }

    /// Close a session.
    pub fn close_session(&mut self, session_id: &str) -> Result<(), Error> {
        match self.call(Request::SessionClose {
            session_id: session_id.to_string(),
        })? {
            ResponseBody::SessionClosed { .. } => Ok(()),
            other => Err(unexpected("close_session", &other)),
        }
    }
}

/// Report a response of the wrong kind.
///
/// A wrong-shaped response is a protocol error, not a result to be reinterpreted, so
/// it is named rather than coerced.
fn unexpected(operation: &str, body: &ResponseBody) -> Error {
    let kind = match body {
        ResponseBody::Status { .. } => "status",
        ResponseBody::SessionCreated { .. } => "session_created",
        ResponseBody::SessionList { .. } => "session_list",
        ResponseBody::SessionInfo { .. } => "session_info",
        ResponseBody::SessionClosed { .. } => "session_closed",
        ResponseBody::Frame { .. } => "frame",
        ResponseBody::Diff { .. } => "diff",
        ResponseBody::Observation { .. } => "observation",
        ResponseBody::Realtime { .. } => "realtime",
    };
    Error::service_protocol_error(format!(
        "{operation} received a {kind} response, which is the wrong kind for that request"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    #[test]
    fn a_session_spec_maps_to_the_right_wire_target() {
        assert_eq!(SessionSpec::desktop().to_target(), RequestTarget::Desktop);
        assert_eq!(
            SessionSpec::region(Rect::new(1, 2, 3, 4).unwrap()).to_target(),
            RequestTarget::Region {
                region: Rect::new(1, 2, 3, 4).unwrap()
            }
        );
        assert_eq!(
            SessionSpec::window(0x10).to_target(),
            RequestTarget::Window { id: 0x10 }
        );
    }

    #[test]
    fn a_window_takes_precedence_over_a_region_in_a_spec() {
        // The CLI makes these mutually exclusive; the library applies the same rule
        // rather than picking one at random.
        let spec = SessionSpec {
            window: Some(0x10),
            region: Some(Rect::new(0, 0, 1, 1).unwrap()),
            history: None,
        };
        assert_eq!(spec.to_target(), RequestTarget::Window { id: 0x10 });
    }

    #[test]
    fn binding_a_region_clears_a_previously_set_window() {
        let spec = SessionSpec::window(0x10).with_region(Rect::new(0, 0, 5, 5).unwrap());
        assert_eq!(
            spec.to_target(),
            RequestTarget::Region {
                region: Rect::new(0, 0, 5, 5).unwrap()
            },
            "a region binding must not be silently overridden by an earlier window"
        );
    }

    #[test]
    fn a_spec_can_carry_a_history_capacity() {
        let spec = SessionSpec::desktop().with_history(16);
        assert_eq!(spec.history, Some(16));
    }

    #[test]
    fn a_jpeg_overview_is_small_inline_jpeg() {
        let image = ImageSpec::jpeg_overview(960, 75);
        assert_eq!(image.format, crate::encode::ImageFormat::Jpeg);
        assert_eq!(image.quality, 75);
        assert_eq!(image.width, Some(960));
        assert!(image.base64);
        assert!(
            image.height.is_none(),
            "aspect ratio is preserved by width alone"
        );
    }

    #[test]
    fn a_metadata_only_spec_requests_no_inline_data() {
        let image = ImageSpec::metadata_only();
        assert!(!image.base64);
        let wire = image.to_wire().unwrap();
        assert!(!wire.base64);
    }

    #[test]
    fn an_image_spec_with_conflicting_resizes_is_rejected_locally() {
        let image = ImageSpec {
            width: Some(100),
            height: Some(50),
            ..ImageSpec::default()
        };

        let error = image.to_wire().unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
    }

    #[test]
    fn a_zero_scale_is_rejected_locally() {
        let image = ImageSpec {
            scale: Some(0.0),
            ..ImageSpec::default()
        };
        assert_eq!(image.to_wire().unwrap_err().code(), "invalid_arguments");
    }

    #[test]
    fn a_wrong_response_kind_is_a_protocol_error() {
        let status = ResponseBody::Status {
            status: ServiceStatus {
                protocol_version: 1,
                version: "0.1.0".to_string(),
                sessions: 0,
                uptime_ms: 0,
            },
        };
        let error = unexpected("realtime", &status);
        assert_eq!(error.code(), "service_protocol_error");
        assert!(
            error.message().contains("status"),
            "the message should name what arrived: {}",
            error.message()
        );
        assert!(error.message().contains("realtime"));
    }

    #[test]
    fn every_response_kind_is_named() {
        // A new variant must be added here, so the exhaustive match in `unexpected`
        // cannot silently fall through to a wrong description.
        let bodies = [
            ResponseBody::SessionCreated {
                session_id: "s".to_string(),
                info: crate::session::SessionInfo {
                    session_id: "s".to_string(),
                    state: crate::session::SessionState::Ready,
                    display: ":0".to_string(),
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
                },
            },
            ResponseBody::SessionClosed {
                session_id: "s".to_string(),
            },
        ];

        for body in &bodies {
            let message = unexpected("op", body).message();
            assert!(
                message.contains("session_"),
                "each kind should be described, got: {message}"
            );
        }
    }

    #[test]
    fn a_client_session_exposes_its_identity() {
        let session = ClientSession {
            id: "s-1".to_string(),
            display: ":99".to_string(),
        };
        assert_eq!(session.id(), "s-1");
        assert_eq!(session.display(), ":99");
    }
}
