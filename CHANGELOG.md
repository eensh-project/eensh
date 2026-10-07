# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Note that `eensh` is at `0.1.0`. Until `1.0.0`, the command-line interface and the
JSON response are still allowed to change between minor versions. Exit statuses
and error codes are the exception: those are intended to be stable now.

## [Unreleased]

## [0.1.0] - 2026-10-06

The first release.

`eensh` captures an X11 desktop, a region, or a single window and returns it
together with a machine-readable description of what was captured. A program
receiving the result can tell where the pixels came from, how large the returned
image is, how to map image coordinates back to screen coordinates, and how long
each stage took — without inferring any of it.

### Added

**Capture.** `eensh capture` writes PNG or JPEG to a file, to stdout, or into a
JSON response, from any of three targets: a whole desktop, a rectangular region,
or one window by ID.

- `SOURCE` geometry is reported separately from the returned `IMAGE` size, with an
  explicit `TRANSFORM` between them. The transform is always present, including
  when it is the identity mapping.
- A proportionate resize via `--width`, `--height`, or `--scale`.
- `--base64` embeds the encoded image in the JSON response, so a single call
  returns both the pixels and the description of them.
- `--json` emits a stable document with four top-level fields: `source`, `image`,
  `transform`, and `timing`.
- Image bytes and JSON are never interleaved on one stream.

**Compare.** `eensh diff BEFORE AFTER` reports exactly what changed between two
images, or between two retained frames of a session.

- Reports the changed-pixel count, the changed fraction, and the bounding box of
  the changed region.
- `--pixel-threshold` sets the per-channel difference that counts as a change;
  `--area-threshold` sets the changed fraction that counts as meaningful. The two
  are deliberately asymmetric: the pixel comparison is strict, the area
  comparison is inclusive.
- A frame may have a non-empty bounding box and still report `changed: false` —
  one pixel differs, but not enough to act on. Both answers are always returned.
- `--changed-crop PATH` writes the changed region as an image, cropped from the
  newer frame. Nothing is written when nothing changed.

**Observation.** Three commands replace a fixed delay with a question about the
screen, differing only in what each frame is compared against.

- `wait-change` compares each frame against a fixed baseline and returns when the
  visible state has changed.
- `wait-stable` compares consecutive frames and returns once the scene stops
  changing.
- `observe` runs a baseline comparison, then consecutive comparison, so a
  half-drawn menu is not mistaken for the settled result.
- Exceeding `--timeout` exits with status `100`, which is an outcome rather than
  an error.

**Sessions.** `eensh serve` runs a persistent service that holds a display open
and keeps a bounded history of raw frames, so later requests read frames already
captured instead of touching the display again.

- `eensh session create`, `list`, `info`, and `close` manage session lifetime.
- `eensh session capture`, `latest`, `frame`, and `diff` capture a new frame or
  read a retained one.
- Frames carry monotonically increasing IDs. Requesting an evicted ID fails with
  `frame_not_available` rather than silently substituting a different frame.
- Sessions are per-user, and the socket is local. The path is taken from
  `--socket`, then `$EENSH_SOCKET`, then `$XDG_RUNTIME_DIR/eensh.sock`.
- `eensh ping` reports whether a service is reachable.

**Real-time sampling.** `eensh session realtime` captures a short temporal stack
of a scene that may never hold still, so movement can be told from stillness
rather than inferred from two frames an unknown distance apart.

- `--frames`, `--interval`, and `--timeout` define the stack.
- Sample opportunities are scheduled from a fixed origin, so a slow capture cannot
  push the cadence later. A missed opportunity is skipped, never replayed, and
  counted in `skipped_opportunities`.
- A stack that runs out of time returns the frames it did capture, with
  `result: "partial"`, rather than discarding them.

**Presentation.** One capture can yield several views at different sizes and
formats, and a response can be fitted to a payload budget.

- A whole-screen overview, controlled by `--overview-width`, `--overview-format`,
  `--overview-quality`, and `--no-overview`.
- Named regions via `--region NAME=X,Y,W,H`, each with its own width, format, and
  quality, plus optional scope and fitting priority.
- `--metadata-only` requests geometry without pixels.
- `--max-base64-bytes` fits a response to a byte budget by reducing older frames
  first, in a fixed order, and never by omitting a required view. Every adjustment
  is reported in `payload.adjustments`. A budget that cannot be met fails with
  `payload_budget_exceeded` and states the smallest budget that would succeed.

**Error model.** Every failure returns a nonzero exit status, a concise message on
stderr, and — with `--json` — a structured error carrying a stable code. Exit
statuses and error codes are part of the interface; branch on those, never on the
message.

**Rust client.** The `client` module speaks the same protocol as the CLI over a
single connection, with one outstanding request per connection enforced by the
type system rather than by convention. Presentation is available through parallel
methods, so adding it did not change any existing signature.

[Unreleased]: https://github.com/eensh-project/eensh/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/eensh-project/eensh/releases/tag/v0.1.0
