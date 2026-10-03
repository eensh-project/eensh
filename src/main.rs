//! `eensh` command line entry point.

use std::process::ExitCode;

use clap::Parser;

use eensh::cli::{Cli, Command};
use eensh::error::Error;
use eensh::output::{file, MetadataDestination};
use eensh::pipeline;

fn main() -> ExitCode {
    let code = run();
    ExitCode::from(code as u8)
}

fn run() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // Clap handles --help and --version itself. For real usage errors we
            // still want a concise message on stderr.
            let _ = error.print();
            return if error.use_stderr() { 2 } else { 0 };
        }
    };

    match cli.command {
        Command::Capture(args) => run_capture(&args),
    }
}

/// Resolve and execute a capture, reporting failures in the requested format.
fn run_capture(args: &eensh::cli::CaptureArgs) -> i32 {
    // Resolution failures happen before we know whether JSON was requested, but
    // `--json` may still be set on the raw arguments, so honour it.
    let json_requested = args.json;

    let config = match args.resolve() {
        Ok(config) => config,
        Err(error) => return report(&error, json_requested),
    };

    match pipeline::run(&config) {
        Ok(outcome) => {
            if config.print_timing {
                // JSON metadata may already be routed to stderr; mixing raw text
                // into that stream would corrupt it. In that case the timings are
                // already in the JSON response and printing them again would be
                // redundant as well as harmful.
                if config.output.metadata == MetadataDestination::Stderr {
                    file::write_stderr(
                        "eensh: --time output suppressed because JSON metadata is already \
                         using stderr; the timings are in the JSON response instead",
                    );
                } else {
                    let timing = &outcome.response.timing;
                    file::write_stderr(&format!(
                        "eensh timing: capture={}us resize={}us encode={}us base64={}us total={}us ({} bytes)",
                        timing.capture_us,
                        timing.resize_us,
                        timing.encode_us,
                        timing.base64_us,
                        timing.total_us,
                        outcome.encoded.len(),
                    ));
                }
            }
            0
        }
        Err(error) => report(&error, json_requested),
    }
}

/// Emit an error in the requested format and return its exit status.
fn report(error: &Error, json: bool) -> i32 {
    if json {
        let text = match serde_json::to_string(&error.to_json()) {
            Ok(text) => text,
            Err(_) => error.message(),
        };
        file::write_stderr(&text);
    } else {
        file::write_stderr(&error.message());
    }
    error.exit_code()
}
