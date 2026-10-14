#![allow(
    clippy::expect_used,
    reason = "unit tests use expect for fixture construction"
)]

use crate::runtime::compaction::fold::nested_observation_token_upper_bound;
use crate::runtime::compaction::request::{
    apply_completed_model_round_occupancy, compaction_fence_base_tokens, occupancy_gauge_percent,
    should_schedule_mid_turn_compaction_with_trigger, tool_group_fence_occupied_tokens,
    total_context_window_tokens,
};
use plakous_model::{
    Actor, CallId, CapacityRule, ContentBlock, ContextAnchor, ContextHeadroom, ContextOverflow,
    ContextStatusKind, ConversationItem, EntryId, Provenance, QualifiedTokens, TextBlock,
    ToolCorrelation, ToolExecutionOwner, ToolResultBlock, ToolResultKind, ToolResultPart,
};
use uuid::Uuid;

fn fixture_overflow() -> ContextOverflow {
    serde_json::from_value(serde_json::json!({
        "shared": {
            "required": 900_000,
            "available": 850_000
        }
    }))
    .expect("valid fixture overflow")
}

#[test]
fn compact_empty_diagnostic_is_not_unavailable() {
    let error = crate::AgentError::new(crate::AgentErrorKind::Internal, "");
    assert_eq!(
        super::request::compact_runtime_endpoint_error(&error),
        crate::AgentSessionEndpointError::OutcomeUnknown,
    );
}

#[test]
fn compact_unknown_error_with_diagnostic_stays_unknown() {
    let error = crate::AgentError::new(crate::AgentErrorKind::OutcomeUnknown, "reply lost");
    assert_eq!(
        super::request::compact_runtime_endpoint_error(&error),
        crate::AgentSessionEndpointError::OutcomeUnknown,
    );
}

#[test]
fn occupancy_gauge_percent_rounds_to_nearest_whole_percent() {
    assert_eq!(occupancy_gauge_percent(850_000, 1_000_000), Some(85));
    assert_eq!(occupancy_gauge_percent(849_999, 1_000_000), Some(85));
    assert_eq!(occupancy_gauge_percent(840_000, 1_000_000), Some(84));
    assert_eq!(occupancy_gauge_percent(0, 0), None);
}

#[test]
fn tool_group_fence_occupied_tokens_adds_nested_local_composition_observations() {
    // An anchor whose request estimate equals its provider count measures the
    // live estimate as is.
    let anchor = ContextAnchor::new(201_582, 201_582);
    assert_eq!(tool_group_fence_occupied_tokens(None, anchor, 0), None);
    assert_eq!(
        tool_group_fence_occupied_tokens(Some(201_582), anchor, 0),
        Some(201_582)
    );
    // Three 40 KiB cats ≈ 30_720 utf8/4; enough to cross 85% of luna's
    // 258_400 admitted envelope from the live 74% estimate.
    let occupied = tool_group_fence_occupied_tokens(Some(201_582), anchor, 30_720);
    assert_eq!(occupied, Some(232_302));
    assert!(occupancy_gauge_percent(201_582, 258_400).expect("gauge") < 85);
    assert!(occupancy_gauge_percent(232_302, 258_400).expect("gauge") >= 85);
    let fits = ContextStatusKind::Fits {
        headroom: ContextHeadroom::Shared(QualifiedTokens::Exact(10_000)),
    };
    assert!(!should_schedule_mid_turn_compaction_with_trigger(
        &fits,
        occupancy_gauge_percent(201_582, 258_400),
        None,
        None,
    ));
    assert!(should_schedule_mid_turn_compaction_with_trigger(
        &fits,
        occupancy_gauge_percent(232_302, 258_400),
        occupied,
        Some(232_000),
    ));
}

/// While the measure is unanchored (before a session's first completed
/// round, which includes a session carried forward from another build, and
/// after a compaction or branch transition) the whole-request
/// estimate is presentation only: it never schedules a compaction, however
/// high, while an overflow disposition still does.
#[test]
fn an_unanchored_measure_never_schedules_the_tool_group_fence() {
    let unanchored = tool_group_fence_occupied_tokens(Some(950_000), None, 30_720);
    assert_eq!(unanchored, None);
    let fits = ContextStatusKind::Fits {
        headroom: ContextHeadroom::Shared(QualifiedTokens::Exact(10_000)),
    };
    assert!(!should_schedule_mid_turn_compaction_with_trigger(
        &fits,
        unanchored.and_then(|occupied| occupancy_gauge_percent(occupied, 1_000_000)),
        unanchored,
        Some(800_000),
    ));
    let overflow = ContextStatusKind::WouldOverflow(fixture_overflow());
    assert!(should_schedule_mid_turn_compaction_with_trigger(
        &overflow, None, unanchored, None,
    ));
}

/// The telemetry case: the provider counted 298,468 input tokens for a
/// request whose `bytes÷4` estimate was 850,715. The fence measures that
/// count plus only what was appended since, not the whole estimate.
#[test]
fn tool_group_fence_is_anchored_on_the_last_provider_input() {
    let anchor = ContextAnchor::new(298_468, 850_715);
    // A 40 KiB tool result and 4 KiB of assistant text since the request.
    let estimate = 850_715 + 10_240 + 1_024;
    assert_eq!(
        tool_group_fence_occupied_tokens(Some(estimate), anchor, 0),
        Some(298_468 + 10_240 + 1_024)
    );
    assert_eq!(
        tool_group_fence_occupied_tokens(Some(estimate), anchor, 2_048),
        Some(298_468 + 10_240 + 1_024 + 2_048)
    );
    // The 400k trigger of a 500k window: the whole estimate would fire it,
    // the anchored measure does not.
    let trigger = Some(400_000);
    let fits = ContextStatusKind::Fits {
        headroom: ContextHeadroom::Shared(QualifiedTokens::Exact(10_000)),
    };
    let anchored = tool_group_fence_occupied_tokens(Some(estimate), anchor, 0);
    assert!(!should_schedule_mid_turn_compaction_with_trigger(
        &fits,
        anchored.and_then(|occupied| occupancy_gauge_percent(occupied, 500_000)),
        anchored,
        trigger,
    ));
    // The same estimate on the anchor's own scale would have fired it.
    let identity = ContextAnchor::new(850_715, 850_715);
    let whole = tool_group_fence_occupied_tokens(Some(estimate), identity, 0);
    assert!(should_schedule_mid_turn_compaction_with_trigger(
        &fits,
        whole.and_then(|occupied| occupancy_gauge_percent(occupied, 500_000)),
        whole,
        trigger,
    ));
    // Growth past the trigger on the anchored scale still fires it.
    let grown = tool_group_fence_occupied_tokens(Some(850_715 + 110_000), anchor, 0);
    assert!(should_schedule_mid_turn_compaction_with_trigger(
        &fits, None, grown, trigger,
    ));
}

/// The actionable meter uses the tighter of the reported window and the admitted
/// input ceiling. Grok's 500k window / 400k envelope is the case that
/// overflowed every round when the fence was the window.
#[test]
fn fence_base_is_the_tighter_of_window_and_admitted_ceiling() {
    use plakous_model::SafeLimit;
    assert_eq!(
        total_context_window_tokens(CapacityRule::ProviderReservedInput {
            reported_context_window: Some(272_000),
            maximum_projected_input: 258_400,
            auto_compact_input: None,
        }),
        Some(272_000)
    );
    assert_eq!(
        compaction_fence_base_tokens(CapacityRule::ProviderReservedInput {
            reported_context_window: Some(272_000),
            maximum_projected_input: 258_400,
            auto_compact_input: None,
        }),
        Some(258_400)
    );
    assert_eq!(
        compaction_fence_base_tokens(CapacityRule::ProviderReservedInput {
            reported_context_window: Some(500_000),
            maximum_projected_input: 500_000,
            auto_compact_input: Some(400_000),
        }),
        Some(500_000)
    );
    assert_eq!(
        compaction_fence_base_tokens(CapacityRule::ProviderReservedInput {
            reported_context_window: None,
            maximum_projected_input: 64_000,
            auto_compact_input: None,
        }),
        Some(64_000)
    );
    assert_eq!(
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(500_000),
            maximum_projected_input: 500_000,
            auto_compact_input: Some(400_000),
        }
        .compaction_trigger_tokens(0, 0, 8192),
        400_000
    );
    assert_eq!(
        compaction_fence_base_tokens(CapacityRule::SharedWindowWithOutputCap {
            total: SafeLimit::Exact(1_000_000),
            output: SafeLimit::Exact(128_000),
        }),
        Some(1_000_000)
    );
    assert_eq!(
        compaction_fence_base_tokens(CapacityRule::Separate {
            input: SafeLimit::GuaranteedLowerBound(90_000),
            output: SafeLimit::Exact(10_000),
        }),
        Some(90_000)
    );
    assert_eq!(occupancy_gauge_percent(219_640, 258_400), Some(85));
    assert_eq!(occupancy_gauge_percent(216_000, 258_400), Some(84));
    assert_eq!(occupancy_gauge_percent(340_000, 400_000), Some(85));
    assert_eq!(occupancy_gauge_percent(850_000, 1_000_000), Some(85));
}

#[test]
fn should_schedule_mid_turn_compaction_matches_spec_thresholds() {
    let overflow = fixture_overflow();
    assert!(should_schedule_mid_turn_compaction_with_trigger(
        &ContextStatusKind::WouldOverflow(overflow),
        None,
        None,
        None,
    ));
    assert!(should_schedule_mid_turn_compaction_with_trigger(
        &ContextStatusKind::ProtectedOverflow(overflow),
        None,
        None,
        None,
    ));
    // The ladder gained the placeholder rung, so protected overflow schedules on
    // a route with no native facet too; the rung, not the fence, decides what
    // can actually be done with it.
    assert!(should_schedule_mid_turn_compaction_with_trigger(
        &ContextStatusKind::ProtectedOverflow(overflow),
        None,
        None,
        None,
    ));
    let fits = ContextStatusKind::Fits {
        headroom: ContextHeadroom::Shared(QualifiedTokens::Exact(10_000)),
    };
    assert!(!should_schedule_mid_turn_compaction_with_trigger(
        &fits,
        Some(85),
        None,
        None,
    ));
    assert!(!should_schedule_mid_turn_compaction_with_trigger(
        &fits,
        Some(84),
        None,
        None,
    ));
    assert!(!should_schedule_mid_turn_compaction_with_trigger(
        &fits, None, None, None
    ));
    assert!(should_schedule_mid_turn_compaction_with_trigger(
        &fits,
        None,
        Some(400_000),
        Some(400_000),
    ));
    assert!(!should_schedule_mid_turn_compaction_with_trigger(
        &fits,
        Some(85),
        Some(399_000),
        Some(400_000),
    ));
}

#[test]
fn exhausted_admission_headroom_fences_even_below_anchored_occupancy() {
    for remaining in [0, 4_096, 4_097] {
        let fits = ContextStatusKind::Fits {
            headroom: ContextHeadroom::ProviderReservedInput {
                input: QualifiedTokens::GuaranteedMinimum(remaining),
            },
        };
        for occupied in [None, Some(346_950)] {
            assert_eq!(
                should_schedule_mid_turn_compaction_with_trigger(
                    &fits,
                    Some(35),
                    occupied,
                    Some(850_000),
                ),
                remaining <= 4_096,
            );
        }
    }
}

#[test]
fn nested_observation_token_upper_bound_counts_tool_result_text() {
    let call = CallId::try_from(Uuid::from_u128(7)).expect("call");
    let correlation = ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)
        .expect("correlation");
    let body = "x".repeat(40_960);
    let result = ToolResultBlock::new(
        call,
        ToolExecutionOwner::PlakLocal,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(
            TextBlock::new(body, None).expect("text"),
        )],
        correlation,
    )
    .expect("result");
    let item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(8)).expect("entry"),
        Actor::Tool,
        vec![ContentBlock::ToolResult(result)],
        Provenance::ToolExecution,
    )
    .expect("item");
    assert_eq!(nested_observation_token_upper_bound(&item), 10_240);
    assert_eq!(
        crate::runtime::compaction::fold::native_compaction_item_token_upper_bound(&item),
        1,
        "native compact retain meter ignores tool-result text; fence occupancy must not"
    );
}

#[test]
fn a_round_re_anchors_only_on_a_positive_provider_count_with_its_request_estimate() {
    let previous = ContextAnchor::new(163_173, 400_000);
    let mut anchor = previous;
    let mut nested = 20_480;
    apply_completed_model_round_occupancy(&mut anchor, &mut nested, Some(0), Some(410_000));
    assert_eq!(anchor, previous);
    assert_eq!(nested, 20_480);
    apply_completed_model_round_occupancy(&mut anchor, &mut nested, None, Some(410_000));
    assert_eq!(anchor, previous);
    assert_eq!(nested, 20_480);
    apply_completed_model_round_occupancy(&mut anchor, &mut nested, Some(170_000), None);
    assert_eq!(anchor, previous);
    assert_eq!(nested, 20_480);
    apply_completed_model_round_occupancy(&mut anchor, &mut nested, Some(170_000), Some(410_000));
    assert_eq!(anchor, ContextAnchor::new(170_000, 410_000));
    assert_eq!(nested, 0);
}

mod placeholders {
    use crate::SessionWorkflowId;
    use crate::runtime::compaction::placeholder::{
        PLACEHOLDER_HARNESS_NOTE_MARKER, PlaceholderCandidate, REPLACED_TOOL_RESULT_MARKER,
        collect_placeholder_candidates, placeholder_successor,
    };
    use plakous_model::{
        Actor, CallId, CanonicalHistory, CompactionBlock, ContentBlock, ConversationItem, EntryId,
        HistorySegment, HistorySegmentKind, Provenance, TextBlock, ToolCallBlock, ToolCallState,
        ToolCorrelation, ToolExecutionOwner, ToolName, ToolResultBlock, ToolResultKind,
        ToolResultPart,
    };
    use uuid::Uuid;

    fn workflow() -> SessionWorkflowId {
        SessionWorkflowId::new([7_u8; 32]).expect("workflow identity")
    }

    #[test]
    fn zero_result_placeholder_pass_requires_a_distinct_replay_projection() {
        let history = CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![user(980_001, "retain intent")],
            )
            .expect("segment"),
        ])
        .expect("history");
        assert!(placeholder_successor(&history, &[], 0, workflow()).is_err());
        assert!(
            crate::runtime::compaction::placeholder::replay_omission_successor(
                &history,
                &history,
                workflow(),
            )
            .is_err()
        );
    }

    /// Pins the projection that responses-dialect's
    /// `fresh_codex_body_matches_portable_reasoning_projection` reproduces:
    /// reasoning is dropped and an item left without blocks is elided.
    #[test]
    fn portable_history_drops_reasoning_and_elides_reasoning_only_items() {
        use plakous_model::{ReasoningBlock, ReasoningVisibility};
        let reasoning = || {
            ContentBlock::Reasoning(
                ReasoningBlock::new(
                    ReasoningVisibility::Visible,
                    Some("thinking".to_owned()),
                    None,
                )
                .expect("reasoning"),
            )
        };
        let assistant = |entry: u128, blocks: Vec<ContentBlock>| {
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(entry)).expect("entry"),
                Actor::Assistant,
                blocks,
                Provenance::ModelOutput,
            )
            .expect("assistant item")
        };
        let mut mixed_blocks = vec![
            reasoning(),
            ContentBlock::Text(TextBlock::new("answer", None).expect("text")),
        ];
        mixed_blocks.extend(
            call(990_003, 990_004, "exec_command", "{}")
                .blocks()
                .iter()
                .cloned(),
        );
        let history = CanonicalHistory::new(vec![
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(990_000, "go")])
                .expect("intent segment"),
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![
                    assistant(990_001, vec![reasoning()]),
                    assistant(990_002, mixed_blocks),
                    result(990_005, 990_004, "ok"),
                ],
            )
            .expect("exchange segment"),
        ])
        .expect("history");
        let portable = crate::runtime::compaction::request::portable_compaction_history(&history)
            .expect("portable history");
        let items = portable
            .segments()
            .iter()
            .flat_map(|segment| segment.items().iter())
            .collect::<Vec<_>>();
        assert_eq!(items.len(), 3, "the reasoning-only item is elided");
        assert!(matches!(
            items[1].blocks(),
            [ContentBlock::Text(_), ContentBlock::ToolCall(_)]
        ));
        assert!(items.iter().all(|item| {
            !item
                .blocks()
                .iter()
                .any(|block| matches!(block, ContentBlock::Reasoning(_)))
        }));
    }

    fn correlation() -> ToolCorrelation {
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)
            .expect("correlation")
    }

    fn user(entry: u128, text: &str) -> ConversationItem {
        ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(entry)).expect("entry"),
            Actor::User,
            vec![ContentBlock::Text(
                TextBlock::new(text, None).expect("text"),
            )],
            Provenance::UserInput,
        )
        .expect("user item")
    }

    fn call(entry: u128, call_id: u128, tool: &str, arguments: &str) -> ConversationItem {
        ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(entry)).expect("entry"),
            Actor::Assistant,
            vec![ContentBlock::ToolCall(
                ToolCallBlock::new(
                    CallId::try_from(Uuid::from_u128(call_id)).expect("call"),
                    ToolName::new(tool).expect("tool name"),
                    ToolExecutionOwner::PlakLocal,
                    ToolCallState::Completed,
                    correlation(),
                    arguments.to_owned(),
                )
                .expect("call block"),
            )],
            Provenance::ModelOutput,
        )
        .expect("call item")
    }

    fn result(entry: u128, call_id: u128, body: &str) -> ConversationItem {
        ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(entry)).expect("entry"),
            Actor::Tool,
            vec![ContentBlock::ToolResult(
                ToolResultBlock::new(
                    CallId::try_from(Uuid::from_u128(call_id)).expect("call"),
                    ToolExecutionOwner::PlakLocal,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(
                        TextBlock::new(body, None).expect("text"),
                    )],
                    correlation(),
                )
                .expect("result block"),
            )],
            Provenance::ToolExecution,
        )
        .expect("result item")
    }

    /// Every replaced placeholder body, in history order.
    fn placeholder_bodies(history: &CanonicalHistory) -> Vec<String> {
        let mut bodies = Vec::new();
        for segment in history.segments() {
            for item in segment.items() {
                for block in item.blocks() {
                    if let ContentBlock::ToolResult(outcome) = block {
                        for part in outcome.parts() {
                            if let ToolResultPart::Text(text) = part
                                && text.text().starts_with(REPLACED_TOOL_RESULT_MARKER)
                            {
                                bodies.push(text.text().to_owned());
                            }
                        }
                    }
                }
            }
        }
        bodies
    }

    /// Every harness note the history carries.
    fn harness_notes(history: &CanonicalHistory) -> Vec<String> {
        history
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::Steering)
            .flat_map(|segment| segment.items().iter())
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::Text(text)
                    if text.text().starts_with(PLACEHOLDER_HARNESS_NOTE_MARKER) =>
                {
                    Some(text.text().to_owned())
                }
                _ => None,
            })
            .collect()
    }

    fn compaction_segments(history: &CanonicalHistory) -> usize {
        history
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
            .count()
    }

    /// The text of the history's sole compaction segment.
    fn notice_text(history: &CanonicalHistory) -> String {
        history
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
            .flat_map(|segment| segment.items().iter())
            .flat_map(ConversationItem::blocks)
            .find_map(|block| match block {
                ContentBlock::Compaction(compaction) => Some(compaction.summary().to_owned()),
                _ => None,
            })
            .expect("a compaction segment with summary text")
    }

    /// Three tool exchanges under one user intent.
    ///
    /// The newest is the current round's batch and is never replaceable, so the
    /// two older ones are the candidates.
    fn fixture() -> CanonicalHistory {
        CanonicalHistory::new(vec![
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(1, "read them")])
                .expect("intent"),
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![
                    call(
                        2,
                        20,
                        "read_file",
                        r#"{"path":"a.txt","offset":0,"limit":200}"#,
                    ),
                    result(3, 20, &"a".repeat(4_000)),
                ],
            )
            .expect("first exchange"),
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![
                    call(4, 21, "exec_command", r#"{"command":"wc -l b.txt"}"#),
                    result(5, 21, &"b".repeat(6_000)),
                ],
            )
            .expect("second exchange"),
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![
                    call(6, 22, "read_file", r#"{"path":"c.txt"}"#),
                    result(7, 22, &"c".repeat(1_000)),
                ],
            )
            .expect("current round batch"),
        ])
        .expect("history")
    }

    fn reduction_cut() -> plakous_model::SemanticCompactionCut {
        use plakous_model::{
            ContextHistoryPlan, ConversationView, ReasoningBlock, ReasoningVisibility,
            SemanticCompactionCut, WorkingProjection,
        };
        let summary = HistorySegment::new(
            HistorySegmentKind::Compaction,
            vec![
                ConversationItem::new(
                    EntryId::try_from(Uuid::from_u128(99)).expect("entry"),
                    Actor::Assistant,
                    vec![ContentBlock::Compaction(
                        CompactionBlock::new("prior summary").expect("summary"),
                    )],
                    Provenance::ModelOutput,
                )
                .expect("summary item"),
            ],
        )
        .expect("summary segment");
        let prose = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![
                ConversationItem::new(
                    EntryId::try_from(Uuid::from_u128(100)).expect("entry"),
                    Actor::Assistant,
                    vec![
                        ContentBlock::Text(TextBlock::new("assistant text", None).expect("text")),
                        ContentBlock::Reasoning(
                            ReasoningBlock::new(
                                ReasoningVisibility::Visible,
                                Some("reasoning text".to_owned()),
                                None,
                            )
                            .expect("reasoning"),
                        ),
                    ],
                    Provenance::ModelOutput,
                )
                .expect("prose item"),
            ],
        )
        .expect("prose segment");
        let history = fixture();
        let mut segments = vec![summary];
        segments.extend(history.segments().iter().take(3).cloned());
        segments.push(prose);
        segments.push(history.segments()[3].clone());
        let source = ConversationView::from_shared(
            CanonicalHistory::new(segments).expect("source"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        SemanticCompactionCut::seal(&source, &plan).expect("cut")
    }

    #[test]
    fn summary_reductions_preserve_r0_and_everything_but_selected_bodies() {
        use crate::runtime::compaction::request::{
            harness_compaction_input, harness_compaction_input_at_depth,
            harness_compaction_reduction_count, validate_harness_compaction_input,
        };
        let cut = reduction_cut();
        let original = harness_compaction_input(&cut, workflow()).expect("R0");
        let zero = harness_compaction_input_at_depth(&cut, workflow(), 0).expect("depth zero");
        assert_eq!(
            serde_json::to_vec(&zero).expect("bytes"),
            serde_json::to_vec(&original).expect("original bytes")
        );
        assert_eq!(harness_compaction_reduction_count(&cut), 2);
        let retained = cut.retained_segments().to_vec();
        for depth in 0..=2 {
            let input = harness_compaction_input_at_depth(&cut, workflow(), depth).expect("input");
            assert_eq!(
                validate_harness_compaction_input(&cut, workflow(), &input).expect("validate"),
                Some(depth)
            );
            assert_eq!(placeholder_bodies(input.history()).len(), depth);
            assert_eq!(
                input.history().segments().len(),
                original.history().segments().len()
            );
            assert_eq!(
                input.history().segments()[0],
                original.history().segments()[0],
                "prior summary"
            );
            assert_eq!(
                input.history().segments()[1],
                original.history().segments()[1],
                "copied latest intent"
            );
            // Prior summary, intent, two exchanges, assistant prose, instruction.
            assert_eq!(
                input
                    .history()
                    .segments()
                    .iter()
                    .skip(4)
                    .collect::<Vec<_>>(),
                original
                    .history()
                    .segments()
                    .iter()
                    .skip(4)
                    .collect::<Vec<_>>()
            );
            for index in 2..=3 {
                let before = &original.history().segments()[index];
                let after = &input.history().segments()[index];
                assert_eq!(before.items()[0], after.items()[0], "call metadata");
                let ContentBlock::ToolResult(old) = &before.items()[1].blocks()[0] else {
                    panic!("result")
                };
                let ContentBlock::ToolResult(new) = &after.items()[1].blocks()[0] else {
                    panic!("result")
                };
                assert_eq!(old.call_id(), new.call_id());
                assert_eq!(old.owner(), new.owner());
                assert_eq!(old.kind(), new.kind());
                assert_eq!(old.correlation(), new.correlation());
                if index - 2 >= depth {
                    assert_eq!(before, after);
                }
            }
        }
        assert_eq!(
            cut.retained_segments(),
            retained,
            "fresh exchange and retained intent"
        );
        assert!(harness_compaction_input_at_depth(&cut, workflow(), 3).is_err());
        let first = harness_compaction_input_at_depth(&cut, workflow(), 1).expect("R1");
        assert!(placeholder_bodies(first.history())[0].contains("tool: read_file"));
        assert!(harness_notes(first.history()).is_empty());
    }

    #[test]
    fn summary_reduction_validation_rejects_non_prefix_and_tampered_inputs() {
        use crate::runtime::compaction::placeholder::{
            collect_compaction_reduction_candidates, replace_compaction_result_body,
        };
        use crate::runtime::compaction::request::{
            harness_compaction_input, harness_compaction_input_at_depth,
            validate_harness_compaction_input,
        };
        use plakous_model::{ConversationView, WorkingProjection};
        let cut = reduction_cut();
        let zero = harness_compaction_input(&cut, workflow()).expect("R0");
        let candidates = collect_compaction_reduction_candidates(&cut);
        let subset = ConversationView::from_shared(
            replace_compaction_result_body(zero.history(), &candidates[1]).expect("second only"),
            WorkingProjection::default(),
        );
        assert_eq!(
            validate_harness_compaction_input(&cut, workflow(), &subset).expect("validate"),
            None
        );
        let one = harness_compaction_input_at_depth(&cut, workflow(), 1).expect("R1");
        for (from, to) in [
            ("removed_bytes: 4000", "removed_bytes: 3999"),
            (REPLACED_TOOL_RESULT_MARKER, "[plak: forged marker]"),
            ("prior summary", "tampered prior summary"),
            ("read them", "different user intent"),
            ("assistant text", "tampered assistant text"),
            ("reasoning text", "tampered reasoning"),
        ] {
            // Rebuild identities so rejection is by the closed candidate
            // validator, not by the history codec's integrity check.
            let mut changed = false;
            let segments = one
                .history()
                .segments()
                .iter()
                .map(|segment| {
                    let items = segment
                        .items()
                        .iter()
                        .map(|item| {
                            let blocks = item
                                .blocks()
                                .iter()
                                .map(|block| {
                                    let encoded =
                                        serde_json::to_string(block).expect("block bytes");
                                    changed |= encoded.contains(from);
                                    serde_json::from_str(&encoded.replace(from, to)).expect("block")
                                })
                                .collect();
                            ConversationItem::new(
                                item.id(),
                                item.actor(),
                                blocks,
                                item.provenance(),
                            )
                            .expect("item")
                        })
                        .collect();
                    HistorySegment::new(segment.kind(), items).expect("segment")
                })
                .collect();
            assert!(changed);
            let tampered = ConversationView::from_shared(
                CanonicalHistory::new(segments).expect("history"),
                WorkingProjection::default(),
            );
            assert_eq!(
                validate_harness_compaction_input(&cut, workflow(), &tampered).expect("validate"),
                None,
                "{from}"
            );
        }
        let mut segments = one.history().segments().to_vec();
        segments.swap(2, 3);
        let reordered = ConversationView::from_shared(
            CanonicalHistory::new(segments).expect("reordered"),
            WorkingProjection::default(),
        );
        assert_eq!(
            validate_harness_compaction_input(&cut, workflow(), &reordered).expect("validate"),
            None
        );
        let mut segments = one.history().segments().to_vec();
        let _ = segments.pop();
        segments.push(
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![user(900, "forged instruction")],
            )
            .expect("instruction"),
        );
        let instruction = ConversationView::from_shared(
            CanonicalHistory::new(segments).expect("instruction history"),
            WorkingProjection::default(),
        );
        assert_eq!(
            validate_harness_compaction_input(&cut, workflow(), &instruction).expect("validate"),
            None
        );
        assert_eq!(
            validate_harness_compaction_input(
                &cut,
                SessionWorkflowId::new([8; 32]).expect("workflow"),
                &one
            )
            .expect("validate"),
            None
        );
    }

    #[test]
    fn summary_reductions_include_older_turns_and_skip_existing_placeholders() {
        use crate::runtime::compaction::placeholder::{
            collect_compaction_reduction_candidates, replace_compaction_result_body,
        };
        use crate::runtime::compaction::request::{
            harness_compaction_input_at_depth, harness_compaction_reduction_count,
            validate_harness_compaction_input,
        };
        use plakous_model::{
            ContextHistoryPlan, ConversationView, SemanticCompactionCut, WorkingProjection,
        };
        let mut segments = fixture().segments().to_vec();
        segments.push(
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![user(101, "next turn")],
            )
            .expect("latest intent"),
        );
        segments.push(
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![call(102, 103, "probe", "{}"), result(104, 103, "fresh")],
            )
            .expect("fresh"),
        );
        let history = CanonicalHistory::new(segments).expect("history");
        assert!(
            collect_placeholder_candidates(&history).is_empty(),
            "current-turn rung has no eligible bodies"
        );
        let source = ConversationView::from_shared(history.clone(), WorkingProjection::default());
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        let cut = SemanticCompactionCut::seal(&source, &plan).expect("cut");
        assert_eq!(harness_compaction_reduction_count(&cut), 3);
        let reduced = harness_compaction_input_at_depth(&cut, workflow(), 3).expect("R3");
        assert_eq!(placeholder_bodies(reduced.history()).len(), 3);
        assert_eq!(cut.retained_segments().len(), 2);
        assert_eq!(cut.retained_segments()[1], history.segments()[5]);
        // The cut's summary positions are the original source positions in
        // this older-turn-only shape, so rewrite the first source result.
        let candidates = collect_compaction_reduction_candidates(&cut);
        let history = replace_compaction_result_body(&history, &candidates[0])
            .expect("already replaced source");
        let source = ConversationView::from_shared(history, WorkingProjection::default());
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        let cut = SemanticCompactionCut::seal(&source, &plan).expect("cut");
        assert_eq!(harness_compaction_reduction_count(&cut), 2);
        let reduced = harness_compaction_input_at_depth(&cut, workflow(), 2).expect("R2");
        assert_eq!(placeholder_bodies(reduced.history()).len(), 3);
        assert_eq!(
            validate_harness_compaction_input(&cut, workflow(), &reduced).expect("validate"),
            Some(2)
        );
    }

    #[test]
    fn candidates_are_oldest_first_and_carry_their_tool_name_and_size() {
        let history = fixture();
        let candidates = collect_placeholder_candidates(&history);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].segment_index(), 1);
        assert_eq!(candidates[1].segment_index(), 2);
        let successor =
            placeholder_successor(&history, &candidates, 1, workflow()).expect("successor");
        assert_eq!(successor.replaced_results, 1);
        assert_eq!(successor.replaced_bytes, 4_000);
    }

    /// At the idle fence the turn's final reply has already answered its
    /// newest batch, so that batch is consumed and replaceable like any older
    /// one. Without this, a session whose last turn was one oversized batch
    /// had nothing to replace before the next prompt.
    #[test]
    fn an_answered_newest_batch_is_a_candidate_but_an_unanswered_one_is_not() {
        let unanswered = fixture();
        let mut segments = unanswered.segments().to_vec();
        segments.push(
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![
                    ConversationItem::new(
                        EntryId::try_from(Uuid::from_u128(8)).expect("entry"),
                        Actor::Assistant,
                        vec![ContentBlock::Text(
                            TextBlock::new("the final answer", None).expect("text"),
                        )],
                        Provenance::ModelOutput,
                    )
                    .expect("answer item"),
                ],
            )
            .expect("final answer"),
        );
        let answered = CanonicalHistory::new(segments).expect("answered history");
        let indexes = |history: &CanonicalHistory| {
            collect_placeholder_candidates(history)
                .iter()
                .map(PlaceholderCandidate::segment_index)
                .collect::<Vec<_>>()
        };
        assert_eq!(indexes(&unanswered), vec![1, 2]);
        assert_eq!(indexes(&answered), vec![1, 2, 3]);
        // A harness note after the batch is not a model round: still unanswered.
        let noted = placeholder_successor(
            &unanswered,
            &collect_placeholder_candidates(&unanswered),
            2,
            workflow(),
        )
        .expect("successor")
        .history;
        assert!(collect_placeholder_candidates(&noted).is_empty());
    }

    #[test]
    fn a_small_result_plus_notices_can_expand_the_successor() {
        let history = CanonicalHistory::new(vec![
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(40, "read")])
                .expect("intent"),
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![call(41, 41, "probe", "{}"), result(42, 41, "ok")],
            )
            .expect("old exchange"),
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![call(43, 43, "probe", "{}"), result(44, 43, "fresh")],
            )
            .expect("fresh exchange"),
        ])
        .expect("history");
        let successor = placeholder_successor(
            &history,
            &collect_placeholder_candidates(&history),
            1,
            workflow(),
        )
        .expect("successor");
        assert!(
            successor.history.retained_bytes_upper_bound() > history.retained_bytes_upper_bound(),
            "the placeholder and notices cost more than the two-byte result"
        );
    }

    #[test]
    fn placeholders_keeps_every_call_and_result_paired_and_only_replaces_the_body() {
        let history = fixture();
        let candidates = collect_placeholder_candidates(&history);
        let successor =
            placeholder_successor(&history, &candidates, 1, workflow()).expect("successor");
        // Source segments are preserved in order behind the sole compaction
        // segment, which a placeholder pass creates at position zero. Appending it
        // instead would give the history two compaction segments as soon as a
        // second pass ran, and the planner refuses that outright.
        // Four source segments, the notice at position zero, and the
        // harness note at the tail.
        assert_eq!(successor.history.segments().len(), 6);
        assert_eq!(
            successor.history.segments().get(0).expect("notice").kind(),
            HistorySegmentKind::Compaction
        );
        assert_eq!(compaction_segments(&successor.history), 1);
        let first = successor.history.segments().get(2).expect("first exchange");
        assert_eq!(first.items().len(), 2, "the call must survive its result");
        let ContentBlock::ToolResult(replaced) = first
            .items()
            .get(1)
            .expect("result item")
            .blocks()
            .first()
            .expect("result block")
        else {
            panic!("expected a tool result");
        };
        let [ToolResultPart::Text(text)] = replaced.parts() else {
            panic!("expected one placeholder part");
        };
        assert!(text.text().starts_with(REPLACED_TOOL_RESULT_MARKER));
        assert!(text.text().contains("tool: read_file"));
        assert!(text.text().contains("removed_bytes: 4000"));
        assert!(text.text().len() < 4_000);
        // The newer exchange is untouched at depth one.
        let second = successor
            .history
            .segments()
            .get(3)
            .expect("second exchange");
        let ContentBlock::ToolResult(kept) = second
            .items()
            .get(1)
            .expect("result item")
            .blocks()
            .first()
            .expect("result block")
        else {
            panic!("expected a tool result");
        };
        let [ToolResultPart::Text(text)] = kept.parts() else {
            panic!("expected the original part");
        };
        assert_eq!(text.text().len(), 6_000);
    }

    #[test]
    fn a_second_pass_skips_bodies_already_replaced() {
        let history = fixture();
        let candidates = collect_placeholder_candidates(&history);
        let first = placeholder_successor(&history, &candidates, 1, workflow()).expect("successor");
        let next = collect_placeholder_candidates(&first.history);
        assert_eq!(
            next.len(),
            1,
            "an already-replaced body is not a candidate again"
        );
        // Position 3 in the successor: the leading notice shifted the source
        // segments by one.
        assert_eq!(next[0].segment_index(), 3);
    }

    /// The placeholder names the call in the model's own vocabulary.
    ///
    /// Live growth runs are the reason this is pinned. A
    /// model whose reads had been replaced wrote one invented line count for
    /// every file rather than re-reading. A placeholder saying only that
    /// "something was removed" cannot be acted on; the tool name plus the
    /// arguments as the model sent them can be reissued verbatim.
    #[test]
    fn the_placeholder_names_the_tool_and_the_arguments_the_model_sent() {
        let history = fixture();
        let candidates = collect_placeholder_candidates(&history);
        let successor =
            placeholder_successor(&history, &candidates, 2, workflow()).expect("successor");
        let bodies = placeholder_bodies(&successor.history);
        assert_eq!(bodies.len(), 2);

        // A read_file shape: the path, offset and limit come back verbatim.
        let read = &bodies[0];
        assert!(read.starts_with(REPLACED_TOOL_RESULT_MARKER));
        assert!(
            read.contains("This is not the tool's output."),
            "a placeholder must not read as a tool that returned nothing: {read}"
        );
        assert!(read.contains("tool: read_file"));
        assert!(read.contains(r#"arguments: {"path":"a.txt","offset":0,"limit":200}"#));
        assert!(read.contains("removed_bytes: 4000"));
        assert!(read.contains("call read_file again with the same arguments"));
        assert!(read.contains("Do not guess"));

        // An exec_command shape: the command line itself.
        let exec = &bodies[1];
        assert!(exec.contains("tool: exec_command"));
        assert!(exec.contains(r#"arguments: {"command":"wc -l b.txt"}"#));
        assert!(exec.contains("removed_bytes: 6000"));
        assert!(exec.contains("call exec_command again with the same arguments"));
    }

    /// Arguments are bounded so a placeholder never costs what it saves.
    #[test]
    fn placeholder_arguments_are_bounded() {
        let long = "x".repeat(4_000);
        let history = CanonicalHistory::new(vec![
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(1, "go")])
                .expect("intent"),
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![
                    call(2, 30, "exec_command", &format!(r#"{{"command":"{long}"}}"#)),
                    result(3, 30, &"a".repeat(9_000)),
                ],
            )
            .expect("exchange"),
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![call(4, 31, "read_file", "{}"), result(5, 31, "tiny")],
            )
            .expect("current batch"),
        ])
        .expect("history");
        let candidates = collect_placeholder_candidates(&history);
        let successor =
            placeholder_successor(&history, &candidates, 1, workflow()).expect("successor");
        let body = &placeholder_bodies(&successor.history)[0];
        assert!(body.contains('…'), "over-long arguments are truncated");
        assert!(
            body.len() < 1_200,
            "the placeholder stays bounded: {}",
            body.len()
        );
    }

    /// One harness note per pass, naming what went, at the tail.
    #[test]
    fn each_pass_leaves_exactly_one_harness_note_naming_what_it_removed() {
        let history = fixture();
        let first = placeholder_successor(
            &history,
            &collect_placeholder_candidates(&history),
            1,
            workflow(),
        )
        .expect("first pass");
        let notes = harness_notes(&first.history);
        assert_eq!(notes.len(), 1, "exactly one note per pass");
        assert!(notes[0].contains("read_file"));
        assert!(notes[0].contains(r#"{"path":"a.txt","offset":0,"limit":200}"#));
        assert!(
            notes[0].contains("write down in your own words"),
            "the note must ask the model to carry forward what it still needs"
        );
        // The note is the last segment, where the model is working, and it is
        // steering rather than user intent so it does not move the turn
        // boundary or make the current turn's own work foldable.
        let segments = first.history.segments();
        let last = segments
            .get(segments.len().saturating_sub(1))
            .expect("a tail segment");
        assert_eq!(last.kind(), HistorySegmentKind::Steering);

        // A second pass supersedes the first note rather than stacking.
        let second = placeholder_successor(
            &first.history,
            &collect_placeholder_candidates(&first.history),
            1,
            SessionWorkflowId::new([9_u8; 32]).expect("workflow"),
        )
        .expect("second pass");
        let notes = harness_notes(&second.history);
        assert_eq!(notes.len(), 1, "the stale note is replaced, not stacked");
        assert!(
            notes[0].contains("exec_command"),
            "the note names this pass's loss, not the previous one"
        );
    }

    /// A second pass merges into the first pass's notice.
    ///
    /// Two compaction segments make `analyze_context_history` return
    /// `NoncanonicalCompaction`, which makes the whole context status
    /// `Unavailable` before the fit rule is evaluated. Stacking notices would
    /// leave the rung able to act exactly once per session and would block
    /// every later compaction too.
    #[test]
    fn a_second_pass_keeps_exactly_one_compaction_segment() {
        let history = fixture();
        let first = placeholder_successor(
            &history,
            &collect_placeholder_candidates(&history),
            1,
            workflow(),
        )
        .expect("first pass");
        let second = placeholder_successor(
            &first.history,
            &collect_placeholder_candidates(&first.history),
            1,
            SessionWorkflowId::new([9_u8; 32]).expect("second workflow"),
        )
        .expect("second pass");
        assert_eq!(
            compaction_segments(&second.history),
            1,
            "the second pass must merge, not stack"
        );
        assert_eq!(
            second.history.segments().len(),
            first.history.segments().len(),
            "merging adds no segment"
        );
        // Both older bodies are placeholders now. The only live result left is
        // the batch the current round received, which is never offered: the
        // model has not consumed it yet, and replacing it would cause rereads.
        let remaining = collect_placeholder_candidates(&second.history);
        assert!(
            remaining.is_empty(),
            "a repeated pass must retain the fresh newest exchange"
        );
        // The notice stays one line however many passes run; the per-pass
        // counts live in provenance and the `context.placeholders` records.
        assert_eq!(notice_text(&second.history).lines().count(), 1);
    }

    /// A placeholder pass refuses to merge into a native compaction item.
    ///
    /// Merging builds a fresh `CompactionBlock`, which carries no
    /// `native_item`, so a merge would silently drop ciphertext the Codex
    /// family contract requires to be replayed byte-exactly. Prepending a
    /// second compaction segment instead would make the history unplannable,
    /// so failing closed is the only correct answer. Unreachable today: a
    /// route with the facet takes the facet branch before this rung.
    #[test]
    fn a_placeholder_pass_refuses_to_merge_into_a_native_compaction_item() {
        let native = HistorySegment::new(
            HistorySegmentKind::Compaction,
            vec![
                ConversationItem::new(
                    EntryId::try_from(Uuid::from_u128(98)).expect("entry"),
                    Actor::Assistant,
                    vec![ContentBlock::Compaction(
                        CompactionBlock::new("opaque-ciphertext")
                            .expect("summary")
                            .with_native_item(
                                r#"{"type":"compaction","encrypted_content":"opaque-ciphertext"}"#
                                    .to_owned(),
                            ),
                    )],
                    Provenance::ModelOutput,
                )
                .expect("native item"),
            ],
        )
        .expect("native segment");
        let history = CanonicalHistory::new(
            std::iter::once(native)
                .chain(fixture().segments().iter().cloned())
                .collect(),
        )
        .expect("history with a native summary");
        let candidates = collect_placeholder_candidates(&history);
        assert!(!candidates.is_empty(), "there is a body to replace");
        let Err(error) = placeholder_successor(&history, &candidates, 1, workflow()) else {
            panic!("merging into native replay material must fail closed");
        };
        assert!(
            error.message().contains("native compaction item"),
            "unexpected diagnostic: {}",
            error.message()
        );
    }

    /// The harness note is marked as harness-authored, not as typed input.
    ///
    /// It must stay `Actor::User` in a `Steering` segment: that is the only
    /// shape that lowers as a user-role message on every protocol while the
    /// history planner still ignores it for intent. `Actor::Harness` and
    /// `Actor::Developer` are refused in history by the Messages protocol. So
    /// provenance is the one field left that can tell the note apart from
    /// something the operator typed, and surfaces label it from that.
    #[test]
    fn the_harness_note_is_instruction_provenance_in_a_steering_segment() {
        let history = fixture();
        let successor = placeholder_successor(
            &history,
            &collect_placeholder_candidates(&history),
            1,
            workflow(),
        )
        .expect("successor");
        let segments = successor.history.segments();
        let note = segments
            .get(segments.len().saturating_sub(1))
            .expect("a tail segment");
        assert_eq!(note.kind(), HistorySegmentKind::Steering);
        let item = note.items().first().expect("a note item");
        assert_eq!(
            item.actor(),
            Actor::User,
            "steering must lower as a user-role message on every protocol"
        );
        assert_eq!(
            item.provenance(),
            Provenance::Instruction,
            "the operator did not type this"
        );
        // Nothing else the placeholder pass publishes claims to be typed input either.
        assert!(
            successor
                .history
                .segments()
                .iter()
                .flat_map(|segment| segment.items().iter())
                .filter(|candidate| candidate.id() == item.id())
                .count()
                == 1,
            "exactly one note"
        );
    }

    /// The note does not become the turn's intent, for the planner or for us.
    ///
    /// `is_semantic_user_intent` ignores `Steering`, and the placeholder rung
    /// mirrors that predicate. If either counted the note, the next pass would
    /// find nothing to replace and the rung would work once per turn.
    #[test]
    fn the_harness_note_does_not_move_the_turn_boundary() {
        let history = fixture();
        let before = collect_placeholder_candidates(&history).len();
        let successor = placeholder_successor(
            &history,
            &collect_placeholder_candidates(&history),
            1,
            workflow(),
        )
        .expect("successor");
        let after = collect_placeholder_candidates(&successor.history).len();
        assert_eq!(
            after,
            before.saturating_sub(1),
            "the note must not hide the remaining candidates behind a new turn boundary"
        );
    }

    #[test]
    fn placeholder_notice_is_constant_across_pass_depths() {
        let history = fixture();
        let candidates = collect_placeholder_candidates(&history);
        let shallow =
            placeholder_successor(&history, &candidates, 1, workflow()).expect("shallow successor");
        let deep =
            placeholder_successor(&history, &candidates, 2, workflow()).expect("deep successor");
        assert_ne!(shallow.replaced_results, deep.replaced_results);
        assert_ne!(shallow.replaced_bytes, deep.replaced_bytes);
        assert_eq!(notice_text(&shallow.history), notice_text(&deep.history));
    }

    /// A placeholder pass after a fold merges into the fold's summary and keeps it.
    #[test]
    fn a_placeholder_pass_after_a_fold_merges_into_the_existing_summary() {
        let summary = HistorySegment::new(
            HistorySegmentKind::Compaction,
            vec![
                ConversationItem::new(
                    EntryId::try_from(Uuid::from_u128(99)).expect("entry"),
                    Actor::Assistant,
                    vec![ContentBlock::Compaction(
                        CompactionBlock::new("earlier turns, summarised").expect("summary"),
                    )],
                    Provenance::ModelOutput,
                )
                .expect("summary item"),
            ],
        )
        .expect("summary segment");
        let folded = CanonicalHistory::new(
            std::iter::once(summary)
                .chain(fixture().segments().iter().cloned())
                .collect(),
        )
        .expect("folded history");
        let candidates = collect_placeholder_candidates(&folded);
        let successor =
            placeholder_successor(&folded, &candidates, 1, workflow()).expect("successor");
        assert_eq!(compaction_segments(&successor.history), 1);
        assert_eq!(
            successor.history.segments().len(),
            folded.segments().len().saturating_add(1),
            "merging into the fold summary adds only the harness note"
        );
        let notice = notice_text(&successor.history);
        assert!(
            notice.contains("earlier turns, summarised"),
            "the paid summary must survive verbatim"
        );
        assert!(notice.contains("Tool result placeholders:"));
    }

    #[test]
    fn selection_evidence_spans_exactly_the_touched_segments() {
        let history = fixture();
        let candidates = collect_placeholder_candidates(&history);
        let successor =
            placeholder_successor(&history, &candidates, 2, workflow()).expect("successor");
        assert_eq!(successor.selected_start, 1);
        assert_eq!(successor.selected_end, 3);
        assert_eq!(successor.replaced_results, 2);
        assert_eq!(successor.replaced_bytes, 10_000);
    }
}
