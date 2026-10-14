//! Anchor-stability metric for rendered transcript frames.
//!
//! The spec gates smoothness, not only speed: "scroll anchoring must not drift
//! while blocks above the viewport change size". This module turns a sequence
//! of rendered frames into four counted defects, so a test can assert a budget
//! instead of eyeballing a recording.
//!
//! It is pure and terminal-free. A frame is whatever the caller can produce —
//! the plain rows of a viewport, or a `ratatui` buffer via
//! [`rows_from_buffer`] — and every judgement is made from row hashes, so the
//! metric never depends on styling, colors, or trailing pad.
//!
//! The four defects, in the operator's vocabulary:
//!
//! - **Reposition** — content the operator was reading moved vertically while
//!   the viewport was pinned. This is anchor drift, the defect §11 names.
//! - **Big pop** — a reposition large enough to lose the reading position
//!   outright (default: a third of the viewport).
//! - **Blink** — a row vanished for exactly one frame and came back. A cache
//!   that rebuilds through an empty intermediate produces these.
//! - **Mass reflow** — most of the previous frame's rows are simply gone,
//!   which is what a transcript-wide re-layout looks like from the outside.
//!
//! Tail-following frames legitimately move content upward, so the caller
//! states the intent of each frame with [`ViewportMotion`]; only
//! [`ViewportMotion::Pinned`] frames can score a reposition.

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};

/// Default share of carried-over rows that must be lost before a transition
/// counts as a mass reflow.
pub const DEFAULT_MASS_REFLOW_RATIO: f64 = 0.5;

/// Default anchor shift, in rows, at or above which a reposition is also a
/// big pop when no viewport height is known.
pub const DEFAULT_BIG_POP_ROWS: usize = 8;

/// What the caller intended the viewport to do between two frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewportMotion {
    /// The operator scrolled up and is holding a position. Content must not
    /// move under them: any anchor shift is a reposition.
    Pinned,
    /// The viewport follows the tail, so appended content legitimately pushes
    /// older rows upward. Anchor shifts are expected and not scored.
    FollowTail,
    /// The operator asked for the move (a scroll, a jump, a resize). The frame
    /// is still observed for blinks and mass reflow, but not for drift.
    Deliberate,
}

/// Counted defects over an observed frame sequence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StabilityReport {
    /// Frames handed to [`AnchorStability::observe`].
    pub frames: usize,
    /// Frame transitions actually scored (one fewer than `frames`).
    pub transitions: usize,
    /// Transitions scored under [`ViewportMotion::Pinned`].
    pub pinned_transitions: usize,
    /// Pinned transitions whose anchor row moved or was lost.
    pub repositions: usize,
    /// Repositions large enough to lose the reading position.
    pub big_pops: usize,
    /// Rows that disappeared for exactly one frame and returned.
    pub blinks: usize,
    /// Transitions that dropped more than the mass-reflow share of rows.
    pub mass_reflows: usize,
    /// Largest absolute anchor shift observed on a pinned transition.
    pub max_anchor_shift: usize,
}

impl StabilityReport {
    /// Repositions per 100 pinned transitions; zero when nothing was pinned.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "frame counts in a gate run are far below f64's exact integer range"
    )]
    pub fn repositions_per_100_frames(&self) -> f64 {
        if self.pinned_transitions == 0 {
            return 0.0;
        }
        (self.repositions as f64) * 100.0 / (self.pinned_transitions as f64)
    }

    /// Every budget line this report violates, in a stable order.
    ///
    /// An empty vector means the report is within budget.
    #[must_use]
    pub fn violations(&self, budget: &StabilityBudget) -> Vec<String> {
        let mut out = Vec::new();
        if self.mass_reflows > budget.max_mass_reflows {
            out.push(format!(
                "mass reflows {} > budget {}",
                self.mass_reflows, budget.max_mass_reflows
            ));
        }
        if self.blinks > budget.max_blinks {
            out.push(format!(
                "blinks {} > budget {}",
                self.blinks, budget.max_blinks
            ));
        }
        if self.big_pops > budget.max_big_pops {
            out.push(format!(
                "big pops {} > budget {}",
                self.big_pops, budget.max_big_pops
            ));
        }
        let rate = self.repositions_per_100_frames();
        if rate > budget.max_repositions_per_100_frames {
            out.push(format!(
                "repositions {rate:.2}/100 frames (raw {}) > budget {:.2}",
                self.repositions, budget.max_repositions_per_100_frames
            ));
        }
        out
    }
}

/// Budget a [`StabilityReport`] is asserted against.
#[derive(Clone, Copy, Debug, PartialEq)]
#[expect(
    clippy::struct_field_names,
    reason = "each field is an upper bound on a measured defect"
)]
pub struct StabilityBudget {
    /// Transitions that may lose most of the previous frame's rows.
    pub max_mass_reflows: usize,
    /// Rows that may vanish for one frame and return.
    pub max_blinks: usize,
    /// Repositions that may be large enough to lose the reading position.
    pub max_big_pops: usize,
    /// Repositions allowed per 100 pinned transitions.
    pub max_repositions_per_100_frames: f64,
}

impl StabilityBudget {
    /// The stability budget: no mass reflow, no blink, no big pop, and at most
    /// one reposition per 100 pinned frames.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            max_mass_reflows: 0,
            max_blinks: 0,
            max_big_pops: 0,
            max_repositions_per_100_frames: 1.0,
        }
    }

    /// The strict budget with repositions forbidden outright.
    #[must_use]
    pub const fn perfect() -> Self {
        Self {
            max_repositions_per_100_frames: 0.0,
            ..Self::strict()
        }
    }
}

impl Default for StabilityBudget {
    fn default() -> Self {
        Self::strict()
    }
}

/// Rolling anchor-stability observer over consecutive rendered frames.
///
/// Hold one per gate run, call [`Self::observe`] once per painted frame, then
/// assert [`Self::report`] against a [`StabilityBudget`].
#[derive(Clone, Debug)]
pub struct AnchorStability {
    previous: Option<Vec<Option<u64>>>,
    before_previous: Option<Vec<Option<u64>>>,
    big_pop_rows: usize,
    mass_reflow_ratio: f64,
    report: StabilityReport,
}

impl Default for AnchorStability {
    fn default() -> Self {
        Self::new()
    }
}

impl AnchorStability {
    /// Observer with the default thresholds.
    #[must_use]
    pub fn new() -> Self {
        Self {
            previous: None,
            before_previous: None,
            big_pop_rows: DEFAULT_BIG_POP_ROWS,
            mass_reflow_ratio: DEFAULT_MASS_REFLOW_RATIO,
            report: StabilityReport::default(),
        }
    }

    /// Observer whose big-pop threshold is a third of `viewport_rows`.
    ///
    /// A shift smaller than that still counts as a reposition; the operator
    /// only loses their place when a full band of the viewport moves.
    #[must_use]
    pub fn for_viewport(viewport_rows: usize) -> Self {
        Self {
            big_pop_rows: (viewport_rows / 3).max(2),
            ..Self::new()
        }
    }

    /// Counted defects so far.
    #[must_use]
    pub const fn report(&self) -> StabilityReport {
        self.report
    }

    /// Score one newly rendered frame against the frames before it.
    ///
    /// `rows` are the viewport's rows, top to bottom. Trailing whitespace and
    /// styling are ignored; blank rows anchor nothing.
    pub fn observe<S: AsRef<str>>(&mut self, rows: &[S], motion: ViewportMotion) {
        let current: Vec<Option<u64>> = rows.iter().map(|row| row_hash(row.as_ref())).collect();
        self.report.frames += 1;
        if let Some(previous) = self.previous.as_ref() {
            self.report.transitions += 1;
            let previous = previous.clone();
            self.score_anchor(&previous, &current, motion);
            self.score_mass_reflow(&previous, &current);
            if let Some(before) = self.before_previous.clone() {
                self.score_blink(&before, &previous, &current);
            }
        }
        self.before_previous = self.previous.take();
        self.previous = Some(current);
    }

    fn score_anchor(
        &mut self,
        previous: &[Option<u64>],
        current: &[Option<u64>],
        motion: ViewportMotion,
    ) {
        if motion != ViewportMotion::Pinned {
            return;
        }
        self.report.pinned_transitions += 1;
        // The anchor is the topmost row the operator can actually see content
        // on. If that row's text repeats elsewhere the first match is still
        // the nearest honest reading of where it went.
        let Some((anchor_index, anchor_hash)) = previous
            .iter()
            .enumerate()
            .find_map(|(index, hash)| hash.map(|hash| (index, hash)))
        else {
            // Nothing was on screen; there is no position to hold.
            return;
        };
        let Some(found) = current.iter().position(|hash| *hash == Some(anchor_hash)) else {
            // The anchor left the viewport entirely: the worst reposition
            // there is, scored at the full viewport height.
            self.report.repositions += 1;
            self.report.big_pops += 1;
            let lost = current.len().max(previous.len());
            self.report.max_anchor_shift = self.report.max_anchor_shift.max(lost);
            return;
        };
        let shift = found.abs_diff(anchor_index);
        if shift == 0 {
            return;
        }
        self.report.repositions += 1;
        self.report.max_anchor_shift = self.report.max_anchor_shift.max(shift);
        if shift >= self.big_pop_rows {
            self.report.big_pops += 1;
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport row counts are small integers"
    )]
    fn score_mass_reflow(&mut self, previous: &[Option<u64>], current: &[Option<u64>]) {
        let live: HashSet<u64> = current.iter().filter_map(|hash| *hash).collect();
        let mut carried = 0_usize;
        let mut total = 0_usize;
        for hash in previous.iter().filter_map(|hash| *hash) {
            total += 1;
            if live.contains(&hash) {
                carried += 1;
            }
        }
        if total == 0 {
            return;
        }
        let lost_ratio = 1.0 - (carried as f64) / (total as f64);
        if lost_ratio > self.mass_reflow_ratio {
            self.report.mass_reflows += 1;
        }
    }

    fn score_blink(
        &mut self,
        before: &[Option<u64>],
        previous: &[Option<u64>],
        current: &[Option<u64>],
    ) {
        let middle: HashSet<u64> = previous.iter().filter_map(|hash| *hash).collect();
        let live: HashSet<u64> = current.iter().filter_map(|hash| *hash).collect();
        let blinked = before
            .iter()
            .filter_map(|hash| *hash)
            .filter(|hash| live.contains(hash) && !middle.contains(hash))
            .count();
        self.report.blinks += blinked;
    }
}

/// Hash one rendered row, or `None` when the row carries no content.
fn row_hash(row: &str) -> Option<u64> {
    let trimmed = row.trim_end();
    if trimmed.trim().is_empty() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    trimmed.hash(&mut hasher);
    Some(hasher.finish())
}

/// Plain rows of a rendered `ratatui` buffer, top to bottom.
///
/// Only the cells inside `area` are read, so a caller can score the transcript
/// body without the chrome around it.
#[must_use]
pub fn rows_from_buffer(
    buffer: &ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
) -> Vec<String> {
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "these rates are exact small-integer ratios"
)]
mod tests {
    use super::*;

    fn frame(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| (*row).to_owned()).collect()
    }

    #[test]
    fn a_still_frame_scores_nothing() {
        let mut metric = AnchorStability::new();
        let rows = frame(&["alpha", "beta", "gamma"]);
        metric.observe(&rows, ViewportMotion::Pinned);
        metric.observe(&rows, ViewportMotion::Pinned);
        let report = metric.report();
        assert_eq!(report.repositions, 0);
        assert_eq!(report.mass_reflows, 0);
        assert_eq!(report.transitions, 1);
        assert_eq!(
            report.violations(&StabilityBudget::perfect()),
            [] as [String; 0]
        );
    }

    #[test]
    fn styling_and_trailing_pad_are_not_content() {
        let mut metric = AnchorStability::new();
        metric.observe(&frame(&["alpha", "beta"]), ViewportMotion::Pinned);
        metric.observe(&frame(&["alpha    ", "beta  "]), ViewportMotion::Pinned);
        assert_eq!(metric.report().repositions, 0);
    }

    #[test]
    fn a_pinned_frame_that_moves_content_is_a_reposition() {
        let mut metric = AnchorStability::new();
        metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
        metric.observe(
            &frame(&["inserted", "alpha", "beta", "gamma"]),
            ViewportMotion::Pinned,
        );
        let report = metric.report();
        assert_eq!(report.repositions, 1);
        assert_eq!(report.max_anchor_shift, 1);
        assert_eq!(report.big_pops, 0);
        assert_eq!(report.repositions_per_100_frames(), 100.0);
    }

    #[test]
    fn the_same_move_while_following_the_tail_is_not_a_defect() {
        let mut metric = AnchorStability::new();
        metric.observe(
            &frame(&["alpha", "beta", "gamma"]),
            ViewportMotion::FollowTail,
        );
        metric.observe(
            &frame(&["beta", "gamma", "delta"]),
            ViewportMotion::FollowTail,
        );
        let report = metric.report();
        assert_eq!(report.repositions, 0);
        assert_eq!(report.pinned_transitions, 0);
        assert_eq!(report.repositions_per_100_frames(), 0.0);
    }

    #[test]
    fn losing_the_anchor_is_a_big_pop() {
        let mut metric = AnchorStability::for_viewport(9);
        metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
        metric.observe(&frame(&["one", "two", "three"]), ViewportMotion::Pinned);
        let report = metric.report();
        assert_eq!(report.repositions, 1);
        assert_eq!(report.big_pops, 1);
        assert_eq!(report.mass_reflows, 1);
    }

    #[test]
    fn a_row_that_vanishes_for_one_frame_is_a_blink() {
        let mut metric = AnchorStability::new();
        metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
        metric.observe(&frame(&["alpha", "", "gamma"]), ViewportMotion::Pinned);
        metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
        assert_eq!(metric.report().blinks, 1);
    }

    #[test]
    fn a_held_viewport_over_three_frames_stays_within_the_perfect_budget() {
        let mut metric = AnchorStability::new();
        for _ in 0..3 {
            metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
        }
        let report = metric.report();
        assert_eq!(
            report.violations(&StabilityBudget::perfect()),
            [] as [String; 0]
        );
        assert_eq!(report.frames, 3);
        assert_eq!(report.pinned_transitions, 2);
    }

    #[test]
    fn a_violating_report_names_every_broken_line() {
        let mut metric = AnchorStability::for_viewport(9);
        metric.observe(&frame(&["alpha", "beta", "gamma"]), ViewportMotion::Pinned);
        metric.observe(&frame(&["one", "two", "three"]), ViewportMotion::Pinned);
        let violations = metric.report().violations(&StabilityBudget::strict());
        assert_eq!(violations.len(), 3, "{violations:?}");
    }
}
