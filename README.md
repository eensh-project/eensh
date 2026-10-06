# eensh

Fast, deterministic X11 screenshot capture, frame comparison, and temporal
observation for software agents.

`eensh` captures a desktop, a rectangular region, or a single X11 window and
returns an image that an agent can consume directly: PNG or JPEG bytes, an
optional base64 payload, and a stable JSON document that states exactly where the
pixels came from, how big the returned image is, and how to map image coordinates
back to screen coordinates. It compares two frames to say precisely what changed
and where. It watches a target over time, so an agent can replace
`sleep(arbitrary)` with "wait until the screen settles and tell me what it looks
like". And it samples a short temporal stack, so an agent can tell *motion* from
*stillness* rather than inferring it from two frames an unknown distance apart.

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

Five things are unusually painful when a *program* takes screenshots:

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
5. **One frame cannot show motion.** Three frames a tenth of a second apart, each
   labelled with when it was taken, answer "is this still moving" directly.
   `eensh session realtime` returns that stack rather than leaving the caller to
   make several calls and reassemble the timing itself.

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
                  wait-change|wait-stable|observe|realtime
                  [--socket PATH] [--json]
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

`--width`, `--height`, and `--scale` are mutually exclusive. Combining `--width` and `--height` is rejected rather
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
| 25 | `invalid_presentation_policy` | The presentation policy is malformed or self-contradictory. |
| 26 | `payload_budget_exceeded` | The required views cannot fit in the requested budget, even at their floors. |
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
a PNG or a JPEG. That is what lets it take live captures at high
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
  faster, and so sessions use a persistent connection rather
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

Persistent sessions are now available - a session is a
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
made the persistent path *slower* than starting a fresh process.

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
history still referred to it.

## Real-time observation

A single frame is one moment. Often that is not enough: to tell whether something
is *moving*, whether a spinner is still spinning, or whether a page is still
settling, you need several moments and you need to know how far apart they are.

```bash
eensh session realtime <SESSION_ID> [--frames N] [--interval D] [--timeout D]
                       [--width W] [--format png|jpeg] [--base64]
```

`session realtime` takes a bounded temporal stack: a small number of fresh frames
across a short window, returned oldest first, each carrying the moment it was
taken.

```bash
eensh session create --display :99 --json          # -> session_id
eensh session realtime s123 --frames 3 --base64 --json
```

The difference between `realtime` and `observe` is worth stating plainly.
`observe` answers *did it change, and when did it settle* and returns only the
final frame: it is a state machine that consumes frames to reach a verdict.
`realtime` answers *what did it look like along the way* and returns every frame:
it is a sampling operation that keeps what it took.

### Options

| Option | Default | Meaning |
|---|---|---|
| `--frames N` | 3 | How many frames, 1–8. A **maximum**, not a promise |
| `--interval D` | 50 ms | Nominal spacing between sample opportunities |
| `--timeout D` | 500 ms | Deadline for *starting* a capture |
| `--width W`, `--height`, `--scale` | — | Resize each returned image; applied after sampling |
| `--format`, `--quality`, `--compression` | png | Encoding for each returned frame |
| `--base64` | off | Embed each image inline |

The defaults are deliberately not the defaults of `observe` (which uses a 100 ms
interval and a 5 s timeout). `observe` is patient because it is waiting for
something to happen. `realtime` is bounded because the caller asked for a short
window, and a 5-second default would silently turn a quick look into a stall.

### The sampling schedule

Sample *opportunities* are scheduled against a fixed origin: exact multiples of
the interval measured from the start of the request. They are not scheduled from
the end of the previous capture.

```text
0ms      50ms     100ms    150ms    200ms
 |        |        |        |        |
 +--------+--------+--------+--------+
 sample   sample   sample   sample   sample
```

This is the difference between a cadence and a chain. If a capture at 50 ms takes
30 ms, the next opportunity is still at 100 ms — 20 ms away, not 50 ms away — so a
slow capture cannot push the whole schedule later.

An opportunity that cannot be taken is **skipped, never replayed.** If the capture
at 50 ms were still running at 100 ms, that slot is gone: the next sample is taken
at the next opportunity that is reachable. Replaying missed slots would quietly
double the length of the window and destroy the property the caller actually asked
for, which is that the frames are spaced by roughly the interval.

`skipped_opportunities` counts exactly this. A large number is not an error; it
means the cadence was optimistic for the machine and the stack is more tightly
spaced in real time than the nominal schedule suggests.

### Partial results

A request can be *accepted* and still not fit in its deadline. `--frames 8
--interval 1s --timeout 250ms` is a reasonable thing to type by mistake, and the
answer is a partial stack rather than an error:

```json
{
  "realtime": {
    "result": "partial",
    "requested_frames": 8,
    "captured_frames": 1,
    "skipped_opportunities": 0,
    "elapsed_ms": 0
  }
}
```

The frames that were obtained are returned, complete. Discarding work that was
already done would be worse than reporting it, and the caller can see from
`result` that the stack is short. Exit status is 0: a partial stack is a result,
not a failure.

The timeout gates **starting** a capture, not finishing one. A capture that began
before the deadline is allowed to complete, so `elapsed_ms` can exceed the timeout
by up to one capture duration. Abandoning a capture mid-flight would produce a
torn frame for no benefit.

### Frame ages

Every frame carries three times:

| Field | Meaning |
|---|---|
| `capture_offset_us` | When the sample was taken, measured from the start of the request |
| `capture_duration_us` | How long the capture itself took |
| `age_us` | How long ago the frame was captured, when the response was assembled |

`age_us` is the honest one for deciding whether a frame is worth acting on. The
newest frame is already an age by the time a caller sees it — in the default
configuration, comfortably over 100 ms, because three PNG encodes happen before
the response is sent:

```text
newest age:      mean 104.8ms  p95 107.0ms
```

Ages are computed *after* every encode, so they describe the moment the response
is finished rather than a moment that the encoding has since aged. `newest_frame_id`
and `newest_frame_age_us` are also hoisted to the top of the response, so the
common question — *how stale is the freshest thing here* — needs no array walk.

### Encoding does not happen between samples

This is the property that makes the timings mean anything, and it is enforced by
the shape of the code rather than by a rule someone remembered to follow.

The sampling loop consumes only raw frames:

```rust
pub trait RealtimeSource {
    fn capture(&mut self) -> Result<SessionFrame, Error>;
}

pub fn sample_stack<S: RealtimeSource>(...) -> Result<RealtimeResult, Error>
```

`RealtimeResult` has no image field and no encoder in scope. All resizing,
encoding, and base64 happen afterwards, in a separate `prepare` pass over the
already-collected stack. Encoding inside the window would stretch the interval the
caller asked for, and the offsets would then describe the encoder as much as the
scene.

The timing block reports the two phases separately, so the claim is checkable
rather than merely asserted:

```text
sampling window:  208382us        <- contains no encoding
  capture:         25710us
  sleep:          182663us
  unaccounted:         9us
presentation:                     <- happens after sampling ends
  encode:         104155us
  base64:             43us
```

Capture plus sleep account for the window to within microseconds. The three PNG
encodes cost about 104 ms and are reported outside it entirely. That gap is also
why `age_us` is not near zero: it is the encoding time that elapsed after the last
sample was taken.

### Stack ordering and identity

Frames are returned **oldest first**, and `capture_offset_us` increases
monotonically. Each frame carries the ordinary session `frame_id`, which means:

- A stack can be interleaved with independent captures — the identifiers will not
  be contiguous, and the ordering is by offset rather than by identifier.
- Every sampled frame is retrievable afterwards by `session frame <ID>`, exactly
  like any other frame.

Every sample enters history (requirement 54), but the returned stack does **not**
depend on history being able to hold it. The operation owns its frames for its own
lifetime, so `--frames 8` against a session with `--history 2` still returns eight
frames; the six that were evicted are simply no longer retrievable afterwards. A
request for an evicted frame is `frame_not_available` (exit 20) rather than a
silent substitution of a different frame.

### Backpressure

Only **one temporal operation may run per session.** A second one is refused
immediately with `session_busy` (exit 18), in either direction, and the same rule
covers `observe`, `wait-change`, and `wait-stable`:

```bash
eensh session realtime s123 &                  # takes the slot
eensh session wait-change s123 --timeout 1s    # -> exit 18, session_busy
```

A queued real-time observation would be stale before it even started, so it is
refused rather than parked. The refusal is immediate — no waiting on a lock — and
it leaves the session untouched, so a retry later sees a clean session.

A one-shot `capture` is treated differently: it is **serialized, not refused.**
`capture` is a request for a frame *now*, and serving it at the next sample
boundary is both possible and useful, so an orchestrator can keep using the
session while a stack is being taken.

Different sessions never contend. There is no global lock, so real-time sampling
in one session leaves another completely free — including another real-time stack.

### Long-lived clients

Spawning a process per observation is wasteful for an agent that observes
repeatedly, and it makes the connection bound hard to reason about. `eensh::client`
exposes the protocol directly over one reusable connection:

```rust
use eensh::client::{EenshClient, ImageSpec, SessionSpec};
use eensh::realtime::RealtimeOptions;

let mut client = EenshClient::connect_default()?;
let session = client.create_session(":99", &SessionSpec::desktop().with_history(4))?;

let stack = client.realtime(session.id(), &RealtimeOptions::default(), ImageSpec::metadata_only())?;
println!("{} frames, newest {} µs old", stack.captured_frames(), stack.newest_frame_age_us());
```

Every method takes `&mut self`, which enforces one outstanding request per
connection at the type level rather than by convention. The client is the same
protocol the CLI speaks, so a caller can mix the two freely, and every session
command has a corresponding method.

Two consequences of reuse are worth knowing. Request identifiers are unique per
connection and every response echoes the one it answers, so a mismatched reply is
an error rather than a confusing success. And a malformed request does not end the
connection: the service answers with a structured error and continues, so one bad
call does not cost the caller its connection.

The service bounds simultaneous connections (64 by default) and answers beyond the
bound with `service_overloaded` (exit 24). That is a deliberate limit rather than
an unbounded thread spawn, and the error is explicit so a caller can retry or fall
back to sharing a connection.

### Cost

Measured over 12 operations at 640×480, with presentation held identical on every
path (PNG at 160 px wide, inline) so the comparison is of transport rather than of
codec:

| Path | Mean | Per frame |
|---|---|---|
| Standalone (`capture` with a process and connection per frame) | 23.8 ms | 23.8 ms |
| Session CLI (one session, a process per frame) | 22.3 ms | 22.3 ms |
| Direct client (one connection, no process per frame) | 19.3 ms | 19.3 ms |
| Direct client, real-time stack of 3 at 50 ms | 142.6 ms | 47.5 ms |

The per-frame number for a stack is *higher* than a single capture, and that is the
whole point: a stack of three frames spaced 50 ms apart cannot finish before
100 ms, because waiting is the feature. Three separate captures return sooner — and
show three nearly identical moments. The cost shape is a schedule, not a
throughput figure: at 50 ms cadence the stack is dominated by sleep (183 ms of a
208 ms window), so a machine of half the speed would barely change it.

Sampling measurements for two cadences, over 8 runs each:

| Cadence | Complete | Skipped slots | Round trip | Newest age |
|---|---|---|---|---|
| 3 frames @ 50 ms | 8/8 | 0 | 214 ms | 105 ms |
| 4 frames @ 25 ms | 8/8 | 0 | 223 ms | 140 ms |

Sample durations are steady on this machine (~7.5 ms for a 640×480 frame), so both
cadences complete. Under a cadence faster than capture — 5 frames at 1 ms — the
schedule degrades exactly as designed: 77 opportunities skipped, all 5 frames still
captured, and the whole thing finished in well under a second because the missing
slots were never replayed.

Memory is bounded by history, not by the size of a stack:

| Configuration | Stack | History | Retained |
|---|---|---|---|
| 8 frames, `--history 4`, 640×480 | 8 frames | 4 frames | 3,686,400 B |

A raw frame is three bytes per pixel, so 640×480 is 921,600 B and the retained
total is exactly four of them. The stack's other four frames are released when the
operation ends. Two concurrent six-frame stacks in separate sessions hold their own
frames independently (5,529,600 B each) and neither is left unusable afterwards.

**No capture buffers are reused here either**: a frame that is retained, or that 
belongs to a returned stack, must own its pixels.

### No new exit codes

Real-time observation reuses the existing table: `session_busy` (18),
`session_closed` (19), `frame_not_available` (20), `target_lost` (14),
`observation_failed` (16), and 100 for a timeout. A destroyed window mid-sampling
is terminal and marks the session failed, exactly as it does for `observe`. A
partial stack is exit 0.

## Efficient presentation

Everything above answers *what is on the screen*. Presentation answers a different
question: **what is worth sending back?** An agent watching a 1920×1080 game does
not need four full-resolution screenshots to know what happened; it needs one recent
whole-frame view and the details it actually cares about.

The rule that keeps this from leaking into everything else is that presentation
happens strictly **after** raw observation:

```text
capture / observe / realtime
    -> raw Frame or raw Frame stack        (when the pixels were taken)
    -> presentation policy                 (what to send)
    -> View(s)                             (overview, regions, the changed crop)
    -> encode                              (presentation work)
    -> base64 / protocol response
```

Nothing in the presentation layer captures, compares, schedules, or touches history.
It is handed frames that already exist and decides how to render them, which is what
lets one raw observation produce several alternative presentations.

### Overview plus ROI

The common request is "show me the whole screen cheaply, and these areas in detail".
`--region` names a rectangle in **source** coordinates, so a crop of a window at
source `(100, 200)` reports `(100, 200)` rather than a frame-local offset and a
caller never has to reconstruct where it came from.

```bash
# A cheap overview plus a HUD strip and a minimap, each with its own format.
eensh session capture "$SID" --json --base64 \
  --overview-width 480 --overview-format jpeg --overview-quality 60 \
  --region 'hud=0,0,1920,180' --region-format png \
  --region 'map=1600,880,320,200@newest!180' --region-format jpeg --region-quality 80
```

A region may carry a scope and a fitting priority as suffixes: `NAME=X,Y,W,H@SCOPE`
or `@!PRIORITY`, comma-separated when both are given (`@all,!140`). The scope
(`all`, `newest`, `older`) limits which frames of a temporal stack the region
applies to; the priority orders what the budget fitter degrades first.

In a response, a view reports `source_rect` (in source coordinates), `transform`
(mapping view pixels back to source pixels), and `applied` — the settings *actually*
used, which is not always what was asked for once a budget has been fitted. A
metadata-only view has a null image and says `"metadata_only": true`, so a view with
no pixels is never confused with one that failed to encode.

### Temporal policy

For a real-time stack, `--temporal` says how each frame should be treated:

| Mode | Effect |
|---|---|
| `all-same` | Every frame identically. |
| `newest-detailed` | Older frames reduced, the newest preserved. |
| `newest-only` | Only the newest carries image bytes. |
| `metadata-older` | Older frames keep identity and timing, no image. |

```bash
eensh session realtime "$SID" --json --base64 --frames 4 --interval 40ms \
  --temporal newest-detailed --older-width 320 --older-quality 45 \
  --newest-width 640 --newest-quality 85
```

The newest frame is protected: the fitting ladder spends every older lever — quality
first, then resolution, then optional views — before touching the newest at all. But
**reduction is not deletion**. Asking for four frames and receiving one would be a
silent lie, because what came back would look complete; every requested frame is
reported, with fewer bytes rather than fewer frames.

### Payload budget

`--max-base64-bytes` bounds the visual payload. Fitting is **opt-in**: with room to
spare, nothing is adjusted and the response says `"fit": "exact"`. Under pressure the
fitter walks a fixed ladder, always in this order:

```text
1. older frame quality        ->   2. older frame resolution
3. omit optional views        ->   4. newest quality
5. newest resolution
```

Each rung is exhausted before the next is considered, or the ordering would be a
formality rather than a protection. Required views are never omitted: a budget that
cannot hold them fails with `payload_budget_exceeded` (exit 26) and a message quoting
**the smallest achievable payload**, so the caller can pick a workable number
instead of guessing again. That floor is computed by walking the same ladder, and it
is never above what the unfitted request would have cost.

Every adjustment is reported, with the numbers that changed:

```json
"payload": {
  "budget_base64_bytes": 300000,
  "actual_base64_bytes": 287400,
  "fit": "adjusted",
  "adjustments": [
    { "change": "quality", "frame_id": 2, "view": "overview", "requested": 85, "actual": 65 },
    { "change": "omitted", "frame_id": 1, "view": "minimap",
      "reason": "optional view omitted to meet the payload budget" }
  ]
}
```

Fitting is deterministic: identical raw frames and an identical policy produce an
identical plan, because the ladder orders views by a single total key rather than by
the order a hash map happened to yield. The fitted plan is itself a comparable value.

### The changed crop

For `diff`, `--changed-region` returns a bounding box as a view, cropped
from the **newer** frame. `--changed-padding` widens it, clamped at the source edges;
the factual `raw_changed_rect` and the `returned_rect` are reported separately so a
change at the screen edge is never mistaken for a large one.

A changed crop is *required* by default — a caller that asked for it asked for a
reason — and `--changed-optional` opts into treating it as expendable. Note that
two answers stay distinct: `bounding_box` is a fact about pixels, `changed`
is a policy verdict about area. A sub-threshold change has an empty verdict and a
real bounding box, and a requested crop is still returned for it.

### Cost

Measured against a live 640×480 Xvfb scene, five frames at 40 ms cadence, comparing
policies **within one format** (this matters: a field of flat blocks compresses to
almost nothing as PNG and costs a great deal as JPEG, so a cross-format comparison
measures the codec rather than the policy):

| Policy | Payload | Response | Presentation | Newest age |
|---|---|---|---|---|
| all-same JPEG q85 | 271,340 B | 290,614 B | 1,009 ms | 1,190 ms |
| `newest-detailed` | 117,100 B | 136,382 B | 469 ms | 649 ms |
| `newest-only` | 54,268 B | 72,845 B | 202 ms | 383 ms |
| `metadata-only` | 0 B | 18,482 B | 0.01 ms | 181 ms |
| all-same PNG *(reference)* | 14,620 B | 33,720 B | 182 ms | 364 ms |

So `newest-detailed` costs about 57% less than sending every frame at the same
settings, `newest-only` about 80% less, and a metadata-only response carries no
pixels at all while still describing every frame. An overview plus two regions
costs about **64%** of whole frames at the crops' own quality, while describing
three views per frame instead of one.

These are observations from one machine, not universal constants; run
`cargo test --offline --test presentation_metrics -- --nocapture --test-threads=1`
to reproduce them. What the tests *assert* is the ordering (each cheaper policy
really is cheaper) and the structural claims, not the numbers.

### Memory

Views share one raw frame rather than copying it: `SessionFrame::frame` is an
`Arc<Frame>`, and the presentation layer never constructs a `Frame`, so nine views
over one frame cost one frame. Measured on a 640×480 screen (921,600 B per raw
frame), presenting nine views of one retained frame and then doing it three more
times produced **zero** further growth in the service's resident set. Raw frames are
not duplicated, and cropping allocates no new frame ID — a `frame` request with nine
views leaves `frames_captured` at 1.

History capacity is unchanged by presentation (`--history` still bounds retention),
and temporary encoded buffers are released when the response is sent.

### Where the time goes

Presented timing is kept separate from sampling timing, so a caller can distinguish
*captured late* from *captured on time, delivered late*:

```json
"timing": {
  "presentation_us": 4690, "crop_us_total": 120, "resize_us_total": 980,
  "encode_us_total": 3100, "base64_us_total": 380, "budget_fit_us": 0
}
```

Presentation cannot influence sampling. Sampling completes entirely before
presentation begins, so the same request under a metadata-only policy and under a
heavy multi-view policy with budget fitting captures the same number of frames, at
the same cadence, with the same skips.

### Presentation exit codes

Two were added: `invalid_presentation_policy` (25) for a malformed or
self-contradictory policy — a zero-sized region, a duplicate region name, a region
named `overview`, a budget that requests nothing — refused before any capture, and
`payload_budget_exceeded` (26) for a budget that cannot hold the required views.

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
* **observation equivalence**: the same scripted scenes — static, one
  change, return to baseline, gradual drift, repeated settling — run through both
  the standalone and the persistent-session paths, asserting the two reach the
  same conclusion *and* that a path is reproducible across runs. Frame IDs and
  capture counts are deliberately not compared; the semantic result is.
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
  the IPC round trip measured in isolation;
* **real-time sampling, synthetic**: the schedule as a pure function —
  exact sample offsets at a fixed origin, skips never replayed, a capture that
  overruns its slot, a cadence faster than capture, the timeout gating the *start*
  of a capture, a partial stack under an impossible deadline, a single-frame stack,
  option validation at both bounds, and the ordering and identity rules;
* **real-time sampling over Xvfb**: a moving scene captured in temporal order, a
  static scene still yielding every requested frame, every sample entering history
  and remaining retrievable, resizing applied only after sampling, an interleaved
  external capture leaving the stack intact, a stack larger than history returned
  whole, a destroyed target reported as `target_lost` and failing the session, and
  base64 payloads each decoding as a standalone image;
* **real-time backpressure**: a second real-time request refused with
  `session_busy` rather than queued, the same refusal for `wait-change`,
  `wait-stable`, and `observe` in both directions, a one-shot capture during a
  stack *served* rather than refused, real-time sampling in one session not
  blocking another, and a partial stack leaving the session usable;
* **real-time cost and memory**: the four paths to a frame compared with
  presentation held identical, sampling and encode phases measured separately and
  shown not to overlap, skip counts under an optimistic cadence, retained bytes
  bounded by history rather than by stack size, and two concurrent stacks holding
  independent frames;
* **long-lived clients**: a whole sequence of operations over one connection, many
  sequential requests without degradation, retrieval after a stack over the same
  connection, a malformed request leaving the connection usable, close leaving the
  connection reusable, a clear error once the service stops, the simultaneous
  connection bound with `service_overloaded` beyond it, unique request IDs echoed
  per response, and a client per concurrent caller;
* **presentation over Xvfb**: an overview and a region verified pixel by
  pixel against four quadrants painted in known colours, several regions returned
  with independent sizes and formats in declared order, a region-only response
  carrying no overview, out-of-bounds and zero-sized regions refused, and multiple
  views of one frame reporting one frame ID and one capture moment — the same-frame
  guarantee;
* **the changed crop over Xvfb**: a known painted rectangle producing the exact
  bounding box, padding widening the returned rectangle but not the factual one,
  padding clamped at the source edge and the clamping visible, no change yielding no
  crop at all, a sub-threshold change still yielding a crop while reporting
  `changed: false`, a whole-screen change reported honestly, and a crop allocating no
  new frame ID;
* **presentation temporal policy over Xvfb**: all four modes compared on one live
  scene, `all-same` rendering every frame identically, `newest-detailed` reducing the
  older frames while the newest keeps its settings, `newest-only` leaving one frame
  with pixels and still reporting the others, `metadata-older` keeping identity and
  timing with no image, a metadata-only policy producing no pixels anywhere while
  still describing every view, a static scene still yielding every requested frame,
  and temporal order following the sampling order rather than numeric frame IDs;
* **payload fitting over Xvfb**: an impossible budget refused as
  `payload_budget_exceeded` with the floor quoted, a budget just above the floor
  returning every captured frame, a generous budget leaving the presentation
  untouched with `"fit": "exact"`, a tighter budget keeping every frame, the newest
  protected until the older levers are spent, an optional region dropped while a
  required one survives, and five repeated fits of the same raw frames producing an
  identical presentation — determinism;
* **timing isolation**: one real-time request made twice, under a
  metadata-only policy and under a heavy multi-view policy with budget fitting,
  asserting the same frame count, outcome, cadence slots, skip count, interval, and
  deadline — presentation cannot influence sampling — while presentation timing and
  payload are shown to differ;
* **presentation cost and memory metrics**: the four policies measured against one
  live scene with the ordering asserted and the numbers printed, an overview plus
  regions compared against whole frames at equal quality, and the memory claims
  established by repeating a nine-view presentation and requiring the resident set to
  settle at zero growth.

The integration tests in `tests/observe_equivalence.rs`, `tests/session_concurrency.rs`,
and `tests/realtime_x11.rs` hold a connection open for a whole scenario, because Xvfb
resets the root window when its last client disconnects. Without that keep-alive a
replayed scene would start from a cleared screen, which is subtle enough that it
silently produced a wrong answer during development. The tests that sample the root
window while nothing is being painted keep a connection open explicitly for the same
reason.

Metrics are printed rather than asserted, because there is no defensible universal
threshold to assert against. Run them with:

```bash
cargo test --offline --test persistence_metrics -- --nocapture --test-threads=1
cargo test --offline --test realtime_metrics -- --nocapture --test-threads=1
cargo test --offline --test presentation_metrics -- --nocapture --test-threads=1
```

What those tests *do* assert is the structural claim — that the persistent path
really does avoid repeating connection setup, that history really is bounded, that
encoding really does not occur inside a sampling window, that skipped
opportunities really are never replayed, that the cheaper presentation policies
really are cheaper, and that views really do share one raw frame — because those are
properties of this implementation rather than of the machine it runs on.

Integration tests start their own `Xvfb` and draw known colours onto it, then
verify the captured pixels at the coordinates they were drawn at. That is what
makes them meaningful rather than smoke tests. The temporal tests paint from a
background thread while the observation runs, so the scene really does change
underneath the observer.
