//! `eensh` — fast, deterministic X11 screenshot capture and frame comparison for
//! software agents.
//!
//! # Architecture
//!
//! The crate is layered so that each stage can be replaced without disturbing
//! the others. Capture and comparison are separate flows that share one
//! abstraction, the raw [`frame::Frame`].
//!
//! Capture:
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
//! Comparison:
//!
//! ```text
//! Frame A ----\
//!              +-- compare (compare/) -- Comparison
//! Frame B ----/
//! ```
//!
//! The invariant worth protecting is that the capture backend produces only a
//! raw [`frame::Frame`], and that comparison consumes only raw frames. Neither
//! knows about PNG, JPEG, base64, JSON, or the filesystem. Later phases (temporal
//! observation, persistent capture) build on exactly those two primitives.
//!
//! # Example: capturing
//!
//! ```no_run
//! use eensh::cli::{Cli, Command};
//! use eensh::pipeline;
//! use clap::Parser;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let cli = Cli::parse_from([
//!     "eensh", "capture", "--display", ":99", "--base64", "--json",
//! ]);
//! let Command::Capture(args) = cli.command else {
//!     unreachable!("the arguments say capture")
//! };
//! let config = args.resolve()?;
//! let outcome = pipeline::run(&config)?;
//! println!("captured {}x{}", outcome.response.image.width, outcome.response.image.height);
//! # Ok(())
//! # }
//! ```
//!
//! # Example: comparing two frames
//!
//! Comparison needs no encoding on either side, so it works equally well on live
//! captures and on frames decoded from files.
//!
//! ```no_run
//! use eensh::compare::{compare_frames, CompareMode, CompareOptions};
//! use eensh::frame::Frame;
//!
//! # fn compare(before: &Frame, after: &Frame) -> Result<(), eensh::Error> {
//! let options = CompareOptions {
//!     mode: CompareMode::RgbThreshold,
//!     pixel_threshold: 12,
//!     area_threshold: 0.005,
//! };
//!
//! let comparison = compare_frames(before, after, &options)?;
//! if comparison.changed {
//!     println!(
//!         "{:.2}% of pixels changed, region {:?}",
//!         comparison.changed_fraction * 100.0,
//!         comparison.bounding_box,
//!     );
//! }
//! # Ok(())
//! # }
//! ```

pub mod capture;
pub mod cli;
pub mod compare;
pub mod diff;
pub mod encode;
pub mod error;
pub mod frame;
pub mod geometry;
pub mod input;
pub mod observe;
pub mod output;
pub mod pipeline;
pub mod resize;
pub mod timing;

pub use error::Error;

/// Default JPEG quality used when `--quality` is omitted.
pub use encode::jpeg::DEFAULT_QUALITY as DEFAULT_JPEG_QUALITY;

/// The raw-frame comparison primitive.
///
/// This is the function later phases will call inside an observation loop. It
/// takes two [`frame::Frame`] values and returns a [`compare::Comparison`], with
/// no encoding, JSON, filesystem, or X11 involvement.
pub use compare::{compare_frames, CompareMode, CompareOptions, Comparison};
