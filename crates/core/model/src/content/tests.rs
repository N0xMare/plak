use super::*;
use crate::{
    CallId, Digest, DigestDomain, ModelFamilyId, ModelId, NamespaceId, ProtocolId, ReplayKindId,
};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use uuid::Uuid;

#[derive(Deserialize)]
struct CanonicalItemFixture {
    schema: String,
    version: u32,
    value: ConversationItem,
}

fn id(value: u128) -> Result<EntryId, crate::IdentityError> {
    EntryId::try_from(Uuid::from_u128(value))
}

#[test]
fn media_bytes_serialize_as_strict_canonical_base64() -> Result<(), Box<dyn std::error::Error>> {
    let source = MediaSource::bytes(vec![0xfb, 0xff, 0x00, 0x3e])?;
    let json = serde_json::to_string(&source)?;
    assert_eq!(json, r#"{"kind":{"Bytes":"+/8APg=="}}"#);
    assert_eq!(serde_json::from_str::<MediaSource>(&json)?, source);
    for refused in [
        r#"{"kind":{"Bytes":"+/8APg"}}"#,
        r#"{"kind":{"Bytes":"-_8APg=="}}"#,
        r#"{"kind":{"Bytes":"+/8A Pg=="}}"#,
        r#"{"kind":{"Bytes":"+/8APg==\n"}}"#,
        r#"{"kind":{"Bytes":"+/8APh=="}}"#,
        r#"{"kind":{"Bytes":"+/8APg==="}}"#,
        r#"{"kind":{"Bytes":""}}"#,
        r#"{"kind":{"Bytes":[251,255,0,62]}}"#,
    ] {
        assert!(
            serde_json::from_str::<MediaSource>(refused).is_err(),
            "accepted {refused}"
        );
    }
    let oversized = format!(
        r#"{{"kind":{{"Bytes":"{}"}}}}"#,
        "A".repeat(MAX_CANONICAL_BLOCK_BYTES.div_ceil(3) * 4 + 4)
    );
    assert!(serde_json::from_str::<MediaSource>(&oversized).is_err());
    Ok(())
}

#[test]
fn mixed_block_order_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let item = ConversationItem::new(
        id(1)?,
        Actor::Assistant,
        vec![
            ContentBlock::Text(TextBlock::new("first", Some(TextPhase::Commentary))?),
            ContentBlock::Reasoning(ReasoningBlock::new(
                ReasoningVisibility::Summary,
                Some(String::from("summary")),
                None,
            )?),
            ContentBlock::Refusal(RefusalBlock::new("cannot")?),
        ],
        Provenance::ModelOutput,
    )?;
    let json = serde_json::to_string(&item)?;
    let decoded: ConversationItem = serde_json::from_str(&json)?;
    assert_eq!(decoded, item);
    Ok(())
}

#[test]
fn account_scoped_replay_cannot_cross_accounts_or_be_retagged()
-> Result<(), Box<dyn std::error::Error>> {
    let protocol = ProtocolId::new("scoped.protocol")?;
    let model = ModelId::new("scoped-model")?;
    let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account-A"])?;
    let other = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account-B"])?;
    let envelope = ReplayEnvelope::new(
        protocol.clone(),
        None,
        ReplayScope::ExactAccountModel {
            model: model.clone(),
            account,
        },
        ReplayKindId::new("scoped.output")?,
        1,
        DataClassification::Sensitive,
        RawValue::from_string(r#"{"encrypted_content":"synthetic"}"#.to_owned())?,
    )?;
    assert!(envelope.is_compatible(&protocol, None, &model, Some(account)));
    assert!(!envelope.is_compatible(&protocol, None, &model, Some(other)));
    let other_version = Digest::from_sha256_bytes(
        DigestDomain::ModelBinding,
        account.version() + 1,
        *account.bytes(),
    )?;
    assert!(!envelope.is_compatible(&protocol, None, &model, Some(other_version)));
    assert!(!envelope.is_compatible(&protocol, None, &model, None));
    assert!(!envelope.is_compatible(
        &protocol,
        None,
        &ModelId::new("other-model")?,
        Some(account)
    ));
    let mut tampered = serde_json::to_value(&envelope)?;
    tampered["scope"]["ExactAccountModel"]["account"] = serde_json::to_value(other)?;
    assert!(serde_json::from_value::<ReplayEnvelope>(tampered).is_err());
    let legacy = ReplayEnvelope::new(
        protocol.clone(),
        None,
        ReplayScope::ExactModel(model.clone()),
        envelope.kind().clone(),
        1,
        DataClassification::Sensitive,
        RawValue::from_string(envelope.payload().get().to_owned())?,
    )?;
    assert!(!legacy.is_compatible(&protocol, None, &model, Some(account)));
    assert!(legacy.is_compatible(&protocol, None, &model, None));
    Ok(())
}

#[test]
fn replay_debug_redacts_exact_payload() -> Result<(), Box<dyn std::error::Error>> {
    let payload = RawValue::from_string(String::from("{\"secret\":\"do-not-log\"}"))?;
    let envelope = ReplayEnvelope::new(
        ProtocolId::new("test-protocol")?,
        Some(ModelFamilyId::new("test-family")?),
        ReplayScope::ModelFamily,
        ReplayKindId::new("state")?,
        1,
        DataClassification::Sensitive,
        payload,
    )?;
    let debug = format!("{envelope:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("do-not-log"));
    assert!(envelope.is_compatible(
        &ProtocolId::new("test-protocol")?,
        Some(&ModelFamilyId::new("test-family")?),
        &ModelId::new("test-model")?,
        None
    ));
    Ok(())
}

#[test]
fn replay_deserialization_verifies_digest() -> Result<(), Box<dyn std::error::Error>> {
    let payload = RawValue::from_string(String::from("{\"value\":1}"))?;
    let envelope = ReplayEnvelope::new(
        ProtocolId::new("test-protocol")?,
        None,
        ReplayScope::Protocol,
        ReplayKindId::new("state")?,
        1,
        DataClassification::Sensitive,
        payload,
    )?;
    let mut value = serde_json::to_value(envelope)?;
    value["payload"] = serde_json::json!({"value": 2});
    assert!(serde_json::from_value::<ReplayEnvelope>(value).is_err());
    Ok(())
}

#[test]
fn replay_scope_discriminant_is_bound_into_digest() -> Result<(), Box<dyn std::error::Error>> {
    let protocol = ProtocolId::new("test-protocol")?;
    let kind = ReplayKindId::new("state")?;
    let protocol_scoped = ReplayEnvelope::new(
        protocol.clone(),
        None,
        ReplayScope::Protocol,
        kind.clone(),
        1,
        DataClassification::Sensitive,
        RawValue::from_string(String::from("{\"value\":1}"))?,
    )?;
    let exact_model = ReplayEnvelope::new(
        protocol,
        None,
        ReplayScope::ExactModel(ModelId::new("protocol")?),
        kind,
        1,
        DataClassification::Sensitive,
        RawValue::from_string(String::from("{\"value\":1}"))?,
    )?;

    assert_ne!(protocol_scoped.digest(), exact_model.digest());
    Ok(())
}

#[test]
fn deserialization_rechecks_content_constructor_invariants()
-> Result<(), Box<dyn std::error::Error>> {
    let mut text = serde_json::to_value(TextBlock::new("valid", None)?)?;
    text["text"] = serde_json::json!("");
    assert!(serde_json::from_value::<TextBlock>(text).is_err());

    let mut reasoning = serde_json::to_value(ReasoningBlock::new(
        ReasoningVisibility::Summary,
        Some(String::from("summary")),
        None,
    )?)?;
    reasoning["text"] = Value::Null;
    assert!(serde_json::from_value::<ReasoningBlock>(reasoning).is_err());

    let mut call = serde_json::to_value(ToolCallBlock::new(
        CallId::try_from(Uuid::from_u128(10))?,
        ToolName::new("lookup")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::default(),
        "{\"value\":1}",
    )?)?;
    call["arguments"] = serde_json::json!({"value": 2});
    assert!(serde_json::from_value::<ToolCallBlock>(call).is_err());

    let mut item = serde_json::to_value(ConversationItem::new(
        id(11)?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new("valid", None)?)],
        Provenance::ModelOutput,
    )?)?;
    item["blocks"] = serde_json::json!([]);
    assert!(serde_json::from_value::<ConversationItem>(item).is_err());
    Ok(())
}

/// Canonical history keeps both names of an aliased call through its
/// encoding; a call built from one emitted name records it as both.
#[test]
fn an_aliased_tool_call_round_trips_both_names() -> Result<(), Box<dyn std::error::Error>> {
    let call = ToolCallBlock::new(
        CallId::try_from(Uuid::from_u128(30))?,
        ToolName::new("edit")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::default(),
        "{}",
    )?;
    assert_eq!(call.canonical_name(), call.exposed_name());
    let call = call.with_canonical_name(ToolName::new("apply_patch")?);
    let value = serde_json::to_value(&call)?;
    assert_eq!(value["canonical_name"], "apply_patch");
    assert_eq!(value["exposed_name"], "edit");
    assert!(value.get("tool").is_none());
    let decoded: ToolCallBlock = serde_json::from_value(value)?;
    assert_eq!(decoded, call);
    assert_eq!(decoded.canonical_name().as_str(), "apply_patch");
    assert_eq!(decoded.exposed_name().as_str(), "edit");
    Ok(())
}

#[test]
fn tool_arguments_validate_jcs_equivalence() -> Result<(), Box<dyn std::error::Error>> {
    for raw in [
        r#"{"a":21345.0,"b":42171.0}"#,
        r#"{"nested":[-0.0,1e2,1.5,9007199254740993,18446744073709551615]}"#,
    ] {
        let call = ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(31))?,
            ToolName::new("sum")?,
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            raw,
        )?;
        let bytes = serde_json_canonicalizer::to_vec(&call)?;
        let decoded: ToolCallBlock = serde_json::from_slice(&bytes)?;
        assert_eq!(decoded, call);
        assert_eq!(decoded.raw_arguments(), raw);
        assert_eq!(
            decoded.arguments(),
            Some(&serde_json::from_str::<Value>(raw)?)
        );

        for invalid in [serde_json::json!({"nested": [1]}), Value::Null] {
            let mut wire: Value = serde_json::from_slice(&bytes)?;
            wire["arguments"] = invalid;
            assert!(serde_json::from_value::<ToolCallBlock>(wire).is_err());
        }
    }

    let call = ToolCallBlock::new(
        CallId::try_from(Uuid::from_u128(32))?,
        ToolName::new("sum")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        ToolCorrelation::default(),
        "not JSON",
    )?;
    assert_eq!(
        serde_json::from_value::<ToolCallBlock>(serde_json::to_value(&call)?)?,
        call
    );
    let mut wire = serde_json::to_value(&call)?;
    wire["arguments"] = serde_json::json!({});
    assert!(serde_json::from_value::<ToolCallBlock>(wire).is_err());
    Ok(())
}

#[test]
fn tool_result_updates_and_exact_media_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let call_id = CallId::try_from(Uuid::from_u128(12))?;
    let correlation =
        ToolCorrelation::new(Some("remote-exec".into()), None, ToolCallerLineage::Direct)?;
    let update = ToolResultUpdateBlock::new(
        call_id,
        ToolExecutionOwner::Local,
        TextBlock::new("still running", None)?,
        correlation,
    )?;
    let decoded: ToolResultUpdateBlock = serde_json::from_slice(&serde_json::to_vec(&update)?)?;
    assert_eq!(decoded, update);

    let data_url = format!("data:image/png;base64,{}", "A".repeat(9 * 1024));
    let image = ImageBlock::with_detail(
        "image/png",
        MediaSource::data_url(data_url.clone())?,
        Some(ImageDetailHint::Original),
    )?;
    let decoded: ImageBlock = serde_json::from_slice(&serde_json::to_vec(&image)?)?;
    assert_eq!(decoded.source().as_data_url(), Some(data_url.as_str()));
    assert_eq!(decoded.detail(), Some(ImageDetailHint::Original));
    assert!(MediaSource::data_url("https://example.invalid/image").is_err());
    assert!(
        ToolResultUpdateBlock::new(
            call_id,
            ToolExecutionOwner::ProviderHosted {
                provider_kind: NamespaceId::new("hosted")?,
            },
            TextBlock::new("invalid", None)?,
            ToolCorrelation::default(),
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn caller_lineage_is_closed_and_independent_from_execution_owner()
-> Result<(), Box<dyn std::error::Error>> {
    let programmatic = ToolCallerLineage::provider_programmatic(
        NamespaceId::new("hosted.program")?,
        "program-call-7",
    )?;
    let local_programmatic = ToolCallerLineage::local_programmatic(
        41,
        7,
        "cell-7",
        3,
        CallId::try_from(Uuid::from_u128(23))?,
        1,
    )?;
    let local = ToolCallBlock::new(
        CallId::try_from(Uuid::from_u128(20))?,
        ToolName::new("local_lookup")?,
        ToolExecutionOwner::Local,
        ToolCallState::Pending,
        ToolCorrelation::new(None, None, programmatic.clone())?,
        r#"{"query":"local"}"#,
    )?;
    let mcp = ToolCallBlock::new(
        CallId::try_from(Uuid::from_u128(21))?,
        ToolName::new("mcp_lookup")?,
        ToolExecutionOwner::Mcp {
            server: McpServerId::new("remote-catalog")?,
        },
        ToolCallState::Pending,
        ToolCorrelation::new(Some("remote-21".into()), None, local_programmatic.clone())?,
        r#"{"query":"mcp"}"#,
    )?;
    let hosted = ToolCallBlock::new(
        CallId::try_from(Uuid::from_u128(22))?,
        ToolName::new("hosted_interpreter")?,
        ToolExecutionOwner::ProviderHosted {
            provider_kind: NamespaceId::new("hosted.interpreter")?,
        },
        ToolCallState::Completed,
        ToolCorrelation::new(None, Some("container-22".into()), ToolCallerLineage::Direct)?,
        "{}",
    )?;

    assert_eq!(local.correlation().caller_lineage(), &programmatic);
    assert_eq!(mcp.correlation().caller_lineage(), &local_programmatic);
    assert!(matches!(
        local_programmatic,
        ToolCallerLineage::LocalProgrammatic { depth: 1, .. }
    ));
    assert_eq!(
        hosted.correlation().caller_lineage(),
        &ToolCallerLineage::Direct
    );
    assert!(matches!(local.owner(), ToolExecutionOwner::Local));
    assert!(matches!(mcp.owner(), ToolExecutionOwner::Mcp { .. }));
    assert!(matches!(
        hosted.owner(),
        ToolExecutionOwner::ProviderHosted { .. }
    ));

    let decoded: ToolCallBlock = serde_json::from_slice(&serde_json::to_vec(&mcp)?)?;
    assert_eq!(decoded, mcp);
    assert!(
        ToolCallerLineage::provider_programmatic(NamespaceId::new("hosted.program")?, "").is_err()
    );
    assert!(
        ToolCallerLineage::local_programmatic(
            41,
            7,
            "cell-7",
            3,
            CallId::try_from(Uuid::from_u128(24))?,
            0,
        )
        .is_err()
    );
    let obsolete = serde_json::json!({
        "remote_call_id": null,
        "container_id": null,
        "caller_id": "untyped"
    });
    assert!(serde_json::from_value::<ToolCorrelation>(obsolete).is_err());
    Ok(())
}

#[test]
fn local_programmatic_lineage_requires_generation_and_cell()
-> Result<(), Box<dyn std::error::Error>> {
    let call_id = CallId::try_from(Uuid::from_u128(24))?;
    assert!(ToolCallerLineage::local_programmatic(41, 0, "cell-7", 3, call_id, 1).is_err());
    assert!(ToolCallerLineage::local_programmatic(41, 7, "", 3, call_id, 1).is_err());

    let lineage = ToolCallerLineage::local_programmatic(41, 7, "cell-7", 3, call_id, 1)?;
    let mut missing_generation = serde_json::to_value(lineage)?;
    missing_generation
        .as_object_mut()
        .expect("lineage object")
        .get_mut("local_programmatic")
        .and_then(Value::as_object_mut)
        .expect("local lineage fields")
        .remove("interpreter_generation");
    assert!(serde_json::from_value::<ToolCallerLineage>(missing_generation).is_err());
    Ok(())
}

#[test]
fn versioned_canonical_item_fixture_is_stable() -> Result<(), Box<dyn std::error::Error>> {
    let fixture: CanonicalItemFixture =
        serde_json::from_str(include_str!("../../tests/fixtures/canonical_item_v1.json"))?;
    assert_eq!(fixture.schema, "plak.canonical-conversation-item");
    assert_eq!(fixture.version, 1);
    assert_eq!(fixture.value.id(), id(1)?);
    assert_eq!(fixture.value.blocks().len(), 1);
    let encoded = serde_json::to_string(&fixture.value)?;
    let decoded: ConversationItem = serde_json::from_str(&encoded)?;
    assert_eq!(decoded, fixture.value);
    Ok(())
}
