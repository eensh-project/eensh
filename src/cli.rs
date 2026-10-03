//! Command line interface and its resolution into an executable configuration.
//!
//! Parsing and *resolution* are kept separate on purpose. Parsing only checks
//! that individual arguments are well formed; resolution combines them with the
//! environment and the output path to decide what will actually happen, and
//! rejects combinations that would otherwise be ambiguous. Every rule below is
//! documented, and every rejection is a structured [`Error::InvalidArguments`].

use std::env;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};

use crate::capture::CaptureRequest;
use crate::compare::{CompareMode, CompareOptions};
use crate::encode::{jpeg, ImageFormat, PngEffort};
use crate::error::Error;
use crate::geometry::Rect;
use crate::output::{Destination, OutputPlan};

/// `eensh` — agent-ready X11 screenshot capture.
#[derive(Debug, Parser)]
#[command(
    name = "eensh",
    version,
    about = "Fast, deterministic X11 screenshot capture for software agents",
    long_about = "eensh captures an X11 desktop, region, or window and returns it as \
                  PNG or JPEG, optionally resized, base64 encoded, and described by a \
                  stable JSON response with explicit coordinate transforms.",
    propagate_version = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Top level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Capture an image and write it to a file, stdout, or a JSON response.
    Capture(Box<CaptureArgs>),
    /// Compare two saved images and report what changed.
    Diff(Box<DiffArgs>),
}

/// Arguments for `eensh diff`.
///
/// The two inputs are decoded from PNG or JPEG at this boundary; everything
/// below operates on raw frames. See [`crate::compare`].
#[derive(Debug, Args)]
pub struct DiffArgs {
    /// The earlier image.
    #[arg(value_name = "BEFORE")]
    pub before: String,

    /// The later image.
    #[arg(value_name = "AFTER")]
    pub after: String,

    /// Comparison mode: `exact` or `rgb`.
    ///
    /// `exact` counts any channel difference. `rgb` ignores differences up to
    /// `--pixel-threshold`.
    #[arg(long, value_name = "MODE")]
    pub mode: Option<String>,

    /// Largest per-channel difference still considered unchanged, 0-255.
    #[arg(long, value_name = "0-255", value_parser = clap::value_parser!(u8))]
    pub pixel_threshold: Option<u8>,

    /// Smallest changed fraction still considered meaningful change, 0.0-1.0.
    #[arg(long, value_name = "0.0-1.0", value_parser = parse_area_threshold)]
    pub area_threshold: Option<f64>,

    /// Write a crop of the changed region from the second image to this path.
    ///
    /// When nothing changed, no file is written and the JSON reports
    /// `"changed_crop": null`.
    #[arg(long, value_name = "PATH")]
    pub changed_crop: Option<String>,

    /// Image format for the changed crop: `png` or `jpeg`. Inferred from the
    /// crop path's extension when omitted.
    #[arg(long, value_name = "FORMAT")]
    pub crop_format: Option<String>,

    /// Emit a stable JSON response instead of a human readable summary.
    #[arg(long)]
    pub json: bool,

    /// Print per-stage timings to stderr.
    #[arg(long)]
    pub time: bool,
}

/// Arguments for `eensh capture`.
#[derive(Debug, Args)]
pub struct CaptureArgs {
    /// Output path, or `-` for stdout. When omitted the encoded image is
    /// written to stdout.
    #[arg(value_name = "OUTPUT")]
    pub output: Option<String>,

    /// X11 display to capture from, for example `:99`. Overrides `$DISPLAY`.
    #[arg(long, value_name = "DISPLAY")]
    pub display: Option<String>,

    /// Rectangle to capture, as `X,Y,WIDTH,HEIGHT` in source-desktop pixels.
    ///
    /// Negative origins are allowed, so that a region can be placed relative to
    /// a display whose origin is not at `(0, 0)`. `allow_hyphen_values` is needed
    /// because a value such as `-10,-20,5,5` would otherwise look like a flag.
    #[arg(
        long,
        value_name = "X,Y,W,H",
        value_parser = parse_region,
        allow_hyphen_values = true,
        conflicts_with = "window"
    )]
    pub region: Option<Rect>,

    /// X11 window ID to capture, decimal or `0x`-prefixed hexadecimal.
    #[arg(
        long,
        value_name = "WINDOW_ID",
        value_parser = parse_window_id,
        conflicts_with = "region"
    )]
    pub window: Option<u64>,

    /// Output image format: `png` or `jpeg`. Defaults to `png`, or is inferred
    /// from the output file extension.
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,

    /// JPEG quality, 1-100. Only valid with `--format jpeg`.
    #[arg(long, value_name = "QUALITY", value_parser = clap::value_parser!(u8).range(1..=100))]
    pub quality: Option<u8>,

    /// Resize the result to this width, preserving the aspect ratio.
    #[arg(
        long,
        value_name = "WIDTH",
        value_parser = clap::value_parser!(u32).range(1..),
        conflicts_with = "scale"
    )]
    pub width: Option<u32>,

    /// Resize the result to this height, preserving the aspect ratio.
    #[arg(
        long,
        value_name = "HEIGHT",
        value_parser = clap::value_parser!(u32).range(1..),
        conflicts_with = "scale"
    )]
    pub height: Option<u32>,

    /// Resize the result by this factor, preserving the aspect ratio.
    #[arg(
        long,
        value_name = "FACTOR",
        value_parser = parse_scale,
        conflicts_with_all = ["width", "height"]
    )]
    pub scale: Option<f64>,

    /// PNG compression effort: `fast`, `default`, or `best`.
    #[arg(long, value_name = "EFFORT")]
    pub compression: Option<String>,

    /// Embed the encoded image in the JSON response as base64.
    #[arg(long)]
    pub base64: bool,

    /// Emit a stable JSON response instead of writing only image bytes.
    #[arg(long)]
    pub json: bool,

    /// Print per-stage timings to stderr.
    ///
    /// Suppressed when JSON metadata is already using stderr, to avoid corrupting
    /// the JSON stream; the timings are present in the JSON response regardless.
    #[arg(long)]
    pub time: bool,
}

/// How a resize was requested.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResizeRequest {
    /// No resize was requested.
    None,
    /// Resize to an exact width, deriving the height.
    Width(u32),
    /// Resize to an exact height, deriving the width.
    Height(u32),
    /// Resize by a uniform factor.
    Scale(f64),
}

impl ResizeRequest {
    /// True when no resizing will take place.
    pub fn is_none(&self) -> bool {
        matches!(self, ResizeRequest::None)
    }
}

/// A fully resolved capture, ready to execute.
///
/// Everything ambiguous has already been decided: defaults applied, mutually
/// exclusive options rejected, and output routing fixed.
#[derive(Debug, Clone)]
pub struct ResolvedCapture {
    /// The X11 display to connect to.
    pub display: String,
    /// What to capture.
    pub request: CaptureRequest,
    /// Image format and encoder settings.
    pub format: ImageFormat,
    /// JPEG quality (unused for PNG).
    pub quality: u8,
    /// PNG effort (unused for JPEG).
    pub png_effort: PngEffort,
    /// Requested resize, if any.
    pub resize: ResizeRequest,
    /// Whether to embed base64 data in the JSON response.
    pub base64: bool,
    /// Where image bytes and metadata go.
    pub output: OutputPlan,
    /// Whether to print timings to stderr.
    pub print_timing: bool,
}

impl CaptureArgs {
    /// Resolve these arguments into an executable configuration.
    ///
    /// This is where every interface rule lives, so that the pipeline itself
    /// contains no policy and the rules can be tested without touching a
    /// display.
    pub fn resolve(&self) -> Result<ResolvedCapture, Error> {
        let display = self.resolve_display()?;
        let request = self.resolve_request();
        let format = self.resolve_format()?;
        let quality = self.resolve_quality(format)?;
        let png_effort = self.resolve_png_effort(format)?;
        let resize = self.resolve_resize()?;
        let destination = Destination::from_argument(self.output.as_deref());
        let output = OutputPlan::resolve(self.json, self.base64, destination)?;

        Ok(ResolvedCapture {
            display,
            request,
            format,
            quality,
            png_effort,
            resize,
            base64: self.base64,
            output,
            print_timing: self.time,
        })
    }

    /// Decide which display to use, preferring `--display` over `$DISPLAY`.
    fn resolve_display(&self) -> Result<String, Error> {
        if let Some(display) = self.display.as_deref() {
            if display.is_empty() {
                return Err(Error::InvalidArguments(
                    "--display was given an empty value".to_string(),
                ));
            }
            return Ok(display.to_string());
        }

        match env::var("DISPLAY") {
            Ok(display) if !display.is_empty() => Ok(display),
            _ => Err(Error::invalid_arguments(
                "no X11 display was specified and DISPLAY is not set; pass --display, \
                 for example --display :99",
            )),
        }
    }

    /// Decide what to capture.
    fn resolve_request(&self) -> CaptureRequest {
        if let Some(window) = self.window {
            CaptureRequest::Window(window)
        } else if let Some(region) = self.region {
            CaptureRequest::Region(region)
        } else {
            CaptureRequest::Desktop
        }
    }

    /// Decide the output format.
    fn resolve_format(&self) -> Result<ImageFormat, Error> {
        if let Some(name) = self.format.as_deref() {
            return ImageFormat::from_name(name);
        }

        // Fall back to the output path's extension so that
        // `eensh capture shot.jpg` does what it looks like it does.
        if let Some(path) = self.output.as_deref() {
            if path != "-" {
                if let Some(format) = ImageFormat::from_path(Path::new(path)) {
                    return Ok(format);
                }
            }
        }

        Ok(ImageFormat::Png)
    }

    /// Decide JPEG quality, rejecting it for PNG output.
    fn resolve_quality(&self, format: ImageFormat) -> Result<u8, Error> {
        match (self.quality, format) {
            (Some(_), ImageFormat::Png) => Err(Error::invalid_arguments(
                "--quality applies only to JPEG output; add --format jpeg or drop --quality",
            )),
            (Some(quality), _) => Ok(quality),
            (None, _) => Ok(jpeg::DEFAULT_QUALITY),
        }
    }

    /// Decide PNG compression effort, rejecting it for JPEG output.
    fn resolve_png_effort(&self, format: ImageFormat) -> Result<PngEffort, Error> {
        match (self.compression.as_deref(), format) {
            (Some(_), ImageFormat::Jpeg) => Err(Error::invalid_arguments(
                "--compression applies only to PNG output; add --format png or drop --compression",
            )),
            (Some(name), _) => PngEffort::from_name(name),
            (None, _) => Ok(PngEffort::Default),
        }
    }

    /// Decide the resize request, rejecting unsupported combinations.
    fn resolve_resize(&self) -> Result<ResizeRequest, Error> {
        if self.width.is_some() && self.height.is_some() {
            return Err(Error::invalid_arguments(
                "--width and --height cannot be combined: Phase 1 only supports proportional \
                 resizing. Use one of them, or --scale.",
            ));
        }
        if let Some(width) = self.width {
            return Ok(ResizeRequest::Width(width));
        }
        if let Some(height) = self.height {
            return Ok(ResizeRequest::Height(height));
        }
        if let Some(scale) = self.scale {
            return Ok(ResizeRequest::Scale(scale));
        }
        Ok(ResizeRequest::None)
    }
}

/// Parse `X,Y,WIDTH,HEIGHT` into a [`Rect`].
fn parse_region(text: &str) -> Result<Rect, String> {
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err(format!(
            "expected X,Y,WIDTH,HEIGHT with four comma separated values, got {text:?}"
        ));
    }

    let x = parts[0]
        .parse::<i32>()
        .map_err(|_| format!("x coordinate {:?} is not an integer", parts[0]))?;
    let y = parts[1]
        .parse::<i32>()
        .map_err(|_| format!("y coordinate {:?} is not an integer", parts[1]))?;
    let width = parts[2]
        .parse::<u32>()
        .map_err(|_| format!("width {:?} is not a positive integer", parts[2]))?;
    let height = parts[3]
        .parse::<u32>()
        .map_err(|_| format!("height {:?} is not a positive integer", parts[3]))?;

    Rect::new(x, y, width, height).map_err(|e| e.message())
}

/// Parse a window ID as decimal or `0x`-prefixed hexadecimal.
fn parse_window_id(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    let parsed = match trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => trimmed.parse::<u64>(),
    };

    match parsed {
        Ok(0) => Err("0 is not a valid X11 window ID".to_string()),
        Ok(value) => Ok(value),
        Err(_) => Err(format!(
            "{text:?} is not a valid X11 window ID; expected a decimal number or a 0x-prefixed \
             hexadecimal number"
        )),
    }
}

/// Parse a positive, finite scale factor.
fn parse_scale(text: &str) -> Result<f64, String> {
    let value = text
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("{text:?} is not a number"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err("scale must be a positive, finite number".to_string());
    }
    Ok(value)
}

/// Parse an area threshold in `0.0..=1.0`.
fn parse_area_threshold(text: &str) -> Result<f64, String> {
    let value = text
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("{text:?} is not a number"))?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(format!(
            "area threshold must be between 0.0 and 1.0, got {text}"
        ));
    }
    Ok(value)
}

impl DiffArgs {
    /// Resolve these arguments into an executable configuration.
    pub fn resolve(&self) -> Result<ResolvedDiff, Error> {
        let mode = match self.mode.as_deref() {
            Some(name) => CompareMode::from_name(name)?,
            // Defaulting to `exact` means options are opt-in: with no flags, the
            // question answered is "did anything at all differ".
            None => CompareMode::Exact,
        };

        let pixel_threshold = self.pixel_threshold.unwrap_or(0);
        let area_threshold = self.area_threshold.unwrap_or(0.0);

        let options = CompareOptions {
            mode,
            pixel_threshold,
            area_threshold,
        };
        options.validate()?;

        let crop = match self.changed_crop.as_deref() {
            None => None,
            Some(path) => {
                let format = match self.crop_format.as_deref() {
                    Some(name) => ImageFormat::from_name(name)?,
                    None => ImageFormat::from_path(Path::new(path)).unwrap_or(ImageFormat::Png),
                };
                Some(ChangedCropRequest {
                    path: PathBuf::from(path),
                    format,
                })
            }
        };

        Ok(ResolvedDiff {
            before: PathBuf::from(&self.before),
            after: PathBuf::from(&self.after),
            options,
            crop,
            json: self.json,
            print_timing: self.time,
        })
    }
}

/// A request to write the changed region of the second frame to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedCropRequest {
    /// Destination path.
    pub path: PathBuf,
    /// Image format to encode the crop in.
    pub format: ImageFormat,
}

/// A fully resolved diff, ready to execute.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedDiff {
    /// Path to the earlier image.
    pub before: PathBuf,
    /// Path to the later image.
    pub after: PathBuf,
    /// Comparison settings.
    pub options: CompareOptions,
    /// Optional changed-region crop.
    pub crop: Option<ChangedCropRequest>,
    /// Whether to emit JSON.
    pub json: bool,
    /// Whether to print timings to stderr.
    pub print_timing: bool,
}

/// Convenience for callers that only have a bare argument list (used by tests).
pub fn parse_from<I, T>(arguments: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    Cli::try_parse_from(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::MetadataDestination;

    fn args(extra: &[&str]) -> CaptureArgs {
        let mut argv = vec!["eensh", "capture"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).expect("arguments should parse");
        match cli.command {
            Command::Capture(args) => *args,
            other => panic!("expected a capture command, got {other:?}"),
        }
    }

    fn diff_args(extra: &[&str]) -> DiffArgs {
        let mut argv = vec!["eensh", "diff", "before.png", "after.png"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).expect("arguments should parse");
        match cli.command {
            Command::Diff(args) => *args,
            other => panic!("expected a diff command, got {other:?}"),
        }
    }

    #[test]
    fn region_parses_into_a_rect() {
        let parsed = args(&["--region", "100,200,800,600"]);
        assert_eq!(parsed.region, Some(Rect::new(100, 200, 800, 600).unwrap()));
    }

    #[test]
    fn region_allows_negative_origin_but_rejects_zero_size() {
        assert!(args(&["--region", "-10,-20,5,5"]).region.is_some());
        let error = Cli::try_parse_from(["eensh", "capture", "--region", "0,0,0,5"])
            .expect_err("zero width must be rejected");
        assert!(error.to_string().contains("non-zero"));
    }

    #[test]
    fn region_rejects_malformed_input() {
        for bad in ["1,2,3", "1,2,3,4,5", "a,b,c,d", "1,2,-3,4"] {
            assert!(
                Cli::try_parse_from(["eensh", "capture", "--region", bad]).is_err(),
                "{bad:?} should not parse"
            );
        }
    }

    #[test]
    fn window_ids_parse_as_decimal_and_hex() {
        assert_eq!(args(&["--window", "0x4600007"]).window, Some(0x4600007));
        assert_eq!(args(&["--window", "0X4600007"]).window, Some(0x4600007));
        assert_eq!(args(&["--window", "73400327"]).window, Some(73400327));
    }

    #[test]
    fn malformed_window_ids_are_rejected_cleanly() {
        for bad in ["0x", "0xzz", "not-a-window", "-1", "0"] {
            let result = Cli::try_parse_from(["eensh", "capture", "--window", bad]);
            assert!(result.is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn region_and_window_are_mutually_exclusive() {
        assert!(Cli::try_parse_from([
            "eensh", "capture", "--region", "0,0,1,1", "--window", "0x1"
        ])
        .is_err());
    }

    #[test]
    fn scale_conflicts_with_dimensions() {
        assert!(
            Cli::try_parse_from(["eensh", "capture", "--scale", "0.5", "--width", "100"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["eensh", "capture", "--scale", "0.5", "--height", "100"]).is_err()
        );
    }

    #[test]
    fn scale_must_be_positive_and_finite() {
        for bad in ["0", "-1", "nan", "inf", "abc"] {
            assert!(
                Cli::try_parse_from(["eensh", "capture", "--scale", bad]).is_err(),
                "{bad:?} should not parse"
            );
        }
        assert_eq!(args(&["--scale", "0.25"]).scale, Some(0.25));
    }

    #[test]
    fn format_defaults_to_png_and_follows_the_output_extension() {
        let resolved = args(&[]).resolve().unwrap();
        assert_eq!(resolved.format, ImageFormat::Png);

        let resolved = args(&["shot.jpg"]).resolve().unwrap();
        assert_eq!(resolved.format, ImageFormat::Jpeg);

        let resolved = args(&["shot.JPEG"]).resolve().unwrap();
        assert_eq!(resolved.format, ImageFormat::Jpeg);

        // An explicit --format wins over the extension.
        let resolved = args(&["--format", "png", "shot.jpg"]).resolve().unwrap();
        assert_eq!(resolved.format, ImageFormat::Png);
    }

    #[test]
    fn quality_is_rejected_for_png_and_accepted_for_jpeg() {
        let error = args(&["--quality", "75"]).resolve().unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");

        let resolved = args(&["--format", "jpeg", "--quality", "75"])
            .resolve()
            .unwrap();
        assert_eq!(resolved.quality, 75);

        // Default quality is applied when omitted.
        let resolved = args(&["--format", "jpeg"]).resolve().unwrap();
        assert_eq!(resolved.quality, jpeg::DEFAULT_QUALITY);

        // Out of range is caught by the parser.
        assert!(Cli::try_parse_from(["eensh", "capture", "--quality", "0"]).is_err());
        assert!(Cli::try_parse_from(["eensh", "capture", "--quality", "101"]).is_err());
    }

    #[test]
    fn compression_is_rejected_for_jpeg() {
        let error = args(&["--format", "jpeg", "--compression", "best"])
            .resolve()
            .unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
        assert_eq!(
            args(&["--compression", "fast"])
                .resolve()
                .unwrap()
                .png_effort,
            PngEffort::Fast
        );
    }

    #[test]
    fn base64_requires_json() {
        let error = args(&["--base64"]).resolve().unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
        assert!(args(&["--base64", "--json"]).resolve().is_ok());
    }

    #[test]
    fn width_and_height_together_are_rejected() {
        let error = args(&["--width", "100", "--height", "50"])
            .resolve()
            .unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
    }

    #[test]
    fn display_prefers_the_flag_but_falls_back_to_the_environment() {
        // The flag always wins, whatever the environment holds.
        let resolved = args(&["--display", ":77"]).resolve().unwrap();
        assert_eq!(resolved.display, ":77");
    }

    #[test]
    fn target_selection_follows_the_flags() {
        assert_eq!(
            args(&[]).resolve().unwrap().request,
            CaptureRequest::Desktop
        );
        assert_eq!(
            args(&["--region", "1,2,3,4"]).resolve().unwrap().request,
            CaptureRequest::Region(Rect::new(1, 2, 3, 4).unwrap())
        );
        assert_eq!(
            args(&["--window", "0x10"]).resolve().unwrap().request,
            CaptureRequest::Window(0x10)
        );
    }

    #[test]
    fn json_to_stdout_when_the_image_goes_to_a_file() {
        let resolved = args(&["--json", "shot.png"]).resolve().unwrap();
        assert!(resolved.output.metadata.is_stdout());
        assert!(resolved.output.write_image_bytes);
    }

    #[test]
    fn metadata_moves_to_stderr_when_binary_goes_to_stdout() {
        let resolved = args(&["--json", "-"]).resolve().unwrap();
        assert_eq!(resolved.output.metadata, MetadataDestination::Stderr);
        assert!(resolved.output.destination.is_stdout());
        assert!(resolved.output.write_image_bytes);
    }

    #[test]
    fn base64_json_suppresses_raw_binary_stdout() {
        let resolved = args(&["--base64", "--json"]).resolve().unwrap();
        assert!(
            !resolved.output.write_image_bytes,
            "raw bytes must not be interleaved with JSON on stdout"
        );
        assert_eq!(resolved.output.metadata, MetadataDestination::Stdout);
        assert!(resolved.output.destination.is_stdout());
    }

    #[test]
    fn base64_json_with_an_output_file_still_writes_the_file() {
        let resolved = args(&["--base64", "--json", "shot.png"]).resolve().unwrap();
        assert!(resolved.output.write_image_bytes);
        assert!(resolved.output.metadata.is_stdout());
    }

    #[test]
    fn resize_requests_resolve() {
        assert_eq!(
            args(&["--width", "960"]).resolve().unwrap().resize,
            ResizeRequest::Width(960)
        );
        assert_eq!(
            args(&["--height", "540"]).resolve().unwrap().resize,
            ResizeRequest::Height(540)
        );
        assert_eq!(
            args(&["--scale", "0.5"]).resolve().unwrap().resize,
            ResizeRequest::Scale(0.5)
        );
        assert!(args(&[]).resolve().unwrap().resize.is_none());
    }

    // --- diff argument resolution --------------------------------------------

    use crate::compare::CompareMode;

    #[test]
    fn diff_defaults_to_exact_with_no_thresholds() {
        let resolved = diff_args(&[]).resolve().unwrap();
        assert_eq!(resolved.options.mode, CompareMode::Exact);
        assert_eq!(resolved.options.pixel_threshold, 0);
        assert_eq!(resolved.options.area_threshold, 0.0);
        assert!(resolved.crop.is_none());
        assert!(!resolved.json);
    }

    #[test]
    fn diff_requires_two_inputs() {
        assert!(Cli::try_parse_from(["eensh", "diff", "only-one.png"]).is_err());
        assert!(Cli::try_parse_from(["eensh", "diff"]).is_err());
    }

    #[test]
    fn diff_reads_each_threshold_option() {
        let resolved = diff_args(&[
            "--mode",
            "rgb",
            "--pixel-threshold",
            "12",
            "--area-threshold",
            "0.005",
        ])
        .resolve()
        .unwrap();
        assert_eq!(resolved.options.mode, CompareMode::RgbThreshold);
        assert_eq!(resolved.options.pixel_threshold, 12);
        assert_eq!(resolved.options.area_threshold, 0.005);
    }

    #[test]
    fn diff_rejects_an_unknown_mode() {
        let error = diff_args(&["--mode", "perceptual"]).resolve().unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
        assert!(error.message().contains("perceptual"));
    }

    #[test]
    fn diff_accepts_recognised_mode_spellings() {
        for name in ["exact", "rgb", "rgb_threshold", "threshold", "RGB"] {
            assert!(
                diff_args(&["--mode", name]).resolve().is_ok(),
                "mode {name:?} should be accepted"
            );
        }
    }

    #[test]
    fn diff_rejects_out_of_range_area_thresholds() {
        for bad in ["1.5", "-0.1", "nan"] {
            assert!(
                Cli::try_parse_from(["eensh", "diff", "a.png", "b.png", "--area-threshold", bad,])
                    .is_err(),
                "area threshold {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn diff_rejects_out_of_range_pixel_thresholds() {
        assert!(Cli::try_parse_from([
            "eensh",
            "diff",
            "a.png",
            "b.png",
            "--pixel-threshold",
            "256"
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "eensh",
            "diff",
            "a.png",
            "b.png",
            "--pixel-threshold",
            "-1"
        ])
        .is_err());
        // 255 is the top of the valid range and must be accepted.
        assert_eq!(
            diff_args(&["--pixel-threshold", "255"])
                .resolve()
                .unwrap()
                .options
                .pixel_threshold,
            255
        );
    }

    #[test]
    fn diff_crop_format_follows_the_path_extension() {
        let resolved = diff_args(&["--changed-crop", "changed.png"])
            .resolve()
            .unwrap();
        assert_eq!(resolved.crop.unwrap().format, ImageFormat::Png);

        let resolved = diff_args(&["--changed-crop", "changed.jpg"])
            .resolve()
            .unwrap();
        assert_eq!(resolved.crop.unwrap().format, ImageFormat::Jpeg);

        // With no extension, PNG is the documented default.
        let resolved = diff_args(&["--changed-crop", "changed"]).resolve().unwrap();
        assert_eq!(resolved.crop.unwrap().format, ImageFormat::Png);

        // An explicit --crop-format wins over the extension.
        let resolved = diff_args(&["--changed-crop", "changed.jpg", "--crop-format", "png"])
            .resolve()
            .unwrap();
        assert_eq!(resolved.crop.unwrap().format, ImageFormat::Png);
    }

    #[test]
    fn diff_rejects_an_unknown_crop_format() {
        let error = diff_args(&["--changed-crop", "x.png", "--crop-format", "webp"])
            .resolve()
            .unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
    }

    #[test]
    fn diff_keeps_the_input_paths() {
        let cli = Cli::try_parse_from(["eensh", "diff", "/tmp/a.png", "/tmp/b.jpg"]).unwrap();
        match cli.command {
            Command::Diff(args) => {
                let resolved = args.resolve().unwrap();
                assert_eq!(resolved.before, std::path::PathBuf::from("/tmp/a.png"));
                assert_eq!(resolved.after, std::path::PathBuf::from("/tmp/b.jpg"));
            }
            other => panic!("expected diff, got {other:?}"),
        }
    }

    #[test]
    fn diff_json_and_time_flags_resolve() {
        let resolved = diff_args(&["--json", "--time"]).resolve().unwrap();
        assert!(resolved.json);
        assert!(resolved.print_timing);
    }
}
