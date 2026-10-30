use super::*;
use plakous_model::{
    DataClassification, ModelId, ProtocolId, ReplayKindId, ReplayScope, ToolCallerLineage,
    ToolExecutionOwner, ToolName,
};
use serde_json::value::RawValue;

fn openai_policy() -> ProjectorPolicy {
    ProjectorPolicy {
        tool_args: ToolArgsFinishPolicy::SoftSeedEmptyOnly,
        reasoning_emit: ReasoningEmitMode::DeferToFinish,
    }
}

impl ResponseProjectorCore {
    fn finish_deferred_reasoning(
        &mut self,
        provider_index: u32,
        replay: &ReplayEnvelope,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        let entry_id = self.item(provider_index)?.entry_id;
        let provider_blocks: Vec<u32> = self
            .item(provider_index)?
            .provider_blocks
            .keys()
            .copied()
            .collect();
        for provider_block in provider_blocks {
            let location = self.location(provider_index, provider_block)?;
            let (chunks, complete) = match self.block_mut(location)? {
                ProjectedBlock::Reasoning {
                    chunks,
                    complete,
                    replay: slot,
                    ..
                } => {
                    *slot = Some(replay.clone());
                    (chunks.clone(), *complete)
                }
                _ => {
                    return Err(FamilyProjectionError::InvalidState(
                        "reasoning item contains a non-reasoning block",
                    ));
                }
            };
            if !complete {
                return Err(FamilyProjectionError::InvalidState(
                    "reasoning output item done before summary part completion",
                ));
            }
            output.push(ModelEvent::BlockStarted(BlockStarted::new(
                entry_id,
                location,
                BlockStart::Reasoning {
                    visibility: ReasoningVisibility::Summary,
                    replay: Some(replay.clone()),
                },
            )));
            for chunk in chunks {
                output.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                    location, chunk,
                )));
            }
            output.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        Ok(())
    }
}

fn empty_core(policy: ProjectorPolicy) -> ResponseProjectorCore {
    core_with_byte_limit(policy, ProjectorLimits::default().max_accumulated_bytes)
}

fn core_with_byte_limit(
    policy: ProjectorPolicy,
    max_accumulated_bytes: usize,
) -> ResponseProjectorCore {
    ResponseProjectorCore::new_with_policy(
        CanonicalIdSources::system(),
        ProjectorLimits {
            max_accumulated_bytes,
            ..ProjectorLimits::default()
        },
        policy,
    )
}

fn test_digest(label: &[u8]) -> Digest {
    Digest::sha256(plakous_model::DigestDomain::RequestPlan, 1, &[label]).expect("digest")
}

fn start_function(core: &mut ResponseProjectorCore) -> EntryId {
    let mut out = Vec::new();
    core.start_response(
        "resp_test".into(),
        ModelResponseMetadata::empty(),
        test_digest(b"fn"),
        &mut out,
    )
    .expect("start");
    let entry = core
        .register_item(
            0,
            ProjectedItemKind::Function,
            Some("fc_1".into()),
            Some("call_1".into()),
            true,
        )
        .expect("register");
    let name = ToolName::new("lookup").expect("name");
    core.start_tool_block_with(
        0,
        entry,
        ToolCorrelation::new(Some("call_1".into()), None, ToolCallerLineage::Direct)
            .expect("correlation"),
        name,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        "",
        true,
        &mut out,
    )
    .expect("tool start");
    entry
}

#[test]
fn soft_seed_empty_only_seeds_when_deltas_empty() {
    let mut core = empty_core(openai_policy());
    let _ = start_function(&mut core);
    let mut out = Vec::new();
    core.finish_function_arguments(0, r#"{"q":1}"#, &mut out)
        .expect("finish");
    let location = core.location(0, 0).expect("loc");
    match core.block(location).expect("block") {
        ProjectedBlock::Tool {
            bytes, complete, ..
        } => {
            assert!(*complete);
            assert_eq!(bytes.as_slice(), br#"{"q":1}"#);
        }
        _ => panic!("expected tool"),
    }
    assert!(
        out.iter()
            .any(|e| matches!(e, ModelEvent::ToolInputDelta(_))),
        "empty known must seed a ToolInputDelta"
    );
}

#[test]
fn soft_seed_empty_only_keeps_non_empty_deltas() {
    let mut core = empty_core(openai_policy());
    let _ = start_function(&mut core);
    let mut out = Vec::new();
    core.append_tool(0, r#"{"q":1}"#, &mut out).expect("delta");
    out.clear();
    core.finish_function_arguments(0, r#"{"q": 1}"#, &mut out)
        .expect("finish keep");
    let location = core.location(0, 0).expect("loc");
    match core.block(location).expect("block") {
        ProjectedBlock::Tool {
            bytes, complete, ..
        } => {
            assert!(*complete);
            // SoftSeedEmptyOnly: keep delta form, not done re-serialization.
            assert_eq!(bytes.as_slice(), br#"{"q":1}"#);
        }
        _ => panic!("expected tool"),
    }
    assert!(
        !out.iter()
            .any(|e| matches!(e, ModelEvent::ToolInputDelta(_))),
        "must not replace or re-emit args when deltas already present"
    );
}

#[test]
fn soft_seed_semantic_diverge_prefers_done_payload() {
    let mut core = empty_core(openai_policy());
    let _ = start_function(&mut core);
    let mut out = Vec::new();
    core.append_tool(0, r#"{"q":1}"#, &mut out).expect("delta");
    assert_eq!(core.accumulated_bytes, r#"{"q":1}"#.len());
    out.clear();
    core.finish_function_arguments(0, r#"{"q":1,"extra":true}"#, &mut out)
        .expect("finish");
    assert_eq!(core.accumulated_bytes, r#"{"q":1,"extra":true}"#.len());
    let location = core.location(0, 0).expect("loc");
    match core.block(location).expect("block") {
        ProjectedBlock::Tool {
            bytes, complete, ..
        } => {
            assert!(*complete);
            assert_eq!(
                std::str::from_utf8(bytes).expect("utf8"),
                r#"{"q":1,"extra":true}"#
            );
        }
        _ => panic!("expected tool"),
    }
}

#[test]
fn soft_seed_semantically_equivalent_repeated_finish_is_idempotent() {
    let mut core = empty_core(openai_policy());
    let _ = start_function(&mut core);
    let mut out = Vec::new();
    core.append_tool(0, r#"{"q":1}"#, &mut out).expect("delta");
    core.finish_function_arguments(0, r#"{"q": 1}"#, &mut out)
        .expect("first finish");
    let accumulated = core.accumulated_bytes;
    out.clear();

    core.finish_function_arguments(0, r#"{ "q" : 1 }"#, &mut out)
        .expect("idempotent repeated finish");

    assert_eq!(core.accumulated_bytes, accumulated);
    assert_eq!(out, [] as [ModelEvent; 0]);
    let location = core.location(0, 0).expect("location");
    let ProjectedBlock::Tool {
        bytes, complete, ..
    } = core.block(location).expect("block")
    else {
        panic!("expected tool")
    };
    assert_eq!(bytes, br#"{"q":1}"#);
    assert!(*complete);
}

#[test]
fn soft_seed_empty_only_empty_done_becomes_object() {
    let mut core = empty_core(openai_policy());
    let _ = start_function(&mut core);
    let mut out = Vec::new();
    core.finish_function_arguments(0, "", &mut out)
        .expect("finish empty");
    let location = core.location(0, 0).expect("loc");
    match core.block(location).expect("block") {
        ProjectedBlock::Tool { bytes, .. } => assert_eq!(bytes.as_slice(), b"{}"),
        _ => panic!("expected tool"),
    }
}

#[test]
fn done_replacement_accounting_is_exact_and_fail_closed_at_boundaries() {
    for limit in 1_usize..=16 {
        for initial_len in 1..=limit {
            for replacement_len in 1..=limit.saturating_add(1) {
                let mut core = core_with_byte_limit(ProjectorPolicy::default(), limit);
                let _ = start_function(&mut core);
                let mut out = Vec::new();
                let initial = "a".repeat(initial_len);
                let replacement = "b".repeat(replacement_len);
                core.append_tool(0, &initial, &mut out).expect("initial");
                assert_eq!(core.accumulated_bytes, initial_len);

                let result = core.finish_function_arguments_with(
                    0,
                    &replacement,
                    ToolArgsFinishPolicy::SoftSeedEmptyOnly,
                    true,
                    &mut out,
                );
                let location = core.location(0, 0).expect("location");
                let ProjectedBlock::Tool {
                    bytes, complete, ..
                } = core.block(location).expect("block")
                else {
                    panic!("expected tool")
                };
                if replacement_len <= limit {
                    result.expect("replacement within bound");
                    assert_eq!(core.accumulated_bytes, replacement_len);
                    assert_eq!(bytes, replacement.as_bytes());
                    assert!(*complete);
                } else {
                    assert!(matches!(
                        result,
                        Err(FamilyProjectionError::ResourceLimit("projected bytes"))
                    ));
                    assert_eq!(core.accumulated_bytes, initial_len);
                    assert_eq!(bytes, initial.as_bytes());
                    assert!(!*complete);
                }
            }
        }
    }
}

#[test]
fn rejected_empty_seed_reservation_leaves_byte_ledger_unchanged() {
    let mut core = core_with_byte_limit(ProjectorPolicy::default(), 1);
    let _ = start_function(&mut core);
    let mut out = Vec::new();
    assert!(matches!(
        core.finish_function_arguments_with(
            0,
            "",
            ToolArgsFinishPolicy::SoftSeedEmptyOnly,
            true,
            &mut out,
        ),
        Err(FamilyProjectionError::ResourceLimit("projected bytes"))
    ));
    assert_eq!(core.accumulated_bytes, 0);
    let location = core.location(0, 0).expect("location");
    let ProjectedBlock::Tool {
        bytes, complete, ..
    } = core.block(location).expect("block")
    else {
        panic!("expected tool")
    };
    assert_eq!(bytes.as_slice(), [] as [u8; 0]);
    assert!(!*complete);
}

#[test]
fn defer_reasoning_buffers_until_finish() {
    let mut core = empty_core(openai_policy());
    let mut out = Vec::new();
    core.start_response(
        "resp_r".into(),
        ModelResponseMetadata::empty(),
        test_digest(b"rs"),
        &mut out,
    )
    .expect("start");
    let _ = core
        .register_item(
            0,
            ProjectedItemKind::Reasoning,
            Some("rs_1".into()),
            None,
            true,
        )
        .expect("register");
    out.clear();
    core.start_reasoning_block(0, 0, &mut out).expect("part");
    assert!(out.is_empty(), "DeferToFinish must not emit on part.added");
    core.append_reasoning(0, 0, "think", &mut out)
        .expect("delta");
    assert!(
        out.is_empty(),
        "DeferToFinish must not emit live ReasoningDelta"
    );
    core.complete_provider_block_with_emit(0, 0, false, &mut out)
        .expect("part done silent");
    assert_eq!(out, [] as [ModelEvent; 0]);

    let payload = RawValue::from_string("{}".into()).expect("raw");
    core.reserve_bytes(payload.get().len())
        .expect("replay bytes");
    let replay = ReplayEnvelope::new(
        ProtocolId::new("openai.responses").expect("proto"),
        None,
        ReplayScope::ExactAccountModel {
            model: ModelId::new("gpt-test").expect("model"),
            account: Digest::sha256(
                plakous_model::DigestDomain::ModelBinding,
                1,
                &[b"test-account"],
            )
            .expect("account"),
        },
        ReplayKindId::new("test.reasoning").expect("kind"),
        1,
        DataClassification::Sensitive,
        payload,
    )
    .expect("replay");
    core.finish_deferred_reasoning(0, &replay, &mut out)
        .expect("finish");
    assert!(
        matches!(out.first(), Some(ModelEvent::BlockStarted(_))),
        "deferred finish starts with BlockStarted"
    );
    assert!(
        out.iter()
            .any(|e| matches!(e, ModelEvent::ReasoningDelta(_))),
        "deferred finish emits buffered deltas"
    );
    assert!(
        matches!(out.last(), Some(ModelEvent::BlockCompleted(_))),
        "deferred finish ends with BlockCompleted"
    );
}
