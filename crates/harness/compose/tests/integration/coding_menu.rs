//! The compiled coding menus: what each vocabulary shows a model, in order.
//!
//! These moved out of `plakous-tools` with the pack list they compile: the
//! vocabulary is core, the menu is the packs, and a test that asserts a menu
//! belongs with the packs.

#![allow(
    missing_docs,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    reason = "integration test crate"
)]

use plakous_compose::{
    CodingPresentationExt as _, standard_coding_menus, standard_coding_packs_in_memory,
};
use serde_json::Value;
use std::str::FromStr;
use std::sync::Arc;

use plakous_model::{
    AttemptId, CallId, Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass,
    RunId, TextBlock, ToolCallerLineage, ToolConcurrency, ToolCorrelation, ToolDefinition,
    ToolEffect, ToolInputContract, ToolName, ToolOutputContract, ToolResultPart, TurnId,
};

use plakous_tools::{
    CodingPresentation, PROCESS_TOOL_PROJECTION_REVISION, ProviderModelProjectionIdentityV2,
    RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId,
    ToolFuture, ToolInvocation, ToolInvocationRequest, ToolOutcome, ToolOutcomeKind,
    ToolProjectionExposureV2, ToolProjectionMapV2, ToolProjectionRaisingV2, ToolRegistration,
    ToolRegistryBuilder, menu_names, object_schema, register_packs,
};
use serde_json::json;

fn claude_projection() -> Result<ToolProjectionMapV2, Box<dyn std::error::Error>> {
    claude_read_projection(false)
}

fn claude_read_projection(nested: bool) -> Result<ToolProjectionMapV2, Box<dyn std::error::Error>> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"coding-presentation-result-contract-test"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "anthropic.messages",
        "anthropic.claude_subscription",
        "anthropic.claude.compat.test",
        "messages-family",
        "claude-test",
        "claude-test",
        effective,
        1,
    )?;
    if nested {
        Ok(CodingPresentation::Claude.compile_nested_projection(
            plakous_tools::JobConfig::default(),
            builder.freeze(),
            target,
            &[ToolName::new("Read")?],
        )?)
    } else {
        Ok(CodingPresentation::Claude.projection_map(
            plakous_tools::JobConfig::default(),
            builder.freeze(),
            target,
        )?)
    }
}

fn request(
    call: &str,
    tool: &str,
    arguments: &str,
) -> Result<ToolInvocationRequest, Box<dyn std::error::Error>> {
    Ok(ToolInvocationRequest::new(
        RunId::from_str("00000000-0000-0000-0000-000000000101")?,
        TurnId::from_str("00000000-0000-0000-0000-000000000102")?,
        AttemptId::from_str("00000000-0000-0000-0000-000000000103")?,
        CallId::from_str(call)?,
        ToolName::new(tool)?,
        ToolCorrelation::new(None, None, ToolCallerLineage::default())?,
        Arc::<str>::from(arguments),
    ))
}

#[test]
fn claude_read_and_edit_raise_real_text_and_reject_non_text_success()
-> Result<(), Box<dyn std::error::Error>> {
    let projection = claude_projection()?;
    for (call, tool, arguments, canonical_text, expected_result) in [
        (
            "00000000-0000-0000-0000-000000000104",
            "Read",
            r#"{"file_path":"src/lib.rs"}"#,
            r#"{"content":"content","offset":1,"lines":1,"eof":true,"next_offset":null}"#,
            "     1\tcontent",
        ),
        (
            "00000000-0000-0000-0000-000000000105",
            "Edit",
            r#"{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
            "changed_files=1",
            "changed_files=1",
        ),
    ] {
        let prepared = projection.prepare(request(call, tool, arguments)?)?;
        let (_, receipt) = prepared.into_parts();
        let receipt = receipt.ok_or("projected request omitted its receipt")?;
        let raised = projection.raise(&receipt, ToolOutcome::success_text(canonical_text)?)?;
        let [ToolResultPart::Text(text)] = raised.parts() else {
            return Err("raised coding result was not one text part".into());
        };
        assert_eq!(
            serde_json::from_str::<Value>(text.text())?,
            json!({"result": expected_result})
        );

        let mismatch = ToolOutcome::success(vec![
            ToolResultPart::Text(TextBlock::new("first", None)?),
            ToolResultPart::Text(TextBlock::new("second", None)?),
        ])?;
        assert!(projection.raise(&receipt, mismatch).is_err());
    }
    Ok(())
}

#[test]
fn claude_read_numbers_empty_and_truncated_slices() -> Result<(), Box<dyn std::error::Error>> {
    let projection = claude_projection()?;
    let prepared = projection.prepare(request(
        "00000000-0000-0000-0000-000000000114",
        "Read",
        r#"{"file_path":"src/lib.rs"}"#,
    )?)?;
    let (_, receipt) = prepared.into_parts();
    let receipt = receipt.ok_or("projected request omitted its receipt")?;
    let empty = projection.raise(
        &receipt,
        ToolOutcome::success_text(
            r#"{"content":"","offset":1,"lines":0,"eof":true,"next_offset":null}"#,
        )?,
    )?;
    let [ToolResultPart::Text(text)] = empty.parts() else {
        return Err("empty read was not one text part".into());
    };
    assert_eq!(
        serde_json::from_str::<Value>(text.text())?,
        json!({"result": ""})
    );
    let page = projection.raise(
        &receipt,
        ToolOutcome::success_text(
            r#"{"content":"foo\nbar","offset":10,"lines":2,"eof":false,"next_offset":12}"#,
        )?,
    )?;
    let [ToolResultPart::Text(text)] = page.parts() else {
        return Err("paged read was not one text part".into());
    };
    assert_eq!(
        serde_json::from_str::<Value>(text.text())?,
        json!({"result": "    10\tfoo\n    11\tbar\n... more lines; next_offset=12"})
    );
    let stamped = projection.raise(
        &receipt,
        ToolOutcome::success_text(
            r#"{"content":"hello","offset":1,"lines":1,"eof":true,"next_offset":null,"workspace_generation":7}"#,
        )?,
    )?;
    let [ToolResultPart::Text(text)] = stamped.parts() else {
        return Err("generation-stamped read was not one text part".into());
    };
    assert_eq!(
        serde_json::from_str::<Value>(text.text())?,
        json!({"result": "     1\thello"})
    );
    assert!(
        projection
            .raise(
                &receipt,
                ToolOutcome::success_text("not a canonical stamp")?
            )
            .is_err()
    );
    let failed = projection.raise(&receipt, ToolOutcome::failure_text("missing file")?)?;
    assert_eq!(failed.kind(), ToolOutcomeKind::Failure);
    let [ToolResultPart::Text(text)] = failed.parts() else {
        return Err("failed read was not one text part".into());
    };
    assert_eq!(text.text(), "missing file");
    Ok(())
}

#[test]
fn claude_lossless_read_identity_changes_only_the_numbered_raising_program()
-> Result<(), Box<dyn std::error::Error>> {
    let direct = claude_projection()?;
    let nested = claude_read_projection(true)?;
    for (projection, name, prior_program, changed) in [
        (&direct, "Read", "text_result_object_v1", true),
        (&direct, "Edit", "text_result_object_v1", false),
        (&direct, "Write", "text_result_object_v1", false),
        (&nested, "Read", "raw_utf8_v1", false),
    ] {
        let entry = projection
            .by_exposed(&ToolName::new(name)?)
            .ok_or("missing entry")?;
        let prior_identity = Digest::sha256(
            DigestDomain::ToolDefinition,
            2,
            &[
                b"plakous-tools/projection-program-v2",
                b"raising",
                prior_program.as_bytes(),
                entry.output_schema().digest().bytes(),
            ],
        )?;
        assert_eq!(
            entry.raising_identity() != prior_identity,
            changed,
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn claude_direct_numbering_preserves_exact_file_bytes_and_nested_json()
-> Result<(), Box<dyn std::error::Error>> {
    let direct = claude_projection()?;
    let nested = claude_read_projection(true)?;
    for content in [
        "",
        "\n",
        "first\n",
        "first",
        "first\r\n",
        "a\r\nb\n\nlast",
        "雪\t🙂\r\n終",
    ] {
        let canonical = serde_json::to_string(&json!({
            "content": content, "offset": 7,
            "lines": content.split_inclusive('\n').count(),
            "eof": true, "next_offset": null
        }))?;
        for (projection, is_nested) in [(&direct, false), (&nested, true)] {
            let prepared = projection.prepare(request(
                "00000000-0000-0000-0000-000000000116",
                "Read",
                r#"{"file_path":"seed.txt"}"#,
            )?)?;
            let (_, receipt) = prepared.into_parts();
            let raised = projection.raise(
                &receipt.ok_or("read omitted receipt")?,
                ToolOutcome::success_text(canonical.clone())?,
            )?;
            let [ToolResultPart::Text(text)] = raised.parts() else {
                return Err("read was not one text part".into());
            };
            if is_nested {
                assert_eq!(text.text(), canonical);
            } else {
                let wrapped: Value = serde_json::from_str(text.text())?;
                let numbered = wrapped["result"].as_str().ok_or("missing result")?;
                let mut restored = String::new();
                for (index, line) in numbered.split_inclusive('\n').enumerate() {
                    let (prefix, body) = line.split_once('\t').ok_or("missing line prefix")?;
                    assert_eq!(prefix.trim().parse::<usize>()?, 7 + index);
                    restored.push_str(body);
                }
                assert_eq!(restored.as_bytes(), content.as_bytes());
            }
        }
    }
    Ok(())
}

#[test]
fn claude_edit_empty_old_string_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let projection = claude_projection()?;
    let Err(error) = projection.prepare(request(
        "00000000-0000-0000-0000-000000000115",
        "Edit",
        r#"{"file_path":"a.txt","old_string":"","new_string":"created"}"#,
    )?) else {
        return Err("empty old_string must fail closed".into());
    };
    let message = error.to_string();
    assert!(message.contains("use Write"), "{message}");
    Ok(())
}

#[test]
fn claude_write_prepare_lowers_to_apply_patch_create_or_overwrite()
-> Result<(), Box<dyn std::error::Error>> {
    let projection = claude_projection()?;
    let prepared = projection.prepare(request(
        "00000000-0000-0000-0000-000000000106",
        "Write",
        r#"{"file_path":"a.txt","content":"new file"}"#,
    )?)?;
    assert_eq!(
        prepared.invocation().identity().name().as_str(),
        "apply_patch"
    );
    assert_eq!(prepared.invocation().arguments().value()["old_string"], "");
    assert_eq!(
        prepared.invocation().arguments().value()["new_string"],
        "new file"
    );
    // Claude Code's Write overwrites an existing file, as Grok's `write` does.
    assert_eq!(
        prepared.invocation().arguments().value()["replace_all"],
        true
    );
    Ok(())
}

fn grok_projection() -> Result<ToolProjectionMapV2, Box<dyn std::error::Error>> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"grok-write-overwrite-lowering-test"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "xai.grok.responses",
        "xai.grok_subscription",
        "xai.grok.compat.test",
        "responses-family",
        "grok-4.6",
        "grok-4.6",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    Ok(CodingPresentation::Grok.projection_map(
        plakous_tools::JobConfig::default(),
        builder.freeze(),
        target,
    )?)
}

#[test]
fn grok_write_prepare_lowers_to_apply_patch_overwrite() -> Result<(), Box<dyn std::error::Error>> {
    let projection = grok_projection()?;
    let prepared = projection.prepare(request(
        "00000000-0000-0000-0000-000000000107",
        "write",
        r#"{"file_path":"a.txt","content":"new file"}"#,
    )?)?;
    assert_eq!(
        prepared.invocation().identity().name().as_str(),
        "apply_patch"
    );
    assert_eq!(prepared.invocation().arguments().value()["old_string"], "");
    assert_eq!(
        prepared.invocation().arguments().value()["new_string"],
        "new file"
    );
    assert_eq!(
        prepared.invocation().arguments().value()["replace_all"],
        true
    );
    Ok(())
}

#[test]
fn claude_read_prepare_defaults_limit_to_2000() -> Result<(), Box<dyn std::error::Error>> {
    let projection = claude_projection()?;
    let prepared = projection.prepare(request(
        "00000000-0000-0000-0000-000000000108",
        "Read",
        r#"{"file_path":"a.txt"}"#,
    )?)?;
    assert_eq!(prepared.invocation().arguments().value()["limit"], 2000);
    Ok(())
}

#[test]
fn grok_search_replace_empty_old_string_stays_create_only() -> Result<(), Box<dyn std::error::Error>>
{
    let projection = grok_projection()?;
    let prepared = projection.prepare(request(
        "00000000-0000-0000-0000-000000000109",
        "search_replace",
        r#"{"file_path":"a.txt","old_string":"","new_string":"created","replace_all":true}"#,
    )?)?;
    assert_eq!(prepared.invocation().arguments().value()["old_string"], "");
    assert_eq!(
        prepared.invocation().arguments().value()["replace_all"],
        false
    );
    Ok(())
}

struct ExtraTool;

impl ToolExecutor for ExtraTool {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async { ToolOutcome::success_text("{}") })
    }
}

fn structured_chat_with_spawn_extra() -> Result<ToolProjectionMapV2, Box<dyn std::error::Error>> {
    let definition = ToolDefinition::new(
        ToolName::new("spawn_agent")?,
        "plak.topology.v1",
        "Spawn a child agent from a template in the frozen roster.",
        ToolInputContract::structured(object_schema(
            json!({
                "template": {"type": "string"},
                "tier": {"type": "string"},
                "description": {"type": "string"},
                "prompt": {"type": "string"},
                "output_schema_json": {"type": "string"}
            }),
            &["template", "tier", "prompt", "output_schema_json"],
        ))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    let extra = ToolRegistration::new(
        RegisteredToolId::new("plak.topology.spawn_agent.v1")?,
        ToolExecutorId::new("plak.topology.spawn_agent.executor.v1")?,
        ToolExecutorBindingIdentity::new(1, &[b"plak.topology.v1", b"spawn_agent"])?,
        definition,
        Arc::new(ExtraTool),
    )
    .nested_eligible();
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    builder.register(extra)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"direct-extra-passthrough-raise-test"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "openrouter.chat",
        "openrouter.api_key",
        "test.product-composition.v1",
        "chat-family",
        "test-model",
        "test-model",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    Ok(CodingPresentation::Pi.projection_map_with_extensions(
        plakous_tools::JobConfig::default(),
        builder.freeze(),
        target,
        &[ToolName::new("spawn_agent")?],
    )?)
}

fn topology_extension_registration(
    name: &str,
    id_suffix: &str,
) -> Result<ToolRegistration, Box<dyn std::error::Error>> {
    let definition = ToolDefinition::new(
        ToolName::new(name)?,
        "plak.topology.v1",
        "Call one product topology extension.",
        ToolInputContract::structured(object_schema(
            json!({"value": {"type": "string"}}),
            &["value"],
        ))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    Ok(ToolRegistration::new(
        RegisteredToolId::new(format!("plak.topology.{id_suffix}.v1"))?,
        ToolExecutorId::new(format!("plak.topology.{id_suffix}.executor.v1"))?,
        ToolExecutorBindingIdentity::new(1, &[b"plak.topology.v1", name.as_bytes()])?,
        definition,
        Arc::new(ExtraTool),
    )
    .nested_eligible())
}

/// Session creation captures, encodes, and revalidates the durable tool
/// descriptors for the compiled product catalog. Presentation order is the
/// model-visible order (catalog names, then product extensions in
/// declaration order), never an ascending set, so the descriptor must
/// reconstruct that exact order for every enrolled costume.
#[test]
fn product_catalog_descriptors_reconstruct_presentation_order_with_extensions()
-> Result<(), Box<dyn std::error::Error>> {
    // `send_message` sorts before `spawn_agent`: extension declaration
    // order is not ascending, exactly as the product compiles it.
    let extras = [
        ToolName::new("spawn_agent")?,
        ToolName::new("send_message")?,
    ];
    for kind in [
        CodingPresentation::Pi,
        CodingPresentation::Codex,
        CodingPresentation::Grok,
        CodingPresentation::Claude,
    ] {
        let mut builder = ToolRegistryBuilder::default();
        register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
        builder.register(topology_extension_registration(
            "spawn_agent",
            "spawn_agent",
        )?)?;
        builder.register(topology_extension_registration(
            "send_message",
            "send_message",
        )?)?;
        let effective: EffectiveProviderConfigurationId =
            serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"product-catalog-durable-descriptor-test"],
            )?)?)?;
        let target = ProviderModelProjectionIdentityV2::new(
            IntegrationClass::Standard,
            "test.responses",
            "test.oauth",
            "test.product-composition.v1",
            "test-family",
            "test-model",
            "test-model",
            effective,
            PROCESS_TOOL_PROJECTION_REVISION,
        )?;
        let registry = builder.freeze();
        let map = kind.projection_map_with_extensions(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            target.clone(),
            &extras,
        )?;
        let expected = map
            .entries()
            .iter()
            .map(|entry| entry.exposed_name().as_str().to_owned())
            .collect::<Vec<_>>();
        // The registry-free menu and the compiled surface are one
        // membership: if they ever disagree, the ordering rule has two
        // implementations again.
        assert_eq!(
            menu_names(
                &standard_coding_menus(plakous_tools::JobConfig::default()),
                kind
            )?,
            kind.compile_surface(
                plakous_tools::JobConfig::default(),
                registry.clone(),
                target.clone(),
                &[]
            )?
            .exposed_names()
            .to_vec(),
            "{kind:?} menu names"
        );
        let mut catalog = kind
            .compile_surface(
                plakous_tools::JobConfig::default(),
                registry.clone(),
                target.clone(),
                &[],
            )?
            .exposed_names()
            .iter()
            .map(|name| name.as_str().to_owned())
            .collect::<Vec<_>>();
        catalog.extend(extras.iter().map(|name| name.as_str().to_owned()));
        assert_eq!(expected, catalog, "{kind:?} presentation order");

        let registry_descriptor = plakous_tools::FrozenToolRegistryDescriptor::capture(&registry)?;
        let registry_bytes = registry_descriptor.encode_v2()?;
        let decoded_registry =
            plakous_tools::FrozenToolRegistryDescriptor::decode_v2(&registry_bytes)?;
        let projection_descriptor = plakous_tools::FrozenToolProjectionDescriptorV2::capture(&map)?;
        let projection_bytes = projection_descriptor.encode_v2()?;
        let decoded = plakous_tools::FrozenToolProjectionDescriptorV2::decode_v2(
            &projection_bytes,
            &decoded_registry,
        )?;
        let restored = decoded.resolve(registry)?;
        assert_eq!(
            restored
                .entries()
                .iter()
                .map(|entry| entry.exposed_name().as_str().to_owned())
                .collect::<Vec<_>>(),
            expected,
            "{kind:?} reconstructed presentation order"
        );
        assert_eq!(restored.digest(), map.digest(), "{kind:?} map identity");
    }
    Ok(())
}

#[test]
fn direct_extra_passthrough_wraps_structured_spawn_receipt()
-> Result<(), Box<dyn std::error::Error>> {
    let projection = structured_chat_with_spawn_extra()?;
    let prepared = projection.prepare(request(
        "00000000-0000-0000-0000-000000000106",
        "spawn_agent",
        r#"{"template":"review","tier":"read_only","description":"reader","prompt":"read HELLO.txt","output_schema_json":"{}"}"#,
    )?)?;
    let (_, receipt) = prepared.into_parts();
    let receipt = receipt.ok_or("projected extra omitted its receipt")?;
    // The spawn receipt: a running attempt, its effective tier, and
    // the workspace it bound — not a result.
    let canonical = r#"{"agent_id":"c1","attempt_id":"c1-a1","status":"running","tier":"read_only","workspace":{"id":"ws-root","mode":"inherit","base":null}}"#;
    let raised = projection.raise(&receipt, ToolOutcome::success_text(canonical)?)?;
    let [ToolResultPart::Text(text)] = raised.parts() else {
        return Err("raised extra result was not one text part".into());
    };
    assert_eq!(
        serde_json::from_str::<Value>(text.text())?,
        json!({"result": canonical})
    );
    Ok(())
}

/// A session reopen (and create's revalidation) rebuilds the registry from its
/// durable descriptor. The claimer flag is re-taken from the live host table,
/// so the same Grok compile paths still claim the slot over the restored
/// registry, and the flagged identity compares equal to the live one.
#[test]
fn a_flagged_claim_survives_the_durable_registry_round_trip()
-> Result<(), Box<dyn std::error::Error>> {
    let name = ToolName::new("get_command_or_subagent_output")?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"grok-flagged-claim-durable-test"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "xai.grok.responses",
        "xai.grok_subscription",
        "xai.grok.compat.test",
        "responses-family",
        "grok-4.6",
        "grok-4.6",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    builder.register(
        topology_extension_registration(name.as_str(), "task_output")?.presented_name_claimer(),
    )?;
    let live = builder.freeze();
    let descriptor = plakous_tools::FrozenToolRegistryDescriptor::decode_v2(
        &plakous_tools::FrozenToolRegistryDescriptor::capture(&live)?.encode_v2()?,
    )?;
    let restored = descriptor.resolve(&plakous_tools::ToolExecutorBindings::capture(&live))?;
    assert_eq!(
        live.registry_identity_digest()?,
        restored.registry_identity_digest()?
    );
    assert_eq!(restored.identity(&name), live.identity(&name));
    for (label, map) in [
        (
            "direct",
            CodingPresentation::Grok.projection_map_with_extensions(
                plakous_tools::JobConfig::default(),
                restored.clone(),
                target.clone(),
                std::slice::from_ref(&name),
            )?,
        ),
        (
            "nested",
            CodingPresentation::Grok.compile_nested_projection(
                plakous_tools::JobConfig::default(),
                restored.clone(),
                target.clone(),
                &[ToolName::new("read_file")?, name.clone()],
            )?,
        ),
    ] {
        let entries = map
            .entries()
            .iter()
            .filter(|entry| entry.exposed_name() == &name)
            .collect::<Vec<_>>();
        let [entry] = entries.as_slice() else {
            return Err(format!("{label}: {name} presented {} times", entries.len()).into());
        };
        assert_eq!(
            entry.raising(),
            ToolProjectionRaisingV2::ModelContentIdentityV1,
            "{label}"
        );
    }
    Ok(())
}

/// Grok's output slot is claimable. A same-name capability flagged
/// `presented_name_claimer` claims it on every compile path that appends it
/// (the passthrough extension list and the nested catalog), not only through
/// a product pack; an unflagged one is a `SameSlot` clash on both.
#[test]
fn only_a_flagged_registration_claims_a_grok_slot_on_every_compile_path()
-> Result<(), Box<dyn std::error::Error>> {
    let name = ToolName::new("get_command_or_subagent_output")?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"grok-flagged-claim-test"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "xai.grok.responses",
        "xai.grok_subscription",
        "xai.grok.compat.test",
        "responses-family",
        "grok-4.6",
        "grok-4.6",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    for flagged in [true, false] {
        let registration = topology_extension_registration(name.as_str(), "task_output")?;
        let registration = if flagged {
            registration.presented_name_claimer()
        } else {
            registration
        };
        let mut builder = ToolRegistryBuilder::default();
        register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
        builder.register(registration)?;
        let registry = builder.freeze();
        let direct = CodingPresentation::Grok.projection_map_with_extensions(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            target.clone(),
            std::slice::from_ref(&name),
        );
        let nested = CodingPresentation::Grok.compile_nested_projection(
            plakous_tools::JobConfig::default(),
            registry,
            target.clone(),
            &[ToolName::new("read_file")?, name.clone()],
        );
        if flagged {
            for map in [direct?, nested?] {
                let entries = map
                    .entries()
                    .iter()
                    .filter(|entry| entry.exposed_name() == &name)
                    .collect::<Vec<_>>();
                let [entry] = entries.as_slice() else {
                    return Err(format!("{name} presented {} times", entries.len()).into());
                };
                assert!(
                    entry
                        .canonical_operations()
                        .values()
                        .all(|identity| identity.name() == &name),
                    "the claimed slot binds the flagged capability"
                );
                assert_eq!(
                    entry.raising(),
                    ToolProjectionRaisingV2::ModelContentIdentityV1
                );
            }
        } else {
            for error in [direct.err(), nested.err()] {
                let error = error.ok_or("an unflagged same-name capability compiled")?;
                assert_eq!(
                    error
                        .namespace_clash()
                        .map(plakous_tools::NamespaceClash::reason),
                    Some(plakous_tools::NamespaceClashReason::SameSlot),
                    "{error}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn grok_nested_catalog_exposes_direct_names_not_canonical_registry_names()
-> Result<(), Box<dyn std::error::Error>> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"grok-nested-direct-catalog"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "xai.grok.responses",
        "xai.grok_subscription",
        "xai.grok.product-javascript.v1",
        "responses-family",
        "grok-4.6",
        "grok-4.6",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let kind = CodingPresentation::Grok;
    let registry = builder.freeze();
    let names = kind
        .compile_surface(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            target.clone(),
            &[],
        )?
        .exposed_names()
        .to_vec();
    let nested = kind.compile_nested_projection(
        plakous_tools::JobConfig::default(),
        registry,
        target,
        &names,
    )?;
    let exposed = nested
        .entries()
        .iter()
        .map(|entry| entry.exposed_name().as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        exposed,
        [
            "get_command_or_subagent_output",
            "grep",
            "kill_command_or_subagent",
            "list_dir",
            "read_file",
            "run_terminal_command",
            "search_replace",
            "todo_write",
            "write"
        ]
    );
    assert!(!exposed.contains(&"search_repository"));
    assert!(!exposed.contains(&"process_session_start"));
    let grep = ToolName::new("grep")?;
    let schema = nested
        .by_exposed(&grep)
        .and_then(ToolProjectionExposureV2::input_schema)
        .ok_or("grep schema missing")?;
    let value = schema.document().value();
    assert!(value["properties"].get("glob").is_some());
    assert!(value["properties"].get("-i").is_some());
    Ok(())
}

#[test]
fn supergrok_oauth_tool_input_schemas_json() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"supergrok-oauth-tool-schemas"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "xai.grok.responses",
        "xai.grok_subscription",
        "xai.grok.product-javascript.v1",
        "responses-family",
        "grok-4.6",
        "grok-4.6",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let map = CodingPresentation::Grok.projection_map(
        plakous_tools::JobConfig::default(),
        builder.freeze(),
        target,
    )?;
    let mut schemas = serde_json::Map::new();
    for tool in map
        .entries()
        .iter()
        .map(|entry| entry.exposed_name().clone())
        .collect::<Vec<_>>()
    {
        let name = tool.as_str();
        let schema = map
            .by_exposed(&tool)
            .and_then(ToolProjectionExposureV2::input_schema)
            .ok_or_else(|| format!("schema missing for {name}"))?;
        schemas.insert(name.to_owned(), schema.document().value().as_ref().clone());
    }
    let todo = &schemas["todo_write"];
    assert!(todo["properties"].get("merge").is_some());
    assert!(
        todo["properties"]["todos"]["items"]["properties"]
            .get("id")
            .is_some()
    );
    assert!(
        todo["properties"]["todos"]["items"]["properties"]
            .get("grok_todos")
            .is_none()
    );
    let printed = serde_json::to_string_pretty(&Value::Object(schemas))?;
    eprintln!("supergrok_oauth_tool_schemas={printed}");
    Ok(())
}

#[test]
fn claude_nested_catalog_exposes_direct_names_not_canonical_registry_names()
-> Result<(), Box<dyn std::error::Error>> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"claude-nested-direct-catalog"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "anthropic.messages",
        "anthropic.claude_subscription",
        "anthropic.claude.product-javascript.v1",
        "messages-family",
        "claude-sonnet-5",
        "claude-sonnet-5",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let kind = CodingPresentation::Claude;
    let registry = builder.freeze();
    let names = kind
        .compile_surface(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            target.clone(),
            &[],
        )?
        .exposed_names()
        .to_vec();
    let nested = kind.compile_nested_projection(
        plakous_tools::JobConfig::default(),
        registry,
        target,
        &names,
    )?;
    let exposed = nested
        .entries()
        .iter()
        .map(|entry| entry.exposed_name().as_str())
        .collect::<Vec<_>>();
    assert_eq!(exposed, ["Bash", "Edit", "Read", "TaskStop", "Write"]);
    assert!(!exposed.contains(&"search_repository"));
    assert!(!exposed.contains(&"read_file"));
    let read = ToolName::new("Read")?;
    assert!(
        nested
            .by_exposed(&read)
            .and_then(ToolProjectionExposureV2::input_schema)
            .is_some()
    );
    for entry in nested.entries() {
        if matches!(entry.exposed_name().as_str(), "Bash" | "TaskStop") {
            continue;
        }
        assert_eq!(entry.raising(), ToolProjectionRaisingV2::RawUtf8V1);
    }
    let write = ToolName::new("Write")?;
    let write_entry = nested.by_exposed(&write).ok_or("Write absent")?;
    let write_schema = write_entry
        .input_schema()
        .ok_or("Write must be structured")?;
    assert_eq!(
        write_schema.document().value()["required"],
        json!(["file_path", "content"])
    );
    Ok(())
}

#[test]
fn direct_catalog_keeps_exposed_names_then_appends_extras() -> Result<(), Box<dyn std::error::Error>>
{
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"claude-direct-presentation-order"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "anthropic.messages",
        "anthropic.claude_subscription",
        "anthropic.claude.compat.test",
        "messages-family",
        "claude-sonnet-5",
        "claude-sonnet-5",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let kind = CodingPresentation::Claude;
    let extra = ToolName::new("update_plan")?;
    let map = kind.projection_map_with_extensions(
        plakous_tools::JobConfig::default(),
        builder.freeze(),
        target,
        &[extra],
    )?;
    let exposed = map
        .entries()
        .iter()
        .map(|entry| entry.exposed_name().as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        exposed,
        ["Bash", "Edit", "Read", "TaskStop", "Write", "update_plan"]
    );
    Ok(())
}

/// Pi's `edit` is pi's schema, lowered onto one canonical multi-edit call that
/// carries every replacement; `write` lowers onto create-or-overwrite.
#[test]
fn pi_edit_lowers_every_replacement_onto_one_canonical_call()
-> Result<(), Box<dyn std::error::Error>> {
    let projection = presentation_projection(CodingPresentation::Pi)?;
    let edit = projection
        .by_exposed(&ToolName::new("edit")?)
        .ok_or("pi edit absent")?;
    let schema = edit.input_schema().ok_or("pi edit must be structured")?;
    assert_eq!(
        schema.document().value()["required"],
        json!(["path", "edits"])
    );
    assert_eq!(
        schema.document().value()["properties"]["edits"]["items"]["required"],
        json!(["oldText", "newText"])
    );
    let prepared = projection.prepare(request(
        "00000000-0000-0000-0000-0000000004a0",
        "edit",
        r#"{"path":"a.txt","edits":[{"oldText":"one","newText":"1"},{"oldText":"two","newText":"2"}]}"#,
    )?)?;
    assert_eq!(
        prepared.invocation().identity().name().as_str(),
        "apply_patch"
    );
    assert_eq!(
        prepared.invocation().arguments().value().as_ref(),
        &json!({
            "file_path": "a.txt",
            "edits": [
                {"old_string": "one", "new_string": "1"},
                {"old_string": "two", "new_string": "2"}
            ]
        })
    );
    let empty = projection.prepare(request(
        "00000000-0000-0000-0000-0000000004a1",
        "edit",
        r#"{"path":"a.txt","edits":[{"oldText":"","newText":"x"}]}"#,
    )?);
    assert!(
        empty.is_err(),
        "an empty oldText is refused before any effect"
    );
    let write = projection.prepare(request(
        "00000000-0000-0000-0000-0000000004a2",
        "write",
        r#"{"path":"notes/a.md","content":"hello"}"#,
    )?)?;
    assert_eq!(
        prepared.invocation().identity().name(),
        write.invocation().identity().name()
    );
    assert_eq!(
        write.invocation().arguments().value()["replace_all"],
        json!(true)
    );
    Ok(())
}

/// Pi's `read` is text only: an image path is refused with typed text before
/// any effect, and a larger `limit` reads the canonical 2000-line page.
#[test]
fn pi_read_refuses_images_and_clamps_its_page() -> Result<(), Box<dyn std::error::Error>> {
    let projection = presentation_projection(CodingPresentation::Pi)?;
    for (index, path) in ["diagram.png", "shots/A.JPG", "x.webp"].iter().enumerate() {
        let call = format!("00000000-0000-0000-0000-0000000004b{index}");
        let error = projection
            .prepare(request(&call, "read", &format!(r#"{{"path":"{path}"}}"#))?)
            .err()
            .ok_or("an image read lowered")?;
        assert_eq!(
            error.message().as_str(),
            plakous_tools::PI_READ_IMAGE_REFUSAL,
            "{path}"
        );
    }
    let prepared = projection.prepare(request(
        "00000000-0000-0000-0000-0000000004b9",
        "read",
        r#"{"path":"big.log","limit":5000}"#,
    )?)?;
    assert_eq!(
        prepared.invocation().arguments().value()["limit"],
        json!(2000)
    );
    Ok(())
}

/// Pi's `wait` and `stop` accept process ids
/// (`N` or `pN`) and refuse anything else before any effect; a bare `wait`
/// awaits the first terminal among every own job.
#[test]
fn pi_wait_and_stop_lower_onto_process_awaits_and_terminate()
-> Result<(), Box<dyn std::error::Error>> {
    let projection = presentation_projection(CodingPresentation::Pi)?;
    let bare = projection.prepare(request(
        "00000000-0000-0000-0000-0000000004c0",
        "wait",
        "{}",
    )?)?;
    let value = bare.invocation().arguments().value();
    assert_eq!(value["wake"], json!("any"));
    assert!(value.get("session_ids").is_none());
    assert_eq!(value["yield_time_ms"], json!(300_000));
    let named = projection.prepare(request(
        "00000000-0000-0000-0000-0000000004c1",
        "wait",
        r#"{"ids":["p2","3"],"timeout_ms":7200000}"#,
    )?)?;
    let value = named.invocation().arguments().value();
    assert_eq!(value["session_ids"], json!([2, 3]));
    assert_eq!(value["yield_time_ms"], json!(3_600_000));
    let stop = projection.prepare(request(
        "00000000-0000-0000-0000-0000000004c2",
        "stop",
        r#"{"id":"p4"}"#,
    )?)?;
    assert_eq!(
        stop.invocation().arguments().value()["session_id"],
        json!(4)
    );
    for (index, (tool, arguments)) in [
        ("wait", r#"{"ids":["agent-7"]}"#),
        ("stop", r#"{"id":"0c1f-child"}"#),
    ]
    .into_iter()
    .enumerate()
    {
        let call = format!("00000000-0000-0000-0000-0000000004d{index}");
        let error = projection
            .prepare(request(&call, tool, arguments)?)
            .err()
            .ok_or("a non-process id lowered")?;
        assert!(
            error
                .message()
                .as_str()
                .contains("is not a process id; this agent has no agent tools"),
            "{tool}: {}",
            error.message().as_str()
        );
    }
    Ok(())
}

#[test]
fn configured_canonical_names_cover_every_projected_operation()
-> Result<(), Box<dyn std::error::Error>> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let registry = builder.freeze();
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"configured-canonical-covers-projection"],
        )?)?)?;
    for kind in [
        CodingPresentation::Pi,
        CodingPresentation::Codex,
        CodingPresentation::Grok,
        CodingPresentation::Claude,
    ] {
        let target = ProviderModelProjectionIdentityV2::new(
            IntegrationClass::Standard,
            "test.protocol",
            "test.route",
            "test.profile",
            "test-family",
            "test-model",
            "test-model",
            effective,
            PROCESS_TOOL_PROJECTION_REVISION,
        )?;
        let map = kind.projection_map(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            target,
        )?;
        let configured = kind.configured_canonical_names();
        for entry in map.entries() {
            for identity in entry.canonical_operations().values() {
                assert!(
                    configured.contains(&identity.name().as_str()),
                    "{kind:?} projection covers {} but configured_canonical_names does not",
                    identity.name().as_str()
                );
            }
        }
    }
    Ok(())
}

#[test]
fn nested_claude_read_returns_exact_json_instead_of_numbered_display()
-> Result<(), Box<dyn std::error::Error>> {
    let projection = claude_read_projection(true)?;
    assert!(
        projection.entries()[0]
            .description()
            .contains("JSON string")
    );
    assert!(!projection.entries()[0].description().contains("cat -n"));
    let prepared = projection.prepare(request(
        "00000000-0000-0000-0000-000000000124",
        "Read",
        r#"{"file_path":"src/lib.rs"}"#,
    )?)?;
    let (_, receipt) = prepared.into_parts();
    let canonical =
        json!({"content":"offset=1\r\nlast", "offset":1,"lines":2,"eof":true,"next_offset":null})
            .to_string();
    let raised = projection.raise(
        &receipt.ok_or("missing receipt")?,
        ToolOutcome::success_text(&canonical)?,
    )?;
    let [ToolResultPart::Text(text)] = raised.parts() else {
        return Err("missing text".into());
    };
    assert_eq!(text.text(), canonical);
    Ok(())
}

// ---------------------------------------------------------------------------
// The vendor-vocabulary contract: every documented example lowers and
// validates.
// ---------------------------------------------------------------------------

/// The arguments each vendor's own documentation shows for its tool.
///
/// A vocabulary is a promise: the words a provider tells its model to use are
/// the words Plakous must accept. Grok's `list_dir` example below is the exact
/// call that a live Grok 4.6 session made and that Plakous rejected, because
/// the lowering never supplied the `query` the canonical contract requires.
fn documented_vendor_example(
    presentation: CodingPresentation,
    exposed: &str,
) -> Option<&'static str> {
    Some(match (presentation, exposed) {
        (CodingPresentation::Pi, "read") => r#"{"path":"src/lib.rs","offset":10,"limit":50}"#,
        (CodingPresentation::Pi, "bash") => r#"{"command":"ls -la","timeout":30}"#,
        (CodingPresentation::Pi, "edit") => {
            r#"{"path":"a.txt","edits":[{"oldText":"one","newText":"two"},{"oldText":"three","newText":"four"}]}"#
        }
        (CodingPresentation::Pi, "write") => r#"{"path":"notes.md","content":"pi\n"}"#,
        (CodingPresentation::Pi, "wait") => r#"{"ids":["p1"],"timeout_ms":0}"#,
        (CodingPresentation::Pi, "stop") => r#"{"id":"p1"}"#,
        (CodingPresentation::Codex, "update_plan") => {
            r#"{"plan":[{"step":"survey the walk","status":"in_progress"}]}"#
        }
        (CodingPresentation::Codex, "exec_command") => r#"{"cmd":"ls -la"}"#,
        (CodingPresentation::Codex, "write_stdin") => r#"{"session_id":1,"chars":"y\n"}"#,
        // The Codex freeform envelope is UTF-8 text, not JSON.
        (CodingPresentation::Codex, "apply_patch") => {
            "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch\n"
        }
        (CodingPresentation::Grok, "grep") => r#"{"pattern":"needle","glob":"docs/spec/*.md"}"#,
        (CodingPresentation::Grok, "list_dir") => r#"{"target_directory":"."}"#,
        (CodingPresentation::Grok, "read_file") => r#"{"target_file":"src/lib.rs"}"#,
        (CodingPresentation::Grok, "search_replace") => {
            r#"{"file_path":"src/lib.rs","old_string":"fn old()","new_string":"fn new()"}"#
        }
        (CodingPresentation::Grok, "write") => r#"{"file_path":"notes.md","content":"grok\n"}"#,
        (CodingPresentation::Grok, "todo_write") => {
            r#"{"todos":[{"id":"1","content":"survey the walk","status":"pending"}]}"#
        }
        (CodingPresentation::Grok, "run_terminal_command") => {
            r#"{"command":"ls -la","description":"list the workspace"}"#
        }
        (CodingPresentation::Grok, "get_command_or_subagent_output") => {
            r#"{"task_ids":["p1"],"timeout_ms":0}"#
        }
        (CodingPresentation::Grok, "kill_command_or_subagent") => r#"{"task_id":"p1"}"#,
        (CodingPresentation::Claude, "Read") => r#"{"file_path":"src/lib.rs"}"#,
        (CodingPresentation::Claude, "Edit") => {
            r#"{"file_path":"a.txt","old_string":"one","new_string":"two"}"#
        }
        (CodingPresentation::Claude, "Write") => r#"{"file_path":"a.txt","content":"hello\n"}"#,
        (CodingPresentation::Claude, "Bash") => r#"{"command":"ls -la"}"#,
        (CodingPresentation::Claude, "TaskStop") => r#"{"task_id":"1"}"#,
        _ => return None,
    })
}

fn presentation_projection(
    presentation: CodingPresentation,
) -> Result<ToolProjectionMapV2, Box<dyn std::error::Error>> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"vendor-vocabulary-lowering-contract-test"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "test.responses",
        "test.oauth",
        "test.vocabulary-contract.v1",
        "test-family",
        "test-model",
        "test-model",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    Ok(presentation.projection_map(
        plakous_tools::JobConfig::default(),
        builder.freeze(),
        target,
    )?)
}

/// Every menu entry of every vocabulary lowers its own documented example into
/// arguments the canonical tool accepts.
///
/// This drives the real projection path, so a lowering that drops a required
/// canonical field fails here instead of in a live session.
#[test]
fn every_vendor_menu_entry_lowers_its_documented_example_into_a_valid_canonical_call()
-> Result<(), Box<dyn std::error::Error>> {
    let mut covered = 0_usize;
    for presentation in [
        CodingPresentation::Pi,
        CodingPresentation::Codex,
        CodingPresentation::Grok,
        CodingPresentation::Claude,
    ] {
        let projection = presentation_projection(presentation)?;
        for (index, entry) in projection.entries().iter().enumerate() {
            let exposed = entry.exposed_name().as_str();
            let arguments = documented_vendor_example(presentation, exposed).ok_or_else(|| {
                format!(
                    "{presentation:?} exposes `{exposed}` with no documented example; \
                     add one so its lowering is covered"
                )
            })?;
            let call = format!("00000000-0000-0000-0000-0000000002{index:02x}");
            let prepared = projection
                .prepare(request(&call, exposed, arguments)?)
                .map_err(|error| {
                    format!("{presentation:?} `{exposed}` example did not lower: {error}")
                })?;
            // Lowered arguments reached the canonical tool and satisfied its
            // structured contract: `prepare` validates them there.
            assert_ne!(prepared.invocation().identity().name().as_str(), "");
            covered = covered.saturating_add(1);
        }
    }
    assert!(covered >= 20, "only {covered} menu entries were covered");
    Ok(())
}

#[test]
fn grok_read_file_advertised_limit_matches_canonical_admission()
-> Result<(), Box<dyn std::error::Error>> {
    let projection = presentation_projection(CodingPresentation::Grok)?;
    let exposed = projection
        .by_exposed(&ToolName::new("read_file")?)
        .ok_or("Grok read_file is not exposed")?;
    let schema = exposed
        .input_schema()
        .ok_or("Grok read_file schema missing")?;
    assert_eq!(
        schema.document().value()["properties"]["limit"]["maximum"],
        json!(2000)
    );

    let at_limit = r#"{"target_file":"seed.txt","limit":2000}"#;
    assert!(schema.parse_instance(at_limit).is_ok());
    let prepared = projection.prepare(request(
        "00000000-0000-0000-0000-0000000003a0",
        "read_file",
        at_limit,
    )?)?;
    assert_eq!(
        prepared.invocation().identity().name().as_str(),
        "read_file"
    );
    assert_eq!(
        prepared.invocation().arguments().value()["limit"],
        json!(2000)
    );

    let beyond_limit = r#"{"target_file":"seed.txt","limit":2001}"#;
    assert!(schema.parse_instance(beyond_limit).is_err());
    assert!(
        projection
            .prepare(request(
                "00000000-0000-0000-0000-0000000003a1",
                "read_file",
                beyond_limit,
            )?)
            .is_err()
    );
    Ok(())
}

#[test]
fn grok_grep_advertised_head_limit_matches_canonical_admission()
-> Result<(), Box<dyn std::error::Error>> {
    let projection = presentation_projection(CodingPresentation::Grok)?;
    let exposed = projection
        .by_exposed(&ToolName::new("grep")?)
        .ok_or("Grok grep is not exposed")?;
    let schema = exposed.input_schema().ok_or("Grok grep schema missing")?;
    assert_eq!(
        schema.document().value()["properties"]["head_limit"]["maximum"],
        json!(4096)
    );

    let at_limit = r#"{"pattern":"needle","head_limit":4096}"#;
    assert!(schema.parse_instance(at_limit).is_ok());
    let prepared = projection.prepare(request(
        "00000000-0000-0000-0000-0000000003a2",
        "grep",
        at_limit,
    )?)?;
    assert_eq!(
        prepared.invocation().identity().name().as_str(),
        "search_repository"
    );
    assert_eq!(
        prepared.invocation().arguments().value()["head_limit"],
        json!(4096)
    );

    let beyond_limit = r#"{"pattern":"needle","head_limit":4097}"#;
    assert!(schema.parse_instance(beyond_limit).is_err());
    assert!(
        projection
            .prepare(request(
                "00000000-0000-0000-0000-0000000003a3",
                "grep",
                beyond_limit,
            )?)
            .is_err()
    );
    Ok(())
}

/// Grok's own `list_dir` vocabulary lowers to a listing of the workspace root.
///
/// `.`, the empty string and the absolute workspace root are the three ways a
/// model names the root, and all three must reach the same canonical call.
#[test]
fn grok_list_dir_lowers_every_spelling_of_the_workspace_root()
-> Result<(), Box<dyn std::error::Error>> {
    let projection = presentation_projection(CodingPresentation::Grok)?;
    for (index, target_directory) in [".", "", "src"].into_iter().enumerate() {
        let call = format!("00000000-0000-0000-0000-0000000003{index:02x}");
        let prepared = projection.prepare(request(
            &call,
            "list_dir",
            &json!({"target_directory": target_directory}).to_string(),
        )?)?;
        assert_eq!(
            prepared.invocation().identity().name().as_str(),
            "search_repository"
        );
        let arguments = prepared.invocation().arguments().value().clone();
        assert_eq!(arguments["path"], json!(target_directory));
        assert_eq!(arguments["query"], json!(""));
        assert_eq!(arguments["list_directory"], json!(true));
    }
    Ok(())
}

/// A persisted Grok hub projection whose claimed output slot binds a
/// registration the live host does NOT flag must not recompile: extension
/// names list it, and the recompile is a clash.
#[test]
fn an_unflagged_host_cannot_recompile_a_claimed_slot() -> Result<(), Box<dyn std::error::Error>> {
    let name = ToolName::new("get_command_or_subagent_output")?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"unflagged-claimed-slot"],
        )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "xai.grok.responses",
        "xai.grok_subscription",
        "xai.grok.compat.test",
        "responses-family",
        "grok-4.6",
        "grok-4.6",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let packs = standard_coding_packs_in_memory()?;
    let registry =
        |flag: bool| -> Result<plakous_tools::FrozenToolRegistry, Box<dyn std::error::Error>> {
            let mut builder = ToolRegistryBuilder::default();
            register_packs(&packs, &mut builder)?;
            let registration = topology_extension_registration(name.as_str(), "task_output")?;
            builder.register(if flag {
                registration.presented_name_claimer()
            } else {
                registration
            })?;
            Ok(builder.freeze())
        };
    let live = registry(true)?;
    let unflagged = registry(false)?;
    let projection = CodingPresentation::Grok.projection_map_with_extensions(
        plakous_tools::JobConfig::default(),
        live.clone(),
        target.clone(),
        std::slice::from_ref(&name),
    )?;
    let descriptor = plakous_tools::FrozenToolRegistryDescriptor::decode_v2(
        &plakous_tools::FrozenToolRegistryDescriptor::capture(&live)?.encode_v2()?,
    )?;
    let restored = descriptor.resolve(&plakous_tools::ToolExecutorBindings::capture(&unflagged))?;
    let extras = CodingPresentation::Grok.projection_extensions(
        plakous_tools::JobConfig::default(),
        restored.clone(),
        &projection,
    )?;
    assert_eq!(extras, vec![name.clone()]);
    let recompiled = CodingPresentation::Grok.projection_map_with_extensions(
        plakous_tools::JobConfig::default(),
        restored,
        target,
        &extras,
    );
    assert!(
        recompiled.is_err(),
        "an unflagged same-name capability recompiled a claimed slot"
    );
    Ok(())
}
