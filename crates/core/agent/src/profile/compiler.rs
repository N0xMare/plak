use super::{
    Arc, CATALOG_DIGEST_VERSION, CachePolicy, CachePreferenceStrategy, Capability,
    CompiledProviderConfiguration, CompilerModelFacts, CompilerRuntimeFacts, CompilerToolFacts,
    ContinuationPreferenceStrategy, Digest, DigestDomain, EffectiveProviderConfigurationId,
    FeatureSupportFact, HARNESS_PROFILE_COMPILER_REVISION, HARNESS_PROFILE_SCHEMA_VERSION,
    HARNESS_STRATEGY_CATALOG_REVISION, HarnessCompilerInputs, HarnessProfileCompiler,
    HarnessProfileId, HarnessProfileProvenance, HarnessProfileSpec, HarnessProfileSpecBuilder,
    InstructionBlock, InstructionCompositionStrategy, LanguageModel, MAX_DIAGNOSTICS,
    MAX_INSTRUCTION_BYTES, MAX_TOOL_EXPOSURE_OVERRIDES, ModelAdapterProfileId, ModelId,
    PLAN_DIGEST_VERSION, ProfileError, ProtocolId, ReasoningEffort, ReasoningEffortDisposition,
    ReasoningPolicy, ReasoningPreferenceStrategy, ResolvedCachePlan, ResolvedContextSourceFact,
    ResolvedContinuationPlan, ResolvedHarnessPlan, ResolvedHarnessPlanInner,
    ResolvedHarnessPlanSource, ResolvedInstructionPlan, ResolvedModelDescriptor,
    ResolvedReasoningPlan, ResolvedToolExposurePlan, RouteId, SafeDiagnostic,
    SelectedProviderConfigurationId, Support, ToolExposureStrategy, validate_context_sources,
    validate_decoded_compiler_inputs, validate_identifier,
};

impl CompilerModelFacts {
    /// Creates compiler model facts from an already-selected binding.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "compiler model facts are an explicit frozen input tuple"
    )]
    pub fn new(
        selected_provider_configuration_id: SelectedProviderConfigurationId,
        effective_provider_configuration_id: EffectiveProviderConfigurationId,
        protocol: ProtocolId,
        route: RouteId,
        model: ModelId,
        model_adapter_profile: ModelAdapterProfileId,
        model_binding_digest: Digest,
        reasoning_support: FeatureSupportFact,
        native_continuation_support: FeatureSupportFact,
        prompt_cache_support: FeatureSupportFact,
    ) -> Self {
        Self {
            selected_provider_configuration_id,
            effective_provider_configuration_id,
            protocol,
            route,
            model,
            model_adapter_profile,
            model_binding_digest,
            reasoning_support,
            native_continuation_support,
            prompt_cache_support,
        }
    }

    /// Returns the complete caller-selected provider-configuration identity.
    #[must_use]
    pub const fn selected_provider_configuration_id(&self) -> SelectedProviderConfigurationId {
        self.selected_provider_configuration_id
    }

    /// Returns the compiled effective provider-configuration identity.
    #[must_use]
    pub const fn effective_provider_configuration_id(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration_id
    }

    /// Returns the already selected route identity.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the model-adapter profile identity.
    #[must_use]
    pub const fn model_adapter_profile(&self) -> &ModelAdapterProfileId {
        &self.model_adapter_profile
    }
}

impl CompilerToolFacts {
    /// Creates tool facts from a frozen registry digest and names.
    ///
    /// # Errors
    ///
    /// Returns a profile error when a name is not portable or the list is oversized.
    pub fn new(
        registry_digest: Digest,
        canonical_tool_names: Vec<String>,
    ) -> Result<Self, ProfileError> {
        if canonical_tool_names.len() > MAX_TOOL_EXPOSURE_OVERRIDES {
            return Err(ProfileError::TooManyToolOverrides);
        }
        for name in &canonical_tool_names {
            if !validate_identifier(name) {
                return Err(ProfileError::InvalidToolOverride);
            }
        }
        Ok(Self {
            registry_digest,
            canonical_tool_names,
        })
    }

    /// Returns the registry digest.
    #[must_use]
    pub const fn registry_digest(&self) -> &Digest {
        &self.registry_digest
    }

    /// Returns canonical tool names.
    #[must_use]
    pub fn canonical_tool_names(&self) -> &[String] {
        &self.canonical_tool_names
    }
}

impl CompilerRuntimeFacts {
    /// Creates runtime facts.
    #[must_use]
    pub const fn new(
        execution_environment_digest: Digest,
        max_instruction_bytes: usize,
        require_reasoning: bool,
        require_native_continuation: bool,
        require_prompt_cache: bool,
    ) -> Self {
        Self {
            execution_environment_digest,
            max_instruction_bytes,
            require_reasoning,
            require_native_continuation,
            require_prompt_cache,
        }
    }
}

impl HarnessCompilerInputs {
    /// Creates compiler inputs.
    ///
    /// # Errors
    ///
    /// Returns a profile error when too many context sources are supplied.
    pub fn new(
        model: CompilerModelFacts,
        tools: CompilerToolFacts,
        runtime: CompilerRuntimeFacts,
        context_sources: Vec<ResolvedContextSourceFact>,
    ) -> Result<Self, ProfileError> {
        validate_context_sources(&context_sources)?;
        Ok(Self {
            selected_provider_configuration_id: model.selected_provider_configuration_id,
            effective_provider_configuration_id: model.effective_provider_configuration_id,
            model,
            tools,
            runtime,
            context_sources,
        })
    }

    /// Returns the complete caller-selected provider-configuration identity.
    #[must_use]
    pub const fn selected_provider_configuration_id(&self) -> SelectedProviderConfigurationId {
        self.selected_provider_configuration_id
    }

    /// Returns the compiled effective provider-configuration identity.
    #[must_use]
    pub const fn effective_provider_configuration_id(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration_id
    }

    /// Returns model facts.
    #[must_use]
    pub const fn model(&self) -> &CompilerModelFacts {
        &self.model
    }

    /// Returns tool facts.
    #[must_use]
    pub const fn tools(&self) -> &CompilerToolFacts {
        &self.tools
    }
}

impl HarnessProfileCompiler {
    /// Creates a compiler.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Compiles a data-only profile against frozen inputs.
    ///
    /// Compilation performs no I/O, credential access, discovery, clock read,
    /// persistence, or dispatch. Required unsupported behavior fails closed.
    /// Preferred unsupported behavior becomes an ordered diagnostic.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the specification is invalid for the inputs
    /// or a required behavior cannot be satisfied.
    pub fn compile(
        &self,
        spec: &HarnessProfileSpec,
        inputs: &HarnessCompilerInputs,
    ) -> Result<ResolvedHarnessPlan, ProfileError> {
        validate_decoded_compiler_inputs(inputs)?;
        if spec.schema_version != HARNESS_PROFILE_SCHEMA_VERSION {
            return Err(ProfileError::UnsupportedSchemaVersion {
                found: spec.schema_version,
            });
        }
        if spec.base_instructions.len() > inputs.runtime.max_instruction_bytes {
            return Err(ProfileError::InstructionBudgetExceeded {
                max_bytes: inputs.runtime.max_instruction_bytes,
            });
        }

        let mut diagnostics = Vec::new();
        let profile_digest = spec.content_digest()?;
        let catalog_digest = strategy_catalog_digest()?;

        let instructions = resolve_instructions(spec, inputs, &mut diagnostics)?;
        let tools = resolve_tools(spec, inputs)?;
        let reasoning = resolve_reasoning(spec, inputs, &mut diagnostics)?;
        let continuation = resolve_continuation(spec, inputs, &mut diagnostics)?;
        let cache = resolve_cache(spec, inputs, &mut diagnostics)?;

        if diagnostics.len() > MAX_DIAGNOSTICS {
            return Err(ProfileError::TooManyDiagnostics);
        }

        let inner = ResolvedHarnessPlanInner {
            profile_id: spec.id.clone(),
            profile_digest,
            plan_digest: Digest::sha256(
                DigestDomain::ResolvedHarnessPlan,
                PLAN_DIGEST_VERSION,
                &[],
            )
            .map_err(ProfileError::from)?, // placeholder replaced below
            catalog_digest,
            compiler_revision: HARNESS_PROFILE_COMPILER_REVISION,
            strategy_catalog_revision: HARNESS_STRATEGY_CATALOG_REVISION,
            schema_version: spec.schema_version,
            semantic_version: spec.semantic_version,
            provenance: spec.provenance,
            selected_provider_configuration_id: inputs.selected_provider_configuration_id,
            effective_provider_configuration_id: inputs.effective_provider_configuration_id,
            model: inputs.model.clone(),
            tool_registry_digest: inputs.tools.registry_digest,
            execution_environment_digest: inputs.runtime.execution_environment_digest,
            instructions,
            tools,
            reasoning,
            continuation,
            cache,
            diagnostics,
        };

        let plan_digest = plan_content_digest(&inner)?;
        let mut sealed = inner;
        sealed.plan_digest = plan_digest;

        Ok(ResolvedHarnessPlan {
            inner: Arc::new(sealed),
            source: Arc::new(ResolvedHarnessPlanSource {
                profile: spec.clone(),
                inputs: inputs.clone(),
            }),
        })
    }
}

fn resolve_instructions(
    spec: &HarnessProfileSpec,
    inputs: &HarnessCompilerInputs,
    diagnostics: &mut Vec<SafeDiagnostic>,
) -> Result<ResolvedInstructionPlan, ProfileError> {
    let mut sources = Vec::new();
    let mut composed = spec.base_instructions.clone();

    match spec.instruction_strategy {
        InstructionCompositionStrategy::BaseOnly => {
            if !spec.instruction_sources.is_empty() {
                diagnostics.push(SafeDiagnostic::new(
                    "instruction sources ignored under BaseOnly strategy",
                ));
            }
        }
        InstructionCompositionStrategy::BaseThenSources => {
            for selector in &spec.instruction_sources {
                let Some(fact) = inputs
                    .context_sources
                    .iter()
                    .find(|fact| fact.source_id() == selector.source_id())
                else {
                    return Err(ProfileError::UnresolvedContextSource {
                        source_id: selector.source_id().to_string(),
                    });
                };
                let separator_bytes = usize::from(!composed.is_empty()) * 2;
                let next_len = composed
                    .len()
                    .checked_add(separator_bytes)
                    .and_then(|length| length.checked_add(fact.content().len()))
                    .ok_or(ProfileError::InstructionBudgetExceeded {
                        max_bytes: inputs.runtime.max_instruction_bytes,
                    })?;
                if next_len > inputs.runtime.max_instruction_bytes {
                    return Err(ProfileError::InstructionBudgetExceeded {
                        max_bytes: inputs.runtime.max_instruction_bytes,
                    });
                }
                if separator_bytes != 0 {
                    composed.push_str("\n\n");
                }
                composed.push_str(fact.content());
                sources.push(fact.clone());
            }
        }
    }

    if composed.len() > inputs.runtime.max_instruction_bytes {
        return Err(ProfileError::InstructionBudgetExceeded {
            max_bytes: inputs.runtime.max_instruction_bytes,
        });
    }

    Ok(ResolvedInstructionPlan {
        strategy: spec.instruction_strategy,
        composed_instructions: composed,
        sources,
    })
}

fn resolve_tools(
    spec: &HarnessProfileSpec,
    inputs: &HarnessCompilerInputs,
) -> Result<ResolvedToolExposurePlan, ProfileError> {
    if spec.tool_exposure_strategy == ToolExposureStrategy::None && !spec.tool_overrides.is_empty()
    {
        return Err(ProfileError::InvalidToolOverride);
    }
    let mut exposed_names = Vec::new();
    match spec.tool_exposure_strategy {
        ToolExposureStrategy::None => {}
        ToolExposureStrategy::CanonicalAll => {
            exposed_names.extend(inputs.tools.canonical_tool_names.iter().cloned());
        }
    }

    for override_entry in &spec.tool_overrides {
        if !inputs
            .tools
            .canonical_tool_names
            .iter()
            .any(|name| name == override_entry.canonical_name())
        {
            return Err(ProfileError::UnknownCanonicalTool {
                name: override_entry.canonical_name().to_string(),
            });
        }
        if let Some(index) = exposed_names
            .iter()
            .position(|name| name == override_entry.canonical_name())
        {
            exposed_names[index] = override_entry.exposed_name().to_string();
        }
    }

    Ok(ResolvedToolExposurePlan {
        strategy: spec.tool_exposure_strategy,
        exposed_names,
        overrides: spec.tool_overrides.clone(),
    })
}

fn resolve_reasoning(
    spec: &HarnessProfileSpec,
    inputs: &HarnessCompilerInputs,
    diagnostics: &mut Vec<SafeDiagnostic>,
) -> Result<ResolvedReasoningPlan, ProfileError> {
    match spec.reasoning_strategy {
        ReasoningPreferenceStrategy::Disabled => {
            if inputs.runtime.require_reasoning {
                return Err(ProfileError::RequiredFeatureUnavailable {
                    feature: "reasoning",
                });
            }
            Ok(ResolvedReasoningPlan {
                enabled: false,
                effort: None,
            })
        }
        ReasoningPreferenceStrategy::PreferEffort => match inputs.model.reasoning_support {
            FeatureSupportFact::Supported => Ok(ResolvedReasoningPlan {
                enabled: true,
                effort: spec.preferred_reasoning_effort,
            }),
            FeatureSupportFact::Unknown | FeatureSupportFact::Unsupported => {
                if inputs.runtime.require_reasoning {
                    return Err(ProfileError::RequiredFeatureUnavailable {
                        feature: "reasoning",
                    });
                }
                diagnostics.push(SafeDiagnostic::new(
                    "preferred reasoning effort downgraded: model support is not available",
                ));
                Ok(ResolvedReasoningPlan {
                    enabled: false,
                    effort: None,
                })
            }
        },
    }
}

fn resolve_continuation(
    spec: &HarnessProfileSpec,
    inputs: &HarnessCompilerInputs,
    diagnostics: &mut Vec<SafeDiagnostic>,
) -> Result<ResolvedContinuationPlan, ProfileError> {
    match spec.continuation_strategy {
        ContinuationPreferenceStrategy::CanonicalReplayOnly => {
            if inputs.runtime.require_native_continuation {
                return Err(ProfileError::RequiredFeatureUnavailable {
                    feature: "native_continuation",
                });
            }
            Ok(ResolvedContinuationPlan {
                use_native_continuation: false,
            })
        }
        ContinuationPreferenceStrategy::PreferNative => {
            match inputs.model.native_continuation_support {
                FeatureSupportFact::Supported => Ok(ResolvedContinuationPlan {
                    use_native_continuation: true,
                }),
                FeatureSupportFact::Unknown | FeatureSupportFact::Unsupported => {
                    if inputs.runtime.require_native_continuation {
                        return Err(ProfileError::RequiredFeatureUnavailable {
                            feature: "native_continuation",
                        });
                    }
                    diagnostics.push(SafeDiagnostic::new(
                        "preferred native continuation downgraded to canonical replay",
                    ));
                    Ok(ResolvedContinuationPlan {
                        use_native_continuation: false,
                    })
                }
            }
        }
    }
}

fn resolve_cache(
    spec: &HarnessProfileSpec,
    inputs: &HarnessCompilerInputs,
    diagnostics: &mut Vec<SafeDiagnostic>,
) -> Result<ResolvedCachePlan, ProfileError> {
    match spec.cache_strategy {
        CachePreferenceStrategy::Disabled => {
            if inputs.runtime.require_prompt_cache {
                return Err(ProfileError::RequiredFeatureUnavailable {
                    feature: "prompt_cache",
                });
            }
            Ok(ResolvedCachePlan {
                prefer_stable_prefix: false,
            })
        }
        CachePreferenceStrategy::PreferStablePrefix => match inputs.model.prompt_cache_support {
            FeatureSupportFact::Supported => Ok(ResolvedCachePlan {
                prefer_stable_prefix: true,
            }),
            FeatureSupportFact::Unknown | FeatureSupportFact::Unsupported => {
                if inputs.runtime.require_prompt_cache {
                    return Err(ProfileError::RequiredFeatureUnavailable {
                        feature: "prompt_cache",
                    });
                }
                diagnostics.push(SafeDiagnostic::new(
                    "preferred prompt cache downgraded: model support is not available",
                ));
                Ok(ResolvedCachePlan {
                    prefer_stable_prefix: false,
                })
            }
        },
    }
}

fn strategy_catalog_digest() -> Result<Digest, ProfileError> {
    let parts: &[&[u8]] = &[
        b"instruction:BaseThenSources|BaseOnly",
        b"tools:CanonicalAll|None",
        b"reasoning:Disabled|PreferEffort",
        b"continuation:PreferNative|CanonicalReplayOnly",
        b"cache:PreferStablePrefix|Disabled",
        &HARNESS_STRATEGY_CATALOG_REVISION.to_be_bytes(),
    ];
    Digest::sha256(
        DigestDomain::HarnessStrategyCatalog,
        CATALOG_DIGEST_VERSION,
        parts,
    )
    .map_err(ProfileError::from)
}

fn plan_content_digest(inner: &ResolvedHarnessPlanInner) -> Result<Digest, ProfileError> {
    // Digest excludes plan_digest itself; use a zeroed placeholder field value.
    let mut for_digest = inner.clone();
    for_digest.plan_digest = Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        PLAN_DIGEST_VERSION,
        &[b"placeholder"],
    )
    .map_err(ProfileError::from)?;
    let bytes = serde_json::to_vec(&for_digest).map_err(|error| ProfileError::Serialization {
        reason: error.to_string(),
    })?;
    Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        PLAN_DIGEST_VERSION,
        &[
            b"resolved-harness-plan.v2",
            &bytes,
            &HARNESS_PROFILE_COMPILER_REVISION.to_be_bytes(),
        ],
    )
    .map_err(ProfileError::from)
}

/// Maps descriptor support into compiler support facts without inventing support.
#[must_use]
pub(crate) fn feature_support_from_descriptor(
    descriptor: &ResolvedModelDescriptor,
    capability: Capability,
) -> FeatureSupportFact {
    match descriptor
        .capabilities()
        .get(capability)
        .map(plakous_model::FeatureSupport::support)
    {
        Some(Support::Native | Support::Emulated | Support::Degraded) => {
            FeatureSupportFact::Supported
        }
        Some(Support::Unsupported) => FeatureSupportFact::Unsupported,
        Some(_) | None => FeatureSupportFact::Unknown,
    }
}

/// Builds pure compiler model facts from an already-selected language model.
///
/// # Errors
///
/// Returns a profile error when the model descriptor cannot be digested.
pub fn compiler_model_facts_from_model(
    _model: &dyn LanguageModel,
    provider_configuration: &CompiledProviderConfiguration,
) -> Result<CompilerModelFacts, ProfileError> {
    let descriptor = provider_configuration.effective().descriptor();
    let binding =
        descriptor
            .semantic_binding_digest()
            .map_err(|error| ProfileError::Serialization {
                reason: error.to_string(),
            })?;
    let behavior = provider_configuration.effective().behavior();
    let reasoning_support = match behavior.reasoning_effort {
        ReasoningEffortDisposition::Disabled => FeatureSupportFact::Unsupported,
        ReasoningEffortDisposition::Enabled(_) => {
            feature_support_from_descriptor(descriptor, Capability::Reasoning)
        }
    };
    let prompt_cache_support = match behavior.cache {
        CachePolicy::Enabled { .. } => {
            feature_support_from_descriptor(descriptor, Capability::Caching)
        }
        _ => FeatureSupportFact::Unsupported,
    };
    Ok(CompilerModelFacts::new(
        provider_configuration.selected_identity(),
        provider_configuration.effective_identity(),
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        descriptor.profile().clone(),
        binding,
        reasoning_support,
        feature_support_from_descriptor(descriptor, Capability::Continuation),
        prompt_cache_support,
    ))
}

/// Compiles a construction-bound plan from explicit builder policy.
///
/// Used when a caller has not supplied a precompiled plan: the agent still
/// binds an immutable plan digest derived only from the construction-time
/// instruction/reasoning/tool selections already chosen by the caller.
///
/// # Errors
///
/// Returns a profile error when the derived specification cannot compile.
#[allow(
    clippy::too_many_arguments,
    reason = "construction-bound plan compilation takes one explicit frozen authority tuple"
)]
pub(crate) fn compile_construction_bound_plan(
    model: &dyn LanguageModel,
    provider_configuration: &CompiledProviderConfiguration,
    registry_digest: Digest,
    canonical_tool_names: Vec<String>,
    execution_environment_digest: Digest,
    instructions: &[InstructionBlock],
    reasoning: ReasoningPolicy,
    prefer_native_continuation: bool,
) -> Result<ResolvedHarnessPlan, ProfileError> {
    let base_instructions = if instructions.is_empty() {
        String::new()
    } else {
        instructions
            .iter()
            .map(InstructionBlock::text)
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    let (reasoning_strategy, preferred_effort) = match reasoning {
        ReasoningPolicy::Enabled { effort, .. } => (
            ReasoningPreferenceStrategy::PreferEffort,
            effort.or(Some(ReasoningEffort::Medium)),
        ),
        ReasoningPolicy::Disabled | _ => (ReasoningPreferenceStrategy::Disabled, None),
    };

    let continuation = if prefer_native_continuation {
        ContinuationPreferenceStrategy::PreferNative
    } else {
        ContinuationPreferenceStrategy::CanonicalReplayOnly
    };

    let spec = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("plakous.construction-bound.v1")?)
        .provenance(HarnessProfileProvenance::Operator)
        .display_name("Construction-bound plan")
        .base_instructions(base_instructions)
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(ToolExposureStrategy::CanonicalAll)
        .reasoning_strategy(reasoning_strategy)
        .preferred_reasoning_effort(preferred_effort)
        .continuation_strategy(continuation)
        .cache_strategy(CachePreferenceStrategy::PreferStablePrefix)
        .build()?;

    let tools = CompilerToolFacts::new(registry_digest, canonical_tool_names)?;
    let runtime = CompilerRuntimeFacts::new(
        execution_environment_digest,
        MAX_INSTRUCTION_BYTES,
        false,
        false,
        false,
    );
    let inputs = HarnessCompilerInputs::new(
        compiler_model_facts_from_model(model, provider_configuration)?,
        tools,
        runtime,
        Vec::new(),
    )?;
    HarnessProfileCompiler::new().compile(&spec, &inputs)
}

/// Compiles an explicit profile against the selected model and frozen tools.
///
/// # Errors
///
/// Returns a profile or compilation error.
pub fn compile_explicit_profile(
    spec: &HarnessProfileSpec,
    model: &dyn LanguageModel,
    provider_configuration: &CompiledProviderConfiguration,
    registry_digest: Digest,
    canonical_tool_names: Vec<String>,
    execution_environment_digest: Digest,
    context_sources: Vec<ResolvedContextSourceFact>,
) -> Result<ResolvedHarnessPlan, ProfileError> {
    let tools = CompilerToolFacts::new(registry_digest, canonical_tool_names)?;
    let runtime = CompilerRuntimeFacts::new(
        execution_environment_digest,
        MAX_INSTRUCTION_BYTES,
        false,
        false,
        false,
    );
    let inputs = HarnessCompilerInputs::new(
        compiler_model_facts_from_model(model, provider_configuration)?,
        tools,
        runtime,
        context_sources,
    )?;
    HarnessProfileCompiler::new().compile(spec, &inputs)
}
