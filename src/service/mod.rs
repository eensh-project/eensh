//! The local persistent-capture service.
//!
//! Phase 4's durability comes from a process that outlives a single command. The
//! layering is:
//!
//! ```text
//!   client / CLI
//!        |  local Unix socket, length-delimited JSON
//!        v
//!   service (handler.rs dispatch)
//!        v
//!   session manager  -- one CaptureSession per target
//!        v
//!   persistent X11 connection, frame IDs, bounded raw history
//!        v
//!   raw Frame
//!        v
//!   the existing compare / observe machinery, unchanged
//! ```
//!
//! Transport concerns are confined to [`protocol`] and [`unix`]. The session type
//! knows nothing about sockets, and the comparison and observation code knows
//! nothing about sessions.

pub mod client;
pub mod handler;
pub mod protocol;
pub mod unix;

pub use handler::Handler;
pub use protocol::{
    ErrorBody, ImageOptionsWire, ObservationKindWire, Request, RequestEnvelope, RequestTarget,
    ResponseBody, ResponseEnvelope, PROTOCOL_VERSION,
};
pub use unix::{connect, request, socket_path, Service};
