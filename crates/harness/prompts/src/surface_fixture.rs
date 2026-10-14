//! A compiled tool surface without a host, for the tests that need one.
//!
//! The agreement paragraphs are earned by names on a compiled surface, so the
//! tests that assert which paragraph a node earns need a surface. They do not
//! need a session: the four coding packs plus a passthrough pack over fake
//! nested-eligible registrations is exactly the shape a host compiles, and it
//! is built here from `plakous-tools` alone.

#![allow(
    clippy::expect_used,
    reason = "a test fixture fails fast on an invalid literal"
)]

use std::sync::Arc;

use plakous_compose::standard_coding_packs_in_memory;
use plakous_model::{
    Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass, ToolConcurrency,
    ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
};
use plakous_tools::{
    CodingPresentation, CompiledSurface, PROCESS_TOOL_PROJECTION_REVISION, PassthroughPack,
    PromptContribution, ProviderModelProjectionIdentityV2, RegisteredToolId, ToolContext,
    ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation,
    ToolOutcome, ToolPack, ToolRegistration, ToolRegistryBuilder, compile,
};
use serde_json::json;

use crate::identity::ProductAgreementPack;

#[derive(Debug)]
struct FixedTool;

impl ToolExecutor for FixedTool {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async { ToolOutcome::success_text("ok") })
    }
}

/// One nested-eligible fake extension under the given name.
fn registration(name: &str) -> ToolRegistration {
    let definition = ToolDefinition::new(
        ToolName::new(name).expect("extension name"),
        "plakous.prompts-fixture.v1",
        format!("Call {name}."),
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))
        .expect("extension input contract"),
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )
    .expect("extension definition");
    ToolRegistration::new(
        RegisteredToolId::new(format!("plakous.prompts-fixture.{name}.v1"))
            .expect("extension tool id"),
        ToolExecutorId::new(format!("plakous.prompts-fixture.{name}.executor.v1"))
            .expect("extension executor id"),
        ToolExecutorBindingIdentity::new(1, &[name.as_bytes()]).expect("extension binding"),
        definition,
        Arc::new(FixedTool),
    )
    .nested_eligible()
}

fn target() -> ProviderModelProjectionIdentityV2 {
    let digest = Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"plakous-prompts-surface-fixture"],
    )
    .expect("fixture configuration digest");
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(digest).expect("fixture identity value"))
            .expect("fixture identity");
    ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "test.responses",
        "test.oauth",
        "test.prompts-fixture.v1",
        "test-family",
        "test-model",
        "test-model",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .expect("fixture target")
}

/// The compiled surface for one vocabulary plus the named fake extensions.
pub(crate) fn surface(presentation: CodingPresentation, names: &[&str]) -> CompiledSurface {
    let extensions = names
        .iter()
        .map(|name| ToolName::new(*name).expect("extension name"))
        .collect::<Vec<_>>();
    let mut packs = standard_coding_packs_in_memory().expect("coding packs");
    packs.push(Arc::new(PassthroughPack::new(extensions.clone())) as Arc<dyn ToolPack>);
    packs.push(Arc::new(ProductAgreementPack::new()));
    let mut builder = ToolRegistryBuilder::default();
    for name in &extensions {
        builder
            .register(registration(name.as_str()))
            .expect("fixture extension registers");
    }
    compile(&packs, presentation, builder, target(), None).expect("fixture surface")
}

/// The contributions `compile` earns for the given fake extensions.
pub(crate) fn earned_contributions(
    presentation: CodingPresentation,
    names: &[&str],
) -> Vec<PromptContribution> {
    surface(presentation, names).contributions().to_vec()
}
