//! Unit tests for the product coding session (construction and children).

use super::*;
use crate::agent_tree::MemoryTreeStore;
use crate::agent_tree_tools::TreeDriver;
use crate::error::CliResult;
use plakous::tools::CodingPresentationExt as _;
use plakous::tools::CompiledSurface;

/// State roots over the port doubles, for tests that build them by hand.
///
/// What these tests drive is the agent a root builds -- the surface, the
/// instructions, the child binding -- not the files behind the ports, so the
/// doubles are the honest fixture. The twelve that *do* read Plak's own
/// authorities back live in the binary.
fn test_state_roots(product: PathBuf, state_home: PathBuf) -> CliResult<ProductStateRoots> {
    Ok(ProductStateRoots::with_doubles(product, state_home))
}

/// The Direct surface for one native provider with no topology extensions.
/// The one admission site, from a route that has been selected but not
/// compiled — exactly what the CLI does before it opens a session.
fn presentation_for(provider: NativeProvider) -> CodingPresentation {
    let model = match provider {
        NativeProvider::Xai => "grok-4.5",
        NativeProvider::Claude => "claude-sonnet-5",
        _ => "gpt-5.6-luna",
    };
    crate::coding_routes::selected_route_presentation(provider, model).expect("presentation")
}

fn direct_surface(provider: NativeProvider) -> CompiledSurface {
    crate::tools::test_support::surface(presentation_for(provider), &[])
}

/// The one nested-catalog membership: the compiled surface's exposed names,
/// core first then extensions in declaration order.
fn nested_catalog(provider: NativeProvider, extensions: &ProductToolExtensions) -> Vec<ToolName> {
    let registry = coding_tool_registry_with_extensions(
        "nested-catalog",
        CompositionSelection::Direct,
        false,
        extensions,
    )
    .expect("nested catalog registry");
    crate::tools::product_surface(
        plakous::tools::JobConfig::default(),
        presentation_for(provider),
        registry,
        rendered_prompts::suite::synthetic_target(b"nested-catalog"),
        extensions,
    )
    .expect("nested catalog surface")
    .exposed_names()
    .to_vec()
}
use plakous::model::{
    AttemptId, CallId, Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass,
    RunId, ToolCallerLineage, ToolConcurrency, ToolCorrelation, ToolDefinition, ToolEffect,
    ToolInputContract, ToolName, ToolOutputContract, ToolResultPart,
};
use plakous::prompts::identity::PLAK_PRODUCT_VCS_POLICY;
use plakous::tools::{
    RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId,
    ToolFuture, ToolInvocation, ToolInvocationRequest, ToolOutcome, ToolPolicy, ToolRegistration,
};
use std::path::PathBuf;
use std::sync::Mutex;

fn fixture_workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("workspace");
    let root = tmp.path().to_path_buf();
    let cwd = root.join("nested");
    std::fs::create_dir_all(&cwd).expect("cwd");
    (tmp, root, cwd)
}

fn empty_product_extensions() -> ProductToolExtensions {
    ProductToolExtensions::new([]).expect("empty extensions")
}

/// A driver over an in-memory store for the turn-finalisation tests that
/// hold the bare tree `Arc`.
fn driver_over(tree: &Arc<Mutex<crate::agent_tree::ProductAgentTree>>) -> TreeDriver {
    let vcs = tree.lock().expect("lock").vcs_port();
    TreeDriver::new(
        Arc::clone(tree),
        Arc::new(MemoryTreeStore::new()),
        vcs,
        None,
    )
}

pub(super) fn openrouter_test_parent() -> ApiTopologyParent {
    ApiTopologyParent {
        created_output_bound: None,
        prompt_host_facts: plakous_host_protocol::PromptHostFacts::default(),
        product_home: None,
        tool_selection: crate::tool_plan::TemplateToolSelection::default(),
        workspace_root: PathBuf::from("/tmp/or-root"),
        cwd: PathBuf::from("/tmp/or-root"),
        plan_state_authority: "tree-or".into(),
        composition: CompositionSelection::Direct,
        topology_tree_id: Some("tree-or".into()),
        topology_caller_id: None,
        state_roots: test_state_roots(PathBuf::from("/tmp/plak"), PathBuf::from("/tmp/state"))
            .expect("state roots"),
        model: "vendor/parent".into(),
        effort: None,
        target: crate::binding::ApiTarget::OpenRouter {
            wire: ResolvedProtocol::Chat,
        },
        scheduled_compaction: true,
        max_model_rounds: NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_enabled: true,
    }
}

#[test]
fn openrouter_parent_names_its_own_root_account() {
    let parent = SpawnParent::Api(openrouter_test_parent());
    assert_eq!(spawn_parent_root_account(&parent), "openrouter");
}

#[test]
fn tree_spawn_accounts_admit_oauth_and_openrouter_and_refuse_platform_keys() {
    for account in [
        "codex",
        "chatgpt",
        "grok",
        "supergrok",
        "xai-oauth",
        "claude",
    ] {
        assert!(
            crate::runtime::oauth_provider_for_account(account).is_some(),
            "roster OAuth account `{account}` must map to a native provider"
        );
    }
    assert!(crate::runtime::oauth_provider_for_account("openrouter").is_none());
    for account in ["openai", "anthropic", "xai"] {
        assert!(
            crate::runtime::oauth_provider_for_account(account).is_none(),
            "platform API-key account `{account}` must stay refused"
        );
        assert_ne!(account, "openrouter");
    }
}

#[test]
fn openrouter_direct_registry_without_extensions_omits_spawn_agent() {
    let registry = coding_tool_registry_with_extensions(
        "plan-or",
        CompositionSelection::Direct,
        false,
        &ProductToolExtensions::empty(),
    )
    .expect("direct OpenRouter registry");
    assert!(
        !registry
            .definitions()
            .iter()
            .any(|definition| definition.name().as_str() == "spawn_agent"),
        "compact and non-hub OpenRouter Direct must not advertise spawn_agent"
    );
}

fn raise_direct_extra(
    projection: &ToolProjectionMap,
    name: &str,
    arguments: &str,
    canonical: &str,
    call_suffix: &str,
) {
    let prepared = projection
        .prepare(ToolInvocationRequest::new(
            RunId::from_str("00000000-0000-0000-0000-000000000201").expect("run"),
            TurnId::from_str("00000000-0000-0000-0000-000000000202").expect("turn"),
            AttemptId::from_str("00000000-0000-0000-0000-000000000203").expect("attempt"),
            CallId::from_str(&format!("00000000-0000-0000-0000-00000000020{call_suffix}"))
                .expect("call"),
            ToolName::new(name).expect("name"),
            ToolCorrelation::new(None, None, ToolCallerLineage::default()).expect("correlation"),
            Arc::<str>::from(arguments),
        ))
        .expect("prepare extra");
    let receipt = prepared.into_parts().1.expect("extra omitted its receipt");
    let raised = projection
        .raise(
            &receipt,
            ToolOutcome::success_text(canonical).expect("text"),
        )
        .unwrap_or_else(|error| panic!("{name} extra must raise: {error}"));
    let [ToolResultPart::Text(text)] = raised.parts() else {
        panic!("{name} raised extra was not one text part");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(text.text()).expect("raised json"),
        serde_json::json!({ "result": canonical })
    );
}

#[test]
fn cloned_product_state_roots_retain_one_runtime_service() {
    let temporary = tempfile::tempdir().expect("temporary product state");
    std::fs::create_dir(temporary.path().join("product")).expect("product state root");
    let roots = test_state_roots(
        temporary.path().join("product"),
        temporary.path().join("state"),
    )
    .expect("product state roots");
    let cloned = roots.clone();

    let first = roots.runtime_sessions().expect("first runtime service");
    let second = cloned.runtime_sessions().expect("retained runtime service");

    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn distinct_product_state_fixtures_do_not_share_runtime_services() {
    let temporary = tempfile::tempdir().expect("temporary product state");
    let first_root = temporary.path().join("first");
    let second_root = temporary.path().join("second");
    std::fs::create_dir_all(&first_root).expect("first fixture root");
    std::fs::create_dir_all(&second_root).expect("second fixture root");
    let first = test_state_roots(first_root.clone(), first_root.join("state"))
        .expect("first product state roots")
        .runtime_sessions()
        .expect("first runtime service");
    let second = test_state_roots(second_root.clone(), second_root.join("state"))
        .expect("second product state roots")
        .runtime_sessions()
        .expect("second runtime service");

    assert!(!Arc::ptr_eq(&first, &second));
    assert!(first_root.join("runtime.sqlite3").is_file());
    assert!(second_root.join("runtime.sqlite3").is_file());
}

#[test]
fn workspace_must_be_disjoint_from_product_and_subscription_state() {
    assert!(
        ensure_workspace_excludes_product_state(
            Path::new("/work/project"),
            [
                PathBuf::from("/state/plak"),
                PathBuf::from("/state/xdg/plak/subscriptions")
            ],
        )
        .is_ok()
    );
    assert!(
        ensure_workspace_excludes_product_state(
            Path::new("/state"),
            [PathBuf::from("/state/plak")],
        )
        .is_err()
    );
    assert!(
        ensure_workspace_excludes_product_state(
            Path::new("/state/plak/session"),
            [PathBuf::from("/state/plak")],
        )
        .is_err()
    );
}

#[derive(Debug)]
struct FixedExtensionTool;

impl ToolExecutor for FixedExtensionTool {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async { ToolOutcome::success_text("ok") })
    }
}

fn synthetic_product_extension(name: &str) -> ToolRegistration {
    let definition = ToolDefinition::new(
        ToolName::new(name).expect("extension name"),
        "plak.synthetic-extension.v1",
        "Call the synthetic product extension.",
        ToolInputContract::structured(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        }))
        .expect("extension input"),
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )
    .expect("extension definition");
    ToolRegistration::new(
        RegisteredToolId::new(format!("plak.synthetic.{name}.v1")).expect("extension id"),
        ToolExecutorId::new(format!("plak.synthetic.{name}.executor.v1"))
            .expect("extension executor id"),
        ToolExecutorBindingIdentity::new(1, &[name.as_bytes()]).expect("extension binding"),
        definition,
        Arc::new(FixedExtensionTool),
    )
    .nested_eligible()
}

#[test]
fn plak_explicitly_selects_always_allow_effect_policy() {
    let revision = product_effect_policy().revision();
    assert_eq!(revision.policy_id(), "plakous.allow-registered");
    assert_eq!(revision.revision(), 1);
}

#[test]
fn new_plan_enrolls_global_then_root_to_cwd_sources() {
    let temporary = tempfile::tempdir().expect("temporary instruction tree");
    let home = temporary.path().join("home");
    let root = temporary.path().join("workspace");
    let cwd = root.join("nested");
    std::fs::create_dir_all(&home).expect("product home");
    std::fs::create_dir_all(&cwd).expect("workspace cwd");
    std::fs::write(home.join("AGENTS.md"), "global instructions").expect("global source");
    std::fs::write(root.join("AGENTS.md"), "root instructions").expect("root source");
    std::fs::write(cwd.join("AGENTS.md"), "nested instructions").expect("nested source");
    let family_profile = plakous::profiles::neutral_coding().expect("family profile");
    let model = ModelId::new("gpt-5.6-sol").expect("model");
    let profile = product_identity_profile(
        &family_profile,
        NativeProvider::Codex,
        &model,
        false,
        CompositionSelection::Direct,
        &direct_surface(NativeProvider::Codex),
        &root,
        &cwd,
    )
    .expect("product profile");

    let (profile, facts) = enroll_product_instructions(&profile, Some(&home), &root, &cwd)
        .expect("instruction enrollment");

    assert_eq!(
        profile.instruction_strategy(),
        InstructionCompositionStrategy::BaseThenSources
    );
    assert!(
        profile
            .base_instructions()
            .starts_with(family_profile.base_instructions())
    );
    assert!(profile.base_instructions().contains("You are gpt-5.6-sol."));
    assert!(profile.base_instructions().contains("Workspace root:"));
    assert_eq!(
        facts
            .iter()
            .map(ResolvedContextSourceFact::content)
            .collect::<Vec<_>>(),
        [
            "global instructions",
            "root instructions",
            "nested instructions"
        ]
    );
    assert_eq!(
        profile
            .instruction_sources()
            .iter()
            .map(ContextSourceSelector::source_id)
            .collect::<Vec<_>>(),
        facts
            .iter()
            .map(ResolvedContextSourceFact::source_id)
            .collect::<Vec<_>>()
    );
}

#[test]
fn javascript_and_python_overlays_enroll_agents_md() {
    let temporary = tempfile::tempdir().expect("temporary instruction tree");
    let home = temporary.path().join("home");
    let root = temporary.path().join("workspace");
    let cwd = root.join("nested");
    std::fs::create_dir_all(&home).expect("product home");
    std::fs::create_dir_all(&cwd).expect("workspace cwd");
    std::fs::write(home.join("AGENTS.md"), "Prefer apply_patch for all edits")
        .expect("global source");
    std::fs::write(root.join("AGENTS.md"), "Use Read and Grep").expect("root source");
    std::fs::write(cwd.join("AGENTS.md"), "nested Direct advice").expect("nested source");
    let model = ModelId::new("gpt-5.6-sol").expect("model");
    for composition in [CompositionSelection::JsPtc, CompositionSelection::PyPtc] {
        let base = selected_harness_profile(
            NativeProvider::Codex,
            &model,
            ReasoningEffort::Medium,
            false,
            false,
            &[],
            false,
            composition,
            &direct_surface(NativeProvider::Codex),
            &root,
            &cwd,
        )
        .expect("overlay profile")
        .expect("new overlay session profile");
        let profile = base;
        let (enrolled, facts) = enroll_product_instructions(&profile, Some(&home), &root, &cwd)
            .expect("overlay enrollment");
        assert_eq!(facts.len(), 3);
        assert_eq!(
            enrolled.instruction_strategy(),
            InstructionCompositionStrategy::BaseThenSources
        );
        assert_eq!(enrolled.instruction_sources().len(), 3);
        assert!(
            facts
                .iter()
                .any(|fact| fact.content().contains("Prefer apply_patch"))
        );
        assert!(
            facts
                .iter()
                .any(|fact| fact.content().contains("Use Read and Grep"))
        );
    }
}

#[test]
fn absent_optional_instructions_preserve_base_only_profile() {
    let temporary = tempfile::tempdir().expect("temporary empty workspace");
    let model = ModelId::new("gpt-5.6-sol").expect("model");
    let profile = selected_harness_profile(
        NativeProvider::Codex,
        &model,
        ReasoningEffort::Medium,
        false,
        false,
        &[],
        false,
        CompositionSelection::FollowCatalog,
        &direct_surface(NativeProvider::Codex),
        temporary.path(),
        temporary.path(),
    )
    .expect("product profile selection")
    .expect("new product session profile");
    let (enrolled, facts) = enroll_product_instructions(
        &profile,
        Some(&temporary.path().join("missing-home")),
        temporary.path(),
        temporary.path(),
    )
    .expect("missing optional instructions");
    assert_eq!(enrolled, profile);
    assert!(
        enrolled
            .base_instructions()
            .contains("You are gpt-5.6-sol.")
    );
    assert!(
            enrolled.base_instructions().contains(
                "This turn's prompts/instructions identify you; earlier messages show session/agent history."
            )
        );
    assert!(
        enrolled
            .base_instructions()
            .contains("Commit or push only when the user asks.")
    );
    assert_eq!(facts, [] as [ResolvedContextSourceFact; 0]);
}

#[test]
fn exact_resume_uses_the_durable_harness_plan_authority() {
    let model = ModelId::new("gpt-5.6-sol").expect("model");
    let (_tmp, root, cwd) = fixture_workspace();
    let resumed = selected_harness_profile(
        NativeProvider::Codex,
        &model,
        ReasoningEffort::Medium,
        true,
        false,
        &[],
        false,
        CompositionSelection::FollowCatalog,
        &direct_surface(NativeProvider::Codex),
        &root,
        &cwd,
    )
    .expect("durable resume profile selection");
    assert!(
        resumed.is_none(),
        "resume must not offer a fresh product plan"
    );
    // Reopen still reconstructs stored V2 from the checkpoint. Pre-V2
    // conversations are refused by require_reconstructed_coding_projection.

    let created = selected_harness_profile(
        NativeProvider::Codex,
        &model,
        ReasoningEffort::Medium,
        false,
        false,
        &[],
        false,
        CompositionSelection::FollowCatalog,
        &direct_surface(NativeProvider::Codex),
        &root,
        &cwd,
    )
    .expect("new-session profile selection")
    .expect("new sessions retain an explicit product plan");
    assert_eq!(created.id().as_str(), "plakous.codex.coding.v1");
}

#[test]
fn subagent_enabled_catalog_suffix_is_valid() {
    let base = plakous::profiles::codex::recommended().expect("Codex family profile");
    let model = ModelId::new("gpt-5.6-sol").expect("model");
    let (_tmp, root, cwd) = fixture_workspace();
    let profile = product_identity_profile(
        &base,
        NativeProvider::Codex,
        &model,
        true,
        CompositionSelection::Direct,
        &direct_surface(NativeProvider::Codex),
        &root,
        &cwd,
    )
    .expect("subagent catalog profile");
    assert_eq!(
        profile.id().as_str(),
        "plakous.codex.coding.v1.subagents.v1"
    );
    let without = product_identity_profile(
        &base,
        NativeProvider::Codex,
        &model,
        false,
        CompositionSelection::Direct,
        &direct_surface(NativeProvider::Codex),
        &root,
        &cwd,
    )
    .expect("base catalog profile");
    assert_eq!(without.id().as_str(), "plakous.codex.coding.v1");
}

#[test]
fn product_identity_is_exact_and_rebind_stable_across_provider_selections() {
    for (provider, model_literal, family_profile) in [
        (
            NativeProvider::Codex,
            "gpt-5.6-sol",
            plakous::profiles::codex::recommended().expect("Codex family profile"),
        ),
        (
            NativeProvider::Xai,
            "grok-code-fast-1",
            plakous::profiles::grok::recommended().expect("Grok family profile"),
        ),
        (
            NativeProvider::Claude,
            "claude-sonnet-4-5",
            plakous::profiles::claude::recommended().expect("Claude family profile"),
        ),
    ] {
        let model = ModelId::new(model_literal).expect("model");
        let (_tmp, root, cwd) = fixture_workspace();
        let selected = selected_harness_profile(
            provider,
            &model,
            ReasoningEffort::Medium,
            false,
            false,
            &[],
            false,
            CompositionSelection::FollowCatalog,
            &direct_surface(provider),
            &root,
            &cwd,
        )
        .expect("product profile selection")
        .expect("new product session profile");
        assert_eq!(selected.id(), family_profile.id());
        assert!(
            selected
                .base_instructions()
                .starts_with(family_profile.base_instructions())
        );
        assert!(selected.base_instructions().contains("Workspace root:"));
        assert!(selected.base_instructions().contains("cwd:"));
        assert!(
            selected
                .base_instructions()
                .contains(&format!("OS: {}.", std::env::consts::OS))
        );
        assert!(
            selected
                .base_instructions()
                .contains(&format!("You are {model_literal}."))
        );
        assert!(
                selected
                    .base_instructions()
                    .contains("This turn's prompts/instructions identify you; earlier messages show session/agent history.")
            );
        assert_eq!(
            selected
                .base_instructions()
                .matches(PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER)
                .count(),
            1
        );
        assert_eq!(
                selected
                    .base_instructions()
                    .matches("This turn's prompts/instructions identify you; earlier messages show session/agent history.")
                    .count(),
                1
            );
        assert_eq!(
            selected
                .base_instructions()
                .matches(PLAK_PRODUCT_VCS_POLICY)
                .count(),
            1
        );
    }
}

#[test]
fn xai_defaults_direct_and_local_composition_stays_product_owned() {
    assert_eq!(
        effective_presentation_label(
            CompositionSelection::FollowCatalog,
            Some(NativeProvider::Xai),
            None,
        ),
        "direct"
    );
    assert_eq!(
        effective_presentation_label(CompositionSelection::JsPtc, Some(NativeProvider::Xai), None,),
        "js-ptc"
    );
    assert_eq!(
        effective_presentation_label(CompositionSelection::PyPtc, Some(NativeProvider::Xai), None,),
        "py-ptc"
    );
}

#[test]
fn code_mode_selection_uses_the_outer_and_nested_tool_contract() {
    let model = ModelId::new("gpt-5.6-sol").expect("model");
    let (_tmp, root, cwd) = fixture_workspace();
    let selected = selected_harness_profile(
        NativeProvider::Codex,
        &model,
        ReasoningEffort::Medium,
        false,
        true,
        &[],
        false,
        CompositionSelection::FollowCatalog,
        &direct_surface(NativeProvider::Codex),
        &root,
        &cwd,
    )
    .expect("Code Mode product profile")
    .expect("new Code Mode session profile");
    assert_eq!(selected.id().as_str(), "plakous.codex.code-mode.v1");
    assert!(selected.base_instructions().contains("exec and wait"));
    assert!(selected.base_instructions().contains("tools object"));
    assert!(selected.base_instructions().contains(
        "nested tools on its tools object: exec_command, write_stdin, apply_patch, update_plan."
    ));
    assert!(!selected.base_instructions().contains("read_file"));
    assert!(!selected.base_instructions().contains("inspect_image"));
    assert!(
            selected
                .base_instructions()
                .contains("This turn's prompts/instructions identify you; earlier messages show session/agent history.")
        );
    assert!(
        selected_harness_profile(
            NativeProvider::Xai,
            &model,
            ReasoningEffort::Medium,
            false,
            true,
            &[],
            false,
            CompositionSelection::FollowCatalog,
            &direct_surface(NativeProvider::Xai),
            &root,
            &cwd,
        )
        .is_err()
    );
}

#[test]
fn hub_harness_profile_does_not_prepend_spawn_class_cards() {
    let (_tmp, root, cwd) = fixture_workspace();
    let model = ModelId::new("gpt-5.6-sol").expect("model");
    let profile = selected_harness_profile(
        NativeProvider::Codex,
        &model,
        ReasoningEffort::Medium,
        false,
        false,
        &[],
        true,
        CompositionSelection::FollowCatalog,
        &direct_surface(NativeProvider::Codex),
        &root,
        &cwd,
    )
    .expect("profile")
    .expect("hub profile");
    assert!(
        !profile
            .base_instructions()
            .contains("Pinned classes this epoch:")
    );
    assert!(!profile.base_instructions().contains("Spawn by class only"));
}

#[test]
fn product_javascript_and_python_replace_direct_tool_catalog_instructions() {
    let (_tmp, root, cwd) = fixture_workspace();
    let catalog = nested_catalog(NativeProvider::Codex, &empty_product_extensions());
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        let model = ModelId::new("gpt-5.6-sol").expect("model");
        let provider_catalog = nested_catalog(provider, &empty_product_extensions());
        let base = selected_harness_profile(
            provider,
            &model,
            ReasoningEffort::Medium,
            false,
            false,
            &[],
            false,
            CompositionSelection::JsPtc,
            &direct_surface(provider),
            &root,
            &cwd,
        )
        .expect("javascript profile")
        .expect("new javascript session profile");
        let javascript = base;
        assert!(javascript.id().as_str().ends_with(".js-ptc.v1"));
        assert!(javascript.base_instructions().contains("exec_js"));
        assert!(javascript.base_instructions().contains("JSON field source"));
        assert!(javascript.base_instructions().contains("text(...)"));
        assert!(
            javascript
                .base_instructions()
                .contains("await tools.name({...})")
        );
        assert!(
            javascript
                .base_instructions()
                .contains("complete provider-visible tool set is exec_js")
        );
        assert!(
            javascript
                .base_instructions()
                .contains("Nested tools available inside cells:")
        );
        for name in &provider_catalog {
            assert!(javascript.base_instructions().contains(name.as_str()));
        }
        assert!(
            javascript
                .base_instructions()
                .contains("do not echo the source as assistant text")
        );
        assert!(
            !javascript
                .base_instructions()
                .contains("prefer apply_patch")
        );
        assert!(!javascript.base_instructions().contains("exec and wait"));

        let base = selected_harness_profile(
            provider,
            &model,
            ReasoningEffort::Medium,
            false,
            false,
            &[],
            false,
            CompositionSelection::PyPtc,
            &direct_surface(provider),
            &root,
            &cwd,
        )
        .expect("python profile")
        .expect("new python session profile");
        let python = base;
        assert!(python.id().as_str().ends_with(".py-ptc.v1"));
        // The overlay replaces the family prompt but must keep the product
        // identity tail (dropped before 2026-09-04).
        assert_eq!(
            python
                .base_instructions()
                .matches(PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER)
                .count(),
            1
        );
        assert_eq!(
            javascript
                .base_instructions()
                .matches(PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER)
                .count(),
            1
        );
        assert!(python.base_instructions().contains(PLAK_PRODUCT_VCS_POLICY));
        assert!(
            javascript
                .base_instructions()
                .contains(PLAK_PRODUCT_VCS_POLICY)
        );
        assert!(python.base_instructions().contains("exec_py"));
        assert!(
            python
                .base_instructions()
                .contains("code (string), not source")
        );
        assert!(
            python
                .base_instructions()
                .contains("complete provider-visible tool set is exec_py")
        );
        assert!(
            python
                .base_instructions()
                .contains("Nested tools available inside cells:")
        );
        for name in &provider_catalog {
            assert!(python.base_instructions().contains(name.as_str()));
        }
        assert!(
            python
                .base_instructions()
                .contains("do not echo the code as assistant text")
        );
        assert!(python.base_instructions().contains("await name({...})"));
        assert!(!python.base_instructions().contains("prefer apply_patch"));
        assert!(python.base_instructions().contains("wait_py"));
        let _ = &catalog;
    }
}

#[test]
fn coding_tool_executors_rebind_by_stable_product_session_identity() {
    let first = coding_tool_registry(
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        CompositionSelection::FollowCatalog,
        false,
    )
    .expect("first product registry");
    let second = coding_tool_registry(
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        CompositionSelection::FollowCatalog,
        false,
    )
    .expect("reconstructed product registry");
    let descriptor = plakous::tools::FrozenToolRegistryDescriptor::capture(&first)
        .expect("durable registry descriptor");
    let bindings = plakous::tools::ToolExecutorBindings::capture(&second);
    let rebound = descriptor
        .resolve(&bindings)
        .expect("every durable executor identity must rebind exactly");
    assert_eq!(
        plakous::tools::FrozenToolRegistryDescriptor::capture(&rebound)
            .expect("rebound descriptor"),
        descriptor
    );
    let names = first
        .definitions()
        .iter()
        .map(|definition| definition.name().as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"process_session_start"));
    assert!(names.contains(&"process_session_write"));
    assert!(names.contains(&"process_session_poll"));
    assert!(names.contains(&"process_session_terminate"));
    assert!(!names.contains(&"exec"));
    assert!(!names.contains(&"wait"));
    assert!(!names.contains(&"run_process"));
}

/// Live-drive regression (`/resume` from an attached session): the reopen
/// registry must be built from the target session's plan-state authority.
/// A registry built for the attached session lacks the target's
/// `update_plan` executor id, and exact reconstruction fails closed.
#[test]
fn exact_reopen_registry_rebinds_only_with_the_target_session_authority() {
    let target = "1d1ddaa2-90e4-4d19-a6c7-5cc4b3e9f07b";
    let attached = "2a209310-02f4-4977-97e2-8c1ecf57d506";
    let persisted = coding_tool_registry(target, CompositionSelection::Direct, false)
        .expect("target session registry");
    let descriptor = plakous::tools::FrozenToolRegistryDescriptor::capture(&persisted)
        .expect("durable registry descriptor");

    let wrong = coding_tool_registry(attached, CompositionSelection::Direct, false)
        .expect("attached session registry");
    let error = descriptor
        .resolve(&plakous::tools::ToolExecutorBindings::capture(&wrong))
        .expect_err("a registry built for another session must not rebind");
    let plakous::tools::ToolBindingError::MissingBinding { id } = error else {
        panic!("expected the target plan executor to be missing, got {error}");
    };
    assert_eq!(
        id.as_str(),
        format!("plakous.standard.update-plan.v1.plak.session.{target}.r1")
    );

    let fresh = coding_tool_registry(target, CompositionSelection::Direct, false)
        .expect("fresh target registry after restart");
    let rebound = descriptor
        .resolve(&plakous::tools::ToolExecutorBindings::capture(&fresh))
        .expect("the target session authority rebinds every durable executor");
    assert_eq!(
        plakous::tools::FrozenToolRegistryDescriptor::capture(&rebound)
            .expect("rebound descriptor"),
        descriptor
    );
}

#[test]
fn code_mode_markers_exist_only_for_the_exact_admitted_attachment() {
    let authority = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let exact_model = ModelId::new("gpt-5.6-sol").expect("exact Code Mode model");
    let direct_model = ModelId::new("gpt-5.3-codex-spark").expect("direct Codex model");
    let grok_model = ModelId::new("grok-4.6").expect("Grok model");
    let claude_model = ModelId::new("claude-sonnet-5").expect("Claude model");
    let cases = [
        (
            NativeProvider::Codex,
            &exact_model,
            Some(NativeToolMode::CodeModeOnly),
            true,
        ),
        (
            NativeProvider::Codex,
            &exact_model,
            Some(NativeToolMode::Direct),
            false,
        ),
        (
            NativeProvider::Codex,
            &direct_model,
            Some(NativeToolMode::CodeModeOnly),
            false,
        ),
        (NativeProvider::Xai, &grok_model, None, false),
        (NativeProvider::Claude, &claude_model, None, false),
    ];
    let unbound_identity =
        coding_tool_registry(authority, CompositionSelection::FollowCatalog, false)
            .expect("unbound product registry")
            .registry_identity_digest()
            .expect("unbound registry identity");

    for (provider, model, mode, expected_attachment) in cases {
        let attachment = is_exact_code_mode_attachment(provider, model, mode);
        assert_eq!(attachment, expected_attachment);
        let registry =
            coding_tool_registry(authority, CompositionSelection::FollowCatalog, attachment)
                .expect("product registry");
        let names = registry
            .definitions()
            .iter()
            .map(|definition| definition.name().as_str())
            .collect::<Vec<_>>();
        assert_eq!(names.contains(&"exec"), expected_attachment);
        assert_eq!(names.contains(&"wait"), expected_attachment);
        if expected_attachment {
            assert_ne!(
                registry
                    .registry_identity_digest()
                    .expect("attached registry identity"),
                unbound_identity
            );
        } else {
            assert_eq!(
                registry
                    .registry_identity_digest()
                    .expect("unbound registry identity"),
                unbound_identity,
                "unbound {provider:?}/{model} must retain the dormant registry identity"
            );
        }
    }
}

#[test]
fn coding_selection_rejects_models_without_an_exact_process_presentation_candidate() {
    for (provider, model) in [
        (NativeProvider::Codex, "gpt-5.3-codex-neighbor"),
        (NativeProvider::Xai, "grok-build-0.1"),
        (NativeProvider::Codex, "grok-4.5"),
        (NativeProvider::Xai, "gpt-5.6-luna"),
    ] {
        assert!(
            selected_coding_provider_configuration(
                provider,
                ModelId::new(model).expect("model"),
                ReasoningEffort::Medium,
                None,
                CompositionSelection::FollowCatalog,
            )
            .is_err(),
            "unenrolled tuple {provider:?}/{model} must fail closed"
        );
    }
}

#[test]
fn child_frozen_registry_compiles_submit_without_spawn() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};
    use crate::topology_extensions::topology_tool_extensions;

    let root = tempfile::tempdir().expect("workspace");
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        "tree1",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    let spawned = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .expect("spawn");
    let extensions = topology_tool_extensions(Arc::new(Mutex::new(tree)), spawned.agent_id)
        .expect("child extensions");
    let registry = coding_tool_registry_with_extensions(
        "child-plan-state",
        CompositionSelection::Direct,
        false,
        &extensions,
    )
    .expect("child registry");
    let names = registry
        .definitions()
        .iter()
        .map(|definition| definition.name().as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"submit_result"));
    assert!(names.contains(&"send_agent_message"));
    assert!(names.contains(&"list_agents"));
    assert!(!names.contains(&"spawn_agent"));
    assert!(!names.contains(&"merge_workspace"));
    assert!(!names.contains(&"discard_workspace"));
    assert!(!names.contains(&"wait_agent"));
    assert!(!names.contains(&"interrupt_agent"));
    assert!(!names.contains(&"close_agent"));
}

#[test]
fn root_topology_extensions_do_not_enter_code_mode_until_explicitly_compiled() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::WriterMode;
    use crate::topology_extensions::topology_tool_extensions;

    let root = tempfile::tempdir().expect("workspace");
    let tree = ProductAgentTree::bootstrap_root_for_test(
        "tree-root-catalog",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    let extensions =
        topology_tool_extensions(Arc::new(Mutex::new(tree)), "root").expect("root extensions");
    let registry = coding_tool_registry_with_extensions(
        "root-plan-state",
        CompositionSelection::FollowCatalog,
        true,
        &extensions,
    )
    .expect("root registry");
    let names = registry
        .definitions()
        .iter()
        .map(|definition| definition.name().as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"spawn_agent"));
    assert!(names.contains(&"exec"));
    assert!(names.contains(&"wait"));

    let catalog = nested_catalog(NativeProvider::Codex, &extensions);
    let nested = catalog.iter().map(ToolName::as_str).collect::<Vec<_>>();
    let core = nested_catalog(NativeProvider::Codex, &ProductToolExtensions::empty())
        .iter()
        .map(|name| name.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(&nested[..core.len()], core.as_slice());
    assert!(nested.contains(&"spawn_agent"));

    let configuration: EffectiveProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"root-topology-code-mode"],
            )
            .expect("configuration digest"),
        )
        .expect("configuration encoding"),
    )
    .expect("configuration identity");
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "openai.codex.responses",
        "openai.codex_subscription",
        "openai.codex.compat.0.153.3",
        "responses-family",
        "gpt-codex-code-mode-test",
        "gpt-codex-code-mode-test",
        configuration,
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .expect("projection target");
    let four = CodexMinimalLocalCodeModeV1::compile(
        plakous::tools::JobConfig::default(),
        registry.clone(),
        target.clone(),
    )
    .expect("four-tool compile ignores NestedEligible extras");
    assert_eq!(four.nested_tools().len(), 4);
    assert!(!four.exec_description().contains("### `spawn_agent`"));

    let extras = extensions.names().cloned().collect::<Vec<_>>();
    let extended = CodexMinimalLocalCodeModeV1::compile_with_extensions(
        plakous::tools::JobConfig::default(),
        registry,
        target,
        &extras,
    )
    .expect("explicit topology extras");
    assert!(
        extended
            .nested_tools()
            .iter()
            .any(|tool| tool.exposed_name().as_str() == "spawn_agent")
    );
    assert!(extended.exec_description().contains("### `spawn_agent`"));
    assert_ne!(four.identity(), extended.identity());
}

#[test]
fn direct_oauth_projection_exposes_explicit_topology_extensions() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::WriterMode;
    use crate::topology_extensions::topology_tool_extensions;

    let root = tempfile::tempdir().expect("workspace");
    let tree = ProductAgentTree::bootstrap_root_for_test(
        "tree-direct-catalog",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    let extensions =
        topology_tool_extensions(Arc::new(Mutex::new(tree)), "root").expect("root extensions");
    let extras = extensions.names().cloned().collect::<Vec<_>>();
    let registry = coding_tool_registry_with_extensions(
        "direct-plan-state",
        CompositionSelection::Direct,
        false,
        &extensions,
    )
    .expect("direct registry");
    let configuration: EffectiveProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"direct-topology"],
            )
            .expect("configuration digest"),
        )
        .expect("configuration encoding"),
    )
    .expect("configuration identity");
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "test.responses",
        "test.oauth",
        "test.product-composition.v1",
        "test-family",
        "test-model",
        "test-model",
        configuration,
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .expect("projection target");
    for kind in [
        CodingPresentation::Grok,
        CodingPresentation::Claude,
        CodingPresentation::Pi,
        CodingPresentation::Codex,
    ] {
        let core = kind
            .projection_map(
                plakous::tools::JobConfig::default(),
                registry.clone(),
                target.clone(),
            )
            .expect("core Direct catalog");
        assert!(
            !core
                .entries()
                .iter()
                .any(|entry| entry.exposed_name().as_str() == "spawn_agent")
        );
        let extended = kind
            .projection_map_with_extensions(
                plakous::tools::JobConfig::default(),
                registry.clone(),
                target.clone(),
                &extras,
            )
            .expect("extended Direct catalog");
        assert!(
            extended
                .entries()
                .iter()
                .any(|entry| entry.exposed_name().as_str() == "spawn_agent")
        );
        assert_ne!(core.digest(), extended.digest());
        raise_direct_extra(
            &extended,
            "spawn_agent",
            r#"{"template":"review","description":"reader","prompt":"t","output_schema_json":"{}","tier":"read_only"}"#,
            r#"{"agent_id":"c1","attempt_id":"c1-a1","status":"running","tier":"read_only","workspace":{"id":"ws","mode":"inherit","base":null}}"#,
            "4",
        );
    }
}

#[test]
fn child_direct_oauth_projection_omits_spawn() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};
    use crate::topology_extensions::topology_tool_extensions;

    let root = tempfile::tempdir().expect("workspace");
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        "tree-direct-child-catalog",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    let spawned = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .expect("child");
    let extensions = topology_tool_extensions(Arc::new(Mutex::new(tree)), spawned.agent_id)
        .expect("child extensions");
    let extras = extensions.names().cloned().collect::<Vec<_>>();
    assert!(extras.iter().any(|name| name.as_str() == "submit_result"));
    assert!(!extras.iter().any(|name| name.as_str() == "spawn_agent"));
    let registry = coding_tool_registry_with_extensions(
        "direct-child-plan-state",
        CompositionSelection::Direct,
        false,
        &extensions,
    )
    .expect("child Direct registry");
    let configuration: EffectiveProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"direct-child-topology"],
            )
            .expect("configuration digest"),
        )
        .expect("configuration encoding"),
    )
    .expect("configuration identity");
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "test.responses",
        "test.oauth",
        "test.product-composition.v1",
        "test-family",
        "test-model",
        "test-model",
        configuration,
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .expect("projection target");
    for kind in [
        CodingPresentation::Grok,
        CodingPresentation::Claude,
        CodingPresentation::Pi,
        CodingPresentation::Codex,
    ] {
        let extended = kind
            .projection_map_with_extensions(
                plakous::tools::JobConfig::default(),
                registry.clone(),
                target.clone(),
                &extras,
            )
            .expect("child Direct catalog");
        assert!(
            extended
                .entries()
                .iter()
                .any(|entry| entry.exposed_name().as_str() == "submit_result")
        );
        assert!(
            !extended
                .entries()
                .iter()
                .any(|entry| entry.exposed_name().as_str() == "spawn_agent")
        );
        raise_direct_extra(
            &extended,
            "submit_result",
            r#"{"turn_token":1,"output_json":"{\"word\":\"ok\"}"}"#,
            r#"{"agent_id":"c1","attempt":"completed"}"#,
            "5",
        );
    }
}

/// A model the projection tests construct a session with and never call. It
/// attests the fixture's runtime authority, as the create revalidates it.
struct UncalledModel {
    descriptor: plakous::model::ResolvedModelDescriptor,
    authority: plakous::model::RuntimeModelAuthority,
}

impl plakous_model::LanguageModel for UncalledModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: plakous::model::Timestamp,
    ) -> plakous_model::RuntimeModelAuthorityOperation {
        Box::pin(async move {
            self.authority.validate(&self.descriptor, checked_at)?;
            Ok(self.authority.clone())
        })
    }

    fn prepare_call(
        self: Arc<Self>,
        _plan: plakous_model::ValidatedRequestPlan,
        _context: plakous_model::ModelPreparationContext,
    ) -> Result<plakous_model::PreparedModelCall, plakous_model::ModelPreparationError> {
        unreachable!("projection admission tests never prepare a call")
    }
}

/// A Standard Claude OAuth model for `claude-opus-5-5`, configured the way the
/// facade configures a resolved one, without a catalog round trip.
fn standard_claude_model(
    selected: &plakous::model::SelectedProviderConfiguration,
) -> Result<plakous::model::ConfiguredLanguageModel, Box<dyn std::error::Error>> {
    use plakous::model::{
        Capability, CapabilityConstraints, CapabilitySet, EffectiveProviderConfiguration,
        FactProvenance, FactSource, FamilyPolicyEnvelope, FeatureSupport, ModelAdapterProfileId,
        ModelFamilyId, ModelImplementationIdentity, ProviderSemanticEvidence,
        ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, RuntimeModelAuthority, Support,
        Timestamp,
    };

    let route = selected.route();
    let family = ModelFamilyId::new("messages-family")?;
    let provenance = FactProvenance::new(FactSource::TestFixture, Timestamp::new(1, 0)?, None);
    let capabilities = [
        Capability::Tools,
        Capability::Reasoning,
        Capability::ParallelTools,
        Capability::Caching,
    ]
    .into_iter()
    .map(|capability| {
        (
            capability,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        )
    })
    .collect::<std::collections::BTreeMap<_, _>>();
    let authority = plakous_model::StandardIntegrationAuthority::release_owned();
    let descriptor = ResolvedModelDescriptor::new(
        &authority,
        route.protocol().clone(),
        route.route().clone(),
        route.requested_model().clone(),
        Some(route.requested_model().clone()),
        Some(family.clone()),
        ModelAdapterProfileId::new(route.integration_profile().as_str())?,
        CapabilitySet::new(capabilities, std::collections::BTreeMap::new()),
    );
    let runtime = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential"])?,
        1,
        Timestamp::new(1, 0)?,
        None,
    )?;
    let binding = ResolvedProviderAuthorityBinding::new(
        &descriptor,
        &runtime,
        ProviderSemanticEvidence::new(1, b"fixture".to_vec())?,
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        selected,
        descriptor.clone(),
        ModelImplementationIdentity::release_owned(&authority, &descriptor, 1)?,
        binding,
        selected.behavior().clone(),
        FamilyPolicyEnvelope::new(family, 1, 1, 1, b"fixture".to_vec())?,
    )?;
    let configuration =
        plakous::model::CompiledProviderConfiguration::new(selected.clone(), effective)?;
    let model = Arc::new(UncalledModel {
        descriptor,
        authority: runtime.clone(),
    });
    Ok(plakous::model::ConfiguredLanguageModel::new(
        &authority,
        model,
        configuration,
        runtime,
    )?)
}

/// A `read_only` or `execute` leaf child on a Direct OAuth route (a
/// reviewer template: claude-opus-5-5, `plakous-default`,
/// tools `read-execute`) carries topology extras, which the session compiles
/// into its provider configuration. `node_session` returns that configuration,
/// the model built with it and the tier-filtered surface together, the way the
/// child binding consumes them; the agent builder must admit the surface's
/// projection for that model, or construction refuses it as "not the exact
/// provider/model catalog candidate" and the spawn never creates a session.
#[tokio::test]
async fn fenced_direct_child_session_admits_its_projection_at_construction()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{SpawnAgentRequest, Tier, WriterMode};
    use crate::topology_extensions::topology_tool_extensions;

    let (_workspace_dir, workspace_root, _) = fixture_workspace();
    let workspace = Arc::new(
        stamped_workspace_policy(&std::fs::canonicalize(&workspace_root)?, Path::new(""))
            .expect("workspace policy"),
    );
    let composition = CompositionSelection::FollowCatalog;
    let selected = selected_coding_provider_configuration(
        NativeProvider::Claude,
        ModelId::new("claude-opus-5-5")?,
        ReasoningEffort::High,
        None,
        composition,
    )
    .expect("Claude selection");
    let resolved = standard_claude_model(&selected)?;
    let job_config = plakous::tools::JobConfig::default();
    let store = tempfile::tempdir()?;
    let sessions = plakous::sessions::AgentSessions::new(
        plakous::sessions::SessionBackend::sqlite(store.path().join("runtime.sqlite3"))?,
    )?;

    for tier in [Tier::ReadOnly, Tier::Execute] {
        let root = tempfile::tempdir()?;
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            "tree-fenced-direct-child",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .expect("tree");
        let spawned = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("review", tier, "reviewer", "review the diff"),
                None,
            )
            .expect("child");
        let extensions = topology_tool_extensions(Arc::new(Mutex::new(tree)), spawned.agent_id)
            .expect("child extensions");
        let extras = extensions.names().cloned().collect::<Vec<_>>();
        assert!(
            !extras.is_empty(),
            "a leaf child still carries topology extras"
        );
        let registry = coding_tool_registry_with_extensions(
            "fenced-direct-child",
            composition,
            false,
            &extensions,
        )
        .expect("child registry");
        let fence = occupancy_session_fence(tier);
        let node = node_session(
            job_config,
            resolved.clone(),
            &registry,
            &extensions,
            &extras,
            fence,
            None,
        )
        .expect("node session");
        assert_ne!(
            resolved.configuration().effective_identity(),
            node.configuration.effective_identity(),
            "{tier:?}: the extras extend the session configuration"
        );
        assert_eq!(
            node.configured_model.configuration().effective_identity(),
            node.configuration.effective_identity(),
            "{tier:?}: the session model is built with the session configuration"
        );
        let names = node
            .surface
            .projection()
            .entries()
            .iter()
            .map(|entry| entry.exposed_name().as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"submit_result"), "{tier:?}: {names:?}");
        assert!(names.contains(&"Bash"), "{tier:?}: {names:?}");
        assert!(
            !names.contains(&"Edit") && !names.contains(&"Write"),
            "{tier:?}: {names:?}"
        );

        // The plan the child binding compiles for a fresh session: it owns the
        // tool exposure the construction checks against the configuration.
        let profile = selected_harness_profile(
            NativeProvider::Claude,
            &ModelId::new("claude-opus-5-5")?,
            ReasoningEffort::High,
            false,
            false,
            &extras,
            true,
            composition,
            &node.surface,
            workspace.root(),
            workspace.cwd(),
        )
        .expect("child profile")
        .expect("a fresh child compiles a plan");
        let plan = plakous::agent::compile_explicit_runtime_profile(
            &profile,
            node.configured_model.model().as_ref(),
            &node.configuration,
            &registry,
            workspace.as_ref(),
            Vec::new(),
        )
        .map_err(|error| format!("{tier:?}: {error}"))?;
        let builder = plakous::agent::AgentBuilder::new(
            node.configured_model,
            registry,
            Arc::clone(&workspace),
        )
        .projection_admission(Arc::new(crate::coding_routes::ProductProjectionAdmission))
        .job_config(job_config)
        .tool_policy(session_tool_policy(fence))
        .tool_projection(node.surface.projection().clone())
        .harness_plan(plan)
        .request_policy(plakous::agent::AgentRequestPolicy::new(
            plakous::agent::GenerationLimits::new(
                node.configuration
                    .effective()
                    .behavior()
                    .output_bound
                    .bound(),
                Vec::new(),
            )
            .expect("generation limits"),
            plakous::agent::ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::High),
                requirement: CapabilityRequirement::Required,
            },
        ));
        let created = sessions
            .create(plakous::sessions::CreateSessionRequest::new(
                plakous::sessions::SessionOperationId::new(format!(
                    "fenced-direct-child-{tier:?}"
                ))?,
                plakous::sessions::SessionRuntimeBinding::new(builder),
            ))
            .await
            .map_err(|error| format!("{tier:?}: {error}"))?;
        let plakous::sessions::CreateSessionOutcome::Created { session, .. } = created else {
            panic!("{tier:?}: a fresh create creates its session");
        };
        session.release().await?;
    }
    Ok(())
}

/// The fence follows the tier. `read_only` and `execute` refuse Plak file
/// writes and allow process tools; `mutate` is unfenced. Identities stay
/// distinct so an `execute` session does not look like a `read_only` policy.
#[test]
fn child_session_fence_follows_the_tier() {
    use crate::agent_tree_protocol::Tier;
    assert_eq!(
        occupancy_session_fence(Tier::ReadOnly),
        SessionToolFence::OccupancyReadOnly
    );
    assert_eq!(
        occupancy_session_fence(Tier::Execute),
        SessionToolFence::OccupancyExecute
    );
    assert_eq!(
        occupancy_session_fence(Tier::Mutate),
        SessionToolFence::Full
    );
    for name in [
        "update_plan",
        "merge_plan",
        "submit_result",
        "send_agent_message",
        "spawn_agent",
        "interrupt_agent",
        "close_agent",
        "recover_agent",
        "adopt_candidate",
        "discard_workspace",
    ] {
        assert!(
            occupancy_read_only_allows(name, ToolEffect::Mutating),
            "{name}"
        );
    }
    assert!(occupancy_read_only_allows(
        "list_agents",
        ToolEffect::ReadOnly
    ));
    for name in PROCESS_SESSION_TOOLS.into_iter().chain([
        "exec_command",
        "run_terminal_command",
        "Bash",
        "write_stdin",
        "shell",
        "TaskStop",
        "get_command_or_subagent_output",
        "kill_command_or_subagent",
    ]) {
        assert!(
            occupancy_read_only_allows(name, ToolEffect::Mutating),
            "{name}"
        );
    }
    for name in [
        "apply_patch",
        "merge_workspace",
        "TaskOutput",
        "process_session_escape",
    ] {
        assert!(
            !occupancy_read_only_allows(name, ToolEffect::Mutating),
            "{name}"
        );
    }
    assert_ne!(
        session_tool_policy(SessionToolFence::OccupancyExecute).revision(),
        session_tool_policy(SessionToolFence::OccupancyReadOnly).revision()
    );
}

#[test]
fn occupancy_plan_allowlist_uses_canonical_names_not_presentation_aliases() {
    assert!(occupancy_read_only_allows(
        "update_plan",
        ToolEffect::Mutating
    ));
    assert!(occupancy_read_only_allows(
        "merge_plan",
        ToolEffect::Mutating
    ));
    assert!(!occupancy_read_only_allows(
        "todo_write",
        ToolEffect::Mutating
    ));
}

#[test]
fn stamp_root_account_accepts_openrouter_literal() {
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        "tree-openrouter",
        "tree-openrouter",
        "root",
        PathBuf::from("/tmp/openrouter-root"),
        "vendor/model",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .expect("bootstrap");
    let root_id = tree.root_agent_id.clone();
    assert!(stamp_root_account(&mut tree, "openrouter"));
    assert_eq!(tree.node(&root_id).expect("root").account, "openrouter");
    assert!(!stamp_root_account(&mut tree, "openrouter"));
}

/// A child runs on its own card's processing tier: a Fast card binds Fast and
/// a Standard card binds Standard under the same caller, even when both cards
/// name the caller's own account and model.
#[tokio::test]
async fn child_processing_tier_is_its_own_cards() {
    use crate::agent_tree_protocol::{SpawnAgentRequest, Tier, WriterMode};
    let root = tempfile::tempdir().expect("tempdir");
    let tree_id = format!("tier-{}", uuid::Uuid::new_v4().simple());
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        &tree_id,
        "root",
        "owner",
        root.path().to_path_buf(),
        "gpt-5.6-luna",
        WriterMode::SingleLease,
    )
    .expect("tree");
    tree.set_node_account_for_test("root", "codex");
    tree.pin_bind_hints_for_test(crate::orchestration::BindHints::from_names(&["codex"], &[]));
    let mut fast = crate::orchestration::RosterBinding::leaf("codex", "gpt-5.6-luna");
    fast.max_tier = Tier::ReadOnly;
    fast.processing_tier = ProcessingTierDisposition::ProviderNamed(
        plakous::model::ProviderServiceTierId::new("priority").expect("tier id"),
    );
    tree.pin_template("review", fast.clone());
    let mut standard = crate::orchestration::RosterBinding::leaf("codex", "gpt-5.6-luna");
    standard.max_tier = Tier::ReadOnly;
    tree.pin_template("explore", standard);
    let fast_child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .expect("fast spawn")
        .agent_id;
    let standard_child = tree
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("explore", Tier::ReadOnly, "scout", "look"),
            None,
        )
        .expect("standard spawn")
        .agent_id;
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None).expect("hub");
    assert_eq!(
        spawn_roster_processing_tier(&tree_id, &fast_child),
        fast.processing_tier
    );
    assert_eq!(
        spawn_roster_processing_tier(&tree_id, &standard_child),
        ProcessingTierDisposition::Standard
    );
    crate::topology_runtime::force_release(&tree_id).await;
}

/// A child of an OpenRouter parent binds only what its own row names: a row
/// without an account or a model is refused, never filled from the parent.
#[tokio::test]
async fn openrouter_child_never_takes_its_parents_account_or_model() {
    let parent = SpawnParent::Api(openrouter_test_parent());
    let Err(missing_account) = openrouter_child_runtime_binding(
        None,
        &parent,
        "c1",
        "/tmp/or-root",
        None,
        None,
        None,
        false,
        SessionToolFence::Full,
    )
    .await
    else {
        panic!("a row without an account bound");
    };
    assert_eq!(missing_account, "API child account is missing");
    let Err(missing_model) = openrouter_child_runtime_binding(
        None,
        &parent,
        "c1",
        "/tmp/or-root",
        Some("openrouter"),
        None,
        None,
        false,
        SessionToolFence::Full,
    )
    .await
    else {
        panic!("a row without a model bound");
    };
    assert_eq!(missing_model, "API child model is missing");
}

#[test]
fn openrouter_frozen_pin_defaults_and_rejects_native_ptc() {
    use crate::binding::ProtocolSelection;
    assert_eq!(
        openrouter_frozen_pin(None, None).unwrap(),
        (CompositionSelection::FollowCatalog, ProtocolSelection::Auto)
    );
    assert_eq!(
        openrouter_frozen_pin(
            Some(CompositionSelection::JsPtc),
            Some(ProtocolSelection::Messages)
        )
        .unwrap(),
        (CompositionSelection::JsPtc, ProtocolSelection::Messages)
    );
    assert!(
        openrouter_frozen_pin(Some(CompositionSelection::NativePtc), None)
            .unwrap_err()
            .contains("OpenRouter has no native programmatic tool calling")
    );
}

#[test]
fn child_session_openrouter_account_never_uses_oauth_provider() {
    assert!(crate::runtime::oauth_provider_for_account("openrouter").is_none());
    assert!(crate::runtime::oauth_provider_for_account("openai").is_none());
    assert!(crate::runtime::oauth_provider_for_account("anthropic").is_none());
    assert!(crate::runtime::oauth_provider_for_account("codex").is_some());
}

#[test]
fn frozen_openrouter_child_pin_fails_closed_without_hub() {
    let error = frozen_openrouter_child_pin("missing-tree", "c1").unwrap_err();
    assert!(error.contains("topology hub is not attached"), "{error}");
    assert!(!error.contains("native OAuth"));
}

#[test]
fn minted_child_session_root_cannot_read_parent_seed() {
    let launch = tempfile::tempdir().expect("launch");
    std::fs::write(launch.path().join("SEED.txt"), "seed\n").expect("seed");
    let minted = launch.path().join(".plak/worktrees/c1");
    std::fs::create_dir_all(&minted).expect("minted");
    std::fs::write(minted.join("CHILD.txt"), "child\n").expect("child");
    assert_eq!(
        child_session_workspace_root(launch.path(), launch.path()),
        launch.path()
    );
    let fenced = child_session_workspace_root(launch.path(), &minted);
    assert_eq!(fenced, minted);
    let policy = WorkspacePolicy::host(&fenced, Path::new(".")).expect("policy");
    assert!(policy.resolve_existing("CHILD.txt").is_ok());
    assert!(policy.resolve_existing("../../SEED.txt").is_err());
}

#[test]
fn child_plan_state_authority_is_deterministic_and_bounded() {
    let authority =
        child_plan_state_authority("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "c1").expect("bounded");
    assert_eq!(authority, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.c1");
    assert!(format!("plak.session.{authority}").len() <= 72);
    let operation = spawn_child_operation_id("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "c1")
        .expect("tree-scoped spawn operation");
    assert_eq!(
        operation.as_str(),
        "plak:spawn:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee:c1"
    );
    let prompt = assignment_start_prompt(
        ChildAssignmentStartKind::Direct,
        "review",
        crate::agent_tree_protocol::Tier::Mutate,
        "",
        "review the diff",
        r#"{"type":"object"}"#,
        1,
        Some("hello"),
        Some("/tmp/child-wt"),
    );
    let brief = assignment_start_prompt(
        ChildAssignmentStartKind::Direct,
        "review",
        crate::agent_tree_protocol::Tier::ReadOnly,
        "",
        "review the diff",
        "{}",
        1,
        None,
        None,
    );
    assert!(brief.contains("Template: review"));
    assert!(brief.contains("Tier: read_only"));
    assert!(prompt.contains("Tier: mutate"));
    assert!(brief.contains("Description: review"));
    assert!(!brief.contains("Description: \n"));
    assert!(brief.contains("Prompt: review the diff"));
    assert!(!brief.contains("Message:"));
    assert!(prompt.contains("Description: review"));
    assert!(prompt.contains("Workspace: /tmp/child-wt"));
    assert!(prompt.contains("Turn token: 1"));
    assert!(prompt.contains("Message:\nhello"));
    assert!(
        prompt.contains("Call submit_result with this turn_token and schema-valid output_json")
    );
    assert!(prompt.contains("retry only if the tool result says the draft was rejected"));
    assert!(prompt.contains("Do not answer the assignment in assistant text"));
    assert!(prompt.contains("completed_without_submission"));
    assert!(prompt.contains("do not treat the last assistant text as a result"));
    let reminder = submit_harness_prompt(1, 1, r#"{"type":"object"}"#);
    assert!(reminder.contains("turn_token=1"));
    assert!(reminder.contains("Call submit_result exactly once"));
    assert!(!reminder.contains("last assistant text as output_json"));
    let force = submit_harness_prompt(2, 1, r#"{"type":"object"}"#);
    assert!(force.contains("Required: call submit_result now"));
    assert!(force.contains("completed_without_submission"));
    assert!(!force.contains("Message:"));
}

#[test]
fn ownership_reopen_copy_names_lease_retry_not_recovery() {
    assert!(SESSION_LEASE_HOLD_HINT.contains("30s"));
    assert!(SESSION_LEASE_HOLD_HINT.contains("not RecoveryRequired"));
}

#[test]
fn coding_selections_are_exact_durable_and_canonically_ordered() {
    for (provider, model, effort) in [
        (
            NativeProvider::Codex,
            "gpt-5.6-sol",
            ReasoningEffort::Medium,
        ),
        (
            NativeProvider::Codex,
            "gpt-5.6-terra",
            ReasoningEffort::High,
        ),
        (
            NativeProvider::Codex,
            "gpt-5.6-terra",
            ReasoningEffort::Medium,
        ),
        (NativeProvider::Xai, "grok-4.6", ReasoningEffort::XHigh),
        (NativeProvider::Xai, "grok-4.5", ReasoningEffort::Low),
        (
            NativeProvider::Claude,
            "claude-sonnet-5",
            ReasoningEffort::High,
        ),
        (
            NativeProvider::Claude,
            "claude-sonnet-5",
            ReasoningEffort::Low,
        ),
        (NativeProvider::Codex, "gpt-5.6-terra", ReasoningEffort::Low),
        (NativeProvider::Codex, "gpt-5.6-luna", ReasoningEffort::Low),
        (
            NativeProvider::Codex,
            "gpt-5.6-luna",
            ReasoningEffort::Medium,
        ),
        (NativeProvider::Xai, "grok-4.6", ReasoningEffort::Low),
        (NativeProvider::Xai, "grok-4.6", ReasoningEffort::Medium),
        (NativeProvider::Xai, "grok-4.5", ReasoningEffort::Medium),
        (
            NativeProvider::Claude,
            "claude-sonnet-5",
            ReasoningEffort::Medium,
        ),
        (
            NativeProvider::Claude,
            "claude-opus-5",
            ReasoningEffort::Low,
        ),
        (
            NativeProvider::Claude,
            "claude-opus-5",
            ReasoningEffort::Medium,
        ),
    ] {
        let selected = selected_coding_provider_configuration(
            provider,
            ModelId::new(model).expect("model"),
            effort,
            if provider == NativeProvider::Codex {
                Some(
                    if NativeProvider::Codex.enrolled_tool_mode(model)
                        == Some(NativeToolMode::CodeModeOnly)
                    {
                        NativeToolMode::CodeModeOnly
                    } else {
                        NativeToolMode::Direct
                    },
                )
            } else {
                None
            },
            CompositionSelection::FollowCatalog,
        )
        .expect("selected coding configuration");
        let behavior = selected.behavior();
        assert_eq!(
            behavior.local_durability,
            LocalDurabilityDisposition::DurableRequired
        );
        assert_eq!(
            behavior.cache,
            match provider {
                NativeProvider::Claude => CachePolicy::Enabled {
                    requirement: CapabilityRequirement::Required,
                },
                NativeProvider::Codex | NativeProvider::Xai => CachePolicy::Disabled,
                _ => CachePolicy::Disabled,
            }
        );
        assert!(
            behavior
                .tools
                .windows(2)
                .all(|pair| { pair[0].definition().name() < pair[1].definition().name() })
        );
        assert!(behavior.tools.iter().any(|tool| matches!(
            tool.definition().concurrency(),
            ToolConcurrency::Serial | ToolConcurrency::ResourceExclusive(_)
        )));
        let names = behavior
            .tools
            .iter()
            .map(|tool| tool.definition().name().as_str())
            .collect::<Vec<_>>();
        if is_codex_code_mode_candidate(provider, &ModelId::new(model).expect("exact tested model"))
        {
            assert_eq!(names, ["exec", "wait"]);
            assert_eq!(
                behavior.tool_mode,
                ToolPresentationMode::LocalCompositionOnly
            );
            continue;
        }
        assert_eq!(behavior.tool_mode, ToolPresentationMode::Direct);
        assert!(names.contains(&"process_session_start"));
        // Claude and Grok expose explicit process stop tools.
        assert_eq!(
            names.contains(&"process_session_terminate"),
            matches!(provider, NativeProvider::Claude | NativeProvider::Xai)
        );
        assert!(!names.contains(&"inspect_image"));
        assert!(!names.contains(&"run_process"));
        if provider == NativeProvider::Xai {
            assert!(names.contains(&"process_session_poll"));
            assert!(names.contains(&"read_file"));
            assert!(names.contains(&"merge_plan"));
            assert!(!names.contains(&"update_plan"));
            assert!(
                behavior
                    .tools
                    .iter()
                    .any(|tool| tool.definition().concurrency() == ToolConcurrency::Parallel)
            );
        } else if provider == NativeProvider::Claude {
            assert!(names.contains(&"process_session_poll"));
            assert!(names.contains(&"read_file"));
            assert!(names.contains(&"search_repository"));
            assert!(names.contains(&"apply_patch"));
            assert!(names.contains(&"update_plan"));
        } else {
            assert!(
                names.contains(&"process_session_poll"),
                "Codex exec_command covers poll so the prefix must bind process_session_poll"
            );
            assert!(!names.contains(&"read_file"));
            assert!(!names.contains(&"search_repository"));
        }
    }
}

#[test]
fn non_v1_codex_selection_retains_the_direct_canonical_surface() {
    let selected = selected_coding_provider_configuration(
        NativeProvider::Codex,
        ModelId::new("gpt-5.6-luna").expect("model"),
        ReasoningEffort::High,
        Some(NativeToolMode::Direct),
        CompositionSelection::FollowCatalog,
    )
    .expect("direct Codex selection");
    let names = selected
        .behavior()
        .tools
        .iter()
        .map(|tool| tool.definition().name().as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        [
            "patch_envelope",
            "process_session_poll",
            "process_session_start",
            "process_session_write",
            "update_plan",
        ]
    );
    assert!(!names.contains(&"exec"));
    assert!(!names.contains(&"wait"));
    assert_eq!(selected.behavior().tool_mode, ToolPresentationMode::Direct);
}

#[test]
fn code_mode_selection_admits_exact_catalog_efforts_but_not_ultra_or_neighbors() {
    for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ] {
            let qualified = selected_coding_provider_configuration(
                NativeProvider::Codex,
                ModelId::new(model).expect("model"),
                effort,
                Some(NativeToolMode::CodeModeOnly),
                CompositionSelection::FollowCatalog,
            )
            .expect("exact enrolled Code Mode tuple");
            assert_eq!(
                qualified.behavior().tool_mode,
                ToolPresentationMode::LocalCompositionOnly
            );
        }
        let ultra = selected_coding_provider_configuration(
            NativeProvider::Codex,
            ModelId::new(model).expect("model"),
            ReasoningEffort::Ultra,
            Some(NativeToolMode::CodeModeOnly),
            CompositionSelection::FollowCatalog,
        )
        .expect_err("Codex Ultra is orchestration, not a single-agent effort");
        assert!(ultra.message().contains("orchestration"));
    }

    let neighboring = selected_coding_provider_configuration(
        NativeProvider::Codex,
        ModelId::new("gpt-5.3-codex-spark").expect("model"),
        ReasoningEffort::Medium,
        Some(NativeToolMode::CodeModeOnly),
        CompositionSelection::FollowCatalog,
    )
    .expect_err("an unenrolled neighboring model must fail before dispatch");
    assert!(
        neighboring
            .message()
            .contains("no exact built-in process-presentation candidate")
    );
}

#[test]
fn product_javascript_selects_outer_markers_on_every_oauth_direct_vertical() {
    for (provider, model, effort, mode) in [
        (
            NativeProvider::Codex,
            "gpt-5.6-luna",
            ReasoningEffort::High,
            Some(NativeToolMode::Direct),
        ),
        (NativeProvider::Xai, "grok-4.5", ReasoningEffort::Low, None),
        (
            NativeProvider::Claude,
            "claude-sonnet-5",
            ReasoningEffort::Low,
            None,
        ),
    ] {
        let selected = selected_coding_provider_configuration(
            provider,
            ModelId::new(model).expect("model"),
            effort,
            mode,
            CompositionSelection::JsPtc,
        )
        .expect("direct-capable product JavaScript selection");
        let names = selected
            .behavior()
            .tools
            .iter()
            .map(|tool| tool.definition().name().as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["exec_js", "wait_js"]);
        assert_eq!(selected.behavior().tool_mode, ToolPresentationMode::Direct);
    }

    for (provider, model, mode) in [
        (NativeProvider::Codex, "gpt-5.6-luna", None),
        (
            NativeProvider::Xai,
            "grok-4.5",
            Some(NativeToolMode::Direct),
        ),
    ] {
        assert!(
            selected_coding_provider_configuration(
                provider,
                ModelId::new(model).expect("model"),
                ReasoningEffort::Medium,
                mode,
                CompositionSelection::JsPtc,
            )
            .is_err(),
            "non-Direct-capable tuple {provider:?}/{model}/{mode:?}"
        );
    }

    // a CodeModeOnly row compiles the explicit Direct presentation
    // and js-ptc replaces it; no `exec`/`wait` is emitted beside `exec_js`.
    let on_code_mode_only_row = selected_coding_provider_configuration(
        NativeProvider::Codex,
        ModelId::new("gpt-5.6-sol").expect("model"),
        ReasoningEffort::Medium,
        Some(NativeToolMode::CodeModeOnly),
        CompositionSelection::JsPtc,
    )
    .expect("js-ptc replaces explicit Direct on a CodeModeOnly row");
    let names = on_code_mode_only_row
        .behavior()
        .tools
        .iter()
        .map(|tool| tool.definition().name().as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["exec_js", "wait_js"]);
    assert_eq!(
        on_code_mode_only_row.behavior().tool_mode,
        ToolPresentationMode::Direct
    );
}

#[test]
fn product_python_selects_outer_markers_on_every_oauth_direct_vertical() {
    for (provider, model, effort, mode) in [
        (
            NativeProvider::Codex,
            "gpt-5.6-luna",
            ReasoningEffort::High,
            Some(NativeToolMode::Direct),
        ),
        (NativeProvider::Xai, "grok-4.6", ReasoningEffort::Low, None),
        (
            NativeProvider::Claude,
            "claude-sonnet-5",
            ReasoningEffort::Low,
            None,
        ),
    ] {
        let selected = selected_coding_provider_configuration(
            provider,
            ModelId::new(model).expect("model"),
            effort,
            mode,
            CompositionSelection::PyPtc,
        )
        .expect("wire-compatible direct Python selection");
        let names = selected
            .behavior()
            .tools
            .iter()
            .map(|tool| tool.definition().name().as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["exec_py", "wait_py"]);
        assert_eq!(selected.behavior().tool_mode, ToolPresentationMode::Direct);
    }

    let on_code_mode_only_row = selected_coding_provider_configuration(
        NativeProvider::Codex,
        ModelId::new("gpt-5.6-sol").expect("model"),
        ReasoningEffort::Medium,
        Some(NativeToolMode::CodeModeOnly),
        CompositionSelection::PyPtc,
    )
    .expect("py-ptc replaces explicit Direct on a CodeModeOnly row");
    let names = on_code_mode_only_row
        .behavior()
        .tools
        .iter()
        .map(|tool| tool.definition().name().as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["exec_py", "wait_py"]);
    assert_eq!(
        on_code_mode_only_row.behavior().tool_mode,
        ToolPresentationMode::Direct
    );
}

#[test]
fn plak_ptc_hub_is_inherited_by_unpinned_children_on_every_vertical() {
    for parent in [CompositionSelection::JsPtc, CompositionSelection::PyPtc] {
        assert_eq!(
            spawn_child_composition(parent, NativeProvider::Codex, None)
                .expect("codex child inherits Plak PTC over explicit Direct"),
            parent
        );
        for provider in [NativeProvider::Claude, NativeProvider::Xai] {
            assert_eq!(
                spawn_child_composition(parent, provider, None)
                    .expect("non-codex child inherits Plak PTC"),
                parent
            );
        }
        assert_eq!(
            spawn_child_composition(
                parent,
                NativeProvider::Codex,
                Some(CompositionSelection::Direct),
            )
            .expect("roster pin stays authoritative"),
            CompositionSelection::Direct
        );
    }
}

#[test]
fn native_ptc_hub_honors_direct_roster_pin_on_non_codex_child() {
    assert_eq!(
        spawn_child_composition(
            CompositionSelection::NativePtc,
            NativeProvider::Xai,
            Some(CompositionSelection::Direct),
        )
        .expect("Direct roster pin"),
        CompositionSelection::Direct
    );
    assert_eq!(
        spawn_child_composition(CompositionSelection::NativePtc, NativeProvider::Xai, None,)
            .expect("an unpinned grok child resolves its own presentation"),
        CompositionSelection::FollowCatalog
    );
    assert_eq!(
        spawn_child_composition(CompositionSelection::NativePtc, NativeProvider::Codex, None,)
            .expect("codex child inherits native-ptc"),
        CompositionSelection::NativePtc
    );
    assert_eq!(
        spawn_child_composition(
            CompositionSelection::NativePtc,
            NativeProvider::Codex,
            Some(CompositionSelection::Direct),
        )
        .expect("same-account Direct pin"),
        CompositionSelection::Direct
    );
}

/// An *explicit* `composition = "native-ptc"` on a non-Codex template is
/// still fail-closed, and the refusal names a remedy.
#[test]
fn explicit_native_ptc_class_pin_off_codex_is_refused_with_a_remedy() {
    let refusal = spawn_child_composition(
        CompositionSelection::NativePtc,
        NativeProvider::Claude,
        Some(CompositionSelection::NativePtc),
    )
    .expect_err("an explicit native-ptc pin on a Claude class is illegal");
    assert!(
        refusal.contains("Claude has no native programmatic tool calling (native-ptc) enrolled"),
        "{refusal}"
    );
    assert!(
        refusal.contains("use js-ptc, py-ptc or direct"),
        "{refusal}"
    );
    assert!(refusal.contains("[agent] template"));
}

#[test]
fn explicit_native_ptc_fails_closed_off_codex_code_mode() {
    for (provider, model, effort, mode, vertical) in [
        (
            NativeProvider::Xai,
            "grok-4.6",
            ReasoningEffort::Low,
            None,
            "Grok has no native programmatic tool calling",
        ),
        (
            NativeProvider::Claude,
            "claude-sonnet-5",
            ReasoningEffort::Low,
            None,
            "Claude has no native programmatic tool calling",
        ),
        (
            NativeProvider::Codex,
            "gpt-5.6-luna",
            ReasoningEffort::High,
            Some(NativeToolMode::Direct),
            "this Codex model has no native programmatic tool calling",
        ),
    ] {
        let err = selected_coding_provider_configuration(
            provider,
            ModelId::new(model).expect("model"),
            effort,
            mode,
            CompositionSelection::NativePtc,
        )
        .expect_err("native-ptc is Codex Code Mode only");
        assert!(
            err.message().contains(vertical)
                && err.message().contains("use js-ptc, py-ptc or direct"),
            "{provider:?}/{model}: {}",
            err.message()
        );
    }
}

#[test]
fn follow_catalog_hello_label_resolves_by_codex_tool_mode() {
    assert_eq!(
        effective_presentation_label(
            CompositionSelection::FollowCatalog,
            Some(NativeProvider::Codex),
            Some(NativeToolMode::CodeModeOnly),
        ),
        "native-ptc"
    );
    assert_eq!(
        effective_presentation_label(
            CompositionSelection::FollowCatalog,
            Some(NativeProvider::Codex),
            Some(NativeToolMode::Direct),
        ),
        "direct"
    );
    assert_eq!(
        effective_presentation_label(CompositionSelection::FollowCatalog, None, None,),
        "direct"
    );
}

#[test]
fn codex_enrolled_direct_and_native_compositions_compile_distinct_presentations() {
    for model_id in [
        "gpt-6.1-sol",
        "gpt-6-sol",
        "gpt-6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ] {
        let model = ModelId::new(model_id).expect("model");
        let mode = Some(NativeToolMode::CodeModeOnly);
        let native = selected_coding_provider_configuration(
            NativeProvider::Codex,
            model.clone(),
            ReasoningEffort::Medium,
            mode,
            CompositionSelection::FollowCatalog,
        )
        .expect("native Code Mode selection");
        let direct = selected_coding_provider_configuration(
            NativeProvider::Codex,
            model,
            ReasoningEffort::Medium,
            mode,
            CompositionSelection::Direct,
        )
        .expect("explicit Direct selection on CodeModeOnly catalog");
        let native_names = native
            .behavior()
            .tools
            .iter()
            .map(|tool| tool.definition().name().as_str())
            .collect::<Vec<_>>();
        let direct_names = direct
            .behavior()
            .tools
            .iter()
            .map(|tool| tool.definition().name().as_str())
            .collect::<Vec<_>>();
        assert_eq!(native_names, ["exec", "wait"], "{model_id}");
        assert_eq!(
            native.behavior().tool_mode,
            ToolPresentationMode::LocalCompositionOnly,
            "{model_id}"
        );
        assert_eq!(
            direct.behavior().tool_mode,
            ToolPresentationMode::Direct,
            "{model_id}"
        );
        assert!(
            direct_names.contains(&"process_session_start"),
            "{model_id}"
        );
        assert!(direct_names.contains(&"process_session_poll"), "{model_id}");
        assert!(direct_names.contains(&"patch_envelope"), "{model_id}");
        assert!(!direct_names.contains(&"apply_patch"), "{model_id}");
        assert!(direct_names.contains(&"update_plan"), "{model_id}");
        assert!(!direct_names.contains(&"exec"), "{model_id}");
        assert!(!direct_names.contains(&"wait"), "{model_id}");
        assert_ne!(native.identity(), direct.identity(), "{model_id}");
    }
}

#[test]
fn newly_enrolled_oauth_models_compile_every_valid_product_presentation() {
    for (provider, model, mode) in [
        (
            NativeProvider::Codex,
            "gpt-6.1-sol",
            Some(NativeToolMode::CodeModeOnly),
        ),
        (
            NativeProvider::Codex,
            "gpt-6-sol",
            Some(NativeToolMode::CodeModeOnly),
        ),
        (
            NativeProvider::Codex,
            "gpt-6-luna",
            Some(NativeToolMode::CodeModeOnly),
        ),
        (NativeProvider::Claude, "claude-opus-5-5", None),
    ] {
        let compositions: &[CompositionSelection] = if provider == NativeProvider::Codex {
            &[
                CompositionSelection::FollowCatalog,
                CompositionSelection::NativePtc,
                CompositionSelection::Direct,
                CompositionSelection::JsPtc,
                CompositionSelection::PyPtc,
            ]
        } else {
            &[
                CompositionSelection::FollowCatalog,
                CompositionSelection::Direct,
                CompositionSelection::JsPtc,
                CompositionSelection::PyPtc,
            ]
        };
        for &composition in compositions {
            let selected = selected_coding_provider_configuration(
                provider,
                ModelId::new(model).expect("model"),
                ReasoningEffort::Low,
                mode,
                composition,
            )
            .expect("exact model and composition must compile through hosted selection");
            let names = selected
                .behavior()
                .tools
                .iter()
                .map(|tool| tool.definition().name().as_str())
                .collect::<Vec<_>>();
            if provider == NativeProvider::Codex {
                assert_eq!(
                    selected.behavior().parallel_tools,
                    ParallelToolDisposition::Disabled,
                    "Responses Lite cannot advertise parallel tools: {model}/{composition:?}"
                );
            }
            match composition {
                CompositionSelection::NativePtc | CompositionSelection::FollowCatalog
                    if provider == NativeProvider::Codex =>
                {
                    assert_eq!(names, ["exec", "wait"]);
                }
                CompositionSelection::JsPtc => assert_eq!(names, ["exec_js", "wait_js"]),
                CompositionSelection::PyPtc => assert_eq!(names, ["exec_py", "wait_py"]),
                CompositionSelection::FollowCatalog
                | CompositionSelection::Direct
                | CompositionSelection::NativePtc => {
                    assert!(
                        names.contains(&"process_session_start"),
                        "{provider:?}/{model}"
                    );
                }
            }
        }
    }
}

#[test]
fn codex_enrollment_matches_the_pinned_reference_catalog() -> Result<(), Box<dyn std::error::Error>>
{
    let reference: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/codex-reference-catalog.json"))?;
    let enrolled = NativeProvider::Codex.enrolled_models();
    let rows = reference["models"].as_object().ok_or("reference models")?;
    assert_eq!(rows.len(), enrolled.len());
    for model in enrolled {
        let row = &rows[model];
        assert_eq!(row["tool_mode"], "code_mode_only", "{model}");
        assert_eq!(
            NativeProvider::Codex.enrolled_tool_mode(model),
            Some(NativeToolMode::CodeModeOnly),
            "{model}"
        );
        assert_eq!(row["use_responses_lite"], true, "{model}");
        assert_eq!(row["service_tiers"][0]["id"], "priority", "{model}");
    }
    Ok(())
}

#[test]
fn new_oauth_product_selections_pass_effective_capability_compile()
-> Result<(), Box<dyn std::error::Error>> {
    use plakous::model::{
        Capability, CapabilityConstraints, CapabilitySet, EffectiveProviderConfiguration,
        FactProvenance, FactSource, FamilyPolicyEnvelope, FeatureSupport, ModelAdapterProfileId,
        ModelFamilyId, ModelImplementationIdentity, NamespaceId, ProviderSemanticEvidence,
        ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, RuntimeModelAuthority, Support,
        Timestamp,
    };
    use std::collections::BTreeMap;

    let codex_reference: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/codex-reference-catalog.json"))?;
    let codex_observed: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/codex-authenticated-catalog.json"))?;

    for (provider, model, mode) in [
        (
            NativeProvider::Codex,
            "gpt-6.1-sol",
            Some(NativeToolMode::CodeModeOnly),
        ),
        (
            NativeProvider::Codex,
            "gpt-6-sol",
            Some(NativeToolMode::CodeModeOnly),
        ),
        (
            NativeProvider::Codex,
            "gpt-6-luna",
            Some(NativeToolMode::CodeModeOnly),
        ),
        (NativeProvider::Claude, "claude-opus-5-5", None),
    ] {
        let compositions: &[CompositionSelection] = if provider == NativeProvider::Codex {
            &[
                CompositionSelection::FollowCatalog,
                CompositionSelection::NativePtc,
                CompositionSelection::Direct,
                CompositionSelection::JsPtc,
                CompositionSelection::PyPtc,
            ]
        } else {
            &[
                CompositionSelection::FollowCatalog,
                CompositionSelection::Direct,
                CompositionSelection::JsPtc,
                CompositionSelection::PyPtc,
            ]
        };
        for &composition in compositions {
            if provider == NativeProvider::Codex {
                assert_eq!(
                    codex_reference["models"][model]["use_responses_lite"],
                    serde_json::json!(true),
                    "effective capability fixture must match the 0.162.1 reference catalog"
                );
                let observed = &codex_observed["models"][model]["use_responses_lite"];
                assert!(
                    observed.is_null() || *observed == serde_json::json!(true),
                    "authenticated catalog observation must not contradict the reference: {model}"
                );
            }
            let selected = selected_coding_provider_configuration(
                provider,
                ModelId::new(model)?,
                ReasoningEffort::Low,
                mode,
                composition,
            )
            .expect("hosted exact tuple selection");
            let route = selected.route();
            let family = ModelFamilyId::new(if provider == NativeProvider::Codex {
                "responses-family"
            } else {
                "messages-family"
            })?;
            let provenance = FactProvenance::new(
                FactSource::TestFixture,
                Timestamp::new(1, 0)?,
                Some(Timestamp::new(2, 0)?),
            );
            let mut capabilities = [Capability::Tools, Capability::Reasoning]
                .into_iter()
                .map(|capability| {
                    (
                        capability,
                        FeatureSupport::new(
                            Support::Native,
                            CapabilityConstraints::default(),
                            provenance,
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            if provider == NativeProvider::Claude {
                for capability in [Capability::ParallelTools, Capability::Caching] {
                    capabilities.insert(
                        capability,
                        FeatureSupport::new(
                            Support::Native,
                            CapabilityConstraints::default(),
                            provenance,
                        ),
                    );
                }
            }
            let descriptor = ResolvedModelDescriptor::new_custom(
                route.protocol().clone(),
                route.route().clone(),
                route.requested_model().clone(),
                Some(route.requested_model().clone()),
                Some(family.clone()),
                ModelAdapterProfileId::new(route.integration_profile().as_str())?,
                CapabilitySet::new(capabilities, BTreeMap::new()),
            );
            let implementation = ModelImplementationIdentity::custom(
                NamespaceId::new("test.exact-oauth-product-compile")?,
                1,
                NamespaceId::new("test.fixture")?,
            )?;
            let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?;
            let credential = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential"])?;
            let runtime = RuntimeModelAuthority::new(
                &descriptor,
                account,
                credential,
                1,
                Timestamp::new(1, 0)?,
                Some(Timestamp::new(2, 0)?),
            )?;
            let authority = ResolvedProviderAuthorityBinding::new(
                &descriptor,
                &runtime,
                ProviderSemanticEvidence::new(1, b"fixture".to_vec())?,
            )?;
            let family_policy = FamilyPolicyEnvelope::new(family, 1, 1, 1, b"fixture".to_vec())?;
            if provider == NativeProvider::Codex {
                let mut old_behavior = selected.behavior().clone();
                old_behavior.parallel_tools = ParallelToolDisposition::Enabled;
                let old_selected = plakous::model::SelectedProviderConfiguration::new(
                    route.clone(),
                    old_behavior.clone(),
                )?;
                assert!(matches!(
                    EffectiveProviderConfiguration::compile(
                        &old_selected,
                        descriptor.clone(),
                        implementation.clone(),
                        authority.clone(),
                        old_behavior,
                        family_policy.clone(),
                    ),
                    Err(plakous::model::ProviderConfigurationError::UnsupportedBehavior)
                ));
            }
            EffectiveProviderConfiguration::compile(
                &selected,
                descriptor,
                implementation,
                authority,
                selected.behavior().clone(),
                family_policy,
            )?;
        }
    }
    Ok(())
}

#[test]
fn product_composition_catalog_starts_from_core_and_adds_extensions_explicitly() {
    let extensions = ProductToolExtensions::empty();
    let names = |provider| {
        nested_catalog(provider, &extensions)
            .iter()
            .map(|name| name.as_str().to_owned())
            .collect::<Vec<_>>()
    };

    let codex = names(NativeProvider::Codex);
    assert_eq!(
        codex,
        ["apply_patch", "exec_command", "update_plan", "write_stdin"]
    );

    let grok = names(NativeProvider::Xai);
    assert_eq!(
        grok,
        [
            "get_command_or_subagent_output",
            "grep",
            "kill_command_or_subagent",
            "list_dir",
            "read_file",
            "run_terminal_command",
            "search_replace",
            "todo_write",
            "write",
        ]
    );
    assert!(!grok.iter().any(|name| name == "search_repository"));

    let claude = names(NativeProvider::Claude);
    assert_eq!(claude, ["Bash", "Edit", "Read", "TaskStop", "Write"]);
    assert!(!claude.iter().any(|name| name == "search_repository"));
}

#[test]
fn both_local_runtimes_compile_every_oauth_core_catalog_through_one_contract() {
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        let extensions = ProductToolExtensions::empty();
        let catalog = nested_catalog(provider, &extensions);
        let configuration: EffectiveProviderConfigurationId = serde_json::from_value(
            serde_json::to_value(
                Digest::sha256(
                    DigestDomain::EffectiveProviderConfiguration,
                    5,
                    &[format!("{provider:?}").as_bytes()],
                )
                .expect("configuration digest"),
            )
            .expect("configuration encoding"),
        )
        .expect("configuration identity");
        let target = ProviderModelProjectionIdentityV2::new(
            IntegrationClass::Standard,
            "test.responses",
            "test.oauth",
            "test.product-composition.v1",
            "test-family",
            "test-model",
            "test-model",
            configuration,
            PROCESS_TOOL_PROJECTION_REVISION,
        )
        .expect("projection target");

        let javascript_registry =
            coding_tool_registry("catalog-javascript", CompositionSelection::JsPtc, false)
                .expect("JavaScript registry");
        let kind = presentation_for(provider);
        let javascript = ProductJavaScriptCompositionV1::compile_for_kind(
            plakous::tools::JobConfig::default(),
            javascript_registry,
            target.clone(),
            kind,
            catalog.iter().cloned(),
        )
        .expect("JavaScript core catalog");
        assert_eq!(javascript.nested_tools().len(), catalog.len());

        let python_registry =
            coding_tool_registry("catalog-python", CompositionSelection::PyPtc, false)
                .expect("Python registry");
        let python = PythonPtcV1::compile_for_kind(
            plakous::tools::JobConfig::default(),
            python_registry,
            target,
            PythonPtcPresentationV1::StructuredFunctions,
            kind,
            catalog.iter().cloned(),
        )
        .expect("Python core catalog");
        assert_eq!(python.nested_tools().len(), catalog.len());
    }
}

#[test]
fn one_extension_declaration_registers_and_compiles_through_both_local_runtimes() {
    let extensions = ProductToolExtensions::new([synthetic_product_extension("custom_lookup")])
        .expect("product extensions");
    let catalog = nested_catalog(NativeProvider::Xai, &extensions);
    assert_eq!(catalog.last().map(ToolName::as_str), Some("custom_lookup"));
    let configuration: EffectiveProviderConfigurationId = serde_json::from_value(
        serde_json::to_value(
            Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"synthetic-product-extension"],
            )
            .expect("configuration digest"),
        )
        .expect("configuration encoding"),
    )
    .expect("configuration identity");
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "test.responses",
        "test.oauth",
        "test.product-composition.v1",
        "test-family",
        "test-model",
        "test-model",
        configuration,
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .expect("projection target");

    let javascript_registry = coding_tool_registry_with_extensions(
        "extension-javascript",
        CompositionSelection::JsPtc,
        false,
        &extensions,
    )
    .expect("JavaScript registry");
    let kind = presentation_for(NativeProvider::Xai);
    let javascript = ProductJavaScriptCompositionV1::compile_for_kind(
        plakous::tools::JobConfig::default(),
        javascript_registry,
        target.clone(),
        kind,
        catalog.iter().cloned(),
    )
    .expect("JavaScript extension catalog");
    assert_eq!(
        javascript
            .nested_tools()
            .last()
            .map(|tool| tool.exposed_name().as_str()),
        Some("custom_lookup")
    );

    let python_registry = coding_tool_registry_with_extensions(
        "extension-python",
        CompositionSelection::PyPtc,
        false,
        &extensions,
    )
    .expect("Python registry");
    let python = PythonPtcV1::compile_for_kind(
        plakous::tools::JobConfig::default(),
        python_registry,
        target,
        PythonPtcPresentationV1::StructuredFunctions,
        kind,
        catalog.iter().cloned(),
    )
    .expect("Python extension catalog");
    assert_eq!(
        python
            .nested_tools()
            .last()
            .map(|tool| tool.canonical_name().as_str()),
        Some("custom_lookup")
    );
}

#[test]
fn coding_output_and_parallel_policies_follow_exact_provider_contracts() {
    let codex = selected_coding_provider_configuration(
        NativeProvider::Codex,
        ModelId::new("gpt-5.6-luna").expect("model"),
        ReasoningEffort::Medium,
        Some(NativeToolMode::Direct),
        CompositionSelection::FollowCatalog,
    )
    .expect("codex");
    assert_eq!(
        codex.behavior().output_bound,
        OutputBoundDisposition::NoRequestBound
    );
    assert_eq!(
        codex.behavior().parallel_tools,
        ParallelToolDisposition::Enabled
    );

    let grok = selected_coding_provider_configuration(
        NativeProvider::Xai,
        ModelId::new("grok-4.5").expect("model"),
        ReasoningEffort::Low,
        None,
        CompositionSelection::FollowCatalog,
    )
    .expect("grok");
    // The model's own 128k ceiling, held to the per-response ceiling: a
    // 4,096 bound cut whole-file writes off mid-call.
    assert_eq!(
        grok.behavior().output_bound,
        OutputBoundDisposition::Enforced(NonZeroU32::new(32_768).expect("bound"))
    );
    assert_eq!(
        grok.behavior().parallel_tools,
        ParallelToolDisposition::Disabled
    );

    let claude = selected_coding_provider_configuration(
        NativeProvider::Claude,
        ModelId::new("claude-sonnet-5").expect("model"),
        ReasoningEffort::Low,
        None,
        CompositionSelection::FollowCatalog,
    )
    .expect("claude");
    assert_eq!(
        claude.behavior().output_bound,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(plakous::anthropic::claude_max_output_tokens(
                "claude-sonnet-5"
            ))
            .expect("bound")
        )
    );
    let mut old_claude_behavior = claude.behavior().clone();
    old_claude_behavior.output_bound =
        OutputBoundDisposition::Enforced(NonZeroU32::new(4_096).expect("old bound"));
    let old_claude = plakous::model::SelectedProviderConfiguration::new(
        claude.route().clone(),
        old_claude_behavior,
    )
    .expect("old selection");
    assert_ne!(claude.identity(), old_claude.identity());
    for model in [
        "claude-haiku-5-5",
        "claude-fable-5-1",
        "claude-opus-5",
        "claude-opus-5-5",
    ] {
        let selected = selected_coding_provider_configuration(
            NativeProvider::Claude,
            ModelId::new(model).expect("model"),
            ReasoningEffort::Max,
            None,
            CompositionSelection::FollowCatalog,
        )
        .expect("Claude model");
        assert_eq!(
            selected.behavior().output_bound,
            OutputBoundDisposition::Enforced(NonZeroU32::new(128_000).expect("bound")),
            "{model}"
        );
    }
    let luna = selected_coding_provider_configuration(
        NativeProvider::Codex,
        ModelId::new("gpt-5.6-luna").expect("model"),
        ReasoningEffort::Medium,
        Some(NativeToolMode::Direct),
        CompositionSelection::FollowCatalog,
    )
    .expect("luna");
    assert_eq!(
        luna.behavior().output_bound,
        OutputBoundDisposition::NoRequestBound
    );
    let opus = selected_coding_provider_configuration(
        NativeProvider::Claude,
        ModelId::new("claude-opus-5").expect("model"),
        ReasoningEffort::Low,
        None,
        CompositionSelection::FollowCatalog,
    )
    .expect("opus");
    assert_eq!(
        opus.behavior().parallel_tools,
        ParallelToolDisposition::Enabled
    );
    assert_eq!(
        claude.behavior().parallel_tools,
        ParallelToolDisposition::Enabled
    );
    assert_eq!(
        claude.behavior().cache,
        CachePolicy::Enabled {
            requirement: CapabilityRequirement::Required,
        }
    );
    let haiku = selected_coding_provider_configuration(
        NativeProvider::Claude,
        ModelId::new("claude-sonnet-5").expect("model"),
        None,
        None,
        CompositionSelection::FollowCatalog,
    );
    assert_eq!(
        haiku
            .expect("Haiku without effort")
            .behavior()
            .reasoning_effort,
        ReasoningEffortDisposition::Disabled
    );
}

#[test]
fn root_grant_caller_is_genesis_tree_id_not_successor_session() {
    assert_eq!(
        topology_grant_caller_id(None, Some("genesis-tree"), "successor-session"),
        "genesis-tree"
    );
    assert_eq!(
        topology_grant_caller_id(Some("c1"), Some("genesis-tree"), "successor-session"),
        "c1"
    );
    assert_eq!(topology_grant_caller_id(None, None, "session"), "session");
}

/// B7: only a quota refusal, or a rate limit the provider
/// marks non-retryable or long, blocks the attempt; a short 429 is an
/// ordinary `rate_limited` failure the parent may retry.
#[test]
fn rate_limits_block_only_when_non_retryable_or_long() {
    use plakous::model::{ModelErrorKind, ModelRetryDelay, ModelRetryability};
    let short = ModelRetryability::Eligible {
        retry_after: Some(ModelRetryDelay::from_millis(30_000).unwrap()),
    };
    let long = ModelRetryability::Eligible {
        retry_after: Some(
            ModelRetryDelay::from_millis(
                u64::try_from(RATE_LIMIT_BLOCK_THRESHOLD.as_millis()).unwrap(),
            )
            .unwrap(),
        ),
    };
    assert_eq!(
        classify_provider_refusal(ModelErrorKind::RateLimited, Some(short), 0),
        Some(TurnRefusal::Failure {
            kind: "rate_limited"
        })
    );
    assert_eq!(
        classify_provider_refusal(ModelErrorKind::RateLimited, None, 0),
        Some(TurnRefusal::Failure {
            kind: "rate_limited"
        })
    );
    assert_eq!(
        classify_provider_refusal(
            ModelErrorKind::RateLimited,
            Some(ModelRetryability::Eligible { retry_after: None }),
            0
        ),
        Some(TurnRefusal::Failure {
            kind: "rate_limited"
        })
    );
    assert!(matches!(
        classify_provider_refusal(ModelErrorKind::RateLimited, Some(long), 0),
        Some(TurnRefusal::Blocked {
            quota: "rate_limited",
            resets_at: Some(_)
        })
    ));
    assert!(matches!(
        classify_provider_refusal(
            ModelErrorKind::RateLimited,
            Some(ModelRetryability::Permanent),
            0
        ),
        Some(TurnRefusal::Blocked {
            quota: "rate_limited",
            resets_at: None
        })
    ));
    assert!(matches!(
        classify_provider_refusal(ModelErrorKind::QuotaExceeded, None, 0),
        Some(TurnRefusal::Blocked {
            quota: "quota_exceeded",
            resets_at: None
        })
    ));
    assert_eq!(
        classify_provider_refusal(ModelErrorKind::InvalidRequest, None, 0),
        None
    );
}

/// B2: the cancelled branch of the turn driver seals only a `Closed`
/// node. An interrupted node keeps its running attempt and seals nothing.
#[test]
fn finalize_of_an_interrupted_turn_seals_nothing() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{AttemptScopedRequest, AttemptStatus, NodeStatus};
    use crate::agent_tree_protocol::{SpawnAgentRequest, Tier, WriterMode};
    use std::process::Command;

    let repo = tempfile::tempdir().expect("tempdir");
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "plak@example.test"],
        vec!["config", "user.name", "plak"],
        vec!["config", "commit.gpgsign", "false"],
        vec!["commit", "--allow-empty", "-m", "seed"],
    ] {
        assert!(
            Command::new("git")
                .args(&args)
                .current_dir(repo.path())
                .status()
                .expect("git")
                .success()
        );
    }
    let tree = Arc::new(Mutex::new(
        ProductAgentTree::bootstrap_root_for_test(
            uuid::Uuid::new_v4().to_string(),
            "root",
            "owner",
            repo.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .expect("tree"),
    ));
    let child = {
        let mut tree = tree.lock().expect("lock");
        let spawned = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("implement", Tier::Mutate, "impl", "edit"),
                None,
            )
            .expect("spawn");
        let path = tree.node(&spawned.agent_id).expect("node").path.clone();
        std::fs::write(path.join("H.txt"), "child\n").expect("write");
        tree.set_active_turn(&spawned.agent_id, true)
            .expect("active");
        tree.interrupt_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: spawned.agent_id.clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .expect("interrupt");
        spawned.agent_id
    };
    finalize_topology_child_turn(&driver_over(&tree), &child, true, false, None);
    let tree = tree.lock().expect("lock");
    assert_eq!(
        tree.node(&child).expect("node").status,
        NodeStatus::Interrupted
    );
    assert_eq!(
        tree.attempt(&child).expect("attempt").status,
        AttemptStatus::Running
    );
    assert!(tree.candidates().is_empty(), "an interrupt does not seal");
}

/// The runtime reports a joined cancel as `Ok(TurnResult)` whose outcome is
/// `Cancelled`; only that classification keeps an interrupted child out of
/// the submit-harness and materialization paths. An outer error means no
/// authoritative terminal was published, even when its kind says Cancelled.
#[test]
fn missing_child_terminal_authority_is_not_classified_as_joined_cancellation() {
    use plakous::agent::AgentError;
    let cancelled: Result<Arc<TurnResult>, AgentError> = Err(AgentError::new(
        AgentErrorKind::Cancelled,
        "turn was cancelled",
    ));
    assert!(!child_turn_was_cancelled(&cancelled));
    let failed: Result<Arc<TurnResult>, AgentError> =
        Err(AgentError::new(AgentErrorKind::Model, "provider refused"));
    assert!(!child_turn_was_cancelled(&failed));
}

/// Gate 3: only the host's own `interrupt_agent`/`close_agent` mark the
/// node, so a cancel on any other status came from the provider or the
/// server and must end the attempt instead of leaving it `running`.
#[test]
fn a_cancel_on_a_running_node_is_not_read_as_an_interrupt() {
    use crate::agent_tree_protocol::NodeStatus;
    assert_eq!(
        TurnOutcome::classify(Some(NodeStatus::Running), true),
        TurnOutcome::CancelledOutside
    );
    assert_eq!(
        TurnOutcome::classify(Some(NodeStatus::Interrupted), true),
        TurnOutcome::Interrupted
    );
    assert_eq!(
        TurnOutcome::classify(Some(NodeStatus::Closed), true),
        TurnOutcome::Closed
    );
    assert_eq!(
        TurnOutcome::classify(Some(NodeStatus::Closed), false),
        TurnOutcome::Closed,
        "a close fences the turn however it ended"
    );
    assert_eq!(
        TurnOutcome::classify(Some(NodeStatus::Running), false),
        TurnOutcome::Ran
    );
    assert_eq!(
        TurnOutcome::classify(None, true),
        TurnOutcome::CancelledOutside,
        "a node the tree cannot read is not an interrupted one"
    );
}

/// A reopen blocked by a holder this host cannot bury says how long to wait.
///
/// A holder this host can prove is gone has its branch reclaimed at once, so
/// reaching this refusal means the holder is alive or is somewhere this process
/// cannot speak for. Then waiting is the remedy, and the line has to say so or
/// the user reads a broken session and stops trying. A refusal that waiting
/// will not fix says nothing about retrying, because it would not be true.
#[test]
fn a_reopen_refused_only_by_a_live_lease_says_when_to_retry() {
    let waiting = build::reopen_refusal("exact session reopen requires recovery", Some(30));
    let message = waiting.message();
    assert!(
        message.contains("could not be confirmed gone"),
        "a holder this host could prove dead would have been reclaimed: {message}"
    );
    assert!(message.contains("retry in about 30 seconds"), "{message}");
    assert!(
        message.starts_with("exact session reopen requires recovery"),
        "the reason still leads: {message}"
    );

    let permanent = build::reopen_refusal("exact session reopen requires recovery", None);
    assert_eq!(
        permanent.message(),
        "exact session reopen requires recovery"
    );
    assert_eq!(
        waiting.is_recovery_required(),
        permanent.is_recovery_required(),
        "the class, and so the exit code, is unchanged"
    );
}

#[test]
fn a_tool_name_clash_fails_construction_naming_both_providers() {
    // A product extension spelled like a Codex pack name covers another slot
    // and declares no alternate: nothing may shadow the other.
    let extensions = ProductToolExtensions::new([synthetic_product_extension("exec_command")])
        .expect("product extensions");
    let registry = coding_tool_registry_with_extensions(
        "namespace-clash",
        CompositionSelection::Direct,
        false,
        &extensions,
    )
    .expect("registry");
    let error = crate::tools::product_surface(
        plakous::tools::JobConfig::default(),
        presentation_for(NativeProvider::Codex),
        registry,
        rendered_prompts::suite::synthetic_target(b"namespace-clash"),
        &extensions,
    )
    .expect_err("an undeclared clash fails construction");
    assert!(
        error.message().contains(
            "tool name `exec_command` is presented by pack `plakous.process.v3` and pack \
             `plak.product-extensions.v1` in one namespace; neither declares an alternate spelling"
        ),
        "{}",
        error.message()
    );
}

/// An API child's pin follows the API table for the model its node runs: an
/// OpenAI API child on `plakous-default` pins `native-ptc`, and an explicit
/// `native-ptc` card on the xAI or Anthropic API is refused at the pin even
/// when the epoch's account hints would let it spawn.
#[tokio::test]
async fn api_child_pin_follows_the_api_table_for_its_node_model() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{
        Delegation, MAX_DIRECT_CHILDREN, MAX_TREE_ATTEMPTS, MAX_TREE_DEPTH, MAX_TREE_NODES,
        SpawnAgentRequest, TemplateName, Tier, WriterMode,
    };
    use crate::orchestration::{
        BindHints, EpochBounds, OrchestrationSnapshot, RootBind, RosterBinding, RosterInput,
    };
    use plakous_host_protocol::selection::CompositionSelection as CardComposition;

    let workspace = tempfile::tempdir().expect("workspace");
    let rows = [
        ("openai-worker", "openai", "gpt-5.6-sol", None),
        (
            "xai-worker",
            "xai-api",
            "grok-4.6",
            Some(CardComposition::NativePtc),
        ),
        (
            "anthropic-worker",
            "anthropic",
            "claude-sonnet-5",
            Some(CardComposition::NativePtc),
        ),
    ];
    let mut templates = std::collections::BTreeMap::new();
    let mut working_set = std::collections::BTreeSet::new();
    for (name, account, model, composition) in rows {
        let mut card = RosterBinding::leaf(account, model);
        card.template = name.to_owned();
        card.description = format!("{model} worker");
        card.effort = Some("low".to_owned());
        card.composition = composition;
        let name = TemplateName::parse(name).expect("template name");
        working_set.insert(name.clone());
        templates.insert(name, card);
    }
    let snapshot = OrchestrationSnapshot::freeze(RosterInput {
        workspace_idle_seconds: 600,
        workspace_budget_mib: 4096,
        child_retention_days: 7,
        child_retention_count: 128,
        root: RootBind {
            delegation: Delegation::default(),
            template: None,
            account: "openai".to_owned(),
            tuple_id: "gpt-5.6-sol".to_owned(),
            effort: Some("low".to_owned()),
        },
        writer_mode: WriterMode::SingleLease,
        bounds: EpochBounds {
            max_depth: u16::from(MAX_TREE_DEPTH),
            max_children: MAX_DIRECT_CHILDREN as u16,
            max_nodes: MAX_TREE_NODES as u16,
            max_attempts: MAX_TREE_ATTEMPTS as u16,
            max_agents_per_cell: crate::cell_budget::DEFAULT_MAX_AGENTS_PER_CELL,
        },
        require_review_evidence: false,
        transient_resumes: 1,
        working_set,
        templates,
        templates_digest: String::new(),
        // The pin, not the hint, is the fence under test.
        bind_hints: BindHints::from_names(&["openai", "xai-api", "anthropic"], &[]),
        tool_sections: std::collections::BTreeMap::new(),
    });
    let tree_id = crate::topology_runtime::test_support::unique_tree_id();
    let mut tree = ProductAgentTree::bootstrap_root(
        &tree_id,
        "root",
        "root",
        workspace.path().to_path_buf(),
        "gpt-5.6-sol",
        WriterMode::SingleLease,
    )
    .expect("tree");
    tree.install_snapshot(snapshot);
    let mut children = Vec::new();
    for (name, ..) in rows {
        let spawned = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named(name, Tier::ReadOnly, name, "review the diff"),
                None,
            )
            .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        children.push((name, spawned.agent_id));
    }
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None).expect("hub");
    for (name, agent_id) in children {
        let pinned = frozen_openrouter_child_pin(&tree_id, &agent_id);
        match name {
            "openai-worker" => assert_eq!(
                pinned.expect("OpenAI API child pins").0,
                CompositionSelection::NativePtc
            ),
            "xai-worker" => assert!(
                pinned
                    .expect_err("native-ptc refused at the pin")
                    .contains("the xAI API has no native programmatic tool calling"),
                "{name}"
            ),
            _ => assert!(
                pinned
                    .expect_err("native-ptc refused at the pin")
                    .contains("the Anthropic API has no native programmatic tool calling"),
                "{name}"
            ),
        }
    }
    crate::topology_runtime::force_release(&tree_id).await;
}

/// A child binds its own frozen card's tool selection, never its parent's:
/// a parent selecting nothing spawns an `x_search` card that carries it, and a
/// card selecting nothing binds nothing.
#[tokio::test]
async fn a_child_binds_its_own_frozen_card_tools() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{SpawnAgentRequest, TemplateName, Tier, WriterMode};
    use crate::orchestration::{BindHints, OrchestrationSnapshot, RosterBinding, RosterInput};

    let workspace = tempfile::tempdir().expect("workspace");
    let mut input = RosterInput::unpinned("gpt-5.6-sol");
    input.bind_hints = BindHints::from_names(&["grok", "codex"], &[]);
    for (name, account, tools) in [
        ("searcher", "grok", vec!["x_search".to_owned()]),
        ("plain", "codex", Vec::new()),
    ] {
        let mut card = RosterBinding::leaf(account, "model-a");
        card.template = name.to_owned();
        card.description = format!("{name} worker");
        card.tools = tools;
        let name = TemplateName::parse(name).expect("template name");
        input.working_set.insert(name.clone());
        input.templates.insert(name, card);
    }
    let tree_id = crate::topology_runtime::test_support::unique_tree_id();
    let mut tree = ProductAgentTree::bootstrap_root(
        &tree_id,
        "root",
        "root",
        workspace.path().to_path_buf(),
        "gpt-5.6-sol",
        WriterMode::SingleLease,
    )
    .expect("tree");
    tree.install_snapshot(OrchestrationSnapshot::freeze(input));
    let mut children = Vec::new();
    for name in ["searcher", "plain"] {
        let spawned = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named(name, Tier::ReadOnly, name, "look"),
                None,
            )
            .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        children.push((name, spawned.agent_id));
    }
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None).expect("hub");
    for (name, agent_id) in children {
        let names: Vec<String> = frozen_child_tool_selection(&tree_id, &agent_id)
            .expect(name)
            .entries()
            .iter()
            .map(ToString::to_string)
            .collect();
        let expected: &[&str] = if name == "searcher" {
            &["x_search"]
        } else {
            &[]
        };
        assert_eq!(names, expected, "{name}");
    }
    crate::topology_runtime::force_release(&tree_id).await;
}

/// For every presentation x fence x composition runtime, the surface compiles
/// under the runtime's reserved markers and renames only what declares an
/// alternate: pi's `wait` becomes `wait_jobs` beside the native-ptc `wait`
/// marker; every other nested catalog equals the Direct surface.
#[test]
fn session_surfaces_rename_nothing_under_any_runtime() {
    use plakous::tools::CompositionRuntime;

    let names = [
        "spawn_agent",
        "wait_agent",
        "send_agent_message",
        "list_agents",
        "close_agent",
        "interrupt_agent",
        "recover_agent",
        "adopt_candidate",
        "discard_workspace",
        "submit_result",
    ];
    for extension_names in [&[][..], &names[..]] {
        let extensions = crate::tools::test_support::extensions(extension_names);
        for presentation in [
            CodingPresentation::Codex,
            CodingPresentation::Claude,
            CodingPresentation::Grok,
            CodingPresentation::Pi,
        ] {
            for fence in [
                SessionToolFence::Full,
                SessionToolFence::OccupancyReadOnly,
                SessionToolFence::OccupancyExecute,
            ] {
                let registry = coding_tool_registry_with_extensions(
                    "session-surfaces",
                    CompositionSelection::Direct,
                    false,
                    &extensions,
                )
                .expect("registry");
                let surface = |runtime| {
                    crate::tools::product_surface_with_fence(
                        plakous::tools::JobConfig::default(),
                        presentation,
                        registry.clone(),
                        rendered_prompts::suite::synthetic_target(b"session-surfaces"),
                        &extensions,
                        fence,
                        runtime,
                    )
                    .expect("surface")
                };
                let direct = surface(None);
                for runtime in [
                    CompositionRuntime::CodexNative,
                    CompositionRuntime::ProductJavaScript,
                    CompositionRuntime::PythonPtc,
                ] {
                    let expected = direct
                        .exposed_names()
                        .iter()
                        .map(|name| match name.as_str() {
                            "wait" if runtime == CompositionRuntime::CodexNative => {
                                ToolName::new("wait_jobs").expect("alternate")
                            }
                            _ => name.clone(),
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(
                        surface(Some(runtime)).exposed_names(),
                        expected,
                        "{presentation:?} {fence:?} {runtime:?}"
                    );
                }
            }
        }
    }
}

/// A Grok root granted agent tools presents its await and interrupt as Grok's
/// output and kill tools: the tree's capability claims the pack's process-only
/// entries of the same names, nothing is presented twice, and the claimed
/// entries raise the tree's own text unchanged.
#[test]
fn grok_hub_surface_presents_the_tree_await_under_grok_names_once() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::WriterMode;
    use crate::agent_tree_tools::TopologyVocabulary;
    use crate::topology_extensions::topology_tool_extensions_in;

    let root = tempfile::tempdir().expect("workspace");
    let tree = ProductAgentTree::bootstrap_root_for_test(
        "tree-grok-hub",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    let extensions = topology_tool_extensions_in(
        Arc::new(Mutex::new(tree)),
        "root",
        TopologyVocabulary::TaskTools,
    )
    .expect("root extensions");
    let registry = coding_tool_registry_with_extensions(
        "grok-hub",
        CompositionSelection::Direct,
        false,
        &extensions,
    )
    .expect("registry");
    let surface = crate::tools::product_surface(
        plakous::tools::JobConfig::default(),
        CodingPresentation::Grok,
        registry,
        rendered_prompts::suite::synthetic_target(b"grok-hub"),
        &extensions,
    )
    .expect("surface");
    let names = surface
        .exposed_names()
        .iter()
        .map(ToolName::as_str)
        .collect::<Vec<_>>();
    for name in ["get_command_or_subagent_output", "kill_command_or_subagent"] {
        assert_eq!(
            names.iter().filter(|exposed| **exposed == name).count(),
            1,
            "{name}: {names:?}"
        );
        let entry = surface
            .projection()
            .by_exposed(&ToolName::new(name).expect("name"))
            .expect("entry");
        assert!(
            entry
                .canonical_operations()
                .values()
                .all(|identity| identity.name().as_str() == name),
            "{name} binds the tree's capability"
        );
        assert_eq!(format!("{:?}", entry.raising()), "ModelContentIdentityV1");
    }
    for name in ["wait_agent", "interrupt_agent"] {
        assert!(!names.contains(&name), "{name}: {names:?}");
    }
    assert!(names.contains(&"close_agent"));
    assert!(names.contains(&"spawn_agent"));
}

/// Spec 01 §8 says every process tool description, including read/await/stop
/// tools, states the bound foreground budget and hard runtime cap. On a Grok
/// hub the claimed output and kill tools are the node's only process
/// read/await/stop tools, so their descriptions carry the same bound tail
/// the leaf's do.
#[test]
fn grok_hub_claimed_process_tools_state_the_bound_job_lifetime() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::WriterMode;
    use crate::agent_tree_tools::TopologyVocabulary;
    use crate::topology_extensions::topology_tool_extensions_in;

    let root = tempfile::tempdir().expect("workspace");
    let tree = ProductAgentTree::bootstrap_root_for_test(
        "tree-grok-hub-tail",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    let extensions = topology_tool_extensions_in(
        Arc::new(Mutex::new(tree)),
        "root",
        TopologyVocabulary::TaskTools,
    )
    .expect("root extensions");
    let compile = |extensions: &ProductToolExtensions| {
        let registry = coding_tool_registry_with_extensions(
            "grok-hub-tail",
            CompositionSelection::Direct,
            false,
            extensions,
        )
        .expect("registry");
        crate::tools::product_surface(
            plakous::tools::JobConfig::default(),
            CodingPresentation::Grok,
            registry,
            rendered_prompts::suite::synthetic_target(b"grok-hub-tail"),
            extensions,
        )
        .expect("surface")
    };
    let hub = compile(&extensions);
    let leaf = compile(&ProductToolExtensions::new([]).expect("empty extensions"));
    for name in ["get_command_or_subagent_output", "kill_command_or_subagent"] {
        let tool = ToolName::new(name).expect("name");
        let leaf_text = leaf
            .projection()
            .by_exposed(&tool)
            .expect("leaf entry")
            .description()
            .to_owned();
        assert!(
            leaf_text.contains("hard runtime cap"),
            "{name}: {leaf_text}"
        );
        let hub_text = hub
            .projection()
            .by_exposed(&tool)
            .expect("hub entry")
            .description()
            .to_owned();
        assert!(
            hub_text.contains("hard runtime cap"),
            "{name} on a Grok hub drops the bound job-lifetime tail: {hub_text}"
        );
    }
}

/// The xAI API admits to the Grok pack through the same presentation compiler
/// a compiled session uses, so a hub's Direct surface and its js-ptc nested
/// catalog both present the await and interrupt as Grok's task tools.
#[test]
fn xai_api_hub_surface_presents_task_tools_for_direct_and_js_ptc() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::WriterMode;
    use crate::binding::PlatformProvider;
    use crate::topology_extensions::topology_tool_extensions_in;

    let presentation =
        crate::coding_routes::platform_route_presentation(PlatformProvider::Xai, "grok-4.6")
            .expect("xAI API presentation");
    assert_eq!(presentation, CodingPresentation::Grok);
    let vocabulary = vocabulary_for_presentation(presentation);
    assert_eq!(
        vocabulary,
        crate::agent_tree_tools::TopologyVocabulary::TaskTools
    );
    let root = tempfile::tempdir().expect("workspace");
    let tree = ProductAgentTree::bootstrap_root_for_test(
        "tree-xai-api-hub",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .expect("tree");
    let extensions = topology_tool_extensions_in(Arc::new(Mutex::new(tree)), "root", vocabulary)
        .expect("extensions");
    for (composition, runtime) in [
        (CompositionSelection::Direct, None),
        (
            CompositionSelection::JsPtc,
            Some(plakous::tools::CompositionRuntime::ProductJavaScript),
        ),
    ] {
        let registry =
            coding_tool_registry_with_extensions("xai-api-hub", composition, false, &extensions)
                .expect("registry");
        let surface = crate::tools::product_surface_with_fence(
            plakous::tools::JobConfig::default(),
            presentation,
            registry,
            rendered_prompts::suite::synthetic_target(b"xai-api-hub"),
            &extensions,
            SessionToolFence::Full,
            runtime,
        )
        .expect("surface");
        let names = surface
            .exposed_names()
            .iter()
            .map(ToolName::as_str)
            .collect::<Vec<_>>();
        for name in ["get_command_or_subagent_output", "kill_command_or_subagent"] {
            assert_eq!(
                names.iter().filter(|exposed| **exposed == name).count(),
                1,
                "{composition:?} {name}: {names:?}"
            );
        }
        for name in ["wait_agent", "interrupt_agent"] {
            assert!(!names.contains(&name), "{composition:?} {name}: {names:?}");
        }
    }
}

/// The vocabulary a node granted agent tools gets follows the pack its route
/// wears: pi (OpenRouter) spells the await and interrupt `wait` and `stop`,
/// Grok (SuperGrok and the xAI API) the task tools, every other pack the Plak
/// names.
#[test]
fn topology_vocabulary_follows_the_pack_the_route_wears() {
    use crate::agent_tree_tools::TopologyVocabulary;

    for (presentation, vocabulary) in [
        (CodingPresentation::Pi, TopologyVocabulary::WaitStop),
        (CodingPresentation::Grok, TopologyVocabulary::TaskTools),
        (CodingPresentation::Codex, TopologyVocabulary::Plak),
        (CodingPresentation::Claude, TopologyVocabulary::Plak),
    ] {
        assert_eq!(
            vocabulary_for_presentation(presentation),
            vocabulary,
            "{presentation:?}"
        );
    }
}

/// A tree node's await spelling comes from the presentation its bound account
/// and model compile, so every account spelling of one subscription agrees
/// (`supergrok` and `xai-oauth` are `grok`) and an unenrolled pair has none.
#[test]
fn node_presentation_reads_the_route_catalog_for_every_account_spelling() {
    for (account, model, expected) in [
        ("grok", "grok-4.5", Some(CodingPresentation::Grok)),
        ("supergrok", "grok-4.5", Some(CodingPresentation::Grok)),
        ("xai-oauth", "grok-4.5", Some(CodingPresentation::Grok)),
        ("xai-api", "grok-4.6", Some(CodingPresentation::Grok)),
        (
            "openrouter",
            "vendor/any-model",
            Some(CodingPresentation::Pi),
        ),
        ("codex", "gpt-5.6-luna", Some(CodingPresentation::Codex)),
        (
            "claude",
            "claude-sonnet-5",
            Some(CodingPresentation::Claude),
        ),
        ("xai-api", "grok-unenrolled", None),
        ("unattached", "grok-4.5", None),
    ] {
        assert_eq!(
            node_coding_presentation(account, model),
            expected,
            "{account}/{model}"
        );
    }
}

fn standard_model(
    selected: &plakous::model::SelectedProviderConfiguration,
    family: &str,
) -> Result<plakous::model::ConfiguredLanguageModel, Box<dyn std::error::Error>> {
    use plakous::model::{
        Capability, CapabilityConstraints, CapabilitySet, EffectiveProviderConfiguration,
        FactProvenance, FactSource, FamilyPolicyEnvelope, FeatureSupport, ModelAdapterProfileId,
        ModelFamilyId, ModelImplementationIdentity, ProviderSemanticEvidence,
        ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, RuntimeModelAuthority, Support,
        Timestamp,
    };

    let route = selected.route();
    let family = ModelFamilyId::new(family)?;
    let provenance = FactProvenance::new(FactSource::TestFixture, Timestamp::new(1, 0)?, None);
    let capabilities = [
        Capability::Tools,
        Capability::Reasoning,
        Capability::ParallelTools,
        Capability::Caching,
    ]
    .into_iter()
    .map(|capability| {
        (
            capability,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        )
    })
    .collect::<std::collections::BTreeMap<_, _>>();
    let authority = plakous_model::StandardIntegrationAuthority::release_owned();
    let descriptor = ResolvedModelDescriptor::new(
        &authority,
        route.protocol().clone(),
        route.route().clone(),
        route.requested_model().clone(),
        Some(route.requested_model().clone()),
        Some(family.clone()),
        ModelAdapterProfileId::new(route.integration_profile().as_str())?,
        CapabilitySet::new(capabilities, std::collections::BTreeMap::new()),
    );
    let runtime = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential"])?,
        1,
        Timestamp::new(1, 0)?,
        None,
    )?;
    let binding = ResolvedProviderAuthorityBinding::new(
        &descriptor,
        &runtime,
        ProviderSemanticEvidence::new(1, b"fixture".to_vec())?,
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        selected,
        descriptor.clone(),
        ModelImplementationIdentity::release_owned(&authority, &descriptor, 1)?,
        binding,
        selected.behavior().clone(),
        FamilyPolicyEnvelope::new(family, 1, 1, 1, b"fixture".to_vec())?,
    )?;
    let configuration =
        plakous::model::CompiledProviderConfiguration::new(selected.clone(), effective)?;
    let model = Arc::new(UncalledModel {
        descriptor,
        authority: runtime.clone(),
    });
    Ok(plakous::model::ConfiguredLanguageModel::new(
        &authority,
        model,
        configuration,
        runtime,
    )?)
}

/// A SuperGrok (native xAI OAuth) hub whose topology output and kill claim the
/// Grok process slots creates and reopens through `ProductProjectionAdmission`
/// and `AgentSessions`, on every occupancy fence.
#[tokio::test]
async fn supergrok_hub_creates_and_reopens_on_every_fence() -> Result<(), Box<dyn std::error::Error>>
{
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::WriterMode;

    let mut failures = Vec::new();
    for model_name in ["grok-4.5", "grok-4.6"] {
        for fence in [
            SessionToolFence::Full,
            SessionToolFence::OccupancyExecute,
            SessionToolFence::OccupancyReadOnly,
        ] {
            let outcome: Result<(), String> = async {
                let (_workspace_dir, workspace_root, _) = fixture_workspace();
                let workspace = Arc::new(
                    stamped_workspace_policy(
                        &std::fs::canonicalize(&workspace_root).map_err(|e| e.to_string())?,
                        Path::new(""),
                    )
                    .map_err(|e| e.message().to_owned())?,
                );
                let composition = CompositionSelection::FollowCatalog;
                let model_id = ModelId::new(model_name).map_err(|e| e.to_string())?;
                let selected = selected_coding_provider_configuration(
                    NativeProvider::Xai,
                    model_id.clone(),
                    ReasoningEffort::High,
                    None,
                    composition,
                )
                .map_err(|e| format!("selection: {}", e.message()))?;
                let resolved = standard_model(&selected, "responses-family")
                    .map_err(|e| format!("model: {e}"))?;
                let job_config = plakous::tools::JobConfig::default();
                let store = tempfile::tempdir().map_err(|e| e.to_string())?;
                let sessions = plakous::sessions::AgentSessions::new(
                    plakous::sessions::SessionBackend::sqlite(store.path().join("runtime.sqlite3"))
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
                let root = tempfile::tempdir().map_err(|e| e.to_string())?;
                let tree = ProductAgentTree::bootstrap_root_for_test(
                    "tree-supergrok-hub",
                    "root",
                    "owner",
                    root.path().to_path_buf(),
                    model_name,
                    WriterMode::SingleLease,
                )
                .map_err(|e| e.message)?;
                let extensions = crate::topology_extensions::topology_tool_extensions_in(
                    Arc::new(Mutex::new(tree)),
                    "root",
                    crate::agent_tree_tools::TopologyVocabulary::TaskTools,
                )
                .map_err(|e| e.message().to_owned())?;
                let extras = extensions.names().cloned().collect::<Vec<_>>();
                let registry = coding_tool_registry_with_extensions(
                    "supergrok-hub",
                    composition,
                    false,
                    &extensions,
                )
                .map_err(|e| e.message().to_owned())?;
                let build =
                    |fresh: bool| -> Result<plakous::sessions::SessionRuntimeBinding, String> {
                        let node = node_session(
                            job_config,
                            resolved.clone(),
                            &registry,
                            &extensions,
                            &extras,
                            fence,
                            None,
                        )
                        .map_err(|e| format!("node session: {}", e.message()))?;
                        let names = node
                            .surface
                            .projection()
                            .entries()
                            .iter()
                            .map(|entry| entry.exposed_name().as_str().to_owned())
                            .collect::<Vec<_>>();
                        if names
                            .iter()
                            .any(|n| n == "wait_agent" || n == "interrupt_agent")
                        {
                            return Err(format!("retired names presented: {names:?}"));
                        }
                        if !names.iter().any(|n| n == "get_command_or_subagent_output") {
                            return Err(format!("claimed output not presented: {names:?}"));
                        }
                        let mut builder = plakous::agent::AgentBuilder::new(
                            node.configured_model.clone(),
                            registry.clone(),
                            Arc::clone(&workspace),
                        )
                        .projection_admission(Arc::new(
                            crate::coding_routes::ProductProjectionAdmission,
                        ))
                        .job_config(job_config)
                        .tool_policy(session_tool_policy(fence))
                        .tool_projection(node.surface.projection().clone())
                        .request_policy(
                            plakous::agent::AgentRequestPolicy::new(
                                plakous::agent::GenerationLimits::new(
                                    node.configuration
                                        .effective()
                                        .behavior()
                                        .output_bound
                                        .bound(),
                                    Vec::new(),
                                )
                                .map_err(|e| e.to_string())?,
                                plakous::agent::ReasoningPolicy::Enabled {
                                    effort: Some(ReasoningEffort::High),
                                    requirement: CapabilityRequirement::Required,
                                },
                            ),
                        );
                        if fresh {
                            let profile = selected_harness_profile(
                                NativeProvider::Xai,
                                &model_id,
                                ReasoningEffort::High,
                                false,
                                false,
                                &extras,
                                true,
                                composition,
                                &node.surface,
                                workspace.root(),
                                workspace.cwd(),
                            )
                            .map_err(|e| format!("profile: {}", e.message()))?
                            .ok_or("no fresh profile")?;
                            let plan = plakous::agent::compile_explicit_runtime_profile(
                                &profile,
                                node.configured_model.model().as_ref(),
                                &node.configuration,
                                &registry,
                                workspace.as_ref(),
                                Vec::new(),
                            )
                            .map_err(|e| format!("plan: {e}"))?;
                            builder = builder.harness_plan(plan);
                        }
                        Ok(plakous::sessions::SessionRuntimeBinding::new(builder))
                    };
                let created = sessions
                    .create(plakous::sessions::CreateSessionRequest::new(
                        plakous::sessions::SessionOperationId::new("supergrok-hub-create")
                            .map_err(|e| e.to_string())?,
                        build(true)?,
                    ))
                    .await
                    .map_err(|e| format!("create: {e}"))?;
                let plakous::sessions::CreateSessionOutcome::Created { session, .. } = created
                else {
                    return Err("create: not created".into());
                };
                let locator = session.locator();
                session.release().await.map_err(|e| e.to_string())?;
                match sessions
                    .open(plakous::sessions::OpenSessionRequest::new(
                        plakous::sessions::SessionOperationId::new("supergrok-hub-reopen")
                            .map_err(|e| e.to_string())?,
                        locator,
                        build(false)?,
                    ))
                    .await
                {
                    Ok(plakous::sessions::OpenSessionOutcome::Opened { session, .. }) => {
                        session.release().await.map_err(|e| e.to_string())?;
                        Ok(())
                    }
                    Ok(_) => Err("reopen: not opened".into()),
                    Err(e) => Err(format!("reopen [{:?}]: {e}", e.kind())),
                }
            }
            .await;
            if let Err(error) = outcome {
                failures.push(format!("{model_name} {fence:?}: {error}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    Ok(())
}

/// A codex root running its `[root] agent` template binds that template's
/// selection, and `x_search` refuses the codex bind with its one-line
/// remedy; a root bound by an explicit tuple takes no template tools.
#[test]
fn a_codex_root_template_selecting_x_search_refuses_the_native_bind()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::agent::build::bind_node_tool_plan;
    let fact = crate::binding::RootTemplateFact {
        name: "root-agent".to_owned(),
        account: "codex".to_owned(),
        model: "gpt-6-astra".to_owned(),
        tools: crate::tool_plan::TemplateToolSelection::parse(["x_search"])?,
    };
    let explicit = crate::binding::RootTemplateFact::root_tool_selection(
        Some(&fact),
        Some("codex"),
        Some("gpt-6-sol"),
    );
    assert!(explicit.is_empty());
    assert!(
        bind_node_tool_plan(
            &explicit,
            crate::tool_plan::ToolRoute::native(NativeProvider::Codex),
            "codex",
        )
        .is_ok()
    );
    let selection = crate::binding::RootTemplateFact::root_tool_selection(
        Some(&fact),
        Some("codex"),
        Some("gpt-6-astra"),
    );
    let refused = bind_node_tool_plan(
        &selection,
        crate::tool_plan::ToolRoute::native(NativeProvider::Codex),
        "codex",
    )
    .err()
    .ok_or("x_search bound on codex")?;
    assert_eq!(
        refused.message(),
        "x_search is not available on codex; \
         remedy: put x_search on a grok or xai-api template and spawn it"
    );
    Ok(())
}

/// A selected tool the route cannot serve fails API session creation with its
/// one-line refusal, before any topology attach or network.
#[tokio::test]
async fn a_selected_tool_the_route_cannot_serve_refuses_api_session_creation()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    for (target, selection, expected) in [
        (
            crate::binding::ApiTarget::OpenRouter {
                wire: ResolvedProtocol::Chat,
            },
            "x_search",
            "x_search is not available on openrouter; \
             remedy: put x_search on a grok or xai-api template and spawn it",
        ),
        (
            crate::binding::ApiTarget::Platform {
                provider: crate::binding::PlatformProvider::Anthropic,
            },
            "mcp:github",
            "mcp:github is not available in this build; \
             remedy: remove mcp:github from the template's tools",
        ),
    ] {
        let mut parent = openrouter_test_parent();
        parent.workspace_root = directory.path().to_path_buf();
        parent.cwd = directory.path().to_path_buf();
        parent.target = target;
        parent.tool_selection = crate::tool_plan::TemplateToolSelection::parse([selection])?;
        let refused = api_session_runtime_binding(&parent, false, SessionToolFence::Full, None)
            .await
            .err()
            .ok_or("a selection the route cannot serve bound")?;
        assert_eq!(refused.message(), expected);
    }
    Ok(())
}
