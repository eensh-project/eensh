//! Bounded, session-local frame history.
//!
//! A persistent session retains a small number of recent **raw** frames so that
//! later operations can compare, crop, or re-encode them without re-capturing and
//! without a decode round trip. Raw retention is the whole point: keeping encoded
//! frames would force a lossy decode before any comparison could run.
//!
//! Two properties matter and are enforced here:
//!
//! * **Bounded.** Retention is a fixed capacity and eviction is deterministic.
//!   An agent that leaves a session running must not be able to exhaust memory.
//! * **Shared, not copied.** A frame handed out while history evicts it stays
//!   valid, because frames are shared rather than borrowed. This is what lets a
//!   running observation keep its baseline even after enough captures have pushed
//!   that baseline out of public history.
//!
//! Frame identity is per-session and monotonically increasing. IDs are never
//! reused, and retrieving an existing frame never allocates a new one.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::frame::Frame;

/// A frame identifier, unique within one session.
///
/// The first frame of a session is `1`; `0` is reserved so that it can mean
/// "no frame" without an `Option`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FrameId(pub u64);

impl FrameId {
    /// The first frame a session will allocate.
    pub const FIRST: FrameId = FrameId(1);

    /// The underlying number.
    pub fn get(self) -> u64 {
        self.0
    }

    /// The next identifier in sequence.
    ///
    /// Returns `None` at [`u64::MAX`] rather than wrapping. IDs are never reused,
    /// so running out is a hard stop; the alternative would silently make two
    /// different frames indistinguishable and quietly corrupt every comparison
    /// that referenced the older one.
    pub fn next(self) -> Option<FrameId> {
        self.0.checked_add(1).map(FrameId)
    }
}

impl std::fmt::Display for FrameId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A raw frame plus the session-local identity and timing that go with it.
///
/// The `Arc` is what makes eviction safe: an operation holding a [`SessionFrame`]
/// keeps its pixels alive regardless of what history does afterwards.
#[derive(Debug, Clone)]
pub struct SessionFrame {
    /// Which session captured it.
    pub session_id: String,
    /// The frame's identity within that session.
    pub frame_id: FrameId,
    /// The raw pixels and their source geometry.
    pub frame: Arc<Frame>,
    /// Monotonic instant at which the capture completed, used for frame age.
    pub captured_at: Instant,
    /// How long the capture itself took.
    pub capture_duration: Duration,
}

impl SessionFrame {
    /// The frame's width in pixels.
    pub fn width(&self) -> u32 {
        self.frame.width()
    }

    /// The frame's height in pixels.
    pub fn height(&self) -> u32 {
        self.frame.height()
    }

    /// How long ago this frame was captured, as of `now`.
    pub fn age_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.captured_at)
    }

    /// Bytes of raw pixel data retained by this frame.
    pub fn byte_len(&self) -> usize {
        use crate::frame::PixelFormat;
        let bytes_per_pixel = match self.frame.pixel_format {
            PixelFormat::Rgb8 => PixelFormat::Rgb8.bytes_per_pixel(),
        };
        self.width() as usize * self.height() as usize * bytes_per_pixel
    }
}

/// Default number of recent frames a session retains.
///
/// Eight is enough for the patterns agents actually use — compare the frame
/// before an action with the one after, and keep a few of each — while keeping
/// the memory cost modest: 8 × 1920×1080×3 bytes is about 50 MB.
pub const DEFAULT_CAPACITY: usize = 8;

/// The largest history a session will accept, as a frame count.
pub const MAX_CAPACITY: usize = 256;

/// A ring buffer of recent raw frames.
#[derive(Debug)]
pub struct FrameHistory {
    capacity: usize,
    frames: VecDeque<SessionFrame>,
    /// Every frame the session has ever captured, including evicted ones.
    /// Used to report the true high-water mark.
    captured_total: u64,
    next: FrameId,
    /// Set once the id counter would overflow, so the session fails loudly
    /// instead of reusing an identifier.
    exhausted: bool,
}

impl FrameHistory {
    /// Create a history with the given capacity.
    ///
    /// Capacity is validated rather than clamped: a caller asking for zero frames
    /// has almost certainly made a mistake, and silently giving them eight would
    /// hide it. Capacity is also capped, because retention is bounded by design.
    pub fn new(capacity: usize) -> Result<Self, Error> {
        if capacity == 0 {
            return Err(Error::invalid_arguments(
                "history capacity must be at least 1",
            ));
        }
        if capacity > MAX_CAPACITY {
            return Err(Error::invalid_arguments(format!(
                "history capacity {capacity} exceeds the maximum of {MAX_CAPACITY} frames; \
                 retained frames are raw pixels, so this is a memory bound"
            )));
        }

        Ok(FrameHistory {
            capacity,
            frames: VecDeque::with_capacity(capacity),
            captured_total: 0,
            next: FrameId::FIRST,
            exhausted: false,
        })
    }

    /// Create a history with the default capacity.
    pub fn with_default_capacity() -> Self {
        FrameHistory::new(DEFAULT_CAPACITY).expect("the default capacity is valid")
    }

    /// The configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many frames are currently retained.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether no frame has been captured yet.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Total captures ever made, including frames since evicted.
    pub fn captured_total(&self) -> u64 {
        self.captured_total
    }

    /// The identifier the next capture will receive.
    pub fn next_frame_id(&self) -> FrameId {
        self.next
    }

    /// The oldest retained frame.
    pub fn oldest(&self) -> Option<&SessionFrame> {
        self.frames.front()
    }

    /// The newest retained frame.
    pub fn latest(&self) -> Option<&SessionFrame> {
        self.frames.back()
    }

    /// The oldest retained frame identifier.
    pub fn oldest_frame_id(&self) -> Option<FrameId> {
        self.oldest().map(|frame| frame.frame_id)
    }

    /// The newest retained frame identifier.
    pub fn latest_frame_id(&self) -> Option<FrameId> {
        self.latest().map(|frame| frame.frame_id)
    }

    /// Look up a retained frame by identifier.
    ///
    /// A frame that was evicted, or never existed, is an explicit
    /// [`Error::FrameNotAvailable`]. The error distinguishes the two cases,
    /// because "you asked for a frame from before this session started" and "you
    /// asked for a frame that has since been evicted" call for different
    /// responses from a caller.
    pub fn get(&self, frame_id: FrameId) -> Result<SessionFrame, Error> {
        if let Some(frame) = self.frames.iter().find(|f| f.frame_id == frame_id) {
            return Ok(frame.clone());
        }

        let newest = self.latest_frame_id();
        let oldest = self.oldest_frame_id();

        let detail = match (oldest, newest) {
            (Some(oldest), Some(newest)) if frame_id > newest => format!(
                "frame {} has not been captured yet; the newest retained frame is {}",
                frame_id.get(),
                newest.get()
            ),
            (Some(oldest), Some(newest)) if frame_id < oldest => format!(
                "frame {} has been evicted; retained frames are {}..={} with capacity {}",
                frame_id.get(),
                oldest.get(),
                newest.get(),
                self.capacity
            ),
            _ => "no frame has been captured yet".to_string(),
        };

        Err(Error::frame_not_available(detail))
    }

    /// Allocate the next identifier.
    ///
    /// Called only when a physical capture is about to succeed, so that a failed
    /// capture does not consume an identifier and frame numbering stays
    /// intuitive.
    pub fn allocate_id(&mut self) -> Result<FrameId, Error> {
        if self.exhausted {
            return Err(Error::session_busy(
                "the session has exhausted its frame identifier space; \
                 start a new session rather than reusing identifiers",
            ));
        }

        let id = self.next;
        match id.next() {
            Some(next) => self.next = next,
            None => self.exhausted = true,
        }
        Ok(id)
    }

    /// Insert a captured frame, evicting the oldest if the capacity is full.
    ///
    /// The frame's identifier must be the one [`FrameHistory::allocate_id`] just
    /// returned. Insertion is atomic from a reader's perspective because the
    /// caller holds the session lock, so a partially populated frame is never
    /// observable.
    pub fn insert(&mut self, frame: SessionFrame) -> Result<FrameId, Error> {
        if frame.frame_id >= self.next {
            return Err(Error::internal(format!(
                "frame {} was inserted without being allocated; the next unallocated \
                 identifier is {}",
                frame.frame_id, self.next
            )));
        }

        if self.frames.len() == self.capacity {
            self.frames.pop_front();
        }

        let id = frame.frame_id;
        self.frames.push_back(frame);
        self.captured_total += 1;
        Ok(id)
    }

    /// Total raw bytes currently retained.
    ///
    /// Reported alongside session info so that the memory cost of raw retention
    /// is visible rather than a surprise.
    pub fn retained_bytes(&self) -> usize {
        self.frames.iter().map(|frame| frame.byte_len()).sum()
    }

    /// Snapshot the retained identifiers, oldest first.
    pub fn frame_ids(&self) -> Vec<FrameId> {
        self.frames.iter().map(|frame| frame.frame_id).collect()
    }
}

/// A compact description of a session's history, for `session info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySummary {
    /// Configured capacity.
    pub capacity: usize,
    /// Frames currently retained.
    pub retained: usize,
    /// Oldest retained identifier, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_frame_id: Option<FrameId>,
    /// Newest retained identifier, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_frame_id: Option<FrameId>,
    /// Total captures ever made, including evicted frames.
    pub captured_total: u64,
    /// Raw bytes currently retained.
    pub retained_bytes: usize,
}

impl FrameHistory {
    /// Summarize the history for reporting.
    pub fn summary(&self) -> HistorySummary {
        HistorySummary {
            capacity: self.capacity,
            retained: self.len(),
            oldest_frame_id: self.oldest_frame_id(),
            newest_frame_id: self.latest_frame_id(),
            captured_total: self.captured_total,
            retained_bytes: self.retained_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{PixelBuffer, PixelFormat};
    use crate::geometry::{CaptureTarget, SourceGeometry};

    /// Build a session frame with a distinguishable single-pixel value.
    fn session_frame(id: FrameId, marker: u8) -> SessionFrame {
        let pixels = PixelBuffer::new(2, 2, PixelFormat::Rgb8, vec![marker; 12]).unwrap();
        let frame = Frame::new(
            SourceGeometry {
                target: CaptureTarget::Desktop,
                display: Some(":99".into()),
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            pixels,
            Instant::now(),
        );
        SessionFrame {
            session_id: "test-session".to_string(),
            frame_id: id,
            frame: Arc::new(frame),
            captured_at: Instant::now(),
            capture_duration: Duration::from_micros(100),
        }
    }

    /// Allocate an id and insert a frame with it, the way a session does.
    fn capture_into(history: &mut FrameHistory, marker: u8) -> FrameId {
        let id = history.allocate_id().unwrap();
        history.insert(session_frame(id, marker)).unwrap()
    }

    #[test]
    fn frame_ids_start_at_one_and_never_repeat() {
        assert_eq!(FrameId::FIRST.get(), 1);

        let mut history = FrameHistory::new(4).unwrap();
        assert_eq!(history.next_frame_id(), FrameId(1));

        let first = capture_into(&mut history, 1);
        let second = capture_into(&mut history, 2);
        let third = capture_into(&mut history, 3);

        assert_eq!((first, second, third), (FrameId(1), FrameId(2), FrameId(3)));
        assert_eq!(history.next_frame_id(), FrameId(4));
    }

    #[test]
    fn retrieval_never_allocates_a_new_id() {
        let mut history = FrameHistory::new(4).unwrap();
        let id = capture_into(&mut history, 7);

        let before = history.next_frame_id();
        for _ in 0..3 {
            let _ = history.get(id).unwrap();
        }
        let _ = history.latest().unwrap();
        assert_eq!(
            history.next_frame_id(),
            before,
            "reading must not advance the identifier sequence"
        );
        assert_eq!(history.captured_total(), 1);
    }

    #[test]
    fn the_first_frame_is_retrievable() {
        let mut history = FrameHistory::new(4).unwrap();
        let id = capture_into(&mut history, 42);
        let frame = history.get(id).unwrap();
        assert_eq!(frame.frame_id, id);
        assert_eq!(frame.frame.pixels.data()[0], 42);
    }

    #[test]
    fn latest_returns_the_newest_frame() {
        let mut history = FrameHistory::new(4).unwrap();
        capture_into(&mut history, 1);
        capture_into(&mut history, 2);
        let newest = capture_into(&mut history, 3);

        let latest = history.latest().unwrap();
        assert_eq!(latest.frame_id, newest);
        assert_eq!(latest.frame.pixels.data()[0], 3);
    }

    #[test]
    fn eviction_is_deterministic_and_oldest_first() {
        let mut history = FrameHistory::new(4).unwrap();
        for marker in 10..14 {
            capture_into(&mut history, marker);
        }
        assert_eq!(
            history.frame_ids(),
            vec![FrameId(1), FrameId(2), FrameId(3), FrameId(4)]
        );

        // The fifth capture evicts frame 1.
        let fifth = capture_into(&mut history, 14);
        assert_eq!(fifth, FrameId(5));
        assert_eq!(
            history.frame_ids(),
            vec![FrameId(2), FrameId(3), FrameId(4), FrameId(5)]
        );
        assert_eq!(history.len(), 4);
        assert_eq!(
            history.captured_total(),
            5,
            "the total counts evicted frames"
        );
    }

    #[test]
    fn repeated_eviction_keeps_exactly_the_capacity() {
        let mut history = FrameHistory::new(3).unwrap();
        for marker in 0..20u8 {
            capture_into(&mut history, marker);
        }
        assert_eq!(history.len(), 3);
        assert_eq!(
            history.frame_ids(),
            vec![FrameId(18), FrameId(19), FrameId(20)]
        );
        assert_eq!(history.captured_total(), 20);
    }

    #[test]
    fn an_evicted_frame_reports_that_it_was_evicted() {
        let mut history = FrameHistory::new(2).unwrap();
        let evicted = capture_into(&mut history, 1);
        capture_into(&mut history, 2);
        capture_into(&mut history, 3);

        let error = history.get(evicted).unwrap_err();
        assert_eq!(error.code(), "frame_not_available");
        assert!(
            error.message().contains("evicted"),
            "message should say it was evicted, was: {}",
            error.message()
        );
    }

    #[test]
    fn a_frame_from_the_future_reports_that_it_does_not_exist_yet() {
        let mut history = FrameHistory::new(2).unwrap();
        capture_into(&mut history, 1);

        let error = history.get(FrameId(99)).unwrap_err();
        assert_eq!(error.code(), "frame_not_available");
        assert!(
            error.message().contains("not been captured yet"),
            "message was: {}",
            error.message()
        );
    }

    #[test]
    fn an_empty_history_explains_that_nothing_was_captured() {
        let history = FrameHistory::new(4).unwrap();
        assert!(history.is_empty());
        assert!(history.latest().is_none());
        assert!(history.oldest().is_none());

        let error = history.get(FrameId(1)).unwrap_err();
        assert_eq!(error.code(), "frame_not_available");
        assert!(error.message().contains("no frame has been captured"));
    }

    #[test]
    fn capacity_one_retains_exactly_one_frame() {
        let mut history = FrameHistory::new(1).unwrap();
        let first = capture_into(&mut history, 1);
        let second = capture_into(&mut history, 2);

        assert_eq!(history.len(), 1);
        assert_eq!(history.latest_frame_id(), Some(second));
        assert!(history.get(first).is_err());
        assert_eq!(history.get(second).unwrap().frame.pixels.data()[0], 2);
    }

    #[test]
    fn a_zero_capacity_is_rejected_rather_than_silently_defaulted() {
        let error = FrameHistory::new(0).unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
        assert!(error.message().contains("at least 1"));
    }

    #[test]
    fn an_absurd_capacity_is_rejected_because_frames_are_raw_pixels() {
        let error = FrameHistory::new(MAX_CAPACITY + 1).unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
        assert!(error.message().contains("memory bound"));
    }

    #[test]
    fn the_default_capacity_is_usable() {
        let history = FrameHistory::with_default_capacity();
        assert_eq!(history.capacity(), DEFAULT_CAPACITY);
    }

    #[test]
    fn a_retained_frame_stays_alive_after_eviction() {
        // The property that lets a running observation keep its baseline after
        // enough captures have pushed it out of public history.
        let mut history = FrameHistory::new(2).unwrap();
        let first = capture_into(&mut history, 99);
        let held = history.get(first).unwrap();

        // Capture well past the capacity so the original is long evicted.
        for marker in 0..10u8 {
            capture_into(&mut history, marker);
        }
        assert!(
            history.get(held.frame_id).is_err(),
            "it left public history"
        );

        // The pixels are still valid because the frame is shared, not borrowed.
        assert_eq!(held.frame.pixels.data()[0], 99);
        assert_eq!(held.width(), 2);
    }

    #[test]
    fn inserting_an_unallocated_id_is_refused() {
        let mut history = FrameHistory::new(2).unwrap();
        // Never allocated: the next free id is 1.
        let error = history.insert(session_frame(FrameId(5), 0)).unwrap_err();
        assert_eq!(error.code(), "internal_error");
    }

    #[test]
    fn identifiers_do_not_wrap_when_the_counter_is_exhausted() {
        let mut history = FrameHistory::new(2).unwrap();
        // Jump the counter to the end of its range.
        history.next = FrameId(u64::MAX);
        assert_eq!(history.allocate_id().unwrap(), FrameId(u64::MAX));

        // The next allocation must fail rather than reuse u64::MAX.
        let error = history.allocate_id().unwrap_err();
        assert_eq!(error.code(), "session_busy");
        assert!(error.message().contains("exhausted"));

        // And it keeps failing rather than recovering into a wrapped sequence.
        assert!(history.allocate_id().is_err());
    }

    #[test]
    fn allocation_and_insertion_agree_on_the_sequence() {
        let mut history = FrameHistory::new(4).unwrap();
        for expected in 1..=10u64 {
            let id = capture_into(&mut history, 0);
            assert_eq!(id, FrameId(expected));
        }
        assert_eq!(history.next_frame_id(), FrameId(11));
    }

    #[test]
    fn the_summary_reports_both_ends_and_the_byte_cost() {
        let mut history = FrameHistory::new(3).unwrap();
        for marker in 0..5u8 {
            capture_into(&mut history, marker);
        }

        let summary = history.summary();
        assert_eq!(summary.capacity, 3);
        assert_eq!(summary.retained, 3);
        assert_eq!(summary.oldest_frame_id, Some(FrameId(3)));
        assert_eq!(summary.newest_frame_id, Some(FrameId(5)));
        assert_eq!(summary.captured_total, 5);
        // 2x2 RGB8 = 12 bytes per frame, 3 retained.
        assert_eq!(summary.retained_bytes, 36);
    }

    #[test]
    fn frame_age_is_measured_from_the_capture_instant() {
        let frame = session_frame(FrameId(1), 0);
        let captured = frame.captured_at;
        let later = captured + Duration::from_millis(250);
        assert_eq!(frame.age_at(later), Duration::from_millis(250));
        // An earlier "now" clamps to zero rather than underflowing.
        assert_eq!(
            frame.age_at(captured - Duration::from_millis(5)),
            Duration::ZERO
        );
    }

    #[test]
    fn frame_ids_serialize_as_numbers() {
        let value = serde_json::to_value(FrameId(1842)).unwrap();
        assert_eq!(value, serde_json::json!(1842));
        let back: FrameId = serde_json::from_value(value).unwrap();
        assert_eq!(back, FrameId(1842));
    }

    #[test]
    fn ordering_is_numeric_not_lexicographic() {
        assert!(FrameId(2) < FrameId(10));
        assert!(FrameId(9) < FrameId(100));
        let mut ids = vec![FrameId(10), FrameId(2), FrameId(100), FrameId(9)];
        ids.sort();
        assert_eq!(ids, vec![FrameId(2), FrameId(9), FrameId(10), FrameId(100)]);
    }
}
