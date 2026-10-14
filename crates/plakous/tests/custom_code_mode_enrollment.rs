//! A Custom adapter cannot borrow the Codex enrollment vocabulary.

#![cfg(feature = "composition")]

use plakous::construction::provider_model_projection_identity;
use plakous::model::{LocalDurabilityDisposition, ToolPresentationMode};
use plakous::tools::{
    CodexMinimalLocalCodeModeV1, JobConfig, ToolRegistryBuilder, register_packs,
    standard_coding_packs_in_memory,
};

#[path = "../examples/common/scripted.rs"]
mod scripted;

#[test]
fn custom_descriptor_cannot_borrow_codex_code_mode_enrollment() -> scripted::Fallible<()> {
    let model = scripted::ScriptedModel::new(
        &scripted::Vocabulary {
            protocol: plakous_tools::CODEX_PROJECTION_PROTOCOL,
            route: plakous_tools::CODEX_PROJECTION_ROUTE,
            profile: plakous_tools::CODEX_PROJECTION_PROFILE,
            family: "responses-family",
            model: "gpt-5.6-luna",
        },
        Vec::new(),
    )?;
    let mut registry = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut registry)?;
    CodexMinimalLocalCodeModeV1::register_markers_into(&mut registry)?;
    let registry = registry.freeze();
    let outer = registry
        .definitions()
        .iter()
        .filter(|tool| matches!(tool.name().as_str(), "exec" | "wait"))
        .cloned()
        .collect::<Vec<_>>();
    let configuration = model.provider_configuration(
        ToolPresentationMode::LocalCompositionOnly,
        LocalDurabilityDisposition::DurableRequired,
        &outer,
    )?;
    let descriptor = CodexMinimalLocalCodeModeV1::compile(
        JobConfig::default(),
        registry.clone(),
        provider_model_projection_identity(&configuration)?,
    )?;
    let error = plakous_compose::validate_codex_minimal_local_code_mode_candidate_v1(
        &descriptor,
        descriptor.outer_projection(),
        registry,
        &configuration,
    )
    .err()
    .ok_or("Custom descriptor incorrectly borrowed Codex enrollment")?;
    assert!(
        error
            .to_string()
            .contains("not an enrolled exact Codex Code Mode target")
    );
    Ok(())
}
