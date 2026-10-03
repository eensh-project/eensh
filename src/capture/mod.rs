//! Capture backends.
//!
//! A backend's only job is to produce a raw [`crate::frame::Frame`]. Nothing in
//! this module knows about PNG, JPEG, base64, JSON, or output destinations.

pub mod display;
pub mod x11;

pub use display::Display;
pub use x11::CaptureRequest;
