//! Comparison benchmark and diagnostic.
//!
//! Phase 2 is expected to become part of a repeated observation loop, so the
//! question worth answering is: does comparison itself matter? This binary
//! measures raw comparison cost at representative frame sizes, with identical,
//! sparse, and widespread changes, at each mode.
//!
//! It is a standalone binary rather than a `#[bench]` because the stable Rust
//! toolchain has no built-in benchmark harness, and the purpose is a baseline
//! number rather than a statistically rigorous measurement.
//!
//! ```bash
//! cargo run --release --bin compare_bench
//! cargo run --release --bin compare_bench -- 1920 1080 50
//! ```
//!
//! Output is deliberately machine-readable and separate from capture, resize,
//! encoding, and base64, so that comparison cost is never conflated with them.

use std::time::Instant;

use eensh::compare::{compare_frames, CompareMode, CompareOptions};
use eensh::frame::{Frame, PixelBuffer, PixelFormat};
use eensh::geometry::{CaptureTarget, SourceGeometry};

/// The frame sizes the specification asks about, plus a small one for contrast.
const DEFAULT_SIZES: [(u32, u32); 3] = [(640, 360), (960, 540), (1920, 1080)];

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let sizes: Vec<(u32, u32)> = if arguments.len() >= 2 {
        match (arguments[0].parse::<u32>(), arguments[1].parse::<u32>()) {
            (Ok(w), Ok(h)) => vec![(w, h)],
            _ => {
                eprintln!("usage: compare_bench [WIDTH HEIGHT [ITERATIONS]]");
                std::process::exit(2);
            }
        }
    } else {
        DEFAULT_SIZES.to_vec()
    };

    let iterations: u32 = arguments
        .get(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);

    println!(
        "{:>10}  {:<14}  {:>9}  {:>12}  {:>11}",
        "size", "mode", "scene", "changed_pct", "compare_us"
    );

    for (width, height) in sizes {
        let scenes = [
            ("identical", scene_identical(width, height)),
            ("sparse", scene_sparse(width, height)),
            ("widespread", scene_widespread(width, height)),
        ];

        for (scene_name, (before, after)) in scenes {
            for mode in [CompareMode::Exact, CompareMode::RgbThreshold] {
                let options = CompareOptions {
                    mode,
                    pixel_threshold: 12,
                    area_threshold: 0.0,
                };

                // Warm up once so that the first-touch page faults do not land in
                // the measurement.
                let comparison = compare_frames(&before, &after, &options).unwrap();

                let mut total = std::time::Duration::ZERO;
                for _ in 0..iterations {
                    let started = Instant::now();
                    let result = compare_frames(&before, &after, &options).unwrap();
                    total += started.elapsed();
                    // Defeat any attempt to hoist the call out of the loop.
                    std::hint::black_box(&result);
                }
                let mean_us = total.as_micros() as f64 / iterations as f64;

                println!(
                    "{:>10}  {:<14}  {:>9}  {:>12.4}  {:>11.1}",
                    format!("{width}x{height}"),
                    mode.name(),
                    scene_name,
                    comparison.changed_fraction * 100.0,
                    mean_us,
                );
            }
        }
    }
}

/// Build a frame from a per-pixel function.
fn frame(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Frame {
    let mut data = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(&pixel(x, y));
        }
    }
    let pixels = PixelBuffer::new(width, height, PixelFormat::Rgb8, data).unwrap();
    Frame::new(
        SourceGeometry {
            target: CaptureTarget::Desktop,
            display: None,
            x: 0,
            y: 0,
            width,
            height,
        },
        pixels,
        Instant::now(),
    )
}

/// A structured background so the comparison is not defeating itself with
/// long runs of identical bytes.
fn background(x: u32, y: u32) -> [u8; 3] {
    [
        ((x * 3) % 256) as u8,
        ((y * 5) % 256) as u8,
        (((x + y) * 7) % 256) as u8,
    ]
}

/// Two identical frames.
fn scene_identical(width: u32, height: u32) -> (Frame, Frame) {
    (
        frame(width, height, background),
        frame(width, height, background),
    )
}

/// A small changed region: roughly 0.1% of the frame.
fn scene_sparse(width: u32, height: u32) -> (Frame, Frame) {
    let before = frame(width, height, background);
    // A band a few pixels tall across a narrow column.
    let band_width = (width / 40).max(4);
    let band_height = (height / 40).max(4);
    let after = frame(width, height, |x, y| {
        if x < band_width && y < band_height {
            [255, 255, 255]
        } else {
            background(x, y)
        }
    });
    (before, after)
}

/// A large changed region: half the frame, in a checkerboard so that no part of
/// the scan can be skipped.
fn scene_widespread(width: u32, height: u32) -> (Frame, Frame) {
    let before = frame(width, height, background);
    let after = frame(width, height, |x, y| {
        if (x / 8 + y / 8) % 2 == 0 {
            [0, 0, 0]
        } else {
            background(x, y)
        }
    });
    (before, after)
}
