use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};

use plakous_model::{
    AttemptId, CallId, DigestDomain, EffectiveProviderConfigurationId, ModelContentContractV1,
    PortableToolSchemaV1, PortableToolSchemaV2, RunId, ToolConcurrency, ToolCorrelation,
    ToolEffect, ToolInputContract, ToolOutputContract, TurnId, Utf8TextContractV1,
};
use serde_json::{Value, json};

use super::*;
use crate::{
    ToolContext, ToolFuture, ToolInvocation, ToolInvocationRequest, ToolOutcome,
    ToolRuntimeErrorKind,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn offline_executor_identity_check_uses_exact_bindings_without_invocation() -> TestResult {
    let (registry, calls) = registry_in_order(&[0, 1, 2])?;
    let descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
    descriptor.check_executor_identities(|name, id| {
        registry
            .identity(name)
            .filter(|identity| identity.executor_id() == id)
            .map(RegisteredToolIdentity::executor_binding_identity)
    })?;
    assert!(matches!(
        descriptor.check_executor_identities(|_, _| None),
        Err(ToolBindingError::MissingBinding { .. })
    ));
    let incompatible = ToolExecutorBindingIdentity::new(2, &[b"incompatible"])?;
    assert!(matches!(
        descriptor.check_executor_identities(|_, _| Some(incompatible)),
        Err(ToolBindingError::BindingIdentityMismatch { .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}

struct CountingExecutor {
    calls: Arc<AtomicUsize>,
}

impl ToolExecutor for CountingExecutor {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolOutcome::success_text("ok") })
    }
}

fn structured_definition(name: &str) -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new(name)?,
        "1.2.3",
        format!("{name} description"),
        ToolInputContract::Structured(PortableToolSchemaV2::new(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{"value":{"type":"string"}},
            "required":["value"],
            "additionalProperties":false
        }))?),
        ToolOutputContract::Structured(PortableToolSchemaV1::new(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{"result":{"type":"integer"}},
            "required":["result"],
            "additionalProperties":false
        }))?),
        ToolConcurrency::Parallel,
        ToolEffect::ReadOnly,
    )?)
}

fn utf8_definition(name: &str) -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new(name)?,
        "2",
        "UTF-8 tool",
        ToolInputContract::Utf8(Utf8TextContractV1::new(4_096)?),
        ToolOutputContract::Utf8(Utf8TextContractV1::new(99)?),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?)
}

fn model_content_definition(name: &str) -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new(name)?,
        "3",
        "model content tool",
        ToolInputContract::Utf8(Utf8TextContractV1::new(55)?),
        ToolOutputContract::ModelContent(ModelContentContractV1::new(7, 777)?),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?)
}

fn registry_in_order(order: &[usize]) -> TestResult<(FrozenToolRegistry, Arc<AtomicUsize>)> {
    let calls = Arc::new(AtomicUsize::new(0));
    let executors = [
        Arc::new(CountingExecutor {
            calls: Arc::clone(&calls),
        }),
        Arc::new(CountingExecutor {
            calls: Arc::clone(&calls),
        }),
        Arc::new(CountingExecutor {
            calls: Arc::clone(&calls),
        }),
    ];
    let definitions = [
        structured_definition("alpha")?,
        utf8_definition("beta")?,
        model_content_definition("gamma")?,
    ];
    let limits = ToolRegistryLimits {
        max_tools: 9,
        max_total_schema_bytes: 32_000,
        schema: SchemaLimits { max_bytes: 8_000 },
        arguments: ArgumentLimits {
            max_bytes: 9_000,
            max_depth: 12,
            max_nodes: 500,
            max_string_bytes: 700,
        },
        reserved_prefixes: vec!["z.".into(), "a.".into(), "z.".into()],
    };
    let mut builder = ToolRegistryBuilder::new(limits);
    for &index in order {
        builder.register(ToolRegistration::new(
            RegisteredToolId::new(format!("tool-{index}"))?,
            ToolExecutorId::new(format!("executor-{index}"))?,
            test_binding(index)?,
            definitions[index].clone(),
            executors[index].clone(),
        ))?;
    }
    Ok((builder.freeze(), calls))
}

fn test_binding(index: usize) -> TestResult<ToolExecutorBindingIdentity> {
    Ok(ToolExecutorBindingIdentity::new(
        1,
        &[format!("executor-binding-{index}").as_bytes()],
    )?)
}

fn bindings(executor_count: usize) -> TestResult<ToolExecutorBindings> {
    bindings_with_calls(executor_count, &Arc::new(AtomicUsize::new(0)))
}

fn bindings_with_calls(
    executor_count: usize,
    calls: &Arc<AtomicUsize>,
) -> TestResult<ToolExecutorBindings> {
    let mut builder = ToolExecutorBindingsBuilder::new();
    for index in 0..executor_count {
        builder.bind(
            ToolExecutorId::new(format!("executor-{index}"))?,
            test_binding(index)?,
            Arc::new(CountingExecutor {
                calls: Arc::clone(calls),
            }),
        )?;
    }
    Ok(builder.freeze())
}

fn product_plan_registry(session_id: &str) -> TestResult<FrozenToolRegistryDescriptor> {
    let authority =
        crate::standard_identity::PlanStateIdentity::new(format!("plak.session.{session_id}"), 1)?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(crate::standard_identity::expected_update_plan_registration(
        &authority.executor_id()?,
    )?)?;
    Ok(FrozenToolRegistryDescriptor::capture(&registry.freeze())?)
}

/// The presented-name claimer flag is host authority: reconstruction
/// re-takes it from the live binding table, never from the persisted
/// descriptor, and it moves no digest.
#[test]
fn reconstruction_retakes_the_claimer_flag_from_the_host_table() -> TestResult {
    let calls = Arc::new(AtomicUsize::new(0));
    let register = |claims: bool| -> TestResult<FrozenToolRegistry> {
        let mut builder = ToolRegistryBuilder::default();
        for (index, definition) in [structured_definition("alpha")?, utf8_definition("beta")?]
            .into_iter()
            .enumerate()
        {
            let registration = ToolRegistration::new(
                RegisteredToolId::new(format!("tool-{index}"))?,
                ToolExecutorId::new(format!("executor-{index}"))?,
                test_binding(index)?,
                definition,
                Arc::new(CountingExecutor {
                    calls: Arc::clone(&calls),
                }),
            );
            builder.register(if claims && index == 1 {
                registration.presented_name_claimer()
            } else {
                registration
            })?;
        }
        Ok(builder.freeze())
    };
    let live = register(true)?;
    let unflagged = register(false)?;
    let descriptor = FrozenToolRegistryDescriptor::capture(&live)?;
    assert_eq!(
        descriptor,
        FrozenToolRegistryDescriptor::capture(&unflagged)?,
        "the flag is in no persisted byte"
    );
    let decoded = FrozenToolRegistryDescriptor::decode_v2(&descriptor.encode_v2()?)?;

    let restored = decoded.resolve(&ToolExecutorBindings::capture(&live))?;
    assert_eq!(
        restored.registry_identity_digest()?,
        live.registry_identity_digest()?
    );
    for identity in live.identities() {
        assert_eq!(
            restored.identity(identity.name()),
            Some(identity),
            "live and restored identities compare equal, flag included"
        );
    }
    assert!(
        restored
            .identity(&ToolName::new("beta")?)
            .is_some_and(RegisteredToolIdentity::claims_presented_name)
    );

    let restored = decoded.resolve(&ToolExecutorBindings::capture(&unflagged))?;
    assert!(
        restored
            .identities()
            .all(|identity| !identity.claims_presented_name()),
        "a host that does not flag the registration confers no claim"
    );

    let mut builder = ToolExecutorBindingsBuilder::new();
    for index in 0..2 {
        builder.bind(
            ToolExecutorId::new(format!("executor-{index}"))?,
            test_binding(index)?,
            Arc::new(CountingExecutor {
                calls: Arc::clone(&calls),
            }),
        )?;
    }
    builder.presented_name_claimer(RegisteredToolId::new("tool-1")?);
    let restored = decoded.resolve(&builder.freeze())?;
    assert_eq!(
        restored.identity(&ToolName::new("beta")?),
        live.identity(&ToolName::new("beta")?)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}

/// The owner class is host authority like the claimer flag: in no
/// persisted byte or digest, re-taken from the live binding table.
#[test]
fn reconstruction_retakes_the_owner_class_from_the_host_table() -> TestResult {
    let calls = Arc::new(AtomicUsize::new(0));
    let github = ToolOwnerClass::Mcp(crate::McpServerName::new("github")?);
    let register = |owner: &ToolOwnerClass| -> TestResult<FrozenToolRegistry> {
        let mut builder = ToolRegistryBuilder::default();
        for (index, definition) in [structured_definition("alpha")?, utf8_definition("beta")?]
            .into_iter()
            .enumerate()
        {
            let registration = ToolRegistration::new(
                RegisteredToolId::new(format!("tool-{index}"))?,
                ToolExecutorId::new(format!("executor-{index}"))?,
                test_binding(index)?,
                definition,
                Arc::new(CountingExecutor {
                    calls: Arc::clone(&calls),
                }),
            );
            builder.register(if index == 1 {
                registration.with_owner(owner.clone())
            } else {
                registration
            })?;
        }
        Ok(builder.freeze())
    };
    let live = register(&github)?;
    let local = register(&ToolOwnerClass::PlakLocal)?;
    let descriptor = FrozenToolRegistryDescriptor::capture(&live)?;
    assert_eq!(
        descriptor,
        FrozenToolRegistryDescriptor::capture(&local)?,
        "the owner is in no persisted byte"
    );
    assert_eq!(
        live.registry_identity_digest()?,
        local.registry_identity_digest()?
    );
    let decoded = FrozenToolRegistryDescriptor::decode_v2(&descriptor.encode_v2()?)?;
    let owner_of = |registry: &FrozenToolRegistry, name: &str| -> TestResult<ToolOwnerClass> {
        Ok(registry
            .identity(&ToolName::new(name)?)
            .ok_or("registered")?
            .owner()
            .clone())
    };

    let restored = decoded.resolve(&ToolExecutorBindings::capture(&live))?;
    assert_eq!(owner_of(&restored, "beta")?, github);
    assert_eq!(owner_of(&restored, "alpha")?, ToolOwnerClass::PlakLocal);
    let restored = decoded.resolve(&ToolExecutorBindings::capture(&local))?;
    assert_eq!(owner_of(&restored, "beta")?, ToolOwnerClass::PlakLocal);

    let mut builder = ToolExecutorBindingsBuilder::new();
    for index in 0..2 {
        builder.bind(
            ToolExecutorId::new(format!("executor-{index}"))?,
            test_binding(index)?,
            Arc::new(CountingExecutor {
                calls: Arc::clone(&calls),
            }),
        )?;
    }
    builder.owner(RegisteredToolId::new("tool-1")?, github.clone());
    let restored = decoded.resolve(&builder.freeze())?;
    assert_eq!(owner_of(&restored, "beta")?, github);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[test]
fn registry_v1_is_order_independent_complete_and_reconstructive() -> TestResult {
    let (forward, _) = registry_in_order(&[0, 1, 2])?;
    let (reverse, _) = registry_in_order(&[2, 1, 0])?;
    let forward = FrozenToolRegistryDescriptor::capture(&forward)?;
    let reverse = FrozenToolRegistryDescriptor::capture(&reverse)?;
    let bytes = forward.encode_v1()?;
    assert_eq!(bytes, reverse.encode_v1()?);
    assert_eq!(FrozenToolRegistryDescriptor::decode_v1(&bytes)?, forward);

    let calls = Arc::new(AtomicUsize::new(0));
    let resolved = forward.resolve(&bindings_with_calls(4, &calls)?)?;
    assert_eq!(resolved.len(), 3);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(resolved.definition(&ToolName::new("extra")?).is_none());
    assert!(
        resolved
            .prepare(request("alpha", r#"{"value":"reconstructed"}"#)?)
            .is_ok()
    );
    assert_eq!(
        resolved
            .prepare(request("beta", "x".repeat(4_097))?)
            .err()
            .map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::ResourceLimit)
    );
    assert_eq!(
        resolved.definition(&ToolName::new("alpha")?),
        Some(&structured_definition("alpha")?)
    );
    assert_eq!(
        resolved
            .definition(&ToolName::new("beta")?)
            .ok_or("beta missing")?
            .output(),
        utf8_definition("beta")?.output()
    );
    assert_eq!(
        resolved
            .definition(&ToolName::new("gamma")?)
            .ok_or("gamma missing")?
            .output(),
        model_content_definition("gamma")?.output()
    );
    Ok(())
}

#[test]
fn registry_interface_identity_allows_executor_rebinding_only() -> TestResult {
    let (original, _) = registry_in_order(&[0, 1, 2])?;
    let mut rebound = ToolRegistryBuilder::new(original.limits().clone());
    for (index, definition) in original.definitions().iter().cloned().enumerate() {
        rebound.register(ToolRegistration::new(
            RegisteredToolId::new(format!("tool-{index}"))?,
            ToolExecutorId::new(format!("rebound-executor-{index}"))?,
            ToolExecutorBindingIdentity::new(1, &[format!("rebound-binding-{index}").as_bytes()])?,
            definition,
            Arc::new(CountingExecutor {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        ))?;
    }
    let original = FrozenToolRegistryDescriptor::capture(&original)?;
    let rebound = FrozenToolRegistryDescriptor::capture(&rebound.freeze())?;
    assert_ne!(original, rebound);
    assert_eq!(
        original.interface_identity()?,
        rebound.interface_identity()?
    );

    let (changed, _) = registry_in_order(&[0, 1])?;
    let changed = FrozenToolRegistryDescriptor::capture(&changed)?;
    assert_ne!(
        original.interface_identity()?,
        changed.interface_identity()?
    );
    Ok(())
}

#[test]
fn canonical_authority_excludes_only_exact_composition_markers() -> TestResult {
    let empty = FrozenToolRegistryDescriptor::capture(&ToolRegistryBuilder::default().freeze())?;
    let mut markers = ToolRegistryBuilder::default();
    crate::composition_markers::register_product_javascript_markers_into(&mut markers)?;
    let markers = FrozenToolRegistryDescriptor::capture(&markers.freeze())?;
    assert_eq!(
        empty.canonical_authority_identity()?,
        markers.canonical_authority_identity()?
    );

    let mut spoofed = ToolRegistryBuilder::default();
    spoofed.register(ToolRegistration::new(
        RegisteredToolId::new("spoofed-composition-marker")?,
        ToolExecutorId::new(
            crate::composition_markers::PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1,
        )?,
        ToolExecutorBindingIdentity::new(1, &[b"spoofed-binding"])?,
        structured_definition("spoofed")?,
        Arc::new(CountingExecutor {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    ))?;
    let spoofed = FrozenToolRegistryDescriptor::capture(&spoofed.freeze())?;
    assert!(spoofed.canonical_authority_identity().is_err());
    Ok(())
}

#[test]
fn canonical_authority_normalizes_only_exact_product_plan_tool_state_identity() -> TestResult {
    let first = product_plan_registry("6276f0c6-e24b-49ae-a630-6ced6b01cbad")?;
    let second = product_plan_registry("a9991d7d-28a8-47c3-8ce2-edc7d57871a1")?;
    assert_ne!(first, second);
    assert_eq!(
        first.canonical_authority_identity()?,
        second.canonical_authority_identity()?
    );

    let malformed = product_plan_registry("not-a-session-uuid")?;
    assert!(malformed.canonical_authority_identity().is_err());

    let exact_id = "plakous.standard.update-plan.v1.plak.session.\
        6276f0c6-e24b-49ae-a630-6ced6b01cbad.r1";
    let mut spoofed = ToolRegistryBuilder::default();
    spoofed.register(
        ToolRegistration::new(
            RegisteredToolId::new("plakous.standard.update-plan")?,
            ToolExecutorId::new(exact_id)?,
            ToolExecutorBindingIdentity::new(4, &[b"spoofed-binding"])?,
            structured_definition("update_plan")?,
            Arc::new(CountingExecutor {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .nested_eligible(),
    )?;
    let spoofed = FrozenToolRegistryDescriptor::capture(&spoofed.freeze())?;
    assert!(spoofed.canonical_authority_identity().is_err());
    Ok(())
}

#[test]
fn bindings_reject_duplicates_and_resolution_requires_every_exact_id() -> TestResult {
    let executor: Arc<dyn ToolExecutor> = Arc::new(CountingExecutor {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let mut duplicates = ToolExecutorBindingsBuilder::new();
    duplicates.bind(
        ToolExecutorId::new("executor-0")?,
        test_binding(0)?,
        Arc::clone(&executor),
    )?;
    assert!(matches!(
        duplicates.bind(ToolExecutorId::new("executor-0")?, test_binding(0)?, executor),
        Err(ToolBindingError::DuplicateBinding { id }) if id.as_str() == "executor-0"
    ));

    let (registry, _) = registry_in_order(&[0, 1, 2])?;
    let descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
    assert!(matches!(
        descriptor.resolve(&bindings(2)?),
        Err(ToolBindingError::MissingBinding { id }) if id.as_str() == "executor-2"
    ));
    Ok(())
}

#[test]
fn registry_codec_refuses_predecessor_unknown_trailing_noncanonical_and_tampering() -> TestResult {
    let (registry, _) = registry_in_order(&[0, 1, 2])?;
    let descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
    let bytes = descriptor.encode_v1()?;

    let mut predecessor = bytes.clone();
    predecessor[8..12].copy_from_slice(&1_u32.to_be_bytes());
    assert_eq!(
        FrozenToolRegistryDescriptor::decode_v1(&predecessor),
        Err(ToolDescriptorError::UnsupportedVersion { found: 1 })
    );
    let mut unknown = bytes.clone();
    unknown[8..12].copy_from_slice(&3_u32.to_be_bytes());
    assert_eq!(
        FrozenToolRegistryDescriptor::decode_v1(&unknown),
        Err(ToolDescriptorError::UnsupportedVersion { found: 3 })
    );
    let mut trailing = bytes.clone();
    trailing.push(b' ');
    assert!(FrozenToolRegistryDescriptor::decode_v1(&trailing).is_err());

    let mut value: Value = serde_json::from_slice(&bytes[HEADER_BYTES..])?;
    value["unknown"] = json!(true);
    assert_eq!(
        FrozenToolRegistryDescriptor::decode_v1(&with_payload(&bytes, &value)?),
        Err(ToolDescriptorError::Malformed)
    );
    let mut missing: Value = serde_json::from_slice(&bytes[HEADER_BYTES..])?;
    missing
        .as_object_mut()
        .ok_or("registry payload is not object")?
        .remove("limits");
    assert_eq!(
        FrozenToolRegistryDescriptor::decode_v1(&with_payload(&bytes, &missing)?),
        Err(ToolDescriptorError::Malformed)
    );
    let mut tampered = descriptor.wire.clone();
    tampered.registrations[0].identity.semantic_version = "wrong".into();
    assert_eq!(
        FrozenToolRegistryDescriptor::decode_v1(&encode_envelope(
            REGISTRY_MAGIC,
            TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
            &tampered,
        )?),
        Err(ToolDescriptorError::IntegrityMismatch)
    );
    let mut eligibility_tampered = descriptor.wire.clone();
    eligibility_tampered.registrations[0]
        .identity
        .nested_eligibility = NestedToolEligibility::NestedEligible;
    assert_eq!(
        FrozenToolRegistryDescriptor::decode_v1(&encode_envelope(
            REGISTRY_MAGIC,
            TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
            &eligibility_tampered,
        )?),
        Err(ToolDescriptorError::IntegrityMismatch)
    );
    let mut reordered: Value = serde_json::from_slice(&bytes[HEADER_BYTES..])?;
    reordered["registrations"]
        .as_array_mut()
        .ok_or("registrations not array")?
        .swap(0, 1);
    assert_eq!(
        FrozenToolRegistryDescriptor::decode_v1(&with_payload(&bytes, &reordered)?),
        Err(ToolDescriptorError::NonCanonical)
    );
    Ok(())
}

#[test]
fn exposure_v1_roundtrips_complete_mapping_and_revalidates_digest() -> TestResult {
    let (registry, _) = registry_in_order(&[0, 1, 2])?;
    let alpha = registry
        .identity(&ToolName::new("alpha")?)
        .ok_or("alpha missing")?
        .clone();
    let beta = registry
        .identity(&ToolName::new("beta")?)
        .ok_or("beta missing")?
        .clone();
    let exposure = ToolExposureMap::new(
        registry.clone(),
        [
            ToolExposure::new(beta, "visible_beta")?,
            ToolExposure::new(alpha, "visible_alpha")?
                .with_description_override(Some("historical override".into()))?,
        ],
    )?;
    let registry_descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
    let exposure_descriptor = FrozenToolExposureDescriptor::capture(&exposure)?;
    let bytes = exposure_descriptor.encode_v1()?;
    let decoded = FrozenToolExposureDescriptor::decode_v1(&bytes, &registry_descriptor)?;
    let reconstructed = decoded.resolve(registry_descriptor.resolve(&bindings(3)?)?)?;
    assert_eq!(reconstructed.digest(), exposure.digest());
    assert_eq!(reconstructed.definitions(), exposure.definitions());
    assert_eq!(
        reconstructed.entries()[0].description_override(),
        Some("historical override")
    );

    let mut tampered = exposure_descriptor.wire.clone();
    tampered.entries[0].description_override = Some("changed".into());
    assert_eq!(
        FrozenToolExposureDescriptor::decode_v1(
            &encode_envelope(
                EXPOSURE_MAGIC,
                TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
                &tampered,
            )?,
            &registry_descriptor,
        ),
        Err(ToolDescriptorError::IntegrityMismatch)
    );
    Ok(())
}

fn projection_target() -> TestResult<ProviderModelProjectionIdentityV2> {
    let effective_configuration: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"test-provider-configuration"],
        )?)?)?;
    Ok(ProviderModelProjectionIdentityV2::new(
        plakous_model::IntegrationClass::Standard,
        "test.responses",
        "test.route",
        "test.profile.r1",
        "responses-family",
        "exact-model",
        "exact-model",
        effective_configuration,
        2,
    )?)
}

fn projection_schema(property: &str) -> TestResult<PortableToolSchemaV2> {
    Ok(PortableToolSchemaV2::new(json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object",
        "properties":{property:{"type":"string"}},
        "required":[property],
        "additionalProperties":false
    }))?)
}

fn projection_map(registry: FrozenToolRegistry) -> TestResult<ToolProjectionMap> {
    projection_map_for_target(registry, projection_target()?)
}

fn projection_map_for_target(
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
) -> TestResult<ToolProjectionMap> {
    let identity = registry
        .identity(&ToolName::new("beta")?)
        .ok_or("beta missing")?
        .clone();
    let output = PortableToolSchemaV2::new(json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object",
        "properties":{"output":{"type":"string"}},
        "required":["output"],
        "additionalProperties":false
    }))?;
    let defaults = SemanticJsonV1::from_value(json!({}))?;
    let entries = [
        ToolProjectionExposureV2::new(
            [(crate::CanonicalProcessOperationV2::Start, identity.clone())],
            "exec_command",
            "start or yield",
            projection_schema("cmd")?,
            defaults.clone(),
            ToolProjectionLoweringV2::CodexExecCommand,
            output.clone(),
            ToolProjectionRaisingV2::ProcessResult,
            target.clone(),
        )?,
        ToolProjectionExposureV2::new(
            [(crate::CanonicalProcessOperationV2::Write, identity)],
            "write_stdin",
            "write or poll",
            projection_schema("chars")?,
            defaults,
            ToolProjectionLoweringV2::CodexWriteStdin,
            output,
            ToolProjectionRaisingV2::ProcessResult,
            target.clone(),
        )?,
    ];
    Ok(ToolProjectionMap::new(registry, target, entries)?)
}

#[test]
fn compiled_request_view_preserves_every_exact_axis_and_v2_definition() -> TestResult {
    let (registry, _) = registry_in_order(&[0, 1, 2])?;
    let map = projection_map(registry)?;
    let compiled = map.compile_request_view_v2()?;

    assert_eq!(compiled.target().protocol(), map.target().protocol());
    assert_eq!(
        compiled.target().integration_profile(),
        map.target().integration_profile()
    );
    assert_eq!(compiled.target().family(), map.target().family());
    assert_eq!(
        compiled.target().requested_model(),
        map.target().requested_model()
    );
    assert_eq!(
        compiled.target().effective_model(),
        map.target().effective_model()
    );
    assert_eq!(
        compiled.target().effective_provider_configuration(),
        map.target().effective_provider_configuration()
    );
    assert_eq!(
        compiled.target().projection_revision(),
        map.target().projection_revision()
    );
    assert_eq!(compiled.registry_identity(), map.registry_digest());
    assert_eq!(compiled.exposure_map_identity(), map.digest());
    assert_eq!(compiled.definitions().len(), map.entries().len());
    for (definition, entry) in compiled.definitions().iter().zip(map.entries()) {
        assert_eq!(
            definition.canonical_name(),
            entry
                .canonical_operations()
                .values()
                .next()
                .ok_or("operation missing")?
                .name()
        );
        assert_eq!(definition.exposed_name(), entry.exposed_name());
        assert_eq!(definition.description(), entry.description());
        assert_eq!(definition.input_schema(), entry.input_schema());
        assert_eq!(definition.output_schema(), entry.output_schema());
        assert_eq!(definition.source_identity(), entry.identity());
    }
    Ok(())
}

#[test]
fn mixed_projection_descriptor_roundtrips_exact_custom_presentation() -> TestResult {
    let (registry, _) = registry_in_order(&[0, 1, 2])?;
    let beta = registry
        .identity(&ToolName::new("beta")?)
        .ok_or("beta missing")?
        .clone();
    let alpha = registry
        .identity(&ToolName::new("alpha")?)
        .ok_or("alpha missing")?
        .clone();
    let target = projection_target()?;
    let output = PortableToolSchemaV2::new(json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object", "properties":{"output":{"type":"string"}},
        "required":["output"], "additionalProperties":false
    }))?;
    let map = ToolProjectionMap::new(
        registry.clone(),
        target.clone(),
        [
            ToolProjectionExposureV2::new_custom(
                [(crate::CanonicalProcessOperationV2::Start, beta)],
                "exec",
                "execute bounded source",
                Utf8TextContractV1::new(4_096)?,
                plakous_model::ProjectedCustomToolFormatV2::lark("start: /(?s).+/")?,
                SemanticJsonV1::from_value(json!({}))?,
                ToolProjectionLoweringV2::CanonicalPassthrough,
                output.clone(),
                ToolProjectionRaisingV2::Identity,
                target.clone(),
            )?,
            ToolProjectionExposureV2::new(
                [(crate::CanonicalProcessOperationV2::Poll, alpha)],
                "wait",
                "wait for completion",
                projection_schema("id")?,
                SemanticJsonV1::from_value(json!({}))?,
                ToolProjectionLoweringV2::CanonicalPassthrough,
                output,
                ToolProjectionRaisingV2::Identity,
                target,
            )?,
        ],
    )?;
    let registry_descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
    let descriptor = FrozenToolProjectionDescriptorV2::capture(&map)?;
    let encoded = descriptor.encode_v2()?;
    assert_eq!(
        u32::from_be_bytes(encoded[8..12].try_into()?),
        TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION
    );
    let decoded = FrozenToolProjectionDescriptorV2::decode_v2(&encoded, &registry_descriptor)?;
    let legacy_lowering = Digest::sha256(
        DigestDomain::ToolDefinition,
        2,
        &[
            b"plakous-tools/projection-program-v2",
            b"lowering",
            b"canonical-passthrough",
            map.entries()[0].defaults().digest().bytes(),
        ],
    )?;
    assert_ne!(map.entries()[0].lowering_identity(), legacy_lowering);
    let mut predecessor = descriptor.wire.clone();
    predecessor.entries[0].lowering_identity = legacy_lowering;
    assert_eq!(
        FrozenToolProjectionDescriptorV2::decode_v2(
            &encode_envelope(
                EXPOSURE_V2_MAGIC,
                TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
                &predecessor,
            )?,
            &registry_descriptor,
        ),
        Err(ToolDescriptorError::IntegrityMismatch)
    );
    let resolved = decoded.resolve(registry_descriptor.resolve(&bindings(3)?)?)?;
    assert_eq!(resolved.digest(), map.digest());
    let exec = resolved.by_exposed(&ToolName::new("exec")?).ok_or("exec")?;
    let ProjectedToolPresentationV2::CustomUtf8 { input, format } = exec.presentation() else {
        return Err("exec did not retain custom presentation".into());
    };
    assert_eq!(input.max_bytes(), 4_096);
    assert_eq!(format.definition(), "start: /(?s).+/");
    assert!(exec.input_schema().is_none());
    assert!(
        resolved
            .by_exposed(&ToolName::new("wait")?)
            .ok_or("wait")?
            .input_schema()
            .is_some()
    );
    Ok(())
}

#[test]
fn compiled_request_view_never_falls_back_across_target_drift() -> TestResult {
    let (registry, _) = registry_in_order(&[0, 1, 2])?;
    let exact = projection_map(registry.clone())?.compile_request_view_v2()?;
    let other_configuration: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"other-provider-configuration"],
        )?)?)?;
    let targets = [
        ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Standard,
            "other.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            projection_target()?.effective_provider_configuration(),
            2,
        )?,
        ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "other.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            projection_target()?.effective_provider_configuration(),
            2,
        )?,
        ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "other-family",
            "exact-model",
            "exact-model",
            projection_target()?.effective_provider_configuration(),
            2,
        )?,
        ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "other-model",
            "other-model",
            projection_target()?.effective_provider_configuration(),
            2,
        )?,
        ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            other_configuration,
            2,
        )?,
        ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            projection_target()?.effective_provider_configuration(),
            3,
        )?,
    ];
    for target in targets {
        let drifted =
            projection_map_for_target(registry.clone(), target)?.compile_request_view_v2()?;
        assert_ne!(drifted.target(), exact.target());
        assert_ne!(
            drifted.exposure_map_identity(),
            exact.exposure_map_identity()
        );
    }
    Ok(())
}

#[test]
fn v2_projection_descriptor_roundtrips_multiple_names_to_one_executor() -> TestResult {
    let (registry, _) = registry_in_order(&[0, 1, 2])?;
    let map = projection_map(registry.clone())?;
    assert_eq!(map.entries().len(), 2);
    assert_eq!(
        map.entries()[0]
            .canonical_operations()
            .values()
            .next()
            .ok_or("operation missing")?
            .tool_id(),
        map.entries()[1]
            .canonical_operations()
            .values()
            .next()
            .ok_or("operation missing")?
            .tool_id()
    );
    assert_eq!(
        map.entries()[0]
            .canonical_operations()
            .values()
            .next()
            .ok_or("operation missing")?
            .executor_id(),
        map.entries()[1]
            .canonical_operations()
            .values()
            .next()
            .ok_or("operation missing")?
            .executor_id()
    );

    let registry_descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
    let registry_bytes = registry_descriptor.encode_v2()?;
    let registry_descriptor = FrozenToolRegistryDescriptor::decode_v2(&registry_bytes)?;
    let descriptor = FrozenToolProjectionDescriptorV2::capture(&map)?;
    let bytes = descriptor.encode_v2()?;
    let decoded = FrozenToolProjectionDescriptorV2::decode_v2(&bytes, &registry_descriptor)?;
    let resolved_registry = registry_descriptor.resolve(&bindings(3)?)?;
    let resolved = decoded.resolve(resolved_registry)?;
    assert_eq!(resolved.digest(), map.digest());
    assert_eq!(resolved.target(), map.target());
    assert!(matches!(
        resolved.prepare(request("alpha", r#"{"value":"x"}"#)?),
        Err(error) if error.kind() == ToolRuntimeErrorKind::UnknownTool
    ));
    assert!(matches!(
        resolved.prepare(request("beta", r#"{"value":"x"}"#)?),
        Err(error) if error.kind() == ToolRuntimeErrorKind::UnknownTool
    ));
    assert_eq!(
        resolved
            .prepare(request("exec_command", r#"{"cmd":"x"}"#)?)?
            .receipt()
            .ok_or("projected receipt missing")?
            .canonical_identity()
            .tool_id()
            .as_str(),
        "tool-1"
    );
    assert_eq!(
        resolved
            .prepare(request("write_stdin", r#"{"chars":"x"}"#)?)?
            .receipt()
            .ok_or("projected receipt missing")?
            .canonical_identity()
            .executor_id()
            .as_str(),
        "executor-1"
    );
    Ok(())
}

#[test]
fn v2_projection_refuses_collision_unknown_target_revision_and_tampering() -> TestResult {
    let (registry, _) = registry_in_order(&[0, 1, 2])?;
    let map = projection_map(registry.clone())?;
    let duplicate = map.entries()[0].clone();
    assert!(
        ToolProjectionMap::new(
            registry.clone(),
            map.target().clone(),
            [duplicate.clone(), duplicate],
        )
        .is_err()
    );
    assert_eq!(
        map.prepare(request("unknown_projection", "{}")?)
            .err()
            .map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::UnknownTool)
    );
    assert!(
        ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "requested",
            "different-effective",
            map.target().effective_provider_configuration(),
            2,
        )
        .is_err()
    );

    let registry_descriptor = FrozenToolRegistryDescriptor::capture(&registry)?;
    let descriptor = FrozenToolProjectionDescriptorV2::capture(&map)?;
    let bytes = descriptor.encode_v2()?;
    let mut predecessor = bytes.clone();
    predecessor[8..12].copy_from_slice(&1_u32.to_be_bytes());
    assert_eq!(
        FrozenToolProjectionDescriptorV2::decode_v2(&predecessor, &registry_descriptor),
        Err(ToolDescriptorError::UnsupportedVersion { found: 1 })
    );
    let mut tampered = descriptor.wire.clone();
    tampered.entries[0].canonical_operations[0].executor_id = "foreign-executor".into();
    assert_eq!(
        FrozenToolProjectionDescriptorV2::decode_v2(
            &encode_envelope(
                EXPOSURE_V2_MAGIC,
                TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
                &tampered,
            )?,
            &registry_descriptor,
        ),
        Err(ToolDescriptorError::IntegrityMismatch)
    );
    // Entry order is presentation order, but one exposed name appears once.
    let mut duplicated = descriptor.wire.clone();
    let repeated = duplicated.entries[0].clone();
    duplicated.entries.push(repeated);
    assert_eq!(
        FrozenToolProjectionDescriptorV2::decode_v2(
            &encode_envelope(
                EXPOSURE_V2_MAGIC,
                TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
                &duplicated,
            )?,
            &registry_descriptor,
        ),
        Err(ToolDescriptorError::InvalidSemantics)
    );
    Ok(())
}

fn with_payload(original: &[u8], value: &Value) -> TestResult<Vec<u8>> {
    let mut bytes = original[..HEADER_BYTES].to_vec();
    bytes.extend_from_slice(&serde_json::to_vec(value)?);
    Ok(bytes)
}

fn request(name: &str, arguments: impl Into<Arc<str>>) -> TestResult<ToolInvocationRequest> {
    Ok(ToolInvocationRequest::new(
        RunId::from_str("00000000-0000-0000-0000-000000000001")?,
        TurnId::from_str("00000000-0000-0000-0000-000000000002")?,
        AttemptId::from_str("00000000-0000-0000-0000-000000000003")?,
        CallId::from_str("00000000-0000-0000-0000-000000000004")?,
        ToolName::new(name)?,
        ToolCorrelation::default(),
        arguments,
    ))
}
