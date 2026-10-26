//! The native trigger changes dispatch purpose, not the source replay prefix.

use plakous_model::{Digest, ModelOperationRoot, ModelRequest, NativeOptions, StableRequestPrefix};

pub(super) fn source_prefix(request: &ModelRequest) -> Option<Digest> {
    if !matches!(
        request.metadata().root(),
        ModelOperationRoot::SessionWorkflow(_)
    ) || request.native().entries().len() != 1
        || !request.native().entries().iter().any(|(namespace, value)| {
            namespace.as_str() == "plakous.compaction"
                && value.as_str() == Some("native_responses_trigger_v1")
        })
    {
        return None;
    }
    let source = request.stable_prefix();
    let mut prefix = StableRequestPrefix::new(
        source.selected_provider_configuration_id(),
        source.effective_provider_configuration_id(),
        source.processing_tier().clone(),
        source.instructions().to_vec(),
        source.tools().to_vec(),
        source.hosted_tools().to_vec(),
        source.output().clone(),
        source.reasoning(),
        source.tool_choice().clone(),
        source.cache(),
        source.limits().clone(),
        NativeOptions::default(),
        source.resolved_harness_plan_identity(),
    )
    .ok()?
    .with_context_admission_requirement(source.context_admission_requirement())
    .ok()?;
    if let Some(projection) = source.tool_projection() {
        prefix = prefix.with_tool_projection(projection.clone()).ok()?;
    }
    Some(prefix.reuse_identity())
}
