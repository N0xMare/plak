#![allow(clippy::expect_used, reason = "unit tests may assert with expect")]

use std::sync::Arc;

use plakous_model::{
    Digest, DigestDomain, IntegrationClass, ToolConcurrency, ToolDefinition, ToolEffect,
    ToolInputContract, ToolName, ToolOutputContract,
};
use plakous_tools::{
    CodingPresentation, FrozenToolRegistry, ProviderModelProjectionIdentityV2,
    PythonHarnessSdkLimitsV1, RegisteredToolId, ToolExecutor, ToolExecutorBindingIdentity,
    ToolExecutorId, ToolFuture, ToolInvocation, ToolOutcome, ToolRegistration, ToolRegistryBuilder,
};
use serde_json::{Value, json};

use super::{
    PythonHarnessSdkAccessV1, PythonHarnessSdkDescriptorV1, PythonHarnessSdkOpOrderV1,
    PythonHarnessSdkV1,
};

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

fn target() -> ProviderModelProjectionIdentityV2 {
    let digest =
        Digest::sha256(DigestDomain::EffectiveProviderConfiguration, 5, &[b"sdk"]).expect("digest");
    let configuration = serde_json::from_value(serde_json::to_value(digest).expect("digest json"))
        .expect("configuration id");
    ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Custom,
        "custom.responses",
        "consumer.route",
        "consumer.python-sdk.v1",
        "responses-family",
        "candidate-model",
        "candidate-model",
        configuration,
        1,
    )
    .expect("target")
}

fn registration(name: &ToolName) -> ToolRegistration {
    let definition = ToolDefinition::new(
        name.clone(),
        "test-v1",
        format!("Call the exact {name} test capability."),
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        }))
        .expect("schema"),
        ToolOutputContract::utf8(4096).expect("output"),
        ToolConcurrency::Parallel,
        ToolEffect::ReadOnly,
    )
    .expect("definition");
    ToolRegistration::new(
        RegisteredToolId::new(format!("test.{name}.v1")).expect("id"),
        ToolExecutorId::new(format!("test.{name}.v1")).expect("executor"),
        ToolExecutorBindingIdentity::new(1, &[name.as_str().as_bytes()]).expect("binding"),
        definition,
        Arc::new(FixedTool),
    )
    .nested_eligible()
}

fn registry(access: PythonHarnessSdkAccessV1) -> FrozenToolRegistry {
    let mut builder = ToolRegistryBuilder::default();
    for name in PythonHarnessSdkV1::nested_names(access).expect("names") {
        builder.register(registration(&name)).expect("register");
    }
    PythonHarnessSdkV1::register_markers_into(&mut builder).expect("marker");
    builder.freeze()
}

fn compile(
    kind: CodingPresentation,
    access: PythonHarnessSdkAccessV1,
) -> PythonHarnessSdkDescriptorV1 {
    PythonHarnessSdkV1::compile(
        plakous_tools::JobConfig::default(),
        registry(access),
        target(),
        kind,
        access,
    )
    .expect("compile")
}

/// The core stub text of spec 01 §9.8.3, read from the chapter itself.
fn spec_stubs() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../docs/spec/01-plakous-library.md"
    );
    let chapter = std::fs::read_to_string(path).expect("spec chapter");
    let section = &chapter[chapter.find("#### 9.8.3").expect("section")..];
    let body = &section[section.find("```python\n").expect("block") + "```python\n".len()..];
    body[..=body.find("\n```\n").expect("block end")].to_owned()
}

#[test]
fn stub_text_is_the_spec_text_byte_for_byte() {
    let descriptor = compile(CodingPresentation::Claude, PythonHarnessSdkAccessV1::Full);
    assert_eq!(descriptor.stubs(), spec_stubs());
    assert_eq!(descriptor.stubs().len(), 4_719);
    assert_eq!(
        descriptor.op_order(),
        PythonHarnessSdkOpOrderV1::ReplaceFirst
    );
    let projection = descriptor.outer_projection();
    assert_eq!(projection.entries().len(), 1);
    let run = projection
        .by_exposed(&ToolName::new("run_py".to_owned()).expect("name"))
        .expect("run_py");
    assert_eq!(run.description(), spec_stubs());
}

#[test]
fn codex_lists_patch_first_and_nothing_else_moves() {
    let codex = compile(CodingPresentation::Codex, PythonHarnessSdkAccessV1::Full);
    let other = compile(CodingPresentation::Pi, PythonHarnessSdkAccessV1::Full);
    assert_eq!(codex.op_order(), PythonHarnessSdkOpOrderV1::PatchFirst);
    let replace = "    class Replace(NamedTuple):";
    let patch = "    class Patch(NamedTuple):";
    let position = |text: &str, needle: &str| text.find(needle).expect("op class");
    assert!(position(codex.stubs(), patch) < position(codex.stubs(), replace));
    assert!(position(other.stubs(), replace) < position(other.stubs(), patch));
    let mut sorted_codex = codex.stubs().lines().collect::<Vec<_>>();
    let mut sorted_other = other.stubs().lines().collect::<Vec<_>>();
    sorted_codex.sort_unstable();
    sorted_other.sort_unstable();
    assert_eq!(sorted_codex, sorted_other);
    assert!(position(codex.bootstrap(), "Patch = ") < position(codex.bootstrap(), "Replace = "));
    assert_ne!(codex.stubs_identity(), other.stubs_identity());
    assert_ne!(codex.identity(), other.identity());
}

#[test]
fn read_execute_omits_fs_everywhere() {
    let descriptor = compile(
        CodingPresentation::Codex,
        PythonHarnessSdkAccessV1::ReadExecute,
    );
    assert!(!descriptor.stubs().contains("class fs"));
    assert!(!descriptor.stubs().contains("fs."));
    assert!(!descriptor.bootstrap().contains("class fs"));
    assert!(!descriptor.bootstrap().contains("fs_apply"));
    assert!(!descriptor.host_functions().contains(&"fs_apply"));
    assert!(
        descriptor
            .nested_tools()
            .iter()
            .all(|tool| tool.canonical_name().as_str() != "edit_files")
    );
    assert!(
        descriptor
            .nested_projection()
            .by_exposed(&ToolName::new("edit_files".to_owned()).expect("name"))
            .is_none()
    );
    let full = compile(CodingPresentation::Codex, PythonHarnessSdkAccessV1::Full);
    assert!(full.host_functions().contains(&"fs_apply"));
    assert!(
        full.nested_tools()
            .iter()
            .any(|tool| tool.canonical_name().as_str() == "edit_files")
    );
    assert_ne!(full.identity(), descriptor.identity());
}

#[test]
fn bootstrap_binds_every_stub_namespace_and_only_listed_host_functions() {
    for access in [
        PythonHarnessSdkAccessV1::Full,
        PythonHarnessSdkAccessV1::ReadExecute,
    ] {
        let descriptor = compile(CodingPresentation::Grok, access);
        for namespace in ["ctx", "fs", "proc", "state", "plan"] {
            assert_eq!(
                descriptor
                    .stubs()
                    .contains(&format!("\nclass {namespace}:")),
                descriptor
                    .bootstrap()
                    .contains(&format!("    class {namespace}:")),
                "{namespace}"
            );
        }
        for line in descriptor.bootstrap().lines() {
            if let Some(call) = line.trim().strip_prefix("return await h.") {
                let name = &call[..call.find('(').expect("call")];
                assert!(descriptor.host_functions().contains(&name), "{name}");
            }
        }
        assert!(!descriptor.bootstrap().contains("del "));
    }
}

#[test]
fn identities_change_with_limits_and_run_py_takes_only_code() {
    fn portable(value: &Value, depth: usize) {
        if let Some(object) = value.as_object() {
            assert!(!object.contains_key("oneOf") && !object.contains_key("const"));
            assert!(depth == 0 || !object.contains_key("$schema"));
            object.values().for_each(|child| portable(child, depth + 1));
        }
    }
    let compile_with = |limits| {
        PythonHarnessSdkV1::compile_with_limits(
            plakous_tools::JobConfig::default(),
            registry(PythonHarnessSdkAccessV1::Full),
            target(),
            PythonHarnessSdkOpOrderV1::ReplaceFirst,
            PythonHarnessSdkAccessV1::Full,
            limits,
        )
        .expect("compile")
    };
    let exact = compile_with(PythonHarnessSdkLimitsV1::EXACT);
    let changed = compile_with(PythonHarnessSdkLimitsV1 {
        background_cell_slots: 2,
        ..PythonHarnessSdkLimitsV1::EXACT
    });
    assert_eq!(exact.limits(), PythonHarnessSdkLimitsV1::EXACT);
    assert_ne!(exact.limits_identity(), changed.limits_identity());
    assert_ne!(exact.identity(), changed.identity());
    assert_eq!(exact.stubs_identity(), changed.stubs_identity());
    assert_eq!(
        exact.identity(),
        compile(CodingPresentation::Claude, PythonHarnessSdkAccessV1::Full).identity()
    );

    let schema = plakous_tools::python_harness_sdk_run_schema();
    assert_eq!(schema["properties"], json!({"code": {"type": "string"}}));
    assert_eq!(schema["required"], json!(["code"]));
    assert_eq!(schema["additionalProperties"], json!(false));
    portable(&schema, 0);
}
