//! Public conformance tests for frozen provider-facing tool exposure maps.

use std::str::FromStr;
use std::sync::{Arc, Mutex};

use plakous_model::{
    AttemptId, CallId, RunId, ToolConcurrency, ToolCorrelation, ToolDefinition, ToolEffect,
    ToolInputContract, ToolName, ToolOutputContract, TurnId,
};
use plakous_tools::{
    ArgumentLimits, FrozenToolRegistry, RegisteredToolId, RegisteredToolIdentity, ToolContext,
    ToolExecutor, ToolExecutorId, ToolExposure, ToolExposureMap, ToolFuture, ToolInvocation,
    ToolInvocationRequest, ToolOutcome, ToolRegistration, ToolRegistryBuilder, ToolRegistryLimits,
    ToolRuntimeErrorKind,
};
use proptest::prelude::*;
use serde_json::{Value, json};

type TestError = Box<dyn std::error::Error>;
type RegistryFixture = (FrozenToolRegistry, Vec<Arc<CapturingTool>>);
type RequestFixture = (ToolInvocationRequest, RunId, TurnId, AttemptId, CallId);

struct ToolSpec<'a> {
    name: &'a str,
    version: &'a str,
    executor: &'a str,
    schema: Value,
    result: &'a str,
}

struct CapturingTool {
    outcome: ToolOutcome,
    captured: Mutex<Vec<ToolInvocation>>,
}

impl CapturingTool {
    fn new(outcome: ToolOutcome) -> Self {
        Self {
            outcome,
            captured: Mutex::new(Vec::new()),
        }
    }

    fn captured(&self) -> Vec<ToolInvocation> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ToolExecutor for CapturingTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(invocation);
        let outcome = self.outcome.clone();
        Box::pin(async move { Ok(outcome) })
    }
}

fn empty_object_schema() -> Value {
    json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false})
}

fn build_registry(specs: &[ToolSpec<'_>]) -> Result<RegistryFixture, TestError> {
    let mut builder = ToolRegistryBuilder::default();
    let mut tools = Vec::with_capacity(specs.len());
    for spec in specs {
        let definition = ToolDefinition::new(
            ToolName::new(spec.name)?,
            spec.version,
            format!("{name} SENSITIVE_DESCRIPTION_SENTINEL", name = spec.name),
            ToolInputContract::structured(spec.schema.clone())?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?;
        let tool = Arc::new(CapturingTool::new(ToolOutcome::success_text(spec.result)?));
        builder.register(ToolRegistration::new(
            RegisteredToolId::new(format!("{}-id", spec.name))?,
            ToolExecutorId::new(spec.executor)?,
            plakous_tools::ToolExecutorBindingIdentity::new(1, &[spec.executor.as_bytes()])?,
            definition,
            tool.clone(),
        ))?;
        tools.push(tool);
    }
    Ok((builder.freeze(), tools))
}

fn build_single_registry(
    spec: &ToolSpec<'_>,
    registered_id: &str,
    effect: ToolEffect,
) -> Result<RegistryFixture, TestError> {
    build_single_registry_with_limits(spec, registered_id, effect, ArgumentLimits::default())
}

fn build_single_registry_with_limits(
    spec: &ToolSpec<'_>,
    registered_id: &str,
    effect: ToolEffect,
    arguments: ArgumentLimits,
) -> Result<RegistryFixture, TestError> {
    let definition = ToolDefinition::new(
        ToolName::new(spec.name)?,
        spec.version,
        format!("{name} SENSITIVE_DESCRIPTION_SENTINEL", name = spec.name),
        ToolInputContract::structured(spec.schema.clone())?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        effect,
    )?;
    let tool = Arc::new(CapturingTool::new(ToolOutcome::success_text(spec.result)?));
    let registration = ToolRegistration::new(
        RegisteredToolId::new(registered_id)?,
        ToolExecutorId::new(spec.executor)?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[spec.executor.as_bytes()])?,
        definition,
        tool.clone(),
    );
    let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits {
        arguments,
        ..ToolRegistryLimits::default()
    });
    builder.register(registration)?;
    Ok((builder.freeze(), vec![tool]))
}

fn identity(
    registry: &FrozenToolRegistry,
    name: &str,
) -> Result<RegisteredToolIdentity, Box<dyn std::error::Error>> {
    registry
        .identity(&ToolName::new(name)?)
        .cloned()
        .ok_or_else(|| format!("missing test identity: {name}").into())
}

fn exposure(
    identity: RegisteredToolIdentity,
    exposed_name: &str,
) -> Result<ToolExposure, Box<dyn std::error::Error>> {
    Ok(ToolExposure::new(identity, exposed_name)?)
}

fn assert_definition_preserved(exposed: &ToolDefinition, canonical: &ToolDefinition) {
    assert_eq!(exposed.description(), canonical.description());
    assert_eq!(exposed.semantic_version(), canonical.semantic_version());
    assert_eq!(exposed.input(), canonical.input());
    assert_eq!(exposed.output(), canonical.output());
    assert_eq!(exposed.concurrency(), canonical.concurrency());
    assert_eq!(exposed.effect(), canonical.effect());
}

fn property_exposure(
    registry: &FrozenToolRegistry,
    canonical_name: &str,
    exposed_name: &str,
) -> Result<ToolExposure, TestCaseError> {
    let identity = identity(registry, canonical_name)
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    ToolExposure::new(identity, exposed_name)
        .map_err(|error| TestCaseError::fail(error.to_string()))
}

fn invocation_request(tool: &str) -> Result<RequestFixture, TestError> {
    let run = RunId::from_str("00000000-0000-0000-0000-000000000011")?;
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000012")?;
    let attempt = AttemptId::from_str("00000000-0000-0000-0000-000000000013")?;
    let call = CallId::from_str("00000000-0000-0000-0000-000000000014")?;
    Ok((
        ToolInvocationRequest::new(
            run,
            turn,
            attempt,
            call,
            ToolName::new(tool)?,
            ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default())?,
            Arc::<str>::from("{}"),
        ),
        run,
        turn,
        attempt,
        call,
    ))
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one portable contract test exercises the complete exposure identity path"
)]
fn exposure_lookup_is_order_independent_and_prepares_exact_canonical_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let specs = [
        ToolSpec {
            name: "read",
            version: "1.0.0",
            executor: "read-executor-v1",
            schema: empty_object_schema(),
            result: "read-result",
        },
        ToolSpec {
            name: "search",
            version: "1.0.0",
            executor: "search-executor-v1",
            schema: empty_object_schema(),
            result: "search-result",
        },
    ];
    let (registry, tools) = build_registry(&specs)?;
    let read = identity(&registry, "read")?;
    let search = identity(&registry, "search")?;
    let first = ToolExposureMap::new(
        registry.clone(),
        [
            exposure(search.clone(), "find_text")?,
            exposure(read.clone(), "read_file")?,
        ],
    )?;
    let reordered = ToolExposureMap::new(
        registry.clone(),
        [
            exposure(read.clone(), "read_file")?,
            exposure(search.clone(), "find_text")?,
        ],
    )?;

    assert!(first.is_for_registry(&registry));
    let (foreign_registry, _) = build_registry(&specs)?;
    assert!(!first.is_for_registry(&foreign_registry));
    assert_eq!(first.digest(), reordered.digest());
    assert_eq!(
        first.entries()[0].canonical_identity().name().as_str(),
        "read"
    );
    assert_eq!(
        first.entries()[1].canonical_identity().name().as_str(),
        "search"
    );
    assert_eq!(first.definitions()[0].name().as_str(), "read_file");
    let ToolInputContract::Structured(schema) = first.definitions()[0].input() else {
        panic!("structured")
    };
    assert_eq!(schema.document().value().as_ref(), &specs[0].schema);
    let canonical_definition = registry
        .definition(&ToolName::new("read")?)
        .ok_or("canonical definition is missing")?;
    assert_definition_preserved(&first.definitions()[0], canonical_definition);
    assert_eq!(
        first.exposed_name(&read).map(ToolName::as_str),
        Some("read_file")
    );
    assert_eq!(
        first
            .canonical_identity(&ToolName::new("find_text")?)
            .map(RegisteredToolIdentity::name),
        Some(search.name())
    );

    let (request, _run, _turn, _attempt, _call) = invocation_request("read_file")?;
    let invocation = first.prepare(request)?;
    assert_eq!(invocation.identity(), &read);
    assert_eq!(invocation.identity().name().as_str(), "read");
    let binding = invocation
        .exposure()
        .ok_or("exposed invocation binding is missing")?;
    assert_eq!(binding.canonical_identity(), &read);
    assert_eq!(binding.exposed_name().as_str(), "read_file");
    assert_eq!(binding.map_digest(), first.digest());
    let direct = registry.prepare(invocation_request("read")?.0)?;
    assert!(direct.exposure().is_none());
    assert_eq!(direct.arguments().digest(), invocation.arguments().digest());
    assert!(tools[0].captured().is_empty());
    assert!(tools[1].captured().is_empty());

    let unknown = first.prepare(invocation_request("read")?.0);
    assert_eq!(
        unknown.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::UnknownTool)
    );
    let debug = format!("{first:?}");
    assert!(!debug.contains("SENSITIVE_DESCRIPTION_SENTINEL"));
    assert!(!debug.contains("type"));
    Ok(())
}

#[test]
fn exposure_digest_is_independent_of_registry_registration_order()
-> Result<(), Box<dyn std::error::Error>> {
    let forward_specs = [
        ToolSpec {
            name: "read",
            version: "1.0.0",
            executor: "read-executor-v1",
            schema: empty_object_schema(),
            result: "read-result",
        },
        ToolSpec {
            name: "search",
            version: "1.0.0",
            executor: "search-executor-v1",
            schema: empty_object_schema(),
            result: "search-result",
        },
    ];
    let reversed_specs = [
        ToolSpec {
            name: "search",
            version: "1.0.0",
            executor: "search-executor-v1",
            schema: empty_object_schema(),
            result: "search-result",
        },
        ToolSpec {
            name: "read",
            version: "1.0.0",
            executor: "read-executor-v1",
            schema: empty_object_schema(),
            result: "read-result",
        },
    ];
    let (forward_registry, _forward_tools) = build_registry(&forward_specs)?;
    let (reversed_registry, _reversed_tools) = build_registry(&reversed_specs)?;
    let forward = ToolExposureMap::new(
        forward_registry.clone(),
        [
            exposure(identity(&forward_registry, "read")?, "read_file")?,
            exposure(identity(&forward_registry, "search")?, "find_text")?,
        ],
    )?;
    let reversed = ToolExposureMap::new(
        reversed_registry.clone(),
        [
            exposure(identity(&reversed_registry, "read")?, "read_file")?,
            exposure(identity(&reversed_registry, "search")?, "find_text")?,
        ],
    )?;
    assert_eq!(forward.registry_digest(), reversed.registry_digest());
    assert_eq!(forward.digest(), reversed.digest());
    Ok(())
}

#[test]
fn exposure_map_rejects_duplicate_alias_reserved_and_foreign_entries()
-> Result<(), Box<dyn std::error::Error>> {
    let specs = [
        ToolSpec {
            name: "read",
            version: "1.0.0",
            executor: "read-executor-v1",
            schema: empty_object_schema(),
            result: "read-result",
        },
        ToolSpec {
            name: "search",
            version: "1.0.0",
            executor: "search-executor-v1",
            schema: empty_object_schema(),
            result: "search-result",
        },
    ];
    let (registry, _tools) = build_registry(&specs)?;
    let read = identity(&registry, "read")?;
    let search = identity(&registry, "search")?;

    let duplicate = ToolExposureMap::new(
        registry.clone(),
        [
            exposure(read.clone(), "read_file")?,
            exposure(read.clone(), "open_file")?,
        ],
    );
    assert_eq!(
        duplicate.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Registration)
    );
    let alias_collision = ToolExposureMap::new(
        registry.clone(),
        [exposure(read.clone(), "tool")?, exposure(search, "tool")?],
    );
    assert_eq!(
        alias_collision.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Registration)
    );
    let reserved =
        ToolExposureMap::new(registry.clone(), [exposure(read.clone(), "plak.internal")?]);
    assert_eq!(
        reserved.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Registration)
    );
    assert_eq!(
        ToolExposure::new(read, "invalid name")
            .err()
            .map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Registration)
    );

    let (foreign_registry, _foreign_tools) = build_registry(&specs)?;
    let foreign = identity(&foreign_registry, "read")?;
    assert_eq!(
        ToolExposureMap::new(registry, [exposure(foreign, "read_file")?])
            .err()
            .map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Registration)
    );
    Ok(())
}

#[test]
fn exposure_digest_binds_alias_and_exact_selected_set() -> Result<(), Box<dyn std::error::Error>> {
    let base = [ToolSpec {
        name: "read",
        version: "1.0.0",
        executor: "read-executor-v1",
        schema: empty_object_schema(),
        result: "read-result",
    }];
    let (registry, _tools) = build_registry(&base)?;
    let read = identity(&registry, "read")?;
    let codex = ToolExposureMap::new(registry.clone(), [exposure(read.clone(), "read_file")?])?;
    let claude = ToolExposureMap::new(registry.clone(), [exposure(read, "open_file")?])?;
    assert_ne!(codex.digest(), claude.digest());
    assert_eq!(codex.registry_digest(), claude.registry_digest());
    let codex_invocation = codex.prepare(invocation_request("read_file")?.0)?;
    let claude_invocation = claude.prepare(invocation_request("open_file")?.0)?;
    let codex_binding = codex_invocation
        .exposure()
        .ok_or("Codex exposure binding is missing")?;
    let claude_binding = claude_invocation
        .exposure()
        .ok_or("Claude exposure binding is missing")?;
    assert_eq!(codex_binding.exposed_name().as_str(), "read_file");
    assert_eq!(claude_binding.exposed_name().as_str(), "open_file");
    assert_eq!(codex_binding.map_digest(), codex.digest());
    assert_eq!(claude_binding.map_digest(), claude.digest());
    assert_ne!(codex_binding.map_digest(), claude_binding.map_digest());

    let (identical_registry, _identical_tools) = build_registry(&base)?;
    let identical = ToolExposureMap::new(
        identical_registry.clone(),
        [exposure(
            identity(&identical_registry, "read")?,
            "read_file",
        )?],
    )?;
    assert_eq!(codex.registry_digest(), identical.registry_digest());
    assert_eq!(codex.digest(), identical.digest());
    assert!(
        codex
            .by_canonical(&identity(&identical_registry, "read")?)
            .is_none()
    );

    let extended_specs = [
        ToolSpec {
            name: "read",
            version: "1.0.0",
            executor: "read-executor-v1",
            schema: empty_object_schema(),
            result: "read-result",
        },
        ToolSpec {
            name: "search",
            version: "1.0.0",
            executor: "search-executor-v1",
            schema: empty_object_schema(),
            result: "search-result",
        },
    ];
    let (extended_registry, _extended_tools) = build_registry(&extended_specs)?;
    let extended = ToolExposureMap::new(
        extended_registry.clone(),
        [exposure(
            identity(&extended_registry, "read")?,
            "read_file",
        )?],
    )?;
    assert_ne!(codex.registry_digest(), extended.registry_digest());
    assert_eq!(codex.digest(), extended.digest());

    Ok(())
}

#[test]
fn exposure_digest_invalidates_for_registered_version_schema_and_executor_replacement()
-> Result<(), Box<dyn std::error::Error>> {
    let base = [ToolSpec {
        name: "read",
        version: "1.0.0",
        executor: "read-executor-v1",
        schema: empty_object_schema(),
        result: "read-result",
    }];
    let (registry, _tools) = build_registry(&base)?;
    let codex = ToolExposureMap::new(
        registry.clone(),
        [exposure(identity(&registry, "read")?, "read_file")?],
    )?;

    let version_replacement_specs = [ToolSpec {
        name: "read",
        version: "2.0.0",
        executor: "read-executor-v1",
        schema: empty_object_schema(),
        result: "read-result",
    }];
    let (version_registry, _version_tools) = build_registry(&version_replacement_specs)?;
    let version_replacement = ToolExposureMap::new(
        version_registry.clone(),
        [exposure(identity(&version_registry, "read")?, "read_file")?],
    )?;
    assert_ne!(
        codex.registry_digest(),
        version_replacement.registry_digest()
    );
    assert_ne!(codex.digest(), version_replacement.digest());

    let schema_replacement_specs = [ToolSpec {
        name: "read",
        version: "1.0.0",
        executor: "read-executor-v1",
        schema: json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "properties":{"path":{"type":"string"}}, "required":["path"], "additionalProperties":false}),
        result: "read-result",
    }];
    let (schema_registry, _schema_tools) = build_registry(&schema_replacement_specs)?;
    let schema_replacement = ToolExposureMap::new(
        schema_registry.clone(),
        [exposure(identity(&schema_registry, "read")?, "read_file")?],
    )?;
    assert_ne!(
        codex.registry_digest(),
        schema_replacement.registry_digest()
    );
    assert_ne!(codex.digest(), schema_replacement.digest());

    let (registered_id_registry, _registered_id_tools) =
        build_single_registry(&base[0], "read-id-v2", ToolEffect::ReadOnly)?;
    let registered_id_replacement = ToolExposureMap::new(
        registered_id_registry.clone(),
        [exposure(
            identity(&registered_id_registry, "read")?,
            "read_file",
        )?],
    )?;
    assert_ne!(
        codex.registry_digest(),
        registered_id_replacement.registry_digest()
    );
    assert_ne!(codex.digest(), registered_id_replacement.digest());

    let executor_replacement_specs = [ToolSpec {
        name: "read",
        version: "1.0.0",
        executor: "read-executor-v2",
        schema: empty_object_schema(),
        result: "read-result",
    }];
    let (executor_registry, _executor_tools) = build_registry(&executor_replacement_specs)?;
    let executor_replacement = ToolExposureMap::new(
        executor_registry.clone(),
        [exposure(
            identity(&executor_registry, "read")?,
            "read_file",
        )?],
    )?;
    assert_ne!(
        codex.registry_digest(),
        executor_replacement.registry_digest()
    );
    assert_ne!(codex.digest(), executor_replacement.digest());
    Ok(())
}

#[test]
fn registry_and_exposure_digests_bind_every_argument_limit()
-> Result<(), Box<dyn std::error::Error>> {
    let spec = ToolSpec {
        name: "read",
        version: "1.0.0",
        executor: "read-executor-v1",
        schema: empty_object_schema(),
        result: "read-result",
    };
    let base_limits = ArgumentLimits::default();
    let (base_registry, _base_tools) =
        build_single_registry_with_limits(&spec, "read-id", ToolEffect::ReadOnly, base_limits)?;
    let base = ToolExposureMap::new(
        base_registry.clone(),
        [exposure(identity(&base_registry, "read")?, "read_file")?],
    )?;
    let replacements = [
        ArgumentLimits {
            max_bytes: base_limits.max_bytes.saturating_sub(1),
            ..base_limits
        },
        ArgumentLimits {
            max_depth: base_limits.max_depth.saturating_sub(1),
            ..base_limits
        },
        ArgumentLimits {
            max_nodes: base_limits.max_nodes.saturating_sub(1),
            ..base_limits
        },
        ArgumentLimits {
            max_string_bytes: base_limits.max_string_bytes.saturating_sub(1),
            ..base_limits
        },
    ];

    for limits in replacements {
        let (registry, _tools) =
            build_single_registry_with_limits(&spec, "read-id", ToolEffect::ReadOnly, limits)?;
        let replacement = ToolExposureMap::new(
            registry.clone(),
            [exposure(identity(&registry, "read")?, "read_file")?],
        )?;
        assert_ne!(base.registry_digest(), replacement.registry_digest());
        assert_ne!(base.digest(), replacement.digest());
        assert_ne!(
            identity(&base_registry, "read")?.argument_limits_digest(),
            identity(&registry, "read")?.argument_limits_digest(),
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn exposure_digest_is_independent_of_input_permutation(
        ordering in prop::collection::vec(any::<u8>(), 3)
    ) {
        let specs = [
            ToolSpec {
                name: "alpha",
                version: "1.0.0",
                executor: "alpha-executor",
        schema: empty_object_schema(),
                result: "alpha",
            },
            ToolSpec {
                name: "beta",
                version: "1.0.0",
                executor: "beta-executor",
        schema: empty_object_schema(),
                result: "beta",
            },
            ToolSpec {
                name: "gamma",
                version: "1.0.0",
                executor: "gamma-executor",
        schema: empty_object_schema(),
                result: "gamma",
            },
        ];
        let (registry, _tools) = match build_registry(&specs) {
            Ok(value) => value,
            Err(error) => return Err(TestCaseError::fail(error.to_string())),
        };
        let canonical_result = ToolExposureMap::new(
            registry.clone(),
            [
                property_exposure(&registry, "alpha", "a")?,
                property_exposure(&registry, "beta", "b")?,
                property_exposure(&registry, "gamma", "g")?,
            ],
        );
        let canonical = match canonical_result {
            Ok(value) => value,
            Err(error) => return Err(TestCaseError::fail(error.to_string())),
        };

        let mut shuffled = vec![
            property_exposure(&registry, "alpha", "a")?,
            property_exposure(&registry, "beta", "b")?,
            property_exposure(&registry, "gamma", "g")?,
        ];
        shuffled.sort_by_key(|entry| {
            let name = entry.canonical_identity().name().as_str();
            let index = match name {
                "alpha" => 0,
                "beta" => 1,
                _ => 2,
            };
            ordering[index]
        });
        let permuted = match ToolExposureMap::new(registry, shuffled) {
            Ok(value) => value,
            Err(error) => return Err(TestCaseError::fail(error.to_string())),
        };
        prop_assert_eq!(canonical.digest(), permuted.digest());
    }
}
