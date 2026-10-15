//! Shared Custom-model scaffolding for the embedding examples.
//!
//! This scaffolding uses the public `plakous` facade, plus `tokio`,
//! `serde_json`, `uuid`, and `futures-util`; the examples also use `tempfile`.
//! It uses no repository test-support crate. The library-owned construction
//! facts come from [`plakous::construction`].

#![allow(dead_code, reason = "each example uses part of the shared scaffolding")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use plakous::construction::{CustomProviderConfigurationRequest, bind_custom_model_authority};
use plakous::model::ModelErrorKind;
use plakous::model::{
    Actor, BlockStart, BlockStarted, CachePolicy, CallId, Capability, CapabilityConstraints,
    CapabilitySet, CapacityCertificate, CapacityRule, CompiledProviderConfiguration,
    ConfiguredTool, ContentBlock, ContextAdmissionRequirement, ContextContract,
    ContextEvidenceSource, ConversationItem, CountCertificateKind, CountObservation,
    CredentialSelectionId, Digest, DigestDomain, EntryId, EvidenceFreshness, FactProvenance,
    FactSource, FeatureSupport, FinishDetails, FinishReason, LanguageModel,
    LocalDurabilityDisposition, MeasurementCertificate, ModelAdapterProfileId, ModelError,
    ModelEvent, ModelFamilyId, ModelId, ModelImplementationIdentity, ModelPreparationContext,
    ModelPreparationError, ModelResponse, ModelStream, NamespaceId, OutputBoundDisposition,
    OutputLocation, OutputPolicy, ParallelToolDisposition, PreparedCacheUse, PreparedCallError,
    PreparedCallRecord, PreparedModelCall, PreparedModelProjection, PreparedTokenProjection,
    ProcessingTierDisposition, ProtocolId, Provenance, ProviderAuthorityClassId,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderSemanticEvidence,
    ProviderStorageDisposition, ReasoningEffortDisposition, ReasoningIncludeDisposition,
    ReasoningSummaryDisposition, ReplayDisposition, ResolvedModelDescriptor, ResponseStarted,
    ResponseStateDisposition, RetentionDisposition, RouteId, RuntimeModelAuthority,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, Support, TextBlock, Timestamp,
    ToolCallBlock, ToolCallState, ToolCallerDisposition, ToolCallerLineage, ToolChoice,
    ToolCorrelation, ToolDefinition, ToolExecutionDisposition, ToolExecutionOwner, ToolName,
    ToolPresentationMode, ValidatedRequestPlan,
};

/// Reports any construction failure inside a scripted stream as an event error.
fn invalid_event(error: impl std::fmt::Display) -> ModelError {
    ModelError::new(ModelErrorKind::InvalidEvent, error.to_string())
}

/// The error type every example returns.
pub type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// One scripted model turn: either a tool call, a final text answer, or a
/// stream that never completes until the caller cancels it.
#[derive(Clone)]
pub enum Turn {
    /// Emit one tool call with the given name and raw input.
    Call {
        /// Model-visible tool name.
        name: &'static str,
        /// Raw tool input, already in the shape that tool accepts.
        input: String,
    },
    /// Emit one final assistant text block.
    Text(&'static str),
    /// Park until the turn is cancelled or the agent shuts down.
    AwaitCancellation,
}

/// The neutral vocabulary one scripted Custom model presents.
pub struct Vocabulary {
    /// Protocol identifier.
    pub protocol: &'static str,
    /// Route identifier.
    pub route: &'static str,
    /// Requested model identifier.
    pub model: &'static str,
    /// Model family identifier.
    pub family: &'static str,
    /// Integration profile identifier.
    pub profile: &'static str,
}

impl Vocabulary {
    /// Returns the neutral vocabulary for a model that exposes tools directly.
    pub const fn direct() -> Self {
        Self {
            protocol: "example-protocol",
            route: "example-route",
            model: "example-model",
            family: "example-family",
            profile: "example-profile",
        }
    }
}

/// A Custom model that replays a fixed script of turns.
pub struct ScriptedModel {
    descriptor: ResolvedModelDescriptor,
    /// Runtime authority bound to exactly this descriptor.
    pub authority: RuntimeModelAuthority,
    script: Vec<Turn>,
    /// Number of dispatched calls, for assertions.
    pub calls: AtomicUsize,
    /// Signalled when a parked turn starts.
    pub parked: tokio::sync::Notify,
}

impl ScriptedModel {
    /// Builds one scripted Custom model over the given vocabulary and script.
    ///
    /// # Errors
    ///
    /// Returns an error when the descriptor, contract, or authority is invalid.
    pub fn new(vocabulary: &Vocabulary, script: Vec<Turn>) -> Fallible<Self> {
        let time = Timestamp::new(1, 0)?;
        let features = [
            Capability::Tools,
            Capability::Continuation,
            Capability::Streaming,
        ]
        .into_iter()
        .map(|capability| {
            (
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    // This adapter really is a deterministic example fixture.
                    FactProvenance::new(FactSource::TestFixture, time, None),
                ),
            )
        })
        .collect();
        let descriptor = ResolvedModelDescriptor::new_custom(
            ProtocolId::new(vocabulary.protocol)?,
            RouteId::new(vocabulary.route)?,
            ModelId::new(vocabulary.model)?,
            None,
            Some(ModelFamilyId::new(vocabulary.family)?),
            ModelAdapterProfileId::new(vocabulary.profile)?,
            CapabilitySet::new(features, BTreeMap::new()),
        );
        let binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"plakous-example"])?;
        let protocol = vocabulary.protocol;
        let (descriptor, authority) = bind_custom_model_authority(
            descriptor,
            binding,
            binding,
            1,
            time,
            None,
            |class| -> Fallible<ContextContract> {
                let freshness = EvidenceFreshness::new(time, None)?;
                Ok(ContextContract::new(
                    ProtocolId::new(protocol)?,
                    "example-accounting-v1",
                    "example-projection-v1",
                    "example-reservation-v1",
                    CapacityCertificate::custom_envelope(
                        "example-capacity",
                        "v1",
                        CapacityRule::ProviderReservedInput {
                            reported_context_window: None,
                            maximum_projected_input: 1_000_000,
                            auto_compact_input: None,
                        },
                        class,
                        freshness,
                        binding,
                        "v1",
                    )?,
                    vec![MeasurementCertificate::new(
                        "example-counter",
                        "v1",
                        "wire-bytes",
                        "v1",
                        ContextEvidenceSource::CallerDeclaration,
                        CountCertificateKind::CustomEnvelopeUpperBound {
                            declaration: binding,
                        },
                        Some(class),
                        freshness,
                    )?],
                )?)
            },
        )?;
        Ok(Self {
            descriptor,
            authority,
            script,
            calls: AtomicUsize::new(0),
            parked: tokio::sync::Notify::new(),
        })
    }

    /// Returns the resolved descriptor this model is bound to.
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    /// Compiles one complete provider configuration for this model.
    ///
    /// Route, integration profile, requested model, and family come off the
    /// descriptor. Everything passed explicitly is an issuer fact this
    /// embedder owns.
    ///
    /// # Errors
    ///
    /// Returns an error when any identifier or the compilation is invalid.
    pub fn provider_configuration(
        &self,
        tool_mode: ToolPresentationMode,
        durability: LocalDurabilityDisposition,
        tools: &[ToolDefinition],
    ) -> Fallible<CompiledProviderConfiguration> {
        let behavior = ProviderBehaviorConfiguration {
            processing_tier: ProcessingTierDisposition::Standard,
            tool_mode,
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::Omit,
            replay: ReplayDisposition::Disabled,
            cache: CachePolicy::Disabled,
            tools: tools
                .iter()
                .cloned()
                .map(|definition| {
                    ConfiguredTool::new(
                        definition,
                        ToolExecutionDisposition::Local,
                        ToolCallerDisposition::DirectOnly,
                    )
                })
                .collect(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::NoRequestBound,
            storage: ProviderStorageDisposition::OmittedRouteContract,
            retention: RetentionDisposition::RouteContract {
                profile: self.descriptor.profile().clone(),
                revision: ProviderProfileRevisionId::new("example-retention-v1")?,
            },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: durability,
        };
        Ok(CustomProviderConfigurationRequest {
            descriptor: &self.descriptor,
            authority: &self.authority,
            behavior,
            integration_revision: ProviderProfileRevisionId::new("example-route-v1")?,
            auth_profile: ModelAdapterProfileId::new("example.auth")?,
            auth_revision: ProviderProfileRevisionId::new("example-auth-v1")?,
            authority_class: ProviderAuthorityClassId::new("example-scripted")?,
            credential_selection: CredentialSelectionId::new("example-credential")?,
            implementation: ModelImplementationIdentity::custom(
                NamespaceId::new("example.model")?,
                1,
                NamespaceId::new("example.embedder")?,
            )?,
            semantic_evidence: ProviderSemanticEvidence::new(1, b"example-semantics-v1".to_vec())?,
            family_policy_schema_revision: 1,
            family_policy_compiler_revision: 1,
            family_policy_dialect_revision: 1,
            family_policy_bytes: b"example-family-policy-v1".to_vec(),
        }
        .compile()?)
    }
}

impl LanguageModel for ScriptedModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            self.authority.validate(&self.descriptor, checked_at)?;
            Ok(self.authority.clone())
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one scripted model builds one complete prepared call and stream"
    )]
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
            .map_err(|_| PreparedCallError::InvalidLabel)?;
        let size = wire.len() as u64;
        let digest =
            Digest::sha256(DigestDomain::Blob, 1, &[&wire]).map_err(PreparedCallError::from)?;
        let observation = CountObservation::new("example-counter", "v1", size)
            .map_err(|_| PreparedCallError::InvalidLabel)?;
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            self.descriptor.protocol().clone(),
            "example-lowering-v1",
            "example-projection-v1",
            "example-accounting-v1",
            digest,
            size,
            PreparedTokenProjection::new(digest, size, Some(observation))?,
            PreparedCacheUse::None,
            None,
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
        Ok(PreparedModelCall::new(record, move |call_context| {
            Box::pin(async move {
                let index = self.calls.fetch_add(1, Ordering::SeqCst);
                let turn = self
                    .script
                    .get(index)
                    .cloned()
                    .unwrap_or(Turn::Text("done"));
                if matches!(turn, Turn::AwaitCancellation) {
                    self.parked.notify_one();
                    return Ok(Box::pin(futures_util::stream::once(async move {
                        call_context.cancellation().cancelled().await;
                        Err(ModelError::cancelled())
                    })) as ModelStream);
                }
                let block = match &turn {
                    Turn::Call { name, input } => ContentBlock::ToolCall(
                        ToolCallBlock::new(
                            CallId::try_from(uuid::Uuid::new_v4()).map_err(invalid_event)?,
                            ToolName::new(*name).map_err(invalid_event)?,
                            ToolExecutionOwner::Local,
                            ToolCallState::Requested,
                            ToolCorrelation::new(None, None, ToolCallerLineage::Direct)
                                .map_err(invalid_event)?,
                            input.clone(),
                        )
                        .map_err(|error| {
                            ModelError::new(ModelErrorKind::InvalidEvent, error.to_string())
                        })?,
                    ),
                    Turn::Text(text) => {
                        ContentBlock::Text(TextBlock::new(*text, None).map_err(|error| {
                            ModelError::new(ModelErrorKind::InvalidEvent, error.to_string())
                        })?)
                    }
                    Turn::AwaitCancellation => unreachable!("handled above"),
                };
                let item = ConversationItem::new(
                    EntryId::try_from(uuid::Uuid::new_v4()).map_err(invalid_event)?,
                    Actor::Assistant,
                    vec![block.clone()],
                    Provenance::ModelOutput,
                )
                .map_err(|error| {
                    ModelError::new(ModelErrorKind::InvalidEvent, error.to_string())
                })?;
                let finish = if matches!(turn, Turn::Call { .. }) {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                };
                let events = vec![
                    Ok(ModelEvent::ResponseStarted(ResponseStarted::new(
                        None,
                        plan.digest(),
                    ))),
                    Ok(ModelEvent::BlockStarted(BlockStarted::new(
                        item.id(),
                        OutputLocation::new(0, 0),
                        BlockStart::Complete(block),
                    ))),
                    Ok(ModelEvent::ResponseCompleted(ModelResponse::new(
                        vec![item],
                        FinishDetails::new(finish, None),
                        None,
                        None,
                        plan.digest(),
                    ))),
                ];
                Ok(Box::pin(futures_util::stream::iter(events)) as ModelStream)
            })
        }))
    }
}
