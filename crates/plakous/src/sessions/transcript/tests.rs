#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;
use plakous_agent::internal::{
    DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, ReconstructiveComponent, RuntimeComponentKind,
};
use plakous_model::{
    HistorySegment, HistorySegmentKind, TextBlock, ToolCallBlock, ToolCallState, ToolCallerLineage,
    ToolCorrelation, ToolExecutionOwner, ToolResultBlock, ToolResultKind, ToolResultPart,
};

fn id<T: std::str::FromStr>(value: u16) -> T
where
    T::Err: fmt::Debug,
{
    format!("00000000-0000-4000-8000-{value:012x}")
        .parse()
        .unwrap()
}

#[allow(
    clippy::too_many_lines,
    reason = "complete canonical and durable composition fixture"
)]
fn source(name: &str, retain_owner: bool) -> SessionTranscriptSource {
    let parent = id(1);
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for index in 0..4 {
        let call_id = id(index + 1);
        let lineage = if index == 0 {
            ToolCallerLineage::Direct
        } else {
            ToolCallerLineage::local_programmatic(41, 7, "cell-7", u32::from(index), call_id, 1)
                .unwrap()
        };
        let correlation = ToolCorrelation::new(None, None, lineage).unwrap();
        calls.push(ContentBlock::ToolCall(
            ToolCallBlock::new(
                call_id,
                ToolName::new(if index == 0 { name } else { "read_file" }).unwrap(),
                ToolExecutionOwner::PlakLocal,
                ToolCallState::Requested,
                correlation.clone(),
                if index != 0 {
                    "{\"path\":\"src/main.rs\"}"
                } else if name == "exec" {
                    "text(await tools.read_file({path:'src/main.rs'}))"
                } else {
                    "{\"code\":\"text(await tools.read_file({path:'src/main.rs'}))\"}"
                },
            )
            .unwrap(),
        ));
        results.push(ContentBlock::ToolResult(
            ToolResultBlock::new(
                call_id,
                ToolExecutionOwner::PlakLocal,
                if index == 2 {
                    ToolResultKind::Failure
                } else {
                    ToolResultKind::Success
                },
                vec![ToolResultPart::Text(
                    TextBlock::new(
                        if index != 0 {
                            "read result"
                        } else if name == "exec" {
                            "Script running with cell ID cell-7"
                        } else {
                            "Script completed"
                        },
                        None,
                    )
                    .unwrap(),
                )],
                correlation,
            )
            .unwrap(),
        ));
    }
    let history = CanonicalHistory::new(vec![
        HistorySegment::new(
            HistorySegmentKind::ToolExchange,
            vec![
                ConversationItem::new(id(10), Actor::Assistant, calls, Provenance::ModelOutput)
                    .unwrap(),
                ConversationItem::new(id(11), Actor::Tool, results, Provenance::ToolExecution)
                    .unwrap(),
            ],
        )
        .unwrap(),
    ])
    .unwrap();
    let mut operations = if retain_owner {
        vec![
            DurableOperationRecord::new(
                41,
                DurableOperationKind::CompositionEvaluation {
                    turn_id: id(20),
                    attempt_id: id(21),
                    call_id: parent,
                    source_index: 0,
                },
                ReconstructiveComponent::new(
                    RuntimeComponentKind::Operation,
                    1,
                    &b"offline evaluation"[..],
                )
                .unwrap(),
                vec![DurableOperationTransition::new(
                    id(22),
                    DurableOperationState::Prepared,
                )],
            )
            .unwrap(),
        ]
    } else {
        vec![]
    };
    if retain_owner {
        for sequence in 1..=3 {
            operations.push(
                DurableOperationRecord::new(
                    41 + u64::from(sequence),
                    DurableOperationKind::CompositionChild {
                        turn_id: id(20),
                        attempt_id: id(21),
                        call_id: id(sequence + 1),
                        evaluation_operation_id: 41,
                        interpreter_generation: 7,
                        cell_id: "cell-7".into(),
                        request_sequence: u32::from(sequence),
                    },
                    ReconstructiveComponent::new(
                        RuntimeComponentKind::Operation,
                        1,
                        &b"offline child edge"[..],
                    )
                    .unwrap(),
                    vec![DurableOperationTransition::new(
                        id(50 + sequence),
                        DurableOperationState::Prepared,
                    )],
                )
                .unwrap(),
            );
        }
    }
    SessionTranscriptSource::new(
        SessionLocator::new(id(30), id(31), id(32)),
        history,
        RuntimeOperationLedger::new(vec![], operations).unwrap(),
    )
}

#[test]
fn transcript_tail_projects_live_lineage_from_explicit_evaluation_receipt() {
    for name in ["exec", "exec_js"] {
        let mut source = source(name, true);
        if name == "exec" {
            append_continuation(&mut source);
        }
        let tail = source.tail(512, usize::MAX);
        assert_eq!(
            tail.items()
                .iter()
                .flat_map(SessionTranscriptItem::parts)
                .filter(|part| matches!(part, SessionTranscriptPart::ToolCall { .. }))
                .count(),
            if name == "exec" { 5 } else { 4 }
        );
        for item in tail.items() {
            for (index, part) in item.parts().iter().enumerate() {
                let (SessionTranscriptPart::ToolCall { lineage, .. }
                | SessionTranscriptPart::ToolResult { lineage, .. }) = part
                else {
                    unreachable!()
                };
                let expected = if index == 0 {
                    ToolCallerLineageView::local_composition_marker(Some("cell-7"))
                } else {
                    ToolCallerLineageView::from_lineage(
                        &ToolCallerLineage::local_programmatic(
                            41,
                            7,
                            "cell-7",
                            u32::try_from(index).unwrap(),
                            id(u16::try_from(index).unwrap() + 1),
                            1,
                        )
                        .unwrap(),
                        Some(id(1)),
                    )
                };
                assert_eq!(lineage, &expected, "{name} part {index}");
            }
        }
    }
}

#[test]
fn transcript_tail_without_retained_receipt_keeps_nested_identity_but_no_parent() {
    let tail = source("exec", false).tail(512, usize::MAX);
    for item in tail.items() {
        for part in item.parts().iter().skip(1) {
            let (SessionTranscriptPart::ToolCall { lineage, .. }
            | SessionTranscriptPart::ToolResult { lineage, .. }) = part
            else {
                unreachable!()
            };
            assert_eq!(lineage.cell_id(), Some("cell-7"));
            assert_eq!(lineage.parent_call_id(), None);
        }
    }
}

#[test]
fn probe_seeded_history_operation_id_collision_does_not_reparent_old_nested_calls() {
    let mut seeded = source("exec", false);
    // Seeded sessions keep canonical history, but start a new operation ledger.
    let record = |operation_id, kind| {
        DurableOperationRecord::new(
            operation_id,
            kind,
            ReconstructiveComponent::new(
                RuntimeComponentKind::Operation,
                1,
                &b"new session record"[..],
            )
            .unwrap(),
            vec![DurableOperationTransition::new(
                id(90 + u16::try_from(operation_id).unwrap()),
                DurableOperationState::Prepared,
            )],
        )
        .unwrap()
    };
    seeded.ledger = RuntimeOperationLedger::new(
        vec![],
        vec![
            record(
                41,
                DurableOperationKind::CompositionEvaluation {
                    turn_id: id(80),
                    attempt_id: id(81),
                    call_id: id(99),
                    source_index: 0,
                },
            ),
            record(
                45,
                DurableOperationKind::CompositionChild {
                    turn_id: id(80),
                    attempt_id: id(81),
                    call_id: id(100),
                    evaluation_operation_id: 41,
                    interpreter_generation: 88,
                    cell_id: "cell-new".into(),
                    request_sequence: 1,
                },
            ),
        ],
    )
    .unwrap();
    append_exchange(&mut seeded, id(99), "exec", "text('unrelated')", 200);
    let tail = seeded.tail(512, usize::MAX);
    for part in tail.items().iter().flat_map(SessionTranscriptItem::parts) {
        let (SessionTranscriptPart::ToolCall {
            call_id, lineage, ..
        }
        | SessionTranscriptPart::ToolResult {
            call_id, lineage, ..
        }) = part
        else {
            continue;
        };
        if [id(2), id(3), id(4)].contains(call_id) {
            assert_eq!(lineage.parent_call_id(), None, "carried call {call_id}");
        }
        if *call_id == id(99) {
            assert_eq!(
                lineage.cell_id(),
                None,
                "new marker must not learn the carried cell"
            );
        }
    }
}

fn append_exchange(
    source: &mut SessionTranscriptSource,
    call_id: CallId,
    name: &str,
    arguments: &str,
    entry: u16,
) {
    let correlation = ToolCorrelation::new(None, None, ToolCallerLineage::Direct).unwrap();
    source.history = source
        .history
        .append(
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![
                    ConversationItem::new(
                        id(entry),
                        Actor::Assistant,
                        vec![ContentBlock::ToolCall(
                            ToolCallBlock::new(
                                call_id,
                                ToolName::new(name).unwrap(),
                                ToolExecutionOwner::PlakLocal,
                                ToolCallState::Requested,
                                correlation.clone(),
                                arguments,
                            )
                            .unwrap(),
                        )],
                        Provenance::ModelOutput,
                    )
                    .unwrap(),
                    ConversationItem::new(
                        id(entry + 1),
                        Actor::Tool,
                        vec![ContentBlock::ToolResult(
                            ToolResultBlock::new(
                                call_id,
                                ToolExecutionOwner::PlakLocal,
                                ToolResultKind::Success,
                                vec![ToolResultPart::Text(
                                    TextBlock::new("Script completed", None).unwrap(),
                                )],
                                correlation,
                            )
                            .unwrap(),
                        )],
                        Provenance::ToolExecution,
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        )
        .unwrap();
}

fn append_continuation(source: &mut SessionTranscriptSource) {
    source.ledger = source
        .ledger
        .append_operation(
            DurableOperationRecord::new(
                45,
                DurableOperationKind::CompositionContinuation {
                    turn_id: id(20),
                    attempt_id: id(21),
                    call_id: id(5),
                    source_index: 1,
                    evaluation_operation_id: 41,
                },
                ReconstructiveComponent::new(
                    RuntimeComponentKind::Operation,
                    1,
                    &b"offline continuation"[..],
                )
                .unwrap(),
                vec![DurableOperationTransition::new(
                    id(23),
                    DurableOperationState::Prepared,
                )],
            )
            .unwrap(),
        )
        .unwrap();
    append_exchange(source, id(5), "wait", "{\"cell_id\":\"cell-7\"}", 12);
}

#[test]
fn continuation_owner_comes_from_the_same_snapshot_not_cell_names() {
    let mut source = source("exec", true);
    append_continuation(&mut source);
    let call_id = id(5);
    let tail = source.tail(512, usize::MAX);
    assert_eq!(tail.composition_owner(id(1)), Some(id(1)));
    assert_eq!(tail.composition_owner(call_id), Some(id(1)));
    assert_eq!(tail.composition_owner(id(99)), None);
    assert_eq!(
        history_composition_calls(&source.ledger, 0),
        [] as [(CallId, CallId); 0]
    );
    assert_eq!(
        history_composition_calls(&source.ledger, 1),
        vec![(call_id, id(1))]
    );
    // The captured source is independent of later ledger retirement.
    let captured = source.clone();
    source.ledger = RuntimeOperationLedger::new(vec![], vec![]).unwrap();
    assert_eq!(
        captured.tail(512, usize::MAX).composition_owner(call_id),
        Some(id(1))
    );
    assert_eq!(
        source.tail(512, usize::MAX).composition_owner(call_id),
        None
    );
}

#[test]
fn transcript_lineage_and_marker_identity_are_inside_the_item_byte_bound() {
    let source = source("exec", true);
    let full = source.tail(512, usize::MAX);
    let newest_bytes = full.items().last().unwrap().retained_bytes();
    let bounded = source.tail(512, newest_bytes);
    assert!(bounded.truncated());
    assert_eq!(bounded.items().len(), 1);
    assert_eq!(bounded.items()[0].retained_bytes(), newest_bytes);
    assert_eq!(source.tail(0, 0).items(), []);
}

#[test]
fn tail_item_truncation_keeps_nested_calls_but_evicts_their_marker() {
    let mut source = source("exec_js", true);
    let segments = source.history.segments();
    let items = segments[0].items();
    source.history = CanonicalHistory::new(
        items[0]
            .blocks()
            .iter()
            .zip(items[1].blocks())
            .enumerate()
            .map(|(index, (call, result))| {
                let entry = 300 + u16::try_from(index).unwrap() * 2;
                HistorySegment::new(
                    HistorySegmentKind::ToolExchange,
                    vec![
                        ConversationItem::new(
                            id(entry),
                            Actor::Assistant,
                            vec![call.clone()],
                            Provenance::ModelOutput,
                        )
                        .unwrap(),
                        ConversationItem::new(
                            id(entry + 1),
                            Actor::Tool,
                            vec![result.clone()],
                            Provenance::ToolExecution,
                        )
                        .unwrap(),
                    ],
                )
                .unwrap()
            })
            .collect(),
    )
    .unwrap();
    let tail = source.tail(2, usize::MAX);
    assert!(tail.truncated());
    assert_eq!(tail.items().len(), 2);
    assert_eq!(tail.composition_owner(id(1)), None);
    for part in tail.items().iter().flat_map(SessionTranscriptItem::parts) {
        let (SessionTranscriptPart::ToolCall {
            call_id, lineage, ..
        }
        | SessionTranscriptPart::ToolResult {
            call_id, lineage, ..
        }) = part
        else {
            unreachable!();
        };
        assert_eq!(*call_id, id(4));
        assert_eq!(lineage.parent_call_id(), Some(id(1)));
    }
}

fn ledger_only_source() -> SessionTranscriptSource {
    ledger_source(false)
}

fn ledger_source(duplicate: bool) -> SessionTranscriptSource {
    let mut source = source("exec_js", true);
    let segments = source.history.segments();
    let items = segments[0].items();
    let calls = items[0].blocks();
    let results = items[1].blocks();
    for (offset, index) in (1..4).chain(duplicate.then_some(1)).enumerate() {
        let ContentBlock::ToolCall(call) = &calls[index] else {
            unreachable!();
        };
        let ContentBlock::ToolResult(result) = &results[index] else {
            unreachable!();
        };
        let request = serde_json::to_vec(&(
            plakous_model::Digest::sha256(
                plakous_model::DigestDomain::ToolDefinition,
                1,
                &[b"fixture"],
            )
            .unwrap(),
            call,
        ))
        .unwrap();
        let outcome = serde_json::to_vec(&(
            "completed-tool-outcome-v1",
            id::<plakous_model::EntryId>(80 + u16::try_from(index).unwrap()),
            serde_json::json!({"result":result,"dispatched":true,"presentation_subject":null}),
        ))
        .unwrap();
        let operation = 51 + u64::try_from(offset).unwrap();
        source.ledger = source
            .ledger
            .append_operation(
                DurableOperationRecord::new(
                    operation,
                    DurableOperationKind::Effect {
                        turn_id: id(20),
                        attempt_id: id(21),
                        call_id: call.call_id(),
                        source_index: 0,
                    },
                    ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, request)
                        .unwrap(),
                    vec![
                        DurableOperationTransition::new(
                            id(600 + u16::try_from(index).unwrap()),
                            DurableOperationState::Prepared,
                        ),
                        DurableOperationTransition::new(
                            id(610 + u16::try_from(index).unwrap()),
                            DurableOperationState::Dispatched,
                        ),
                        DurableOperationTransition::new(
                            id(620 + u16::try_from(index).unwrap()),
                            DurableOperationState::Completed(
                                ReconstructiveComponent::new(
                                    RuntimeComponentKind::Operation,
                                    1,
                                    outcome,
                                )
                                .unwrap(),
                            ),
                        ),
                    ],
                )
                .unwrap(),
            )
            .unwrap();
    }
    source.history = CanonicalHistory::new(vec![
        HistorySegment::new(
            HistorySegmentKind::ToolExchange,
            vec![
                ConversationItem::new(
                    id(10),
                    Actor::Assistant,
                    vec![calls[0].clone()],
                    Provenance::ModelOutput,
                )
                .unwrap(),
                ConversationItem::new(
                    id(11),
                    Actor::Tool,
                    vec![results[0].clone()],
                    Provenance::ToolExecution,
                )
                .unwrap(),
            ],
        )
        .unwrap(),
    ])
    .unwrap();
    source
}

#[test]
fn duplicate_retained_effect_call_ids_are_injected_once() {
    let source = ledger_source(true);
    let tail = source.tail(512, usize::MAX);
    let calls = tail.items().iter().flat_map(SessionTranscriptItem::parts)
        .filter(|part| matches!(part, SessionTranscriptPart::ToolCall { call_id, .. } if *call_id == id(2)))
        .count();
    assert_eq!(calls, 1);
}

#[test]
fn retained_effects_are_ordered_deduplicated_and_charge_tail_item_and_byte_bounds() {
    let source = ledger_only_source();
    let tail = source.tail(512, usize::MAX);
    let calls = tail
        .items()
        .iter()
        .flat_map(SessionTranscriptItem::parts)
        .filter_map(|part| {
            if let SessionTranscriptPart::ToolCall { call_id, .. } = part {
                Some(*call_id)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(calls, vec![id(1), id(2), id(3), id(4)]);
    assert_eq!(tail.items().len(), 5);
    assert!(!tail.truncated());
    let bounded = source.tail(3, usize::MAX);
    assert!(bounded.truncated());
    assert_eq!(bounded.items().len(), 3);
    assert!(
        !bounded
            .items()
            .iter()
            .flat_map(SessionTranscriptItem::parts)
            .any(
                |part| matches!(part,SessionTranscriptPart::ToolCall{call_id,..} if *call_id==id(1))
            )
    );
    let bytes = tail.items().last().unwrap().retained_bytes();
    assert_eq!(source.tail(512, bytes).items().len(), 1);
    let owners = HashSet::from([id(1)]);
    assert!(history_nested_tools(&source.ledger, &owners, 2048, 0).is_empty());
    assert!(history_nested_tools(&source.ledger, &owners, 1, usize::MAX).is_empty());
    let no_owners: HashSet<CallId> = HashSet::new();
    assert!(history_nested_tools(&source.ledger, &no_owners, 2048, usize::MAX).is_empty());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "offline evidence corruption and redaction cases share one fixture"
)]
fn effect_projection_skips_malformed_oversized_and_pruned_evidence_and_redacts_previews() {
    use plakous_agent::internal::facade_support::history_nested_tools;
    let original = ledger_only_source();
    let owners = HashSet::from([id(1)]);
    let effect = original
        .ledger
        .operations()
        .into_iter()
        .find(|record| record.operation_id() == 51)
        .unwrap();
    let replace = |request: Vec<u8>, outcome: Vec<u8>| {
        let component =
            ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, outcome).unwrap();
        let transitions = vec![
            DurableOperationTransition::new(id(601), DurableOperationState::Prepared),
            DurableOperationTransition::new(id(611), DurableOperationState::Dispatched),
            DurableOperationTransition::new(id(621), DurableOperationState::Completed(component)),
        ];
        let request =
            ReconstructiveComponent::new(RuntimeComponentKind::Operation, 1, request).unwrap();
        // Reuse the public projection's retained exact request by reconstructing
        // the one offline fixture operation rather than touching production data.
        let record =
            DurableOperationRecord::new(51, effect.kind().clone(), request, transitions).unwrap();
        let mut source = original.clone();
        let mut records = source
            .ledger
            .operations()
            .into_iter()
            .filter(|prior| prior.operation_id() != 51)
            .cloned()
            .collect::<Vec<_>>();
        records.push(record);
        source.ledger = RuntimeOperationLedger::new(vec![], records).unwrap();
        history_nested_tools(&source.ledger, &owners, 2048, 8 * 1_048_576)
    };
    let raw = serde_json::to_vec(&(
        plakous_model::Digest::sha256(
            plakous_model::DigestDomain::ToolDefinition,
            1,
            &[b"fixture"],
        )
        .unwrap(),
        ToolCallBlock::new(
            id(2),
            ToolName::new("read_file").unwrap(),
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::new(
                None,
                None,
                ToolCallerLineage::local_programmatic(41, 7, "cell-7", 1, id(2), 1).unwrap(),
            )
            .unwrap(),
            "{\"path\":\"safe\"}",
        )
        .unwrap(),
    ))
    .unwrap();
    let bad = replace(raw.clone(), b"invalid outcome".to_vec());
    assert!(!bad.iter().any(|call| call.call_id == id(2)));
    let oversized = replace(raw.clone(), vec![b'x'; 1_048_577]);
    assert!(!oversized.iter().any(|call| call.call_id == id(2)));
    let result = ToolResultBlock::new(
        id(2),
        ToolExecutionOwner::PlakLocal,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(
            TextBlock::new("token=sk-abcdefghijklmnopqrstuvwxyz0123", None).unwrap(),
        )],
        ToolCorrelation::new(
            None,
            None,
            ToolCallerLineage::local_programmatic(41, 7, "cell-7", 1, id(2), 1).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let redacted = replace(
        raw.clone(),
        serde_json::to_vec(&(
            "completed-tool-outcome-v1",
            id::<plakous_model::EntryId>(81),
            serde_json::json!({"result":result,"dispatched":true}),
        ))
        .unwrap(),
    );
    assert!(
        !redacted
            .iter()
            .find(|call| call.call_id == id(2))
            .unwrap()
            .result
            .preview()
            .contains("abcdefghijklmnopqrstuvwxyz0123")
    );
    let failed = replace(
        raw,
        serde_json::to_vec(&(
            "failed-tool-outcome-v1",
            id::<plakous_model::EntryId>(81),
            serde_json::json!({"kind":"Internal","message":"offline rejection","model_error":null,"tool_kind":null,"store_error":null}),
        ))
        .unwrap(),
    );
    assert!(
        !failed
            .iter()
            .find(|call| call.call_id == id(2))
            .unwrap()
            .result
            .succeeded()
    );
    let mut pruned = original;
    pruned.ledger = RuntimeOperationLedger::new(vec![], vec![]).unwrap();
    assert!(history_nested_tools(&pruned.ledger, &owners, 2048, 8 * 1_048_576).is_empty());
}
