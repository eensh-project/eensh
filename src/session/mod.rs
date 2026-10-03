//! Persistent capture sessions.
//!
//! A [`CaptureSession`] is one long-lived capture context: an X11 connection that
//! stays open, a resolved target, a frame-identifier sequence, and a bounded
//! history of recent raw frames.
//!
//! # What this buys
//!
//! Phase 3 opened and closed a display for every sample. That handshake dominates
//! sampling latency on some X servers — measured at 2–42 ms per sample on the
//! Xvfb build used in development, against comparison at well under a
//! millisecond. A session opens the connection once:
//!
//! ```text
//! Phase 3:  open, capture, close, open, capture, close, ...
//! Phase 4:  open, capture, capture, capture, capture, close
//! ```
//!
//! # What this deliberately does not change
//!
//! Nothing about *meaning*. The session supplies frames to the same
//! [`crate::compare`] and [`crate::observe`] machinery Phase 2 and 3 use. Thresholds,
//! timing rules, timeout semantics, geometry rules, and observation outcomes are
//! untouched, and an equivalence test runs the same scripted scenarios through
//! both the non-persistent and persistent frame sources to prove it.
//!
//! # Lifecycle
//!
//! A session is a process-lifetime object. Its identifier means nothing after the
//! service restarts, and there is no reconnect: a dropped X11 connection fails the
//! session explicitly rather than transparently reopening, because a reconnected
//! display could have different geometry, a different target, and a broken frame
//! sequence.

pub mod history;
pub mod manager;
pub mod pipeline;
pub mod realtime;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::capture::{x11, CaptureRequest, Display};
use crate::error::Error;
use crate::frame::Frame;
use crate::geometry::{Rect, SourceGeometry};

pub use history::{FrameHistory, FrameId, HistorySummary, SessionFrame, DEFAULT_CAPACITY};

/// Where a session's frames come from.
///
/// `CaptureRequest` is not serializable itself, because a window identifier and a
/// rectangle cannot share one tagged representation. Its [`TargetDescription`] is
/// what appears in output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSpec {
    /// The X11 display to observe.
    pub display: String,
    /// What to capture on that display.
    pub request: CaptureRequest,
}

impl TargetSpec {
    /// A desktop capture target.
    pub fn desktop(display: impl Into<String>) -> Self {
        TargetSpec {
            display: display.into(),
            request: CaptureRequest::Desktop,
        }
    }

    /// A region capture target.
    pub fn region(display: impl Into<String>, rect: Rect) -> Self {
        TargetSpec {
            display: display.into(),
            request: CaptureRequest::Region(rect),
        }
    }

    /// A window capture target.
    pub fn window(display: impl Into<String>, window_id: u64) -> Self {
        TargetSpec {
            display: display.into(),
            request: CaptureRequest::Window(window_id),
        }
    }

    /// How the target should be described in output.
    pub fn kind(&self) -> &'static str {
        match self.request {
            CaptureRequest::Desktop => "desktop",
            CaptureRequest::Region(_) => "region",
            CaptureRequest::Window(_) => "window",
        }
    }

    /// A serializable description of this target.
    pub fn describe(&self) -> TargetDescription {
        TargetDescription::from_spec(self)
    }
}

/// Whether a session is usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Accepting operations.
    Ready,
    /// An observation is running; captures and further observations are refused.
    Observing,
    /// Closed, or failed and therefore unusable.
    Closed,
    /// The X11 connection or the target was lost.
    Failed,
}

impl SessionState {
    /// Canonical name for output.
    pub fn name(self) -> &'static str {
        match self {
            SessionState::Ready => "ready",
            SessionState::Observing => "observing",
            SessionState::Closed => "closed",
            SessionState::Failed => "failed",
        }
    }
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Configuration for a new session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    /// The display and target to capture.
    pub target: TargetSpec,
    /// How many recent frames to retain.
    pub history_capacity: usize,
}

impl SessionConfig {
    /// A configuration with the default history capacity.
    pub fn new(target: TargetSpec) -> Self {
        SessionConfig {
            target,
            history_capacity: DEFAULT_CAPACITY,
        }
    }

    /// Override the history capacity.
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.history_capacity = capacity;
        self
    }
}

/// A long-lived capture context.
///
/// Owns its X11 connection, target, frame sequence, and history. It does **not**
/// own global state, and it does not control input or launch anything.
pub struct CaptureSession {
    session_id: String,
    config: SessionConfig,
    /// The open connection. `None` once the session has been closed or failed.
    display: Option<Display>,
    /// Geometry resolved from the first capture, used for consistency checks.
    geometry: Option<SourceGeometry>,
    history: FrameHistory,
    state: SessionState,
    /// Why the session failed, if it did.
    failure: Option<String>,
    created_at: Instant,
}

impl CaptureSession {
    /// Create a session, opening the display and validating the target.
    ///
    /// Creation resolves the target geometry immediately, so that a bad region or
    /// a missing window is reported at creation rather than on first use. It does
    /// **not** capture a frame, so the first capture receives frame `1`.
    pub fn create(session_id: impl Into<String>, config: SessionConfig) -> Result<Self, Error> {
        let history = FrameHistory::new(config.history_capacity)?;

        let display = Display::open(&config.target.display)?;

        // Resolve the target now, so creation fails fast on a bad target.
        let geometry = resolve_geometry(&display, &config.target.request)?;

        Ok(CaptureSession {
            session_id: session_id.into(),
            config,
            display: Some(display),
            geometry: Some(geometry),
            history,
            state: SessionState::Ready,
            failure: None,
            created_at: Instant::now(),
        })
    }

    /// The session's identifier.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The configuration this session was created with.
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// The current state.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// A session with no display, for exercising the state machine.
    ///
    /// The state machine — which operations are refused, and in which states — is
    /// the part of a session that can be wrong in a way a live test would not
    /// catch, because a live test needs a display and therefore tends to test one
    /// happy path. This constructor exists so those transitions can be tested
    /// directly, with no display and no X11 at all.
    #[cfg(test)]
    pub(crate) fn without_display(session_id: impl Into<String>, capacity: usize) -> Self {
        CaptureSession {
            session_id: session_id.into(),
            config: SessionConfig::new(TargetSpec::desktop(":0")).with_capacity(capacity),
            display: None,
            geometry: None,
            history: FrameHistory::new(capacity).expect("test capacity should be valid"),
            state: SessionState::Ready,
            failure: None,
            created_at: Instant::now(),
        }
    }

    /// The resolved target geometry, once known.
    pub fn geometry(&self) -> Option<&SourceGeometry> {
        self.geometry.as_ref()
    }

    /// The history, for read-only inspection.
    pub fn history(&self) -> &FrameHistory {
        &self.history
    }

    /// How long ago the session was created.
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }

    /// Why the session failed, if it did.
    pub fn failure_reason(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Refuse the operation unless the session can still do useful work.
    ///
    /// A running observation does **not** make the session unusable. Captures are
    /// *serialized* by the per-session mutex, not refused, so a capture that
    /// arrives while an observation is running simply waits its turn. Refusing it
    /// would be wrong in a way that is easy to miss: an observation performs its
    /// own captures, so refusing capture during an observation makes the first
    /// observation of a fresh session fail with `session_busy`.
    ///
    /// Only a closed or failed session is genuinely unusable.
    fn ensure_usable(&self, operation: &str) -> Result<(), Error> {
        match self.state {
            SessionState::Ready | SessionState::Observing => Ok(()),
            SessionState::Closed => Err(Error::session_closed(format!(
                "session {} is closed; {operation} was refused",
                self.session_id
            ))),
            SessionState::Failed => Err(Error::target_lost(format!(
                "session {} failed and cannot be reused: {}",
                self.session_id,
                self.failure.as_deref().unwrap_or("the target was lost")
            ))),
        }
    }

    /// Capture a new frame from the target.
    ///
    /// This is the "capture now" operation, and it is deliberately distinct from
    /// [`CaptureSession::latest`], which returns the newest frame already
    /// retained without touching X11. An agent that wants freshness asks for a
    /// capture; an agent that wants to re-inspect what it already has asks for
    /// the latest frame. Conflating them would make freshness unpredictable.
    pub fn capture(&mut self) -> Result<SessionFrame, Error> {
        self.ensure_usable("capture")?;

        let display = self
            .display
            .as_ref()
            .ok_or_else(|| Error::session_closed("the display connection is gone"))?;

        let started = Instant::now();
        let frame = match &self.config.target.request {
            CaptureRequest::Desktop => x11::capture_desktop(display),
            CaptureRequest::Region(rect) => x11::capture_region(display, rect),
            CaptureRequest::Window(id) => x11::capture_window(display, *id),
        };

        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                // A window that the session resolved at creation and has now lost
                // is `target_lost`, not `window_not_found`. The distinction is
                // available here precisely because a persistent session has the
                // lifecycle context: the window existed when the session was
                // created, so its disappearance is a change in the world rather
                // than a bad request. Creation-time resolution is what makes this
                // knowable -- a window that never existed never reaches here.
                //
                // No attempt is made to find another window with the same title:
                // silently following a replacement window would break the
                // geometry and frame-continuity assumptions the session relies on.
                let error = match (&self.config.target.request, error) {
                    (CaptureRequest::Window(id), Error::WindowNotFound { .. }) => {
                        Error::target_lost(format!(
                            "the window target 0x{id:x} no longer exists; it was present when \
                             the session was created, and the session will not follow a \
                             replacement. Create a new session for the new window."
                        ))
                    }
                    (_, error) => error,
                };

                self.record_failure(&error);
                return Err(error);
            }
        };
        let capture_duration = started.elapsed();

        // A capture that returns different geometry is a hard error, matching
        // Phase 3's rule. History would otherwise hold frames that cannot be
        // compared, and `changed_fraction` would silently stop meaning anything.
        if let Some(baseline) = &self.geometry {
            check_geometry_consistent(baseline, &frame)?;
        } else {
            self.geometry = Some(frame.source_geometry.clone());
        }

        // Allocate only once the capture has succeeded, so a failed capture does
        // not consume a frame identifier.
        let frame_id = self.history.allocate_id()?;
        let captured_at = Instant::now();

        let session_frame = SessionFrame {
            session_id: self.session_id.clone(),
            frame_id,
            frame: Arc::new(frame),
            captured_at,
            capture_duration,
        };

        self.history.insert(session_frame.clone())?;
        Ok(session_frame)
    }

    /// The newest frame already retained, without capturing.
    ///
    /// Returns [`Error::NoFrameAvailable`] when nothing has been captured yet.
    pub fn latest(&self) -> Result<SessionFrame, Error> {
        self.history.latest().cloned().ok_or_else(|| {
            Error::no_frame_available(format!(
                "session {} has not captured a frame yet",
                self.session_id
            ))
        })
    }

    /// A retained frame by identifier, without capturing.
    pub fn frame(&self, frame_id: FrameId) -> Result<SessionFrame, Error> {
        self.history.get(frame_id)
    }

    /// Compare two retained frames.
    ///
    /// This calls the Phase 2 [`crate::compare::compare_frames`] on the retained
    /// raw frames. There is no second comparison implementation.
    pub fn compare(
        &self,
        before: FrameId,
        after: FrameId,
        options: &crate::compare::CompareOptions,
    ) -> Result<crate::compare::Comparison, Error> {
        let before = self.frame(before)?;
        let after = self.frame(after)?;

        if before.session_id != after.session_id {
            return Err(Error::incompatible_frames(format!(
                "frames belong to different sessions: {} and {}",
                before.session_id, after.session_id
            )));
        }

        crate::compare::compare_frames(&before.frame, &after.frame, options)
    }

    /// Record that the session is running an observation.
    ///
    /// Only one observation may run per session, and the service relies on this
    /// flag to refuse a second one with an explicit [`Error::SessionBusy`] rather
    /// than queueing it invisibly.
    ///
    /// This is also where `close` is refused while an observation is running. The
    /// alternative policy — cancelling the observation — would have to interrupt a
    /// running state machine, and Phase 4 chooses the explicit refusal: the caller
    /// is told to wait rather than being silently blocked or silently losing the
    /// result it asked for. See the concurrency section of the Phase 4 report.
    pub fn begin_observation(&mut self) -> Result<(), Error> {
        self.ensure_usable("observe")?;

        if self.state == SessionState::Observing {
            return Err(Error::session_busy(format!(
                "session {} is already running an observation; a second one was refused",
                self.session_id
            )));
        }

        self.state = SessionState::Observing;
        Ok(())
    }

    /// Record that the observation finished, successfully or not.
    ///
    /// Idempotent, so the `Drop` on a frame source that never took the slot is
    /// harmless. A session that was closed or failed while observing keeps that
    /// state: an observation ending must not resurrect a session that has been
    /// torn down.
    pub fn end_observation(&mut self) {
        if self.state == SessionState::Observing {
            self.state = SessionState::Ready;
        }
    }

    /// Close the session, releasing the connection and retained frames.
    ///
    /// Closing is idempotent: closing an already-closed session succeeds, so a
    /// caller can treat it as a cleanup step without checking first. Closing a
    /// session that never existed is still [`Error::SessionNotFound`] — that is
    /// decided by the manager, which can tell the difference.
    ///
    /// Closing while an observation is running is refused with
    /// [`Error::SessionBusy`], which is the documented policy for active-session
    /// closure. It is checked before anything is torn down, so a refused close
    /// leaves the session exactly as it was.
    pub fn close(&mut self) -> Result<(), Error> {
        if self.state == SessionState::Observing {
            return Err(Error::session_busy(format!(
                "session {} is running an observation; close was refused, wait for the \
                 observation to finish and try again",
                self.session_id
            )));
        }

        self.display = None;
        self.state = SessionState::Closed;
        self.geometry = None;
        Ok(())
    }

    /// Close unconditionally, ignoring the observation refusal.
    ///
    /// Used only by service shutdown, which must not be blocked by an observation
    /// that is still running. This is not a second policy: it is the same close
    /// without the courtesy check, applied at the one moment where a refusal has
    /// no meaning because the process is going away.
    pub fn force_close(&mut self) {
        self.display = None;
        self.state = SessionState::Closed;
        self.geometry = None;
    }

    /// Record a failure and mark the session unusable.
    fn record_failure(&mut self, error: &Error) {
        if error.is_session_fatal() {
            self.failure = Some(error.message());
            self.state = SessionState::Failed;
        }
    }

    /// Mark the session failed, for callers that detected a fatal condition.
    pub fn fail(&mut self, reason: impl Into<String>) {
        self.failure = Some(reason.into());
        self.state = SessionState::Failed;
    }

    /// A factual description of the session for `session info`.
    pub fn info(&self) -> SessionInfo {
        let history = self.history.summary();
        SessionInfo {
            session_id: self.session_id.clone(),
            state: self.state,
            display: self.config.target.display.clone(),
            target: TargetDescription::from_spec(&self.config.target),
            geometry: self.geometry.clone(),
            frames_captured: history.captured_total,
            history,
            age_ms: self.age().as_millis() as u64,
            failure: self.failure.clone(),
        }
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // Dropping the `Display` closes the X11 connection. History is dropped
        // with the struct, releasing every retained raw frame.
        self.display = None;
    }
}

impl std::fmt::Debug for CaptureSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureSession")
            .field("session_id", &self.session_id)
            .field("state", &self.state)
            .field("display", &self.config.target.display)
            .field("frames", &self.history.captured_total())
            .finish()
    }
}

/// A compact description of a session's target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetDescription {
    /// `desktop`, `region`, or `window`.
    pub kind: String,
    /// Window identifier, for window targets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Requested region, for region targets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<Rect>,
}

impl TargetDescription {
    /// Describe a target specification.
    pub fn from_spec(spec: &TargetSpec) -> Self {
        match &spec.request {
            CaptureRequest::Desktop => TargetDescription {
                kind: "desktop".to_string(),
                id: None,
                region: None,
            },
            CaptureRequest::Region(rect) => TargetDescription {
                kind: "region".to_string(),
                id: None,
                region: Some(*rect),
            },
            CaptureRequest::Window(id) => TargetDescription {
                kind: "window".to_string(),
                id: Some(format!("0x{id:x}")),
                region: None,
            },
        }
    }
}

/// Session status, as reported by `session info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The session identifier.
    pub session_id: String,
    /// Current state.
    pub state: SessionState,
    /// The display in use.
    pub display: String,
    /// What the session observes.
    pub target: TargetDescription,
    /// Resolved target geometry, once known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geometry: Option<SourceGeometry>,
    /// Total frames captured over the session's life.
    pub frames_captured: u64,
    /// History occupancy and byte cost.
    pub history: HistorySummary,
    /// How long the session has existed.
    pub age_ms: u64,
    /// Why the session failed, if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// Resolve a target's geometry without capturing a full frame.
fn resolve_geometry(display: &Display, request: &CaptureRequest) -> Result<SourceGeometry, Error> {
    match request {
        CaptureRequest::Desktop => {
            let root = display.root_window()?;
            let bounds = display.geometry_of(root)?;
            Ok(SourceGeometry::desktop(
                Some(display.name().to_string()),
                bounds.width,
                bounds.height,
            ))
        }
        CaptureRequest::Region(rect) => {
            let root = display.root_window()?;
            let bounds = display.geometry_of(root)?;
            rect.ensure_within(&bounds)?;
            Ok(SourceGeometry::region(
                Some(display.name().to_string()),
                rect.x,
                rect.y,
                rect.width,
                rect.height,
            ))
        }
        CaptureRequest::Window(id) => {
            // Window geometry is resolved by reading it; this also validates that
            // the window exists and is viewable, so creation fails fast.
            let root = display.root_window()?;
            let bounds = display.geometry_of(root)?;
            let attributes = display.window_attributes(*id)?;
            let (abs_x, abs_y) = display.translate_coordinates(*id, root, 0, 0)?;
            let window_rect = Rect::new(
                abs_x,
                abs_y,
                attributes.width.max(1) as u32,
                attributes.height.max(1) as u32,
            )?;

            // A window extending past the screen is clipped, as in Phase 1.
            let left = window_rect.x.max(bounds.x) as i64;
            let top = window_rect.y.max(bounds.y) as i64;
            let right = window_rect.right().min(bounds.right());
            let bottom = window_rect.bottom().min(bounds.bottom());
            if right <= left || bottom <= top {
                return Err(Error::capture_failed(format!(
                    "window 0x{id:x} lies entirely outside the display"
                )));
            }

            Ok(SourceGeometry::window(
                Some(display.name().to_string()),
                format!("0x{id:x}"),
                left as i32,
                top as i32,
                (right - left) as u32,
                (bottom - top) as u32,
            ))
        }
    }
}

/// Reject a capture whose geometry drifted from the session baseline.
fn check_geometry_consistent(baseline: &SourceGeometry, frame: &Frame) -> Result<(), Error> {
    if baseline.width != frame.width()
        || baseline.height != frame.height()
        || baseline.x != frame.source_geometry.x
        || baseline.y != frame.source_geometry.y
    {
        return Err(Error::geometry_changed(format!(
            "session target changed from {}x{} at ({},{}) to {}x{} at ({},{})",
            baseline.width,
            baseline.height,
            baseline.x,
            baseline.y,
            frame.width(),
            frame.height(),
            frame.source_geometry.x,
            frame.source_geometry.y,
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_spec_describes_itself() {
        assert_eq!(TargetSpec::desktop(":99").kind(), "desktop");
        assert_eq!(
            TargetSpec::region(":99", Rect::new(0, 0, 10, 10).unwrap()).kind(),
            "region"
        );
        assert_eq!(TargetSpec::window(":99", 0x10).kind(), "window");
    }

    #[test]
    fn target_descriptions_carry_the_right_detail() {
        let desktop = TargetDescription::from_spec(&TargetSpec::desktop(":99"));
        assert_eq!(desktop.kind, "desktop");
        assert!(desktop.id.is_none());
        assert!(desktop.region.is_none());

        let region = TargetDescription::from_spec(&TargetSpec::region(
            ":99",
            Rect::new(1, 2, 3, 4).unwrap(),
        ));
        assert_eq!(region.kind, "region");
        assert_eq!(region.region, Some(Rect::new(1, 2, 3, 4).unwrap()));

        let window = TargetDescription::from_spec(&TargetSpec::window(":99", 0x4600007));
        assert_eq!(window.kind, "window");
        assert_eq!(window.id.as_deref(), Some("0x4600007"));
    }

    #[test]
    fn session_config_defaults_to_the_documented_capacity() {
        let config = SessionConfig::new(TargetSpec::desktop(":99"));
        assert_eq!(config.history_capacity, DEFAULT_CAPACITY);
        assert_eq!(
            config.with_capacity(3).history_capacity,
            3,
            "capacity should be overridable"
        );
    }

    #[test]
    fn an_invalid_capacity_fails_before_any_display_is_opened() {
        // Capacity is validated ahead of the connection attempt, so this needs no
        // display.
        let config = SessionConfig::new(TargetSpec::desktop(":54321")).with_capacity(0);
        let error = CaptureSession::create("s", config).unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
    }

    #[test]
    fn creating_a_session_on_a_missing_display_fails_cleanly() {
        let config = SessionConfig::new(TargetSpec::desktop(":54321"));
        let error = CaptureSession::create("s", config).unwrap_err();
        assert_eq!(error.code(), "display_unavailable");
    }

    #[test]
    fn session_state_names_are_stable() {
        assert_eq!(SessionState::Ready.name(), "ready");
        assert_eq!(SessionState::Observing.name(), "observing");
        assert_eq!(SessionState::Closed.name(), "closed");
        assert_eq!(SessionState::Failed.name(), "failed");
        assert_eq!(
            serde_json::to_value(SessionState::Ready).unwrap(),
            serde_json::json!("ready")
        );
    }

    #[test]
    fn an_observation_may_begin_from_a_ready_session() {
        let mut session = CaptureSession::without_display("s", 4);
        assert_eq!(session.state(), SessionState::Ready);

        session.begin_observation().unwrap();
        assert_eq!(session.state(), SessionState::Observing);

        session.end_observation();
        assert_eq!(session.state(), SessionState::Ready);
    }

    #[test]
    fn a_second_observation_is_refused_while_one_is_running() {
        // The regression this guards: exclusivity used to be applied by refusing
        // *capture*, which meant an observation's own first capture was rejected
        // and every observation of a fresh session failed with `session_busy`.
        let mut session = CaptureSession::without_display("s", 4);

        session.begin_observation().unwrap();
        let error = session.begin_observation().unwrap_err();

        assert_eq!(error.code(), "session_busy");
        assert!(
            error.message().contains("already running an observation"),
            "the message should name the real reason, was: {}",
            error.message()
        );
    }

    #[test]
    fn capture_is_permitted_while_an_observation_is_running() {
        // An observation captures its own frames, so capture must not be refused
        // merely because an observation is active. Serialization is the mutex's
        // job, not this check's.
        let mut session = CaptureSession::without_display("s", 4);
        session.begin_observation().unwrap();

        // The display is absent, so capture fails for that reason -- but it must
        // *not* fail with `session_busy`.
        let error = session.capture().unwrap_err();
        assert_ne!(
            error.code(),
            "session_busy",
            "capture must not be refused during an observation, got: {}",
            error.message()
        );
    }

    #[test]
    fn capture_is_refused_once_the_session_is_closed() {
        let mut session = CaptureSession::without_display("s", 4);
        session.close().unwrap();

        let error = session.capture().unwrap_err();
        assert_eq!(error.code(), "session_closed");
    }

    #[test]
    fn closing_while_observing_is_refused_and_leaves_the_session_intact() {
        let mut session = CaptureSession::without_display("s", 4);
        session.begin_observation().unwrap();

        let error = session.close().unwrap_err();
        assert_eq!(error.code(), "session_busy");

        // The refusal must not have partially torn the session down.
        assert_eq!(session.state(), SessionState::Observing);
    }

    #[test]
    fn ending_an_observation_does_not_revive_a_closed_session() {
        // `end_observation` runs from a frame source's `Drop`, which can fire after
        // the session has been closed by a shutdown. It must not resurrect it.
        let mut session = CaptureSession::without_display("s", 4);
        session.begin_observation().unwrap();
        session.force_close();

        session.end_observation();
        assert_eq!(session.state(), SessionState::Closed);
    }

    #[test]
    fn ending_an_observation_without_beginning_one_is_harmless() {
        let mut session = CaptureSession::without_display("s", 4);
        session.end_observation();
        assert_eq!(session.state(), SessionState::Ready);
    }

    #[test]
    fn a_failed_session_refuses_every_operation() {
        let mut session = CaptureSession::without_display("s", 4);
        session.fail("the target was lost");

        assert_eq!(session.state(), SessionState::Failed);
        assert_eq!(
            session.begin_observation().unwrap_err().code(),
            "target_lost"
        );
        assert_eq!(session.capture().unwrap_err().code(), "target_lost");
    }

    #[test]
    fn a_closed_session_refuses_a_new_observation() {
        let mut session = CaptureSession::without_display("s", 4);
        session.close().unwrap();

        assert_eq!(
            session.begin_observation().unwrap_err().code(),
            "session_closed"
        );
    }

    #[test]
    fn closing_is_idempotent() {
        let mut session = CaptureSession::without_display("s", 4);
        session.close().unwrap();
        session.close().unwrap();
        assert_eq!(session.state(), SessionState::Closed);
    }
}
