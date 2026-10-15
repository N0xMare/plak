//! Construction-time harness-plan binding and two-plan actor behavior.

use std::collections::BTreeMap;
use std::sync::Arc;

use plakous_agent::internal::{
    AgentBuilder, CachePreferenceStrategy, ContinuationPreferenceStrategy, HarnessProfileCompiler,
    HarnessProfileId, HarnessProfileProvenance, HarnessProfileSpecBuilder,
    InstructionCompositionStrategy, ProfileSemanticVersion, ReasoningPreferenceStrategy,
    ToolExposureOverride, ToolExposureStrategy, compile_explicit_profile,
    compiler_model_facts_from_model,
};
use plakous_model::{
    CachePolicy, Capability, CapabilityConstraints, CapabilitySet, CompiledProviderConfiguration,
    ConfiguredLanguageModel, FactProvenance, FactSource, FeatureSupport, LanguageModel,
    ModelAdapterProfileId, ModelFamilyId, ModelId, ModelPreparationContext, ModelPreparationError,
    ModelPreparationErrorKind, ModelStream, OutputBoundDisposition, PreparedCacheUse,
    PreparedCallRecord, PreparedModelCall, PreparedModelProjection, PreparedTokenProjection,
    ProtocolId, ResolvedModelDescriptor, RouteId, RuntimeModelAuthority,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, Support, Timestamp,
    ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
    ValidatedRequestPlan,
};
use plakous_tools::{
    FrozenToolRegistry, RegisteredToolId, ToolExecutorId, ToolOutcome, ToolRegistration,
    ToolRegistryBuilder, WorkspacePolicy,
};
use serde_json::json;

use crate::support::{self, Fixture, TestResult, workspace};

const FIXTURE: Fixture = Fixture {
    label: "harness-profile",
    provider_semantics: b"harness-profile-semantics-v1",
};

fn descriptor_with_route(route: &str) -> TestResult<ResolvedModelDescriptor> {
    let provenance = FactProvenance::new(FactSource::BuiltIn, Timestamp::new(1, 0)?, None);
    let mut features = BTreeMap::new();
    features.insert(
        Capability::Tools,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    );
    features.insert(
        Capability::Continuation,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    );
    features.insert(
        Capability::Reasoning,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    );
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test.protocol")?,
        RouteId::new(route)?,
        ModelId::new("test.model")?,
        None,
        Some(ModelFamilyId::new("test.harness-profile")?),
        ModelAdapterProfileId::new("test.adapter")?,
        CapabilitySet::new(features, BTreeMap::new()),
    ))
}

#[derive(Debug)]
struct SilentModel {
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
}

impl SilentModel {
    fn new(route: &str) -> TestResult<Self> {
        let descriptor = descriptor_with_route(route)?;
        let account = plakous_model::Digest::sha256(
            plakous_model::DigestDomain::ModelBinding,
            1,
            &[b"silent-model-account"],
        )?;
        let credential = plakous_model::Digest::sha256(
            plakous_model::DigestDomain::ModelBinding,
            1,
            &[b"silent-model-credential"],
        )?;
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            account,
            credential,
            1,
            Timestamp::new(1, 0)?,
            None,
        )?;
        Ok(Self {
            descriptor,
            authority,
        })
    }

    fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }
}

impl LanguageModel for SilentModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        let authority = self.authority.clone();
        let result = authority
            .validate(&self.descriptor, checked_at)
            .map(|()| authority);
        Box::pin(async move { result })
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.descriptor.validate_plan_binding(&plan)?;
        context
            .authority()
            .validate(&self.descriptor, context.prepared_at())?;
        if !self.authority.same_selected_authority(context.authority()) {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        let wire = serde_json::to_vec(&(plan.request().model_input_identity(), plan.decisions()))
            .map_err(|_| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::Projection,
                "silent test-model wire serialization failed",
            )
        })?;
        let wire_size = u64::try_from(wire.len()).map_err(|_| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::ResourceLimit,
                "silent test-model wire exceeds addressable size",
            )
        })?;
        let wire_digest = plakous_model::Digest::sha256(
            plakous_model::DigestDomain::Blob,
            1,
            &[b"plakous-agent/silent-model-wire-v1", &wire],
        )
        .map_err(plakous_model::PreparedCallError::from)?;
        let token_projection = PreparedTokenProjection::new(wire_digest, wire_size, None)?;
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            self.descriptor.protocol().clone(),
            "silent-model-lowering-v1",
            "silent-model-projection-v1",
            "silent-model-accounting-v1",
            wire_digest,
            wire_size,
            token_projection,
            PreparedCacheUse::None,
            None,
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
        let prepared_wire: Arc<[u8]> = wire.into();
        Ok(PreparedModelCall::new(record, move |_call_context| {
            Box::pin(async move {
                drop(prepared_wire);
                Ok(Box::pin(futures_util::stream::empty()) as ModelStream)
            })
        }))
    }
}

fn empty_tools() -> FrozenToolRegistry {
    ToolRegistryBuilder::default().freeze()
}

fn provider_configuration(
    model: &SilentModel,
    tools: &[ToolDefinition],
) -> TestResult<CompiledProviderConfiguration> {
    support::provider_configuration(
        &FIXTURE,
        model.descriptor(),
        &model.authority,
        tools,
        CachePolicy::Disabled,
        OutputBoundDisposition::NoRequestBound,
    )
}

fn configured_model(
    model: Arc<SilentModel>,
    tools: &FrozenToolRegistry,
) -> TestResult<ConfiguredLanguageModel> {
    let configuration = provider_configuration(model.as_ref(), tools.definitions())?;
    let authority = model.authority.clone();
    Ok(ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        model,
        configuration,
        authority,
    )?)
}

fn workspace_digest(workspace: &WorkspacePolicy) -> TestResult<plakous_model::Digest> {
    let root = workspace.root().as_os_str().as_encoded_bytes();
    let cwd = workspace.cwd().as_os_str().as_encoded_bytes();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"plakous-agent/workspace-environment-v1\0");
    bytes.extend_from_slice(std::env::consts::OS.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(b"host-path-policy-only\0");
    bytes.extend_from_slice(&u64::try_from(root.len())?.to_be_bytes());
    bytes.extend_from_slice(root);
    bytes.extend_from_slice(&u64::try_from(cwd.len())?.to_be_bytes());
    bytes.extend_from_slice(cwd);
    Ok(plakous_model::Digest::sha256(
        plakous_model::DigestDomain::ExecutionEnvironment,
        1,
        &[b"plakous-agent/workspace-facts-v1", &bytes],
    )?)
}

fn one_tool() -> TestResult<FrozenToolRegistry> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "canonical description",
        ToolInputContract::structured(
            json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}),
        )?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let tool = Arc::new(plakous_test_support::ScriptedTool::new([
        ToolOutcome::success_text("ok"),
    ]));
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        definition,
        tool,
    ))?;
    Ok(registry.freeze())
}

fn explicit_plan(
    model: &SilentModel,
    registry: &FrozenToolRegistry,
    workspace: &WorkspacePolicy,
    id: &str,
    instructions: &str,
    prefer_native: bool,
) -> TestResult<plakous_agent::internal::ResolvedHarnessPlan> {
    let configuration = provider_configuration(model, registry.definitions())?;
    let registry_digest = registry.registry_identity_digest()?;
    let continuation = if prefer_native {
        ContinuationPreferenceStrategy::PreferNative
    } else {
        ContinuationPreferenceStrategy::CanonicalReplayOnly
    };
    let spec = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new(id)?)
        .semantic_version(ProfileSemanticVersion::new(0, 1, 0))
        .provenance(HarnessProfileProvenance::Synthetic)
        .base_instructions(instructions)
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(ToolExposureStrategy::None)
        .reasoning_strategy(ReasoningPreferenceStrategy::Disabled)
        .continuation_strategy(continuation)
        .cache_strategy(CachePreferenceStrategy::Disabled)
        .build()?;
    let env = workspace_digest(workspace)?;
    Ok(compile_explicit_profile(
        &spec,
        model,
        &configuration,
        registry_digest,
        Vec::new(),
        env,
        Vec::new(),
    )?)
}

#[tokio::test]
async fn construction_binds_a_harness_plan_by_default() -> TestResult<()> {
    let model = Arc::new(SilentModel::new("test.route")?);
    let (_dir, workspace) = workspace()?;
    let tools = empty_tools();
    let configured = configured_model(model, &tools)?;
    let agent = AgentBuilder::new(configured, tools, workspace).build()?;
    let plan = agent.harness_plan();
    assert_eq!(plan.profile_id().as_str(), "plakous.construction-bound.v1");
    assert_eq!(
        plan.plan_digest().domain(),
        plakous_model::DigestDomain::ResolvedHarnessPlan
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn explicit_plan_derives_exact_alias_and_description_prefix() -> TestResult<()> {
    let model = Arc::new(SilentModel::new("test.route")?);
    let tools = one_tool()?;
    let (_directory, workspace) = workspace()?;
    let spec = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("plan.alias")?)
        .base_instructions("use tools")
        .tool_exposure_strategy(ToolExposureStrategy::CanonicalAll)
        .tool_overrides(vec![ToolExposureOverride::new(
            "probe",
            "probe_alias",
            Some("model-facing description".to_owned()),
        )?])
        .cache_strategy(CachePreferenceStrategy::Disabled)
        .build()?;
    let canonical = tools
        .definitions()
        .first()
        .ok_or("tool registry omitted its canonical definition")?;
    let exposed = ToolDefinition::new(
        ToolName::new("probe_alias")?,
        canonical.semantic_version(),
        "model-facing description",
        canonical.input().clone(),
        canonical.output().clone(),
        canonical.concurrency(),
        canonical.effect(),
    )?;
    let configuration = provider_configuration(model.as_ref(), std::slice::from_ref(&exposed))?;
    let plan = compile_explicit_profile(
        &spec,
        model.as_ref(),
        &configuration,
        tools.registry_identity_digest()?,
        vec!["probe".to_owned()],
        workspace_digest(&workspace)?,
        Vec::new(),
    )?;
    let authority = model.authority.clone();
    let configured = ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        model,
        configuration,
        authority,
    )?;
    let agent = AgentBuilder::new(configured, tools, workspace)
        .harness_plan(plan)
        .build()?;
    assert_eq!(agent.stable_request_prefix().tools().len(), 1);
    assert_eq!(
        agent.stable_request_prefix().tools()[0]
            .definition()
            .name()
            .as_str(),
        "probe_alias"
    );
    assert_eq!(
        agent.stable_request_prefix().tools()[0]
            .definition()
            .description(),
        "model-facing description"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn two_explicit_plans_bind_distinct_digests_on_same_model() -> TestResult<()> {
    let model_a = Arc::new(SilentModel::new("test.route")?);
    let model_b = Arc::new(SilentModel::new("test.route")?);
    let (_dir_a, workspace_a) = workspace()?;
    let (_dir_b, workspace_b) = workspace()?;
    let tools_a = empty_tools();
    let tools_b = empty_tools();
    let plan_a = explicit_plan(
        model_a.as_ref(),
        &tools_a,
        &workspace_a,
        "plan.a",
        "be brief",
        true,
    )?;
    let plan_b = explicit_plan(
        model_b.as_ref(),
        &tools_b,
        &workspace_b,
        "plan.b",
        "be thorough",
        false,
    )?;
    assert_ne!(plan_a.plan_digest(), plan_b.plan_digest());
    assert_eq!(
        plan_a.model_facts().route().as_str(),
        plan_b.model_facts().route().as_str()
    );

    let configured_a = configured_model(model_a, &tools_a)?;
    let configured_b = configured_model(model_b, &tools_b)?;
    let agent_a = AgentBuilder::new(configured_a, tools_a, workspace_a)
        .harness_plan(plan_a.clone())
        .build()?;
    let agent_b = AgentBuilder::new(configured_b, tools_b, workspace_b)
        .harness_plan(plan_b.clone())
        .build()?;

    assert_eq!(agent_a.harness_plan().plan_digest(), plan_a.plan_digest());
    assert_eq!(agent_b.harness_plan().plan_digest(), plan_b.plan_digest());
    assert!(agent_a.harness_plan().allows_native_continuation());
    assert!(!agent_b.harness_plan().allows_native_continuation());

    agent_a.shutdown().await?;
    agent_b.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn foreign_plan_model_binding_is_rejected() -> TestResult<()> {
    let home = Arc::new(SilentModel::new("test.route")?);
    let foreign = Arc::new(SilentModel::new("foreign.route")?);
    let (_dir, home_workspace) = workspace()?;
    let tools = empty_tools();
    let plan = explicit_plan(
        home.as_ref(),
        &tools,
        &home_workspace,
        "plan.home",
        "home",
        true,
    )?;
    let configured_foreign = configured_model(foreign, &tools)?;
    let Err(error) = AgentBuilder::new(
        configured_foreign,
        tools.clone(),
        Arc::clone(&home_workspace),
    )
    .harness_plan(plan.clone())
    .build() else {
        panic!("foreign plan must fail");
    };
    assert!(
        error
            .message()
            .contains("harness profile construction failed"),
        "unexpected error: {}",
        error.message()
    );

    let (_foreign_directory, foreign_workspace) = workspace()?;
    let environment_model = Arc::new(SilentModel::new("test.route")?);
    let configured_environment = configured_model(environment_model, &tools)?;
    let Err(environment_error) =
        AgentBuilder::new(configured_environment, tools, foreign_workspace)
            .harness_plan(plan.clone())
            .build()
    else {
        panic!("foreign environment plan must fail");
    };
    assert!(
        environment_error
            .message()
            .contains("execution-environment")
    );

    let registry_model = Arc::new(SilentModel::new("test.route")?);
    let foreign_tools = one_tool()?;
    let configured_registry = configured_model(registry_model, &foreign_tools)?;
    let Err(registry_error) = AgentBuilder::new(configured_registry, foreign_tools, home_workspace)
        .harness_plan(plan)
        .build()
    else {
        panic!("foreign registry plan must fail");
    };
    assert!(
        registry_error
            .message()
            .contains("harness profile construction failed"),
        "unexpected error: {}",
        registry_error.message()
    );
    Ok(())
}

#[test]
fn compiler_model_facts_do_not_invent_support() -> TestResult<()> {
    let model = SilentModel::new("test.route")?;
    let empty = empty_tools();
    let configuration = provider_configuration(&model, empty.definitions())?;
    let facts = compiler_model_facts_from_model(&model, &configuration)?;
    assert_eq!(facts.route().as_str(), "test.route");
    HarnessProfileCompiler::new().compile(
        &HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new("facts")?)
            .base_instructions("x")
            .build()?,
        &plakous_agent::internal::HarnessCompilerInputs::new(
            facts,
            plakous_agent::internal::CompilerToolFacts::new(
                plakous_model::Digest::sha256(
                    plakous_model::DigestDomain::ToolDefinition,
                    1,
                    &[b"t"],
                )?,
                Vec::new(),
            )?,
            plakous_agent::internal::CompilerRuntimeFacts::new(
                plakous_model::Digest::sha256(
                    plakous_model::DigestDomain::ExecutionEnvironment,
                    1,
                    &[b"e"],
                )?,
                64 * 1024,
                false,
                false,
                false,
            ),
            Vec::new(),
        )?,
    )?;
    Ok(())
}
