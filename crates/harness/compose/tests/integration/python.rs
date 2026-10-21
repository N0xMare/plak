//! Backend-independent Python profile compilation and fail-closed catalog evidence.

use std::str::FromStr;
use std::sync::Arc;

use plakous_compose::{
    CodexNativeLocalCodeModeV1, PYTHON_PTC_PROFILE_V1, PYTHON_PTC_RESULT_GRAMMAR_V1,
    ProductJavaScriptCompositionV1, PythonPtcLimitsV1, PythonPtcPresentationV1, PythonPtcV1,
};
use plakous_model::{
    AttemptId, CallId, Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass,
    ProjectedToolPresentationV2, RunId, ToolConcurrency, ToolCorrelation, ToolDefinition,
    ToolEffect, ToolInputContract, ToolName, ToolOutputContract, TurnId,
};
use plakous_tools::{
    CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1, CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1,
    CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1, PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1,
    PYTHON_PTC_RUN_NAME_V1, RegisteredToolId, ToolExecutor, ToolExecutorBindingIdentity,
    ToolExecutorId, ToolFuture, ToolInvocation, ToolInvocationRequest, ToolOutcome,
    ToolRegistration, ToolRegistryBuilder, ToolRuntimeErrorKind,
};
use serde_json::json;

type TestError = Box<dyn std::error::Error>;

#[derive(Debug)]
struct FixedTool;

impl ToolExecutor for FixedTool {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        _context: plakous_tools::ToolContext,
    ) -> ToolFuture {
        Box::pin(async { ToolOutcome::success_text("ok") })
    }
}

fn configuration(seed: &[u8]) -> Result<EffectiveProviderConfigurationId, TestError> {
    Ok(serde_json::from_value(serde_json::to_value(
        Digest::sha256(DigestDomain::EffectiveProviderConfiguration, 5, &[seed])?,
    )?)?)
}

fn target(seed: &[u8]) -> Result<plakous_tools::ProviderModelProjectionIdentityV2, TestError> {
    Ok(plakous_tools::ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Custom,
        "custom.responses",
        "consumer.route",
        "consumer.python-composition.v1",
        "responses-family",
        "candidate-model",
        "candidate-model",
        configuration(seed)?,
        1,
    )?)
}

fn registration(
    name: &str,
    executor: &str,
    nested: bool,
    structured: bool,
) -> Result<ToolRegistration, TestError> {
    let input = if structured {
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        }))?
    } else {
        ToolInputContract::utf8(1024)?
    };
    let definition = ToolDefinition::new(
        ToolName::new(name)?,
        "test-v1",
        format!("Call the exact {name} test capability."),
        input,
        ToolOutputContract::utf8(4096)?,
        ToolConcurrency::Parallel,
        ToolEffect::ReadOnly,
    )?;
    let registration = ToolRegistration::new(
        RegisteredToolId::new(format!("test.{name}.v1"))?,
        ToolExecutorId::new(executor)?,
        ToolExecutorBindingIdentity::new(1, &[name.as_bytes(), executor.as_bytes()])?,
        definition,
        Arc::new(FixedTool),
    );
    Ok(if nested {
        registration.nested_eligible()
    } else {
        registration
    })
}

fn registry(
    registrations: impl IntoIterator<Item = ToolRegistration>,
) -> Result<plakous_tools::FrozenToolRegistry, TestError> {
    let mut builder = ToolRegistryBuilder::default();
    for registration in registrations {
        builder.register(registration)?;
    }
    PythonPtcV1::register_markers_into(&mut builder)?;
    Ok(builder.freeze())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the positive contract test intentionally verifies every public axis together"
)]
fn structured_projection_is_exact_ordered_and_backend_independent() -> Result<(), TestError> {
    let registry = registry([
        registration("read-local", "test.read.v1", true, true)?,
        registration("class", "test.class.v1", true, true)?,
    ])?;
    let descriptor = PythonPtcV1::compile(
        registry.clone(),
        target(b"structured")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("class")?, ToolName::new("read-local")?],
    )?;

    assert_eq!(PYTHON_PTC_PROFILE_V1, "PythonPtcV1");
    assert_eq!(
        descriptor
            .outer_projection()
            .entries()
            .iter()
            .map(|entry| entry.exposed_name().as_str())
            .collect::<Vec<_>>(),
        [PYTHON_PTC_RUN_NAME_V1, "wait_py"]
    );
    assert!(
        descriptor
            .outer_projection()
            .entries()
            .iter()
            .all(|entry| matches!(
                entry.presentation(),
                ProjectedToolPresentationV2::StructuredFunction { .. }
            ))
    );
    assert_eq!(
        descriptor
            .nested_tools()
            .iter()
            .map(|tool| (tool.canonical_name().as_str(), tool.python_name()))
            .collect::<Vec<_>>(),
        [("class", "class_"), ("read-local", "read_local")]
    );
    assert!(
        descriptor
            .authoring_contract()
            .contains("top-level `await`")
    );
    assert!(descriptor.authoring_contract().contains("asyncio.gather"));
    assert!(
        descriptor
            .authoring_contract()
            .contains("Required JSON argument: code (string, not source)")
    );
    assert!(
        descriptor
            .authoring_contract()
            .contains("structured tools take a dictionary; freeform tools take the string")
    );
    assert!(
        descriptor
            .authoring_contract()
            .contains("returns a raw UTF-8 string")
    );
    assert!(descriptor.authoring_contract().contains("persist"));
    assert!(descriptor.authoring_contract().contains("observed reset"));
    assert!(
        descriptor
            .authoring_contract()
            .contains("never replays source")
    );
    assert!(
        descriptor
            .authoring_contract()
            .contains("catchable Python exceptions")
    );
    assert!(
        descriptor
            .authoring_contract()
            .contains("sanitized UTF-8 text")
    );
    assert!(
        descriptor
            .authoring_contract()
            .contains("not hosted code execution")
    );
    assert!(
        descriptor
            .authoring_contract()
            .find("`class`")
            .ok_or("class catalog entry absent")?
            < descriptor
                .authoring_contract()
                .find("`read-local`")
                .ok_or("read catalog entry absent")?
    );
    for tool in descriptor.nested_tools() {
        let projection = descriptor
            .nested_projection()
            .by_exposed(tool.canonical_name())
            .ok_or("nested projection absent")?;
        assert_eq!(projection.identity(), tool.projection_identity());
        assert!(projection.input_schema().is_some());
    }
    for marker in [
        PYTHON_PTC_RUN_NAME_V1,
        "plakous_python_custom_run_marker",
        "wait_py",
    ] {
        assert_eq!(
            registry.nested_eligibility(&ToolName::new(marker)?),
            Some(plakous_tools::NestedToolEligibility::DirectOnly)
        );
    }
    Ok(())
}

#[test]
fn structured_schemas_bind_code_and_output_and_unknown_field_refusal() -> Result<(), TestError> {
    let descriptor = PythonPtcV1::compile(
        registry([registration("read", "test.read.v1", true, true)?])?,
        target(b"schemas")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("read")?],
    )?;
    let run = descriptor
        .outer_projection()
        .by_exposed(&ToolName::new(PYTHON_PTC_RUN_NAME_V1)?)
        .ok_or("run absent")?;
    let run_schema = run
        .input_schema()
        .ok_or("run must be structured")?
        .document()
        .value();
    assert_eq!(run_schema["required"], json!(["code"]));
    assert_eq!(run_schema["additionalProperties"], json!(false));
    assert_eq!(
        run_schema["properties"]["code"]["minLength"].as_f64(),
        Some(1.0)
    );
    assert!(run_schema["properties"]["code"].get("maxLength").is_none());
    assert_eq!(
        run_schema["properties"]["yield_time_ms"]["maximum"].as_f64(),
        Some(10_000.0)
    );
    assert_eq!(
        run_schema["properties"]["max_output_bytes"]["maximum"].as_f64(),
        Some(1_048_576.0)
    );
    assert_eq!(descriptor.outer_projection().entries().len(), 2);
    Ok(())
}

#[test]
fn custom_run_is_raw_bounded_utf8() -> Result<(), TestError> {
    let registry = registry([registration("read", "test.read.v1", true, true)?])?;
    let structured = PythonPtcV1::compile(
        registry.clone(),
        target(b"same-target")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("read")?],
    )?;
    let custom = PythonPtcV1::compile(
        registry,
        target(b"same-target")?,
        PythonPtcPresentationV1::CustomRunFunctionWait,
        [ToolName::new("read")?],
    )?;
    let run = custom
        .outer_projection()
        .by_exposed(&ToolName::new(PYTHON_PTC_RUN_NAME_V1)?)
        .ok_or("custom run absent")?;
    let ProjectedToolPresentationV2::CustomUtf8 { input, format } = run.presentation() else {
        return Err("run was not custom UTF-8".into());
    };
    assert_eq!(input.max_bytes(), 262_144);
    assert_eq!(format.definition(), "start: /(?s).+/");
    assert!(run.description().contains("raw UTF-8 source"));
    let source = "print('λ')\n";
    let prepared = custom
        .outer_projection()
        .prepare(ToolInvocationRequest::new(
            RunId::from_str("00000000-0000-0000-0000-000000000301")?,
            TurnId::from_str("00000000-0000-0000-0000-000000000302")?,
            AttemptId::from_str("00000000-0000-0000-0000-000000000303")?,
            CallId::from_str("00000000-0000-0000-0000-000000000304")?,
            ToolName::new(PYTHON_PTC_RUN_NAME_V1)?,
            ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default())?,
            source,
        ))?;
    assert_eq!(
        prepared.invocation().arguments().value().as_str(),
        Some(source)
    );
    assert_eq!(custom.outer_projection().entries().len(), 2);
    assert_eq!(structured.profile_identity(), custom.profile_identity());
    assert_eq!(structured.catalog_identity(), custom.catalog_identity());
    assert_eq!(structured.result_identity(), custom.result_identity());
    assert_eq!(structured.limits_identity(), custom.limits_identity());
    assert_ne!(
        structured.presentation_identity(),
        custom.presentation_identity()
    );
    assert_ne!(structured.identity(), custom.identity());
    Ok(())
}

#[test]
fn limits_result_and_component_identities_are_explicit_and_exact() -> Result<(), TestError> {
    let descriptor = PythonPtcV1::compile(
        registry([registration("read", "test.read.v1", true, true)?])?,
        target(b"identities")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("read")?],
    )?;
    assert_eq!(descriptor.limits(), PythonPtcLimitsV1::EXACT);
    assert_eq!(descriptor.limits().source_bytes, 262_144);
    assert_eq!(descriptor.limits().worker_request_ms, 90_000);
    assert_eq!(descriptor.limits().engine_memory_bytes, 268_435_456);
    assert_eq!(descriptor.limits().stdout_bytes, 1_048_576);
    assert_eq!(descriptor.limits().stderr_bytes, 1_048_576);
    assert_eq!(descriptor.limits().final_result_bytes, 1_048_576);
    assert_eq!(descriptor.limits().nested_calls, 100);
    assert_eq!(descriptor.limits().concurrent_nested_calls, 16);
    assert_eq!(descriptor.limits().nested_argument_bytes, 1_048_576);
    assert_eq!(descriptor.limits().nested_result_bytes, 1_048_576);
    assert_eq!(
        descriptor.limits().aggregate_nested_result_bytes,
        16_777_216
    );
    assert_eq!(descriptor.limits().nesting_depth, 1);
    assert_eq!(descriptor.result_grammar(), PYTHON_PTC_RESULT_GRAMMAR_V1);
    for fact in [
        "Python cell running with cell ID",
        "Python cell terminated",
        "Python cell completed",
        "Python cell failed",
        "Python interpreter reset",
        "bounded_utf8",
        "python_exception",
        "sanitized_utf8_failure",
        "without_source_replay",
    ] {
        assert!(descriptor.result_grammar().contains(fact));
    }
    let identities = [
        descriptor.profile_identity(),
        descriptor.presentation_identity(),
        descriptor.result_identity(),
        descriptor.catalog_identity(),
        descriptor.limits_identity(),
        descriptor.identity(),
    ];
    assert!(
        identities
            .iter()
            .all(|identity| identity.domain() == DigestDomain::ToolDefinition)
    );
    assert!(
        identities
            .iter()
            .all(|identity| identity.bytes() != &[0; 32])
    );
    assert!(identities.iter().enumerate().all(|(index, identity)| {
        identities[index + 1..]
            .iter()
            .all(|other| identity != other)
    }));
    Ok(())
}

#[test]
fn catalog_order_target_and_presentation_rotate_only_the_bound_identities() -> Result<(), TestError>
{
    let registry = registry([
        registration("alpha", "test.alpha.v1", true, true)?,
        registration("beta", "test.beta.v1", true, true)?,
    ])?;
    let first = PythonPtcV1::compile(
        registry.clone(),
        target(b"target-a")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("alpha")?, ToolName::new("beta")?],
    )?;
    let same = PythonPtcV1::compile(
        registry.clone(),
        target(b"target-a")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("alpha")?, ToolName::new("beta")?],
    )?;
    let reversed = PythonPtcV1::compile(
        registry.clone(),
        target(b"target-a")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("beta")?, ToolName::new("alpha")?],
    )?;
    let other_target = PythonPtcV1::compile(
        registry,
        target(b"target-b")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("alpha")?, ToolName::new("beta")?],
    )?;
    assert_eq!(first.identity(), same.identity());
    assert_eq!(first.catalog_identity(), same.catalog_identity());
    assert_ne!(first.catalog_identity(), reversed.catalog_identity());
    assert_ne!(first.identity(), reversed.identity());
    assert_ne!(first.catalog_identity(), other_target.catalog_identity());
    assert_ne!(first.identity(), other_target.identity());
    assert_eq!(first.limits_identity(), other_target.limits_identity());
    Ok(())
}

#[test]
fn invalid_catalogs_fail_closed_before_any_execution() -> Result<(), TestError> {
    let reserved = PythonPtcV1::compile(
        registry([registration(
            "_plakous_host_0",
            "test.reserved.v1",
            true,
            true,
        )?])?,
        target(b"reserved")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("_plakous_host_0")?],
    )
    .err()
    .ok_or("reserved host binding prefix accepted")?;
    assert!(
        reserved
            .message()
            .as_str()
            .contains("reserved host binding")
    );

    let empty = PythonPtcV1::compile(
        registry([])?,
        target(b"empty")?,
        PythonPtcPresentationV1::StructuredFunctions,
        std::iter::empty::<ToolName>(),
    )
    .err()
    .ok_or("empty catalog accepted")?;
    assert_eq!(empty.kind(), ToolRuntimeErrorKind::Registration);

    let direct = PythonPtcV1::compile(
        registry([registration("direct", "test.direct.v1", false, true)?])?,
        target(b"direct")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("direct")?],
    )
    .err()
    .ok_or("direct-only tool accepted")?;
    assert!(direct.message().as_str().contains("nested-eligible"));

    let freeform = PythonPtcV1::compile(
        registry([registration("freeform", "test.freeform.v1", true, false)?])?,
        target(b"freeform")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("freeform")?],
    )
    .err()
    .ok_or("non-dictionary tool accepted")?;
    assert!(
        freeform
            .message()
            .as_str()
            .contains("structured dictionary")
    );

    let missing = PythonPtcV1::compile(
        registry([])?,
        target(b"missing")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("missing")?],
    )
    .err()
    .ok_or("missing tool accepted")?;
    assert!(missing.message().as_str().contains("binding is missing"));

    let duplicate = PythonPtcV1::compile(
        registry([registration("same", "test.same.v1", true, true)?])?,
        target(b"duplicate")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("same")?, ToolName::new("same")?],
    )
    .err()
    .ok_or("duplicate tool accepted")?;
    assert!(duplicate.message().as_str().contains("duplicate"));

    let collision = PythonPtcV1::compile(
        registry([
            registration("same-name", "test.hyphen.v1", true, true)?,
            registration("same_name", "test.underscore.v1", true, true)?,
        ])?,
        target(b"collision")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new("same-name")?, ToolName::new("same_name")?],
    )
    .err()
    .ok_or("Python identifier collision accepted")?;
    assert!(
        collision
            .message()
            .as_str()
            .contains("Python normalization")
    );

    let many = (0..65)
        .map(|index| ToolName::new(format!("tool_{index}")))
        .collect::<Result<Vec<_>, _>>()?;
    let too_many = PythonPtcV1::compile(
        registry([])?,
        target(b"many")?,
        PythonPtcPresentationV1::StructuredFunctions,
        many,
    )
    .err()
    .ok_or("oversized catalog accepted")?;
    assert!(too_many.message().as_str().contains("one through 64"));
    Ok(())
}

#[test]
fn python_signatures_preserve_freeform_and_structured_input_kinds() -> Result<(), TestError> {
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    PythonPtcV1::register_markers_into(&mut builder)?;
    let descriptor = PythonPtcV1::compile_for_kind(
        plakous_tools::JobConfig::default(),
        builder.freeze(),
        target(b"python-freeform-signature")?,
        PythonPtcPresentationV1::StructuredFunctions,
        plakous_tools::CodingPresentation::Codex,
        [
            ToolName::new("apply_patch")?,
            ToolName::new("exec_command")?,
        ],
    )?;
    assert!(descriptor.nested_tools()[0].string_input());
    assert!(!descriptor.nested_tools()[1].string_input());
    assert_eq!(
        descriptor.nested_tools()[0].composition_input_schema_value(),
        json!({"type": "string"})
    );
    assert!(
        descriptor
            .authoring_contract()
            .contains("async def apply_patch(arguments: str) -> str")
    );
    assert!(
        descriptor
            .authoring_contract()
            .contains("async def exec_command(arguments: ")
    );
    assert!(descriptor.authoring_contract().contains(") -> dict"));
    Ok(())
}

#[test]
fn every_known_composition_marker_executor_is_recursive_and_rejected() -> Result<(), TestError> {
    for (index, executor) in [
        PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1,
        CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1,
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("recursive_{index}");
        let error = PythonPtcV1::compile(
            registry([registration(&name, executor, true, true)?])?,
            target(name.as_bytes())?,
            PythonPtcPresentationV1::StructuredFunctions,
            [ToolName::new(name)?],
        )
        .err()
        .ok_or("recursive marker accepted")?;
        assert!(error.message().as_str().contains("outer markers"));
    }
    let own_marker = PythonPtcV1::compile(
        registry([])?,
        target(b"own-marker")?,
        PythonPtcPresentationV1::StructuredFunctions,
        [ToolName::new(PYTHON_PTC_RUN_NAME_V1)?],
    )
    .err()
    .ok_or("own outer marker accepted")?;
    assert!(own_marker.message().as_str().contains("nested-eligible"));
    Ok(())
}

#[test]
fn enrolling_python_markers_preserves_javascript_and_codex_projection_bytes()
-> Result<(), TestError> {
    fn coding_registry(with_python: bool) -> Result<plakous_tools::FrozenToolRegistry, TestError> {
        let mut builder = ToolRegistryBuilder::default();
        plakous_tools::register_packs(
            &plakous_compose::standard_coding_packs_in_memory()?,
            &mut builder,
        )?;
        CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)?;
        ProductJavaScriptCompositionV1::register_markers_into(&mut builder)?;
        if with_python {
            PythonPtcV1::register_markers_into(&mut builder)?;
        }
        Ok(builder.freeze())
    }
    let baseline = CodexNativeLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        coding_registry(false)?,
        target(b"codex")?,
    )?
    .outer_projection()
    .compile_request_view_v2()?;
    let enrolled = CodexNativeLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        coding_registry(true)?,
        target(b"codex")?,
    )?
    .outer_projection()
    .compile_request_view_v2()?;
    assert_eq!(
        serde_json::to_vec(baseline.definitions())?,
        serde_json::to_vec(enrolled.definitions())?
    );
    assert_eq!(
        baseline
            .definitions()
            .iter()
            .map(|item| item.exposed_name().as_str())
            .collect::<Vec<_>>(),
        [
            CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1,
            CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1
        ]
    );

    let mut builder = ToolRegistryBuilder::default();
    builder.register(registration("read", "test.read.v1", true, true)?)?;
    ProductJavaScriptCompositionV1::register_markers_into(&mut builder)?;
    let js_baseline = ProductJavaScriptCompositionV1::compile(
        builder.freeze(),
        target(b"js")?,
        [ToolName::new("read")?],
    )?
    .outer_projection()
    .compile_request_view_v2()?;
    let mut builder = ToolRegistryBuilder::default();
    builder.register(registration("read", "test.read.v1", true, true)?)?;
    ProductJavaScriptCompositionV1::register_markers_into(&mut builder)?;
    PythonPtcV1::register_markers_into(&mut builder)?;
    let js_enrolled = ProductJavaScriptCompositionV1::compile(
        builder.freeze(),
        target(b"js")?,
        [ToolName::new("read")?],
    )?
    .outer_projection()
    .compile_request_view_v2()?;
    assert_eq!(
        serde_json::to_vec(js_baseline.definitions())?,
        serde_json::to_vec(js_enrolled.definitions())?
    );
    Ok(())
}
