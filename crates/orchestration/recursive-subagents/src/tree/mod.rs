//! Durable product-owned `ProductAgentTree`.
//!
//! Each node is a clean identity over a distinct public-facade Plakous session.
//! This module owns topology, grants, workspaces, candidates, mail, and the
//! per-attempt contract. It does not run a second inference loop.

mod bootstrap;
mod effects;
mod ledger;
mod records;

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
use crate::epoch::{OrchestrationSnapshot, RosterBinding};
use crate::protocol::{
    AdoptCandidateRequest, AdoptOutcome, AttemptId, AttemptScopedRequest, AttemptStatus,
    CandidateId, DEFAULT_DIRECT_CHILDREN, DEFAULT_GRANT_DEPTH, DEFAULT_TREE_ATTEMPTS,
    DEFAULT_TREE_NODES, ListAgentsRequest, MAX_DESCRIPTION_PROMPT_SCHEMA_BYTES,
    MAX_DIRECT_CHILDREN, MAX_MAILBOX_RECORDS, MAX_PENDING_SPAWNS, MAX_QUEUED_MAIL_BYTES,
    MAX_TREE_ATTEMPTS, MAX_TREE_DEPTH, MAX_TREE_NODES, MessagePriority, MessagePurpose, NodeStatus,
    SendAgentMessageRequest, SpawnAgentRequest, SpawnWorkspace, SubmitResultRequest, TemplateName,
    Tier, TopologyErrorCode, TopologyGrant, WaitAgentRequest, WorkspaceIdRequest, WorkspaceMode,
    WorkspaceStatus, WriterMode,
};
use plakous_workspace::occupancy_vcs::{
    AdoptFailure, AdoptLanding, AdoptRequest, MintRequest, MintedCopy, SystemVcs, VcsError,
    VcsErrorKind, VcsOps, VcsSnapshot, digest_text,
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
/// test-only convenience (`spawn_agent`, `list_agents`, `adopt_candidate`,
/// `settle_for_test`), which blocks on those tickets through this port.
#[derive(Clone)]
struct TreeVcs(Arc<dyn VcsOps>);

/// Drives `future` to its end on this thread, for the synchronous test
/// conveniences over the async VCS port. A port answer that runs on a
/// blocking worker wakes this thread when it is done.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn block_on_for_test<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = Box::pin(plakous_workspace::occupancy_vcs::stage::unstaged(
        "a synchronous test convenience",
        future,
    ));
    loop {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::park();
    }
}

impl std::fmt::Debug for TreeVcs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TreeVcs")
    }
}

impl NodeRecord {
    /// The status a face shows. A tree no longer pauses on reopen, so it is
    /// the node's own.
    #[must_use]
    pub const fn presentation_status(&self, _state: TreeAdmissionState) -> NodeStatus {
        self.status
    }
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

/// The failure `kind` on an attempt whose turn was cancelled from outside the
/// host.
pub const CANCELLED_KIND: &str = "cancelled";

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
    /// The runners a candidate sealed on another runner than its parent's
    /// is carried back between, into the parent's checkout.
    pub(crate) carry: Option<cross_runner::SealCarry>,
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
    resolution_refusal: Option<String>,
    resolver_paths: Vec<String>,
    revert: Option<revert::RevertPlan>,
    accept_conflicts: bool,
    conflict_pin: Option<plakous_workspace::occupancy_vcs::ConflictPin>,
    source: Option<CandidateRecord>,
    source_copy_agent: Option<String>,
    interrupted_adopt: bool,
    namespace: String,
    advance_copy_base: bool,
    caller_id: String,
    candidate: CandidateRecord,
    target_path: PathBuf,
    child_path: PathBuf,
    expected_target: Option<String>,
    /// The evidence `begin_adopt` admitted, recorded on the candidate when the
    /// landing succeeds.
    evidence: Vec<String>,
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
    namespace: String,
    launch_checkout: PathBuf,
    /// Logical node identity, retained for VCS snapshot labels and test ports.
    agent_id: String,
    caller_path: PathBuf,
    mode: WorkspaceMode,
    input_base: Option<VcsSnapshot>,
    merge_base: Option<VcsSnapshot>,
    /// The runners a copy on another runner than its parent's is carried
    /// between; `None` mints on the parent's runner.
    carry: Option<Box<cross_runner::Carry>>,
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
    /// The caller's epoch, which the child is spawned under.
    epoch_no: u64,
    /// The copy shape the plan chose; `Inherit` when nothing was minted.
    mode: WorkspaceMode,
    parent_grant: TopologyGrant,
    child_grant: TopologyGrant,
    /// The bind for an `inherit` spawn; `None` when a [`MintTicket`] owes the
    /// row its path, branch, and base.
    bound: Option<BoundWorkspace>,
    /// The spawning parent's composition, frozen on the child's node.
    parent_composition: Option<plakous_host_protocol::selection::CompositionSelection>,
    /// The runner the child is placed on when it is not its parent's.
    runner: Option<String>,
}

impl SpawnReservation {
    /// Freezes the spawning parent's composition on the child this
    /// reservation commits: the one spawn-parent input a profile switch of
    /// the parent would otherwise change under the child.
    pub fn set_parent_composition(
        &mut self,
        composition: Option<plakous_host_protocol::selection::CompositionSelection>,
    ) {
        self.parent_composition = composition;
    }
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

#[derive(Clone, Debug)]
pub(crate) struct BasePinTicket {
    ownership: Arc<()>,
    workspace_id: String,
    path: PathBuf,
    agent_id: String,
    namespace: String,
    base: String,
}

impl BasePinTicket {
    pub(crate) fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub(crate) fn base(&self) -> &str {
        &self.base
    }
}

impl AdoptTicket {
    /// The adopter and the candidate it lands, as a stage records them.
    pub(crate) fn stage_identity(&self) -> (&str, &CandidateId) {
        (&self.caller_id, &self.candidate.candidate_id)
    }
}

impl MintTicket {
    pub(crate) fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The commit the copy is minted over, when an input names one.
    pub(crate) fn input_commit(&self) -> Option<&str> {
        self.input_base.as_ref().map(|base| base.commit.as_str())
    }

    /// The runner the copy is minted on when it is not its parent's.
    pub(crate) fn target_runner(&self) -> Option<&str> {
        self.carry.as_ref().map(|carry| carry.target.as_str())
    }
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
    pub(crate) refreshes: Vec<refresh::RefreshTicket>,
    // Reopen-owned; ordinary mutation drains leave these for driver attachment.
    pub(crate) base_pins: Vec<BasePinTicket>,
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

/// The VCS answers to one [`Effects`] batch, ready for
/// [`ProductAgentTree::complete`].
#[derive(Debug, Default)]
pub struct Completed {
    refreshes: Vec<(
        refresh::RefreshTicket,
        Result<refresh::RefreshAnswer, VcsError>,
    )>,
    /// `Ok(false)`: the copy was absent, so it had no pin to restore.
    base_pins: Vec<(BasePinTicket, Result<bool, VcsError>)>,
    dormancy: Vec<(
        dormancy::DormancyTicket,
        Result<dormancy::DormancyAnswer, VcsError>,
    )>,
    seals: Vec<(SealTicket, Result<Option<VcsSnapshot>, VcsError>)>,
    adopts: Vec<(AdoptTicket, Result<AdoptLanding, AdoptFailure>)>,
    hooks: Vec<(MintHook, Result<(), VcsError>)>,
    unmints: Vec<(UnmintTicket, Result<(), VcsError>)>,
    /// The stages the batch ran as on a runner, settled with its answers.
    stages: Vec<Arc<plakous_workspace::occupancy_vcs::stage::VcsStage>>,
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

impl EpochRebindRefusal {
    /// The operator sentence for this refusal, which names the state and the
    /// exact keystrokes that clear it.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
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
    fn matches(&self, row: &WaitAgentRow) -> bool {
        let output_status = |status| {
            matches!(
                status,
                AttemptStatus::Submitted | AttemptStatus::Completed | AttemptStatus::Adopted
            )
        };
        self.attempt_id == row.attempt_id
            && ((output_status(self.attempt) && output_status(row.attempt))
                || (self.status == row.status && self.attempt == row.attempt))
    }

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
    /// Epoch 0's frozen bind authority: root bind, bounds, working set,
    /// and the product's account roster. Private: readers name whose epoch
    /// they need ([`Self::caller_epoch`], [`Self::epoch_of`],
    /// [`Self::current_epoch`]).
    snapshot: OrchestrationSnapshot,
    /// The epochs, the fork provenance and the per-runner ancestors.
    lineage: TreeLineage,
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
    /// Children whose bound or active turn the last process left: each
    /// takes no new turn until its `recover_node` ran in this process
    /// (spec 03 section 14.1). Process-local; never persisted.
    recovery_owed: BTreeSet<String>,
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
    /// The part of `pending_spawns` the root reserved: a spawn that binds the
    /// current epoch, so an epoch append must wait for it. A child's spawn
    /// binds its own epoch and never blocks an append.
    root_pending_spawns: u32,
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
    /// One base-pin reconciliation per workspace in this host generation.
    base_pin_recovery: BTreeMap<String, workspaces::BasePinRecovery>,
    /// Exact seal ownership survives draining the effect queue and changes to
    /// presentation status. Only the matching completion releases it.
    outstanding_seals: BTreeMap<String, Arc<()>>,
    /// Unlocked adopts currently owned by this host, including interrupted retries.
    inflight_adopts: BTreeSet<String>,
    /// Refresh effects currently owned by this process, including recovery.
    inflight_refreshes: BTreeSet<String>,
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
    /// Process-local automatic reclamation pacing; never recovery authority.
    reclaim_backoff: BTreeMap<String, dormancy::ReclaimBackoff>,
    reclaim_trim_epochs: BTreeMap<String, u64>,
    /// Failure kind for the next materialization failure of a node, set by the
    /// turn driver when it knows a better label than the generic one.
    /// Every conflicting path of an adopt that refused with `merge_conflict`,
    /// by candidate id, until the adopting call takes it: the bounded refusal
    /// message cannot carry them all.
    adopt_conflicts: BTreeMap<String, (Vec<String>, Option<String>)>,
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
    /// Per-cell agent admissions, one per held epoch. Built from each
    /// epoch's `max_agents_per_cell` at install, so a config change mid-tree
    /// never moves a running tree's ceiling. Process-local: a cell cannot
    /// outlive the process.
    cell_budgets: BTreeMap<u64, Arc<CellAgentBudget>>,
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
mod conflicts;
pub mod cross_runner;
mod custody;
mod dormancy;
mod effect_outcomes;
mod epoch;
mod feed;
mod followup;
mod intents;
mod lifecycle;
mod lineage;
mod mail;
pub mod observer;
mod operator_intents;
pub use operator_intents::{InterruptCause, interrupt_cause, submitted_output};
mod park;
mod projection;
mod projector;
mod recovery;
mod refresh;
mod reopen_law;
mod retention;
mod revert;
mod stages;
mod succession;
pub use refresh::{RefreshReport, RefreshStatus};
pub(crate) use stages::EffectStages;
mod runtime_error;
pub mod text;
mod tree_store;
pub mod turn_outcome;
pub mod view;

pub use await_output::{
    AwaitAgents, AwaitJobRow, AwaitOmitted, AwaitOutput, AwaitRequest, WAIT_OUTPUT_BUDGET_BYTES,
};
pub use candidates::{AdoptFacts, AdoptProbe, merge_conflict_message};
pub use commands::{AgentCommand, AgentCommandKind, CommandFact, MAX_TREE_COMMANDS};
pub use custody::{
    CustodyClaim, CustodyError, CustodyHold, CustodyHolding, CustodyHost, CustodyLiveness,
    CustodyRecord, CustodyTake, HolderGone, HostInstance, TreeCustody, TreeHostedElsewhere,
    acquire_custody, bind_custody_host, custody_host, holder_runs_here, machine_name,
    release_custody_hold, with_custody,
};
pub use feed::TreeFeed;
pub use intents::{
    ContinuationAnchor, ContinuationCause, ContinuationDecision, ContinuationIntent, EpochPending,
    TerminalRefusalIntent, VcsExpectedOutcome, VcsStageClass, VcsStageIntent, VcsStageKind,
};
use lineage::TreeLineage;
pub use lineage::{
    ForkedFrom, MAX_REFERENCED_EPOCHS, NodeLineId, NodePlacement, NodeWorkspace, TreeEpochs,
};
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
    fence_revision, prove_write,
};
#[cfg(test)]
mod base_pin_recovery_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod test_support;
#[cfg(any(test, feature = "test-support"))]
pub use plakous_workspace::vcs_fixtures;
mod workspaces;

impl ProductAgentTree {
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

    pub(crate) fn grant(&self, id: &str) -> Result<&TopologyGrant, TopologyError> {
        self.grants
            .get(id)
            .ok_or_else(|| TopologyError::new(TopologyErrorCode::GrantDenied, "grant is missing"))
    }

    fn require_live(&self) -> Result<(), TopologyError> {
        match self.state {
            // A stored `restored_paused` reads as live (the reopen law).
            TreeAdmissionState::Live
            | TreeAdmissionState::Interrupted
            | TreeAdmissionState::RestoredPaused => Ok(()),
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
            | TreeAdmissionState::RestoredPaused
            | TreeAdmissionState::TreeTerminal => Ok(()),
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
}

fn operation_request<T: Serialize>(
    tool_name: &str,
    request: &T,
) -> Result<(String, String), TopologyError> {
    let canonical = canonical_json(request)?;
    let digest = digest_text(&canonical);
    if tool_name != "submit_result" {
        return Ok((digest, canonical));
    }
    let request: SubmitResultRequest = serde_json::from_str(&canonical).map_err(|_| {
        TopologyError::new(
            TopologyErrorCode::OperationRecoveryRequired,
            "submit receipt request could not be encoded",
        )
    })?;
    let compact = canonical_json(&serde_json::json!({
        "turn_token": request.turn_token,
        "output_sha256": digest_text(&request.output_json),
        "output_bytes": request.output_json.len(),
    }))?;
    Ok((digest, compact))
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
    jj_copy_dirs: &BTreeSet<String>,
) -> String {
    let mut n = floor.saturating_add(1);
    loop {
        let id = format!("c{n}");
        // A jj workspace under this tree-scoped id that no row records is a
        // crashed mint. Skip the id; this listing does not reclaim it.
        if !nodes.contains_key(&id)
            && !workspaces.contains_key(&format!("ws-{id}"))
            && !minting.contains(&id)
            && !jj_copy_dirs.contains(&copy_id(tree_id, &id))
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
    plakous_workspace::occupancy_vcs::copy_names(&digest_text(tree_id)[..8], agent_id).dir
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

pub use records::{
    AbandonedChildTurn, AttemptFailure, AttemptRecord, BlockedReason, CandidateKind,
    CandidateRecord, DiscardWorkspaceOutput, EpochRebindRefusal, FoldAnchor, InboxMail,
    ListAgentsOutput, MailDisposition, MailDurableState, MailRecord, MintHook, NodeRecord,
    OperationRecord, SendMessageOutput, Settled, SpawnAgentOutput, SubmitResultOutput,
    UnmintTicket, UsageRow, WaitAgentRow, WaitReceiptSettlement, WaitWakeReason, WorkspaceReceipt,
    WorkspaceRecord,
};

#[cfg(any(test, feature = "test-support"))]
mod test_fixtures;
