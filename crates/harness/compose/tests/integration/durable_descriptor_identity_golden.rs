//! Identity golden: a change here is a wire/durable-format change and needs
//! the maintainers' decision.
//!
//! The durable tool descriptors are the only thing standing between a crate
//! move and silent identity drift: `RegisteredToolId`, `ToolExecutorId` and
//! `ToolExecutorBindingIdentity` are strings and digests over strings
//! (`coding.rs` ~83-92), not types, so a rename that a compiler cannot see
//! reopens every persisted session against a registry it no longer matches.
//!
//! Shell enrollment reads host metadata; the serialization fixture replaces
//! only that host-specific binding after independently checking its composition.
//! No tools execute. Re-pin deliberately with
//! `PLAK_PRINT_DESCRIPTOR_PINS=1 cargo test -p plakous-compose --test integration durable_descriptor_identity_golden:: -- --nocapture`.

#![allow(
    missing_docs,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test crate"
)]

use std::sync::Arc;

use plakous_compose::CodingPresentationExt as _;
use plakous_compose::standard_coding_packs;
use plakous_model::{
    Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass, ToolConcurrency,
    ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
};
use plakous_tool_plan::InMemoryPlanState;
use plakous_tool_process::ProcessSessionTools;
use plakous_tools::{
    CodingPresentation, FrozenToolProjectionDescriptorV2, FrozenToolRegistry,
    FrozenToolRegistryDescriptor, JoinedProcessShellProfile, NestedToolEligibility,
    PROCESS_SESSION_EXECUTOR_ID, PROCESS_SESSION_POLL_REGISTERED_TOOL_ID,
    PROCESS_SESSION_REGISTERED_TOOL_ID, PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID,
    PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID, PROCESS_TOOL_PROJECTION_REVISION, PlanStateIdentity,
    ProviderModelProjectionIdentityV2, RegisteredToolId, ToolContext, ToolExecutor,
    ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation, ToolOutcome,
    ToolProjectionMap, ToolRegistration, ToolRegistryBuilder, register_packs,
};
use serde_json::json;

type TestError = Box<dyn std::error::Error>;

fn sdk_registry(marker_in_nested: bool) -> Result<FrozenToolRegistry, TestError> {
    let mut builder = ToolRegistryBuilder::default();
    let process = Arc::new(ProcessSessionTools::posix_sh()?);
    register_packs(
        &standard_coding_packs(
            Arc::new(InMemoryPlanState::with_identity(PlanStateIdentity::new(
                "golden.plan",
                1,
            )?)),
            Arc::clone(&process),
        ),
        &mut builder,
    )?;
    plakous_compose::PythonHarnessSdkV1::register_into(&mut builder, process)?;
    let real = stable_serialization_fixture(&builder.freeze())?;
    if !marker_in_nested {
        return Ok(real);
    }
    let mut forged = ToolRegistryBuilder::default();
    let fixed: Arc<dyn ToolExecutor> = Arc::new(ExtensionTool);
    for definition in real.definitions() {
        if definition.name().as_str() == "run_py" {
            continue;
        }
        let identity = real.identity(definition.name()).ok_or("missing identity")?;
        let executor = if definition.name().as_str() == "read_file" {
            ToolExecutorId::new(plakous_tools::PYTHON_SDK_DRIVER_MARKER_EXECUTOR_ID_V1)?
        } else {
            identity.executor_id().clone()
        };
        let registration = ToolRegistration::new(
            identity.tool_id().clone(),
            executor,
            identity.executor_binding_identity(),
            definition.clone(),
            Arc::clone(&fixed),
        );
        forged.register(match identity.nested_eligibility() {
            NestedToolEligibility::NestedEligible => registration.nested_eligible(),
            NestedToolEligibility::DirectOnly => registration.direct_only(),
        })?;
    }
    Ok(forged.freeze())
}

#[test]
fn python_harness_sdk_identity_golden() -> Result<(), TestError> {
    let descriptor = plakous_compose::PythonHarnessSdkV1::compile(
        plakous_tools::JobConfig::default(),
        sdk_registry(false)?,
        target()?,
    )?;
    assert_eq!(
        descriptor.identity().to_string(),
        "3d607d677131a57c307ac6c0597d62932de9deacb53dfbafc656aa2d6366557a"
    );
    Ok(())
}

#[test]
fn python_harness_sdk_compile_refuses_missing_registration_and_nested_marker()
-> Result<(), TestError> {
    assert!(
        plakous_compose::PythonHarnessSdkV1::compile(
            plakous_tools::JobConfig::default(),
            registry(false)?,
            target()?
        )
        .is_err()
    );
    let error = plakous_compose::PythonHarnessSdkV1::compile(
        plakous_tools::JobConfig::default(),
        sdk_registry(true)?,
        target()?,
    )
    .err()
    .ok_or("nested marker accepted")?;
    assert!(
        error.to_string().contains("markers cannot enter"),
        "{error}"
    );
    // Both representable classes (Standard and Custom) are admitted; an
    // unknown target class is refused while decoding, before compilation.
    assert!(serde_json::from_value::<IntegrationClass>(json!("native-subscription")).is_err());
    Ok(())
}

/// identity golden: a change here is a wire/durable-format change and needs the
/// maintainers' decision.
///
/// Row order is `(case, registry descriptor bytes, projection descriptor
/// bytes, registered-identity string set)`. `case` is the
/// `CodingPresentation` for the plain catalog and `<kind>+topology` for the
/// product catalog with the two topology extensions the product compiles.
const DURABLE_DESCRIPTOR_PINS: &[(&str, &str, &str, &str)] = &[
    (
        "Pi",
        "6b928cca7e7434b3792290744857df0b3c44c72d6ef0c117906eb4c34df462c6",
        "170d9a3b76ee09517f7e02797de296343af0e27bf474bb0556cbd9d7a60cdcb1",
        "44b8430fce9ce883c54102f9808f260a6c1687c139ce13db82fe85791a178f62",
    ),
    (
        "CodexOAuth",
        "6b928cca7e7434b3792290744857df0b3c44c72d6ef0c117906eb4c34df462c6",
        "f41214594ea965f89d923f685a018371de030d371ae28f575edcb4fdac49f018",
        "44b8430fce9ce883c54102f9808f260a6c1687c139ce13db82fe85791a178f62",
    ),
    (
        "SuperGrokOAuth",
        "6b928cca7e7434b3792290744857df0b3c44c72d6ef0c117906eb4c34df462c6",
        "fe11b2a72f87e0d94d103b8f340a82df50570190b51a82f728562acc38f434cd",
        "44b8430fce9ce883c54102f9808f260a6c1687c139ce13db82fe85791a178f62",
    ),
    (
        "ClaudeOAuth",
        "6b928cca7e7434b3792290744857df0b3c44c72d6ef0c117906eb4c34df462c6",
        "6850332b1f6c4e71c324582c1e4dde3adbc940cc01486b63b27ada2d04be2f7b",
        "44b8430fce9ce883c54102f9808f260a6c1687c139ce13db82fe85791a178f62",
    ),
    (
        "Pi+topology",
        "0c88f7c742b0952a5bc7979a9736f2d07539758db138eaef8d94190e5627f94f",
        "3b78aebaff7b4c0e28b4dedfbe89c0cea06f78503fc1f54aaeb3b1896e49ddc5",
        "237f71d3fab19c96694b061fe1f8a057a7fe2d20e3a383a7ce784fb8f3ed409f",
    ),
    (
        "CodexOAuth+topology",
        "0c88f7c742b0952a5bc7979a9736f2d07539758db138eaef8d94190e5627f94f",
        "54ede79591d23b47ff7ca99e9f0052e6b8052bce965aae65a31196c55eed884d",
        "237f71d3fab19c96694b061fe1f8a057a7fe2d20e3a383a7ce784fb8f3ed409f",
    ),
    (
        "SuperGrokOAuth+topology",
        "0c88f7c742b0952a5bc7979a9736f2d07539758db138eaef8d94190e5627f94f",
        "187b854a35860e467ef8d352fb889aa59e17677e3c4686f1801627ca13ff18f1",
        "237f71d3fab19c96694b061fe1f8a057a7fe2d20e3a383a7ce784fb8f3ed409f",
    ),
    (
        "ClaudeOAuth+topology",
        "0c88f7c742b0952a5bc7979a9736f2d07539758db138eaef8d94190e5627f94f",
        "cb7479abd4694e9211bf288b69e4611a8a9fdfd5a53896cada121bcc5ace79a1",
        "237f71d3fab19c96694b061fe1f8a057a7fe2d20e3a383a7ce784fb8f3ed409f",
    ),
];

/// identity golden: a change here is a wire/durable-format change and needs the
/// maintainers' decision.
///
/// The literal durable registration strings from `coding.rs` ~83-92. They are
/// persisted in session descriptors, so a rename is a durable-format change
/// even though no type changes.
const DURABLE_PROCESS_SESSION_IDS: &[(&str, &str)] = &[
    (
        "PROCESS_SESSION_REGISTERED_TOOL_ID",
        "plakous.process-session-start.v3",
    ),
    (
        "PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID",
        "plakous.process-session-write.v3",
    ),
    (
        "PROCESS_SESSION_POLL_REGISTERED_TOOL_ID",
        "plakous.process-session-poll.v3",
    ),
    (
        "PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID",
        "plakous.process-session-terminate.v3",
    ),
    (
        "PROCESS_SESSION_EXECUTOR_ID",
        "plakous.process-session-executor.v3",
    ),
];

fn print_pins_requested() -> bool {
    std::env::var("PLAK_PRINT_DESCRIPTOR_PINS").is_ok_and(|value| value == "1")
}

/// Lowercase hex sha256 of one byte string, used by the in-code pin tables in
/// place of a committed file comparison.
fn digest_hex(bytes: &[u8]) -> String {
    let digest = Digest::sha256(DigestDomain::Blob, 1, &[bytes]).expect("golden digest");
    hex(digest.bytes())
}

fn hex(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

struct ExtensionTool;

impl ToolExecutor for ExtensionTool {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async { ToolOutcome::success_text("{}") })
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "callers construct owned JSON that becomes part of the returned schema"
)]
fn object_schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// The two product topology extensions the P0-fix test compiles, in the
/// declaration order the product uses (`send_message` sorts before
/// `spawn_agent`, so declaration order is deliberately not ascending).
fn topology_extension(name: &str) -> Result<ToolRegistration, TestError> {
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
        RegisteredToolId::new(format!("plak.topology.{name}.v1"))?,
        ToolExecutorId::new(format!("plak.topology.{name}.executor.v1"))?,
        ToolExecutorBindingIdentity::new(1, &[b"plak.topology.v1", name.as_bytes()])?,
        definition,
        Arc::new(ExtensionTool),
    )
    .nested_eligible())
}

fn registry(with_topology: bool) -> Result<FrozenToolRegistry, TestError> {
    let mut builder = ToolRegistryBuilder::default();
    // Process-local plan state mints an authority that carries the PID and a
    // process-local counter, which would put a fresh `update_plan` executor id
    // into the descriptor bytes on every run. The golden pins a fixed authority
    // instead, so the bytes are the packs' identity and nothing else.
    let plan_state = InMemoryPlanState::with_identity(PlanStateIdentity::new("golden.plan", 1)?);
    register_packs(
        &standard_coding_packs(
            Arc::new(plan_state),
            Arc::new(ProcessSessionTools::posix_sh()?),
        ),
        &mut builder,
    )?;
    if with_topology {
        builder.register(topology_extension("spawn_agent")?)?;
        builder.register(topology_extension("send_message")?)?;
    }
    stable_serialization_fixture(&builder.freeze())
}

// A shell's device/inode is execution authority, not portable format identity.
// Keep the real pack's definitions, IDs, eligibility and other bindings exact;
// use existing registration construction to substitute only the process binding.
// These executors are never invoked by descriptor/projection serialization.
pub(super) fn stable_serialization_fixture(
    real: &FrozenToolRegistry,
) -> Result<FrozenToolRegistry, TestError> {
    let shell = JoinedProcessShellProfile::new("/bin/sh", [String::from("-c")])?;
    let login =
        JoinedProcessShellProfile::new("/bin/sh", [String::from("-l"), String::from("-c")])?;
    let expected = ToolExecutorBindingIdentity::new(
        1,
        &[
            b"process-session-leftover-jobs",
            b"non-login-shell",
            &shell
                .identity()
                .binding_identity()
                .digest()
                .identity_bytes(),
            b"login-shell",
            &login
                .identity()
                .binding_identity()
                .digest()
                .identity_bytes(),
        ],
    )?;
    let stable = ToolExecutorBindingIdentity::new(1, &[b"golden.process-shell-authority"])?;
    let mut builder = ToolRegistryBuilder::default();
    let mut process_ids = Vec::new();
    let executor: Arc<dyn ToolExecutor> = Arc::new(ExtensionTool);
    for definition in real.definitions() {
        let identity = real
            .identity(definition.name())
            .expect("registered definition");
        let binding = if identity.executor_id().as_str() == PROCESS_SESSION_EXECUTOR_ID {
            assert_eq!(
                identity.executor_binding_identity(),
                expected,
                "real process registration must bind both exact enrolled shells"
            );
            process_ids.push(identity.tool_id().as_str());
            stable
        } else {
            identity.executor_binding_identity()
        };
        let registration = ToolRegistration::new(
            identity.tool_id().clone(),
            identity.executor_id().clone(),
            binding,
            definition.clone(),
            Arc::clone(&executor),
        );
        let registration = match identity.nested_eligibility() {
            NestedToolEligibility::NestedEligible => registration.nested_eligible(),
            NestedToolEligibility::DirectOnly => registration.direct_only(),
        };
        builder.register(registration)?;
    }
    process_ids.sort_unstable();
    let mut expected_ids = [
        PROCESS_SESSION_REGISTERED_TOOL_ID,
        PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID,
        PROCESS_SESSION_POLL_REGISTERED_TOOL_ID,
        PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID,
    ];
    expected_ids.sort_unstable();
    assert_eq!(process_ids, expected_ids);
    Ok(builder.freeze())
}

/// A fixed synthetic projection target: nothing here reaches a provider, and
/// every field is a literal so the descriptor bytes are deterministic.
fn target() -> Result<ProviderModelProjectionIdentityV2, TestError> {
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"durable-descriptor-identity-golden"],
        )?)?)?;
    Ok(ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "test.responses",
        "test.oauth",
        "test.product-composition.v1",
        "test-family",
        "test-model",
        "test-model",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?)
}

fn projection(
    kind: CodingPresentation,
    registry: FrozenToolRegistry,
    with_topology: bool,
) -> Result<ToolProjectionMap, TestError> {
    if with_topology {
        let extras = [
            ToolName::new("spawn_agent")?,
            ToolName::new("send_message")?,
        ];
        Ok(kind.projection_map_with_extensions(
            plakous_tools::JobConfig::default(),
            registry,
            target()?,
            &extras,
        )?)
    } else {
        Ok(kind.projection_map(plakous_tools::JobConfig::default(), registry, target()?)?)
    }
}

/// Every persisted registration string in the frozen registry, one line each,
/// in canonical (tool-id) order. This is the set that must survive a crate
/// move byte for byte.
fn registered_identity_set(registry: &FrozenToolRegistry) -> String {
    let mut rows = registry
        .definitions()
        .iter()
        .map(|definition| {
            let identity = registry
                .identity(definition.name())
                .expect("frozen registry knows its own definition");
            let binding: Digest = identity.executor_binding_identity().digest();
            let binding_hex = hex(binding.bytes());
            format!(
                "{}\t{}\t{}\t{binding_hex}",
                identity.tool_id().as_str(),
                identity.name().as_str(),
                identity.executor_id().as_str(),
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows.join("\n")
}

fn cases() -> [(&'static str, CodingPresentation, bool); 8] {
    [
        ("Pi", CodingPresentation::Pi, false),
        ("CodexOAuth", CodingPresentation::Codex, false),
        ("SuperGrokOAuth", CodingPresentation::Grok, false),
        ("ClaudeOAuth", CodingPresentation::Claude, false),
        ("Pi+topology", CodingPresentation::Pi, true),
        ("CodexOAuth+topology", CodingPresentation::Codex, true),
        ("SuperGrokOAuth+topology", CodingPresentation::Grok, true),
        ("ClaudeOAuth+topology", CodingPresentation::Claude, true),
    ]
}

/// identity golden: a change here is a wire/durable-format change and needs the
/// maintainers' decision.
#[test]
fn durable_tool_descriptor_bytes_are_pinned() -> Result<(), TestError> {
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    for (case, kind, with_topology) in cases() {
        let frozen = registry(with_topology)?;
        let map = projection(kind, frozen.clone(), with_topology)?;

        let registry_descriptor = FrozenToolRegistryDescriptor::capture(&frozen)?;
        let registry_bytes = registry_descriptor.encode_v2()?;
        let projection_descriptor = FrozenToolProjectionDescriptorV2::capture(&map)?;
        let projection_bytes = projection_descriptor.encode_v2()?;

        // The descriptors must still round-trip: a golden that pins bytes
        // nothing can decode would pass on a broken codec.
        let decoded_registry = FrozenToolRegistryDescriptor::decode_v2(&registry_bytes)?;
        let restored =
            FrozenToolProjectionDescriptorV2::decode_v2(&projection_bytes, &decoded_registry)?
                .resolve(frozen.clone())?;
        assert_eq!(restored.digest(), map.digest(), "{case} map identity");

        let registry_pin = digest_hex(&registry_bytes);
        let projection_pin = digest_hex(&projection_bytes);
        let identity_pin = digest_hex(registered_identity_set(&frozen).as_bytes());
        rows.push(format!(
            "    (\n        \"{case}\",\n        \"{registry_pin}\",\n        \"{projection_pin}\",\n        \"{identity_pin}\",\n    ),"
        ));

        let pinned = DURABLE_DESCRIPTOR_PINS
            .iter()
            .find(|(name, _, _, _)| *name == case);
        match pinned {
            None => failures.push(format!("{case}: case has no pinned descriptor row")),
            Some((_, registry_expected, projection_expected, identity_expected)) => {
                if *registry_expected != registry_pin {
                    failures.push(format!(
                        "{case}: registry descriptor bytes changed (pinned {registry_expected}, encoded {registry_pin})"
                    ));
                }
                if *projection_expected != projection_pin {
                    failures.push(format!(
                        "{case}: projection descriptor bytes changed (pinned {projection_expected}, encoded {projection_pin})"
                    ));
                }
                if *identity_expected != identity_pin {
                    failures.push(format!(
                        "{case}: registered identity set changed (pinned {identity_expected}, computed {identity_pin})\n{}",
                        registered_identity_set(&frozen)
                    ));
                }
            }
        }
    }

    for (name, pinned) in DURABLE_PROCESS_SESSION_IDS {
        let actual = match *name {
            "PROCESS_SESSION_REGISTERED_TOOL_ID" => PROCESS_SESSION_REGISTERED_TOOL_ID,
            "PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID" => PROCESS_SESSION_WRITE_REGISTERED_TOOL_ID,
            "PROCESS_SESSION_POLL_REGISTERED_TOOL_ID" => PROCESS_SESSION_POLL_REGISTERED_TOOL_ID,
            "PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID" => {
                PROCESS_SESSION_TERMINATE_REGISTERED_TOOL_ID
            }
            "PROCESS_SESSION_EXECUTOR_ID" => PROCESS_SESSION_EXECUTOR_ID,
            other => panic!("unknown durable id constant {other}"),
        };
        if actual != *pinned {
            failures.push(format!(
                "{name}: durable registration string changed (pinned {pinned}, actual {actual})"
            ));
        }
    }

    if print_pins_requested() {
        println!("DURABLE_DESCRIPTOR_PINS:\n{}", rows.join("\n"));
    }
    assert!(
        failures.is_empty(),
        "durable tool descriptor drift; a change here is a wire/durable-format change and needs the maintainers' decision (re-pin with PLAK_PRINT_DESCRIPTOR_PINS=1 cargo test -p plakous-compose --test integration durable_descriptor_identity_golden:: -- --nocapture):\n\n{}",
        failures.join("\n\n")
    );
    Ok(())
}
