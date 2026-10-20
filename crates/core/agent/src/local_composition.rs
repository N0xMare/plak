//! Authority-poor host seam for driver-owned local programmatic composition.
//!
//! The private runtime owns ordering, durable operations, nested authorization,
//! and cleanup. An integration behind this seam owns only its exact parser,
//! wire conversion, and evaluator session.

#![allow(
    missing_docs,
    reason = "unsupported sibling-composition DTOs are documented by the module contract"
)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    reason = "the narrow unsupported seam is consumed only by official sibling composition"
)]

use std::{future::Future, pin::Pin, sync::Arc};

use plakous_model::{CompiledProviderConfiguration, Digest};
use plakous_tools::{
    FrozenToolRegistry, ProviderModelProjectionIdentityV2, ToolExecutorId, ToolProjectionMap,
};
use serde_json::Value;

pub type LocalCompositionFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

pub type LocalCompositionSessionFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, LocalCompositionSessionLoss>> + Send + 'a>>;

/// Exact ownership lifetime of one admitted evaluator generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCompositionLifecycle {
    /// The evaluator is joined at the terminal boundary of every root turn.
    RootTurnJoined,
    /// A healthy idle evaluator survives successful root turns and compaction.
    AgentIncarnationPersistent,
}

/// Exact model-visible grammar and output-unit contract for evaluator results.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCompositionResultProjection {
    /// Existing Codex/product JavaScript token-budgeted `Script ...` grammar.
    TokensScriptV1,
    /// Byte-budgeted `PythonPtcV1` Python-cell grammar.
    BytesPythonPtcV1,
}

/// Declared runtime behavior, independent of tool spelling or language.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalCompositionRuntimeContract {
    /// Observation may yield without terminating the live cell.
    pub supports_yield: bool,
}

impl LocalCompositionRuntimeContract {
    /// `QuickJS` host contract.
    pub const JAVASCRIPT: Self = Self {
        supports_yield: true,
    };
    /// Monty host contract.
    pub const PYTHON: Self = Self {
        supports_yield: true,
    };
}

#[derive(Clone)]
pub struct LocalCompositionDescriptor {
    pub outer_projection: ToolProjectionMap,
    pub nested_projection: ToolProjectionMap,
    pub nested_tools: Arc<[LocalCompositionToolDefinition]>,
    pub identity: Digest,
    pub execute_name: Arc<str>,
    pub wait_name: Arc<str>,
    pub marker_executor: ToolExecutorId,
}

#[derive(Clone, Debug)]
pub struct LocalCompositionToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug)]
pub struct LocalCompositionExecuteInput {
    pub source: String,
    pub yield_time_ms: u64,
    pub max_output_tokens: usize,
}

/// Runtime-bound final source admission. Evaluators call this after their last
/// lock/join wait and immediately before admitting source execution. Waiting,
/// delivering an existing result, termination and shutdown do not use it.
#[derive(Clone)]
pub struct LocalCompositionStartAdmission {
    check: Arc<dyn Fn() -> LocalCompositionSessionFuture<'static, ()> + Send + Sync>,
}

impl LocalCompositionStartAdmission {
    pub fn new(
        check: impl Fn() -> LocalCompositionSessionFuture<'static, ()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            check: Arc::new(check),
        }
    }

    pub fn admit(&self) -> LocalCompositionSessionFuture<'static, ()> {
        (self.check)()
    }
}

impl std::fmt::Debug for LocalCompositionStartAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LocalCompositionStartAdmission(<runtime-bound>)")
    }
}

#[derive(Clone, Debug)]
pub struct LocalCompositionExecuteRequest {
    pub start_admission: LocalCompositionStartAdmission,
    pub tool_call_id: String,
    pub enabled_tools: Vec<LocalCompositionToolDefinition>,
    pub source: String,
    pub yield_time_ms: u64,
    pub max_output_tokens: usize,
}

#[derive(Clone, Debug)]
pub struct LocalCompositionWaitRequest {
    pub cell_id: String,
    pub yield_time_ms: u64,
}

#[derive(Clone, Debug)]
pub struct LocalCompositionWaitInput {
    pub cell_id: String,
    pub yield_time_ms: u64,
    pub max_output_tokens: usize,
    pub terminate: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LocalCompositionContentItem {
    Text(String),
    Image {
        url: String,
        detail: Option<LocalCompositionImageDetail>,
    },
    Audio(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCompositionImageDetail {
    Auto,
    Low,
    High,
    Original,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LocalCompositionResponse {
    Yielded {
        cell_id: String,
        items: Vec<LocalCompositionContentItem>,
    },
    Terminated {
        cell_id: String,
        items: Vec<LocalCompositionContentItem>,
    },
    Result {
        cell_id: String,
        items: Vec<LocalCompositionContentItem>,
        error: Option<String>,
    },
}

impl LocalCompositionResponse {
    pub fn cell_id(&self) -> &str {
        match self {
            Self::Yielded { cell_id, .. }
            | Self::Terminated { cell_id, .. }
            | Self::Result { cell_id, .. } => cell_id,
        }
    }
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminated { .. } | Self::Result { .. })
    }
}

pub enum LocalCompositionWaitOutcome {
    Live(LocalCompositionResponse),
    Missing(LocalCompositionResponse),
    Lost(LocalCompositionSessionLoss),
}

/// Closed reason a stateful evaluator owner can no longer continue safely.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalCompositionSessionLoss {
    HostLost(String),
    ProtocolViolation(String),
    Cancellation(String),
    AgentShutdown(String),
}

impl std::fmt::Display for LocalCompositionSessionLoss {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detail = match self {
            Self::HostLost(detail)
            | Self::ProtocolViolation(detail)
            | Self::Cancellation(detail)
            | Self::AgentShutdown(detail) => detail,
        };
        formatter.write_str(detail)
    }
}

#[derive(Clone, Debug)]
pub struct LocalCompositionNestedCall {
    pub cell_id: String,
    pub runtime_call_id: String,
    pub tool_name: String,
    pub input: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct LocalCompositionNotification {
    pub call_id: String,
    pub cell_id: String,
    pub text: String,
}

pub trait LocalCompositionNestedResponder: Send + Sync {
    fn resolve(
        self: Box<Self>,
        value: Value,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery>;
    /// Resolves with the committed canonical value and, when the executor
    /// produced one, the typed execution value of the same call. A responder
    /// that presents only canonical values keeps this default.
    fn resolve_with_execution(
        self: Box<Self>,
        value: Value,
        execution: Option<Arc<plakous_tools::ExecutionValue>>,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        let _ = execution;
        self.resolve(value)
    }
    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery>;
}

/// Exact host acknowledgement of one correlated nested result delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalCompositionNestedDelivery {
    /// The owning interpreter generation accepted the exact result.
    Accepted,
    /// The evaluator refused the acknowledgement because it names no pending
    /// request of the owning cell: an unissued id, a foreign cell, or a repeat
    /// of an already settled id. The cell and its genuine outstanding requests
    /// are undisturbed, and this is not a loss.
    Refused(String),
    /// The host proved the exact result never reached the cell: the cell had
    /// already finished, or the batch carrying the result was abandoned before
    /// it was handed to the interpreter. The cell cannot have observed it and
    /// cannot have acted on it, so the outcome is definite and no recovery is
    /// owed. A host reports this only when non-delivery is provable; when it
    /// cannot prove non-delivery it reports [`Self::ProtocolLost`].
    NotObserved(String),
    /// The host proved the exact result never reached the cell **while the cell
    /// is still waiting on it**: the host refused or failed before the result
    /// was handed over, so the cell has no outcome of its own and can no longer
    /// reach one. The composition ends, but it ends definitely: nothing was
    /// observed, nothing could have been acted on, and no recovery is owed.
    ///
    /// This is the verdict for a host that could not deliver, as against
    /// [`Self::NotObserved`] for a cell that had already finished.
    NotDelivered(String),
    /// The owning generation was reset before accepting the result.
    ///
    /// By this contract the reset preceded acceptance, so the result was
    /// provably not observed. A host that cannot order the two reports
    /// [`Self::ProtocolLost`] instead.
    GenerationLost,
    /// The owned host disappeared and acceptance could neither be proved nor
    /// ruled out. This is the one genuinely undetermined delivery outcome a
    /// host reports by disappearance.
    HostLost,
    /// The protocol could not prove acceptance of the exact result.
    ProtocolLost,
}

pub trait LocalCompositionNotificationResponder: Send {
    fn acknowledge(self: Box<Self>) -> bool;
    fn reject(self: Box<Self>, reason: String) -> bool;
}

pub enum LocalCompositionEvent {
    NestedCall {
        call: LocalCompositionNestedCall,
        responder: Box<dyn LocalCompositionNestedResponder>,
    },
    Notification {
        notification: LocalCompositionNotification,
        responder: Box<dyn LocalCompositionNotificationResponder>,
    },
}

pub trait LocalCompositionEventReceiver: Send + Sync {
    fn receive(&mut self) -> LocalCompositionSessionFuture<'_, Option<LocalCompositionEvent>>;
}

pub struct LocalCompositionStartedCell {
    pub cell_id: String,
    /// Observes an engine-owned cell. Profiles exposing a wait vocabulary must
    /// retain the cell and unread output for `wait` when this future is dropped.
    pub initial_response: LocalCompositionSessionFuture<'static, LocalCompositionResponse>,
}

pub enum LocalCompositionTerminateResult {
    Joined(LocalCompositionWaitOutcome),
    Lost(LocalCompositionSessionLoss),
}

pub enum LocalCompositionShutdownResult {
    Joined {
        /// Resource sample for the owned child process when one exited cleanly.
        child_exit: Option<ChildProcessExit>,
    },
    Lost(LocalCompositionSessionLoss),
}

/// How far a shutdown had to escalate before the worker process was gone.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChildExitEscalation {
    /// The worker exited on its own inside the shutdown bound.
    #[default]
    Voluntary,
    /// The worker was still alive at the bound and took `SIGTERM`.
    Terminated,
    /// `SIGTERM` was ignored and the worker took `SIGKILL`.
    Killed,
    /// The worker was still not reaped after `SIGKILL` and the final bound.
    Unreaped,
}

/// Content-free child-process exit sample returned from local-composition shutdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildProcessExit {
    /// Which local-composition worker process exited.
    pub child: crate::telemetry::ChildProcessKind,
    /// User CPU time attributed to the exit sample.
    pub user_cpu: std::time::Duration,
    /// System CPU time attributed to the exit sample.
    pub system_cpu: std::time::Duration,
    /// Peak resident set size when available.
    pub peak_rss_bytes: Option<u64>,
    /// How far the bounded reap had to escalate to make the worker exit.
    pub escalation: ChildExitEscalation,
}

pub trait LocalCompositionSession: Send + Sync {
    fn execute(
        &self,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'_, LocalCompositionStartedCell>;
    /// Observes an existing cell; dropping the observation must not cancel it
    /// or discard output. Termination is exclusively an explicit operation.
    fn wait(
        &self,
        request: LocalCompositionWaitRequest,
    ) -> LocalCompositionFuture<'_, LocalCompositionWaitOutcome>;
    fn terminate(
        &self,
        cell_id: String,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionTerminateResult> + Send + '_>>;
    fn shutdown(&self)
    -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>>;

    /// Whether a definite cell outcome cost this session its evaluator.
    ///
    /// A host that loses its evaluator *without* a definite outcome reports a
    /// [`LocalCompositionSessionLoss`] instead. This is the narrower case: the
    /// cell's outcome is known and delivered, and only the generation is gone,
    /// so the driver joins the dead session and records a typed reset rather
    /// than turning a known result into recovery. Source is never re-evaluated
    /// either way.
    fn generation_lost(&self) -> bool {
        false
    }
}

pub struct LocalCompositionSessionBundle {
    pub session: Box<dyn LocalCompositionSession>,
    pub events: Box<dyn LocalCompositionEventReceiver>,
}

pub trait LocalCompositionIntegration: Send + Sync {
    fn runtime_contract(&self) -> LocalCompositionRuntimeContract;
    /// Declares whether a healthy idle evaluator is root-turn or agent owned.
    fn lifecycle(&self) -> LocalCompositionLifecycle;
    /// Declares the exact result grammar and meaning of the numeric output cap.
    fn result_projection(&self) -> LocalCompositionResultProjection;
    fn environment_identity(&self) -> Vec<u8>;
    fn parse_execute(&self, input: Value) -> Result<LocalCompositionExecuteInput, String>;
    fn parse_wait(&self, input: Value) -> Result<LocalCompositionWaitInput, String>;
    fn create_session(
        &self,
        generation: u64,
        max_yield_time_ms: u64,
        max_output_tokens: usize,
    ) -> LocalCompositionFuture<'_, LocalCompositionSessionBundle>;
    fn shutdown(&self)
    -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>>;
    fn rebuild_descriptor(
        &self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String>;
    fn validate_configuration(
        &self,
        job_config: plakous_tools::JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String>;
    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        max_output_tokens: usize,
    ) -> Vec<LocalCompositionContentItem>;
    fn max_yield_time_ms(&self) -> u64;
    fn max_output_tokens(&self) -> usize;

    /// Maximum broker requests accepted but not yet answered, counting queued
    /// and executing children together. Read-only children may physically
    /// overlap so a child can depend on a later admitted child without
    /// deadlock. Mutating children remain deliberately serialized by ordinary
    /// effect authority; this limit does not promise concurrent mutations.
    fn max_outstanding_nested_calls(&self) -> usize {
        16
    }
}

#[derive(Clone)]
pub struct LocalCompositionConstruction {
    pub descriptor: LocalCompositionDescriptor,
    pub integration: Arc<dyn LocalCompositionIntegration>,
    pub attachment: LocalCompositionAttachmentIdentity,
}

/// Exact immutable identities supplied by the facade for one admitted binding.
///
/// The executable integration is deliberately separate: these facts are the
/// reconstruction authority persisted in the runtime manifest, while the
/// process-local integration must freshly prove the same artifact on reopen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCompositionAttachmentIdentity {
    pub profile: Digest,
    pub artifact: Digest,
    pub binding: Digest,
    pub engine_contract: Digest,
    pub evidence_receipt: Option<Digest>,
}
