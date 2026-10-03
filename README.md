# eensh

Fast, deterministic X11 screenshot capture and frame comparison for software
agents.

`eensh` captures a desktop, a rectangular region, or a single X11 window and
returns an image that an agent can consume directly: PNG or JPEG bytes, an
optional base64 payload, and a stable JSON document that states exactly where the
pixels came from, how big the returned image is, and how to map image coordinates
back to screen coordinates. It can also compare two frames and report precisely
what changed and where.

It is designed to replace `scrot`-style capture in agent tooling, especially on
Xvfb-backed desktops, without the usual ambiguity about coordinates.

```bash
eensh capture \
  --display :99 \
  --width 960 \
  --format jpeg \
  --quality 75 \
  --base64 \
  --json
```

```json
{
  "source": { "kind": "desktop", "display": ":99", "x": 0, "y": 0, "width": 1920, "height": 1080 },
  "image": {
    "width": 960, "height": 540,
    "media_type": "image/jpeg", "format": "jpeg", "byte_length": 14907,
    "encoding": "base64", "quality": 75,
    "data": "/9j/4AAQSkZJRgABAQAAAQABAAD..."
  },
  "transform": { "origin": "top-left", "offset_x": 0, "offset_y": 0, "scale_x": 2.0, "scale_y": 2.0 },
  "timing": { "capture_us": 910, "resize_us": 620, "encode_us": 3870, "base64_us": 330, "total_us": 5730 }
}
```

## Why another screenshot tool?

Three things are unusually painful when a *program* takes screenshots:

1. **Coordinate ambiguity.** A resized screenshot has two different widths: the
   screen's and the image's. Tools tend to report one and leave the caller to
   guess. `eensh` keeps them in separate fields and always ships an explicit
   transform.
2. **Presentation bolted onto capture.** If the capture path writes a file, that
   path cannot later support frame comparison, multiple crops from one capture,
   or a persistent service. `eensh` captures into a raw in-memory frame and
   encodes afterwards.
3. **Error classification.** "Something went wrong" is not actionable. `eensh`
   returns a stable error code and a stable exit status for each failure class,
   so an agent can decide whether to retry, fix the region, or give up.

## Installation

```bash
cargo build --release
install -m 755 target/release/eensh /usr/local/bin/eensh
```

Requirements:

* Rust 1.74 or newer to build;
* a Linux host with X11 (X.Org or Xvfb) at runtime;
* `libX11` present at runtime. It is loaded dynamically, so **X11 development
  headers are not needed to build**.

Both image encoders are pure Rust, so there are no native image dependencies
either. The dependency graph is identical on every platform.

## Usage

There are two commands:

```text
eensh capture [TARGET OPTIONS] [IMAGE OPTIONS] [OUTPUT OPTIONS]
eensh diff BEFORE AFTER [COMPARISON OPTIONS]
```

### Capturing

### Targets

| Option | Meaning |
|---|---|
| *(none)* | Capture the whole root window / display. |
| `--region X,Y,WIDTH,HEIGHT` | Capture a rectangle in source-desktop pixels. |
| `--window WINDOW_ID` | Capture an X11 window by ID (decimal or `0x…` hex). |

`--region` and `--window` are mutually exclusive.

```bash
eensh capture --display :99
eensh capture --display :99 --region 100,200,800,600 crop.png
eensh capture --display :99 --window 0x4600007 --format jpeg --quality 75 game.jpg
```

### Display selection

`--display` wins if given. Otherwise `DISPLAY` is used. If neither is available
the command fails with `invalid_arguments`. Xvfb needs no special handling: it is
just another X11 display.

### Image options

| Option | Meaning |
|---|---|
| `--format png\|jpeg` | Output format. Defaults to `png`, or is inferred from the output file extension. |
| `--quality N` | JPEG quality, 1–100. Default `80`. JPEG output only. |
| `--compression fast\|default\|best` | PNG effort. Default `default`. PNG output only. |
| `--width N` | Resize to a width, preserving the aspect ratio. |
| `--height N` | Resize to a height, preserving the aspect ratio. |
| `--scale F` | Resize by a uniform factor. |

`--width`, `--height`, and `--scale` are mutually exclusive. Phase 1 only does
proportional resizing, so combining `--width` and `--height` is rejected rather
than silently distorting the image.

### Output options

| Option | Meaning |
|---|---|
| `OUTPUT` (positional) | Output path. `-` or omitted means stdout. |
| `--base64` | Embed the encoded image in the JSON response. Requires `--json`. |
| `--json` | Emit the JSON response. |
| `--time` | Print per-stage timings to stderr. |

`--time` writes to stderr, which is also where JSON metadata goes when binary
output owns stdout. Rather than corrupt that stream, `eensh` notes the
suppression and leaves the timings in the JSON response, which already contains
them.

```bash
eensh capture screenshot.png                       # PNG to a file
eensh capture --format jpeg screenshot.jpg         # JPEG to a file
eensh capture --format png - | some-consumer       # PNG on stdout
eensh capture --base64 --json                      # agent-facing JSON on stdout
```

## Where each stream goes

Image bytes and JSON are never interleaved. The rules are:

| `--json` | `--base64` | output path | stdout | JSON metadata |
|---|---|---|---|---|
| no | no | any | image bytes | — |
| yes | no | file | JSON | stdout |
| yes | no | `-` | image bytes | **stderr** |
| yes | yes | file | JSON | stdout |
| yes | yes | `-` or omitted | JSON (includes the image) | stdout |

Only in the last row does the JSON carry the image, so only there are raw bytes
suppressed. That is the agent-facing default: one self-describing document on
stdout.

`--base64` without `--json` is rejected, because there would be no document to
carry the payload.

## Coordinate transform

`transform` maps image coordinates back to source-desktop coordinates:

```text
source_x = offset_x + image_x * scale_x
source_y = offset_y + image_y * scale_y
```

`offset_*` is the source position of image pixel `(0, 0)`; `scale_*` is the number
of source pixels per image pixel. `origin` is always `top-left`: `x` increases to
the right, `y` increases downwards.

Worked example — a `640x480` region at `(640, 200)`, resized to `320x240`:

```json
{ "origin": "top-left", "offset_x": 640, "offset_y": 200, "scale_x": 2.0, "scale_y": 2.0 }
```

Image pixel `(100, 50)` is therefore source pixel `(840, 300)`.

## Error model

Every failure returns a nonzero exit status, a concise message on stderr, and —
with `--json` — a structured error:

```json
{ "error": { "code": "display_unavailable", "message": "unable to connect to X11 display :99: ..." } }
```

| Exit | Code | Meaning |
|---|---|---|
| 1 | `internal_error` | Unexpected internal failure. |
| 2 | `invalid_arguments` | Unparsable or contradictory arguments. |
| 3 | `display_unavailable` | The X11 display could not be opened. |
| 4 | `invalid_region` | Region is malformed, zero sized, or out of bounds. |
| 5 | `window_not_found` | The requested window does not exist. |
| 6 | `capture_failed` | The backend could not produce a frame. |
| 7 | `resize_failed` | The requested resize could not be performed. |
| 8 | `encode_failed` | PNG/JPEG encoding failed. |
| 9 | `output_failed` | Writing the result failed. |
| 10 | `incompatible_frames` | The two frames cannot be compared. |
| 11 | `comparison_failed` | The comparison could not be performed. |
| 12 | `image_load_failed` | An input image could not be read or decoded. |

Exit statuses are stable and are part of the interface.

For `diff`, a *visual difference is not an error*: a successful comparison exits
`0` whether or not the images differ. Whether they differ is reported in the
output, so the exit status is never ambiguous with a real failure.

## Comparing two frames

```bash
eensh diff before.png after.png --json
```

```json
{
  "before": { "source": { "kind": "file", "path": "before.png", "x": 0, "y": 0, "width": 800, "height": 600 },
              "width": 800, "height": 600 },
  "after":  { "source": { "kind": "file", "path": "after.png",  "x": 0, "y": 0, "width": 800, "height": 600 },
              "width": 800, "height": 600 },
  "comparison": {
    "mode": "exact", "pixel_threshold": 0, "area_threshold": 0.0,
    "changed": true,
    "changed_pixels": 5000,
    "total_pixels": 480000,
    "changed_fraction": 0.010416666666666666,
    "bounding_box": { "x": 200, "y": 150, "width": 100, "height": 50 }
  },
  "timing": { "load_us": 3611, "compare_us": 347, "crop_us": 832, "total_us": 4791 }
}
```

| Option | Meaning |
|---|---|
| `--mode exact\|rgb` | `exact` counts any channel difference; `rgb` tolerates `--pixel-threshold` |
| `--pixel-threshold N` | Largest per-channel difference ignored, 0–255. Default `0` |
| `--area-threshold F` | Smallest changed fraction that counts as meaningful, 0.0–1.0. Default `0.0` |
| `--changed-crop PATH` | Write a crop of the changed region from the *second* image |
| `--crop-format png\|jpeg` | Crop format; inferred from the path extension |
| `--json` | Emit the JSON response instead of a one-line summary |

Without `--json`, a one-line summary goes to stderr, leaving stdout empty:

```text
rgb_threshold: 5000/480000 pixels changed (1.0417%), changed region 100x50+200+150
```

### The comparison rules

These boundaries are exact and tested, because a threshold whose edge is
unspecified is worse than no threshold at all.

```text
difference = max(|r1 - r2|, |g1 - g2|, |b1 - b2|)

pixel changed    iff  difference >  pixel_threshold     (strict)
frame changed    iff  changed_pixels > 0
                      and changed_fraction >= area_threshold   (inclusive)
```

The largest channel difference is used rather than a Euclidean distance, because
it is cheaper, deterministic, and free of the rounding questions that a distance
metric raises at the boundary. No perceptual colour space is involved.

`changed_pixels`, `changed_fraction`, and `bounding_box` are always exact, even
when the area threshold decides the frame is not meaningfully changed. That
distinction is deliberate: a caller can see that something moved even when the
change is too small to act on.

```json
{
  "changed": false,
  "changed_pixels": 1,
  "total_pixels": 480000,
  "changed_fraction": 0.0000020833,
  "bounding_box": { "x": 10, "y": 10, "width": 1, "height": 1 }
}
```

The bounding box is in **frame-local** coordinates. Frames decoded from files have
a top-left origin; frames from a capture carry their source geometry, so a crop
can be mapped back to the desktop with the usual transform.

### Incompatible frames

Frames must have the same pixel dimensions. They are never silently resized and
never silently compared over their overlapping area, because either would make
`changed_fraction` mean something the caller did not ask for:

```json
{ "error": { "code": "incompatible_frames", "message": "incompatible frames: frame dimensions differ: 1920x1080 vs 1280x720" } }
```

### The changed crop follows the bounding box

The crop is written whenever a changed region was located, which is whenever any
pixel exceeded the pixel threshold. The area threshold is **not** consulted: it
expresses a policy judgement about significance, while the bounding box is a
factual statement about where differences were found. The JSON still reports
`changed: false`. When nothing changed, no file is written — `eensh` does not
invent a placeholder image.

### Comparison needs no encoding

`compare_frames` works on raw frames. The `diff` command decodes its inputs at the
boundary because files are what a human hands it, but the engine itself never sees
a PNG or a JPEG. That is what lets Phase 3 call it on live captures at high
frequency. There is a benchmark:

```bash
cargo run --release --bin compare_bench
```

## Semantics worth knowing

**Regions are never clipped silently.** A region that does not fit inside the
display is an `invalid_region` error. If you want a clipped capture, clip it
yourself and ask for the smaller rectangle.

**Window capture reads the visible desktop.** `eensh` captures the root-window
pixels lying under the window's on-screen rectangle. Consequences:

* the returned image is the *visible* representation, so an occluded window shows
  whatever is covering it;
* window decorations are included only to the extent that the window's own
  geometry includes them;
* a window that is not viewable (unmapped or iconified) is a hard error rather
  than a guess;
* a window partly off-screen is reported with its clipped rectangle, so the
  transform stays honest about which source pixels were actually returned.

This is a deliberate choice: reading a redirected window pixmap can return
contents the desktop never showed, and claiming to capture unobscured window
contents when we cannot see them would be worse than documenting the limitation.

**Timing uses a monotonic clock.** Durations are for diagnosing agent observation
latency, not for benchmarking the machine.

**Output files are written atomically.** Bytes are staged in a sibling temporary
file and renamed into place, so a reader never sees a half-written image, and a
failed capture never disturbs an existing file.

**Zero-sized and absurd captures are rejected.** Geometry arithmetic is checked
for overflow, and a capture that would produce no pixels is an error rather than a
successful empty image.

## Performance notes

Stage timings are reported per capture, so measure rather than assume:

```bash
eensh capture --display :99 --time screenshot.png
# eensh timing: capture=910us resize=620us encode=3870us base64=330us total=5730us (14907 bytes)
```

Two things are worth knowing when reading those numbers:

* **Capture cost is dominated by the X server's connection handshake, not by
  pixel count.** A 64x64 region and a full-screen capture cost about the same on
  a given server. On a local X.Org or Xvfb display the handshake is sub-millisecond;
  some Xvfb builds (for example snap-packaged ones) spend tens of milliseconds
  there, which affects every X client equally. `eensh` cannot make the handshake
  faster, and later phases will amortise it with a persistent connection rather
  than by micro-optimising this path.
* **Resize and encode are linear in pixel count** and are reported separately, so
  it is easy to see which stage to attack.
* **Comparison is roughly linear in pixel count too**, and is measured
  separately from the other stages. At 1920×1080 it costs on the order of a
  millisecond, which is small compared to a full capture. Use
  `cargo run --release --bin compare_bench` to measure it on your own hardware.

There is no temporary-file round trip and no external screenshot subprocess on
the base64/JSON path.

## Architecture

Capture and comparison are two flows over one shared abstraction, the raw frame:

```text
capture:                            compare:

X11 / Xvfb                          Frame A ----\
    │                                            +-- compare/ -- Comparison
    ▼                               Frame B ----/
capture backend      capture/       raw Frame + source geometry
    │
    ▼
crop / resize        resize.rs      raw Frame, still uncompressed
    │
    ▼
image encoder        encode/        PNG or JPEG bytes
    │
    ▼
optional base64      output/        text carrying those exact bytes
    │
    ▼
JSON / file / stdout output/        presentation
```

The invariant is that the capture backend produces only a raw `Frame`, and that
comparison consumes only raw frames. Neither knows about PNG, JPEG, base64, JSON,
or the filesystem. That separation is what later phases need: temporal
observation and persistent capture are built from exactly these two primitives.

```text
src/
  main.rs            CLI entry point, error reporting, exit statuses
  lib.rs             crate documentation
  cli.rs             argument parsing and resolution into a concrete plan
  pipeline.rs        capture stage orchestration and timing
  diff.rs            comparison stage orchestration and timing
  capture/
    mod.rs
    display.rs       X11 connection, error trapping, window queries
    x11.rs           direct pixel capture into a Frame
  compare.rs         raw-frame comparison: one pass, no allocations
  input.rs           decoding saved images into frames (the input boundary)
  frame.rs           raw Frame, PixelBuffer, cropping
  geometry.rs        Rect, SourceGeometry, Transform, resize maths
  resize.rs          deterministic box-filter resizing
  encode/
    mod.rs           format selection and dispatch
    png.rs
    jpeg.rs          self-contained baseline JPEG encoder
  output/
    mod.rs           output routing rules
    json.rs          capture and diff response schemas
    base64.rs        base64 of encoded bytes
    file.rs          atomic writes, stdout/stderr
  timing.rs          monotonic stage timers
  error.rs           error classes and exit statuses
  bin/
    compare_bench.rs comparison benchmark and diagnostic
```

### Why a hand-written JPEG encoder?

To keep the build dependency-free on any machine: no `libjpeg`, no `cc`, no
system image libraries. It is a baseline 4:4:4 encoder with the standard Annex K
Huffman tables, which suits screenshots full of small text. PNG uses the
pure-Rust `png` crate.

### Why comparison is a single pass with no mask

A later observation loop may evaluate a comparison dozens of times per second, so
`compare_frames` computes its counts and bounding box in one traversal and
allocates nothing. It does not build a per-pixel change mask, and it does not stop
early when the area threshold is satisfied, because the counts and the bounding
box must be exact regardless. The inner comparison is selected once, at
monomorphisation, rather than per pixel, and a byte-equality fast path skips the
wider arithmetic for the common case of identical pixels.

## Testing

```bash
cargo test                      # unit tests; Xvfb tests skip if unavailable
EENSH_REQUIRE_XVFB=1 cargo test # make a missing Xvfb a failure (use this in CI)
```

The suite covers, among other things:

* geometry: region bounds, crop geometry, resize dimensions, coordinate
  transforms in both directions, invalid rectangles, odd dimensions,
  aspect-ratio preservation;
* encoding: valid output for PNG and JPEG, declared dimensions matching encoded
  dimensions, correct media types, base64 round-tripping to the exact encoded
  bytes, determinism, and JPEG correctness against reference values;
* the JSON schema: desktop, region, window, resized, base64, and error responses;
* Xvfb integration: screen-sized captures, region captures with known painted
  colours verified pixel by pixel, resized captures, unavailable displays,
  window capture by ID, occlusion semantics, and capture of a destroyed window;
* the CLI: exit statuses, stream routing, argument validation, and error format;
* comparison: identical frames, a single changed pixel, changes at known
  corners, exact mode, both threshold boundaries (below, equal, above), geometry
  mismatch, arithmetic at frame sizes too large to allocate, bounding-box shape
  for every arrangement, and determinism across repeated runs;
* comparison over Xvfb: the full `X11 → Frame → compare` path with no encoding,
  including a change ignored by the pixel threshold and a change suppressed by
  the area threshold while its bounding box is still reported;
* the diff CLI: PNG and JPEG inputs, the JSON schema, both thresholds from the
  command line, the changed crop, and the rule that a visual difference is not an
  error exit status.

Integration tests start their own `Xvfb` and draw known colours onto it, then
verify the captured pixels at the coordinates they were drawn at. That is what
makes them meaningful rather than smoke tests.

## Scope

Implemented:

* **Phase 1** — capture a desktop, region, or window; PNG and JPEG; resizing;
  base64; structured JSON with an explicit coordinate transform; timing.
* **Phase 2** — raw-frame comparison with exact and thresholded RGB modes, pixel
  and area thresholds, changed-pixel counts, changed fraction, and a bounding box;
  plus a `diff` command for comparing saved images.

Deliberately **not** implemented, and reserved for later phases: input injection,
window management, desktop lifecycle, `wait-change`, `wait-stable`, `observe`,
polling loops, temporal observation, persistent sessions, frame IDs, frame
history, ignore masks, named regions, connected-component segmentation, tile
summaries, perceptual hashes, optical flow, payload-budget logic, OCR, template
matching, and Wayland support. See `specs/` for the roadmap.
