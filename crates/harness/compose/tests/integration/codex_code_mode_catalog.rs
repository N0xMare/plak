//! Exact presentation evidence for Codex OAuth `CodeModeOnly`.

use std::str::FromStr;
use std::sync::Arc;

use plakous_compose::standard_coding_packs_in_memory;
use plakous_compose::{
    CODEX_NATIVE_CODE_MODE_DEFAULT_OUTPUT_TOKENS_V1, CODEX_NATIVE_CODE_MODE_DEFAULT_YIELD_MS_V1,
    CODEX_NATIVE_CODE_MODE_EXEC_GRAMMAR_V1, CodexMinimalLocalCodeModeV1,
    CodexNativeLocalCodeModeDescriptorV1, CodexNativeLocalCodeModeV1,
};
use plakous_model::{
    AttemptId, CallId, CancellationToken, Clock, Digest, DigestDomain,
    EffectiveProviderConfigurationId, FixedClock, IntegrationClass, ProjectedToolPresentationV2,
    RunId, Timestamp, ToolConcurrency, ToolCorrelation, ToolDefinition, ToolEffect,
    ToolInputContract, ToolName, ToolOutputContract, TurnId,
};
use plakous_tool_process::ProcessSessionTools;
use plakous_tools::{
    CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1, CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
    EffectAuthorizationGrantSource, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentScope, FrozenToolRegistryDescriptor, NestedToolEligibility, ProgressSink,
    ProviderModelProjectionIdentityV2, RegisteredToolId, ToolContext, ToolExecutor,
    ToolExecutorBindingIdentity, ToolExecutorBindings, ToolExecutorId, ToolFuture, ToolInvocation,
    ToolInvocationRequest, ToolOutcome, ToolPolicyRevision, ToolRegistration, ToolRegistryBuilder,
    ToolRuntimeErrorKind, register_packs,
};

type TestError = Box<dyn std::error::Error>;

fn configuration(seed: &'static [u8]) -> Result<EffectiveProviderConfigurationId, TestError> {
    Ok(serde_json::from_value(serde_json::to_value(
        Digest::sha256(DigestDomain::EffectiveProviderConfiguration, 5, &[seed])?,
    )?)?)
}

fn target(seed: &'static [u8]) -> Result<ProviderModelProjectionIdentityV2, TestError> {
    Ok(ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "openai.codex.responses",
        "openai.codex_subscription",
        "openai.codex.compat.0.153.3",
        "responses-family",
        "gpt-codex-code-mode-test",
        "gpt-codex-code-mode-test",
        configuration(seed)?,
        1,
    )?)
}

fn registry() -> Result<plakous_tools::FrozenToolRegistry, TestError> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)?;
    Ok(builder.freeze())
}

fn descriptor() -> Result<CodexNativeLocalCodeModeDescriptorV1, TestError> {
    Ok(CodexNativeLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry()?,
        target(b"codex-code-mode-catalog")?,
    )?)
}

fn request(name: &str, arguments: &str) -> Result<ToolInvocationRequest, TestError> {
    Ok(ToolInvocationRequest::new(
        RunId::from_str("00000000-0000-0000-0000-000000000201")?,
        TurnId::from_str("00000000-0000-0000-0000-000000000202")?,
        AttemptId::from_str("00000000-0000-0000-0000-000000000203")?,
        CallId::from_str("00000000-0000-0000-0000-000000000204")?,
        ToolName::new(name)?,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default())?,
        arguments,
    ))
}

#[test]
fn outer_menu_kind_grammar_and_wait_contract_are_exact() -> Result<(), TestError> {
    let descriptor = descriptor()?;
    let definitions = descriptor.outer_projection().entries();
    assert_eq!(
        definitions
            .iter()
            .map(|definition| definition.exposed_name().as_str())
            .collect::<Vec<_>>(),
        ["exec", "wait"]
    );
    let exec = &definitions[0];
    let ProjectedToolPresentationV2::CustomUtf8 { input, format } = exec.presentation() else {
        return Err("exec must be explicitly CustomUtf8".into());
    };
    assert_eq!(input.max_bytes(), 1_048_576);
    assert_eq!(format.definition(), CODEX_NATIVE_CODE_MODE_EXEC_GRAMMAR_V1);
    assert_eq!(exec.description(), descriptor.exec_description());
    let wait = &definitions[1];
    let schema = wait.input_schema().ok_or("wait must be structured")?;
    assert_eq!(
        schema.document().value()["required"],
        serde_json::json!(["cell_id"])
    );
    assert_eq!(
        schema.document().value()["additionalProperties"],
        serde_json::Value::Bool(false)
    );
    assert_eq!(
        schema.document().value()["properties"]["yield_time_ms"]["type"],
        "number"
    );
    assert_eq!(
        schema.document().value()["properties"]["max_tokens"]["type"],
        "number"
    );
    assert_eq!(wait.description(), descriptor.wait_description());
    assert_eq!(CODEX_NATIVE_CODE_MODE_DEFAULT_YIELD_MS_V1, 10_000);
    assert_eq!(CODEX_NATIVE_CODE_MODE_DEFAULT_OUTPUT_TOKENS_V1, 10_000);
    Ok(())
}

#[test]
fn wait_projection_preserves_official_number_contract_as_semantic_whole_numbers()
-> Result<(), TestError> {
    let descriptor = descriptor()?;
    let projected = descriptor.outer_projection().prepare(request(
        CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
        r#"{"cell_id":"cell-1","yield_time_ms":100,"max_tokens":200,"terminate":false}"#,
    )?)?;
    let arguments = projected.invocation().arguments().value();
    assert_eq!(arguments["cell_id"], "cell-1");
    assert_eq!(arguments["yield_time_ms"].as_f64(), Some(100.0));
    assert_eq!(arguments["max_tokens"].as_f64(), Some(200.0));
    assert_eq!(arguments["terminate"], false);
    Ok(())
}

#[test]
fn custom_exec_projection_preserves_raw_source_text() -> Result<(), TestError> {
    let descriptor = descriptor()?;
    let source = "text('line 1\\nline 2')\n";
    let projected = descriptor
        .outer_projection()
        .prepare(request(CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1, source)?)?;
    assert_eq!(
        projected.invocation().arguments().value().as_str(),
        Some(source)
    );
    Ok(())
}

/// A `cell_id` the model echoed as the numeral it was shown is accepted and
/// normalised to the string the contract declares.
///
/// The product announces a cell as `Script running with cell ID 2`, so a model
/// that sends `{"cell_id": 2}` is reading its own transcript. The presented
/// schema still says `string` and still decides; the numeral is an accepted
/// superset applied before validation, and dispatch only ever sees the string.
#[test]
fn a_numeric_wait_cell_id_is_accepted_and_normalised_to_its_string_form() -> Result<(), TestError> {
    let descriptor = descriptor()?;
    let projection = descriptor.outer_projection();

    let numeric = projection.prepare(request(
        CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
        r#"{"cell_id":2,"yield_time_ms":100}"#,
    )?)?;
    let arguments = numeric.invocation().arguments().value();
    assert_eq!(arguments["cell_id"], "2");
    assert!(arguments["cell_id"].is_string());
    assert_eq!(arguments["yield_time_ms"].as_f64(), Some(100.0));

    // A string identifier is untouched, including one that is not a numeral.
    for spelling in ["2", "cell-1"] {
        let prepared = projection.prepare(request(
            CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
            &serde_json::json!({"cell_id": spelling}).to_string(),
        )?)?;
        assert_eq!(
            prepared.invocation().arguments().value()["cell_id"],
            spelling
        );
    }

    // Anything that is neither a string nor a whole non-negative number is
    // still refused by the presented schema.
    for refused in [
        serde_json::json!({"cell_id": true}),
        serde_json::json!({"cell_id": -1}),
        serde_json::json!({"cell_id": 1.5}),
        serde_json::json!({"cell_id": ["2"]}),
        serde_json::json!({"cell_id": {"id": "2"}}),
        serde_json::json!({}),
    ] {
        let error = projection
            .prepare(request(
                CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
                &refused.to_string(),
            )?)
            .err()
            .ok_or("the wait contract admitted an invalid cell_id")?;
        assert_eq!(error.kind(), ToolRuntimeErrorKind::InvalidArguments);
    }
    Ok(())
}

/// A Code Mode `wait` that names no particular cell reaches the running one.
///
/// A call sent
/// `{"cell_id": "none", "yield_time_ms": 1}` and was rejected
/// `InvalidArguments`, twice. Only the driver knows what is running, so the two
/// stages are shown together: the driver substitutes the active cell, and the
/// unchanged contract validates the result.
#[test]
fn a_wait_that_names_no_particular_cell_reaches_the_running_one() -> Result<(), TestError> {
    let descriptor = descriptor()?;
    let projection = descriptor.outer_projection();

    for spelling in [
        r#"{"cell_id":"none"}"#,
        r#"{"cell_id":"NONE"}"#,
        r#"{"cell_id":"null"}"#,
        r#"{"cell_id":"all"}"#,
    ] {
        let lowered = plakous_tools::normalized_wait_arguments(spelling, Some("2"))
            .ok_or("the driver did not lower a no-particular-cell wait")?;
        let prepared =
            projection.prepare(request(CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1, &lowered)?)?;
        assert_eq!(
            prepared.invocation().arguments().value()["cell_id"],
            "2",
            "{spelling}"
        );
    }

    // The exact live arguments, budget and all.
    let lowered = plakous_tools::normalized_wait_arguments(
        r#"{"cell_id":"none","yield_time_ms":1}"#,
        Some("2"),
    )
    .ok_or("lowering dropped the call")?;
    let prepared = projection.prepare(request(CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1, &lowered)?)?;
    let arguments = prepared.invocation().arguments().value();
    assert_eq!(arguments["cell_id"], "2");
    assert_eq!(arguments["yield_time_ms"].as_f64(), Some(1.0));

    // An identifier that merely looks like one of the spellings is still an
    // identifier, and is never reinterpreted.
    assert_eq!(
        plakous_tools::normalized_wait_arguments(r#"{"cell_id":"nonesuch"}"#, Some("2"),),
        None
    );

    // The superset never widens the contract. Nothing below is a spelling of
    // "no particular cell", so nothing below is lowered, and each is left for
    // the presented schema to decide exactly as it always did.
    for untouched in [r"{}", r#"{"cell_id":null}"#, r#"{"cell_id":""}"#] {
        assert_eq!(
            plakous_tools::normalized_wait_arguments(untouched, Some("2"),),
            None,
            "{untouched}"
        );
    }
    // An absent or null identifier is a violation of a required string field.
    for violation in [r"{}", r#"{"cell_id":null}"#] {
        assert_eq!(
            projection
                .prepare(request(CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1, violation)?)
                .err()
                .ok_or("the wait contract admitted a missing cell_id")?
                .kind(),
            ToolRuntimeErrorKind::InvalidArguments,
            "{violation}"
        );
    }
    // The Code Mode contract declares `cell_id` a string with no length bound,
    // unlike the product `wait_js` one, so an empty identifier is admitted here
    // and is an identifier like any other. Both schemas are identity-bearing
    // and digest-pinned, so this asymmetry is described, not corrected.
    let empty = projection.prepare(request(
        CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
        r#"{"cell_id":""}"#,
    )?)?;
    assert_eq!(empty.invocation().arguments().value()["cell_id"], "");
    Ok(())
}

/// The model-visible bytes did not change. The presented `wait` schema and its
/// description are identity-bearing, and accepting the numeral happens before
/// validation rather than in the contract the model is shown.
#[test]
fn the_presented_wait_contract_is_unchanged_by_the_accepted_superset() -> Result<(), TestError> {
    let descriptor = descriptor()?;
    let entry = descriptor
        .outer_projection()
        .entries()
        .iter()
        .find(|entry| entry.exposed_name().as_str() == CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1)
        .ok_or("the outer projection does not expose wait")?;
    let schema = entry.input_schema().ok_or("wait has no presented schema")?;
    assert_eq!(
        schema.document().value().as_ref(),
        &plakous_tools::codex_code_mode_wait_schema()
    );
    assert_eq!(
        schema.document().value()["properties"]["cell_id"]["type"],
        "string"
    );
    assert_eq!(
        schema.document().value()["additionalProperties"],
        serde_json::json!(false)
    );
    assert_eq!(
        schema.digest(),
        plakous_model::PortableToolSchemaV2::new(plakous_tools::codex_code_mode_wait_schema())?
            .digest()
    );
    Ok(())
}

#[test]
fn production_exec_preamble_advertises_enrolled_helpers_only() -> Result<(), TestError> {
    let minimal = CodexMinimalLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry()?,
        target(b"production-preamble")?,
    )?;
    let reference = descriptor()?;
    for (description, wait_description) in [
        (reference.exec_description(), reference.wait_description()),
        (minimal.exec_description(), minimal.wait_description()),
    ] {
        assert!(description.contains("`tools` object"));
        assert!(description.contains("`text("));
        assert!(description.contains("`store("));
        assert!(description.contains("`load("));
        assert!(description.contains("`notify("));
        assert!(description.contains("`yield_control("));
        assert!(description.contains("`exit("));
        assert!(description.contains("`setTimeout("));
        assert!(description.contains("`ALL_TOOLS`"));
        assert!(
            !description.contains("mcp__ologs__get_profile"),
            "default nested catalog has no MCP tools; do not advertise MCP identifier examples"
        );
        assert!(
            !description.contains("`image("),
            "blob helpers stay implemented; do not advertise them until a blob-producing nested tool is enrolled"
        );
        assert!(!description.contains("`audio("));
        assert!(!description.contains("`generatedImage("));
        assert_eq!(
            wait_description,
            format!(
                "Waits on a yielded `exec` cell and returns new output or completion.\n{}",
                plakous_code_mode::build_wait_description().trim().replace(
                    "Defaults to 10000 ms.",
                    "Defaults to 10000 ms; clamped to at most 600000 ms."
                )
            )
        );
    }
    Ok(())
}

#[test]
fn exact_reference_preserves_reviewed_exec_template_and_minimal_uses_authored_text()
-> Result<(), TestError> {
    let reference = descriptor()?;
    let (preamble, _) = reference
        .exec_description()
        .split_once("\n\nNested tool declarations, all members of `declare const tools`:")
        .ok_or("missing nested declaration header")?;
    // Pin the reviewed upstream-derived preamble, including every whitespace byte.
    assert_eq!(
        plakous_model::sha256_hex(&[preamble.as_bytes()]),
        "c9584414de5212cf0dc808c779688fea7e8164577e933db6e2497c613d9bdb49"
    );
    let minimal = CodexMinimalLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry()?,
        target(b"minimal-preamble")?,
    )?;
    assert!(
        minimal
            .exec_description()
            .starts_with("Run raw JavaScript as an async module")
    );
    assert!(!minimal.exec_description().starts_with(preamble));
    Ok(())
}

#[test]
fn nested_catalog_is_exact_ordered_collision_free_and_non_recursive() -> Result<(), TestError> {
    let descriptor = descriptor()?;
    assert_eq!(
        descriptor
            .nested_tools()
            .iter()
            .map(|tool| tool.exposed_name().as_str())
            .collect::<Vec<_>>(),
        CodexNativeLocalCodeModeV1::nested_names()
    );
    assert_eq!(descriptor.nested_tools().len(), 7);
    for forbidden in [
        "exec",
        "wait",
        "spawn_agent",
        "send_agent_message",
        "mcp",
        "skill",
        "hosted_tool",
    ] {
        assert!(
            descriptor
                .nested_tools()
                .iter()
                .all(|tool| tool.exposed_name().as_str() != forbidden),
            "nested catalog exposed {forbidden}"
        );
    }
    let projection = descriptor.nested_projection();
    for nested in descriptor.nested_tools() {
        let entry = projection
            .by_exposed(nested.exposed_name())
            .ok_or("nested entry missing from executor-bound projection")?;
        assert_eq!(entry.identity(), nested.projection_identity());
        assert_eq!(entry.input_schema(), nested.input_schema());
        for identity in entry.canonical_operations().values() {
            assert_eq!(identity.executor_id(), entry.executor_id());
            assert_eq!(
                identity.nested_eligibility(),
                NestedToolEligibility::NestedEligible
            );
        }
    }
    let registry = registry()?;
    for marker in [
        CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1,
        CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
    ] {
        assert_eq!(
            registry.nested_eligibility(&ToolName::new(marker)?),
            Some(NestedToolEligibility::DirectOnly)
        );
    }
    assert!(descriptor.exec_description().contains("exec_command(args:"));
    assert!(descriptor.exec_description().contains(
        "Promise<{ output: string; exit_code?: number; session_id?: number; status?: string }>;"
    ));
    assert!(!descriptor.exec_description().contains("Promise<{ status:"));
    Ok(())
}

#[test]
fn minimal_native_catalog_reuses_outer_contract_and_contains_only_codex_core_tools()
-> Result<(), TestError> {
    let registry = registry()?;
    let target = target(b"codex-minimal-code-mode")?;
    let minimal = CodexMinimalLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target.clone(),
    )?;
    let reference =
        CodexNativeLocalCodeModeV1::compile(plakous_tools::JobConfig::default(), registry, target)?;

    assert_eq!(
        minimal
            .nested_tools()
            .iter()
            .map(|tool| tool.exposed_name().as_str())
            .collect::<Vec<_>>(),
        CodexMinimalLocalCodeModeV1::nested_names()
    );
    assert_eq!(minimal.nested_tools().len(), 4);
    assert_eq!(
        minimal
            .outer_projection()
            .entries()
            .iter()
            .map(|entry| entry.exposed_name().as_str())
            .collect::<Vec<_>>(),
        ["exec", "wait"]
    );
    assert_eq!(
        minimal.outer_projection().entries()[0].presentation(),
        reference.outer_projection().entries()[0].presentation()
    );
    assert!(!minimal.exec_description().contains("### `read_file`"));
    assert!(
        !minimal
            .exec_description()
            .contains("### `search_repository`")
    );
    assert!(!minimal.exec_description().contains("### `inspect_image`"));
    assert_ne!(minimal.identity(), reference.identity());
    assert_ne!(
        minimal.outer_projection().digest(),
        reference.outer_projection().digest()
    );
    Ok(())
}

#[test]
fn minimal_nested_process_names_lower_and_raise_through_only_the_canonical_registrations()
-> Result<(), TestError> {
    let descriptor = CodexMinimalLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry()?,
        target(b"codex-minimal-nested-process-authority")?,
    )?;
    let projection = descriptor.nested_projection();

    for (exposed, arguments, canonical) in [
        (
            "exec_command",
            r#"{"cmd":"pwd","login":false}"#,
            "process_session_start",
        ),
        (
            "exec_command",
            r#"{"cmd":"","session_id":7}"#,
            "process_session_poll",
        ),
        (
            "write_stdin",
            r#"{"session_id":7,"chars":"x"}"#,
            "process_session_write",
        ),
    ] {
        let prepared = projection.prepare(request(exposed, arguments)?)?;
        assert_eq!(prepared.invocation().identity().name().as_str(), canonical);
        assert_eq!(
            prepared
                .receipt()
                .ok_or("projected process call has no receipt")?
                .exposed_name()
                .as_str(),
            exposed
        );
    }

    let started = projection.prepare(request("exec_command", r#"{"cmd":"pwd","login":false}"#)?)?;
    let receipt = started.receipt().ok_or("exec_command receipt is absent")?;
    let raised = projection.raise(
        receipt,
        ToolOutcome::success_text(
            r#"{"status":"exited","exit_code":0,"session_id":null,"cursor":4,"output":"/work"}"#,
        )?,
    )?;
    let [plakous_model::ToolResultPart::Text(text)] = raised.parts() else {
        return Err("raised exec_command result is not one text part".into());
    };
    assert_eq!(text.text(), r#"{"exit_code":0,"output":"/work"}"#);

    let widened = projection
        .prepare(request(
            "process_session_start",
            r#"{"cmd":"pwd","login":false}"#,
        )?)
        .err()
        .ok_or("canonical process name must not widen the nested catalog")?;
    assert_eq!(widened.kind(), ToolRuntimeErrorKind::UnknownTool);
    Ok(())
}

#[test]
fn nested_text_capabilities_raise_the_raw_string_without_a_result_envelope() -> Result<(), TestError>
{
    // G1-2: every nested lineage hands the cell the raw text. Direct keeps the
    // `{"result": ...}` envelope; a nested cell must never have to unwrap it.
    fn assert_raw(
        projection: &plakous_tools::ToolProjectionMap,
        label: &str,
    ) -> Result<(), TestError> {
        let prepared = projection.prepare(request(
            "update_plan",
            r#"{"plan":[{"step":"read","status":"in_progress"}]}"#,
        )?)?;
        let receipt = prepared.receipt().ok_or("update_plan receipt is absent")?;
        let raised = projection.raise(receipt, ToolOutcome::success_text("plan updated")?)?;
        let [plakous_model::ToolResultPart::Text(text)] = raised.parts() else {
            return Err(format!("{label}: raised update_plan is not one text part").into());
        };
        assert_eq!(text.text(), "plan updated", "{label}");
        Ok(())
    }
    let minimal = CodexMinimalLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry()?,
        target(b"codex-minimal-nested-raw")?,
    )?;
    assert_raw(minimal.nested_projection(), "minimal")?;
    let native = CodexNativeLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry()?,
        target(b"codex-native-nested-raw")?,
    )?;
    assert_raw(native.nested_projection(), "native")?;
    Ok(())
}

#[test]
fn compile_rejects_a_present_but_direct_only_nested_capability() -> Result<(), TestError> {
    let mut builder = ToolRegistryBuilder::default();
    let plan_state: Arc<dyn plakous_tool_plan::PlanState> =
        Arc::new(plakous_tool_plan::InMemoryPlanState::new());
    let process = Arc::new(ProcessSessionTools::posix_sh()?);
    // Every pack but the process pack, whose first operation is re-registered
    // Direct-only below so the nested compile has a present but ineligible name.
    for pack in [
        Arc::new(plakous_tool_files::FilesToolPack) as Arc<dyn plakous_tools::ToolPack>,
        Arc::new(plakous_tool_search::SearchToolPack),
        Arc::new(plakous_tool_plan::PlanToolPack::new(plan_state)),
    ] {
        register_packs(&[pack], &mut builder)?;
    }
    for (index, registration) in process.registrations()?.into_iter().enumerate() {
        builder.register(if index == 0 {
            registration.direct_only()
        } else {
            registration
        })?;
    }
    CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)?;
    let error = CodexNativeLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        builder.freeze(),
        target(b"codex-code-mode-direct-only-negative")?,
    )
    .err()
    .ok_or("direct-only nested capability must reject")?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Registration);
    assert!(
        error.message().as_str().contains("nested-eligible")
            || error
                .message()
                .as_str()
                .contains("nested Code Mode capability"),
        "{}",
        error.message().as_str()
    );
    Ok(())
}

#[test]
fn complete_identity_is_stable_and_rotates_with_exact_target() -> Result<(), TestError> {
    let registry = registry()?;
    let first = CodexNativeLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target(b"code-mode-identity-a")?,
    )?;
    let same = CodexNativeLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target(b"code-mode-identity-a")?,
    )?;
    let drifted = CodexNativeLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry,
        target(b"code-mode-identity-b")?,
    )?;
    assert_eq!(first.identity(), same.identity());
    assert_eq!(
        serde_json::to_vec(first.nested_tools())?,
        serde_json::to_vec(same.nested_tools())?
    );
    assert_ne!(first.identity(), drifted.identity());
    assert_ne!(
        first.outer_projection().digest(),
        drifted.outer_projection().digest()
    );
    assert_ne!(
        first.nested_projection().digest(),
        drifted.nested_projection().digest()
    );
    Ok(())
}

#[test]
fn durable_registry_reconstruction_requires_rebinding_without_semantic_rotation()
-> Result<(), TestError> {
    let original_registry = registry()?;
    let bindings = ToolExecutorBindings::capture(&original_registry);
    let descriptor = FrozenToolRegistryDescriptor::capture(&original_registry)?;
    let encoded = descriptor.encode_v2()?;
    let decoded = FrozenToolRegistryDescriptor::decode_v2(&encoded)?;
    let reconstructed_registry = decoded.resolve(&bindings)?;
    assert!(!original_registry.is_same_snapshot(&reconstructed_registry));
    let target = target(b"code-mode-durable-rebind")?;
    let original = CodexNativeLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        original_registry,
        target.clone(),
    )?;
    let reconstructed = CodexNativeLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        reconstructed_registry.clone(),
        target,
    )?;

    assert_eq!(original.identity(), reconstructed.identity());
    assert_eq!(
        original.outer_projection().digest(),
        reconstructed.outer_projection().digest()
    );
    assert!(
        reconstructed
            .outer_projection()
            .is_for_registry(&reconstructed_registry)
    );
    assert!(
        reconstructed
            .nested_projection()
            .is_for_registry(&reconstructed_registry)
    );
    for entry in reconstructed.nested_projection().entries() {
        for identity in entry.canonical_operations().values() {
            assert_eq!(
                identity.nested_eligibility(),
                NestedToolEligibility::NestedEligible
            );
        }
    }
    Ok(())
}

#[test]
fn marker_registration_rejects_name_collisions() -> Result<(), TestError> {
    let mut builder = ToolRegistryBuilder::default();
    CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)?;
    let error = CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)
        .err()
        .ok_or("duplicate marker registration must reject")?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Registration);
    Ok(())
}

#[tokio::test]
async fn ordinary_marker_executor_fails_closed_for_driver_mediation() -> Result<(), TestError> {
    let registry = registry()?;
    for (name, arguments) in [
        (CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1, "text('x')"),
        (
            CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
            r#"{"cell_id":"cell-1"}"#,
        ),
    ] {
        let invocation = registry.prepare(request(name, arguments)?)?;
        assert!(
            CodexNativeLocalCodeModeDescriptorV1::is_driver_marker_executor(
                invocation.identity().executor_id()
            )
        );
        let run_id = invocation.run_id();
        let turn_id = invocation.turn_id();
        let attempt_id = invocation.attempt_id();
        let call_id = invocation.call_id();
        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(run_id),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let revision = ToolPolicyRevision::new("code-mode-marker-fail-closed", 1)?;
        let binding = plakous_tools::internal::authorization_binding(
            &invocation,
            environment.descriptor().clone(),
            revision,
        )?;
        let grant = plakous_tools::internal::authorization_grant(
            binding,
            EffectAuthorizationGrantSource::Policy,
        );
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(91, 0)?));
        let (progress, _receiver) = ProgressSink::channel(8, call_id, Arc::clone(&clock))?;
        let context = plakous_tools::internal::tool_context(
            run_id,
            turn_id,
            attempt_id,
            CancellationToken::new(),
            None,
            clock,
            environment,
            grant,
            progress,
        );
        let executor = plakous_tools::internal::executor(&registry, &invocation)?;
        let error = executor
            .execute(invocation, context)
            .await
            .err()
            .ok_or("ordinary marker execution must fail")?;
        assert_eq!(error.kind(), ToolRuntimeErrorKind::Execution);
        assert!(error.message().as_str().contains("driver-mediated"));
    }
    Ok(())
}

struct FixedTool;

impl ToolExecutor for FixedTool {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async { ToolOutcome::success_text("ok") })
    }
}

fn extra_registration(name: &str) -> Result<ToolRegistration, TestError> {
    let definition = ToolDefinition::new(
        ToolName::new(name)?,
        "test-v1",
        format!("Call the exact {name} test capability."),
        ToolInputContract::structured(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    Ok(ToolRegistration::new(
        RegisteredToolId::new(format!("test.{name}.v1"))?,
        ToolExecutorId::new(format!("test.{name}.executor.v1"))?,
        ToolExecutorBindingIdentity::new(1, &[name.as_bytes()])?,
        definition,
        Arc::new(FixedTool),
    )
    .nested_eligible())
}

fn registry_with_extra(name: &str) -> Result<plakous_tools::FrozenToolRegistry, TestError> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)?;
    builder.register(extra_registration(name)?)?;
    Ok(builder.freeze())
}

#[test]
fn exec_description_declares_the_callable_javascript_identifier() -> Result<(), TestError> {
    let name = "mcp__everything__get-sum";
    let descriptor = CodexMinimalLocalCodeModeV1::compile_with_extensions(
        plakous_tools::JobConfig::default(),
        registry_with_extra(name)?,
        target(b"normalized-description")?,
        &[ToolName::new(name)?],
    )?;
    let description = descriptor.exec_description();
    assert!(
        description
            .contains("### `mcp__everything__get-sum` (call `tools.mcp__everything__get_sum`)")
    );
    assert!(description.contains("mcp__everything__get_sum(args: { value: string; })"));
    assert!(!description.contains("mcp__everything__get-sum(args:"));
    Ok(())
}

#[test]
fn empty_product_extensions_preserve_four_tool_minimal_identity() -> Result<(), TestError> {
    let registry = registry()?;
    let target = target(b"codex-minimal-empty-extras")?;
    let compiled = CodexMinimalLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target.clone(),
    )?;
    let with_empty = CodexMinimalLocalCodeModeV1::compile_with_extensions(
        plakous_tools::JobConfig::default(),
        registry,
        target,
        &[],
    )?;
    assert_eq!(compiled.identity(), with_empty.identity());
    assert_eq!(compiled.nested_tools().len(), 4);
    assert_eq!(compiled.extra_nested_names(), [] as [ToolName; 0]);
    Ok(())
}

#[test]
fn explicit_product_extensions_append_to_minimal_catalog_and_rotate_identity()
-> Result<(), TestError> {
    let registry = registry_with_extra("spawn_agent")?;
    let target = target(b"codex-minimal-subagents")?;
    let four = CodexMinimalLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target.clone(),
    )?;
    assert_eq!(four.nested_tools().len(), 4);
    assert!(!four.exec_description().contains("### `spawn_agent`"));

    let extra = [ToolName::new("spawn_agent")?];
    let extended = CodexMinimalLocalCodeModeV1::compile_with_extensions(
        plakous_tools::JobConfig::default(),
        registry,
        target,
        &extra,
    )?;
    assert_eq!(
        extended
            .nested_tools()
            .iter()
            .map(|tool| tool.exposed_name().as_str())
            .collect::<Vec<_>>(),
        [
            "exec_command",
            "write_stdin",
            "apply_patch",
            "update_plan",
            "spawn_agent"
        ]
    );
    assert_eq!(
        extended
            .extra_nested_names()
            .iter()
            .map(ToolName::as_str)
            .collect::<Vec<_>>(),
        ["spawn_agent"]
    );
    assert!(extended.exec_description().contains("### `spawn_agent`"));
    assert_ne!(four.identity(), extended.identity());
    Ok(())
}

#[test]
fn minimal_catalog_can_omit_file_writing_without_losing_plan_or_extensions() -> Result<(), TestError>
{
    let registry = registry_with_extra("submit_result")?;
    let target = target(b"codex-read-only")?;
    let catalog = [
        "exec_command",
        "write_stdin",
        "update_plan",
        "submit_result",
    ]
    .into_iter()
    .map(ToolName::new)
    .collect::<Result<Vec<_>, _>>()?;
    let descriptor = CodexMinimalLocalCodeModeV1::compile_for_catalog(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target.clone(),
        &catalog,
    )?;
    assert_eq!(
        descriptor
            .nested_tools()
            .iter()
            .map(|tool| tool.exposed_name().as_str())
            .collect::<Vec<_>>(),
        [
            "exec_command",
            "write_stdin",
            "update_plan",
            "submit_result"
        ]
    );
    assert!(
        descriptor
            .nested_projection()
            .by_exposed(&ToolName::new("apply_patch")?)
            .is_none()
    );
    assert_eq!(
        descriptor.extra_nested_names(),
        vec![ToolName::new("submit_result")?]
    );
    assert!(!descriptor.exec_description().contains("tools.apply_patch"));
    let mut invalid = catalog.clone();
    invalid.remove(2);
    assert!(
        CodexMinimalLocalCodeModeV1::compile_for_catalog(
            plakous_tools::JobConfig::default(),
            registry,
            target,
            &invalid
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn compile_with_extensions_rejects_recursive_or_core_colliding_names() -> Result<(), TestError> {
    let registry = registry()?;
    let target = target(b"codex-minimal-reject-extras")?;
    let exec = [ToolName::new("exec")?];
    assert!(
        CodexMinimalLocalCodeModeV1::compile_with_extensions(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            target.clone(),
            &exec
        )
        .is_err()
    );
    let core = [ToolName::new("exec_command")?];
    assert!(
        CodexMinimalLocalCodeModeV1::compile_with_extensions(
            plakous_tools::JobConfig::default(),
            registry,
            target,
            &core
        )
        .is_err()
    );
    Ok(())
}

fn extra_registration_with_schema(
    name: &str,
    schema: serde_json::Value,
) -> Result<ToolRegistration, TestError> {
    let definition = ToolDefinition::new(
        ToolName::new(name)?,
        "test-v1",
        format!("Call the exact {name} test capability."),
        ToolInputContract::structured(schema)?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    Ok(ToolRegistration::new(
        RegisteredToolId::new(format!("test.{name}.v1"))?,
        ToolExecutorId::new(format!("test.{name}.executor.v1"))?,
        ToolExecutorBindingIdentity::new(1, &[name.as_bytes()])?,
        definition,
        Arc::new(FixedTool),
    )
    .nested_eligible())
}

#[test]
fn compile_with_extensions_admits_required_minitems_array_fields() -> Result<(), TestError> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)?;
    builder.register(extra_registration_with_schema(
        "wait_agent",
        serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "agent_ids": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            },
            "required": ["agent_ids"],
            "additionalProperties": false
        }),
    )?)?;
    let registry = builder.freeze();
    let target = target(b"codex-minimal-minitems")?;
    let extra = [ToolName::new("wait_agent")?];
    let descriptor = CodexMinimalLocalCodeModeV1::compile_with_extensions(
        plakous_tools::JobConfig::default(),
        registry,
        target,
        &extra,
    )?;
    assert!(
        descriptor
            .nested_tools()
            .iter()
            .any(|tool| tool.exposed_name().as_str() == "wait_agent")
    );
    Ok(())
}

/// A marker cannot enter the nested catalog through either public compile
/// path, and the refusal is the executor-identity one.
#[test]
fn markers_are_refused_nested_by_identity_on_every_path() -> Result<(), TestError> {
    for marker in ["exec", "wait"] {
        let extras = [ToolName::new(marker)?];
        let error = CodexMinimalLocalCodeModeV1::compile_with_extensions(
            plakous_tools::JobConfig::default(),
            registry()?,
            target(b"nested-marker-extensions")?,
            &extras,
        )
        .err()
        .ok_or("a marker extra must be refused")?;
        // `exec` is CustomUtf8, so the structured-input refusal precedes the
        // identity one; `wait` reaches the executor-identity refusal.
        let expected = if marker == "exec" {
            "nested coding capability is not structured"
        } else {
            "composition outer markers cannot enter a nested catalog"
        };
        assert_eq!(error.to_string(), expected, "{marker}");
        let catalog = [
            ToolName::new("exec_command")?,
            ToolName::new("write_stdin")?,
            ToolName::new("update_plan")?,
            ToolName::new(marker)?,
        ];
        let error = CodexMinimalLocalCodeModeV1::compile_for_catalog(
            plakous_tools::JobConfig::default(),
            registry()?,
            target(b"nested-marker-catalog")?,
            &catalog,
        )
        .err()
        .ok_or("a marker catalog member must be refused")?;
        // `exec` is CustomUtf8, so the structured-input refusal precedes the
        // identity one; `wait` reaches the executor-identity refusal.
        let expected = if marker == "exec" {
            "nested coding capability is not structured"
        } else {
            "composition outer markers cannot enter a nested catalog"
        };
        assert_eq!(error.to_string(), expected, "{marker}");
    }
    // A non-marker tool cannot even be registered under a marker spelling
    // beside the markers, so no nested `wait`/`exec` of another slot exists.
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)?;
    assert!(builder.register(extra_registration("wait")?).is_err());
    // Outer markers stay the marker executors.
    let descriptor = descriptor()?;
    for entry in descriptor.outer_projection().entries() {
        assert!(entry.canonical_operations().values().all(|identity| {
            plakous_tools::is_composition_marker_executor(identity.executor_id())
        }));
    }
    for entry in descriptor.nested_projection().entries() {
        assert!(entry.canonical_operations().values().all(|identity| {
            !plakous_tools::is_composition_marker_executor(identity.executor_id())
        }));
    }
    Ok(())
}
