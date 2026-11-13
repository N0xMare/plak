//! Shared predecessor/candidate production construction with explicit non-dispatching ports.
use super::*;
use crate::CliResult;
use crate::agent::build::coding_session_runtime_binding_for;
use crate::agent_tree::{MemoryTreeStore, TreeStore as _};

struct Trees(Arc<MemoryTreeStore>);
impl TreeStoreFactory for Trees {
    fn open(&self) -> CliResult<Arc<dyn crate::agent_tree::TreeStore>> {
        Ok(self.0.clone())
    }
}

pub(crate) async fn production_predecessor_runtime(
    roots: ProductStateRoots,
    authority: &str,
    composition: CompositionSelection,
    opening: bool,
) -> Result<plakous::sessions::SessionRuntimeBinding, Box<dyn std::error::Error>> {
    let artifact = crate::platform::tests::fixture_artifact()?;
    let workspace = artifact.parent().unwrap().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let provider = if composition == CompositionSelection::NativePtc {
        NativeProvider::Codex
    } else {
        NativeProvider::Claude
    };
    let (model, family, mode) = if provider == NativeProvider::Codex {
        (
            "gpt-5.6-sol",
            "responses-family",
            Some(NativeToolMode::CodeModeOnly),
        )
    } else {
        ("claude-opus-5-5", "messages-family", None)
    };
    let selected = selected_coding_provider_configuration(
        provider,
        ModelId::new(model)?,
        ReasoningEffort::High,
        mode,
        composition,
    )?;
    let resolved = Arc::new(plakous::subscriptions::test_support::resolved_provider(
        standard_model(&selected, family)?,
        mode,
    )?);
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        authority,
        authority,
        "owner",
        workspace.clone(),
        model,
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let store = Arc::new(MemoryTreeStore::new());
    let _revision = store.save_in_custody(&tree).map_err(|error| error.detail)?;
    let roots = roots
        .with_tree_store_factory(Arc::new(Trees(store)))
        .with_test_sidecars(Arc::new(crate::platform::tests::StableFixtureWorkers));
    let spec = ProductAgentSpec {
        root_profile: None,
        prompt_host_facts: plakous_host_protocol::PromptHostFacts::default(),
        provider,
        account: if provider == NativeProvider::Codex {
            "codex"
        } else {
            "claude"
        }
        .parse()?,
        resolved,
        effort: Some(ReasoningEffort::High),
        effort_explicit: true,
        workspace_root: workspace.clone(),
        cwd: workspace.clone(),
        plan_state_authority: authority.into(),
        composition,
        topology_tree_id: Some(authority.into()),
        topology_caller_id: None,
        state_roots: roots,
        scheduled_compaction: false,
        max_model_rounds: NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_enabled: true,
        continue_after_loss: false,
        product_home: Some(workspace.parent().unwrap().join("home")),
        tool_selection: crate::tool_plan::NodeToolSelection::default(),
    };
    Ok(Box::pin(coding_session_runtime_binding_for(
        spec,
        &mut Vec::new(),
        opening,
        SessionToolFence::Full,
        crate::topology_runtime::TopologyHubAttach::Inspect,
        &mut None,
    ))
    .await?)
}
