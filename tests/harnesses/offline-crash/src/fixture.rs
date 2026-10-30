use std::collections::BTreeMap;
use std::error::Error;
use std::future;
use std::io::{self, BufReader, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

mod assertions;
pub(crate) use assertions::*;
mod bindings;
pub use bindings::*;
mod boundary;
use boundary::pause_if;
mod coding;
pub use coding::*;
mod loopback;
mod responses;
use responses::{
    loopback_response, native_summary_response, scripted_error, scripted_stall_is_deaf,
    scripted_stream, stall,
};

#[cfg(test)]
use responses::loopback_identity_for_attempt;

pub(crate) type HarnessResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const FINAL_TEXT: &str = "loopback turn committed";

pub(super) fn descriptor_with_projected_input(
    account: &[u8],
    maximum_projected_input: u64,
) -> HarnessResult<ResolvedModelDescriptor> {
    descriptor_over_protocol(account, maximum_projected_input, LOOPBACK_PROTOCOL, false)
}

pub(super) fn fixture_time() -> HarnessResult<Timestamp> {
    Ok(Timestamp::new(10, 0)?)
}

#[derive(Clone)]
/// Fsynced loopback model/tool boundary controller for repository process tests.
pub struct BoundaryController {
    cut: CrashBoundary,
    evidence: Arc<FsyncEvidenceLog>,
    announce_stdout: bool,
    release_file: Option<Arc<PathBuf>>,
}

struct LoopbackModel {
    credential_prepared: AtomicBool,
    credential_ready: AtomicBool,
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
    ScriptedSummary(Arc<ScriptPlan>, FinishReason),
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

/// The two Code Mode outer marker names (`composition_markers.rs`).
const CODE_MODE_EXEC_NAME: &str = "exec";
const CODE_MODE_WAIT_NAME: &str = "wait";

struct LoopbackEffect {
    controller: BoundaryController,
    /// When set, the effect reports a bounded execution failure instead of a
    /// successful outcome, so a driver can be scripted against a tool error.
    fail: bool,
    /// When set, the successful outcome carries a body of this many bytes, so
    /// a turn can grow its own context past a small admitted ceiling.
    body_bytes: Option<usize>,
}

/// The loopback protocol family every ordinary scripted binding speaks.
const LOOPBACK_PROTOCOL: &str = "offline.loopback.responses";

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

enum ScriptedOperation {
    Turn(Arc<AtomicU64>),
    ArtifactTurn(Arc<AtomicU64>),
    Compact(FinishReason),
}

#[derive(Debug, Deserialize, Serialize)]
struct LocatorState {
    version: u16,
    source: String,
    latest: String,
}

/// What an open of the probed head did, for the prediction check.
#[derive(Clone, Copy)]
enum Opened<'a> {
    Failed,
    RecoveryRequired(&'a RecoveryRequiredReport),
    Ordinary { abandoned: bool },
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
