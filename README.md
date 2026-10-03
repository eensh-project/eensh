# eensh

Fast, deterministic X11 screenshot capture, frame comparison, and temporal
observation for software agents.

`eensh` captures a desktop, a rectangular region, or a single X11 window and
returns an image that an agent can consume directly: PNG or JPEG bytes, an
optional base64 payload, and a stable JSON document that states exactly where the
pixels came from, how big the returned image is, and how to map image coordinates
back to screen coordinates. It compares two frames to say precisely what changed
and where. And it watches a target over time, so an agent can replace
`sleep(arbitrary)` with "wait until the screen settles and tell me what it looks
like".

It is designed to replace `scrot`-style capture in agent tooling, especially on
Xvfb-backed desktops, without the usual ambiguity about coordinates — or about
when a screenshot is worth taking.

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

Four things are unusually painful when a *program* takes screenshots:

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
4. **Timing.** `sleep(2)` and hope is the standard way to wait for a UI, and it is
   wrong in both directions. `eensh observe` waits for the transition and then for
   the result to stop moving, and reports how long that took.

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

There are five standalone commands, plus the service and its client:

```text
eensh capture     [TARGET OPTIONS] [IMAGE OPTIONS] [OUTPUT OPTIONS]
eensh diff        BEFORE AFTER [COMPARISON OPTIONS]
eensh wait-change [TARGET OPTIONS] [IMAGE OPTIONS] [OBSERVATION OPTIONS]
eensh wait-stable [TARGET OPTIONS] [IMAGE OPTIONS] [OBSERVATION OPTIONS]
eensh observe     [TARGET OPTIONS] [IMAGE OPTIONS] [OBSERVATION OPTIONS]

eensh serve       [--socket PATH]
eensh ping        [--socket PATH] [--json]
eensh session     create|list|info|close|capture|latest|frame|diff
                  wait-change|wait-stable|observe   [--socket PATH] [--json]
```

The five standalone commands each open a display, do their work, and exit. The
session commands talk to a running `eensh serve`, which holds the display open
between calls. See [Persistent sessions](#persistent-sessions).

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
| 13 | `geometry_changed` | The observed target changed shape or moved. |
| 14 | `target_lost` | The observed target disappeared. |
| 15 | `invalid_duration` | A duration was zero, negative, or unparsable. |
| 16 | `observation_failed` | The observation could not be performed. |
| 17 | `session_not_found` | The session is not registered — closed, or the service restarted. |
| 18 | `session_busy` | The session is running an observation; a second one, or a close, was refused. |
| 19 | `session_closed` | The session has been closed. |
| 20 | `frame_not_available` | The requested frame ID is not retained (or was never captured). |
| 21 | `no_frame_available` | The session has not captured any frame yet. |
| 22 | `service_unavailable` | No `eensh serve` could be reached at the socket path. |
| 23 | `service_protocol_error` | The service could not decode a request, or a protocol version was refused. |
| 24 | `service_overloaded` | The service refused work to preserve freshness. |
| 100 | *(none)* | **Timeout**: the observation ran, the condition did not occur. |

Exit statuses are stable and are part of the interface. A service error crosses
the process boundary and is mapped back to the same status it would have from the
standalone path, so the table above stays the single source of truth: a
`frame_not_available` from the service exits `20`, exactly as the standalone
equivalent would.

For `diff`, a *visual difference is not an error*: a successful comparison exits
`0` whether or not the images differ. For observation, a *timeout is not an
error* either: it exits `100`, outside the error range, so the two can never be
confused. In both cases the JSON carries the authoritative result.

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

## Temporal observation

Capturing and comparing are enough to answer "what does the screen look like" and
"what changed". They are not enough to answer the question an agent actually has
after it clicks something: **is it done yet?**

```text
execute input
sleep(arbitrary)        <-- wrong in both directions
capture
```

`eensh observe` replaces the guess:

```bash
eensh observe --display :99 --stable-for 300ms --timeout 10s --json --base64
```

```text
capture baseline
    -> compare each sample against the baseline until it changes
    -> then compare consecutive samples until they stop differing
    -> return the settled frame
```

| Command | Question it answers |
|---|---|
| `wait-change` | Has the visible state changed? |
| `wait-stable` | Has the visible state stopped changing? |
| `observe` | What is the resulting settled state? |

### The one distinction that matters

The three commands differ in exactly one way, and it is not cosmetic:

```text
wait-change : compare every frame against a FIXED BASELINE
wait-stable : compare CONSECUTIVE frames
observe     : fixed baseline until a change, then consecutive frames
```

A fixed baseline is what lets `wait-change` notice a **gradual** transition whose
every individual step is below the area threshold. Consecutive comparison is what
lets `wait-stable` tell whether the scene is *currently still moving*. Using the
wrong one for either job gives a plausible but wrong answer.

`observe` deliberately does not return on the first changed frame. That frame is
usually a half-drawn menu, an animation step, or an incomplete layout. The point
is the state the transition *settles into*.

### Options

| Option | Default | Meaning |
|---|---|---|
| `--mode exact\|rgb` | `rgb` | Comparison mode for change detection |
| `--pixel-threshold N` | `12` | Largest per-channel difference treated as unchanged |
| `--area-threshold F` | `0.005` | Smallest changed fraction treated as meaningful |
| `--interval D` | `100ms` | Target cadence between samples |
| `--timeout D` | `5s` | Total deadline for the whole operation |
| `--stable-for D` | `300ms` | How long the scene must hold still |
| `--width`, `--height`, `--scale` | none | Resize the **returned** frame |
| `--format`, `--quality`, `--compression` | `png` | Format of the **returned** frame |
| `--base64` | off | Embed the returned frame in the JSON |
| `--json` | required | Observation is agent-facing; the JSON is the result |

Durations take an explicit unit: `100ms`, `300ms`, `1s`, `5s`. A bare `5` is
**rejected** rather than guessed at, because `--timeout 5` is ambiguous between
five seconds and five milliseconds and a silently wrong timeout is worse than a
parse error.

The temporal defaults are **not** the same as `eensh diff`'s, deliberately:

```text
eensh diff        asks  "did anything differ?"        -> exact, no tolerance
eensh wait-change asks  "did anything meaningfully change?" -> rgb, threshold 12
```

### Results

```json
{
  "observation": {
    "kind": "observe",
    "result": "observed",
    "elapsed_ms": 931,
    "captures": 10,
    "comparisons": 9,
    "stable_for_ms": 300,
    "change_detected_ms": 204
  },
  "source": { "kind": "desktop", "display": ":99", "x": 0, "y": 0, "width": 1920, "height": 1080 },
  "transform": { "origin": "top-left", "offset_x": 0, "offset_y": 0, "scale_x": 2.0, "scale_y": 2.0 },
  "image": { "width": 960, "height": 540, "media_type": "image/jpeg", "encoding": "base64", "data": "..." },
  "first_change": { "changed": true, "changed_fraction": 0.032, "bounding_box": { "x": 411, "y": 208, "width": 619, "height": 327 } },
  "comparison": { "changed": false, "changed_fraction": 0.0004, "bounding_box": { "x": 1201, "y": 17, "width": 2, "height": 31 } },
  "timing": {
    "captures": 10,
    "comparisons": 9,
    "capture_us_total": 76420,
    "compare_us_total": 1520,
    "sleep_us_total": 853000,
    "encode": { "resize_us": 0, "encode_us": 13300, "base64_us": 90 }
  }
}
```

`observation.result` is the authoritative statement of what happened:

| `result` | Meaning |
|---|---|
| `changed` | `wait-change`: the target departed from the baseline |
| `stable` | `wait-stable`: the target held still for `stable_for` |
| `observed` | `observe`: a transition happened and settled |
| `timeout` | The deadline passed before the condition occurred |

**A timeout is an outcome, not an error.** The observation ran; the visual
condition simply did not occur in time. It gets exit status `100` — outside the
range of every error code — and the JSON always carries the latest frame and the
latest comparison, so an agent can inspect the current state after a transition
that failed to happen.

`comparison` always reports the exact numbers even when the area threshold decided
the change was not meaningful, so a caller can see that *something* moved even
when it was too small to act on.

### Counting and timing

Two terms are used precisely:

```text
captures    = number of frames captured
comparisons = number of frame pairs compared = captures - 1
```

The timing section is there to answer three questions without a profiler:

```text
Are we slow because capture is slow?     -> capture_us_total
Are we slow because comparison is slow?  -> compare_us_total
Are we mostly sleeping between polls?    -> sleep_us_total
```

`capture_us_total` is often the dominant term, and this is expected: each sample
opens its own X11 connection. That is a deliberate Phase 3 simplification —
Phase 4 exists to amortise it without changing any of these semantics. The numbers
are reported rather than hidden so the cost stays visible.

### Semantics worth knowing

**Comparison runs at native resolution.** `--width 960` resizes what is
*returned*, never what is *compared*. Change detection must not depend on the
output format, so `comparison.total_pixels` always reflects the captured frame.

**Only the final frame is ever encoded.** Sampling encodes nothing; the single
`prepare_image` call runs after the state machine has finished.

**The target must not drift.** A temporal observer is watching *the same thing
over time*, so the effective source geometry is checked on every sample. A window
that is resized or moved mid-observation fails with `geometry_changed` rather
than silently comparing pixel grids that no longer mean the same coordinates:

```json
{ "error": { "code": "geometry_changed", "message": "geometry changed: observed target changed from 1280x720 at (0,0) to 1920x1080 at (0,0)" } }
```

**A scene that settles back to its baseline still completes.** A popup that opens
and closes, or a button flash, is a real transition that settled. `observe` does
not require the final frame to differ from the baseline.

**Further changes while settling do not restart the search.** Once the target has
departed from the baseline, the operation is watching that transition; it never
returns to waiting for a change. Each further change merely resets the stability
timer.

**Polling does not accumulate drift.** Samples are scheduled against a fixed
origin, not against the end of the previous sample. A slow capture does not push
every subsequent sample later; missed opportunities are skipped rather than
queued, so the observer always works from fresh frames.

**Capture failures abort the observation** with the underlying structured error.
Phase 3 does not retry; a transient-failure policy belongs with the persistent
session in Phase 4.

### Observation output routing

Observation uses a simpler rule than `capture`, because the JSON *is* the result
rather than an optional annotation:

```text
JSON          -> always stdout
frame         -> a file, or embedded in the JSON with --base64
raw binary    -> never stdout
```

`capture` can put binary on stdout and metadata on stderr because its primary
product is the image. An observation's primary product is the observation, so
burying it on stderr while binary lands on stdout would be backwards. `-` is
accepted and means "JSON to stdout", which is already the default.

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

An observation's cost is reported the same way, broken down by stage:

```bash
eensh wait-stable --display :99 --stable-for 300ms --time --json
# eensh wait_stable timing: elapsed=301ms captures=4 comparisons=3 \
#   capture=7810us compare=467us sleep=293275us encode=2518us
```

Read that as: 4 captures cost 7.8 ms in total, comparison cost 0.5 ms for the whole
run, and 293 ms was deliberate waiting for the requested stability. Comparison is
not the bottleneck; the cadence is.

## Architecture

Four flows over one shared abstraction, the raw frame:

```text
capture:                            compare:                 observe:

X11 / Xvfb                          Frame A ----\            FrameSource
    │                                            +-- compare      │
    ▼                               Frame B ----/   /            ▼
capture backend      capture/       raw Frame + source geometry  compare_frames
    │                                                             │
    ▼                                                             ▼
crop / resize        resize.rs      raw Frame, still uncompressed  state machine
    │                                                             │
    ▼                                                             ▼
image encoder        encode/        PNG or JPEG bytes           one final frame
    │                                                             │
    ▼                                                             ▼
optional base64      output/        text carrying those exact bytes
    │
    ▼
JSON / file / stdout output/        presentation
```

Phase 4 adds a persistent session without adding a fifth flow. A session is a
`FrameSource` like any other, so the observation state machine is *unchanged* and
reused verbatim:

```text
client / CLI
     │  local Unix socket, length-delimited JSON
     ▼
service (accept loop, one thread per connection)
     ▼
session manager  ── one CaptureSession per target ──┐
     ▼                                             │
persistent X11 connection, frame IDs, bounded raw history
     ▼                                             │
raw Frame ────────────────────────────────────────┘
     ▼
the existing compare / observe machinery, unchanged
```

The invariants are that the capture backend produces only a raw `Frame`, that
comparison consumes only raw frames, and that the temporal state machine consumes
only raw frames. None of them knows about PNG, JPEG, base64, JSON, sessions, or
the filesystem. That separation is what makes the same comparison primitive usable
on live captures, on decoded files, inside a high-frequency observation loop, and
across a process boundary.

```text
src/
  main.rs            CLI entry point, error reporting, exit statuses
  lib.rs             crate documentation
  cli.rs             argument parsing and resolution into a concrete plan
  pipeline.rs        capture stage orchestration, and the shared image path
  diff.rs            comparison stage orchestration and timing
  capture/
    mod.rs
    display.rs       X11 connection, error trapping, window queries
    x11.rs           direct pixel capture into a Frame
  compare.rs         raw-frame comparison: one pass, no allocations
  observe/
    mod.rs           temporal state machines and result types
    clock.rs         Clock trait, SystemClock, ManualClock
    pipeline.rs      observation orchestration and the single final encode
  session/
    mod.rs           CaptureSession: display, frame sequence, state machine
    history.rs       FrameId, SessionFrame, bounded frame history
    manager.rs       session registry, and the FrameSource adapter
    pipeline.rs      session-scoped capture, diff, and observation
  service/
    protocol.rs      wire types, framing, version check
    handler.rs       request dispatch; one response per request, always
    unix.rs          socket binding, permissions, accept loop
    client.rs        serve, ping, and the session CLI as a service client
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
    json.rs          capture, diff, and observation response schemas
    base64.rs        base64 of encoded bytes
    file.rs          atomic writes, stdout/stderr
  timing.rs          monotonic stage timers
  error.rs           error classes and exit statuses
  bin/
    compare_bench.rs comparison benchmark and diagnostic
```

### Why the accept loop uses `poll` rather than a sleeping loop

A blocking `accept` cannot be interrupted into noticing a shutdown flag: the
standard library retries it across `EINTR`, so a signal handler that only sets a
flag leaves the loop parked and the process appears hung when asked to stop.

The obvious workaround — a non-blocking listener retried in a loop with a short
sleep between attempts — is worse than the problem, and measurably so. Every
incoming connection then waits up to the whole sleep interval just to be
accepted, which turned a trivially cheap request into a multi-millisecond one and
made the persistent path *slower* than starting a fresh process. That is the exact
opposite of the point of Phase 4, and it is why the measurement is a test rather
than a note.

`poll` blocks until either a connection arrives or the timeout elapses, so a
connection is accepted immediately while shutdown is still noticed inside one
(much longer) interval. The wakeup cost falls on an idle service, where it does
not matter, instead of on every request.

### Why connections are handled on their own threads

This is correctness, not throughput. A single-threaded loop makes one long
observation block *every* other session, because the next connection's request
cannot even be read until the observation finishes. That is a global lock in
effect, and it would make the documented `session_busy` refusal unreachable: a
second observation would be silently queued behind the first instead of being
told to wait.

Concurrency is safe because the locking is already per-session. Two connections
touching different sessions never contend, and two touching the same session
serialize on that session's own mutex. No lock is taken across sessions.

### Why a hand-written JPEG encoder?

To keep the build dependency-free on any machine: no `libjpeg`, no `cc`, no
system image libraries. It is a baseline 4:4:4 encoder with the standard Annex K
Huffman tables, which suits screenshots full of small text. PNG uses the
pure-Rust `png` crate.

### Why comparison is a single pass with no mask

A temporal observation loop may evaluate a comparison dozens of times per second,
so `compare_frames` computes its counts and bounding box in one traversal and
allocates nothing. It does not build a per-pixel change mask, and it does not stop
early when the area threshold is satisfied, because the counts and the bounding
box must be exact regardless. The inner comparison is selected once, at
monomorphisation, rather than per pixel, and a byte-equality fast path skips the
wider arithmetic for the common case of identical pixels.

### Why observation has a `Clock` trait

Temporal logic is a state machine over time, and a state machine tested against
the real clock can only be tested slowly and unreliably. `observe/clock.rs`
defines a two-method `Clock`, with a `SystemClock` for the CLI and a `ManualClock`
whose time only advances when a test says so. Every state-machine scenario —
thirty polls, a timeout, a stability window that must *not* complete — runs in
microseconds with exact timing assertions instead of approximated ones. Only two
real-time tests exist, with generous margins, so the system clock path is
exercised at least once.

### Why one engine, not three loops

`wait-change`, `wait-stable`, and `observe` share capture scheduling, timeout
handling, target-consistency checking, and timing accumulation. They differ in
one thing: `wait-change` compares against a fixed baseline, `wait-stable`
compares consecutive frames, and `observe` switches from the first to the second.
That difference is stated explicitly at each call site in `observe/mod.rs` rather
than hidden behind shared abstraction, because getting it backwards produces a
plausible but wrong answer.

## Persistent sessions

A session holds a display open between calls, so repeated captures do not each pay
for an X11 connection handshake. It also keeps a bounded history of recent raw
frames, which makes retrieval and comparison local memory operations.

The service is **never started automatically**. Explicit lifecycle is easier to
reason about, and hiding a daemon behind an ordinary capture command is exactly the
kind of surprise this tool should not introduce.

```bash
eensh serve &                                   # listens on $EENSH_SOCKET, or a default path
eensh ping --json                               # {"protocol_version":1,...}

SESSION=$(eensh session create --display :99 --json | sed 's/.*"session_id":"\([^"]*\)".*/\1/')

eensh session capture $SESSION --json --base64       # frame 1
eensh session capture $SESSION --json                # frame 2
eensh session latest  $SESSION --json                # frame 2, no capture
eensh session frame   $SESSION 1 --json              # frame 1, no capture
eensh session diff    $SESSION 1 2 --json            # compare two retained frames
eensh session observe $SESSION --json --base64       # observe through the session

eensh session info    $SESSION --json
eensh session list    --json
eensh session close   $SESSION --json
```

The socket lives at `$EENSH_SOCKET`, then `$XDG_RUNTIME_DIR/eensh.sock`, then a
user-scoped temporary path. It is created `0600` and removed on clean shutdown. A
stale socket left by a crashed service is reclaimed on the next start, and a
pre-existing file that is *not* a socket is refused rather than overwritten.

### Fresh capture versus the latest frame

These are deliberately distinct, and conflating them would make freshness
unpredictable:

| Command | Touches X11 | Returns |
|---|---|---|
| `session capture` | yes | a new frame, with a new ID, appended to history |
| `session latest` | no | the newest frame already retained |
| `session frame ID` | no | a specific retained frame, or `frame_not_available` |

### Frame identity, and what happens when history evicts

Every capture receives a monotonically increasing ID starting at 1. History holds
the most recent `--history N` frames (default 8, maximum 256); older frames are
evicted, and asking for an evicted frame fails explicitly with
`frame_not_available` rather than silently substituting a different one.

An observation *owns* the frames it is using. Its baseline stays valid internally
even after the baseline is evicted from public history, so a long observation
outliving its own baseline still produces the correct answer. The identifiers it
reports are the true ones, and retrieving an evicted one still fails explicitly.

### Concurrency

Within one session, capture is **serialized**, not refused: a capture arriving
during an observation is served at the next sample boundary. Only the three
temporal observations are mutually exclusive — a second one is refused with
`session_busy` rather than queued invisibly, so an agent is told to wait instead
of hanging.

`close` during an observation is refused with `session_busy`, and the session is
left exactly as it was. The alternative — cancelling the observation — would have
to interrupt a running state machine, and an explicit refusal is easier to reason
about than a silent cancellation.

Different sessions never contend. There is no global lock, so an observation in
one session does not block a capture in another.

### What persistence does and does not save

Measured over 20 captures at 640×480 on Xvfb, with the same image options on both
paths:

| Path | Mean | Total |
|---|---|---|
| Standalone (a process and a connection per capture) | 15.7 ms | 313 ms |
| Persistent session | 14.4 ms | 288 ms |

The saving is real but modest, and it is worth being honest about why. Both paths
still pay for a **client process** on every capture, because the CLI is one process
per invocation. The only thing the session removes is the X11 connection setup,
which is a small fraction of a round trip dominated by process start and by the
capture itself. A caller that spoke the protocol directly would avoid the process
start too and see a larger difference.

What persistence does deliver without qualification is that retrieval and
comparison stop touching the display at all:

| Operation | Mean at 640×480 | Touches X11 |
|---|---|---|
| `session diff` | 9.0 ms | no |
| `session latest` | 38.1 ms | no (but still encodes the frame it returns) |
| `session capture` | 46.0 ms | yes |

History trades memory for that speed. A raw frame is three bytes per pixel, so
1920×1080 is about 6.2 MB and a default 8-frame history is about 50 MB per
session. It is bounded by capacity, so the cost is predictable rather than
open-ended.

**No capture buffers are reused.** Frames are retained in history, so each must
own its pixels; a reused buffer would be overwritten by the next capture while
history still referred to it. The Phase 4 saving is connection reuse, and the
allocation saved is the per-capture connection state rather than the pixel buffer.

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
  error exit status;
* temporal state machines, entirely without X11 and without sleeping: fixed-baseline
  versus consecutive comparison, gradual cumulative drift, both threshold
  boundaries, geometry drift and a moved target, capture failure mid-observation,
  a stability window that must not complete early, a single-sample blip resetting
  the timer, returning to baseline, multiple transition bursts, and the timeout
  deadline; plus exact polling-cadence and count assertions;
* observation over Xvfb: a change detected with the correct bounding box, a
  sub-threshold shading ignored, a one-pixel change rejected by the area
  threshold, stability withheld while painting continues, a transition settling on
  the final state, a scene returning to baseline, and the timeout status for each
  of the three commands;
* **observation equivalence (Phase 4)**: the same scripted scenes — static, one
  change, return to baseline, gradual drift, repeated settling — run through both
  the standalone and the persistent-session paths, asserting the two reach the
  same conclusion *and* that a path is reproducible across runs. Frame IDs and
  capture counts are deliberately not compared; the semantic result is.
  This is the strongest protection against Phase 4 quietly changing behaviour
  while optimising it;
* **frame history**: identifiers, monotonicity, capacity boundaries, eviction and
  repeated eviction, capacity 1, invalid capacity, a retained frame staying alive
  while an operation holds it, and identifiers not being reused after eviction;
* **session lifecycle over the socket**: creation resolving geometry eagerly, list,
  info, close, an unregistered session reported as `session_not_found` on every
  method, and `no_frame_available` distinguished from `frame_not_available`;
* **the service**: startup, socket creation and `0600` permissions, ping, capture,
  retrieval, diff, observation, close, shutdown, socket removal on shutdown, stale
  socket reclamation after a simulated crash, a malformed request answered rather
  than hung, a request missing a required field rejected with its request ID echoed
  back, an unsupported protocol version refused, and an unreachable service
  reported as `service_unavailable`;
* **concurrency**, all against a real service over a real socket: concurrent
  captures producing distinct contiguous IDs with no history corruption, readers
  never observing a partially inserted frame, observation exclusivity returning
  `session_busy`, a capture during an observation being *served* rather than
  refused (and served at the next sample boundary, not after the whole
  observation), an observation in one session not blocking another, a close during
  an observation refused without deadlock and with the session left intact, and a
  close racing a capture resolving without a hang;
* **persistence and memory metrics**: standalone versus persistent capture
  latency with mean/p50/p95 and totals, the ordering of retrieval, comparison, and
  capture cost, history bounded by capacity with retained bytes equal to retained
  frames times the frame size, retained bytes unchanged by repeated retrievals, and
  the IPC round trip measured in isolation.

The integration tests in `tests/observe_equivalence.rs` and
`tests/session_concurrency.rs` hold a connection open for a whole scenario,
because Xvfb resets the root window when its last client disconnects. Without that
keep-alive a replayed scene would start from a cleared screen, which is subtle
enough that it silently produced a wrong answer during development.

Metrics are printed rather than asserted, because there is no defensible universal
threshold to assert against. Run them with:

```bash
cargo test --offline --test persistence_metrics -- --nocapture --test-threads=1
```

What those tests *do* assert is the structural claim — that the persistent path
really does avoid repeating connection setup, and that history really is bounded —
because those are properties of this implementation rather than of the machine it
runs on.

Integration tests start their own `Xvfb` and draw known colours onto it, then
verify the captured pixels at the coordinates they were drawn at. That is what
makes them meaningful rather than smoke tests. The temporal tests paint from a
background thread while the observation runs, so the scene really does change
underneath the observer.

## Scope

Implemented:

* **Phase 1** — capture a desktop, region, or window; PNG and JPEG; resizing;
  base64; structured JSON with an explicit coordinate transform; timing.
* **Phase 2** — raw-frame comparison with exact and thresholded RGB modes, pixel
  and area thresholds, changed-pixel counts, changed fraction, and a bounding box;
  plus a `diff` command for comparing saved images.
* **Phase 3** — temporal observation: `wait-change`, `wait-stable`, and `observe`,
  with configurable thresholds, cadence, timeout, and stability duration; a
  library API over a `FrameSource` seam; and per-stage observation timing.
* **Phase 4** — a persistent capture service: `eensh serve` over a local Unix
  socket, sessions that hold a display open, monotonic frame IDs, a bounded raw
  frame history, retrieval and comparison of retained frames without touching
  the display, observation inside a session using the unchanged Phase 3 state
  machines, and a `eensh session` client.

Deliberately **not** implemented, and reserved for later phases: ignore masks,
named regions, connected-component segmentation, tile summaries, perceptual
hashes, optical flow, adaptive payload selection, multi-region observation, a
network service, input injection, and Wayland support. Also deliberately deferred:
XDamage (Phase 4 still polls, per the Phase 3 cadence) and MIT-SHM (capture goes
through the ordinary X11 path). Disk persistence of frames: history lives in
memory and is deliberately not written anywhere. See `specs/` for the roadmap.

The one thing Phase 3 is *bad* at is per-sample connection cost: every sample opens
its own X11 connection. Phase 4 amortises that, without changing any of the
semantics above — which is exactly what the equivalence tests are there to
establish.
