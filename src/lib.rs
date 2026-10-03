//! `eensh` — fast, deterministic X11 screenshot capture for software agents.
//!
//! # Architecture
//!
//! The crate is layered so that each stage can be replaced without disturbing
//! the others. Data flows in one direction:
//!
//! ```text
//! X11 / Xvfb
//!     -> capture backend  (capture/)   raw Frame with source geometry
//!     -> transformation   (resize)     raw Frame, still uncompressed
//!     -> encoding         (encode/)    PNG or JPEG bytes
//!     -> optional base64  (output/)    text carrying those exact bytes
//!     -> presentation     (output/)    file, stdout, JSON
//! ```
//!
//! The invariant worth protecting is that the capture backend produces only a
//! raw [`frame::Frame`]. It knows nothing about PNG, JPEG, base64, JSON, or the
//! filesystem. Later phases (frame comparison, temporal observation, persistent
//! capture) all build on that raw frame, so it is kept free of output concerns.
//!
//! # Example
//!
//! ```no_run
//! use eensh::cli::{CaptureArgs, Cli, Command};
//! use eensh::pipeline;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let cli = <Cli as clap::Parser>::parse_from([
//!     "eensh", "capture", "--display", ":99", "--base64", "--json",
//! ]);
//! let Command::Capture(args) = cli.command;
//! let config = args.resolve()?;
//! let outcome = pipeline::run(&config)?;
//! println!("captured {}x{}", outcome.response.image.width, outcome.response.image.height);
//! # Ok(())
//! # }
//! ```

pub mod capture;
pub mod cli;
pub mod encode;
pub mod error;
pub mod frame;
pub mod geometry;
pub mod output;
pub mod pipeline;
pub mod resize;
pub mod timing;

pub use error::Error;

/// Default JPEG quality used when `--quality` is omitted.
pub use encode::jpeg::DEFAULT_QUALITY as DEFAULT_JPEG_QUALITY;
