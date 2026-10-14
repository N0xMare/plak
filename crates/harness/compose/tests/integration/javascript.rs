//! Product-owned JavaScript profile compilation and fail-closed catalog evidence.

use std::sync::Arc;

use plakous_compose::{
    CodexNativeLocalCodeModeV1, PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_REVISION_V1,
    PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_V1, ProductJavaScriptCompositionV1,
};
use plakous_model::{
    AttemptId, CallId, Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass,
    ProjectedToolPresentationV2, RunId, ToolConcurrency, ToolCorrelation, ToolDefinition,
    ToolEffect, ToolInputContract, ToolName, ToolOutputContract, TurnId,
};
use plakous_tools::{
    CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1, CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1,
    CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1, NestedToolEligibility, PRODUCT_JAVASCRIPT_RUN_NAME_V1,
    PRODUCT_JAVASCRIPT_WAIT_NAME_V1, PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1, RegisteredToolId,
    ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation,
    ToolInvocationRequest, ToolOutcome, ToolRegistration, ToolRegistryBuilder,
    ToolRuntimeErrorKind,
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

fn target(
    class: IntegrationClass,
    seed: &[u8],
) -> Result<plakous_tools::ProviderModelProjectionIdentityV2, TestError> {
    Ok(plakous_tools::ProviderModelProjectionIdentityV2::new(
        class,
        "xai.grok.responses",
        "xai.grok_subscription",
        "xai.grok.product-javascript.v1",
        "responses-family",
        "grok-4.6",
        "grok-4.6",
        configuration(seed)?,
        1,
    )?)
}

fn registration(name: &str, executor: &str, nested: bool) -> Result<ToolRegistration, TestError> {
    let definition = ToolDefinition::new(
        ToolName::new(name)?,
        "test-v1",
        format!("Call the exact {name} test capability."),
        ToolInputContract::structured(json!({
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
    ProductJavaScriptCompositionV1::register_markers_into(&mut builder)?;
    Ok(builder.freeze())
}

#[test]
fn grok_target_gets_ordinary_function_outer_projection_and_ordered_nested_catalog()
-> Result<(), TestError> {
    let registry = registry([
        registration("read_local", "test.read.v1", true)?,
        registration("mutate_local", "test.mutate.v1", true)?,
    ])?;
    let descriptor = ProductJavaScriptCompositionV1::compile(
        registry.clone(),
        target(IntegrationClass::Standard, b"grok-standard")?,
        [ToolName::new("mutate_local")?, ToolName::new("read_local")?],
    )?;

    assert_eq!(
        descriptor
            .outer_projection()
            .entries()
            .iter()
            .map(|entry| entry.exposed_name().as_str())
            .collect::<Vec<_>>(),
        [
            PRODUCT_JAVASCRIPT_RUN_NAME_V1,
            PRODUCT_JAVASCRIPT_WAIT_NAME_V1
        ]
    );
    for entry in descriptor.outer_projection().entries() {
        assert!(matches!(
            entry.presentation(),
            ProjectedToolPresentationV2::StructuredFunction { .. }
        ));
        assert!(
            ProductJavaScriptCompositionV1::compile(
                registry.clone(),
                entry.target().clone(),
                [ToolName::new("read_local")?]
            )
            .is_ok()
        );
    }
    let run = descriptor
        .outer_projection()
        .by_exposed(&ToolName::new(PRODUCT_JAVASCRIPT_RUN_NAME_V1)?)
        .ok_or("run function absent")?;
    assert_eq!(run.description(), descriptor.run_description());
    assert!(
        descriptor
            .run_description()
            .contains("Required JSON argument: source (string, not code)")
    );
    assert!(descriptor.run_description().contains("text(...)"));
    assert_eq!(
        descriptor
            .nested_tools()
            .iter()
            .map(|tool| tool.exposed_name().as_str())
            .collect::<Vec<_>>(),
        ["mutate_local", "read_local"]
    );
    for tool in descriptor.nested_tools() {
        assert!(descriptor.run_description().contains(&format!(
            "- `{}`: {}",
            tool.exposed_name(),
            tool.description()
        )));
        assert!(descriptor.run_description().contains(&format!(
            "tools.{}(args: {{ value: string; }}): Promise<string>",
            tool.exposed_name()
        )));
    }
    assert!(
        descriptor
            .run_description()
            .find("`mutate_local`")
            .ok_or("mutate signature absent")?
            < descriptor
                .run_description()
                .find("`read_local`")
                .ok_or("read signature absent")?
    );
    for tool in descriptor.nested_tools() {
        let entry = descriptor
            .nested_projection()
            .by_exposed(tool.exposed_name())
            .ok_or("nested projection missing")?;
        assert_eq!(entry.identity(), tool.projection_identity());
        assert!(entry.canonical_operations().values().all(|identity| {
            identity.nested_eligibility() == NestedToolEligibility::NestedEligible
        }));
    }
    for marker in [
        PRODUCT_JAVASCRIPT_RUN_NAME_V1,
        PRODUCT_JAVASCRIPT_WAIT_NAME_V1,
    ] {
        assert_eq!(
            registry.nested_eligibility(&ToolName::new(marker)?),
            Some(NestedToolEligibility::DirectOnly)
        );
    }
    Ok(())
}

#[test]
fn outer_schemas_require_non_empty_source_and_cell_without_claiming_source_bytes()
-> Result<(), TestError> {
    let registry = registry([registration("read_local", "test.read.v1", true)?])?;
    let descriptor = ProductJavaScriptCompositionV1::compile(
        registry,
        target(IntegrationClass::Standard, b"outer-schema")?,
        [ToolName::new("read_local")?],
    )?;
    let run = descriptor
        .outer_projection()
        .by_exposed(&ToolName::new(PRODUCT_JAVASCRIPT_RUN_NAME_V1)?)
        .ok_or("run function absent")?;
    let schema = run.input_schema().ok_or("run must be structured")?;
    assert_eq!(schema.document().value()["required"], json!(["source"]));
    assert_eq!(
        schema.document().value()["properties"]["source"]["minLength"].as_f64(),
        Some(1.0)
    );
    assert!(
        schema.document().value()["properties"]["source"]
            .get("maxLength")
            .is_none(),
        "JSON Schema character length must not claim runtime byte enforcement"
    );
    let wait = descriptor
        .outer_projection()
        .by_exposed(&ToolName::new(PRODUCT_JAVASCRIPT_WAIT_NAME_V1)?)
        .ok_or("wait function absent")?;
    assert_eq!(
        wait.input_schema()
            .ok_or("wait must be structured")?
            .document()
            .value()["properties"]["cell_id"]["minLength"]
            .as_f64(),
        Some(1.0)
    );
    let wait_schema = wait.input_schema().ok_or("wait must be structured")?;
    assert_eq!(
        wait_schema.document().value()["properties"]["yield_time_ms"]["maximum"].as_f64(),
        Some(600_000.0)
    );
    assert_eq!(
        wait_schema.document().value()["properties"]["max_tokens"]["maximum"].as_f64(),
        Some(10_000.0)
    );
    assert_eq!(
        schema.document().value()["properties"]["yield_time_ms"]["maximum"].as_f64(),
        Some(10_000.0)
    );
    assert_eq!(
        schema.document().value()["properties"]["max_output_tokens"]["maximum"].as_f64(),
        Some(10_000.0)
    );
    Ok(())
}

#[test]
fn result_grammar_revision_is_explicit_complete_and_descriptor_bound() -> Result<(), TestError> {
    let registry = registry([registration("read_local", "test.read.v1", true)?])?;
    let descriptor = ProductJavaScriptCompositionV1::compile(
        registry,
        target(IntegrationClass::Standard, b"result-grammar")?,
        [ToolName::new("read_local")?],
    )?;
    assert_eq!(
        descriptor.result_grammar_revision(),
        PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_REVISION_V1
    );
    assert_eq!(
        descriptor.result_grammar(),
        PRODUCT_JAVASCRIPT_RESULT_GRAMMAR_V1
    );
    for required_fact in [
        "Script running with cell ID",
        "Script terminated",
        "Script completed",
        "Script failed",
        "Wall time",
        "max_output_tokens truncation",
        "text(",
        "image(",
        "audio(",
        "Script error:",
    ] {
        assert!(
            descriptor.result_grammar().contains(required_fact),
            "result grammar omitted {required_fact}"
        );
    }
    assert_ne!(descriptor.identity().bytes(), &[0; 32]);
    Ok(())
}

#[test]
fn custom_targets_are_admitted_but_identity_and_catalog_order_stay_exact() -> Result<(), TestError>
{
    let registry = registry([
        registration("alpha", "test.alpha.v1", true)?,
        registration("beta", "test.beta.v1", true)?,
    ])?;
    let custom = target(IntegrationClass::Custom, b"custom-target")?;
    let first = ProductJavaScriptCompositionV1::compile(
        registry.clone(),
        custom.clone(),
        [ToolName::new("alpha")?, ToolName::new("beta")?],
    )?;
    let reversed = ProductJavaScriptCompositionV1::compile(
        registry.clone(),
        custom.clone(),
        [ToolName::new("beta")?, ToolName::new("alpha")?],
    )?;
    let same = ProductJavaScriptCompositionV1::compile(
        registry,
        custom,
        [ToolName::new("alpha")?, ToolName::new("beta")?],
    )?;
    assert_eq!(
        first.outer_projection().target().integration_class(),
        IntegrationClass::Custom
    );
    assert_ne!(first.identity(), reversed.identity());
    assert_eq!(first.identity(), same.identity());
    assert_eq!(first.run_description(), same.run_description());
    assert_ne!(first.run_description(), reversed.run_description());
    assert!(
        first
            .run_description()
            .find("`alpha`")
            .ok_or("alpha signature absent")?
            < first
                .run_description()
                .find("`beta`")
                .ok_or("beta signature absent")?
    );
    assert!(
        reversed
            .run_description()
            .find("`beta`")
            .ok_or("beta reversed signature absent")?
            < reversed
                .run_description()
                .find("`alpha`")
                .ok_or("alpha reversed signature absent")?
    );
    assert_eq!(
        first.nested_projection().digest(),
        reversed.nested_projection().digest(),
        "projection map is set-ordered while descriptor identity binds engine order"
    );
    Ok(())
}

#[test]
fn missing_direct_only_duplicate_collision_and_recursive_members_fail_closed()
-> Result<(), TestError> {
    let error = ProductJavaScriptCompositionV1::compile(
        registry([])?,
        target(IntegrationClass::Standard, b"empty")?,
        std::iter::empty::<ToolName>(),
    )
    .err()
    .ok_or("empty catalog must reject")?;
    assert!(error.message().as_str().contains("1..=64"));

    let direct_registry = registry([registration("direct_only", "test.direct.v1", false)?])?;
    let error = ProductJavaScriptCompositionV1::compile(
        direct_registry,
        target(IntegrationClass::Standard, b"direct-only")?,
        [ToolName::new("direct_only")?],
    )
    .err()
    .ok_or("direct-only member must reject")?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Registration);
    assert!(error.message().as_str().contains("nested-eligible"));

    let missing_registry = registry([])?;
    let error = ProductJavaScriptCompositionV1::compile(
        missing_registry,
        target(IntegrationClass::Standard, b"missing")?,
        [ToolName::new("missing_tool")?],
    )
    .err()
    .ok_or("missing member must reject")?;
    assert!(error.message().as_str().contains("binding is missing"));

    let duplicate_registry = registry([registration("same", "test.same.v1", true)?])?;
    let error = ProductJavaScriptCompositionV1::compile(
        duplicate_registry,
        target(IntegrationClass::Standard, b"duplicate")?,
        [ToolName::new("same")?, ToolName::new("same")?],
    )
    .err()
    .ok_or("duplicate member must reject")?;
    assert!(error.message().as_str().contains("duplicate"));

    let collision_registry = registry([
        registration("same-name", "test.hyphen.v1", true)?,
        registration("same_name", "test.underscore.v1", true)?,
    ])?;
    let error = ProductJavaScriptCompositionV1::compile(
        collision_registry,
        target(IntegrationClass::Standard, b"normalized-collision")?,
        [ToolName::new("same-name")?, ToolName::new("same_name")?],
    )
    .err()
    .ok_or("normalized collision must reject")?;
    assert!(error.message().as_str().contains("normalization"));

    let recursive_registry = registry([registration(
        "recursive_alias",
        CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1,
        true,
    )?])?;
    let error = ProductJavaScriptCompositionV1::compile(
        recursive_registry,
        target(IntegrationClass::Standard, b"recursive")?,
        [ToolName::new("recursive_alias")?],
    )
    .err()
    .ok_or("recursive marker executor must reject")?;
    assert!(error.message().as_str().contains("outer markers"));

    let recursive_registry = registry([registration(
        "python_recursive_alias",
        PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1,
        true,
    )?])?;
    let error = ProductJavaScriptCompositionV1::compile(
        recursive_registry,
        target(IntegrationClass::Standard, b"python-recursive")?,
        [ToolName::new("python_recursive_alias")?],
    )
    .err()
    .ok_or("Python composition marker executor must reject")?;
    assert!(error.message().as_str().contains("outer markers"));

    let freeform_definition = ToolDefinition::new(
        ToolName::new("freeform")?,
        "test-v1",
        "Call a freeform test capability.",
        ToolInputContract::utf8(1024)?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let freeform_registration = ToolRegistration::new(
        RegisteredToolId::new("test.freeform.v1")?,
        ToolExecutorId::new("test.freeform-executor.v1")?,
        ToolExecutorBindingIdentity::new(1, &[b"freeform"])?,
        freeform_definition,
        Arc::new(FixedTool),
    )
    .nested_eligible();
    let error = ProductJavaScriptCompositionV1::compile(
        registry([freeform_registration])?,
        target(IntegrationClass::Standard, b"freeform")?,
        [ToolName::new("freeform")?],
    )
    .err()
    .ok_or("freeform nested member must reject")?;
    assert!(error.message().as_str().contains("structured functions"));
    Ok(())
}

#[test]
fn catalog_cardinality_is_bounded_before_descriptor_or_host_construction() -> Result<(), TestError>
{
    let registrations = (0..65)
        .map(|index| {
            registration(
                &format!("tool_{index}"),
                &format!("test.tool-{index}.v1"),
                true,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let names = (0..65)
        .map(|index| ToolName::new(format!("tool_{index}")))
        .collect::<Result<Vec<_>, _>>()?;
    let error = ProductJavaScriptCompositionV1::compile(
        registry(registrations)?,
        target(IntegrationClass::Standard, b"too-many-tools")?,
        names,
    )
    .err()
    .ok_or("65 tools must reject")?;
    assert!(error.message().as_str().contains("1..=64"));
    Ok(())
}

#[test]
fn product_profile_does_not_change_codex_native_outer_contract() -> Result<(), TestError> {
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)?;
    ProductJavaScriptCompositionV1::register_markers_into(&mut builder)?;
    let descriptor = CodexNativeLocalCodeModeV1::compile(
        plakous_tools::JobConfig::default(),
        builder.freeze(),
        target(IntegrationClass::Standard, b"codex-preservation")?,
    )?;
    let definitions = descriptor.outer_projection().compile_request_view_v2()?;
    assert_eq!(
        definitions
            .definitions()
            .iter()
            .map(|definition| definition.exposed_name().as_str())
            .collect::<Vec<_>>(),
        [
            CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1,
            CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1
        ]
    );
    assert!(matches!(
        definitions.definitions()[0].presentation(),
        ProjectedToolPresentationV2::CustomUtf8 { .. }
    ));
    Ok(())
}

fn wait_request(arguments: &str) -> Result<ToolInvocationRequest, TestError> {
    use std::str::FromStr as _;
    Ok(ToolInvocationRequest::new(
        RunId::from_str("00000000-0000-0000-0000-0000000004a1")?,
        TurnId::from_str("00000000-0000-0000-0000-0000000004a2")?,
        AttemptId::from_str("00000000-0000-0000-0000-0000000004a3")?,
        CallId::from_str("00000000-0000-0000-0000-0000000004a4")?,
        ToolName::new(PRODUCT_JAVASCRIPT_WAIT_NAME_V1)?,
        ToolCorrelation::default(),
        Arc::<str>::from(arguments),
    ))
}

/// A `wait_js` that names no particular cell reaches dispatch once the driver
/// has said what is running, and its presented contract is unchanged.
///
/// A call sent `{"cell_id": "none"}` and was rejected
/// `InvalidArguments`. The projection alone cannot know what is running, so the
/// two stages are shown together here: the driver substitutes the active cell,
/// and the unchanged contract then validates the result.
#[test]
fn a_wait_js_that_names_no_particular_cell_reaches_the_running_one() -> Result<(), TestError> {
    let registry = registry([registration("read_local", "test.read.v1", true)?])?;
    let descriptor = ProductJavaScriptCompositionV1::compile(
        registry,
        target(IntegrationClass::Standard, b"wait-no-cell")?,
        [ToolName::new("read_local")?],
    )?;
    let projection = descriptor.outer_projection();

    for spelling in [
        r#"{"cell_id":"none"}"#,
        r#"{"cell_id":"null"}"#,
        r#"{"cell_id":"all"}"#,
        r#"{"cell_id":"none","yield_time_ms":1}"#,
    ] {
        // Stage one: the driver lowers "no particular cell" onto the running
        // one. Nothing else in the call is touched.
        let lowered = plakous_tools::normalized_wait_arguments(spelling, Some("28"))
            .ok_or("the driver did not lower a no-particular-cell wait")?;
        // Stage two: the unchanged contract validates it and dispatch sees the
        // string it always saw.
        let prepared = projection.prepare(wait_request(&lowered)?)?;
        assert_eq!(
            prepared.invocation().arguments().value()["cell_id"],
            "28",
            "{spelling}"
        );
    }
    // The yield budget rides through the substitution.
    let lowered = plakous_tools::normalized_wait_arguments(
        r#"{"cell_id":"none","yield_time_ms":1}"#,
        Some("28"),
    )
    .ok_or("lowering dropped the call")?;
    let prepared = projection.prepare(wait_request(&lowered)?)?;
    assert_eq!(
        prepared.invocation().arguments().value()["yield_time_ms"],
        1
    );

    // With nothing running there is no cell to name, so the spelling survives
    // the projection as the ordinary string the schema declares. The driver
    // answers that call before dispatch rather than letting it reach a cell
    // lookup that would fail.
    let unlowered = projection.prepare(wait_request(r#"{"cell_id":"none"}"#)?)?;
    assert_eq!(
        unlowered.invocation().arguments().value()["cell_id"],
        "none"
    );
    // The superset never widens the contract: an absent, null or empty
    // `cell_id` is lowered by nothing and refused by the presented schema, the
    // same as any other violation of the field.
    for violation in [r"{}", r#"{"cell_id":null}"#, r#"{"cell_id":""}"#] {
        assert_eq!(
            plakous_tools::normalized_wait_arguments(violation, Some("28"),),
            None,
            "{violation}"
        );
        assert_eq!(
            projection
                .prepare(wait_request(violation)?)
                .err()
                .ok_or("wait_js admitted a missing cell_id")?
                .kind(),
            ToolRuntimeErrorKind::InvalidArguments,
            "{violation}"
        );
    }
    Ok(())
}

/// The product `wait_js` vocabulary accepts a numeric `cell_id` and normalises
/// it, exactly as the Codex one does, and its presented contract is unchanged.
#[test]
fn a_numeric_wait_js_cell_id_is_accepted_and_normalised_to_its_string_form() -> Result<(), TestError>
{
    let registry = registry([registration("read_local", "test.read.v1", true)?])?;
    let descriptor = ProductJavaScriptCompositionV1::compile(
        registry,
        target(IntegrationClass::Standard, b"wait-cell-id")?,
        [ToolName::new("read_local")?],
    )?;
    let projection = descriptor.outer_projection();

    let numeric = projection.prepare(wait_request(r#"{"cell_id":28}"#)?)?;
    assert_eq!(numeric.invocation().arguments().value()["cell_id"], "28");
    let string = projection.prepare(wait_request(r#"{"cell_id":"28"}"#)?)?;
    assert_eq!(string.invocation().arguments().value()["cell_id"], "28");
    for refused in [r#"{"cell_id":true}"#, r#"{"cell_id":-1}"#, r"{}"] {
        assert_eq!(
            projection
                .prepare(wait_request(refused)?)
                .err()
                .ok_or("wait_js admitted an invalid cell_id")?
                .kind(),
            ToolRuntimeErrorKind::InvalidArguments
        );
    }

    // The model-visible contract still declares a string.
    let wait = projection
        .by_exposed(&ToolName::new(PRODUCT_JAVASCRIPT_WAIT_NAME_V1)?)
        .ok_or("wait function absent")?;
    let schema = wait.input_schema().ok_or("wait must be structured")?;
    assert_eq!(
        schema.document().value()["properties"]["cell_id"]["type"],
        "string"
    );
    assert_eq!(
        schema.digest(),
        plakous_model::PortableToolSchemaV2::new(plakous_tools::product_javascript_wait_schema())?
            .digest()
    );
    Ok(())
}
