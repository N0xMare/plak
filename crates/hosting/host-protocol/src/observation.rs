//! Observations: what the hosted session tells its driver.

use serde::{Deserialize, Serialize};

use crate::intent::{Core, Draft, ProtocolExtension, SessionIntent};
use crate::selection::{CompositionSelection, ProtocolSelection};
use crate::views::transcript::TranscriptRow;
use crate::views::{
    ActivityKind, AgentTranscript, CallDetail, ContextUsage, PresentationSnapshot, ProviderDef,
    ResumeSessionEntry, SessionInfoView, SubscriptionUsageView, TemplateRow, TreeView,
    UsageLimitsView,
};

/// Observations the hosted session publishes to its driver: transcript and
/// activity deltas, immutable view snapshots, admission receipts, and the
/// binding/usage projections.
///
/// The closed protocol is every variant but [`Self::Extension`].
#[allow(
    clippy::large_enum_variant,
    reason = "the bounded in-process observation protocol keeps typed snapshots inline"
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionObservation<P: ProtocolExtension = Core> {
    /// A process entered the session-owned job ledger.
    JobStarted {
        /// Vendor-independent process id (`p<N>`).
        job_id: String,
        /// Owning agent identity.
        owner: String,
        /// Display command, not process authority.
        command: String,
    },
    /// The process detached from foreground dispatch cancellation.
    JobPromoted {
        /// Vendor-independent process id (`p<N>`).
        job_id: String,
        /// Owning agent identity.
        owner: String,
    },
    /// An exact terminal snapshot became available (not a model notice receipt).
    JobTerminal {
        /// Vendor-independent process id (`p<N>`).
        job_id: String,
        /// Owning agent identity.
        owner: String,
        /// `exited`, `stopped`, or `outcome_unknown`.
        status: String,
        /// Natural exit code, when known.
        exit_code: Option<i32>,
        /// Stop attribution, when applicable.
        stop_reason: Option<String>,
        /// Elapsed process time, in milliseconds.
        elapsed_ms: u64,
        /// Retained bounded output tail.
        output: String,
    },
    /// Replace the typed, observational conversation/activity presentation.
    PresentationSnapshot(PresentationSnapshot),
    /// Replace product-interpreted provider presentation data.
    ProviderSnapshot {
        /// Every provider supported by this product build, including signed-out rows.
        providers: Vec<ProviderDef>,
    },
    /// Non-authoritative, non-transcript lifecycle activity.
    Activity {
        /// Compact operator-facing activity label.
        label: String,
        /// Whether work is still in progress.
        active: bool,
    },
    /// Whether the Plak controller has attached the single canonical agent runtime.
    AgentAttached(bool),
    /// Append one typed transcript row. The same row a restore reads back
    /// from the product transcript, so live and restored rows match.
    Row(TranscriptRow),
    /// Replace status strip fields.
    Status {
        /// Phase label: `"idle"` | `"running"` | `"interrupting"` |
        /// `"interrupted"` | `"error"`. `"interrupting"` means a cancel was
        /// requested and has not been observed to settle.
        phase: String,
        /// Number of product-visible queued follow-ups.
        queue_len: usize,
        /// Optional short status detail (tool name, error snippet, etc.).
        detail: Option<String>,
    },
    /// Replace the complete product queue view and its delivery authority.
    QueueSnapshot {
        /// Ordered FIFO queue items. This replaces the entire prior UI snapshot.
        items: Vec<QueueItem>,
        /// Whether the host may auto-drain or awaits explicit recovery approval.
        drain: QueueDrainState,
    },
    /// Replace the terminal context-capacity recovery presentation.
    ContextCapacityRecovery(Option<ContextCapacityRecovery>),
    /// Replace and open a cwd-scoped exact-resume picker snapshot.
    ///
    /// The app host must filter these rows to the active cwd. Selecting a row
    /// emits [`SessionIntent::ResumeSession`] with its full, unmodified id; the host
    /// remains the authority that validates and performs the exact switch.
    ResumeSessions(Vec<ResumeSessionEntry>),
    /// Explicit operator invocation provenance, independent of root-turn origin.
    ToolInvocationOrigin {
        /// Generated operation UUID shared with its tool observations.
        call_id: String,
        /// Surface command identity; never the operation identity.
        command_id: String,
    },
    /// Durable prompt admission identity for optional surface correlation.
    PromptAdmitted(QueueItem),
    /// Prompt delivery boundary; an unknown identity clears the prior origin.
    PromptStarted(Option<QueueItem>),
    /// Bounded observation loss invalidated correlation for this attachment.
    PromptCorrelationReset,
    /// The host started handling one queued action: the sender's opaque
    /// correlation when it gave one. Answers until the next boundary belong to
    /// that action; `None` means they belong to no caller's command.
    IntentStarted(Option<String>),
    /// One text action crossed the host's durable product admission boundary.
    DraftAccepted(SessionIntent<P>),
    /// One text action was refused before durable product admission.
    DraftRejected(SessionIntent<P>),
    /// Restore a prompt, its images included, after runtime admission without
    /// changing caller correlation.
    DraftRestored(Draft),
    /// A queued edit was durably applied; its submitted composer text may clear.
    QueueEditApplied {
        /// Stable queue item identity.
        id: String,
    },
    /// A queued edit was refused; the editable text and refreshed revision remain.
    QueueEditRefused {
        /// Stable queue item identity.
        id: String,
    },
    /// Set busy flag for key routing (Enter → `FollowUp` vs `Prompt`).
    SetBusy(bool),
    /// Replace current-session submitted-prompt recall (oldest to newest).
    PromptHistorySnapshot(Vec<String>),
    /// Structured user message block: the live echo of the admitted
    /// prompt, with the send time its transcript row carries.
    UserMessage {
        /// Prompt text.
        text: String,
        /// When the operator sent it, Unix milliseconds.
        sent_at_ms: Option<u64>,
    },
    /// Streaming assistant text delta.
    AssistantDelta(String),
    /// Assistant stream finished for this segment.
    AssistantDone,
    /// A published terminal refusal. No automatic retry or new turn is implied.
    ModelRefusal {
        /// Optional non-secret provider diagnostic; absence means no reason was supplied.
        detail: Option<String>,
    },
    /// Tool lifecycle line (`state`: start|done|rejected|…).
    ///
    /// Prefer `call_id` for correlation when present (parallel same-name tools).
    /// `detail` is expand-body text (args/result preview); omitted on most
    /// progress ticks so subjects are not clobbered.
    ToolLine {
        /// Stable call id when known (UUID string); `None` falls back to name match.
        call_id: Option<String>,
        /// Outer local-composition marker call owning this nested call.
        ///
        /// `None` for a direct call and for the outer marker itself. The
        /// renderer nests Compose children on this field alone (§5.8): a lineage
        /// it was not given stays a flat row, never a guess.
        parent_call_id: Option<String>,
        /// Local-composition cell owning this row.
        ///
        /// Present on every child of that cell and on the cell's `wait*`
        /// yields. A Compose row carries it from the moment the host has observed
        /// it — the marker's own opening event precedes the interpreter minting
        /// the cell, so the first `running` line of a fresh cell has `None` and
        /// the terminal line carries the cell. `None` for a direct call.
        cell_id: Option<String>,
        /// Host-populated activity category. Never defaulted at the renderer.
        kind: ActivityKind,
        /// Tool name.
        name: String,
        /// Lifecycle / status label.
        state: String,
        /// Optional subject (path, command snippet, query, …).
        subject: Option<String>,
        /// Optional expandable detail body (multi-line plain text).
        detail: Option<String>,
        /// Host-observed wall time of this call, from its own `started_at_ms`
        /// to this event. Carried on every line the host can
        /// stamp; `None` when the host holds no start for the call.
        ///
        /// The renderer prefers it over any clock of its own. A cell that
        /// yields terminalises and reopens, and a renderer clock re-armed or
        /// frozen on those edges reported a fragment of the span the persisted
        /// row derives from the same stamps: live and resumed rows
        /// disagreed about the same call. The host's stamps are the one
        /// authority both paths read.
        duration_ms: Option<u64>,
        /// The host's start stamp for the call, Unix milliseconds: the
        /// `started_at_ms` its persisted tool row carries.
        started_at_ms: Option<u64>,
        /// The host's finish stamp, Unix milliseconds: the persisted row's
        /// `finished_at_ms`, on a terminal line. With the start it gives a
        /// tool group its elapsed span, live as after a restore. In-process
        /// only: the JSONL face does not carry the stamps.
        finished_at_ms: Option<u64>,
        /// Facts of a wait call, the same value the persisted tool row
        /// carries: requested timeout from the start, and what woke it, the
        /// actual wait and the settled agent once it ends.
        wait: Option<crate::views::transcript::WaitFacts>,
    },
    /// A turn's model request will be sent again under the retry policy.
    ModelRetry(ModelRetryNotice),
    /// Replace the complete chrome binding projection.
    Binding {
        /// Optional session id update (e.g. after clear → new session).
        session_id: Option<String>,
        /// Account label update.
        account: Option<String>,
        /// Model label update.
        model: Option<String>,
        /// Effort label update.
        effort: Option<String>,
        /// Optional exact model extra (`priority`); `None` means Standard/no extra.
        extra: Option<String>,
        /// Persisted composition selection.
        composition: CompositionSelection,
        /// Persisted `OpenRouter` wire selection, or `Auto` for native providers.
        protocol: ProtocolSelection,
        /// Effective compiled model-visible presentation.
        effective_presentation: String,
    },
    /// Host ack after clear — empty the local scrollback.
    ClearView,
    /// Current provider-grounded context projection.
    Usage(ContextUsage),
    /// Terminal token-usage observation projected from Plakous `TurnResult`.
    ///
    /// Token values are provider-reported subtotals when reporting coverage is
    /// partial. Cached input is a provider-reported counter, not a cache-hit,
    /// savings, billing, or context-capacity claim.
    TurnUsage {
        /// Provider-reported input tokens summed across reporting operations.
        input_tokens: u64,
        /// Provider-reported output tokens summed across reporting operations.
        output_tokens: u64,
        /// Provider-reported reasoning tokens summed across reporting operations.
        reasoning_tokens: u64,
        /// Provider-reported cached-input tokens summed across reporting operations.
        cached_input_tokens: u64,
        /// Provider-reported cache-write tokens summed across reporting operations.
        cache_write_tokens: u64,
        /// All model operations observed during the turn, or `None` when the
        /// host does not know — a live provider snapshot covers an unknown
        /// number of operations and the host never estimates one.
        model_operations: Option<u64>,
        /// Model operations which supplied a usage report, or `None` when
        /// coverage is unknown.
        reporting_model_operations: Option<u64>,
        /// Largest single-operation prepared input observed during the turn.
        ///
        /// Unlike every other field here this is **not** a provider-reported
        /// billing subtotal and never sums: it is the largest complete prompt
        /// any one request of this turn carried, which is what measures how
        /// far a turn grew against the model's window. `input_tokens` sums
        /// over operations, so a many-round loop inflates it without any one
        /// prompt growing, and it cannot answer that question. `None` when no
        /// operation reported one.
        peak_prepared_input_tokens: Option<u64>,
        /// `true` only for the projection of the terminal `TurnResult`.
        ///
        /// A `false` value is a provider-reported cumulative snapshot observed
        /// mid-turn: it is never a total, never a cost claim, and never turn
        /// authority. The terminal `TurnResult` remains the only authority for
        /// turn totals.
        terminal: bool,
    },
    /// Correlated lifecycle evidence for one admitted turn.
    ///
    /// `phase` is projected from the canonical `TurnResult` (or an explicit
    /// `Admitted`/`Unresolved` marker around it): it is never inferred from
    /// busy, usage, or presentation observations. `Unresolved` reports
    /// missing terminal evidence; it never invents an outcome.
    TurnLifecycle(TurnLifecycle),
    /// One admitted reasoning delta for one output location.
    ///
    /// Emitted only while the provider's `ReasoningVisibilityBound` for that
    /// location is `Visible` or `Summary`. `Omitted`, `Redacted` and
    /// `OpaqueReplay` reasoning never reaches this event, and no reasoning is
    /// ever synthesised from output text or timing.
    ReasoningDelta {
        /// Opaque per-turn location identity (`item_index:block_index`).
        location_key: String,
        /// Already-redacted reasoning text.
        text: String,
    },
    /// One admitted reasoning block reached its end.
    ReasoningSealed {
        /// Opaque per-turn location identity matching a prior delta.
        location_key: String,
    },
    /// A tool is blocked awaiting an operator decision.
    ///
    /// This is the *only* constructor of an approval row. No tool result, no
    /// transcript line and no assistant markdown can produce one, and the
    /// decision address (`EffectDecisionRef`) never leaves the host.
    ApprovalRequired {
        /// Host-minted, session-scoped, opaque decision handle.
        approval_id: String,
        /// Canonical call awaiting the decision, when known.
        call_id: Option<String>,
        /// Canonical tool name.
        tool_name: String,
        /// Optional bounded subject.
        subject: Option<String>,
        /// Operator-facing prompt; already a redaction-safe diagnostic.
        prompt: Option<String>,
        /// Display timestamp of the decision deadline, in Unix milliseconds.
        expires_at_ms: u64,
    },
    /// A pending approval reached a terminal resolution.
    ApprovalResolved {
        /// The host-minted handle carried by the matching request.
        approval_id: String,
        /// Closed resolution label: `allowed` | `denied` | `expired` |
        /// `cancelled` | `shutdown` | `invalidated`.
        resolution: String,
    },
    /// Inline acknowledgement of an already-admitted steer.
    ///
    /// This neither creates nor cancels queue state; the host queue remains its
    /// own projection.
    SteerAck {
        /// `queued` or `applied`.
        state: String,
        /// One-based queue position at admission, when the runtime reported one.
        position: Option<usize>,
    },
    /// Replace the inert subscription allowance view.
    SubscriptionUsage(SubscriptionUsageView),
    /// Replace the complete host-owned Usage projection: all four verticals,
    /// their plan/tier labels and meters, and the session ledger. Carries an
    /// age when the host served the last held projection during a turn.
    UsageLimits(UsageLimitsView),
    /// Replace the typed session facts the Usage Session section renders
    /// (spec 02 §5.5). Emitted at attach and whenever a field changes.
    SessionInfo(SessionInfoView),
    /// Replace the agent-template readiness projection. Emitted at
    /// session open and in answer to [`SessionIntent::RefreshTemplates`].
    TemplateReadiness(Vec<TemplateRow>),
    /// A shell binding was durably committed and the app may reattach.
    ReattachReady,
    /// The host reopened the session in place and it is accepting work again.
    /// Carries the one claim the abandonment note made, when the reopen closed
    /// an unfinished turn; both are `None` when there was nothing to close.
    Recovered {
        /// `running_tool`, `model_request`, `compaction_not_applied` or
        /// `nothing_started` or `tool_round_completed`.
        claim: Option<String>,
        /// `process_lost` or `interrupted`.
        cause: Option<String>,
        /// The operations the abandonment closed.
        operations: Vec<u64>,
    },
    /// The host began opening the Plakous session. The startup row ticks its
    /// own elapsed seconds from here; the host thread is blocked and cannot
    /// publish progress of its own (spec 02 §5.4).
    SessionOpening,
    /// A model round was dispatched. Starts the presentation's waiting clock;
    /// busy, tool, and compaction phase transitions do not start this clock.
    ModelRequestStarted,
    /// A model request is waiting for a slot in its account's shared request
    /// budget. `ModelRequestStarted` follows once a slot frees.
    ModelRequestQueued {
        /// The account whose budget is full.
        slots: String,
    },
    /// The host finished opening the session. Carries the host's own
    /// `session_open_ms` measurement (logged, not painted as TUI chrome).
    SessionOpened {
        /// Wall time the canonical session open took, as the host logged it.
        /// `None` when the canonical open never completed: the startup row is
        /// retired, but no duration is claimed for a session that never opened.
        session_open_ms: Option<u64>,
    },
    /// Replace the inert agent-tree navigator snapshot. Never patch locally.
    TopologyNavigatorSnapshot(TreeView),
    /// One agent's committed transcript, answering
    /// [`SessionIntent::OpenAgentTranscript`] and re-sent when its live turn
    /// ends or a new one starts. Replaces the agent view's rows. In-process only.
    AgentTranscript(AgentTranscript),
    /// The answer to one [`SessionIntent::RequestCallDetail`]. In-process only,
    /// an ordinary delta: saturation may drop it, and the surface asks again.
    /// At most one answer is sent per request.
    CallDetail(CallDetail),
    /// A child-only presentation operation. Never apply to the root transcript.
    /// The operation uses the root block constructors (text, reasoning, tool,
    /// usage and terminal), or a system note marking a retained-history gap.
    /// This observation is in-process only.
    ChildLive {
        /// Exact child agent.
        agent_id: String,
        /// Exact running turn the operation belongs to.
        turn_id: String,
        /// Root presentation vocabulary, isolated from root state mutation.
        op: Box<SessionObservation<Core>>,
    },
    /// Navigator cannot open (no hub, flag off, or detached shell).
    TopologyNavigatorUnavailable {
        /// Operator-facing reason. Contains no secrets.
        reason: String,
    },
    /// Idle pending work for the composer HUD.
    ///
    /// Emitted on attach and whenever the counts or pause flags change. The
    /// driver hides it while a turn runs. Presentation only: omitted from JSONL.
    PendingContinuation {
        /// Undelivered child results addressed to the root.
        results: u32,
        /// Undelivered child questions addressed to the root.
        questions: u32,
        /// Process exits not yet seen by a turn.
        process_exits: u32,
        /// Auto-continue is paused (Esc suppression) until the next user prompt.
        auto_continue_paused: bool,
        /// The attached tree is restored-paused.
        tree_paused: bool,
    },
    /// When the operator last sent the attached root a prompt or steer, as
    /// Unix milliseconds read from its committed canonical history at attach
    /// (spec 02 §5.4). `None` when no stored entry carries that time.
    /// Presentation only, in-process: omitted from JSONL and never persisted.
    LastUserPromptAt(Option<u64>),
    /// Product-owned traffic the protocol carries but never interprets.
    ///
    /// The mailbox classifies it through [`ObservationExtension`](crate::intent::ObservationExtension) and nothing
    /// else reads it; the JSONL projection drops it.
    Extension(P::Observation),
}

/// The idle pending-work snapshot carried by
/// [`SessionObservation::PendingContinuation`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PendingContinuation {
    /// Undelivered child results addressed to the root.
    pub results: u32,
    /// Undelivered child questions addressed to the root.
    pub questions: u32,
    /// Process exits not yet seen by a turn.
    pub process_exits: u32,
    /// Auto-continue is paused (Esc suppression) until the next user prompt.
    pub auto_continue_paused: bool,
    /// The attached tree is restored-paused.
    pub tree_paused: bool,
}

/// Immutable queue row supplied by the authoritative product host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueItem {
    /// This revision was definitely refused and requires editing or new submission.
    pub rejected: bool,
    /// Stable identity, independent of prompt text or position.
    pub id: String,
    /// Optimistic-concurrency revision for exact mutations.
    pub revision: u64,
    /// Product-visible prompt text.
    pub text: String,
    /// Display chips of the images attached to the item, in order:
    /// `[image: shot.png 1280x720 412 KB]`. An edit keeps them.
    pub images: Vec<String>,
}

/// Host-owned automatic queue delivery state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueDrainState {
    /// An uninterrupted attached session may deliver FIFO, one item at a time.
    LiveAutoDrain,
    /// Crash-restored work remains visible but cannot auto-dispatch.
    RestoredPaused,
}

/// Host-authoritative terminal context-capacity recovery presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextCapacityRecovery {
    /// The withheld prompt can be shortened and explicitly sent again.
    ResendShorter,
    /// Explicit compaction may be attempted, without replaying the failed turn.
    Compactable,
    /// Only protected material remains and compaction cannot make progress.
    Protected,
}

/// The observational phase marker a [`TurnLifecycle`] carries.
///
/// Terminal phases are projected from the canonical `TurnResult` only; busy
/// and usage observations are never completion evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    /// The host admitted the turn; no terminal evidence exists yet.
    Admitted,
    /// The canonical result finished without a refusal.
    Completed,
    /// The canonical result finished as a model refusal.
    Refused,
    /// The canonical result settled a cancellation.
    Cancelled,
    /// The canonical result reported failure.
    Failed,
    /// The canonical result requires recovery before new work.
    RecoveryRequired,
    /// The turn ended without canonical terminal evidence.
    Unresolved,
}

impl TurnPhase {
    /// The stable `snake_case` label, identical to the serde representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Completed => "completed",
            Self::Refused => "refused",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::RecoveryRequired => "recovery_required",
            Self::Unresolved => "unresolved",
        }
    }
}

/// Correlated lifecycle evidence for one admitted turn.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnLifecycle {
    /// Canonical turn identity.
    pub turn_id: String,
    /// Observational phase marker.
    pub phase: TurnPhase,
    /// Typed model-error kind at the terminal, when one was reported.
    pub model_error_kind: Option<String>,
    /// Provider HTTP status at the terminal, when one was reported.
    pub http_status: Option<u16>,
}

/// One announced replay of a turn's model request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRetryNotice {
    /// The 1-based physical request about to be sent.
    pub next_attempt: u32,
    /// The physical-request budget of the model call.
    pub max_attempts: u32,
    /// Wait before the replay is sent, in milliseconds.
    pub delay_ms: u64,
    /// Stable cause: `http_status`, `connect`, `transport_after_send` or
    /// `stream_interrupted`.
    pub cause_kind: String,
    /// Operator-facing cause, such as `provider busy (HTTP 503)`.
    pub cause: String,
    /// The failed request had streamed output, which was discarded.
    pub output_discarded: bool,
}
