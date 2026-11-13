//! Production native construction over caller-owned, never-invoked authority.
use super::*;

pub(crate) fn native_fixture_spec(
    workspace: &Path,
    roots: ProductStateRoots,
    authority: &str,
) -> Result<ProductAgentSpec, Box<dyn std::error::Error>> {
    let selected = selected_coding_provider_configuration(
        NativeProvider::Claude,
        ModelId::new("claude-opus-5-5")?,
        ReasoningEffort::High,
        None,
        CompositionSelection::Direct,
    )?;
    let configured = standard_claude_model(&selected)?;
    Ok(ProductAgentSpec {
        root_profile: None,
        prompt_host_facts: plakous_host_protocol::PromptHostFacts::default(),
        provider: NativeProvider::Claude,
        account: "claude".parse()?,
        resolved: Arc::new(plakous::subscriptions::test_support::resolved_provider(
            configured, None,
        )?),
        effort: Some(ReasoningEffort::High),
        effort_explicit: true,
        workspace_root: workspace.into(),
        cwd: workspace.into(),
        plan_state_authority: authority.into(),
        composition: CompositionSelection::Direct,
        topology_tree_id: Some(authority.into()),
        topology_caller_id: None,
        state_roots: roots,
        scheduled_compaction: false,
        max_model_rounds: NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_enabled: true,
        continue_after_loss: false,
        product_home: Some(workspace.join("fixture-product")),
        tool_selection: crate::tool_plan::NodeToolSelection::default(),
    })
}
