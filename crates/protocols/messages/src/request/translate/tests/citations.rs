use super::*;

fn native_block(
    policy: &MessagesRequestPolicy,
    target: Option<u32>,
    block: &str,
) -> TestResult<ContentBlock> {
    let raw = serde_json::value::to_raw_value(&NativeReplay {
        target,
        block: block.to_owned(),
    })?;
    Ok(ContentBlock::Opaque(plakous_model::OpaqueBlock::new(
        bind_replay_payload(
            policy,
            ModelId::new("claude-exact")?,
            NATIVE_REPLAY_KIND,
            raw,
        )?,
        OpaqueFallback::CompatibleReplay,
    )?))
}

const CITED: &str = r#"{"type":"text","text":"Synthetic cited answer.","citations":[{"type":"web_search_result_location","encrypted_index":"synthetic-private-index"}]}"#;

fn cited_blocks(policy: &MessagesRequestPolicy) -> TestResult<Vec<ContentBlock>> {
    Ok(vec![
        ContentBlock::Text(TestTextBlock::new("Synthetic cited answer.", None)?),
        native_block(policy, Some(0), CITED)?,
    ])
}

fn lower_native(
    blocks: &[ContentBlock],
    target: &MessagesRequestPolicy,
    diagnostics: &mut Vec<String>,
) -> TestResult<Vec<MessageBlock>> {
    Ok(translate_assistant_bound(
        blocks,
        target,
        &ToolOwnership::default(),
        false,
        true,
        AssistantReplayState {
            container: &mut None,
            server_lineage: &mut crate::artifact::MessagesArtifactLineage::default(),
            diagnostics,
        },
    )?
    .0)
}

fn refuses_same_binding_untargeted_native(block: &str) -> TestResult {
    let policy = policy()?;
    let blocks = [native_block(&policy, None, block)?];
    let Err(error) = lower_native(&blocks, &policy, &mut Vec::new()) else {
        return Err("excluded native replay was admitted under the same binding".into());
    };
    assert_eq!(
        error.to_string(),
        "native Messages replay cannot bypass typed block validation"
    );
    Ok(())
}

#[test]
fn same_binding_native_fallback_replay_refuses() -> TestResult {
    refuses_same_binding_untargeted_native(r#"{"type":"fallback"}"#)
}

#[test]
fn same_binding_native_image_replay_refuses() -> TestResult {
    refuses_same_binding_untargeted_native(
        r#"{"type":"image","source":{"type":"url","url":"https://example.com/fixture.png"}}"#,
    )
}

#[test]
fn cited_text_survives_same_protocol_binding_changes() -> TestResult {
    let source = policy()?;
    let blocks = cited_blocks(&source)?;
    let mut targets = Vec::new();
    let mut model = source.clone();
    model.requested_model = ModelId::new("synthetic-other-model")?;
    model.expected_reported_model = Some(model.requested_model.clone());
    targets.push(model);
    targets.push(policy_with_authority(
        b"other-account",
        b"selected",
        b"effective",
    )?);
    targets.push(policy_with_authority(
        b"account",
        b"other-selected",
        b"effective",
    )?);
    targets.push(policy_with_authority(
        b"account",
        b"selected",
        b"other-effective",
    )?);
    let mut api = source.clone();
    api.api_version = "2099-01-01".to_owned();
    targets.push(api);
    let mut beta = source.clone();
    beta.beta_header = "synthetic-beta".to_owned();
    targets.push(beta);
    let mut descriptor = source.clone();
    descriptor.semantic_descriptor_binding =
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"other-descriptor"])?;
    targets.push(descriptor);
    for target in targets {
        let mut diagnostics = Vec::new();
        for _ in 0..2 {
            let output = lower_native(&blocks, &target, &mut diagnostics)?;
            assert!(
                matches!(output.as_slice(), [MessageBlock::Text { text, .. }] if text == "Synthetic cited answer.")
            );
        }
        assert_eq!(
            diagnostics,
            ["incompatible Messages citation replay was stripped; answer text was retained"]
        );
        assert!(!diagnostics[0].contains("synthetic-private-index"));
    }
    Ok(())
}

#[test]
fn incompatible_binding_does_not_hide_invalid_native_content() -> TestResult {
    let source = policy()?;
    let target = policy_with_authority(b"other-account", b"selected", b"effective")?;
    for (index, raw) in [
        (Some(0), "not JSON"),
        (
            Some(0),
            r#"{"type":"text","text":"changed","citations":[{}]}"#,
        ),
        (Some(99), CITED),
        (
            None,
            r#"{"type":"future_note","data":"synthetic-redacted"}"#,
        ),
    ] {
        let blocks = [
            ContentBlock::Text(TestTextBlock::new("Synthetic cited answer.", None)?),
            native_block(&source, index, raw)?,
        ];
        assert!(lower_native(&blocks, &target, &mut Vec::new()).is_err());
    }
    let mut duplicate = cited_blocks(&source)?;
    duplicate.push(duplicate[1].clone());
    assert!(lower_native(&duplicate, &target, &mut Vec::new()).is_err());
    Ok(())
}

#[test]
fn incompatible_binding_still_refuses_malformed_source_envelopes() -> TestResult {
    let source = policy()?;
    let target = policy_with_authority(b"other-account", b"selected", b"effective")?;
    let blocks = cited_blocks(&source)?;
    let ContentBlock::Opaque(original) = &blocks[1] else {
        return Err("missing fixture replay".into());
    };
    let replay = original.replay();
    let value: Value = serde_json::from_str(replay.payload().get())?;
    for field in ["actual_model", "replay_schema_revision", "value"] {
        let mut malformed = value.clone();
        malformed
            .as_object_mut()
            .ok_or("fixture envelope")?
            .remove(field);
        let envelope = ReplayEnvelope::new(
            replay.protocol().clone(),
            None,
            replay.scope().clone(),
            replay.kind().clone(),
            replay.version(),
            replay.classification(),
            serde_json::value::to_raw_value(&malformed)?,
        )?;
        let bad = [
            blocks[0].clone(),
            ContentBlock::Opaque(plakous_model::OpaqueBlock::new(
                envelope,
                OpaqueFallback::CompatibleReplay,
            )?),
        ];
        assert!(lower_native(&bad, &target, &mut Vec::new()).is_err());
    }
    Ok(())
}
