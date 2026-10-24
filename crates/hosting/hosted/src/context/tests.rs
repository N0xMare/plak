use super::test_verdicts::{disposition_schedules, idle_fence_schedules};
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
    assert!(!idle_fence_schedules(projected));
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
    assert!(!idle_fence_schedules(on_its_own_scale));
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
    assert!(!idle_fence_schedules(unanchored));
    let mut triggered = unanchored;
    triggered.auto_compact_input = Some(800_000);
    assert!(!idle_fence_schedules(triggered));
    assert!(
        unanchored.compaction_verdict().observation_headroom(),
        "the reserve reads the admission bound, not the measure"
    );
    let mut overflow = unanchored;
    overflow.disposition = ContextDisposition::WouldOverflow;
    assert!(idle_fence_schedules(overflow));
    assert!(idle_fence_schedules(
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
        assert!(!idle_fence_schedules(below), "{}", fixture.name);
        assert!(idle_fence_schedules(at), "{}", fixture.name);
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
    assert!(!idle_fence_schedules(under));
    let mut at = projection(PreparedInputCount::Exact(400_000), 500_000, 500_000);
    at.auto_compact_input = Some(400_000);
    assert_eq!(at.actionable_utilization().unwrap().gauge_percent(), 80);
    assert!(idle_fence_schedules(at));
}

#[test]
fn occupancy_advances_chrome_and_idle_gauge_without_changing_excess() {
    let projected = projection(PreparedInputCount::Exact(46_208), 128_000, 121_600);
    let envelope = projected.actionable_utilization().unwrap();
    assert_eq!(envelope.gauge_percent(), 38);
    assert_eq!(envelope.display_percent(), 38);
    assert_eq!(envelope.ceiling_excess(), None);
    assert!(!disposition_schedules(ContextDisposition::Fits));

    let occupancy = projected
        .with_occupied_tokens(Some(110_000))
        .actionable_utilization()
        .unwrap();
    assert_eq!(occupancy.display_percent(), 90);
    assert_eq!(occupancy.gauge_percent(), 90);
    assert_eq!(occupancy.ceiling_excess(), None);
    assert!(!disposition_schedules(ContextDisposition::Fits));
    assert!(!disposition_schedules(ContextDisposition::Fits));
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
    assert!(!disposition_schedules(at.disposition()));
    assert!(!below.compaction_verdict().observation_headroom());
    assert!(at.compaction_verdict().observation_headroom());
    // The reserve guards admission: a low context measure does not hide
    // a bound that is about to reach the ceiling.
    assert!(
        at.with_occupied_tokens(Some(5_000))
            .compaction_verdict()
            .observation_headroom()
    );
}

#[test]
fn compaction_without_a_token_trigger_schedules_only_on_typed_overflow() {
    assert!(disposition_schedules(ContextDisposition::WouldOverflow));
    // Protected overflow schedules on every route now: the ladder's
    // placeholder rung can act where the semantic cut cannot.
    assert!(disposition_schedules(ContextDisposition::ProtectedOverflow));
    assert!(disposition_schedules(ContextDisposition::ProtectedOverflow));
    assert!(!disposition_schedules(ContextDisposition::Fits));
    assert!(disposition_schedules(ContextDisposition::ProtectedOverflow));
    assert!(!disposition_schedules(ContextDisposition::Fits));
    assert!(!disposition_schedules(ContextDisposition::Fits));
    assert!(!disposition_schedules(ContextDisposition::Fits));
    assert_eq!(
        scheduled_compaction_disabled_note(ContextDisposition::WouldOverflow),
        "authoritative context would overflow; scheduled compaction is disabled"
    );
    assert_eq!(
        scheduled_compaction_disabled_note(ContextDisposition::Fits),
        "scheduled compaction is disabled at the context trigger"
    );
    assert!(!disposition_schedules(ContextDisposition::Unavailable));
    assert!(disposition_schedules(ContextDisposition::WouldOverflow));
    assert!(disposition_schedules(ContextDisposition::ProtectedOverflow));
    assert!(!disposition_schedules(ContextDisposition::Fits));
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
