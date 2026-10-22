use std::collections::BTreeMap;
use std::error::Error;
use std::future;
use std::io::{self, BufReader, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::{StreamExt, stream};
use plakous::agent::{
    AgentBuilder, AgentRequestPolicy, GenerationLimits, ReasoningPolicy, TurnResultOutcome,
};
use plakous::model::{
    Actor, CachePolicy, CallId, Capability, CapabilityConstraints, CapabilitySet,
    CapacityCertificate, CapacityRule, CompiledProviderConfiguration, ConfiguredLanguageModel,
    ConfiguredTool, ContentBlock, ContextAdmissionRequirement, ContextContract,
    ContextEvidenceSource, ConversationItem, CountCertificateKind, CountObservation,
    CredentialSelectionId, Digest, DigestDomain, EffectiveProviderConfiguration, EntryId,
    EvidenceFreshness, FactProvenance, FactSource, FamilyPolicyEnvelope, FeatureSupport,
    FinishDetails, FinishReason, FixedClock, LanguageModel, LocalDurabilityDisposition,
    MeasurementCertificate, ModelAdapterProfileId, ModelError, ModelErrorKind, ModelEvent,
    ModelFamilyId, ModelId, ModelImplementationIdentity, ModelPreparationContext,
    ModelPreparationError, ModelPreparationErrorKind, ModelResponse, ModelStream, NamespaceId,
    OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, PreparedCacheUse,
    PreparedCallRecord, PreparedModelCall, PreparedModelProjection, PreparedOutputLimit,
    PreparedTokenProjection, ProtocolId, Provenance, ProviderAuthorityClassId,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderRouteSelection,
    ProviderSemanticEvidence, ProviderStorageDisposition, ReasoningEffortDisposition,
    ReasoningIncludeDisposition, ReasoningSummaryDisposition, ReplayDisposition,
    ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, ResponseStateDisposition,
    RetentionDisposition, RouteId, RuntimeModelAuthority, RuntimeModelAuthorityError,
    RuntimeModelAuthorityOperation, SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS,
    SelectedProviderConfiguration, Support, SystemIdGenerator, TextBlock, TextPhase, Timestamp,
    ToolCallBlock, ToolCallState, ToolChoice, ToolConcurrency, ToolCorrelation, ToolDefinition,
    ToolEffect, ToolExecutionDisposition, ToolExecutionOwner, ToolInputContract, ToolName,
    ToolOutputContract, ToolResultPart, ValidatedRequestPlan,
};
use plakous::sessions::rehearsal::{OfflineReopenInspection, OfflineReopenOutcome};
use plakous::sessions::{
    AgentSessions, AgentSessionsError, AgentSessionsErrorKind, CompactReason,
    CompactSessionOutcome, CompactSessionRequest, ContextQuery, ContextStatusRequest,
    CreateSessionOutcome, CreateSessionRequest, ForkSessionOutcome, ForkSessionRequest,
    OpenSessionOutcome, OpenSessionRequest, RebindSessionOutcome, RebindSessionRequest,
    RecoverSessionOutcome, RecoverSessionRequest, RecoveryRequiredReport, SessionBackend,
    SessionContextStatusKind, SessionLocator, SessionOperationId, SessionRecoveryAction,
    SessionRecoveryPhase, SessionRuntimeBinding, SessionWorkflowUnavailable,
};
use plakous::tools::{
    AlwaysAllowPolicy, EffectDecisionPrompt, InMemoryPlanState, PlanStateIdentity,
    ProcessSessionTools, RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorId, ToolExposure,
    ToolExposureMap, ToolFuture, ToolInvocation, ToolOutcome, ToolPolicy, ToolPolicyDecision,
    ToolPolicyFuture, ToolPolicyRequest, ToolPolicyRevision, ToolRegistration, ToolRegistryBuilder,
    ToolRuntimeError, ToolRuntimeErrorKind, WorkspacePolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::artifact_fixture::{artifact_fixture_descriptor, seal_fixture_artifact};
use crate::evidence::{EvidenceKind, FsyncEvidenceLog, write_fsynced_json};
use crate::protocol::{
    BlockedWorkingWorkflow, ChildReply, CrashBoundary, PROTOCOL_VERSION, ProbeReport,
    RecoveryAction, RecoveryFact, RecoveryPhase,
};
use crate::script::{
    FoldDisposition, ScriptErrorKind, ScriptFaultPhase, ScriptFinish, ScriptOutput, ScriptPlan,
    ScriptStep, ScriptStreamEvent, ScriptToolPolicy,
};

pub(crate) type HarnessResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const FINAL_TEXT: &str = "loopback turn committed";

#[derive(Clone)]
/// Fsynced loopback model/tool boundary controller for repository process tests.
pub struct BoundaryController {
    cut: CrashBoundary,
    evidence: Arc<FsyncEvidenceLog>,
    announce_stdout: bool,
    release_file: Option<Arc<PathBuf>>,
}

impl BoundaryController {
    /// Creates a loopback boundary controller backed by one fsynced evidence file.
    ///
    /// # Errors
    ///
    /// Returns an error when the evidence file cannot be opened and fsynced.
    pub fn from_evidence_path(
        cut: CrashBoundary,
        evidence_path: impl Into<PathBuf>,
    ) -> io::Result<Self> {
        Ok(Self::new(
            cut,
            Arc::new(FsyncEvidenceLog::open(evidence_path)?),
        ))
    }

    /// Creates a loopback controller that records the same fsynced physical
    /// evidence without writing process-protocol frames to stdout.
    ///
    /// This is intended for repository tests whose stdout is an application
    /// terminal rather than the crash harness control channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the evidence file cannot be opened and fsynced.
    pub fn quiet_from_evidence_path(
        cut: CrashBoundary,
        evidence_path: impl Into<PathBuf>,
    ) -> io::Result<Self> {
        Ok(Self {
            cut,
            evidence: Arc::new(FsyncEvidenceLog::open(evidence_path)?),
            announce_stdout: false,
            release_file: None,
        })
    }

    /// Creates a quiet controller whose selected provider/effect boundary can
    /// be released by atomically creating `release_file` from another process.
    ///
    /// This retains the crash harness's physical dispatch evidence while
    /// allowing a real-terminal test to deterministically exercise UI actions
    /// during a long-running turn.
    ///
    /// # Errors
    ///
    /// Returns an error when the evidence file cannot be opened and fsynced.
    pub fn quiet_gated_from_evidence_path(
        cut: CrashBoundary,
        evidence_path: impl Into<PathBuf>,
        release_file: impl Into<PathBuf>,
    ) -> io::Result<Self> {
        Ok(Self {
            cut,
            evidence: Arc::new(FsyncEvidenceLog::open(evidence_path)?),
            announce_stdout: false,
            release_file: Some(Arc::new(release_file.into())),
        })
    }

    pub(crate) fn new(cut: CrashBoundary, evidence: Arc<FsyncEvidenceLog>) -> Self {
        Self {
            cut,
            evidence,
            announce_stdout: true,
            release_file: None,
        }
    }

    async fn pause_at_selected_boundary(&self, selected: bool) {
        if !selected {
            return;
        }
        let Some(release_file) = &self.release_file else {
            future::pending::<()>().await;
            return;
        };
        while !release_file.exists() {
            std::thread::sleep(std::time::Duration::from_millis(1));
            tokio::task::yield_now().await;
        }
    }

    /// A gated pause that a cancelled model call leaves immediately.
    ///
    /// A provider that ignores its cancellation token is an *unjoinable*
    /// operation, and plakous correctly reports an unjoinable cancel as
    /// recovery-required rather than as an interrupt. Modelling the ordinary
    /// §6.1 interrupt therefore needs a gate that a real provider's
    /// cancellation would also unblock. Only the gated form observes the token:
    /// an ungated crash boundary still parks forever, because the child process
    /// is meant to be killed there.
    async fn pause_at_selected_boundary_until_cancelled(
        &self,
        selected: bool,
        cancellation: &plakous::model::CancellationToken,
    ) {
        if !selected {
            return;
        }
        let Some(release_file) = &self.release_file else {
            future::pending::<()>().await;
            return;
        };
        while !release_file.exists() && !cancellation.is_cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(1));
            tokio::task::yield_now().await;
        }
    }

    fn announce(
        &self,
        boundary: CrashBoundary,
        kind: EvidenceKind,
        round: Option<u64>,
    ) -> io::Result<bool> {
        self.announce_operation(boundary, kind, round, None)
    }

    fn announce_operation(
        &self,
        boundary: CrashBoundary,
        kind: EvidenceKind,
        round: Option<u64>,
        operation_id: Option<String>,
    ) -> io::Result<bool> {
        let evidence_sequence = self.evidence.append(kind, round, boundary, operation_id)?;
        if self.announce_stdout {
            write_reply(&ChildReply::Boundary {
                version: PROTOCOL_VERSION,
                boundary,
                evidence_kind: boundary.evidence_kind(),
                evidence_sequence,
            })?;
        }
        Ok(self.cut == boundary)
    }
}

async fn pause_if(should_pause: bool) {
    if should_pause {
        future::pending::<()>().await;
    }
}

struct LoopbackModel {
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    controller: BoundaryController,
    /// Shared so every runtime built for one session advances one script.
    next_round: Arc<AtomicU64>,
    scenario: LoopbackScenario,
    identity_namespace: u128,
    seal_final_artifact: bool,
}

#[derive(Clone)]
enum LoopbackScenario {
    Effect,
    /// [`LoopbackScenario::Effect`] whose projected count follows the request's
    /// own bytes, so an oversized tool result really does grow the context and
    /// replacing it really does shrink it.
    ContentSensitiveEffect,
    TerminalFailure(ModelErrorKind),
    Text {
        prefix: &'static str,
        terminal: &'static str,
        finish: FinishReason,
    },
    TextBlocks(&'static [&'static str]),
    Compaction,
    ExitedChildContinuation,
    /// One tree turn per round: `spawn_agent`, then `wait_agent` on the agent
    /// the spawn named, then `adopt_candidate` on the candidate the wait row
    /// named, then final text. Every argument after the first comes from the
    /// previous round's tool result, so the script drives a real tree rather
    /// than replaying fixed ids.
    Topology,
    /// A declarative script read from a file, so a spawned process can select
    /// the round-by-round shape a Rust constructor would otherwise pin.
    Scripted(Arc<ScriptPlan>),
    /// The summary half of a scripted binding: it answers a fold request the
    /// way the script's [`FoldDisposition`] says, and nothing else.
    ScriptedSummary(Arc<ScriptPlan>),
}

impl LoopbackModel {
    fn new(
        controller: BoundaryController,
        account: &[u8],
        compaction_summary: bool,
    ) -> HarnessResult<Self> {
        Self::with_descriptor(
            controller,
            account,
            compaction_summary,
            descriptor(account)?,
        )
    }

    fn with_descriptor(
        controller: BoundaryController,
        account: &[u8],
        compaction_summary: bool,
        descriptor: ResolvedModelDescriptor,
    ) -> HarnessResult<Self> {
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
            Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"offline-loopback-credential-authority"],
            )?,
            1,
            fixture_time()?,
            None,
        )?;
        Ok(Self {
            descriptor,
            authority,
            controller,
            next_round: Arc::new(AtomicU64::new(1)),
            scenario: if compaction_summary {
                LoopbackScenario::Compaction
            } else {
                LoopbackScenario::Effect
            },
            seal_final_artifact: false,
            identity_namespace: account.iter().fold(1_u128, |value, byte| {
                value.wrapping_mul(257).wrapping_add(u128::from(*byte))
            }) & 0x0000_0000_0000_0000_0000_ffff_ffff_ffff,
        })
    }

    /// Replaces this model's private round cursor with a shared one.
    ///
    /// A script's position belongs to the session, not to the binding. The host
    /// may build more than one runtime for one session, and each would
    /// otherwise restart the script at step one.
    fn with_shared_round(mut self, cursor: Arc<AtomicU64>) -> Self {
        self.next_round = cursor;
        self
    }

    fn topology(controller: BoundaryController, account: &[u8]) -> HarnessResult<Self> {
        let mut model = Self::new(controller, account, false)?;
        model.scenario = LoopbackScenario::Topology;
        Ok(model)
    }

    fn exited_child_continuation(
        controller: BoundaryController,
        account: &[u8],
    ) -> HarnessResult<Self> {
        let mut model = Self::new(controller, account, false)?;
        model.scenario = LoopbackScenario::ExitedChildContinuation;
        Ok(model)
    }
}

impl LanguageModel for LoopbackModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        let authority = self.authority.clone();
        let descriptor = self.descriptor.clone();
        Box::pin(async move {
            authority.validate(&descriptor, checked_at)?;
            Ok(authority)
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the loopback preparation keeps one exact inspected projection and dispatch capability together"
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
        // This repository fake deliberately gives the independently admitted
        // summary request a small projection while measuring the ordinary
        // source request in full. The private compaction instruction is the
        // only stable discriminator in this no-tools loopback fixture; drift
        // should make the crash precondition fail visibly. The driver emits it
        // as the trailing user item of the fold's input — never as a prefix
        // instruction — so the input items are where it has to be found.
        let is_summary_request = plan.request().input().items().iter().any(|item| {
            item.blocks().iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text(text)
                        if text.text() == plakous::model::SEMANTIC_COMPACTION_INSTRUCTION
                )
            })
        });
        let default_output_tokens = NonZeroU32::new(1_024).ok_or_else(|| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "loopback output maximum is invalid",
            )
        })?;
        let output_tokens = plan
            .request()
            .limits()
            .max_output_tokens()
            .unwrap_or(default_output_tokens);
        if let LoopbackScenario::Scripted(script) | LoopbackScenario::ScriptedSummary(script) =
            &self.scenario
            && is_summary_request
            && script.fold == FoldDisposition::Refuse
        {
            // A refused fold is not a turn failure: the compaction ladder is
            // expected to fall to its offline placeholder rung instead.
            return Err(ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "scripted fold refusal",
            ));
        }
        let content_sensitive = match &self.scenario {
            // Scripted folds measure source and successor on this model. A flat
            // counter would publish a fold that did not shrink. The summary
            // request itself stays on the identity projection below.
            LoopbackScenario::Compaction
            | LoopbackScenario::ContentSensitiveEffect
            | LoopbackScenario::ScriptedSummary(_) => true,
            // Turn scripts opt in. Otherwise a host pass compares identity-sized
            // counts and no placeholder successor can shrink.
            LoopbackScenario::Scripted(script) => script.content_sensitive_projection,
            _ => false,
        };
        let wire = if content_sensitive && !is_summary_request {
            serde_json::to_vec(&(
                plan.request(),
                plan.decisions(),
                ("max_output_tokens", output_tokens),
            ))
        } else {
            serde_json::to_vec(&(
                plan.request().model_input_identity(),
                plan.decisions(),
                ("max_output_tokens", output_tokens),
            ))
        }
        .map_err(|_| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::Projection,
                "loopback request projection serialization failed",
            )
        })?;
        let wire_size = u64::try_from(wire.len()).map_err(|_| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::ResourceLimit,
                "loopback request projection exceeds addressable size",
            )
        })?;
        let wire_digest = Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"plakous-offline-crash-harness/loopback-wire-v1", &wire],
        )
        .map_err(plakous::model::PreparedCallError::from)?;
        // Integrity digests are transport-only. Their decimal widths varied with
        // fresh ids, making independent complete/crash runs clip differently.
        // A fixed maximum-width encoding preserves this fake's byte upper bound.
        let counted_tokens = if content_sensitive
            && !is_summary_request
            && !matches!(self.scenario, LoopbackScenario::Compaction)
        {
            fn normalize_digests(value: &mut serde_json::Value) {
                match value {
                    serde_json::Value::Object(object) => {
                        if object.contains_key("algorithm")
                            && object.contains_key("domain")
                            && object.contains_key("version")
                            && object
                                .get("bytes")
                                .and_then(serde_json::Value::as_array)
                                .is_some_and(|bytes| bytes.len() == 32)
                        {
                            object.insert("bytes".to_owned(), serde_json::json!(vec![255; 32]));
                        } else {
                            for value in object.values_mut() {
                                normalize_digests(value);
                            }
                        }
                    }
                    serde_json::Value::Array(values) => {
                        for value in values {
                            normalize_digests(value);
                        }
                    }
                    _ => {}
                }
            }
            let mut counted: serde_json::Value = serde_json::from_slice(&wire).map_err(|_| {
                ModelPreparationError::rejected(
                    ModelPreparationErrorKind::Projection,
                    "loopback input counting failed",
                )
            })?;
            normalize_digests(&mut counted);
            let counted = serde_json::to_vec(&counted).map_err(|_| {
                ModelPreparationError::rejected(
                    ModelPreparationErrorKind::Projection,
                    "loopback input counting failed",
                )
            })?;
            u64::try_from(counted.len()).map_err(|_| {
                ModelPreparationError::rejected(
                    ModelPreparationErrorKind::ResourceLimit,
                    "loopback input count is too large",
                )
            })?
        } else {
            wire_size
        };
        let token_projection = PreparedTokenProjection::new(
            wire_digest,
            wire_size,
            Some(
                CountObservation::new("offline-loopback-byte-counter", "exact-v1", counted_tokens)
                    .map_err(|_| {
                        ModelPreparationError::rejected(
                            ModelPreparationErrorKind::InvalidConfiguration,
                            "loopback token observation is invalid",
                        )
                    })?,
            ),
        )?
        // This fake counts logical input bytes; chrome uses that same scale.
        .with_input_parts(plakous::model::ContextInputParts::measure_wire(
            format!(
                r#"{{"input":"{}"}}"#,
                "x".repeat(
                    usize::try_from(counted_tokens)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(4)
                )
            )
            .as_bytes(),
        ));
        let output_limit = PreparedOutputLimit::enforced_wire_maximum(
            output_tokens,
            "max_output_tokens",
            wire_digest,
            "offline-loopback-lowering-v1",
        )?;
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            self.descriptor.protocol().clone(),
            "offline-loopback-lowering-v1",
            "offline-loopback-projection-v1",
            "offline-loopback-accounting-v1",
            wire_digest,
            wire_size,
            token_projection,
            PreparedCacheUse::None,
            Some(output_limit),
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
        // This request's own complete prompt, which is what a provider
        // reports as `input_tokens` for the operation it belongs to.
        let prepared_input_tokens = counted_tokens;
        let cache_affinity = record.projection().cache_affinity();
        Ok(PreparedModelCall::new(record, move |call_context| {
            let cancellation = call_context.cancellation().clone();
            let round = self.next_round.fetch_add(1, Ordering::AcqRel);
            let dispatch_boundary = match round {
                1 => CrashBoundary::ProviderDispatchOne,
                _ => CrashBoundary::ProviderDispatchTwo,
            };
            Box::pin(async move {
                if is_summary_request {
                    self.controller
                        .evidence
                        .append_summary_dispatch(round, dispatch_boundary, wire_digest.to_string())
                        .map_err(model_evidence_error)?;
                }
                let pause = self
                    .controller
                    .announce_operation(
                        dispatch_boundary,
                        EvidenceKind::ProviderDispatched,
                        Some(round),
                        Some(wire_digest.to_string()),
                    )
                    .map_err(model_evidence_error)?;
                self.controller
                    .pause_at_selected_boundary_until_cancelled(pause, &cancellation)
                    .await;
                if let LoopbackScenario::Scripted(script)
                | LoopbackScenario::ScriptedSummary(script) = &self.scenario
                    && is_summary_request
                {
                    match script.fold {
                        FoldDisposition::Fail => {
                            return Err(ModelError::new(
                                ModelErrorKind::RateLimited,
                                "scripted fold failure",
                            ));
                        }
                        FoldDisposition::Stall(milliseconds)
                            if stall(
                                milliseconds,
                                &cancellation,
                                script.stall_ignores_cancellation,
                            )
                            .await =>
                        {
                            return Err(ModelError::new(
                                ModelErrorKind::Cancelled,
                                "the scripted fold stall was cancelled",
                            ));
                        }
                        _ => {}
                    }
                }
                if let LoopbackScenario::TerminalFailure(kind) = self.scenario {
                    return Err(ModelError::new(kind, "injected terminal model failure"));
                }
                if let LoopbackScenario::Scripted(script) = &self.scenario
                    && let Some(gate) = script.step(round).and_then(|step| step.gate.as_ref())
                {
                    crate::gate::wait_gate(gate, &cancellation, script.stall_ignores_cancellation)
                        .await?;
                }
                if let LoopbackScenario::Scripted(script) = &self.scenario
                    && let Some(step) = script.step(round)
                    && stall(
                        step.stall_ms,
                        &cancellation,
                        script.stall_ignores_cancellation,
                    )
                    .await
                {
                    return Err(ModelError::new(
                        ModelErrorKind::Cancelled,
                        "the scripted stall was cancelled",
                    ));
                }
                let scripted_fault = match &self.scenario {
                    LoopbackScenario::Scripted(script) => {
                        script.step(round).and_then(|step| step.fault.clone())
                    }
                    _ => None,
                };
                if let Some(fault) = &scripted_fault {
                    match fault.at {
                        ScriptFaultPhase::PreStream => {
                            return Err(scripted_error(fault, &self.descriptor));
                        }
                        ScriptFaultPhase::Stalled => {
                            if stall(
                                fault.stall_ms,
                                &cancellation,
                                scripted_stall_is_deaf(&self.scenario),
                            )
                            .await
                            {
                                return Err(ModelError::new(
                                    ModelErrorKind::Cancelled,
                                    "the scripted stall was cancelled",
                                ));
                            }
                            return Err(scripted_error(fault, &self.descriptor));
                        }
                        ScriptFaultPhase::InBand => {}
                    }
                }
                let response = loopback_response(
                    round,
                    &plan,
                    &self.scenario,
                    self.identity_namespace,
                    prepared_input_tokens,
                )?;
                let response = if self.seal_final_artifact {
                    seal_fixture_artifact(&plan, &response, cache_affinity)?
                } else {
                    response
                };
                let controller = self.controller.clone();
                let stream_cancellation = cancellation.clone();
                let outcome_boundary = match round {
                    1 => CrashBoundary::ProviderOutcomeOne,
                    _ => CrashBoundary::ProviderOutcomeTwo,
                };
                let events: ModelStream = Box::pin(stream::once(async move {
                    let pause = controller
                        .announce(outcome_boundary, EvidenceKind::ProviderOutcome, Some(round))
                        .map_err(model_evidence_error)?;
                    controller
                        .pause_at_selected_boundary_until_cancelled(pause, &cancellation)
                        .await;
                    Ok(ModelEvent::ResponseCompleted(response))
                }));
                if let Some(fault) = scripted_fault
                    && fault.at == ScriptFaultPhase::InBand
                {
                    // An in-band error frame: the stream commits deltas the
                    // face has already shown, and only then reports the fault.
                    let deltas = (0..fault.after_events)
                        .map(|ordinal| {
                            Ok(ModelEvent::TextDelta(plakous::model::TextDelta::new(
                                plakous::model::OutputLocation::new(0, 0),
                                format!("scripted delta {ordinal}\n").into_bytes(),
                            )))
                        })
                        .collect::<Vec<Result<ModelEvent, ModelError>>>();
                    let failure = scripted_error(&fault, &self.descriptor);
                    return Ok(Box::pin(
                        stream::iter(deltas).chain(stream::once(async move { Err(failure) })),
                    ) as ModelStream);
                }
                if let LoopbackScenario::Scripted(script) = &self.scenario
                    && let Some(step) = script.step(round)
                    && !step.stream.is_empty()
                {
                    let leading = scripted_stream(
                        step,
                        stream_cancellation,
                        script.stall_ignores_cancellation,
                    )?;
                    return Ok(Box::pin(leading.chain(events)) as ModelStream);
                }
                let prefix = match &self.scenario {
                    LoopbackScenario::Text { prefix, .. } => Some((*prefix).to_owned()),
                    LoopbackScenario::Scripted(script) => script
                        .step(round)
                        .and_then(|step| step.prefix.clone())
                        .filter(|prefix| !prefix.is_empty()),
                    _ => None,
                };
                if let Some(prefix) = prefix {
                    let prefix_event = ModelEvent::TextDelta(plakous::model::TextDelta::new(
                        plakous::model::OutputLocation::new(0, 0),
                        prefix.into_bytes(),
                    ));
                    return Ok(
                        Box::pin(stream::once(async move { Ok(prefix_event) }).chain(events))
                            as ModelStream,
                    );
                }
                Ok(events)
            })
        }))
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err transfers sole ownership of the source I/O error"
)]
fn model_evidence_error(error: io::Error) -> ModelError {
    ModelError::new(
        ModelErrorKind::Protocol,
        format!("loopback evidence persistence failed: {error}"),
    )
}

/// The provider error one scripted fault reports.
fn scripted_error(
    fault: &crate::script::ScriptFault,
    descriptor: &ResolvedModelDescriptor,
) -> ModelError {
    let kind = match fault.kind {
        ScriptErrorKind::Unavailable => ModelErrorKind::Unavailable,
        ScriptErrorKind::Transport => ModelErrorKind::Transport,
        ScriptErrorKind::Timeout => ModelErrorKind::Timeout,
        ScriptErrorKind::RateLimited => ModelErrorKind::RateLimited,
        ScriptErrorKind::IncompleteResponse => ModelErrorKind::IncompleteResponse,
        ScriptErrorKind::OutcomeUnknown => ModelErrorKind::OutcomeUnknown,
        ScriptErrorKind::Protocol => ModelErrorKind::Protocol,
        ScriptErrorKind::ContextOverflow => ModelErrorKind::ContextLimit,
    };
    let message = fault
        .message
        .clone()
        .unwrap_or_else(|| "scripted provider fault".to_owned());
    if fault.phase.is_none() && fault.retryability.is_none() {
        return ModelError::new(kind, message);
    }
    let context = plakous::model::ModelErrorContext::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        fault
            .phase
            .unwrap_or(plakous::model::ModelErrorPhase::Stream),
        fault
            .retryability
            .unwrap_or(plakous::model::ModelRetryability::Unknown),
    );
    context
        .and_then(|context| ModelError::contextual(kind, message, context))
        .unwrap_or_else(|error| {
            ModelError::new(
                ModelErrorKind::Protocol,
                format!("invalid scripted error context: {error}"),
            )
        })
}

/// The events one scripted step streams ahead of its terminal response: its
/// `prefix`, then its `stream`.
///
/// Reasoning blocks take consecutive output indexes, restarting after a
/// discarding retry the way a replayed request does.
fn scripted_stream(
    step: &ScriptStep,
    cancellation: plakous::model::CancellationToken,
    deaf: bool,
) -> Result<ModelStream, ModelError> {
    enum Item {
        Event(Box<ModelEvent>),
        Stall(u64),
        Gate(crate::gate::ScriptGate),
    }
    let event = |model_event: ModelEvent| Item::Event(Box::new(model_event));
    let text = |text: &str| {
        event(ModelEvent::TextDelta(plakous::model::TextDelta::new(
            plakous::model::OutputLocation::new(0, 0),
            text.as_bytes().to_vec(),
        )))
    };
    let mut items = Vec::new();
    if let Some(prefix) = step.prefix.as_deref().filter(|prefix| !prefix.is_empty()) {
        items.push(text(prefix));
    }
    let mut next_reasoning = 1_u32;
    for streamed in &step.stream {
        match streamed {
            ScriptStreamEvent::Reasoning(reasoning) => {
                let location = plakous::model::OutputLocation::new(next_reasoning, 0);
                let item_id =
                    EntryId::try_from(Uuid::from_u128(0x5c21_0000 + u128::from(next_reasoning)))
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::Protocol,
                                "scripted reasoning identity is invalid",
                            )
                        })?;
                next_reasoning += 1;
                items.push(event(ModelEvent::BlockStarted(
                    plakous::model::BlockStarted::new(
                        item_id,
                        location,
                        plakous::model::BlockStart::Reasoning {
                            visibility: plakous::model::ReasoningVisibility::Visible,
                            replay: None,
                        },
                    ),
                )));
                items.push(event(ModelEvent::ReasoningDelta(
                    plakous::model::ReasoningDelta::new(location, reasoning.as_bytes().to_vec()),
                )));
                items.push(event(ModelEvent::BlockCompleted(
                    plakous::model::BlockCompleted::new(location),
                )));
            }
            ScriptStreamEvent::Text(delta) => items.push(text(delta)),
            ScriptStreamEvent::RetryDiscarded => {
                next_reasoning = 1;
                items.push(event(ModelEvent::Retry(plakous::model::ModelRetry::new(
                    2,
                    4,
                    std::time::Duration::ZERO,
                    plakous::model::ModelRetryCause::StreamInterrupted {
                        fault: plakous::model::ModelTransportFault::Io,
                    },
                    true,
                    "scripted stream reset",
                ))));
            }
            ScriptStreamEvent::Stall(milliseconds) => items.push(Item::Stall(*milliseconds)),
            ScriptStreamEvent::Gate(gate) => items.push(Item::Gate(gate.clone())),
        }
    }
    Ok(Box::pin(
        stream::iter(items)
            .then(move |item| {
                let cancellation = cancellation.clone();
                async move {
                    match item {
                        Item::Event(event) => Some(Ok(*event)),
                        Item::Gate(gate) => crate::gate::wait_gate(&gate, &cancellation, deaf)
                            .await
                            .err()
                            .map(Err),
                        Item::Stall(milliseconds) => {
                            stall(milliseconds, &cancellation, deaf).await.then(|| {
                                Err(ModelError::new(
                                    ModelErrorKind::Cancelled,
                                    "the scripted stream stall was cancelled",
                                ))
                            })
                        }
                    }
                }
            })
            .filter_map(future::ready),
    ))
}

/// Whether this scenario's stalls ignore cancellation.
fn scripted_stall_is_deaf(scenario: &LoopbackScenario) -> bool {
    match scenario {
        LoopbackScenario::Scripted(script) | LoopbackScenario::ScriptedSummary(script) => {
            script.stall_ignores_cancellation
        }
        _ => false,
    }
}

/// Stalls one scripted operation for its declared duration, or until the call
/// is cancelled.
///
/// A real client deadline is a wall-clock fact the gate cannot afford to wait
/// out, so a script names a short stall and the error the deadline produces.
///
/// **The token is the contract, not the drop.** A model implementation is
/// handed a [`plakous::model::CancellationToken`] and is expected to observe
/// it; the runtime does not simply drop the in-flight future. A stall that
/// ignored the token therefore made every cancellation look broken from the
/// outside: the face reported "cancel did not settle", and the scripted answer
/// arrived afterwards as though nothing had been asked. That was this fixture
/// misreporting the product, and it is the reason this polls rather than
/// sleeping once.
///
/// Returns `true` when the stall ended because the call was cancelled.
async fn stall(
    milliseconds: u64,
    cancellation: &plakous::model::CancellationToken,
    deaf: bool,
) -> bool {
    // The token exposes only `is_cancelled`, so observing it means polling. The
    // slice is short enough that a cancellation is answered promptly and long
    // enough that a multi-second stall is not a busy loop.
    const SLICE: std::time::Duration = std::time::Duration::from_millis(10);
    if deaf {
        if milliseconds > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(milliseconds)).await;
        }
        return false;
    }
    if milliseconds == 0 {
        return cancellation.is_cancelled();
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(milliseconds);
    while tokio::time::Instant::now() < deadline {
        if cancellation.is_cancelled() {
            return true;
        }
        tokio::time::sleep(SLICE.min(deadline - tokio::time::Instant::now())).await;
    }
    cancellation.is_cancelled()
}

#[allow(
    clippy::too_many_lines,
    reason = "the deterministic fixture keeps its three closed response scenarios visibly exhaustive"
)]
fn loopback_response(
    round: u64,
    plan: &ValidatedRequestPlan,
    scenario: &LoopbackScenario,
    identity_namespace: u128,
    prepared_input_tokens: u64,
) -> Result<ModelResponse, ModelError> {
    if let LoopbackScenario::Scripted(script) = scenario {
        return scripted_response(
            round,
            plan,
            script.step(round),
            identity_namespace,
            prepared_input_tokens,
            script.hosted_tool_call_final,
        );
    }
    if let LoopbackScenario::ScriptedSummary(_) = scenario {
        return scripted_summary_response(round, plan, identity_namespace);
    }
    let block = if let LoopbackScenario::Text { terminal: "", .. } = *scenario {
        ContentBlock::Reasoning(
            plakous::model::ReasoningBlock::new(
                plakous::model::ReasoningVisibility::Omitted,
                None,
                None,
            )
            .map_err(invalid_model_fixture)?,
        )
    } else if let LoopbackScenario::Text { terminal, .. } = *scenario {
        ContentBlock::Text(
            TextBlock::new(terminal, Some(TextPhase::Final)).map_err(invalid_model_fixture)?,
        )
    } else if matches!(*scenario, LoopbackScenario::Compaction) {
        ContentBlock::Text(
            TextBlock::new("older intent groups compacted", Some(TextPhase::Final))
                .map_err(invalid_model_fixture)?,
        )
    } else if matches!(*scenario, LoopbackScenario::ExitedChildContinuation) && round <= 2 {
        let (name, arguments) = if round == 1 {
            (
                "process_session_start",
                // The child is still running when the start returns, so the
                // poll that follows is what finds it exited and joins it.
                json!({
                    "command": "head -c 10106 /dev/zero | tr '\\0' x; sleep 0.5",
                    "workdir": ".",
                    "login": false,
                    "tty": false,
                    "keep_stdin_open": true,
                    "yield_time_ms": 100,
                    "max_output_bytes": 8192
                }),
            )
        } else {
            let (session_id, cursor) = process_session_projection(plan).ok_or_else(|| {
                ModelError::new(
                    ModelErrorKind::InvalidEvent,
                    "process continuation fixture did not receive a session id and cursor",
                )
            })?;
            (
                "process_session_poll",
                json!({
                    "session_id": session_id,
                    "cursor": cursor,
                    "yield_time_ms": 5000,
                    "max_output_bytes": 8192,
                    "max_output_records": 256
                }),
            )
        };
        ContentBlock::ToolCall(
            ToolCallBlock::new(
                CallId::try_from(loopback_identity(plan, identity_namespace, round, b"call")?)
                    .map_err(invalid_model_fixture)?,
                ToolName::new(name).map_err(invalid_model_fixture)?,
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                ToolCorrelation::default(),
                serde_json::to_string(&arguments).map_err(invalid_model_fixture)?,
            )
            .map_err(invalid_model_fixture)?,
        )
    } else if matches!(scenario, LoopbackScenario::Topology) && round <= 3 {
        let (name, arguments) = topology_call(round, plan)?;
        ContentBlock::ToolCall(
            ToolCallBlock::new(
                CallId::try_from(loopback_identity(plan, identity_namespace, round, b"call")?)
                    .map_err(invalid_model_fixture)?,
                ToolName::new(name).map_err(invalid_model_fixture)?,
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                ToolCorrelation::default(),
                serde_json::to_string(&arguments).map_err(invalid_model_fixture)?,
            )
            .map_err(invalid_model_fixture)?,
        )
    } else if matches!(scenario, LoopbackScenario::ExitedChildContinuation) {
        ContentBlock::Text(
            TextBlock::new(
                "large exited child continued and joined",
                Some(TextPhase::Final),
            )
            .map_err(invalid_model_fixture)?,
        )
    } else if round == 1
        || (round == 2 && matches!(scenario, LoopbackScenario::ContentSensitiveEffect))
    {
        // The growth scenario reads twice: the placeholder rung never touches the
        // batch the current round just received, so a single read leaves it
        // nothing to act on.
        ContentBlock::ToolCall(
            ToolCallBlock::new(
                CallId::try_from(loopback_identity(plan, identity_namespace, round, b"call")?)
                    .map_err(invalid_model_fixture)?,
                ToolName::new("loopback_effect").map_err(invalid_model_fixture)?,
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                ToolCorrelation::default(),
                "{}",
            )
            .map_err(invalid_model_fixture)?,
        )
    } else {
        ContentBlock::Text(
            TextBlock::new(FINAL_TEXT, Some(TextPhase::Final)).map_err(invalid_model_fixture)?,
        )
    };
    let output = ConversationItem::new(
        EntryId::try_from(loopback_identity(
            plan,
            identity_namespace,
            round,
            b"entry",
        )?)
        .map_err(invalid_model_fixture)?,
        Actor::Assistant,
        if let LoopbackScenario::TextBlocks(texts) = scenario {
            texts
                .iter()
                .map(|text| {
                    TextBlock::new(*text, Some(TextPhase::Final))
                        .map(ContentBlock::Text)
                        .map_err(invalid_model_fixture)
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            vec![block]
        },
        Provenance::ModelOutput,
    )
    .map_err(invalid_model_fixture)?;
    Ok(ModelResponse::new(
        if matches!(
            *scenario,
            LoopbackScenario::Text {
                terminal: "",
                finish: FinishReason::Refusal,
                ..
            }
        ) {
            Vec::new()
        } else {
            vec![output]
        },
        FinishDetails::new(
            if (round == 1
                && matches!(
                    *scenario,
                    LoopbackScenario::Effect | LoopbackScenario::ContentSensitiveEffect
                ))
                || (round == 2 && matches!(*scenario, LoopbackScenario::ContentSensitiveEffect))
                || (round <= 2 && matches!(*scenario, LoopbackScenario::ExitedChildContinuation))
                || (round <= 3 && matches!(*scenario, LoopbackScenario::Topology))
            {
                FinishReason::ToolCalls
            } else if let LoopbackScenario::Text { finish, .. } = *scenario {
                finish
            } else {
                FinishReason::Stop
            },
            None,
        ),
        // Only the budget-exhausted scenario reports usage: a turn that stops
        // at `Length` having spent the whole bound on reasoning is what the
        // host has to tell apart from an ordinary empty stream, and it can
        // only do that from the counts. Every other scenario stays silent and
        // its goldens are unchanged.
        match *scenario {
            LoopbackScenario::Text {
                finish: FinishReason::Length,
                ..
            } => Some(plakous::model::Usage::new(64, 4_096, 4_095, 0, 0)),
            // The growth scenario reports its counts only on the terminal
            // response and never mid-stream. That is the shape several live
            // routes have, and the one that left the host's prepared-input
            // peak unpopulated until it was recorded here too.
            LoopbackScenario::ContentSensitiveEffect => Some(plakous::model::Usage::new(
                prepared_input_tokens,
                1,
                0,
                0,
                0,
            )),
            _ => None,
        },
        None,
        plan.digest(),
    ))
}

/// Deterministic identity scoped to one model attempt. A reopen binds a fresh
/// model whose round counter restarts at one while the session's committed
/// items can already carry identities the earlier process minted, and
/// history-derived lengths are not monotonic under compaction; the request's
/// generated attempt id is fresh for every model call, so it — not the
/// history shape — keeps a replayed round off the calls and entries the
/// canonical assembler refuses to duplicate.
fn loopback_identity(
    plan: &ValidatedRequestPlan,
    namespace: u128,
    round: u64,
    kind: &[u8],
) -> Result<Uuid, ModelError> {
    let attempt = plan.request().metadata().attempt_id().to_string();
    loopback_identity_for_attempt(&attempt, namespace, round, kind)
}

fn loopback_identity_for_attempt(
    attempt: &str,
    namespace: u128,
    round: u64,
    kind: &[u8],
) -> Result<Uuid, ModelError> {
    let digest = plakous::model::sha256_hex(&[
        attempt.as_bytes(),
        &namespace.to_le_bytes(),
        &round.to_le_bytes(),
        kind,
    ]);
    Uuid::parse_str(&digest[..32]).map_err(invalid_model_fixture)
}

/// The terminal response one scripted round commits.
///
/// A round the script does not name commits the fixture's ordinary final text,
/// so a script only has to describe the rounds it cares about.
#[allow(
    clippy::too_many_lines,
    reason = "closed scripted output variants and response assembly"
)]
fn scripted_response(
    round: u64,
    plan: &ValidatedRequestPlan,
    step: Option<&ScriptStep>,
    identity_namespace: u128,
    prepared_input_tokens: u64,
    hosted_tool_call_final: bool,
) -> Result<ModelResponse, ModelError> {
    let default_step = ScriptStep::default();
    let step = step.unwrap_or(&default_step);
    // Response identities must stay unique across processes, because a resumed
    // session replays the identities the previous process minted and the
    // canonical assembler refuses a duplicate tool call. The round counter
    // restarts at one in every process, so the identity is derived from the
    // request instead: its projected size, its input length and its round.
    // Sessions that share a namespace can still collide, because projected
    // size is a digest's JSON length and two digests can have the same
    // decimal width. `ScriptPlan::identity_namespace` separates those
    // sessions in the high 64 bits.
    let ordinal = u128::from(prepared_input_tokens)
        .saturating_mul(1 << 24)
        .saturating_add(
            u128::try_from(plan.request().input().items().len())
                .unwrap_or_default()
                .saturating_mul(1 << 8),
        )
        .saturating_add(u128::from(round));
    let call_identity = (identity_namespace << 64).saturating_add(ordinal.saturating_mul(2));
    let entry_identity = call_identity.saturating_add(1);
    let mut blocks = match &step.output {
        ScriptOutput::Empty => Vec::new(),
        ScriptOutput::HistoryReport => {
            let text = plan
                .request()
                .input()
                .items()
                .iter()
                .flat_map(ConversationItem::blocks)
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            vec![ContentBlock::Text(
                TextBlock::new(text, Some(TextPhase::Final)).map_err(invalid_model_fixture)?,
            )]
        }
        ScriptOutput::ImageReport => {
            let images = plan
                .request()
                .input()
                .items()
                .iter()
                .filter(|item| item.actor() == Actor::User)
                .last()
                .map(|item| {
                    item.blocks()
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Image(image) => Some(format!(
                                "{} {} bytes",
                                image.media_type(),
                                image.source().as_bytes().map_or(0, <[u8]>::len)
                            )),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            vec![ContentBlock::Text(
                TextBlock::new(
                    format!("images seen: [{}]", images.join(", ")),
                    Some(TextPhase::Final),
                )
                .map_err(invalid_model_fixture)?,
            )]
        }
        ScriptOutput::FinalText => vec![ContentBlock::Text(
            TextBlock::new(FINAL_TEXT, Some(TextPhase::Final)).map_err(invalid_model_fixture)?,
        )],
        ScriptOutput::Text(text) => vec![ContentBlock::Text(
            TextBlock::new(text.clone(), Some(TextPhase::Final)).map_err(invalid_model_fixture)?,
        )],
        ScriptOutput::ReasoningOnly => vec![ContentBlock::Reasoning(
            plakous::model::ReasoningBlock::new(
                plakous::model::ReasoningVisibility::Omitted,
                None,
                None,
            )
            .map_err(invalid_model_fixture)?,
        )],
        ScriptOutput::Tool(call) => vec![scripted_tool_call(
            call_identity,
            &call.name,
            serde_json::to_string(&resolved_arguments(&call.arguments, plan))
                .map_err(invalid_model_fixture)?,
        )?],
        // The first call keeps the single-call identity. The others step above
        // the 48-bit namespace (bits 64..112), so no ordinal of any round can
        // reach them.
        ScriptOutput::Tools(calls) => calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                let step = u128::try_from(index)
                    .map_err(invalid_model_fixture)?
                    .saturating_mul(1 << 112);
                scripted_tool_call(
                    call_identity.saturating_add(step),
                    &call.name,
                    serde_json::to_string(&resolved_arguments(&call.arguments, plan))
                        .map_err(invalid_model_fixture)?,
                )
            })
            .collect::<Result<_, _>>()?,
        // The exec marker's contract is `ToolInputContract::Utf8`, which is a
        // claim about the *decoded* value, not the wire bytes. The runtime
        // requires `call.arguments()` to parse to a JSON string and takes the
        // program from inside it (`runtime/composition/projection.rs`, "local
        // composition exec input is not a custom UTF-8 string"). Sending the
        // program as bare text is refused, because bare JavaScript is not
        // valid JSON.
        ScriptOutput::Cell(program) => vec![scripted_tool_call(
            call_identity,
            CODE_MODE_EXEC_NAME,
            serde_json::to_string(program).map_err(invalid_model_fixture)?,
        )?],
        ScriptOutput::Wait(wait) => {
            let mut arguments = serde_json::Map::new();
            arguments.insert(
                "cell_id".to_owned(),
                serde_json::Value::from(wait.cell_id.clone()),
            );
            if let Some(yield_time_ms) = wait.yield_time_ms {
                arguments.insert(
                    "yield_time_ms".to_owned(),
                    serde_json::Value::from(yield_time_ms),
                );
            }
            if wait.terminate {
                arguments.insert("terminate".to_owned(), serde_json::Value::from(true));
            }
            vec![scripted_tool_call(
                call_identity,
                CODE_MODE_WAIT_NAME,
                serde_json::to_string(&serde_json::Value::Object(arguments))
                    .map_err(invalid_model_fixture)?,
            )?]
        }
    };
    if hosted_tool_call_final
        && matches!(step.output, ScriptOutput::Text(_) | ScriptOutput::FinalText)
    {
        blocks.insert(0, scripted_hosted_call(call_identity)?);
        blocks.insert(1, scripted_hosted_result(call_identity)?);
    }
    let output = if blocks.is_empty() {
        Vec::new()
    } else {
        vec![
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(entry_identity))
                    .map_err(invalid_model_fixture)?,
                Actor::Assistant,
                blocks,
                Provenance::ModelOutput,
            )
            .map_err(invalid_model_fixture)?,
        ]
    };
    Ok(ModelResponse::new(
        output,
        FinishDetails::new(
            match step.finish {
                ScriptFinish::Stop => FinishReason::Stop,
                ScriptFinish::ToolCalls => FinishReason::ToolCalls,
                ScriptFinish::Length => FinishReason::Length,
                ScriptFinish::Refusal => FinishReason::Refusal,
            },
            None,
        ),
        step.usage.map(|usage| {
            plakous::model::Usage::new(
                usage.input.unwrap_or(prepared_input_tokens),
                usage.output,
                usage.reasoning,
                usage.cached_input,
                usage.cache_write,
            )
        }),
        None,
        plan.digest(),
    ))
}

fn scripted_hosted_call(identity: u128) -> Result<ContentBlock, ModelError> {
    Ok(ContentBlock::ToolCall(
        ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(identity)).map_err(invalid_model_fixture)?,
            ToolName::new("hosted_interpreter").map_err(invalid_model_fixture)?,
            ToolExecutionOwner::ProviderHosted {
                provider_kind: NamespaceId::new("hosted.interpreter")
                    .map_err(invalid_model_fixture)?,
            },
            ToolCallState::Completed,
            ToolCorrelation::default(),
            "{}",
        )
        .map_err(invalid_model_fixture)?,
    ))
}

fn scripted_hosted_result(identity: u128) -> Result<ContentBlock, ModelError> {
    Ok(ContentBlock::ToolResult(
        plakous::model::ToolResultBlock::new(
            CallId::try_from(Uuid::from_u128(identity)).map_err(invalid_model_fixture)?,
            ToolExecutionOwner::ProviderHosted {
                provider_kind: NamespaceId::new("hosted.interpreter")
                    .map_err(invalid_model_fixture)?,
            },
            plakous::model::ToolResultKind::Success,
            vec![ToolResultPart::Text(
                TextBlock::new("hosted result", None).map_err(invalid_model_fixture)?,
            )],
            ToolCorrelation::default(),
        )
        .map_err(invalid_model_fixture)?,
    ))
}

/// Fills a scripted call's placeholders from the previous tool result.
///
/// A process session's id is minted by the run, so a script cannot write it
/// down in advance. `"$session_id"` and `"$cursor"` are replaced with what the
/// last process observation reported, which is the same substitution the
/// topology script does by hand for agent and candidate ids. Anything else is
/// passed through untouched, and a placeholder with nothing to fill it stays as
/// written so the refusal names it.
fn resolved_arguments(
    arguments: &serde_json::Value,
    plan: &ValidatedRequestPlan,
) -> serde_json::Value {
    fn fill(
        value: &serde_json::Value,
        process: Option<(u64, u64)>,
        plan: &ValidatedRequestPlan,
    ) -> serde_json::Value {
        match value {
            serde_json::Value::String(text) if text == "$session_id" => process.map_or_else(
                || value.clone(),
                |(session_id, _)| serde_json::Value::from(session_id),
            ),
            serde_json::Value::String(text) if text == "$cursor" => process.map_or_else(
                || value.clone(),
                |(_, cursor)| serde_json::Value::from(cursor),
            ),
            // A tree script cannot restate ids the tree mints, so it names the
            // field it wants from the last tool result that carried one:
            // `$result.agent_id`, `$result.candidate`. An unresolved reference
            // stays as written, so the refusal names the placeholder rather
            // than a silently-empty string.
            serde_json::Value::String(text) => text.strip_prefix("$result.").map_or_else(
                || value.clone(),
                |field| {
                    tool_result_string(plan, field)
                        .map_or_else(|| value.clone(), serde_json::Value::from)
                },
            ),
            serde_json::Value::Array(items) => serde_json::Value::Array(
                items.iter().map(|item| fill(item, process, plan)).collect(),
            ),
            serde_json::Value::Object(fields) => serde_json::Value::Object(
                fields
                    .iter()
                    .map(|(key, field)| (key.clone(), fill(field, process, plan)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    fill(arguments, process_session_projection(plan), plan)
}

/// The two Code Mode outer marker names (`composition_markers.rs`).
const CODE_MODE_EXEC_NAME: &str = "exec";
const CODE_MODE_WAIT_NAME: &str = "wait";

/// One requested tool call with arguments already in their wire form.
fn scripted_tool_call(
    identity: u128,
    name: &str,
    arguments: String,
) -> Result<ContentBlock, ModelError> {
    Ok(ContentBlock::ToolCall(
        ToolCallBlock::new(
            CallId::try_from(Uuid::from_u128(identity)).map_err(invalid_model_fixture)?,
            ToolName::new(name.to_owned()).map_err(invalid_model_fixture)?,
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            arguments,
        )
        .map_err(invalid_model_fixture)?,
    ))
}

/// The bounded summary one accepted scripted fold commits.
fn scripted_summary_response(
    round: u64,
    plan: &ValidatedRequestPlan,
    identity_namespace: u128,
) -> Result<ModelResponse, ModelError> {
    let output = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(
            (identity_namespace << 64)
                .saturating_add(60_000)
                .saturating_add(u128::from(round)),
        ))
        .map_err(invalid_model_fixture)?,
        Actor::Assistant,
        vec![ContentBlock::Text(
            TextBlock::new("older intent groups compacted", Some(TextPhase::Final))
                .map_err(invalid_model_fixture)?,
        )],
        Provenance::ModelOutput,
    )
    .map_err(invalid_model_fixture)?;
    Ok(ModelResponse::new(
        vec![output],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        plan.digest(),
    ))
}

/// The tree call for one round of [`LoopbackScenario::Topology`].
///
/// Round 1 spawns; rounds 2 and 3 read the previous round's tool result, so
/// the script names the exact agent the tree minted and the exact candidate it
/// sealed instead of assuming the tree's id shapes.
fn topology_call(
    round: u64,
    plan: &ValidatedRequestPlan,
) -> Result<(&'static str, serde_json::Value), ModelError> {
    match round {
        1 => Ok((
            "spawn_agent",
            json!({
                "template": "implement",
                "tier": "mutate",
                "workspace": "fork",
                "description": "offline parity child",
                "prompt": "make the offline parity change",
                "output_schema_json": "{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
            }),
        )),
        2 => {
            let agent_id = tool_result_string(plan, "agent_id").ok_or_else(|| {
                ModelError::new(
                    ModelErrorKind::InvalidEvent,
                    "topology fixture did not receive a spawned agent id",
                )
            })?;
            Ok(("wait_agent", json!({ "agent_ids": [agent_id] })))
        }
        _ => {
            let candidate = tool_result_string(plan, "candidate").ok_or_else(|| {
                ModelError::new(
                    ModelErrorKind::InvalidEvent,
                    "topology fixture did not receive a sealed candidate",
                )
            })?;
            Ok(("adopt_candidate", json!({ "candidate_id": candidate })))
        }
    }
}

/// The most recent tool result carrying `field`, anywhere in its JSON body.
fn tool_result_string(plan: &ValidatedRequestPlan, field: &str) -> Option<String> {
    plan.request()
        .input()
        .items()
        .iter()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .flat_map(|item| item.blocks().iter().rev())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .flat_map(|result| result.parts().iter())
        .filter_map(|part| match part {
            ToolResultPart::Text(text) => {
                serde_json::from_str::<serde_json::Value>(text.text()).ok()
            }
            _ => None,
        })
        .find_map(|value| first_string_field(&value, field))
}

/// Depth-first search for the first string at `field`, so the script does not
/// restate a tool output's envelope shape.
fn first_string_field(value: &serde_json::Value, field: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(found)) = map.get(field) {
                return Some(found.clone());
            }
            map.values()
                .find_map(|nested| first_string_field(nested, field))
        }
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|nested| first_string_field(nested, field)),
        _ => None,
    }
}

fn process_session_projection(plan: &ValidatedRequestPlan) -> Option<(u64, u64)> {
    let items = plan.request().input().items().iter().collect::<Vec<_>>();
    items
        .into_iter()
        .rev()
        .flat_map(|item| item.blocks().iter().rev())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .flat_map(|result| result.parts().iter())
        .filter_map(|part| match part {
            ToolResultPart::Text(text) => {
                serde_json::from_str::<serde_json::Value>(text.text()).ok()
            }
            _ => None,
        })
        .find_map(|value| {
            Some((
                value.get("session_id")?.as_u64()?,
                value.get("cursor")?.as_u64()?,
            ))
        })
}

fn invalid_model_fixture(error: impl std::fmt::Display) -> ModelError {
    ModelError::new(
        ModelErrorKind::InvalidEvent,
        format!("invalid loopback fixture: {error}"),
    )
}

struct LoopbackEffect {
    controller: BoundaryController,
    /// When set, the effect reports a bounded execution failure instead of a
    /// successful outcome, so a driver can be scripted against a tool error.
    fail: bool,
    /// When set, the successful outcome carries a body of this many bytes, so
    /// a turn can grow its own context past a small admitted ceiling.
    body_bytes: Option<usize>,
}

impl ToolExecutor for LoopbackEffect {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        let dispatch = self.controller.announce_operation(
            CrashBoundary::EffectDispatch,
            EvidenceKind::EffectDispatched,
            None,
            Some(invocation.call_id().as_uuid().to_string()),
        );
        Box::pin(async move {
            let pause = dispatch.map_err(tool_evidence_error)?;
            let cancellation = context.cancellation().clone();
            self.controller
                .pause_at_selected_boundary_until_cancelled(pause, &cancellation)
                .await;
            if pause && cancellation.is_cancelled() {
                // A running effect that observes the interrupt joins it with its
                // own classification (spec 4.3). The runtime records that as a
                // definite cancelled terminal, never as an unknown outcome; an
                // ungated crash cut above still parks until the child is killed.
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Cancelled,
                    "loopback effect joined the cancel",
                ));
            }
            // A tool *error* is a failed observation the model reads and
            // answers, not a runtime failure that abandons the turn.
            let outcome = if self.fail {
                ToolOutcome::failure_text("loopback effect refused")?
            } else if let Some(bytes) = self.body_bytes {
                ToolOutcome::success_text("x".repeat(bytes))?
            } else {
                ToolOutcome::success_text("loopback effect committed")?
            };
            let pause = self
                .controller
                .announce(
                    CrashBoundary::EffectOutcome,
                    EvidenceKind::EffectOutcome,
                    None,
                )
                .map_err(tool_evidence_error)?;
            self.controller.pause_at_selected_boundary(pause).await;
            Ok(outcome)
        })
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err transfers sole ownership of the source I/O error"
)]
fn tool_evidence_error(error: io::Error) -> ToolRuntimeError {
    ToolRuntimeError::new(
        ToolRuntimeErrorKind::Execution,
        format!("loopback evidence persistence failed: {error}"),
    )
}

fn descriptor(account: &[u8]) -> HarnessResult<ResolvedModelDescriptor> {
    descriptor_with_projected_input(account, 10_000_000)
}

pub(super) fn descriptor_with_projected_input(
    account: &[u8],
    maximum_projected_input: u64,
) -> HarnessResult<ResolvedModelDescriptor> {
    descriptor_over_protocol(account, maximum_projected_input, LOOPBACK_PROTOCOL, false)
}

/// The loopback descriptor a script declares: its projected-input bound and,
/// when [`ScriptPlan::image_input`] is set, native image input.
fn script_descriptor(
    account: &[u8],
    script: &ScriptPlan,
) -> HarnessResult<ResolvedModelDescriptor> {
    let descriptor = descriptor_over_protocol(
        account,
        script.maximum_projected_input,
        LOOPBACK_PROTOCOL,
        script.image_input,
    )?;
    if script.omit_context_contract {
        return Ok(ResolvedModelDescriptor::new_custom(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.requested_model().clone(),
            None,
            descriptor.model_family().cloned(),
            ModelAdapterProfileId::new("offline-loopback-adapter-v1")?,
            descriptor.capabilities().clone(),
        ));
    }
    Ok(descriptor)
}

/// The loopback protocol family every ordinary scripted binding speaks.
const LOOPBACK_PROTOCOL: &str = "offline.loopback.responses";

/// [`descriptor_with_projected_input`] over a named protocol family.
///
/// The context contract names its own protocol family, and the descriptor's
/// admission compares the two (`crates/core/model/src/descriptor.rs`, the
/// `"prepared context contract"` drift). A descriptor that borrows another
/// family's contract is refused at admission, not at construction, so the two
/// are minted together here rather than substituted afterwards.
fn descriptor_over_protocol(
    account: &[u8],
    maximum_projected_input: u64,
    protocol: &str,
    image_input: bool,
) -> HarnessResult<ResolvedModelDescriptor> {
    let provenance = FactProvenance::new(FactSource::TestFixture, fixture_time()?, None);
    let mut features = BTreeMap::from([
        (
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        ),
        (
            Capability::Tools,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        ),
    ]);
    if image_input {
        features.insert(
            Capability::ImageInput,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        );
    }
    let freshness = EvidenceFreshness::new(fixture_time()?, None)?;
    let (account_binding, credential_binding) = authority_bindings(account)?;
    let authority_binding =
        RuntimeModelAuthority::authority_class_for(account_binding, credential_binding)?;
    let declaration = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[format!(
            "offline loopback complete-projection upper bound: {maximum_projected_input} bytes"
        )
        .as_bytes()],
    )?;
    let contract = ContextContract::new(
        ProtocolId::new(protocol)?,
        "offline-loopback-accounting-v1",
        "offline-loopback-projection-v1",
        "offline-loopback-output-reservation-v1",
        CapacityCertificate::custom_envelope(
            "offline-loopback-capacity",
            "exact-v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: None,
                maximum_projected_input,
                auto_compact_input: None,
            },
            authority_binding,
            freshness,
            declaration,
            "offline-loopback-declaration-v1",
        )?,
        vec![MeasurementCertificate::new(
            "offline-loopback-byte-counter",
            "exact-v1",
            "complete-loopback-wire",
            "byte-tokenizer-v1",
            ContextEvidenceSource::CallerDeclaration,
            CountCertificateKind::CustomEnvelopeUpperBound { declaration },
            Some(authority_binding),
            freshness,
        )?],
    )?;
    Ok(ResolvedModelDescriptor::new_custom(
        ProtocolId::new(protocol)?,
        RouteId::new("offline.loopback")?,
        ModelId::new("offline-loopback-v1")?,
        None,
        Some(ModelFamilyId::new("offline.loopback.responses")?),
        ModelAdapterProfileId::new("offline-loopback-adapter-v1")?,
        CapabilitySet::new(features, BTreeMap::new()),
    )
    .with_context_contract(contract))
}

/// The scripted Code Mode descriptor, whose ids are the enrolled target.
///
/// Every field carries a `scripted` marker and matches the extra enrolment
/// `plakous-compose` adds under `scripted-code-mode-enrolment`, so this can
/// never be mistaken for a Codex model and never widens anything when that
/// feature is off.
fn scripted_code_mode_descriptor(
    account: &[u8],
    maximum_projected_input: u64,
) -> HarnessResult<ResolvedModelDescriptor> {
    let base = descriptor_over_protocol(
        account,
        maximum_projected_input,
        "offline.scripted.code-mode.responses",
        false,
    )?;
    Ok(ResolvedModelDescriptor::new_custom(
        ProtocolId::new("offline.scripted.code-mode.responses")?,
        RouteId::new("offline.scripted.code-mode")?,
        ModelId::new("offline-scripted-code-mode-v1")?,
        None,
        Some(ModelFamilyId::new("offline.scripted.code-mode.responses")?),
        ModelAdapterProfileId::new("offline-scripted-code-mode-adapter-v1")?,
        base.capabilities().clone(),
    )
    .with_context_contract(
        base.context_contract()
            .cloned()
            .ok_or("scripted Code Mode descriptor lost its context contract")?,
    ))
}

fn authority_bindings(account: &[u8]) -> HarnessResult<(Digest, Digest)> {
    Ok((
        Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"offline-loopback-credential-authority"],
        )?,
    ))
}

pub(super) fn fixture_time() -> HarnessResult<Timestamp> {
    Ok(Timestamp::new(10, 0)?)
}

/// Builds the default deterministic loopback runtime binding.
///
/// # Errors
///
/// Returns an error when the deterministic model, tool, or workspace fixture is invalid.
pub fn runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_for_account(workspace, controller, b"offline-loopback-account")
}

/// Builds the loopback runtime whose one tool reports a bounded execution
/// failure, so a driver can be scripted against a turn with a tool error.
///
/// # Errors
///
/// Returns an error when the deterministic model, tool, or workspace fixture is invalid.
pub fn failing_effect_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_inner(
        workspace,
        controller,
        b"offline-loopback-account",
        10_000_000,
        None,
        true,
    )
}

/// Builds a deterministic loopback runtime binding for an exact account identity.
///
/// # Errors
///
/// Returns an error when the account-bound model, tool, or workspace fixture is invalid.
pub fn runtime_binding_for_account(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_with_input(workspace, controller, account, 10_000_000, None)
}

/// Builds an account-bound loopback runtime with a distinct response-identity namespace.
///
/// The namespace affects only synthetic response entry/call identities. It is
/// intentionally outside provider/model semantic authority so tests can create
/// multiple runtime instances for one exact binding without inventing duplicate
/// provider response identities across forked history.
///
/// # Errors
///
/// Returns an error when the account-bound model, tool, or workspace fixture is invalid.
pub fn runtime_binding_for_account_with_identity_namespace(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    identity_namespace: u128,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_with_input(
        workspace,
        controller,
        account,
        10_000_000,
        Some(identity_namespace),
    )
}

/// Builds a loopback runtime whose admitted input ceiling is one token.
///
/// An empty conversation still overflows because the loopback wire projection
/// is larger than that ceiling, so inspect can report overflow without a paid turn.
///
/// # Errors
///
/// Returns an error when the deterministic model, tool, or workspace fixture is invalid.
pub fn overflow_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_with_input(workspace, controller, b"offline-loopback-account", 1, None)
}

/// Builds a deterministic canonical joined-process lifecycle whose first
/// observation is exactly 8,192 bytes from a 10,106-byte already-exited child.
///
/// This repository-only injected fixture proves the continuation handle,
/// follow-up drain, and quiescent terminal turn through the real executor.
///
/// # Errors
///
/// Returns an error when the exact model, projection, process, or workspace
/// authority cannot be constructed.
pub fn exited_child_process_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-process-account";
    let model: Arc<dyn LanguageModel> = Arc::new(LoopbackModel::exited_child_continuation(
        controller, account,
    )?);
    let process = Arc::new(ProcessSessionTools::posix_sh()?);
    let plan_state = Arc::new(InMemoryPlanState::with_identity(PlanStateIdentity::new(
        "offline.process.fixture.plan-state",
        1,
    )?));
    let mut registry = ToolRegistryBuilder::default();
    plakous::tools::register_packs(
        &plakous::tools::standard_coding_packs(plan_state, process),
        &mut registry,
    )?;
    let registry = registry.freeze();
    let exposures = registry
        .definitions()
        .iter()
        .map(|definition| {
            let identity = registry
                .identity(definition.name())
                .cloned()
                .ok_or("process fixture registry identity is missing")?;
            ToolExposure::new(identity, definition.name().as_str().to_owned())
                .map_err(|_| "process fixture exposure is invalid".into())
        })
        .collect::<HarnessResult<Vec<_>>>()?;
    let exposure = ToolExposureMap::new(registry.clone(), exposures)?;
    let definitions = exposure.definitions().to_vec();
    let configured = configured_model_with_tools(
        Arc::clone(&model),
        &descriptor(account)?,
        definitions,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(fixture_time()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

/// Builds the loopback runtime whose one turn per round drives a tree:
/// `spawn_agent`, `wait_agent`, `adopt_candidate`, then final text.
///
/// The tree's own tool executors are the caller's — the harness knows nothing
/// about topology and never links it — so the caller compiles them against
/// whatever tree and ports its scenario wants and passes them in.
///
/// # Errors
///
/// Returns an error when the model, workspace, or a supplied registration is
/// invalid.
pub fn topology_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    tree_tools: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-topology-account";
    let model: Arc<dyn LanguageModel> = Arc::new(LoopbackModel::topology(controller, account)?);
    let mut registry = ToolRegistryBuilder::default();
    for registration in tree_tools {
        registry.register(registration)?;
    }
    let registry = registry.freeze();
    let exposures = registry
        .definitions()
        .iter()
        .map(|definition| {
            let identity = registry
                .identity(definition.name())
                .cloned()
                .ok_or("topology fixture registry identity is missing")?;
            ToolExposure::new(identity, definition.name().as_str().to_owned())
                .map_err(|_| "topology fixture exposure is invalid".into())
        })
        .collect::<HarnessResult<Vec<_>>>()?;
    let exposure = ToolExposureMap::new(registry.clone(), exposures)?;
    let definitions = exposure.definitions().to_vec();
    let configured = configured_model_with_tools(
        Arc::clone(&model),
        &descriptor(account)?,
        definitions,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(fixture_time()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

/// [`topology_runtime_binding`] driven by a script rather than the fixed
/// topology scenario.
///
/// The registry is the caller's tree registrations and nothing else, so a
/// scripted step's `tool` output names a topology tool by its registered name.
/// This is the only route from a spawned process to a real child: an injected
/// runtime binding replaces the whole tool registry, so unless the topology
/// tools are the registry, `spawn_agent` does not exist for the model to call.
///
/// # Errors
///
/// Returns an error when the scripted model, the tree registrations, or the
/// workspace fixture is invalid.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_topology_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
    tree_tools: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_topology_runtime_binding_with_clock(
        workspace,
        controller,
        &script,
        cursor,
        tree_tools,
        Arc::new(FixedClock::new(fixture_time()?)),
    )
}

/// Builds a scripted topology binding with an injectable observation clock.
///
/// # Errors
/// Returns an error when the fixture model, tools or workspace are invalid.
pub fn scripted_topology_runtime_binding_with_clock(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
    tree_tools: Vec<ToolRegistration>,
    clock: Arc<dyn plakous::model::Clock>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_topology_runtime_binding_inner(
        workspace,
        controller,
        script,
        ScriptedOperation::Turn(cursor),
        tree_tools,
        clock,
    )
}

fn scripted_topology_runtime_binding_inner(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
    operation: ScriptedOperation,
    tree_tools: Vec<ToolRegistration>,
    clock: Arc<dyn plakous::model::Clock>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-topology-account";
    let descriptor = script_descriptor(account, script)?;
    let model = operation.model(controller, account, descriptor.clone(), script)?;
    let mut registry = ToolRegistryBuilder::default();
    for registration in tree_tools {
        registry.register(registration)?;
    }
    let registry = registry.freeze();
    let exposures = registry
        .definitions()
        .iter()
        .map(|definition| {
            let identity = registry
                .identity(definition.name())
                .cloned()
                .ok_or("scripted topology registry identity is missing")?;
            ToolExposure::new(identity, definition.name().as_str().to_owned())
                .map_err(|_| "scripted topology exposure is invalid".into())
        })
        .collect::<HarnessResult<Vec<_>>>()?;
    let exposure = ToolExposureMap::new(registry.clone(), exposures)?;
    let definitions = exposure.definitions().to_vec();
    let configured = configured_model_with_tools(
        Arc::clone(&model),
        &descriptor,
        definitions,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
        .config(scripted_agent_config(script))
        .tool_policy(scripted_tool_policy(script))
        .identity_sources(clock, Arc::new(SystemIdGenerator));
    let builder = if script.scheduled_compaction {
        builder.with_scheduled_compaction_enabled(true)
    } else {
        builder
    };
    Ok(SessionRuntimeBinding::new(builder))
}

fn runtime_binding_with_input(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    maximum_projected_input: u64,
    identity_namespace: Option<u128>,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_inner(
        workspace,
        controller,
        account,
        maximum_projected_input,
        identity_namespace,
        false,
    )
}

fn runtime_binding_inner(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    maximum_projected_input: u64,
    identity_namespace: Option<u128>,
    fail_effect: bool,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_inner_with_body(
        workspace,
        controller,
        account,
        maximum_projected_input,
        identity_namespace,
        fail_effect,
        None,
    )
}

/// Builds a loopback binding whose admitted ceiling a large tool body crosses.
///
/// The placeholder rung only has something to do when a committed tool result is
/// big relative to the window, so this is the one fixture whose effect body
/// size and admitted ceiling are both dialled in.
///
/// # Errors
///
/// Returns an error when the exact model, tool, or workspace authority cannot
/// be constructed.
pub fn placeholder_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    runtime_binding_inner_with_body(
        workspace,
        controller,
        b"offline-loopback-account",
        20_000,
        None,
        false,
        // The second read clips at the 20k ceiling. Pressure replaces the first
        // body, so the crash matrix exercises both clipping and placeholders.
        Some(5_800),
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the loopback binding's axes are all independent fixture dials"
)]
fn runtime_binding_inner_with_body(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    maximum_projected_input: u64,
    identity_namespace: Option<u128>,
    fail_effect: bool,
    body_bytes: Option<usize>,
) -> HarnessResult<SessionRuntimeBinding> {
    Ok(SessionRuntimeBinding::new(loopback_builder(
        workspace,
        controller,
        account,
        maximum_projected_input,
        identity_namespace,
        fail_effect,
        body_bytes,
    )?))
}

/// Builds a loopback binding agent construction refuses before it writes
/// anything: its instruction prefix exceeds the canonical block limit. A
/// create with it fails `Revalidation` and leaves no session behind, the way a
/// product binding whose tool projection is not admitted does.
///
/// # Errors
///
/// Returns an error when the deterministic model, tool, or workspace fixture is invalid.
pub fn construction_refused_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    let instructions = (0..257)
        .map(|_| {
            plakous::model::InstructionBlock::new(
                plakous::model::InstructionSource::Developer,
                "over the canonical block limit",
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let builder = loopback_builder(
        workspace,
        controller,
        b"offline-loopback-account",
        10_000_000,
        None,
        false,
        None,
    )?;
    Ok(SessionRuntimeBinding::new(
        builder.instructions(instructions),
    ))
}

#[allow(
    clippy::too_many_arguments,
    reason = "the loopback binding's axes are all independent fixture dials"
)]
fn loopback_builder(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    maximum_projected_input: u64,
    identity_namespace: Option<u128>,
    fail_effect: bool,
    body_bytes: Option<usize>,
) -> HarnessResult<AgentBuilder> {
    let descriptor = descriptor_with_projected_input(account, maximum_projected_input)?;
    let mut model =
        LoopbackModel::with_descriptor(controller.clone(), account, false, descriptor.clone())?;
    if let Some(identity_namespace) = identity_namespace {
        model.identity_namespace = identity_namespace;
    }
    if body_bytes.is_some() {
        model.scenario = LoopbackScenario::ContentSensitiveEffect;
    }
    let model: Arc<dyn LanguageModel> = Arc::new(model);
    let definition = ToolDefinition::new(
        ToolName::new("loopback_effect")?,
        "1.0.0",
        "Record one offline loopback effect.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    let configured = configured_model(
        Arc::clone(&model),
        &descriptor,
        definition.clone(),
        account,
        None,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("offline.loopback.effect")?,
        ToolExecutorId::new("offline.loopback.effect.v1")?,
        plakous::tools::ToolExecutorBindingIdentity::new(1, &[b"offline.loopback.effect.v1"])?,
        definition,
        Arc::new(LoopbackEffect {
            controller,
            fail: fail_effect,
            body_bytes,
        }),
    ))?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry.freeze(), workspace).identity_sources(
        Arc::new(FixedClock::new(fixture_time()?)),
        Arc::new(SystemIdGenerator),
    );
    let builder = builder.tool_policy(Arc::new(AlwaysAllowPolicy));
    // Only the placeholder pass fixture arms the between-rounds fence; every other
    // loopback binding keeps the default so its observations stay unchanged.
    Ok(if body_bytes.is_some() {
        builder.with_scheduled_compaction_enabled(true)
    } else {
        builder
    })
}

/// Builds the loopback binding one declarative script drives.
///
/// This is the same model, tool, descriptor and workspace authority every
/// other loopback binding uses; only the round-by-round shape comes from data
/// instead of from a Rust constructor. It exists so a spawned `plak` process
/// can select a scenario the gate names in a file.
///
/// # Errors
///
/// Returns an error when the scripted model, tool, or workspace fixture is
/// invalid.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_runtime_binding_with_cursor(workspace, controller, script, Arc::new(AtomicU64::new(1)))
}

/// Scripted process fixture whose final response seals a managed family artifact.
///
/// # Errors
/// Returns invalid fixture construction inputs.
pub fn scripted_artifact_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_runtime_binding_with_cursor_inner(
        workspace,
        controller,
        script,
        ScriptedOperation::ArtifactTurn(Arc::new(AtomicU64::new(1))),
        Vec::new(),
    )
}

/// [`scripted_runtime_binding`] over a caller-owned script cursor.
///
/// One session's script position must survive the host building more than one
/// runtime for it; a private cursor per binding restarts the script instead.
///
/// # Errors
///
/// Returns an error when the scripted model, tool, or workspace fixture is
/// invalid.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_runtime_binding_with_cursor(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_runtime_binding_with_extensions(workspace, controller, script, cursor, Vec::new())
}

/// [`scripted_runtime_binding_with_cursor`] whose registry also holds a host's
/// bound tool `extensions` (a template's MCP tools), each offered Direct
/// under its registration's owner.
///
/// # Errors
///
/// Returns an error when the scripted model, a tool, or the workspace fixture
/// is invalid, an extension collides with a scripted tool, or the script
/// asks for the process tools, which take no extensions.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_runtime_binding_with_extensions(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
    extensions: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_runtime_binding_with_cursor_inner(
        workspace,
        controller,
        &script,
        ScriptedOperation::Turn(cursor),
        extensions,
    )
}

fn scripted_runtime_binding_with_cursor_inner(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
    operation: ScriptedOperation,
    extensions: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-account";
    let descriptor = if matches!(&operation, ScriptedOperation::ArtifactTurn(_)) {
        artifact_fixture_descriptor(account, script.maximum_projected_input)?
    } else {
        script_descriptor(account, script)?
    };
    let model = operation.model(controller.clone(), account, descriptor.clone(), script)?;
    if script.process_tools {
        if !extensions.is_empty() {
            return Err("scripted process tools take no host tool extensions".into());
        }
        return scripted_process_runtime_binding(workspace, model, &descriptor, account, script);
    }
    let definition = ToolDefinition::new(
        ToolName::new("loopback_effect")?,
        "1.0.0",
        "Record one offline loopback effect.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("offline.loopback.effect")?,
        ToolExecutorId::new("offline.loopback.effect.v1")?,
        plakous::tools::ToolExecutorBindingIdentity::new(1, &[b"offline.loopback.effect.v1"])?,
        definition,
        Arc::new(LoopbackEffect {
            controller,
            fail: script.tool_fails,
            body_bytes: script.tool_result_bytes,
        }),
    ))?;
    for extension in extensions {
        registry.register(extension)?;
    }
    let registry = registry.freeze();
    let configured = configured_model_with_configured_tools(
        Arc::clone(&model),
        &descriptor,
        direct_configured_tools(&registry)?,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
        .config(scripted_agent_config(script))
        .tool_policy(scripted_tool_policy(script))
        .identity_sources(
            Arc::new(FixedClock::new(fixture_time()?)),
            Arc::new(SystemIdGenerator),
        );
    let builder = if script.scheduled_compaction {
        builder.with_scheduled_compaction_enabled(true)
    } else {
        builder
    };
    Ok(SessionRuntimeBinding::new(builder))
}

/// The scripted binding whose tools are the standard coding packs.
///
/// Same scripted model; the registry is the one
/// [`exited_child_process_runtime_binding`] builds, over a real POSIX shell, so
/// a script can start, write to, poll and terminate genuine child processes.
/// That is the only route from a spawned `plak` to the process backend's own
/// spawn and teardown.
fn scripted_process_runtime_binding(
    workspace: &Path,
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    account: &[u8],
    script: &ScriptPlan,
) -> HarnessResult<SessionRuntimeBinding> {
    let process = Arc::new(ProcessSessionTools::posix_sh()?);
    let plan_state = Arc::new(InMemoryPlanState::with_identity(PlanStateIdentity::new(
        "offline.scripted.process.plan-state",
        1,
    )?));
    let mut registry = ToolRegistryBuilder::default();
    plakous::tools::register_packs(
        &plakous::tools::standard_coding_packs(plan_state, process),
        &mut registry,
    )?;
    let registry = registry.freeze();
    let exposures = registry
        .definitions()
        .iter()
        .map(|definition| {
            let identity = registry
                .identity(definition.name())
                .cloned()
                .ok_or("scripted process registry identity is missing")?;
            ToolExposure::new(identity, definition.name().as_str().to_owned())
                .map_err(|_| "scripted process exposure is invalid".into())
        })
        .collect::<HarnessResult<Vec<_>>>()?;
    let exposure = ToolExposureMap::new(registry.clone(), exposures)?;
    let definitions = exposure.definitions().to_vec();
    let configured = configured_model_with_tools(
        model,
        descriptor,
        definitions,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
        .config(scripted_agent_config(script))
        .tool_policy(scripted_tool_policy(script))
        .identity_sources(
            Arc::new(FixedClock::new(fixture_time()?)),
            Arc::new(SystemIdGenerator),
        );
    let builder = if script.scheduled_compaction {
        builder.with_scheduled_compaction_enabled(true)
    } else {
        builder
    };
    Ok(SessionRuntimeBinding::new(builder))
}

/// The agent configuration a script asks for.
///
/// Only the observation budget cap is scripted today. It is applied through
/// `config`, which replaces the whole configuration, so every caller sets it
/// before the scheduled-compaction toggle rather than after.
/// A policy that parks every mutating call in an effect-decision frame.
///
/// This is what lets a cut land *inside* an effect decision rather than before
/// or after one: the call pauses until the control plane answers, and a process
/// killed in that window was killed mid-decision.
struct DecisionRequiredPolicy;

impl ToolPolicy for DecisionRequiredPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        // The id is portable ASCII and the revision is nonzero, so this cannot
        // fail; falling back rather than unwrapping keeps the fixture free of
        // a panic path the gate would have to reason about.
        ToolPolicyRevision::new("offline.loopback.decision-required", 1)
            .unwrap_or_else(|_| AlwaysAllowPolicy.revision())
    }

    fn evaluate(&self, request: ToolPolicyRequest) -> ToolPolicyFuture {
        // `ToolEffect` is `#[non_exhaustive]`, so a future variant has to land
        // somewhere. It lands on the decision, because a tool whose effect
        // class this fixture does not recognise is the last thing that should
        // be waved through.
        let decision = match request.definition().effect() {
            ToolEffect::ReadOnly => ToolPolicyDecision::Allowed,
            _ => ToolPolicyDecision::DecisionRequired(EffectDecisionPrompt::new(
                "offline loopback effect needs a decision",
            )),
        };
        Box::pin(async move { Ok(decision) })
    }
}

/// The policy a script asks for.
fn scripted_tool_policy(script: &ScriptPlan) -> Arc<dyn ToolPolicy> {
    match script.tool_policy {
        ScriptToolPolicy::Allow => Arc::new(AlwaysAllowPolicy),
        ScriptToolPolicy::DecisionRequired => Arc::new(DecisionRequiredPolicy),
    }
}

fn scripted_agent_config(script: &ScriptPlan) -> plakous::agent::AgentConfig {
    plakous::agent::AgentConfig::default()
        .with_observation_budget_cap_bytes(script.observation_budget_cap_bytes)
}

/// The scripted binding that attaches a real Code Mode profile.
///
/// A `composition` label on the session record changes nothing on its own: the
/// binding still exposes the bare tools and the turn takes the ordinary direct
/// path. This is what actually puts the model behind the `exec` and `wait`
/// markers over the Codex minimal nested catalog, so a scripted cell runs a
/// JavaScript program in the sidecar and its nested calls are real.
///
/// Built the way the `tests/fixtures/facade-consumer` inventory builds it, over
/// the public surface only.
///
/// # Errors
///
/// Returns an error when the registry, projection target, profile, sidecar
/// artifact or workspace fixture is invalid.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_code_mode_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
    sidecar: &Path,
    telemetry: Option<Arc<plakous::agent::TelemetryEmitter>>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_code_mode_runtime_binding_with_extensions(
        workspace,
        controller,
        script,
        cursor,
        sidecar,
        telemetry,
        Vec::new(),
    )
}

/// [`scripted_code_mode_runtime_binding`] whose nested catalog also lists a
/// host's bound nested-eligible tool `extensions` (a template's MCP tools),
/// in order after the core tools, as the product's Code Mode catalogs do.
///
/// # Errors
///
/// Returns an error when the registry, projection target, profile, sidecar
/// artifact or workspace fixture is invalid, or an extension is not
/// nested-eligible or collides with a core tool.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the caller hands the script's ownership to the binding it builds"
)]
pub fn scripted_code_mode_runtime_binding_with_extensions(
    workspace: &Path,
    controller: BoundaryController,
    script: Arc<ScriptPlan>,
    cursor: Arc<AtomicU64>,
    sidecar: &Path,
    telemetry: Option<Arc<plakous::agent::TelemetryEmitter>>,
    extensions: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    scripted_code_mode_runtime_binding_inner(
        workspace,
        controller,
        &script,
        ScriptedOperation::Turn(cursor),
        sidecar,
        telemetry,
        extensions,
    )
}

fn scripted_code_mode_runtime_binding_inner(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
    operation: ScriptedOperation,
    sidecar: &Path,
    telemetry: Option<Arc<plakous::agent::TelemetryEmitter>>,
    extensions: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-scripted-code-mode-account";
    let descriptor = scripted_code_mode_descriptor(account, script.maximum_projected_input)?;
    let model = operation.model(controller, account, descriptor.clone(), script)?;

    // The Codex minimal catalog plus the two outer markers, which is exactly
    // what `CodexMinimalLocalCodeModeV1::compile` requires to be present.
    //
    // **Not `standard_coding_packs_in_memory`.** That helper mints its plan
    // state with `InMemoryPlanState::new()`, whose authority is
    // `memory.plan.<pid>.<counter>` (`tool-plan/src/lib.rs`). The harness plan
    // binds that authority, so a session created in one process compiles a
    // different plan from the one that reopens it in another, and revalidation
    // refuses with "builder harness plan disagrees with durable semantics". A
    // spawned-binary harness reopens across processes by definition, so the
    // identity has to be stable. Everything else here is what that helper
    // passes, `posix_sh` included.
    let plan_state = Arc::new(InMemoryPlanState::with_identity(PlanStateIdentity::new(
        "offline.scripted.code-mode.plan-state",
        1,
    )?));
    let mut registry = ToolRegistryBuilder::default();
    plakous::tools::register_packs(
        &plakous::tools::standard_coding_packs(
            plan_state,
            Arc::new(ProcessSessionTools::posix_sh()?),
        ),
        &mut registry,
    )?;
    let extension_names = extensions
        .iter()
        .map(|extension| extension.name().clone())
        .collect::<Vec<_>>();
    for extension in extensions {
        registry.register(extension)?;
    }
    if script.js_ptc {
        plakous::tools::ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?;
    } else {
        plakous::tools::CodexMinimalLocalCodeModeV1::register_markers_into(&mut registry)?;
    }
    let registry = registry.freeze();

    // The projection target is *derived from the compiled configuration*, the
    // same way the qualification check derives the one it compares against.
    // Inventing it by hand produces a descriptor the check rejects even when
    // the target is enrolled.
    //
    // The configuration owns **exactly** the two outer markers, not the whole
    // registry, because that is what the enrolment gate requires of a Code Mode
    // provider configuration (`codex_code_mode.rs`, "does not own the exact
    // local exec/wait markers"). The nested catalog is reached through the
    // registry, which the agent is still built over; exposing it here as well
    // is what made this refuse.
    let marker_names = if script.js_ptc {
        ["exec_js", "wait_js"]
    } else {
        [CODE_MODE_EXEC_NAME, CODE_MODE_WAIT_NAME]
    };
    let markers = marker_names
        .into_iter()
        .map(|name| {
            registry
                .definition(&ToolName::new(name)?)
                .cloned()
                .ok_or_else(|| format!("Code Mode marker {name} is not registered").into())
        })
        .collect::<HarnessResult<Vec<ToolDefinition>>>()?;
    let configured = configured_model_with_tools(
        Arc::clone(&model),
        &descriptor,
        markers,
        account,
        None,
        plakous::model::ToolPresentationMode::LocalCompositionOnly,
    )?;
    let attachment = scripted_local_composition_attachment(
        sidecar,
        script.js_ptc,
        &registry,
        &configured,
        &extension_names,
    )?;

    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    // A composition deployment exposes its nested catalog through the
    // evaluator, never to the provider, so the harness plan's exposure strategy
    // is `None` and the derived exposure map is empty. The default plan exposes
    // the whole registry, which the agent then finds disagrees with a
    // configuration that owns only the two markers. This is the same call
    // `tests/fixtures/facade-consumer/src/embedded.rs` makes.
    let plan = plakous::construction::compile_local_composition_plan(
        plakous::agent::HarnessProfileId::new("offline.scripted.code-mode")?,
        &model,
        configured.configuration(),
        &registry,
        &workspace,
    )?;
    let builder = AgentBuilder::new(configured, registry, workspace)
        .harness_plan(plan)
        .config(scripted_agent_config(script))
        .tool_policy(scripted_tool_policy(script))
        .identity_sources(
            Arc::new(FixedClock::new(fixture_time()?)),
            Arc::new(SystemIdGenerator),
        );
    let builder = if script.scheduled_compaction {
        builder.with_scheduled_compaction_enabled(true)
    } else {
        builder
    };
    let builder = if let Some(telemetry) = telemetry {
        builder.telemetry(telemetry)
    } else {
        builder
    };
    Ok(SessionRuntimeBinding::new(
        plakous::agent::attach_local_composition(builder, Some(attachment))?,
    ))
}

fn scripted_local_composition_attachment(
    sidecar: &Path,
    js_ptc: bool,
    registry: &plakous::tools::FrozenToolRegistry,
    configured: &ConfiguredLanguageModel,
    extensions: &[ToolName],
) -> HarnessResult<plakous::agent::LocalCompositionAttachment> {
    let effective = configured.configuration().effective();
    let model_facts = effective.descriptor();
    let family = model_facts
        .model_family()
        .ok_or("scripted Code Mode descriptor has no model family")?;
    let target = plakous::tools::ProviderModelProjectionIdentityV2::new(
        model_facts.integration_class(),
        model_facts.protocol().as_str(),
        model_facts.route().as_str(),
        model_facts.profile().as_str(),
        family.as_str(),
        model_facts.requested_model().as_str(),
        effective.effective_model().as_str(),
        configured.configuration().effective_identity(),
        plakous::tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let artifact = plakous::agent::LocalCompositionEngineArtifact::rquickjs_sidecar_v1(
        std::fs::canonicalize(sidecar)?,
    )?;
    let attachment = if js_ptc {
        let compiled = plakous::tools::ProductJavaScriptCompositionV1::compile_for_kind(
            plakous::tools::JobConfig::default(),
            registry.clone(),
            target,
            plakous::tools::CodingPresentation::Codex,
            [
                ToolName::new("exec_command")?,
                ToolName::new("write_stdin")?,
            ]
            .into_iter()
            .chain(extensions.iter().cloned()),
        )?;
        assert!(compiled.outer_projection().is_for_registry(registry));
        let profile = plakous::agent::LocalCompositionProfile::product_javascript_v1(&compiled)?;
        let identity = Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[b"offline.scripted.js-ptc"],
        )?;
        let evidence = plakous::agent::ProductExtensionEvidence::product_javascript_v1(
            &profile,
            &artifact,
            plakous::agent::ProductExtensionLimits {
                max_source_bytes: 1_048_576,
                max_yield_time_ms: 10_000,
                max_output_tokens: 10_000,
            },
            identity,
            identity,
            configured.configuration(),
        )?;
        plakous::agent::LocalCompositionAttachment::builder(profile)
            .engine_artifact(artifact)
            .product_extension_evidence(evidence)
            .coding_presentation_kind(plakous::tools::CodingPresentation::Codex)
            .build()?
    } else {
        let compiled = plakous::tools::CodexMinimalLocalCodeModeV1::compile_with_extensions(
            plakous::tools::JobConfig::default(),
            registry.clone(),
            target,
            extensions,
        )?;
        assert!(compiled.outer_projection().is_for_registry(registry));
        let profile =
            plakous::agent::LocalCompositionProfile::codex_minimal_local_code_mode_host_v1(
                &compiled,
            )?;
        plakous::agent::LocalCompositionAttachment::builder(profile)
            .engine_artifact(artifact)
            .build()?
    };

    Ok(attachment)
}

enum ScriptedOperation {
    Turn(Arc<AtomicU64>),
    ArtifactTurn(Arc<AtomicU64>),
    Compact,
}

impl ScriptedOperation {
    fn model(
        self,
        controller: BoundaryController,
        account: &[u8],
        descriptor: ResolvedModelDescriptor,
        script: &Arc<ScriptPlan>,
    ) -> HarnessResult<Arc<dyn LanguageModel>> {
        let mut model = LoopbackModel::with_descriptor(controller, account, false, descriptor)?;
        match self {
            Self::ArtifactTurn(cursor) => {
                model.scenario = LoopbackScenario::Scripted(Arc::clone(script));
                model = model.with_shared_round(cursor);
                model.seal_final_artifact = true;
            }
            Self::Turn(cursor) => {
                model.scenario = LoopbackScenario::Scripted(Arc::clone(script));
                model = model.with_shared_round(cursor);
            }
            Self::Compact => {
                model.scenario = LoopbackScenario::ScriptedSummary(Arc::clone(script));
            }
        }
        if let Some(namespace) = script.identity_namespace {
            // Same 48-bit width as the account-derived namespace. Wider values
            // are refused by `ScriptPlan::validate`; mask here too so a caller
            // that skips validation cannot shift into a debug overflow.
            model.identity_namespace = namespace & 0x0000_0000_0000_0000_0000_ffff_ffff_ffff;
        }
        Ok(Arc::new(model))
    }
}

/// Builds an exact twin of the scripted source binding for summary requests.
///
/// Source tools, configuration identities and authority are preserved; only
/// the model scenario and compaction generation limit change. `extensions`
/// are the source binding's root extensions; a topology twin carries
/// `tree_tools` instead, as its source does.
///
/// # Errors
///
/// Returns an error when the source configuration, topology registrations,
/// Code Mode sidecar or compaction output policy cannot be built.
pub fn scripted_compaction_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
    tree_tools: Option<Vec<ToolRegistration>>,
    sidecar: Option<&Path>,
    extensions: Vec<ToolRegistration>,
) -> HarnessResult<SessionRuntimeBinding> {
    let runtime = if let Some(tree_tools) = tree_tools {
        scripted_topology_runtime_binding_inner(
            workspace,
            controller,
            script,
            ScriptedOperation::Compact,
            tree_tools,
            Arc::new(FixedClock::new(fixture_time()?)),
        )?
    } else if script.code_mode {
        scripted_code_mode_runtime_binding_inner(
            workspace,
            controller,
            script,
            ScriptedOperation::Compact,
            sidecar.ok_or("scripted Code Mode compaction needs its source sidecar")?,
            None,
            extensions,
        )?
    } else {
        scripted_runtime_binding_with_cursor_inner(
            workspace,
            controller,
            script,
            ScriptedOperation::Compact,
            extensions,
        )?
    };
    Ok(runtime.with_compaction_output_limit()?)
}

/// Builds the no-tools loopback summary binding used by compaction evidence.
///
/// # Errors
///
/// Returns an error when the summary model or workspace fixture is invalid.
pub fn compaction_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult<SessionRuntimeBinding> {
    compaction_runtime_binding_with_projected_input(workspace, controller, 10_000_000)
}

/// Builds the no-tools loopback summary binding with an exact input ceiling.
///
/// # Errors
///
/// Returns an error when the summary model or workspace fixture is invalid.
pub fn compaction_runtime_binding_with_projected_input(
    workspace: &Path,
    controller: BoundaryController,
    maximum_projected_input: u64,
) -> HarnessResult<SessionRuntimeBinding> {
    compaction_runtime_binding_with_projected_input_inner(
        workspace,
        controller,
        maximum_projected_input,
        None,
    )
}

/// Builds the bounded no-tools summary binding with a caller-separated response namespace.
///
/// # Errors
///
/// Returns an error when the summary model or workspace fixture is invalid.
pub fn compaction_runtime_binding_with_projected_input_and_identity_namespace(
    workspace: &Path,
    controller: BoundaryController,
    maximum_projected_input: u64,
    identity_namespace: u128,
) -> HarnessResult<SessionRuntimeBinding> {
    compaction_runtime_binding_with_projected_input_inner(
        workspace,
        controller,
        maximum_projected_input,
        Some(identity_namespace),
    )
}

/// Builds a text-only fixture with a streamed preview and independent canonical text.
/// The selected provider-outcome boundary gates completion after the preview.
///
/// # Errors
/// Returns an error when model or workspace authority cannot be constructed.
pub fn text_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    prefix: &'static str,
    terminal: &'static str,
) -> HarnessResult<SessionRuntimeBinding> {
    text_runtime_binding_with_finish(workspace, controller, prefix, terminal, FinishReason::Stop)
}

/// Builds a text fixture with an explicit canonical terminal reason.
///
/// # Errors
/// Returns an error when model or workspace authority cannot be constructed.
pub fn text_runtime_binding_with_finish(
    workspace: &Path,
    controller: BoundaryController,
    prefix: &'static str,
    terminal: &'static str,
    finish: FinishReason,
) -> HarnessResult<SessionRuntimeBinding> {
    text_scenario_runtime_binding(
        workspace,
        controller,
        LoopbackScenario::Text {
            prefix,
            terminal,
            finish,
        },
    )
}

/// Builds a text-only fixture preserving multiple canonical blocks.
///
/// # Errors
/// Returns an error when model or workspace authority cannot be constructed.
pub fn text_blocks_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    texts: &'static [&'static str],
) -> HarnessResult<SessionRuntimeBinding> {
    text_scenario_runtime_binding(workspace, controller, LoopbackScenario::TextBlocks(texts))
}

/// Builds a text-only fixture over an explicit account and protocol, for
/// cross-provider history tests without credentials or inference.
///
/// # Errors
/// Returns invalid target or workspace construction inputs.
pub fn text_runtime_binding_for_target(
    workspace: &Path,
    controller: BoundaryController,
    account: &[u8],
    protocol: &str,
) -> HarnessResult<SessionRuntimeBinding> {
    text_scenario_runtime_binding_for_target(
        workspace,
        controller,
        LoopbackScenario::Text {
            prefix: "carry",
            terminal: "carry",
            finish: FinishReason::Stop,
        },
        account,
        protocol,
    )
}

fn text_scenario_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    scenario: LoopbackScenario,
) -> HarnessResult<SessionRuntimeBinding> {
    text_scenario_runtime_binding_for_target(
        workspace,
        controller,
        scenario,
        b"offline-loopback-account",
        LOOPBACK_PROTOCOL,
    )
}

fn text_scenario_runtime_binding_for_target(
    workspace: &Path,
    controller: BoundaryController,
    scenario: LoopbackScenario,
    account: &[u8],
    protocol: &str,
) -> HarnessResult<SessionRuntimeBinding> {
    let descriptor = descriptor_over_protocol(account, 10_000_000, protocol, false)?;
    let mut model = LoopbackModel::with_descriptor(controller, account, true, descriptor.clone())?;
    model.scenario = scenario;
    let configured = configured_model_with_tools(
        Arc::new(model),
        &descriptor,
        Vec::new(),
        account,
        NonZeroU32::new(1_024),
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let builder = AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        Arc::new(WorkspacePolicy::host(workspace, ".")?),
    )
    .request_policy(AgentRequestPolicy::new(
        GenerationLimits::new(NonZeroU32::new(1_024), Vec::new())?,
        ReasoningPolicy::Disabled,
    ))
    .identity_sources(
        Arc::new(FixedClock::new(fixture_time()?)),
        Arc::new(SystemIdGenerator),
    );
    Ok(SessionRuntimeBinding::new(builder))
}

/// Builds a no-tools runtime whose dispatched provider call returns an exact
/// classified failure for terminal-authority integration tests.
///
/// # Errors
///
/// Returns an error if the deterministic runtime fixture cannot be constructed.
pub fn terminal_failure_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    kind: ModelErrorKind,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-account";
    let descriptor = descriptor(account)?;
    let mut model = LoopbackModel::with_descriptor(controller, account, true, descriptor.clone())?;
    model.scenario = LoopbackScenario::TerminalFailure(kind);
    let configured = configured_model_with_tools(
        Arc::new(model),
        &descriptor,
        Vec::new(),
        account,
        NonZeroU32::new(1_024),
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let builder = AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        Arc::new(WorkspacePolicy::host(workspace, ".")?),
    )
    .request_policy(AgentRequestPolicy::new(
        GenerationLimits::new(NonZeroU32::new(1_024), Vec::new())?,
        ReasoningPolicy::Disabled,
    ))
    .identity_sources(
        Arc::new(FixedClock::new(fixture_time()?)),
        Arc::new(SystemIdGenerator),
    );
    Ok(SessionRuntimeBinding::new(builder))
}

/// [`terminal_failure_runtime_binding`] shaped like a product coding session:
/// the standard coding packs under an exact V2 Direct projection, so a
/// product host's projection-reconstruction gate admits its reopen. The model
/// still fails its first turn with `kind` and never calls a tool.
///
/// # Errors
///
/// Returns an error when the model, the coding packs, or the projection is invalid.
pub fn projected_terminal_failure_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    kind: ModelErrorKind,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-account";
    let descriptor = descriptor(account)?;
    let mut model = LoopbackModel::with_descriptor(controller, account, true, descriptor.clone())?;
    model.scenario = LoopbackScenario::TerminalFailure(kind);
    let builder = projected_coding_builder(
        workspace,
        Arc::new(model),
        &descriptor,
        account,
        plakous::tools::CodingPresentation::Codex,
    )?;
    Ok(SessionRuntimeBinding::new(builder))
}

/// The scripted model over the standard coding packs, presented in pi's
/// vocabulary under an exact V2 Direct projection.
///
/// pi exposes one canonical patch tool, `apply_patch`, as two names, `edit`
/// and `write`, so a script that calls both commits two calls with one
/// canonical name and two exposed names.
///
/// # Errors
///
/// Returns an error when the scripted model, the coding packs, or the
/// projection is invalid.
pub fn scripted_pi_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
    script: &Arc<ScriptPlan>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-account";
    let descriptor = script_descriptor(account, script)?;
    let model = ScriptedOperation::Turn(Arc::new(AtomicU64::new(1))).model(
        controller,
        account,
        descriptor.clone(),
        script,
    )?;
    let builder = projected_coding_builder(
        workspace,
        model,
        &descriptor,
        account,
        plakous::tools::CodingPresentation::Pi,
    )?
    .config(scripted_agent_config(script))
    .tool_policy(scripted_tool_policy(script));
    Ok(SessionRuntimeBinding::new(builder))
}

/// The standard coding packs under `presentation`'s exact V2 Direct
/// projection, so a product host's projection-reconstruction gate admits a
/// reopen.
fn projected_coding_builder(
    workspace: &Path,
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    account: &[u8],
    presentation: plakous::tools::CodingPresentation,
) -> HarnessResult<AgentBuilder> {
    use plakous::tools::CodingPresentationExt as _;
    // A fixed plan-state identity, so a reopen's fresh binding names the same
    // executor registrations the create froze.
    let plan_state = Arc::new(InMemoryPlanState::with_identity(PlanStateIdentity::new(
        "offline.projected.plan-state",
        1,
    )?));
    let mut registry = ToolRegistryBuilder::default();
    plakous::tools::register_packs(
        &plakous::tools::standard_coding_packs(
            plan_state,
            Arc::new(ProcessSessionTools::posix_sh()?),
        ),
        &mut registry,
    )?;
    let registry = registry.freeze();
    // The agent's default exposure lists the registry in canonical-name
    // order, and the configured tools must match it exactly.
    let mut definitions = registry.definitions().to_vec();
    definitions.sort_by(|left, right| left.name().as_str().cmp(right.name().as_str()));
    let configured = configured_model_with_tools(
        model,
        descriptor,
        definitions,
        account,
        None,
        plakous::model::ToolPresentationMode::Direct,
    )?;
    let effective = configured.configuration().effective();
    let facts = effective.descriptor();
    let family = facts
        .model_family()
        .ok_or("projected fixture descriptor has no model family")?;
    let target = plakous::tools::ProviderModelProjectionIdentityV2::new(
        facts.integration_class(),
        facts.protocol().as_str(),
        facts.route().as_str(),
        facts.profile().as_str(),
        family.as_str(),
        facts.requested_model().as_str(),
        effective.effective_model().as_str(),
        configured.configuration().effective_identity(),
        plakous::tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let projection = presentation.projection_map(
        plakous::tools::JobConfig::default(),
        registry.clone(),
        target,
    )?;
    Ok(AgentBuilder::new(
        configured,
        registry,
        Arc::new(WorkspacePolicy::host(workspace, ".")?),
    )
    .tool_projection(projection)
    .tool_policy(Arc::new(AlwaysAllowPolicy))
    .identity_sources(
        Arc::new(FixedClock::new(fixture_time()?)),
        Arc::new(SystemIdGenerator),
    ))
}

fn compaction_runtime_binding_with_projected_input_inner(
    workspace: &Path,
    controller: BoundaryController,
    maximum_projected_input: u64,
    identity_namespace: Option<u128>,
) -> HarnessResult<SessionRuntimeBinding> {
    let account = b"offline-loopback-account";
    let descriptor = descriptor_with_projected_input(account, maximum_projected_input)?;
    let mut model = LoopbackModel::with_descriptor(controller, account, true, descriptor.clone())?;
    if let Some(identity_namespace) = identity_namespace {
        model.identity_namespace = identity_namespace;
    }
    let model: Arc<dyn LanguageModel> = Arc::new(model);
    let configured = configured_model(
        Arc::clone(&model),
        &descriptor,
        ToolDefinition::new(
            ToolName::new("unused_compaction_tool")?,
            "1.0.0",
            "Never exposed during compaction.",
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?,
        account,
        Some(SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS),
    )?;
    let registry = ToolRegistryBuilder::default().freeze();
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let builder = AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(Some(SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .identity_sources(
            Arc::new(FixedClock::new(fixture_time()?)),
            Arc::new(SystemIdGenerator),
        );
    Ok(SessionRuntimeBinding::new(builder))
}

fn configured_model(
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    tool: ToolDefinition,
    account: &[u8],
    output_bound: Option<NonZeroU32>,
) -> HarnessResult<ConfiguredLanguageModel> {
    configured_model_with_tools(
        model,
        descriptor,
        vec![tool],
        account,
        output_bound,
        plakous::model::ToolPresentationMode::Direct,
    )
}

fn configured_model_with_tools(
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    tools: Vec<ToolDefinition>,
    account: &[u8],
    output_bound: Option<NonZeroU32>,
    tool_mode: plakous::model::ToolPresentationMode,
) -> HarnessResult<ConfiguredLanguageModel> {
    let tools = tools
        .into_iter()
        .map(|tool| {
            ConfiguredTool::new(
                tool,
                ToolExecutionDisposition::Local,
                plakous::model::ToolCallerDisposition::DirectOnly,
            )
        })
        .collect();
    configured_model_with_configured_tools(
        model,
        descriptor,
        tools,
        account,
        output_bound,
        tool_mode,
    )
}

/// Direct configured tools for every definition in `registry`, in registry
/// order, each carrying its registration's owner as the product's Direct
/// extensions do.
fn direct_configured_tools(
    registry: &plakous::tools::FrozenToolRegistry,
) -> HarnessResult<Vec<ConfiguredTool>> {
    registry
        .definitions()
        .iter()
        .map(|definition| {
            let tool = ConfiguredTool::new(
                definition.clone(),
                ToolExecutionDisposition::Local,
                plakous::model::ToolCallerDisposition::DirectOnly,
            );
            match registry
                .identity(definition.name())
                .map(plakous::tools::RegisteredToolIdentity::owner)
            {
                Some(plakous::tools::ToolOwnerClass::Mcp(server)) => {
                    Ok(tool.with_mcp_owner(server.canonical().clone())?)
                }
                _ => Ok(tool),
            }
        })
        .collect()
}

#[allow(
    clippy::too_many_lines,
    reason = "complete crash-evidence fixture construction"
)]
fn configured_model_with_configured_tools(
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    tools: Vec<ConfiguredTool>,
    account: &[u8],
    output_bound: Option<NonZeroU32>,
    tool_mode: plakous::model::ToolPresentationMode,
) -> HarnessResult<ConfiguredLanguageModel> {
    let (account_binding, credential_binding) = authority_bindings(account)?;
    let authority = RuntimeModelAuthority::new(
        descriptor,
        account_binding,
        credential_binding,
        1,
        fixture_time()?,
        None,
    )?;
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: plakous::model::ProcessingTierDisposition::Standard,
        tool_mode,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: CachePolicy::Disabled,
        tools: if output_bound.is_some() {
            Vec::new()
        } else {
            tools
        },
        hosted_tools: Vec::new(),
        tool_choice: if output_bound.is_some() {
            ToolChoice::None
        } else {
            ToolChoice::Auto
        },
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: output_bound.map_or(
            OutputBoundDisposition::NoRequestBound,
            OutputBoundDisposition::Enforced,
        ),
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("offline-loopback-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::DurableRequired,
    };
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new("offline-loopback-route-v1")?,
            ModelAdapterProfileId::new("offline.loopback.auth")?,
            ProviderProfileRevisionId::new("offline-loopback-auth-v1")?,
            ProviderAuthorityClassId::new("offline-loopback-synthetic")?,
            CredentialSelectionId::new("offline-loopback-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let stable = ResolvedProviderAuthorityBinding::new(
        descriptor,
        &authority,
        ProviderSemanticEvidence::new(1, b"offline-loopback-provider-semantics-v1".to_vec())?,
    )?;
    let family = FamilyPolicyEnvelope::new(
        descriptor
            .model_family()
            .cloned()
            .ok_or("offline loopback model family is missing")?,
        1,
        1,
        1,
        if output_bound.is_some() {
            b"offline-loopback-compaction-family-policy-v1".to_vec()
        } else {
            b"offline-loopback-family-policy-v1".to_vec()
        },
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        if descriptor.integration_class() == plakous::model::IntegrationClass::Standard {
            ModelImplementationIdentity::release_owned(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                descriptor,
                1,
            )?
        } else {
            ModelImplementationIdentity::custom(
                NamespaceId::new("offline.crash-harness.loopback-model")?,
                1,
                NamespaceId::new("offline.crash-harness.fixture")?,
            )?
        },
        stable,
        behavior,
        family,
    )?;
    let configuration = CompiledProviderConfiguration::new(selected, effective)?;
    if descriptor.integration_class() == plakous::model::IntegrationClass::Standard {
        Ok(ConfiguredLanguageModel::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            model,
            configuration,
            authority,
        )?)
    } else {
        Ok(ConfiguredLanguageModel::custom(
            model,
            configuration,
            authority,
        )?)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct LocatorState {
    version: u16,
    source: String,
    latest: String,
}

pub(crate) async fn run_fixture(
    database: &Path,
    workspace: &Path,
    evidence_path: &Path,
    state_path: &Path,
    cut: CrashBoundary,
) -> HarnessResult<String> {
    std::fs::create_dir_all(workspace)?;
    let evidence = Arc::new(FsyncEvidenceLog::open(evidence_path)?);
    let controller = BoundaryController::new(cut, evidence);
    let backend = SessionBackend::sqlite(database)
        .map_err(|error| session_workflow_error("backend-open", &error))?;
    let sessions = AgentSessions::new(backend)
        .map_err(|error| session_workflow_error("service-create", &error))?;
    let created = Box::pin(sessions.create(CreateSessionRequest::new(
        SessionOperationId::new("crash-harness:create")?,
        runtime_binding(workspace, controller.clone())?,
    )))
    .await
    .map_err(|error| session_workflow_error("session-create", &error))?;
    let CreateSessionOutcome::Created { session, .. } = created else {
        return Err("session create returned an unknown outcome".into());
    };
    let old_locator = session
        .release()
        .await
        .map_err(|error| session_workflow_error("genesis-release", &error))?;
    write_locators(state_path, old_locator, old_locator)?;
    let pause = controller.announce(
        CrashBoundary::GenesisReleased,
        EvidenceKind::OldCheckpointReleased,
        None,
    )?;
    pause_if(pause).await;

    let opened = Box::pin(sessions.open(OpenSessionRequest::new(
        SessionOperationId::new("crash-harness:open-for-turn")?,
        old_locator,
        runtime_binding(workspace, controller.clone())?,
    )))
    .await
    .map_err(|error| session_workflow_error("turn-open", &error))?;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        return Err("released genesis did not reopen as quiescent".into());
    };
    let turn = session
        .agent()
        .prompt("perform the loopback effect")
        .await?;
    let result = turn.result().await?;
    if !matches!(result.outcome(), TurnResultOutcome::Completed(_))
        || result.response().map(ModelResponse::output_text).as_deref() != Some(FINAL_TEXT)
    {
        return Err(format!(
            "loopback turn did not produce the exact completed response: outcome={:?}, text={:?}",
            result.outcome(),
            result.response().map(ModelResponse::output_text)
        )
        .into());
    }
    let new_locator = session
        .release()
        .await
        .map_err(|error| session_workflow_error("turn-release", &error))?;
    write_locators(state_path, old_locator, new_locator)?;
    let pause = controller.announce(
        CrashBoundary::CheckpointReleased,
        EvidenceKind::NewCheckpointReleased,
        None,
    )?;
    pause_if(pause).await;
    Ok(new_locator.to_string())
}

pub(crate) async fn probe_fixture(
    database: &Path,
    workspace: &Path,
    evidence_path: &Path,
    state_path: &Path,
) -> HarnessResult<ProbeReport> {
    let state: LocatorState =
        serde_json::from_reader(BufReader::new(std::fs::File::open(state_path)?))?;
    if state.version != PROTOCOL_VERSION {
        return Err("unsupported locator-state version".into());
    }
    let source = state.source.parse::<SessionLocator>()?;
    let mut locator = state.latest.parse::<SessionLocator>()?;
    let evidence = Arc::new(FsyncEvidenceLog::open(evidence_path)?);
    let controller = BoundaryController::new(CrashBoundary::None, evidence);
    let backend = SessionBackend::sqlite(database)
        .map_err(|error| session_workflow_error("probe-backend-open", &error))?;
    let sessions = AgentSessions::new(backend)
        .map_err(|error| session_workflow_error("probe-service-create", &error))?;
    for attempt in 1_u8..=2 {
        let predicted = match predicted_reopen(database, locator) {
            Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
                locator = error
                    .current_locator()
                    .ok_or("stale locator did not expose the current exact head")?;
                continue;
            }
            other => other?,
        };
        let opened = Box::pin(sessions.open(OpenSessionRequest::new(
            SessionOperationId::new(format!("crash-harness:probe-open-{attempt}"))?,
            locator,
            runtime_binding(workspace, controller.clone())?,
        )))
        .await;
        match opened {
            Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
                locator = error
                    .current_locator()
                    .ok_or("stale locator did not expose the current exact head")?;
            }
            Err(error) => {
                check_prediction(&predicted, Opened::Failed)?;
                return Err(error.into());
            }
            Ok(OpenSessionOutcome::RecoveryRequired { recovery, .. }) => {
                check_prediction(&predicted, Opened::RecoveryRequired(&recovery))?;
                Box::pin(assert_recovery_remains_non_dispatching(
                    &sessions,
                    source,
                    &recovery,
                    workspace,
                    controller.clone(),
                ))
                .await?;
                return recovery_report(source, &recovery);
            }
            Ok(OpenSessionOutcome::Opened { session, .. }) => {
                check_prediction(
                    &predicted,
                    Opened::Ordinary {
                        abandoned: session.agent().abandoned_turn().is_some(),
                    },
                )?;
                let view = session.agent().conversation();
                // A turn a lost process left is closed by this very open, which
                // abandons its unfinished model request. The session is
                // ordinary again, so it is neither of the two exact states: the
                // prompt is in the history and no answer ever will be.
                let report = match session.agent().abandoned_turn() {
                    Some(abandoned) => abandoned_report(
                        source,
                        session.locator(),
                        &view,
                        abandoned.operations(),
                        abandoned.nothing_started() || abandoned.tool_round_completed(),
                    )?,
                    None => exact_report(source, session.locator(), &view)?,
                };
                session.release().await?;
                return Ok(report);
            }
            Ok(_) => return Err("session open returned an unknown outcome".into()),
        }
    }
    Err("stale locator retry did not reach the current exact head".into())
}

/// What an open of the probed head did, for the prediction check.
#[derive(Clone, Copy)]
enum Opened<'a> {
    Failed,
    RecoveryRequired(&'a RecoveryRequiredReport),
    Ordinary { abandoned: bool },
}

/// The open did what the read-only loaders predicted for the same head.
fn check_prediction(
    predicted: &(OfflineReopenInspection, Option<RecoveryRequiredReport>),
    opened: Opened<'_>,
) -> HarnessResult {
    let (inspection, report) = predicted;
    let agrees = match opened {
        Opened::Failed => inspection.outcome == OfflineReopenOutcome::RefusesRecoveryScope,
        Opened::RecoveryRequired(recovery) => {
            matches!(
                inspection.outcome,
                OfflineReopenOutcome::RefusesRecoveryScope
                    | OfflineReopenOutcome::RestoresEffectDecision
                    | OfflineReopenOutcome::RestoresHostedContinuation
            ) && report.as_ref() == Some(recovery)
        }
        Opened::Ordinary { abandoned } => {
            let expected = if abandoned {
                &[OfflineReopenOutcome::RecoversAbandonedTurn][..]
            } else {
                &[
                    OfflineReopenOutcome::OpensQuiescent,
                    OfflineReopenOutcome::RecoversSavedTerminal,
                    OfflineReopenOutcome::ResolvesCompactionWorkflow,
                ][..]
            };
            expected.contains(&inspection.outcome)
                && (inspection.outcome == OfflineReopenOutcome::RecoversAbandonedTurn) == abandoned
                && report.is_none() == (inspection.outcome == OfflineReopenOutcome::OpensQuiescent)
        }
    };
    if agrees {
        Ok(())
    } else {
        Err(format!("the open disagreed with the predicate's prediction {predicted:?}").into())
    }
}

/// What the read-only loaders of the one recovery predicate (the rehearsal
/// and the `SQLite` recovery inspection) predict an open of this exact head
/// does, over the same bytes the open loads (spec 01 §5.5).
fn predicted_reopen(
    database: &Path,
    locator: SessionLocator,
) -> Result<(OfflineReopenInspection, Option<RecoveryRequiredReport>), AgentSessionsError> {
    let inspection = plakous::sessions::rehearsal::inspect_sqlite(database, locator)?;
    let report = AgentSessions::inspect_recovery_sqlite(database, locator)?;
    Ok((inspection, report))
}

#[allow(
    clippy::too_many_lines,
    reason = "one ordered negative proof keeps all public Working bypass workflows visibly bound to one report"
)]
async fn assert_recovery_remains_non_dispatching(
    sessions: &AgentSessions,
    source: SessionLocator,
    recovery: &RecoveryRequiredReport,
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult {
    let recover_operation = SessionOperationId::new("crash-harness:probe-recover")?;
    let outcome = Box::pin(sessions.recover(RecoverSessionRequest::new(
        recover_operation.clone(),
        recovery.locator(),
        runtime_binding(workspace, controller.clone())?,
    )))
    .await?;
    match &outcome {
        RecoverSessionOutcome::Unavailable {
            operation_id,
            recovery: still_required,
            reason: SessionWorkflowUnavailable::WorkingRecovery,
        } if operation_id == &recover_operation && still_required == recovery => {}
        other => {
            return Err(format!(
                "working recovery guessed an outcome or changed authority: {other:?}"
            )
            .into());
        }
    }

    let context_operation = SessionOperationId::new("crash-harness:probe-context")?;
    let context = sessions
        .context_status(ContextStatusRequest::new(
            context_operation.clone(),
            recovery.locator(),
            ContextQuery::Current,
        ))
        .await?;
    if context.operation_id() != &context_operation
        || context.locator() != recovery.locator()
        || !matches!(
            context.kind(),
            SessionContextStatusKind::RecoveryRequired(still_required)
                if still_required == recovery
        )
    {
        return Err("Working context status did not preserve exact recovery authority".into());
    }

    let compact_operation = SessionOperationId::new("crash-harness:probe-compact")?;
    let compact = sessions
        .compact(CompactSessionRequest::new(
            compact_operation.clone(),
            recovery.locator(),
            CompactReason::Manual,
            runtime_binding(workspace, controller.clone())?,
        ))
        .await?;
    if !matches!(
        compact,
        CompactSessionOutcome::RecoveryRequired {
            operation_id,
            recovery: still_required,
            ..
        } if operation_id == compact_operation && still_required == *recovery
    ) {
        return Err("Working compaction escaped exact recovery authority".into());
    }

    let fork_operation = SessionOperationId::new("crash-harness:probe-fork")?;
    let fork = sessions
        .fork(ForkSessionRequest::new(
            fork_operation.clone(),
            recovery.locator(),
        ))
        .await?;
    if !matches!(
        fork,
        ForkSessionOutcome::RecoveryRequired {
            operation_id,
            recovery: still_required,
        } if operation_id == fork_operation && still_required == *recovery
    ) {
        return Err("Working fork escaped exact recovery authority".into());
    }

    let runtime = runtime_binding(workspace, controller.clone())?;
    let target = runtime.target_binding();
    let rebind_operation = SessionOperationId::new("crash-harness:probe-rebind")?;
    let rebound = Box::pin(sessions.rebind(RebindSessionRequest::new(
        rebind_operation.clone(),
        recovery.locator(),
        target,
        runtime,
    )))
    .await?;
    if !matches!(
        rebound,
        RebindSessionOutcome::RecoveryRequired {
            operation_id,
            recovery: still_required,
        } if operation_id == rebind_operation && still_required == *recovery
    ) {
        return Err("Working rebind escaped exact recovery authority".into());
    }

    let reopened = Box::pin(sessions.open(OpenSessionRequest::new(
        SessionOperationId::new("crash-harness:probe-reopen-working")?,
        recovery.locator(),
        runtime_binding(workspace, controller)?,
    )))
    .await?;
    if !matches!(
        reopened,
        OpenSessionOutcome::RecoveryRequired {
            recovery: still_required,
            ..
        } if still_required == *recovery
    ) {
        return Err("Working checkpoint changed after blocked bypass operations".into());
    }
    if source.session_id() != recovery.locator().session_id()
        || source.branch_id() != recovery.locator().branch_id()
        || source.checkpoint_id() == recovery.locator().checkpoint_id()
    {
        return Err("Working checkpoint does not preserve exact source lineage".into());
    }
    Ok(())
}

/// Classifies the quiescent state a reopen reached by abandoning a turn.
fn abandoned_report(
    source: SessionLocator,
    locator: SessionLocator,
    view: &plakous::model::ConversationView,
    operations: &[u64],
    nothing_open: bool,
) -> HarnessResult<ProbeReport> {
    // A turn cut at a round boundary, or before anything it asked for
    // started, closes with no open operation of its own.
    if operations.is_empty() && !nothing_open {
        return Err("an abandoned turn reported no abandoned operation".into());
    }
    let note = view
        .items()
        .into_iter()
        .filter_map(|item| {
            item.blocks().first().and_then(|block| match block {
                ContentBlock::Text(text) => Some(text.text().to_owned()),
                _ => None,
            })
        })
        // Three claims, not two. An abandonment always writes a note; only the
        // wording varies, and the strongest applicable claim wins: a running
        // tool may have changed something outside the session, an unobserved
        // request may have been billed, and exact evidence that nothing
        // started says neither.
        //
        // This detector is deliberately permissive, because its job is to find
        // *a* note so the report can be built. It cannot catch a *wrong* note,
        // and that is the expectation arms' job: each asserts the wording its
        // own cut requires. A detector that accepted anything and arms that
        // asserted nothing is how a borrowed warning ships.
        .find(|text| {
            text.contains("abandoned after its outcome became uncertain")
                || text.contains("abandoned after the process was lost while a tool was running")
                || text.contains("never started")
                || text.contains("this turn was abandoned after its tool round completed")
                || text.contains("this turn was abandoned after its model round completed")
                || text.starts_with("this turn ")
                || text.starts_with("the model request for this turn ")
        })
        .ok_or("an abandoned turn left no note in its canonical history")?;
    Ok(ProbeReport::RecoveredByAbandonment {
        source: source.to_string(),
        locator: locator.to_string(),
        canonical_items: view.items().into_iter().count(),
        abandoned_operations: operations.to_vec(),
        note,
    })
}

fn exact_report(
    source: SessionLocator,
    locator: SessionLocator,
    view: &plakous::model::ConversationView,
) -> HarnessResult<ProbeReport> {
    let items = view.items();
    if items.is_empty() {
        return Ok(ProbeReport::OldExact {
            source: source.to_string(),
            locator: locator.to_string(),
            canonical_items: 0,
        });
    }
    if items.len() != 4 {
        return Err(format!(
            "quiescent state has an impossible canonical length: {}",
            items.len()
        )
        .into());
    }
    let final_text = items
        .into_iter()
        .last()
        .and_then(|item| item.blocks().first())
        .and_then(|block| match block {
            ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .ok_or("new state lacks the final assistant text")?;
    if final_text != FINAL_TEXT {
        return Err("new state contains an unexpected final assistant text".into());
    }
    Ok(ProbeReport::NewExact {
        source: source.to_string(),
        locator: locator.to_string(),
        canonical_items: items.len(),
        final_text: final_text.to_owned(),
    })
}

fn recovery_report(
    source: SessionLocator,
    recovery: &RecoveryRequiredReport,
) -> HarnessResult<ProbeReport> {
    let phase = match recovery.phase() {
        SessionRecoveryPhase::DispatchEvidence => RecoveryPhase::DispatchEvidence,
        SessionRecoveryPhase::EffectReconciliation => RecoveryPhase::EffectReconciliation,
        SessionRecoveryPhase::CommitRecovery => RecoveryPhase::CommitRecovery,
        SessionRecoveryPhase::Finalization => RecoveryPhase::Finalization,
        _ => return Err("unknown public recovery phase".into()),
    };
    let obligations = recovery
        .obligations()
        .iter()
        .map(|obligation| {
            let action = match obligation.action() {
                SessionRecoveryAction::CommitRecovery => RecoveryAction::CommitRecovery,
                SessionRecoveryAction::ProviderEvidenceRequired => {
                    RecoveryAction::ProviderEvidenceRequired
                }
                SessionRecoveryAction::EffectEvidenceRequired => {
                    RecoveryAction::EffectEvidenceRequired
                }
                SessionRecoveryAction::ProvenNotDispatched => RecoveryAction::ProvenNotDispatched,
                SessionRecoveryAction::FinalizationRequired => RecoveryAction::FinalizationRequired,
                _ => return Err("unknown public recovery action"),
            };
            Ok(RecoveryFact {
                operation_id: obligation.operation_id(),
                action,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ProbeReport::RecoveryRequired {
        source: source.to_string(),
        locator: recovery.locator().to_string(),
        phase,
        obligations,
        blocked: vec![
            BlockedWorkingWorkflow::Recover,
            BlockedWorkingWorkflow::ContextStatus,
            BlockedWorkingWorkflow::Compact,
            BlockedWorkingWorkflow::Fork,
            BlockedWorkingWorkflow::Rebind,
        ],
    })
}

fn write_locators(path: &Path, source: SessionLocator, latest: SessionLocator) -> io::Result<()> {
    write_fsynced_json(
        path,
        &LocatorState {
            version: PROTOCOL_VERSION,
            source: source.to_string(),
            latest: latest.to_string(),
        },
    )
}

fn session_workflow_error(stage: &'static str, error: &AgentSessionsError) -> io::Error {
    io::Error::other(format!("{stage} failed with {:?}", error.kind()))
}

pub(crate) fn write_reply(reply: &ChildReply) -> io::Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, reply).map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "the identity helper's own unit tests")]
mod tests {
    use super::{
        BoundaryController, ScriptPlan, loopback_identity_for_attempt,
        scripted_compaction_runtime_binding, scripted_runtime_binding,
        scripted_topology_runtime_binding,
    };
    use crate::protocol::CrashBoundary;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn scripted_compaction_preserves_source_configuration_identity() {
        let workspace = tempfile::tempdir().expect("workspace");
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            workspace.path().join("evidence.jsonl"),
        )
        .expect("controller");
        for process_tools in [false, true] {
            let script: ScriptPlan = serde_json::from_value(serde_json::json!({
                "version": 1,
                "process_tools": process_tools,
            }))
            .expect("script");
            let script = Arc::new(script);
            let source =
                scripted_runtime_binding(workspace.path(), controller.clone(), Arc::clone(&script))
                    .expect("source");
            let compact = scripted_compaction_runtime_binding(
                workspace.path(),
                controller.clone(),
                &script,
                None,
                None,
                Vec::new(),
            )
            .expect("compact");
            assert_eq!(source.target_binding(), compact.target_binding());

            let source = scripted_topology_runtime_binding(
                workspace.path(),
                controller.clone(),
                Arc::clone(&script),
                Arc::new(AtomicU64::new(1)),
                Vec::new(),
            )
            .expect("topology source");
            let compact = scripted_compaction_runtime_binding(
                workspace.path(),
                controller.clone(),
                &script,
                Some(Vec::new()),
                None,
                Vec::new(),
            )
            .expect("topology compact");
            assert_eq!(source.target_binding(), compact.target_binding());
        }
    }

    #[test]
    fn loopback_identity_is_deterministic_within_one_attempt() {
        let minted = loopback_identity_for_attempt("attempt-a", 7, 1, b"call").expect("uuid");
        let reminted = loopback_identity_for_attempt("attempt-a", 7, 1, b"call").expect("uuid");
        assert_eq!(minted, reminted);
    }

    #[test]
    fn loopback_identity_changes_across_attempts_at_the_same_position() {
        let first = loopback_identity_for_attempt("attempt-a", 7, 1, b"call").expect("uuid");
        let replayed = loopback_identity_for_attempt("attempt-b", 7, 1, b"call").expect("uuid");
        assert_ne!(first, replayed);
    }

    #[test]
    fn loopback_identity_separates_call_and_entry_kinds() {
        let call = loopback_identity_for_attempt("attempt-a", 7, 1, b"call").expect("uuid");
        let entry = loopback_identity_for_attempt("attempt-a", 7, 1, b"entry").expect("uuid");
        assert_ne!(call, entry);
    }
}
