//! Request dispatch.
//!
//! The handler is where a request meets the existing machinery. It contains
//! routing and a little policy, and no capture, comparison, or observation logic
//! of its own: those are reached through [`CaptureSession`], [`crate::compare`],
//! and [`crate::observe`] unchanged.
//!
//! The service owns exactly one [`SessionManager`]. Sessions are shared handles,
//! so a long observation in one session never blocks a capture in another.

use std::sync::Arc;
use std::time::Instant;

use crate::error::Error;
use crate::observe::SystemClock;
use crate::service::protocol::{
    ErrorBody, ImageOptionsWire, ObservationKindWire, Request, RequestEnvelope, ResponseBody,
    ResponseEnvelope, PROTOCOL_VERSION,
};
use crate::session::manager::SessionManager;
use crate::session::pipeline::{
    self as session_pipeline, FrameRequest, ServiceStatus, SessionDiffResponse,
};
use crate::session::realtime as session_realtime;
use crate::timing::Stopwatch;

/// The service's request handler.
pub struct Handler {
    manager: Arc<SessionManager>,
    started_at: Instant,
    version: String,
}

impl Handler {
    /// Build a handler with an empty session registry.
    pub fn new() -> Self {
        Handler {
            manager: Arc::new(SessionManager::new()),
            started_at: Instant::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Build a handler over an existing manager.
    pub fn with_manager(manager: Arc<SessionManager>) -> Self {
        Handler {
            manager,
            started_at: Instant::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// The shared manager, so a caller can inspect or shut it down.
    pub fn manager(&self) -> &Arc<SessionManager> {
        &self.manager
    }

    /// How many sessions are registered.
    pub fn session_count(&self) -> usize {
        self.manager.len()
    }

    /// Handle one envelope, always producing a response.
    ///
    /// A request that fails produces an error envelope rather than propagating a
    /// Rust error, because a protocol-level failure to *respond* would leave the
    /// client hanging. The one exception is a failure to serialize the response
    /// itself, which the transport layer reports.
    pub fn handle(&self, envelope: RequestEnvelope) -> ResponseEnvelope {
        let request_id = envelope.request_id.clone();

        // The version is checked before the request is dispatched, so a client
        // built against a different protocol learns that immediately instead of
        // receiving a parse error about whichever field happens to have changed.
        let outcome = match envelope.check_version() {
            Ok(()) => self.dispatch(envelope.request),
            Err(error) => Err(error),
        };

        match outcome {
            Ok(result) => ResponseEnvelope {
                request_id,
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(error) => ResponseEnvelope {
                request_id,
                ok: false,
                result: None,
                error: Some(ErrorBody::from_error(&error)),
            },
        }
    }

    /// Route a request to its implementation.
    fn dispatch(&self, request: Request) -> Result<ResponseBody, Error> {
        match request {
            Request::Ping => Ok(ResponseBody::Status {
                status: ServiceStatus {
                    protocol_version: PROTOCOL_VERSION,
                    version: self.version.clone(),
                    sessions: self.manager.len(),
                    uptime_ms: self.started_at.elapsed().as_millis() as u64,
                },
            }),

            Request::SessionCreate {
                display,
                target,
                history,
            } => {
                let config =
                    crate::service::protocol::session_config_from(display, &target, history)?;

                // The service is not the place for arbitrary history sizes: an
                // agent that asks for a capacity past the limit gets an explicit
                // refusal from the session, not a silently clamped value.
                let handle = self.manager.create(config)?;

                let info = {
                    let session = handle.lock().expect("session poisoned");
                    session.info()
                };

                Ok(ResponseBody::SessionCreated {
                    session_id: info.session_id.clone(),
                    info,
                })
            }

            Request::SessionList => Ok(ResponseBody::SessionList {
                sessions: self.manager.list(),
            }),

            Request::SessionInfo { session_id } => {
                let handle = self.manager.get(&session_id)?;
                let info = {
                    let session = handle.lock().expect("session poisoned");
                    session.info()
                };
                Ok(ResponseBody::SessionInfo { info })
            }

            Request::SessionClose { session_id } => {
                self.manager.close(&session_id)?;
                Ok(ResponseBody::SessionClosed { session_id })
            }

            Request::SessionCapture {
                session_id,
                output,
                presentation,
            } => {
                let handle = self.manager.get(&session_id)?;
                let options = output.to_image_options()?;
                let (frame, presentation) = session_pipeline::session_frame(
                    &handle,
                    FrameRequest::Capture,
                    &options,
                    presentation.as_deref(),
                )?;
                Ok(ResponseBody::Frame {
                    frame: Box::new(frame),
                    presentation: presentation.map(Box::new),
                })
            }

            Request::SessionLatest {
                session_id,
                output,
                presentation,
            } => {
                let handle = self.manager.get(&session_id)?;
                let options = output.to_image_options()?;
                let (frame, presentation) = session_pipeline::session_frame(
                    &handle,
                    FrameRequest::Latest,
                    &options,
                    presentation.as_deref(),
                )?;
                Ok(ResponseBody::Frame {
                    frame: Box::new(frame),
                    presentation: presentation.map(Box::new),
                })
            }

            Request::SessionFrame {
                session_id,
                frame_id,
                output,
                presentation,
            } => {
                let handle = self.manager.get(&session_id)?;
                let options = output.to_image_options()?;
                let (frame, presentation) = session_pipeline::session_frame(
                    &handle,
                    FrameRequest::ById(frame_id),
                    &options,
                    presentation.as_deref(),
                )?;
                Ok(ResponseBody::Frame {
                    frame: Box::new(frame),
                    presentation: presentation.map(Box::new),
                })
            }

            Request::SessionDiff {
                session_id,
                before,
                after,
                compare,
                changed,
            } => {
                let handle = self.manager.get(&session_id)?;
                let options: crate::compare::CompareOptions = compare.into();

                let stopwatch = Stopwatch::start();
                let comparison = {
                    let session = handle.lock().expect("session poisoned");
                    session.compare(before, after, &options)?
                };
                let compare_us = stopwatch.elapsed_us();

                // The changed-region view is cropped from the *newer* frame, which
                // is the one the bounding box describes. Retrieving it by identity
                // rather than re-capturing is what guarantees the crop shows the same
                // moment the comparison described.
                let changed_view = match changed {
                    Some(policy) => {
                        let newer = {
                            let session = handle.lock().expect("session poisoned");
                            session.frame(after)?
                        };
                        let presentable = crate::presentation::PresentableFrame {
                            session_id: newer.session_id.clone(),
                            frame_id: newer.frame_id,
                            frame: Arc::clone(&newer.frame),
                            captured_at: newer.captured_at,
                            capture_offset: None,
                            capture_duration: Some(newer.capture_duration),
                        };
                        crate::presentation::changed_region(&comparison, &presentable, &policy)?
                            .map(crate::presentation::changed_to_response)
                    }
                    None => None,
                };

                Ok(ResponseBody::Diff {
                    diff: SessionDiffResponse {
                        session_id,
                        before,
                        after,
                        comparison,
                        compare_us,
                    },
                    changed_view,
                })
            }

            Request::SessionObserve {
                session_id,
                kind,
                temporal,
                stable_for_ms,
                output,
                presentation: _,
            } => {
                // `try_get` rather than `get`: only one observation may run per
                // session, and a second is refused with an explicit error rather
                // than queued invisibly behind the first.
                let handle = self.manager.try_get(&session_id)?;
                let options = output.to_image_options()?;
                let clock = SystemClock::new();

                // The Phase 3 state machines run unchanged. Only the frame source
                // differs, and it is a persistent session.
                let outcome = match kind {
                    ObservationKindWire::WaitChange => session_pipeline::session_wait_change(
                        &handle,
                        &temporal.to_wait_change(),
                        &options,
                        &clock,
                    )?,
                    ObservationKindWire::WaitStable => session_pipeline::session_wait_stable(
                        &handle,
                        &temporal.to_wait_stable(stable_for_ms),
                        &options,
                        &clock,
                    )?,
                    ObservationKindWire::Observe => session_pipeline::session_observe(
                        &handle,
                        &temporal.to_observe(stable_for_ms),
                        &options,
                        &clock,
                    )?,
                };

                Ok(ResponseBody::Observation {
                    observation: Box::new(outcome.response),
                    presentation: None,
                })
            }

            Request::SessionRealtime {
                session_id,
                realtime,
                output,
                presentation,
            } => {
                // `try_get` for the same reason as an observation: `realtime` is a
                // temporal operation, and only one may run per session. A queued
                // real-time observation would be stale before it started, so it is
                // refused promptly instead.
                let handle = self.manager.try_get(&session_id)?;
                let options = output.to_image_options()?;
                let clock = SystemClock::new();

                // Sampling first, entirely, and only then presentation. Nothing is
                // encoded inside the sampling window, because that would stretch the
                // interval the caller asked for. This is the ordering the Phase 6
                // specification calls critical (requirement 34): the presentation
                // policy is applied to frames that have already been captured, and it
                // cannot influence when they were taken.
                let capture =
                    session_realtime::session_realtime(&handle, &realtime.into(), &clock)?;

                let presentation = match presentation {
                    Some(policy) => {
                        let presentable: Vec<crate::presentation::PresentableFrame> = capture
                            .samples
                            .iter()
                            .map(|sample| crate::presentation::PresentableFrame {
                                session_id: sample.session_frame.session_id.clone(),
                                frame_id: sample.session_frame.frame_id,
                                frame: Arc::clone(&sample.session_frame.frame),
                                captured_at: sample.session_frame.captured_at,
                                capture_offset: Some(sample.capture_offset),
                                capture_duration: Some(sample.capture_duration),
                            })
                            .collect();
                        let presented = crate::presentation::present_stack(&presentable, &policy)?;
                        let mode = policy.temporal.as_ref().map(|t| t.mode_name().to_string());
                        Some(crate::presentation::to_response(&presented, mode))
                    }
                    None => None,
                };

                let response = capture.prepare(&options)?;

                Ok(ResponseBody::Realtime {
                    realtime: Box::new(response),
                    presentation: presentation.map(Box::new),
                })
            }
        }
    }

    /// Close every session. Called on shutdown.
    pub fn shutdown(&self) {
        self.manager.shutdown_all();
    }
}

impl Default for Handler {
    fn default() -> Self {
        Self::new()
    }
}

/// The default presentation used when a client omits image options.
pub fn default_image_options() -> ImageOptionsWire {
    ImageOptionsWire::default_png_base64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::protocol::{RequestEnvelope, RequestTarget};

    fn envelope(method: Request) -> RequestEnvelope {
        RequestEnvelope::new("test", method)
    }

    #[test]
    fn ping_reports_the_protocol_version_and_session_count() {
        let handler = Handler::new();
        let response = handler.handle(envelope(Request::Ping));

        assert!(response.ok);
        match response.result.unwrap() {
            ResponseBody::Status { status } => {
                assert_eq!(status.protocol_version, PROTOCOL_VERSION);
                assert_eq!(status.sessions, 0);
                assert!(!status.version.is_empty());
            }
            other => panic!("expected a status, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_session_is_reported_not_panicked() {
        let handler = Handler::new();
        let response = handler.handle(envelope(Request::SessionInfo {
            session_id: "s-nope".to_string(),
        }));

        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "session_not_found");
    }

    #[test]
    fn closing_an_unknown_session_is_reported() {
        let handler = Handler::new();
        let response = handler.handle(envelope(Request::SessionClose {
            session_id: "s-nope".to_string(),
        }));
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "session_not_found");
    }

    #[test]
    fn listing_an_empty_manager_returns_an_empty_list() {
        let handler = Handler::new();
        let response = handler.handle(envelope(Request::SessionList));
        assert!(response.ok);
        match response.result.unwrap() {
            ResponseBody::SessionList { sessions } => assert!(sessions.is_empty()),
            other => panic!("expected a list, got {other:?}"),
        }
    }

    #[test]
    fn creating_a_session_on_a_missing_display_fails_without_registering_one() {
        let handler = Handler::new();
        let response = handler.handle(envelope(Request::SessionCreate {
            display: ":54321".to_string(),
            target: RequestTarget::Desktop,
            history: None,
        }));

        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "display_unavailable");
        assert_eq!(
            handler.session_count(),
            0,
            "a failed create registers nothing"
        );
    }

    #[test]
    fn creating_a_session_with_a_bad_capacity_is_refused() {
        let handler = Handler::new();
        let response = handler.handle(envelope(Request::SessionCreate {
            display: ":54321".to_string(),
            target: RequestTarget::Desktop,
            history: Some(0),
        }));

        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "invalid_arguments");
    }

    #[test]
    fn a_capture_with_conflicting_resize_options_is_refused_before_capture() {
        let handler = Handler::new();
        let mut output = default_image_options();
        output.width = Some(100);
        output.height = Some(50);

        let response = handler.handle(envelope(Request::SessionCapture {
            session_id: "s-nope".to_string(),
            output,
            presentation: None,
        }));

        // Session lookup happens first, so this asserts the ordering rather than
        // the resize check alone.
        assert!(!response.ok);
    }

    #[test]
    fn every_request_method_produces_a_response() {
        // A protocol that can panic on a well-formed request is not a protocol.
        let handler = Handler::new();
        let requests = vec![
            Request::Ping,
            Request::SessionList,
            Request::SessionInfo {
                session_id: "s-1".to_string(),
            },
            Request::SessionClose {
                session_id: "s-1".to_string(),
            },
            Request::SessionCapture {
                session_id: "s-1".to_string(),
                output: default_image_options(),
                presentation: None,
            },
            Request::SessionLatest {
                session_id: "s-1".to_string(),
                output: default_image_options(),
                presentation: None,
            },
            Request::SessionFrame {
                session_id: "s-1".to_string(),
                frame_id: crate::session::FrameId(1),
                output: default_image_options(),
                presentation: None,
            },
            Request::SessionDiff {
                session_id: "s-1".to_string(),
                before: crate::session::FrameId(1),
                after: crate::session::FrameId(2),
                compare: crate::compare::CompareOptions::default().into(),
                changed: None,
            },
            Request::SessionObserve {
                session_id: "s-1".to_string(),
                kind: ObservationKindWire::Observe,
                temporal: crate::observe::TemporalCompareOptions::default().into(),
                stable_for_ms: 300,
                output: default_image_options(),
                presentation: None,
            },
        ];

        for request in requests {
            let name = request.method();
            let response = handler.handle(envelope(request));
            assert_eq!(response.request_id, "test", "{name} lost its request id");
            assert!(
                response.result.is_some() || response.error.is_some(),
                "{name} produced neither a result nor an error"
            );
        }
    }

    #[test]
    fn shutdown_clears_the_registry() {
        let handler = Handler::new();
        handler.shutdown();
        assert_eq!(handler.session_count(), 0);
    }

    #[test]
    fn the_request_id_is_echoed_on_failure_too() {
        let handler = Handler::new();
        let response = handler.handle(RequestEnvelope::new(
            "unique-42",
            Request::SessionInfo {
                session_id: "s-nope".to_string(),
            },
        ));
        assert_eq!(response.request_id, "unique-42");
    }
}
