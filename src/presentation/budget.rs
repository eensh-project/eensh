//! Deterministic payload fitting.
//!
//! # Why this is a ladder and not an optimiser
//!
//! The problem — choose presentation settings so the base64 payload fits a budget,
//! degrading the least valuable things first — has an obvious greedy answer, and a
//! sophisticated global optimiser would be more code, more risk, and *less*
//! predictable. What matters instead is that the process is **deterministic and
//! explainable**: same input, same output, and every change reported (requirements
//! 21, 23, 51, 66).
//!
//! # The ladder
//!
//! Rungs are applied in a fixed order, each stepping every eligible view one rung
//! down its own ladder, and the whole ladder is re-walked until the budget fits:
//!
//! ```text
//! 1. lower older-frame quality     (cheapest loss: motion context stays legible)
//! 2. lower older-frame resolution
//! 3. omit optional older images    (identity and timing remain)
//! 4. lower newest quality          (the newest frame starts being degraded)
//! 5. lower newest resolution
//! then: fail with payload_budget_exceeded
//! ```
//!
//! The newest frame is protected longer than the older context because the newest
//! frame is the reason the caller is observing at all. That is the entire point of
//! the ordering, and it is tested by asserting *which element changed* rather than
//! only that the total fell.
//!
//! # Determinism
//!
//! Views are ordered by a total key — priority, then frame position, then kind, then
//! source rectangle, then name — so no step depends on hash iteration order or on the
//! order a caller happened to list regions in. Two identical requests produce
//! identical plans (requirement 51).

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::presentation::policy::{
    ImageFloors, PayloadBudget, ResizePolicy, QUALITY_LADDER, WIDTH_LADDER,
};
use crate::presentation::view::{encode_view, Plan, ViewKind, ViewPlan};
use crate::presentation::PresentableFrame;

/// What the fitter had to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PayloadFit {
    /// The budget the caller set.
    pub budget_base64_bytes: usize,
    /// What the payload actually came to.
    pub actual_base64_bytes: usize,
    /// Whether anything had to change.
    pub fit: PayloadFitState,
    /// Every adjustment, in the order it was applied.
    pub adjustments: Vec<PayloadAdjustment>,
    /// Time spent in the fitting loop, including re-encodes.
    #[serde(skip)]
    pub fit_us: u64,
}

impl PayloadFit {
    /// Attach the measured fitting time.
    pub fn with_fit_time(mut self, fit_us: u64) -> Self {
        self.fit_us = fit_us;
        self
    }

    /// Whether the payload ended up within budget.
    pub fn within_budget(&self) -> bool {
        self.actual_base64_bytes <= self.budget_base64_bytes
    }
}

/// How the payload related to the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadFitState {
    /// It fit as requested; nothing was changed.
    Exact,
    /// Something was reduced or omitted to fit.
    Adjusted,
}

impl PayloadFitState {
    /// The name used in a response.
    pub fn name(self) -> &'static str {
        match self {
            PayloadFitState::Exact => "exact",
            PayloadFitState::Adjusted => "adjusted",
        }
    }
}

/// One change the fitter made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum PayloadAdjustment {
    /// A view's quality was lowered.
    Quality {
        /// The frame whose view changed.
        frame_id: crate::session::FrameId,
        /// The view's name.
        view: String,
        /// What was asked for.
        requested: u8,
        /// What was applied.
        actual: u8,
    },
    /// A view's resolution was lowered.
    Resolution {
        /// The frame whose view changed.
        frame_id: crate::session::FrameId,
        /// The view's name.
        view: String,
        /// The requested width, when one was named.
        requested_width: Option<u32>,
        /// The applied width.
        actual_width: u32,
    },
    /// A view was dropped entirely.
    Omitted {
        /// The frame whose view was dropped.
        frame_id: crate::session::FrameId,
        /// The view's name.
        view: String,
        /// Why it was dropped.
        reason: String,
    },
}

/// Which rung of the ladder a pass applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rung {
    /// Lower older-frame quality.
    OlderQuality,
    /// Lower older-frame resolution.
    OlderResolution,
    /// Omit optional images.
    ///
    /// Applies to every frame, including the newest. An optional view is one the
    /// caller has already said it can do without (requirement 29), so the fitter is
    /// allowed to drop it — and must be able to, or a single-frame request could
    /// never shed an optional region, since its only frame is the newest one.
    OmitOptional,
    /// Lower newest quality.
    NewestQuality,
    /// Lower newest resolution.
    NewestResolution,
}

impl Rung {
    /// Every rung, in order.
    const ALL: [Rung; 5] = [
        Rung::OlderQuality,
        Rung::OlderResolution,
        Rung::OmitOptional,
        Rung::NewestQuality,
        Rung::NewestResolution,
    ];
}

/// Fit a plan's payload into a budget.
///
/// Returns the fitted plan and a report of what changed.
///
/// The loop is monotone: every rung only ever lowers a quality, lowers a resolution,
/// or removes an image, and each is bounded from below by its floor. It therefore
/// terminates. A step counter is kept anyway, because a loop whose termination rests
/// on an argument is worth being able to bound in practice as well as in principle.
pub fn fit_plan(
    plan: Plan,
    frames: &[PresentableFrame],
    budget: &PayloadBudget,
) -> Result<(Plan, PayloadFit), Error> {
    let mut cache = MeasureCache::new();
    // Kept as well as moved into `current`, because the failure path needs the *preferred* plan to
    // compute the floor from, not the fully degraded plan the fitting loop arrived at.
    let preferred = plan.clone();
    let mut current = plan;
    let mut adjustments = Vec::new();

    let mut measured = cache.measure(&current, frames)?;
    if measured <= budget.max_base64_bytes {
        // Requirement 24: no budget pressure means no opportunistic adaptation.
        return Ok((
            current,
            PayloadFit {
                budget_base64_bytes: budget.max_base64_bytes,
                actual_base64_bytes: measured,
                fit: PayloadFitState::Exact,
                adjustments,
                fit_us: 0,
            },
        ));
    }

    let mut guard = 0;
    'ladder: for rung in Rung::ALL {
        // Each rung is applied *repeatedly until it can do no more*, before the next
        // rung is considered. That is what makes the ordering a real protection rather
        // than a formality: if the rungs advanced in lockstep, one pass would lower the
        // older frames and the newest frame by one step each, and the newest would be
        // degraded while the older frames were still nowhere near their floors — which
        // is precisely the outcome the ordering exists to prevent.
        loop {
            guard += 1;
            if guard > 512 {
                // Unreachable while every rung is monotone. Retained as a backstop so a
                // future rung that is accidentally non-monotone fails loudly rather
                // than spinning with a caller's request held open.
                return Err(Error::internal(
                    "payload fitting did not converge; a fitting rung is not monotone",
                ));
            }

            if !apply(rung, &mut current, frames, &mut adjustments) {
                break;
            }
            measured = cache.measure(&current, frames)?;
            if measured <= budget.max_base64_bytes {
                break 'ladder;
            }
        }
    }

    if measured <= budget.max_base64_bytes {
        return Ok((
            current,
            PayloadFit {
                budget_base64_bytes: budget.max_base64_bytes,
                actual_base64_bytes: measured,
                fit: PayloadFitState::Adjusted,
                adjustments,
                fit_us: 0,
            },
        ));
    }

    // The required minimum does not fit. Reported with the *real* floor, computed by walking the
    // same ladder to exhaustion, so the caller is told the number to raise the budget to rather
    // than only that its own was too small. The floor has to be recomputed rather than taken from
    // `measured`, because the plan `fit_plan` left behind is the fully degraded one, and that is
    // not always the cheapest — see the note on `minimum_payload`.
    let (_, floor) = minimum_payload(&preferred, frames)?;
    Err(Error::payload_budget_exceeded(format!(
        "the required views cannot fit in a budget of {} base64 bytes even at their \
         minimum settings; the smallest achievable payload is {floor} bytes. Raise the \
         budget, lower the floors, or make a view optional.",
        budget.max_base64_bytes
    )))
}

/// Apply one rung in one pass, returning whether anything changed.
fn apply(
    rung: Rung,
    plan: &mut Plan,
    frames: &[PresentableFrame],
    adjustments: &mut Vec<PayloadAdjustment>,
) -> bool {
    match rung {
        Rung::OlderQuality => lower_quality(plan, false, adjustments),
        Rung::OlderResolution => lower_resolution(plan, frames, false, adjustments),
        Rung::OmitOptional => omit_optional(plan, adjustments),
        Rung::NewestQuality => lower_quality(plan, true, adjustments),
        Rung::NewestResolution => lower_resolution(plan, frames, true, adjustments),
    }
}

/// Lower quality one ladder step on the eligible views.
fn lower_quality(plan: &mut Plan, newest: bool, adjustments: &mut Vec<PayloadAdjustment>) -> bool {
    let mut changed = false;

    for view in order_for_fitting(plan) {
        if !view.has_image() || view.is_newest != newest {
            continue;
        }
        // A PNG view has no quality to lower, which is why the resolution rung is the
        // only lever for PNG (requirement 25).
        if view.image.format != crate::encode::ImageFormat::Jpeg {
            continue;
        }
        if let Some(lowered) = next_quality(view.image.quality, view.image.floors) {
            let requested = view.image.quality;
            view.image = view.image.with_quality(lowered);
            adjustments.push(PayloadAdjustment::Quality {
                frame_id: view.frame_id,
                view: view.kind.name().to_string(),
                requested,
                actual: lowered,
            });
            changed = true;
        }
    }

    changed
}

/// Lower resolution one ladder step on the eligible views.
fn lower_resolution(
    plan: &mut Plan,
    frames: &[PresentableFrame],
    newest: bool,
    adjustments: &mut Vec<PayloadAdjustment>,
) -> bool {
    let mut changed = false;

    for view in order_for_fitting(plan) {
        if !view.has_image() || view.is_newest != newest {
            continue;
        }
        let current = width_of(view, frames);
        if let Some(lowered) = next_width(current, view.image.floors) {
            view.image = view
                .image
                .with_resize(ResizePolicy::Width { width: lowered });
            adjustments.push(PayloadAdjustment::Resolution {
                frame_id: view.frame_id,
                view: view.kind.name().to_string(),
                requested_width: Some(current),
                actual_width: lowered,
            });
            changed = true;
        }
    }

    changed
}

/// Drop optional images.
///
/// The fitting order already sorts the least valuable views first, so the oldest and
/// lowest-priority optional views are dropped before the newest frame's optional ones
/// — and a *required* view is never dropped here, whatever its frame.
fn omit_optional(plan: &mut Plan, adjustments: &mut Vec<PayloadAdjustment>) -> bool {
    let mut changed = false;

    for view in order_for_fitting(plan) {
        if !view.has_image() || view.required {
            continue;
        }
        view.image = crate::presentation::policy::ImagePolicy::metadata_only();
        adjustments.push(PayloadAdjustment::Omitted {
            frame_id: view.frame_id,
            view: view.kind.name().to_string(),
            reason: "optional view omitted to meet the payload budget".to_string(),
        });
        changed = true;
    }

    changed
}

/// The view's quality, when it has one.
///
/// Used by the fitting loop's reporting and by tests; the attribute keeps the
/// compiler from calling it dead in a build that does not run the tests.
#[cfg_attr(not(test), allow(dead_code))]
fn quality_of(view: &ViewPlan) -> Option<u8> {
    (view.image.format == crate::encode::ImageFormat::Jpeg).then_some(view.image.quality)
}

/// The view's current effective width.
///
/// A native-resolution view has no width until it is encoded, so the frame's own
/// width stands in. Deciding on the plan rather than on encoded pixels keeps the
/// ladder a matter of policy instead of a search over images.
fn width_of(view: &ViewPlan, frames: &[PresentableFrame]) -> u32 {
    let source_width = frames[view.frame_index].width();
    match view.image.resize {
        ResizePolicy::Width { width } => width,
        ResizePolicy::Height { height } => {
            let source_height = frames[view.frame_index].height().max(1);
            ((source_width as u64 * height as u64) / source_height as u64).max(1) as u32
        }
        ResizePolicy::Scale { factor } => (source_width as f64 * factor).round().max(1.0) as u32,
        ResizePolicy::None => source_width,
    }
}

/// The next lower quality from the ladder, honouring the floor.
fn next_quality(current: u8, floors: Option<ImageFloors>) -> Option<u8> {
    let floor = floors.and_then(|f| f.min_quality).unwrap_or(u8::MIN);
    QUALITY_LADDER
        .iter()
        .copied()
        .filter(|step| *step < current && *step >= floor)
        .max()
}

/// Whether the fitter reported any change to the frame with this id.
///
/// Used by tests to state requirement 21 as a claim about a report rather than as a
/// claim about the shape of a final plan.
#[cfg_attr(not(test), allow(dead_code))]
fn newest_was_degraded(report: &PayloadFit, newest_id: u64) -> bool {
    report.adjustments.iter().any(|adjustment| {
        let frame_id = match adjustment {
            PayloadAdjustment::Quality { frame_id, .. }
            | PayloadAdjustment::Resolution { frame_id, .. }
            | PayloadAdjustment::Omitted { frame_id, .. } => frame_id,
        };
        frame_id.get() == newest_id
    })
}

/// Whether every older view has spent both of its levers.
///
/// "Nothing left to give" is bounded by the ladder, not by the floor: the quality
/// rungs step down a fixed ladder and stop when the next step would fall below the
/// floor, so an older view can come to rest slightly above its floor. The honest claim
/// is therefore that each older view sits at the cheapest step the ladder still offers
/// it — a claim about the ladder rather than about the floor.
#[cfg_attr(not(test), allow(dead_code))]
fn older_views_exhausted(plan: &Plan, frames: &[PresentableFrame]) -> bool {
    plan.views
        .iter()
        .filter(|view| !view.is_newest && view.has_image())
        .all(|view| {
            let floors = view.image.floors;
            let quality_exhausted = quality_of(view)
                .map(|quality| next_quality(quality, floors).is_none())
                .unwrap_or(true);
            let width_exhausted = next_width(width_of(view, frames), floors).is_none();
            quality_exhausted && width_exhausted
        })
}

/// The next lower width from the ladder, honouring the floor.
fn next_width(current: u32, floors: Option<ImageFloors>) -> Option<u32> {
    let floor = floors.and_then(|f| f.min_width).unwrap_or(u32::MIN);
    WIDTH_LADDER
        .iter()
        .copied()
        .filter(|step| *step < current && *step >= floor)
        .max()
}

/// The order the fitter degrades views in: least valuable first.
///
/// Expressed as a single tuple key rather than a chain of `then_with`s, so the
/// ordering is a value that can be read, compared, and tested directly. The key is
/// total — two distinct views never compare equal — which is what makes the fitted
/// plan a reproducible value rather than something that merely happens to come out
/// the same (requirement 51).
///
/// Components, in order of significance:
///
/// ```text
/// 1. priority          ascending, so the least protected view is at the head
/// 2. frame position    oldest first, so older context degrades before newer
/// 3. kind              overview before regions before a changed crop
/// 4. source rectangle  whole frame before crops, then left to right, top to bottom
/// 5. name              the final tie-break, so the key is total
/// ```
fn fitting_key(view: &ViewPlan) -> (u8, usize, u8, u8, i32, i32, i32, i32, String) {
    let (is_crop, x, y, width, height) = source_key(&view.source);
    (
        view.priority,
        view.frame_index,
        kind_rank(&view.kind),
        is_crop,
        x,
        y,
        width,
        height,
        view.kind.name().to_string(),
    )
}

fn order_for_fitting(plan: &mut Plan) -> Vec<&mut ViewPlan> {
    let mut indexed: Vec<usize> = (0..plan.views.len()).collect();
    indexed.sort_by_key(|index| fitting_key(&plan.views[*index]));

    // The plan's views are reordered to match, so the fitted plan is itself a
    // stable, comparable value and a caller sees a consistent order regardless of
    // the order it declared its regions in.
    plan.views = indexed.iter().map(|i| plan.views[*i].clone()).collect();
    plan.views.iter_mut().collect()
}

fn kind_rank(kind: &ViewKind) -> u8 {
    match kind {
        // Among views of equal priority, the overview is the most valuable single
        // view, so it degrades last; a changed crop is the most disposable because it
        // is the most specific thing about one particular pair of frames.
        ViewKind::Overview => 0,
        ViewKind::Region { .. } => 1,
        ViewKind::ChangedRegion => 2,
    }
}

fn source_key(source: &crate::presentation::view::ViewSource) -> (u8, i32, i32, i32, i32) {
    match source {
        crate::presentation::view::ViewSource::Whole => (0, 0, 0, 0, 0),
        crate::presentation::view::ViewSource::Crop { rect } => {
            (1, rect.x, rect.y, rect.width as i32, rect.height as i32)
        }
    }
}

/// A within-request cache of measured payload sizes.
///
/// Each pass wants to know what the plan costs, and re-encoding every unchanged view
/// at every rung would make fitting cost more than the work it is trying to save. The
/// cache is keyed on everything that affects the encoded size, so a hit is only ever
/// a *correct* hit — and it is dropped when the request finishes, because there is
/// deliberately no persistent cache of encoded variants (requirement 59).
struct MeasureCache {
    sizes: std::collections::HashMap<String, usize>,
}

impl MeasureCache {
    fn new() -> Self {
        MeasureCache {
            sizes: std::collections::HashMap::new(),
        }
    }

    /// Total base64 bytes for the plan's image views.
    fn measure(&mut self, plan: &Plan, frames: &[PresentableFrame]) -> Result<usize, Error> {
        let mut total = 0usize;
        for view in plan.views.iter().filter(|v| v.has_image()) {
            let key = measure_key(view);
            if let Some(size) = self.sizes.get(&key) {
                total += size;
                continue;
            }
            let size = self.encode_size(view, frames)?;
            self.sizes.insert(key, size);
            total += size;
        }
        Ok(total)
    }

    fn encode_size(&self, view: &ViewPlan, frames: &[PresentableFrame]) -> Result<usize, Error> {
        let encoded = encode_view(&frames[view.frame_index].frame, view)?;
        // Base64 length is read from the actual string rather than estimated from the
        // encoded size: an estimate would be wrong by the padding, and the budget is
        // expressed in the unit that actually travels.
        Ok(encoded
            .image
            .as_ref()
            .and_then(|image| image.data.as_ref())
            .map(String::len)
            .unwrap_or(0))
    }
}

/// Everything about a view that can change its encoded size.
///
/// The cache is keyed on this string rather than the key being a struct, because the
/// key is only ever compared and stored — never inspected — and the allocation is
/// insignificant next to the encode it avoids.
fn measure_key(view: &ViewPlan) -> String {
    let (source, x, y, width, height) = source_key(&view.source);
    let resize = match view.image.resize {
        ResizePolicy::None => 0,
        ResizePolicy::Width { width } => width as i64,
        ResizePolicy::Height { height } => -(height as i64),
        ResizePolicy::Scale { factor } => (factor * 1000.0) as i64 + 1_000_000,
    };
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}|{}|{}",
        view.frame_index,
        view.frame_id.get(),
        view.kind.name(),
        source,
        x,
        y,
        width,
        height,
        view.image.format.name(),
        view.image.quality,
        resize,
        view.image.metadata_only,
        view.image.base64,
    )
}

/// The smallest payload the required views can be reduced to.
///
/// Applies every rung to exhaustion, ignoring any budget, and reports what the plan
/// costs at its floors. This is what makes "the budget cannot be met" an actionable
/// error: the caller is told the actual floor rather than only that its number was
/// too small, and it is what lets tests pick budgets by measurement instead of by
/// guesswork.
///
/// # It must mirror the fitter, and it must never exceed the unfitted cost
///
/// Two properties that a floor has to have to be worth reporting, and neither is free:
///
/// * The rungs are applied **in the same order and to the same exhaustion as the fitter
///   does**, because a floor that is not reachable by the ladder the fitter walks is not a
///   floor. An earlier version applied all five rungs in lockstep in each pass, which walks a
///   different path through the plan space.
/// * The result is the cheapest of the plans the rungs can reach, which is **not** always the
///   fully degraded one. JPEG does not shrink monotonically with width: a resized image can
///   carry a larger header and a worse block structure than the original, so the most-degraded
///   plan can cost more than the preferred one. Returning that as "the floor" would tell a
///   caller that a budget above its own unfitted payload was impossible. The unfitted plan is
///   always a candidate, so it is always included in the comparison.
pub fn minimum_payload(plan: &Plan, frames: &[PresentableFrame]) -> Result<(Plan, usize), Error> {
    let mut cache = MeasureCache::new();
    let mut current = plan.clone();
    let mut adjustments = Vec::new();

    let mut best_plan = current.clone();
    let mut best_size = cache.measure(&best_plan, frames)?;

    for rung in Rung::ALL {
        loop {
            if !apply(rung, &mut current, frames, &mut adjustments) {
                break;
            }
            let size = cache.measure(&current, frames)?;
            if size < best_size {
                best_size = size;
                best_plan = current.clone();
            }
        }
    }

    Ok((best_plan, best_size))
}

/// Measure a plan without fitting it. Used by reporting and tests.
pub fn measure_plan(plan: &Plan, frames: &[PresentableFrame]) -> Result<usize, Error> {
    MeasureCache::new().measure(plan, frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Frame, PixelBuffer, PixelFormat};
    use crate::geometry::{Rect, SourceGeometry};
    use crate::presentation::policy::{
        ImagePolicy, ObservationPolicy, RegionPolicy, TemporalFramePolicy,
    };
    use crate::presentation::view::{plan_for_frame, plan_for_stack, ViewSource};
    use crate::session::FrameId;
    use std::sync::Arc;
    use std::time::Instant;

    /// A frame with real, varied content, so encoders produce a realistic size.
    ///
    /// A flat colour compresses to almost nothing in PNG, which would make a budget
    /// test pass for the wrong reason: the payload would fit not because the fitter
    /// worked but because there was nothing to encode.
    fn textured(width: u32, height: u32, seed: u8) -> Arc<Frame> {
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                let n = ((x * 7 + y * 13) as u8) ^ seed;
                data.extend_from_slice(&[n, n.wrapping_mul(3), n.wrapping_add(91)]);
            }
        }
        let pixels = PixelBuffer::new(width, height, PixelFormat::Rgb8, data).unwrap();
        Arc::new(Frame::new(
            SourceGeometry::desktop(None, width, height),
            pixels,
            Instant::now(),
        ))
    }

    fn presentable(id: u64, width: u32, height: u32, seed: u8) -> PresentableFrame {
        PresentableFrame {
            session_id: "s".into(),
            frame_id: FrameId(id),
            frame: textured(width, height, seed),
            captured_at: Instant::now(),
            capture_offset: None,
            capture_duration: None,
        }
    }

    fn stack(count: usize, width: u32, height: u32) -> Vec<PresentableFrame> {
        (0..count)
            .map(|i| presentable(i as u64 + 1, width, height, (i as u8).wrapping_mul(40)))
            .collect()
    }

    /// Fit, tolerating a refusal, and report both outcomes.
    fn try_fit(plan: Plan, frames: &[PresentableFrame], max: usize) -> (Option<Plan>, PayloadFit) {
        match fit_plan(plan, frames, &PayloadBudget::new(max)) {
            Ok((plan, report)) => (Some(plan), report),
            Err(_error) => (
                None,
                PayloadFit {
                    budget_base64_bytes: max,
                    actual_base64_bytes: 0,
                    fit: PayloadFitState::Adjusted,
                    adjustments: Vec::new(),
                    fit_us: 0,
                },
            ),
        }
    }

    /// A budget strictly between two measured payloads.
    ///
    /// Tests take their budgets from measurements rather than from guessed numbers,
    /// so a test that fails is failing about behaviour rather than about an encoder's
    /// exact output size on one machine.
    fn between(lower: usize, upper: usize) -> usize {
        assert!(lower < upper, "need room between {lower} and {upper}");
        lower + (upper - lower) / 2
    }

    fn fitted(plan: Plan, frames: &[PresentableFrame], max: usize) -> (Plan, PayloadFit) {
        fit_plan(plan, frames, &PayloadBudget::new(max))
            .unwrap_or_else(|error| panic!("fitting failed for budget {max}: {}", error.message()))
    }

    fn three_frame_policy(quality: u8) -> ObservationPolicy {
        ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(128, quality)),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::AllSame {
                image: ImagePolicy::jpeg_width(128, quality),
            }),
            payload_budget: None,
            changed: None,
        }
    }

    #[test]
    fn a_plan_that_already_fits_is_left_completely_alone() {
        // Requirement 24: adaptation is opt-in. A generous budget must not cause an
        // opportunistic quality reduction.
        let frames = stack(1, 64, 48);
        let policy = ObservationPolicy::phase5_default(ImagePolicy::jpeg_width(32, 75));
        let plan = plan_for_frame(&frames[0], &policy).unwrap();

        let before = plan.views[0].image;
        let (fitted, report) = fitted(plan, &frames, 10_000_000);

        assert_eq!(report.fit, PayloadFitState::Exact);
        assert!(report.adjustments.is_empty());
        assert_eq!(fitted.views[0].image, before, "nothing changed");
        assert!(report.within_budget());
    }

    #[test]
    fn a_small_overrun_reduces_older_quality_without_touching_the_newest() {
        // Requirement 50's "reduce older quality" scenario, which is rung 1. The
        // budget is derived from a measurement rather than guessed, so the test
        // asserts the *behaviour* rather than a lucky number.
        let frames = stack(3, 128, 96);
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(128, 85)),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::NewestDetailed {
                older: ImagePolicy::jpeg_width(128, 85),
                newest: ImagePolicy::jpeg_width(128, 85),
            }),
            payload_budget: None,
            changed: None,
        };
        let plan = plan_for_stack(&frames, &policy).unwrap();
        let preferred = measure_plan(&plan, &frames).unwrap();

        // The newest frame alone, which is the smallest payload that this ladder can
        // reach without touching the newest at all.
        let newest_only = plan_for_frame(
            frames.last().unwrap(),
            &ObservationPolicy::phase5_default(ImagePolicy::jpeg_width(128, 85)),
        )
        .unwrap();
        let newest_floor = measure_plan(&newest_only, &[frames.last().unwrap().clone()]).unwrap();

        // A budget between the two: reachable by discarding older detail, not by
        // doing nothing.
        let budget = between(newest_floor, preferred);
        let (fitted, report) = fitted(plan, &frames, budget);

        assert_eq!(report.fit, PayloadFitState::Adjusted);
        assert!(report.within_budget());

        let newest_id = frames.last().unwrap().frame_id.get();
        assert!(
            report.adjustments.iter().all(|a| match a {
                PayloadAdjustment::Quality { frame_id, .. }
                | PayloadAdjustment::Resolution { frame_id, .. }
                | PayloadAdjustment::Omitted { frame_id, .. } => frame_id.get() != newest_id,
            }),
            "no adjustment should name the newest frame: {:?}",
            report.adjustments
        );

        // The older frames paid for it, by whichever lever the ladder reached first.
        assert!(
            fitted
                .views
                .iter()
                .filter(|v| !v.is_newest)
                .any(|v| !v.has_image()
                    || quality_of(v).is_some_and(|q| q < 85)
                    || v.image.resize.width().is_some_and(|w| w < 128)),
            "the older frames should have been degraded: {:?}",
            report.adjustments
        );
    }

    #[test]
    fn older_resolution_is_reduced_only_after_older_quality_is_exhausted() {
        // Requirement 50's ordering assertion, with floors low enough that both rungs
        // are reachable and the order between them is observable.
        let frames = stack(4, 256, 192);
        let mut older = ImagePolicy::jpeg_width(256, 85);
        older.floors = Some(ImageFloors {
            min_width: Some(128),
            min_quality: Some(65),
        });
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(256, 85)),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::NewestDetailed {
                older,
                newest: ImagePolicy::jpeg_width(256, 85),
            }),
            payload_budget: None,
            changed: None,
        };
        let plan = plan_for_stack(&frames, &policy).unwrap();
        let preferred = measure_plan(&plan, &frames).unwrap();
        let (_, cheapest) = minimum_payload(&plan, &frames).unwrap();

        // Halfway up the ladder, so quality is lowered first and resolution has to
        // follow.
        let (_, report) = fitted(plan, &frames, between(cheapest, preferred));

        let first_resolution = report
            .adjustments
            .iter()
            .position(|a| matches!(a, PayloadAdjustment::Resolution { .. }));
        let first_quality = report
            .adjustments
            .iter()
            .position(|a| matches!(a, PayloadAdjustment::Quality { .. }));

        if let Some(first_res) = first_resolution {
            assert!(
                first_quality.is_some_and(|q| q < first_res),
                "resolution was reduced before any quality was: {:?}",
                report.adjustments
            );
        }

        for adjustment in &report.adjustments {
            if let PayloadAdjustment::Quality { actual, .. } = adjustment {
                assert!(*actual >= 65, "quality floor was violated: {actual}");
            }
        }
        assert!(report.within_budget());
    }

    #[test]
    fn an_optional_view_is_omitted_before_the_newest_is_degraded() {
        // Requirements 29 and 50: optional content drops first.
        let frames = stack(1, 256, 192);
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(256, 80)),
            regions: vec![RegionPolicy::optional(
                "minimap",
                Rect::new(0, 0, 128, 128).unwrap(),
                ImagePolicy::jpeg_width(128, 80),
            )],
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        let plan = plan_for_frame(&frames[0], &policy).unwrap();
        let preferred = measure_plan(&plan, &frames).unwrap();
        let (_, floor) = minimum_payload(&plan, &frames).unwrap();

        // Between the floor and the preferred payload, so the fitter must shed the
        // optional view: the required overview alone is cheaper than this.
        let (fitted, report) = fitted(plan, &frames, between(floor, preferred));

        assert!(report.within_budget());
        let omitted: Vec<&str> = report
            .adjustments
            .iter()
            .filter_map(|a| match a {
                PayloadAdjustment::Omitted { view, .. } => Some(view.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            omitted.contains(&"minimap"),
            "the optional view should have been dropped: {omitted:?}"
        );

        // The required overview is still there, and still an image.
        assert!(fitted
            .views
            .iter()
            .any(|v| matches!(v.kind, ViewKind::Overview) && v.has_image()));
    }

    #[test]
    fn a_required_view_is_never_omitted() {
        let frames = stack(1, 128, 96);
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(128, 70)),
            regions: vec![RegionPolicy::required(
                "hud",
                Rect::new(0, 0, 64, 64).unwrap(),
                ImagePolicy::jpeg_width(64, 70),
            )],
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        let plan = plan_for_frame(&frames[0], &policy).unwrap();

        // Strictly below the floor: this must fail rather than drop the region.
        let (_, cheapest) = minimum_payload(&plan, &frames).unwrap();
        assert!(cheapest > 1, "the floor must leave room to go below it");
        let error = fit_plan(plan, &frames, &PayloadBudget::new(cheapest - 1)).unwrap_err();
        assert_eq!(error.code(), "payload_budget_exceeded");
    }

    #[test]
    fn an_unreachable_budget_reports_how_far_short_it_fell() {
        let frames = stack(2, 192, 144);
        let policy = ObservationPolicy::phase5_default(ImagePolicy::jpeg_width(192, 75));
        let plan = plan_for_stack(&frames, &policy).unwrap();

        let error = fit_plan(plan, &frames, &PayloadBudget::new(64)).unwrap_err();
        assert_eq!(error.code(), "payload_budget_exceeded");
        assert!(
            error.message().contains("smallest achievable payload"),
            "the message should say how far short it fell: {}",
            error.message()
        );
    }

    #[test]
    fn the_reported_floor_matches_a_measured_minimum() {
        // The floor quoted in the failure message must be a number the caller can
        // act on, so it is checked against an independent computation of it.
        let frames = stack(3, 192, 144);
        let mut image = ImagePolicy::jpeg_width(192, 80);
        image.floors = Some(ImageFloors::older());
        let policy = ObservationPolicy {
            overview: Some(image),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::AllSame { image }),
            payload_budget: None,
            changed: None,
        };
        let plan = plan_for_stack(&frames, &policy).unwrap();

        let (_, floor) = minimum_payload(&plan, &frames).unwrap();
        let error = fit_plan(plan, &frames, &PayloadBudget::new(floor - 1)).unwrap_err();
        assert!(
            error.message().contains(&floor.to_string()),
            "the message should quote the real floor ({floor}): {}",
            error.message()
        );
    }

    #[test]
    fn fitting_is_deterministic_across_repeated_runs() {
        // Requirement 51: identical input, identical output. Regions are listed in a
        // deliberately awkward order to catch an implementation that sorts by
        // iteration order rather than by a total key.
        let frames = stack(3, 192, 144);
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(192, 80)),
            regions: vec![
                RegionPolicy::optional(
                    "zulu",
                    Rect::new(0, 0, 64, 64).unwrap(),
                    ImagePolicy::jpeg_width(64, 80),
                ),
                RegionPolicy::required(
                    "alpha",
                    Rect::new(64, 0, 64, 64).unwrap(),
                    ImagePolicy::jpeg_width(64, 80),
                ),
                RegionPolicy::optional(
                    "mike",
                    Rect::new(0, 64, 64, 64).unwrap(),
                    ImagePolicy::jpeg_width(64, 80),
                ),
            ],
            temporal: Some(TemporalFramePolicy::NewestDetailed {
                older: ImagePolicy::jpeg_width(96, 60),
                newest: ImagePolicy::jpeg_width(192, 85),
            }),
            payload_budget: None,
            changed: None,
        };

        let reference = plan_for_stack(&frames, &policy).unwrap();
        let preferred = measure_plan(&reference, &frames).unwrap();
        let (_, floor) = minimum_payload(&reference, &frames).unwrap();
        let budget = between(floor, preferred);

        let mut signatures = Vec::new();
        for _ in 0..5 {
            let plan = plan_for_stack(&frames, &policy).unwrap();
            let (fitted, report) = fitted(plan, &frames, budget);
            let sizes: Vec<(String, u64, Option<u32>, Option<u8>)> = fitted
                .views
                .iter()
                .map(|v| {
                    (
                        v.kind.name().to_string(),
                        v.frame_id.get(),
                        v.image.resize.width(),
                        quality_of(v),
                    )
                })
                .collect();
            signatures.push((sizes, report.actual_base64_bytes));
        }

        let first = &signatures[0];
        for other in &signatures[1..] {
            assert_eq!(
                first, other,
                "fitting produced a different plan on a repeated run"
            );
        }
    }

    #[test]
    fn fitting_never_exceeds_the_budget_when_it_succeeds() {
        let frames = stack(4, 200, 150);
        let policy = three_frame_policy(80);
        let reference = plan_for_stack(&frames, &policy).unwrap();
        let preferred = measure_plan(&reference, &frames).unwrap();
        let (_, floor) = minimum_payload(&reference, &frames).unwrap();

        // Span the whole range from reachable to impossible.
        let span = preferred - floor;
        for step in [0usize, 1, 2, 3, 4] {
            let budget = preferred - (span * step) / 4;
            let plan = plan_for_stack(&frames, &policy).unwrap();
            match fit_plan(plan, &frames, &PayloadBudget::new(budget)) {
                Ok((_, report)) => assert!(
                    report.actual_base64_bytes <= budget,
                    "budget {budget} was exceeded: {}",
                    report.actual_base64_bytes
                ),
                Err(error) => assert_eq!(error.code(), "payload_budget_exceeded"),
            }
        }
    }

    #[test]
    fn adjustments_are_reported_with_the_numbers_that_changed() {
        let frames = stack(3, 160, 120);
        let policy = three_frame_policy(85);
        let reference = plan_for_stack(&frames, &policy).unwrap();
        let preferred = measure_plan(&reference, &frames).unwrap();

        let (_, floor) = minimum_payload(&reference, &frames).unwrap();
        let plan = plan_for_stack(&frames, &policy).unwrap();
        let (_, report) = fitted(plan, &frames, between(floor, preferred));

        assert_eq!(report.fit, PayloadFitState::Adjusted);
        assert!(!report.adjustments.is_empty(), "something had to change");
        for adjustment in &report.adjustments {
            match adjustment {
                PayloadAdjustment::Quality {
                    requested, actual, ..
                } => assert!(actual < requested, "a quality cut must actually cut"),
                PayloadAdjustment::Resolution {
                    requested_width,
                    actual_width,
                    ..
                } => assert!(
                    requested_width.is_none_or(|r| actual_width < &r),
                    "a resolution cut must actually cut"
                ),
                PayloadAdjustment::Omitted { reason, .. } => assert!(!reason.is_empty()),
            }
        }
    }

    #[test]
    fn the_newest_frame_is_not_degraded_while_a_cheaper_older_option_remains() {
        // The central promise of requirement 21: the newest frame is protected longer
        // than the older context. It is stated as a sweep rather than at one guessed
        // budget, because a single midpoint can land beyond what the older frames are
        // able to absorb — at 256px wide the width ladder offers no step below, so
        // quality is their only lever — and the test would then be asserting something
        // untrue about a correct fitter. Sweeping makes the claim the real invariant:
        // *whenever* the newest is touched, the older views have nothing left to give.
        let frames = stack(4, 256, 192);
        let older = ImagePolicy {
            floors: Some(ImageFloors {
                min_width: Some(32),
                min_quality: Some(20),
            }),
            ..ImagePolicy::jpeg_width(256, 70)
        };
        let newest = ImagePolicy {
            floors: Some(ImageFloors {
                min_width: Some(32),
                min_quality: Some(20),
            }),
            ..ImagePolicy::jpeg_width(256, 70)
        };
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(256, 70)),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::NewestDetailed { older, newest }),
            payload_budget: None,
            changed: None,
        };

        let reference = plan_for_stack(&frames, &policy).unwrap();
        let preferred = measure_plan(&reference, &frames).unwrap();
        let (_, floor) = minimum_payload(&reference, &frames).unwrap();
        let newest_id = frames.last().unwrap().frame_id.get();

        let mut spared = 0;
        let mut touched = 0;

        // Ten budgets spanning from "everything as asked for" down to the floor.
        for step in 0..=10usize {
            let budget = preferred - ((preferred - floor) * step) / 10;
            let plan = plan_for_stack(&frames, &policy).unwrap();

            let Ok((fitted, report)) = fit_plan(plan, &frames, &PayloadBudget::new(budget)) else {
                continue;
            };

            assert!(report.within_budget(), "budget {budget} was exceeded");

            if newest_was_degraded(&report, newest_id) {
                touched += 1;
                assert!(
                    older_views_exhausted(&fitted, &frames),
                    "the newest was degraded at budget {budget} while an older view still had \
                     room to be reduced: {:?}",
                    report.adjustments
                );
            } else {
                spared += 1;
            }
        }

        // And the protection has to be observable, or the sweep proves nothing: there
        // must be budgets that the older frames absorb entirely, and tighter ones they
        // cannot.
        assert!(
            spared > 0,
            "the oldest frames had levers available; some budget should have spared the newest"
        );
        assert!(
            touched > 0,
            "some budget should have forced the newest to pay, or the sweep is too generous"
        );
    }

    #[test]
    fn when_the_newest_must_be_degraded_every_older_option_is_exhausted_first() {
        // The other half of requirement 21: if the newest is touched at all, it is
        // only after the older views have nothing left to give.
        let frames = stack(4, 256, 192);
        let older = ImagePolicy {
            floors: Some(ImageFloors {
                min_width: Some(96),
                min_quality: Some(50),
            }),
            ..ImagePolicy::jpeg_width(256, 70)
        };
        let newest = ImagePolicy {
            floors: Some(ImageFloors {
                min_width: Some(192),
                min_quality: Some(60),
            }),
            ..ImagePolicy::jpeg_width(256, 70)
        };
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(256, 70)),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::NewestDetailed { older, newest }),
            payload_budget: None,
            changed: None,
        };
        let reference = plan_for_stack(&frames, &policy).unwrap();
        let (_, floor) = minimum_payload(&reference, &frames).unwrap();

        let newest_id = frames.last().unwrap().frame_id.get();
        // A budget just above the absolute floor, so the newest has to give.
        let plan = plan_for_stack(&frames, &policy).unwrap();
        let (fitted, report) = fitted(plan, &frames, floor + 1_000);

        let newest_touched = report.adjustments.iter().any(|a| match a {
            PayloadAdjustment::Quality { frame_id, .. }
            | PayloadAdjustment::Resolution { frame_id, .. }
            | PayloadAdjustment::Omitted { frame_id, .. } => frame_id.get() == newest_id,
        });

        if newest_touched {
            // Every adjustment that names the newest must come after every adjustment
            // that does not, which is what "exhausted first" means read directly off the
            // log the fitter produced.
            let names_newest = |adjustment: &PayloadAdjustment| match adjustment {
                PayloadAdjustment::Quality { frame_id, .. }
                | PayloadAdjustment::Resolution { frame_id, .. }
                | PayloadAdjustment::Omitted { frame_id, .. } => frame_id.get() == newest_id,
            };
            let first_newest = report.adjustments.iter().position(names_newest).unwrap();
            let last_older = report.adjustments.iter().rposition(|a| !names_newest(a));

            assert!(
                last_older.is_none_or(|older| older < first_newest),
                "the newest was degraded interleaved with the older views rather than after \
                 them: {:?}",
                report.adjustments
            );
            assert!(
                older_views_exhausted(&fitted, &frames),
                "the newest was degraded while an older view still had room to be reduced: {:?}",
                report.adjustments
            );
        }
        assert!(report.within_budget());
    }

    #[test]
    fn the_reported_floor_is_never_above_the_unfitted_payload() {
        // A floor is only worth reporting if a budget at it is actually achievable, and the cheapest
        // plan is not always the most degraded one. JPEG does not shrink monotonically with width on
        // this content: a resized image can carry a larger header and a worse block structure than the
        // original, so walking every rung to exhaustion can land *above* where it started. A floor of
        // 56268 bytes for a payload that costs 51256 unfitted was reported by a live run of the metrics
        // suite, and would have told a caller that a budget above its own request was impossible.
        //
        // The property is checked across a spread of shapes and qualities, because which plan comes out
        // cheapest is a property of the encoder's behaviour on the content rather than of the ladder.
        for (width, height, quality) in [
            (640, 480, 85u8),
            (320, 240, 85),
            (200, 150, 60),
            (400, 300, 45),
            (128, 96, 90),
        ] {
            let frames = stack(3, width, height);
            let policy = ObservationPolicy {
                overview: Some(ImagePolicy::jpeg_width(width, quality)),
                regions: Vec::new(),
                temporal: Some(TemporalFramePolicy::NewestDetailed {
                    older: ImagePolicy::jpeg_width(width, quality),
                    newest: ImagePolicy::jpeg_width(width, quality),
                }),
                payload_budget: None,
                changed: None,
            };
            let plan = plan_for_stack(&frames, &policy).unwrap();

            let preferred = measure_plan(&plan, &frames).unwrap();
            let (_, floor) = minimum_payload(&plan, &frames).unwrap();

            assert!(
                floor <= preferred,
                "the reported floor ({floor}) exceeds the unfitted payload ({preferred}) for \
                 {width}x{height} at quality {quality}; a caller would be told that a budget above \
                 its own request was impossible"
            );

            // And a budget *at* the floor must genuinely work. This is the claim the message makes,
            // so it is the claim worth checking rather than merely the inequality above.
            let (fitted, report) = fit_plan(plan, &frames, &PayloadBudget::new(floor))
                .unwrap_or_else(|error| {
                    panic!(
                        "a budget at the reported floor of {floor} must be reachable for \
                         {width}x{height} at quality {quality}: {}",
                        error.message()
                    )
                });
            assert!(report.within_budget());
            assert_eq!(
                fitted.views.len(),
                frames.len(),
                "every frame must survive fitting at the floor"
            );
        }
    }

    #[test]
    fn a_failure_reports_the_floor_a_caller_can_actually_use() {
        // The message quotes a number, and the only thing that makes quoting it honest is that a
        // budget of exactly that number succeeds. A message quoting a floor that is itself
        // unreachable would send the caller round the same loop again.
        let frames = stack(3, 320, 240);
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(320, 85)),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::NewestDetailed {
                older: ImagePolicy::jpeg_width(160, 45),
                newest: ImagePolicy::jpeg_width(320, 85),
            }),
            payload_budget: None,
            changed: None,
        };
        let plan = plan_for_stack(&frames, &policy).unwrap();
        let (_, floor) = minimum_payload(&plan, &frames).unwrap();

        // One byte below the floor fails, and says what the floor is.
        let error = fit_plan(plan.clone(), &frames, &PayloadBudget::new(floor - 1)).unwrap_err();
        assert_eq!(error.code(), "payload_budget_exceeded");
        let message = error.message().to_string();
        assert!(
            message.contains(&floor.to_string()),
            "the refusal should quote the real floor ({floor}): {message}"
        );

        // And one byte above it succeeds, which is what makes the quoted number usable.
        let (_, report) =
            fit_plan(plan, &frames, &PayloadBudget::new(floor + 1)).unwrap_or_else(|error| {
                panic!("a budget just above the floor failed: {}", error.message())
            });
        assert!(report.within_budget());
    }

    #[test]
    fn omitting_an_older_image_keeps_its_identity_in_the_plan() {
        let frames = stack(3, 160, 120);
        let policy = three_frame_policy(80);
        let reference = plan_for_stack(&frames, &policy).unwrap();
        let preferred = measure_plan(&reference, &frames).unwrap();
        let (_, floor) = minimum_payload(&reference, &frames).unwrap();

        let plan = plan_for_stack(&frames, &policy).unwrap();
        let (fitted, _) = fitted(plan, &frames, between(floor, preferred));

        // Every frame is still represented, even when its image was dropped.
        assert_eq!(fitted.views_for(0).count(), 1);
        assert_eq!(fitted.views_for(1).count(), 1);
        assert_eq!(fitted.views_for(2).count(), 1);
    }

    #[test]
    fn every_requested_frame_survives_fitting_even_at_the_floor() {
        // A regression test for a defect found in a live run rather than in a unit
        // test: a caller asking for a three-frame stack, with a tight budget, received
        // one frame. The frame was marked optional because it was not the newest, so
        // the fitter dropped it — and the response *looked* complete, because the frames
        // that remained were whole. Asking for three frames and silently getting one is
        // the worst shape of failure: correct-looking and undetectable by the caller.
        //
        // The rule now is that the overview of every requested frame is required. This
        // test holds the line by driving the fitter all the way down to the floor, where
        // every lever has been spent and dropping images is the only saving left, and
        // checking that no frame lost its overview.
        let frames = stack(5, 240, 180);
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(240, 70)),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::NewestDetailed {
                older: ImagePolicy::jpeg_width(240, 70),
                newest: ImagePolicy::jpeg_width(240, 70),
            }),
            payload_budget: None,
            changed: None,
        };

        let reference = plan_for_stack(&frames, &policy).unwrap();
        let preferred = measure_plan(&reference, &frames).unwrap();
        let (_, floor) = minimum_payload(&reference, &frames).unwrap();

        // Every budget from the preferred payload down to the floor, including the
        // floor itself, which is by construction reachable.
        for step in 0..=8usize {
            let budget = preferred - ((preferred - floor) * step) / 8;
            // One byte below the floor is added at the end to confirm the failure path
            // still reports rather than quietly returning a truncated stack.
            for budget in [
                budget,
                if step == 8 {
                    floor.saturating_sub(1)
                } else {
                    budget
                },
            ] {
                let plan = plan_for_stack(&frames, &policy).unwrap();
                let fitted = fit_plan(plan, &frames, &PayloadBudget::new(budget));

                if let Ok((fitted, _)) = fitted {
                    for (index, frame) in frames.iter().enumerate() {
                        assert!(
                            fitted
                                .views_for(index)
                                .any(|view| matches!(view.kind, ViewKind::Overview)),
                            "frame {} ({:?}) lost its overview at budget {budget}; a caller \
                             asking for {} frames must not silently receive fewer",
                            index,
                            frame.frame_id,
                            frames.len()
                        );
                    }
                } else {
                    // An impossible budget must say so. Returning a short stack instead
                    // is exactly the silent-truncation failure this test exists for.
                    let code = fitted.unwrap_err().code();
                    assert_eq!(code, "payload_budget_exceeded");
                }
            }
        }
    }

    #[test]
    fn a_png_view_is_fitted_by_resolution_and_omission_only() {
        // Requirement 25: there is no lossy PNG semantics to invent. Whatever the
        // fitter does, it may not touch a quality field on a PNG view.
        //
        // The assertion is structural rather than numeric because resizing a
        // pathologically periodic test pattern can *increase* PNG size: box filtering
        // it produces values the filters handle worse. A test that demanded the
        // payload shrink would be asserting something about this pattern rather than
        // about the fitter.
        let frames = stack(1, 400, 300);
        let policy = ObservationPolicy::phase5_default(ImagePolicy::png_width(400));
        let plan = plan_for_frame(&frames[0], &policy).unwrap();
        let preferred = measure_plan(&plan, &frames).unwrap();

        let plan = plan_for_frame(&frames[0], &policy).unwrap();
        let (fitted, report) = try_fit(plan, &frames, (preferred * 3) / 4);

        for adjustment in &report.adjustments {
            assert!(
                !matches!(adjustment, PayloadAdjustment::Quality { .. }),
                "a PNG view must not be quality-adjusted: {adjustment:?}"
            );
        }
        if let Some(fitted) = fitted {
            assert_eq!(
                fitted.views[0].image.format,
                crate::encode::ImageFormat::Png
            );
            assert!(report.actual_base64_bytes <= (preferred * 3) / 4);
        }
    }

    #[test]
    fn a_metadata_only_view_contributes_nothing_to_the_payload() {
        let frames = stack(2, 128, 96);
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(128, 75)),
            regions: Vec::new(),
            temporal: Some(TemporalFramePolicy::NewestOnly {
                image: ImagePolicy::jpeg_width(128, 75),
            }),
            payload_budget: None,
            changed: None,
        };
        let plan = plan_for_stack(&frames, &policy).unwrap();
        let measured = measure_plan(&plan, &frames).unwrap();

        // Only the newest frame contributes, so the total equals a single-frame
        // presentation of the newest frame alone.
        let newest_only = plan_for_frame(
            &frames[1],
            &ObservationPolicy::phase5_default(ImagePolicy::jpeg_width(128, 75)),
        )
        .unwrap();
        let newest_measured = measure_plan(&newest_only, &[frames[1].clone()]).unwrap();
        assert_eq!(measured, newest_measured);
    }

    #[test]
    fn the_measurement_cache_returns_the_same_answer_as_a_fresh_measurement() {
        // The cache exists for speed, so it must be invisible in its effect.
        let frames = stack(3, 160, 120);
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(160, 80)),
            regions: vec![RegionPolicy::required(
                "hud",
                Rect::new(0, 0, 80, 60).unwrap(),
                ImagePolicy::jpeg_width(80, 70),
            )],
            temporal: Some(TemporalFramePolicy::NewestDetailed {
                older: ImagePolicy::jpeg_width(80, 50),
                newest: ImagePolicy::jpeg_width(160, 80),
            }),
            payload_budget: None,
            changed: None,
        };
        let plan = plan_for_stack(&frames, &policy).unwrap();

        let mut cache = MeasureCache::new();
        let first = cache.measure(&plan, &frames).unwrap();
        let second = cache.measure(&plan, &frames).unwrap();
        let third = MeasureCache::new().measure(&plan, &frames).unwrap();

        assert_eq!(
            first, second,
            "a cache hit must agree with a fresh computation"
        );
        assert_eq!(first, third);
    }

    #[test]
    fn fitting_terminates_for_every_budget_from_impossible_to_generous() {
        // A termination test rather than a correctness one: the ladder must not spin.
        let frames = stack(5, 256, 192);
        let policy = three_frame_policy(85);
        let reference = plan_for_stack(&frames, &policy).unwrap();
        let full = measure_plan(&reference, &frames).unwrap();

        for budget in [1usize, 100, full / 10, full / 2, full, full * 2] {
            let plan = plan_for_stack(&frames, &policy).unwrap();
            let started = Instant::now();
            let _ = fit_plan(plan, &frames, &PayloadBudget::new(budget));
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "fitting must terminate promptly for budget {budget}"
            );
        }
    }

    #[test]
    fn a_whole_frame_view_is_ordered_after_a_crop_of_equal_priority() {
        // The tie-break is total and visible, which is what makes the ordering
        // reproducible rather than merely usually the same.
        let frames = stack(1, 128, 96);
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(128, 70)),
            regions: vec![RegionPolicy {
                priority: crate::presentation::view::VIEW_PRIORITY_NEWEST,
                ..RegionPolicy::required(
                    "same-priority",
                    Rect::new(0, 0, 64, 64).unwrap(),
                    ImagePolicy::jpeg_width(64, 70),
                )
            }],
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        let mut plan = plan_for_frame(&frames[0], &policy).unwrap();
        let ordered: Vec<String> = order_for_fitting(&mut plan)
            .iter()
            .map(|v| v.kind.name().to_string())
            .collect();
        assert_eq!(
            ordered,
            vec!["overview".to_string(), "same-priority".to_string()],
            "the overview sorts before a region of equal priority"
        );
    }

    #[test]
    fn a_scoped_region_keeps_its_source_rectangle_through_fitting() {
        let frames = stack(2, 200, 160);
        let policy = ObservationPolicy {
            overview: Some(ImagePolicy::jpeg_width(200, 70)),
            regions: vec![RegionPolicy::required(
                "hud",
                Rect::new(20, 40, 100, 40).unwrap(),
                ImagePolicy::jpeg_width(100, 70),
            )],
            temporal: None,
            payload_budget: None,
            changed: None,
        };
        let reference = plan_for_stack(&frames, &policy).unwrap();
        let preferred = measure_plan(&reference, &frames).unwrap();
        let (_, floor) = minimum_payload(&reference, &frames).unwrap();

        let plan = plan_for_stack(&frames, &policy).unwrap();
        let (fitted, _) = fitted(plan, &frames, between(floor, preferred));

        let hud = fitted
            .views
            .iter()
            .find(|v| v.kind.name() == "hud")
            .expect("the region survives");
        assert_eq!(
            hud.source,
            ViewSource::Crop {
                rect: Rect::new(20, 40, 100, 40).unwrap()
            },
            "fitting changes how a view is rendered, never where it came from"
        );
    }

    #[test]
    fn a_whole_stack_that_fits_is_identical_to_the_unfitted_plan() {
        // A budget that is met must leave a byte-identical plan, which is the
        // strongest statement of "no opportunistic adaptation".
        let frames = stack(3, 160, 120);
        let policy = three_frame_policy(75);
        let plan = plan_for_stack(&frames, &policy).unwrap();
        let preferred = measure_plan(&plan, &frames).unwrap();

        // A budget of exactly the measured payload: the boundary case where "fits"
        // means "fits exactly".
        let (fitted, report) = fitted(plan.clone(), &frames, preferred);
        assert_eq!(report.fit, PayloadFitState::Exact);
        assert_eq!(fitted.views.len(), plan.views.len());
        for (a, b) in fitted.views.iter().zip(plan.views.iter()) {
            assert_eq!(a.image, b.image, "no view may be changed when it fits");
        }
    }

    #[test]
    fn the_minimum_payload_helper_reports_a_plan_that_actually_achieves_it() {
        let frames = stack(3, 192, 144);
        let policy = three_frame_policy(85);
        let plan = plan_for_stack(&frames, &policy).unwrap();

        let (floor_plan, floor) = minimum_payload(&plan, &frames).unwrap();
        assert_eq!(
            measure_plan(&floor_plan, &frames).unwrap(),
            floor,
            "the reported floor must be what the floor plan really costs"
        );
        assert!(floor < measure_plan(&plan, &frames).unwrap());
    }
}
