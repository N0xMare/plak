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
        "cbb3d16db1ea628de8f1f565ce37974a7ed1b855deb7223e662d7e53dbd03f45",
        "7ae80997146a5e600164896b31e7940b7a9ee2fc30baaefaf4554e6c7b2c0e8c",
        "51dd93a3e90afc7cd958c8887e131957f68cfe81fd5f8c55bcbf8b9768652c62",
    ),
    (
        "CodexOAuth",
        "cbb3d16db1ea628de8f1f565ce37974a7ed1b855deb7223e662d7e53dbd03f45",
        "63252eb216cba748f5ca6631438b68d53ce0a0de6b6bbe0c9579896eb0e23355",
        "51dd93a3e90afc7cd958c8887e131957f68cfe81fd5f8c55bcbf8b9768652c62",
    ),
    (
        "SuperGrokOAuth",
        "cbb3d16db1ea628de8f1f565ce37974a7ed1b855deb7223e662d7e53dbd03f45",
        "6b68463d367d8aea2a98e353a95c083964eea3ad7d709abc246cf13bfa009337",
        "51dd93a3e90afc7cd958c8887e131957f68cfe81fd5f8c55bcbf8b9768652c62",
    ),
    (
        "ClaudeOAuth",
        "cbb3d16db1ea628de8f1f565ce37974a7ed1b855deb7223e662d7e53dbd03f45",
        "883534d853bca8b2b6061bdc8fb89030ceab72e1ee335eb18ba5e9c103c37147",
        "51dd93a3e90afc7cd958c8887e131957f68cfe81fd5f8c55bcbf8b9768652c62",
    ),
    (
        "Pi+topology",
        "a26a1fb236812d9abfbaead1a26ec4320b346086244a96ab714989ee201902bb",
        "2ac5de52b9c3b25f3c48160a22536cf0267465dd187141cba8bfa96bbd4a526c",
        "a3e5d879b6cab514636931e7f8f63b29517c8fdb315bbbd40ca62a686c3d3313",
    ),
    (
        "CodexOAuth+topology",
        "a26a1fb236812d9abfbaead1a26ec4320b346086244a96ab714989ee201902bb",
        "2200491026c4886cfe41c169fe5035d2765afa87c07e833b194a40ec45369392",
        "a3e5d879b6cab514636931e7f8f63b29517c8fdb315bbbd40ca62a686c3d3313",
    ),
    (
        "SuperGrokOAuth+topology",
        "a26a1fb236812d9abfbaead1a26ec4320b346086244a96ab714989ee201902bb",
        "bdc25fdbc22f01c4faf7ce8911c050598e6ccdcc88de5442a4c3e65bdf54f907",
        "a3e5d879b6cab514636931e7f8f63b29517c8fdb315bbbd40ca62a686c3d3313",
    ),
    (
        "ClaudeOAuth+topology",
        "a26a1fb236812d9abfbaead1a26ec4320b346086244a96ab714989ee201902bb",
        "48a5eba613819ebda2c9ac53dc40c9713e2ea07d90233caec59deb734d687489",
        "a3e5d879b6cab514636931e7f8f63b29517c8fdb315bbbd40ca62a686c3d3313",
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
fn stable_serialization_fixture(
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
