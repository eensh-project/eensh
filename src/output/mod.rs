//! Presentation: where the finished bytes and metadata go.
//!
//! Nothing here knows how the image was captured or encoded. The pipeline hands
//! over finished byte streams and strings, and this module decides whether they
//! land in a file, on stdout, or on stderr.

pub mod base64;
pub mod file;
pub mod json;

use std::path::PathBuf;

use crate::error::Error;

/// Where the agent-facing result should be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// A file on disk.
    File(PathBuf),
    /// Standard output. Selected by `-`.
    Stdout,
}

impl Destination {
    /// Interpret a positional output argument.
    ///
    /// `None` means no path was given, which defaults to standard output so
    /// that `eensh capture ...` behaves like a filter.
    pub fn from_argument(argument: Option<&str>) -> Self {
        match argument {
            None | Some("-") => Destination::Stdout,
            Some(path) => Destination::File(PathBuf::from(path)),
        }
    }

    /// True when the destination is standard output.
    pub fn is_stdout(&self) -> bool {
        matches!(self, Destination::Stdout)
    }

    /// Write encoded image bytes to this destination.
    pub fn write_image(&self, bytes: &[u8]) -> Result<(), Error> {
        match self {
            Destination::File(path) => file::write_file(path, bytes),
            Destination::Stdout => file::write_stdout(bytes),
        }
    }

    /// Human readable description used in error messages and diagnostics.
    pub fn describe(&self) -> String {
        match self {
            Destination::File(path) => path.display().to_string(),
            Destination::Stdout => "<stdout>".to_string(),
        }
    }
}

/// Where the JSON metadata goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataDestination {
    /// `--json` was not requested.
    None,
    /// JSON goes to standard output.
    Stdout,
    /// JSON goes to standard error, leaving stdout free for image bytes.
    Stderr,
}

impl MetadataDestination {
    /// Write the metadata text to the chosen stream.
    pub fn write(&self, text: &str) -> Result<(), Error> {
        match self {
            MetadataDestination::None => Ok(()),
            MetadataDestination::Stdout => file::write_stdout_text(text),
            MetadataDestination::Stderr => {
                if text.ends_with('\n') {
                    file::write_stderr(text.trim_end_matches('\n'));
                } else {
                    file::write_stderr(text);
                }
                Ok(())
            }
        }
    }

    /// True when metadata would be written to standard output.
    pub fn is_stdout(&self) -> bool {
        matches!(self, MetadataDestination::Stdout)
    }
}

/// A fully resolved decision about where the two result streams go.
///
/// The rules below exist so that image bytes and JSON are never interleaved on
/// the same stream:
///
/// | `--json` | `--base64` | output path | stdout        | metadata |
/// |----------|------------|-------------|---------------|----------|
/// | no       | no         | any         | image bytes   | none     |
/// | yes      | no         | file        | JSON          | stdout   |
/// | yes      | no         | `-`         | image bytes   | stderr   |
/// | yes      | yes        | file        | JSON          | stdout   |
/// | yes      | yes        | `-`         | JSON          | stdout   |
///
/// The last row is the agent-facing default: the image travels inside the JSON
/// document as base64, so no raw bytes are written to stdout at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputPlan {
    /// Where encoded image bytes go.
    pub destination: Destination,
    /// Where JSON metadata goes.
    pub metadata: MetadataDestination,
    /// Whether raw encoded image bytes should be written at all.
    pub write_image_bytes: bool,
}

impl OutputPlan {
    /// Resolve the output routing from the requested flags.
    ///
    /// Returns [`Error::InvalidArguments`] for `--base64` without `--json`,
    /// because there would be no document to carry the payload.
    pub fn resolve(json: bool, base64: bool, destination: Destination) -> Result<Self, Error> {
        if base64 && !json {
            return Err(Error::invalid_arguments(
                "--base64 requires --json: the base64 payload is delivered inside the JSON \
                 response",
            ));
        }

        // Raw bytes are suppressed only when the JSON document on stdout is
        // itself carrying the image, and only when that document goes to stdout.
        let write_image_bytes = !(json && base64 && destination.is_stdout());

        let metadata = if !json {
            MetadataDestination::None
        } else if destination.is_stdout() && write_image_bytes {
            // Binary owns stdout; metadata steps aside onto stderr.
            MetadataDestination::Stderr
        } else {
            MetadataDestination::Stdout
        };

        Ok(OutputPlan {
            destination,
            metadata,
            write_image_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_defaults_to_stdout() {
        assert_eq!(Destination::from_argument(None), Destination::Stdout);
        assert_eq!(Destination::from_argument(Some("-")), Destination::Stdout);
        assert_eq!(
            Destination::from_argument(Some("/tmp/out.png")),
            Destination::File(PathBuf::from("/tmp/out.png"))
        );
    }

    #[test]
    fn stdout_destination_is_recognised() {
        assert!(Destination::Stdout.is_stdout());
        assert!(!Destination::File(PathBuf::from("x")).is_stdout());
        assert!(MetadataDestination::Stdout.is_stdout());
        assert!(!MetadataDestination::Stderr.is_stdout());
        assert!(!MetadataDestination::None.is_stdout());
    }

    #[test]
    fn plain_file_output_writes_no_metadata() {
        let plan =
            OutputPlan::resolve(false, false, Destination::File(PathBuf::from("a.png"))).unwrap();
        assert!(plan.write_image_bytes);
        assert_eq!(plan.metadata, MetadataDestination::None);
    }

    #[test]
    fn plain_stdout_output_writes_no_metadata() {
        let plan = OutputPlan::resolve(false, false, Destination::Stdout).unwrap();
        assert!(plan.write_image_bytes);
        assert_eq!(plan.metadata, MetadataDestination::None);
    }

    #[test]
    fn json_with_a_file_keeps_metadata_on_stdout() {
        let plan =
            OutputPlan::resolve(true, false, Destination::File(PathBuf::from("a.png"))).unwrap();
        assert!(plan.write_image_bytes);
        assert_eq!(plan.metadata, MetadataDestination::Stdout);
    }

    #[test]
    fn json_with_binary_stdout_moves_metadata_to_stderr() {
        let plan = OutputPlan::resolve(true, false, Destination::Stdout).unwrap();
        assert!(plan.write_image_bytes);
        assert_eq!(plan.metadata, MetadataDestination::Stderr);
    }

    #[test]
    fn base64_json_alone_never_writes_raw_bytes_to_stdout() {
        let plan = OutputPlan::resolve(true, true, Destination::Stdout).unwrap();
        assert!(!plan.write_image_bytes);
        assert_eq!(plan.metadata, MetadataDestination::Stdout);
    }

    #[test]
    fn base64_json_with_a_file_still_writes_the_file() {
        let plan =
            OutputPlan::resolve(true, true, Destination::File(PathBuf::from("a.jpg"))).unwrap();
        assert!(plan.write_image_bytes);
        assert_eq!(plan.metadata, MetadataDestination::Stdout);
    }

    #[test]
    fn base64_without_json_is_rejected() {
        let error = OutputPlan::resolve(false, true, Destination::Stdout).unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
    }
}
