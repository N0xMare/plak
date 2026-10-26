use super::*;
use crate::request::{AssistantReplayState, MessageBlock, translate_assistant_bound};
use plakous_model::{ModelResponse, ModelResponseAssembler};

const INITIAL: &str = include_str!("../../../tests/fixtures/stream_citations_initial.sse");
const DELTAS: &str = include_str!("../../../tests/fixtures/stream_citations_deltas.sse");
const UNKNOWN: &str = include_str!("../../../tests/fixtures/stream_unknown_blocks.sse");
const DIRECT: &str = include_str!("../../../tests/fixtures/stream_unknown_direct_tool.sse");

fn project(
    mut projector: MessagesStreamProjector,
    stream: &str,
) -> (ModelResponse, Vec<ModelEvent>) {
    let mut assembler =
        ModelResponseAssembler::new(projector.prepared.plan_digest()).expect("sanitized fixture");
    let mut events = Vec::new();
    for chunk in stream.as_bytes().chunks(13) {
        events.extend(projector.push(chunk).expect("sanitized stream"));
    }
    events.extend(projector.finish_eof().expect("sanitized fixture"));
    let mut terminal = None;
    for event in events.iter().cloned() {
        if let Some(response) = assembler.push(event).expect("dense canonical stream") {
            terminal = Some(response);
        }
    }
    let (response, _, _) = projector
        .into_terminal_projection()
        .expect("sanitized fixture")
        .into_parts();
    assert_eq!(terminal.as_ref(), Some(&response));
    (response, events)
}

fn replay(response: &ModelResponse, policy: &MessagesRequestPolicy) -> Vec<MessageBlock> {
    translate_assistant_bound(
        response.output()[0].blocks(),
        policy,
        &ToolOwnership::default(),
        false,
        true,
        AssistantReplayState {
            container: &mut None,
            server_lineage: &mut crate::artifact::MessagesArtifactLineage::default(),
            diagnostics: &mut Vec::new(),
        },
    )
    .expect("exact replay")
    .0
}

fn assert_checkpoint_replay(response: &ModelResponse) {
    let segment = plakous_model::HistorySegment::new(
        plakous_model::HistorySegmentKind::ReasoningReplay,
        response.output().to_vec(),
    )
    .expect("sanitized fixture");
    let reopened: plakous_model::HistorySegment =
        serde_json::from_str(&serde_json::to_string(&segment).expect("sanitized fixture"))
            .expect("sanitized fixture");
    assert_eq!(segment.canonical_bytes(), reopened.canonical_bytes());
    assert_eq!(
        response.output()[0].blocks(),
        reopened.items()[0].blocks(),
        "native citation bytes survive checkpoint freeze"
    );
}

#[test]
fn initial_and_delta_citations_keep_text_targets_and_exact_native_replay() {
    for (fixture, answer, count) in [(INITIAL, "Cited answer.", 1), (DELTAS, "First. Second.", 2)] {
        for parser in [
            projector(),
            claude_oauth_projector(),
            openrouter_projector(),
        ] {
            let policy = parser.prepared.request_policy.clone();
            let stream =
                if parser.prepared.dialect.extensions() == MessagesStreamExtensions::OpenRouterV1 {
                    format!("{fixture}event: data\ndata: [DONE]\n\n")
                } else {
                    fixture.to_owned()
                };
            let (response, _) = project(parser, &stream);
            let blocks = response.output()[0].blocks();
            assert!(matches!(&blocks[0], ContentBlock::Text(text) if text.text() == answer));
            let citations: Vec<_> = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Citation(citation) => Some(citation),
                    _ => None,
                })
                .collect();
            assert_eq!(citations.len(), count);
            for citation in citations {
                assert_eq!(citation.target().item(), response.output()[0].id());
                assert_eq!(citation.target().block_index(), 0);
                assert_eq!(
                    citation.range(),
                    None,
                    "source offsets are not answer offsets"
                );
            }
            let native = replay(&response, &policy);
            assert_eq!(native.len(), 1, "semantic text must not be replayed twice");
            let MessageBlock::Native { block } = &native[0] else {
                panic!("native cited text")
            };
            assert!(
                block
                    .get()
                    .contains(r#""encrypted_index":"synthetic\u002dindex""#)
            );
            let value: serde_json::Value =
                serde_json::from_str(block.get()).expect("sanitized fixture");
            assert_eq!(value["text"], answer);
            assert_eq!(
                value["citations"]
                    .as_array()
                    .expect("sanitized fixture")
                    .len(),
                count
            );
            assert_checkpoint_replay(&response);
            if fixture == INITIAL {
                let original: serde_json::Value = serde_json::from_str(
                    fixture
                        .lines()
                        .find(|line| line.contains("content_block_start\","))
                        .expect("sanitized fixture")
                        .strip_prefix("data: ")
                        .expect("sanitized fixture"),
                )
                .expect("sanitized fixture");
                assert_eq!(value, original["content_block"]);
            }
        }
    }
}

#[test]
fn unknown_informational_blocks_are_counted_without_content_and_keep_dense_tool_locations() {
    for (fixture, count) in [(UNKNOWN, 1), (DIRECT, 2)] {
        let (response, events) = project(projector(), fixture);
        let diagnostics: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::Diagnostic(d) => Some(d.message().as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            diagnostics,
            [format!(
                "Messages skipped {count} unfamiliar informational content block(s)."
            )]
        );
        assert!(!format!("{events:?}").contains("synthetic private"));
        if fixture == DIRECT {
            assert!(
                matches!(&response.output()[0].blocks()[0], ContentBlock::ToolCall(call) if call.raw_arguments() == r#"{"path":"fixture.txt"}"#)
            );
        } else {
            assert!(
                matches!(&response.output()[0].blocks()[0], ContentBlock::Text(text) if text.text() == "Answer retained.")
            );
        }
    }
}

#[test]
fn unknown_blocks_obey_byte_limits_and_protected_tool_lineage() {
    let mut bounded = projector();
    bounded.limits = super::super::MessagesStreamLimits::new(
        4096.try_into().expect("sanitized fixture"),
        8192.try_into().expect("sanitized fixture"),
        100.try_into().expect("sanitized fixture"),
        10.try_into().expect("sanitized fixture"),
        4096.try_into().expect("sanitized fixture"),
        32768.try_into().expect("sanitized fixture"),
        16.try_into().expect("sanitized fixture"),
    );
    assert!(
        bounded
            .push(UNKNOWN.as_bytes())
            .expect_err("protected fixture refuses")
            .message()
            .contains("byte limit")
    );
    for block in [
        r#"{"type":"future_tool_result","tool_use_id":"unknown","content":[]}"#,
        r#"{"type":"future_status","caller":{"type":"future"}}"#,
    ] {
        let stream = UNKNOWN.replace(
            r#"{"type":"future_status","status":"synthetic private status"}"#,
            block,
        );
        assert!(
            projector()
                .push(stream.as_bytes())
                .expect_err("protected fixture refuses")
                .message()
                .contains("hosted tool lineage")
        );
    }
    let stream = UNKNOWN.replace(
        r#"{"type":"future_status_delta","status":"synthetic private update"}"#,
        r#"{"type":"signature_delta","signature":"synthetic-signature"}"#,
    );
    assert!(projector().push(stream.as_bytes()).is_err());
    let mut hosted = projector_with_hosted(true);
    assert!(
        hosted
            .push(include_bytes!(
                "../../../tests/fixtures/stream_unknown_hosted.sse"
            ))
            .expect_err("protected fixture refuses")
            .message()
            .contains("hosted tool lineage")
    );
    assert!(hosted.hosted_effects_possible());
}

#[test]
fn unknown_only_output_retains_a_content_free_nonreplayed_skip_marker() {
    let retained = UNKNOWN
        .find("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1")
        .expect("text fixture block");
    let terminal = UNKNOWN
        .find("event: message_delta")
        .expect("fixture terminal");
    let stream = format!("{}{}", &UNKNOWN[..retained], &UNKNOWN[terminal..]);
    let (response, events) = project(projector(), &stream);
    assert!(
        matches!(response.output()[0].blocks(), [ContentBlock::Opaque(block)] if block.fallback() == &plakous_model::OpaqueFallback::Omit)
    );
    assert_eq!(replay(&response, &projector().prepared.request_policy), []);
    let ContentBlock::Opaque(marker) = &response.output()[0].blocks()[0] else {
        panic!("skip marker")
    };
    assert!(
        !marker
            .replay()
            .payload()
            .get()
            .contains("synthetic private")
    );
    assert!(!marker.replay().payload().get().contains("future_status"));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ModelEvent::Diagnostic(_)))
            .count(),
        1
    );
}

#[test]
fn excluded_content_blocks_do_not_take_the_informational_skip_path() {
    for kind in [
        "fallback",
        "model_fallback",
        "multimodal",
        "image",
        "document",
        "audio",
        "video",
        "structured_output",
        "server_tools",
    ] {
        let stream = UNKNOWN.replace("future_status\"", &format!("{kind}\""));
        for mut parser in [
            projector(),
            claude_oauth_projector(),
            openrouter_projector(),
        ] {
            let error = parser
                .push(stream.as_bytes())
                .expect_err("excluded feature");
            assert_eq!(error.kind(), MessagesStreamErrorKind::UnsupportedSemantic);
            assert!(error.message().contains("excluded"));
        }
    }
}

#[test]
fn data_only_unfamiliar_blocks_preserve_redacted_bytes() {
    for raw in [
        r#"{"type":"future_note","data":"synthetic\u002dredacted"}"#,
        r#"{"type":"future_note","nested":{"data":"synthetic\u002dredacted"}}"#,
    ] {
        let stream = UNKNOWN
            .replace(
                r#"{"type":"future_status","status":"synthetic private status"}"#,
                raw,
            )
            .lines()
            .filter(|line| {
                !line.contains("content_block_delta") && !line.contains("future_status_delta")
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n\n";
        let (response, _) = project(projector(), &stream);
        assert!(
            matches!(&replay(&response, &projector().prepared.request_policy)[0], MessageBlock::Native { block } if block.get() == raw)
        );
    }
}

#[test]
fn complete_unknown_signed_block_replays_exactly_but_unrecognized_deltas_fail_closed() {
    let raw = r#"{"type":"future_signed","signature":"synthetic\u002dsignature","data":{"v":1}}"#;
    let stream = UNKNOWN.replace(
        r#"{"type":"future_status","status":"synthetic private status"}"#,
        raw,
    );
    let mut parser = projector();
    assert!(
        parser.push(stream.as_bytes()).is_err(),
        "unknown signed deltas cannot be reconstructed"
    );
    let stream = stream
        .lines()
        .filter(|line| {
            !line.contains("content_block_delta") && !line.contains("future_status_delta")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n\n";
    let policy = projector().prepared.request_policy.clone();
    let (response, _) = project(projector(), &stream);
    assert!(
        matches!(&replay(&response, &policy)[0], MessageBlock::Native { block } if block.get() == raw)
    );
}

#[test]
fn native_citation_replay_refuses_target_text_and_binding_mutations() {
    let policy = projector().prepared.request_policy.clone();
    let (response, _) = project(projector(), INITIAL);
    let blocks = response.output()[0].blocks();
    let ContentBlock::Opaque(opaque) = blocks.last().expect("sanitized fixture") else {
        panic!("replay")
    };
    let mut changed = blocks.to_vec();
    changed[0] = ContentBlock::Text(
        plakous_model::TextBlock::new("changed", None).expect("sanitized fixture"),
    );
    assert!(crate::request::native_replay_block(opaque, &policy, &changed).is_err());
    let mut duplicate = blocks.to_vec();
    duplicate.push(ContentBlock::Opaque(opaque.clone()));
    assert!(
        translate_assistant_bound(
            &duplicate,
            &policy,
            &ToolOwnership::default(),
            false,
            true,
            AssistantReplayState {
                container: &mut None,
                server_lineage: &mut crate::artifact::MessagesArtifactLineage::default(),
                diagnostics: &mut Vec::new()
            }
        )
        .is_err()
    );
    let original = opaque.replay();
    let foreign = plakous_model::ReplayEnvelope::new(
        ProtocolId::new("foreign.messages").expect("sanitized fixture"),
        original.model_family().cloned(),
        original.scope().clone(),
        original.kind().clone(),
        original.version(),
        original.classification(),
        serde_json::value::RawValue::from_string(original.payload().get().to_owned())
            .expect("sanitized fixture"),
    )
    .expect("sanitized fixture");
    let foreign =
        plakous_model::OpaqueBlock::new(foreign, plakous_model::OpaqueFallback::CompatibleReplay)
            .expect("sanitized fixture");
    assert!(crate::request::native_replay_block(&foreign, &policy, blocks).is_err());
}
