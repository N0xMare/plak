//! Durable product-owned `ProductAgentTree`.
//!
//! Each node is a clean identity over a distinct public-facade Plakous session.
//! This module owns topology, grants, workspaces, candidates, mail, and the
//! per-attempt contract. It does not run a second inference loop.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// The tree's only clock is `now_unix_millis`: attempt start and end stamps and
// candidate seal stamps. Millisecond resolution so
// `usage.elapsed_ms` is a real measurement rather than whole seconds.
use plakous::model::{PortableToolSchemaV2, now_unix_millis};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::cell_budget::CellAgentBudget;
#[cfg(any(test, feature = "test-support"))]
use crate::epoch::BindHints;
use crate::epoch::{OrchestrationSnapshot, RosterBinding};
use crate::protocol::{
    AdoptCandidateRequest, AdoptOutcome, AttemptId, AttemptScopedRequest, AttemptStatus,
    CandidateId, DEFAULT_GRANT_DEPTH, ListAgentsRequest, MAX_DESCRIPTION_PROMPT_SCHEMA_BYTES,
    MAX_DIRECT_CHILDREN, MAX_MAILBOX_RECORDS, MAX_PENDING_SPAWNS, MAX_QUEUED_MAIL_BYTES,
    MAX_RUNNING_CHILDREN, MAX_TREE_ATTEMPTS, MAX_TREE_DEPTH, MAX_TREE_NODES, MessagePriority,
    MessagePurpose, NodeStatus, SendAgentMessageRequest, SpawnAgentRequest, SpawnWorkspace,
    SubmitResultRequest, TemplateName, Tier, TopologyErrorCode, TopologyGrant, WaitAgentRequest,
    WorkspaceIdRequest, WorkspaceMode, WorkspaceStatus, WriterMode,
};
use plakous_workspace::occupancy_vcs::{
    AdoptFailure, AdoptLanding, MintedCopy, SystemVcs, VcsError, VcsErrorKind, VcsOps, VcsSnapshot,
    digest_text,
};
use text::errors::{
    topology_mail_request_invalid_message, topology_output_schema_mismatch_message,
    topology_submit_request_invalid_message,
};

pub use crate::protocol::{TopologyError, TreeAdmissionState};

const ID_PATTERN_MAX: usize = 64;

/// The one place a VCS failure becomes a topology wire code. Every kind
/// today lands on `mint_unavailable`, the code the tree has always reported
/// for a VCS it could not drive; the kinds exist so `plakous-workspace` can
/// say what happened without naming the wire. A free function rather than a
/// `From` impl because both types are now foreign to this crate — which is
/// the point: neither the workspace crate nor the protocol crate knows the
/// other, and this is the only seam that maps between them.
pub fn topology_error_from_vcs(error: VcsError) -> TopologyError {
    let code = match error.kind {
        VcsErrorKind::MintUnavailable | VcsErrorKind::MintFailed | VcsErrorKind::SealFailed => {
            TopologyErrorCode::MintUnavailable
        }
        VcsErrorKind::RecoveryRequired => TopologyErrorCode::WorkspaceRecoveryRequired,
        VcsErrorKind::HookFailed => TopologyErrorCode::MintHookFailed,
    };
    TopologyError::new(code, error.detail)
}

/// The tree's handle on the VCS port. Process-local, never persisted, and
/// never driven by a production caller holding the tree mutex: mints are
/// [`MintTicket`]s handed back by [`ProductAgentTree::spawn_reserve`], the
/// occupancy probes `list_agents` renders are an [`OccupancyProbe`], and
/// seals, adopts, hooks, and unmints are [`Effects`]. What is left is the
/// in-process convenience the tree's own tests drive
/// ([`ProductAgentTree::spawn_agent`], [`ProductAgentTree::list_agents`]),
/// which runs those tickets inline through this port.
#[derive(Clone)]
struct TreeVcs(Arc<dyn VcsOps>);

impl std::fmt::Debug for TreeVcs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TreeVcs")
    }
}

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
    /// What a reopen had to abandon to get this node's session back, if
    /// anything, until the caller's face has said it once.
    pub abandoned_turn: Option<AbandonedChildTurn>,
}

impl NodeRecord {
    /// The restored tree's pause is presentation, not a child fold input.
    #[must_use]
    pub const fn presentation_status(&self, state: TreeAdmissionState) -> NodeStatus {
        match (state, self.status) {
            (TreeAdmissionState::RestoredPaused, NodeStatus::Running) => NodeStatus::Paused,
            _ => self.status,
        }
    }
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

impl AbandonedChildTurn {
    /// The one line a caller's face shows for this node.
    ///
    /// It names the node, because a caller may be running several, and then
    /// says only what is true of this cut: a tool may have changed the
    /// workspace the caller is about to carry on in; a model request or a
    /// compaction may have been billed for output nobody kept.
    #[must_use]
    pub fn note(&self, agent_id: &str) -> String {
        use plakous::agent::AbandonedTurnClaim;
        match self.claim {
            AbandonedTurnClaim::ToolRoundCompleted => format!(
                "plak: {agent_id} recovered an interrupted turn; its tool round completed, \
                 but the turn was interrupted before the next model request; completed tool results were kept"
            ),
            AbandonedTurnClaim::RunningTool => format!(
                "plak: {agent_id} recovered an interrupted turn; a tool may have run and \
                 changed its workspace; its unobserved result was not kept in the conversation, \
                 and the tool is not retried. Workspace changes remain in the child's workspace \
                 or candidate for review; check them before carrying on"
            ),
            AbandonedTurnClaim::ModelRequest => format!(
                "plak: {agent_id} recovered an interrupted turn; its model request may have \
                 been processed and billed; no response was kept in the conversation"
            ),
            AbandonedTurnClaim::CompactionNotApplied => format!(
                "plak: {agent_id} recovered an interrupted turn; compaction was not applied \
                 (it may have been billed) and its conversation is unchanged"
            ),
            _ => format!(
                "plak: {agent_id} recovered an interrupted turn; the tool its last round asked \
                 for never started, so nothing after that round ran"
            ),
        }
    }
}

/// Captured pre-opening view, restored only by a guarded withdrawal.
///
/// A refused Start used to leave the fresh attempt in place: the node fell to
/// `recovery_required`, its partial-work seal minted a candidate for an
/// attempt that never ran a turn, and that seal moved the workspace head out
/// from under the parent's adopt of the *previous* attempt (`target_changed`).
/// Undoing the attempt leaves the child exactly where the
/// followup found it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct FollowupRollback {
    /// The message whose send opened the attempt: only that send may undo it.
    opener: String,
    opened: AttemptId,
    previous: Option<(AttemptStatus, Option<u64>)>,
    node_attempt_id: Option<AttemptId>,
    node_status: NodeStatus,
    node_active_turn: bool,
    workspace_id: String,
    workspace_status: Option<WorkspaceStatus>,
    workspace_generation: Option<u64>,
    fault_reason: Option<String>,
    node_fold_order: Option<u64>,
    prior_operations: Vec<String>,
    prior_commands: Vec<String>,
}

/// Immutable admitted request identity for keyed spawn replay. Repository heads,
/// followups, and candidate retirement cannot change what the key admitted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnIdentity {
    caller_id: String,
    template: TemplateName,
    description: String,
    prompt: String,
    schema_digest: String,
    #[serde(default, skip_serializing_if = "SpawnWorkspace::is_inherit")]
    workspace: SpawnWorkspace,
    inputs: Vec<String>,
}

impl SpawnIdentity {
    fn admitted(caller_id: &str, request: &SpawnAgentRequest, schema_digest: String) -> Self {
        Self {
            caller_id: caller_id.to_owned(),
            template: request.template.clone(),
            description: request.description.clone(),
            prompt: request.prompt.clone(),
            schema_digest,
            workspace: request.workspace,
            inputs: request.input_ids().to_vec(),
        }
    }
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
    /// The result the child submitted, still as the exact text it sent.
    pub output_json: Option<String>,
    /// Admitted submit-harness Starts on this attempt. Persisted so a
    /// crash-cut rejoin cannot reset the 1+1 budget.
    pub submit_harness_nudge: SubmitNudgeBudget,
    /// The candidate this attempt sealed, once it has one.
    pub candidate_id: Option<CandidateId>,
}

impl AttemptRecord {
    pub(super) fn candidate_handoff(&self) -> Option<String> {
        let failure = self.failure.as_ref()?;
        if !matches!(failure.kind.as_str(), "context_exhausted" | "turn_failed") {
            return None;
        }
        let candidate = self.candidate_id.as_ref()?;
        Some(format!(
            "partial candidate {candidate}; spawn(inputs=[\"{candidate}\"]) with a handoff of completed work and the remaining task"
        ))
    }

    pub(super) fn presented_failure(&self) -> Option<AttemptFailure> {
        let mut failure = self.failure.clone()?;
        if let Some(handoff) = self.candidate_handoff() {
            failure.message.push_str("; ");
            failure.message.push_str(&handoff);
        }
        Some(failure)
    }

    pub(super) fn recovery_was_abandoned(&self) -> bool {
        self.status == AttemptStatus::Failed
            && self
                .failure
                .as_ref()
                .is_some_and(|failure| failure.kind == recovery::RECOVERY_ABANDONED)
    }

    /// Whether this attempt's spawn never produced a session: the create was
    /// refused before it wrote anything.
    pub(super) fn never_created(&self) -> bool {
        self.status == AttemptStatus::Failed
            && self
                .failure
                .as_ref()
                .is_some_and(|failure| failure.kind == recovery::SPAWN_REFUSED)
    }

    /// Whether a reopen abandoned this attempt's turn.
    #[must_use]
    pub fn turn_was_abandoned(&self) -> bool {
        self.failure
            .as_ref()
            .is_some_and(|failure| failure.kind == TURN_ABANDONED)
    }

    /// A canonical unresolved turn or refused retirement is not cleared by
    /// closing or sealing the copy, even if it already submitted output; only
    /// The tree projector's recovery joins settle it.
    pub(super) fn has_unresolved_session_failure(&self) -> bool {
        self.failure.as_ref().is_some_and(|failure| {
            matches!(
                failure.kind.as_str(),
                "session_retirement" | "turn_recovery_required"
            )
        })
    }

    /// Whether only a recovery reopen can say what this attempt's session
    /// did: an unresolved session failure, or a recovery-required attempt
    /// whose failure is not a VCS step's (a failed seal or a failed copy
    /// removal), which no session reopen can answer.
    pub(super) fn session_outcome_unresolved(&self) -> bool {
        self.has_unresolved_session_failure()
            || (self.status == AttemptStatus::RecoveryRequired
                && self.failure.as_ref().is_none_or(|failure| {
                    failure.kind != ProductAgentTree::SEAL_FAILED_KIND
                        && failure.kind != WORKSPACE_CLEANUP_KIND
                }))
    }

    /// Whether a recovery or an abandoning reopen already settled this
    /// attempt. Such a settlement is final for the attempt: a later failure
    /// report for it (a waiter that finished after a recovery) changes
    /// nothing.
    pub(super) fn settled_by_abandonment(&self) -> bool {
        self.recovery_was_abandoned()
            || (self.status == AttemptStatus::Failed
                && self.failure.as_ref().is_some_and(|failure| {
                    matches!(
                        failure.kind.as_str(),
                        TURN_ABANDONED | RECOVERED_WITHOUT_TURN
                    )
                }))
    }
}

/// Failure kind of an attempt whose turn a reopen abandoned after its owner
/// was lost or fenced. The node is failed and idle, so a `followup` can re-task
/// it, and the attempt does not count against the tree's attempt budget.
pub const TURN_ABANDONED: &str = "turn_abandoned";

/// The 1+1 submit-harness budget on one attempt.
///
/// A child that finished its turn without submitting is asked once, and once
/// more; the third ask never happens. The count is persisted, so a crash-cut
/// rejoin resumes the budget rather than restarting it, and it is a type
/// rather than a `u8` because the ceiling is the whole point: a value above
/// it is refused at construction, never clamped into looking admitted.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct SubmitNudgeBudget(u8);

impl SubmitNudgeBudget {
    /// Harness Starts one attempt may admit in total.
    pub const CEILING: u8 = 2;

    /// An attempt that has been asked nothing yet.
    pub const NONE: Self = Self(0);

    /// A budget with `rounds` already spent, or `None` above the ceiling.
    ///
    /// The refusal is the point: a persisted row or a caller that counted
    /// past the ceiling is a fault to report, not a number to clamp.
    #[must_use]
    pub const fn spent(rounds: u8) -> Option<Self> {
        if rounds > Self::CEILING {
            return None;
        }
        Some(Self(rounds))
    }

    /// The budget after one more admitted round, or `None` when it is spent.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        Self::spent(self.0 + 1)
    }

    /// Harness Starts admitted so far.
    #[must_use]
    pub const fn rounds(self) -> u8 {
        self.0
    }

    /// Whether the attempt has had both asks, so the harness stops here.
    #[must_use]
    pub const fn is_exhausted(self) -> bool {
        self.0 >= Self::CEILING
    }
}

/// Failure kind of an attempt whose copy removal failed.
pub(super) const WORKSPACE_CLEANUP_KIND: &str = "workspace_cleanup";

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

/// The failure `kind` on an attempt whose turn was cancelled from outside the
/// host.
pub const CANCELLED_KIND: &str = "cancelled";

/// A sealed snapshot of one attempt's copy, pinned outside the
/// worktree so discarding the copy cannot orphan it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRecord {
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
    /// Review attempt ids the adopter recorded with the landing.
    /// Validated against the caller's subtree at `begin_adopt`; kept here so
    /// "what evidence stood behind this landing" survives the process.
    pub adopted_evidence: Vec<String>,
}

/// One checkout the tree is responsible for: the launch
/// checkout itself, or a copy minted for one node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRecord {
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
    /// Divergence and adoption are both asked against this change, never
    /// against `base_commit`: jj rewrites commit ids under a copy whenever
    /// anything snapshots elsewhere in the repo.
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
    /// The request in canonical JSON, kept for the conflict message.
    pub canonical_request: String,
    /// Digest of `canonical_request`; the replay check compares this.
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
    /// The first `INBOX_BODY_BYTES` of the body.
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

/// The operation-ledger kind of every await receipt, whichever presentation
/// returned it; retirement and abandonment recovery select on it.
pub(crate) const AWAIT_RECEIPT_KIND: &str = "wait_agent";

/// The operation-ledger kind recording that a host returned an admitted
/// command's acknowledged mail to unread.
pub(crate) const RETURNED_ADMISSION_KIND: &str = "returned_admission";

/// The durable await receipt: the exact output returned, the mail it
/// acknowledged and the rows it newly delivered.
#[derive(Serialize, Deserialize)]
pub(crate) struct WaitReceipt {
    pub(crate) output: serde_json::Value,
    pub(crate) acknowledged: Vec<MailRecord>,
    pub(crate) newly_delivered: Vec<String>,
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

/// Seam between turn completion and the VCS seal. Held by the
/// caller between `begin_seal` and `finish_seal`; the snapshot itself runs
/// outside the tree mutex. Everything the snapshot needs is copied in here so
/// the ticket can travel without the tree.
#[derive(Clone, Debug)]
pub struct SealTicket {
    /// Identity of this individual in-process seal, including same-attempt reseals.
    ownership: Arc<()>,
    /// The node whose copy is being sealed.
    pub agent_id: String,
    /// The attempt the candidate will be recorded against.
    pub attempt_id: Option<AttemptId>,
    /// The copy's workspace row.
    pub workspace_id: String,
    /// The id the candidate will carry; already minted, so the pin the seal
    /// writes and the row `finish_seal` records name the same thing.
    pub candidate_id: CandidateId,
    /// Full tree digest; physical pins use a collision-checked short prefix.
    pub namespace: String,
    /// The child copy the snapshot runs in.
    pub path: PathBuf,
    /// The commit the copy was minted from.
    pub base_commit: String,
    /// The tree hash at that base; a copy whose content still equals this
    /// sealed nothing and yields no candidate.
    pub base_content: String,
    /// The jj change the copy was minted on; `None` on git.
    pub base_change_id: Option<String>,
    /// What the termination path had already set; `finish_seal` restores it
    /// rather than declaring `Completed` on every path.
    pub prior_status: NodeStatus,
}

impl SealTicket {
    /// The VCS half of the seal. Takes no tree reference precisely so it can
    /// be called with the tree mutex released.
    ///
    /// A copy that never diverged from its base yields no candidate, and the
    /// pin `seal_candidate` placed is dropped here, in the same unlocked
    /// phase. An unpin refusal is a recoverable seal failure, never an empty
    /// candidate success.
    pub fn seal(&self, vcs: &dyn VcsOps) -> Result<Option<VcsSnapshot>, VcsError> {
        let sealed = vcs.seal(&self.path, &self.namespace, self.candidate_id.as_str())?;
        if !diverged_from_base(&sealed, &self.base_content) {
            vcs.unpin(&self.path, &self.namespace, self.candidate_id.as_str())?;
            return Ok(None);
        }
        Ok(Some(sealed))
    }
}

/// Whether a sealed snapshot carries anything beyond the copy's base:
/// a copy that never diverged has nothing to keep.
fn diverged_from_base(snapshot: &VcsSnapshot, base_content: &str) -> bool {
    snapshot.content != base_content
}

/// Seam between the authority checks of `adopt_candidate` and the three-way
/// landing. Carries no tree reference for the same reason.
#[derive(Clone, Debug)]
pub struct AdoptTicket {
    caller_id: String,
    candidate: CandidateRecord,
    target_path: PathBuf,
    child_path: PathBuf,
    expected_target: Option<String>,
    /// The evidence `begin_adopt` admitted, recorded on the candidate when the
    /// landing succeeds.
    evidence: Vec<String>,
}

impl AdoptTicket {
    /// The subprocess half of the adopt. Holds no tree
    /// reference so it can run with the tree mutex released.
    pub fn land(&self, vcs: &dyn VcsOps) -> Result<AdoptLanding, AdoptFailure> {
        vcs.adopt(
            &self.target_path,
            &self.child_path,
            &self.candidate.base_commit,
            self.candidate.base_change_id.as_deref(),
            &VcsSnapshot {
                commit: self.candidate.commit.clone(),
                content: self.candidate.content.clone(),
                change_id: self.candidate.change_id.clone(),
            },
            self.expected_target.as_deref(),
        )
    }
}

/// The template's `on_mint` hook for one freshly minted copy.
/// Carries no tree reference: the command runs with the tree mutex released,
/// through [`VcsOps::run_mint_hook`].
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

/// The mint one reserved spawn owes the VCS. Carries no
/// tree reference for the same reason [`SealTicket`] and [`AdoptTicket`] do:
/// `spawn_reserve` hands it back with the mutex held, the runtime runs it
/// with the mutex released, and `spawn_commit`/`spawn_abort` record the
/// answer under the lock again. It is not queued on [`Effects`] because the
/// spawn receipt names what the mint returned, so the answer has to reach
/// the caller rather than only the tree.
#[derive(Clone, Debug)]
pub struct MintTicket {
    launch_checkout: PathBuf,
    /// Logical node identity, retained for VCS snapshot labels and test ports.
    agent_id: String,
    /// Tree-scoped VCS identity. Unlike the logical agent id, this cannot be reused by
    /// another tree sharing the launch checkout.
    copy_id: String,
    caller_path: PathBuf,
    mode: WorkspaceMode,
    input_base: Option<VcsSnapshot>,
}

impl MintTicket {
    /// The copy shape the plan chose.
    #[must_use]
    pub(super) const fn mode(&self) -> WorkspaceMode {
        self.mode
    }

    /// The subprocess half of the mint. Holds no tree reference so it can
    /// run with the tree mutex released.
    pub fn mint(&self, vcs: &dyn VcsOps) -> Result<MintedCopy, VcsError> {
        vcs.mint_from(
            &self.launch_checkout,
            &self.agent_id,
            &self.copy_id,
            &self.caller_path,
            self.mode,
            self.input_base.as_ref(),
        )
    }
}

/// What `bind_workspace` planned for one reserved spawn: either the caller's
/// own copy (an `inherit` bind, which mints nothing) or a [`MintTicket`].
#[derive(Debug)]
enum WorkspacePlan {
    Inherit(BoundWorkspace),
    Mint(MintTicket),
}

/// One admitted-but-unminted spawn. Everything
/// `spawn_agent` decided under the lock, held until the mint answers: the
/// node, attempt, grant, and workspace rows are written by
/// [`ProductAgentTree::spawn_commit`], and [`ProductAgentTree::spawn_abort`]
/// gives the reservation back. Only the workspace row is reserved eagerly,
/// under the `Minting` marker, so a second spawn racing this one cannot mint
/// the same id.
#[derive(Debug)]
pub struct SpawnReservation {
    caller_id: String,
    caller_path: PathBuf,
    request: SpawnAgentRequest,
    operation_id: Option<String>,
    agent_id: String,
    attempt_id: AttemptId,
    child_depth: u8,
    tier: Tier,
    account: String,
    tuple_id: String,
    effort: Option<String>,
    output_schema_json: String,
    schema_digest: String,
    template: TemplateName,
    /// The copy shape the plan chose; `Inherit` when nothing was minted.
    mode: WorkspaceMode,
    parent_grant: TopologyGrant,
    child_grant: TopologyGrant,
    /// The bind for an `inherit` spawn; `None` when a [`MintTicket`] owes the
    /// row its path, branch, and base.
    bound: Option<BoundWorkspace>,
}

/// What [`ProductAgentTree::spawn_reserve`] admitted: a receipt the operation
/// ledger already holds, or a reservation waiting on its mint.
#[derive(Debug)]
pub enum SpawnAdmission {
    /// this call, or this `request_key`, already has a receipt.
    Replayed(SpawnAgentOutput),
    /// The reservation and the mint it owes; `None` for an `inherit` bind.
    Reserved(Box<SpawnReservation>, Option<MintTicket>),
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
    /// Candidate pins sealed from this copy, dropped once the copy is gone.
    /// Automatic cleanup removes only adopted candidates. Explicit discard
    /// relinquishes unadopted candidates too.
    pub candidate_ids: Vec<String>,
    /// Full tree digest owning these candidate pins.
    pub namespace: String,
}

/// What one tree mutation owes the VCS once the mutex is released.
/// The tree never runs a subprocess: every mutating
/// entry queues its seals, adopts, hooks, and unmints here, the runtime takes
/// them with [`ProductAgentTree::take_effects`], runs them unlocked through
/// [`Effects::run`], and re-locks once to [`ProductAgentTree::complete`] the
/// result. A ticket that is never completed leaves its node `Sealing` or its
/// row `Adopting` or `Discarding`, which reopens `RecoveryRequired` — the same fault a crash
/// mid-subprocess leaves.
#[derive(Clone, Debug, Default)]
#[must_use = "effects the caller drops are stranded: the node stays Sealing or the row Adopting/Discarding, which reopens RecoveryRequired"]
pub struct Effects {
    /// Copies to seal into candidates.
    pub seals: Vec<SealTicket>,
    pub(crate) dormancy: Vec<dormancy::DormancyTicket>,
    /// Candidates to land onto their targets.
    pub adopts: Vec<AdoptTicket>,
    /// `on_mint` commands owed to freshly minted copies.
    pub hooks: Vec<MintHook>,
    /// Copies to unmint, under this launch checkout.
    pub unmints: Vec<UnmintTicket>,
    /// The checkout every unmint is asked against; an empty path means the
    /// batch queued nothing that needs one.
    pub launch_checkout: PathBuf,
}

impl Effects {
    /// Folds a later batch into this one, so a caller that mutated twice
    /// settles once.
    pub fn absorb(&mut self, other: Self) {
        let Self {
            seals,
            dormancy,
            adopts,
            hooks,
            unmints,
            launch_checkout,
        } = other;
        self.seals.extend(seals);
        self.dormancy.extend(dormancy);
        self.adopts.extend(adopts);
        self.hooks.extend(hooks);
        self.unmints.extend(unmints);
        if self.launch_checkout.as_os_str().is_empty() {
            self.launch_checkout = launch_checkout;
        }
    }

    /// Whether this batch owes the VCS nothing, so the caller can skip the
    /// unlocked phase entirely.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dormancy.is_empty()
            && self.seals.is_empty()
            && self.adopts.is_empty()
            && self.hooks.is_empty()
            && self.unmints.is_empty()
    }

    /// Runs every queued effect against `vcs`. Holds no tree reference, so it
    /// cannot be called with the tree mutex held by construction of its
    /// arguments; a `FakeVcs` in tests records whether it was anyway.
    #[must_use]
    pub fn run(self, vcs: &dyn VcsOps) -> Completed {
        let seals = self
            .seals
            .into_iter()
            .map(|ticket| {
                let sealed = ticket.seal(vcs);
                (ticket, sealed)
            })
            .collect();
        let adopts = self
            .adopts
            .into_iter()
            .map(|ticket| {
                let landed = ticket.land(vcs);
                (ticket, landed)
            })
            .collect();
        let hooks = self
            .hooks
            .into_iter()
            .map(|hook| {
                let ran = vcs.run_mint_hook(&hook.cwd, &hook.command, &hook.env);
                (hook, ran)
            })
            .collect();
        let unmints = self
            .unmints
            .into_iter()
            .map(|ticket| {
                let removed = vcs.unmint(&self.launch_checkout, &ticket.path);
                if removed.is_ok() {
                    for candidate_id in &ticket.candidate_ids {
                        // A surviving pin keeps adopted or explicitly
                        // relinquished content reachable; the copy is gone.
                        let _ = vcs.unpin(&self.launch_checkout, &ticket.namespace, candidate_id);
                    }
                }
                (ticket, removed)
            })
            .collect();
        let dormancy = self
            .dormancy
            .into_iter()
            .map(|ticket| {
                let answer = ticket.run(vcs, &self.launch_checkout);
                (ticket, answer)
            })
            .collect();
        Completed {
            dormancy,
            seals,
            adopts,
            hooks,
            unmints,
        }
    }
}

/// The VCS answers to one [`Effects`] batch, ready for
/// [`ProductAgentTree::complete`].
#[derive(Debug, Default)]
pub struct Completed {
    dormancy: Vec<(
        dormancy::DormancyTicket,
        Result<dormancy::DormancyAnswer, VcsError>,
    )>,
    seals: Vec<(SealTicket, Result<Option<VcsSnapshot>, VcsError>)>,
    adopts: Vec<(AdoptTicket, Result<AdoptLanding, AdoptFailure>)>,
    hooks: Vec<(MintHook, Result<(), VcsError>)>,
    unmints: Vec<(UnmintTicket, Result<(), VcsError>)>,
}

impl Completed {
    pub(crate) fn refuse_unpublished_unmints(&mut self, tickets: Vec<UnmintTicket>) {
        self.unmints.extend(tickets.into_iter().map(|ticket| {
            (
                ticket,
                Err(VcsError::new(
                    VcsErrorKind::RecoveryRequired,
                    "removal preparation was not durably published; no removal dispatched",
                )),
            )
        }));
    }
}

/// What [`ProductAgentTree::complete`] recorded: the adopt receipts in the
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
    /// Removal failures; no affected copy or operation is reported discarded.
    pub cleanup_failures: Vec<TopologyError>,
}

/// What the workspace plan hands the spawn: the row it bound or minted.
#[derive(Debug)]
struct BoundWorkspace {
    workspace_id: String,
    path: PathBuf,
    branch: Option<String>,
    mode: WorkspaceMode,
    base: Option<String>,
}

/// Why a rebind or roster apply is refused for this epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpochRebindRefusal {
    /// The tree reopened paused and has not been resumed.
    RestoredPaused,
    /// A spawn is mid-mint, so the tree is not quiescent.
    PendingSpawns,
    /// Descendants are still live; a successor epoch needs a quiet tree.
    LiveDescendants,
}

impl EpochRebindRefusal {
    /// The operator sentence for this refusal, which names the state and the
    /// exact keystrokes that clear it.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::RestoredPaused => ProductAgentTree::TREE_EPOCH_REBIND_PAUSED,
            Self::PendingSpawns => ProductAgentTree::TREE_EPOCH_REBIND_PENDING_SPAWNS,
            Self::LiveDescendants => ProductAgentTree::TREE_EPOCH_REBIND_LIVE_DESCENDANTS,
        }
    }
}

/// The part of a settled wait row that identifies what was delivered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DeliveredRow {
    attempt_id: Option<AttemptId>,
    status: NodeStatus,
    attempt: AttemptStatus,
}

impl DeliveredRow {
    fn of(row: &WaitAgentRow) -> Self {
        Self {
            attempt_id: row.attempt_id.clone(),
            status: row.status,
            attempt: row.attempt,
        }
    }
}

/// One durable agent tree: nodes, their grants and attempts, the candidates
/// they sealed, the workspaces they work in, the mailbox, and the operation
/// ledger, all under one epoch.
///
/// Every mutation runs under one mutex and never runs a subprocess: what a
/// mutation owes the VCS leaves as [`Effects`] or a [`MintTicket`] and is run
/// with the lock released. `revision` advances on every mutation and is the
/// store's compare-and-swap token.
#[derive(Clone, Debug)]
pub struct ProductAgentTree {
    /// The tree's durable id; one tree per root session.
    pub tree_id: String,
    /// The root node's agent id.
    pub root_agent_id: String,
    /// Advances on every mutation; the projections and the store both read
    /// it, so a stale view is recognisable.
    pub revision: u64,
    /// The revision the tree's store last acknowledged for this handle: 0
    /// for a tree never published, otherwise what `load` handed back or the
    /// last `save` wrote. The store's compare-and-swap token;
    /// process-local, never a persisted row.
    pub stored_revision: u64,
    /// Which host process generation last opened this tree; a reopen under a
    /// new generation is what pauses admission.
    pub host_generation: u64,
    /// Whether the tree admits new work, is paused after a reopen, or is
    /// closed for shutdown.
    pub state: TreeAdmissionState,
    /// Whether one lease or many may write the launch checkout.
    pub writer_mode: WriterMode,
    /// Models this tree may bind, when the epoch restricts them.
    pub allowed_tuples: BTreeSet<String>,
    /// The root's own model.
    pub selected_tuple: String,
    /// The checkout the session was launched in; every copy is minted from it
    /// and every adopt lands back onto it.
    pub launch_checkout: PathBuf,
    /// This epoch's frozen bind authority: root bind, bounds, working set,
    /// and the product's account roster.
    pub snapshot: OrchestrationSnapshot,
    nodes: BTreeMap<String, NodeRecord>,
    grants: BTreeMap<String, TopologyGrant>,
    attempts: BTreeMap<AttemptId, AttemptRecord>,
    candidates: BTreeMap<CandidateId, CandidateRecord>,
    workspaces: BTreeMap<String, WorkspaceRecord>,
    messages: BTreeMap<String, MailRecord>,
    inbound: BTreeMap<String, Vec<String>>,
    next_message_sequence: u64,
    pub(crate) parked_waits: BTreeMap<(String, String), Vec<String>>,
    pub(crate) delivery_claims: BTreeSet<String>,
    /// The Start a send reserved for a child, by child, naming the message
    /// whose delivery starts it: at most one send delivers a Start to a
    /// child at a time; process-local.
    pub(crate) start_reservations: BTreeMap<String, String>,
    /// Host turn waiters running in this process, per child. A bound turn
    /// with no waiter is released by nobody. Process-local; never persisted.
    turn_waiters: BTreeMap<String, usize>,
    /// Startup and waiter ownership, mirrored under the hub admission lock.
    /// Never included in tree rows or view rebuilds.
    turn_owners: BTreeMap<String, (Option<AttemptId>, Option<String>)>,
    /// Parked children, process-local (`park`).
    parked: BTreeMap<String, park::ParkedChild>,
    operations: BTreeMap<String, OperationRecord>,
    /// Runtime command intents by command id (`agent_commands`).
    commands: BTreeMap<String, AgentCommand>,
    /// The tree projector's cursor per runtime session (`agent_feed_cursors`).
    feed_cursors: BTreeMap<String, u64>,
    compaction_views: BTreeMap<AttemptId, plakous_host_protocol::views::AttemptCompaction>,
    compaction_cursors: BTreeMap<String, (u64, Option<AttemptId>, bool)>,
    /// Process-local work list, never evidence that a close was applied.
    pub(crate) pending_close_folds: BTreeSet<String>,
    #[cfg(test)]
    fold_scratch_clones: usize,
    /// Projector observations awaiting this handle's next save; never persisted.
    pub(crate) pending_turn_usages: BTreeMap<plakous::model::TurnId, ProjectedChildTurnUsage>,
    pending_spawns: u32,
    /// Child ids reserved by a `spawn_reserve` whose mint has not answered
    /// — the tree's `Minting` state. Process-local and
    /// never persisted: a reservation cannot outlive the process that took
    /// it, and until it commits no node, attempt, grant, or workspace row
    /// names the id, so every other tool answers for it exactly as it does
    /// for an id that was never spawned.
    minting: BTreeSet<String>,
    live_generations: BTreeMap<String, LiveGeneration>,
    /// What the mutations since the last [`Self::take_effects`] owe the VCS.
    /// Never persisted; never run by the tree.
    effects: Effects,
    /// Exact seal ownership survives draining the effect queue and changes to
    /// presentation status. Only the matching completion releases it.
    outstanding_seals: BTreeMap<String, Arc<()>>,
    /// In-process detach telemetry guard; never persisted.
    detach_emitted: HashSet<String>,
    /// Copies whose automatic prune failed in this process; they are not
    /// retried until the next open, so a stuck removal cannot loop.
    prune_failed: BTreeSet<String>,
    /// Copies whose removal is queued or running in this process. A
    /// `discarding` row outside this set is stale (a failed or interrupted
    /// removal), so the prune may retry it once its occupants are closed.
    removals_in_flight: BTreeSet<String>,
    /// What each caller's bare `wait_agent` already handed back, per child:
    /// the settled row's attempt and statuses. A bare wait watches only
    /// children whose current row differs, so every settled result reaches a
    /// caller once. Process-local; acknowledged current-attempt Result
    /// envelopes separately preserve delivery across reopen (spec 03 §4.4).
    delivered: BTreeMap<(String, String), DeliveredRow>,
    /// The `delivered` marks an admitted host command's acknowledgement
    /// made (none stood for that row before), so returning that command's
    /// mail withdraws only those. Dropped when a later acknowledgement did
    /// not make the mark, or a later wait overwrites or rolls the row back.
    /// Process-local, like `delivered`.
    admission_marks: BTreeSet<(String, String)>,
    /// Why a node fell to `RecoveryRequired` in this process (spawn start,
    /// bind, or mail delivery failure). Process-local diagnostics for the
    /// canvas and the caller's tool result; never persisted and never
    /// recovery authority.
    fault_reasons: BTreeMap<String, String>,
    /// Failure kind for the next materialization failure of a node, set by the
    /// turn driver when it knows a better label than the generic one.
    /// Every conflicting path of an adopt that refused with `merge_conflict`,
    /// by candidate id, until the adopting call takes it: the bounded refusal
    /// message cannot carry them all.
    adopt_conflicts: BTreeMap<String, Vec<String>>,
    /// Recovery refusals no retry can get past (an incompatible session, or a
    /// head that needs effect evidence the host does not hold), per node and
    /// for the attempt they were observed on. They let `close_agent` end the
    /// node. Process-local: the next recovery observes them again.
    terminal_recovery: BTreeMap<String, (AttemptId, AttemptFailure)>,
    /// Attempts one root prompt may open (`max_attempts`). Seeded from
    /// the host constant; `lower_max_attempts` may only lower it.
    max_attempts: usize,
    /// How many attempts existed when the root's current prompt began; the
    /// attempt budget counts from here. Process-local: a reopened tree
    /// starts counting at the next prompt.
    prompt_attempt_base: usize,
    /// Fired from `bump()` so a `wait_agent` poll wakes on a settle, seal,
    /// question, mail, or steer instead of waiting out its tick.
    /// Process-local; never persisted.
    notify: Arc<tokio::sync::Notify>,
    /// The VCS port; see [`TreeVcs`].
    vcs: TreeVcs,
    /// Per-cell agent admissions for this epoch. Built from
    /// the snapshot's `max_agents_per_cell` at install, so a config change
    /// mid-tree never moves a running tree's ceiling. Process-local: a cell
    /// cannot outlive the process.
    cell_budget: Arc<CellAgentBudget>,
}

#[derive(Clone)]
struct LiveGeneration(Arc<AtomicU64>);

impl std::fmt::Debug for LiveGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("LiveGeneration")
            .field(&self.0.load(Ordering::Relaxed))
            .finish()
    }
}

impl LiveGeneration {
    fn new(generation: u64) -> Self {
        Self(Arc::new(AtomicU64::new(generation)))
    }
}

mod await_output;
mod candidates;
mod commands;
mod dormancy;
mod effect_outcomes;
mod epoch;
mod feed;
mod followup;
mod lifecycle;
mod mail;
pub mod observer;
mod operator_intents;
mod park;
mod projection;
mod projector;
mod recovery;
mod retention;
mod runtime_error;
pub mod text;
mod tree_store;
pub mod turn_outcome;
pub mod view;

pub use await_output::{
    AwaitAgents, AwaitJobRow, AwaitOmitted, AwaitOutput, AwaitRequest, WAIT_OUTPUT_BUDGET_BYTES,
};
pub use candidates::merge_conflict_message;
pub use commands::{AgentCommand, AgentCommandKind, CommandFact, MAX_TREE_COMMANDS};
pub use feed::TreeFeed;
pub use observer::{NodeDetach, TopologyObserver};
pub use park::WaitingOn;
/// The recording VCS double, for the topology crates' tests at the cut.
#[cfg(any(test, feature = "test-support"))]
pub use plakous_workspace::occupancy_vcs::fake::{FakeCall, FakeOp, FakeVcs};
pub use projector::{
    BindConflict, ChildFinalizationDecision, ChildTerminal, FailureRead, IntentJoinRefusal,
    ProjectedChildTurnUsage, TerminalRead, TreeFeedReads, TreeProjection, TreeProjectorError,
    terminal_reads_needed,
};
pub use recovery::{RECOVERED_WITHOUT_TURN, RecoverAgentOutput, RecoveryNeed, RecoveryResolution};
pub use runtime_error::{TreeRuntimeError, TreeRuntimeResult};
pub use tree_store::{
    MemoryTreeStore, PersistedTree, Revision, TreeStore, TreeStoreError, TreeStoreErrorKind,
    fence_revision,
};
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod test_support;
#[cfg(any(test, feature = "test-support"))]
pub use plakous_workspace::vcs_fixtures;
mod workspaces;

impl ProductAgentTree {
    /// Bootstraps a root over the system `git`/`jj` port.
    pub fn bootstrap_root(
        tree_id: impl Into<String>,
        root_agent_id: impl Into<String>,
        root_role: impl Into<String>,
        launch_checkout: PathBuf,
        selected_tuple: impl Into<String>,
        writer_mode: WriterMode,
    ) -> Result<Self, TopologyError> {
        Self::bootstrap_root_with(
            Arc::new(SystemVcs),
            tree_id,
            root_agent_id,
            root_role,
            launch_checkout,
            selected_tuple,
            writer_mode,
        )
    }

    /// [`Self::bootstrap_root`] over an explicit VCS port. The root's branch
    /// label is the first thing the port answers.
    pub fn bootstrap_root_with(
        vcs: Arc<dyn VcsOps>,
        tree_id: impl Into<String>,
        root_agent_id: impl Into<String>,
        root_role: impl Into<String>,
        launch_checkout: PathBuf,
        selected_tuple: impl Into<String>,
        writer_mode: WriterMode,
    ) -> Result<Self, TopologyError> {
        let tree_id = tree_id.into();
        let root_agent_id = root_agent_id.into();
        let selected_tuple = selected_tuple.into();
        if !valid_id(&tree_id) || !valid_id(&root_agent_id) {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "tree or root id is not a portable agent id",
            ));
        }
        let workspace_id = format!("ws-{root_agent_id}");
        if !valid_id(&workspace_id) {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "root workspace id is not portable",
            ));
        }
        let root = NodeRecord {
            fold_order: Some(0),
            abandoned_turn: None,
            agent_id: root_agent_id.clone(),
            parent_agent_id: None,
            depth: 0,
            description: root_role.into(),
            prompt: String::new(),
            output_schema_json: String::new(),
            schema_digest: String::new(),
            template: None,
            tuple_id: selected_tuple.clone(),
            account: String::new(),
            effort: None,
            status: NodeStatus::Running,
            workspace_id: workspace_id.clone(),
            tier: Tier::Mutate,
            attempt_id: None,
            session_locator: None,
            path: launch_checkout.clone(),
            branch: vcs.label(&launch_checkout),
            active_turn: false,
        };
        let grant = TopologyGrant {
            visible_ids: vec![root_agent_id.clone()],
            message_peers: Vec::new(),
            managed_ids: Vec::new(),
            may_spawn: true,
            may_adopt: true,
            max_depth: DEFAULT_GRANT_DEPTH,
            max_nodes: MAX_TREE_NODES as u8,
            max_children: MAX_DIRECT_CHILDREN as u8,
            interrupt_ids: Vec::new(),
            close_ids: Vec::new(),
        };
        let workspace = WorkspaceRecord {
            workspace_id: workspace_id.clone(),
            owner_agent_id: root_agent_id.clone(),
            path: launch_checkout.clone(),
            mode: WorkspaceMode::Inherit,
            status: WorkspaceStatus::Active,
            base_commit: None,
            base_content: None,
            base_change_id: None,
            generation: 0,
            adopter_agent_id: None,
        };
        let mut nodes = BTreeMap::new();
        nodes.insert(root_agent_id.clone(), root);
        let mut grants = BTreeMap::new();
        grants.insert(root_agent_id.clone(), grant);
        let mut workspaces = BTreeMap::new();
        workspaces.insert(workspace_id.clone(), workspace);
        let mut allowed = BTreeSet::new();
        allowed.insert(selected_tuple.clone());
        Ok(Self {
            tree_id,
            root_agent_id,
            revision: 1,
            stored_revision: 0,
            host_generation: 1,
            state: TreeAdmissionState::Live,
            writer_mode,
            allowed_tuples: allowed,
            selected_tuple: selected_tuple.clone(),
            launch_checkout,
            snapshot: OrchestrationSnapshot::unpinned(selected_tuple),
            nodes,
            grants,
            attempts: BTreeMap::new(),
            candidates: BTreeMap::new(),
            workspaces,
            messages: BTreeMap::new(),
            inbound: BTreeMap::new(),
            next_message_sequence: 1,
            parked_waits: BTreeMap::new(),
            delivery_claims: BTreeSet::new(),
            start_reservations: BTreeMap::new(),
            turn_waiters: BTreeMap::new(),
            pending_close_folds: BTreeSet::new(),
            turn_owners: BTreeMap::new(),
            parked: BTreeMap::new(),
            operations: BTreeMap::new(),
            pending_spawns: 0,
            minting: BTreeSet::new(),
            live_generations: BTreeMap::from([(workspace_id.clone(), LiveGeneration::new(0))]),
            effects: Effects::default(),
            #[cfg(test)]
            fold_scratch_clones: 0,
            pending_turn_usages: BTreeMap::new(),
            compaction_views: BTreeMap::new(),
            compaction_cursors: BTreeMap::new(),
            outstanding_seals: BTreeMap::new(),
            detach_emitted: HashSet::new(),
            delivered: BTreeMap::new(),
            admission_marks: BTreeSet::new(),
            commands: BTreeMap::new(),
            feed_cursors: BTreeMap::new(),
            prune_failed: BTreeSet::new(),
            removals_in_flight: BTreeSet::new(),
            fault_reasons: BTreeMap::new(),
            adopt_conflicts: BTreeMap::new(),
            terminal_recovery: BTreeMap::new(),
            max_attempts: MAX_TREE_ATTEMPTS,
            prompt_attempt_base: 0,
            notify: Arc::new(tokio::sync::Notify::new()),
            vcs: TreeVcs(vcs),
            cell_budget: Arc::new(CellAgentBudget::for_snapshot(
                &OrchestrationSnapshot::unpinned(String::new()),
            )),
        })
    }

    pub(super) fn vcs(&self) -> &dyn VcsOps {
        self.vcs.0.as_ref()
    }

    /// The tree's VCS port, shared: what a runtime runs the tree's effects
    /// on when it has not installed a port of its own.
    #[must_use]
    pub fn vcs_port(&self) -> Arc<dyn VcsOps> {
        Arc::clone(&self.vcs.0)
    }

    /// Installs the runtime's VCS port, so the mint inside `spawn_agent` and
    /// the effects the runtime runs unlocked drive one implementation.
    pub fn set_vcs(&mut self, vcs: Arc<dyn VcsOps>) {
        self.vcs = TreeVcs(vcs);
    }

    /// This epoch's per-cell agent budget. Shared by every
    /// topology tool built on the tree: the extensions are rebuilt per caller
    /// and per turn, so a counter owned by one set would enforce nothing.
    #[must_use]
    pub fn cell_budget(&self) -> Arc<CellAgentBudget> {
        Arc::clone(&self.cell_budget)
    }

    /// Drains what the mutations so far owe the VCS. The
    /// caller runs them with the mutex released and hands the answers back
    /// to [`Self::complete`].
    pub fn take_effects(&mut self) -> Effects {
        let mut effects = std::mem::take(&mut self.effects);
        effects.launch_checkout.clone_from(&self.launch_checkout);
        effects
    }

    /// Whether `agent_id` owns a queued or already dispatched seal.
    pub(super) fn seal_pending(&self, agent_id: &str) -> bool {
        self.outstanding_seals.contains_key(agent_id)
    }

    /// Re-lock half of an effects batch: records each seal, each adopt, and
    /// each failed hook. Adopt receipts come back in ticket order. A batch
    /// that recorded anything bumps the revision, so the publish that
    /// follows is a real change to the store's compare-and-swap.
    pub fn complete(&mut self, completed: Completed) -> Settled {
        let Completed {
            dormancy,
            seals,
            adopts,
            hooks,
            unmints,
        } = completed;
        let recorded_anything = !dormancy.is_empty()
            || !seals.is_empty()
            || !adopts.is_empty()
            || !unmints.is_empty()
            || hooks.iter().any(|(_, ran)| ran.is_err());
        let before = self.revision;
        let mut settled = Settled::default();
        for (ticket, answer) in dormancy {
            if let Err(error) = self.complete_dormancy(ticket, answer) {
                settled.cleanup_failures.push(error);
            }
        }
        for (ticket, sealed) in seals {
            let failure = sealed
                .as_ref()
                .err()
                .filter(|_| {
                    self.outstanding_seals
                        .get(&ticket.agent_id)
                        .is_some_and(|owner| Arc::ptr_eq(owner, &ticket.ownership))
                })
                .map(|error| {
                    let error = topology_error_from_vcs(error.clone());
                    TopologyError::new(
                        error.code,
                        format!(
                            "{}'s copy could not be sealed: {}",
                            ticket.agent_id, error.message,
                        ),
                    )
                });
            self.finish_seal_with(ticket, sealed);
            if let Some(error) = failure {
                settled.seal_failures.push(error);
            }
        }
        for (ticket, landed) in adopts {
            settled.adopts.push(self.finish_adopt(&ticket, landed));
        }
        for (hook, ran) in hooks {
            if let Err(error) = ran {
                settled.hook_failures.push(self.fail_mint_hook(
                    &hook.caller_id,
                    &hook.request,
                    hook.operation_id.as_deref(),
                    &hook.agent_id,
                    &error.detail,
                ));
            }
        }
        for (ticket, removed) in unmints {
            if let Err(error) = self.finish_unmint(ticket, removed) {
                settled.cleanup_failures.push(error);
            }
        }
        // A seal or adopt that just finished may be what a closed copy was
        // waiting on.
        self.prune_closed_copies();
        if recorded_anything && self.revision == before {
            self.bump();
        }
        settled
    }

    /// Runs the queued effects synchronously through the tree's own port
    /// and records them: what a test does where the runtime would release
    /// the mutex in between. Not for production callers.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(
        clippy::expect_used,
        reason = "test driver refuses malformed close fixtures"
    )]
    pub fn settle_for_test(&mut self) -> Settled {
        let mut settled = Settled::default();
        loop {
            self.fold_reserved_closes()
                .expect("test close fold must succeed");
            self.refresh_epoch_after_close();
            self.prune_closed_copies();
            self.publish_workspace_generations();
            let effects = self.take_effects();
            if effects.is_empty() {
                return settled;
            }
            let more = self.complete(effects.run(self.vcs()));
            settled.adopts.extend(more.adopts);
            settled.hook_failures.extend(more.hook_failures);
            settled.seal_failures.extend(more.seal_failures);
            settled.cleanup_failures.extend(more.cleanup_failures);
        }
    }

    /// One persisted message by id, for a deliverer that holds only the id.
    #[must_use]
    pub fn mail(&self, message_id: &str) -> Option<&MailRecord> {
        self.messages.get(message_id)
    }

    /// One node by id, refusing an unknown id as `grant_denied` rather than
    /// as a missing key: a caller may not learn whether an id it cannot see
    /// exists.
    ///
    /// # Errors
    ///
    /// Refuses when no node carries `id`.
    pub fn node(&self, id: &str) -> Result<&NodeRecord, TopologyError> {
        self.nodes.get(id).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::GrantDenied, "agent id is unknown")
        })
    }

    /// The node's current attempt; the root has none.
    #[must_use]
    pub fn attempt(&self, agent_id: &str) -> Option<&AttemptRecord> {
        self.current_attempt(agent_id)
    }

    fn current_attempt(&self, agent_id: &str) -> Option<&AttemptRecord> {
        let attempt_id = self.nodes.get(agent_id)?.attempt_id.as_ref()?;
        self.attempts.get(attempt_id)
    }

    fn current_attempt_mut(&mut self, agent_id: &str) -> Option<&mut AttemptRecord> {
        let attempt_id = self.nodes.get(agent_id)?.attempt_id.clone()?;
        self.attempts.get_mut(&attempt_id)
    }

    /// `expected_attempt_id` fences a request to the attempt the
    /// caller saw; a re-tasked node refuses with `attempt_mismatch`.
    fn require_expected_attempt(
        &self,
        agent_id: &str,
        expected: Option<&AttemptId>,
    ) -> Result<(), TopologyError> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let current = self.node(agent_id)?.attempt_id.as_ref();
        if current != Some(expected) {
            return Err(TopologyError::new(
                TopologyErrorCode::AttemptMismatch,
                format!(
                    "expected_attempt_id {expected} is not the current attempt{}",
                    current.map_or(String::new(), |id| format!(" ({id})"))
                ),
            ));
        }
        Ok(())
    }

    /// One workspace row by id.
    #[must_use]
    pub fn workspace(&self, id: &str) -> Option<&WorkspaceRecord> {
        self.workspaces.get(id)
    }

    /// Every node, keyed by agent id. The store writes these rows verbatim.
    pub fn nodes(&self) -> &BTreeMap<String, NodeRecord> {
        &self.nodes
    }

    /// `bootstrap_root` plus [`crate::epoch::RosterInput::test_working_set`], so a test
    /// tree can spawn `SpawnAgentRequest::test_review()` /
    /// `test_orchestrate()` without installing a config-derived epoch.
    #[cfg(any(test, feature = "test-support"))]
    pub fn bootstrap_root_for_test(
        tree_id: impl Into<String>,
        root_agent_id: impl Into<String>,
        root_role: impl Into<String>,
        launch_checkout: PathBuf,
        selected_tuple: impl Into<String>,
        writer_mode: WriterMode,
    ) -> Result<Self, TopologyError> {
        Self::bootstrap_root_for_test_with(
            Arc::new(SystemVcs),
            tree_id,
            root_agent_id,
            root_role,
            launch_checkout,
            selected_tuple,
            writer_mode,
        )
    }

    /// [`Self::bootstrap_root_for_test`] over an explicit VCS port, for the
    /// tests that run on `FakeVcs`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn bootstrap_root_for_test_with(
        vcs: Arc<dyn VcsOps>,
        tree_id: impl Into<String>,
        root_agent_id: impl Into<String>,
        root_role: impl Into<String>,
        launch_checkout: PathBuf,
        selected_tuple: impl Into<String>,
        writer_mode: WriterMode,
    ) -> Result<Self, TopologyError> {
        let selected_tuple = selected_tuple.into();
        let mut tree = Self::bootstrap_root_with(
            vcs,
            tree_id,
            root_agent_id,
            root_role,
            launch_checkout,
            selected_tuple.clone(),
            writer_mode,
        )?;
        let working_set = crate::epoch::RosterInput::test_working_set(selected_tuple);
        for (name, binding) in working_set.templates {
            tree.pin_template(name.as_str(), binding);
        }
        tree.pin_bind_hints_for_test(working_set.bind_hints);
        Ok(tree)
    }

    /// Installs an account roster on the live epoch. Production epochs carry
    /// the product's roster in through `install_snapshot`; a tree bootstrapped
    /// straight into a test admits only its working set's `codex` cards until
    /// this runs, so the caller — the product, or a fixture — says which
    /// accounts its epoch admits.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pin_bind_hints_for_test(&mut self, hints: BindHints) {
        self.snapshot.bind_hints = hints;
        self.snapshot.digest = self.snapshot.compute_digest();
    }

    /// Forces one node's status, so a test can stage a state the tree would
    /// otherwise only reach through a live turn.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_node_status_for_test(&mut self, agent_id: &str, status: NodeStatus) {
        if let Some(node) = self.nodes.get_mut(agent_id) {
            node.status = status;
        }
    }

    /// The node's current attempt, mutable, so a test can stage usage or a
    /// failure the runtime would have written.
    #[cfg(any(test, feature = "test-support"))]
    pub fn attempt_mut_for_test(&mut self, agent_id: &str) -> &mut AttemptRecord {
        self.current_attempt_mut(agent_id)
            .unwrap_or_else(|| unreachable!("test node {agent_id} has an attempt"))
    }

    /// Whether this tree is holding effects it has not handed out yet.
    ///
    /// Only ever true between a mutation and the `take_effects` that carries
    /// its work away; a caller that sees it true outside that window has
    /// stranded something.
    #[must_use]
    pub fn owes_effects(&self) -> bool {
        !self.effects.is_empty()
    }

    /// Records the revision a store acknowledged for this handle.
    ///
    /// The store's compare-and-swap token: the next save must
    /// find exactly this revision, so a handle that publishes without
    /// acknowledging is refused rather than allowed to overwrite.
    pub fn acknowledge_revision(&mut self, revision: Revision) {
        self.stored_revision = revision.value();
        self.publish_workspace_generations();
    }

    /// Every node's compiled grant, keyed by agent id.
    pub fn grants(&self) -> &BTreeMap<String, TopologyGrant> {
        &self.grants
    }

    /// Every attempt the tree has opened, keyed by attempt id; a node's
    /// history is the attempts that name it.
    pub fn attempts(&self) -> &BTreeMap<AttemptId, AttemptRecord> {
        &self.attempts
    }

    /// Every sealed candidate, adopted or not. An unadopted one pins its
    /// workspace against automatic cleanup, but not explicit discard.
    pub fn candidates(&self) -> &BTreeMap<CandidateId, CandidateRecord> {
        &self.candidates
    }

    /// Every checkout the tree is responsible for, including the launch one.
    pub fn workspaces(&self) -> &BTreeMap<String, WorkspaceRecord> {
        &self.workspaces
    }

    /// The whole mailbox, keyed by message id.
    pub fn messages(&self) -> &BTreeMap<String, MailRecord> {
        &self.messages
    }

    /// The operation ledger: what every idempotent call asked and answered.
    pub fn operations(&self) -> &BTreeMap<String, OperationRecord> {
        &self.operations
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "exact persisted tree reconstruction"
    )]
    /// Rebuilds a tree from the rows a store handed back, exactly as they
    /// were written. Applying the reopen law — paused admission, no restored
    /// turn — is [`Self::reopen`]'s job, not this one's.
    pub fn from_persisted(
        tree_id: String,
        root_agent_id: String,
        revision: u64,
        host_generation: u64,
        state: TreeAdmissionState,
        writer_mode: WriterMode,
        selected_tuple: String,
        launch_checkout: PathBuf,
        snapshot: OrchestrationSnapshot,
        nodes: BTreeMap<String, NodeRecord>,
        grants: BTreeMap<String, TopologyGrant>,
        attempts: BTreeMap<AttemptId, AttemptRecord>,
        candidates: BTreeMap<CandidateId, CandidateRecord>,
        workspaces: BTreeMap<String, WorkspaceRecord>,
        messages: BTreeMap<String, MailRecord>,
        operations: BTreeMap<String, OperationRecord>,
    ) -> Self {
        let mut allowed_tuples = BTreeSet::new();
        allowed_tuples.insert(selected_tuple.clone());
        for node in nodes.values() {
            allowed_tuples.insert(node.tuple_id.clone());
        }
        let mut inbound = BTreeMap::<String, Vec<String>>::new();
        // Each recipient retains its newest acknowledged records, including
        // the high-water id, so pruning never loses the minting sequence.
        let next_message_sequence = messages
            .keys()
            .filter_map(|id| id.strip_prefix('m')?.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let mut ordered_messages: Vec<_> = messages.values().collect();
        // Generated ids are canonical m<positive decimal sequence>. Length then
        // lexical order preserves their insertion order across the digit boundary.
        ordered_messages.sort_by(|left, right| {
            left.message_id
                .len()
                .cmp(&right.message_id.len())
                .then_with(|| left.message_id.cmp(&right.message_id))
        });
        for mail in ordered_messages {
            inbound
                .entry(mail.to.clone())
                .or_default()
                .push(mail.message_id.clone());
        }
        let pending_spawns = u32::try_from(
            nodes
                .values()
                .filter(|node| node.status == NodeStatus::Pending)
                .count(),
        )
        .unwrap_or(u32::MAX);
        let live_generations = workspaces
            .iter()
            .map(|(id, row)| (id.clone(), LiveGeneration::new(row.generation)))
            .collect();
        let cell_budget = Arc::new(CellAgentBudget::for_snapshot(&snapshot));
        Self {
            tree_id,
            root_agent_id,
            revision,
            stored_revision: revision,
            host_generation,
            state,
            writer_mode,
            allowed_tuples,
            selected_tuple,
            launch_checkout,
            snapshot,
            nodes,
            grants,
            attempts,
            candidates,
            workspaces,
            messages,
            inbound,
            next_message_sequence,
            parked_waits: BTreeMap::new(),
            delivery_claims: BTreeSet::new(),
            start_reservations: BTreeMap::new(),
            turn_waiters: BTreeMap::new(),
            pending_close_folds: BTreeSet::new(),
            turn_owners: BTreeMap::new(),
            parked: BTreeMap::new(),
            operations,
            pending_spawns,
            minting: BTreeSet::new(),
            live_generations,
            effects: Effects::default(),
            #[cfg(test)]
            fold_scratch_clones: 0,
            pending_turn_usages: BTreeMap::new(),
            compaction_views: BTreeMap::new(),
            compaction_cursors: BTreeMap::new(),
            outstanding_seals: BTreeMap::new(),
            detach_emitted: HashSet::new(),
            delivered: BTreeMap::new(),
            admission_marks: BTreeSet::new(),
            commands: BTreeMap::new(),
            feed_cursors: BTreeMap::new(),
            prune_failed: BTreeSet::new(),
            removals_in_flight: BTreeSet::new(),
            fault_reasons: BTreeMap::new(),
            adopt_conflicts: BTreeMap::new(),
            terminal_recovery: BTreeMap::new(),
            max_attempts: MAX_TREE_ATTEMPTS,
            prompt_attempt_base: 0,
            notify: Arc::new(tokio::sync::Notify::new()),
            vcs: TreeVcs(Arc::new(SystemVcs)),
            cell_budget,
        }
    }

    fn grant(&self, id: &str) -> Result<&TopologyGrant, TopologyError> {
        self.grants
            .get(id)
            .ok_or_else(|| TopologyError::new(TopologyErrorCode::GrantDenied, "grant is missing"))
    }

    fn require_live(&self) -> Result<(), TopologyError> {
        match self.state {
            TreeAdmissionState::Live | TreeAdmissionState::Interrupted => Ok(()),
            TreeAdmissionState::RestoredPaused => Err(TopologyError::new(
                TopologyErrorCode::TreeRestoredPaused,
                Self::TREE_PAUSED_REFUSAL,
            )),
            TreeAdmissionState::TreeTerminal => Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                Self::TREE_STOPPED_REFUSAL,
            )),
            TreeAdmissionState::Closed => Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "tree is closed",
            )),
        }
    }

    fn require_live_or_terminal_cleanup(&self) -> Result<(), TopologyError> {
        match self.state {
            TreeAdmissionState::Live
            | TreeAdmissionState::Interrupted
            | TreeAdmissionState::TreeTerminal => Ok(()),
            TreeAdmissionState::RestoredPaused => Err(TopologyError::new(
                TopologyErrorCode::TreeRestoredPaused,
                Self::TREE_PAUSED_REFUSAL,
            )),
            TreeAdmissionState::Closed => Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "tree is closed",
            )),
        }
    }

    fn bound_text(&self, value: &str) -> Result<(), TopologyError> {
        if value.len() > MAX_DESCRIPTION_PROMPT_SCHEMA_BYTES {
            return Err(TopologyError::new(
                TopologyErrorCode::BoundExceeded,
                "description, prompt, or schema exceeds 64 KiB",
            ));
        }
        Ok(())
    }

    fn can_list(&self, grant: &TopologyGrant, id: &str) -> bool {
        grant.visible_ids.iter().any(|item| item == id)
            || grant.message_peers.iter().any(|item| item == id)
            || grant.managed_ids.iter().any(|item| item == id)
    }

    fn preorder(&self, id: &str, out: &mut Vec<String>) {
        out.push(id.to_owned());
        let mut children: Vec<_> = self
            .nodes
            .values()
            .filter(|node| node.parent_agent_id.as_deref() == Some(id))
            .map(|node| node.agent_id.clone())
            .collect();
        children.sort();
        for child in children {
            self.preorder(&child, out);
        }
    }

    /// Every node id in preorder from the root: the order every projection
    /// and every rendered tree uses.
    pub fn preorder_ids(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.preorder(&self.root_agent_id, &mut out);
        out
    }

    fn collect_subtree(&self, id: &str, out: &mut Vec<String>) {
        out.push(id.to_owned());
        let children: Vec<_> = self
            .nodes
            .values()
            .filter(|node| node.parent_agent_id.as_deref() == Some(id))
            .map(|node| node.agent_id.clone())
            .collect();
        for child in children {
            self.collect_subtree(&child, out);
        }
    }

    fn bump(&mut self) {
        self.revision = self.revision.saturating_add(1);
        // a settle, seal, question, mail, or steer wakes a
        // waiting parent instead of leaving it parked on the next poll tick.
        self.notify.notify_waiters();
    }

    /// Lowers this tree's attempt ceiling. A raise is ignored:
    /// the host constant is the cap, and a tree may only ask for less.
    pub fn lower_max_attempts(&mut self, value: usize) {
        self.max_attempts = self.max_attempts.min(value.max(1));
    }

    /// Attempts remaining before `max_attempts` refuses the next one.
    #[must_use]
    pub fn max_attempts(&self) -> usize {
        self.max_attempts
    }

    /// Attempts a crash abandoned are refunded: the attempt did not fail on
    /// its merits, so it does not spend the tree's budget.
    pub(super) fn attempts_spent(&self) -> usize {
        self.attempts
            .values()
            .filter(|attempt| {
                attempt
                    .failure
                    .as_ref()
                    .is_none_or(|failure| failure.kind != TURN_ABANDONED)
            })
            .count()
            .saturating_add(self.collected_attempts_spent())
    }

    /// Spent attempts opened since the root's current prompt began.
    fn attempts_this_prompt(&self) -> usize {
        self.attempts_spent()
            .saturating_sub(self.prompt_attempt_base)
    }

    fn attempt_budget_exhausted(&self) -> bool {
        self.attempts_this_prompt() >= self.max_attempts
    }

    fn replay_success<T: Serialize>(
        &self,
        operation_id: &str,
        tool_name: &str,
        request: &T,
    ) -> Result<Option<String>, TopologyError> {
        let Some(prior) = self.operations.get(operation_id) else {
            return Ok(None);
        };
        let canonical = canonical_json(request)?;
        if prior.tool_name != tool_name || prior.canonical_request != canonical {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "retry content conflicts with the prepared operation",
            ));
        }
        if let Some(code) = prior.terminal_code {
            return Err(TopologyError::new(
                code,
                "retry of a terminal failed receipt",
            ));
        }
        match prior.terminal_response.as_ref() {
            Some(response) => Ok(Some(response.clone())),
            None if self.state == TreeAdmissionState::RestoredPaused => Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "prepared operation did not finish before restore",
            )),
            None => Ok(None),
        }
    }

    fn replay_typed<T: Serialize, R: DeserializeOwned>(
        &self,
        operation_id: Option<&str>,
        tool_name: &str,
        request: &T,
    ) -> Result<Option<R>, TopologyError> {
        let Some(id) = operation_id else {
            return Ok(None);
        };
        let Some(prior) = self.replay_success(id, tool_name, request)? else {
            return Ok(None);
        };
        serde_json::from_str(&prior).map(Some).map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "prior topology receipt could not be reconstructed",
            )
        })
    }

    /// Opens one idempotent call: returns the recorded response
    /// when this `operation_id` already answered, and otherwise records the
    /// request so a repeat can replay it.
    ///
    /// # Errors
    ///
    /// Refuses when the id was used for a different tool or a different
    /// request — a conflict, not a replay.
    pub fn prepare_operation<T: Serialize>(
        &mut self,
        operation_id: &str,
        caller_id: &str,
        tool_name: &str,
        request: &T,
    ) -> Result<Option<String>, TopologyError> {
        if let Some(prior) = self.replay_success(operation_id, tool_name, request)? {
            return Ok(Some(prior));
        }
        if self.operations.contains_key(operation_id) {
            return Ok(None);
        }
        let canonical = canonical_json(request)?;
        self.operations.insert(
            operation_id.to_owned(),
            OperationRecord {
                fold: None,
                operation_id: operation_id.to_owned(),
                caller_agent_id: caller_id.to_owned(),
                tool_name: tool_name.to_owned(),
                request_digest: digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
            },
        );
        Ok(None)
    }

    /// Records the exact success body a replay of this operation returns.
    ///
    /// # Errors
    ///
    /// Refuses when the response cannot be canonically encoded.
    pub fn remember_success<T: Serialize, R: Serialize>(
        &mut self,
        operation_id: &str,
        caller_id: &str,
        tool_name: &str,
        request: &T,
        response: &R,
    ) -> Result<(), TopologyError> {
        let canonical = canonical_json(request)?;
        let terminal_response = serde_json::to_string(response).map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "spawn receipt could not be encoded",
            )
        })?;
        self.operations.insert(
            operation_id.to_owned(),
            OperationRecord {
                fold: None,
                operation_id: operation_id.to_owned(),
                caller_agent_id: caller_id.to_owned(),
                tool_name: tool_name.to_owned(),
                request_digest: digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: Some(terminal_response),
            },
        );
        Ok(())
    }

    /// Records a refusal as this operation's terminal answer, so a repeat is
    /// refused the same way instead of being retried.
    ///
    /// # Errors
    ///
    /// Refuses when the request cannot be canonically encoded.
    pub fn remember_failure<T: Serialize>(
        &mut self,
        operation_id: &str,
        caller_id: &str,
        tool_name: &str,
        request: &T,
        code: TopologyErrorCode,
    ) -> Result<(), TopologyError> {
        let canonical = canonical_json(request)?;
        self.operations.insert(
            operation_id.to_owned(),
            OperationRecord {
                fold: None,
                operation_id: operation_id.to_owned(),
                caller_agent_id: caller_id.to_owned(),
                tool_name: tool_name.to_owned(),
                request_digest: digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: Some(code),
                terminal_response: None,
            },
        );
        Ok(())
    }
}

fn valid_id(value: &str) -> bool {
    (1..=ID_PATTERN_MAX).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn next_child_id(
    floor: u32,
    tree_id: &str,
    nodes: &BTreeMap<String, NodeRecord>,
    workspaces: &BTreeMap<String, WorkspaceRecord>,
    minting: &BTreeSet<String>,
    launch: &Path,
) -> String {
    let mut n = floor.saturating_add(1);
    loop {
        let id = format!("c{n}");
        let copy_id = copy_id(tree_id, &id);
        // A git worktree under this tree-scoped id that no row records is a
        // crashed mint of this tree: the git mint reclaims it rather than
        // skipping the id forever.
        let workspace_dest = launch.join(".plak/workspaces").join(&copy_id);
        if !nodes.contains_key(&id)
            && !workspaces.contains_key(&format!("ws-{id}"))
            && !minting.contains(&id)
            && !workspace_dest.exists()
        {
            return id;
        }
        n = n.saturating_add(1);
        if n == u32::MAX {
            return id;
        }
    }
}

fn copy_id(tree_id: &str, agent_id: &str) -> String {
    format!("{}-{agent_id}", digest_text(tree_id))
}

/// The exact root dialect the portable V2 subset requires.
pub const OUTPUT_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// The copyable shape quoted by the `spawn_agent` advice and every refusal.
pub const OUTPUT_SCHEMA_EXAMPLE: &str =
    r#"{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}"#;

/// One actionable `output_schema_json` refusal.
///
/// The message is the model's only repair instruction: the Code Mode `exec`
/// catalog renders nested tools as TypeScript declarations and drops per-field
/// descriptions, so a content-free code is unrecoverable in that composition.
fn output_schema_error(detail: &str) -> TopologyError {
    TopologyError::new(
        TopologyErrorCode::SchemaMismatch,
        format!(
            "output_schema_json {detail}. Send a JSON string holding one closed object schema, \
             like {OUTPUT_SCHEMA_EXAMPLE}. Property types are object, array, string, number, \
             integer, or boolean; no $ref and no combinators. \"$schema\" and \
             \"additionalProperties\": false are filled in when omitted; \"additionalProperties\": \
             true is refused because a submitted result is validated closed."
        ),
    )
}

/// Normalises the benign JSON Schema shapes exact models actually emit into the
/// portable V2 subset before admission.
///
/// A model (`codex · gpt-5.6-luna`, native Code Mode) can burn four
/// `spawn_agent` calls here: the model emitted a correct
/// Draft 2020-12 object schema with no `$schema` and no `additionalProperties`,
/// was refused with a content-free "not PortableToolSchemaV2", then guessed
/// `{"version":2,…}` and `{}`. Both omissions are JSON Schema defaults this
/// product already fixes, and the *normalised* document is what is frozen and
/// handed to the child, so the child is told exactly the closed contract its
/// `submit_result` is validated against. `additionalProperties: true` is the
/// one omission that cannot be filled in truthfully, so it is refused by name.
fn normalize_output_schema(value: &mut serde_json::Value, root: bool) -> Result<(), TopologyError> {
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    if root {
        match object.get("$schema") {
            None => {
                object.insert("$schema".to_owned(), OUTPUT_SCHEMA_DIALECT.into());
            }
            Some(serde_json::Value::String(uri)) if uri == OUTPUT_SCHEMA_DIALECT => {}
            Some(_) => {
                return Err(output_schema_error(
                    "names a dialect other than Draft 2020-12",
                ));
            }
        }
    }
    match object.get("type").and_then(serde_json::Value::as_str) {
        Some("object") => {}
        Some("array") => {
            if let Some(items) = object.get_mut("items") {
                normalize_output_schema(items, false)?;
            }
            return Ok(());
        }
        _ => return Ok(()),
    }
    match object.get("additionalProperties") {
        None => {
            object.insert("additionalProperties".to_owned(), false.into());
        }
        Some(serde_json::Value::Bool(false)) => {}
        Some(_) => {
            return Err(output_schema_error(
                "declares additionalProperties other than false",
            ));
        }
    }
    if !object.contains_key("required") {
        object.insert("required".to_owned(), serde_json::Value::Array(Vec::new()));
    }
    let Some(properties) = object
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(());
    };
    for property in properties.values_mut() {
        normalize_output_schema(property, false)?;
    }
    Ok(())
}

fn admit_output_schema(raw: &str) -> Result<(String, String), TopologyError> {
    let mut value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| output_schema_error("is not JSON"))?;
    // Some exact models double-encode the field: the argument is a JSON string
    // whose content is the schema text. One unwrap is the whole allowance.
    if let serde_json::Value::String(inner) = &value {
        let inner = serde_json::from_str(inner).map_err(|_| output_schema_error("is not JSON"))?;
        value = inner;
    }
    normalize_output_schema(&mut value, true)?;
    // The V2 reason is a static, content-free string; without it models loop on
    // schema_mismatch (231 rejections across one live corpus) because they cannot
    // tell which closed-dialect rule the authored schema broke.
    let schema = PortableToolSchemaV2::new(value).map_err(|error| {
        TopologyError::new(
            TopologyErrorCode::SchemaMismatch,
            format!(
                "output_schema_json is not PortableToolSchemaV2: {error}; every object needs \
                 $schema=https://json-schema.org/draft/2020-12/schema at the root, type, \
                 properties, required (array), additionalProperties=false"
            ),
        )
    })?;
    let frozen =
        String::from_utf8(schema.document().canonical_bytes().as_ref().to_vec()).map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "admitted schema is not UTF-8",
            )
        })?;
    Ok((frozen, schema.digest().to_string()))
}

fn validate_output_json(schema_json: &str, output_json: &str) -> Result<(), TopologyError> {
    let value: serde_json::Value = serde_json::from_str(schema_json).map_err(|_| {
        TopologyError::new(
            TopologyErrorCode::SchemaMismatch,
            "frozen output schema is not JSON",
        )
    })?;
    let schema = PortableToolSchemaV2::new(value).map_err(|_| {
        TopologyError::new(
            TopologyErrorCode::SchemaMismatch,
            "frozen output schema is not PortableToolSchemaV2",
        )
    })?;
    schema.parse_instance(output_json).map_err(|error| {
        TopologyError::new(
            TopologyErrorCode::SchemaMismatch,
            topology_output_schema_mismatch_message(&error.to_string()),
        )
    })?;
    Ok(())
}

fn canonical_json<T: Serialize>(value: &T) -> Result<String, TopologyError> {
    serde_json::to_string(value).map_err(|_| {
        TopologyError::new(
            TopologyErrorCode::OperationRecoveryRequired,
            "canonical request could not be encoded",
        )
    })
}

/// How much of a child's `output_json` a pushed `result` envelope carries.
/// The payload itself is on the wait row; this is the transcript preview.
const RESULT_MAIL_OUTPUT_BYTES: usize = 1024;

/// The first non-blank line of `text`, truncated on a character boundary.
fn first_line_bounded(text: &str, limit: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    bounded_text(line, limit)
}

/// The wire spelling of an attempt status, for host-authored mail bodies.
const fn attempt_status_label(status: AttemptStatus) -> &'static str {
    match status {
        AttemptStatus::Running => "running",
        AttemptStatus::Submitted => "submitted",
        AttemptStatus::Completed => "completed",
        AttemptStatus::CompletedWithoutSubmission => "completed_without_submission",
        AttemptStatus::Failed => "failed",
        AttemptStatus::Cancelled => "cancelled",
        AttemptStatus::RecoveryRequired => "recovery_required",
        AttemptStatus::Closed => "closed",
        AttemptStatus::Blocked => "blocked",
        AttemptStatus::Adopted => "adopted",
    }
}

/// How much of a mail body an `InboxMail` row carries. The whole body stays on
/// the mail record; this is what `list_agents` shows a caller that has no
/// session delivery (the root, or a hub between turns).
const INBOX_BODY_BYTES: usize = 2048;

#[cfg(test)]
use plakous_host_protocol::topology::MAX_MESSAGE_BYTES;

/// `text` truncated on a character boundary.
pub(crate) fn bounded_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
