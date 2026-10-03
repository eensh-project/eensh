//! `eensh` command line entry point.

use std::process::ExitCode;

use clap::Parser;

use eensh::cli::{Cli, Command, ObservationKind};
use eensh::error::Error;
use eensh::observe::pipeline as observe_pipeline;
use eensh::observe::SystemClock;
use eensh::output::{file, MetadataDestination};
use eensh::{diff, pipeline};

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
        Command::Diff(args) => run_diff(&args),
        Command::WaitChange(args) => run_observation(ObservationKind::WaitChange, &args),
        Command::WaitStable(args) => run_observation(ObservationKind::WaitStable, &args),
        Command::Observe(args) => run_observation(ObservationKind::Observe, &args),
        Command::Serve(args) => eensh::service::client::run_serve(&args),
        Command::Ping(args) => eensh::service::client::run_ping(&args),
        Command::Session(args) => eensh::service::client::run_session(&args.command),
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

/// Resolve and execute a diff, reporting failures in the requested format.
///
/// A visual difference is *not* an operational failure: the exit status reports
/// whether the comparison ran, and whether the images differ is communicated
/// through the output. That keeps `diff` unambiguous alongside the stable Phase 1
/// exit codes, where a nonzero status always means an error.
fn run_diff(args: &eensh::cli::DiffArgs) -> i32 {
    let config = match args.resolve() {
        Ok(config) => config,
        Err(error) => return report(&error, args.json),
    };

    match diff::run(&config) {
        Ok(outcome) => {
            if config.json {
                match outcome.response.to_json_string() {
                    Ok(text) => {
                        if let Err(error) = file::write_stdout_text(&text) {
                            return report(&error, false);
                        }
                    }
                    Err(error) => return report(&error, false),
                }
            } else {
                file::write_stderr(&diff::summary(&outcome.response.comparison));
            }

            if config.print_timing {
                let timing = &outcome.response.timing;
                file::write_stderr(&format!(
                    "eensh diff timing: load={}us compare={}us crop={}us total={}us",
                    timing.load_us, timing.compare_us, timing.crop_us, timing.total_us,
                ));
            }
            0
        }
        Err(error) => report(&error, args.json),
    }
}

/// Resolve and execute one of the three temporal observation commands.
///
/// A timeout is not an error: the observation ran and the requested visual
/// condition simply did not occur. It is reported in the JSON `result` and gets
/// the dedicated timeout exit status, which is kept distinct from every error
/// code so that a caller can tell "nothing happened" from "capture broke"
/// without parsing anything.
fn run_observation(kind: ObservationKind, args: &eensh::cli::ObservationArgs) -> i32 {
    let clock = SystemClock::new();

    let outcome = match kind {
        ObservationKind::WaitChange => args.resolve_wait_change().and_then(|config| {
            let print = config.print_timing;
            observe_pipeline::run_wait_change(&config, &clock).map(|o| (o, print))
        }),
        ObservationKind::WaitStable => args.resolve_wait_stable().and_then(|config| {
            let print = config.print_timing;
            observe_pipeline::run_wait_stable(&config, &clock).map(|o| (o, print))
        }),
        ObservationKind::Observe => args.resolve_observe().and_then(|config| {
            let print = config.print_timing;
            observe_pipeline::run_observe(&config, &clock).map(|o| (o, print))
        }),
    };

    match outcome {
        Ok((outcome, print_timing)) => {
            if !args.json {
                // resolve_output requires --json, so this is unreachable in
                // practice; kept so the branch is explicit rather than silent.
                file::write_stderr(&format!("{}", outcome.response.observation.result));
            }

            if print_timing {
                let timing = &outcome.response.timing;
                let observation = &outcome.response.observation;
                file::write_stderr(&format!(
                    "eensh {} timing: elapsed={}ms captures={} comparisons={} capture={}us compare={}us sleep={}us encode={}us",
                    kind,
                    observation.elapsed_ms,
                    timing.captures,
                    timing.comparisons,
                    timing.capture_us_total,
                    timing.compare_us_total,
                    timing.sleep_us_total,
                    timing.encode.encode_us,
                ));
            }

            match outcome.response.observation.result {
                eensh::observe::Outcome::Timeout => EXIT_TIMEOUT,
                _ => 0,
            }
        }
        Err(error) => report(&error, args.json),
    }
}

/// Exit status for a timeout.
///
/// Distinct from every error code (1-16) so that an agent can distinguish
/// "the condition did not occur" from "something failed". The JSON `result`
/// field remains the authoritative statement.
const EXIT_TIMEOUT: i32 = 100;
