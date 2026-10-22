use super::*;

fn replay() -> ReplayEnvelope {
    ReplayEnvelope::new(
        ProtocolId::new("carry.protocol").unwrap(),
        None,
        ReplayScope::ExactModel(ModelId::new("carry.model").unwrap()),
        ReplayKindId::new("carry.reasoning").unwrap(),
        1,
        DataClassification::Sensitive,
        serde_json::value::RawValue::from_string("\"sanitized-opaque\"".to_owned()).unwrap(),
    )
    .unwrap()
}
fn entry(id: u128, actor: Actor, blocks: Vec<ContentBlock>) -> ConversationItem {
    ConversationItem::new(
        EntryId::try_from(uuid::Uuid::from_u128(id)).unwrap(),
        actor,
        blocks,
        if actor == Actor::User {
            Provenance::UserInput
        } else {
            Provenance::ModelOutput
        },
    )
    .unwrap()
}
fn blocks() -> Vec<ContentBlock> {
    let call = CallId::try_from(uuid::Uuid::from_u128(3)).unwrap();
    let owner = ToolExecutionOwner::Mcp {
        server: McpServerId::new("docs").unwrap(),
    };
    let correlation = ToolCorrelation::new(
        Some("remote".into()),
        Some("container".into()),
        ToolCallerLineage::provider_programmatic(
            NamespaceId::new("hosted.program").unwrap(),
            "program-1",
        )
        .unwrap(),
    )
    .unwrap();
    let text = TextBlock::new("tool contents", Some(TextPhase::Commentary)).unwrap();
    let image = ImageBlock::with_detail(
        "image/png",
        MediaSource::bytes(vec![1, 2, 3, 4]).unwrap(),
        Some(ImageDetailHint::Original),
    )
    .unwrap();
    let binding = CompactionReplayBinding::new(
        ProtocolId::new("carry.protocol").unwrap(),
        Some(ModelFamilyId::new("carry.family").unwrap()),
        ModelId::new("carry.model").unwrap(),
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"carry-account"]).unwrap(),
    );
    vec![
        ContentBlock::Text(text.clone()),
        ContentBlock::Image(image.clone()),
        ContentBlock::Document(
            DocumentBlock::new("application/pdf", MediaSource::bytes(vec![5, 6]).unwrap()).unwrap(),
        ),
        ContentBlock::Audio(
            AudioBlock::new("audio/wav", MediaSource::bytes(vec![7, 8]).unwrap()).unwrap(),
        ),
        ContentBlock::ToolCall(
            ToolCallBlock::new(
                call,
                ToolName::new("search").unwrap(),
                owner.clone(),
                ToolCallState::Requested,
                correlation.clone(),
                "{\"q\":\"carry\"}",
            )
            .unwrap()
            .with_canonical_name(ToolName::new("mcp__docs__search").unwrap()),
        ),
        ContentBlock::ToolResult(
            ToolResultBlock::new(
                call,
                owner.clone(),
                ToolResultKind::Success,
                vec![
                    ToolResultPart::Text(text.clone()),
                    ToolResultPart::Image(image),
                    ToolResultPart::Opaque(
                        OpaqueBlock::new(
                            replay(),
                            OpaqueFallback::SemanticSummary("result fallback".into()),
                        )
                        .unwrap(),
                    ),
                ],
                correlation.clone(),
            )
            .unwrap(),
        ),
        ContentBlock::ToolResultUpdate(
            ToolResultUpdateBlock::new(call, owner, text, correlation).unwrap(),
        ),
        ContentBlock::Reasoning(
            ReasoningBlock::new(
                ReasoningVisibility::Summary,
                Some("visible summary".into()),
                Some(replay()),
            )
            .unwrap(),
        ),
        ContentBlock::Refusal(RefusalBlock::new("refusal").unwrap()),
        ContentBlock::Citation(CitationBlock::new(
            CitationTarget::new(EntryId::try_from(uuid::Uuid::from_u128(1)).unwrap(), 0),
            CitationSource::new(
                "source",
                "annotation",
                Some("https://example.com".into()),
                Some("title".into()),
            )
            .unwrap(),
            Some(CitationRange::new(0, 3, CitationUnit::Utf8Byte).unwrap()),
        )),
        ContentBlock::Compaction(
            CompactionBlock::new("summary")
                .unwrap()
                .with_summarized_turns(SummarizedTurns::new(3, 4).unwrap())
                .with_native_item("sanitized-native-item".into())
                .with_native_binding(binding),
        ),
        ContentBlock::Opaque(
            OpaqueBlock::new(replay(), OpaqueFallback::SemanticSummary("fallback".into())).unwrap(),
        ),
    ]
}

#[test]
fn every_neutral_block_round_trips_without_runtime_serde() {
    for original in blocks() {
        let encoded = serde_json::to_vec(&Block::export(&original).unwrap()).unwrap();
        let decoded: Block = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.import().unwrap(), original);
    }
    for source in [
        MediaSource::uri("https://example.com/image.png").unwrap(),
        MediaSource::data_url("data:image/png;base64,AQID").unwrap(),
    ] {
        assert_eq!(Source::export(&source).unwrap().import().unwrap(), source);
    }
    let lineage = ToolCallerLineage::local_programmatic(
        1,
        2,
        "cell",
        3,
        CallId::try_from(uuid::Uuid::from_u128(4)).unwrap(),
        1,
    )
    .unwrap();
    let correlation = ToolCorrelation::new(None, None, lineage).unwrap();
    assert_eq!(
        Correlation::export(&correlation).import().unwrap(),
        correlation
    );
}

#[test]
fn full_neutral_history_is_equal_and_has_no_canonical_identity() {
    let all = blocks();
    let history = PortableSessionHistory::new(
        vec![
            HistorySegment::new(
                HistorySegmentKind::Compaction,
                vec![entry(10, Actor::Assistant, vec![all[10].clone()])],
            )
            .unwrap(),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![entry(
                    11,
                    Actor::User,
                    vec![
                        all[0].clone(),
                        all[1].clone(),
                        all[2].clone(),
                        all[3].clone(),
                    ],
                )],
            )
            .unwrap(),
            HistorySegment::new(
                HistorySegmentKind::ReasoningReplay,
                vec![entry(
                    12,
                    Actor::Assistant,
                    vec![all[7].clone(), all[11].clone()],
                )],
            )
            .unwrap(),
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![
                    entry(13, Actor::Assistant, vec![all[4].clone()]),
                    entry(14, Actor::Tool, vec![all[6].clone()]),
                    entry(15, Actor::Tool, vec![all[5].clone()]),
                ],
            )
            .unwrap(),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![entry(
                    16,
                    Actor::Assistant,
                    vec![all[0].clone(), all[8].clone()],
                )],
            )
            .unwrap(),
        ],
        "00ff".into(),
        ModelId::new("carry.model").unwrap(),
    );
    let neutral = History::export(&history).unwrap();
    let value = serde_json::to_value(&neutral).unwrap();
    assert!(value.get("identity").is_none());
    assert!(value["groups"][0].get("canonical_bytes").is_none());
    assert_eq!(
        serde_json::from_value::<History>(value)
            .unwrap()
            .import()
            .unwrap(),
        history
    );
    assert!(!format!("{neutral:?}").contains("sanitized"));
}

#[test]
fn malformed_and_oversized_neutral_history_refuses() {
    for block in [
        serde_json::json!({"kind":"text","text":"hello","phase":null,"unexpected":true}),
        serde_json::json!({"kind":"future_block"}),
        serde_json::json!({"kind":"image","mime":"image/png","detail":null,"source":{"kind":"bytes","bytes":[256]}}),
    ] {
        assert!(serde_json::from_value::<Block>(block).is_err());
    }
    let bad = serde_json::json!({"replay_account":"00ff","replay_model":"carry.model","groups":[{"kind":"conversation","messages":[{"id":"not-a-uuid","actor":"user","origin":"user_input","blocks":[{"kind":"text","text":"hello","phase":null}]}]}]});
    assert!(
        serde_json::from_value::<History>(bad)
            .unwrap()
            .import()
            .is_err()
    );
    assert!(
        Source::Bytes {
            bytes: base64::engine::general_purpose::STANDARD
                .encode(vec![0_u8; 16 * 1024 * 1024 + 1])
        }
        .import()
        .is_err()
    );
    let huge = History {
        replay_account: "00ff".into(),
        replay_model: "carry.model".into(),
        groups: vec![Group {
            kind: "conversation".into(),
            messages: (0..100_001)
                .map(|_| Message {
                    id: String::new(),
                    actor: String::new(),
                    origin: String::new(),
                    presentation_subject: None,
                    blocks: Vec::new(),
                })
                .collect(),
        }],
    };
    assert!(matches!(
        huge.validate_size(),
        Err(ImportError::Oversized {
            section: "history messages",
            ..
        })
    ));
}

#[tokio::test]
async fn neutral_seed_preserves_same_binding_and_strips_a_different_provider() {
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
        SessionOperationId,
    };
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, text_runtime_binding_for_target};
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("sessions.sqlite");
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap();
    let binding = |account: &[u8], protocol: &str| {
        text_runtime_binding_for_target(
            root.path(),
            BoundaryController::quiet_from_evidence_path(
                CrashBoundary::None,
                root.path().join("evidence"),
            )
            .unwrap(),
            account,
            protocol,
        )
        .unwrap()
    };
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("source").unwrap(),
            binding(b"provider-a/account", "provider-a.protocol"),
        ))
        .await
        .unwrap()
    else {
        panic!("create outcome");
    };
    let locator = session.release().await.unwrap();
    let source = AgentSessions::export_history_sqlite(&database, locator).unwrap();
    let seed_replay = || {
        ReplayEnvelope::new(
            ProtocolId::new("provider-a.protocol").unwrap(),
            None,
            ReplayScope::ExactModel(source.replay_model().clone()),
            ReplayKindId::new("carry.reasoning").unwrap(),
            1,
            DataClassification::Sensitive,
            serde_json::value::RawValue::from_string("\"sanitized-seed\"".to_owned()).unwrap(),
        )
        .unwrap()
    };
    let carried = PortableSessionHistory::new(
        vec![
            HistorySegment::new(
                HistorySegmentKind::Compaction,
                vec![entry(
                    10,
                    Actor::Assistant,
                    vec![ContentBlock::Compaction(
                        CompactionBlock::new("sanitized-native-summary")
                            .unwrap()
                            .with_native_item("sanitized-native-replay".into())
                            .with_summarized_turns(SummarizedTurns::new(3, 4).unwrap())
                            .with_native_binding(CompactionReplayBinding::new(
                                ProtocolId::new("provider-a.protocol").unwrap(),
                                Some(ModelFamilyId::new("offline.loopback.responses").unwrap()),
                                source.replay_model().clone(),
                                Digest::sha256(
                                    DigestDomain::ModelBinding,
                                    1,
                                    &[b"provider-a/account"],
                                )
                                .unwrap(),
                            )),
                    )],
                )],
            )
            .unwrap(),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![entry(
                    1,
                    Actor::User,
                    vec![ContentBlock::Text(
                        TextBlock::new("original goal", None).unwrap(),
                    )],
                )],
            )
            .unwrap(),
            HistorySegment::new(
                HistorySegmentKind::ReasoningReplay,
                vec![entry(
                    2,
                    Actor::Assistant,
                    vec![
                        ContentBlock::Reasoning(
                            ReasoningBlock::new(
                                ReasoningVisibility::Summary,
                                Some("visible summary".into()),
                                Some(seed_replay()),
                            )
                            .unwrap(),
                        ),
                        ContentBlock::Reasoning(
                            ReasoningBlock::new(
                                ReasoningVisibility::OpaqueReplay,
                                None,
                                Some(seed_replay()),
                            )
                            .unwrap(),
                        ),
                        ContentBlock::Opaque(
                            OpaqueBlock::new(
                                seed_replay(),
                                OpaqueFallback::SemanticSummary("portable fallback".into()),
                            )
                            .unwrap(),
                        ),
                    ],
                )],
            )
            .unwrap(),
        ],
        source.replay_account().to_owned(),
        source.replay_model().clone(),
    );
    for (name, account, protocol, same) in [
        (
            "same",
            b"provider-a/account".as_slice(),
            "provider-a.protocol",
            true,
        ),
        (
            "another-provider",
            b"provider-b/account".as_slice(),
            "provider-b.protocol",
            false,
        ),
    ] {
        let neutral = History::export(&carried).unwrap();
        let decoded: History =
            serde_json::from_slice(&serde_json::to_vec(&neutral).unwrap()).unwrap();
        let history = decoded.import().unwrap();
        let runtime = binding(account, protocol);
        assert_eq!(history.retains_replay_for(&runtime), same);
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(
                CreateSessionRequest::new(SessionOperationId::new(name).unwrap(), runtime)
                    .with_history(history),
            )
            .await
            .unwrap()
        else {
            panic!("create outcome");
        };
        let locator = session.release().await.unwrap();
        let exported = AgentSessions::export_history_sqlite(&database, locator).unwrap();
        if same {
            assert_eq!(exported.segments(), carried.segments());
        } else {
            assert_eq!(exported.segments()[1], carried.segments()[1]);
            assert!(
                matches!(&exported.segments()[0].items()[0].blocks()[0],ContentBlock::Compaction(c)
                if c.native_item().is_none() && c.summary().contains("cannot be carried") && c.summarized_turns()==SummarizedTurns::new(3,4))
            );
            let remaining = exported.segments()[2].items()[0].blocks();
            assert_eq!(remaining.len(), 2);
            assert!(
                matches!(&remaining[0],ContentBlock::Reasoning(r) if r.text()==Some("visible summary") && r.replay().is_none())
            );
            assert!(matches!(&remaining[1],ContentBlock::Text(t) if t.text()=="portable fallback"));
        }
    }
}

#[test]
fn closed_unit_variants_refuse_extra_fields_and_media_urls_are_https_only() {
    assert!(serde_json::from_str::<Owner>(r#"{"kind":"local","extra":1}"#).is_err());
    assert!(serde_json::from_str::<Caller>(r#"{"kind":"direct","extra":1}"#).is_err());
    for kind in ["omit", "compatible_replay"] {
        assert!(
            serde_json::from_value::<Fallback>(serde_json::json!({"kind":kind,"extra":1})).is_err()
        );
    }
    for uri in [
        "file:///etc/passwd",
        "http://example.com/image.png",
        "javascript:alert(1)",
    ] {
        assert!(
            Source::Uri {
                uri: uri.to_owned()
            }
            .import()
            .is_err()
        );
    }
    assert!(
        Source::Uri {
            uri: "https://example.com/image.png".to_owned()
        }
        .import()
        .is_ok()
    );
    let source = Source::export(&MediaSource::bytes(vec![1, 2, 3]).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(source).unwrap(),
        serde_json::json!({"kind":"bytes","bytes":"AQID"})
    );
}

#[test]
fn parse_enforces_global_messages_and_per_message_blocks() {
    let message = serde_json::json!({"id":"00000000-0000-0000-0000-000000000001","actor":"user","origin":"user_input","presentation_subject":null,"blocks":[{"kind":"text","text":"x","phase":null}]});
    let history = serde_json::json!({"replay_account":"00ff","replay_model":"model","groups":[{"kind":"conversation","messages":vec![message.clone();50_001]},{"kind":"conversation","messages":vec![message.clone();50_000]}]});
    assert!(serde_json::from_value::<History>(history).is_err());
    let mut too_many_blocks = message;
    too_many_blocks["blocks"] = serde_json::json!(vec![
        serde_json::json!({"kind":"text","text":"x","phase":null});
        4097
    ]);
    assert!(serde_json::from_value::<Message>(too_many_blocks).is_err());
}

#[test]
fn neutral_history_preserves_a_bounded_presentation_subject() {
    let original = entry(
        123,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new("visible", None).unwrap())],
    )
    .with_presentation_subject(Some("reader subject"));
    let history = PortableSessionHistory::new(
        vec![HistorySegment::new(HistorySegmentKind::Conversation, vec![original]).unwrap()],
        "00ff".into(),
        ModelId::new("model").unwrap(),
    );
    let encoded = serde_json::to_vec(&History::export(&history).unwrap()).unwrap();
    assert_eq!(
        serde_json::from_slice::<History>(&encoded)
            .unwrap()
            .import()
            .unwrap(),
        history
    );
}
