use super::*;
use plakous_model::{
    Actor, ContentBlock, ConversationItem, ConversationView, EntryId, Provenance, TextBlock,
};
use uuid::Uuid;

fn digest(domain: DigestDomain, label: &[u8]) -> Digest {
    Digest::sha256(domain, 1, &[label]).expect("digest")
}

fn artifact() -> MessagesFamilyArtifact {
    let item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(91)).expect("entry"),
        Actor::Assistant,
        vec![ContentBlock::Text(
            TextBlock::new("fixture", None).expect("text"),
        )],
        Provenance::ModelOutput,
    )
    .expect("item");
    let covered = ConversationView::from_items(vec![item])
        .expect("test conversation items form one valid segment")
        .canonical_item_prefix_identity(1)
        .expect("prefix");
    MessagesFamilyArtifact::new(MessagesArtifactProduction {
        binding: digest(DigestDomain::ModelBinding, b"binding"),
        producer_plan: digest(DigestDomain::RequestPlan, b"plan"),
        tool_set: digest(DigestDomain::Blob, b"tools"),
        configuration: digest(DigestDomain::Blob, b"configuration"),
        covered,
        container: "container_1".to_owned(),
        prior_lineage: MessagesArtifactLineage::default(),
        assistant_blocks: vec![
            MessageBlock::ServerToolUse {
                id: "srv_1".to_owned(),
                name: "code_execution".to_owned(),
                input: serde_json::json!({"code":"weather()"}),
            },
            MessageBlock::ToolUse {
                id: "tool_1".to_owned(),
                name: "weather".to_owned(),
                input: serde_json::json!({}),
                caller: Some(crate::request::ToolCallerParam {
                    kind: "code_execution_20260120".to_owned(),
                    tool_id: Some("srv_1".to_owned()),
                }),
            },
        ],
        stop: MessagesArtifactStop {
            reason: "tool_use".to_owned(),
            sequence: None,
            details: None,
        },
    })
    .expect("artifact")
}

fn mutate(value: &serde_json::Value) -> Result<MessagesFamilyArtifact, MessagesArtifactError> {
    let mut bytes = b"PLAKMSG\0".to_vec();
    bytes.extend(serde_json::to_vec(&value).expect("json"));
    MessagesFamilyArtifact::decode(&bytes)
}

fn mixed_direct_artifact() -> MessagesFamilyArtifact {
    let mut value = serde_json::to_value(artifact()).expect("value");
    let blocks = value["assistant_blocks"].as_array_mut().expect("blocks");
    blocks.push(serde_json::json!({
        "type": "tool_use",
        "id": "tool_explicit",
        "name": "clock",
        "input": {},
        "caller": {"type": "direct"}
    }));
    blocks.push(serde_json::json!({
        "type": "tool_use",
        "id": "tool_legacy",
        "name": "calendar",
        "input": {}
    }));
    mutate(&value).expect("mixed direct artifact")
}

#[test]
fn hosted_artifact_roundtrips_without_expiry_or_secret_state() {
    let artifact = artifact();
    let bytes = artifact.encode().expect("encode");
    assert_eq!(
        MessagesFamilyArtifact::decode(&bytes).expect("decode"),
        artifact
    );
    let json = String::from_utf8(bytes).expect("utf8");
    assert!(!json.contains("expires_at"));
    assert!(!json.contains("authorization"));
}

#[test]
fn mixed_programmatic_explicit_direct_and_legacy_absent_callers_have_exact_golden_replay() {
    let artifact = mixed_direct_artifact();
    let blocks = serde_json::to_string(artifact.native_blocks()).expect("blocks json");
    assert_eq!(
        blocks,
        concat!(
            r#"[{"type":"server_tool_use","id":"srv_1","name":"code_execution","input":{"code":"weather()"}},"#,
            r#"{"type":"tool_use","id":"tool_1","name":"weather","input":{},"caller":{"type":"code_execution_20260120","tool_id":"srv_1"}},"#,
            r#"{"type":"tool_use","id":"tool_explicit","name":"clock","input":{},"caller":{"type":"direct"}},"#,
            r#"{"type":"tool_use","id":"tool_legacy","name":"calendar","input":{}}]"#
        )
    );

    let reopened =
        MessagesFamilyArtifact::decode(&artifact.encode().expect("encode")).expect("decode");
    assert_eq!(reopened.native_blocks(), artifact.native_blocks());

    let mut canonical = artifact.native_blocks().to_vec();
    let MessageBlock::ToolUse { caller, .. } = &mut canonical[2] else {
        panic!("explicit direct fixture block")
    };
    *caller = None;
    assert!(native_blocks_match_canonical(
        artifact.native_blocks(),
        &canonical
    ));
    assert_ne!(
        serde_json::to_vec(artifact.native_blocks()).expect("native json"),
        serde_json::to_vec(&canonical).expect("canonical json")
    );
}

#[test]
fn direct_caller_one_field_mutations_do_not_collapse_into_legacy_absence() {
    let artifact = mixed_direct_artifact();
    let value = serde_json::to_value(&artifact).expect("value");

    let mut invented_tool_id = value.clone();
    invented_tool_id["assistant_blocks"][2]["caller"]["tool_id"] = serde_json::json!("srv_1");
    assert_eq!(
        mutate(&invented_tool_id),
        Err(MessagesArtifactError::InvalidBlocks)
    );

    let mut missing_programmatic_tool_id = value.clone();
    missing_programmatic_tool_id["assistant_blocks"][1]["caller"]
        .as_object_mut()
        .expect("caller")
        .remove("tool_id");
    assert_eq!(
        mutate(&missing_programmatic_tool_id),
        Err(MessagesArtifactError::InvalidBlocks)
    );

    let mut legacy_absent = value;
    legacy_absent["assistant_blocks"][2]
        .as_object_mut()
        .expect("tool block")
        .remove("caller");
    let absent = mutate(&legacy_absent).expect("legacy absent is independently valid");
    assert_ne!(
        artifact.encode().expect("explicit encode"),
        absent.encode().expect("absent encode")
    );
    assert_ne!(artifact.native_blocks(), absent.native_blocks());
}

#[test]
fn hosted_artifact_rejects_container_order_caller_revision_and_binding_mutations() {
    let value = serde_json::to_value(artifact()).expect("value");
    let mut missing_container = value.clone();
    missing_container["container"] = serde_json::json!("");
    assert_eq!(
        mutate(&missing_container),
        Err(MessagesArtifactError::InvalidContainer)
    );

    let mut order = value.clone();
    order["assistant_blocks"]
        .as_array_mut()
        .expect("blocks")
        .swap(0, 1);
    assert_eq!(mutate(&order), Err(MessagesArtifactError::InvalidBlocks));

    let mut caller = value.clone();
    caller["assistant_blocks"][1]["caller"]["tool_id"] = serde_json::json!("srv_wrong");
    assert_eq!(mutate(&caller), Err(MessagesArtifactError::InvalidBlocks));

    let mut revision = value.clone();
    revision["assistant_blocks"][1]["caller"]["type"] =
        serde_json::json!("code_execution_20260521");
    assert_eq!(mutate(&revision), Err(MessagesArtifactError::InvalidBlocks));

    let active = ActiveFamilyArtifact::new(
        FAMILY,
        MESSAGES_ARTIFACT_SCHEMA_REVISION,
        MESSAGES_ARTIFACT_CODEC_REVISION,
        digest(DigestDomain::ModelBinding, b"wrong-binding"),
        Arc::<[u8]>::from(artifact().encode().expect("encode")),
    )
    .expect("active");
    assert_eq!(
        MessagesFamilyArtifact::from_active(
            &active,
            digest(DigestDomain::ModelBinding, b"binding")
        ),
        Err(MessagesArtifactError::BindingMismatch)
    );
}

#[test]
fn terminal_hosted_artifact_is_active_replay_authority_not_continuation_state() {
    let mut value = serde_json::to_value(artifact()).expect("value");
    value["assistant_blocks"]
        .as_array_mut()
        .expect("blocks")
        .pop();
    value["stop"]["reason"] = serde_json::json!("end_turn");
    let terminal = mutate(&value).expect("terminal hosted artifact");
    assert!(!terminal.requires_continuation());

    let active = ActiveFamilyArtifact::new(
        FAMILY,
        MESSAGES_ARTIFACT_SCHEMA_REVISION,
        MESSAGES_ARTIFACT_CODEC_REVISION,
        digest(DigestDomain::ModelBinding, b"binding"),
        Arc::<[u8]>::from(terminal.encode().expect("encode")),
    )
    .expect("active terminal artifact");
    let reopened = MessagesFamilyArtifact::from_active(
        &active,
        digest(DigestDomain::ModelBinding, b"binding"),
    )
    .expect("active terminal replay");
    assert_eq!(reopened, terminal);
    assert!(!reopened.requires_continuation());
}

#[test]
fn lineage_enforces_cross_round_result_uniqueness_and_exact_order() {
    let mut first_pause = MessagesArtifactLineage::default();
    first_pause.push_server("srv_1").expect("first server");
    assert!(first_pause.is_unresolved("srv_1"));

    let mut second_pause = first_pause.clone();
    second_pause
        .complete_next("srv_1")
        .expect("prior server completes first");
    second_pause.push_server("srv_2").expect("second server");
    assert!(second_pause.complete_next("srv_1").is_err());

    second_pause.push_server("srv_3").expect("third server");
    assert!(second_pause.complete_next("srv_3").is_err());
    second_pause
        .complete_next("srv_2")
        .expect("results preserve server order");
    second_pause
        .complete_next("srv_3")
        .expect("final result completes once");
    assert!(second_pause.complete_next("srv_3").is_err());
}
