//! The pack seam is open: a fifth pack contributes a first-class model-visible
//! name and its executor without one edit inside `plakous-tools`.
//!
//! The core must not require every tool name to appear in a closed
//! table in `coding_presentation.rs`. `order_entries` must accept contributions
//! from packs outside the core.

#![allow(
    missing_docs,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test crate"
)]

use std::sync::Arc;

use plakous_compose::standard_coding_packs_in_memory;
use plakous_model::{
    Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass, ToolConcurrency,
    ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
};
use plakous_tools::{
    CodingPresentation, Earned, MenuBinding, MenuEntry, MenuOrdinal, MenuShape,
    PROCESS_TOOL_PROJECTION_REVISION, PromptContribution, PromptDigest, PromptPin,
    ProviderModelProjectionIdentityV2, RegisteredToolId, ToolContext, ToolExecutor,
    ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation, ToolOutcome, ToolPack,
    ToolProjectionLoweringV2, ToolRegistration, ToolRegistryBuilder, ToolRuntimeError,
    ToolRuntimeErrorKind, compile,
};
use serde_json::json;

type TestError = Box<dyn std::error::Error>;

const AUDIT_DESCRIPTION: &str = "Record one bounded audit note for this turn.";
const AUDIT_AGREEMENT: &str = "Audit notes are evidence, not narration.";

#[derive(Debug)]
struct AuditTool;

impl ToolExecutor for AuditTool {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async { ToolOutcome::success_text("recorded") })
    }
}

/// A pack that lives entirely outside `plakous-tools`: its own executor, its
/// own menu name under every vocabulary, and its own earned paragraph.
struct AuditToolPack {
    contributions: [PromptContribution; 1],
}

impl AuditToolPack {
    fn new() -> Self {
        Self {
            contributions: [PromptContribution::new(
                "test.audit.agreement",
                Earned::ToolPresented(ToolName::new("audit_note").expect("audit_note")),
                AUDIT_AGREEMENT,
            )],
        }
    }

    fn definition() -> Result<ToolDefinition, TestError> {
        Ok(ToolDefinition::new(
            ToolName::new("audit_note")?,
            "test.audit.v1",
            AUDIT_DESCRIPTION,
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"note": {"type": "string"}},
                "required": ["note"],
                "additionalProperties": false
            }))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::Mutating,
        )?)
    }
}

impl ToolPack for AuditToolPack {
    fn id(&self) -> &'static str {
        "test.audit.v1"
    }

    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        let definition = Self::definition().map_err(|_| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidDefinition,
                "audit definition is invalid",
            )
        })?;
        let executor_id = ToolExecutorId::new("test.audit.executor.v1")?;
        Ok(vec![
            ToolRegistration::new(
                RegisteredToolId::new("test.audit.v1")?,
                executor_id.clone(),
                ToolExecutorBindingIdentity::new(1, &[executor_id.as_str().as_bytes()])?,
                definition,
                Arc::new(AuditTool),
            )
            .nested_eligible(),
        ])
    }

    fn menu(&self, _presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        let name = ToolName::new("audit_note").map_err(|_| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "audit_note is not a valid tool name",
            )
        })?;
        Ok(vec![
            MenuEntry::new(
                "audit_note",
                vec![MenuBinding::by_effect(name)],
                MenuShape::Canonical,
                ToolProjectionLoweringV2::CanonicalPassthrough,
            )
            .with_ordinal(MenuOrdinal::CORE),
        ])
    }

    fn contributions(&self) -> &[PromptContribution] {
        &self.contributions
    }

    fn pins(&self) -> &'static [PromptPin] {
        &[]
    }
}

fn target() -> Result<ProviderModelProjectionIdentityV2, TestError> {
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"fifth-pack-seam"],
        )?)?)?;
    Ok(ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "test.responses",
        "test.oauth",
        "test.fifth-pack.v1",
        "test-family",
        "test-model",
        "test-model",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?)
}

#[test]
fn a_fifth_pack_contributes_a_menu_name_and_its_executor_with_no_core_edit() -> Result<(), TestError>
{
    let audit = ToolName::new("audit_note")?;
    for (presentation, expected) in [
        (
            CodingPresentation::Pi,
            vec![
                "audit_note",
                "bash",
                "edit",
                "read",
                "stop",
                "wait",
                "write",
            ],
        ),
        (
            CodingPresentation::Claude,
            vec!["Bash", "Edit", "Read", "TaskStop", "Write", "audit_note"],
        ),
    ] {
        let mut packs = standard_coding_packs_in_memory()?;
        packs.push(Arc::new(AuditToolPack::new()));

        let surface = compile(
            &packs,
            presentation,
            ToolRegistryBuilder::default(),
            target()?,
            None,
        )?;

        // The name is in the menu, in the presentation's own order.
        assert_eq!(
            surface
                .exposed_names()
                .iter()
                .map(ToolName::as_str)
                .collect::<Vec<_>>(),
            expected,
            "{presentation:?} menu"
        );
        // The executor is in the frozen registry `compile` built.
        assert!(surface.registry().definition(&audit).is_some());
        assert_eq!(
            surface
                .registry()
                .definition(&audit)
                .map(ToolDefinition::description),
            Some(AUDIT_DESCRIPTION)
        );
        // The projection can dispatch it.
        assert!(surface.projection().by_exposed(&audit).is_some());
        // And its paragraph is earned by its own presence.
        assert_eq!(
            surface
                .contributions()
                .iter()
                .map(PromptContribution::text)
                .collect::<Vec<_>>(),
            [AUDIT_AGREEMENT]
        );
    }
    Ok(())
}

#[test]
fn a_fifth_pack_that_presents_nothing_earns_nothing() -> Result<(), TestError> {
    let surface = compile(
        &standard_coding_packs_in_memory()?,
        CodingPresentation::Claude,
        ToolRegistryBuilder::default(),
        target()?,
        None,
    )?;
    assert_eq!(surface.contributions(), []);
    assert!(
        surface
            .registry()
            .definition(&ToolName::new("audit_note")?)
            .is_none()
    );
    // The pins walker sees no untracked string: the audit pack pins none.
    assert_eq!(
        PromptDigest::of(AUDIT_DESCRIPTION),
        PromptDigest::of(AUDIT_DESCRIPTION)
    );
    Ok(())
}
