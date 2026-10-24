//! The one rule that decides whether a context measure schedules compaction
//! (spec 01 §10.3). The turn fences and a host's idle fence read the same
//! verdict; which rung then acts is the compaction ladder's business.

use plakous_model::{ContextHeadroom, ContextStatusKind};

/// Input headroom kept ahead of tool observations. A measure at or under it
/// schedules compaction even below the occupancy trigger.
pub(crate) const OBSERVATION_HEADROOM_RESERVE_TOKENS: u64 = 4_096;

/// How the measured request relates to its admitted envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeasuredFit {
    /// The request fits the admitted envelope.
    Fits,
    /// The request would overflow, with or without an eligible cut.
    Overflows,
    /// No usable measurement exists.
    Unmeasured,
}

impl MeasuredFit {
    /// The fit a status classification reports.
    #[must_use]
    pub(crate) const fn of(kind: &ContextStatusKind) -> Self {
        match kind {
            ContextStatusKind::Fits { .. } => Self::Fits,
            ContextStatusKind::WouldOverflow(_) | ContextStatusKind::ProtectedOverflow(_) => {
                Self::Overflows
            }
            _ => Self::Unmeasured,
        }
    }
}

/// The inputs of one scheduling decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionMeasure {
    /// The request's fit.
    pub fit: MeasuredFit,
    /// Occupancy the trigger may act on: measured from a provider-count
    /// anchor, so `None` before a completed round anchors the measure. An
    /// unanchored estimate never schedules a paid fold by itself; the one
    /// exception is a seeded genesis's first fold (§10.3).
    pub occupancy: Option<u64>,
    /// The occupancy compaction trigger, when the envelope has one.
    pub trigger: Option<u64>,
    /// Remaining admitted input tokens, when a credible measure bounds them.
    pub input_headroom: Option<u64>,
}

/// Whether a measure schedules compaction, and on which grounds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompactionVerdict {
    overflow: bool,
    occupancy: bool,
    observation_headroom: bool,
}

impl CompactionVerdict {
    /// Decides one measure.
    #[must_use]
    pub fn of(measure: CompactionMeasure) -> Self {
        let fits = measure.fit == MeasuredFit::Fits;
        Self {
            overflow: measure.fit == MeasuredFit::Overflows,
            occupancy: fits
                && measure
                    .occupancy
                    .zip(measure.trigger)
                    .is_some_and(|(occupied, trigger)| occupied >= trigger),
            observation_headroom: fits
                && measure
                    .input_headroom
                    .is_some_and(|headroom| headroom <= OBSERVATION_HEADROOM_RESERVE_TOKENS),
        }
    }

    /// Whether any ground schedules compaction.
    #[must_use]
    pub const fn is_due(self) -> bool {
        self.overflow || self.occupancy || self.observation_headroom
    }

    /// The request would overflow.
    #[must_use]
    pub const fn overflow(self) -> bool {
        self.overflow
    }

    /// Anchored occupancy reached the trigger.
    #[must_use]
    pub const fn occupancy(self) -> bool {
        self.occupancy
    }

    /// The admitted input reached the observation reserve.
    #[must_use]
    pub const fn observation_headroom(self) -> bool {
        self.observation_headroom
    }
}

/// Remaining admitted input tokens of a fit headroom; an unclassified
/// headroom counts as none left.
#[must_use]
pub(crate) fn input_headroom_tokens(headroom: ContextHeadroom) -> u64 {
    match headroom {
        ContextHeadroom::ProviderReservedInput { input }
        | ContextHeadroom::Shared(input)
        | ContextHeadroom::Separate { input, .. } => input.value(),
        ContextHeadroom::SharedWithOutputCap { shared, .. } => shared.value(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn measure(
        fit: MeasuredFit,
        occupancy: Option<u64>,
        trigger: Option<u64>,
        input_headroom: Option<u64>,
    ) -> CompactionMeasure {
        CompactionMeasure {
            fit,
            occupancy,
            trigger,
            input_headroom,
        }
    }

    /// Every scheduling case: (measure, overflow, occupancy, headroom).
    #[test]
    fn the_verdict_table() {
        use MeasuredFit::{Fits, Overflows, Unmeasured};
        let reserve = OBSERVATION_HEADROOM_RESERVE_TOKENS;
        let cases = [
            // Fits below every trigger.
            (
                measure(Fits, Some(10), Some(100), Some(reserve + 1)),
                false,
                false,
                false,
            ),
            // Anchored occupancy at and over the trigger.
            (
                measure(Fits, Some(100), Some(100), None),
                false,
                true,
                false,
            ),
            (
                measure(Fits, Some(101), Some(100), None),
                false,
                true,
                false,
            ),
            // An unanchored measure never reaches the occupancy trigger.
            (measure(Fits, None, Some(100), None), false, false, false),
            // No trigger, no occupancy ground.
            (measure(Fits, Some(1_000), None, None), false, false, false),
            // The observation reserve, at and just over it.
            (measure(Fits, None, None, Some(reserve)), false, false, true),
            (measure(Fits, None, None, Some(0)), false, false, true),
            (
                measure(Fits, None, None, Some(reserve + 1)),
                false,
                false,
                false,
            ),
            // Both fit grounds at once.
            (
                measure(Fits, Some(100), Some(100), Some(0)),
                false,
                true,
                true,
            ),
            // Overflow schedules without an anchor; fit grounds do not apply.
            (measure(Overflows, None, None, None), true, false, false),
            (
                measure(Overflows, Some(100), Some(100), Some(0)),
                true,
                false,
                false,
            ),
            // Nothing measured, nothing scheduled.
            (
                measure(Unmeasured, Some(100), Some(100), Some(0)),
                false,
                false,
                false,
            ),
        ];
        for (case, overflow, occupancy, headroom) in cases {
            let verdict = CompactionVerdict::of(case);
            assert_eq!(verdict.overflow(), overflow, "{case:?}");
            assert_eq!(verdict.occupancy(), occupancy, "{case:?}");
            assert_eq!(verdict.observation_headroom(), headroom, "{case:?}");
            assert_eq!(
                verdict.is_due(),
                overflow || occupancy || headroom,
                "{case:?}"
            );
        }
    }
}
