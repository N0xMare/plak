//! Unsupported sibling seams for official agent and hosting implementations.
//!
//! These preserve one-way dependencies while keeping implementation details
//! out of the supported tools inventory.

#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    missing_docs,
    reason = "unsupported sibling seam is documented as one implementation tier"
)]

use std::num::NonZeroU64;
use std::sync::Arc;

pub use plakous_model::redaction as preview;

/// The runner seam: a backend whose operations run on a runner, and the
/// runner side that answers them (spec 01 §8.5). Unsupported, and never
/// selected for a session by this crate.
#[doc(hidden)]
pub mod runner {
    pub use crate::environment::runner::{
        LocalDaemonBackend, PlacementGate, RunnerBackend, RunnerCall, RunnerCallFuture,
        RunnerClass, RunnerExtension, RunnerFailure, RunnerLink, RunnerOpenFuture,
        RunnerSecretResolver, RunnerServices, RunnerSessions, RunnerStreams, RunnerVerb,
        RunnerVerbFamily, RunnerWorkspaceProbe, ServeFailure, ServeFuture, ServeServiceFuture,
        adopt_runner_process_sessions, answer_too_large, call_blocking, decode_jobs_snapshot,
        decode_preempt, encode_preempt, environment_failure, job_capacity, line_sessions,
        local_daemon_environment, lost_process_owner, requested_workspace, runner_descriptor,
        runner_environment, serve, serve_failure, serve_jobs_watch, serve_probe,
        serve_service_spawn, serve_service_terminate, serve_sessions, service_spawned,
        terminated_service, with_runner_secrets, workspace_refusal,
    };
}

use plakous_model::{AttemptId, CancellationToken, Clock, Digest, RunId, Timestamp, TurnId};

use crate::{
    EffectAuthorizationBinding, EffectAuthorizationGrant, EffectAuthorizationGrantSource,
    EffectDecision, EffectDecisionDisposition, ExecutionEnvironment,
    ExecutionEnvironmentDescriptor, FrozenToolRegistry, ProgressSink, ToolContext, ToolExecutor,
    ToolInvocation, ToolOutcome, ToolOutcomeLimits, ToolPolicyRequest, ToolPolicyRevision,
    ToolRuntimeError, ToolRuntimeErrorKind,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessSessionUpdate {
    Promoted {
        session_id: [u8; 16],
        owner_generation: NonZeroU64,
        number: u32,
    },
    JobTerminal {
        number: u32,
    },
    JobTerminals {
        numbers: Vec<u32>,
    },
    Active {
        session_id: [u8; 16],
        owner_generation: NonZeroU64,
    },
    Joined {
        session_id: [u8; 16],
        owner_generation: NonZeroU64,
    },
}

/// Hydrates durable rows as read-only snapshots; never reconstructs a live process.
pub fn restore_job_terminals(sessions: &crate::JoinedProcessSessions, views: &[crate::JobView]) {
    sessions.restore_job_terminals(views);
}

pub fn sanitize_job_command(command: &str) -> String {
    let mut text = String::new();
    for ch in command.chars() {
        match ch {
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            ch if ch.is_control() => text.push('\u{fffd}'),
            ch => text.push(ch),
        }
    }
    let mut end = text.len().min(1024);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

pub fn seed_job_counter(sessions: &crate::JoinedProcessSessions, next_id: u32) {
    sessions.seed_job_counter(next_id);
}

pub fn set_job_owner(sessions: &crate::JoinedProcessSessions, owner: String) {
    sessions.set_job_owner(owner);
}

pub fn close_job_admission(sessions: &crate::JoinedProcessSessions, owner: &str) {
    sessions.close_job_admission(owner);
}

pub fn stop_jobs_for_owner(
    sessions: &crate::JoinedProcessSessions,
    owner: &str,
    by: crate::JobStopReason,
    grace: std::time::Duration,
) -> Result<(), crate::ExecutionBackendError> {
    sessions.stop_jobs_for_owner(owner, by, grace)
}

/// Terminates unpromoted processes after their instance's authority ended.
///
/// Successful stops remain reportable even when another process fails cleanup.
pub fn stop_foreground_after_owner_loss(
    sessions: &crate::JoinedProcessSessions,
) -> ForegroundProcessStops {
    sessions.stop_foreground_after_owner_loss()
}

/// Live foreground identity pairs. A process.start answer/Ack does not mean
/// its driver operation has settled.
pub fn live_foreground(sessions: &crate::JoinedProcessSessions) -> Vec<(String, u32)> {
    sessions.live_foreground()
}

/// Stops only the unpromoted process whose starting call and PID both match.
pub fn stop_matching_foreground(
    sessions: &crate::JoinedProcessSessions,
    call_key: &str,
    pid: u32,
) -> ForegroundProcessStops {
    sessions.stop_foreground_matching(Some((call_key, pid)))
}

/// Completed foreground stops and any still-pending cleanup failure.
pub struct ForegroundProcessStops {
    /// Processes the terminate path joined.
    pub stopped: Vec<ForegroundProcessStop>,
    /// A remaining process exceeded the cleanup bound.
    pub failure: Option<crate::ExecutionBackendError>,
}

/// Diagnostic evidence for an unpromoted process; never a durable job row.
pub struct ForegroundProcessStop {
    /// Starting tool call and attempt, or `unkeyed` for a host probe.
    pub call_key: String,
    /// Former process leader, for diagnostics only.
    pub pid: u32,
    /// Status after the existing terminate path joined it.
    pub status: crate::JoinedProcessStatus,
}

pub fn next_job_number(sessions: &crate::JoinedProcessSessions) -> u32 {
    sessions.next_job_number()
}

pub fn promote(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
) -> Result<(), crate::ExecutionBackendError> {
    sessions.promote(id)
}

/// Room under the jobs root for one promoted job's output file.
#[derive(Debug)]
#[must_use]
pub struct JobOutputReservation {
    fits: bool,
}

impl JobOutputReservation {
    const fn into_fits(self) -> bool {
        self.fits
    }
}

/// Sweeps and budgets the jobs root for one promotion of `id`, unless the
/// promotion would be refused now. It walks the root: async callers run it on
/// the blocking pool, then [`promote_reserved`] inline.
pub fn reserve_job_output(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
) -> JobOutputReservation {
    JobOutputReservation {
        fits: sessions.reserve_job_output(id),
    }
}

/// Promotes without walking the jobs root, so an async caller publishes the
/// job in the same poll that produces its result and cancellation cannot
/// drop the caller after the job's ownership moved to the job table.
pub fn promote_reserved(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
    reservation: JobOutputReservation,
) -> Result<(), crate::ExecutionBackendError> {
    sessions.promote_reserved(id, reservation.into_fits())
}

pub fn consume_jobs(sessions: &crate::JoinedProcessSessions, numbers: &[u32]) {
    sessions.consume_jobs(numbers);
}

/// What a job's output tail starts with when it was cut inside its only
/// line: the text after it is the end of a line whose start is gone.
pub const JOB_TAIL_FRAGMENT: &str = "…";

/// The most bytes a job's output tail holds.
const JOB_TAIL_BYTES: usize = 4096;

/// Bounds and sanitizes a terminal tail without letting process controls enter a checkpoint.
///
/// A tail always starts where a line does: where `text` does, after a line
/// its line bound left out, or, when its byte bound cut into a line, at the
/// next line. A tail cut inside its only line starts with
/// [`JOB_TAIL_FRAGMENT`] instead. A tail is its own tail.
pub fn job_output_tail(text: &str) -> String {
    output_tail(text, false)
}

/// [`job_output_tail`] of `text`, the end of an output whose earlier bytes
/// were cut away when `cut`: its start is then not a line's start either.
pub(crate) fn output_tail(text: &str, cut: bool) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "");
    let text = normalized.as_str();
    let text = text.strip_suffix('\n').unwrap_or(text);
    let mut reversed = Vec::new();
    let mut bytes = 0;
    let mut lines = 1;
    // Whether the kept text starts inside a line.
    let mut partial = cut;
    for ch in text.chars().rev() {
        if ch == '\n' {
            if lines == 40 {
                partial = false;
                break;
            }
            lines += 1;
        }
        let ch = if ch.is_control() && ch != '\n' && ch != '\t' {
            '\u{fffd}'
        } else {
            ch
        };
        if bytes + ch.len_utf8() > JOB_TAIL_BYTES {
            partial = true;
            break;
        }
        bytes += ch.len_utf8();
        reversed.push(ch);
    }
    let kept: String = reversed.into_iter().rev().collect();
    if !partial {
        return kept;
    }
    if let Some((_, whole)) = kept.split_once('\n') {
        return whole.to_owned();
    }
    let mut start = kept
        .len()
        .saturating_sub(JOB_TAIL_BYTES - JOB_TAIL_FRAGMENT.len());
    while !kept.is_char_boundary(start) {
        start += 1;
    }
    format!("{JOB_TAIL_FRAGMENT}{}", &kept[start..])
}

fn make_process_session_update(
    sessions: &crate::JoinedProcessSessions,
    session_id: crate::JoinedProcessSessionId,
    joined: bool,
) -> ProcessSessionUpdate {
    let owner_generation = sessions.runtime_owner_generation();
    if sessions.is_promoted(session_id) {
        let number = crate::JoinedProcessSessions::job_number(session_id);
        if joined {
            ProcessSessionUpdate::JobTerminal { number }
        } else {
            ProcessSessionUpdate::Promoted {
                session_id: session_id.runtime_bytes(),
                owner_generation,
                number,
            }
        }
    } else if joined {
        ProcessSessionUpdate::Joined {
            session_id: session_id.runtime_bytes(),
            owner_generation,
        }
    } else {
        ProcessSessionUpdate::Active {
            session_id: session_id.runtime_bytes(),
            owner_generation,
        }
    }
}

pub fn process_session_active(
    outcome: ToolOutcome,
    sessions: &crate::JoinedProcessSessions,
    session_id: crate::JoinedProcessSessionId,
) -> ToolOutcome {
    outcome.with_process_session_update(make_process_session_update(sessions, session_id, false))
}

pub fn process_session_joined(
    outcome: ToolOutcome,
    sessions: &crate::JoinedProcessSessions,
    session_id: crate::JoinedProcessSessionId,
) -> ToolOutcome {
    outcome.with_process_session_update(make_process_session_update(sessions, session_id, true))
}

/// Starts a process whose full output is spooled only once its first page
/// would truncate (or its in-memory window would evict output).
pub fn start_process_with_output_limit(
    sessions: &crate::JoinedProcessSessions,
    request: crate::JoinedProcessStartRequest,
    control: crate::OperationContext,
    max_output_bytes: usize,
) -> Result<crate::JoinedProcessSessionId, crate::ExecutionBackendError> {
    sessions.start_with_output_limit(request, control, max_output_bytes)
}

pub fn process_job_terminals(outcome: ToolOutcome, numbers: Vec<u32>) -> ToolOutcome {
    if numbers.is_empty() {
        outcome
    } else {
        outcome.with_process_session_update(ProcessSessionUpdate::JobTerminals { numbers })
    }
}

pub fn process_session_update(outcome: &ToolOutcome) -> Option<ProcessSessionUpdate> {
    outcome.process_session_update()
}

/// How a raising program rendered this outcome as a text file read, if it did.
pub fn raised_file_read(outcome: &ToolOutcome) -> Option<&RaisedFileRead> {
    outcome.file_read()
}

/// How a raising program rendered a text file read. The runtime's observation
/// cut keys on this mark, never on the text, to keep the read head-first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RaisedFileRead {
    /// The selected text, then one read metadata line the cut rewrites.
    PlainWithMetadata,
    /// Rendered lines, then notes and a continuation the presentation owns.
    Page(RaisedReadPage),
}

/// One raised read page: rendered lines, then the presentation's notes and,
/// when the page stops short of the end of the file, where to continue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RaisedReadPage {
    style: crate::exposure::ReadPageStyle,
    offset: u64,
    lines_bytes: usize,
    text_bytes: usize,
    /// The page's clipping and decoding notes, rendered.
    notes: Vec<String>,
}

impl RaisedReadPage {
    pub(crate) const fn new(
        style: crate::exposure::ReadPageStyle,
        offset: u64,
        lines_bytes: usize,
        text_bytes: usize,
        notes: Vec<String>,
    ) -> Self {
        Self {
            style,
            offset,
            lines_bytes,
            text_bytes,
            notes,
        }
    }

    /// Length of the raised text this page describes: the wrapped string of a
    /// `{"result": string}` presentation, or the whole text otherwise.
    pub const fn text_bytes(&self) -> usize {
        self.text_bytes
    }

    /// Length of the leading part of that text holding the rendered lines.
    pub const fn lines_bytes(&self) -> usize {
        self.lines_bytes
    }

    /// The page cut after its first `shown` lines. `head` is those lines,
    /// each ending in a newline, or the caller's withheld notice and a
    /// newline when `shown` is 0. The page's notes follow, then the
    /// presentation's continuation from the line after the last one shown.
    pub fn cut(&self, head: &str, shown: u64) -> String {
        crate::exposure::cut_read_page(self.style, head, &self.notes, self.offset, shown)
    }
}

#[derive(Default)]
pub struct ProcessQuietPollState {
    pub completion_notices: bool,
    pub leftover_observed: bool,
    pub quiet_polls: u32,
    pub output_cursor: u64,
    pub observers: u32,
    pub repeat_observers: u32,
    pub joined: bool,
}

pub type ProcessSessionProjectionState = (
    Arc<std::sync::Mutex<crate::JoinedProcessOutputCursor>>,
    Arc<std::sync::Mutex<Vec<u8>>>,
    Arc<std::sync::Mutex<ProcessQuietPollState>>,
);

pub fn set_process_unread_mail_pending(sessions: &crate::JoinedProcessSessions, pending: bool) {
    sessions.set_unread_mail_pending(pending);
}

pub fn process_unread_mail_pending(sessions: &crate::JoinedProcessSessions) -> bool {
    sessions.unread_mail_pending()
}

pub fn set_process_notice_delivery_available(
    sessions: &crate::JoinedProcessSessions,
    available: bool,
) {
    sessions.set_notice_delivery_available(available);
}

pub fn process_notice_delivery_available(sessions: &crate::JoinedProcessSessions) -> bool {
    sessions.notice_delivery_available()
}

pub fn process_session_projection_state(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
) -> Result<ProcessSessionProjectionState, crate::ExecutionBackendError> {
    sessions.projection_state(id)
}

pub fn process_session_tail(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
    max_bytes: usize,
    max_records: usize,
) -> Result<crate::JoinedProcessCursorPollResult, crate::ExecutionBackendError> {
    sessions.poll_tail(id, max_bytes, max_records)
}

/// Subscribe before polling to observe output and status without lost wakeups.
pub fn process_session_changes(
    sessions: &crate::JoinedProcessSessions,
    id: crate::JoinedProcessSessionId,
) -> Result<tokio::sync::watch::Receiver<u64>, crate::ExecutionBackendError> {
    sessions.session_changes(id)
}

pub fn reported_process_session(
    sessions: &crate::JoinedProcessSessions,
    number: u32,
) -> Option<(crate::JoinedProcessSessionId, crate::JoinedProcessStatus)> {
    sessions.reported_session_for_number(number)
}

pub fn active_process_session_identities(
    sessions: &crate::JoinedProcessSessions,
) -> Vec<([u8; 16], NonZeroU64)> {
    let generation = sessions.runtime_owner_generation();
    sessions
        .active_session_ids()
        .into_iter()
        .map(|id| (id.runtime_bytes(), generation))
        .collect()
}

pub fn joined_process_session_identities(
    sessions: &crate::JoinedProcessSessions,
) -> Vec<([u8; 16], NonZeroU64)> {
    let generation = sessions.runtime_owner_generation();
    sessions
        .joined_terminal_identities()
        .into_iter()
        .map(|id| (id, generation))
        .collect()
}

pub fn policy_request(
    invocation: ToolInvocation,
    definition: plakous_model::ToolDefinition,
    environment: Arc<ExecutionEnvironment>,
    policy_revision: ToolPolicyRevision,
    authorization_binding: EffectAuthorizationBinding,
) -> ToolPolicyRequest {
    ToolPolicyRequest::new(
        invocation,
        definition,
        environment,
        policy_revision,
        authorization_binding,
    )
}

pub fn authorization_binding(
    invocation: &ToolInvocation,
    environment_descriptor: ExecutionEnvironmentDescriptor,
    policy_revision: ToolPolicyRevision,
) -> Result<EffectAuthorizationBinding, ToolRuntimeError> {
    EffectAuthorizationBinding::for_invocation(invocation, environment_descriptor, policy_revision)
}

pub fn effect_decision_disposition(decision: EffectDecision) -> EffectDecisionDisposition {
    decision.into_disposition()
}

pub const fn authorization_grant(
    binding: EffectAuthorizationBinding,
    source: EffectAuthorizationGrantSource,
) -> EffectAuthorizationGrant {
    EffectAuthorizationGrant::new(binding, source)
}

pub fn validate_outcome_limits(
    limits: ToolOutcomeLimits,
) -> Result<ToolOutcomeLimits, ToolRuntimeError> {
    limits.validate()
}

pub fn enforce_outcome_limits(
    outcome: ToolOutcome,
    limits: ToolOutcomeLimits,
) -> Result<ToolOutcome, ToolRuntimeError> {
    outcome.enforce_limits(limits)
}

pub fn enforce_completed_output_contract(
    outcome: ToolOutcome,
    contract: &plakous_model::ToolOutputContract,
    effect: plakous_model::ToolEffect,
) -> ToolOutcome {
    outcome.enforce_completed_output_contract(contract, effect)
}

pub fn runtime_outcome_unknown() -> ToolOutcome {
    ToolOutcome::runtime_outcome_unknown()
}

pub fn executor(
    registry: &FrozenToolRegistry,
    invocation: &ToolInvocation,
) -> Result<Arc<dyn ToolExecutor>, ToolRuntimeError> {
    registry.executor(invocation)
}

pub fn bind_effect_dispatch(
    invocation: ToolInvocation,
    intent_digest: Digest,
    claim_evidence_digest: Digest,
) -> ToolInvocation {
    invocation.with_effect_dispatch(intent_digest, claim_evidence_digest)
}

pub use crate::visual_read::VisualReadSession;

/// Marks images decoded by the installed host normalizer in this live outcome.
pub fn mark_tool_images_prepared(mut outcome: ToolOutcome) -> ToolOutcome {
    outcome.images_prepared = true;
    outcome
}

/// Whether this live outcome's images have already passed the host normalizer.
pub const fn tool_images_prepared(outcome: &ToolOutcome) -> bool {
    outcome.images_prepared
}

/// Omits images without changing a completed tool's semantic outcome.
pub fn omit_tool_images(
    outcome: ToolOutcome,
    reason: &str,
) -> Result<ToolOutcome, ToolRuntimeError> {
    crate::outcome::images::omit_images(outcome, reason)
}

/// Runs one decoder under the process-wide two-decoder bound.
pub(crate) static IMAGE_DECODERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Runs one decoder under the process-wide two-decoder bound.
pub async fn normalize_tool_image(
    normalizer: Arc<dyn crate::ImageNormalizer>,
    bytes: Result<Vec<u8>, String>,
    cancellation: &CancellationToken,
) -> Result<Result<crate::PreparedImage, String>, ToolRuntimeError> {
    use std::future::Future as _;
    use std::pin::Pin;
    use std::task::Poll;

    let mut acquire = std::pin::pin!(IMAGE_DECODERS.acquire());
    let mut cancelled = cancellation.cancelled();
    let permit = std::future::poll_fn(|cx| {
        if Pin::new(&mut cancelled).poll(cx).is_ready() {
            return Poll::Ready(Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "image normalization cancelled",
            )));
        }
        acquire.as_mut().poll(cx).map(|permit| {
            permit.map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "image decoder semaphore closed",
                )
            })
        })
    })
    .await?;
    join_blocking(cancellation, move || {
        let _permit = permit;
        bytes.and_then(|bytes| normalizer.normalize(&bytes))
    })
    .await
}

/// Installs the host's bound, authority-narrowing tool-image policy.
pub fn install_tool_result_images(
    environment: ExecutionEnvironment,
    allowed: bool,
) -> ExecutionEnvironment {
    environment.with_tool_result_images(allowed)
}

/// Applies route acceptance and bounded decoding before a tool result enters model history.
pub async fn prepare_tool_images(
    outcome: ToolOutcome,
    environment: &ExecutionEnvironment,
    cancellation: &CancellationToken,
) -> Result<ToolOutcome, ToolRuntimeError> {
    crate::outcome::prepare_tool_images(outcome, environment, cancellation).await
}

/// Installs image-read facts the agent derived from its provider configuration.
///
/// Not part of the environment descriptor, and not a supported inventory item.
pub fn install_visual_read(
    environment: ExecutionEnvironment,
    route: impl Into<String>,
    model: impl Into<String>,
    accepts_images: bool,
    normalizer: Option<Arc<dyn crate::ImageNormalizer>>,
) -> ExecutionEnvironment {
    environment.with_visual_read(Arc::new(VisualReadSession::new(
        route,
        model,
        accepts_images,
        normalizer,
    )))
}

/// The image-read facts installed on `environment`, if any.
pub fn visual_read(environment: &ExecutionEnvironment) -> Option<&VisualReadSession> {
    environment.visual_read()
}

/// Runs `work` on the blocking pool and waits for it without occupying an
/// async worker. The harness calls this for image decode.
///
/// Cancellation returns `Cancelled` at once and drops the handle: the work
/// already on the pool runs to completion and its result is discarded.
pub async fn join_blocking<T: Send + 'static>(
    cancellation: &CancellationToken,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ToolRuntimeError> {
    use std::future::Future as _;
    use std::pin::Pin;
    use std::task::Poll;

    let mut handle = tokio::task::spawn_blocking(work);
    let mut cancelled = cancellation.cancelled();
    std::future::poll_fn(|cx| {
        if Pin::new(&mut cancelled).poll(cx).is_ready() {
            return Poll::Ready(Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "tool execution was cancelled",
            )));
        }
        Pin::new(&mut handle).poll(cx).map(|joined| {
            joined.map_err(|_| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, "blocking tool task failed")
            })
        })
    })
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "the context authority is explicit"
)]
pub fn tool_context(
    run_id: RunId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    cancellation: CancellationToken,
    deadline: Option<Timestamp>,
    clock: Arc<dyn Clock>,
    environment: Arc<ExecutionEnvironment>,
    authorization_grant: EffectAuthorizationGrant,
    progress: ProgressSink,
) -> ToolContext {
    ToolContext::authorized(
        run_id,
        turn_id,
        attempt_id,
        cancellation,
        deadline,
        clock,
        environment,
        authorization_grant,
        progress,
    )
}

// ---------------------------------------------------------------------------
// Projection construction for the composition crates.
// ---------------------------------------------------------------------------
//
// A composition presents one or two outer tools and runs the pack tools inside
// a cell, so it builds its own projections instead of taking a compiled menu.
// `ToolProjectionMap` retains executable authority, so its constructors are
// not part of the supported inventory; they are here, on the same unsupported
// tier as the agent's dispatch seam, for `plakous-compose`.

use crate::{
    CanonicalProcessOperationV2, PortableToolSchemaV2, ProviderModelProjectionIdentityV2,
    RegisteredToolIdentity, ToolProjectionExposureV2, ToolProjectionLoweringV2, ToolProjectionMap,
    ToolProjectionRaisingV2,
};
use plakous_model::{ProjectedCustomToolFormatV2, SemanticJsonV1, Utf8TextContractV1};

pub fn projection_map(
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    entries: impl IntoIterator<Item = ToolProjectionExposureV2>,
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    ToolProjectionMap::new(registry, target, entries)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one pass-through of the exposure constructor"
)]
pub fn projection_exposure(
    canonical_operations: impl IntoIterator<
        Item = (CanonicalProcessOperationV2, RegisteredToolIdentity),
    >,
    exposed_name: impl Into<String>,
    description: impl Into<String>,
    input_schema: PortableToolSchemaV2,
    defaults: SemanticJsonV1,
    lowering: ToolProjectionLoweringV2,
    output_schema: PortableToolSchemaV2,
    raising: ToolProjectionRaisingV2,
    target: ProviderModelProjectionIdentityV2,
) -> Result<ToolProjectionExposureV2, ToolRuntimeError> {
    ToolProjectionExposureV2::new(
        canonical_operations,
        exposed_name,
        description,
        input_schema,
        defaults,
        lowering,
        output_schema,
        raising,
        target,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "one pass-through of the custom exposure constructor"
)]
pub fn projection_exposure_custom(
    canonical_operations: impl IntoIterator<
        Item = (CanonicalProcessOperationV2, RegisteredToolIdentity),
    >,
    exposed_name: impl Into<String>,
    description: impl Into<String>,
    input: Utf8TextContractV1,
    format: ProjectedCustomToolFormatV2,
    defaults: SemanticJsonV1,
    lowering: ToolProjectionLoweringV2,
    output_schema: PortableToolSchemaV2,
    raising: ToolProjectionRaisingV2,
    target: ProviderModelProjectionIdentityV2,
) -> Result<ToolProjectionExposureV2, ToolRuntimeError> {
    ToolProjectionExposureV2::new_custom(
        canonical_operations,
        exposed_name,
        description,
        input,
        format,
        defaults,
        lowering,
        output_schema,
        raising,
        target,
    )
}

/// Neutral operation labels supplied by the process adapter, not inferred by L0.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOperationKind {
    Start,
    Poll,
    WriteWait,
    Await,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessHintSource {
    Default,
    ModelHint,
    Background,
}

/// Content-free process observation, independent of retained tool text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessTelemetry {
    Promoted {
        operation: ProcessOperationKind,
        hint_source: ProcessHintSource,
        hint_ms: u64,
        preempt: crate::ToolPreempt,
    },
    Waited {
        operation: ProcessOperationKind,
        yielded: bool,
    },
}

pub fn with_process_telemetry(
    mut outcome: ToolOutcome,
    observation: ProcessTelemetry,
) -> ToolOutcome {
    outcome.process_telemetry = Some(observation);
    outcome
}

pub fn process_telemetry(outcome: &ToolOutcome) -> Option<ProcessTelemetry> {
    outcome.process_telemetry
}

#[cfg(test)]
mod tests;
