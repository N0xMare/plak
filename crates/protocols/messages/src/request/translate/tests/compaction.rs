use super::*;
use plakous_model::{
    CanonicalHistory, CompactionBlock, ContextHistoryPlan, HistorySegment, HistorySegmentKind,
    ReasoningBlock, SemanticCompactionCut, WorkingProjection,
};

fn text_item(id: u128, actor: Actor, value: &str) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        TestEntryId::try_from(TestUuid::from_u128(id))?,
        actor,
        vec![ContentBlock::Text(TestTextBlock::new(value, None)?)],
        Provenance::UserInput,
    )?)
}

#[test]
fn unchanged_tool_loop_keeps_complete_thinking_and_exact_correlations() -> TestResult {
    let policy = policy()?.with_strict_thinking_replay(true);
    let thinking = ContentBlock::Reasoning(ReasoningBlock::new(
        ReasoningVisibility::Omitted,
        None,
        Some(bind_replay_payload(
            &policy,
            ModelId::new("claude-exact")?,
            SIGNATURE_REPLAY_KIND,
            raw("\"synthetic-tool-signature\"")?,
        )?),
    )?);
    let source = ConversationView::from_items(vec![
        batch_item(
            1,
            Actor::User,
            vec![ContentBlock::Text(TestTextBlock::new("task", None)?)],
        )?,
        batch_item(2, Actor::Assistant, vec![thinking, batch_call(10)?])?,
        batch_item(3, Actor::Tool, vec![batch_result(10)?])?,
    ])?;
    let (messages, _, _) = translate_history(
        &source,
        &policy,
        &ToolOwnership::default(),
        false,
        true,
        &mut Vec::new(),
    )?;
    let wire = serde_json::to_value(messages)?;
    assert_eq!(
        wire[1]["content"][0]["signature"],
        "synthetic-tool-signature"
    );
    assert_eq!(wire[1]["content"][1]["id"], "call_10");
    assert_eq!(wire[2]["content"][0]["tool_use_id"], "call_10");
    Ok(())
}

#[test]
fn manual_tool_loop_without_its_original_thinking_refuses_before_dispatch() -> TestResult {
    let history = vec![
        text_item(1, Actor::User, "task")?,
        batch_item(2, Actor::Assistant, vec![batch_call(10)?])?,
        batch_item(3, Actor::Tool, vec![batch_result(10)?])?,
    ];
    for effort in [
        ReasoningEffort::Low,
        ReasoningEffort::TokenBudget(std::num::NonZeroU32::new(1024).ok_or("budget")?),
    ] {
        let (plan, policy) = lowering_fixture_with_tool_choice_and_history(
            ToolChoice::Auto,
            history.clone(),
            plakous_model::GenerationLimits::new(std::num::NonZeroU32::new(4096), Vec::new())?,
            ReasoningPolicy::Enabled {
                effort: Some(effort),
                requirement: CapabilityRequirement::Required,
            },
        )?;
        let lowered = lower_request(&plan, &policy, ToolOwnership::default());
        if matches!(effort, ReasoningEffort::TokenBudget(_)) {
            let error = lowered.err().ok_or("manual loop must refuse")?;
            assert_eq!(error.kind(), MessagesRequestErrorKind::ProtectedHistory);
        } else {
            assert_eq!(
                serde_json::from_slice::<Value>(lowered?.body())?["thinking"]["type"],
                "adaptive"
            );
        }
    }
    Ok(())
}

#[test]
fn manual_multi_round_tool_loop_keeps_the_first_assistant_thinking() -> TestResult {
    let reasoning = ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::TokenBudget(
            std::num::NonZeroU32::new(1024).ok_or("budget")?,
        )),
        requirement: CapabilityRequirement::Required,
    };
    let limits = plakous_model::GenerationLimits::new(std::num::NonZeroU32::new(4096), Vec::new())?;
    let (_, policy) = lowering_fixture_with_tool_choice_and_history(
        ToolChoice::Auto,
        vec![text_item(1, Actor::User, "task")?],
        limits.clone(),
        reasoning,
    )?;
    let thinking = ContentBlock::Reasoning(ReasoningBlock::new(
        ReasoningVisibility::Omitted,
        None,
        Some(bind_replay_payload(
            &policy,
            ModelId::new("model/exact")?,
            SIGNATURE_REPLAY_KIND,
            raw("\"synthetic-tool-signature\"")?,
        )?),
    )?);
    let (plan, policy) = lowering_fixture_with_tool_choice_and_history(
        ToolChoice::Auto,
        vec![
            text_item(1, Actor::User, "task")?,
            batch_item(2, Actor::Assistant, vec![thinking, batch_call(10)?])?,
            batch_item(3, Actor::Tool, vec![batch_result(10)?])?,
            batch_item(4, Actor::Assistant, vec![batch_call(11)?])?,
            batch_item(5, Actor::Tool, vec![batch_result(11)?])?,
        ],
        limits,
        reasoning,
    )?;
    let lowered = lower_request(&plan, &policy, ToolOwnership::default())?;
    let wire: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(
        wire["messages"][1]["content"][0]["signature"],
        "synthetic-tool-signature"
    );
    assert_eq!(wire["messages"][4]["content"][0]["tool_use_id"], "call_11");
    Ok(())
}

#[test]
fn compaction_never_replays_thinking_bound_to_the_replaced_prefix() -> TestResult {
    for strict in [false, true] {
        for (kind, visibility) in [
            (SIGNATURE_REPLAY_KIND, ReasoningVisibility::Visible),
            (SIGNATURE_REPLAY_KIND, ReasoningVisibility::Omitted),
            (REDACTED_REPLAY_KIND, ReasoningVisibility::Redacted),
        ] {
            let policy = policy()?.with_strict_thinking_replay(strict);
            let assistant = ConversationItem::new(
                TestEntryId::try_from(TestUuid::from_u128(4))?,
                Actor::Assistant,
                vec![
                    ContentBlock::Reasoning(ReasoningBlock::new(
                        visibility,
                        (visibility == ReasoningVisibility::Visible)
                            .then(|| "synthetic reasoning".into()),
                        Some(bind_replay_payload(
                            &policy,
                            ModelId::new("claude-exact")?,
                            kind,
                            raw("\"synthetic-signature\"")?,
                        )?),
                    )?),
                    ContentBlock::Text(TestTextBlock::new("synthetic answer", None)?),
                ],
                Provenance::ModelOutput,
            )?;
            let source = ConversationView::from_shared(
                CanonicalHistory::new(vec![
                    HistorySegment::new(
                        HistorySegmentKind::RecentIntent,
                        vec![text_item(1, Actor::User, "old intent")?],
                    )?,
                    HistorySegment::new(
                        HistorySegmentKind::Conversation,
                        vec![text_item(2, Actor::Assistant, "old answer")?],
                    )?,
                    HistorySegment::new(
                        HistorySegmentKind::RecentIntent,
                        vec![text_item(3, Actor::User, "latest intent")?],
                    )?,
                    HistorySegment::new(HistorySegmentKind::ReasoningReplay, vec![assistant])?,
                ])?,
                WorkingProjection::default(),
            );
            let translate = |view: &ConversationView| -> TestResult<Value> {
                let (messages, _, _) = translate_history(
                    view,
                    &policy,
                    &ToolOwnership::default(),
                    false,
                    true,
                    &mut Vec::new(),
                )?;
                Ok(serde_json::to_value(messages)?)
            };
            let original = translate(&source)?;
            assert_eq!(
                original[3]["content"][0]["type"],
                if kind == REDACTED_REPLAY_KIND {
                    "redacted_thinking"
                } else {
                    "thinking"
                }
            );
            let cut = SemanticCompactionCut::seal(&source, &ContextHistoryPlan::new(&source)?)?;
            // The sealed summary request retains the original prefix unchanged.
            assert_eq!(
                translate(cut.summary_input())?,
                serde_json::to_value(&original.as_array().ok_or("messages")?[..2])?
            );
            let summary = HistorySegment::new(
                HistorySegmentKind::Compaction,
                vec![ConversationItem::new(
                    TestEntryId::try_from(TestUuid::from_u128(5))?,
                    Actor::Assistant,
                    vec![ContentBlock::Compaction(CompactionBlock::new(
                        "synthetic summary",
                    )?)],
                    Provenance::ModelOutput,
                )?],
            )?;
            let successor = cut.build_successor(&summary)?;
            let round_trip: CanonicalHistory =
                serde_json::from_slice(&serde_json::to_vec(&successor)?)?;
            let wire = translate(&ConversationView::from_shared(
                round_trip,
                WorkingProjection::default(),
            ))?;
            assert_eq!(
                wire[2]["content"],
                serde_json::json!([{"type":"text","text":"synthetic answer"}])
            );
            assert!(!wire.to_string().contains("synthetic-signature"));
            assert!(!wire.to_string().contains("synthetic reasoning"));
        }
    }
    Ok(())
}

fn steering_item(id: u128, value: &str) -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        TestEntryId::try_from(TestUuid::from_u128(id))?,
        Actor::User,
        vec![ContentBlock::Text(TestTextBlock::new(value, None)?)],
        Provenance::Instruction,
    )?)
}

fn manual_budget() -> plakous_model::ReasoningPolicy {
    ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::TokenBudget(
            std::num::NonZeroU32::new(1024).expect("budget"),
        )),
        requirement: CapabilityRequirement::Required,
    }
}

#[test]
fn manual_validator_treats_steering_merged_into_tool_results_as_an_open_loop() -> TestResult {
    let limits = plakous_model::GenerationLimits::new(std::num::NonZeroU32::new(4096), Vec::new())?;
    let (_, policy) = lowering_fixture_with_tool_choice_and_history(
        ToolChoice::Auto,
        vec![text_item(1, Actor::User, "task")?],
        limits.clone(),
        manual_budget(),
    )?;
    let thinking = ContentBlock::Reasoning(ReasoningBlock::new(
        ReasoningVisibility::Omitted,
        None,
        Some(bind_replay_payload(
            &policy,
            ModelId::new("model/exact")?,
            SIGNATURE_REPLAY_KIND,
            raw("\"synthetic-tool-signature\"")?,
        )?),
    )?);
    let open = vec![
        text_item(1, Actor::User, "task")?,
        batch_item(2, Actor::Assistant, vec![batch_call(10)?])?,
        batch_item(3, Actor::Tool, vec![batch_result(10)?])?,
        steering_item(4, "steer: continue")?,
    ];
    let (plan, policy) = lowering_fixture_with_tool_choice_and_history(
        ToolChoice::Auto,
        open,
        limits.clone(),
        manual_budget(),
    )?;
    let error = lower_request(&plan, &policy, ToolOwnership::default())
        .err()
        .ok_or("merged steering must still refuse a tool loop that lost its thinking")?;
    assert_eq!(error.kind(), MessagesRequestErrorKind::ProtectedHistory);

    let (plan, policy) = lowering_fixture_with_tool_choice_and_history(
        ToolChoice::Auto,
        vec![
            text_item(1, Actor::User, "task")?,
            batch_item(2, Actor::Assistant, vec![thinking, batch_call(10)?])?,
            batch_item(3, Actor::Tool, vec![batch_result(10)?])?,
            steering_item(4, "steer: continue")?,
        ],
        limits.clone(),
        manual_budget(),
    )?;
    let lowered = lower_request(&plan, &policy, ToolOwnership::default())?;
    let wire: Value = serde_json::from_slice(lowered.body())?;
    assert_eq!(
        wire["messages"][1]["content"][0]["signature"],
        "synthetic-tool-signature"
    );
    let last = wire["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .ok_or("messages")?;
    let content = last["content"].as_array().ok_or("content")?;
    assert!(content.iter().any(|block| block["type"] == "tool_result"));
    assert!(
        content
            .iter()
            .any(|block| block["text"] == "steer: continue")
    );

    let (plan, policy) = lowering_fixture_with_tool_choice_and_history(
        ToolChoice::Auto,
        vec![
            text_item(1, Actor::User, "task")?,
            batch_item(2, Actor::Assistant, vec![batch_call(10)?])?,
            batch_item(3, Actor::Tool, vec![batch_result(10)?])?,
            batch_item(
                6,
                Actor::Assistant,
                vec![ContentBlock::Text(TestTextBlock::new("done", None)?)],
            )?,
            text_item(7, Actor::User, "next question")?,
        ],
        limits,
        manual_budget(),
    )?;
    lower_request(&plan, &policy, ToolOwnership::default())?;
    Ok(())
}
