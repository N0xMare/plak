use plakous_host_protocol::selection::CompositionSelection;

use super::lineage::{NodeLineId, NodePlacement};
use super::{
    AdoptOutcome, AttemptId, AttemptStatus, BTreeMap, BTreeSet, CandidateId, Deserialize,
    InterruptCause, LossContinuationState, MessagePriority, MessagePurpose, NodeStatus, PathBuf,
    RefreshReport, Serialize, SpawnAgentRequest, SpawnIdentity, SubmitNudgeBudget, TemplateName,
    Tier, TopologyError, TopologyErrorCode, WaitingOn, WorkspaceMode, WorkspaceStatus,
};

/// What one accepted message did to the child's turn: the wire
/// word `send_agent_message` reports back.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailDisposition {
    /// The child had no turn running, so this message started one.
    Started,
    /// A turn was running and would not take a steer, so the message waits.
    Queued,
    /// The running turn took the message mid-flight.
    Steered,
}

/// How far one persisted message has got. Abandoned uncommitted wait receipts
/// restore acknowledgement to Accepted; committed delivery is never re-queued.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailDurableState {
    /// Written to the mailbox; the deliverer has not seen it.
    Accepted,
    /// A wait or admitted host command reported it to the recipient.
    Acknowledged,
    /// The deliverer handed it to the child's session.
    Delivered,
    /// Settled: delivered and answered, or refused for good.
    Terminal,
}

/// One node of the tree: who it is, what it was asked for, and where its work
/// happens. Status and cost live on its [`AttemptRecord`], not here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeRecord {
    /// Durable id, assigned by the tree (`c1`, `c1-c2`); never a session id.
    pub agent_id: String,
    /// The node that spawned this one; only the root has none.
    pub parent_agent_id: Option<String>,
    /// Distance from the root, checked against the epoch's `max_depth`.
    pub depth: u8,
    /// The one line the spawner wrote to name the job, shown in the tree.
    pub description: String,
    /// The child's brief, as the spawner wrote it.
    pub prompt: String,
    /// JSON Schema this node's `submit_result` must satisfy.
    pub output_schema_json: String,
    /// Digest of `output_schema_json`, so a schema swapped mid-attempt is
    /// detectable without keeping the old text.
    pub schema_digest: String,
    /// The working-set template this node was spawned from; the root, bound
    /// by the session, has none.
    pub template: Option<TemplateName>,
    /// The exact model this node is bound to for its epoch.
    pub tuple_id: String,
    /// The account behind `tuple_id`, as the node's card names it.
    pub account: String,
    /// Reasoning effort, when the binding named one.
    pub effort: Option<String>,
    /// The node's own status; `disposition()` is what predicates read.
    pub status: NodeStatus,
    /// The workspace row this node works in; inherit shares its caller's.
    pub workspace_id: String,
    /// Effective spawn tier after the template clip.
    pub tier: Tier,
    /// Folded current attempt; deletion seeds the `retry_of` chain root.
    pub attempt_id: Option<AttemptId>,
    /// The child session bound to this node, once a binder has opened one.
    pub session_locator: Option<String>,
    /// The checkout this node runs in: its own copy, or its caller's.
    pub path: PathBuf,
    /// The branch a minted copy sits on, when the VCS named one.
    pub branch: Option<String>,
    /// Whether a turn is in flight right now; a steer and an interrupt both
    /// read this rather than the status.
    pub active_turn: bool,
    /// Last intent order applied at the session's tree cursor; NULL restarts the fold.
    pub fold_order: Option<u64>,
    /// The L0 agent line this node maps to for life; `None` until the line
    /// integration records it.
    pub line_id: Option<NodeLineId>,
    /// The tree epoch this node was spawned under; 0 is the header snapshot.
    pub epoch_no: u64,
    /// Where this node's tools run; the default is the local runner at `path`.
    pub placement: NodePlacement,
    /// The spawning parent's tool composition, frozen at spawn; `None` when
    /// it was not recorded.
    pub parent_composition: Option<CompositionSelection>,
    /// What a reopen had to abandon to get this node's session back, if
    /// anything, until the caller's face has said it once.
    pub abandoned_turn: Option<AbandonedChildTurn>,
}

/// An unfinished child turn, closed by the reopen that found it.
///
/// A child's session is opened away from any face, inside the caller's tool
/// work, so the node is where the fact waits until the caller's face reads it.
/// It is cleared once said, because it describes one reopen rather than a
/// standing condition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbandonedChildTurn {
    /// The operations the abandonment closed.
    pub operations: Vec<u64>,
    /// The one claim the child's abandonment note made.
    ///
    /// A child runs in the caller's workspace, so a running tool is the more
    /// serious case and the caller is the one who has to hear about it.
    pub claim: plakous::agent::AbandonedTurnClaim,
}

/// One attempt of one node: the unit of status, token, and result.
/// A `followup` opens a fresh row on the same node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptRecord {
    /// Immutable schema admitted when this attempt was created.
    pub output_schema_json: String,
    /// Digest of this attempt's immutable admitted schema.
    pub schema_digest: String,
    /// This attempt's durable id, `<agent>-a<n>`.
    pub attempt_id: AttemptId,
    /// The node this attempt belongs to.
    pub agent_id: String,
    /// The attempt this one re-ran, when a `followup` opened it.
    pub retry_of: Option<AttemptId>,
    /// The caller's own idempotency key, recorded so a replayed spawn is
    /// recognisable.
    pub request_key: Option<String>,
    /// Original admitted brief; present exactly when a request key is present.
    pub spawn_identity: Option<SpawnIdentity>,
    /// Unix milliseconds the attempt opened.
    pub started_at: u64,
    /// Unix milliseconds it reached a terminal status; `None` while running.
    pub ended_at: Option<u64>,
    /// Set when a provider quota refusal ended the attempt.
    pub blocked: Option<BlockedReason>,
    /// Why the attempt failed, when it failed for anything but quota. Without
    /// it a `recovery_required` attempt carried no reason anywhere a caller
    /// could read.
    pub failure: Option<AttemptFailure>,
    /// What this attempt spent, model-visible.
    pub usage: UsageRow,
    /// The attempt's own status; the node's is derived from it.
    pub status: AttemptStatus,
    /// The token a `submit_result` must carry; a stale one is refused, so a
    /// turn that lost its race cannot submit over its successor.
    pub turn_token: u64,
    /// The runtime turn currently bound to this attempt, when one is.
    pub turn_id: Option<String>,
    /// Own-position abandonment reads applied by this attempt's fold.
    pub applied_terminal_read_seqs: BTreeSet<u64>,
    /// The exact submitted text, derived from its intent rather than persisted as a view.
    pub output_json: Option<String>,
    /// Admitted submit-harness Starts on this attempt. Persisted so a
    /// crash-cut rejoin cannot reset the 1+1 budget.
    pub submit_harness_nudge: SubmitNudgeBudget,
    /// The candidate this attempt sealed, once it has one.
    pub candidate_id: Option<CandidateId>,
}

/// Why an attempt failed. The kind is a stable label; the message is the
/// host's own diagnostic, never provider payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AttemptFailure {
    /// Stable failure label; the wait row and the projections match on it.
    pub kind: String,
    /// The host's own sentence about the failure.
    pub message: String,
}

/// Why an attempt is `blocked`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BlockedReason {
    /// Which quota the provider named.
    pub quota: String,
    /// The account that hit it, so the operator knows which login to fix.
    pub account: String,
    /// Unix seconds the quota resets, when the provider said.
    pub resets_at: Option<u64>,
}

/// Per-attempt usage, model-visible. Never rolled up in a
/// prompt; cost and quota balances stay out of it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UsageRow {
    /// Wall-clock milliseconds the attempt has been open.
    pub elapsed_ms: u64,
    /// Model turns the attempt has driven.
    pub turns: u32,
    /// Tool calls across those turns.
    pub tool_calls: u32,
    /// Tokens the attempt sent.
    pub input_tokens: u64,
    /// Tokens it received.
    pub output_tokens: u64,
}

/// A sealed snapshot of one attempt's copy, pinned outside the
/// worktree so discarding the copy cannot orphan it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    /// Ordinary sealed child work.
    #[default]
    Work,
    /// A textual merge pinned without touching the adopter's checkout.
    Conflict,
}

/// Retained work or a pinned conflicted merge, with immutable resolution lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRecord {
    /// Exact target snapshot after the last successful reversal.
    pub reverted_at: Option<String>,
    /// Node which performed that reversal.
    pub reverted_by: Option<String>,
    /// Ordinary work or a conflicted merge.
    pub kind: CandidateKind,
    /// Original work from which this conflict was produced.
    pub source_candidate_id: Option<CandidateId>,
    /// Only this node may land the conflict or a resolver derived from it.
    pub conflict_target: Option<String>,
    /// Exact paths and kinds observed when the conflict was pinned.
    pub conflict_paths: Vec<plakous_workspace::occupancy_vcs::ConflictPath>,
    /// Original work which this resolver's candidate accepts.
    pub resolves: Option<CandidateId>,
    /// Candidate which accepted this work or superseded this resolution.
    /// Without `adopted_by`, this row is superseded rather than adopted.
    pub adopted_via: Option<CandidateId>,
    /// The candidate's durable id; an adopt names exactly this.
    pub candidate_id: CandidateId,
    /// The copy this was sealed from; the copy may be discarded afterwards.
    pub workspace_id: String,
    /// The node whose work this is.
    pub producing_agent_id: String,
    /// The attempt that produced it, so a later attempt's seal is a separate
    /// candidate rather than an overwrite.
    pub producing_attempt_id: AttemptId,
    /// The commit the copy was minted from; an adopt's three-way starts here.
    pub base_commit: String,
    /// The tree hash at that base, so divergence is asked by content rather
    /// than by commit identity.
    pub base_content: String,
    /// The jj change the copy was minted on; git leaves `None`. Adoption
    /// starts its range here, because commit ids move under any parent-side
    /// snapshot while change ids survive rewrites.
    pub base_change_id: Option<String>,
    /// The sealed commit this candidate names.
    pub commit: String,
    /// The tree hash at `commit`; a candidate whose content equals its base
    /// is never minted, so this always differs from `base_content`.
    pub content: String,
    /// jj change id; git leaves `None`.
    pub change_id: Option<String>,
    /// Unix milliseconds the seal completed.
    pub sealed_at: u64,
    /// The node that landed it, once one has; a candidate lands at most once.
    /// Automatic cleanup retains unadopted work; explicit discard gives it up.
    pub adopted_by: Option<String>,
    /// Target snapshot commit after adoption; never a commit on the root branch.
    pub merged_at: Option<String>,
    /// Exact target snapshot before the landing, pinned for provenance.
    pub target_before: Option<String>,
    /// Review attempt ids the adopter recorded with the landing.
    /// Validated against the caller's subtree at `begin_adopt`; kept here so
    /// "what evidence stood behind this landing" survives the process.
    pub adopted_evidence: Vec<String>,
}

/// One checkout the tree is responsible for: the launch
/// checkout itself, or a copy minted for one node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRecord {
    /// Original source when this copy was minted as a conflict resolver.
    pub resolves: Option<CandidateId>,
    /// Mint-time merge base, independent of the collectable conflict record.
    pub conflict_base: Option<String>,
    /// Original adopter whose checkout may accept the resolution.
    pub conflict_target: Option<String>,
    /// Immutable marker-inspection paths captured at mint.
    pub conflict_paths: Vec<plakous_workspace::occupancy_vcs::ConflictPath>,
    /// Candidate this copy was minted over, absent for a live-state fork.
    pub input_candidate_id: Option<CandidateId>,
    /// Durable id; `ws-<agent>` for a minted copy.
    pub workspace_id: String,
    /// The node this copy was minted for. Other nodes may share it by
    /// inheriting, but only the owner may seal or discard it.
    pub owner_agent_id: String,
    /// Where the checkout is on disk.
    pub path: PathBuf,
    /// Whether this row is the caller's own checkout or a copy of it.
    pub mode: WorkspaceMode,
    /// The row's own in-flight status; a node status never carries it, so a
    /// row is adopting or adopted without moving the node.
    pub status: WorkspaceStatus,
    /// The commit the copy was minted from. A minted copy
    /// always carries one; `inherit` rows — the launch checkout, and any node
    /// sharing its caller's checkout — never do, because nothing was minted.
    pub base_commit: Option<String>,
    /// The tree hash at that base, so divergence is asked by content.
    pub base_content: Option<String>,
    /// The jj change the copy was minted on (`None` on git and on `inherit`).
    /// Presentation and lineage only; the exact frozen `base_commit` is the
    /// merge and divergence authority. Parent snapshots cannot rewrite it.
    pub base_change_id: Option<String>,
    /// Bumped every time the copy is re-minted at the same id, so a live
    /// handle from an earlier generation is recognisable as stale.
    pub generation: u64,
    /// The caller whose adopt is landing this copy's candidate: set when the
    /// row enters `Adopting` and cleared when it leaves, so a reopen faults
    /// the node whose checkout the landing writes rather than the copy's owner.
    pub adopter_agent_id: Option<String>,
}

/// One message in the tree's own mailbox. Mail is durable
/// because a message that started a child's turn must survive a crash cut.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MailRecord {
    /// Durable id, unique within the tree.
    pub message_id: String,
    /// The node that sent it; the host writes `result` mail as the child.
    pub from: String,
    /// The recipient node.
    pub to: String,
    /// The message text, already bounded to the mailbox's byte ceiling.
    pub body: String,
    /// Why it was sent; `result` mail is host-authored and the rest is not.
    pub purpose: MessagePurpose,
    /// Whether it may steer a running turn or must wait for the next one.
    pub priority: MessagePriority,
    /// The message this answers, when it answers one.
    pub in_reply_to: Option<String>,
    /// What accepting it did to the recipient's turn.
    pub disposition: MailDisposition,
    /// How far delivery has got; only ever advances.
    pub durable: MailDurableState,
}

/// One entry of the operation ledger: what a caller asked, and
/// the exact answer it was given.
///
/// A repeated `operation_id` replays the recorded response instead of running
/// the tool again, so a retried spawn cannot mint a second child; a repeat
/// whose request digest differs is a conflict rather than a replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationRecord {
    /// The caller's idempotency key for this call.
    pub operation_id: String,
    /// The node that made the call.
    pub caller_agent_id: String,
    /// Which tool it was, so a key reused across tools is a conflict.
    pub tool_name: String,
    /// Canonical request JSON, or a digest summary for submit-result receipts.
    pub canonical_request: String,
    /// Digest of the full canonical request; submit receipts retain only its summary.
    pub request_digest: String,
    /// Immutable position of a host fold intent; ordinary tool receipts have none.
    pub fold: Option<FoldAnchor>,
    /// The refusal code, when the call ended in one.
    pub terminal_code: Option<TopologyErrorCode>,
    /// The exact response body a replay returns.
    pub terminal_response: Option<String>,
}

/// Position of an agent's intent in its session's feed fold.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct FoldAnchor {
    /// The session's tree cursor when the intent was recorded, or zero unbound.
    pub seq: u64,
    /// The tree revision at insert, distinct for each inserted intent.
    pub order: u64,
}

/// The spawn receipt: a running attempt, not a result. Bind
/// identity is read back from the node, never carried here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpawnAgentOutput {
    /// The child's durable id, which every later call names it by.
    pub agent_id: String,
    /// The attempt the spawn opened.
    pub attempt_id: AttemptId,
    /// The child's status at the moment the spawn returned.
    pub status: NodeStatus,
    /// Effective tier after the `max_tier` clip.
    pub tier: Tier,
    /// Which checkout the child got.
    pub workspace: WorkspaceReceipt,
}

/// What a spawn says about the child's checkout: enough to name it in a later
/// `discard_workspace`, and nothing about the VCS.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceReceipt {
    /// The workspace row's durable id.
    pub id: String,
    /// Whether the child got its own copy or shares the caller's checkout.
    pub mode: WorkspaceMode,
    /// Base snapshot commit the copy was minted from, once minting snapshots.
    pub base: Option<String>,
}

/// What `send_agent_message` returns: the id to reply against, and what the
/// message did to the recipient's turn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SendMessageOutput {
    /// Explicit recovery consent never claims that uncertain workspace effects did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_warning: Option<String>,
    /// Automatic catch-up before a follow-up; omitted for unchanged/ineligible copies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<RefreshReport>,
    /// The accepted message's durable id; a reply cites it as `in_reply_to`.
    pub message_id: String,
    /// Whether it started, steered, or queued behind the recipient's turn.
    pub disposition: MailDisposition,
}

/// One inbox row of `list_agents`. A caller with no session
/// delivery — the root, or a hub between turns — reads mail only here, so the
/// row carries a bounded `body`; a host-authored `result` additionally carries
/// its envelope fields so the parent can adopt without parsing the body.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InboxMail {
    /// The message's durable id; a reply cites it as `in_reply_to`.
    pub message_id: String,
    /// The node that sent it.
    pub from: String,
    /// Why it was sent.
    pub purpose: MessagePurpose,
    /// Whether it could steer a running turn.
    pub priority: MessagePriority,
    /// The message this one answers, when it answers one.
    pub in_reply_to: Option<String>,
    /// Full body size before preview truncation.
    pub body_bytes: usize,
    /// Body preview: 2048 bytes unread, 160 bytes acknowledged.
    pub body: String,
    /// `result` envelope: the attempt that finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    /// `result` envelope: the attempt's terminal status label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// `result` envelope: the candidate the attempt sealed, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
}

/// What `list_agents` returns: the caller's subtree as projected rows, plus
/// its own inbox.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ListAgentsOutput {
    /// The tree revision these rows were read at; it advances on every
    /// mutation, so a caller can tell a stale projection from a fresh one.
    pub revision: u64,
    /// One projected row per node the caller may see, in preorder.
    pub agents: Vec<serde_json::Value>,
    /// Messages addressed to the caller. Parents need `message_id` here to
    /// send `purpose=reply` (`in_reply_to`). `list_agents` does not otherwise
    /// project the mailbox.
    pub inbox: Vec<InboxMail>,
}

/// What settling retained await receipts against canonical commit evidence
/// did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WaitReceiptSettlement {
    /// Committed receipts retired; their acknowledgements stand.
    pub retired: usize,
    /// Uncommitted receipts rolled back.
    pub rolled_back: usize,
    /// Envelopes those rollbacks restored to unread.
    pub restored_mail: usize,
}

/// The condition that ended a wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitWakeReason {
    /// A watched attempt settled.
    Settled,
    /// A watched child sent unread mail.
    Mail,
    /// A steer superseded this observation; its text remains driver-owned.
    Steer,
    /// Host information is ready; its delivery remains host-owned.
    Notice,
    /// An owned process became terminal after wait admission.
    Process,
    /// No new mail or settlement arrived before the ceiling.
    Timeout,
}

/// One settled (or still-running) agent as `wait_agent` reports it: the only
/// channel through which a spawner reads a delegated result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WaitAgentRow {
    /// The agent this row is about.
    pub agent_id: String,
    /// Its node status.
    pub status: NodeStatus,
    /// The attempt the row reports, when the node has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<AttemptId>,
    /// That attempt's status; a wait settles on this reaching a terminal one.
    pub attempt: AttemptStatus,
    /// The sealed candidate, when the attempt has one; may exist without a
    /// submission.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<CandidateId>,
    /// The child's `submit_result` payload once it has one. This is the only
    /// channel a spawner has to the delegated result; without it a root that
    /// waited had to redo the work itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_json: Option<String>,
    /// Why the attempt is blocked, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedReason>,
    /// Why the attempt failed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<AttemptFailure>,
    /// What the attempt spent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageRow>,
    /// What a parked child waits on; absent unless it is parked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<WaitingOn>,
    /// Why an interrupted child stopped (`owner_lost`, `handoff`,
    /// `operator`, `parent`, `cascade` or `stopped`); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<InterruptCause>,
    /// Where its continuation after a lost or handed-off host stands
    /// (`held`, `pending` or `running`); absent when it has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<LossContinuationState>,
}

/// What a child's own `submit_result` returns to it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubmitResultOutput {
    /// The submitting node.
    pub agent_id: String,
    /// Its attempt's status after the submission.
    pub attempt: AttemptStatus,
    /// The candidate this attempt sealed, when it has one. A
    /// submission is not a seal: this stays absent until the copy is sealed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<CandidateId>,
}

/// What `discard_workspace` returns: the row and the status the unmint left
/// it in.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscardWorkspaceOutput {
    /// The discarded workspace row.
    pub workspace_id: String,
    /// `Discarding` until VCS acknowledgement; only completion reports `Discarded`.
    pub status: WorkspaceStatus,
    /// Unadopted candidates explicitly relinquished with this copy.
    pub discarded_candidates: Vec<CandidateId>,
}

/// The template's `on_mint` hook for one freshly minted copy.
/// Carries no tree reference: the command runs with the tree mutex released,
/// through [`plakous_workspace::occupancy_vcs::VcsOps::run_mint_hook`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MintHook {
    /// The node whose copy was minted.
    pub agent_id: String,
    /// The template's `on_mint` command, as written.
    pub command: String,
    /// The template environment plus authoritative tree-supplied copy context.
    pub env: BTreeMap<String, String>,
    /// The new copy; the hook's working directory.
    pub cwd: PathBuf,
    /// The spawn that minted the copy, so a failed hook can rewrite its
    /// operation to the refusal.
    pub caller_id: String,
    /// The spawn request, kept so the rewritten operation replays the same
    /// refusal rather than a fresh one.
    pub request: SpawnAgentRequest,
    /// That spawn's operation id, when the caller gave one.
    pub operation_id: Option<String>,
}

/// One owned copy removal, acknowledged only after the VCS reports success.
#[derive(Clone, Debug)]
pub struct UnmintTicket {
    /// Copy whose contents must remain recoverable when removal fails.
    pub path: PathBuf,
    /// Durable workspace row, absent only for uncommitted mint rollback.
    pub workspace_id: Option<String>,
    /// Prepared discard receipt that completion terminalizes.
    pub operation_id: Option<String>,
    /// Set for an automatic prune of a closed copy: the status the row goes
    /// back to if removal fails, which leaves the copy in place instead of
    /// putting its closed occupants into recovery.
    pub prune_of: Option<WorkspaceStatus>,
    /// Unadopted candidate pins explicitly relinquished by this discard.
    /// Automatic copy removal never owns retained candidate or provenance pins.
    pub candidate_ids: Vec<String>,
    /// Full tree digest owning these candidate pins.
    pub namespace: String,
}

/// What [`super::ProductAgentTree::complete`] recorded: the adopt receipts in the
/// order their tickets were queued, and the hooks that failed.
#[derive(Debug, Default)]
pub struct Settled {
    /// One receipt per queued adopt, in ticket order, so a caller that
    /// queued several knows which landed.
    pub adopts: Vec<Result<AdoptOutcome, TopologyError>>,
    /// Hooks that failed; their spawns were rewritten to the refusal.
    pub hook_failures: Vec<TopologyError>,
    /// Seal failures, recorded on their attempts before they are reported.
    pub seal_failures: Vec<TopologyError>,
    /// Copy removal failures; no affected operation is acknowledged.
    pub cleanup_failures: Vec<TopologyError>,
}

/// Why a rebind or roster apply is refused for this epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpochRebindRefusal {
    /// A spawn is mid-mint, so the tree is not quiescent.
    PendingSpawns,
    /// Descendants are still live; a successor epoch needs a quiet tree.
    LiveDescendants,
}
