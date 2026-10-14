//! Exercises the production product binding without sending inference requests.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "offline regression assertions"
)]
use super::*;
use crate::binding::CompositionSelection;
use crate::platform::ApiWindow;
use plakous::agent::ReasoningEffort;

struct Credentials(&'static str);
impl PlatformCredentials for Credentials {
    fn openai(&self) -> CliResult<Arc<dyn plakous::openai::PlatformCredentialSource>> {
        use plakous::openai::{
            CredentialAuthorityId, CredentialReference, OpenAiApiKey,
            StaticPlatformCredentialSource,
        };
        Ok(Arc::new(StaticPlatformCredentialSource::new(
            OpenAiApiKey::new(self.0).map_err(api_error)?,
            CredentialReference::application("plak/openai").map_err(api_error)?,
            CredentialAuthorityId::new("fixture/openai").map_err(api_error)?,
        )))
    }
    fn anthropic(&self) -> CliResult<Arc<dyn plakous::anthropic::AnthropicApiKeySource>> {
        Ok(Arc::new(
            plakous::anthropic::AnthropicApiKey::new(self.0).map_err(api_error)?,
        ))
    }
    fn xai(&self) -> CliResult<Arc<dyn plakous::xai::XaiPlatformApiKeySource>> {
        Ok(Arc::new(XaiKey(self.0)))
    }
}
struct XaiKey(&'static str);
impl plakous::xai::XaiPlatformApiKeySource for XaiKey {
    fn grant(
        &self,
    ) -> Result<plakous::xai::XaiPlatformCredentialGrant, plakous::xai::XaiPlatformBuildError> {
        let authority =
            plakous::xai::XaiPlatformApiKeyAuthority::new("fixture/xai", "fixture/key", 1)
                .map_err(|_| plakous::xai::XaiPlatformBuildError::Authentication)?;
        plakous::xai::XaiPlatformApiKey::new(self.0)?.grant(authority)
    }
    fn api_key(
        &self,
        expected: &plakous::xai::XaiPlatformCredentialGrant,
    ) -> Result<plakous::xai::XaiPlatformApiKey, plakous::xai::XaiPlatformBuildError> {
        if &self.grant()? != expected {
            return Err(plakous::xai::XaiPlatformBuildError::Authentication);
        }
        plakous::xai::XaiPlatformApiKey::new(self.0)
    }
}
struct NoWorkers;
impl plakous::tools::SidecarLocator for NoWorkers {
    fn javascript_host(&self) -> Result<std::path::PathBuf, String> {
        Err("no test worker".into())
    }
    fn python_host(&self) -> Result<std::path::PathBuf, String> {
        Err("no test worker".into())
    }
}

#[tokio::test]
async fn product_api_bindings_compile_exact_tools_and_reconstruct_same_credential_only() {
    let directory = tempfile::tempdir().unwrap();
    let extensions = crate::tools::ProductToolExtensions::default();
    for (provider, model) in [
        (PlatformProvider::OpenAi, "gpt-5.6-luna"),
        (PlatformProvider::OpenAi, "gpt-5.6-sol"),
        (PlatformProvider::Anthropic, "claude-sonnet-5"),
        (PlatformProvider::Anthropic, "claude-opus-5"),
        (PlatformProvider::Anthropic, "claude-haiku-5-5"),
        (PlatformProvider::Xai, "grok-4.6"),
    ] {
        for effort in [ReasoningEffort::Low, ReasoningEffort::Medium] {
            let spec = ApiRuntimeSpec {
                job_config: plakous::tools::JobConfig::default(),
                account: provider.account(),
                sidecars: &NoWorkers,
                product_home: None,
                literal_model: model,
                effort: Some(effort),
                composition: CompositionSelection::Direct,
                wire: provider.protocol(),
                workspace_root: directory.path(),
                cwd: directory.path(),
                plan_state_authority: "fixture-session",
                opening_durable_session: false,
                tool_fence: crate::agent::SessionToolFence::Full,
                scheduled_compaction: false,
                max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
                topology_extensions: &extensions,
                telemetry: None,
                spawned_child: false,
            };
            let first =
                construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
                    .await
                    .unwrap_or_else(|error| panic!("{provider:?}/{model}/{effort:?}: {error:?}"));
            let same =
                construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
                    .await
                    .unwrap();
            let changed =
                construct_platform_runtime(provider, &Credentials("fixture-key-two"), &spec, None)
                    .await
                    .unwrap();
            assert_eq!(first.target_binding(), same.target_binding());
            assert_ne!(
                first.target_binding(),
                changed.target_binding(),
                "key replacement must not inherit encrypted history"
            );
            assert_eq!(first.target_binding().model().as_str(), model);
        }
    }
}

struct FixtureWorkers(std::path::PathBuf);
impl plakous::tools::SidecarLocator for FixtureWorkers {
    fn javascript_host(&self) -> Result<std::path::PathBuf, String> {
        Ok(self.0.clone())
    }
    fn python_host(&self) -> Result<std::path::PathBuf, String> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn resumed_js_session_keeps_construction_jobs_after_policy_change() {
    use crate::agent::roots::doubles;
    use crate::agent::{ApiTopologyParent, ProductStateRoots};

    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("uninvoked-worker");
    std::fs::write(&worker, b"offline artifact identity fixture").unwrap();
    let roots = ProductStateRoots::new(
        directory.path().to_path_buf(),
        directory.path().join("runtime.sqlite3"),
        directory.path().join("state"),
        Arc::new(doubles::NoTelemetry),
        Arc::new(doubles::MemoryTreeStoreFactory),
        Arc::new(doubles::UnpinnedRosterSource),
        Arc::new(FixtureWorkers(worker)),
        Arc::new(doubles::NoOpenRouterCredential),
        Arc::new(Credentials("fixture-key-one")),
        Arc::new(crate::session_store::MemoryStore::new()),
    )
    .unwrap();
    let mut parent = ApiTopologyParent {
        prompt_host_facts: plakous_host_protocol::PromptHostFacts::default(),
        created_output_bound: None,
        workspace_root: directory.path().to_path_buf(),
        cwd: directory.path().to_path_buf(),
        plan_state_authority: "fixture-jobs".into(),
        composition: CompositionSelection::JsPtc,
        topology_tree_id: None,
        topology_caller_id: None,
        state_roots: roots,
        model: "gpt-5.6-luna".into(),
        effort: Some(ReasoningEffort::Low),
        target: crate::binding::ApiTarget::Platform {
            provider: PlatformProvider::OpenAi,
        },
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_enabled: false,
        product_home: Some(directory.path().to_path_buf()),
    };
    let original =
        crate::agent::build_api_root_coding_session(parent.clone(), "fixture:jobs:create", None)
            .await
            .unwrap();
    let original_plan = original.agent().unwrap().harness_plan().clone();
    assert_eq!(
        original
            .sessions()
            .created_job_config(original.locator())
            .await
            .unwrap(),
        plakous::tools::JobConfig::default(),
    );
    let locator = original.release().await.unwrap();

    parent.state_roots = parent
        .state_roots
        .with_job_config(plakous::tools::JobConfig::new(47, 3).unwrap());
    let resumed = crate::agent::open_api_root_coding_session(parent.clone(), locator, None)
        .await
        .unwrap_or_else(|error| panic!("resume: {}", error.failure.message()));
    assert_eq!(resumed.agent().unwrap().harness_plan(), &original_plan);
    assert_eq!(
        resumed
            .sessions()
            .created_job_config(resumed.locator())
            .await
            .unwrap(),
        plakous::tools::JobConfig::default(),
    );
    resumed.release().await.unwrap();

    parent.plan_state_authority = "fixture-new-jobs".into();
    let fresh = crate::agent::build_api_root_coding_session(parent, "fixture:jobs:new", None)
        .await
        .unwrap();
    assert_ne!(fresh.agent().unwrap().harness_plan(), &original_plan);
    assert_eq!(
        fresh
            .sessions()
            .created_job_config(fresh.locator())
            .await
            .unwrap(),
        plakous::tools::JobConfig::new(47, 3).unwrap(),
    );
    fresh.release().await.unwrap();
}

#[tokio::test]
async fn api_compositions_create_and_reopen_required_durable_sessions_without_dispatch() {
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
        OpenSessionRequest, SessionBackend, SessionOperationId,
    };
    for (provider, model) in [
        (PlatformProvider::OpenAi, "gpt-5.6-luna"),
        (PlatformProvider::Anthropic, "claude-sonnet-5"),
        (PlatformProvider::Xai, "grok-4.6"),
    ] {
        for composition in [
            CompositionSelection::Direct,
            CompositionSelection::JsPtc,
            CompositionSelection::PyPtc,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let worker = directory.path().join("uninvoked-worker");
            // Construction binds artifact bytes; worker execution is covered by runtime tests.
            std::fs::write(&worker, b"offline artifact identity fixture").unwrap();
            let workers = FixtureWorkers(worker);
            let extensions = crate::tools::ProductToolExtensions::default();
            let mut spec = ApiRuntimeSpec {
                job_config: plakous::tools::JobConfig::default(),
                account: provider.account(),
                sidecars: &workers,
                product_home: None,
                literal_model: model,
                effort: Some(ReasoningEffort::Low),
                composition,
                wire: provider.protocol(),
                workspace_root: directory.path(),
                cwd: directory.path(),
                plan_state_authority: "fixture-session",
                opening_durable_session: false,
                tool_fence: crate::agent::SessionToolFence::Full,
                scheduled_compaction: false,
                max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
                topology_extensions: &extensions,
                telemetry: None,
                spawned_child: false,
            };
            let sessions = AgentSessions::new(
                SessionBackend::sqlite(directory.path().join("sessions.sqlite3")).unwrap(),
            )
            .unwrap();
            let runtime =
                construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
                    .await
                    .unwrap();
            let created = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new("fixture:create").unwrap(),
                    runtime,
                ))
                .await
                .unwrap_or_else(|error| panic!("{provider:?}/{composition:?}: {error:?}"));
            let CreateSessionOutcome::Created { session, .. } = created else {
                panic!("new fixture did not create a session");
            };
            let locator = session.locator();
            session.release().await.unwrap();
            spec.opening_durable_session = true;
            let runtime =
                construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
                    .await
                    .unwrap();
            let reopened = sessions
                .open(OpenSessionRequest::new(
                    SessionOperationId::new("fixture:reopen").unwrap(),
                    locator,
                    runtime,
                ))
                .await
                .unwrap_or_else(|error| panic!("{provider:?}/{composition:?}: {error:?}"));
            let OpenSessionOutcome::Opened { session, .. } = reopened else {
                panic!("quiescent fixture did not reopen");
            };
            session.release().await.unwrap();
        }
    }
}

#[cfg(test)]
mod catalog_admission;
#[cfg(test)]
pub(crate) mod compaction;

#[test]
fn openai_api_code_mode_table_matches_the_codex_reference_catalog() {
    // OpenAI API rows the Codex catalog does not list, enrolled Direct on
    // review. Empty today: every enrolled API row is a Codex catalog row.
    const API_ONLY_DIRECT: &[&str] = &[];
    let reference: serde_json::Value = serde_json::from_str(include_str!(
        "../agent/fixtures/codex-reference-catalog.json"
    ))
    .unwrap();
    assert_eq!(
        reference["client_version"].as_str(),
        Some(plakous_openai::OPENAI_CODEX_REFERENCE_CLIENT_VERSION),
        "the table is reviewed against the pinned reference client's catalog"
    );
    for model in OPENAI_API_CODE_MODE_ONLY {
        assert!(
            platform_model(PlatformProvider::OpenAi, model).is_some(),
            "{model} is marked CodeModeOnly but is not an enrolled OpenAI API row"
        );
    }
    let rows = platform_models(PlatformProvider::OpenAi);
    assert!(!rows.is_empty());
    for row in rows {
        assert!(
            reference["models"].get(&row.model).is_some()
                || API_ONLY_DIRECT.contains(&row.model.as_str()),
            "{} is neither a reference catalog row nor reviewed as API-only Direct",
            row.model
        );
        let catalog = reference["models"][&row.model]["tool_mode"].as_str();
        let expected = match catalog {
            Some("code_mode_only") => ApiToolMode::CodeModeOnly,
            _ => ApiToolMode::Direct,
        };
        assert_eq!(row.tool_mode, expected, "{}", row.model);
    }
    for provider in [PlatformProvider::Anthropic, PlatformProvider::Xai] {
        assert!(
            platform_models(provider)
                .iter()
                .all(|row| row.tool_mode == ApiToolMode::Direct),
            "{provider:?}"
        );
    }
}

/// The OpenAI and xAI API routes configure exactly the tools (names, schemas
/// and descriptions) their OAuth vertical configures for the same model and
/// composition: the pack without the costume.
#[test]
fn api_routes_configure_their_oauth_pack_tools() {
    use plakous::subscriptions::NativeProvider;
    for (provider, model, native) in [
        (
            PlatformProvider::OpenAi,
            "gpt-5.6-luna",
            NativeProvider::Codex,
        ),
        (PlatformProvider::Xai, "grok-4.6", NativeProvider::Xai),
    ] {
        for composition in [
            CompositionSelection::Direct,
            CompositionSelection::NativePtc,
            CompositionSelection::JsPtc,
            CompositionSelection::PyPtc,
        ] {
            if composition == CompositionSelection::NativePtc
                && provider != PlatformProvider::OpenAi
            {
                assert!(api_composition(provider, model, composition).is_err());
                continue;
            }
            let composition = api_composition(provider, model, composition).unwrap();
            let native_ptc = composition == CompositionSelection::NativePtc;
            let registry = crate::agent::coding_tool_registry_with_extensions(
                "provider-selection",
                composition,
                native_ptc,
                &crate::tools::ProductToolExtensions::default(),
            )
            .unwrap();
            let mut api = crate::api_runtime::configured_tools(
                crate::coding_routes::platform_presentation(provider),
                composition,
                provider.protocol(),
                &registry,
                &[],
            )
            .unwrap()
            .into_iter()
            .map(|tool| tool.definition().clone())
            .collect::<Vec<_>>();
            api.sort_by(|left, right| left.name().cmp(right.name()));
            let oauth = crate::agent::selected_coding_provider_configuration(
                native,
                plakous::model::ModelId::new(model).unwrap(),
                ReasoningEffort::Low,
                native.enrolled_tool_mode(model),
                composition,
            )
            .unwrap()
            .behavior()
            .tools
            .iter()
            .map(|tool| tool.definition().clone())
            .collect::<Vec<_>>();
            assert!(!api.is_empty(), "{provider:?}/{composition:?}");
            assert_eq!(api, oauth, "{provider:?}/{composition:?}");
        }
    }
}

/// Code Mode on the OpenAI API needs the JavaScript host: with no sidecar the
/// construction fails naming it, before any session exists.
#[tokio::test]
async fn openai_api_native_ptc_without_the_javascript_host_names_the_sidecar() {
    let directory = tempfile::tempdir().unwrap();
    let extensions = crate::tools::ProductToolExtensions::default();
    let provider = PlatformProvider::OpenAi;
    let spec = ApiRuntimeSpec {
        job_config: plakous::tools::JobConfig::default(),
        account: provider.account(),
        sidecars: &NoWorkers,
        product_home: None,
        literal_model: "gpt-5.6-sol",
        effort: Some(ReasoningEffort::Low),
        composition: CompositionSelection::FollowCatalog,
        wire: provider.protocol(),
        workspace_root: directory.path(),
        cwd: directory.path(),
        plan_state_authority: "fixture-session",
        opening_durable_session: false,
        tool_fence: crate::agent::SessionToolFence::Full,
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: &extensions,
        telemetry: None,
        spawned_child: false,
    };
    let Err(error) =
        construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None).await
    else {
        panic!("Code Mode constructed without its JavaScript host");
    };
    let message = error.message();
    assert!(
        message.contains("Codex Code Mode sidecar is unavailable")
            && message.contains("PLAK_JS_HOST"),
        "{message}"
    );
}

#[tokio::test]
async fn openai_api_native_ptc_creates_and_reopens_from_the_enrolled_table() {
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
        OpenSessionRequest, SessionBackend, SessionOperationId,
    };
    for (provider, model, composition, admitted) in [
        (
            PlatformProvider::OpenAi,
            "gpt-5.6-luna",
            CompositionSelection::FollowCatalog,
            true,
        ),
        (
            PlatformProvider::OpenAi,
            "gpt-6-sol",
            CompositionSelection::NativePtc,
            true,
        ),
        (
            PlatformProvider::Anthropic,
            "claude-sonnet-5",
            CompositionSelection::NativePtc,
            false,
        ),
        (
            PlatformProvider::Xai,
            "grok-4.6",
            CompositionSelection::NativePtc,
            false,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("uninvoked-worker");
        std::fs::write(&worker, b"offline artifact identity fixture").unwrap();
        let workers = FixtureWorkers(worker);
        let extensions = crate::tools::ProductToolExtensions::default();
        let mut spec = ApiRuntimeSpec {
            job_config: plakous::tools::JobConfig::default(),
            account: provider.account(),
            sidecars: &workers,
            product_home: None,
            literal_model: model,
            effort: Some(ReasoningEffort::Low),
            composition,
            wire: provider.protocol(),
            workspace_root: directory.path(),
            cwd: directory.path(),
            plan_state_authority: "fixture-session",
            opening_durable_session: false,
            tool_fence: crate::agent::SessionToolFence::Full,
            scheduled_compaction: false,
            max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            topology_extensions: &extensions,
            telemetry: None,
            spawned_child: false,
        };
        let runtime =
            construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
                .await;
        if !admitted {
            assert!(runtime.is_err(), "{provider:?}/{composition:?}");
            continue;
        }
        let runtime = runtime.unwrap_or_else(|error| panic!("{provider:?}/{model}: {error:?}"));
        let sessions = AgentSessions::new(
            SessionBackend::sqlite(directory.path().join("sessions.sqlite3")).unwrap(),
        )
        .unwrap();
        let created = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("fixture:create").unwrap(),
                runtime,
            ))
            .await
            .unwrap_or_else(|error| panic!("{model}: {error:?}"));
        let CreateSessionOutcome::Created { session, .. } = created else {
            panic!("new fixture did not create a session");
        };
        let locator = session.locator();
        session.release().await.unwrap();
        spec.opening_durable_session = true;
        let runtime =
            construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
                .await
                .unwrap();
        let reopened = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new("fixture:reopen").unwrap(),
                locator,
                runtime,
            ))
            .await
            .unwrap_or_else(|error| panic!("{model}: {error:?}"));
        let OpenSessionOutcome::Opened { session, .. } = reopened else {
            panic!("quiescent fixture did not reopen");
        };
        session.release().await.unwrap();
    }
}

/// An OpenAI API Direct session bound under another pack (the generic pi
/// tools, projection and base under the unchanged
/// `plak.openai.responses.direct.v1` profile id, as before the API routes wore
/// the Codex pack) does not reopen under the Codex-pack runtime: the reopen
/// fails closed, never re-presenting the session. Product state never reaches this reopen, because the pack change
/// rides a runtime-format epoch bump (spec 02 §3.1); this pins the library
/// fence beneath that cutover.
#[tokio::test]
async fn pre_pack_openai_api_direct_session_reopen_fails_closed() {
    use plakous::agent::{
        AgentBuilder, AgentConfig, AgentRequestPolicy, CachePreferenceStrategy,
        CapabilityRequirement, ContinuationPreferenceStrategy, HarnessProfileProvenance,
        HarnessProfileSpecBuilder, InstructionCompositionStrategy, ProfileSemanticVersion,
        ReasoningPolicy, ReasoningPreferenceStrategy, ToolExposureStrategy,
    };
    use plakous::sessions::{
        AgentSessions, AgentSessionsErrorKind, CreateSessionOutcome, CreateSessionRequest,
        OpenSessionRequest, SessionBackend, SessionOperationId,
    };
    use plakous::tools::CodingPresentationExt as _;

    #[derive(Debug)]
    struct AdmitPrePack;
    impl plakous::tools::ProjectionCandidateAdmission for AdmitPrePack {
        fn admit(
            &self,
            _job_config: plakous::tools::JobConfig,
            _projection: &plakous::tools::ToolProjectionMapV2,
            _registry: &plakous::tools::FrozenToolRegistry,
            _configuration: &plakous::model::CompiledProviderConfiguration,
        ) -> Result<(), plakous::tools::ToolRuntimeError> {
            Ok(())
        }
    }

    let provider = PlatformProvider::OpenAi;
    let model = "gpt-5.6-luna";
    let directory = tempfile::tempdir().unwrap();
    let extensions = crate::tools::ProductToolExtensions::default();
    let mut spec = ApiRuntimeSpec {
        job_config: plakous::tools::JobConfig::default(),
        account: provider.account(),
        sidecars: &NoWorkers,
        product_home: None,
        literal_model: model,
        effort: Some(ReasoningEffort::Low),
        composition: CompositionSelection::Direct,
        wire: provider.protocol(),
        workspace_root: directory.path(),
        cwd: directory.path(),
        plan_state_authority: "fixture-pre-pack",
        opening_durable_session: false,
        tool_fence: crate::agent::SessionToolFence::Full,
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: &extensions,
        telemetry: None,
        spawned_child: false,
    };

    // The runtime another build bound for this record: the generic vocabulary.
    let other = plakous::tools::CodingPresentation::Pi;
    let registry = crate::agent::coding_tool_registry_with_extensions(
        spec.plan_state_authority,
        CompositionSelection::Direct,
        false,
        &extensions,
    )
    .unwrap();
    let row = platform_model(provider, model).unwrap();
    let behavior = platform_behavior(
        provider,
        ReasoningEffort::Low,
        &row,
        false,
        crate::api_runtime::configured_tools(
            other,
            CompositionSelection::Direct,
            spec.wire,
            &registry,
            &[],
        )
        .unwrap(),
    )
    .unwrap();
    let maximum = NonZeroU64::new(row.window(ApiWindow::Working)).unwrap();
    let configured: ConfiguredLanguageModel = {
        use plakous::openai::{
            CredentialReference, OpenAiPlatformModel, OpenAiPlatformModelBuilder,
            OpenAiPlatformModelFacts, PlatformProjectPolicy, PlatformStoragePolicy,
        };
        let authority = OpenAiPlatformModelBuilder::new(
            CredentialReference::application("plak/openai").unwrap(),
            Credentials("fixture-key-one").openai().unwrap(),
            OpenAiPlatformModelFacts::from_model_id(model).unwrap(),
            PlatformStoragePolicy::Stateless,
            PlatformProjectPolicy::DefaultProject,
        )
        .context_envelope(maximum)
        .seal()
        .unwrap();
        let provisional = authority.selected_configuration(behavior).unwrap();
        let selected = authority
            .selected_configuration(exact_route_retention(&provisional))
            .unwrap();
        Arc::new(OpenAiPlatformModel::with_exchange_observer(authority, selected, None).unwrap())
            .configured()
            .await
            .unwrap()
    };
    let configuration = configured.configuration().clone();
    let physical = std::fs::canonicalize(directory.path()).unwrap();
    let workspace = Arc::new(
        crate::agent::stamped_workspace_policy(&physical, std::path::Path::new("")).unwrap(),
    );
    let target = crate::agent::projection_target(&configuration, "pre-pack fixture").unwrap();
    let surface = crate::tools::product_surface_with_fence(
        spec.job_config,
        other,
        registry.clone(),
        target.clone(),
        &extensions,
        crate::agent::SessionToolFence::Full,
        None,
    )
    .unwrap();
    let full = other
        .projection_map_with_extensions(spec.job_config, registry.clone(), target.clone(), &[])
        .unwrap();
    let omitted = full
        .entries()
        .iter()
        .filter(|entry| !surface.exposed_names().contains(entry.exposed_name()))
        .map(|entry| entry.exposed_name().as_str())
        .collect::<Vec<_>>();
    let projection = if omitted.is_empty() {
        full.clone()
    } else {
        other
            .projection_map_with_extensions_omitting(
                spec.job_config,
                registry.clone(),
                target,
                &[],
                &omitted,
            )
            .unwrap()
    };
    let instructions = plakous::prompts::plan::target_instructions(
        &plakous::prompts::bases::pi_base_instructions(surface.exposed_names()),
        crate::prompt_inputs::prompt_composition(CompositionSelection::Direct),
        &surface,
        workspace.root(),
        workspace.cwd(),
        model,
        Some(plakous::prompts::identity::product_effort_label(
            ReasoningEffort::Low,
        ))
        .as_deref(),
    );
    let profile = HarnessProfileSpecBuilder::new()
        .id(
            crate::agent::subagent_catalog_profile_id("plak.openai.responses.direct.v1", false)
                .unwrap(),
        )
        .semantic_version(ProfileSemanticVersion::new(0, 1, 0))
        .provenance(HarnessProfileProvenance::Operator)
        .base_instructions(instructions)
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(ToolExposureStrategy::None)
        .reasoning_strategy(ReasoningPreferenceStrategy::PreferEffort)
        .preferred_reasoning_effort(Some(ReasoningEffort::Low))
        .continuation_strategy(ContinuationPreferenceStrategy::CanonicalReplayOnly)
        .cache_strategy(CachePreferenceStrategy::Disabled)
        .build()
        .unwrap();
    let (profile, context_sources) = crate::agent::enroll_product_instructions(
        &profile,
        None,
        workspace.root(),
        workspace.cwd(),
    )
    .unwrap();
    let plan = plakous::agent::compile_explicit_runtime_profile(
        &profile,
        configured.model().as_ref(),
        &configuration,
        &registry,
        workspace.as_ref(),
        context_sources,
    )
    .unwrap();
    let limits =
        crate::api_runtime::behavior_generation_limits(configuration.effective().behavior())
            .unwrap();
    let request = AgentRequestPolicy::new(
        limits,
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: CapabilityRequirement::Required,
        },
    )
    .with_context_admission_requirement(plakous::model::ContextAdmissionRequirement::AnyBounded);
    let config = AgentConfig::default()
        .with_max_model_rounds(spec.max_model_rounds)
        .with_model_cleanup_timeout(crate::agent::NATIVE_RUN_MODEL_CLEANUP_TIMEOUT)
        .with_model_operation_timeout(crate::agent::NATIVE_RUN_MODEL_OPERATION_TIMEOUT)
        .with_event_capacity(4_096)
        .with_event_byte_capacity(16 * 1024 * 1024)
        .with_scheduled_compaction_enabled(false);
    let builder = AgentBuilder::new(configured, registry.clone(), Arc::clone(&workspace))
        .projection_admission(Arc::new(AdmitPrePack))
        .post_compaction_note(Arc::new(plakous::tools::plan_restoration_note))
        .tool_policy(crate::agent::session_tool_policy(
            crate::agent::SessionToolFence::Full,
        ))
        .host_process_options(crate::process_environment::options(&physical).unwrap())
        .job_config(spec.job_config)
        .tool_projection_v2(projection)
        .harness_plan(plan);
    let pre_pack = plakous::sessions::SessionRuntimeBinding::new(
        builder.config(config).request_policy(request),
    );

    let sessions = AgentSessions::new(
        SessionBackend::sqlite(directory.path().join("sessions.sqlite3")).unwrap(),
    )
    .unwrap();
    let created = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("fixture:pre-pack:create").unwrap(),
            pre_pack,
        ))
        .await
        .unwrap_or_else(|error| panic!("pre-pack fixture did not create: {error:?}"));
    let CreateSessionOutcome::Created { session, .. } = created else {
        panic!("new fixture did not create a session");
    };
    let locator = session.locator();
    session.release().await.unwrap();

    // This build reopens the same record (composition Direct) on its own runtime.
    spec.opening_durable_session = true;
    let runtime =
        construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
            .await
            .unwrap();
    let error = match sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("fixture:pre-pack:reopen").unwrap(),
            locator,
            runtime,
        ))
        .await
    {
        Ok(_) => panic!("a pre-pack OpenAI API session reopened under the Codex pack"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind(),
        AgentSessionsErrorKind::Revalidation,
        "pre-pack reopen must be refused at harness revalidation: {error:?}"
    );
}

/// A Grok session recorded while `wait_agent` and `interrupt_agent` were still
/// presented (the legacy hub, under the unchanged
/// `plak.xai-api.responses.direct.v1` profile id) does not reopen on this
/// build. The live host binds the claimed output and kill tools and no longer
/// binds the retired executors, so reopen fails with
/// `ExternalBindingUnavailable` (surfaced as an internal session error) instead
/// of re-presenting the session under `get_command_or_subagent_output` /
/// `kill_command_or_subagent`. Creating that projection on the live Direct host
/// is also refused at harness admission.
#[tokio::test]
async fn pre_w3_b_grok_session_reopen_fails_closed() {
    use plakous::agent::{
        AgentBuilder, AgentConfig, AgentRequestPolicy, CachePreferenceStrategy,
        CapabilityRequirement, ContinuationPreferenceStrategy, HarnessProfileProvenance,
        HarnessProfileSpecBuilder, InstructionCompositionStrategy, ProfileSemanticVersion,
        ReasoningPolicy, ReasoningPreferenceStrategy, ToolExposureStrategy,
    };
    use plakous::sessions::{
        AgentSessions, AgentSessionsErrorKind, CreateSessionOutcome, CreateSessionRequest,
        OpenSessionRequest, SessionBackend, SessionOperationId,
    };
    use plakous::tools::CodingPresentationExt as _;

    #[derive(Debug)]
    struct AdmitPrePack;
    impl plakous::tools::ProjectionCandidateAdmission for AdmitPrePack {
        fn admit(
            &self,
            _job_config: plakous::tools::JobConfig,
            _projection: &plakous::tools::ToolProjectionMapV2,
            _registry: &plakous::tools::FrozenToolRegistry,
            _configuration: &plakous::model::CompiledProviderConfiguration,
        ) -> Result<(), plakous::tools::ToolRuntimeError> {
            Ok(())
        }
    }

    let provider = PlatformProvider::Xai;
    let model = "grok-4.6";
    let directory = tempfile::tempdir().unwrap();
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        "tree-pre-w3b-grok",
        "root",
        "owner",
        directory.path().to_path_buf(),
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let extensions = crate::topology_extensions::topology_tool_extensions_in(
        Arc::new(std::sync::Mutex::new(tree)),
        "root",
        crate::agent_tree_tools::TopologyVocabulary::Plak,
    )
    .unwrap();
    let mut spec = ApiRuntimeSpec {
        job_config: plakous::tools::JobConfig::default(),
        account: provider.account(),
        sidecars: &NoWorkers,
        product_home: None,
        literal_model: model,
        effort: Some(ReasoningEffort::Low),
        composition: CompositionSelection::Direct,
        wire: provider.protocol(),
        workspace_root: directory.path(),
        cwd: directory.path(),
        plan_state_authority: "fixture-pre-w3b-grok",
        opening_durable_session: false,
        tool_fence: crate::agent::SessionToolFence::Full,
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: &extensions,
        telemetry: None,
        spawned_child: false,
    };

    let presentation = plakous::tools::CodingPresentation::Grok;
    let registry = crate::agent::coding_tool_registry_with_extensions(
        spec.plan_state_authority,
        CompositionSelection::Direct,
        false,
        &extensions,
    )
    .unwrap();
    let row = platform_model(provider, model).unwrap();
    let extra: Vec<&str> = extensions.names().map(ToolName::as_str).collect();
    let behavior = platform_behavior(
        provider,
        ReasoningEffort::Low,
        &row,
        false,
        crate::api_runtime::configured_tools(
            presentation,
            CompositionSelection::Direct,
            spec.wire,
            &registry,
            &extra,
        )
        .unwrap(),
    )
    .unwrap();
    let maximum = NonZeroU64::new(row.window(ApiWindow::Working)).unwrap();
    let configured: ConfiguredLanguageModel = {
        use plakous::xai::{XaiPlatformModel, xai_platform_selection};
        let source = Credentials("fixture-key-one").xai().unwrap();
        let provisional = xai_platform_selection(behavior).unwrap();
        let selected = xai_platform_selection(exact_route_retention(&provisional)).unwrap();
        let authority = source.grant().unwrap().authority().clone();
        Arc::new(
            XaiPlatformModel::with_exchange_observer(
                authority,
                source,
                maximum,
                selected,
                Arc::new(plakous::model::SystemClock),
                None,
            )
            .unwrap(),
        )
        .configured()
        .await
        .unwrap()
    };
    let configuration = configured.configuration().clone();
    let physical = std::fs::canonicalize(directory.path()).unwrap();
    let workspace = Arc::new(
        crate::agent::stamped_workspace_policy(&physical, std::path::Path::new("")).unwrap(),
    );
    let target = crate::agent::projection_target(&configuration, "legacy Grok fixture").unwrap();
    let surface = crate::tools::product_surface_with_fence(
        spec.job_config,
        presentation,
        registry.clone(),
        target.clone(),
        &extensions,
        crate::agent::SessionToolFence::Full,
        None,
    )
    .unwrap();
    let extra_names = extensions.names().cloned().collect::<Vec<_>>();
    let full = presentation
        .projection_map_with_extensions(
            spec.job_config,
            registry.clone(),
            target.clone(),
            &extra_names,
        )
        .unwrap();
    let omitted = full
        .entries()
        .iter()
        .filter(|entry| !surface.exposed_names().contains(entry.exposed_name()))
        .map(|entry| entry.exposed_name().as_str())
        .collect::<Vec<_>>();
    let projection = if omitted.is_empty() {
        full.clone()
    } else {
        presentation
            .projection_map_with_extensions_omitting(
                spec.job_config,
                registry.clone(),
                target,
                &extra_names,
                &omitted,
            )
            .unwrap()
    };
    assert!(
        projection
            .entries()
            .iter()
            .any(|entry| entry.exposed_name().as_str() == "wait_agent"),
        "legacy fixture must still present wait_agent"
    );
    let base = plakous::profiles::grok::recommended_for(crate::agent::product_prompt_variant())
        .unwrap()
        .base_instructions()
        .to_owned();
    let instructions = plakous::prompts::plan::target_instructions(
        &base,
        crate::prompt_inputs::prompt_composition(CompositionSelection::Direct),
        &surface,
        workspace.root(),
        workspace.cwd(),
        model,
        Some(plakous::prompts::identity::product_effort_label(
            ReasoningEffort::Low,
        ))
        .as_deref(),
    );
    let profile = HarnessProfileSpecBuilder::new()
        .id(
            crate::agent::subagent_catalog_profile_id("plak.xai-api.responses.direct.v1", true)
                .unwrap(),
        )
        .semantic_version(ProfileSemanticVersion::new(0, 1, 0))
        .provenance(HarnessProfileProvenance::Operator)
        .base_instructions(instructions)
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(ToolExposureStrategy::None)
        .reasoning_strategy(ReasoningPreferenceStrategy::PreferEffort)
        .preferred_reasoning_effort(Some(ReasoningEffort::Low))
        .continuation_strategy(ContinuationPreferenceStrategy::CanonicalReplayOnly)
        .cache_strategy(CachePreferenceStrategy::Disabled)
        .build()
        .unwrap();
    let (profile, context_sources) = crate::agent::enroll_product_instructions(
        &profile,
        None,
        workspace.root(),
        workspace.cwd(),
    )
    .unwrap();
    let plan = plakous::agent::compile_explicit_runtime_profile(
        &profile,
        configured.model().as_ref(),
        &configuration,
        &registry,
        workspace.as_ref(),
        context_sources,
    )
    .unwrap();
    let limits =
        crate::api_runtime::behavior_generation_limits(configuration.effective().behavior())
            .unwrap();
    let request = AgentRequestPolicy::new(
        limits,
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: CapabilityRequirement::Required,
        },
    )
    .with_context_admission_requirement(plakous::model::ContextAdmissionRequirement::AnyBounded);
    let config = AgentConfig::default()
        .with_max_model_rounds(spec.max_model_rounds)
        .with_model_cleanup_timeout(crate::agent::NATIVE_RUN_MODEL_CLEANUP_TIMEOUT)
        .with_model_operation_timeout(crate::agent::NATIVE_RUN_MODEL_OPERATION_TIMEOUT)
        .with_event_capacity(4_096)
        .with_event_byte_capacity(16 * 1024 * 1024)
        .with_scheduled_compaction_enabled(false);
    let builder = AgentBuilder::new(configured, registry.clone(), Arc::clone(&workspace))
        .projection_admission(Arc::new(AdmitPrePack))
        .post_compaction_note(Arc::new(plakous::tools::plan_restoration_note))
        .tool_policy(crate::agent::session_tool_policy(
            crate::agent::SessionToolFence::Full,
        ))
        .host_process_options(crate::process_environment::options(&physical).unwrap())
        .job_config(spec.job_config)
        .tool_projection_v2(projection)
        .harness_plan(plan);
    let pre_w3_b = plakous::sessions::SessionRuntimeBinding::new(
        builder.config(config).request_policy(request),
    );

    let sessions = AgentSessions::new(
        SessionBackend::sqlite(directory.path().join("sessions.sqlite3")).unwrap(),
    )
    .unwrap();
    let created = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("fixture:pre-w3b:create").unwrap(),
            pre_w3_b,
        ))
        .await
        .unwrap_or_else(|error| panic!("legacy fixture did not create: {error:?}"));
    let CreateSessionOutcome::Created { session, .. } = created else {
        panic!("new fixture did not create a session");
    };
    let locator = session.locator();
    session.release().await.unwrap();

    // Creating that projection on the live Direct host is refused at harness
    // admission. The historical record above bypasses admission so the reopen
    // below has a legacy session to refuse.
    let admitted =
        construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
            .await
            .unwrap();
    let create_error = match sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("fixture:pre-w3b:admit").unwrap(),
            admitted,
        ))
        .await
    {
        Ok(_) => panic!("a Grok Direct create presenting wait_agent was admitted"),
        Err(error) => error,
    };
    assert_eq!(
        create_error.kind(),
        AgentSessionsErrorKind::Revalidation,
        "Grok Direct create must be refused at harness admission: {create_error:?}"
    );
    assert!(
        create_error
            .to_string()
            .contains("wait_agent or interrupt_agent"),
        "admission refusal must name the retired presentation: {create_error}"
    );

    let live_tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        "tree-pre-w3b-grok",
        "root",
        "owner",
        directory.path().to_path_buf(),
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let live_extensions = crate::topology_extensions::topology_tool_extensions_in(
        Arc::new(std::sync::Mutex::new(live_tree)),
        "root",
        crate::agent_tree_tools::TopologyVocabulary::TaskTools,
    )
    .unwrap();
    spec.topology_extensions = &live_extensions;
    spec.opening_durable_session = true;
    let runtime =
        construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
            .await
            .unwrap();
    let error = match sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("fixture:pre-w3b:reopen").unwrap(),
            locator,
            runtime,
        ))
        .await
    {
        Ok(_) => panic!("a legacy Grok session reopened under the claimed vocabulary"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind(),
        AgentSessionsErrorKind::Internal,
        "legacy reopen is ExternalBindingUnavailable, surfaced as internal: {error}"
    );
    assert!(
        error
            .to_string()
            .contains("a required tool executor binding is unavailable: plak.topology."),
        "refusal must be the unbound retired executor, not a re-presentation: {error}"
    );
}

/// Creates a Grok hub on the xAI API (topology output and kill claim the Grok
/// process slots), releases it and reopens it through the real projection
/// admission and `AgentSessions`. Reopen resolves the registry from its
/// durable descriptor, so the claim must be re-taken from the live host.
async fn grok_api_hub_creates_and_reopens(
    composition: CompositionSelection,
    spawned_child: bool,
) -> Result<(), String> {
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
        OpenSessionRequest, SessionBackend, SessionOperationId,
    };
    let provider = PlatformProvider::Xai;
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("uninvoked-worker");
    std::fs::write(&worker, b"offline artifact identity fixture").unwrap();
    let workers = FixtureWorkers(worker);
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        "tree-grok-hub",
        "root",
        "owner",
        directory.path().to_path_buf(),
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let extensions = crate::topology_extensions::topology_tool_extensions_in(
        Arc::new(std::sync::Mutex::new(tree)),
        "root",
        crate::agent_tree_tools::TopologyVocabulary::TaskTools,
    )
    .unwrap();
    let mut spec = ApiRuntimeSpec {
        job_config: plakous::tools::JobConfig::default(),
        account: provider.account(),
        sidecars: &workers,
        product_home: None,
        literal_model: "grok-4.6",
        effort: Some(ReasoningEffort::Low),
        composition,
        wire: provider.protocol(),
        workspace_root: directory.path(),
        cwd: directory.path(),
        plan_state_authority: "fixture-grok-hub",
        opening_durable_session: false,
        tool_fence: crate::agent::SessionToolFence::Full,
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: &extensions,
        telemetry: None,
        spawned_child,
    };
    let sessions = AgentSessions::new(
        SessionBackend::sqlite(directory.path().join("sessions.sqlite3")).unwrap(),
    )
    .unwrap();
    let runtime =
        construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
            .await
            .map_err(|error| format!("construct: {error:?}"))?;
    let created = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("fixture:create").unwrap(),
            runtime,
        ))
        .await
        .map_err(|error| format!("create: {error}"))?;
    let CreateSessionOutcome::Created { session, .. } = created else {
        return Err("create: not created".into());
    };
    let locator = session.locator();
    session.release().await.unwrap();
    spec.opening_durable_session = true;
    let runtime =
        construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
            .await
            .map_err(|error| format!("reconstruct: {error:?}"))?;
    match sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("fixture:reopen").unwrap(),
            locator,
            runtime,
        ))
        .await
    {
        Ok(OpenSessionOutcome::Opened { session, .. }) => {
            session.release().await.unwrap();
            Ok(())
        }
        Ok(_) => Err("reopen: not opened".into()),
        Err(error) => Err(format!("reopen: {error}")),
    }
}

/// A Grok API hub creates and reopens on every composition, as a root and as
/// a spawned child, and its Direct surface presents each claimed slot once,
/// answered by the topology capability's own text.
#[tokio::test]
async fn grok_api_hub_with_claimed_slots_creates_and_reopens_on_every_composition() {
    use plakous::tools::CodingPresentationExt as _;
    let mut failures = Vec::new();
    for composition in [
        CompositionSelection::Direct,
        CompositionSelection::JsPtc,
        CompositionSelection::PyPtc,
    ] {
        for spawned_child in [false, true] {
            if let Err(error) = grok_api_hub_creates_and_reopens(composition, spawned_child).await {
                failures.push(format!("{composition:?} child={spawned_child} {error}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");

    let directory = tempfile::tempdir().unwrap();
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        "tree-grok-hub-surface",
        "root",
        "owner",
        directory.path().to_path_buf(),
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let extensions = crate::topology_extensions::topology_tool_extensions_in(
        Arc::new(std::sync::Mutex::new(tree)),
        "root",
        crate::agent_tree_tools::TopologyVocabulary::TaskTools,
    )
    .unwrap();
    let registry = crate::agent::coding_tool_registry_with_extensions(
        "fixture-grok-hub-surface",
        CompositionSelection::Direct,
        false,
        &extensions,
    )
    .unwrap();
    let live = plakous::tools::FrozenToolRegistryDescriptor::capture(&registry)
        .unwrap()
        .resolve(&plakous::tools::ToolExecutorBindings::capture(&registry))
        .unwrap();
    let target = plakous::tools::ProviderModelProjectionIdentityV2::new(
        plakous::model::IntegrationClass::Standard,
        "xai.grok.responses",
        "xai.grok_subscription",
        "xai.grok.compat.test",
        "responses-family",
        "grok-4.6",
        "grok-4.6",
        serde_json::from_value(
            serde_json::to_value(
                plakous::model::Digest::sha256(
                    plakous::model::DigestDomain::EffectiveProviderConfiguration,
                    5,
                    &[b"grok-hub-surface"],
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap(),
        plakous::tools::PROCESS_TOOL_PROJECTION_REVISION,
    )
    .unwrap();
    let names = extensions.names().cloned().collect::<Vec<_>>();
    let projection = plakous::tools::CodingPresentation::Grok
        .projection_map_with_extensions(plakous::tools::JobConfig::default(), live, target, &names)
        .unwrap();
    for claimed in ["get_command_or_subagent_output", "kill_command_or_subagent"] {
        let entries = projection
            .entries()
            .iter()
            .filter(|entry| entry.exposed_name().as_str() == claimed)
            .collect::<Vec<_>>();
        let [entry] = entries.as_slice() else {
            panic!("{claimed} presented {} times", entries.len());
        };
        assert_eq!(
            format!("{:?}", entry.raising()),
            "ModelContentIdentityV1",
            "{claimed}"
        );
    }
    assert!(projection.entries().iter().all(|entry| !matches!(
        entry.exposed_name().as_str(),
        "wait_agent" | "interrupt_agent"
    )));
}

/// The js-/py-ptc twin of [`pre_w3_b_grok_session_reopen_fails_closed`]. A
/// legacy Grok hub under a program runtime presented `wait_agent` and
/// `interrupt_agent` in its nested catalog, so the outer projection guard
/// does not see them; reopen on this build is refused because the live host
/// registers the claimed output and kill tools instead and no longer binds
/// the retired executors (`ExternalBindingUnavailable`, surfaced as an
/// internal session error). It is never re-presented.
#[tokio::test]
async fn pre_w3_b_grok_ptc_session_reopen_fails_closed() {
    use plakous::sessions::{
        AgentSessions, AgentSessionsErrorKind, CreateSessionOutcome, CreateSessionRequest,
        OpenSessionRequest, SessionBackend, SessionOperationId,
    };
    let provider = PlatformProvider::Xai;
    for composition in [CompositionSelection::JsPtc, CompositionSelection::PyPtc] {
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("uninvoked-worker");
        std::fs::write(&worker, b"offline artifact identity fixture").unwrap();
        let workers = FixtureWorkers(worker);
        let extensions_for = |vocabulary| {
            let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
                "tree-pre-w3b-ptc",
                "root",
                "owner",
                directory.path().to_path_buf(),
                "selected-tuple",
                crate::agent_tree_protocol::WriterMode::SingleLease,
            )
            .unwrap();
            crate::topology_extensions::topology_tool_extensions_in(
                Arc::new(std::sync::Mutex::new(tree)),
                "root",
                vocabulary,
            )
            .unwrap()
        };
        let plak = extensions_for(crate::agent_tree_tools::TopologyVocabulary::Plak);
        let task = extensions_for(crate::agent_tree_tools::TopologyVocabulary::TaskTools);
        let mut spec = ApiRuntimeSpec {
            job_config: plakous::tools::JobConfig::default(),
            account: provider.account(),
            sidecars: &workers,
            product_home: None,
            literal_model: "grok-4.6",
            effort: Some(ReasoningEffort::Low),
            composition,
            wire: provider.protocol(),
            workspace_root: directory.path(),
            cwd: directory.path(),
            plan_state_authority: "fixture-pre-w3b-ptc",
            opening_durable_session: false,
            tool_fence: crate::agent::SessionToolFence::Full,
            scheduled_compaction: false,
            max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            topology_extensions: &plak,
            telemetry: None,
            spawned_child: false,
        };
        let sessions = AgentSessions::new(
            SessionBackend::sqlite(directory.path().join("sessions.sqlite3")).unwrap(),
        )
        .unwrap();
        let runtime =
            construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
                .await
                .unwrap();
        let created = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("fixture:pre-w3b-ptc:create").unwrap(),
                runtime,
            ))
            .await
            .unwrap_or_else(|error| panic!("{composition:?} legacy fixture: {error}"));
        let CreateSessionOutcome::Created { session, .. } = created else {
            panic!("new fixture did not create a session");
        };
        let locator = session.locator();
        session.release().await.unwrap();
        spec.opening_durable_session = true;
        spec.topology_extensions = &task;
        let runtime =
            construct_platform_runtime(provider, &Credentials("fixture-key-one"), &spec, None)
                .await
                .unwrap();
        let error = match sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new("fixture:pre-w3b-ptc:reopen").unwrap(),
                locator,
                runtime,
            ))
            .await
        {
            Ok(_) => panic!("{composition:?}: a legacy Grok hub reopened"),
            Err(error) => error,
        };
        assert_eq!(
            error.kind(),
            AgentSessionsErrorKind::Internal,
            "{composition:?}: unbound retired executors surface as internal: {error}"
        );
        assert!(
            error
                .to_string()
                .contains("a required tool executor binding is unavailable: plak.topology."),
            "{composition:?}: {error}"
        );
    }
}
