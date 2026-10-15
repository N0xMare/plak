//! Compile-time facade workflow and current-nameability contracts.
//!
//! This proves embedders can name the build, turn-control, observation, result,
//! and shutdown workflow without importing a component crate. Low-level
//! durability/store mechanics deliberately remain absent. Every test in this
//! file is part of the exact supported `plakous` inventory; the external
//! facade fixture separately proves an executable consumer workflow.

#![allow(
    clippy::doc_markdown,
    clippy::expect_used,
    clippy::items_after_statements,
    clippy::too_many_lines,
    clippy::unwrap_used,
    reason = "facade inventory pins type names before compile-pinning local workflow signatures; serde/assert helpers use expect"
)]

/// Always-on facade: version + curated agent + model + tools + profiles.
#[test]
fn facade_reports_its_package_version() {
    assert_eq!(plakous::VERSION, env!("CARGO_PKG_VERSION"));
}

#[test]
fn construction_helpers_are_supported_facade_workflows() {
    use plakous::construction::{
        CustomProviderConfigurationRequest, bind_custom_model_authority,
        compile_local_composition_plan, provider_model_projection_identity,
    };
    use plakous::model::{ContextContract, Digest, ResolvedModelDescriptor, Timestamp};

    type Error = Box<dyn std::error::Error>;
    fn bind(
        descriptor: ResolvedModelDescriptor,
        binding: Digest,
        time: Timestamp,
        contract: fn(Digest) -> Result<ContextContract, Error>,
    ) -> Result<
        (
            ResolvedModelDescriptor,
            plakous::model::RuntimeModelAuthority,
        ),
        Error,
    > {
        bind_custom_model_authority(descriptor, binding, binding, 1, time, None, contract)
    }
    fn hold<T>(_: T) {}
    hold(std::any::type_name::<CustomProviderConfigurationRequest<'_>>());
    hold(bind);
    hold(compile_local_composition_plan);
    hold(provider_model_projection_identity);
}

#[test]
fn application_accepts_caller_supplied_cancellation_without_subscription_features() {
    use plakous::agent::Agent;
    use plakous::application::{TurnDriveError, TurnDriveOutcome, run_once_until_signal};
    use plakous::sessions::{AgentSessionsError, isolated_tool_scheduler};

    async fn drive(agent: Agent) -> Result<TurnDriveOutcome, TurnDriveError> {
        run_once_until_signal(agent, "hello", std::future::pending(), |_| Ok(())).await
    }
    fn hold<T>(_: T) {}
    hold(drive);
    hold(
        isolated_tool_scheduler
            as fn(
                plakous::agent::AgentBuilder,
            ) -> Result<plakous::agent::AgentBuilder, AgentSessionsError>,
    );
}

#[cfg(feature = "native-subscriptions")]
#[test]
fn local_composition_attachment_is_a_capability_poor_facade_workflow() {
    use plakous::agent::{
        AgentBuilder, AgentError, LocalCompositionAttachment, LocalCompositionAttachmentBuilder,
        LocalCompositionContainment, LocalCompositionEngineArtifact, LocalCompositionLanguage,
        LocalCompositionProfile, ProductExtensionEvidence, ProductExtensionLimits,
        attach_local_composition,
    };
    use plakous::model::{CompiledProviderConfiguration, Digest};
    use plakous::tools::{
        CodexMinimalLocalCodeModeDescriptorV1, CodexNativeLocalCodeModeDescriptorV1,
        ProductJavaScriptCompositionDescriptorV1, PythonPtcDescriptorV1,
    };

    fn select_profile(
        descriptor: &CodexNativeLocalCodeModeDescriptorV1,
    ) -> Result<LocalCompositionProfile, AgentError> {
        LocalCompositionProfile::codex_native_local_code_mode_host_v1(descriptor)
    }
    fn select_minimal_profile(
        descriptor: &CodexMinimalLocalCodeModeDescriptorV1,
    ) -> Result<LocalCompositionProfile, AgentError> {
        LocalCompositionProfile::codex_minimal_local_code_mode_host_v1(descriptor)
    }
    fn verify_sidecar_artifact(
        path: &std::path::Path,
    ) -> Result<LocalCompositionEngineArtifact, AgentError> {
        LocalCompositionEngineArtifact::rquickjs_sidecar_v1(path)
    }
    fn select_product_profile(
        descriptor: &ProductJavaScriptCompositionDescriptorV1,
    ) -> Result<LocalCompositionProfile, AgentError> {
        LocalCompositionProfile::product_javascript_v1(descriptor)
    }
    fn select_python_profile(
        descriptor: &PythonPtcDescriptorV1,
    ) -> Result<LocalCompositionProfile, AgentError> {
        LocalCompositionProfile::python_ptc_v1(descriptor)
    }
    fn verify_python_artifact(
        host: &std::path::Path,
    ) -> Result<LocalCompositionEngineArtifact, AgentError> {
        LocalCompositionEngineArtifact::monty_pool_v1(host)
    }
    fn close_product_binding(
        profile: LocalCompositionProfile,
        artifact: LocalCompositionEngineArtifact,
        evidence: ProductExtensionEvidence,
    ) -> Result<LocalCompositionAttachment, AgentError> {
        LocalCompositionAttachment::builder(profile)
            .engine_artifact(artifact)
            .product_extension_evidence(evidence)
            .coding_presentation_kind(plakous::tools::CodingPresentation::Codex)
            .build()
    }
    fn qualify_product_binding(
        profile: &LocalCompositionProfile,
        artifact: &LocalCompositionEngineArtifact,
        limits: ProductExtensionLimits,
        issuer: Digest,
        qualification: Digest,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<ProductExtensionEvidence, AgentError> {
        ProductExtensionEvidence::product_javascript_v1(
            profile,
            artifact,
            limits,
            issuer,
            qualification,
            configuration,
        )
    }
    fn qualify_python_binding(
        profile: &LocalCompositionProfile,
        artifact: &LocalCompositionEngineArtifact,
        issuer: Digest,
        qualification: Digest,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<ProductExtensionEvidence, AgentError> {
        ProductExtensionEvidence::python_ptc_v1(
            profile,
            artifact,
            issuer,
            qualification,
            configuration,
        )
    }
    fn close_binding(
        profile: LocalCompositionProfile,
        artifact: LocalCompositionEngineArtifact,
    ) -> Result<LocalCompositionAttachment, AgentError> {
        LocalCompositionAttachment::builder(profile)
            .engine_artifact(artifact)
            .build()
    }
    fn dormant(builder: AgentBuilder) -> Result<AgentBuilder, AgentError> {
        attach_local_composition(builder, None)
    }

    fn hold<T: ?Sized>(_: &T) {}
    hold(&std::any::type_name::<LocalCompositionAttachment>());
    hold(&std::any::type_name::<LocalCompositionAttachmentBuilder>());
    hold(&std::any::type_name::<LocalCompositionEngineArtifact>());
    hold(&std::any::type_name::<LocalCompositionContainment>());
    hold(&std::any::type_name::<LocalCompositionLanguage>());
    hold(&std::any::type_name::<LocalCompositionProfile>());
    hold(&select_profile);
    hold(&select_minimal_profile);
    hold(&verify_sidecar_artifact);
    hold(&select_product_profile);
    hold(&select_python_profile);
    hold(&verify_python_artifact);
    hold(&close_binding);
    hold(&close_product_binding);
    hold(&qualify_product_binding);
    hold(&qualify_python_binding);
    hold(&dormant);
}

#[cfg(feature = "native-subscriptions")]
mod codex_local_composition_reconstruction {
    use plakous::agent::{
        AgentError, AgentErrorKind, LocalCompositionAttachment, LocalCompositionEngineArtifact,
        LocalCompositionProfile,
    };
    use plakous::model::{
        Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass,
    };
    use plakous::tools::{
        CodexMinimalLocalCodeModeV1, CodexNativeLocalCodeModeV1, FrozenToolRegistryDescriptor,
        ProviderModelProjectionIdentityV2, ToolExecutorBindings, ToolRegistryBuilder,
        register_packs, standard_coding_packs_in_memory,
    };

    type TestError = Box<dyn std::error::Error>;

    fn target() -> Result<ProviderModelProjectionIdentityV2, TestError> {
        let configuration: EffectiveProviderConfigurationId =
            serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"public-facade-codex-code-mode-reconstruction"],
            )?)?)?;
        Ok(ProviderModelProjectionIdentityV2::new(
            IntegrationClass::Standard,
            "openai.codex.responses",
            "openai.codex_subscription",
            "openai.codex.compat.0.153.3",
            "responses-family",
            "gpt-codex-code-mode-test",
            "gpt-codex-code-mode-test",
            configuration,
            1,
        )?)
    }

    fn registry() -> Result<plakous::tools::FrozenToolRegistry, TestError> {
        let mut builder = ToolRegistryBuilder::default();
        register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
        CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)?;
        Ok(builder.freeze())
    }

    fn require_exact_profile(
        recorded: Digest,
        reconstructed: &LocalCompositionProfile,
    ) -> Result<(), AgentError> {
        if reconstructed.identity() != recorded {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "reconstructed local composition profile differs from the recorded profile",
            ));
        }
        Ok(())
    }

    #[test]
    fn four_and_seven_tool_profiles_reconstruct_exactly_and_refuse_cross_profile_substitution()
    -> Result<(), TestError> {
        let original_registry = registry()?;
        let bindings = ToolExecutorBindings::capture(&original_registry);
        let encoded = FrozenToolRegistryDescriptor::capture(&original_registry)?.encode_v2()?;
        let reconstructed_registry =
            FrozenToolRegistryDescriptor::decode_v2(&encoded)?.resolve(&bindings)?;
        let target = target()?;

        let minimal = CodexMinimalLocalCodeModeV1::compile(
            plakous::tools::JobConfig::default(),
            original_registry.clone(),
            target.clone(),
        )?;
        let native = CodexNativeLocalCodeModeV1::compile(
            plakous::tools::JobConfig::default(),
            original_registry,
            target.clone(),
        )?;
        let reconstructed_minimal = CodexMinimalLocalCodeModeV1::compile(
            plakous::tools::JobConfig::default(),
            reconstructed_registry.clone(),
            target.clone(),
        )?;
        let reconstructed_native = CodexNativeLocalCodeModeV1::compile(
            plakous::tools::JobConfig::default(),
            reconstructed_registry,
            target,
        )?;

        assert_eq!(
            minimal
                .nested_tools()
                .iter()
                .map(|tool| tool.exposed_name().as_str())
                .collect::<Vec<_>>(),
            CodexMinimalLocalCodeModeV1::nested_names()
        );
        assert_eq!(
            native
                .nested_tools()
                .iter()
                .map(|tool| tool.exposed_name().as_str())
                .collect::<Vec<_>>(),
            CodexNativeLocalCodeModeV1::nested_names()
        );
        assert_eq!(minimal.identity(), reconstructed_minimal.identity());
        assert_eq!(native.identity(), reconstructed_native.identity());
        assert_eq!(
            minimal.outer_projection().digest(),
            reconstructed_minimal.outer_projection().digest()
        );
        assert_eq!(
            native.outer_projection().digest(),
            reconstructed_native.outer_projection().digest()
        );
        assert_ne!(minimal.identity(), native.identity());
        assert_ne!(
            minimal.outer_projection().digest(),
            native.outer_projection().digest()
        );

        let minimal_profile =
            LocalCompositionProfile::codex_minimal_local_code_mode_host_v1(&reconstructed_minimal)?;
        let native_profile =
            LocalCompositionProfile::codex_native_local_code_mode_host_v1(&reconstructed_native)?;
        assert_eq!(minimal_profile.identity(), minimal.identity());
        assert_eq!(native_profile.identity(), native.identity());
        require_exact_profile(minimal.identity(), &minimal_profile)?;
        require_exact_profile(native.identity(), &native_profile)?;
        assert!(require_exact_profile(minimal.identity(), &native_profile).is_err());
        assert!(require_exact_profile(native.identity(), &minimal_profile).is_err());
        Ok(())
    }

    #[test]
    fn rquickjs_sidecar_attaches_minimal_code_mode_profile() -> Result<(), TestError> {
        let registry = registry()?;
        let target = target()?;
        let minimal = CodexMinimalLocalCodeModeV1::compile(
            plakous::tools::JobConfig::default(),
            registry,
            target,
        )?;
        let profile = LocalCompositionProfile::codex_minimal_local_code_mode_host_v1(&minimal)?;
        // This contract checks attachment identity, not sidecar execution. Use
        // a file present on every test host, including BusyBox-only systems.
        let host = std::fs::canonicalize(std::env::current_exe()?)?;
        let attachment = LocalCompositionAttachment::builder(profile)
            .engine_artifact(LocalCompositionEngineArtifact::rquickjs_sidecar_v1(host)?)
            .build()?;
        assert_eq!(attachment.profile().identity(), minimal.identity());
        Ok(())
    }
}

/// Exact supported imports for the embedder workflow.
///
/// If this fails to compile, a public signature is no longer nameable through
/// `plakous` or a withdrawn durability primitive leaked back into the inventory.
#[test]
fn exact_supported_root_inventory_is_importable() {
    use plakous::agent::{
        Agent, AgentBuilder, AgentConfig, AgentError, AgentErrorKind, AgentEvent, AgentEventKind,
        AgentRequestPolicy, AgentViewSnapshot, CancellationStatus, CapabilityRequirement,
        CheckpointId, CommandAck, CommandAckDisposition, CommandId, ContextSourceCatalog,
        ContextSourceResolver, ContextSourceSelector, ControlError, ControlErrorKind,
        ConversationView, Digest, DigestDomain, EffectDecisionChoice, EffectDecisionRef,
        EffectDecisionResolution, EffectDecisionStatus, EventCursor, EventScope, EventSequence,
        EventSubscription, EventVisibility, GenerationLimits, HarnessProfileId, HarnessProfileSpec,
        HarnessProfileSpecBuilder, InstructionCompositionStrategy, ProfileError, ReasoningEffort,
        ReasoningPolicy, RegisteredContextSource, ResolvedContextSourceFact, ResolvedHarnessPlan,
        ResolvedInstructionPlan, ResumeOutcome, SteeringStatus, TelemetryContext, TelemetryEmitter,
        TelemetryObservation, TelemetryObservationKind, TelemetrySink, TelemetrySinkError,
        TelemetryStatus, TelemetryTerminalStatus, ToolCallerClass, ToolCallerLineageView, Turn,
        TurnCommand, TurnControl, TurnEventError, TurnEvents, TurnResult, TurnResultHandle,
        TurnResultOutcome, TurnSubmission, TurnUsage, VerifiedContextSourceRef, ViewAttachment,
        ViewError, ViewRequest, WeakAgent, compile_explicit_profile,
    };
    use plakous::model::{
        CompiledToolProjectionV2, HostedTool, LanguageModel, ModelDiagnostic, ModelEvent,
        PortableToolSchemaV2, ToolCallerDisposition, ToolCallerLineage,
    };
    use plakous::profiles::{claude, grok, neutral_coding};
    use plakous::tools::{
        AlwaysAllowPolicy, FilesToolPack, FrozenToolRegistry, PlanToolPack, ProcessSessionTools,
        ProcessToolPack, ProcessToolProjectionKind, ProviderModelProjectionIdentityV2,
        SearchToolPack, ToolProjectionMap, ToolRegistryBuilder, WorkspacePolicy,
        process_presentation_candidate_v2, process_presentation_candidate_with_extensions_v2,
    };

    fn hold<T: ?Sized>(_: &T) {}
    hold(&std::any::type_name::<Agent>());
    hold(&std::any::type_name::<AgentBuilder>());
    hold(&std::any::type_name::<AgentConfig>());
    hold(&std::any::type_name::<AgentError>());
    hold(&std::any::type_name::<AgentErrorKind>());
    hold(&std::any::type_name::<AgentEvent>());
    hold(&std::any::type_name::<AgentEventKind>());
    hold(&std::any::type_name::<AgentRequestPolicy>());
    hold(&std::any::type_name::<AgentViewSnapshot>());
    hold(&std::any::type_name::<EffectDecisionChoice>());
    hold(&std::any::type_name::<CancellationStatus>());
    hold(&std::any::type_name::<CapabilityRequirement>());
    hold(&std::any::type_name::<HostedTool>());
    hold(&std::any::type_name::<ToolCallerDisposition>());
    hold(&std::any::type_name::<ToolCallerLineage>());
    hold(&std::any::type_name::<ToolCallerClass>());
    hold(&std::any::type_name::<ToolCallerLineageView>());
    hold(&std::any::type_name::<CheckpointId>());
    hold(&std::any::type_name::<CommandAck>());
    hold(&std::any::type_name::<CommandAckDisposition>());
    hold(&std::any::type_name::<CommandId>());
    hold(&std::any::type_name::<ControlError>());
    hold(&std::any::type_name::<ControlErrorKind>());
    hold(&std::any::type_name::<ContextSourceCatalog>());
    hold(&std::any::type_name::<ContextSourceResolver>());
    hold(&std::any::type_name::<ContextSourceSelector>());
    hold(&std::any::type_name::<Digest>());
    hold(&std::any::type_name::<DigestDomain>());
    hold(&std::any::type_name::<ConversationView>());
    hold(&std::any::type_name::<EventCursor>());
    hold(&std::any::type_name::<EventScope>());
    hold(&std::any::type_name::<EventSequence>());
    hold(&std::any::type_name::<EventSubscription>());
    hold(&std::any::type_name::<EventVisibility>());
    hold(&std::any::type_name::<GenerationLimits>());
    hold(&std::any::type_name::<HarnessProfileId>());
    hold(&std::any::type_name::<HarnessProfileSpec>());
    hold(&std::any::type_name::<HarnessProfileSpecBuilder>());
    hold(&std::any::type_name::<InstructionCompositionStrategy>());
    hold(&std::any::type_name::<dyn LanguageModel>());
    hold(&std::any::type_name::<ModelDiagnostic>());
    hold(&std::any::type_name::<ModelEvent>());
    hold(&std::any::type_name::<ProfileError>());
    hold(&std::any::type_name::<ReasoningEffort>());
    hold(&std::any::type_name::<ReasoningPolicy>());
    hold(&std::any::type_name::<RegisteredContextSource>());
    hold(&std::any::type_name::<ResolvedContextSourceFact>());
    hold(&std::any::type_name::<ResolvedHarnessPlan>());
    hold(&std::any::type_name::<ResolvedInstructionPlan>());
    hold(&std::any::type_name::<ResumeOutcome>());
    hold(&std::any::type_name::<SteeringStatus>());
    hold(&std::any::type_name::<TelemetryContext>());
    hold(&std::any::type_name::<TelemetryEmitter>());
    hold(&std::any::type_name::<TelemetryObservation>());
    hold(&std::any::type_name::<TelemetryObservationKind>());
    hold(&std::any::type_name::<dyn TelemetrySink>());
    hold(&std::any::type_name::<TelemetrySinkError>());
    hold(&std::any::type_name::<TelemetryStatus>());
    hold(&std::any::type_name::<TelemetryTerminalStatus>());
    hold(&std::any::type_name::<EffectDecisionRef>());
    hold(&std::any::type_name::<EffectDecisionResolution>());
    hold(&std::any::type_name::<EffectDecisionStatus>());
    hold(&std::any::type_name::<Turn>());
    hold(&std::any::type_name::<TurnCommand>());
    hold(&std::any::type_name::<TurnControl>());
    hold(&std::any::type_name::<TurnEventError>());
    hold(&std::any::type_name::<TurnEvents>());
    hold(&std::any::type_name::<TurnResult>());
    hold(&std::any::type_name::<TurnResultHandle>());
    hold(&std::any::type_name::<TurnResultOutcome>());
    hold(&std::any::type_name::<TurnSubmission>());
    hold(&std::any::type_name::<TurnUsage>());
    hold(&std::any::type_name::<VerifiedContextSourceRef>());
    hold(&std::any::type_name::<ViewAttachment>());
    hold(&std::any::type_name::<ViewError>());
    hold(&std::any::type_name::<ViewRequest>());
    hold(&std::any::type_name::<WeakAgent>());
    let f1 = compile_explicit_profile;
    let f2 = neutral_coding;
    let f3 = grok::recommended;
    let f4 = claude::recommended;
    hold(&std::any::type_name_of_val(&f1));
    hold(&std::any::type_name_of_val(&f2));
    hold(&std::any::type_name_of_val(&f3));
    hold(&std::any::type_name_of_val(&f4));
    hold(&std::any::type_name::<AlwaysAllowPolicy>());
    hold(&std::any::type_name::<FrozenToolRegistry>());
    hold(&std::any::type_name::<ProcessSessionTools>());
    hold(&std::any::type_name::<ProcessToolProjectionKind>());
    hold(&std::any::type_name::<ProviderModelProjectionIdentityV2>());
    hold(&std::any::type_name::<FilesToolPack>());
    hold(&std::any::type_name::<SearchToolPack>());
    hold(&std::any::type_name::<ProcessToolPack>());
    hold(&std::any::type_name::<PlanToolPack>());
    hold(&std::any::type_name::<ToolProjectionMap>());
    hold(&std::any::type_name::<CompiledToolProjectionV2>());
    hold(&std::any::type_name::<PortableToolSchemaV2>());
    hold(&std::any::type_name::<ToolRegistryBuilder>());
    hold(&std::any::type_name::<WorkspacePolicy>());
    hold(&process_presentation_candidate_v2);
    hold(&process_presentation_candidate_with_extensions_v2);
    hold(&std::any::type_name::<plakous::tools::CodingPresentation>());
    assert_eq!(
        plakous::tools::PROCESS_SESSION_CAPABILITY_NAME,
        "process_session_start"
    );
    assert_ne!(plakous::tools::PROCESS_TOOL_PROJECTION_REVISION, 0);
}

#[test]
fn exact_context_source_workflow_is_nameable_through_the_facade() {
    use plakous::agent::{
        ContextSourceCatalog, ContextSourceResolver, ContextSourceSelector, HarnessProfileId,
        HarnessProfileSpecBuilder, InstructionCompositionStrategy, RegisteredContextSource,
    };

    let selector = ContextSourceSelector::new("repo.agents").expect("selector");
    let spec = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("facade.context").expect("id"))
        .base_instructions("base")
        .instruction_strategy(InstructionCompositionStrategy::BaseThenSources)
        .instruction_sources(vec![selector])
        .build()
        .expect("spec");
    let mut catalog = ContextSourceCatalog::new();
    catalog
        .insert(
            RegisteredContextSource::new("repo.agents", "exact bytes", "1", "facade")
                .expect("source"),
        )
        .expect("insert");
    let facts = ContextSourceResolver::new(catalog)
        .resolve(spec.instruction_sources())
        .expect("resolve");

    assert_eq!(facts[0].content(), "exact bytes");
}

/// Compile-pin the complete workflow rather than isolated type names.
#[test]
fn wave_1_agent_workflow_is_nameable_through_the_facade() {
    use plakous::agent::{
        Agent, AgentBuilder, AgentError, AgentErrorKind, CancellationStatus, CheckpointId,
        CommandId, ControlError, ResumeOutcome, SteeringStatus, Turn, TurnCommand, TurnControl,
        TurnEvents, TurnResult, TurnResultHandle, TurnSubmission, ViewAttachment, ViewError,
        ViewRequest,
    };
    use plakous::model::{TurnId, TurnOrigin};

    assert_eq!(TurnOrigin::UserPrompt.as_str(), "user_prompt");
    assert_eq!(TurnOrigin::HostWake.as_str(), "host_wake");

    fn build(builder: AgentBuilder) -> Result<Agent, AgentError> {
        builder.build()
    }

    async fn start(agent: &Agent) -> Result<Turn, AgentError> {
        agent.prompt("start").await
    }

    async fn acknowledged_start(
        agent: &Agent,
        id: CommandId,
    ) -> Result<TurnSubmission, AgentError> {
        agent.start(id, "start").await
    }

    async fn submit(agent: &Agent, command: TurnCommand) -> Result<TurnSubmission, ControlError> {
        agent.command(command).await
    }

    async fn resolve_duplicate(
        handle: TurnResultHandle,
    ) -> Result<std::sync::Arc<TurnResult>, AgentError> {
        handle.result().await
    }

    async fn follow_up(agent: &Agent, active: TurnId) -> Result<Turn, ControlError> {
        agent.follow_up(active, "follow-up").await
    }

    async fn observe_and_finish(
        mut turn: Turn,
    ) -> (
        Result<TurnEvents, ViewError>,
        Result<TurnResult, AgentError>,
    ) {
        let events = turn.take_events();
        let result = turn.result().await;
        (events, result)
    }

    async fn control(
        control: &TurnControl,
    ) -> (
        Result<SteeringStatus, ControlError>,
        Result<CancellationStatus, ControlError>,
    ) {
        (control.steer("steer").await, control.cancel().await)
    }

    async fn observe_and_resync(
        agent: &Agent,
        request: ViewRequest,
    ) -> Result<ViewAttachment, ViewError> {
        agent.attach(request).await
    }

    async fn resync_from_cursor(
        agent: &Agent,
        attachment: &ViewAttachment,
        request: ViewRequest,
    ) -> Result<ResumeOutcome, ViewError> {
        agent.resume(attachment.snapshot().cursor(), request).await
    }

    async fn shutdown(agent: &Agent) -> Result<(), AgentError> {
        agent.shutdown().await
    }

    fn active_checkpoint(agent: &Agent) -> CheckpointId {
        agent.checkpoint_id()
    }

    fn terminal_checkpoint(result: &TurnResult) -> CheckpointId {
        result.checkpoint_id()
    }

    fn stable_error(error: &AgentError) -> (AgentErrorKind, &str) {
        (error.kind(), error.message())
    }

    let _ = (
        build,
        start,
        acknowledged_start,
        submit,
        resolve_duplicate,
        follow_up,
        observe_and_finish,
        control,
        observe_and_resync,
        resync_from_cursor,
        shutdown,
        active_checkpoint,
        terminal_checkpoint,
        stable_error,
    );
}

/// Durable-session callers see exact workflow values, never store rows,
/// leases, compare-and-swap receipts, or a backend implementation.
#[test]
fn exact_session_workflow_is_nameable_through_the_facade() {
    use plakous::sessions::{
        AbandonCodeCellOutcome, AbandonCodeCellRequest, AbandonCompactionOutcome,
        AbandonCompactionRequest, AgentSessions, AgentSessionsError, AgentSessionsErrorKind,
        ArchiveSessionReport, ArtifactPortability, CompactReason, CompactSessionOutcome,
        CompactSessionRequest, ContextQuery, ContextStatusRequest, CreateSessionOutcome,
        CreateSessionRequest, DeleteSessionReport, ForkSessionOutcome, ForkSessionRequest,
        OpenSessionOutcome, OpenSessionRequest, OpenedSession, PortableSessionHistory,
        RebindSessionOutcome, RebindSessionRequest, RecoverEffectEvidenceOutcome,
        RecoverEffectEvidenceRequest, RecoverSessionOutcome, RecoverSessionRequest,
        RecoveryObligationReport, RecoveryRequiredReport, SESSION_HISTORY_VISIBILITY_REVISION,
        SessionBackend, SessionBindingReport, SessionCallDetail, SessionCallDetailOutcome,
        SessionCallDetailText, SessionContextStatus, SessionContextStatusKind,
        SessionEffectEvidence, SessionHistoryCompaction, SessionHistoryCompactionPass,
        SessionHistoryCursor, SessionHistoryDirection, SessionHistoryItem,
        SessionHistoryPageOutcome, SessionHistoryPageRequest, SessionHistoryResnapshot,
        SessionHistoryVisibility, SessionLocator, SessionOperationId, SessionRecoveryAction,
        SessionRecoveryPhase, SessionRuntimeBinding, SessionTargetBinding,
        SessionWorkflowUnavailable,
    };

    fn hold<T: ?Sized>(_: &T) {}
    hold(&std::any::type_name::<AbandonCompactionOutcome>());
    hold(&std::any::type_name::<AbandonCompactionRequest>());
    hold(&std::any::type_name::<AbandonCodeCellOutcome>());
    hold(&std::any::type_name::<AbandonCodeCellRequest>());
    hold(&std::any::type_name::<AgentSessions>());
    hold(&std::any::type_name::<AgentSessionsError>());
    hold(&std::any::type_name::<AgentSessionsErrorKind>());
    hold(&std::any::type_name::<ArchiveSessionReport>());
    hold(&std::any::type_name::<ArtifactPortability>());
    hold(&std::any::type_name::<CompactReason>());
    hold(&std::any::type_name::<CompactSessionOutcome>());
    hold(&std::any::type_name::<CompactSessionRequest>());
    hold(&std::any::type_name::<ContextQuery>());
    hold(&std::any::type_name::<ContextStatusRequest>());
    hold(&std::any::type_name::<CreateSessionOutcome>());
    hold(&std::any::type_name::<CreateSessionRequest>());
    hold(&std::any::type_name::<DeleteSessionReport>());
    hold(&std::any::type_name::<ForkSessionOutcome>());
    hold(&std::any::type_name::<ForkSessionRequest>());
    hold(&std::any::type_name::<OpenSessionOutcome>());
    hold(&std::any::type_name::<OpenSessionRequest>());
    hold(&std::any::type_name::<OpenedSession>());
    hold(&std::any::type_name::<PortableSessionHistory>());
    hold(&std::any::type_name::<RebindSessionOutcome>());
    hold(&std::any::type_name::<RebindSessionRequest>());
    hold(&std::any::type_name::<RecoverSessionOutcome>());
    hold(&std::any::type_name::<RecoverSessionRequest>());
    hold(&std::any::type_name::<RecoverEffectEvidenceOutcome>());
    hold(&std::any::type_name::<RecoverEffectEvidenceRequest>());
    hold(&std::any::type_name::<RecoveryObligationReport>());
    hold(&std::any::type_name::<RecoveryRequiredReport>());
    hold(&std::any::type_name::<SessionBackend>());
    hold(&std::any::type_name::<SessionBindingReport>());
    hold(&std::any::type_name::<SessionCallDetail>());
    hold(&std::any::type_name::<SessionCallDetailOutcome>());
    hold(&std::any::type_name::<SessionCallDetailText>());
    hold(&plakous::sessions::MAX_CALL_DETAIL_SIDE_BYTES);
    hold(&std::any::type_name::<SessionContextStatus>());
    hold(&std::any::type_name::<SessionContextStatusKind>());
    hold(&std::any::type_name::<SessionEffectEvidence>());
    hold(&std::any::type_name::<SessionHistoryCompaction>());
    hold(&std::any::type_name::<SessionHistoryCompactionPass>());
    hold(&std::any::type_name::<SessionHistoryCursor>());
    hold(&std::any::type_name::<SessionHistoryDirection>());
    hold(&std::any::type_name::<SessionHistoryItem>());
    hold(&std::any::type_name::<SessionHistoryPageOutcome>());
    hold(&std::any::type_name::<SessionHistoryPageRequest>());
    hold(&std::any::type_name::<SessionHistoryResnapshot>());
    hold(&std::any::type_name::<SessionHistoryVisibility>());
    hold(&std::any::type_name::<SessionLocator>());
    hold(&std::any::type_name::<SessionOperationId>());
    hold(&std::any::type_name::<SessionRecoveryAction>());
    hold(&std::any::type_name::<SessionRecoveryPhase>());
    hold(&std::any::type_name::<SessionRuntimeBinding>());
    hold(&std::any::type_name::<SessionTargetBinding>());
    hold(&std::any::type_name::<SessionWorkflowUnavailable>());

    fn compose_memory() -> Result<AgentSessions, AgentSessionsError> {
        AgentSessions::new(SessionBackend::memory()?)
    }

    fn genesis(operation: &SessionOperationId) -> Result<SessionLocator, AgentSessionsError> {
        AgentSessions::genesis_locator_for(operation)
    }

    #[cfg(feature = "sessions-sqlite")]
    fn compose_sqlite(path: &std::path::Path) -> Result<AgentSessions, AgentSessionsError> {
        AgentSessions::new(SessionBackend::sqlite(path)?)
    }

    async fn create(
        sessions: &AgentSessions,
        request: CreateSessionRequest,
    ) -> Result<CreateSessionOutcome, AgentSessionsError> {
        Box::pin(sessions.create(request)).await
    }

    async fn open(
        sessions: &AgentSessions,
        request: OpenSessionRequest,
    ) -> Result<OpenSessionOutcome, AgentSessionsError> {
        Box::pin(sessions.open(request)).await
    }

    async fn release(session: OpenedSession) -> Result<SessionLocator, AgentSessionsError> {
        session.release().await
    }

    async fn archive(
        sessions: &AgentSessions,
        locator: SessionLocator,
    ) -> Result<ArchiveSessionReport, AgentSessionsError> {
        sessions.archive(locator).await
    }

    async fn delete(
        sessions: &AgentSessions,
        archive: ArchiveSessionReport,
    ) -> Result<DeleteSessionReport, AgentSessionsError> {
        sessions
            .delete(archive.session_id(), archive.archived_revision())
            .await
    }

    async fn recover_delete(
        sessions: &AgentSessions,
        archive: ArchiveSessionReport,
    ) -> Result<DeleteSessionReport, AgentSessionsError> {
        sessions
            .recover_delete(archive.session_id(), archive.archived_revision())
            .await
    }

    async fn recover(
        sessions: &AgentSessions,
        request: RecoverSessionRequest,
    ) -> Result<RecoverSessionOutcome, AgentSessionsError> {
        sessions.recover(request).await
    }

    async fn recover_effect_evidence(
        sessions: &AgentSessions,
        request: RecoverEffectEvidenceRequest,
    ) -> Result<RecoverEffectEvidenceOutcome, AgentSessionsError> {
        sessions.recover_effect_evidence(request).await
    }

    async fn history_page(
        sessions: &AgentSessions,
        request: SessionHistoryPageRequest,
    ) -> Result<SessionHistoryPageOutcome, AgentSessionsError> {
        sessions.history_page(request).await
    }

    async fn call_detail(
        sessions: &AgentSessions,
        locator: SessionLocator,
        call_id: plakous::model::CallId,
    ) -> Result<SessionCallDetailOutcome, AgentSessionsError> {
        sessions.call_detail(locator, call_id).await
    }

    #[cfg(feature = "sessions-sqlite")]
    fn inspect_history_page_sqlite(
        path: &std::path::Path,
        request: &SessionHistoryPageRequest,
    ) -> Result<SessionHistoryPageOutcome, AgentSessionsError> {
        AgentSessions::inspect_history_page_sqlite(path, request)
    }

    async fn abandon_code_cell(
        sessions: &AgentSessions,
        request: AbandonCodeCellRequest,
    ) -> Result<AbandonCodeCellOutcome, AgentSessionsError> {
        sessions.abandon_code_cell(request).await
    }

    async fn context_status(
        sessions: &AgentSessions,
        request: ContextStatusRequest,
    ) -> Result<SessionContextStatus, AgentSessionsError> {
        sessions.context_status(request).await
    }

    async fn compact(
        sessions: &AgentSessions,
        request: CompactSessionRequest,
    ) -> Result<CompactSessionOutcome, AgentSessionsError> {
        sessions.compact(request).await
    }

    async fn fork(
        sessions: &AgentSessions,
        request: ForkSessionRequest,
    ) -> Result<ForkSessionOutcome, AgentSessionsError> {
        sessions.fork(request).await
    }

    async fn rebind(
        sessions: &AgentSessions,
        request: RebindSessionRequest,
    ) -> Result<RebindSessionOutcome, AgentSessionsError> {
        Box::pin(sessions.rebind(request)).await
    }

    let _ = (
        create,
        open,
        release,
        archive,
        delete,
        recover_delete,
        recover,
        recover_effect_evidence,
        history_page,
        call_detail,
        abandon_code_cell,
        context_status,
        compact,
        fork,
        rebind,
        compose_memory,
        genesis,
    );
    hold(&SESSION_HISTORY_VISIBILITY_REVISION);

    #[cfg(feature = "sessions-sqlite")]
    let _ = (compose_sqlite, inspect_history_page_sqlite);
}

#[test]
fn public_memory_session_identities_are_exact_and_portable() {
    use plakous::model::{BranchId, SessionId};
    use plakous::sessions::{
        AgentSessions, AgentSessionsErrorKind, SessionBackend, SessionLocator, SessionOperationId,
    };

    let sessions =
        AgentSessions::new(SessionBackend::memory().expect("memory backend lease clock"))
            .expect("memory facade backend");
    let debug = format!("{sessions:?}");
    assert!(debug.contains("AgentSessions"));
    assert!(!debug.contains("SessionStore"));

    let operation: SessionOperationId = "consumer.session:compact-1"
        .parse()
        .expect("portable operation identity");
    assert_eq!(operation.as_str(), "consumer.session:compact-1");
    let error = SessionOperationId::new("contains whitespace").expect_err("invalid operation");
    assert_eq!(error.kind(), AgentSessionsErrorKind::InvalidInput);

    let locator = SessionLocator::new(
        "00000000-0000-0000-0000-000000000101"
            .parse::<SessionId>()
            .expect("session id"),
        "00000000-0000-0000-0000-000000000102"
            .parse::<BranchId>()
            .expect("branch id"),
        "00000000-0000-0000-0000-000000000103"
            .parse::<plakous::agent::CheckpointId>()
            .expect("checkpoint id"),
    );
    assert_eq!(
        locator
            .to_string()
            .parse::<SessionLocator>()
            .expect("locator"),
        locator
    );
}

/// The change feed is supported inventory under `plakous::sessions::feed`,
/// with the `feed_seq` accessors on acknowledgements, turn results, events
/// and the compact, fork and rebind outcomes.
/// Removed receipt methods and request/result names have negative compile pins
/// in the `sessions` module documentation; publication is observed here instead.
#[test]
fn change_feed_replaces_receipt_reconciliation_in_supported_inventory() {
    use plakous::agent::{AgentEvent, CommandAck, TurnResult};
    use plakous::model::SessionId;
    use plakous::sessions::feed::{
        FeedBranchTransition, FeedCall, FeedCallOutcome, FeedCallRequest, FeedCallTarget,
        FeedCallerLineage, FeedCancelCause, FeedChange, FeedCommandKind, FeedCompactReason,
        FeedContextLimit, FeedCursor, FeedFact, FeedHeadKind, FeedHeader, FeedItemsOutcome,
        FeedItemsRequest, FeedJobTerminal, FeedLocator, FeedOperationKind, FeedOperationState,
        FeedPage, FeedPageRequest, FeedRecord, FeedSeq, FeedSubscription, FeedTerminal,
        FeedTerminalOutcome, FeedTerminalRequest, FeedTurnOutcome, FeedWorkflowKind,
        FeedWorkflowOutcome, MAX_FEED_ITEMS, MAX_FEED_PAGE_BYTES, MAX_FEED_PAGE_RECORDS,
    };
    use plakous::sessions::{
        AgentSessions, AgentSessionsErrorKind, CompactSessionOutcome, ForkSessionOutcome,
        RebindSessionOutcome, SessionBackend,
    };

    fn hold<T: ?Sized>(_: &T) {}
    hold(&std::any::type_name::<FeedBranchTransition>());
    hold(&std::any::type_name::<FeedCall>());
    hold(&std::any::type_name::<FeedCallOutcome>());
    hold(&std::any::type_name::<FeedCallRequest>());
    hold(&std::any::type_name::<FeedCallTarget>());
    hold(&std::any::type_name::<FeedCallerLineage>());
    hold(&std::any::type_name::<FeedCancelCause>());
    hold(&std::any::type_name::<FeedChange>());
    hold(&std::any::type_name::<FeedCommandKind>());
    hold(&std::any::type_name::<FeedCompactReason>());
    hold(&std::any::type_name::<FeedContextLimit>());
    hold(&std::any::type_name::<FeedFact>());
    hold(&std::any::type_name::<FeedHeadKind>());
    hold(&std::any::type_name::<FeedHeader>());
    hold(&std::any::type_name::<FeedItemsOutcome>());
    hold(&std::any::type_name::<FeedItemsRequest>());
    hold(&std::any::type_name::<FeedJobTerminal>());
    hold(&std::any::type_name::<FeedLocator>());
    hold(&std::any::type_name::<FeedOperationKind>());
    hold(&std::any::type_name::<FeedOperationState>());
    hold(&std::any::type_name::<FeedRecord>());
    hold(&std::any::type_name::<FeedSubscription>());
    hold(&std::any::type_name::<FeedTerminal>());
    hold(&std::any::type_name::<FeedTerminalOutcome>());
    hold(&std::any::type_name::<FeedTerminalRequest>());
    hold(&std::any::type_name::<FeedTurnOutcome>());
    hold(&std::any::type_name::<FeedWorkflowKind>());
    hold(&std::any::type_name::<FeedWorkflowOutcome>());
    assert_eq!(MAX_FEED_PAGE_RECORDS, 256);
    assert_eq!(MAX_FEED_PAGE_BYTES, 1024 * 1024);
    assert_eq!(MAX_FEED_ITEMS, 4_096);

    fn seq_accessors(
        ack: &CommandAck,
        result: &TurnResult,
        event: &AgentEvent,
        compacted: &CompactSessionOutcome,
        forked: &ForkSessionOutcome,
        rebound: &RebindSessionOutcome,
    ) -> [Option<FeedSeq>; 6] {
        [
            ack.feed_seq(),
            result.feed_seq(),
            event.feed_seq(),
            compacted.feed_seq(),
            forked.feed_seq(),
            rebound.feed_seq(),
        ]
    }
    hold(&seq_accessors);

    #[cfg(feature = "sessions-sqlite")]
    {
        hold(&|path: &std::path::Path,
               page: FeedPageRequest,
               items: &FeedItemsRequest,
               call: &FeedCallRequest,
               terminal: &FeedTerminalRequest| {
            let _ = AgentSessions::inspect_feed_sqlite(path, page);
            let _ = AgentSessions::inspect_feed_items_sqlite(path, items);
            let _ = AgentSessions::inspect_feed_call_sqlite(path, call);
            let _ = AgentSessions::inspect_feed_terminal_sqlite(path, terminal);
        });
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let sessions =
        AgentSessions::new(SessionBackend::memory().expect("memory backend lease clock"))
            .expect("memory facade backend");
    let unknown = "00000000-0000-0000-0000-00000000feed"
        .parse::<SessionId>()
        .expect("session id");
    let page =
        runtime.block_on(sessions.feed_page(FeedPageRequest::after(FeedCursor::start(unknown))));
    let refused: Result<FeedPage, _> = page;
    assert_eq!(
        refused.expect_err("unknown session").kind(),
        AgentSessionsErrorKind::NotFound
    );
    let subscribed = runtime.block_on(sessions.subscribe_feed(unknown));
    assert_eq!(
        subscribed.expect_err("unknown session").kind(),
        AgentSessionsErrorKind::NotFound
    );
}

#[cfg(feature = "sessions-sqlite")]
#[test]
fn sqlite_persistence_admission_distinguishes_unsupported_from_corrupt() {
    use plakous::sessions::{AgentSessionsErrorKind, SessionBackend};

    let unsupported = tempfile::tempdir().expect("unsupported fixture");
    let unsupported_path = unsupported.path().join("runtime.sqlite3");
    let connection = rusqlite::Connection::open(&unsupported_path).expect("predecessor fixture");
    connection
        .pragma_update(None, "application_id", 0x504c_4b35_i64)
        .expect("predecessor application identity");
    connection
        .pragma_update(None, "user_version", 13_i64)
        .expect("predecessor schema identity");
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .expect("predecessor durability pragmas");
    drop(connection);
    assert_eq!(
        SessionBackend::sqlite(&unsupported_path)
            .expect_err("predecessor persistence must fail closed")
            .kind(),
        AgentSessionsErrorKind::IncompatibleBuild
    );

    let corrupt = tempfile::tempdir().expect("corrupt fixture");
    let corrupt_path = corrupt.path().join("runtime.sqlite3");
    std::fs::write(&corrupt_path, b"not a sqlite database").expect("corrupt bytes");
    assert_eq!(
        SessionBackend::sqlite(&corrupt_path)
            .expect_err("corrupt persistence must fail closed")
            .kind(),
        AgentSessionsErrorKind::Integrity
    );
}

/// Event types used for TUI transcript / cursor resume must stay on the facade.
///
/// Wire serde round-trips: `agent_event_and_cursor_serde_round_trip` below.
#[test]
fn transcript_cursor_types_remain_on_curated_agent_surface() {
    use plakous::agent::{
        AgentEvent, AgentEventKind, EventCursor, EventSequence, EventSubscription, TurnControl,
        TurnEvents,
    };
    assert_ne!(std::any::type_name::<AgentEvent>(), "");
    assert_ne!(std::any::type_name::<AgentEventKind>(), "");
    assert_ne!(std::any::type_name::<EventCursor>(), "");
    assert_ne!(std::any::type_name::<EventSequence>(), "");
    assert_ne!(std::any::type_name::<TurnEvents>(), "");
    assert_ne!(std::any::type_name::<TurnControl>(), "");
    assert_ne!(std::any::type_name::<EventSubscription>(), "");
}

/// Serde round-trip for transcript affordances (claimed by facade freeze docs).
#[test]
fn agent_event_and_cursor_serde_round_trip() {
    use plakous::agent::{AgentEventKind, EventSequence, EventVisibility};
    let kind = AgentEventKind::TurnStarted;
    let json = serde_json::to_string(&kind).expect("serialize kind");
    let back: AgentEventKind = serde_json::from_str(&json).expect("deserialize kind");
    assert_eq!(
        serde_json::to_value(&kind).unwrap(),
        serde_json::to_value(&back).unwrap()
    );

    let seq = EventSequence::default();
    let json = serde_json::to_string(&seq).expect("serialize seq");
    let back: EventSequence = serde_json::from_str(&json).expect("deserialize seq");
    assert_eq!(seq, back);

    let vis = EventVisibility::default();
    let json = serde_json::to_string(&vis).expect("serialize vis");
    let back: EventVisibility = serde_json::from_str(&json).expect("deserialize vis");
    assert_eq!(vis, back);
}

/// Retired existing-login surface must stay compile-fail closed.
///
/// ```compile_fail
/// use plakous::auth::ExistingLoginAccess;
/// ```
#[cfg(feature = "auth")]
#[test]
fn auth_module_exists_without_existing_login() {
    use plakous::auth::{CredentialGeneration, SecretString};
    assert_ne!(std::any::type_name::<CredentialGeneration>(), "");
    assert_ne!(std::any::type_name::<SecretString>(), "");
}

/// Telemetry is on the curated facade for content-free embedder observation.
///
/// `AgentBuilder::telemetry` takes `Arc<TelemetryEmitter>`; sinks implement
/// [`plakous::agent::TelemetrySink`].
#[test]
fn curated_agent_telemetry_surface_is_importable() {
    use plakous::agent::{
        TelemetryContext, TelemetryEmitter, TelemetryObservation, TelemetryObservationKind,
        TelemetrySink, TelemetrySinkError, TelemetryStatus, TelemetryTerminalStatus,
    };

    assert_ne!(std::any::type_name::<TelemetryContext>(), "");
    assert_ne!(std::any::type_name::<TelemetryEmitter>(), "");
    assert_ne!(std::any::type_name::<TelemetryObservation>(), "");
    assert_ne!(std::any::type_name::<TelemetryObservationKind>(), "");
    assert_ne!(std::any::type_name::<dyn TelemetrySink>(), "");
    assert_ne!(std::any::type_name::<TelemetrySinkError>(), "");
    assert_ne!(std::any::type_name::<TelemetryStatus>(), "");
    assert_ne!(std::any::type_name::<TelemetryTerminalStatus>(), "");
    // Default emitter is disabled; product paths opt in explicitly.
    let _ = TelemetryEmitter::disabled();
}

/// Provider namespaces are explicit facade inventories. Nameability means the
/// integration is supported at the stated qualification level; it never turns
/// offline-only API lanes into admitted or exact live-qualified routes.
///
/// Bare colliding names must **not** reappear under `plakous::openai`.
///
/// ```compile_fail
/// use plakous::openai::SecretString;
/// ```
///
/// ```compile_fail
/// use plakous::openai::ConfigError;
/// ```
///
/// ```compile_fail
/// use plakous::openai::AuthError;
/// ```
#[cfg(feature = "openai")]
#[test]
fn curated_provider_namespaces_are_nameable_without_unqualified_lanes() {
    use plakous::anthropic::{
        ANTHROPIC_API_VERSION, AnthropicMessagesDialect, AnthropicPlatformCapabilityProfile,
        AnthropicPlatformModelConfig, AnthropicPlatformModelResolver,
    };
    use plakous::openai::{
        CodexSubscriptionRouteConfig, OPENAI_CHATGPT_ORIGIN, OPENAI_CODEX_RESPONSES_PATH,
        OpenAiAccessMode, OpenAiAuthError, OpenAiConfigError, OpenAiError, OpenAiErrorPhase,
        OpenAiRetryClass, OpenAiRoute, OpenAiTarget,
    };
    use plakous::openrouter::{
        OpenRouterApiKey, OpenRouterApiKeySource, OpenRouterAuthError,
        OpenRouterCatalogCredentialRequest, OpenRouterChatClient, OpenRouterChatConnectionError,
        OpenRouterChatCredentialRequest, OpenRouterChatDiscoveryConfig,
        OpenRouterChatDiscoveryError, OpenRouterChatEndpointCandidate, OpenRouterChatModelEvidence,
        OpenRouterChatProfile, OpenRouterChatReasoningAlias, OpenRouterChatReasoningReplay,
        OpenRouterCredentialBinding, OpenRouterCredentialRequest, OpenRouterReasoningFacts,
        OpenRouterRetentionPolicy, OpenRouterRouteError, OpenRouterToolChoiceModes,
        StaticOpenRouterApiKeySource,
    };
    use plakous::xai::{
        GrokCodingProfile, SUPERGROK_RESPONSES_ORIGIN, SuperGrokResponsesDialect,
        XaiGrokSubscriptionRoute, XaiModel, XaiResolvedModel,
    };

    fn hold<T: ?Sized>(_: &T) {}
    hold(&std::any::type_name::<CodexSubscriptionRouteConfig>());
    hold(&OPENAI_CHATGPT_ORIGIN);
    hold(&OPENAI_CODEX_RESPONSES_PATH);
    hold(&std::any::type_name::<OpenAiAccessMode>());
    hold(&std::any::type_name::<OpenAiAuthError>());
    hold(&std::any::type_name::<OpenAiConfigError>());
    hold(&std::any::type_name::<OpenAiError>());
    hold(&std::any::type_name::<OpenAiErrorPhase>());
    hold(&std::any::type_name::<OpenAiRetryClass>());
    hold(&std::any::type_name::<OpenAiRoute>());
    hold(&std::any::type_name::<OpenAiTarget>());
    hold(&ANTHROPIC_API_VERSION);
    hold(&std::any::type_name::<AnthropicMessagesDialect>());
    hold(&std::any::type_name::<AnthropicPlatformCapabilityProfile>());
    hold(&std::any::type_name::<AnthropicPlatformModelConfig>());
    hold(&std::any::type_name::<AnthropicPlatformModelResolver>());
    hold(&std::any::type_name::<OpenRouterApiKey>());
    hold(&std::any::type_name::<dyn OpenRouterApiKeySource>());
    hold(&std::any::type_name::<OpenRouterAuthError>());
    hold(&std::any::type_name::<OpenRouterCatalogCredentialRequest<'_>>());
    hold(&std::any::type_name::<OpenRouterChatClient>());
    hold(&std::any::type_name::<OpenRouterChatConnectionError>());
    hold(&std::any::type_name::<OpenRouterChatCredentialRequest<'_>>());
    hold(&std::any::type_name::<OpenRouterChatDiscoveryConfig>());
    hold(&std::any::type_name::<OpenRouterChatDiscoveryError>());
    hold(&std::any::type_name::<OpenRouterChatEndpointCandidate>());
    hold(&std::any::type_name::<OpenRouterChatModelEvidence>());
    hold(&std::any::type_name::<OpenRouterReasoningFacts>());
    hold(&std::any::type_name::<OpenRouterToolChoiceModes>());
    hold(&std::any::type_name::<OpenRouterChatProfile>());
    hold(&std::any::type_name::<OpenRouterChatReasoningAlias>());
    hold(&std::any::type_name::<OpenRouterChatReasoningReplay>());
    hold(&std::any::type_name::<OpenRouterCredentialBinding>());
    hold(&std::any::type_name::<OpenRouterCredentialRequest<'_>>());
    hold(&std::any::type_name::<OpenRouterRetentionPolicy>());
    hold(&std::any::type_name::<OpenRouterRouteError>());
    hold(&std::any::type_name::<StaticOpenRouterApiKeySource>());
    hold(&std::any::type_name::<GrokCodingProfile>());
    hold(&SUPERGROK_RESPONSES_ORIGIN);
    hold(&std::any::type_name::<SuperGrokResponsesDialect>());
    hold(&std::any::type_name::<XaiGrokSubscriptionRoute>());
    hold(&std::any::type_name::<XaiModel>());
    hold(&std::any::type_name::<XaiResolvedModel>());
}

#[cfg(feature = "openrouter")]
#[test]
fn openrouter_facade_is_custom_bounded_and_redacted() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;

    use plakous::model::IntegrationClass;
    use plakous::openrouter::{
        OpenRouterApiKey, OpenRouterApiKeyGenerationGrant, OpenRouterApiKeySource,
        OpenRouterAuthError, OpenRouterCatalogCredentialRequest, OpenRouterChatClient,
        OpenRouterChatCredentialRequest, OpenRouterCredentialBinding, OpenRouterCredentialRequest,
        StaticOpenRouterApiKeySource,
    };

    const SECRET: &str = "facade-openrouter-secret-that-must-not-appear";

    struct FacadeOnlySource {
        binding: OpenRouterCredentialBinding,
        generation: OpenRouterApiKeyGenerationGrant,
    }

    impl OpenRouterApiKeySource for FacadeOnlySource {
        fn credential_binding(&self) -> Option<&OpenRouterCredentialBinding> {
            Some(&self.binding)
        }

        fn resolve(
            &self,
            _request: &OpenRouterCredentialRequest<'_>,
        ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
            OpenRouterApiKey::new(SECRET)
        }

        fn resolve_chat(
            &self,
            _request: &OpenRouterChatCredentialRequest<'_>,
        ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
            OpenRouterApiKey::new(SECRET)
        }

        fn resolve_catalog(
            &self,
            _request: &OpenRouterCatalogCredentialRequest<'_>,
        ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
            OpenRouterApiKey::new(SECRET)
        }

        fn current_generation_grant(
            &self,
        ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
            Ok(self.generation)
        }

        fn authority_id(&self) -> &'static str {
            "facade-only-authority"
        }
    }

    let source = Arc::new(StaticOpenRouterApiKeySource::new(
        OpenRouterApiKey::new(SECRET)?,
        "facade-test-account",
        "facade-test-authority",
    )?);
    assert!(!format!("{source:?}").contains(SECRET));

    let connection = OpenRouterChatClient::connect(source)?;
    assert_eq!(connection.integration_class(), IntegrationClass::Custom);
    assert!(!format!("{connection:?}").contains(SECRET));

    let key = OpenRouterApiKey::new(SECRET)?;
    let facade_only = Arc::new(FacadeOnlySource {
        binding: OpenRouterCredentialBinding::platform(
            "facade-only-account",
            "facade-only-authority",
        )?,
        generation: key.generation_grant(),
    });
    let connection = OpenRouterChatClient::connect(facade_only)?;
    assert_eq!(connection.integration_class(), IntegrationClass::Custom);
    Ok(())
}

#[cfg(all(feature = "native-subscriptions", feature = "openai"))]
#[test]
fn curated_subscription_models_are_nameable() {
    use plakous::model::ReasoningEffort;
    use plakous::subscriptions::{
        NativeModelSummary, NativeModelVisibility, NativeToolMode, NativeToolModeRequirementError,
        ResolvedProvider,
    };

    assert_ne!(
        std::any::type_name::<plakous::openai::OpenAiLanguageModel>(),
        ""
    );
    assert_ne!(std::any::type_name::<plakous::xai::XaiLanguageModel>(), "");
    let reasoning_efforts: fn(&NativeModelSummary) -> &[ReasoningEffort] =
        NativeModelSummary::reasoning_efforts;
    let visibility: fn(&NativeModelSummary) -> NativeModelVisibility =
        NativeModelSummary::visibility;
    let catalog_tool_mode: fn(&NativeModelSummary) -> Option<NativeToolMode> =
        NativeModelSummary::native_tool_mode;
    let tool_mode: fn(&ResolvedProvider) -> Option<NativeToolMode> =
        ResolvedProvider::native_tool_mode;
    let require_tool_mode: fn(
        &ResolvedProvider,
        NativeToolMode,
    ) -> Result<(), NativeToolModeRequirementError> = ResolvedProvider::require_native_tool_mode;
    let _ = reasoning_efforts;
    let _ = visibility;
    let _ = catalog_tool_mode;
    let _ = tool_mode;
    let _ = require_tool_mode;
}
