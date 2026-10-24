//! Truthful product projection of Plakous context-admission status.
//!
//! This module is presentation-only. It does not count tokens, choose capacity,
//! or authorize admission or compaction; those remain Plakous authorities.

use plakous::model::{
    CapacityRule, ContextEvidenceClass, ContextNonGuaranteeDisposition, ContextStatus,
    ContextStatusKind, CountBound, SafeLimit,
};

/// Complete prepared model-input count used by context admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedInputCount {
    /// The complete prepared input contains exactly this many tokens.
    Exact(u64),
    /// The complete prepared input contains at most this many tokens.
    UpperBound(u64),
}

impl PreparedInputCount {
    /// Returns the number admission compared with the safe boundary.
    #[must_use]
    pub const fn tokens(self) -> u64 {
        match self {
            Self::Exact(tokens) | Self::UpperBound(tokens) => tokens,
        }
    }
}

impl From<CountBound> for PreparedInputCount {
    fn from(bound: CountBound) -> Self {
        if bound.is_exact() {
            Self::Exact(bound.upper_bound())
        } else {
            Self::UpperBound(bound.upper_bound())
        }
    }
}

/// The model's total context window: the meter denominator, separate from
/// the capacity-derived auto-compact trigger.
///
/// This is the figure the provider states for the model (272k, 1M, 500k),
/// never the admitted haircut below it. It is not, by itself, a provider
/// capacity guarantee; the evidence class and non-guarantee travel with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TotalContextWindow {
    tokens: u64,
    basis: TotalWindowBasis,
}

impl TotalContextWindow {
    /// Returns the total window in tokens.
    #[must_use]
    pub const fn tokens(self) -> u64 {
        self.tokens
    }

    /// Returns where the total-window figure comes from.
    #[must_use]
    pub const fn basis(self) -> TotalWindowBasis {
        self.basis
    }
}

/// Where the total-window figure comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TotalWindowBasis {
    /// The provider's authenticated catalog or a release certificate states
    /// this total input-plus-output window.
    ProviderReported,
    /// Input and output are metered separately; the total is the input
    /// capacity, the only window prepared input can occupy.
    InputCapacity,
    /// No provider window is known; the caller-declared input maximum is the
    /// only stated bound and stands in as the window.
    DeclaredInputMaximum,
}

impl TotalWindowBasis {
    /// Returns stable product wording for the Context tab.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ProviderReported => "provider-reported",
            Self::InputCapacity => "input-capacity",
            Self::DeclaredInputMaximum => "declared-input-maximum",
        }
    }
}

/// Authority behind the conservative boundary Plakous actually admits against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmittedCeilingBasis {
    /// The exact capacity axis is known for this admitted scope.
    ExactCapacity,
    /// Admission uses a guaranteed capacity floor, not the unknown true limit.
    GuaranteedCapacityFloor,
    /// Admission reproduces a non-guaranteed operating envelope.
    OperatingEnvelope,
}

/// Maximum prepared input actionable under the admitted context rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SafeAdmittedInputCeiling {
    tokens: u64,
    basis: AdmittedCeilingBasis,
}

impl SafeAdmittedInputCeiling {
    /// Returns the dispatch gate: prepared input above this would overflow.
    /// It is a separate, labelled fact beside the window, never the meter
    /// denominator.
    #[must_use]
    pub const fn tokens(self) -> u64 {
        self.tokens
    }

    /// Returns why this boundary is safe to act on.
    #[must_use]
    pub const fn basis(self) -> AdmittedCeilingBasis {
        self.basis
    }
}

/// Product-visible context-evidence class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceClass {
    /// Release-owned exact-scope evidence.
    ReleaseVerified,
    /// Pinned standard reference-client operating envelope.
    ReferenceClientEnvelope,
    /// Caller-declared custom-integration operating envelope.
    CustomEnvelope,
    /// No safely understood evidence class is available.
    Unavailable,
}

impl EvidenceClass {
    /// Returns stable product wording without broadening the evidence claim.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ReleaseVerified => "release-verified",
            Self::ReferenceClientEnvelope => "reference-client-envelope",
            Self::CustomEnvelope => "custom-envelope",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Mandatory qualification for a non-guaranteed operating envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NonGuarantee {
    /// Reference-client behavior is not a provider hard-cap guarantee.
    ReferenceClientOperatingEnvelope,
    /// A custom caller declaration is not provider capacity evidence.
    CustomCallerOperatingEnvelope,
    /// An unanchored estimate delegates context acceptance to the provider.
    UnanchoredProviderDecision,
}

impl NonGuarantee {
    /// Returns explicit product wording for the non-guarantee.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::UnanchoredProviderDecision => {
                "unanchored estimate; the provider decides context acceptance"
            }
            Self::ReferenceClientOperatingEnvelope => {
                "reference-client operating envelope; not a provider hard-cap guarantee"
            }
            Self::CustomCallerOperatingEnvelope => {
                "custom caller operating envelope; not provider capacity evidence"
            }
        }
    }
}

/// Half-up percent of `occupied` over `window`.
#[must_use]
pub fn window_percent(occupied: u64, window: u64) -> Option<u128> {
    if window == 0 {
        return None;
    }
    let total = u128::from(window);
    Some((u128::from(occupied) * 100 + total / 2) / total)
}

/// Authoritative context-status disposition projected for the product.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextDisposition {
    /// The complete prepared projection fits.
    Fits,
    /// Capacity overflowed and whole compactable groups may exist.
    WouldOverflow,
    /// Capacity overflowed with only protected material remaining.
    ProtectedOverflow,
    /// Context status is safely unavailable.
    Unavailable,
    /// Session recovery must complete before context can be inspected.
    RecoveryRequired,
}

impl ContextDisposition {
    /// Returns stable product wording for the runtime classification.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fits => "fits",
            Self::WouldOverflow => "would-overflow",
            Self::ProtectedOverflow => "protected-overflow",
            Self::Unavailable => "unavailable",
            Self::RecoveryRequired => "recovery-required",
        }
    }
}

/// Actionable utilization with honest overflow and gauge semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionableUtilization {
    display_percent: u128,
    gauge_percent: u8,
    ceiling_excess: Option<CeilingExcess>,
}

/// What a positive difference above the admitted ceiling actually proves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CeilingExcess {
    /// An exact prepared-input count is this many tokens beyond the ceiling.
    ExactOverflow(u64),
    /// A conservative upper bound exceeds the ceiling by this amount; actual
    /// overflow is not established.
    UpperBoundExcess(u64),
}

impl ActionableUtilization {
    /// Returns half-up utilization, which may exceed 100 to expose overflow.
    #[must_use]
    pub const fn display_percent(self) -> u128 {
        self.display_percent
    }

    /// Returns the rendering-safe gauge value clamped to 0 through 100.
    #[must_use]
    pub const fn gauge_percent(self) -> u8 {
        self.gauge_percent
    }

    /// Returns the evidence-aware difference above the admission boundary.
    #[must_use]
    pub const fn ceiling_excess(self) -> Option<CeilingExcess> {
        self.ceiling_excess
    }
}

/// One truthful, presentation-only context snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextProjection {
    disposition: ContextDisposition,
    prepared_input: Option<PreparedInputCount>,
    total_window: Option<TotalContextWindow>,
    safe_admitted_input_ceiling: Option<SafeAdmittedInputCeiling>,
    auto_compact_input: Option<u64>,
    evidence: EvidenceClass,
    non_guarantee: Option<NonGuarantee>,
    input_parts: Option<plakous::model::ContextInputParts>,
    /// The whole-request estimate the Plakous context measure starts from.
    occupancy_estimate: Option<u64>,
    authoritative_occupied: Option<u64>,
    unanchored_estimate: bool,
    /// The provider count the measure is anchored on (spec 01 §10.3).
    context_anchor: Option<plakous::model::ContextAnchor>,
}

/// Per-part token estimate for presentation occupancy.
///
/// Tokens are UTF-8 part bytes divided by four — the same method as spec
/// §6.7 — not an apportionment of the admission bound. Admission still uses
/// each vertical's `CountObservation`. Enrolled instruction-source bytes are
/// subtracted from the system part when supplied. Envelope JSON (`model`,
/// `stream`, options) is the remainder of `total_bytes`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextInputEstimate {
    /// Estimated core system-prompt tokens (instructions minus enrolled sources).
    pub system_tokens: u64,
    /// Estimated enrolled instruction-source tokens (`AGENTS.md` chain).
    pub enrolled_source_tokens: u64,
    /// Estimated tool definition tokens.
    pub tool_definition_tokens: u64,
    /// Exact number of tool definitions on the wire.
    pub tool_definition_count: u32,
    /// Estimated conversation message tokens.
    pub message_tokens: u64,
    /// Estimated replayed/encrypted reasoning tokens.
    pub reasoning_tokens: u64,
    /// Estimated envelope JSON tokens (unattributed request keys).
    pub envelope_tokens: u64,
}

impl ContextInputEstimate {
    /// Spec 02 §5.6 token method: UTF-8 bytes divided by four.
    #[must_use]
    pub const fn approx_tokens(bytes: u64) -> u64 {
        bytes / 4
    }

    fn from_parts(
        parts: plakous::model::ContextInputParts,
        enrolled_source_bytes: u64,
    ) -> Option<Self> {
        if parts.total_bytes() == 0 {
            return None;
        }
        let enrolled = enrolled_source_bytes.min(parts.system_bytes());
        let system_core = parts.system_bytes().saturating_sub(enrolled);
        let attributed = parts
            .system_bytes()
            .saturating_add(parts.tool_definition_bytes())
            .saturating_add(parts.message_bytes())
            .saturating_add(parts.reasoning_bytes());
        let envelope = parts
            .total_bytes()
            .saturating_sub(parts.image_transport_bytes())
            .saturating_sub(attributed);
        Some(Self {
            system_tokens: Self::approx_tokens(system_core),
            enrolled_source_tokens: Self::approx_tokens(enrolled),
            tool_definition_tokens: Self::approx_tokens(parts.tool_definition_bytes()),
            tool_definition_count: parts.tool_definition_count(),
            message_tokens: Self::approx_tokens(parts.message_bytes())
                .saturating_add(parts.image_tokens()),
            reasoning_tokens: Self::approx_tokens(parts.reasoning_bytes()),
            envelope_tokens: Self::approx_tokens(envelope),
        })
    }

    /// Occupying tokens for the composer meter and Context board.
    #[must_use]
    pub const fn occupancy_tokens(self) -> u64 {
        self.system_tokens
            .saturating_add(self.enrolled_source_tokens)
            .saturating_add(self.tool_definition_tokens)
            .saturating_add(self.message_tokens)
            .saturating_add(self.reasoning_tokens)
            .saturating_add(self.envelope_tokens)
    }
}

impl ContextProjection {
    pub(crate) fn with_prepared_request(
        mut self,
        parts: Option<plakous::model::ContextInputParts>,
        occupied: Option<u64>,
        prepared: Option<u64>,
    ) -> Self {
        self.input_parts = parts;
        self.occupancy_estimate = parts.map(plakous::model::ContextInputParts::occupancy_tokens);
        self.authoritative_occupied = occupied;
        self.prepared_input = prepared.map(PreparedInputCount::UpperBound);
        self
    }
    /// Returns a projection for a session that must first be recovered.
    #[must_use]
    pub const fn recovery_required() -> Self {
        Self::empty(ContextDisposition::RecoveryRequired)
    }

    /// Returns a projection when provider-grounded status is unavailable.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self::empty(ContextDisposition::Unavailable)
    }

    const fn empty(disposition: ContextDisposition) -> Self {
        Self {
            disposition,
            prepared_input: None,
            total_window: None,
            safe_admitted_input_ceiling: None,
            auto_compact_input: None,
            evidence: EvidenceClass::Unavailable,
            non_guarantee: None,
            input_parts: None,
            occupancy_estimate: None,
            authoritative_occupied: None,
            unanchored_estimate: false,
            context_anchor: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_fits(
        prepared_input: PreparedInputCount,
        total_window: u64,
        safe_ceiling: u64,
        input_parts: Option<plakous::model::ContextInputParts>,
    ) -> Self {
        Self {
            disposition: ContextDisposition::Fits,
            prepared_input: Some(prepared_input),
            total_window: Some(TotalContextWindow {
                tokens: total_window,
                basis: TotalWindowBasis::ProviderReported,
            }),
            safe_admitted_input_ceiling: Some(SafeAdmittedInputCeiling {
                tokens: safe_ceiling,
                basis: AdmittedCeilingBasis::OperatingEnvelope,
            }),
            auto_compact_input: Some(
                CapacityRule::ProviderReservedInput {
                    reported_context_window: None,
                    maximum_projected_input: safe_ceiling,
                    auto_compact_input: None,
                }
                .compaction_trigger_tokens(
                    0,
                    0,
                    u64::from(plakous::model::SEMANTIC_COMPACTION_SUMMARY_TOKENS),
                ),
            ),
            evidence: EvidenceClass::ReferenceClientEnvelope,
            non_guarantee: Some(NonGuarantee::ReferenceClientOperatingEnvelope),
            input_parts,
            authoritative_occupied: None,
            unanchored_estimate: false,
            occupancy_estimate: Some(input_parts.map_or(
                prepared_input.tokens(),
                plakous::model::ContextInputParts::occupancy_tokens,
            )),
            context_anchor: None,
        }
    }

    /// The same measure under another admission disposition.
    #[cfg(test)]
    pub(crate) const fn with_disposition(mut self, disposition: ContextDisposition) -> Self {
        self.disposition = disposition;
        self
    }

    /// An unanchored measure of `tokens`: a whole-request estimate no round
    /// has anchored yet.
    #[cfg(test)]
    pub(crate) const fn with_occupied_tokens(mut self, tokens: Option<u64>) -> Self {
        self.occupancy_estimate = tokens;
        self.authoritative_occupied = None;
        self.context_anchor = None;
        self
    }

    /// A measure of `tokens` anchored on a provider count of that request.
    #[cfg(test)]
    pub(crate) const fn with_anchored_occupied_tokens(self, tokens: u64) -> Self {
        self.with_occupancy_measure(
            Some(tokens),
            plakous::model::ContextAnchor::new(tokens, tokens),
        )
    }

    #[cfg(test)]
    pub(crate) const fn with_occupancy_measure(
        mut self,
        estimate: Option<u64>,
        anchor: Option<plakous::model::ContextAnchor>,
    ) -> Self {
        self.occupancy_estimate = estimate;
        self.authoritative_occupied = None;
        self.context_anchor = anchor;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_auto_compact_input(mut self, tokens: Option<u64>) -> Self {
        self.auto_compact_input = tokens;
        self
    }

    /// Returns the per-part occupancy estimate when the preparing provider
    /// measured the body's byte composition. Tokens are `part_bytes÷4`.
    #[must_use]
    pub fn input_estimate(self) -> Option<ContextInputEstimate> {
        self.input_estimate_with_enrolled(0)
    }

    /// Like [`Self::input_estimate`], subtracting enrolled instruction-source
    /// bytes from the system part so `AGENTS.md` is not folded into it.
    #[must_use]
    pub fn input_estimate_with_enrolled(
        self,
        enrolled_source_bytes: u64,
    ) -> Option<ContextInputEstimate> {
        ContextInputEstimate::from_parts(self.input_parts?, enrolled_source_bytes)
    }

    /// The context measure Plakous reported for this request: the last
    /// provider-reported input plus the `bytes÷4` estimate of the suffix
    /// appended since, or the whole request's `bytes÷4` without a provider
    /// report. The chrome meter, the Context board and the compaction fences
    /// read this number; compaction records use
    /// [`Self::unanchored_occupied_tokens`].
    #[must_use]
    pub const fn occupied_tokens(self) -> Option<u64> {
        if matches!(
            self.disposition,
            ContextDisposition::Unavailable | ContextDisposition::RecoveryRequired
        ) {
            return None;
        }
        if let Some(occupied) = self.authoritative_occupied {
            return Some(occupied);
        }
        match self.occupancy_estimate {
            Some(estimate) => Some(plakous::model::context_occupancy_tokens(
                estimate,
                self.context_anchor,
            )),
            None => None,
        }
    }

    /// The whole-request estimate with no anchor applied: the one basis a
    /// compaction's before and after figures share.
    #[must_use]
    pub const fn unanchored_occupied_tokens(self) -> Option<u64> {
        if matches!(
            self.disposition,
            ContextDisposition::Unavailable | ContextDisposition::RecoveryRequired
        ) {
            return None;
        }
        self.occupancy_estimate
    }

    /// The whole-request estimate [`Self::occupied_tokens`] is measured from.
    #[must_use]
    pub const fn occupancy_estimate(self) -> Option<u64> {
        self.occupancy_estimate
    }

    /// Whether the preparing provider measured the body's parts, so
    /// [`Self::occupancy_estimate`] is a parts estimate a provider count may
    /// anchor rather than the admission bound.
    #[must_use]
    pub(crate) const fn measures_input_parts(self) -> bool {
        self.input_parts.is_some()
    }

    /// The provider count [`Self::occupied_tokens`] is anchored on. `None`
    /// until a completed round reported one, so an occupancy fence may not
    /// act on the measure yet (spec 01 §10.3).
    #[must_use]
    pub const fn context_anchor(self) -> Option<plakous::model::ContextAnchor> {
        self.context_anchor
    }

    /// Token trigger for idle auto-compact, when the envelope publishes one
    /// (Grok OAuth: 400k of a 500k window).
    #[must_use]
    pub fn auto_compact_trigger(self) -> Option<u64> {
        self.auto_compact_input.or_else(|| {
            self.safe_admitted_input_ceiling.map(|ceiling| {
                CapacityRule::ProviderReservedInput {
                    reported_context_window: None,
                    maximum_projected_input: ceiling.tokens(),
                    auto_compact_input: None,
                }
                .compaction_trigger_tokens(
                    0,
                    0,
                    u64::from(plakous::model::SEMANTIC_COMPACTION_SUMMARY_TOKENS),
                )
            })
        })
    }

    /// Returns the authoritative status classification.
    #[must_use]
    pub const fn disposition(self) -> ContextDisposition {
        self.disposition
    }

    /// Returns the exact or conservative complete prepared-input count.
    #[must_use]
    pub const fn prepared_input(self) -> Option<PreparedInputCount> {
        self.prepared_input
    }

    /// Returns the model's total context window: the meter denominator.
    #[must_use]
    pub const fn total_window(self) -> Option<TotalContextWindow> {
        self.total_window
    }

    /// Returns the admitted input ceiling (dispatch gate), a labelled fact
    /// beside the window.
    #[must_use]
    pub const fn safe_admitted_input_ceiling(self) -> Option<SafeAdmittedInputCeiling> {
        self.safe_admitted_input_ceiling
    }

    /// Returns the non-coercible evidence class.
    #[must_use]
    pub const fn evidence(self) -> EvidenceClass {
        self.evidence
    }

    /// Returns the mandatory envelope qualification, when applicable.
    #[must_use]
    pub const fn non_guarantee(self) -> Option<NonGuarantee> {
        self.non_guarantee
    }

    /// Percent is [Self::occupied_tokens] over the working envelope for the
    /// occupancy gauge; only the overflow excess is measured against the
    /// admitted ceiling (dispatch gate), from the admission bound. Fits /
    /// WouldOverflow / `ceiling_excess` stay envelope-only; occupancy never
    /// admits or refuses a turn. A zero window or ceiling yields no
    /// percentage rather than a misleading one.
    pub fn actionable_utilization(self) -> Option<ActionableUtilization> {
        let occupied_tokens = self.occupied_tokens()?;
        let envelope_tokens = self.prepared_input?.tokens();
        let ceiling = self.safe_admitted_input_ceiling?.tokens();
        let window = self.total_window?.tokens();
        if ceiling == 0 || window == 0 {
            return None;
        }
        let occupied = u128::from(occupied_tokens);
        // Chrome is percent of the reported window. Grok OAuth compact
        // trigger is 80% of that window (400k of 500k), not 85% of a 400k
        // admit haircut. Routes without auto_compact_input still fence at
        // the capacity-derived token trigger.
        let fence_base = u128::from(window.min(ceiling));
        let display_percent = (occupied * 100 + fence_base / 2) / fence_base;
        let gauge_percent = u8::try_from(display_percent.min(100)).ok()?;
        let difference = envelope_tokens.saturating_sub(ceiling);
        let ceiling_excess = if difference == 0 {
            None
        } else {
            Some(match self.prepared_input? {
                PreparedInputCount::Exact(_) => CeilingExcess::ExactOverflow(difference),
                PreparedInputCount::UpperBound(_) => CeilingExcess::UpperBoundExcess(difference),
            })
        };
        Some(ActionableUtilization {
            display_percent,
            gauge_percent,
            ceiling_excess,
        })
    }
}

/// Projects a public Plakous status without creating admission authority.
///
/// Unknown or internally contradictory future variants fail closed to an
/// unavailable product projection.
pub fn project_context_status(status: &ContextStatus) -> ContextProjection {
    let disposition = match status.kind() {
        ContextStatusKind::Fits { .. } => ContextDisposition::Fits,
        ContextStatusKind::WouldOverflow(_) => ContextDisposition::WouldOverflow,
        ContextStatusKind::ProtectedOverflow(_) => ContextDisposition::ProtectedOverflow,
        ContextStatusKind::Unavailable(_) => return ContextProjection::unavailable(),
        _ => return ContextProjection::unavailable(),
    };
    let Some((evidence, non_guarantee)) =
        project_evidence(status.evidence_class(), status.non_guarantee_disposition())
    else {
        return ContextProjection::unavailable();
    };
    let Some(prepared_input) = status
        .local_input_upper_bound()
        .map(PreparedInputCount::UpperBound)
        .or_else(|| {
            status
                .measurement()
                .map(plakous::model::ContextMeasurement::bound)
                .map(PreparedInputCount::from)
        })
    else {
        return ContextProjection::unavailable();
    };
    let Some(capacity) = status.capacity() else {
        return ContextProjection::unavailable();
    };
    let Some((total_window, safe_admitted_input_ceiling, auto_compact_input)) = capacity_projection(
        capacity.rule(),
        status
            .reservation()
            .map(plakous::model::OutputReservation::tokens),
    ) else {
        return ContextProjection::unavailable();
    };
    ContextProjection {
        disposition,
        prepared_input: Some(prepared_input),
        total_window: Some(total_window),
        safe_admitted_input_ceiling: Some(safe_admitted_input_ceiling),
        auto_compact_input: status.compaction_trigger_tokens().or(auto_compact_input),
        evidence,
        non_guarantee,
        input_parts: status.input_parts(),
        occupancy_estimate: status.unanchored_occupancy_tokens(),
        authoritative_occupied: status.occupancy_tokens(),
        unanchored_estimate: status.uses_unanchored_estimate(),
        // An anchor only applies on the parts scale; a bound-only status
        // reads unanchored.
        context_anchor: status
            .occupancy_estimate_tokens()
            .and(status.context_anchor()),
    }
}

impl ContextProjection {
    /// The scheduling verdict Plakous's one rule gives this measure (chapter
    /// 01 §10.3): an overflow disposition; anchored occupancy at the compact
    /// trigger; or an admission bound within the tool-observation reserve of
    /// the admitted ceiling. An unanchored measure reaches neither fit ground;
    /// the reserve reads the admission bound, never occupancy, and a measure
    /// admitted only on the unanchored estimate has no bound to read.
    #[must_use]
    pub fn compaction_verdict(self) -> plakous::agent::CompactionVerdict {
        use plakous::agent::{CompactionMeasure, CompactionVerdict, MeasuredFit};
        let fit = match self.disposition {
            ContextDisposition::Fits => MeasuredFit::Fits,
            ContextDisposition::WouldOverflow | ContextDisposition::ProtectedOverflow => {
                MeasuredFit::Overflows
            }
            ContextDisposition::Unavailable | ContextDisposition::RecoveryRequired => {
                MeasuredFit::Unmeasured
            }
        };
        let input_headroom = if self.unanchored_estimate {
            None
        } else {
            self.prepared_input
                .zip(self.safe_admitted_input_ceiling)
                .map(|(bound, ceiling)| ceiling.tokens().saturating_sub(bound.tokens()))
        };
        CompactionVerdict::of(CompactionMeasure {
            fit,
            occupancy: self.context_anchor.and(self.occupied_tokens()),
            trigger: self.auto_compact_trigger(),
            input_headroom,
        })
    }
}

/// At most one extra scheduled compact after a committed compact still triggers.
pub const SCHEDULED_COMPACT_FOLLOW_ON_LIMIT: u32 = 1;

/// Operator note when scheduled compact would fire but `compaction.enabled=false`.
#[must_use]
pub const fn scheduled_compaction_disabled_note(disposition: ContextDisposition) -> &'static str {
    match disposition {
        ContextDisposition::WouldOverflow | ContextDisposition::ProtectedOverflow => {
            "authoritative context would overflow; scheduled compaction is disabled"
        }
        _ => "scheduled compaction is disabled at the context trigger",
    }
}

fn capacity_projection(
    rule: CapacityRule,
    reserved_output: Option<u64>,
) -> Option<(TotalContextWindow, SafeAdmittedInputCeiling, Option<u64>)> {
    let reserved = match rule {
        CapacityRule::SharedWindow { .. } | CapacityRule::SharedWindowWithOutputCap { .. } => {
            reserved_output.unwrap_or(0)
        }
        _ => 0,
    };
    let auto_compact_input = Some(rule.compaction_trigger_tokens(
        reserved,
        0,
        u64::from(plakous::model::SEMANTIC_COMPACTION_SUMMARY_TOKENS),
    ));
    let (window, tokens, basis) = match rule {
        CapacityRule::ProviderReservedInput {
            reported_context_window,
            maximum_projected_input,
            ..
        } if reserved_output.is_none() => (
            match reported_context_window {
                Some(tokens) => TotalContextWindow {
                    tokens,
                    basis: TotalWindowBasis::ProviderReported,
                },
                None => TotalContextWindow {
                    tokens: maximum_projected_input,
                    basis: TotalWindowBasis::DeclaredInputMaximum,
                },
            },
            maximum_projected_input,
            AdmittedCeilingBasis::OperatingEnvelope,
        ),
        CapacityRule::SharedWindow { total }
        | CapacityRule::SharedWindowWithOutputCap { total, .. } => (
            TotalContextWindow {
                tokens: total.lower_bound(),
                basis: TotalWindowBasis::ProviderReported,
            },
            total.lower_bound().saturating_sub(reserved_output?),
            safe_limit_basis(total),
        ),
        CapacityRule::Separate { input, .. } if reserved_output.is_some() => (
            TotalContextWindow {
                tokens: input.lower_bound(),
                basis: TotalWindowBasis::InputCapacity,
            },
            input.lower_bound(),
            safe_limit_basis(input),
        ),
        _ => return None,
    };
    Some((
        window,
        SafeAdmittedInputCeiling { tokens, basis },
        auto_compact_input,
    ))
}

const fn safe_limit_basis(limit: SafeLimit) -> AdmittedCeilingBasis {
    if limit.is_exact() {
        AdmittedCeilingBasis::ExactCapacity
    } else {
        AdmittedCeilingBasis::GuaranteedCapacityFloor
    }
}

fn project_evidence(
    evidence: Option<&ContextEvidenceClass>,
    non_guarantee: Option<ContextNonGuaranteeDisposition>,
) -> Option<(EvidenceClass, Option<NonGuarantee>)> {
    if non_guarantee == Some(ContextNonGuaranteeDisposition::UnanchoredProviderDecision) {
        let evidence = match evidence? {
            ContextEvidenceClass::ReleaseVerified => EvidenceClass::ReleaseVerified,
            ContextEvidenceClass::ReferenceClientEnvelope { .. } => {
                EvidenceClass::ReferenceClientEnvelope
            }
            ContextEvidenceClass::CustomEnvelope { .. } => EvidenceClass::CustomEnvelope,
            _ => return None,
        };
        return Some((evidence, Some(NonGuarantee::UnanchoredProviderDecision)));
    }
    match (evidence, non_guarantee) {
        (Some(ContextEvidenceClass::ReleaseVerified), None) => {
            Some((EvidenceClass::ReleaseVerified, None))
        }
        (
            Some(ContextEvidenceClass::ReferenceClientEnvelope { .. }),
            Some(ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope),
        ) => Some((
            EvidenceClass::ReferenceClientEnvelope,
            Some(NonGuarantee::ReferenceClientOperatingEnvelope),
        )),
        (
            Some(ContextEvidenceClass::CustomEnvelope { .. }),
            Some(ContextNonGuaranteeDisposition::CustomCallerOperatingEnvelope),
        ) => Some((
            EvidenceClass::CustomEnvelope,
            Some(NonGuarantee::CustomCallerOperatingEnvelope),
        )),
        _ => None,
    }
}

/// The verdict questions the context and compaction tests ask of a projection.
#[cfg(test)]
pub(crate) mod test_verdicts {
    use super::{ContextDisposition, ContextProjection, PreparedInputCount};

    /// Whether the idle fence's occupancy question schedules a compact:
    /// overflow or anchored occupancy at the trigger.
    pub(crate) fn idle_fence_schedules(projection: ContextProjection) -> bool {
        let verdict = projection.compaction_verdict();
        verdict.overflow() || verdict.occupancy()
    }

    /// The verdict of a measure far below every fit trigger, with
    /// `disposition`: only an overflow schedules it.
    pub(crate) fn disposition_schedules(disposition: ContextDisposition) -> bool {
        ContextProjection::test_fits(PreparedInputCount::Exact(1_000), 100_000, 100_000, None)
            .with_disposition(disposition)
            .compaction_verdict()
            .is_due()
    }
}

#[cfg(test)]
mod tests;
