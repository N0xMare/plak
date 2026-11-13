//! Detached dormant reconstruction: no tree registration, custody, driver or VCS call.
use super::super::{HostConfig, ResumeAfterLoss, startup};
use crate::agent::{ProductAgentSpec, build};
use crate::agent_tree::ProductAgentTree;
use crate::{CliFailure, CliResult};
use plakous::sessions::SessionRuntimeBinding;

pub(super) async fn runtime<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    tree: Option<&ProductAgentTree>,
    plan_authority: &str,
    stderr: &mut Vec<u8>,
) -> CliResult<SessionRuntimeBinding> {
    let node = &config.session.node;
    let child = tree.is_some_and(|tree| tree.root_agent_id != node.agent_id);
    let selection = if child {
        child_selection(
            tree.ok_or_else(|| CliFailure::new("move child has no tree"))?,
            &node.agent_id,
        )?
    } else {
        startup::root_tool_selection(&config.session)
    };
    if plan_authority.is_empty()
        || format!("plak.session.{plan_authority}").len() > 72
        || !plan_authority
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        || child && plan_authority != format!("{}.{}", node.tree_id, node.agent_id)
    {
        return Err(CliFailure::new("move plan-state identity is invalid"));
    }
    let authority = plan_authority.to_owned();
    if config.target.is_api() {
        let target = config
            .session
            .target_selection
            .api_target(&config.session.account_ref()?)?
            .ok_or_else(|| CliFailure::new("move API authority is missing"))?;
        let mut parent = crate::agent::api_topology_parent(
            config.product_home.clone(),
            node.tree_id.clone(),
            &config.session,
            config.cwd.clone(),
            config.state_roots.clone(),
            config.model_id.clone(),
            super::super::effort_from_record(config.session.effort.as_deref())?,
            target,
            authority.clone(),
            config.compaction_enabled,
            config.max_model_rounds,
            config.topology_enabled,
        );
        parent.tool_selection = selection;
        parent.plan_state_authority = authority;
        parent.topology_tree_id = Some(node.tree_id.clone());
        parent.topology_caller_id = child.then(|| node.agent_id.clone());
        let prepared = Box::pin(build::api_session_runtime_binding_for(
            &parent,
            false,
            crate::topology_runtime::TopologyHubAttach::Inspect,
            &mut None,
        ))
        .await?;
        return Ok(prepared.runtime);
    }
    native_runtime(config, child, selection, authority, stderr).await
}

async fn native_runtime<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    child: bool,
    selection: crate::tool_plan::NodeToolSelection,
    authority: String,
    stderr: &mut Vec<u8>,
) -> CliResult<SessionRuntimeBinding> {
    let node = &config.session.node;
    let spec = ProductAgentSpec {
        root_profile: (!child)
            .then(|| config.session.root_profile.clone())
            .flatten(),
        account: config.session.account_ref()?,
        prompt_host_facts: config.prompt_host_facts(),
        product_home: Some(config.product_home.clone()),
        tool_selection: selection,
        provider: config
            .target
            .native_provider()
            .ok_or_else(|| CliFailure::new("move native provider is missing"))?,
        resolved: config
            .target
            .native_resolved()
            .cloned()
            .ok_or_else(|| CliFailure::new("move native authority is missing"))?,
        effort: config.effort,
        effort_explicit: config.effort_explicit,
        workspace_root: config.session.workspace_root.clone(),
        cwd: config.cwd.clone(),
        plan_state_authority: authority,
        composition: config.session.composition,
        topology_tree_id: Some(node.tree_id.clone()),
        topology_caller_id: child.then(|| node.agent_id.clone()),
        state_roots: config.state_roots.clone(),
        scheduled_compaction: config.compaction_enabled,
        max_model_rounds: config.max_model_rounds,
        topology_enabled: config.topology_enabled,
        continue_after_loss: config.resume_after_loss == ResumeAfterLoss::Continue,
    };
    Box::pin(build::coding_move_runtime_binding(spec, stderr)).await
}

fn child_selection(
    tree: &ProductAgentTree,
    agent: &str,
) -> CliResult<crate::tool_plan::NodeToolSelection> {
    let node = tree
        .node(agent)
        .map_err(|error| CliFailure::new(error.message))?;
    let Some(template) = &node.template else {
        return Ok(crate::tool_plan::NodeToolSelection::default());
    };
    let epoch = tree
        .epoch_of(agent)
        .ok_or_else(|| CliFailure::new("move child epoch is missing"))?;
    let binding = epoch
        .binding(template)
        .ok_or_else(|| CliFailure::new("move child template is missing"))?;
    Ok(crate::tool_plan::NodeToolSelection {
        selection: crate::tool_plan::TemplateToolSelection::parse(&binding.tools)
            .map_err(|error| CliFailure::new(error.to_string()))?,
        tool_sections: epoch.tool_sections.clone(),
        mcp_tools: None,
    })
}
