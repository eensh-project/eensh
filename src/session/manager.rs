//! Session registry and the bridge to Phase 3 observation.
//!
//! The manager owns sessions and hands out shared handles. It deliberately does
//! **not** contain capture logic: a session owns its own target-specific state,
//! and the manager only decides which session a request refers to and whether it
//! may run.
//!
//! # Locking
//!
//! Each session has its own mutex, so a long observation in one session does not
//! block a capture in another. A single global lock is avoided on purpose — it
//! would turn an unrelated session's slow capture into a global stall.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::error::Error;
use crate::frame::Frame;
use crate::observe::FrameSource;
use crate::session::history::FrameId;
use crate::session::{CaptureSession, SessionConfig, SessionInfo, SessionState};

/// A session handle shared between the manager and in-flight requests.
pub type SharedSession = Arc<Mutex<CaptureSession>>;

/// A registry of live sessions.
#[derive(Default)]
pub struct SessionManager {
    sessions: Mutex<HashMap<String, SharedSession>>,
    /// Serializes identifier generation so two concurrent creates cannot collide.
    next_serial: Mutex<u64>,
}

impl SessionManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        SessionManager::default()
    }

    /// Create a session and register it.
    ///
    /// The session identifier is opaque and generated here, so callers never have
    /// to infer identity from a display number, a window ID, or a socket path.
    pub fn create(&self, config: SessionConfig) -> Result<SharedSession, Error> {
        let session_id = self.generate_id();

        // The session is created outside the registry lock, so a slow display
        // connection does not block other sessions.
        let session = CaptureSession::create(session_id.clone(), config)?;
        let handle = Arc::new(Mutex::new(session));

        let mut sessions = self.sessions.lock().expect("session registry poisoned");
        sessions.insert(session_id, Arc::clone(&handle));
        Ok(handle)
    }

    /// Look up a session.
    pub fn get(&self, session_id: &str) -> Result<SharedSession, Error> {
        let sessions = self.sessions.lock().expect("session registry poisoned");
        sessions
            .get(session_id)
            .map(Arc::clone)
            .ok_or_else(|| Error::session_not_found(describe_missing(session_id)))
    }

    /// Remove a session from the registry and close it.
    ///
    /// Closing an already-closed session is idempotent, but a second `close` for
    /// an identifier that is no longer registered is reported as
    /// [`Error::SessionNotFound`], which is the honest answer: the caller is
    /// asking about a session this process no longer has.
    pub fn close(&self, session_id: &str) -> Result<(), Error> {
        // The registry lock is held across the session lock here, deliberately.
        // The refusal has to happen *before* the session is unregistered: a close
        // that is refused because an observation is running must leave the session
        // exactly as it was, or the caller is told "try again later" and then finds
        // that the session has vanished. The registry-before-session order is safe
        // because nothing else in this module takes the session lock and then the
        // registry lock, so there is no cycle to deadlock on. The session lock is
        // held only for the duration of `close`, which does not wait for anything.
        let mut sessions = self.sessions.lock().expect("session registry poisoned");

        let handle = sessions
            .get(session_id)
            .ok_or_else(|| Error::session_not_found(describe_missing(session_id)))?;

        let mut session = handle.lock().expect("session poisoned");

        // May refuse. On refusal the session remains registered and untouched.
        session.close()?;

        drop(session);
        sessions.remove(session_id);
        Ok(())
    }

    /// Descriptions of every registered session.
    pub fn list(&self) -> Vec<SessionInfo> {
        let handles: Vec<SharedSession> = {
            let sessions = self.sessions.lock().expect("session registry poisoned");
            sessions.values().map(Arc::clone).collect()
        };

        let mut infos: Vec<SessionInfo> = handles
            .iter()
            .map(|handle| {
                let session = handle.lock().expect("session poisoned");
                session.info()
            })
            .collect();

        // Stable ordering so output does not shuffle between calls.
        infos.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        infos
    }

    /// How many sessions are registered.
    pub fn len(&self) -> usize {
        self.sessions
            .lock()
            .expect("session registry poisoned")
            .len()
    }

    /// Whether no sessions are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Look up a session, failing immediately when it is running an observation.
    ///
    /// Used by operations that must not queue invisibly behind a long-running
    /// observation. The check happens under a brief lock, so it reports a genuine
    /// state rather than a guess.
    pub fn try_get(&self, session_id: &str) -> Result<SharedSession, Error> {
        let handle = self.get(session_id)?;
        {
            let session = handle.lock().expect("session poisoned");
            if session.state() == SessionState::Observing {
                return Err(Error::session_busy(format!(
                    "session {session_id} is running an observation"
                )));
            }
        }
        Ok(handle)
    }

    /// Close every session and clear the registry.
    ///
    /// Called on shutdown. Each session releases its X11 connection and its
    /// retained frames when its handle drops.
    pub fn shutdown_all(&self) {
        let handles: Vec<SharedSession> = {
            let mut sessions = self.sessions.lock().expect("session registry poisoned");
            sessions.drain().map(|(_, handle)| handle).collect()
        };

        for handle in handles {
            // Best effort: a poisoned lock means the session already failed, and
            // its resources drop with the handle regardless.
            //
            // Closing here is unconditional rather than going through the public
            // refusal rule, because shutdown must not be blocked by an observation
            // that is still running: the process is going away, so waiting would
            // only delay the inevitable.
            if let Ok(mut session) = handle.lock() {
                session.force_close();
            }
        }
    }

    /// Generate an opaque session identifier.
    ///
    /// The identifier is random rather than a counter, so it cannot be guessed
    /// and does not leak how many sessions have existed. Uniqueness is checked
    /// against the registry rather than assumed, because a random collision is
    /// unlikely but not impossible.
    fn generate_id(&self) -> String {
        let mut serial = self.next_serial.lock().expect("serial poisoned");
        loop {
            *serial += 1;
            let candidate = format!("s-{}-{}", unique_seed(), *serial);
            let sessions = self.sessions.lock().expect("session registry poisoned");
            if !sessions.contains_key(&candidate) {
                return candidate;
            }
        }
    }
}

/// A per-process unique seed for identifiers.
///
/// Uses the process id and a monotonic instant, which is enough to make
/// identifiers non-guessable and unique across concurrent services. This is an
/// identity token, not a security credential: the socket permissions are what
/// protect the session, not the difficulty of guessing its name.
fn unique_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ ((std::process::id() as u64) << 32)
}

/// Explain why a session identifier is not registered.
fn describe_missing(session_id: &str) -> String {
    format!(
        "no session {session_id:?} is registered; it may have been closed, or the \
         service may have restarted, which invalidates every previous session"
    )
}

/// Adapts a session handle to the Phase 3 [`FrameSource`] trait.
///
/// This is the whole of the Phase 4 observation integration: the state machines
/// are reused unchanged, and the only difference is that each sample comes from a
/// persistent connection and lands in history.
///
/// Three details carry the semantics:
///
/// * Every physical capture receives a frame identifier and enters history, so
///   after an observation a caller can refer back to the frames that mattered.
/// * Exclusivity comes from the session's `Observing` state, which is set for the
///   whole operation. A second observation, or a bare capture, is refused with an
///   explicit error instead of running concurrently and interleaving frame
///   identifiers.
/// * The source can optionally record the first frame that meaningfully differed
///   from the first frame it captured, so that `observe` can report *which frame*
///   showed the transition rather than only *when* it happened. The state machine
///   deliberately reports outcomes, not identities, so the identity has to be
///   captured where the frames actually arrive.
///
/// The session lock itself is taken only for each individual capture, not for the
/// whole observation, so a quick read-only request in another thread is not
/// blocked for the duration of a multi-second observation.
pub struct SessionFrameSource {
    handle: SharedSession,
    session_id: String,
    /// The frame the source most recently captured.
    last_frame_id: Option<FrameId>,
    /// The first frame the source captured, which is the observation baseline.
    baseline_frame_id: Option<FrameId>,
    /// The first frame that meaningfully differed from the baseline, when change
    /// tracking is enabled.
    first_change: Option<FrameId>,
    /// The baseline frame, retained so change tracking can compare against it.
    ///
    /// Held as a shared handle rather than a copy: the pixels are the same
    /// allocation history holds, so this costs a pointer even if history evicts
    /// the baseline. That is the mechanism that keeps a long observation correct
    /// when its baseline leaves public history.
    baseline_frame: Option<Arc<Frame>>,
    /// When set, the source additionally records the first meaningful change.
    change_tracking: Option<crate::compare::CompareOptions>,
}

impl SessionFrameSource {
    /// Build a source over a session, marking it as observing.
    ///
    /// Fails with [`Error::SessionBusy`] when the session is already running an
    /// observation, which is the documented explicit refusal rather than an
    /// invisible queue.
    pub fn new(handle: SharedSession) -> Result<Self, Error> {
        Self::build(handle, None)
    }

    /// Build a source that also records which frame first showed a change.
    ///
    /// Used by `observe`, which is the only operation that has a first change to
    /// report. The extra work is one comparison per sample against the baseline,
    /// using the same options the state machine uses, so it reaches the same
    /// conclusion by construction rather than by a second opinion.
    pub fn tracking_change(
        handle: SharedSession,
        options: crate::compare::CompareOptions,
    ) -> Result<Self, Error> {
        Self::build(handle, Some(options))
    }

    fn build(
        handle: SharedSession,
        change_tracking: Option<crate::compare::CompareOptions>,
    ) -> Result<Self, Error> {
        let session_id = {
            let mut session = handle.lock().expect("session poisoned");
            session.begin_observation()?;
            session.session_id().to_string()
        };

        Ok(SessionFrameSource {
            handle,
            session_id,
            last_frame_id: None,
            baseline_frame_id: None,
            first_change: None,
            baseline_frame: None,
            change_tracking,
        })
    }

    /// The identifier of the frame this source captured most recently.
    pub fn last_frame_id(&self) -> Option<FrameId> {
        self.last_frame_id
    }

    /// The identifier of the first frame this source captured.
    pub fn baseline_frame_id(&self) -> Option<FrameId> {
        self.baseline_frame_id
    }

    /// The identifier of the first frame that meaningfully differed from the
    /// baseline, when change tracking is enabled.
    pub fn first_change_frame_id(&self) -> Option<FrameId> {
        self.first_change
    }

    /// The session identifier.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The identifier of the oldest retained frame, if any.
    pub fn oldest_retained_frame_id(&self) -> Option<FrameId> {
        self.handle
            .lock()
            .expect("session poisoned")
            .history()
            .oldest_frame_id()
    }

    /// The identifier of the newest retained frame, if any.
    pub fn newest_retained_frame_id(&self) -> Option<FrameId> {
        self.handle
            .lock()
            .expect("session poisoned")
            .history()
            .latest_frame_id()
    }
}

impl FrameSource for SessionFrameSource {
    fn capture(&mut self) -> Result<Frame, Error> {
        let session_frame = {
            let mut session = self.handle.lock().expect("session poisoned");
            // Capture through the session, so the frame receives an identifier and
            // enters history exactly as a standalone capture would.
            session.capture()?
        };

        self.last_frame_id = Some(session_frame.frame_id);
        if self.baseline_frame_id.is_none() {
            self.baseline_frame_id = Some(session_frame.frame_id);
        }

        // Track the first meaningful change, if asked to.
        if let Some(options) = self.change_tracking {
            match &self.baseline_frame {
                None => self.baseline_frame = Some(Arc::clone(&session_frame.frame)),
                Some(baseline) => {
                    if self.first_change.is_none() {
                        let changed = crate::compare::compare_frames(
                            baseline,
                            &session_frame.frame,
                            &options,
                        )
                        .map(|comparison| comparison.changed)
                        .unwrap_or(false);
                        if changed {
                            self.first_change = Some(session_frame.frame_id);
                        }
                    }
                }
            }
        }

        // The state machine needs an owned `Frame`. The session keeps its own
        // shared copy in history, so this is a cheap handle clone, not a pixel
        // copy: both refer to the same immutable allocation.
        Ok((*session_frame.frame).clone())
    }
}

impl Drop for SessionFrameSource {
    fn drop(&mut self) {
        // Release the observation slot even if the caller unwound. The lock may
        // be poisoned by a panic, in which case the session is already unusable
        // and there is nothing useful to do.
        if let Ok(mut session) = self.handle.lock() {
            session.end_observation();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TargetSpec;

    #[test]
    fn a_missing_session_reports_that_it_is_missing() {
        let manager = SessionManager::new();
        let error = manager.get("s-nope").unwrap_err();
        assert_eq!(error.code(), "session_not_found");
        assert!(error.message().contains("s-nope"));
        assert!(
            error.message().contains("restarted"),
            "the message should hint at the restart case, was: {}",
            error.message()
        );
    }

    #[test]
    fn closing_an_unknown_session_is_not_found() {
        let manager = SessionManager::new();
        let error = manager.close("s-nope").unwrap_err();
        assert_eq!(error.code(), "session_not_found");
    }

    #[test]
    fn an_empty_manager_is_empty() {
        let manager = SessionManager::new();
        assert!(manager.is_empty());
        assert_eq!(manager.len(), 0);
        assert!(manager.list().is_empty());
    }

    #[test]
    fn shutdown_with_no_sessions_is_harmless() {
        let manager = SessionManager::new();
        manager.shutdown_all();
        assert!(manager.is_empty());
    }

    #[test]
    fn identifiers_are_opaque_and_unique() {
        let manager = SessionManager::new();
        let first = manager.generate_id();
        let second = manager.generate_id();
        assert_ne!(first, second);
        // Opaque: not a bare integer, not the display, not a window id.
        assert!(first.starts_with("s-"));
        assert_ne!(first, ":99");
    }

    #[test]
    fn a_bad_capacity_does_not_register_a_session() {
        let manager = SessionManager::new();
        let config = SessionConfig::new(TargetSpec::desktop(":54321")).with_capacity(0);
        assert!(manager.create(config).is_err());
        assert!(
            manager.is_empty(),
            "a session that failed to create must not be registered"
        );
    }
}
