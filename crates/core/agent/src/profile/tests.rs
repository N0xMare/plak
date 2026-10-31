use super::*;
use plakous_model::{
    Digest, DigestDomain, EffectiveProviderConfigurationId, SelectedProviderConfigurationId,
};

fn provider_configuration_ids(
    tag: &[u8],
) -> (
    SelectedProviderConfigurationId,
    EffectiveProviderConfigurationId,
) {
    let typed = |domain| {
        let digest = Digest::sha256(domain, 5, &[tag]).expect("configuration identity");
        serde_json::to_value(digest).expect("identity wire")
    };
    (
        serde_json::from_value(typed(DigestDomain::SelectedProviderConfiguration))
            .expect("selected identity"),
        serde_json::from_value(typed(DigestDomain::EffectiveProviderConfiguration))
            .expect("effective identity"),
    )
}

fn model_facts(support: FeatureSupportFact) -> CompilerModelFacts {
    let (selected, effective) = provider_configuration_ids(b"configuration");
    CompilerModelFacts::new(
        selected,
        effective,
        ProtocolId::new("test.protocol").expect("protocol"),
        RouteId::new("test.route").expect("route"),
        ModelId::new("test-model").expect("model"),
        ModelAdapterProfileId::new("test.adapter.profile").expect("adapter"),
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"binding"]).expect("binding"),
        support,
        support,
        support,
    )
}

fn tool_facts(names: &[&str]) -> CompilerToolFacts {
    CompilerToolFacts::new(
        Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"registry"]).expect("tools"),
        names.iter().map(|name| (*name).to_owned()).collect(),
    )
    .expect("tool facts")
}

fn runtime_facts(require_all: bool) -> CompilerRuntimeFacts {
    CompilerRuntimeFacts::new(
        Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"env"]).expect("env"),
        MAX_INSTRUCTION_BYTES,
        require_all,
        require_all,
        require_all,
    )
}

fn inputs(support: FeatureSupportFact, require_all: bool, tools: &[&str]) -> HarnessCompilerInputs {
    HarnessCompilerInputs::new(
        model_facts(support),
        tool_facts(tools),
        runtime_facts(require_all),
        Vec::new(),
    )
    .expect("inputs")
}

fn inputs_with_sources(
    support: FeatureSupportFact,
    sources: Vec<ResolvedContextSourceFact>,
) -> HarnessCompilerInputs {
    HarnessCompilerInputs::new(
        model_facts(support),
        tool_facts(&[]),
        runtime_facts(false),
        sources,
    )
    .expect("inputs")
}

fn synthetic_spec(id: &str, reasoning: ReasoningPreferenceStrategy) -> HarnessProfileSpec {
    let mut builder = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new(id).expect("id"))
        .base_instructions("be precise")
        .tool_exposure_strategy(ToolExposureStrategy::CanonicalAll)
        .continuation_strategy(ContinuationPreferenceStrategy::PreferNative)
        .cache_strategy(CachePreferenceStrategy::PreferStablePrefix)
        .reasoning_strategy(reasoning);
    if reasoning == ReasoningPreferenceStrategy::PreferEffort {
        builder = builder.preferred_reasoning_effort(Some(ReasoningEffort::Medium));
    }
    builder.build().expect("spec")
}

#[test]
fn compile_is_deterministic_for_identical_inputs() {
    let compiler = HarnessProfileCompiler::new();
    let spec = synthetic_spec("plan.a", ReasoningPreferenceStrategy::PreferEffort);
    let inputs = inputs(
        FeatureSupportFact::Supported,
        false,
        &["read_file", "shell"],
    );
    let left = compiler.compile(&spec, &inputs).expect("left");
    let right = compiler.compile(&spec, &inputs).expect("right");
    assert_eq!(left.plan_digest(), right.plan_digest());
    assert_eq!(left.profile_digest(), right.profile_digest());
    assert_eq!(
        left.instructions().composed_instructions(),
        right.instructions().composed_instructions()
    );
}

#[test]
fn plan_model_binding_still_compares_every_identity_axis_exactly() {
    let spec = synthetic_spec("plan.exact-binding", ReasoningPreferenceStrategy::Disabled);
    let inputs = inputs(FeatureSupportFact::Supported, false, &[]);
    let plan = HarnessProfileCompiler::new()
        .compile(&spec, &inputs)
        .expect("plan");
    let model = inputs.model().clone();
    assert_eq!(plan.ensure_model_binding(&model), Ok(()));
    for axis in [
        "route",
        "model",
        "protocol",
        "adapter",
        "binding",
        "selected",
        "effective",
    ] {
        let mut changed = model.clone();
        match axis {
            "route" => changed.route = RouteId::new("different.route").expect("route"),
            "model" => changed.model = ModelId::new("different-model").expect("model"),
            "protocol" => {
                changed.protocol = ProtocolId::new("different.protocol").expect("protocol");
            }
            "adapter" => {
                changed.model_adapter_profile =
                    ModelAdapterProfileId::new("different.adapter").expect("adapter");
            }
            "binding" => {
                changed.model_binding_digest =
                    Digest::sha256(DigestDomain::ModelBinding, 1, &[b"different-binding"])
                        .expect("binding");
            }
            "selected" => {
                changed.selected_provider_configuration_id =
                    provider_configuration_ids(b"different").0;
            }
            _ => {
                changed.effective_provider_configuration_id =
                    provider_configuration_ids(b"different").1;
            }
        }
        let recorded = |value: &str, current: &str| (value.to_owned(), current.to_owned());
        let expected = match axis {
            "route" => {
                let (recorded, current) = recorded(model.route().as_str(), "different.route");
                crate::BindingChange::Route { recorded, current }
            }
            "model" => {
                let (recorded, current) = recorded(model.model.as_str(), "different-model");
                crate::BindingChange::Model { recorded, current }
            }
            "protocol" => {
                let (recorded, current) = recorded(model.protocol.as_str(), "different.protocol");
                crate::BindingChange::Protocol { recorded, current }
            }
            "adapter" => {
                let (recorded, current) =
                    recorded(model.model_adapter_profile().as_str(), "different.adapter");
                crate::BindingChange::AdapterProfile { recorded, current }
            }
            "binding" => crate::BindingChange::ModelCapabilities,
            _ => crate::BindingChange::ProviderConfiguration,
        };
        let refusal = plan.ensure_model_binding(&changed);
        assert_eq!(
            refusal,
            Err(ProfileError::PlanModelBindingMismatch {
                change: expected.clone()
            }),
            "{axis}"
        );
        assert_eq!(
            refusal.err().map(|error| error.to_string()),
            Some(format!(
                "resolved harness plan is bound to a different {expected}"
            )),
            "{axis}"
        );
    }
}

#[test]
fn plan_model_binding_names_the_adapter_revision_change_in_words() {
    let change = crate::BindingChange::AdapterProfile {
        recorded: "anthropic.claude.oauth.v8".to_owned(),
        current: "anthropic.claude.oauth.v9".to_owned(),
    };
    assert_eq!(
        ProfileError::PlanModelBindingMismatch { change }.to_string(),
        "resolved harness plan is bound to a different model adapter profile (recorded `anthropic.claude.oauth.v8`, this build `anthropic.claude.oauth.v9`)"
    );
}

#[test]
fn provider_configuration_ids_are_required_plan_authority() {
    let compiler = HarnessProfileCompiler::new();
    let spec = synthetic_spec("plan.configuration", ReasoningPreferenceStrategy::Disabled);
    let baseline_inputs = inputs(FeatureSupportFact::Supported, false, &[]);
    let baseline = compiler.compile(&spec, &baseline_inputs).expect("baseline");
    assert_eq!(
        baseline.selected_provider_configuration_id(),
        baseline_inputs.selected_provider_configuration_id()
    );
    assert_eq!(
        baseline.effective_provider_configuration_id(),
        baseline_inputs.effective_provider_configuration_id()
    );

    let (other_selected, other_effective) = provider_configuration_ids(b"other");
    let mut selected_model = model_facts(FeatureSupportFact::Supported);
    selected_model.selected_provider_configuration_id = other_selected;
    let selected_inputs = HarnessCompilerInputs::new(
        selected_model,
        tool_facts(&[]),
        runtime_facts(false),
        Vec::new(),
    )
    .expect("selected inputs");
    let selected_plan = compiler
        .compile(&spec, &selected_inputs)
        .expect("selected plan");
    assert_ne!(baseline.plan_digest(), selected_plan.plan_digest());
    assert_eq!(
        baseline.ensure_model_binding(selected_inputs.model()),
        Err(ProfileError::PlanModelBindingMismatch {
            change: crate::BindingChange::ProviderConfiguration
        })
    );

    let mut effective_model = model_facts(FeatureSupportFact::Supported);
    effective_model.effective_provider_configuration_id = other_effective;
    let effective_inputs = HarnessCompilerInputs::new(
        effective_model,
        tool_facts(&[]),
        runtime_facts(false),
        Vec::new(),
    )
    .expect("effective inputs");
    let effective_plan = compiler
        .compile(&spec, &effective_inputs)
        .expect("effective plan");
    assert_ne!(baseline.plan_digest(), effective_plan.plan_digest());
    assert_eq!(
        baseline.ensure_model_binding(effective_inputs.model()),
        Err(ProfileError::PlanModelBindingMismatch {
            change: crate::BindingChange::ProviderConfiguration
        })
    );

    let canonical_inputs = String::from_utf8(
        canonical_compiler_input_bytes(&baseline_inputs).expect("canonical inputs"),
    )
    .expect("utf8 inputs");
    let canonical_plan =
        String::from_utf8(canonical_plan_bytes(&baseline).expect("canonical plan"))
            .expect("utf8 plan");
    for field in [
        "selected_provider_configuration_id",
        "effective_provider_configuration_id",
    ] {
        assert!(canonical_inputs.contains(field));
        assert!(canonical_plan.contains(field));
    }
}

#[test]
fn request_affecting_change_changes_digests() {
    let compiler = HarnessProfileCompiler::new();
    let left_spec = synthetic_spec("plan.a", ReasoningPreferenceStrategy::PreferEffort);
    let right_spec = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("plan.a").expect("id"))
        .base_instructions("be precise and thorough")
        .reasoning_strategy(ReasoningPreferenceStrategy::PreferEffort)
        .preferred_reasoning_effort(Some(ReasoningEffort::Medium))
        .continuation_strategy(ContinuationPreferenceStrategy::PreferNative)
        .cache_strategy(CachePreferenceStrategy::PreferStablePrefix)
        .build()
        .expect("spec");
    let inputs = inputs(FeatureSupportFact::Supported, false, &["read_file"]);
    let left = compiler.compile(&left_spec, &inputs).expect("left");
    let right = compiler.compile(&right_spec, &inputs).expect("right");
    assert_ne!(left.profile_digest(), right.profile_digest());
    assert_ne!(left.plan_digest(), right.plan_digest());
}

#[test]
fn exact_context_content_is_model_visible_and_changes_plan_identity() {
    let spec = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("plan.context").expect("id"))
        .base_instructions("base policy")
        .instruction_strategy(InstructionCompositionStrategy::BaseThenSources)
        .instruction_sources(vec![
            ContextSourceSelector::new("repo.agents").expect("selector"),
        ])
        .build()
        .expect("spec");
    let exact_digest = Digest::sha256(DigestDomain::Blob, 1, &[b"exact source A"]).expect("digest");
    let immutable_reference =
        VerifiedContextSourceRef::new("object.repo-agents", exact_digest, "1").expect("reference");
    let left_source = ResolvedContextSourceFact::from_verified_reference(
        "repo.agents",
        "exact source A",
        "1",
        "test",
        immutable_reference,
    )
    .expect("source");
    let right_source = ResolvedContextSourceFact::new("repo.agents", "exact source B", "1", "test")
        .expect("source");
    let compiler = HarnessProfileCompiler::new();
    let left = compiler
        .compile(
            &spec,
            &inputs_with_sources(FeatureSupportFact::Supported, vec![left_source.clone()]),
        )
        .expect("left");
    let right = compiler
        .compile(
            &spec,
            &inputs_with_sources(FeatureSupportFact::Supported, vec![right_source]),
        )
        .expect("right");

    assert_eq!(
        left.instructions().composed_instructions(),
        "base policy\n\nexact source A"
    );
    assert_eq!(left.instructions().sources(), &[left_source]);
    assert_eq!(
        left.instructions().sources()[0]
            .immutable_reference()
            .expect("reference")
            .reference_id(),
        "object.repo-agents"
    );
    assert_eq!(
        left.instruction_blocks()
            .expect("instruction blocks")
            .first()
            .expect("model-visible block")
            .text(),
        "base policy\n\nexact source A"
    );
    assert!(
        !left
            .instructions()
            .composed_instructions()
            .contains("[source:")
    );
    assert_ne!(left.plan_digest(), right.plan_digest());
}

#[test]
fn duplicate_resolved_context_id_fails_closed() {
    let first = ResolvedContextSourceFact::new("repo.agents", "one", "1", "test").expect("first");
    let second = ResolvedContextSourceFact::new("repo.agents", "two", "2", "test").expect("second");
    let error = HarnessCompilerInputs::new(
        model_facts(FeatureSupportFact::Supported),
        tool_facts(&[]),
        runtime_facts(false),
        vec![first, second],
    )
    .expect_err("duplicate");
    assert!(matches!(error, ProfileError::DuplicateContextSource { .. }));
}

#[test]
fn aggregate_resolved_context_bound_fails_closed() {
    let content = "x".repeat((MAX_INSTRUCTION_BYTES / 2) + 1);
    let first = ResolvedContextSourceFact::new("repo.one", &content, "1", "test").expect("first");
    let second = ResolvedContextSourceFact::new("repo.two", content, "1", "test").expect("second");
    let error = HarnessCompilerInputs::new(
        model_facts(FeatureSupportFact::Supported),
        tool_facts(&[]),
        runtime_facts(false),
        vec![first, second],
    )
    .expect_err("aggregate bound");
    assert!(matches!(
        error,
        ProfileError::InstructionBudgetExceeded { .. }
    ));
}

#[test]
fn tampered_deserialized_context_content_fails_closed() {
    let source =
        ResolvedContextSourceFact::new("repo.agents", "trusted", "1", "test").expect("source");
    let mut value = serde_json::to_value(source).expect("serialize");
    value["content"] = serde_json::Value::String("tampered".to_owned());
    let tampered: ResolvedContextSourceFact =
        serde_json::from_value(value).expect("structural decode");
    let error = HarnessCompilerInputs::new(
        model_facts(FeatureSupportFact::Supported),
        tool_facts(&[]),
        runtime_facts(false),
        vec![tampered],
    )
    .expect_err("digest mismatch");
    assert!(matches!(
        error,
        ProfileError::ContextSourceDigestMismatch { .. }
    ));
}

#[test]
fn preferred_unsupported_features_downgrade_with_diagnostics() {
    let compiler = HarnessProfileCompiler::new();
    let spec = synthetic_spec("plan.soft", ReasoningPreferenceStrategy::PreferEffort);
    let plan = compiler
        .compile(&spec, &inputs(FeatureSupportFact::Unsupported, false, &[]))
        .expect("plan");
    assert!(!plan.reasoning().enabled());
    assert!(!plan.continuation().use_native_continuation());
    assert!(!plan.cache().prefer_stable_prefix());
    assert!(plan.diagnostics().len() >= 3);
}

#[test]
fn required_unsupported_features_fail_closed() {
    let compiler = HarnessProfileCompiler::new();
    let spec = synthetic_spec("plan.hard", ReasoningPreferenceStrategy::PreferEffort);
    let error = compiler
        .compile(&spec, &inputs(FeatureSupportFact::Unsupported, true, &[]))
        .expect_err("must fail");
    assert!(matches!(
        error,
        ProfileError::RequiredFeatureUnavailable { .. }
    ));
}

#[test]
fn deleted_profile_axes_are_rejected_as_unknown_fields() {
    let profile = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("serde.axes").expect("id"))
        .base_instructions("active")
        .build()
        .expect("profile");
    for (field, value) in [
        ("context_strategy", serde_json::json!("ManualOnly")),
        ("verbosity_strategy", serde_json::json!("Concise")),
        ("steering_template", serde_json::json!("steer")),
        ("recovery_template", serde_json::json!("recover")),
    ] {
        let mut wire = serde_json::to_value(&profile).expect("wire");
        wire.as_object_mut()
            .expect("profile object")
            .insert(field.to_owned(), value);
        let bytes = serde_json::to_vec(&wire).expect("bytes");
        let error = ProfileError::decode_spec(&bytes).expect_err("deleted field");
        assert!(matches!(error, ProfileError::InvalidJson { .. }));
    }
}

#[test]
fn canonical_encodings_omit_deferred_axes() {
    let spec = synthetic_spec("serde.active", ReasoningPreferenceStrategy::Disabled);
    let inputs = inputs(FeatureSupportFact::Supported, false, &[]);
    let plan = HarnessProfileCompiler::new()
        .compile(&spec, &inputs)
        .expect("plan");
    let plan_encoding =
        String::from_utf8(canonical_plan_bytes(&plan).expect("plan bytes")).expect("utf8");
    let encoded = [
        serde_json::to_string(&spec).expect("profile"),
        serde_json::to_string(&inputs).expect("inputs"),
        plan_encoding.clone(),
    ]
    .join("\n");

    for deleted in [
        "context_strategy",
        "verbosity_strategy",
        "steering_template",
        "recovery_template",
        "automatic_compaction_support",
        "require_automatic_compaction",
        "\"context\":",
        "\"output\":",
    ] {
        assert!(!encoded.contains(deleted), "retained {deleted}");
    }
    assert!(encoded.contains("continuation_strategy"));
    assert!(encoded.contains("cache_strategy"));
    assert!(plan_encoding.contains("\"use_native_continuation\":true"));
    assert!(plan_encoding.contains("\"prefer_stable_prefix\":true"));
}

#[test]
fn profiles_cannot_satisfy_model_adapter_or_subscription_identities() {
    // Compile-time non-coercion is structural: the three ID types are distinct.
    // Runtime proof: digests and plan bindings keep model-adapter identity separate
    // from harness-profile identity even when display strings collide.
    let harness = HarnessProfileId::new("same-display-name").expect("harness");
    let adapter = ModelAdapterProfileId::new("same-display-name").expect("adapter");
    assert_eq!(harness.as_str(), adapter.as_str());
    let compiler = HarnessProfileCompiler::new();
    let spec = synthetic_spec(harness.as_str(), ReasoningPreferenceStrategy::Disabled);
    let (selected, effective) = provider_configuration_ids(b"configuration");
    let model = CompilerModelFacts::new(
        selected,
        effective,
        ProtocolId::new("test.protocol").expect("protocol"),
        RouteId::new("test.route").expect("route"),
        ModelId::new("test-model").expect("model"),
        adapter.clone(),
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"binding"]).expect("binding"),
        FeatureSupportFact::Supported,
        FeatureSupportFact::Supported,
        FeatureSupportFact::Supported,
    );
    let inputs = HarnessCompilerInputs::new(
        model,
        tool_facts(&["read_file"]),
        runtime_facts(false),
        Vec::new(),
    )
    .expect("inputs");
    let plan = compiler.compile(&spec, &inputs).expect("plan");
    assert_eq!(plan.profile_id().as_str(), harness.as_str());
    assert_eq!(
        plan.model_facts().model_adapter_profile().as_str(),
        adapter.as_str()
    );
    // Distinct digest domains prove the plan is not a model-adapter binding.
    assert_eq!(
        plan.profile_digest().domain(),
        DigestDomain::HarnessProfileSpec
    );
    assert_eq!(
        plan.plan_digest().domain(),
        DigestDomain::ResolvedHarnessPlan
    );
    assert_eq!(
        plan.model_facts().model_adapter_profile().as_str(),
        plan.profile_id().as_str()
    );
    // Equal display strings do not make the identities interchangeable types.
    let _: &HarnessProfileId = plan.profile_id();
    let _: &ModelAdapterProfileId = plan.model_facts().model_adapter_profile();
}

#[test]
fn tool_override_for_unknown_canonical_tool_fails() {
    let compiler = HarnessProfileCompiler::new();
    let spec = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("plan.tools").expect("id"))
        .base_instructions("tools")
        .tool_overrides(vec![
            ToolExposureOverride::new("missing_tool", "alias", None).expect("override"),
        ])
        .build()
        .expect("spec");
    let error = compiler
        .compile(
            &spec,
            &inputs(FeatureSupportFact::Supported, false, &["read_file"]),
        )
        .expect_err("unknown tool");
    assert!(matches!(error, ProfileError::UnknownCanonicalTool { .. }));
}

#[test]
fn tool_override_is_reconstruction_complete_and_requires_exposure() {
    let compiler = HarnessProfileCompiler::new();
    let override_entry =
        ToolExposureOverride::new("read_file", "read", Some("Read exactly one file".into()))
            .expect("override");
    let exposed = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("plan.exposed-tools").expect("id"))
        .base_instructions("tools")
        .tool_exposure_strategy(ToolExposureStrategy::CanonicalAll)
        .tool_overrides(vec![override_entry.clone()])
        .build()
        .expect("spec");
    let plan = compiler
        .compile(
            &exposed,
            &inputs(FeatureSupportFact::Supported, false, &["read_file"]),
        )
        .expect("plan");
    assert_eq!(plan.tools().exposed_names(), &["read".to_owned()]);
    assert_eq!(
        plan.tools().overrides(),
        std::slice::from_ref(&override_entry)
    );

    let hidden = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("plan.hidden-tools").expect("id"))
        .base_instructions("tools")
        .tool_exposure_strategy(ToolExposureStrategy::None)
        .tool_overrides(vec![override_entry])
        .build()
        .expect("spec");
    assert_eq!(
        compiler.compile(
            &hidden,
            &inputs(FeatureSupportFact::Supported, false, &["read_file"]),
        ),
        Err(ProfileError::InvalidToolOverride)
    );
}

#[test]
fn recommended_neutral_profile_compiles() {
    let profile = recommended_neutral_coding_profile().expect("recommended");
    let plan = HarnessProfileCompiler::new()
        .compile(
            &profile,
            &inputs(FeatureSupportFact::Supported, false, &["read_file"]),
        )
        .expect("compile");
    assert!(plan.reasoning().enabled());
    assert!(plan.continuation().use_native_continuation());
    assert!(!plan.cache().prefer_stable_prefix());
    assert_eq!(profile.cache_strategy(), CachePreferenceStrategy::Disabled);
    assert_eq!(plan.tools().exposed_names(), &["read_file".to_owned()]);
    assert_eq!(profile.provenance(), HarnessProfileProvenance::Recommended);
}

#[test]
fn preferred_reasoning_effort_override_replaces_profile_default() {
    let profile = recommended_neutral_coding_profile().expect("recommended");
    assert_eq!(
        profile.preferred_reasoning_effort(),
        Some(ReasoningEffort::High)
    );
    let overridden = profile
        .with_preferred_reasoning_effort(ReasoningEffort::XHigh)
        .expect("override");
    assert_eq!(
        overridden.preferred_reasoning_effort(),
        Some(ReasoningEffort::XHigh)
    );
    assert_eq!(
        overridden.reasoning_strategy(),
        ReasoningPreferenceStrategy::PreferEffort
    );
    assert_eq!(overridden.base_instructions(), profile.base_instructions());
    let plan = HarnessProfileCompiler::new()
        .compile(
            &overridden,
            &inputs(FeatureSupportFact::Supported, false, &["read_file"]),
        )
        .expect("compile");
    assert_eq!(
        plan.reasoning_policy(),
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::XHigh),
            requirement: CapabilityRequirement::Required,
        }
    );
}
