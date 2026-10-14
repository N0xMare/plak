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

/// Input headroom Plakous keeps ahead of tool observations. A prompt that
/// reaches this boundary must compact before dispatch. This observation
/// reserve also acts independently of the capacity-derived token trigger.
pub const OBSERVATION_HEADROOM_RESERVE_TOKENS: u64 = 4_096;

impl ContextProjection {
    /// Whether the admitted input envelope has reached the tool-observation
    /// reserve. This guards admission, so it compares the admission bound to
    /// the admitted ceiling, never occupancy to the advertised window.
    #[must_use]
    pub fn observation_headroom_requires_compaction(self) -> bool {
        if self.disposition != ContextDisposition::Fits || self.unanchored_estimate {
            return false;
        }
        let Some(bound) = self.prepared_input.map(PreparedInputCount::tokens) else {
            return false;
        };
        let Some(ceiling) = self.safe_admitted_input_ceiling else {
            return false;
        };
        ceiling.tokens().saturating_sub(bound) <= OBSERVATION_HEADROOM_RESERVE_TOKENS
    }
}

/// Reports whether product policy may request compaction for this status.
///
/// `ProtectedOverflow` schedules on every route. Latest-intent tool growth is
/// protected against the semantic cut, and only Codex remote compact used to
/// be able to drop tool material, so every other route refused the turn. The
/// placeholder rung now replaces the oldest tool-result bodies offline on any
/// route, so the disposition alone decides that a compact runs; which rung
/// acts is the ladder's business, not this predicate's.
#[must_use]
pub const fn should_schedule_compaction(disposition: ContextDisposition) -> bool {
    matches!(
        disposition,
        ContextDisposition::WouldOverflow | ContextDisposition::ProtectedOverflow
    )
}

/// Whether occupancy has reached this projection's compact trigger.
///
/// Grok OAuth publishes `auto_compact_input` (400k of a 500k window). Other
/// routes use the ceiling-minus-headroom token trigger. An unanchored measure (before
/// the session's first completed round, which includes a session carried
/// forward from another build, and after a compaction or branch transition)
/// never fires the occupancy fence: its whole-request
/// estimate can overstate the provider's count several times over, and the
/// compaction it would schedule is paid. Only an overflow disposition acts
/// before the first round anchors.
#[must_use]
pub fn occupancy_requires_scheduled_compact(projection: ContextProjection) -> bool {
    match projection.disposition() {
        ContextDisposition::WouldOverflow | ContextDisposition::ProtectedOverflow => true,
        ContextDisposition::Fits => {
            if projection.context_anchor.is_none() {
                return false;
            }
            let Some(occupied) = projection.occupied_tokens() else {
                return false;
            };
            projection
                .auto_compact_trigger()
                .is_some_and(|trigger| occupied >= trigger)
        }
        _ => false,
    }
}

/// At most one extra scheduled compact after a committed compact still triggers.
pub const SCHEDULED_COMPACT_FOLLOW_ON_LIMIT: u32 = 1;

/// True when a committed scheduled compact may start one new-op follow-on.
#[must_use]
pub const fn scheduled_compact_should_follow_on(
    follow_ons_used: u32,
    disposition: ContextDisposition,
) -> bool {
    follow_ons_used < SCHEDULED_COMPACT_FOLLOW_ON_LIMIT
        && !matches!(disposition, ContextDisposition::Fits)
        && should_schedule_compaction(disposition)
}

/// Overflow parks the queued prompt when scheduled compact is disabled.
/// Fits above its token trigger still dispatches; it does not invent an overflow park.
#[must_use]
pub const fn scheduled_compaction_parks_when_disabled(disposition: ContextDisposition) -> bool {
    matches!(
        disposition,
        ContextDisposition::WouldOverflow | ContextDisposition::ProtectedOverflow
    )
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn projection(
        prepared_input: PreparedInputCount,
        total_window: u64,
        safe_ceiling: u64,
    ) -> ContextProjection {
        ContextProjection {
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
            auto_compact_input: None,
            evidence: EvidenceClass::ReferenceClientEnvelope,
            non_guarantee: Some(NonGuarantee::ReferenceClientOperatingEnvelope),
            input_parts: None,
            occupancy_estimate: Some(prepared_input.tokens()),
            authoritative_occupied: None,
            unanchored_estimate: false,
            context_anchor: plakous::model::ContextAnchor::new(
                prepared_input.tokens(),
                prepared_input.tokens(),
            ),
        }
    }

    /// One vertical's fixture: reported window and admitted input ceiling.
    /// The actionable meter uses the tighter of the two.
    struct VerticalFixture {
        name: &'static str,
        total_window: u64,
        admitted_ceiling: u64,
    }

    /// Catalog truth as of 2026-09-03 (see the context-window table in the
    /// change that introduced `TotalContextWindow`).
    const VERTICALS: [VerticalFixture; 5] = [
        VerticalFixture {
            name: "codex gpt-5.6-luna/sol/terra (272k, 95% envelope)",
            total_window: 272_000,
            admitted_ceiling: 258_400,
        },
        VerticalFixture {
            name: "codex gpt-5.3-codex-spark (128k, 95% envelope)",
            total_window: 128_000,
            admitted_ceiling: 121_600,
        },
        VerticalFixture {
            name: "claude sonnet-5/opus-5/fable (1M, full window)",
            total_window: 1_000_000,
            admitted_ceiling: 1_000_000,
        },
        VerticalFixture {
            name: "claude sonnet-4-6/opus-4-6 (200k, full window)",
            total_window: 200_000,
            admitted_ceiling: 200_000,
        },
        VerticalFixture {
            name: "grok-4.6/grok-4.5 (500k window, 400k compact trigger)",
            total_window: 500_000,
            admitted_ceiling: 500_000,
        },
    ];

    /// Every vertical's occupancy is `part_bytes÷4`, independent of the
    /// admission bound: the tool count is exact, a body the provider did not
    /// measure yields no estimate, and an inflated bound does not smear onto
    /// tools.
    #[test]
    fn every_vertical_estimates_occupancy_from_part_bytes_over_four() {
        let responses = br#"{"model":"m","instructions":"you are plak","tools":[{"type":"function","name":"read_file"},{"type":"function","name":"exec_command"}],"input":[{"role":"user","content":[{"type":"input_text","text":"hello there"}]},{"type":"reasoning","encrypted_content":"ciphertextciphertext"}],"include":["reasoning.encrypted_content"],"store":false}"#;
        let messages = br#"{"model":"m","system":"you are plak","tools":[{"name":"read_file"}],"messages":[{"role":"user","content":"hello there"}],"thinking":{"type":"adaptive"},"max_tokens":8}"#;
        let bodies: [(&str, &[u8]); 2] = [("responses", responses), ("messages", messages)];
        for fixture in &VERTICALS {
            for (kernel, body) in bodies {
                let parts = plakous::model::ContextInputParts::measure_wire(body).unwrap();
                let bound = fixture.total_window / 10;
                let mut projection = projection(
                    PreparedInputCount::UpperBound(bound),
                    fixture.total_window,
                    fixture.admitted_ceiling,
                );
                assert!(
                    projection.input_estimate().is_none(),
                    "{} {kernel}",
                    fixture.name
                );
                projection.input_parts = Some(parts);
                let estimate = projection.input_estimate().unwrap();
                assert_eq!(
                    estimate.system_tokens,
                    ContextInputEstimate::approx_tokens(parts.system_bytes()),
                    "{} {kernel}",
                    fixture.name
                );
                assert_eq!(
                    estimate.tool_definition_tokens,
                    ContextInputEstimate::approx_tokens(parts.tool_definition_bytes()),
                    "{} {kernel}",
                    fixture.name
                );
                assert_eq!(
                    estimate.tool_definition_count,
                    if kernel == "responses" { 2 } else { 1 },
                    "{} {kernel}",
                    fixture.name
                );
                assert_eq!(
                    estimate.reasoning_tokens > 0,
                    kernel == "responses",
                    "{} {kernel}: only the Responses body replays reasoning",
                    fixture.name
                );
                let occupancy = estimate.occupancy_tokens();
                assert!(occupancy > 0, "{} {kernel}", fixture.name);
                assert!(
                    occupancy < bound,
                    "{} {kernel}: occupancy {occupancy} must not inherit inflated bound {bound}",
                    fixture.name
                );
                assert_eq!(
                    occupancy,
                    parts.occupancy_tokens(),
                    "{} {kernel}: the categories split the Plakous whole-request estimate",
                    fixture.name
                );
            }
        }
    }

    #[test]
    fn enrolled_source_bytes_split_out_of_the_system_part() {
        let body = br#"{"model":"m","instructions":"core prompt\n\nrepo agents","tools":[],"input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let parts = plakous::model::ContextInputParts::measure_wire(body).unwrap();
        let enrolled = "repo agents".len() as u64;
        let mut projection = projection(PreparedInputCount::UpperBound(50_000), 200_000, 200_000);
        projection.input_parts = Some(parts);
        let without = projection.input_estimate().unwrap();
        let with = projection.input_estimate_with_enrolled(enrolled).unwrap();
        assert_eq!(
            with.enrolled_source_tokens,
            ContextInputEstimate::approx_tokens(enrolled)
        );
        assert!(
            with.system_tokens < without.system_tokens,
            "enrolled bytes must leave the system row: {with:?} vs {without:?}"
        );
        assert!(
            with.occupancy_tokens().abs_diff(without.occupancy_tokens()) <= 1,
            "integer division of parts may differ by one token: {with:?} vs {without:?}"
        );
    }

    /// The 2026 telemetry case: the provider counted 298,468 input tokens for
    /// a request whose bound was 850,715. The meter, the idle fence and the
    /// compact trigger read the Plakous context measure; only the ceiling
    /// excess still reads the admission bound.
    #[test]
    fn occupancy_is_the_plakous_context_measure_not_the_bound() {
        let mut projected = projection(
            PreparedInputCount::UpperBound(850_715),
            1_000_000,
            1_000_000,
        );
        projected.occupancy_estimate = Some(850_715 + 1_000);
        projected.context_anchor = plakous::model::ContextAnchor::new(298_468, 850_715);
        projected.auto_compact_input = Some(800_000);
        assert_eq!(projected.occupied_tokens(), Some(299_468));
        let utilization = projected.actionable_utilization().unwrap();
        assert_eq!(utilization.display_percent(), 30);
        assert_eq!(utilization.ceiling_excess(), None);
        assert!(!occupancy_requires_scheduled_compact(projected));
        let on_its_own_scale = projection(
            PreparedInputCount::UpperBound(850_715),
            1_000_000,
            1_000_000,
        );
        assert_eq!(
            on_its_own_scale
                .actionable_utilization()
                .unwrap()
                .display_percent(),
            85
        );
        assert!(!occupancy_requires_scheduled_compact(on_its_own_scale));
        let mut unavailable = projected;
        unavailable.disposition = ContextDisposition::Unavailable;
        assert_eq!(unavailable.occupied_tokens(), None);
    }

    /// The anchor is absent before a session's first completed round (which
    /// includes a session carried forward from another build) and after a
    /// compaction or branch transition. Its whole-request estimate is shown, but it
    /// never schedules the paid idle compaction by itself; an overflow
    /// disposition and the bound-based observation reserve still do.
    #[test]
    fn an_unanchored_measure_never_fires_the_idle_occupancy_fence() {
        let unanchored = projection(
            PreparedInputCount::UpperBound(997_000),
            1_000_000,
            1_000_000,
        )
        .with_occupied_tokens(Some(997_000));
        assert_eq!(unanchored.context_anchor(), None);
        assert_eq!(unanchored.occupied_tokens(), Some(997_000));
        assert_eq!(
            unanchored.actionable_utilization().unwrap().gauge_percent(),
            100
        );
        assert!(!occupancy_requires_scheduled_compact(unanchored));
        let mut triggered = unanchored;
        triggered.auto_compact_input = Some(800_000);
        assert!(!occupancy_requires_scheduled_compact(triggered));
        assert!(
            unanchored.observation_headroom_requires_compaction(),
            "the reserve reads the admission bound, not the measure"
        );
        let mut overflow = unanchored;
        overflow.disposition = ContextDisposition::WouldOverflow;
        assert!(occupancy_requires_scheduled_compact(overflow));
        assert!(occupancy_requires_scheduled_compact(
            unanchored.with_anchored_occupied_tokens(997_000)
        ));
    }

    /// The meter remains a percent; token triggers come from capacity policy.
    #[test]
    fn every_vertical_meters_the_working_envelope_and_fences_at_its_token_trigger() {
        for fixture in &VERTICALS {
            let ceiling = fixture.total_window.min(fixture.admitted_ceiling);
            let trigger = CapacityRule::ProviderReservedInput {
                reported_context_window: None,
                maximum_projected_input: ceiling,
                auto_compact_input: None,
            }
            .compaction_trigger_tokens(
                0,
                0,
                u64::from(plakous::model::SEMANTIC_COMPACTION_SUMMARY_TOKENS),
            );
            let below = projection(
                PreparedInputCount::UpperBound(trigger - 1),
                fixture.total_window,
                fixture.admitted_ceiling,
            );
            let at = projection(
                PreparedInputCount::UpperBound(trigger),
                fixture.total_window,
                fixture.admitted_ceiling,
            );
            let half = projection(
                PreparedInputCount::Exact(ceiling / 2),
                fixture.total_window,
                fixture.admitted_ceiling,
            );
            assert_eq!(
                half.actionable_utilization().unwrap().gauge_percent(),
                50,
                "{}",
                fixture.name
            );
            assert!(
                !occupancy_requires_scheduled_compact(below),
                "{}",
                fixture.name
            );
            assert!(occupancy_requires_scheduled_compact(at), "{}", fixture.name);
            assert_eq!(
                at.actionable_utilization().unwrap().ceiling_excess(),
                None,
                "{}",
                fixture.name
            );
        }
    }

    /// Luna: percent is over the 258.4k admitted envelope, not the 272k window.
    #[test]
    fn luna_percent_is_over_the_258k_envelope() {
        let projected = projection(PreparedInputCount::UpperBound(129_200), 272_000, 258_400);
        let utilization = projected.actionable_utilization().unwrap();
        assert_eq!(utilization.display_percent(), 50);
        assert_eq!(utilization.gauge_percent(), 50);
        assert_eq!(utilization.ceiling_excess(), None);
        assert_eq!(
            projected.total_window().map(TotalContextWindow::tokens),
            Some(272_000)
        );
        assert_eq!(
            projected
                .safe_admitted_input_ceiling()
                .map(SafeAdmittedInputCeiling::tokens),
            Some(258_400)
        );
        let exact = projection(PreparedInputCount::Exact(27_200), 272_000, 258_400);
        assert_eq!(
            exact.actionable_utilization().unwrap().display_percent(),
            11
        );
    }

    /// 2026-09-03 artifact: a turn-summed `784_251` count over luna's 272k
    /// window painted `288%/272k` while the branch itself fit at 25k.
    #[test]
    fn turn_summed_floor_reproduces_the_288_percent_artifact() {
        let projected = projection(PreparedInputCount::UpperBound(25_000), 272_000, 258_400);
        let bogus = projected
            .with_occupied_tokens(Some(784_251))
            .actionable_utilization()
            .unwrap();
        assert_eq!(bogus.display_percent, 304);
        assert_eq!(bogus.gauge_percent, 100);
        let truthful = projected
            .with_occupied_tokens(Some(25_000))
            .actionable_utilization()
            .unwrap();
        assert_eq!(truthful.display_percent, 10);
        assert_eq!(truthful.gauge_percent, 10);
        assert_eq!(truthful.ceiling_excess, None);
    }

    #[test]
    fn chrome_and_gauge_share_the_working_envelope_denominator() {
        let projected = projection(PreparedInputCount::Exact(80_000), 200_000, 160_000);

        assert_eq!(
            projected.actionable_utilization(),
            Some(ActionableUtilization {
                display_percent: 50,
                gauge_percent: 50,
                ceiling_excess: None,
            })
        );
        assert_eq!(
            projected.total_window().map(TotalContextWindow::tokens),
            Some(200_000)
        );
    }

    #[test]
    fn grok_shaped_envelope_schedules_at_400k_of_500k() {
        // SuperGrok: 500k admit, compact at 400k (80% of window). 382k is
        // under the trigger; 400k fires. Chrome is percent of the 500k window.
        let mut under = projection(PreparedInputCount::Exact(382_000), 500_000, 500_000);
        under.auto_compact_input = Some(400_000);
        let utilization = under.actionable_utilization().unwrap();
        assert_eq!(utilization.gauge_percent(), 76);
        assert_eq!(utilization.display_percent(), 76);
        assert_eq!(utilization.ceiling_excess(), None);
        assert!(!occupancy_requires_scheduled_compact(under));
        let mut at = projection(PreparedInputCount::Exact(400_000), 500_000, 500_000);
        at.auto_compact_input = Some(400_000);
        assert_eq!(at.actionable_utilization().unwrap().gauge_percent(), 80);
        assert!(occupancy_requires_scheduled_compact(at));
    }

    #[test]
    fn occupancy_advances_chrome_and_idle_gauge_without_changing_excess() {
        let projected = projection(PreparedInputCount::Exact(46_208), 128_000, 121_600);
        let envelope = projected.actionable_utilization().unwrap();
        assert_eq!(envelope.gauge_percent(), 38);
        assert_eq!(envelope.display_percent(), 38);
        assert_eq!(envelope.ceiling_excess(), None);
        assert!(!should_schedule_compaction(ContextDisposition::Fits));

        let occupancy = projected
            .with_occupied_tokens(Some(110_000))
            .actionable_utilization()
            .unwrap();
        assert_eq!(occupancy.display_percent(), 90);
        assert_eq!(occupancy.gauge_percent(), 90);
        assert_eq!(occupancy.ceiling_excess(), None);
        assert!(!should_schedule_compaction(ContextDisposition::Fits));
        assert!(!scheduled_compaction_parks_when_disabled(
            ContextDisposition::Fits
        ));
    }

    #[test]
    fn occupancy_below_the_bound_lowers_the_gauge_but_not_the_excess() {
        let projected = projection(PreparedInputCount::UpperBound(200_000), 200_000, 160_000)
            .with_occupied_tokens(Some(80_000));
        let utilization = projected.actionable_utilization().unwrap();
        assert_eq!(utilization.display_percent(), 50);
        assert_eq!(
            utilization.ceiling_excess(),
            Some(CeilingExcess::UpperBoundExcess(40_000))
        );
        assert_eq!(
            projected
                .with_occupied_tokens(None)
                .actionable_utilization(),
            None
        );
    }

    #[test]
    fn occupancy_does_not_invent_ceiling_excess() {
        let projected = projection(PreparedInputCount::Exact(80_000), 200_000, 160_000);
        let occupancy = projected
            .with_occupied_tokens(Some(200_000))
            .actionable_utilization()
            .unwrap();
        assert_eq!(occupancy.gauge_percent(), 100);
        assert_eq!(occupancy.display_percent(), 125);
        assert_eq!(occupancy.ceiling_excess(), None);
    }

    #[test]
    fn overflow_stays_visible_while_gauge_is_clamped() {
        let projected = projection(PreparedInputCount::UpperBound(200_000), 200_000, 160_000);
        let Some(utilization) = projected.actionable_utilization() else {
            panic!("nonzero safe ceiling must produce utilization");
        };

        assert_eq!(utilization.display_percent(), 125);
        assert_eq!(utilization.gauge_percent(), 100);
        assert_eq!(
            utilization.ceiling_excess(),
            Some(CeilingExcess::UpperBoundExcess(40_000))
        );
        assert!(matches!(
            projected.prepared_input(),
            Some(PreparedInputCount::UpperBound(_))
        ));
    }

    #[test]
    fn exact_overflow_is_distinct_from_conservative_bound_excess() {
        let projected = projection(PreparedInputCount::Exact(200_000), 200_000, 160_000);
        assert_eq!(
            projected
                .actionable_utilization()
                .and_then(ActionableUtilization::ceiling_excess),
            Some(CeilingExcess::ExactOverflow(40_000))
        );
    }

    #[test]
    fn shared_window_subtracts_enforced_output_reservation_but_keeps_the_total() {
        let Some((window, ceiling, _)) = capacity_projection(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100_000),
            },
            Some(20_000),
        ) else {
            panic!("complete shared-window facts must project");
        };

        assert_eq!(window.tokens(), 100_000);
        assert_eq!(window.basis(), TotalWindowBasis::ProviderReported);
        assert_eq!(ceiling.tokens(), 80_000);
        assert_eq!(ceiling.basis(), AdmittedCeilingBasis::ExactCapacity);
    }

    #[test]
    fn guaranteed_floor_is_not_presented_as_exact_capacity() {
        let Some((window, ceiling, _)) = capacity_projection(
            CapacityRule::Separate {
                input: SafeLimit::GuaranteedLowerBound(90_000),
                output: SafeLimit::Exact(10_000),
            },
            Some(5_000),
        ) else {
            panic!("complete separate-window facts must project");
        };

        assert_eq!(window.tokens(), 90_000);
        assert_eq!(window.basis(), TotalWindowBasis::InputCapacity);
        assert_eq!(ceiling.tokens(), 90_000);
        assert_eq!(
            ceiling.basis(),
            AdmittedCeilingBasis::GuaranteedCapacityFloor
        );
    }

    /// Reference envelopes carry the provider window beside the ceiling; a
    /// custom declaration without one states its own maximum as the window
    /// and says so.
    #[test]
    fn reserved_input_window_is_provider_reported_or_declared() {
        let Some((window, ceiling, _)) = capacity_projection(
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(272_000),
                maximum_projected_input: 258_400,
                auto_compact_input: None,
            },
            None,
        ) else {
            panic!("reference envelope must project");
        };
        assert_eq!(window.tokens(), 272_000);
        assert_eq!(window.basis(), TotalWindowBasis::ProviderReported);
        assert_eq!(ceiling.tokens(), 258_400);
        assert_eq!(ceiling.basis(), AdmittedCeilingBasis::OperatingEnvelope);

        let Some((declared, _, _)) = capacity_projection(
            CapacityRule::ProviderReservedInput {
                reported_context_window: None,
                maximum_projected_input: 64_000,
                auto_compact_input: None,
            },
            None,
        ) else {
            panic!("custom envelope must project");
        };
        assert_eq!(declared.tokens(), 64_000);
        assert_eq!(declared.basis(), TotalWindowBasis::DeclaredInputMaximum);
        assert_eq!(declared.basis().label(), "declared-input-maximum");
    }

    #[test]
    fn zero_ceiling_or_window_has_no_misleading_percentage() {
        assert_eq!(
            projection(PreparedInputCount::Exact(1), 100, 0).actionable_utilization(),
            None
        );
        assert_eq!(
            projection(PreparedInputCount::Exact(1), 0, 100).actionable_utilization(),
            None
        );
    }

    #[test]
    fn unavailable_status_has_no_actionable_percentage() {
        let mut projected = projection(PreparedInputCount::Exact(50), 100, 80);
        projected.disposition = ContextDisposition::Unavailable;

        assert_eq!(projected.actionable_utilization(), None);
    }

    #[test]
    fn observation_headroom_schedules_before_a_small_admitted_envelope_starves_tools() {
        let below = projection(PreparedInputCount::Exact(15_903), 1_000_000, 20_000);
        let at = projection(PreparedInputCount::Exact(15_904), 1_000_000, 20_000);

        // Chrome uses the 20k working envelope, so this is 80%, not 2% of the
        // 1M advertised window. The observation reserve acts without a token trigger.
        assert_eq!(at.actionable_utilization().unwrap().gauge_percent(), 80);
        assert!(!should_schedule_compaction(at.disposition()));
        assert!(!below.observation_headroom_requires_compaction());
        assert!(at.observation_headroom_requires_compaction());
        // The reserve guards admission: a low context measure does not hide
        // a bound that is about to reach the ceiling.
        assert!(
            at.with_occupied_tokens(Some(5_000))
                .observation_headroom_requires_compaction()
        );
    }

    #[test]
    fn compaction_without_a_token_trigger_schedules_only_on_typed_overflow() {
        assert!(should_schedule_compaction(
            ContextDisposition::WouldOverflow
        ));
        // Protected overflow schedules on every route now: the ladder's
        // placeholder rung can act where the semantic cut cannot.
        assert!(should_schedule_compaction(
            ContextDisposition::ProtectedOverflow
        ));
        assert!(should_schedule_compaction(
            ContextDisposition::ProtectedOverflow
        ));
        assert!(!should_schedule_compaction(ContextDisposition::Fits));
        assert!(should_schedule_compaction(
            ContextDisposition::ProtectedOverflow
        ));
        assert!(!should_schedule_compaction(ContextDisposition::Fits));
        assert!(!should_schedule_compaction(ContextDisposition::Fits));
        assert!(!should_schedule_compaction(ContextDisposition::Fits));
        assert_eq!(
            scheduled_compaction_disabled_note(ContextDisposition::WouldOverflow),
            "authoritative context would overflow; scheduled compaction is disabled"
        );
        assert_eq!(
            scheduled_compaction_disabled_note(ContextDisposition::Fits),
            "scheduled compaction is disabled at the context trigger"
        );
        assert!(!should_schedule_compaction(ContextDisposition::Unavailable));
        assert!(scheduled_compaction_parks_when_disabled(
            ContextDisposition::WouldOverflow
        ));
        assert!(scheduled_compaction_parks_when_disabled(
            ContextDisposition::ProtectedOverflow
        ));
        assert!(!scheduled_compaction_parks_when_disabled(
            ContextDisposition::Fits
        ));
        assert!(scheduled_compact_should_follow_on(
            0,
            ContextDisposition::WouldOverflow
        ));
        assert!(!scheduled_compact_should_follow_on(
            0,
            ContextDisposition::Fits
        ));
        assert!(!scheduled_compact_should_follow_on(
            SCHEDULED_COMPACT_FOLLOW_ON_LIMIT,
            ContextDisposition::WouldOverflow
        ));
        assert!(!scheduled_compact_should_follow_on(
            0,
            ContextDisposition::Fits
        ));
    }

    #[test]
    fn non_guarantee_wording_keeps_evidence_boundary_explicit() {
        assert!(
            NonGuarantee::ReferenceClientOperatingEnvelope
                .label()
                .contains("not a provider hard-cap guarantee")
        );
        assert!(
            NonGuarantee::CustomCallerOperatingEnvelope
                .label()
                .contains("not provider capacity evidence")
        );
    }

    #[test]
    fn evidence_and_non_guarantee_cannot_be_coerced() {
        assert_eq!(
            project_evidence(
                Some(&ContextEvidenceClass::ReleaseVerified),
                Some(ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope),
            ),
            None
        );
    }
}
