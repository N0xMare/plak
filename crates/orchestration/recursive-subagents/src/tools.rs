//! Product-hosted topology tools (Tact names + occupancy).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures_util::FutureExt as _;
use futures_util::future::{Either, select};

use plakous::model::{
    ToolCallerLineage, ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName,
    ToolOutputContract,
};
use plakous::tools::{
    RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId,
    ToolFuture, ToolInvocation, ToolOutcome, ToolRegistration, ToolRuntimeError,
    ToolRuntimeErrorKind,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::cell_budget::{CellAgentBudget, CellIdentity};
use crate::epoch::RosterBinding;
use crate::protocol::{
    AdoptCandidateRequest, AttemptScopedRequest, ListAgentsRequest, MAX_DIRECT_CHILDREN,
    MAX_TREE_NODES, MessagePurpose, SendAgentMessageRequest, SpawnAgentRequest,
    SubmitResultRequest, TemplateName, Tier, TopologyErrorCode, WaitAgentRequest,
    WorkspaceIdRequest,
};
use crate::tree::text::errors::topology_wait_request_invalid_message;
use crate::tree::text::{
    PLAK_TOPOLOGY_ADOPT_DESCRIPTION_TEMPLATE, PLAK_TOPOLOGY_CLOSE_DESCRIPTION,
    PLAK_TOPOLOGY_DISCARD_DESCRIPTION, PLAK_TOPOLOGY_INTERRUPT_DESCRIPTION,
    PLAK_TOPOLOGY_JOBS_STOP_DESCRIPTION, PLAK_TOPOLOGY_JOBS_WAIT_DESCRIPTION,
    PLAK_TOPOLOGY_LIST_DESCRIPTION, PLAK_TOPOLOGY_OPERATION_FALLBACK_TEMPLATE,
    PLAK_TOPOLOGY_RECOVER_DESCRIPTION, PLAK_TOPOLOGY_SEND_DESCRIPTION,
    PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE, PLAK_TOPOLOGY_SUBMIT_DESCRIPTION,
    PLAK_TOPOLOGY_TASK_KILL_DESCRIPTION, PLAK_TOPOLOGY_TASK_OUTPUT_DESCRIPTION,
    PLAK_TOPOLOGY_WAIT_DESCRIPTION,
};
use crate::tree::{
    AWAIT_RECEIPT_KIND, AwaitAgents, AwaitJobRow, AwaitOutput, AwaitRequest, Effects,
    MailDisposition, MailDurableState, MintTicket, NodeDetach, ProductAgentTree,
    RecoverAgentOutput, SendMessageOutput, Settled, SpawnAdmission, SpawnAgentOutput,
    SpawnReservation, TopologyError, TopologyObserver, TreeStore, bounded_text,
    topology_error_from_vcs,
};
use crate::tree::{TreeRuntimeError, TreeRuntimeResult};
use plakous_workspace::occupancy_vcs::{VcsError, VcsOps};

enum MailConsumer {
    Wait {
        caller: String,
        operation: String,
    },
    Delivery(String),
    /// The Start `message` reserved for `agent`: a send's only exclusive
    /// right to deliver a Start to that child.
    Start {
        agent: String,
        message: String,
    },
}

struct MailConsumerGuard {
    tree: Arc<Mutex<ProductAgentTree>>,
    consumer: MailConsumer,
}

impl Drop for MailConsumerGuard {
    fn drop(&mut self) {
        if let Ok(mut tree) = self.tree.lock() {
            match &self.consumer {
                MailConsumer::Wait { caller, operation } => {
                    tree.parked_waits
                        .remove(&(caller.clone(), operation.clone()));
                }
                MailConsumer::Delivery(id) => {
                    tree.delivery_claims.remove(id);
                }
                MailConsumer::Start { agent, message } => {
                    if tree.start_reservations.get(agent) == Some(message) {
                        tree.start_reservations.remove(agent);
                    }
                }
            }
            tree.waiters().notify_waiters();
        }
    }
}

/// Everything the host needs to bind a spawned child's session and start its
/// attempt turn: the receipt plus the node's bind identity and brief, read
/// from the tree so a replayed receipt carries the same start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnStart {
    /// The spawn receipt the tree produced.
    pub spawned: SpawnAgentOutput,
    /// The one line naming the child's job.
    pub description: String,
    /// The child's brief, which its first turn runs.
    pub prompt: String,
    /// The schema the child's `submit_result` must satisfy.
    pub output_schema_json: String,
    /// The token that first turn must submit under.
    pub turn_token: u64,
    /// The checkout the child works in.
    pub path: String,
    /// Its effective tier after the template clip.
    pub tier: Tier,
    /// The account the child's card binds.
    pub account: String,
    /// The exact model the child's card binds.
    pub tuple_id: String,
    /// Reasoning effort, when the card named one.
    pub effort: Option<String>,
}

/// A bound child session. `turn_id` is the started assignment turn; a binder
/// that only materializes a session (tests) leaves it `None`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnBound {
    /// The binder already durably published the locator and turn before live
    /// admission. The caller must not restore an already completed child.
    pub durably_registered: bool,
    /// The child session the binder opened.
    pub session_locator: String,
    /// The assignment turn it started; `None` when the binder only
    /// materialised a session.
    pub turn_id: Option<String>,
}

/// A refused bind or a startup whose admitted work has not settled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnBindError {
    /// No session or turn remains owned by the binder.
    Refused(String),
    /// Ownership remains with an independent worker; sealing is forbidden.
    RecoveryRequired {
        /// The last observed startup stage.
        stage: crate::hub::ChildStartupStage,
        /// Actionable diagnostic without provider or credential payloads.
        message: String,
    },
}

impl From<String> for SpawnBindError {
    fn from(message: String) -> Self {
        Self::Refused(message)
    }
}
impl From<&str> for SpawnBindError {
    fn from(message: &str) -> Self {
        Self::Refused(message.into())
    }
}

/// Binds a spawned child to a real session **and starts its assignment turn**.
///
/// `spawn_agent` carries the prompt (mail `delegate` is refused), so
/// the child must be running its brief when spawn returns. Leaving the first
/// turn to a later `send_agent_message` left children `running` with empty
/// transcripts forever and every `wait_agent` burning its full timeout.
pub trait SpawnedSessionBinder: Send + Sync {
    /// Product admission of the spawn request itself, before the tree
    /// resolves its template. The default admits everything.
    ///
    /// # Errors
    ///
    /// Refuses when the product will not admit this spawn.
    fn admit_spawn(&self, request: &SpawnAgentRequest) -> Result<(), TopologyError> {
        let _ = request;
        Ok(())
    }

    /// Product admission of the frozen card a spawn resolved to:
    /// account availability, and nothing about the wire. Called after
    /// [`Self::admit_spawn`], with the tree mutex released and before the
    /// tree admits a node, once the caller's menu contains
    /// `request.template`. A template the epoch does not carry, or one that
    /// is not on the caller's menu, never reaches here.
    /// A refusal here spends no node, attempt, or copy. The default admits
    /// everything. Wire-vocabulary rules (an OpenRouter card asking for
    /// `native-ptc`) live in the tree's own resolution, so a spawn with no
    /// binder installed is held to them too.
    ///
    /// # Errors
    ///
    /// Refuses when the product will not admit this card's bind.
    fn admit_binding<'a>(
        &'a self,
        template: &'a TemplateName,
        binding: &'a RosterBinding,
    ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + 'a>> {
        let _ = (template, binding);
        Box::pin(async { Ok(()) })
    }

    /// Opens the child's session and starts its assignment turn. Awaited
    /// with the tree mutex released; the spawn records the answer afterwards.
    fn bind(
        &self,
        start: SpawnStart,
    ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>>;
}

/// Delivers one persisted mail record onto the bound child session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MailDeliveryRequest {
    /// The recipient node.
    pub agent_id: String,
    /// Its bound session.
    pub session_locator: String,
    /// What the tree already decided this message does to the turn.
    pub disposition: MailDisposition,
    /// The persisted message being delivered.
    pub message_id: String,
    /// Its text.
    pub body: String,
    /// The node's job line, for a delivery that starts a fresh turn.
    pub description: String,
    /// The node's brief, for the same reason.
    pub prompt: String,
    /// The schema a turn started by this delivery must submit against.
    pub output_schema_json: String,
    /// The checkout the recipient works in.
    pub path: String,
    /// The token the recipient's turn must submit under.
    pub turn_token: u64,
}

/// Runtime acknowledgement after Start / FollowUp / Steer.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct MailDeliveryAck {
    /// The turn the delivery started or joined, when it named one. A Start's
    /// turn the deliverer already bound; a joined turn was already bound. The
    /// send never binds it.
    pub turn_id: Option<String>,
}

/// Why an accepted mail record could not be delivered to its child session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MailDeliveryFailure {
    /// Delivery was definitely refused before a turn started; existing child
    /// authority remains reusable after the tree rolls back the new attempt.
    Refused(String),
    /// Delivery may have changed runtime authority and requires recovery.
    RecoveryRequired(String),
    /// The mail reached the child after the turn it targeted ended, or (a
    /// Start) found the child's slot held by a turn another command admitted.
    /// Nothing was delivered: the send looks at the child again (spec 03
    /// §4.3, §9).
    TurnEnded,
    /// The child's live slot is still starting or retiring: there is no turn
    /// to join yet, and none ended. Nothing was delivered; the send looks
    /// again once the slot changes.
    SlotBusy,
}

impl MailDeliveryFailure {
    fn into_parts(self) -> (bool, String) {
        match self {
            Self::Refused(message) => (true, message),
            Self::RecoveryRequired(message) => (false, message),
            Self::TurnEnded => (true, "the child's turn ended before delivery".to_owned()),
            Self::SlotBusy => (
                true,
                "the child's live slot was still starting or retiring".to_owned(),
            ),
        }
    }
}

impl From<String> for MailDeliveryFailure {
    fn from(message: String) -> Self {
        Self::RecoveryRequired(message)
    }
}

impl From<&str> for MailDeliveryFailure {
    fn from(message: &str) -> Self {
        Self::RecoveryRequired(message.to_owned())
    }
}

/// Starts, follows up, or steers the bound child session for one mail record.
///
/// A Start the deliverer admits is bound to the recipient's attempt and
/// marked active by the deliverer, under the tree lock, before that turn can
/// end. A follow-up or steer joins the turn the attempt already binds; a
/// follow-up the runtime admits as the next turn is bound by the host's turn
/// waiter when the turn before it ends. The send never binds or activates a
/// turn or acknowledges mail: the deliverer records a command intent and
/// projects its committed acceptance, including the exact envelope, before
/// returning success. A no-fact refusal remains a keyed host decision.
pub trait MailDeliverer: Send + Sync {
    /// Withdraws only after a definitive Start refusal, holding startup and
    /// exact session ownership across an authoritative tail read and tree CAS.
    /// The opening mail and supplied send failure receipt settle in that CAS.
    fn withdraw_followup(
        &self,
        driver: TreeDriver,
        agent_id: String,
        message_id: String,
        receipt: Option<(String, TopologyErrorCode)>,
    ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + '_>>;

    /// Hands one accepted message to the recipient's session, in whatever
    /// shape the tree's `disposition` already chose.
    fn deliver(
        &self,
        request: MailDeliveryRequest,
    ) -> Pin<Box<dyn Future<Output = Result<MailDeliveryAck, MailDeliveryFailure>> + Send + '_>>;
}

fn settle_followup_refusal(
    tree: &mut ProductAgentTree,
    agent: &str,
    message: &str,
    receipt: Option<(String, TopologyErrorCode)>,
) -> Result<(), TopologyError> {
    tree.record_undeliverable_mail(message)?;
    tree.project_mail_decisions(agent)?;
    if let Some((id, code)) = receipt {
        let operation = tree.operations().get(&id).cloned().ok_or_else(|| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "followup send receipt is missing",
            )
        })?;
        let request: Value = serde_json::from_str(&operation.canonical_request).map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "followup send receipt has invalid immutable request data",
            )
        })?;
        tree.remember_failure(
            &id,
            &operation.caller_agent_id,
            &operation.tool_name,
            &request,
            code,
        )?;
    }
    Ok(())
}

/// Cancels bound child turns after durable interrupt/close.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveCancelFailure {
    /// The operator-facing sentence, from the first failure.
    pub message: String,
    /// Agents whose live cancel or release failed. These fail closed.
    pub failed_ids: Vec<String>,
    /// Bounded joiner refusals; these never authorize another persisted fault.
    pub joiner_expired_ids: Vec<String>,
    /// Owner failures with the attempt identity captured before cancellation.
    pub owner_failures: Vec<crate::hub::LiveRetirementFailure>,
    /// Agents whose live turn was cancelled before the failure. Their work
    /// stopped; nothing about them needs recovery.
    pub cancelled_ids: Vec<String>,
    /// Agents that held no live turn. Not a cancel and not a failure — a
    /// close that reached nothing, which the caller may need to say.
    pub not_live_ids: Vec<String>,
}

/// What a batch cancel did when nothing failed.
///
/// Separated from "everything was cancelled" on purpose: a close whose
/// children had already finished reaches no live turn at all, and a caller
/// that reports "stopped 3 agents" for that is lying.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LiveCancelOutcome {
    /// Agents whose live turn was cancelled and released.
    pub cancelled_ids: Vec<String>,
    /// Agents that held no live turn.
    pub not_live_ids: Vec<String>,
}

/// Cancels the live turns behind nodes the tree has already moved.
pub trait LiveTurnCanceller: Send + Sync {
    /// Cancels the live turns of these agents, after the tree has already
    /// recorded the interrupt or close durably.
    fn cancel_agents(
        &self,
        agent_ids: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<LiveCancelOutcome, LiveCancelFailure>> + Send + '_>>;
}

/// Settles a `recovery_required` child by reopening its session exactly.
///
/// The tree cannot open a session; the host can. Recovery never replays a
/// model call or a tool, and a refusal leaves the node exactly as it was.
pub trait ChildRecoverer: Send + Sync {
    /// Recovers `agent_id`, fenced to `expected_attempt_id` when given.
    fn recover(
        &self,
        agent_id: String,
        expected_attempt_id: Option<crate::protocol::AttemptId>,
    ) -> Pin<Box<dyn Future<Output = Result<RecoverAgentOutput, TopologyError>> + Send + '_>>;
}

const ISSUER: &[u8] = b"plak.topology.v1";

#[derive(Clone, Copy, PartialEq, Eq)]
enum TopologyKind {
    Spawn,
    Send,
    List,
    Wait,
    Interrupt,
    Close,
    Recover,
    Submit,
    Adopt,
    Discard,
    /// The await slot under Grok's output tool: children and own processes.
    TaskOutput,
    /// The interrupt slot under Grok's kill tool: a child or an own process.
    TaskKill,
    /// The await slot under pi's `wait`: children and own processes.
    JobsWait,
    /// The interrupt slot under pi's `stop`: a child or an own process.
    JobsStop,
}

/// The vocabulary a node's pack presents the await and interrupt slots in.
///
/// The grant decides whether a node has the slots at all (spec 03 §3); the
/// vocabulary only decides their spelling and request shape.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TopologyVocabulary {
    /// `wait_agent` and `interrupt_agent` (Codex, Claude, OpenRouter).
    #[default]
    Plak,
    /// `get_command_or_subagent_output` and `kill_command_or_subagent`, which
    /// also cover the node's own processes (Grok Build's task tools). The
    /// host chooses it per node; this crate names no account.
    TaskTools,
    /// `wait` and `stop`, which also cover the node's own processes (the pi
    /// pack's process tools).
    WaitStop,
}

impl TopologyVocabulary {
    /// The kind that presents a granted slot under this vocabulary.
    const fn presents(self, slot: TopologyKind) -> TopologyKind {
        match (self, slot) {
            (Self::TaskTools, TopologyKind::Wait) => TopologyKind::TaskOutput,
            (Self::TaskTools, TopologyKind::Interrupt) => TopologyKind::TaskKill,
            (Self::WaitStop, TopologyKind::Wait) => TopologyKind::JobsWait,
            (Self::WaitStop, TopologyKind::Interrupt) => TopologyKind::JobsStop,
            (_, kind) => kind,
        }
    }

    /// The spelling a granted slot name takes under this vocabulary.
    fn presented_name(self, slot: &'static str) -> &'static str {
        TopologyKind::from_name(slot).map_or(slot, |kind| self.presents(kind).name())
    }
}

impl TopologyKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn_agent",
            Self::Send => "send_agent_message",
            Self::List => "list_agents",
            Self::Wait => "wait_agent",
            Self::Interrupt => "interrupt_agent",
            Self::Close => "close_agent",
            Self::Recover => "recover_agent",
            Self::Submit => "submit_result",
            Self::Adopt => "adopt_candidate",
            Self::Discard => "discard_workspace",
            Self::TaskOutput => "get_command_or_subagent_output",
            Self::TaskKill => "kill_command_or_subagent",
            Self::JobsWait => "wait",
            Self::JobsStop => "stop",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "spawn_agent" => Self::Spawn,
            "send_agent_message" => Self::Send,
            "list_agents" => Self::List,
            "wait_agent" => Self::Wait,
            "interrupt_agent" => Self::Interrupt,
            "close_agent" => Self::Close,
            "recover_agent" => Self::Recover,
            "submit_result" => Self::Submit,
            "adopt_candidate" => Self::Adopt,
            "discard_workspace" => Self::Discard,
            _ => return None,
        })
    }

    const fn mutating(self) -> bool {
        !matches!(
            self,
            Self::List | Self::Wait | Self::TaskOutput | Self::JobsWait
        )
    }

    /// Whether one admitted call of this kind spends one unit of the caller
    /// cell's agent budget.
    ///
    /// Only kinds that produce *new* agent work count. `wait_agent`,
    /// `list_agents`, `close_agent`, `recover_agent`, `interrupt_agent`,
    /// `submit_result`,
    /// `adopt_candidate` and `discard_workspace` are deliberately free: an
    /// exhausted cell must still be able to await, steer, and close the
    /// children it already has.
    ///
    /// `send_agent_message` is free here but not unconditionally: a
    /// `purpose=followup` re-tasks an idle child on a new attempt, which is new
    /// agent work. That decision needs the decoded request, so it lives in
    /// `TopologyTool::consumes_cell_agent_budget_for` beside this one rather
    /// than flipping `Send` to `true` for ordinary mail.
    const fn consumes_cell_agent_budget(self) -> bool {
        matches!(self, Self::Spawn)
    }
}

const fn presentation_kinds() -> [TopologyKind; 10] {
    [
        TopologyKind::Spawn,
        TopologyKind::List,
        TopologyKind::Send,
        TopologyKind::Wait,
        TopologyKind::Interrupt,
        TopologyKind::Close,
        TopologyKind::Recover,
        TopologyKind::Submit,
        TopologyKind::Adopt,
        TopologyKind::Discard,
    ]
}

/// Closed JSONL `invoke_tool` names: the same topology set the model can be granted.
pub fn is_topology_invoke_name(name: &str) -> bool {
    TopologyKind::from_name(name).is_some()
}

/// Where [`TopologyTool::spawn_admit`]'s first locked phase left the spawn:
/// a receipt (replayed, or an `inherit` bind that minted nothing), or a
/// reservation waiting on the mint that runs with the mutex released.
enum SpawnPhase {
    Done(SpawnAgentOutput, bool),
    Minting(Box<SpawnReservation>, MintTicket),
}

/// One tree with its store and its VCS port: the engine every tree mutation
/// runs on. [`Self::mutate`] locks and applies; the effects the
/// mutation queued come out with the result and [`Self::settle`] runs them
/// with the mutex released, re-locking once to record them and publish.
/// [`Self::save`] is the fenced whole-tree publish. The guard a caller sees
/// inside `mutate` is the tree alone — it offers no VCS, so a subprocess
/// under the lock is not expressible here.
#[derive(Clone)]
pub struct TreeDriver {
    /// The tree under its one mutex.
    ///
    /// `pub(crate)` and not `pub`: the tree's own tools interleave locked and
    /// unlocked phases deliberately and are reviewed as such, but a caller
    /// above this crate that locked the tree could mutate it and drop the
    /// guard without ever taking the [`Effects`] that mutation queued — a
    /// stranded seal or adopt, which reopens `RecoveryRequired`. Outside
    /// callers get [`Self::read`] and [`Self::mutate`] instead, and the
    /// mutation path cannot lose its effects because it returns them.
    pub(crate) tree: Arc<Mutex<ProductAgentTree>>,
    store: Arc<dyn TreeStore>,
    vcs: Arc<dyn VcsOps>,
    job_host: Arc<Mutex<Option<Arc<dyn plakous::tools::JobHost>>>>,
    retirement_recovery: Arc<Mutex<Option<RetirementRecovery>>>,
    observer: Option<Arc<dyn TopologyObserver>>,
    next_reclaim_probe: Arc<Mutex<u64>>,
}

type RetirementRecovery = Arc<dyn Fn() + Send + Sync>;

fn directory_bytes(path: &std::path::Path) -> std::io::Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut bytes = metadata.len();
    for entry in std::fs::read_dir(path)? {
        bytes = bytes.saturating_add(directory_bytes(&entry?.path())?);
    }
    Ok(bytes)
}

impl TreeDriver {
    /// Binds `tree` to `store` and `vcs`, installing `vcs` as the tree's own
    /// port so the mint inside `spawn_agent` and the effects run here drive
    /// one implementation.
    pub fn new(
        tree: Arc<Mutex<ProductAgentTree>>,
        store: Arc<dyn TreeStore>,
        vcs: Arc<dyn VcsOps>,
        observer: Option<Arc<dyn TopologyObserver>>,
    ) -> Self {
        if let Ok(mut guard) = tree.lock() {
            guard.set_vcs(Arc::clone(&vcs));
        }
        Self {
            tree,
            store,
            vcs,
            job_host: Arc::new(Mutex::new(None)),
            retirement_recovery: Arc::new(Mutex::new(None)),
            observer,
            next_reclaim_probe: Arc::new(Mutex::new(0)),
        }
    }

    /// Reclaims only acknowledged, settled dormant copies. Disk probes and
    /// VCS work run on blocking workers, never under the tree mutex.
    pub async fn reclaim_dormant_checkouts(&self) -> Result<(), TopologyError> {
        let now = plakous::model::now_unix_millis();
        let delay = self.read(|tree| tree.snapshot.workspace_idle_seconds)?;
        {
            let mut next = self
                .next_reclaim_probe
                .lock()
                .map_err(|_| Self::lock_poisoned())?;
            if now < *next {
                return Ok(());
            }
            *next = now.saturating_add(u64::from(delay.min(30)) * 1000);
        }
        let inventory = self.read(ProductAgentTree::dormant_checkouts)?;
        let sizes = tokio::task::spawn_blocking(move || {
            inventory
                .into_iter()
                .filter_map(|(id, path)| directory_bytes(&path).ok().map(|bytes| (id, bytes)))
                .collect::<std::collections::BTreeMap<_, _>>()
        })
        .await
        .map_err(|_| Self::lock_poisoned())?;
        let (owners, effects) = self.mutate(|tree| {
            let snapshot = tree.clone();
            match tree
                .reclaim_dormant(now, &sizes)
                .and_then(|owners| self.save(tree).map(|()| owners))
            {
                Ok(owners) => Ok(owners),
                Err(error) => {
                    *tree = snapshot;
                    Err(error)
                }
            }
        })?;
        let owners = owners?;
        if let Err(error) = self.stop_agent_jobs(&owners).await {
            // No VCS effect was dispatched. Release only process-local
            // claims so the acknowledged intent survives for recovery.
            let _ = self.mutate(ProductAgentTree::abandon_reclamation_dispatch);
            return Err(error);
        }
        let driver = self.clone();
        let settled = tokio::task::spawn_blocking(move || driver.settle(effects))
            .await
            .map_err(|_| Self::lock_poisoned())??;
        if let Some(error) = settled.cleanup_failures.into_iter().next() {
            return Err(error);
        }
        Ok(())
    }

    /// Retires sealed dormant conversations after publishing exact deletion intent.
    /// Unadopted nonempty work refs are retained independently of runtime sessions.
    pub async fn collect_dormant_children(
        &self,
        sessions: &plakous::sessions::AgentSessions,
    ) -> Result<(), TopologyError> {
        let (tickets, _) = self.mutate(|tree| {
            let snapshot = tree.clone();
            match tree
                .prepare_child_retention(plakous::model::now_unix_millis())
                .and_then(|tickets| self.save(tree).map(|()| tickets))
            {
                Ok(tickets) => Ok(tickets),
                Err(error) => {
                    *tree = snapshot;
                    Err(error)
                }
            }
        })?;
        for ticket in tickets? {
            self.stop_agent_jobs(std::slice::from_ref(&ticket.agent_id))
                .await?;
            if let Some(raw) = &ticket.locator {
                let locator: plakous::sessions::SessionLocator = raw.parse().map_err(|_| {
                    TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "retired child locator is invalid; session retained",
                    )
                })?;
                match sessions.delete_archived(locator.session_id()).await {
                    Ok(_) => {}
                    Err(error)
                        if error.kind()
                            == plakous::sessions::AgentSessionsErrorKind::InvalidInput =>
                    {
                        let archived = sessions.archive(locator).await.map_err(|_| TopologyError::new(
                            TopologyErrorCode::OperationRecoveryRequired, "child session retirement refused; exact session and work refs retained"))?;
                        sessions
                            .delete(archived.session_id(), archived.archived_revision())
                            .await
                            .map_err(|_| {
                                TopologyError::new(
                                    TopologyErrorCode::OperationRecoveryRequired,
                                    "child session deletion remains pending; work refs retained",
                                )
                            })?;
                    }
                    Err(_) => {
                        return Err(TopologyError::new(
                            TopologyErrorCode::OperationRecoveryRequired,
                            "child session deletion remains pending; work refs retained",
                        ));
                    }
                }
            }
            let namespace =
                self.read(|tree| plakous::model::sha256_hex(&[tree.tree_id.as_bytes()]))?;
            let launch = self.read(|tree| tree.launch_checkout.clone())?;
            let vcs = self.read(ProductAgentTree::vcs_port)?;
            for candidate in &ticket.pins {
                vcs.unpin(&launch, &namespace, candidate.as_str())
                    .map_err(|_| {
                        TopologyError::new(
                            TopologyErrorCode::WorkspaceRecoveryRequired,
                            "collected child pin cleanup remains pending; other work refs retained",
                        )
                    })?;
            }
            let (finished, _) = self.mutate(|tree| {
                self.fold_and_commit(tree, |tree| tree.finish_child_retention(&ticket))
            })?;
            finished?;
        }
        Ok(())
    }

    pub(crate) fn set_retirement_recovery(&self, recovery: RetirementRecovery) {
        *self
            .retirement_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(recovery);
    }

    /// Requests a pass after caller-layer retirement evidence is published.
    pub fn request_retirement_recovery(&self) {
        let recovery = self
            .retirement_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(recovery) = recovery {
            recovery();
        }
    }

    fn bind_job_host(&self, host: Option<Arc<dyn plakous::tools::JobHost>>) {
        if let Some(host) = host {
            *self
                .job_host
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(host);
        }
    }

    fn job_service(&self, agent: &str) -> Option<plakous::tools::JoinedProcessSessions> {
        let locator = self
            .read(|tree| tree.node(agent).ok()?.session_locator.clone())
            .ok()??;
        let locator: plakous::sessions::SessionLocator = locator.parse().ok()?;
        self.job_host
            .lock()
            .ok()?
            .as_ref()?
            .service(locator.session_id(), locator.branch_id())
    }

    /// Terminal-change edges of one agent's own process service, so a host
    /// that parks the agent wakes on a process exit without polling.
    #[must_use]
    pub fn job_terminal_changes(&self, agent: &str) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.job_service(agent)
            .map(|service| service.job_terminal_changes())
    }

    /// Stops promoted processes before an attempt's terminal seal.
    ///
    /// # Errors
    /// Refuses if exact process cleanup cannot be established.
    pub async fn stop_agent_jobs(&self, ids: &[String]) -> Result<(), TopologyError> {
        let expected = self.read(|tree| {
            ids.iter()
                .map(|id| {
                    (
                        id.clone(),
                        tree.attempt(id).map(|attempt| attempt.attempt_id.clone()),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        })?;
        let mut stops = tokio::task::JoinSet::new();
        for id in ids {
            if let Some(service) = self.job_service(id) {
                let id = id.clone();
                stops.spawn_blocking(move || {
                    let result = service.stop_jobs(
                        plakous::tools::JobStopReason::AttemptEnded,
                        std::time::Duration::from_millis(250),
                    );
                    (id, result)
                });
            }
        }
        let mut failures = Vec::new();
        while let Some(result) = stops.join_next().await {
            match result {
                Ok((_, Ok(()))) => {}
                Ok((id, Err(error))) => failures.push((id, error.to_string())),
                Err(error) => failures.extend(
                    ids.iter()
                        .map(|id| (id.clone(), format!("job stop worker failed: {error}"))),
                ),
            }
        }
        if failures.is_empty() {
            return Ok(());
        }
        let message = failures
            .iter()
            .map(|(id, error)| format!("{id}: {error}"))
            .collect::<Vec<_>>()
            .join("; ");
        let mut tree = self.lock()?;
        let mut recorded = false;
        for (id, error) in failures {
            recorded |= tree
                .record_host_retirement_for_attempt(
                    &id,
                    expected.get(&id).and_then(Option::as_ref),
                    error,
                )
                .and_then(|value| self.apply_recorded(&mut tree, &id).map(|()| value))?;
        }
        self.save(&mut tree)?;
        drop(tree);
        if recorded {
            self.request_retirement_recovery();
        }
        Err(TopologyError::new(
            TopologyErrorCode::OperationRecoveryRequired,
            message,
        ))
    }

    fn lock_poisoned() -> TopologyError {
        TopologyError::new(
            TopologyErrorCode::OperationRecoveryRequired,
            "topology tree lock is poisoned",
        )
    }

    /// The tree's guard, for a read that queues nothing.
    ///
    /// # Errors
    ///
    /// Refuses when the tree mutex is poisoned.
    pub(crate) fn lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, ProductAgentTree>, TopologyError> {
        self.tree.lock().map_err(|_| Self::lock_poisoned())
    }

    /// Reads the tree under its mutex.
    ///
    /// `work` gets `&ProductAgentTree`, so a read cannot queue an effect and
    /// therefore cannot strand one.
    ///
    /// # Errors
    ///
    /// Refuses when the tree mutex is poisoned.
    pub fn read<R>(&self, work: impl FnOnce(&ProductAgentTree) -> R) -> Result<R, TopologyError> {
        let tree = self.lock()?;
        Ok(work(&tree))
    }

    /// Reads the tree without waiting for its mutex.
    ///
    /// `None` when the tree is locked or poisoned — a caller rendering a view
    /// would rather skip a frame than block behind a turn.
    pub fn try_read<R>(&self, work: impl FnOnce(&ProductAgentTree) -> R) -> Option<R> {
        let tree = self.tree.try_lock().ok()?;
        Some(work(&tree))
    }

    /// Raw tree access for fixtures that install or inspect a synthetic tree.
    /// Production consumers use closure mutations and settle their effects.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn tree_handle_for_test(&self) -> &Arc<Mutex<ProductAgentTree>> {
        &self.tree
    }

    /// Locks the tree, applies `work`, and takes the effects it queued.
    pub fn mutate<R>(
        &self,
        work: impl FnOnce(&mut ProductAgentTree) -> R,
    ) -> Result<(R, Effects), TopologyError> {
        let mut tree = self.lock()?;
        let result = work(&mut tree);
        let effects = tree.take_effects();
        Ok((result, effects))
    }

    /// One mutation, settled: applies `work`, then runs everything it queued
    /// with the mutex released and records the result.
    ///
    /// The pairing a caller above this crate should reach for, because the
    /// two halves cannot come apart — [`Self::mutate`] hands the effects out
    /// and only a caller that then drops them can strand one.
    ///
    /// # Errors
    ///
    /// Refuses a poisoned mutex, and reports the first save error the drain
    /// hit after every effect the port ran has been recorded.
    pub fn mutate_and_settle<R>(
        &self,
        work: impl FnOnce(&mut ProductAgentTree) -> R,
    ) -> Result<(R, Settled), TopologyError> {
        let (result, effects) = self.mutate(|tree| {
            let result = work(tree);
            self.save(tree).map(|()| result)
        })?;
        let result = result?;
        let settled = self.settle(effects)?;
        Ok((result, settled))
    }

    /// Runs `effects` against the port with the mutex released, then
    /// re-locks once to record them and publishes the tree. Recording can
    /// owe more, so an acknowledged result save continues the drain. A
    /// refused result save retains follow-on effects without dispatching
    /// them. Every effect already run is recorded before reporting the error.
    /// Destructive removal requires an acknowledged preparation; a refused
    /// preparation retains the copy and records recovery without dispatch.
    pub fn settle(&self, mut effects: Effects) -> Result<Settled, TopologyError> {
        let mut settled = Settled::default();
        let mut first_save_error = None;
        while !effects.is_empty() {
            let refused = {
                let tree = self.lock()?;
                if tree.revision != tree.stored_revision
                    && (!effects.seals.is_empty()
                        || !effects.adopts.is_empty()
                        || !effects.dormancy.is_empty())
                {
                    return Err(TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "seal and adopt effects require an acknowledged revision",
                    ));
                }
                let mut refused = Vec::new();
                effects.unmints.retain(|ticket| {
                    let admitted = tree.revision == tree.stored_revision
                        && ticket.workspace_id.as_deref().is_some_and(|id| {
                            tree.workspace(id).is_some_and(|row| {
                                row.status == crate::protocol::WorkspaceStatus::Discarding
                                    && row.path == ticket.path
                            })
                        })
                        && ticket.operation_id.as_deref().is_none_or(|id| {
                            tree.operations().get(id).is_some_and(|receipt| {
                                receipt.terminal_response.is_none()
                                    && receipt.terminal_code.is_none()
                            })
                        });
                    if !admitted {
                        refused.push(ticket.clone());
                    }
                    admitted
                });
                refused
            };
            let mut completed = effects.run(self.vcs.as_ref());
            completed.refuse_unpublished_unmints(refused);
            let mut tree = self.lock()?;
            let more = tree.complete(completed);
            settled.adopts.extend(more.adopts);
            settled.hook_failures.extend(more.hook_failures);
            settled.seal_failures.extend(more.seal_failures);
            settled.cleanup_failures.extend(more.cleanup_failures);
            tree.lift_restored_pause_if_quiescent();
            if let Err(error) = self.save(&mut tree) {
                first_save_error = Some(error);
                // Completed effects remain recorded. Follow-on reservations
                // stay queued until a later successful publication.
                break;
            }
            effects = tree.take_effects();
        }
        match first_save_error {
            Some(error) => {
                if let Some(seal) = settled.seal_failures.first() {
                    // Leave wire space for both diagnostics and the host's recovery prefix.
                    Err(TopologyError::new(
                        error.code,
                        format!(
                            "{}; seal failure: {}",
                            bounded_text(&error.message, 128),
                            bounded_text(&seal.message, 128),
                        ),
                    ))
                } else {
                    Err(error)
                }
            }
            None => {
                if let Some(error) = settled.cleanup_failures.first() {
                    Err(error.clone())
                } else {
                    Ok(settled)
                }
            }
        }
    }

    /// The port the runtime runs a tree's tickets on, shared. Used for the
    /// mint a `spawn_reserve` owes, which runs with the mutex released.
    pub fn vcs(&self) -> Arc<dyn VcsOps> {
        Arc::clone(&self.vcs)
    }

    pub(crate) fn fold_startup(&self) -> Result<Effects, TopologyError> {
        let mut tree = self.lock()?;
        if tree.revision != tree.stored_revision {
            self.save(&mut tree)?;
        }
        let targets = tree.startup_fold_targets();
        let lift_pause = tree.state == crate::protocol::TreeAdmissionState::RestoredPaused
            && tree.quiescence_refusal().is_none();
        if targets.is_empty() && tree.pending_close_folds.is_empty() && !lift_pause {
            return Ok(tree.take_effects());
        }
        self.fold_and_commit(&mut tree, |tree| {
            for agent in targets {
                tree.fold_due_in_place(&agent)?;
            }
            Ok(())
        })?;
        Ok(tree.take_effects())
    }

    /// Whole-tree publish as the store's compare-and-swap:
    /// refused with `StaleRevision` when another handle published this tree
    /// since it was loaded. Always sent — an operation receipt lands
    /// without a revision bump — and always safe: an unchanged revision is
    /// a compare-and-swap hit only for the handle the store acknowledged.
    pub fn save(&self, tree: &mut ProductAgentTree) -> Result<(), TopologyError> {
        let acknowledged = self.store.save(tree).map_err(TopologyError::from)?;
        tree.acknowledge_revision(acknowledged);
        for terminal in std::mem::take(&mut tree.pending_turn_usages).into_values() {
            if let Some(observer) = &self.observer {
                observer.turn_usage(terminal);
            }
        }
        Ok(())
    }

    /// Applies consequences only to an acknowledged tree, and publishes them
    /// before their effects may be drained. A refusal restores the exact
    /// pre-work snapshot, including the feed cursor and pending usage.
    ///
    /// # Errors
    /// Refuses unacknowledged input, a failed mutation, or a refused save.
    /// `work` must not save: intent publication precedes this boundary.
    pub fn fold_and_commit<R>(
        &self,
        tree: &mut ProductAgentTree,
        work: impl FnOnce(&mut ProductAgentTree) -> Result<R, TopologyError>,
    ) -> Result<R, TopologyError> {
        if tree.revision != tree.stored_revision {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "tree consequences require an acknowledged revision",
            ));
        }
        let snapshot = tree.clone();
        match work(tree).and_then(|result| {
            if tree.state == crate::protocol::TreeAdmissionState::RestoredPaused {
                for agent in tree.startup_fold_targets() {
                    tree.fold_due_in_place(&agent)?;
                }
            }
            tree.fold_reserved_closes_in_place()?;
            tree.refresh_epoch_after_close();
            tree.prune_closed_copies();
            tree.lift_restored_pause_if_quiescent();
            self.save(tree).map(|()| result)
        }) {
            Ok(result) => Ok(result),
            Err(error) => {
                *tree = snapshot;
                Err(error)
            }
        }
    }

    /// Withdraws an unfacted followup in one revision CAS. The caller holds
    /// the child session and startup reservation while reading `tail` and
    /// calling this method; a failed guard or save changes no live rows.
    /// The opening envelope and optional send failure receipt publish with it.
    ///
    /// # Errors
    /// Refuses a changed feed, another intent/effect, or a failed revision CAS.
    pub fn withdraw_unstarted_followup(
        &self,
        agent_id: &str,
        message_id: &str,
        session_id: &str,
        tail: u64,
        receipt: Option<(String, TopologyErrorCode)>,
    ) -> Result<(), TopologyError> {
        let mut tree = self.tree.lock().map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "tree lock is poisoned",
            )
        })?;
        self.fold_and_commit(&mut tree, |tree| {
            tree.withdraw_unstarted_followup(agent_id, message_id, session_id, tail)?;
            settle_followup_refusal(tree, agent_id, message_id, receipt)
        })
    }

    /// Publishes recorded host decisions before projecting their consequences.
    ///
    /// # Errors
    /// A refused intent save leaves the views and effect reservations untouched.
    /// Reports a malformed intent or a refused consequence save.
    pub fn apply_recorded(
        &self,
        tree: &mut ProductAgentTree,
        agent_id: &str,
    ) -> Result<(), TopologyError> {
        self.save_host_intent(tree, agent_id)?;
        self.fold_and_commit(tree, |tree| {
            tree.fold_due_in_place(agent_id)?;
            tree.lift_restored_pause_if_quiescent();
            Ok(())
        })
    }

    fn save_host_intent(
        &self,
        tree: &mut ProductAgentTree,
        agent_id: &str,
    ) -> Result<(), TopologyError> {
        self.save(tree).inspect_err(|error| {
            let reason = tree.fault_reason(agent_id).map_or_else(
                || format!("tree publish refused: {}", error.message),
                |prior| format!("{prior}; tree publish refused: {}", error.message),
            );
            tree.record_fault(agent_id, reason);
        })
    }
}

#[derive(Clone)]
struct TopologyTool {
    driver: TreeDriver,
    caller_id: Arc<Mutex<String>>,
    kind: TopologyKind,
    /// How this node's pack spells the await and interrupt slots.
    vocabulary: TopologyVocabulary,
    binder: Option<Arc<dyn SpawnedSessionBinder>>,
    deliverer: Option<Arc<dyn MailDeliverer>>,
    canceller: Option<Arc<dyn LiveTurnCanceller>>,
    recoverer: Option<Arc<dyn ChildRecoverer>>,
    observer: Option<Arc<dyn TopologyObserver>>,
    /// The tree's per-cell agent budget. Shared by every
    /// `TopologyTool` bound to the same tree, so it survives the per-turn
    /// rebuild of the extension set.
    cell_budget: Arc<CellAgentBudget>,
}

/// What a delivery that reached no turn does with its mail.
enum TurnEndedMail {
    /// The mail stays unread for the running child's park or wake (or it was
    /// already consumed): the send stands.
    Left,
    /// No turn will ever carry the mail: the send is refused.
    Refused(String),
    /// The child has no turn: deliver the mail again as the Start this send
    /// has reserved (an interrupted child's resume, or a Start that found
    /// the slot still starting or retiring).
    Restart,
    /// A turn another command admitted runs: deliver the mail into it as a
    /// follow-up.
    FollowUp,
    /// The sender was cancelled before any redelivery: the mail stays unread
    /// under its original receipt, unless it opened a `followup` attempt
    /// that never started, which is rolled back with the mail settled read
    /// (`TopologyTool::cancel_followup_send`).
    Cancelled(ToolRuntimeError),
    /// An interrupted child's cancelled turn is still bound and no turn
    /// waiter is left to release it: the send fails closed, as a delivery
    /// that found it so does (spec 03 section 9).
    Unwatched,
}

/// The sender's cancellation, polled only where a send waits.
type SendCancelled<'c> = Pin<Box<dyn Future<Output = ()> + Send + 'c>>;

/// What a delivery that came back without reaching a turn found.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Unreached {
    /// A follow-up or steer reached the live turn it targeted after that
    /// turn ended: its waiter parks or wakes a running child with the mail.
    EndedTurn,
    /// A Start found the slot held or vacant of its own turn, or the slot
    /// was still starting or retiring, or the send waited for an unwind or
    /// another send's Start: no turn the mail targeted has ended.
    NoTurn,
}

/// What one look at a recipient the delivery did not reach knows about its
/// send.
#[derive(Clone, Copy)]
struct TurnEndedLook {
    /// The send's budget is spent.
    expired: bool,
    /// The sender was cancelled.
    cancelled: bool,
    /// What the delivery found.
    unreached: Unreached,
}

/// Cancellation is reported after reconciling any followup opening.
fn send_cancelled() -> ToolRuntimeError {
    ToolRuntimeError::new(
        ToolRuntimeErrorKind::Cancelled,
        "send_agent_message was cancelled before delivery was confirmed",
    )
}

/// One send's budget for reaching a turn: an interrupted recipient's
/// unwind, a slot still starting or retiring, and every redelivery.
const TURN_UNWIND_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
/// Safety tick behind the tree notifications while it waits.
const TURN_UNWIND_TICK: std::time::Duration = std::time::Duration::from_secs(1);

struct MailDeliverySnapshot {
    root_id: String,
    agent_id: String,
    session_locator: Option<String>,
    disposition: MailDisposition,
    message_id: String,
    body: String,
    description: String,
    prompt: String,
    output_schema_json: String,
    path: String,
    durable: MailDurableState,
    turn_token: u64,
}

impl ToolExecutor for TopologyTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            if matches!(
                self.kind,
                TopologyKind::TaskOutput
                    | TopologyKind::TaskKill
                    | TopologyKind::JobsWait
                    | TopologyKind::JobsStop
            ) {
                return self
                    .dispatch_task(
                        invocation.call_id().to_string(),
                        invocation.arguments().value().as_ref(),
                        &context,
                        invocation.correlation().caller_lineage(),
                    )
                    .await;
            }
            self.dispatch_cancellable_preempt(
                invocation.correlation().caller_lineage(),
                invocation.call_id().to_string(),
                invocation.arguments().value().as_ref(),
                context.cancellation().cancelled(),
                Some(context.preempt().clone()),
            )
            .await
        })
    }
}

impl TopologyTool {
    #[cfg(test)]
    async fn dispatch_cancellable(
        &self,
        lineage: &ToolCallerLineage,
        operation_id: String,
        value: &Value,
        cancelled: impl Future<Output = ()> + Send,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        self.dispatch_cancellable_preempt(lineage, operation_id, value, cancelled, None)
            .await
    }

    async fn dispatch_cancellable_preempt(
        &self,
        lineage: &ToolCallerLineage,
        operation_id: String,
        value: &Value,
        cancelled: impl Future<Output = ()> + Send,
        preempt: Option<tokio::sync::watch::Receiver<plakous::tools::ToolPreempt>>,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        if matches!(self.kind, TopologyKind::Send) {
            // A send waits only after its mail is durable, so it observes the
            // cancellation itself and reconciles an unknown delivery outcome.
            if let Some(refusal) = self.refuse_over_cell_agent_budget(lineage, value) {
                return refusal;
            }
            return self
                .send_cancellable(operation_id, value, Box::pin(cancelled))
                .await;
        }
        if matches!(self.kind, TopologyKind::Wait) {
            // Wait parks only outside the tree lock. Its ready result and mail
            // acknowledgement are synchronous; effectful dispatches retain
            // their existing owner rather than being dropped on cancellation.
            // `select` polls its left future first, including before a ready
            // wait can acknowledge mail in an already-cancelled context.
            match select(
                Box::pin(cancelled),
                Box::pin(self.dispatch_preempt(lineage, operation_id, value, preempt)),
            )
            .await
            {
                Either::Left(((), _)) => Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Cancelled,
                    "topology wait was cancelled",
                )),
                Either::Right((outcome, _)) => outcome,
            }
        } else {
            self.dispatch_preempt(lineage, operation_id, value, preempt)
                .await
        }
    }

    /// The single admission point for every topology call, from a model
    /// (`Direct`) and from a Code Mode cell (`LocalProgrammatic`) alike.
    ///
    /// The per-cell agent budget is checked **here**, before any kind-specific
    /// work, because `native-ptc`, `js-ptc` and `py-ptc` share this host-call
    /// dispatch: L0's `execute_local_composition_nested` stamps the same
    /// `ToolCallerLineage::LocalProgrammatic` on the invocation whichever
    /// interpreter produced the nested request, so one check binds all three.
    ///
    async fn dispatch(
        &self,
        lineage: &ToolCallerLineage,
        operation_id: String,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        self.dispatch_preempt(lineage, operation_id, value, None)
            .await
    }

    async fn dispatch_preempt(
        &self,
        lineage: &ToolCallerLineage,
        operation_id: String,
        value: &Value,
        preempt: Option<tokio::sync::watch::Receiver<plakous::tools::ToolPreempt>>,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        if let Some(refusal) = self.refuse_over_cell_agent_budget(lineage, value) {
            return refusal;
        }
        match self.kind {
            TopologyKind::Wait => self
                .wait_operation_preempt(&operation_id, value, preempt)
                .await
                .map(|outcome| {
                    let yielded = leave_wait_facts(&operation_id, &outcome);
                    outcome.with_wait_observation(yielded)
                }),
            TopologyKind::Spawn => self.spawn_with(operation_id, value).await,
            TopologyKind::Send => self.send_with(operation_id, value).await,
            TopologyKind::Interrupt => self.interrupt_with(operation_id, value).await,
            TopologyKind::Close => self.close_with(operation_id, value).await,
            TopologyKind::Recover => self.recover_with(value).await,
            TopologyKind::Adopt => self.adopt_with(&operation_id, value),
            TopologyKind::List | TopologyKind::Submit | TopologyKind::Discard => {
                self.run_named(&operation_id, value)
            }
            TopologyKind::TaskOutput
            | TopologyKind::TaskKill
            | TopologyKind::JobsWait
            | TopologyKind::JobsStop => Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "task presentations dispatch with their tool context",
            )),
        }
    }

    /// Grok's output and kill tools on a node granted agent tools. A call
    /// naming only process ids is the process tool's own call, through the
    /// process service's pack-neutral entry points, so its bytes match the
    /// process-only presentation; a call naming a child is the await (or the
    /// interrupt), and its receipt comes back in the topology envelope.
    async fn dispatch_task(
        &self,
        operation_id: String,
        value: &Value,
        context: &ToolContext,
        lineage: &ToolCallerLineage,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        match self.kind {
            TopologyKind::JobsStop => {
                return self.jobs_stop(operation_id, value, context, lineage).await;
            }
            TopologyKind::JobsWait => {
                return self.jobs_wait(operation_id, value, context, lineage).await;
            }
            _ => {}
        }
        if self.kind == TopologyKind::TaskKill {
            let request = match decode::<TaskKillRequest>(value) {
                Ok(request) => request,
                Err(error) => return Self::topology_failure(&error),
            };
            if let Some(number) = task_process_number(&request.task_id) {
                return plakous::tools::ProcessSessionTools::stop_job(context, number).await;
            }
            return self.task_kill_child(operation_id, &request, lineage).await;
        }
        let request = match decode::<TaskOutputRequest>(value) {
            Ok(request) => request,
            Err(error) => return Self::topology_failure(&error),
        };
        if request
            .task_ids
            .iter()
            .all(|id| task_process_number(id).is_some())
        {
            let numbers = request
                .task_ids
                .iter()
                .filter_map(|id| task_process_number(id))
                .collect::<Vec<_>>();
            return plakous::tools::ProcessSessionTools::await_jobs(
                context,
                &numbers,
                request.timeout_ms.unwrap_or(0),
            )
            .await;
        }
        // Same cancellation rule as `wait_agent`: the await parks only
        // outside the tree lock, and a cancelled context wins over ready mail.
        match select(
            Box::pin(context.cancellation().cancelled()),
            Box::pin(self.task_output_with(
                &operation_id,
                &request,
                Some(context.preempt().clone()),
                lineage,
            )),
        )
        .await
        {
            Either::Left(((), _)) => Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "topology wait was cancelled",
            )),
            Either::Right((outcome, _)) => outcome,
        }
    }

    /// The await a Grok output call naming a child runs: those children
    /// plus its named own processes, one receipt kind. Direct calls wrap
    /// that receipt in the topology envelope; a cell (js-ptc, py-ptc,
    /// native-ptc) sees the raw receipt, as `wait_agent` does.
    async fn task_output_with(
        &self,
        operation_id: &str,
        request: &TaskOutputRequest,
        preempt: Option<tokio::sync::watch::Receiver<plakous::tools::ToolPreempt>>,
        lineage: &ToolCallerLineage,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let (processes, agents): (Vec<_>, Vec<_>) = request
            .task_ids
            .iter()
            .partition(|id| task_process_number(id).is_some());
        let awaited = match AwaitRequest::task_output(
            agents.into_iter().cloned().collect(),
            processes
                .iter()
                .filter_map(|id| task_process_number(id))
                .map(|number| format!("p{number}"))
                .collect(),
            request.timeout_ms.unwrap_or(0),
        ) {
            Ok(awaited) => awaited,
            Err(error) => return Self::topology_failure(&error),
        };
        let outcome = self
            .await_operation(operation_id, &awaited, request, preempt)
            .await?;
        let yielded = leave_wait_facts(operation_id, &outcome);
        present_task_receipt(lineage, outcome).map(|outcome| outcome.with_wait_observation(yielded))
    }

    /// Pi's `stop` on a node granted agent tools: a process id is the process
    /// tool's own stop (the same bytes as on a leaf), any other id names a
    /// child and is `interrupt_agent` exactly, never a close.
    async fn jobs_stop(
        &self,
        operation_id: String,
        value: &Value,
        context: &ToolContext,
        lineage: &ToolCallerLineage,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let request = match decode::<JobsStopRequest>(value) {
            Ok(request) => request,
            Err(error) => return Self::topology_failure(&error),
        };
        if let Some(number) = task_process_number(&request.id) {
            return plakous::tools::ProcessSessionTools::stop_job(context, number).await;
        }
        self.task_kill_child(
            operation_id,
            &TaskKillRequest {
                task_id: request.id,
            },
            lineage,
        )
        .await
    }

    /// Pi's `wait` on a node granted agent tools; [`Self::jobs_wait_plan`]
    /// decides between the process tool's own await and the tree's.
    async fn jobs_wait(
        &self,
        operation_id: String,
        value: &Value,
        context: &ToolContext,
        lineage: &ToolCallerLineage,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let request = match decode::<JobsWaitRequest>(value) {
            Ok(request) => request,
            Err(error) => return Self::topology_failure(&error),
        };
        let awaited = match self.jobs_wait_plan(&operation_id, &request) {
            Ok(JobsWaitPlan::Processes(numbers)) => {
                return plakous::tools::ProcessSessionTools::await_first_job(
                    context,
                    &numbers,
                    request.timeout(),
                )
                .await;
            }
            Ok(JobsWaitPlan::Await(awaited)) => awaited,
            Err(error) => return Self::topology_failure(&error),
        };
        // Same cancellation rule as `wait_agent`: the await parks only
        // outside the tree lock, and a cancelled context wins over ready mail.
        match select(
            Box::pin(context.cancellation().cancelled()),
            Box::pin(self.jobs_wait_with(
                &operation_id,
                &awaited,
                &request,
                Some(context.preempt().clone()),
                lineage,
            )),
        )
        .await
        {
            Either::Left(((), _)) => Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "topology wait was cancelled",
            )),
            Either::Right((outcome, _)) => outcome,
        }
    }

    /// What pi's `wait` runs on a node granted agent tools (spec 03 §4.4):
    ///
    /// - ids naming only processes: the process tool's own first-terminal
    ///   await over them, the same bytes as on a leaf;
    /// - ids naming a child: the await over those children and the named
    ///   processes;
    /// - no ids: the process tool's own bare await (every own job still
    ///   listed) when an own job already ended unreported (it returns at once
    ///   and consumes its notice, as on a leaf) or no child is pending;
    ///   otherwise the await over every pending child and every own running
    ///   job, which returns on the first child settle or mail, process end,
    ///   steer or timeout.
    ///
    /// A call id the ledger already holds as an await is that await again,
    /// whatever the live state now says: the same-call replay returns its
    /// retained receipt (a rejoined Direct turn and a re-run cell alike).
    fn jobs_wait_plan(
        &self,
        operation_id: &str,
        request: &JobsWaitRequest,
    ) -> Result<JobsWaitPlan, TopologyError> {
        let timeout_ms = request.timeout();
        let replayed = self.driver.read(|tree| {
            tree.operations()
                .get(operation_id)
                .is_some_and(|receipt| receipt.tool_name == AWAIT_RECEIPT_KIND)
        })?;
        if replayed {
            return Ok(JobsWaitPlan::Await(AwaitRequest {
                agents: AwaitAgents::Pending,
                processes: Vec::new(),
                timeout_ms,
            }));
        }
        if let Some(ids) = request.ids.as_deref() {
            let (processes, agents): (Vec<_>, Vec<_>) =
                ids.iter().partition(|id| task_process_number(id).is_some());
            let numbers = processes
                .iter()
                .filter_map(|id| task_process_number(id))
                .collect::<Vec<_>>();
            if agents.is_empty() {
                return Ok(JobsWaitPlan::Processes(numbers));
            }
            return AwaitRequest::task_output(
                agents.into_iter().cloned().collect(),
                numbers.iter().map(|number| format!("p{number}")).collect(),
                timeout_ms,
            )
            .map(JobsWaitPlan::Await);
        }
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let service = self.driver.job_service(&caller);
        if service
            .as_ref()
            .is_some_and(plakous::tools::JoinedProcessSessions::has_unreported_terminal_job)
        {
            return Ok(JobsWaitPlan::Processes(Vec::new()));
        }
        let running = service.map_or_else(Vec::new, |service| {
            service
                .jobs()
                .into_iter()
                .filter(|job| job.status == plakous::tools::JobStatus::Running)
                .collect()
        });
        let pending = self
            .driver
            .read(|tree| tree.wait_target_ids(&caller, &AwaitAgents::Pending))?
            .is_ok_and(|targets| !targets.is_empty());
        Ok(if pending {
            JobsWaitPlan::Await(AwaitRequest {
                agents: AwaitAgents::Pending,
                processes: running.iter().map(job_id).collect(),
                timeout_ms,
            })
        } else {
            JobsWaitPlan::Processes(Vec::new())
        })
    }

    /// The tree's await a pi `wait` over children runs, under the one await
    /// receipt kind: in the topology envelope on Direct calls, raw in a cell.
    async fn jobs_wait_with(
        &self,
        operation_id: &str,
        awaited: &AwaitRequest,
        request: &JobsWaitRequest,
        preempt: Option<tokio::sync::watch::Receiver<plakous::tools::ToolPreempt>>,
        lineage: &ToolCallerLineage,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let outcome = self
            .await_operation(operation_id, awaited, request, preempt)
            .await?;
        let yielded = leave_wait_facts(operation_id, &outcome);
        present_task_receipt(lineage, outcome).map(|outcome| outcome.with_wait_observation(yielded))
    }

    /// A Grok kill naming a child: `interrupt_agent` exactly (its receipt
    /// kind and request), never a close. The topology envelope is Direct only.
    async fn task_kill_child(
        &self,
        operation_id: String,
        request: &TaskKillRequest,
        lineage: &ToolCallerLineage,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let interrupted = self
            .interrupt_with(operation_id, &json!({ "agent_id": request.task_id }))
            .await?;
        present_task_receipt(lineage, interrupted)
    }

    /// Spends one unit of the caller cell's agent budget, or refuses.
    ///
    /// A direct model call has no cell identity and is never budgeted, so the
    /// root agent's own `spawn_agent` is unaffected. The refusal is an ordinary
    /// tool failure, so the cell observes it as a failed call and can keep
    /// running.
    ///
    /// The check runs before `prepare_operation`, so an operation the tree
    /// would have *replayed* still spends a unit. That costs nothing today —
    /// L0's `CompositionChild` record is not replay-complete, so no nested
    /// request is ever re-offered. Any future replay support
    /// must define whether replay spends a unit.
    fn refuse_over_cell_agent_budget(
        &self,
        lineage: &ToolCallerLineage,
        value: &Value,
    ) -> Option<Result<ToolOutcome, ToolRuntimeError>> {
        if !self.consumes_cell_agent_budget_for(value) {
            return None;
        }
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let cell = CellIdentity::from_lineage(lineage, &caller)?;
        self.cell_budget
            .admit_agent(&cell)
            .err()
            .map(|exhausted| ToolOutcome::failure_text(exhausted.to_failure_json().to_string()))
    }

    /// Whether *this* call spends a unit of the cell's agent budget.
    ///
    ///
    /// Beyond the kinds that always do, a `send_agent_message` carrying
    /// `purpose=followup` does: it re-tasks an idle finished child on a fresh
    /// attempt, which is a new agent turn the cell asked for. Ordinary mail on
    /// an already-running child stays free, so an exhausted cell can still
    /// steer, await, and close what it has. A request that does not decode is
    /// not charged; the kind's own handler is what reports the shape error.
    fn consumes_cell_agent_budget_for(&self, value: &Value) -> bool {
        if self.kind.consumes_cell_agent_budget() {
            return true;
        }
        matches!(self.kind, TopologyKind::Send)
            && decode::<SendAgentMessageRequest>(value)
                .is_ok_and(|request| request.purpose == Some(MessagePurpose::Followup))
    }

    fn running_jobs(&self, caller: &str) -> Vec<String> {
        self.driver
            .job_service(caller)
            .map_or_else(Vec::new, |service| {
                service
                    .jobs()
                    .into_iter()
                    .filter(|job| job.status == plakous::tools::JobStatus::Running)
                    .map(|job| format!("p{}", job.number))
                    .collect()
            })
    }

    fn run_named(
        &self,
        operation_id: &str,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        if self.kind == TopologyKind::Submit {
            let running = self.running_jobs(&caller);
            let (children, remedies) = self
                .driver
                .read(|tree| {
                    (
                        tree.live_child_ids(&caller),
                        tree.compiled_tool_names(&caller)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|name| {
                                matches!(*name, "wait_agent" | "close_agent" | "interrupt_agent")
                            })
                            .map(|name| self.vocabulary.presented_name(name))
                            .collect::<Vec<_>>(),
                    )
                })
                .map_err(|error| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                })?;
            if !running.is_empty() || !children.is_empty() {
                let mut work = Vec::new();
                if !children.is_empty() {
                    work.push(format!(
                        "children {} (finish them with {})",
                        children.join(", "),
                        remedies.join(", ")
                    ));
                }
                if !running.is_empty() {
                    work.push(format!(
                        "processes {} (finish or stop them with your process tools)",
                        running.join(", ")
                    ));
                }
                return Self::topology_failure(&TopologyError::new(
                    TopologyErrorCode::LiveWork,
                    format!(
                        "live work: {}; finish or stop it before submitting",
                        work.join("; ")
                    ),
                ));
            }
        }
        if self.kind == TopologyKind::List {
            return match self.list_agents(&caller, value) {
                Ok(payload) => ToolOutcome::success_text(payload.to_string()),
                Err(error) => Self::topology_failure(&error),
            };
        }
        let (result, effects) = self
            .driver
            .mutate(|tree| match self.kind {
                TopologyKind::Spawn
                | TopologyKind::Wait
                | TopologyKind::Send
                | TopologyKind::Interrupt
                | TopologyKind::Close
                | TopologyKind::Recover
                | TopologyKind::TaskOutput
                | TopologyKind::TaskKill
                | TopologyKind::JobsWait
                | TopologyKind::JobsStop => Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "async topology kind must not use the sync dispatch path",
                )),
                TopologyKind::List => Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "list_agents uses the split occupancy path",
                )),
                TopologyKind::Submit => decode::<SubmitResultRequest>(value).and_then(|request| {
                    self.dispatch_mutating(
                        tree,
                        &caller,
                        operation_id,
                        "submit_result",
                        &request,
                        |tree, caller, request, operation_id| {
                            let vocabulary = self.vocabulary;
                            tree.submit_result_presented(caller, request, operation_id, |slot| {
                                vocabulary.presented_name(slot)
                            })
                        },
                    )
                    .map(|(output, _replayed)| json!(output))
                }),
                TopologyKind::Adopt => Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "adopt_candidate uses the split dispatch path",
                )),
                TopologyKind::Discard => decode::<WorkspaceIdRequest>(value).and_then(|request| {
                    self.dispatch_mutating(
                        tree,
                        &caller,
                        operation_id,
                        "discard_workspace",
                        &request,
                        |tree, caller, request, operation_id| {
                            tree.discard_workspace(caller, request, operation_id)
                        },
                    )
                    .map(|(output, _replayed)| json!(output))
                }),
            })
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        if let Err(error) = self.driver.settle(effects) {
            return Self::topology_failure(&error);
        }
        if self.kind == TopologyKind::Discard && result.is_ok() {
            let tree = self.driver.lock().map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
            let Some(receipt) = tree
                .operations()
                .get(operation_id)
                .and_then(|receipt| receipt.terminal_response.as_ref())
            else {
                return Self::topology_failure(&TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "discard did not publish a completed removal receipt",
                ));
            };
            return ToolOutcome::success_text(receipt.clone());
        }
        match result {
            Ok(payload) => ToolOutcome::success_text(payload.to_string()),
            Err(error) => ToolOutcome::failure_text(
                json!({
                    "code": error.code,
                    "message": error.message,
                })
                .to_string(),
            ),
        }
    }

    /// `adopt_candidate` in three phases: authority and the
    /// `Adopting` mark under the lock, the `merge-tree`/`apply` (or jj
    /// rebase/squash) subprocesses with the lock released, then the receipt
    /// under the lock again. Every other tool call and TUI read proceeds while
    /// the landing runs.
    fn adopt_with(
        &self,
        operation_id: &str,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let running = self.running_jobs(&caller);
        let warning = if running.is_empty() {
            String::new()
        } else {
            format!(
                "running processes {} still use this checkout",
                running.join(", ")
            )
        };
        let request = match decode::<AdoptCandidateRequest>(value) {
            Ok(request) => request,
            Err(error) => return Self::topology_failure(&error),
        };
        let lock_poisoned = || {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "topology tree lock is poisoned",
            )
        };
        let effects = {
            let mut tree = self.driver.tree.lock().map_err(|_| lock_poisoned())?;
            let prepared = tree
                .prepare_operation(operation_id, &caller, "adopt_candidate", &request)
                .and_then(|prior| match prior {
                    Some(prior) => Ok(Some(prior)),
                    None => self.driver.save(&mut tree).map(|()| None),
                });
            match prepared {
                Ok(Some(prior)) => {
                    if let Some(files_changed) = serde_json::from_str::<Value>(&prior)
                        .ok()
                        .and_then(|receipt| receipt.get("files_changed")?.as_u64())
                        .and_then(|files| u32::try_from(files).ok())
                    {
                        crate::call_facts::leave(
                            operation_id,
                            crate::call_facts::TopologyCallFacts::Adopted { files_changed },
                        );
                    }
                    return ToolOutcome::success_text(prior);
                }
                Ok(None) => {}
                Err(error) => return Self::topology_failure(&error),
            }
            if let Err(error) = self
                .driver
                .fold_and_commit(&mut tree, |tree| tree.begin_adopt(&caller, &request))
            {
                // Prepare persisted a non-terminal receipt; close it.
                if let Err(persist) = tree
                    .remember_failure(
                        operation_id,
                        &caller,
                        "adopt_candidate",
                        &request,
                        error.code,
                    )
                    .and_then(|()| self.driver.save(&mut tree))
                {
                    return Self::topology_failure(&persist);
                }
                return Self::topology_failure(&error);
            }
            tree.take_effects()
        };
        let landed = match self.driver.settle(effects) {
            Ok(settled) => settled.adopts.into_iter().next().unwrap_or_else(|| {
                Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "the adopt ticket was not completed",
                ))
            }),
            Err(error) => return Self::topology_failure(&error),
        };
        let mut tree = self.driver.tree.lock().map_err(|_| lock_poisoned())?;
        let conflict = tree.take_adopt_conflict(request.candidate_id.as_str());
        match landed {
            Ok(outcome) => {
                crate::call_facts::leave(
                    operation_id,
                    crate::call_facts::TopologyCallFacts::Adopted {
                        files_changed: outcome.files_changed,
                    },
                );
                let mut outcome = json!(outcome);
                if !warning.is_empty() {
                    outcome["warning"] = json!(warning);
                }
                if let Err(error) = tree
                    .remember_success(operation_id, &caller, "adopt_candidate", &request, &outcome)
                    .and_then(|()| self.driver.save(&mut tree))
                {
                    return Self::topology_failure(&error);
                }
                ToolOutcome::success_text(outcome.to_string())
            }
            Err(error) => {
                if let Some(paths) =
                    conflict.filter(|_| error.code == TopologyErrorCode::MergeConflict)
                {
                    crate::call_facts::leave(
                        operation_id,
                        crate::call_facts::TopologyCallFacts::AdoptConflict { paths },
                    );
                }
                if let Err(persist) = tree
                    .remember_failure(
                        operation_id,
                        &caller,
                        "adopt_candidate",
                        &request,
                        error.code,
                    )
                    .and_then(|()| self.driver.save(&mut tree))
                {
                    return Self::topology_failure(&persist);
                }
                Self::topology_failure(&error)
            }
        }
    }

    /// `list_agents` in three phases: the visible rows and
    /// the paths they occupy under the lock, the `git status` / `jj log`
    /// probes with the lock released, then the render under the lock again
    /// against that snapshot. The occupancy is observational — a copy that
    /// changed between the two phases renders as the probe saw it — so a
    /// listing never holds the tree while a subprocess runs.
    fn list_agents(&self, caller: &str, value: &Value) -> Result<Value, TopologyError> {
        let request = decode::<ListAgentsRequest>(value)?;
        let probe = {
            let tree = self.driver.lock()?;
            tree.occupancy_probe(caller, &request)?
        };
        let occupancy = probe.run(self.driver.vcs().as_ref());
        let tree = self.driver.lock()?;
        tree.list_agents_with(caller, &request, &occupancy)
            .map(|output| json!(output))
    }

    fn topology_failure(error: &TopologyError) -> Result<ToolOutcome, ToolRuntimeError> {
        ToolOutcome::failure_text(
            json!({
                "code": error.code,
                "message": error.message,
            })
            .to_string(),
        )
    }

    /// Runs an effects batch with the mutex released. A batch carrying an
    /// `on_mint` hook can run for minutes, so it goes to a blocking thread;
    /// seals, adopts, and unmints are short and run inline.
    async fn settle(&self, effects: Effects) -> Result<Settled, ToolRuntimeError> {
        let settled = if effects.hooks.is_empty() && effects.dormancy.is_empty() {
            self.driver.settle(effects)
        } else {
            let driver = self.driver.clone();
            tokio::task::spawn_blocking(move || driver.settle(effects))
                .await
                .unwrap_or_else(|error| {
                    Err(TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        format!("effects task failed: {error}"),
                    ))
                })
        };
        settled
            .map_err(|error| ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message))
    }

    /// `spawn_agent` in three phases, the same shape the
    /// adopt uses: admission, the reserved child id, and the prepared
    /// operation under the lock; the mint — a `git worktree add` or a
    /// `jj workspace add`, seconds of subprocess — with the lock released;
    /// the node, attempt, grant, and workspace rows and the receipt under the
    /// lock again. Every other tool call and TUI read proceeds while the mint
    /// runs, and until it answers no row names the reserved id, so a wait, a
    /// followup, or a close for it is refused exactly as one for an id that
    /// was never spawned.
    async fn spawn_admit(
        &self,
        caller: &str,
        operation_id: &str,
        request: &SpawnAgentRequest,
    ) -> Result<(Result<(SpawnAgentOutput, bool), TopologyError>, Effects), ToolRuntimeError> {
        let (phase, effects) = self
            .driver
            .mutate(|tree| self.spawn_reserve_locked(tree, caller, operation_id, request))
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        let (reservation, ticket) = match phase {
            Ok(SpawnPhase::Done(output, replayed)) => {
                return Ok((Ok((output, replayed)), effects));
            }
            Ok(SpawnPhase::Minting(reservation, ticket)) => (reservation, ticket),
            Err(error) => return Ok((Err(error), effects)),
        };
        let port = self.driver.vcs();
        let minted = tokio::task::spawn_blocking(move || ticket.mint(port.as_ref()))
            .await
            .unwrap_or_else(|error| {
                Err(VcsError::mint_failed(format!("mint task failed: {error}")))
            });
        let (result, more) = self
            .driver
            .mutate(|tree| match minted {
                Ok(minted) => match tree.spawn_commit(*reservation, Some(minted)) {
                    Ok(output) => {
                        self.driver.save(tree)?;
                        Ok((output, false))
                    }
                    Err(error) => {
                        tree.remember_failure(
                            operation_id,
                            caller,
                            "spawn_agent",
                            request,
                            error.code,
                        )?;
                        self.driver.save(tree)?;
                        Err(error)
                    }
                },
                Err(error) => {
                    let error = tree.spawn_abort(*reservation, topology_error_from_vcs(error));
                    self.driver.save(tree)?;
                    Err(error)
                }
            })
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        let mut effects = effects;
        effects.absorb(more);
        Ok((result, effects))
    }

    /// The locked half of [`Self::spawn_admit`]: the operation ledger and the
    /// admission checks, with no subprocess.
    fn spawn_reserve_locked(
        &self,
        tree: &mut ProductAgentTree,
        caller: &str,
        operation_id: &str,
        request: &SpawnAgentRequest,
    ) -> Result<SpawnPhase, TopologyError> {
        if let Some(prior) = tree.prepare_operation(operation_id, caller, "spawn_agent", request)? {
            let output = serde_json::from_str(&prior).map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "prior topology receipt could not be reconstructed",
                )
            })?;
            return Ok(SpawnPhase::Done(output, true));
        }
        self.driver.save(tree)?;
        let admitted = tree.spawn_reserve(caller, request, Some(operation_id));
        match admitted {
            Ok(SpawnAdmission::Replayed(output)) => {
                self.driver.save(tree)?;
                Ok(SpawnPhase::Done(output, false))
            }
            // An `inherit` bind mints nothing, so its rows and its receipt
            // land in this same lock.
            Ok(SpawnAdmission::Reserved(reservation, None)) => {
                match tree.spawn_commit(*reservation, None) {
                    Ok(output) => {
                        self.driver.save(tree)?;
                        Ok(SpawnPhase::Done(output, false))
                    }
                    Err(error) => {
                        tree.remember_failure(
                            operation_id,
                            caller,
                            "spawn_agent",
                            request,
                            error.code,
                        )?;
                        self.driver.save(tree)?;
                        Err(error)
                    }
                }
            }
            Ok(SpawnAdmission::Reserved(reservation, Some(ticket))) => {
                Ok(SpawnPhase::Minting(reservation, ticket))
            }
            Err(error) => {
                // Prepare already persisted a non-terminal receipt. Failures
                // must close it or reopen treats the op as RecoveryRequired.
                tree.remember_failure(operation_id, caller, "spawn_agent", request, error.code)?;
                self.driver.save(tree)?;
                Err(error)
            }
        }
    }

    async fn spawn_with(
        &self,
        operation_id: String,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let request = match decode::<SpawnAgentRequest>(value) {
            Ok(request) => request,
            Err(error) => {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
        };
        // A completed operation is already authorized by its exact durable
        // receipt. Account readiness may change before replay, so inspect the
        // recorded request before consulting the current account state.
        let replay = {
            let mut tree = self.driver.tree.lock().map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?;
            if let Some(prior) = tree.operations().get(&operation_id) {
                if prior.caller_agent_id != caller {
                    return Self::topology_failure(&TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "retry caller conflicts with the prepared operation",
                    ));
                }
                Some(tree.prepare_operation(&operation_id, &caller, "spawn_agent", &request))
            } else {
                None
            }
        };
        let replay = match replay {
            Some(Ok(Some(prior))) => {
                let output = serde_json::from_str::<SpawnAgentOutput>(&prior).map_err(|_| {
                    TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "prior topology receipt could not be reconstructed",
                    )
                });
                let output = match output {
                    Ok(output) => output,
                    Err(error) => return Self::topology_failure(&error),
                };
                Some(output)
            }
            Some(Err(error)) => return Self::topology_failure(&error),
            Some(Ok(None)) => {
                return Self::topology_failure(&TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "prepared spawn operation has no terminal receipt yet",
                ));
            }
            None => None,
        };
        // The caller's menu is checked under the tree lock, before either
        // binder admission. Product binders re-lock the hub snapshot, so the
        // lock is released before `admit_spawn` / `admit_binding`; holding it
        // across those calls deadlocks live spawn.
        if replay.is_none()
            && let Some(binder) = &self.binder
        {
            let binding = {
                let tree = self.driver.tree.lock().map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "topology tree lock is poisoned",
                    )
                })?;
                let caller_template = match tree.node(&caller) {
                    Ok(node) => node.template.clone(),
                    Err(error) => return Self::topology_failure(&error),
                };
                if tree.snapshot.binding(&request.template).is_none() {
                    return Self::topology_failure(&TopologyError::new(
                        TopologyErrorCode::TemplateUnknown,
                        format!(
                            "template {} is not in this tree epoch's roster",
                            request.template
                        ),
                    ));
                }
                if !tree.snapshot.menu_contains(
                    caller_template.as_ref().map(TemplateName::as_str),
                    &request.template,
                ) {
                    return Self::topology_failure(&TopologyError::new(
                        TopologyErrorCode::TemplateNotSpawnable,
                        format!(
                            "template {} is not on this node's spawn menu: the root spawns the working \
                             set, every other node its own template's spawns list minus itself",
                            request.template
                        ),
                    ));
                }
                tree.snapshot.binding(&request.template).cloned()
            };
            if let Err(error) = binder.admit_spawn(&request) {
                return Self::topology_failure(&error);
            }
            if let Some(binding) = binding
                && let Err(error) = binder.admit_binding(&request.template, &binding).await
            {
                return Self::topology_failure(&error);
            }
        }
        let (output, effects) = if let Some(output) = replay {
            (Ok((output, true)), Effects::default())
        } else {
            self.spawn_admit(&caller, &operation_id, &request).await?
        };
        let (output, replayed) = match output {
            Ok(output) => output,
            Err(error) => {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
        };
        // the hook runs once per mint, after the copy exists and
        // with the lock released, before any session is bound to the copy. A
        // non-zero exit or a timeout unminted the copy and failed the spawn
        // closed under the lock that recorded it.
        let settled = self.settle(effects).await?;
        if let Some(error) = settled.hook_failures.into_iter().next() {
            return Self::topology_failure(&error);
        }
        if !replayed {
            let tree = self.driver.tree.lock().map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?;
            if let Some(observer) = self.observer.as_ref() {
                observer.node_spawned(
                    &tree.tree_id,
                    &output.agent_id,
                    Some(caller.as_str()),
                    request.template.as_str(),
                );
            }
        }
        let already_bound = {
            let tree = self.driver.tree.lock().map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?;
            if replayed {
                let node = tree.node(&output.agent_id).map_err(|error| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                })?;
                let unresolved = tree.attempt(&output.agent_id).is_some_and(|attempt| {
                    attempt.has_unresolved_session_failure()
                        || (node.active_turn && attempt.turn_id.is_none())
                });
                if unresolved {
                    return ToolOutcome::failure_text(json!({ "code": TopologyErrorCode::OperationRecoveryRequired, "message": "child startup or session outcome remains unresolved; exact replay cannot dispatch another start" }).to_string());
                }
            }
            tree.node(&output.agent_id)
                .ok()
                .and_then(|node| node.session_locator.clone())
        };
        if already_bound.is_some() {
            let mut tree = self.driver.tree.lock().map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?;
            if let Err(error) = tree.record_session_bound_after_failure(&output.agent_id) {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
            if let Err(error) = self.driver.save(&mut tree) {
                drop(tree);
                return self.fail_closed_session(
                    &output.agent_id,
                    format!("bound locator could not be persisted: {}", error.message),
                );
            }
            if let Err(error) = self.driver.fold_and_commit(&mut tree, |tree| {
                tree.fold_due_in_place(&output.agent_id).map(|_| ())
            }) {
                return Self::topology_failure(&error);
            }
            return ToolOutcome::success_text(json!(output).to_string());
        }
        if let Some(binder) = &self.binder {
            let start = {
                let tree = self.driver.tree.lock().map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "topology tree lock is poisoned",
                    )
                })?;
                let node = tree.node(&output.agent_id).map_err(|error| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                })?;
                SpawnStart {
                    spawned: output.clone(),
                    description: node.description.clone(),
                    prompt: node.prompt.clone(),
                    output_schema_json: node.output_schema_json.clone(),
                    turn_token: tree
                        .attempt(&output.agent_id)
                        .map(|attempt| attempt.turn_token)
                        .unwrap_or(1),
                    path: node.path.display().to_string(),
                    tier: node.tier,
                    account: node.account.clone(),
                    tuple_id: node.tuple_id.clone(),
                    effort: node.effort.clone(),
                }
            };
            match binder.bind(start).await {
                Ok(bound) => {
                    if bound.durably_registered {
                        return ToolOutcome::success_text(json!(output).to_string());
                    }
                    let mut tree = self.driver.tree.lock().map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "topology tree lock is poisoned",
                        )
                    })?;
                    let bind = match tree
                        .node(&output.agent_id)
                        .ok()
                        .and_then(|node| node.session_locator.as_deref())
                    {
                        Some(existing) if existing == bound.session_locator => {
                            tree.record_child_session(&output.agent_id, &bound.session_locator)
                        }
                        Some(_) => Err(TopologyError::new(
                            TopologyErrorCode::OperationRecoveryRequired,
                            "bound session locator differs from startup publication",
                        )),
                        None => tree.record_child_session(&output.agent_id, &bound.session_locator),
                    };
                    if let Err(error) = bind {
                        return ToolOutcome::failure_text(
                            json!({ "code": error.code, "message": error.message }).to_string(),
                        );
                    }
                    if let Err(error) = self.driver.save(&mut tree) {
                        drop(tree);
                        return self.fail_closed_session(
                            &output.agent_id,
                            format!(
                                "bound session intent could not be persisted: {}",
                                error.message
                            ),
                        );
                    }
                    let locator = bound
                        .session_locator
                        .parse::<plakous::sessions::SessionLocator>()
                        .map_err(|_| {
                            ToolRuntimeError::new(
                                ToolRuntimeErrorKind::Execution,
                                "bound session allocation is invalid",
                            )
                        })?;
                    if let Err(error) = tree.project_feed(
                        &output.agent_id,
                        &locator.session_id().to_string(),
                        &[],
                        &crate::tree::TreeFeedReads::default(),
                    ) {
                        return Self::topology_failure(&TopologyError::from(error));
                    }
                    if let Some(turn_id) = bound.turn_id {
                        tree.bind_turn_owner(&output.agent_id, turn_id);
                    }
                    if let Err(error) = self.driver.save(&mut tree) {
                        drop(tree);
                        return self.fail_closed_session(
                            &output.agent_id,
                            format!("bound locator could not be persisted: {}", error.message),
                        );
                    }
                    return ToolOutcome::success_text(json!(output).to_string());
                }
                Err(SpawnBindError::Refused(message)) => {
                    let (persisted, effects) = self
                        .driver
                        .mutate(|tree| {
                            let error = tree.fail_spawn_refused(
                                &caller,
                                &request,
                                Some(&operation_id),
                                &output.agent_id,
                                &message,
                            );
                            let status = tree
                                .node(&output.agent_id)
                                .ok()
                                .and_then(|node| NodeDetach::from_node_status(node.status))
                                .unwrap_or(NodeDetach::RecoveryRequired);
                            tree.try_emit_tree_node_detached(
                                self.observer.as_ref(),
                                &output.agent_id,
                                status,
                            );
                            self.driver.save(tree).map(|()| error)
                        })
                        .map_err(|error| {
                            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                        })?;
                    self.settle(effects).await?;
                    return match persisted {
                        Ok(error) | Err(error) => Self::topology_failure(&error),
                    };
                }
                Err(SpawnBindError::RecoveryRequired { stage, message }) => {
                    let (persisted, effects) = self
                        .driver
                        .mutate(|tree| {
                            let recorded = tree
                                .record_host_retirement_for_attempt(
                                    &output.agent_id,
                                    Some(&output.attempt_id),
                                    format!("startup {stage:?}: {message}"),
                                )
                                .and_then(|value| {
                                    self.driver
                                        .apply_recorded(tree, &output.agent_id)
                                        .map(|()| value)
                                })?;
                            self.driver.save(tree)?;
                            Ok::<_, TopologyError>(recorded)
                        })
                        .map_err(|error| {
                            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                        })?;
                    self.driver.settle(effects).map_err(|error| {
                        ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                    })?;
                    match persisted {
                        Err(error) => return Self::topology_failure(&error),
                        Ok(true) => self.driver.request_retirement_recovery(),
                        Ok(false) => {}
                    }
                    return ToolOutcome::failure_text(json!({ "code": TopologyErrorCode::OperationRecoveryRequired, "stage": format!("{stage:?}"), "message": message }).to_string());
                }
            }
        }
        ToolOutcome::success_text(json!(output).to_string())
    }

    async fn send_with(
        &self,
        operation_id: String,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        self.send_cancellable(operation_id, value, Box::pin(std::future::pending()))
            .await
    }

    /// Sends one message and delivers it live when the recipient can take it.
    /// `cancelled` is the sender's cancellation. It is observed only where
    /// the send waits (behind a parked wait on the mail, or for an
    /// interrupted recipient's turn to unwind), never mid-delivery, and a
    /// cancelled send leaves its mail unread under its original receipt.
    async fn send_cancellable(
        &self,
        operation_id: String,
        value: &Value,
        mut cancelled: SendCancelled<'_>,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        if let Ok(request) = decode::<SendAgentMessageRequest>(value)
            && request.purpose == Some(MessagePurpose::Followup)
        {
            let (prepared, effects) = self
                .driver
                .mutate(|tree| {
                    let result = tree.rehydrate_followup(&caller, &request, &operation_id);
                    self.driver.save(tree).and(result)
                })
                .map_err(|e| ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, e.message))?;
            if let Err(error) = prepared {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
            let settled = self.settle(effects).await?;
            if let Some(error) = settled.cleanup_failures.first() {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
        }
        let (output, effects) = self
            .driver
            .mutate(|tree| {
                decode::<SendAgentMessageRequest>(value).and_then(|request| {
                    self.dispatch_mutating(
                        tree,
                        &caller,
                        &operation_id,
                        "send_agent_message",
                        &request,
                        |tree, caller, request, operation_id| {
                            tree.send_agent_message(caller, request, operation_id)
                        },
                    )
                    .map(|(output, _replayed)| output)
                })
            })
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        self.settle(effects).await?;
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
        };
        let Some(deliverer) = &self.deliverer else {
            return ToolOutcome::success_text(json!(output).to_string());
        };
        let snapshot = loop {
            let notify = {
                let mut tree = self.driver.tree.lock().map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "topology tree lock is poisoned",
                    )
                })?;
                let Some(mail) = tree.mail(&output.message_id) else {
                    // Only acknowledged history is pruned; its durable send
                    // receipt remains replayable without delivering it again.
                    return ToolOutcome::success_text(json!(output).to_string());
                };
                // Root and parked-child mail waits for the host's next
                // admission of that node; it is never delivered live.
                if mail.to == tree.root_agent_id
                    || mail.durable != MailDurableState::Accepted
                    || tree.delivery_claims.contains(&mail.message_id)
                    || tree.is_parked(&mail.to)
                {
                    return ToolOutcome::success_text(json!(output).to_string());
                }
                if tree.parked_waits.iter().any(|((recipient, _), targets)| {
                    recipient == &mail.to && targets.contains(&mail.from)
                }) || (output.disposition == MailDisposition::Started
                    && tree
                        .start_reservations
                        .get(&mail.to)
                        .is_some_and(|message| message != &mail.message_id))
                {
                    // A cancelled or timed-out wait may not consume this mail,
                    // and another send's Start to the same child is in
                    // flight. Subscribe under the ownership lock and recheck
                    // on release.
                    let mut notified = Box::pin(tree.waiters().notified_owned());
                    notified.as_mut().enable();
                    notified
                } else {
                    let node = tree.node(&mail.to).map_err(|error| {
                        ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                    })?;
                    let snapshot = MailDeliverySnapshot {
                        root_id: tree.root_agent_id.clone(),
                        agent_id: node.agent_id.clone(),
                        session_locator: node.session_locator.clone(),
                        disposition: output.disposition,
                        message_id: output.message_id.clone(),
                        body: mail.body.clone(),
                        description: node.description.clone(),
                        prompt: node.prompt.clone(),
                        output_schema_json: node.output_schema_json.clone(),
                        path: node.path.display().to_string(),
                        durable: mail.durable,
                        turn_token: tree
                            .attempt(&node.agent_id)
                            .map(|attempt| attempt.turn_token)
                            .unwrap_or(1),
                    };
                    tree.delivery_claims.insert(output.message_id.clone());
                    if output.disposition == MailDisposition::Started {
                        tree.start_reservations
                            .insert(snapshot.agent_id.clone(), output.message_id.clone());
                    }
                    break snapshot;
                }
            };
            if let Either::Left(((), _)) = select(cancelled.as_mut(), notify).await {
                let agent = decode::<SendAgentMessageRequest>(value)
                    .map_err(|error| {
                        ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                    })?
                    .agent_id;
                self.cancel_followup_send(&operation_id, &agent, &output.message_id)
                    .await?;
                return Err(send_cancelled());
            }
        };
        let _delivery = MailConsumerGuard {
            tree: Arc::clone(&self.driver.tree),
            consumer: MailConsumer::Delivery(output.message_id.clone()),
        };
        // A send's Starts to one child are exclusive: the reservation lasts
        // until the send returns.
        let mut start =
            (snapshot.disposition == MailDisposition::Started).then(|| MailConsumerGuard {
                tree: Arc::clone(&self.driver.tree),
                consumer: MailConsumer::Start {
                    agent: snapshot.agent_id.clone(),
                    message: snapshot.message_id.clone(),
                },
            });
        if snapshot.agent_id == snapshot.root_id {
            return ToolOutcome::success_text(json!(output).to_string());
        }
        if snapshot.durable != MailDurableState::Accepted {
            return ToolOutcome::success_text(json!(output).to_string());
        }
        let Some(session_locator) = snapshot.session_locator.clone() else {
            return self.fail_closed_session(
                &snapshot.agent_id,
                "child session locator is missing; spawn bind did not complete".into(),
            );
        };
        let mut snapshot = snapshot;
        let mut output = output;
        // One budget for the whole send, however often it redelivers.
        let deadline = tokio::time::Instant::now() + TURN_UNWIND_WAIT;
        // Set once the send is delivering its mail again (a Start it reserved,
        // or a follow-up into a turn another command admitted). The recorded
        // receipt changes only when that delivery is admitted.
        let mut redelivered = false;
        loop {
            if cancelled.as_mut().now_or_never().is_some() {
                self.cancel_followup_send(&operation_id, &snapshot.agent_id, &snapshot.message_id)
                    .await?;
                return Err(send_cancelled());
            }
            // The budget bounds waiting for a turn, never a submission in
            // flight: cutting one would fence an attempt whose turn may be
            // running. The delivery is bounded by its own reopen and admission.
            let delivery = Box::pin(deliverer.deliver(MailDeliveryRequest {
                agent_id: snapshot.agent_id.clone(),
                session_locator: session_locator.clone(),
                disposition: snapshot.disposition,
                message_id: snapshot.message_id.clone(),
                body: snapshot.body.clone(),
                description: snapshot.description.clone(),
                prompt: snapshot.prompt.clone(),
                output_schema_json: snapshot.output_schema_json.clone(),
                path: snapshot.path.clone(),
                turn_token: snapshot.turn_token,
            }));
            let answer = match select(cancelled.as_mut(), delivery).await {
                Either::Left(_) => {
                    self.finish_followup_start(
                        &snapshot.agent_id,
                        &snapshot.message_id,
                        true,
                        None,
                    )
                    .await?;
                    return Err(send_cancelled());
                }
                Either::Right((answer, _)) => answer,
            };
            match answer {
                Ok(_) => {
                    let mut tree = self.driver.tree.lock().map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "topology tree lock is poisoned",
                        )
                    })?;
                    // Only an admitted redelivery rewrites the receipt. Until
                    // that admission `output` stays the receipt already
                    // recorded, so a later `Left` does not answer the
                    // in-memory follow-up.
                    if redelivered {
                        output.disposition = snapshot.disposition;
                        if let Err(error) = self.record_redelivery(
                            &mut tree,
                            &operation_id,
                            &caller,
                            value,
                            &output,
                        ) {
                            return ToolOutcome::failure_text(
                                json!({ "code": error.code, "message": error.message }).to_string(),
                            );
                        }
                    }
                    // A Start's deliverer bound the turn it admitted before
                    // that turn could end, and a joined turn was already
                    // bound; the send binds nothing. A turn that has ended,
                    // or that a later turn replaced, is never bound or marked
                    // active again here.
                    if let Err(error) = self.driver.save(&mut tree) {
                        return ToolOutcome::failure_text(
                            json!({ "code": error.code, "message": error.message }).to_string(),
                        );
                    }
                    return ToolOutcome::success_text(json!(output).to_string());
                }
                // The claim is released on return. Unread mail wakes a running
                // child whose turn ended; a child with no turn is started by
                // this same mail once its slot is vacant; a turn another
                // command admitted gets the mail as a follow-up; an attempt
                // that already submitted or ended is never woken, so the send
                // is refused and the mail settled read.
                Err(failure @ (MailDeliveryFailure::TurnEnded | MailDeliveryFailure::SlotBusy)) => {
                    let busy = failure == MailDeliveryFailure::SlotBusy;
                    let unreached = if busy || snapshot.disposition == MailDisposition::Started {
                        Unreached::NoTurn
                    } else {
                        Unreached::EndedTurn
                    };
                    match self
                        .after_turn_ended(
                            &snapshot,
                            unreached,
                            busy || (redelivered && unreached == Unreached::NoTurn),
                            deadline,
                            &mut cancelled,
                        )
                        .await?
                    {
                        TurnEndedMail::Left => {
                            // `output` is still the recorded receipt. The
                            // follow-up disposition lives only on `snapshot`
                            // until a turn admits it.
                            return ToolOutcome::success_text(json!(output).to_string());
                        }
                        TurnEndedMail::Refused(reason) => {
                            self.finish_followup_start(
                                &snapshot.agent_id,
                                &snapshot.message_id,
                                false,
                                Some((
                                    operation_id.clone(),
                                    TopologyErrorCode::RecipientAttemptTerminal,
                                )),
                            )
                            .await?;
                            let mut tree = self.driver.lock().map_err(|error| {
                                ToolRuntimeError::new(
                                    ToolRuntimeErrorKind::Execution,
                                    error.message,
                                )
                            })?;
                            return self.refuse_turn_ended(
                                &mut tree,
                                &operation_id,
                                &caller,
                                value,
                                &snapshot,
                                &reason,
                            );
                        }
                        TurnEndedMail::Cancelled(error) => {
                            self.cancel_followup_send(
                                &operation_id,
                                &snapshot.agent_id,
                                &snapshot.message_id,
                            )
                            .await?;
                            return Err(error);
                        }
                        TurnEndedMail::Unwatched => {
                            if redelivered {
                                self.remember_redelivery_failure(&operation_id, &caller, value)?;
                            }
                            return self
                                .refuse_delivery(
                                    Some(&operation_id),
                                    &snapshot.agent_id,
                                    &snapshot.message_id,
                                    MailDeliveryFailure::RecoveryRequired(
                                        "the child's interrupted turn is still bound and no turn \
                                     waiter is left to release it"
                                            .to_owned(),
                                    ),
                                )
                                .await;
                        }
                        TurnEndedMail::Restart => {
                            let _ = start.get_or_insert_with(|| MailConsumerGuard {
                                tree: Arc::clone(&self.driver.tree),
                                consumer: MailConsumer::Start {
                                    agent: snapshot.agent_id.clone(),
                                    message: snapshot.message_id.clone(),
                                },
                            });
                            snapshot.disposition = MailDisposition::Started;
                            redelivered = true;
                        }
                        TurnEndedMail::FollowUp => {
                            snapshot.disposition = MailDisposition::Queued;
                            redelivered = true;
                        }
                    }
                }
                Err(failure) => {
                    // After a redelivery the receipt becomes this refusal, so
                    // it never records a delivery no turn admitted and a
                    // replay is refused too.
                    if redelivered {
                        self.remember_redelivery_failure(&operation_id, &caller, value)?;
                    }
                    return self
                        .refuse_delivery(
                            Some(&operation_id),
                            &snapshot.agent_id,
                            &snapshot.message_id,
                            failure,
                        )
                        .await;
                }
            }
        }
    }

    /// Rewrites an admitted redelivery's mail and operation receipt to the
    /// delivery that carried it: `started` for a Start, `queued` for a
    /// follow-up into a turn another command admitted (spec 03 sections 4.3
    /// and 9).
    fn record_redelivery(
        &self,
        tree: &mut ProductAgentTree,
        operation_id: &str,
        caller: &str,
        value: &Value,
        output: &SendMessageOutput,
    ) -> Result<(), TopologyError> {
        let request = decode::<SendAgentMessageRequest>(value)?;
        tree.redeliver_mail(&output.message_id, output.disposition);
        if tree.operations().contains_key(operation_id) {
            tree.remember_success(operation_id, caller, "send_agent_message", &request, output)?;
        }
        Ok(())
    }

    /// A redelivery that was not admitted: the operation receipt becomes
    /// the refusal [`Self::refuse_delivery`] reports.
    fn remember_redelivery_failure(
        &self,
        operation_id: &str,
        caller: &str,
        value: &Value,
    ) -> Result<(), ToolRuntimeError> {
        let request = decode::<SendAgentMessageRequest>(value).map_err(|error| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        })?;
        let mut tree = self.driver.tree.lock().map_err(|_| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "topology tree lock is poisoned",
            )
        })?;
        if tree.operations().contains_key(operation_id) {
            tree.remember_failure(
                operation_id,
                caller,
                "send_agent_message",
                &request,
                TopologyErrorCode::OperationRecoveryRequired,
            )
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        }
        Ok(())
    }

    /// What a delivery that reached no turn does with its mail. An
    /// interrupted child has no park and no wake: its turn waiter only
    /// releases the cancelled turn (spec 03 section 9). The mail is held
    /// (still claimed) until the child can take it, then delivered again:
    /// as the Start this send reserves when the child has no turn, or as a
    /// follow-up into a turn another command admitted. `wait_first` looks
    /// only after the next tree change: the slot was still starting or
    /// retiring, or a redelivery raced it. The send's one `deadline` bounds
    /// the whole wait; the sender's cancellation ends it before any
    /// redelivery.
    async fn after_turn_ended(
        &self,
        snapshot: &MailDeliverySnapshot,
        mut unreached: Unreached,
        wait_first: bool,
        deadline: tokio::time::Instant,
        cancelled: &mut SendCancelled<'_>,
    ) -> Result<TurnEndedMail, ToolRuntimeError> {
        let lock_error = || {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "topology tree lock is poisoned",
            )
        };
        let mut wait_first = wait_first;
        let mut cancel_seen = false;
        loop {
            let notified = {
                let mut tree = self.driver.tree.lock().map_err(|_| lock_error())?;
                let mut notified = Box::pin(tree.waiters().notified_owned());
                notified.as_mut().enable();
                if !wait_first {
                    cancel_seen = cancel_seen || cancelled.as_mut().now_or_never().is_some();
                    let look = TurnEndedLook {
                        expired: tokio::time::Instant::now() >= deadline,
                        cancelled: cancel_seen,
                        unreached,
                    };
                    if let Some(settled) = self.settle_turn_ended(&mut tree, snapshot, look)? {
                        return Ok(settled);
                    }
                    // Whatever the send waits for (an unwind, a slot, another
                    // send's Start), the turn its mail targeted is gone.
                    unreached = Unreached::NoTurn;
                }
                notified
            };
            wait_first = false;
            let tick = Box::pin(tokio::time::timeout(TURN_UNWIND_TICK, notified));
            if let Either::Left(((), _)) = select(cancelled.as_mut(), tick).await {
                cancel_seen = true;
            }
        }
    }

    /// One look at a recipient the delivery did not reach: `None` while an
    /// interrupted turn is still unwinding (a turn no waiter will release
    /// fails the send closed instead), while another send's Start for
    /// the child is in flight, or while a slot keeps the next delivery out.
    /// A child with no turn is started by this send, exclusively: it reserves
    /// the Start under the tree lock, and later sends wait for that Start,
    /// then follow up into the turn it admitted. The budget is checked
    /// before any reservation is granted or renewed.
    fn settle_turn_ended(
        &self,
        tree: &mut ProductAgentTree,
        snapshot: &MailDeliverySnapshot,
        look: TurnEndedLook,
    ) -> Result<Option<TurnEndedMail>, ToolRuntimeError> {
        use crate::protocol::NodeStatus;
        let agent_id = snapshot.agent_id.as_str();
        let state = tree.attempt(agent_id).map(|attempt| {
            (
                attempt.output_json.is_some(),
                attempt.status.is_stopped(),
                attempt.status,
                attempt.turn_id.is_some(),
            )
        });
        let refusal = match state {
            None => Some("its attempt is gone".to_owned()),
            Some((true, ..)) => Some("it submitted its result".to_owned()),
            Some((_, true, status, _)) => Some(format!(
                "its attempt ended {}",
                serde_json::to_value(status)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{status:?}"))
            )),
            Some(_) if tree.is_park_ending(agent_id) => {
                Some("its wake was not admitted and its attempt is ending".to_owned())
            }
            Some(_) => None,
        };
        if let Some(reason) = refusal {
            return Ok(Some(TurnEndedMail::Refused(reason)));
        }
        if tree
            .mail(&snapshot.message_id)
            .is_none_or(|mail| mail.durable != MailDurableState::Accepted)
        {
            return Ok(Some(TurnEndedMail::Left));
        }
        let node = tree.node(agent_id).map_err(|error| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        })?;
        // A parked child's wake carries the mail.
        if tree.is_parked(agent_id) {
            return Ok(Some(TurnEndedMail::Left));
        }
        let bound = state.is_some_and(|(.., bound)| bound);
        let turn = tree.has_current_turn_owner(agent_id);
        let reserved = tree
            .start_reservations
            .get(agent_id)
            .is_some_and(|message| message != &snapshot.message_id);
        let (next, reason) = match node.status {
            // Nobody will release this bound turn: waiting out the budget
            // for it would only stall the send. An active turn not bound yet
            // is a Start still being admitted; its waiter registers with
            // its bind, so it waits below.
            NodeStatus::Interrupted if bound && !tree.has_turn_waiter(agent_id) => {
                return Ok(Some(TurnEndedMail::Unwatched));
            }
            NodeStatus::Interrupted if turn => {
                (None, "its interrupted turn did not finish unwinding")
            }
            NodeStatus::Interrupted if reserved => {
                (None, "its resume turn was not admitted in time")
            }
            NodeStatus::Interrupted => (
                Some(TurnEndedMail::Restart),
                "its resume turn was not admitted in time",
            ),
            // A running child's turn waiter parks or wakes it with this mail.
            NodeStatus::Running if look.unreached == Unreached::EndedTurn => {
                return Ok(Some(TurnEndedMail::Left));
            }
            NodeStatus::Running if turn => (
                Some(TurnEndedMail::FollowUp),
                "the turn it would join was not live in time",
            ),
            NodeStatus::Running if reserved => (None, "its turn was not admitted in time"),
            NodeStatus::Running => (
                Some(TurnEndedMail::Restart),
                "its turn was not admitted in time",
            ),
            // No Start or follow-up is admissible here: the mail waits for
            // the child's next admission.
            _ => return Ok(Some(TurnEndedMail::Left)),
        };
        if look.cancelled {
            return Ok(Some(TurnEndedMail::Cancelled(send_cancelled())));
        }
        // A turn another command admitted is still the child's: its end
        // parks or wakes the child with the unread mail.
        if look.expired && matches!(next, Some(TurnEndedMail::FollowUp)) {
            return Ok(Some(TurnEndedMail::Left));
        }
        if look.expired {
            return Ok(Some(TurnEndedMail::Refused(reason.to_owned())));
        }
        if matches!(next, Some(TurnEndedMail::Restart)) {
            tree.start_reservations
                .insert(agent_id.to_owned(), snapshot.message_id.clone());
        }
        Ok(next)
    }

    /// Refuses a send whose mail no turn will ever carry: the mail is settled
    /// read after its opening is withdrawn or held for recovery, and the
    /// send's Start reservation is released. The receipt becomes
    /// the refusal, so a replay is refused too instead of returning the
    /// success it first recorded.
    fn refuse_turn_ended(
        &self,
        tree: &mut ProductAgentTree,
        operation_id: &str,
        caller: &str,
        value: &Value,
        snapshot: &MailDeliverySnapshot,
        reason: &str,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let code = TopologyErrorCode::RecipientAttemptTerminal;
        let request = decode::<SendAgentMessageRequest>(value).map_err(|error| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        })?;
        if tree.start_reservations.get(&snapshot.agent_id) == Some(&snapshot.message_id) {
            tree.start_reservations.remove(&snapshot.agent_id);
        }
        let settled = tree
            .record_undeliverable_mail(&snapshot.message_id)
            .and_then(|()| tree.project_mail_decisions(&snapshot.agent_id))
            .and_then(|()| {
                if tree.operations().contains_key(operation_id) {
                    tree.remember_failure(
                        operation_id,
                        caller,
                        "send_agent_message",
                        &request,
                        code,
                    )?;
                }
                self.driver.save(tree)
            });
        if let Err(error) = settled {
            return ToolOutcome::failure_text(
                json!({ "code": error.code, "message": error.message }).to_string(),
            );
        }
        ToolOutcome::failure_text(
            json!({
                "code": code,
                "message": format!(
                    "message {} did not reach {}: {reason}; the message was not \
                     delivered",
                    snapshot.message_id, snapshot.agent_id
                ),
            })
            .to_string(),
        )
    }

    async fn finish_followup_start(
        &self,
        agent: &str,
        message: &str,
        uncertain: bool,
        receipt: Option<(String, TopologyErrorCode)>,
    ) -> Result<bool, ToolRuntimeError> {
        let failure = |error: TopologyError| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        };
        let Some(expected) = self
            .driver
            .read(|tree| tree.owned_followup_attempt(agent, message))
            .map_err(failure)?
            .map_err(failure)?
        else {
            return Ok(false);
        };
        if !uncertain
            && let Some(deliverer) = &self.deliverer
            && deliverer
                .withdraw_followup(
                    self.driver.clone(),
                    agent.to_owned(),
                    message.to_owned(),
                    receipt.clone(),
                )
                .await
                .is_ok()
        {
            return Ok(true);
        }
        let (held, effects) = self
            .driver
            .mutate(|tree| {
                if tree.node(agent)?.attempt_id.as_ref() == Some(&expected)
                    && !tree
                        .attempt(agent)
                        .is_some_and(|attempt| attempt.status.is_stopped())
                    && (uncertain
                        || tree
                            .attempt(agent)
                            .is_none_or(|attempt| attempt.turn_id.is_none()))
                {
                    tree.record_fault(
                        agent,
                        if uncertain {
                            "followup Start submission outcome is unknown"
                        } else {
                            "followup Start did not reach the runtime but withdrawal was refused"
                        },
                    );
                    tree.record_host_materialization(agent)?;
                    self.driver.apply_recorded(tree, agent)?;
                }
                if receipt.is_some() {
                    self.driver.fold_and_commit(tree, |tree| {
                        settle_followup_refusal(tree, agent, message, receipt)
                    })?;
                }
                Ok(())
            })
            .map_err(failure)?;
        held.map_err(failure)?;
        self.settle(effects).await?;
        Ok(true)
    }

    async fn cancel_followup_send(
        &self,
        operation: &str,
        agent: &str,
        message: &str,
    ) -> Result<(), ToolRuntimeError> {
        self.finish_followup_start(
            agent,
            message,
            false,
            Some((
                operation.to_owned(),
                TopologyErrorCode::RecipientAttemptTerminal,
            )),
        )
        .await?;
        Ok(())
    }

    /// A refused delivery of `message_id`. A `followup` whose Start never
    /// reached the child is undone first, by the send that opened it only:
    /// the child keeps the attempt the followup found, so the caller can
    /// still adopt it, and no candidate is minted for an attempt that never
    /// ran. Anything else is a materialization fault and
    /// the node falls to recovery.
    async fn refuse_delivery(
        &self,
        operation: Option<&str>,
        agent_id: &str,
        message_id: &str,
        failure: MailDeliveryFailure,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let (definite_refusal, message) = failure.into_parts();
        if !definite_refusal {
            return self.fail_closed_session(agent_id, message);
        }
        let withdrawn = match &self.deliverer {
            Some(deliverer) => {
                deliverer
                    .withdraw_followup(
                        self.driver.clone(),
                        agent_id.to_owned(),
                        message_id.to_owned(),
                        operation.map(|id| (id.to_owned(), TopologyErrorCode::Capacity)),
                    )
                    .await
            }
            None => Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "followup withdrawal has no authoritative session owner",
            )),
        };
        if withdrawn.is_err() {
            return self.fail_closed_session(agent_id, message);
        }
        ToolOutcome::failure_text(
            json!({
                "code": if definite_refusal {
                    TopologyErrorCode::Capacity
                } else {
                    TopologyErrorCode::OperationRecoveryRequired
                },
                "message": format!(
                    "{message}; the followup attempt was rolled back and {agent_id} still \
                     carries the attempt it had"
                ),
            })
            .to_string(),
        )
    }

    async fn interrupt_with(
        &self,
        operation_id: String,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let request = match decode::<AttemptScopedRequest>(value) {
            Ok(request) => request,
            Err(error) => {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
        };
        let (status, effects) = self
            .driver
            .mutate(|tree| {
                self.dispatch_mutating(
                    tree,
                    &caller,
                    &operation_id,
                    "interrupt_agent",
                    &request,
                    |tree, caller, request, operation_id| {
                        tree.interrupt_agent(caller, request, operation_id)
                    },
                )
            })
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        self.settle(effects).await?;
        let (status, replayed) = match status {
            Ok(status) => status,
            Err(error) => {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
        };
        let payload = json!({ "agent_id": request.agent_id, "status": status });
        if replayed {
            return ToolOutcome::success_text(payload.to_string());
        }
        let Some(canceller) = &self.canceller else {
            return ToolOutcome::success_text(payload.to_string());
        };
        let cancellation = canceller
            .cancel_agents(vec![request.agent_id.clone()])
            .await;
        self.finish_cancel_receipt(
            &operation_id,
            &caller,
            "interrupt_agent",
            &request,
            &status,
            &cancellation,
        )?;
        match cancellation {
            Ok(_) => ToolOutcome::success_text(payload.to_string()),
            Err(failure) => self.fail_closed_cancel(failure),
        }
    }

    async fn close_with(
        &self,
        operation_id: String,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let request = match decode::<AttemptScopedRequest>(value) {
            Ok(request) => request,
            Err(error) => {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
        };
        // an idle closed copy seals, and the snapshot MUST run
        // with the tree mutex released. Phase one closes and queues tickets
        // under the lock; the seals land after it drops.
        let (closed, effects) = self
            .driver
            .mutate(|tree| {
                self.dispatch_mutating(
                    tree,
                    &caller,
                    &operation_id,
                    "close_agent",
                    &request,
                    |tree, caller, request, operation_id| {
                        tree.close_agent(caller, request, operation_id)
                    },
                )
            })
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        let (closed, replayed) = match closed {
            Ok(closed) => closed,
            Err(error) => {
                self.settle(effects).await?;
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
        };
        let ids: Vec<String> = closed
            .iter()
            .map(|(agent_id, _)| agent_id.clone())
            .collect();
        let payload = json!({
            "agents": closed
                .iter()
                .map(|(agent_id, status)| {
                    if *status == crate::protocol::NodeStatus::RecoveryRequired {
                        json!({ "agent_id": agent_id, "status": status, "remedy": "recover_agent" })
                    } else {
                        json!({ "agent_id": agent_id, "status": status })
                    }
                })
                .collect::<Vec<_>>()
        });
        if replayed {
            self.settle(effects).await?;
            return ToolOutcome::success_text(payload.to_string());
        }
        let mut cancellation = match &self.canceller {
            Some(canceller) => canceller.cancel_agents(ids.clone()).await,
            None => Ok(LiveCancelOutcome {
                cancelled_ids: Vec::new(),
                not_live_ids: ids.clone(),
            }),
        };
        let stop = self.driver.stop_agent_jobs(&ids).await;
        // Closing owns these effects even if cancellation or process cleanup failed.
        let settled = self.settle(effects).await;
        if let Err(error) = stop {
            let failed_ids = self
                .driver
                .read(|tree| {
                    ids.iter()
                        .filter(|id| {
                            tree.node(id).is_ok_and(|node| {
                                node.status == crate::protocol::NodeStatus::RecoveryRequired
                            })
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|_| ids.clone());
            cancellation = Err(match cancellation {
                Ok(report) => LiveCancelFailure {
                    message: error.message,
                    failed_ids,
                    joiner_expired_ids: Vec::new(),
                    owner_failures: Vec::new(),
                    cancelled_ids: report.cancelled_ids,
                    not_live_ids: report.not_live_ids,
                },
                Err(mut failure) => {
                    failure.message.push_str(&format!("; {}", error.message));
                    failure.failed_ids.extend(failed_ids);
                    failure
                }
            });
        }
        {
            let mut tree = self.driver.tree.lock().map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?;
            for (agent_id, status) in &closed {
                let detach = NodeDetach::from_node_status(*status).unwrap_or(NodeDetach::Closed);
                tree.try_emit_tree_node_detached(self.observer.as_ref(), agent_id, detach);
            }
        }
        self.finish_cancel_receipt(
            &operation_id,
            &caller,
            "close_agent",
            &request,
            &closed,
            &cancellation,
        )?;
        settled?;
        match cancellation {
            Ok(_) => ToolOutcome::success_text(payload.to_string()),
            Err(failure) => self.fail_closed_cancel(failure),
        }
    }

    /// `recover_agent`: authority under the lock, then the host's exact
    /// session reopen with the lock released. No operation receipt is kept:
    /// recovery is idempotent by construction, so a repeat answers from the
    /// node as it stands.
    async fn recover_with(&self, value: &Value) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let request = match decode::<AttemptScopedRequest>(value) {
            Ok(request) => request,
            Err(error) => return Self::topology_failure(&error),
        };
        let authorized = self
            .driver
            .read(|tree| tree.authorize_recover(&caller, &request))
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        if let Err(error) = authorized {
            return Self::topology_failure(&error);
        }
        let Some(recoverer) = &self.recoverer else {
            return Self::topology_failure(&TopologyError::new(
                TopologyErrorCode::RecoverySessionUnavailable,
                "this host cannot reopen child sessions; nothing changed",
            ));
        };
        match recoverer
            .recover(request.agent_id, request.expected_attempt_id)
            .await
        {
            Ok(output) => ToolOutcome::success_text(json!(output).to_string()),
            Err(error) => Self::topology_failure(&error),
        }
    }

    fn finish_cancel_receipt<Req: serde::Serialize, Out: serde::Serialize>(
        &self,
        operation_id: &str,
        caller: &str,
        tool_name: &str,
        request: &Req,
        output: &Out,
        cancellation: &Result<LiveCancelOutcome, LiveCancelFailure>,
    ) -> Result<(), ToolRuntimeError> {
        let (result, effects) = self
            .driver
            .mutate(|tree| {
                if cancellation.is_ok() {
                    tree.remember_success(operation_id, caller, tool_name, request, output)?;
                } else {
                    tree.remember_failure(
                        operation_id,
                        caller,
                        tool_name,
                        request,
                        TopologyErrorCode::OperationRecoveryRequired,
                    )?;
                }
                if let Err(error) = self.driver.save(tree) {
                    // A refused publish cannot leave an in-memory success replay.
                    tree.remember_failure(
                        operation_id,
                        caller,
                        tool_name,
                        request,
                        TopologyErrorCode::OperationRecoveryRequired,
                    )?;
                    return Err(error);
                }
                Ok(())
            })
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        self.driver.settle(effects).map_err(|error| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        })?;
        result
            .map_err(|error| ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message))
    }

    fn fail_closed_cancel(
        &self,
        failure: LiveCancelFailure,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let mut recorded = false;
        for owner in &failure.owner_failures {
            let id = &owner.agent_id;
            let (result, effects) = self
                .driver
                .mutate(|tree| {
                    let changed = tree
                        .record_host_retirement_for_attempt(
                            id,
                            owner.expected_attempt_id.as_ref(),
                            owner.message.clone(),
                        )
                        .and_then(|value| self.driver.apply_recorded(tree, id).map(|()| value))?;
                    if !changed {
                        return Ok(false);
                    }
                    tree.try_emit_tree_node_detached(
                        self.observer.as_ref(),
                        id,
                        NodeDetach::RecoveryRequired,
                    );
                    self.driver.save(tree)?;
                    Ok(true)
                })
                .map_err(|error| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                })?;
            self.driver.settle(effects).map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
            recorded |= result.map_err(|error: TopologyError| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        }
        if recorded {
            self.driver.request_retirement_recovery();
        }
        ToolOutcome::failure_text(
            json!({
                "code": TopologyErrorCode::OperationRecoveryRequired,
                "message": failure.message,
            })
            .to_string(),
        )
    }

    fn fail_closed_session(
        &self,
        agent_id: &str,
        message: String,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let (persist_error, pending) = {
            let Ok(mut tree) = self.driver.tree.lock() else {
                return ToolOutcome::failure_text(
                    json!({
                        "code": TopologyErrorCode::OperationRecoveryRequired,
                        "message": format!(
                            "{message}; topology tree lock is poisoned while persisting recovery"
                        ),
                    })
                    .to_string(),
                );
            };
            tree.record_fault(agent_id, message.clone());
            // the partial-work seal runs with the lock released.
            let materialized = tree
                .record_host_materialization(agent_id)
                .and_then(|value| {
                    self.driver
                        .apply_recorded(&mut tree, agent_id)
                        .map(|()| value)
                });
            let status = tree
                .node(agent_id)
                .ok()
                .and_then(|node| NodeDetach::from_node_status(node.status))
                .unwrap_or(NodeDetach::RecoveryRequired);
            tree.try_emit_tree_node_detached(self.observer.as_ref(), agent_id, status);
            let pending = if materialized.is_ok() {
                tree.take_effects()
            } else {
                Effects::default()
            };
            (materialized, pending)
        };
        if let Err(error) = self.driver.settle(pending) {
            return Self::topology_failure(&error);
        }
        match persist_error {
            Ok(()) => ToolOutcome::failure_text(
                json!({
                    "code": TopologyErrorCode::OperationRecoveryRequired,
                    "message": message,
                })
                .to_string(),
            ),
            Err(error) => ToolOutcome::failure_text(
                json!({
                    "code": error.code,
                    "message": format!("{message}; persist failed: {}", error.message),
                })
                .to_string(),
            ),
        }
    }

    fn dispatch_mutating<Req, Out>(
        &self,
        tree: &mut ProductAgentTree,
        caller: &str,
        operation_id: &str,
        tool_name: &str,
        request: &Req,
        work: impl FnOnce(&mut ProductAgentTree, &str, &Req, Option<&str>) -> Result<Out, TopologyError>,
    ) -> Result<(Out, bool), TopologyError>
    where
        Req: serde::Serialize,
        Out: DeserializeOwned,
    {
        let settles_live =
            self.canceller.is_some() && matches!(tool_name, "interrupt_agent" | "close_agent");
        let prepared = tree.operations().contains_key(operation_id);
        if tool_name == "discard_workspace" && prepared && tree.revision != tree.stored_revision {
            self.driver.save(tree)?;
        }
        if let Some(prior) = tree.prepare_operation(operation_id, caller, tool_name, request)? {
            let output = serde_json::from_str(&prior).map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "prior topology receipt could not be reconstructed",
                )
            })?;
            return Ok((output, true));
        }
        if (settles_live || tool_name == "discard_workspace") && prepared {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "the original operation is pending or was interrupted; replay cannot dispatch it again",
            ));
        }
        self.driver.save(tree)?;
        // The domain intent is durable before cancellation, but is not its
        // completion receipt. A prepared replay must never cancel a new turn.
        let domain_operation = (!settles_live).then_some(operation_id);
        match self
            .driver
            .fold_and_commit(tree, |tree| work(tree, caller, request, domain_operation))
        {
            Ok(output) => Ok((output, false)),
            Err(error) => {
                // Prepare already persisted a non-terminal receipt. Failures
                // must close it or reopen treats the op as RecoveryRequired.
                tree.remember_failure(operation_id, caller, tool_name, request, error.code)?;
                match self.driver.save(tree) {
                    Ok(()) => Err(error),
                    Err(persist) => Err(persist),
                }
            }
        }
    }

    #[cfg(test)]
    async fn wait_with(&self, value: &Value) -> Result<ToolOutcome, ToolRuntimeError> {
        self.wait_operation_with(&format!("test-wait-{}", uuid::Uuid::new_v4()), value)
            .await
    }

    #[cfg(test)]
    async fn wait_operation_with(
        &self,
        operation_id: &str,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        self.wait_operation_preempt(operation_id, value, None).await
    }

    async fn wait_operation_preempt(
        &self,
        operation_id: &str,
        value: &Value,
        preempt: Option<tokio::sync::watch::Receiver<plakous::tools::ToolPreempt>>,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let request = match decode::<WaitAgentRequest>(value) {
            Ok(request) => request,
            Err(error) => return Self::topology_failure(&error),
        };
        let awaited = match AwaitRequest::wait_agent(&request) {
            Ok(awaited) => awaited,
            Err(code) => {
                return Self::topology_failure(&TopologyError::new(
                    code,
                    topology_wait_request_invalid_message(code, &request),
                ));
            }
        };
        self.await_operation(operation_id, &awaited, &request, preempt)
            .await
    }

    /// The one await every await presentation drives: over the caller's
    /// children, their mail and its own processes, with one receipt kind.
    /// `presented` is the presentation's own request; a same-call replay
    /// must present it again.
    async fn await_operation<R: serde::Serialize>(
        &self,
        operation_id: &str,
        request: &AwaitRequest,
        presented: &R,
        mut preempt: Option<tokio::sync::watch::Receiver<plakous::tools::ToolPreempt>>,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let jobs = self.driver.job_service(&caller);
        {
            let mut tree = self.driver.tree.lock().map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?;
            if tree
                .operations()
                .get(operation_id)
                .is_some_and(|receipt| receipt.caller_agent_id != caller)
            {
                return Self::topology_failure(&TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "retry caller conflicts with the wait receipt",
                ));
            }
            if tree.operations().contains_key(operation_id)
                && let Some(response) = tree
                    .prepare_operation(operation_id, &caller, AWAIT_RECEIPT_KIND, presented)
                    .map_err(|error| {
                        ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                    })?
            {
                let receipt: crate::tree::WaitReceipt =
                    serde_json::from_str(&response).map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "wait receipt could not be reconstructed",
                        )
                    })?;
                return ToolOutcome::success_text(receipt.output.to_string());
            }
            if let Err(error) = admit_awaited_jobs(jobs.as_ref(), request) {
                return Self::topology_failure(&error);
            }
            // An empty or cancelled wait has no durable receipt.
            let targets = match tree.wait_target_ids(&caller, &request.agents) {
                Ok(targets) => targets,
                Err(error) => return Self::topology_failure(&error),
            };
            tree.parked_waits
                .insert((caller.clone(), operation_id.to_owned()), targets);
        }
        let generation = jobs.as_ref().map_or(
            0,
            plakous::tools::JoinedProcessSessions::job_terminal_generation,
        );
        let _waiting = MailConsumerGuard {
            tree: Arc::clone(&self.driver.tree),
            consumer: MailConsumer::Wait {
                caller: caller.clone(),
                operation: operation_id.to_owned(),
            },
        };
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(request.timeout_ms);
        let steered =
            |preempt: Option<&tokio::sync::watch::Receiver<plakous::tools::ToolPreempt>>| {
                preempt.is_some_and(|flag| *flag.borrow() == plakous::tools::ToolPreempt::Steer)
            };
        loop {
            let jobs = self.driver.job_service(&caller);
            let mut job_changes = jobs
                .as_ref()
                .map(plakous::tools::JoinedProcessSessions::job_terminal_changes);
            // the wait stays poll-driven. The notifier is
            // subscribed before the poll so a settle, seal, question, mail, or
            // steer that lands during it still wakes the next park instead of
            // being lost until the tick expires. This is not an event loop.
            let (output, notify) = {
                let mut tree = self.driver.tree.lock().map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "topology tree lock is poisoned",
                    )
                })?;
                let mut notified = Box::pin(tree.waiters().notified_owned());
                notified.as_mut().enable();
                let output = match tree.poll_wait(&caller, &request.agents) {
                    Ok(Some(output)) => Some(self.finish_await(
                        &mut tree,
                        &caller,
                        operation_id,
                        presented,
                        request,
                        jobs.as_ref(),
                        output,
                    )),
                    Ok(None) => None,
                    Err(error) => Some(Self::topology_failure(&error)),
                };
                if output.is_some() {
                    tree.parked_waits
                        .remove(&(caller.clone(), operation_id.to_owned()));
                    tree.waiters().notify_waiters();
                }
                (output, notified)
            };
            if let Some(outcome) = output {
                return outcome;
            }
            let processes = jobs
                .as_ref()
                .map_or_else(Vec::new, |service| service.terminal_jobs_since(generation));
            if steered(preempt.as_ref())
                || !processes.is_empty()
                || named_job_settled(jobs.as_ref(), &request.processes)
            {
                let mut tree = self.driver.lock().map_err(|error| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                })?;
                let mut output = tree
                    .timeout_wait(&caller, &request.agents)
                    .map_err(|error| {
                        ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                    })?;
                // Recheck under the lock: mail may have arrived after the first poll.
                if output.woke_by == crate::tree::WaitWakeReason::Timeout {
                    output.timed_out = false;
                    output.woke_by = if steered(preempt.as_ref()) {
                        crate::tree::WaitWakeReason::Steer
                    } else {
                        output.processes = processes;
                        crate::tree::WaitWakeReason::Process
                    };
                }
                return self.finish_await(
                    &mut tree,
                    &caller,
                    operation_id,
                    presented,
                    request,
                    jobs.as_ref(),
                    output,
                );
            }
            if tokio::time::Instant::now() >= deadline {
                let mut tree = self.driver.tree.lock().map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "topology tree lock is poisoned",
                    )
                })?;
                let output = tree.timeout_wait(&caller, &request.agents);
                tree.parked_waits
                    .remove(&(caller.clone(), operation_id.to_owned()));
                tree.waiters().notify_waiters();
                return match output {
                    Ok(output) => self.finish_await(
                        &mut tree,
                        &caller,
                        operation_id,
                        presented,
                        request,
                        jobs.as_ref(),
                        output,
                    ),
                    Err(error) => Self::topology_failure(&error),
                };
            }
            // Park until the tree changes or the tick expires,
            // whichever comes first.
            tokio::select! {
                () = notify => {},
                () = async {
                    if let Some(changes) = job_changes.as_mut() && changes.changed().await.is_ok() { return; }
                    std::future::pending::<()>().await;
                } => {},
                () = tokio::time::sleep(std::time::Duration::from_secs(1)
                    .min(deadline.saturating_duration_since(tokio::time::Instant::now()))) => {},
                () = async {
                    if let Some(flag) = preempt.as_mut() && flag.changed().await.is_ok() { return; }
                    std::future::pending::<()>().await;
                } => {},
            }
        }
    }

    /// Attaches the named job rows, applies the output budget, and records
    /// the receipt once the result exists: a result too large to return
    /// leaves its mail and its rows for the next await.
    #[allow(
        clippy::too_many_arguments,
        reason = "one await's identity, request and observation"
    )]
    fn finish_await<R: serde::Serialize>(
        &self,
        tree: &mut ProductAgentTree,
        caller: &str,
        operation_id: &str,
        presented: &R,
        request: &AwaitRequest,
        jobs: Option<&plakous::tools::JoinedProcessSessions>,
        mut output: AwaitOutput,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        output.jobs = await_job_rows(jobs, &request.processes);
        output.fit_budget();
        let subject = output.presentation_subject();
        let outcome =
            ToolOutcome::success_text(json!(output).to_string()).map(|outcome| match subject {
                Some(subject) => outcome.with_presentation_subject(subject),
                None => outcome,
            });
        if outcome.is_ok()
            && let Err(error) =
                self.commit_wait_receipt(tree, caller, operation_id, presented, &output)
        {
            return Self::topology_failure(&error);
        }
        outcome
    }

    fn commit_wait_receipt<R: serde::Serialize>(
        &self,
        tree: &mut ProductAgentTree,
        caller: &str,
        operation_id: &str,
        presented: &R,
        output: &AwaitOutput,
    ) -> Result<(), TopologyError> {
        if output.mail.is_empty() && output.woke_by != crate::tree::WaitWakeReason::Settled {
            return Ok(());
        }
        // A failed publish must not hide mail from the next wait or listing.
        let mut acknowledged = tree.clone();
        acknowledged.acknowledge_wait_mail(caller, output);
        acknowledged.mark_wait_delivered(caller, output);
        let receipt = crate::tree::WaitReceipt {
            output: json!(output),
            newly_delivered: output
                .agents
                .iter()
                .filter(|row| {
                    acknowledged.delivered_to(caller, &row.agent_id)
                        && !tree.delivered_to(caller, &row.agent_id)
                })
                .map(|row| row.agent_id.clone())
                .collect(),
            acknowledged: output
                .mail
                .iter()
                .filter_map(|mail| tree.mail(&mail.message_id).cloned())
                .collect(),
        };
        acknowledged.remember_success(
            operation_id,
            caller,
            AWAIT_RECEIPT_KIND,
            presented,
            &receipt,
        )?;
        self.driver.save(&mut acknowledged)?;
        *tree = acknowledged;
        Ok(())
    }
}

/// Leaves the facts the host reads from an await's whole result at the tool
/// boundary, never from the bounded preview a fan-out wait outgrows, and
/// reports whether a steer woke it.
fn leave_wait_facts(operation_id: &str, outcome: &ToolOutcome) -> bool {
    let result = outcome.parts().iter().find_map(|part| match part {
        plakous::model::ToolResultPart::Text(text) => {
            serde_json::from_str::<Value>(text.text()).ok()
        }
        _ => None,
    });
    let yielded = result
        .as_ref()
        .is_some_and(|value| value.get("woke_by").and_then(Value::as_str) == Some("steer"));
    if let Some(result) = result {
        crate::call_facts::leave(
            operation_id,
            crate::call_facts::TopologyCallFacts::Wait(result),
        );
    }
    yielded
}

/// The `{ "result": string }` envelope every topology passthrough raises
/// into. A task presentation's own text is model-visible as is, so a call
/// that reached the tree wraps its receipt the same way here.
/// Direct model calls see the topology envelope. A cell sees the raw
/// receipt, the same bytes `wait_agent` returns. Process-only calls never
/// reach this; they return the process tool's own bytes.
fn present_task_receipt(
    lineage: &ToolCallerLineage,
    outcome: ToolOutcome,
) -> Result<ToolOutcome, ToolRuntimeError> {
    if matches!(lineage, ToolCallerLineage::Direct) {
        topology_envelope(outcome)
    } else {
        Ok(outcome)
    }
}

fn topology_envelope(outcome: ToolOutcome) -> Result<ToolOutcome, ToolRuntimeError> {
    if !outcome.is_success() {
        return Ok(outcome);
    }
    let subject = outcome.presentation_subject().map(str::to_owned);
    ToolOutcome::success_text(json!({ "result": outcome.joined_text() }).to_string()).map(
        |wrapped| match subject {
            Some(subject) => wrapped.with_presentation_subject(subject),
            None => wrapped,
        },
    )
}

/// The job number of a process task id ([`crate::protocol::is_process_task_id`]);
/// `None` for an id that names an agent.
fn task_process_number(id: &str) -> Option<u64> {
    crate::protocol::is_process_task_id(id)
        .then(|| id.strip_prefix('p').unwrap_or(id).parse::<u64>().ok())
        .flatten()
}

/// `get_command_or_subagent_output` on a node granted agent tools.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct TaskOutputRequest {
    task_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
}

/// `kill_command_or_subagent` on a node granted agent tools.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct TaskKillRequest {
    task_id: String,
}

/// Pi's `wait` on a node granted agent tools.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct JobsWaitRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
}

/// Pi's `stop` on a node granted agent tools.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct JobsStopRequest {
    id: String,
}

impl JobsWaitRequest {
    /// The await ceiling: 300000 ms when omitted, clamped to one hour.
    fn timeout(&self) -> u64 {
        self.timeout_ms
            .unwrap_or(crate::protocol::DEFAULT_WAIT_TIMEOUT_MS)
            .min(crate::protocol::MAX_WAIT_TIMEOUT_MS)
    }
}

/// Which await a pi `wait` on a node granted agent tools runs.
#[derive(Debug, Eq, PartialEq)]
enum JobsWaitPlan {
    /// The process tool's own first-terminal await over these job numbers,
    /// or over every own job still listed when empty.
    Processes(Vec<u64>),
    /// The tree's await over children, their mail and named own processes.
    Await(AwaitRequest),
}

/// Named processes must be the caller's own promoted jobs in this session.
fn admit_awaited_jobs(
    jobs: Option<&plakous::tools::JoinedProcessSessions>,
    request: &AwaitRequest,
) -> Result<(), TopologyError> {
    if request.agents == AwaitAgents::Unwatched && request.processes.is_empty() {
        return Err(TopologyError::new(
            TopologyErrorCode::SchemaMismatch,
            "the await has nothing to watch: name agents or processes",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    if request.processes.iter().any(|id| !seen.insert(id)) {
        return Err(TopologyError::new(
            TopologyErrorCode::SchemaMismatch,
            "process ids must not repeat",
        ));
    }
    let owned = jobs.map_or_else(Vec::new, plakous::tools::JoinedProcessSessions::jobs);
    let missing = request
        .processes
        .iter()
        .filter(|id| !owned.iter().any(|job| job_id(job) == **id))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    // Only a refusal reads the service's terminal history: observing it
    // at every admission would move the wait's terminal generation. An id
    // in that history was this session's job and has since been reaped or
    // consumed; the call still returns, with an `already_reported` row.
    let ended = jobs.map_or_else(Vec::new, |service| service.terminal_jobs_since(0));
    for id in missing {
        unlisted_job_admitted(id, &ended)?;
    }
    Ok(())
}

/// An id in `ended` was this session's job and has been reaped or consumed;
/// the await reports it. Anything else was never issued here.
fn unlisted_job_admitted(id: &str, ended: &[String]) -> Result<(), TopologyError> {
    if ended.iter().any(|ended| ended == id) {
        Ok(())
    } else {
        Err(TopologyError::new(
            TopologyErrorCode::GrantDenied,
            format!("process {id} is not in this session"),
        ))
    }
}

fn job_id(job: &plakous::tools::JobView) -> String {
    format!("p{}", job.number)
}

/// Whether a named process is terminal, or already gone from the service.
fn named_job_settled(
    jobs: Option<&plakous::tools::JoinedProcessSessions>,
    named: &[String],
) -> bool {
    if named.is_empty() {
        return false;
    }
    let views = jobs.map_or_else(Vec::new, plakous::tools::JoinedProcessSessions::jobs);
    named.iter().any(|id| {
        views
            .iter()
            .find(|job| job_id(job) == *id)
            .is_none_or(|job| job.status != plakous::tools::JobStatus::Running)
    })
}

fn await_job_rows(
    jobs: Option<&plakous::tools::JoinedProcessSessions>,
    named: &[String],
) -> Vec<AwaitJobRow> {
    if named.is_empty() {
        return Vec::new();
    }
    let views = jobs.map_or_else(Vec::new, plakous::tools::JoinedProcessSessions::jobs);
    named
        .iter()
        .map(|id| {
            let Some(job) = views.iter().find(|job| job_id(job) == *id) else {
                return AwaitJobRow {
                    id: id.clone(),
                    status: "already_reported",
                    exit_code: None,
                    output: None,
                };
            };
            let (status, exit_code) = match job.status {
                plakous::tools::JobStatus::Running => ("running", None),
                plakous::tools::JobStatus::Exited(code) => ("exited", code),
                plakous::tools::JobStatus::Stopped(_) => ("stopped", None),
                plakous::tools::JobStatus::OutcomeUnknown => ("unknown", None),
            };
            AwaitJobRow {
                id: id.clone(),
                status,
                exit_code,
                output: Some(job.output.clone()),
            }
        })
        .collect()
}

fn decode<T: DeserializeOwned>(value: &Value) -> Result<T, TopologyError> {
    serde_json::from_value(value.clone()).map_err(|error| {
        let code = if error.to_string().contains("unknown field") {
            TopologyErrorCode::UnknownField
        } else {
            TopologyErrorCode::SchemaMismatch
        };
        TopologyError {
            code,
            message: error.to_string(),
        }
    })
}

/// The driver for a tree the process hub already owns, or — in tests only —
/// a fresh one over an in-memory store and the tree's own port. Production
/// tool sets and the turn waiter run on the hub's own driver
/// ([`topology_tool_registrations_on`], [`invoke_granted_topology_tool_on`]);
/// this is the bridge for the prompt goldens and unit tests that still hold
/// the bare tree `Arc`.
fn driver_for_tree(tree: &Arc<Mutex<ProductAgentTree>>) -> TreeRuntimeResult<TreeDriver> {
    let (tree_id, vcs) = {
        let guard = tree
            .lock()
            .map_err(|_| TreeRuntimeError::new("topology tree lock is poisoned"))?;
        (guard.tree_id.clone(), guard.vcs_port())
    };
    if let Some(runtime) = crate::hub::lookup(&tree_id)
        && Arc::ptr_eq(&runtime.tree, tree)
    {
        return Ok(runtime.driver().clone());
    }
    #[cfg(any(test, feature = "test-support"))]
    {
        Ok(TreeDriver::new(
            Arc::clone(tree),
            Arc::new(crate::tree::MemoryTreeStore::new()),
            vcs,
            None,
        ))
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        drop(vcs);
        Err(TreeRuntimeError::new(format!(
            "topology tree {tree_id} is not attached to the hub; no store to persist it"
        )))
    }
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// The spawn enum is this node's menu (the working set for the root, its
/// own card's `spawns` list otherwise), never a fixed set.
fn schema(kind: TopologyKind, spawnable_templates: &[String]) -> Value {
    match kind {
        TopologyKind::Spawn => object_schema(
            json!({
                "template": {
                    "type": "string",
                    "enum": spawnable_templates,
                    "description": "Template to spawn, from the cards below."
                },
                "description": {
                    "type": "string",
                    "description": "Short label for the attempt; does not select bind."
                },
                "prompt": {
                    "type": "string",
                    "description": "Task brief for the child agent."
                },
                "output_schema_json": {
                    "type": "string",
                    "description": "JSON string holding the child's submit_result object schema. Property types are object, array, string, number, integer, or boolean; no $ref and no combinators."
                },
                "tier": {
                    "type": "string",
                    "enum": ["read_only", "execute", "mutate"]
                },
                "workspace": {
                    "type": "string",
                    "enum": ["inherit", "fork"],
                    "description": "Omit or inherit to share your checkout. fork mints an isolated copy."
                },
                "inputs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 8,
                    "description": "One candidate id, from a finished child's wait or list row, to start this child's copy from instead of your current state. Requires workspace=fork: inherit has no copy of its own."
                },
                "request_key": {
                    "type": "string",
                    "description": "Your own key for this spawn. A repeat with the same key and the same brief returns the same child instead of spawning a second one; the same key on a different brief is refused request_key_conflict."
                }
            }),
            &["template", "prompt", "output_schema_json", "tier"],
        ),
        TopologyKind::Send => object_schema(
            json!({
                "agent_id": { "type": "string" },
                "message": { "type": "string" },
                "purpose": {
                    "type": "string",
                    "enum": ["coordinate", "finding", "question", "reply", "followup"],
                    "description": "Omit for coordinate. Use reply only with in_reply_to set to an existing message_id. Finished children are revivable until retired. Use followup to re-task one on a new attempt; a running child takes priority=urgent instead."
                },
                "priority": {
                    "type": "string",
                    "enum": ["deferred", "urgent"],
                    "description": "Omit for urgent from a parent to its running child; deferred otherwise."
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "Omit on the first message to a child. Required only when purpose is reply."
                },
                "output_schema_json": {
                    "type": "string",
                    "description": "Optional, and only with purpose=followup: a new result schema for the new attempt. Omit to keep the schema the child already has."
                }
            }),
            &["agent_id", "message"],
        ),
        TopologyKind::List => object_schema(
            json!({
                "include_self": { "type": "boolean" },
                "include_completed": { "type": "boolean" },
                "include_read": { "type": "boolean" }
            }),
            &[],
        ),
        TopologyKind::Wait => object_schema(
            json!({
                "agent_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Child ids to watch; omit for the children that have not reported yet. Returns when any of them settles, with a row per watched id."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Omit for 300000. Capped at 3600000."
                }
            }),
            &[],
        ),
        TopologyKind::Interrupt | TopologyKind::Close | TopologyKind::Recover => object_schema(
            json!({
                "agent_id": { "type": "string" },
                "expected_attempt_id": {
                    "type": "string",
                    "description": "Refused with attempt_mismatch unless it is the child's current attempt."
                }
            }),
            &["agent_id"],
        ),
        TopologyKind::Submit => object_schema(
            json!({
                "turn_token": { "type": "integer", "minimum": 1 },
                "output_json": { "type": "string" }
            }),
            &["turn_token", "output_json"],
        ),
        TopologyKind::Adopt => object_schema(
            json!({
                "candidate_id": { "type": "string" },
                "expected_target": {
                    "type": "string",
                    "description": "Snapshot of your copy the adoption must land on; a moved copy is refused target_changed."
                },
                "evidence": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Review attempt ids recorded with the adoption."
                }
            }),
            &["candidate_id"],
        ),
        TopologyKind::Discard => object_schema(
            json!({ "workspace_id": { "type": "string" } }),
            &["workspace_id"],
        ),
        TopologyKind::TaskOutput => object_schema(
            json!({
                "task_ids": {
                    "description": "Process ids returned by run_terminal_command (N or pN) and child agent ids.",
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 128,
                    "items": { "type": "string" }
                },
                "timeout_ms": {
                    "description": "How long to await, in milliseconds, when this turn needs the result; clamped to 3600000 (1 hour). Omit or pass 0 for an immediate snapshot.",
                    "type": "integer",
                    "minimum": 0
                }
            }),
            &["task_ids"],
        ),
        TopologyKind::TaskKill => object_schema(
            json!({
                "task_id": {
                    "description": "Process id (N or pN) to stop, or child agent id to interrupt.",
                    "type": "string"
                }
            }),
            &["task_id"],
        ),
        TopologyKind::JobsWait => object_schema(
            json!({
                "ids": {
                    "description": "Process ids returned by bash (N or pN) and child agent ids; omit for every child that has not reported and every own process",
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 128,
                    "items": { "type": "string" }
                },
                "timeout_ms": {
                    "description": "Omit for 300000; clamped to 3600000; 0 returns a snapshot",
                    "type": "integer",
                    "minimum": 0
                }
            }),
            &[],
        ),
        TopologyKind::JobsStop => object_schema(
            json!({
                "id": {
                    "description": "Process id returned by bash (N or pN) to stop, or child agent id to interrupt",
                    "type": "string"
                }
            }),
            &["id"],
        ),
    }
}

/// The caller-specific parts of the `spawn_agent` description.
struct SpawnDescription {
    nodes: usize,
    children: usize,
    cards: String,
}

#[allow(
    clippy::too_many_arguments,
    reason = "one registration binds every collaborator the executor owns"
)]
fn registration(
    driver: TreeDriver,
    caller_id: Arc<Mutex<String>>,
    kind: TopologyKind,
    spawn_description: Option<&SpawnDescription>,
    spawnable_templates: &[String],
    binder: Option<Arc<dyn SpawnedSessionBinder>>,
    deliverer: Option<Arc<dyn MailDeliverer>>,
    canceller: Option<Arc<dyn LiveTurnCanceller>>,
    recoverer: Option<Arc<dyn ChildRecoverer>>,
    observer: Option<Arc<dyn TopologyObserver>>,
    cell_budget: Arc<CellAgentBudget>,
    vocabulary: TopologyVocabulary,
) -> TreeRuntimeResult<ToolRegistration> {
    let name = kind.name();
    let description = match (kind, spawn_description) {
        (TopologyKind::Spawn, Some(spawn)) => {
            // The Code Mode `exec` catalog renders a nested tool as a bare
            // TypeScript declaration and drops per-field descriptions, so
            // the `output_schema_json` shape has to be stated here or a
            // Code Mode root never sees it.
            PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE
                .replace("{await}", vocabulary.presented_name("wait_agent"))
                .replace("{nodes}", &spawn.nodes.to_string())
                .replace("{children}", &spawn.children.to_string())
                .replace("{cards}", &spawn.cards)
        }
        (TopologyKind::TaskOutput, _) => PLAK_TOPOLOGY_TASK_OUTPUT_DESCRIPTION.to_owned(),
        (TopologyKind::TaskKill, _) => PLAK_TOPOLOGY_TASK_KILL_DESCRIPTION.to_owned(),
        (TopologyKind::JobsWait, _) => PLAK_TOPOLOGY_JOBS_WAIT_DESCRIPTION.to_owned(),
        (TopologyKind::JobsStop, _) => PLAK_TOPOLOGY_JOBS_STOP_DESCRIPTION.to_owned(),
        (TopologyKind::Send, _) => PLAK_TOPOLOGY_SEND_DESCRIPTION.to_owned(),
        (TopologyKind::List, _) => PLAK_TOPOLOGY_LIST_DESCRIPTION.to_owned(),
        (TopologyKind::Wait, _) => PLAK_TOPOLOGY_WAIT_DESCRIPTION.to_owned(),
        (TopologyKind::Submit, _) => PLAK_TOPOLOGY_SUBMIT_DESCRIPTION.to_owned(),
        (TopologyKind::Interrupt, _) => PLAK_TOPOLOGY_INTERRUPT_DESCRIPTION.to_owned(),
        (TopologyKind::Close, _) => PLAK_TOPOLOGY_CLOSE_DESCRIPTION.to_owned(),
        (TopologyKind::Recover, _) => PLAK_TOPOLOGY_RECOVER_DESCRIPTION.to_owned(),
        (TopologyKind::Adopt, _) => PLAK_TOPOLOGY_ADOPT_DESCRIPTION_TEMPLATE
            .replace("{await}", vocabulary.presented_name("wait_agent")),
        (TopologyKind::Discard, _) => PLAK_TOPOLOGY_DISCARD_DESCRIPTION.to_owned(),
        _ => PLAK_TOPOLOGY_OPERATION_FALLBACK_TEMPLATE.replace("{name}", name),
    };
    let definition = ToolDefinition::new(
        ToolName::new(name).map_err(|_| TreeRuntimeError::new("topology tool name is invalid"))?,
        "plak.topology.v1",
        description,
        ToolInputContract::structured(schema(kind, spawnable_templates))
            .map_err(|_| TreeRuntimeError::new("topology tool schema is invalid"))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        if kind.mutating() {
            ToolEffect::Mutating
        } else {
            ToolEffect::ReadOnly
        },
    )
    .map_err(|_| TreeRuntimeError::new("topology tool definition is invalid"))?;
    let registration = ToolRegistration::new(
        RegisteredToolId::new(format!("plak.topology.{name}.v1"))
            .map_err(|_| TreeRuntimeError::new("topology tool id is invalid"))?,
        ToolExecutorId::new(format!("plak.topology.{name}.executor.v1"))
            .map_err(|_| TreeRuntimeError::new("topology executor id is invalid"))?,
        ToolExecutorBindingIdentity::new(1, &[ISSUER, name.as_bytes()])
            .map_err(|_| TreeRuntimeError::new("topology binding identity is invalid"))?,
        definition,
        Arc::new(TopologyTool {
            driver,
            caller_id,
            kind,
            vocabulary,
            binder,
            deliverer,
            canceller,
            recoverer,
            observer,
            cell_budget,
        }),
    )
    .nested_eligible();
    // Only these registrations (Grok's output and kill, pi's wait and stop)
    // may claim the process pack's core entries of the same names. Any other
    // same-name capability is a namespace clash.
    Ok(
        if matches!(
            kind,
            TopologyKind::TaskOutput
                | TopologyKind::TaskKill
                | TopologyKind::JobsWait
                | TopologyKind::JobsStop
        ) {
            registration.presented_name_claimer()
        } else {
            registration
        },
    )
}

/// Grant-compiled Tact names plus occupancy tools, nested-eligible, bound to
/// one tree. The host decides what an extension set is; the tree hands back
/// the registrations it grants and nothing more.
pub fn topology_tool_registrations(
    tree: Arc<Mutex<ProductAgentTree>>,
    caller_id: impl Into<String>,
) -> TreeRuntimeResult<Vec<ToolRegistration>> {
    topology_tool_registrations_in(tree, caller_id, TopologyVocabulary::default())
}

/// [`topology_tool_registrations`] with the await and interrupt slots
/// spelled in `vocabulary`.
pub fn topology_tool_registrations_in(
    tree: Arc<Mutex<ProductAgentTree>>,
    caller_id: impl Into<String>,
    vocabulary: TopologyVocabulary,
) -> TreeRuntimeResult<Vec<ToolRegistration>> {
    let driver = driver_for_tree(&tree)?;
    topology_tool_registrations_on(
        driver,
        caller_id,
        TopologyHostPorts {
            vocabulary,
            ..TopologyHostPorts::default()
        },
    )
}

/// The host's session-side ports a topology tool reaches. Each one is
/// optional: a tool whose port is absent refuses rather than pretending.
#[derive(Clone, Default)]
pub struct TopologyHostPorts {
    /// Exact session-owned process services for topology waits and settlement.
    pub job_host: Option<Arc<dyn plakous::tools::JobHost>>,
    /// Opens a spawned child's session and starts its assignment turn.
    pub binder: Option<Arc<dyn SpawnedSessionBinder>>,
    /// Delivers mail onto a bound child session.
    pub deliverer: Option<Arc<dyn MailDeliverer>>,
    /// Cancels live child turns after an interrupt or close.
    pub canceller: Option<Arc<dyn LiveTurnCanceller>>,
    /// Settles a `recovery_required` child by an exact session reopen.
    pub recoverer: Option<Arc<dyn ChildRecoverer>>,
    /// Telemetry for node lifecycle.
    pub observer: Option<Arc<dyn TopologyObserver>>,
    /// How the node's pack spells the await and interrupt slots.
    pub vocabulary: TopologyVocabulary,
}

/// The registrations over an explicit driver: the tree, its store, and its port.
pub fn topology_tool_registrations_on(
    driver: TreeDriver,
    caller_id: impl Into<String>,
    ports: TopologyHostPorts,
) -> TreeRuntimeResult<Vec<ToolRegistration>> {
    driver.bind_job_host(ports.job_host.clone());
    let TopologyHostPorts {
        job_host: _,
        binder,
        deliverer,
        canceller,
        recoverer,
        observer,
        vocabulary,
    } = ports;
    let caller_id = caller_id.into();
    let (compiled, spawn_description, spawnable_templates, cell_budget) = {
        let tree = driver
            .tree
            .lock()
            .map_err(|_| TreeRuntimeError::new("topology tree lock is poisoned"))?;
        let cell_budget = tree.cell_budget();
        let compiled = tree
            .compiled_tool_names(&caller_id)
            .map_err(|error| TreeRuntimeError::new(error.message))?;
        let caller_template = tree
            .node(&caller_id)
            .map_err(|error| TreeRuntimeError::new(error.message))?
            .template
            .as_ref()
            .map(|template| template.as_str().to_owned());
        let spawn_description = if compiled
            .iter()
            .any(|name| *name == TopologyKind::Spawn.name())
        {
            let grant = tree
                .grant(&caller_id)
                .map_err(|error| TreeRuntimeError::new(error.message))?;
            Some(SpawnDescription {
                nodes: MAX_TREE_NODES.min(usize::from(grant.max_nodes)),
                children: MAX_DIRECT_CHILDREN.min(usize::from(grant.max_children)),
                cards: tree.snapshot.template_cards(caller_template.as_deref()),
            })
        } else {
            None
        };
        let spawnable_templates: Vec<String> = tree
            .snapshot
            .spawnable_template_names(caller_template.as_deref())
            .map(|name| name.as_str().to_owned())
            .collect();
        (
            compiled,
            spawn_description,
            spawnable_templates,
            cell_budget,
        )
    };
    let caller_id = Arc::new(Mutex::new(caller_id));
    let mut registrations = Vec::new();
    for slot in presentation_kinds() {
        if !compiled.iter().any(|name| *name == slot.name()) {
            continue;
        }
        registrations.push(registration(
            driver.clone(),
            Arc::clone(&caller_id),
            vocabulary.presents(slot),
            spawn_description.as_ref(),
            &spawnable_templates,
            binder.as_ref().map(Arc::clone),
            deliverer.as_ref().map(Arc::clone),
            canceller.as_ref().map(Arc::clone),
            recoverer.as_ref().map(Arc::clone),
            observer.as_ref().map(Arc::clone),
            Arc::clone(&cell_budget),
            vocabulary,
        )?);
    }
    Ok(registrations)
}

/// Idle host / JSONL `invoke_tool`: same TopologyTool executor as a model
/// call, over the hub's own driver (tree, store, and port).
///
/// Grant compile is not bypassed. Unknown coding names are rejected. Does not
/// admit a Plakous turn or write a fake tool call into the conversation prefix.
pub async fn invoke_granted_topology_tool_on(
    driver: TreeDriver,
    caller_id: String,
    name: &str,
    arguments: &Value,
    operation_id: String,
    ports: TopologyHostPorts,
) -> TreeRuntimeResult<ToolOutcome> {
    driver.bind_job_host(ports.job_host.clone());
    let TopologyHostPorts {
        job_host: _,
        binder,
        deliverer,
        canceller,
        recoverer,
        ..
    } = ports;
    let tree = Arc::clone(&driver.tree);
    let Some(kind) = TopologyKind::from_name(name) else {
        return Err(TreeRuntimeError::new(format!(
            "invoke_tool name must be a topology tool (`{name}` is not)"
        )));
    };
    let compiled = {
        let tree = tree
            .lock()
            .map_err(|_| TreeRuntimeError::new("topology tree lock is poisoned"))?;
        tree.compiled_tool_names(&caller_id)
            .map_err(|error| TreeRuntimeError::new(error.message))?
    };
    if !compiled.contains(&name) {
        return Err(TreeRuntimeError::new(format!(
            "topology tool `{name}` is not granted to `{caller_id}`"
        )));
    }
    let cell_budget = {
        let tree = tree
            .lock()
            .map_err(|_| TreeRuntimeError::new("topology tree lock is poisoned"))?;
        tree.cell_budget()
    };
    let tool = TopologyTool {
        driver,
        caller_id: Arc::new(Mutex::new(caller_id)),
        kind,
        vocabulary: TopologyVocabulary::default(),
        binder,
        deliverer,
        canceller,
        recoverer,
        observer: None,
        cell_budget,
    };
    // The idle host and JSONL `invoke_tool` are not a cell: `Direct` lineage
    // carries no cell identity, so the per-cell agent budget never applies.
    tool.dispatch(&ToolCallerLineage::Direct, operation_id, arguments)
        .await
        .map_err(|error| TreeRuntimeError::new(error.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::*;
    use crate::cell_budget::{CEILING_MAX_AGENTS_PER_CELL, DEFAULT_MAX_AGENTS_PER_CELL};
    use crate::protocol::{
        CandidateId, MessagePriority, NodeStatus, SendAgentMessageRequest, SpawnAgentRequest,
        SpawnWorkspace, TOPOLOGY_PRESENTATION_ORDER, Tier, TopologyErrorCode, WorkspaceIdRequest,
        WorkspaceMode, WorkspaceStatus, WriterMode,
    };
    use crate::tree::{MailDisposition, MailDurableState, MemoryTreeStore, ProductAgentTree};
    use plakous::model::{PortableToolSchemaV2, ToolName, ToolResultPart};
    use plakous::tools::{ToolOutcomeKind, ToolRegistryBuilder, WorkspacePolicy};
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn cascade_targets_fold_within_commit_and_restore_on_refused_save() {
        for refuse in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let store = Arc::new(SecondSaveStore {
                inner: MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(if refuse { 3 } else { usize::MAX }),
                stale: false,
            });
            let mut tree = bootstrap_tree("cascade-post-pass", root.path());
            tree.grants_mut_for_test("root").max_depth = 2;
            let parent = tree
                .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                .unwrap()
                .agent_id;
            let child = tree
                .spawn_agent(&parent, &spawn_request(), None)
                .unwrap()
                .agent_id;
            let driver = driver_on(tree, store);
            let mut tree = driver.lock().unwrap();
            driver.save(&mut tree).unwrap();
            tree.record_host_cancellation(&parent).unwrap();
            driver.save(&mut tree).unwrap();
            let before = tree.node(&child).unwrap().clone();
            let result = driver.fold_and_commit(&mut tree, |tree| {
                tree.fold_due(&parent)?;
                assert_eq!(tree.node(&child).unwrap(), &before);
                assert!(tree.pending_close_folds.contains(&child));
                Ok(())
            });
            assert_eq!(result.is_err(), refuse);
            assert!(tree.pending_close_folds.is_empty());
            if refuse {
                assert_eq!(tree.node(&child).unwrap(), &before);
                assert!(tree.take_effects().is_empty());
            } else {
                assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
            }
        }
    }

    fn spawn_request() -> SpawnAgentRequest {
        SpawnAgentRequest::test_review()
    }

    #[test]
    fn refused_startup_consequence_does_not_install_or_dispatch() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(SecondSaveStore {
            inner: MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(3),
            stale: false,
        });
        let mut tree = bootstrap_tree("startup-save-refusal", root.path());
        let child = tree
            .spawn_agent("root", &spawn_request(), None)
            .unwrap()
            .agent_id;
        tree.record_host_cancellation(&child).unwrap();
        tree.acknowledge_revision(store.save(&tree).unwrap());
        let id = tree.tree_id.clone();
        let loaded = ProductAgentTree::reopen(store.load(&id).unwrap().unwrap());
        let hub = crate::hub::TopologyHub::new();
        assert!(
            hub.insert_root(&id, loaded, store.clone(), tree.vcs_port(), None)
                .is_err()
        );
        assert!(hub.lookup(&id).is_none());
        let stored = store.load(&id).unwrap().unwrap();
        assert_eq!(
            stored.rows().node(&child).unwrap().status,
            NodeStatus::Running
        );
        assert!(
            stored
                .rows()
                .operations()
                .values()
                .filter(|operation| operation.fold.is_some())
                .all(|operation| operation.terminal_response.is_none())
        );
    }

    #[test]
    fn scratch_and_refused_folds_do_not_publish_workspace_generation() {
        use plakous_workspace::occupancy_vcs::fake::FakeVcs;
        for scratch in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let vcs = Arc::new(FakeVcs::new());
            let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
                vcs.clone(),
                "generation-boundary",
                "root",
                "owner",
                root.path().to_owned(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
            binding.max_tier = Tier::Mutate;
            tree.pin_template("implement", binding);
            let child = tree
                .spawn_agent("root", &mint_spawn_request(), None)
                .unwrap()
                .agent_id;
            let counter = tree
                .generation_counter_for_root(&tree.node(&child).unwrap().path)
                .unwrap();
            let store = Arc::new(SecondSaveStore {
                inner: MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(2),
                stale: false,
            });
            let driver = driver_on(tree, store);
            let mut tree = driver.lock().unwrap();
            driver.save(&mut tree).unwrap();
            if scratch {
                tree.clone().record_turn_completed(&child).unwrap();
            } else {
                assert!(
                    driver
                        .fold_and_commit(&mut tree, |tree| tree.record_turn_completed(&child))
                        .is_err()
                );
            }
            assert_eq!(counter.load(Ordering::SeqCst), 0);
            assert_eq!(
                tree.workspace(&tree.node(&child).unwrap().workspace_id)
                    .unwrap()
                    .generation,
                0
            );
            if !scratch {
                driver
                    .fold_and_commit(&mut tree, |tree| tree.record_turn_completed(&child))
                    .unwrap();
                assert_eq!(counter.load(Ordering::SeqCst), 1);
            }
        }
    }

    fn bootstrap_tree(tree_id: &str, root: &Path) -> ProductAgentTree {
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id,
            "root",
            "owner",
            root.to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        tree.pin_bind_hints_for_test(crate::epoch::BindHints::from_names(
            &[
                "codex",
                "grok",
                "openrouter",
                crate::epoch::RosterInput::TEST_ACCOUNT,
            ],
            &["openrouter"],
        ));
        tree
    }

    /// Pins `review` to an explicit account/tuple/effort.
    fn pin_review(tree: &mut ProductAgentTree, account: &str, tuple: &str, effort: Option<&str>) {
        let mut binding = RosterBinding::leaf(account, tuple);
        binding.effort = effort.map(str::to_owned);
        binding.max_tier = Tier::ReadOnly;
        tree.pin_template("review", binding);
    }

    fn child_path(tree: &Arc<Mutex<ProductAgentTree>>, agent_id: &str) -> std::path::PathBuf {
        tree.lock().unwrap().node(agent_id).unwrap().path.clone()
    }

    #[test]
    fn topology_kinds_match_presentation_order() {
        assert_eq!(
            presentation_kinds().map(TopologyKind::name).as_slice(),
            TOPOLOGY_PRESENTATION_ORDER
        );
    }

    /// A tree published once, plus the store handle the test's driver
    /// publishes through.
    ///
    /// The store is the port, not a file: what these tests assert is that the
    /// tree published its rows and that they load back. Which implementation
    /// backs the port is the conformance suite's business, and it runs the
    /// same contract over the memory store and over SQLite.
    fn persist_fixture(root: &tempfile::TempDir) -> (Arc<dyn TreeStore>, ProductAgentTree) {
        let store: Arc<dyn TreeStore> = Arc::new(MemoryTreeStore::new());
        let mut tree = bootstrap_tree("tree1", root.path());
        tree.acknowledge_revision(store.save(&tree).unwrap());
        (store, tree)
    }

    /// The tree as the store holds it, with the reopen law applied.
    fn reload(store: &Arc<dyn TreeStore>, tree_id: &str) -> ProductAgentTree {
        ProductAgentTree::reopen(store.load(tree_id).unwrap().unwrap())
    }

    /// The driver a tool runs on: the tree, `store`, and the tree's own port.
    fn driver_on(tree: ProductAgentTree, store: Arc<dyn TreeStore>) -> TreeDriver {
        let vcs = tree.vcs_port();
        TreeDriver::new(Arc::new(Mutex::new(tree)), store, vcs, None)
    }

    struct StubBinder {
        binds: AtomicUsize,
        result: Result<SpawnBound, SpawnBindError>,
    }

    impl StubBinder {
        fn ok() -> Self {
            Self {
                binds: AtomicUsize::new(0),
                result: Ok(SpawnBound {
                    durably_registered: false,
                    session_locator: "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001".into(),
                    turn_id: Some("turn-c1".into()),
                }),
            }
        }

        /// Binds a session without a started turn: the shape mail `Started`
        /// still serves when a bound child has no turn (recovered reopen).
        fn bind_only() -> Self {
            Self {
                binds: AtomicUsize::new(0),
                result: Ok(SpawnBound {
                    durably_registered: false,
                    session_locator: "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001".into(),
                    turn_id: None,
                }),
            }
        }

        fn fail() -> Self {
            Self {
                binds: AtomicUsize::new(0),
                result: Err("child session could not be created".into()),
            }
        }
    }

    impl SpawnedSessionBinder for StubBinder {
        fn bind(
            &self,
            start: SpawnStart,
        ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>> {
            self.binds.fetch_add(1, Ordering::SeqCst);
            assert!(
                !start.prompt.is_empty(),
                "spawn start carries the prompt brief"
            );
            assert_eq!(start.turn_token, 1);
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    struct RelockingBinder {
        tree: Arc<Mutex<ProductAgentTree>>,
        binds: AtomicUsize,
    }

    impl SpawnedSessionBinder for RelockingBinder {
        fn admit_spawn(&self, request: &SpawnAgentRequest) -> Result<(), TopologyError> {
            let tree = self.tree.lock().map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "topology tree lock is poisoned",
                )
            })?;
            tree.snapshot
                .binding(&request.template)
                .map(|_| ())
                .ok_or_else(|| {
                    TopologyError::new(
                        TopologyErrorCode::TemplateUnknown,
                        "template is not in this tree epoch's roster",
                    )
                })
        }

        fn bind(
            &self,
            _start: SpawnStart,
        ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>> {
            self.binds.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(SpawnBound {
                    durably_registered: false,
                    session_locator: "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001".into(),
                    turn_id: None,
                })
            })
        }
    }

    /// A budget nothing in these fixtures shares, so a test that does not
    /// exercise the bound cannot be perturbed by one that does.
    fn test_budget() -> Arc<CellAgentBudget> {
        Arc::new(CellAgentBudget::new(DEFAULT_MAX_AGENTS_PER_CELL))
    }

    fn spawn_tool(
        driver: TreeDriver,
        binder: Option<Arc<dyn SpawnedSessionBinder>>,
    ) -> TopologyTool {
        TopologyTool {
            driver,
            caller_id: Arc::new(Mutex::new("root".into())),
            kind: TopologyKind::Spawn,
            vocabulary: TopologyVocabulary::default(),
            binder,
            deliverer: None,
            canceller: None,
            recoverer: None,
            observer: None,
            cell_budget: test_budget(),
        }
    }

    fn wait_tool(driver: TreeDriver) -> TopologyTool {
        TopologyTool {
            driver,
            caller_id: Arc::new(Mutex::new("root".into())),
            kind: TopologyKind::Wait,
            vocabulary: TopologyVocabulary::default(),
            binder: None,
            deliverer: None,
            canceller: None,
            recoverer: None,
            observer: None,
            cell_budget: test_budget(),
        }
    }

    fn send_tool(driver: TreeDriver, deliverer: Option<Arc<dyn MailDeliverer>>) -> TopologyTool {
        struct ProjectedDeliverer {
            inner: Arc<dyn MailDeliverer>,
            driver: TreeDriver,
        }
        impl MailDeliverer for ProjectedDeliverer {
            fn withdraw_followup(
                &self,
                driver: TreeDriver,
                agent_id: String,
                message_id: String,
                receipt: Option<(String, TopologyErrorCode)>,
            ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + '_>> {
                self.inner
                    .withdraw_followup(driver, agent_id, message_id, receipt)
            }
            fn deliver(
                &self,
                request: MailDeliveryRequest,
            ) -> Pin<
                Box<dyn Future<Output = Result<MailDeliveryAck, MailDeliveryFailure>> + Send + '_>,
            > {
                Box::pin(async move {
                    use plakous::sessions::feed::{FeedFact, FeedRecord};
                    let ack = self.inner.deliver(request.clone()).await?;
                    let command = format!("fixture:mail:{}", request.message_id);
                    self.driver.mutate_and_settle(|tree| {
                        let session = tree.node(&request.agent_id)?.session_locator.as_deref()
                            .and_then(|locator| locator.parse::<plakous::sessions::SessionLocator>().ok())
                            .map_or_else(|| "00000000-0000-4000-8000-000000000001".to_owned(), |locator| locator.session_id().to_string());
                        let seq = tree.feed_cursor(&session) + 1;
                        let header = serde_json::from_value(json!({
                            "session_id": session, "seq": seq,
                            "branch_id": "00000000-0000-4000-8000-000000000002", "committed_at_ms": seq,
                            "change": {"created": {"genesis": "00000000-0000-4000-8000-000000000003", "revision": seq}}
                        })).unwrap();
                        tree.record_command(crate::tree::AgentCommand::intent(&command, &request.agent_id, None,
                            crate::tree::AgentCommandKind::MailStart, request.body.clone(), vec![request.message_id.clone()]))?;
                        let record = FeedRecord::from_store(header, vec![FeedFact::CommandAccepted {
                            command_id: plakous::agent::CommandId::new(&command).unwrap(),
                            kind: plakous::sessions::feed::FeedCommandKind::Root { origin: plakous::model::TurnOrigin::UserPrompt },
                            turn_id: None, sequence: seq, operation_id: None,
                        }]);
                        tree.project_feed(&request.agent_id, &session, &[record], &crate::tree::TreeFeedReads::default()).map_err(TopologyError::from)?;
                        Ok::<_, TopologyError>(())
                    }).map_err(|error| MailDeliveryFailure::RecoveryRequired(error.message))?.0
                        .map_err(|error| MailDeliveryFailure::RecoveryRequired(error.message))?;
                    Ok(ack)
                })
            }
        }
        let deliverer = deliverer.map(|inner| {
            Arc::new(ProjectedDeliverer {
                inner,
                driver: driver.clone(),
            }) as Arc<dyn MailDeliverer>
        });
        TopologyTool {
            driver,
            caller_id: Arc::new(Mutex::new("root".into())),
            kind: TopologyKind::Send,
            vocabulary: TopologyVocabulary::default(),
            binder: None,
            deliverer,
            canceller: None,
            recoverer: None,
            observer: None,
            cell_budget: test_budget(),
        }
    }

    fn interrupt_tool(
        driver: TreeDriver,
        canceller: Option<Arc<dyn LiveTurnCanceller>>,
    ) -> TopologyTool {
        TopologyTool {
            driver,
            caller_id: Arc::new(Mutex::new("root".into())),
            kind: TopologyKind::Interrupt,
            vocabulary: TopologyVocabulary::default(),
            binder: None,
            deliverer: None,
            canceller,
            recoverer: None,
            observer: None,
            cell_budget: test_budget(),
        }
    }

    fn close_tool(
        driver: TreeDriver,
        canceller: Option<Arc<dyn LiveTurnCanceller>>,
    ) -> TopologyTool {
        TopologyTool {
            driver,
            caller_id: Arc::new(Mutex::new("root".into())),
            kind: TopologyKind::Close,
            vocabulary: TopologyVocabulary::default(),
            binder: None,
            deliverer: None,
            canceller,
            recoverer: None,
            observer: None,
            cell_budget: test_budget(),
        }
    }

    fn adopt_tool(driver: TreeDriver) -> TopologyTool {
        TopologyTool {
            driver,
            caller_id: Arc::new(Mutex::new("root".into())),
            kind: TopologyKind::Adopt,
            vocabulary: TopologyVocabulary::default(),
            binder: None,
            deliverer: None,
            canceller: None,
            recoverer: None,
            observer: None,
            cell_budget: test_budget(),
        }
    }

    /// The registry a caller's granted registrations compile into. What the
    /// host wraps them in — an extension set, an agreement pack — is the
    /// host's; the tree's output is the registrations.
    fn registry_for(
        tree: Arc<Mutex<ProductAgentTree>>,
        caller_id: impl Into<String>,
    ) -> plakous::tools::FrozenToolRegistry {
        let mut builder = ToolRegistryBuilder::default();
        for registration in topology_tool_registrations(tree, caller_id).unwrap() {
            builder.register(registration).unwrap();
        }
        builder.freeze()
    }

    /// The names a caller is granted, in registration order.
    fn granted_names(
        tree: Arc<Mutex<ProductAgentTree>>,
        caller_id: impl Into<String>,
    ) -> Vec<String> {
        topology_tool_registrations(tree, caller_id)
            .unwrap()
            .iter()
            .map(|registration| registration.name().as_str().to_owned())
            .collect()
    }

    /// A [`TopologyObserver`] that records what the tree told it. The product's
    /// mapping onto its own telemetry vocabulary is asserted where that
    /// mapping lives (`plak-host`'s `topology_telemetry`); what the tree owes
    /// is that it reports the spawn and the detach at all, exactly once.
    /// One reported spawn: tree, node, parent, template.
    type SpawnReport = (String, String, Option<String>, String);
    /// One reported detach: tree, node, status.
    type DetachReport = (String, String, NodeDetach);

    #[derive(Default)]
    struct RecordingObserver {
        spawned: Mutex<Vec<SpawnReport>>,
        detached: Mutex<Vec<DetachReport>>,
    }

    impl TopologyObserver for RecordingObserver {
        fn turn_usage(&self, _terminal: crate::tree::ProjectedChildTurnUsage) {}
        fn node_spawned(
            &self,
            tree_id: &str,
            node_id: &str,
            parent_node_id: Option<&str>,
            template: &str,
        ) {
            self.spawned.lock().unwrap().push((
                tree_id.to_owned(),
                node_id.to_owned(),
                parent_node_id.map(str::to_owned),
                template.to_owned(),
            ));
        }

        fn node_detached(
            &self,
            tree_id: &str,
            node_id: &str,
            _parent_node_id: Option<&str>,
            status: NodeDetach,
        ) {
            self.detached
                .lock()
                .unwrap()
                .push((tree_id.to_owned(), node_id.to_owned(), status));
        }

        fn epoch_advanced(&self, _tree_id: &str) {}
    }

    fn list_tool(driver: TreeDriver) -> TopologyTool {
        TopologyTool {
            driver,
            caller_id: Arc::new(Mutex::new("root".into())),
            kind: TopologyKind::List,
            vocabulary: TopologyVocabulary::default(),
            binder: None,
            deliverer: None,
            canceller: None,
            recoverer: None,
            observer: None,
            cell_budget: test_budget(),
        }
    }

    fn discard_tool(driver: TreeDriver) -> TopologyTool {
        TopologyTool {
            driver,
            caller_id: Arc::new(Mutex::new("root".into())),
            kind: TopologyKind::Discard,
            vocabulary: TopologyVocabulary::default(),
            binder: None,
            deliverer: None,
            canceller: None,
            recoverer: None,
            observer: None,
            cell_budget: test_budget(),
        }
    }

    fn workspace_request(workspace_id: &str) -> Value {
        serde_json::to_value(WorkspaceIdRequest {
            workspace_id: workspace_id.into(),
        })
        .unwrap()
    }

    fn mint_spawn_request() -> SpawnAgentRequest {
        let mut request = SpawnAgentRequest::test_named(
            "implement",
            Tier::Mutate,
            "implementer",
            "edit the parser",
        );
        request.workspace = SpawnWorkspace::Fork;
        request
    }

    fn git_repo() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-b", "main"])
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "user.email", "plak@example.test"])
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "user.name", "plak"])
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "commit.gpgsign", "false"])
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "SEED.txt"])
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "seed"])
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
        root
    }

    fn git_persist_fixture() -> (tempfile::TempDir, Arc<dyn TreeStore>, ProductAgentTree) {
        let root = git_repo();
        let (store, tree) = persist_fixture(&root);
        (root, store, tree)
    }

    fn jj_repo() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        assert!(
            Command::new("jj")
                .args(["--no-pager", "git", "init"])
                .args([
                    "--config",
                    "user.name=plak",
                    "--config",
                    "user.email=plak@local"
                ])
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
        assert!(
            Command::new("jj")
                .args(["--no-pager", "commit", "-m", "seed"])
                .args([
                    "--config",
                    "user.name=plak",
                    "--config",
                    "user.email=plak@local"
                ])
                .env("JJ_CONFIG", "/dev/null")
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
        root
    }

    fn jj_persist_fixture() -> (tempfile::TempDir, Arc<dyn TreeStore>, ProductAgentTree) {
        let root = jj_repo();
        let (store, tree) = persist_fixture(&root);
        (root, store, tree)
    }

    fn send_request(agent_id: &str, message: &str, priority: Option<MessagePriority>) -> Value {
        serde_json::to_value(SendAgentMessageRequest {
            agent_id: agent_id.into(),
            message: message.into(),
            purpose: None,
            priority,
            in_reply_to: None,
            output_schema_json: None,
        })
        .unwrap()
    }

    fn outcome_json(outcome: &ToolOutcome) -> Value {
        let ToolResultPart::Text(block) = &outcome.parts()[0] else {
            panic!("expected text tool outcome");
        };
        serde_json::from_str(block.text()).unwrap()
    }

    struct StubDeliverer {
        calls: Mutex<Vec<MailDeliveryRequest>>,
        result: Result<MailDeliveryAck, MailDeliveryFailure>,
        /// Answers for the first calls, in order, before `result` repeats.
        first: Mutex<std::collections::VecDeque<Result<MailDeliveryAck, MailDeliveryFailure>>>,
        /// Calls from this index on answer only once the test adds a permit.
        hold: Option<(usize, Arc<tokio::sync::Semaphore>)>,
        /// The tree an admitted Start binds its turn on at admission, as the
        /// product deliverer does (`MailDeliverer`).
        tree: Option<Arc<Mutex<ProductAgentTree>>>,
    }

    impl StubDeliverer {
        fn ok() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                first: Mutex::default(),
                hold: None,
                tree: None,
                result: Ok(MailDeliveryAck {
                    turn_id: Some("turn-1".into()),
                }),
            }
        }

        fn fail() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                first: Mutex::default(),
                hold: None,
                tree: None,
                result: Err("child turn could not be admitted".into()),
            }
        }

        fn turn_ended() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                first: Mutex::default(),
                hold: None,
                tree: None,
                result: Err(MailDeliveryFailure::TurnEnded),
            }
        }

        /// The turn ends under the first delivery; later ones start a turn.
        fn turn_ended_then_ok() -> Self {
            let stub = Self::ok();
            stub.first
                .lock()
                .unwrap()
                .push_back(Err(MailDeliveryFailure::TurnEnded));
            stub
        }

        /// The turn ends under the first delivery; the restart after it fails.
        fn turn_ended_then_fail() -> Self {
            let stub = Self::fail();
            stub.first
                .lock()
                .unwrap()
                .push_back(Err(MailDeliveryFailure::TurnEnded));
            stub
        }

        /// Holds every call from `index` on until the returned gate gets a
        /// permit per call.
        fn holding_from(mut self, index: usize) -> (Self, Arc<tokio::sync::Semaphore>) {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            self.hold = Some((index, Arc::clone(&gate)));
            (self, gate)
        }

        /// Admitted Starts bind their turn on `tree` and mark it active
        /// as the call returns, as the product deliverer does.
        fn binding(mut self, tree: &Arc<Mutex<ProductAgentTree>>) -> Self {
            self.tree = Some(Arc::clone(tree));
            self
        }

        fn refuse() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                first: Mutex::default(),
                hold: None,
                tree: None,
                result: Err(MailDeliveryFailure::Refused(
                    "child turn was refused before admission".into(),
                )),
            }
        }
    }

    impl MailDeliverer for StubDeliverer {
        fn withdraw_followup(
            &self,
            driver: TreeDriver,
            agent_id: String,
            message_id: String,
            receipt: Option<(String, TopologyErrorCode)>,
        ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + '_>> {
            Box::pin(async move {
                let (session, tail) = driver.read(|tree| {
                    let session = tree
                        .node(&agent_id)
                        .unwrap()
                        .session_locator
                        .as_ref()
                        .unwrap()
                        .parse::<plakous::sessions::SessionLocator>()
                        .unwrap()
                        .session_id()
                        .to_string();
                    let tail = tree.feed_cursor(&session);
                    (session, tail)
                })?;
                driver.withdraw_unstarted_followup(&agent_id, &message_id, &session, tail, receipt)
            })
        }
        fn deliver(
            &self,
            request: MailDeliveryRequest,
        ) -> Pin<Box<dyn Future<Output = Result<MailDeliveryAck, MailDeliveryFailure>> + Send + '_>>
        {
            let started =
                (request.disposition == MailDisposition::Started).then(|| request.agent_id.clone());
            let index = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(request);
                calls.len() - 1
            };
            let result = self
                .first
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.result.clone());
            let gate = self
                .hold
                .as_ref()
                .filter(|(from, _)| index >= *from)
                .map(|(_, gate)| Arc::clone(gate));
            let tree = self.tree.clone();
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.acquire().await.unwrap().forget();
                }
                // The Start is admitted as the call returns.
                if let (
                    Some(tree),
                    Some(agent),
                    Ok(MailDeliveryAck {
                        turn_id: Some(turn),
                    }),
                ) = (tree, started, &result)
                {
                    let mut tree = tree.lock().unwrap();
                    tree.set_active_turn(&agent, true).unwrap();
                    tree.bind_attempt_turn(&agent, turn.clone()).unwrap();
                }
                result
            })
        }
    }

    struct StubCanceller {
        calls: Mutex<Vec<Vec<String>>>,
        fail_requested: bool,
        fail_empty_ids: bool,
    }

    impl StubCanceller {
        fn ok() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_requested: false,
                fail_empty_ids: false,
            }
        }

        fn fail() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_requested: true,
                fail_empty_ids: false,
            }
        }

        fn fail_without_ids() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_requested: false,
                fail_empty_ids: true,
            }
        }
    }

    impl LiveTurnCanceller for StubCanceller {
        fn cancel_agents(
            &self,
            agent_ids: Vec<String>,
        ) -> Pin<Box<dyn Future<Output = Result<LiveCancelOutcome, LiveCancelFailure>> + Send + '_>>
        {
            self.calls.lock().unwrap().push(agent_ids.clone());
            let result = if self.fail_empty_ids {
                Err(LiveCancelFailure {
                    message: "child turn could not be cancelled".into(),
                    failed_ids: Vec::new(),
                    joiner_expired_ids: Vec::new(),
                    owner_failures: Vec::new(),
                    cancelled_ids: Vec::new(),
                    not_live_ids: agent_ids,
                })
            } else if self.fail_requested {
                Err(LiveCancelFailure {
                    message: "child turn could not be cancelled".into(),
                    owner_failures: agent_ids
                        .iter()
                        .map(|id| crate::hub::LiveRetirementFailure {
                            agent_id: id.clone(),
                            expected_attempt_id: Some(crate::protocol::AttemptId::new(id, 1)),
                            message: "child turn could not be cancelled".into(),
                        })
                        .collect(),
                    failed_ids: agent_ids,
                    joiner_expired_ids: Vec::new(),
                    cancelled_ids: Vec::new(),
                    not_live_ids: Vec::new(),
                })
            } else {
                Ok(LiveCancelOutcome {
                    cancelled_ids: agent_ids,
                    not_live_ids: Vec::new(),
                })
            };
            Box::pin(async move { result })
        }
    }

    fn agent_id_request(agent_id: &str) -> Value {
        serde_json::to_value(AttemptScopedRequest {
            agent_id: agent_id.into(),
            expected_attempt_id: None,
        })
        .unwrap()
    }

    #[test]
    fn root_catalog_omits_submit_result() {
        let root = tempfile::tempdir().unwrap();
        let tree = bootstrap_tree("tree1", root.path());
        let names = granted_names(Arc::new(Mutex::new(tree)), "root");
        assert!(names.iter().any(|name| name == "spawn_agent"));
        assert!(names.iter().any(|name| name == "adopt_candidate"));
        assert!(!names.iter().any(|name| name == "merge_workspace"));
        assert!(!names.iter().any(|name| name == "submit_result"));
    }

    #[test]
    fn spawn_template_enum_lists_only_the_spawnable_working_set() {
        let spawnable = vec!["review".to_owned(), "docs".to_owned()];
        let spawn_schema = schema(TopologyKind::Spawn, &spawnable);
        let template_enum = spawn_schema["properties"]["template"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(
            template_enum
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["review", "docs"]
        );
        assert_eq!(
            spawn_schema["required"],
            json!(["template", "prompt", "output_schema_json", "tier"])
        );
        assert_eq!(
            spawn_schema["properties"]["tier"]["enum"],
            json!(["read_only", "execute", "mutate"])
        );
        for removed in [
            "class",
            "workspace_mode",
            "workspace_from",
            "workspace_id",
            "write_effect",
        ] {
            assert!(
                spawn_schema["properties"].get(removed).is_none(),
                "{removed}"
            );
        }
        let unpinned_schema = schema(TopologyKind::Spawn, &[]);
        assert_eq!(
            unpinned_schema["properties"]["template"]["enum"]
                .as_array()
                .unwrap()
                .as_slice(),
            [] as [Value; 0]
        );
    }

    /// The menu excludes the caller's own template, so a spawned
    /// supervisor's `spawn_agent` cards differ from the root's.
    #[test]
    fn spawn_cards_exclude_the_callers_own_template() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("tree1", root.path());
        tree.grants_mut_for_test("root").max_depth = 2;
        let spawned = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        let shared = Arc::new(Mutex::new(tree));
        let root_registry = registry_for(Arc::clone(&shared), "root");
        let child_registry = registry_for(shared, spawned.agent_id);
        let name = ToolName::new("spawn_agent").unwrap();
        for (registry, expect_orchestrate) in [(root_registry, true), (child_registry, false)] {
            let description = registry.definition(&name).unwrap().description();
            assert_eq!(
                description.contains("- orchestrate:"),
                expect_orchestrate,
                "{description}"
            );
            assert!(description.contains("- review:"));
        }
    }

    #[test]
    fn spawn_tool_description_includes_template_cards_not_accounts() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("tree1", root.path());
        pin_review(&mut tree, "grok", "grok-4", None);
        let registry = registry_for(Arc::new(Mutex::new(tree)), "root");
        let name = ToolName::new("spawn_agent").unwrap();
        let description = registry.definition(&name).unwrap().description();
        assert!(description.contains("Spawn a child from a template"));
        // The child starts at spawn; the card must not send the root to mail.
        assert!(description.contains("Do not call send_agent_message to start it"));
        assert!(description.contains("output_json"));
        assert!(description.contains("- review:"));
        assert!(description.contains("tier ≤ read_only"));
        assert!(!description.contains("grok"));
        assert!(!description.contains("account:"));
        assert!(!description.contains("merge_workspace"));
    }

    /// `tier` and the `output_schema_json` example are each stated once: in the
    /// tool description, which every composition renders, and not again in the
    /// property descriptions beside it.
    #[test]
    fn spawn_documents_tier_and_output_schema_exactly_once() {
        let spawn_schema = schema(TopologyKind::Spawn, &[]);
        let description = PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE;
        let output = spawn_schema["properties"]["output_schema_json"]["description"]
            .as_str()
            .unwrap();
        assert!(
            description.contains("Required fields: template, prompt, output_schema_json, tier")
        );
        assert!(description.contains("children share your checkout"));
        assert!(description.contains("workspace=fork"));
        assert!(description.contains("clipped to the template ceiling"));
        assert_eq!(
            spawn_schema["properties"]["workspace"]["enum"],
            json!(["inherit", "fork"])
        );
        assert!(
            spawn_schema["properties"]["inputs"]["description"]
                .as_str()
                .unwrap()
                .contains("workspace=fork")
        );
        assert!(!description.contains("mint_unavailable"));
        assert!(
            spawn_schema["properties"]["tier"]
                .get("description")
                .is_none()
        );
        // The advice must quote a shape the admission actually accepts. The
        // keywords admission fills in are stated by the refusal, not here
        // (luna emitted the example shape and was refused).
        assert!(description.contains(crate::tree::OUTPUT_SCHEMA_EXAMPLE));
        assert!(!output.contains(crate::tree::OUTPUT_SCHEMA_EXAMPLE));
        assert!(output.contains("no $ref and no combinators"));
        assert!(!output.contains("filled in when omitted"));
        assert!(!output.contains("PortableToolSchemaV2"));
        let mut example: Value = serde_json::from_str(crate::tree::OUTPUT_SCHEMA_EXAMPLE).unwrap();
        example["$schema"] = crate::tree::OUTPUT_SCHEMA_DIALECT.into();
        example["additionalProperties"] = false.into();
        assert!(PortableToolSchemaV2::new(example).is_ok());
    }

    #[test]
    fn child_catalog_includes_submit_and_omits_spawn() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("tree1", root.path());
        let spawned = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        let names = granted_names(Arc::new(Mutex::new(tree)), spawned.agent_id);
        assert!(names.iter().any(|name| name == "submit_result"));
        assert!(names.iter().any(|name| name == "send_agent_message"));
        assert!(!names.iter().any(|name| name == "spawn_agent"));
        assert!(!names.iter().any(|name| name == "adopt_candidate"));
    }

    #[test]
    fn spawn_template_binding_resolves_account_and_tuple() {
        let root = tempfile::tempdir().unwrap();
        let (_store, mut tree) = persist_fixture(&root);
        tree.set_node_account_for_test("root", "codex");
        pin_review(&mut tree, "grok", "grok-4", Some("high"));
        let spawned = tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let node = tree.node(&spawned.agent_id).unwrap();
        assert_eq!(node.account, "grok");
        assert_eq!(node.tuple_id, "grok-4");
        assert_eq!(node.effort.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn spawn_admit_may_relock_hub_tree_without_deadlock() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let binder = Arc::new(RelockingBinder {
            tree: Arc::clone(&tree),
            binds: AtomicUsize::new(0),
        });
        let tool = spawn_tool(
            driver.clone(),
            Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
        );
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tool.spawn_with("op-spawn-relock".into(), &arguments),
        )
        .await
        .expect("spawn deadlocked on nested tree lock")
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
    }

    struct NotReadyBinder {
        binds: AtomicUsize,
    }

    struct ChangingReadinessBinder {
        ready: AtomicBool,
        admissions: AtomicUsize,
        binds: AtomicUsize,
    }

    impl SpawnedSessionBinder for ChangingReadinessBinder {
        fn admit_binding<'a>(
            &'a self,
            _template: &'a TemplateName,
            _binding: &'a RosterBinding,
        ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + 'a>> {
            self.admissions.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                if self.ready.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(TopologyError::new(
                        TopologyErrorCode::ModelNotAdmitted,
                        "account is no longer Ready",
                    ))
                }
            })
        }

        fn bind(
            &self,
            _start: SpawnStart,
        ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>> {
            self.binds.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(SpawnBound {
                    durably_registered: false,
                    session_locator: "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001".into(),
                    turn_id: Some("turn-c1".into()),
                })
            })
        }
    }

    #[tokio::test]
    async fn spawn_receipt_replays_after_account_readiness_changes() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let binder = Arc::new(ChangingReadinessBinder {
            ready: AtomicBool::new(true),
            admissions: AtomicUsize::new(0),
            binds: AtomicUsize::new(0),
        });
        let tool = spawn_tool(
            driver,
            Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
        );
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        let first = tool
            .spawn_with("ready-replay".into(), &arguments)
            .await
            .unwrap();
        assert_eq!(first.kind(), ToolOutcomeKind::Success);
        binder.ready.store(false, Ordering::SeqCst);
        let replay = tool
            .spawn_with("ready-replay".into(), &arguments)
            .await
            .unwrap();
        assert_eq!(replay.kind(), ToolOutcomeKind::Success);
        assert_eq!(outcome_json(&first), outcome_json(&replay));
        assert_eq!(binder.admissions.load(Ordering::SeqCst), 1);
        assert_eq!(binder.binds.load(Ordering::SeqCst), 1);

        let mut different = spawn_request();
        different.description.push_str(" changed");
        let conflict = tool
            .spawn_with(
                "ready-replay".into(),
                &serde_json::to_value(different).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(conflict.kind(), ToolOutcomeKind::Failure);
        assert_eq!(
            outcome_json(&conflict)["code"],
            "operation_recovery_required"
        );
        assert_eq!(binder.admissions.load(Ordering::SeqCst), 1);
        assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
    }

    impl SpawnedSessionBinder for NotReadyBinder {
        fn admit_binding<'a>(
            &'a self,
            _template: &'a TemplateName,
            _binding: &'a RosterBinding,
        ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + 'a>> {
            Box::pin(async {
                Err(TopologyError::new(
                    TopologyErrorCode::ModelNotAdmitted,
                    "template review binds account codex, which is not signed in and Ready",
                ))
            })
        }

        fn bind(
            &self,
            _start: SpawnStart,
        ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>> {
            self.binds.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(SpawnBindError::Refused("unreachable".into())) })
        }
    }

    /// An account that is not Ready is refused before the tree admits a
    /// node: no node, attempt, or bind is spent, and the tree stays live.
    #[tokio::test]
    async fn a_not_ready_account_is_refused_before_any_node_is_admitted() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let binder = Arc::new(NotReadyBinder {
            binds: AtomicUsize::new(0),
        });
        let tool = spawn_tool(
            driver.clone(),
            Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
        );
        let outcome = tool
            .spawn_with(
                "op-spawn-not-ready".into(),
                &serde_json::to_value(spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&outcome)["code"], "model_not_admitted");
        assert_eq!(binder.binds.load(Ordering::SeqCst), 0);
        let tree = tree.lock().unwrap();
        assert!(tree.node("c1").is_err(), "no node was admitted");
        assert_eq!(tree.state, crate::protocol::TreeAdmissionState::Live);
    }

    /// Counts every binder entry. An off-menu spawn must not reach either.
    struct MenuProbeBinder {
        admits: AtomicUsize,
    }

    impl SpawnedSessionBinder for MenuProbeBinder {
        fn admit_spawn(&self, _request: &SpawnAgentRequest) -> Result<(), TopologyError> {
            self.admits.fetch_add(1, Ordering::SeqCst);
            Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "template account is not a supported tree account",
            ))
        }

        fn admit_binding<'a>(
            &'a self,
            _template: &'a TemplateName,
            _binding: &'a RosterBinding,
        ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + 'a>> {
            self.admits.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(TopologyError::new(
                    TopologyErrorCode::ModelNotAdmitted,
                    "template binds an account which is not signed in and Ready",
                ))
            })
        }

        fn bind(
            &self,
            _start: SpawnStart,
        ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>> {
            self.admits.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(SpawnBindError::Refused("unreachable".into())) })
        }
    }

    /// Puts `nested` in the epoch only because `orchestrate` names it, not on
    /// the root's working set, and narrows that card so it does not name
    /// `review`.
    fn closure_only_nested(tree: &mut ProductAgentTree) {
        let name = |raw: &str| TemplateName::parse(raw).unwrap();
        let mut nested =
            RosterBinding::leaf(crate::epoch::RosterInput::TEST_ACCOUNT, "selected-tuple");
        nested.template = "nested".into();
        tree.snapshot.roster.insert(name("nested"), nested);
        tree.snapshot
            .roster
            .get_mut(&name("orchestrate"))
            .unwrap()
            .spawns = vec![name("nested")];
        tree.snapshot.digest = tree.snapshot.compute_digest();
    }

    /// The root naming a closure-only template is `template_not_spawnable`
    /// before a not-Ready binder runs.
    #[tokio::test]
    async fn root_off_menu_spawn_refuses_before_the_binder_runs() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        closure_only_nested(&mut tree);
        let driver = driver_on(tree, Arc::clone(&store));
        let binder = Arc::new(MenuProbeBinder {
            admits: AtomicUsize::new(0),
        });
        let tool = spawn_tool(
            driver,
            Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
        );
        let request =
            SpawnAgentRequest::test_named("nested", Tier::Mutate, "nested", "do the nested work");
        let outcome = tool
            .spawn_with(
                "op-root-off-menu".into(),
                &serde_json::to_value(request).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&outcome)["code"], "template_not_spawnable");
        assert_eq!(binder.admits.load(Ordering::SeqCst), 0);
    }

    /// A child naming a working-set template its card omits is
    /// `template_not_spawnable`, and the binder still does not run.
    #[tokio::test]
    async fn child_off_menu_spawn_refuses_before_the_binder_runs() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        closure_only_nested(&mut tree);
        tree.grants_mut_for_test("root").max_depth = 3;
        let supervisor = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let binder = Arc::new(MenuProbeBinder {
            admits: AtomicUsize::new(0),
        });
        let tool = spawn_tool(
            driver,
            Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
        );
        *tool.caller_id.lock().unwrap() = supervisor.agent_id;
        let outcome = tool
            .spawn_with(
                "op-child-off-menu".into(),
                &serde_json::to_value(spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&outcome)["code"], "template_not_spawnable");
        assert_eq!(binder.admits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn spawn_binder_binds_locator_and_skips_replay() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let binder = Arc::new(StubBinder::ok());
        let tool = spawn_tool(
            driver.clone(),
            Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
        );
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        let first = tool
            .spawn_with("op-spawn-bind".into(), &arguments)
            .await
            .unwrap();
        assert_eq!(first.kind(), ToolOutcomeKind::Success);
        let first_json = outcome_json(&first);
        assert!(first_json.get("session_locator").is_none());
        assert!(first_json.get("account").is_none());
        assert!(first_json.get("tuple_id").is_none());
        assert!(first_json.get("effort").is_none());
        assert_eq!(first_json["agent_id"], "c1");
        assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
        {
            let tree = tree.lock().unwrap();
            let child = tree.node("c1").unwrap();
            assert_eq!(
                child.session_locator.as_deref(),
                Some(
                    "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
                )
            );
            assert_ne!(child.status, NodeStatus::RecoveryRequired);
        }
        let replay = tool
            .spawn_with("op-spawn-bind".into(), &arguments)
            .await
            .unwrap();
        assert_eq!(replay.kind(), ToolOutcomeKind::Success);
        assert!(outcome_json(&replay).get("session_locator").is_none());
        assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
        let loaded = reload(&store, "tree1");
        assert_eq!(
            loaded.node("c1").unwrap().session_locator.as_deref(),
            Some(
                "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
            )
        );
    }

    #[tokio::test]
    async fn binder_accepts_only_exact_previously_published_locator() {
        struct PublishedBinder {
            driver: TreeDriver,
            same: bool,
        }
        impl SpawnedSessionBinder for PublishedBinder {
            fn bind(
                &self,
                start: SpawnStart,
            ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>>
            {
                Box::pin(async move {
                    let mut tree = self.driver.tree.lock().unwrap();
                    tree.record_child_session(&start.spawned.agent_id, "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001")
                        .unwrap();
                    let session = "00000000-0000-4000-8000-0000000000c1";
                    tree.project_feed(
                        &start.spawned.agent_id,
                        session,
                        &[],
                        &crate::tree::TreeFeedReads::default(),
                    )
                    .unwrap();
                    self.driver.save(&mut tree).unwrap();
                    Ok(SpawnBound {
                        durably_registered: false,
                        session_locator: if self.same {
                            "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
                        } else {
                            "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003002"
                        }
                        .into(),
                        turn_id: None,
                    })
                })
            }
        }
        for same in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let (store, tree) = persist_fixture(&root);
            let driver = driver_on(tree, Arc::clone(&store));
            let tool = spawn_tool(
                driver.clone(),
                Some(Arc::new(PublishedBinder {
                    driver: driver.clone(),
                    same,
                })),
            );
            let arguments = serde_json::to_value(spawn_request()).unwrap();
            let outcome = tool
                .spawn_with("published-spawn".into(), &arguments)
                .await
                .unwrap();
            assert_eq!(
                outcome.kind(),
                if same {
                    ToolOutcomeKind::Success
                } else {
                    ToolOutcomeKind::Failure
                }
            );
            assert_eq!(
                driver
                    .tree
                    .lock()
                    .unwrap()
                    .node("c1")
                    .unwrap()
                    .session_locator
                    .as_deref(),
                Some(
                    "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
                )
            );
        }
    }

    #[tokio::test]
    async fn unresolved_spawn_is_durable_unsealed_and_never_rebound_on_replay() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let binder = Arc::new(StubBinder {
            binds: AtomicUsize::new(0),
            result: Err(SpawnBindError::RecoveryRequired {
                stage: crate::hub::ChildStartupStage::Starting,
                message: "acknowledgement delayed; owner retained".into(),
            }),
        });
        let tool = spawn_tool(driver.clone(), Some(binder.clone()));
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        assert_eq!(
            tool.spawn_with("uncertain-spawn".into(), &arguments)
                .await
                .unwrap()
                .kind(),
            ToolOutcomeKind::Failure
        );
        let loaded = reload(&store, "tree1");
        assert_eq!(
            loaded.node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(loaded.attempt("c1").unwrap().candidate_id.is_none());
        let _ = tool
            .spawn_with("uncertain-spawn".into(), &arguments)
            .await
            .unwrap();
        assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn spawn_replay_restores_running_after_bound_recovery() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let tool = spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        );
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        tool.spawn_with("op-spawn-bind".into(), &arguments)
            .await
            .unwrap();
        {
            let mut tree = tree.lock().unwrap();
            tree.record_host_materialization("c1")
                .and_then(|value| tree.fold_due("c1").map(|_| value))
                .unwrap();
            assert_eq!(
                tree.node("c1").unwrap().status,
                NodeStatus::RecoveryRequired
            );
        }
        let replay = tool
            .spawn_with("op-spawn-bind".into(), &arguments)
            .await
            .unwrap();
        assert_eq!(replay.kind(), ToolOutcomeKind::Success);
        let tree = tree.lock().unwrap();
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
        assert_eq!(
            tree.node("c1").unwrap().session_locator.as_deref(),
            Some(
                "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
            )
        );
        let expected = tree.attempt("c1").unwrap().clone();
        let mut rebuilt = reload(&store, "tree1");
        rebuilt.clear_feed_views();
        rebuilt
            .project_feed(
                "c1",
                &rebuilt.tree_id.clone(),
                &[],
                &crate::tree::TreeFeedReads::default(),
            )
            .unwrap();
        assert_eq!(rebuilt.attempt("c1").unwrap(), &expected);
        rebuilt.record_fault("c1", "second materialization failure");
        rebuilt.record_host_materialization("c1").unwrap();
        rebuilt.fold_due("c1").unwrap();
        assert_eq!(
            rebuilt.attempt("c1").unwrap().status,
            crate::protocol::AttemptStatus::RecoveryRequired
        );
    }

    /// Spawn is the only carrier of the prompt (mail `delegate` is
    /// refused), so the child's assignment turn is running when spawn returns.
    /// Mail that follows is a follow-up on that turn, never a second Start.
    #[tokio::test]
    async fn spawn_starts_the_assignment_turn_and_later_mail_follows_up() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let tool = spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        );
        let outcome = tool
            .spawn_with(
                "op-spawn-start".into(),
                &serde_json::to_value(spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        {
            let mut tree = tree.lock().unwrap();
            let child = tree.node("c1").unwrap();
            assert_eq!(child.status, NodeStatus::Running);
            assert!(
                !child.active_turn,
                "a construction turn is not feed evidence"
            );
            assert!(tree.has_turn_owner("c1"));
            assert_eq!(tree.attempt("c1").unwrap().turn_id.as_deref(), None);
            let mail = tree
                .send_agent_message(
                    "root",
                    &SendAgentMessageRequest {
                        agent_id: "c1".into(),
                        message: "also check the tests".into(),
                        purpose: None,
                        priority: Some(MessagePriority::Deferred),
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap();
            assert_eq!(mail.disposition, MailDisposition::Queued);
        }
        let loaded = reload(&store, "tree1");
        assert_eq!(
            loaded.attempt("c1").unwrap().turn_id.as_deref(),
            None,
            "a construction turn never becomes a durable binding"
        );
    }

    #[tokio::test]
    async fn cancelling_pending_topology_wait_preserves_running_child() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
            .spawn_with(
                "spawn-cancel-wait".into(),
                &serde_json::to_value(spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        let waiter = wait_tool(driver.clone());
        let arguments = json!({"agent_ids": ["c1"], "timeout_ms": 30_000});
        let cancellation = plakous::model::CancellationToken::new();
        let mut pending = Box::pin(waiter.dispatch_cancellable(
            &ToolCallerLineage::Direct,
            "cancel-wait".into(),
            &arguments,
            cancellation.cancelled(),
        ));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        cancellation.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), pending).await;
        assert_eq!(
            result
                .expect("wait must cooperate before its 30s deadline")
                .unwrap_err()
                .kind(),
            ToolRuntimeErrorKind::Cancelled
        );
        assert_eq!(
            driver.tree.lock().unwrap().node("c1").unwrap().status,
            NodeStatus::Running
        );
        // Cancellation must not settle the child; its later completion still
        // returns normally through the ordinary wait path.
        driver
            .tree
            .lock()
            .unwrap()
            .record_turn_completed("c1")
            .unwrap();
        assert_eq!(
            waiter.wait_with(&arguments).await.unwrap().kind(),
            ToolOutcomeKind::Success
        );
    }

    #[tokio::test]
    async fn cancelled_topology_wait_does_not_acknowledge_ready_mail() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
            .spawn_with(
                "spawn-ready-mail".into(),
                &serde_json::to_value(spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        let (messages, revision) = {
            let mut tree = driver.tree.lock().unwrap();
            tree.send_agent_message(
                "c1",
                &SendAgentMessageRequest {
                    agent_id: "root".into(),
                    message: "synthetic finding".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
            (tree.messages().clone(), tree.revision)
        };
        let waiter = wait_tool(driver.clone());
        let arguments = json!({"agent_ids": ["c1"], "timeout_ms": 30_000});
        let cancellation = plakous::model::CancellationToken::new();
        cancellation.cancel();
        let error = waiter
            .dispatch_cancellable(
                &ToolCallerLineage::Direct,
                "cancel-ready-mail".into(),
                &arguments,
                cancellation.cancelled(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ToolRuntimeErrorKind::Cancelled);
        {
            let tree = driver.tree.lock().unwrap();
            assert_eq!(tree.messages(), &messages);
            assert_eq!(tree.revision, revision);
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
        }
        assert_eq!(
            waiter.wait_with(&arguments).await.unwrap().kind(),
            ToolOutcomeKind::Success
        );
        assert_ne!(driver.tree.lock().unwrap().messages(), &messages);
    }

    /// A child completing settles a pending `wait_agent` within one
    /// poll interval, not at the timeout.
    #[tokio::test]
    async fn child_completion_settles_a_pending_wait_within_a_poll_interval() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-wait".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        let waiter = wait_tool(driver.clone());
        let arguments = json!({ "agent_ids": ["c1"], "timeout_ms": 30_000 });
        let started = tokio::time::Instant::now();
        let pending = tokio::spawn(async move { waiter.wait_with(&arguments).await });
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(
            !pending.is_finished(),
            "a running child does not settle the wait"
        );
        tree.lock().unwrap().record_turn_completed("c1").unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), pending)
            .await
            .expect("the wait returns well before its 30 s timeout")
            .unwrap()
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let output = outcome_json(&outcome);
        assert_eq!(output["timed_out"], false);
        assert_eq!(output["agents"][0]["agent_id"], "c1");
        assert_eq!(output["agents"][0]["status"], "failed");
        assert_eq!(
            output["agents"][0]["attempt"],
            "completed_without_submission"
        );
        assert!(
            output["agents"][0].get("output_json").is_none(),
            "no submission, no output on the wire"
        );
    }

    /// The spawner reads the delegated result off the wait row (and the full
    /// list row); nothing else carries `output_json` back up the tree.
    #[tokio::test]
    async fn a_submitted_result_rides_the_wait_row_and_the_full_list_row() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-out".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        {
            let mut tree = tree.lock().unwrap();
            tree.submit_result(
                "c1",
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed("c1").unwrap();
        }
        let outcome = wait_tool(driver.clone())
            .wait_with(&json!({ "agent_ids": ["c1"], "timeout_ms": 1000 }))
            .await
            .unwrap();
        let output = outcome_json(&outcome);
        assert_eq!(output["timed_out"], false);
        assert_eq!(output["agents"][0]["status"], "completed");
        assert_eq!(output["agents"][0]["attempt"], "completed");
        assert_eq!(output["agents"][0]["output_json"], r#"{"ok":true}"#);
        let listed = tree
            .lock()
            .unwrap()
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: None,
                    include_completed: Some(true),
                },
            )
            .unwrap();
        assert_eq!(listed.agents[0]["agent_id"], "c1");
        assert_eq!(listed.agents[0]["output_json"], r#"{"ok":true}"#);
    }

    /// A bare `wait_agent {}` hands back each submitted result once; after
    /// the last one it refuses instead of returning the same rows again.
    #[tokio::test]
    async fn bare_waits_return_each_result_once_then_refuse() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        for key in ["a", "b"] {
            let mut request = spawn_request();
            request.request_key = Some(format!("wave-{key}"));
            spawn_tool(
                driver.clone(),
                Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
            )
            .spawn_with(
                format!("op-spawn-{key}"),
                &serde_json::to_value(request).unwrap(),
            )
            .await
            .unwrap();
        }
        let mut seen = Vec::new();
        for child in ["c2", "c1"] {
            {
                let mut tree = tree.lock().unwrap();
                tree.submit_result(
                    child,
                    &SubmitResultRequest {
                        turn_token: 1,
                        output_json: r#"{"ok":true}"#.into(),
                    },
                    None,
                )
                .unwrap();
                tree.record_turn_completed(child).unwrap();
            }
            let outcome = wait_tool(driver.clone())
                .wait_with(&json!({ "timeout_ms": 1000 }))
                .await
                .unwrap();
            let output = outcome_json(&outcome);
            let results: Vec<String> = output["agents"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["output_json"].is_string())
                .map(|row| row["agent_id"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(results, [child]);
            seen.push(child);
        }
        assert_eq!(seen, ["c2", "c1"]);
        let refused = wait_tool(driver.clone())
            .wait_with(&json!({ "timeout_ms": 1000 }))
            .await
            .unwrap();
        assert_eq!(outcome_json(&refused)["code"], "no_pending_children");
    }

    /// A child that cannot start is terminal with a reason at once, so
    /// a `wait_agent` on it returns instead of parking behind a `running` node
    /// nothing is driving.
    #[tokio::test]
    async fn a_child_that_cannot_start_is_faulted_with_a_reason_and_settles_waits() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let outcome = spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::fail()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-nostart".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        let output = outcome_json(&outcome);
        assert_eq!(output["code"], "model_not_admitted");
        let message = output["message"].as_str().unwrap().to_owned();
        assert!(message.contains("child c1 could not start"), "{message}");
        assert!(
            message.contains("child session could not be created"),
            "{message}"
        );
        assert!(message.contains("no session was created"), "{message}");
        {
            let tree = tree.lock().unwrap();
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Failed);
            assert!(!tree.node("c1").unwrap().active_turn);
            assert_eq!(tree.fault_reason("c1"), Some(message.as_str()));
            let attempt = tree.attempt("c1").unwrap();
            assert_eq!(attempt.status, crate::protocol::AttemptStatus::Failed);
            assert_eq!(
                attempt
                    .failure
                    .as_ref()
                    .map(|failure| failure.kind.as_str()),
                Some("spawn_refused")
            );
        }
        let started = tokio::time::Instant::now();
        let outcome = wait_tool(driver.clone())
            .wait_with(&json!({ "agent_ids": ["c1"], "timeout_ms": 30_000 }))
            .await
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let output = outcome_json(&outcome);
        assert_eq!(output["timed_out"], false);
        assert_eq!(output["agents"][0]["status"], "failed");
    }

    /// A binder refusal proves no session exists: the node is an ordinary
    /// failed spawn, the replay repeats the refusal, and the tree stays open.
    #[tokio::test]
    async fn spawn_binder_refusal_fails_the_node_without_a_recovery_fence() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let binder = Arc::new(StubBinder::fail());
        let tool = spawn_tool(
            driver.clone(),
            Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
        );
        let outcome = tool
            .spawn_with(
                "op-spawn-fail".into(),
                &serde_json::to_value(spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(binder.binds.load(Ordering::SeqCst), 1);
        {
            let tree = tree.lock().unwrap();
            let child = tree.node("c1").unwrap();
            assert!(child.session_locator.is_none());
            assert_eq!(child.status, NodeStatus::Failed);
            assert_eq!(tree.state, crate::protocol::TreeAdmissionState::Live);
        }
        let loaded = reload(&store, "tree1");
        assert_eq!(loaded.node("c1").unwrap().status, NodeStatus::Failed);
        assert_eq!(
            loaded.operations()["op-spawn-fail"].terminal_code,
            Some(TopologyErrorCode::ModelNotAdmitted),
            "an exact replay repeats the refusal instead of binding again"
        );
        assert!(loaded.node("c1").unwrap().session_locator.is_none());
    }

    #[tokio::test]
    async fn send_without_deliverer_leaves_mail_accepted() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let tool = send_tool(driver.clone(), None);
        let outcome = tool
            .send_with("op-mail-1".into(), &send_request("c1", "hello", None))
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let payload = outcome_json(&outcome);
        assert_eq!(payload["disposition"], "started");
        let tree = tree.lock().unwrap();
        assert_eq!(tree.mail("m1").unwrap().durable, MailDurableState::Accepted);
        assert!(!tree.node("c1").unwrap().active_turn);
    }

    #[tokio::test]
    async fn send_deliverer_starts_idle_child_and_skips_replay() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let binder = Arc::new(StubBinder::bind_only());
        let spawn = spawn_tool(
            driver.clone(),
            Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
        );
        spawn
            .spawn_with(
                "op-spawn-bind".into(),
                &serde_json::to_value(spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        let deliverer = Arc::new(StubDeliverer::ok().binding(&tree));
        let tool = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        let first = tool
            .send_with("op-mail-1".into(), &send_request("c1", "hello", None))
            .await
            .unwrap();
        assert_eq!(first.kind(), ToolOutcomeKind::Success);
        assert_eq!(outcome_json(&first)["disposition"], "started");
        {
            let calls = deliverer.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].disposition, MailDisposition::Started);
            assert_eq!(calls[0].body, "hello");
            assert_eq!(
                calls[0].session_locator,
                "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
            );
            assert_eq!(calls[0].turn_token, 1);
        }
        {
            let tree = tree.lock().unwrap();
            assert_eq!(
                tree.mail("m1").unwrap().durable,
                MailDurableState::Acknowledged
            );
            assert!(tree.node("c1").unwrap().active_turn);
            assert_eq!(
                tree.attempt("c1").unwrap().turn_id.as_deref(),
                Some("turn-1")
            );
        }
        let replay = tool
            .send_with("op-mail-1".into(), &send_request("c1", "hello", None))
            .await
            .unwrap();
        assert_eq!(replay.kind(), ToolOutcomeKind::Success);
        assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
        let loaded = reload(&store, "tree1");
        assert_eq!(
            loaded.mail("m1").unwrap().durable,
            MailDurableState::Acknowledged
        );
        assert_eq!(
            loaded.attempt("c1").unwrap().turn_id.as_deref(),
            Some("turn-1")
        );
    }

    #[tokio::test]
    async fn send_receipt_replays_after_acknowledged_mail_history_is_pruned() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
            .spawn_with(
                "history-spawn".into(),
                &serde_json::to_value(spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        let deliverer = Arc::new(StubDeliverer::ok());
        let sender = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        let request = send_request("c1", "original", None);
        let first = sender
            .send_with("original-send".into(), &request)
            .await
            .unwrap();
        {
            let mut tree = driver.tree.lock().unwrap();
            for _ in 0..70 {
                let request: SendAgentMessageRequest =
                    serde_json::from_value(send_request("c1", "later", None)).unwrap();
                let next = tree.send_agent_message("root", &request, None).unwrap();
                tree.acknowledge_mail(&next.message_id).unwrap();
            }
            assert!(tree.mail("m1").is_none());
            driver.save(&mut tree).unwrap();
        }
        let replay = sender
            .send_with("original-send".into(), &request)
            .await
            .unwrap();
        assert_eq!(outcome_json(&replay), outcome_json(&first));
        assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn send_without_locator_is_fail_closed_when_deliverer_present() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let deliverer = Arc::new(StubDeliverer::ok());
        let tool = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        let outcome = tool
            .send_with("op-mail-missing".into(), &send_request("c1", "hello", None))
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert!(deliverer.calls.lock().unwrap().is_empty());
        {
            let tree = tree.lock().unwrap();
            assert_eq!(
                tree.node("c1").unwrap().status,
                NodeStatus::RecoveryRequired
            );
        }
        assert_eq!(
            reload(&store, "tree1").node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }

    #[tokio::test]
    async fn send_deliverer_failure_is_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let binder = Arc::new(StubBinder::ok());
        spawn_tool(
            driver.clone(),
            Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        let deliverer = Arc::new(StubDeliverer::fail());
        let outcome = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        )
        .send_with("op-mail-fail".into(), &send_request("c1", "hello", None))
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
        assert_eq!(
            reload(&store, "tree1").node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }

    /// Mail sent between a child's turn end and the host's park decision
    /// reaches an ended turn: the send stands, the child is not faulted, and
    /// the mail stays unread and unclaimed so the host's wake carries it.
    #[tokio::test]
    async fn send_to_a_turn_that_already_ended_leaves_the_mail_for_the_wake() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        let deliverer = Arc::new(StubDeliverer::turn_ended());
        let outcome = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        )
        .send_with(
            "op-mail-late".into(),
            &send_request("c1", "one more thing", None),
        )
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
        let message_id = outcome_json(&outcome)["message_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let tree = tree.lock().unwrap();
        assert_ne!(
            tree.node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(
            tree.wake_mail("c1")
                .iter()
                .any(|mail| mail.message_id == message_id)
        );
        drop(tree);
        assert_ne!(
            reload(&store, "tree1").node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }

    /// `TurnEnded` is benign only while a wake can still carry the mail
    /// (spec 03 §5.2). A child whose turn submitted and then ended is never
    /// woken again: the send is refused `recipient_attempt_terminal` and the
    /// mail is settled instead of staying unread on the terminal attempt.
    #[tokio::test]
    async fn turn_ended_send_to_a_submitted_child_is_refused_and_settled() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        // The child called submit_result; its turn is still unwinding, so the
        // send is admitted against the open attempt.
        tree.lock()
            .unwrap()
            .submit_result(
                "c1",
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.to_owned(),
                },
                None,
            )
            .unwrap();
        let deliverer = Arc::new(StubDeliverer::turn_ended());
        let outcome = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        )
        .send_with(
            "op-mail-late".into(),
            &send_request("c1", "also check the docs", None),
        )
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(
            outcome_json(&outcome)["code"],
            json!(TopologyErrorCode::RecipientAttemptTerminal)
        );
        // A replay is refused the same way instead of returning the success
        // the send first recorded.
        let replay = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        )
        .send_with(
            "op-mail-late".into(),
            &send_request("c1", "also check the docs", None),
        )
        .await
        .unwrap();
        assert_eq!(replay.kind(), ToolOutcomeKind::Failure);
        assert_eq!(
            outcome_json(&replay)["code"],
            json!(TopologyErrorCode::RecipientAttemptTerminal)
        );
        assert_eq!(
            deliverer.calls.lock().unwrap().len(),
            1,
            "never re-delivered"
        );
        // The submitted turn ends: the attempt completes and nothing wakes it,
        // and no unread mail is left on it.
        let mut tree = tree.lock().unwrap();
        tree.record_turn_completed("c1").unwrap();
        assert!(tree.attempt("c1").unwrap().status.is_stopped());
        assert_eq!(tree.wake_mail("c1"), [] as [crate::tree::MailRecord; 0]);
    }

    /// Spec 03 section 9: mail that reaches an interrupted child while its
    /// cancelled turn is still unwinding has no park and no wake to carry
    /// it. The send waits for the unwind to release the turn, then delivers
    /// the same message again as the Start of the resume turn: its
    /// disposition and receipt say `started` and the mail is acknowledged.
    #[tokio::test]
    async fn turn_ended_send_to_an_interrupted_child_starts_its_resume_turn() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        {
            let mut tree = tree.lock().unwrap();
            tree.set_active_turn("c1", true).unwrap();
            if tree.attempt("c1").unwrap().turn_id.is_none() {
                tree.bind_attempt_turn("c1", "00000000-0000-0000-0000-000000000001")
                    .unwrap();
            }
            // The parent interrupts the child; its turn is still unwinding.
            tree.interrupt_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: "c1".into(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap();
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
            // The cancelled turn's waiter is still running.
            tree.enter_turn_waiter("c1");
        }
        // The redirect reaches the live session after the cancelled turn ended.
        let deliverer = Arc::new(StubDeliverer::turn_ended_then_ok().binding(&tree));
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let request = send_request("c1", "use the v2 schema instead", None);
        let pending_send = tokio::spawn({
            let (tool, request) = (Arc::clone(&tool), request.clone());
            async move {
                tool.send_with("op-mail-redirect".into(), &request)
                    .await
                    .unwrap()
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while deliverer.calls.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!pending_send.is_finished(), "the send waits for the unwind");
        // The interrupted turn's waiter releases the turn; nothing parks it.
        {
            let mut tree = tree.lock().unwrap();
            tree.release_attempt_turn("c1").unwrap();
            tree.release_turn_owner("c1");
            tree.set_active_turn("c1", false).unwrap();
            tree.leave_turn_waiter("c1");
            assert!(!tree.is_parked("c1"));
        }
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending_send)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            outcome.kind(),
            ToolOutcomeKind::Success,
            "{}",
            outcome_json(&outcome)
        );
        let sent = outcome_json(&outcome);
        assert_eq!(sent["disposition"], json!(MailDisposition::Started));
        let calls = deliverer.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].message_id, calls[1].message_id);
        assert_eq!(calls[1].disposition, MailDisposition::Started);
        let message_id = sent["message_id"].as_str().unwrap().to_owned();
        {
            let tree = tree.lock().unwrap();
            assert!(
                tree.mail(&message_id)
                    .is_none_or(|mail| mail.durable != MailDurableState::Accepted),
                "the resume turn carried the mail"
            );
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
            assert_eq!(
                tree.attempt("c1").unwrap().turn_id.as_deref(),
                Some("turn-1")
            );
        }
        // A replay returns the updated receipt.
        let replay = tool
            .send_with("op-mail-redirect".into(), &request)
            .await
            .unwrap();
        assert_eq!(
            outcome_json(&replay)["disposition"],
            json!(MailDisposition::Started)
        );
    }

    /// A child `c1` whose turn the parent interrupted and which is still
    /// unwinding: bound, active, and `interrupted`.
    async fn interrupted_unwinding_child(driver: &TreeDriver) {
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        let mut tree = driver.tree.lock().unwrap();
        tree.set_active_turn("c1", true).unwrap();
        if tree.attempt("c1").unwrap().turn_id.is_none() {
            tree.bind_attempt_turn("c1", "00000000-0000-0000-0000-000000000001")
                .unwrap();
        }
        tree.interrupt_agent(
            "root",
            &AttemptScopedRequest {
                agent_id: "c1".into(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
        // The cancelled turn's waiter is still running.
        tree.enter_turn_waiter("c1");
    }

    /// The interrupted turn's waiter releases the cancelled turn and ends.
    fn release_unwound_turn(tree: &Mutex<ProductAgentTree>) {
        let mut tree = tree.lock().unwrap();
        tree.release_attempt_turn("c1").unwrap();
        tree.release_turn_owner("c1");
        tree.set_active_turn("c1", false).unwrap();
        tree.leave_turn_waiter("c1");
    }

    async fn until_calls(deliverer: &StubDeliverer, count: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while deliverer.calls.lock().unwrap().len() < count {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for delivery {count}"));
    }

    /// Two sends that both found an interrupted child's turn ended restart
    /// it once: the first to look reserves the resume Start under the tree
    /// lock, and the other waits for that Start, then follows up into the
    /// turn it admitted with its receipt as it was.
    #[tokio::test]
    async fn concurrent_turn_ended_sends_restart_an_interrupted_child_once() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        let stub = StubDeliverer::ok().binding(&tree);
        for _ in 0..2 {
            stub.first
                .lock()
                .unwrap()
                .push_back(Err(MailDeliveryFailure::TurnEnded));
        }
        // The resume Start is held until both sends have looked.
        let (stub, gate) = stub.holding_from(2);
        let deliverer = Arc::new(stub);
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let send = |operation: &'static str, text: &'static str| {
            let tool = Arc::clone(&tool);
            tokio::spawn(async move {
                tool.send_with(operation.into(), &send_request("c1", text, None))
                    .await
                    .unwrap()
            })
        };
        let first = send("op-mail-a", "use the v2 schema");
        until_calls(&deliverer, 1).await;
        let second = send("op-mail-b", "keep the old field too");
        until_calls(&deliverer, 2).await;
        release_unwound_turn(&tree);
        until_calls(&deliverer, 3).await;
        // Both sends saw the release; give the other one its look.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            deliverer.calls.lock().unwrap().len(),
            3,
            "a second resume Start was delivered for the same interrupted child"
        );
        gate.add_permits(4);
        let mut outcomes = Vec::new();
        for pending in [first, second] {
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
            outcomes.push(outcome_json(&outcome));
        }
        let calls = deliverer.calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            4,
            "one resume Start, then the rider's follow-up"
        );
        assert_eq!(calls[2].disposition, MailDisposition::Started);
        assert_eq!(calls[3].disposition, MailDisposition::Queued);
        assert_ne!(calls[3].message_id, calls[2].message_id);
        let started = outcomes
            .iter()
            .find(|sent| sent["message_id"] == json!(calls[2].message_id))
            .unwrap();
        let rider = outcomes
            .iter()
            .find(|sent| sent["message_id"] != json!(calls[2].message_id))
            .unwrap();
        assert_eq!(started["disposition"], json!(MailDisposition::Started));
        assert_eq!(rider["disposition"], json!(MailDisposition::Queued));
        let tree = tree.lock().unwrap();
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
        assert_eq!(
            tree.mail(rider["message_id"].as_str().unwrap())
                .map(|mail| mail.durable),
            Some(MailDurableState::Acknowledged),
            "the other send's mail reached the resumed turn as a follow-up"
        );
    }

    /// A sender cancelled while its send waits for an interrupted child's
    /// turn to unwind returns `Cancelled` at once, before any restart: the
    /// mail stays unread under its original receipt.
    #[tokio::test]
    async fn a_cancelled_sender_stops_waiting_for_the_unwind_before_any_restart() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        let deliverer = Arc::new(StubDeliverer::turn_ended_then_ok());
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let cancellation = plakous::model::CancellationToken::new();
        let pending = tokio::spawn({
            let (tool, cancellation) = (Arc::clone(&tool), cancellation.clone());
            async move {
                tool.dispatch_cancellable(
                    &ToolCallerLineage::Direct,
                    "op-mail-cancelled".into(),
                    &send_request("c1", "use the v2 schema", None),
                    cancellation.cancelled(),
                )
                .await
            }
        });
        until_calls(&deliverer, 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!pending.is_finished(), "the send waits for the unwind");
        cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
            .await
            .expect("a cancelled send stops waiting")
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), ToolRuntimeErrorKind::Cancelled);
        release_unwound_turn(&tree);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            deliverer.calls.lock().unwrap().len(),
            1,
            "nothing restarted"
        );
        let tree = tree.lock().unwrap();
        let mail = tree
            .messages()
            .values()
            .find(|mail| mail.to == "c1")
            .unwrap();
        assert_eq!(mail.durable, MailDurableState::Accepted);
        assert_eq!(mail.disposition, MailDisposition::Queued);
        let receipt = tree.operations()["op-mail-cancelled"]
            .terminal_response
            .clone()
            .unwrap();
        assert!(receipt.contains("queued"), "{receipt}");
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
    }

    /// The same holds for a send waiting behind a parked wait on its mail.
    #[tokio::test]
    async fn a_cancelled_sender_stops_waiting_behind_a_parked_wait() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        tree.lock()
            .unwrap()
            .parked_waits
            .insert(("c1".into(), "op-wait".into()), vec!["root".into()]);
        let deliverer = Arc::new(StubDeliverer::ok());
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let cancellation = plakous::model::CancellationToken::new();
        let pending = tokio::spawn({
            let (tool, cancellation) = (Arc::clone(&tool), cancellation.clone());
            async move {
                tool.dispatch_cancellable(
                    &ToolCallerLineage::Direct,
                    "op-mail-behind-wait".into(),
                    &send_request("c1", "answer when you can", None),
                    cancellation.cancelled(),
                )
                .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!pending.is_finished(), "the send waits behind the wait");
        cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
            .await
            .expect("a cancelled send stops waiting")
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), ToolRuntimeErrorKind::Cancelled);
        assert!(deliverer.calls.lock().unwrap().is_empty());
        let tree = tree.lock().unwrap();
        assert!(
            tree.messages()
                .values()
                .any(|mail| mail.to == "c1" && mail.durable == MailDurableState::Accepted)
        );
    }

    /// A resume Start that fails refuses the send, and the receipt becomes
    /// that refusal: it never read `started` for a Start that was not
    /// admitted, so a replay is refused without delivering again.
    #[tokio::test]
    async fn a_failed_resume_start_never_records_a_started_receipt() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        let deliverer = Arc::new(StubDeliverer::turn_ended_then_fail());
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let request = send_request("c1", "use the v2 schema", None);
        let pending = tokio::spawn({
            let (tool, request) = (Arc::clone(&tool), request.clone());
            async move {
                tool.send_with("op-mail-resume".into(), &request)
                    .await
                    .unwrap()
            }
        });
        until_calls(&deliverer, 1).await;
        release_unwound_turn(&tree);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(deliverer.calls.lock().unwrap().len(), 2);
        assert_eq!(
            tree.lock().unwrap().operations()["op-mail-resume"].terminal_code,
            Some(TopologyErrorCode::OperationRecoveryRequired),
            "the receipt is the refusal, not a `started` success"
        );
        let replay = tool
            .send_with("op-mail-resume".into(), &request)
            .await
            .unwrap();
        assert_eq!(replay.kind(), ToolOutcomeKind::Failure);
        assert_eq!(
            outcome_json(&replay)["code"],
            json!(TopologyErrorCode::OperationRecoveryRequired)
        );
        assert_eq!(
            deliverer.calls.lock().unwrap().len(),
            2,
            "a replay is not delivered again"
        );
    }

    /// A `followup` Start whose delivery finds the child's previous slot
    /// still retiring is retried once the slot is vacant, under the send's
    /// budget: it is never left unread under a `started` receipt on an
    /// attempt nothing will start.
    #[tokio::test]
    async fn a_followup_start_on_a_retiring_slot_is_retried() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        release_unwound_turn(&tree);
        let stub = StubDeliverer::ok().binding(&tree);
        stub.first
            .lock()
            .unwrap()
            .push_back(Err(MailDeliveryFailure::SlotBusy));
        let deliverer = Arc::new(stub);
        let tool = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        let request = serde_json::to_value(SendAgentMessageRequest {
            agent_id: "c1".into(),
            message: "now do the v3 schema".into(),
            purpose: Some(MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        })
        .unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tool.send_with("op-followup".into(), &request),
        )
        .await
        .expect("the send returns")
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let sent = outcome_json(&outcome);
        assert_eq!(sent["disposition"], json!(MailDisposition::Started));
        let calls = deliverer.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "the followup Start was retried once");
        assert!(
            calls
                .iter()
                .all(|call| call.disposition == MailDisposition::Started)
        );
        let tree = tree.lock().unwrap();
        assert_eq!(
            tree.mail(sent["message_id"].as_str().unwrap())
                .map(|mail| mail.durable),
            Some(MailDurableState::Acknowledged)
        );
        assert!(tree.node("c1").unwrap().active_turn);
        assert_eq!(
            tree.attempt("c1").unwrap().turn_id.as_deref(),
            Some("turn-1")
        );
        assert!(tree.start_reservations.is_empty());
    }

    /// A Started mail whose Start finds a turn another command admitted is
    /// delivered into that turn as a follow-up, and its mail and receipt
    /// become `queued`. The second send's Start waits for the first's, which
    /// it reserved.
    #[tokio::test]
    async fn a_started_mail_that_finds_another_commands_turn_follows_up_into_it() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::bind_only()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        let stub = StubDeliverer::ok().binding(&tree);
        // The first Start is admitted; the second finds its turn live.
        stub.first.lock().unwrap().extend([
            Ok(MailDeliveryAck {
                turn_id: Some("turn-1".into()),
            }),
            Err(MailDeliveryFailure::TurnEnded),
        ]);
        let (stub, gate) = stub.holding_from(0);
        let deliverer = Arc::new(stub);
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let send = |operation: &'static str, text: &'static str| {
            let tool = Arc::clone(&tool);
            tokio::spawn(async move {
                tool.send_with(operation.into(), &send_request("c1", text, None))
                    .await
                    .unwrap()
            })
        };
        let first = send("op-mail-a", "use the v2 schema");
        until_calls(&deliverer, 1).await;
        let second = send("op-mail-b", "keep the old field too");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            deliverer.calls.lock().unwrap().len(),
            1,
            "a second Start was delivered while the first was in flight"
        );
        gate.add_permits(8);
        let mut receipts = Vec::new();
        for pending in [first, second] {
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
            receipts.push(outcome_json(&outcome));
        }
        assert_eq!(receipts[0]["disposition"], json!(MailDisposition::Started));
        assert_eq!(receipts[1]["disposition"], json!(MailDisposition::Queued));
        let calls = deliverer.calls.lock().unwrap().clone();
        let shapes: Vec<_> = calls
            .iter()
            .map(|call| (call.message_id.clone(), call.disposition))
            .collect();
        let (a, b) = (
            receipts[0]["message_id"].as_str().unwrap().to_owned(),
            receipts[1]["message_id"].as_str().unwrap().to_owned(),
        );
        assert_eq!(
            shapes,
            [
                (a.clone(), MailDisposition::Started),
                (b.clone(), MailDisposition::Started),
                (b.clone(), MailDisposition::Queued),
            ]
        );
        let tree = tree.lock().unwrap();
        let mail = tree.mail(&b).unwrap();
        assert_eq!(mail.durable, MailDurableState::Acknowledged);
        assert_eq!(mail.disposition, MailDisposition::Queued);
        let receipt = tree.operations()["op-mail-b"]
            .terminal_response
            .clone()
            .unwrap();
        assert!(receipt.contains("queued"), "{receipt}");
    }

    /// One 60 s budget per send, however often it restarts: the
    /// reservation holder checks it before it renews its Start, so a resume
    /// Start that keeps finding the slot taken is refused definitively, its
    /// mail settled read, its receipt the refusal, and its reservation
    /// released.
    #[tokio::test(start_paused = true)]
    async fn a_restart_loop_is_bounded_by_the_send_budget() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        release_unwound_turn(&tree);
        let deliverer = Arc::new(StubDeliverer::turn_ended());
        let tool = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            tool.send_with(
                "op-stuck".into(),
                &send_request("c1", "use the v2 schema", None),
            ),
        )
        .await;
        let calls = deliverer.calls.lock().unwrap().len();
        let outcome = outcome.unwrap_or_else(|_| {
            panic!(
                "the send was still restarting after 300 s (virtual): {calls} Starts \
                 delivered, the 60 s budget never ended it"
            )
        });
        let outcome = outcome.unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        let refused = outcome_json(&outcome);
        assert_eq!(
            refused["code"],
            json!(TopologyErrorCode::RecipientAttemptTerminal)
        );
        assert!(
            refused["message"]
                .as_str()
                .is_some_and(|message| message.contains("not admitted in time")),
            "{refused}"
        );
        let tree = tree.lock().unwrap();
        assert!(
            tree.start_reservations.is_empty(),
            "the reservation is released"
        );
        assert_eq!(
            tree.operations()["op-stuck"].terminal_code,
            Some(TopologyErrorCode::RecipientAttemptTerminal)
        );
        assert!(
            tree.messages()
                .values()
                .filter(|mail| mail.to == "c1")
                .all(|mail| mail.durable == MailDurableState::Acknowledged),
            "the refused mail is settled read"
        );
    }

    /// SlotBusy confirms no submission; expiry withdraws the opening.
    #[tokio::test(start_paused = true)]
    async fn a_followup_start_that_never_finds_the_slot_vacant_withdraws_its_attempt() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        release_unwound_turn(&tree);
        let found = tree
            .lock()
            .unwrap()
            .attempt("c1")
            .unwrap()
            .attempt_id
            .clone();
        let mut stub = StubDeliverer::ok();
        stub.result = Err(MailDeliveryFailure::SlotBusy);
        let deliverer = Arc::new(stub);
        let tool = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        let request = serde_json::to_value(SendAgentMessageRequest {
            agent_id: "c1".into(),
            message: "now do the v3 schema".into(),
            purpose: Some(MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        })
        .unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            tool.send_with("op-followup".into(), &request),
        )
        .await
        .expect("the budget ends the send")
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        let refused = outcome_json(&outcome);
        assert_eq!(
            refused["code"],
            json!(TopologyErrorCode::RecipientAttemptTerminal)
        );
        assert!(
            deliverer.calls.lock().unwrap().len() > 1,
            "the Start was retried"
        );
        let tree = tree.lock().unwrap();
        assert_eq!(tree.attempt("c1").unwrap().attempt_id, found);
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
        assert!(tree.start_reservations.is_empty());
        assert_eq!(
            tree.operations()["op-followup"].terminal_code,
            Some(TopologyErrorCode::RecipientAttemptTerminal)
        );
        assert_followup_views_rebuild(&store, &tree);
    }

    /// A Started mail whose Start finds another command's turn follows up as
    /// `queued`. When that follow-up never reaches a live turn before the
    /// budget ends, the mail stays unread and the send answers the receipt it
    /// recorded (`started`), not the in-memory follow-up disposition.
    #[tokio::test(start_paused = true)]
    async fn pf5_a_left_follow_up_answers_its_durable_receipt() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        release_unwound_turn(&tree);
        let stub = StubDeliverer::ok();
        stub.first
            .lock()
            .unwrap()
            .push_back(Err(MailDeliveryFailure::TurnEnded));
        let mut stub = stub;
        stub.result = Err(MailDeliveryFailure::SlotBusy);
        let (stub, gate) = stub.holding_from(0);
        let deliverer = Arc::new(stub);
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let request = serde_json::to_value(SendAgentMessageRequest {
            agent_id: "c1".into(),
            message: "now do the v3 schema".into(),
            purpose: Some(MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        })
        .unwrap();
        let pending = tokio::spawn({
            let tool = Arc::clone(&tool);
            async move { tool.send_with("op-left".into(), &request).await.unwrap() }
        });
        until_calls(&deliverer, 1).await;
        // Another command's Start now holds the child's turn.
        tree.lock().unwrap().set_active_turn("c1", true).unwrap();
        gate.add_permits(100_000);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(300), pending)
            .await
            .expect("the budget ends the send")
            .unwrap();
        let answered = outcome_json(&outcome);
        let tree = tree.lock().unwrap();
        let durable = tree.operations()["op-left"]
            .terminal_response
            .clone()
            .unwrap();
        let durable: Value = serde_json::from_str(&durable).unwrap();
        let message_id = answered["message_id"].as_str().unwrap();
        let mail = tree
            .mail(message_id)
            .map(|mail| (mail.durable, mail.disposition));
        assert_eq!(
            answered["disposition"], durable["disposition"],
            "the send answered {answered} but its durable receipt (a replay) reads {durable}; \
             mail {mail:?}"
        );
        assert_eq!(answered["disposition"], json!(MailDisposition::Started));
        assert_eq!(
            mail,
            Some((MailDurableState::Accepted, MailDisposition::Started)),
            "the unread mail keeps the recorded disposition"
        );
    }

    /// Cancellation after SlotBusy, before resubmission, withdraws the opening.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_followup_withdraws_its_unstarted_attempt() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        release_unwound_turn(&tree);
        let found = tree
            .lock()
            .unwrap()
            .attempt("c1")
            .unwrap()
            .attempt_id
            .clone();
        let mut stub = StubDeliverer::ok();
        stub.result = Err(MailDeliveryFailure::SlotBusy);
        let deliverer = Arc::new(stub);
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let request = serde_json::to_value(SendAgentMessageRequest {
            agent_id: "c1".into(),
            message: "now do the v3 schema".into(),
            purpose: Some(MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        })
        .unwrap();
        let cancellation = plakous::model::CancellationToken::new();
        let pending = tokio::spawn({
            let (tool, cancellation, request) =
                (Arc::clone(&tool), cancellation.clone(), request.clone());
            async move {
                tool.dispatch_cancellable(
                    &ToolCallerLineage::Direct,
                    "op-followup-cancelled".into(),
                    &request,
                    cancellation.cancelled(),
                )
                .await
            }
        });
        until_calls(&deliverer, 1).await;
        cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), ToolRuntimeErrorKind::Cancelled);
        {
            let tree = tree.lock().unwrap();
            assert_eq!(tree.attempt("c1").unwrap().attempt_id, found);
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
            assert!(
                tree.messages()
                    .values()
                    .any(|mail| mail.to == "c1" && mail.durable == MailDurableState::Acknowledged)
            );
            assert!(tree.start_reservations.is_empty());
            assert_eq!(
                tree.operations()["op-followup-cancelled"].terminal_code,
                Some(TopologyErrorCode::RecipientAttemptTerminal)
            );
            assert_followup_views_rebuild(&store, &tree);
        }
    }

    fn assert_followup_views_rebuild(store: &Arc<dyn TreeStore>, live: &ProductAgentTree) {
        let mut rebuilt = reload(store, "tree1");
        rebuilt.clear_feed_views();
        rebuilt
            .project_feed(
                "root",
                &rebuilt.tree_id.clone(),
                &[],
                &crate::tree::TreeFeedReads::default(),
            )
            .unwrap();
        rebuilt
            .project_feed(
                "c1",
                &rebuilt.tree_id.clone(),
                &[],
                &crate::tree::TreeFeedReads::default(),
            )
            .unwrap();
        rebuilt.fold_due("c1").unwrap();
        assert_eq!(
            rebuilt.node("c1").unwrap().status,
            live.node("c1").unwrap().status
        );
        assert_eq!(
            rebuilt.node("c1").unwrap().attempt_id,
            live.node("c1").unwrap().attempt_id
        );
        assert_eq!(
            rebuilt.node("c1").unwrap().output_schema_json,
            live.node("c1").unwrap().output_schema_json
        );
        assert_eq!(
            rebuilt.attempt("c1").unwrap().status,
            live.attempt("c1").unwrap().status
        );
    }

    #[tokio::test]
    async fn a_followup_cancelled_before_delivery_is_withdrawn_and_rebuilds() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        interrupted_unwinding_child(&driver).await;
        release_unwound_turn(&driver.tree);
        let previous = driver
            .read(|tree| tree.attempt("c1").unwrap().attempt_id.clone())
            .unwrap();
        let deliverer = Arc::new(StubDeliverer::ok());
        let tool = send_tool(driver.clone(), Some(deliverer.clone()));
        let request = json!({"agent_id": "c1", "message": "again", "purpose": "followup"});
        let answer = tool
            .dispatch_cancellable(
                &ToolCallerLineage::Direct,
                "before-delivery-followup".to_owned(),
                &request,
                std::future::ready(()),
            )
            .await;
        assert_eq!(answer.unwrap_err().kind(), ToolRuntimeErrorKind::Cancelled);
        assert!(deliverer.calls.lock().unwrap().is_empty());
        let tree = driver.tree.lock().unwrap();
        assert_eq!(tree.attempt("c1").unwrap().attempt_id, previous);
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
        assert!(
            tree.messages()
                .values()
                .any(|mail| mail.to == "c1" && mail.durable == MailDurableState::Acknowledged)
        );
        assert!(tree.start_reservations.is_empty());
        assert_followup_views_rebuild(&store, &tree);
    }

    #[tokio::test(start_paused = true)]
    async fn followup_cancel_and_slot_expiry_with_a_future_tail_hold_recovery_and_rebuild() {
        struct FutureTail {
            inner: Arc<StubDeliverer>,
        }
        impl MailDeliverer for FutureTail {
            fn deliver(
                &self,
                request: MailDeliveryRequest,
            ) -> Pin<
                Box<dyn Future<Output = Result<MailDeliveryAck, MailDeliveryFailure>> + Send + '_>,
            > {
                self.inner.deliver(request)
            }

            fn withdraw_followup(
                &self,
                driver: TreeDriver,
                agent: String,
                message: String,
                receipt: Option<(String, TopologyErrorCode)>,
            ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + '_>> {
                Box::pin(async move {
                    let (session, tail) = driver.read(|tree| {
                        let session = tree
                            .node(&agent)
                            .unwrap()
                            .session_locator
                            .as_ref()
                            .unwrap()
                            .parse::<plakous::sessions::SessionLocator>()
                            .unwrap()
                            .session_id()
                            .to_string();
                        let tail = tree.feed_cursor(&session) + 1;
                        (session, tail)
                    })?;
                    driver.withdraw_unstarted_followup(&agent, &message, &session, tail, receipt)
                })
            }
        }

        for expiry in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (store, tree) = persist_fixture(&root);
            let driver = driver_on(tree, Arc::clone(&store));
            interrupted_unwinding_child(&driver).await;
            release_unwound_turn(&driver.tree);
            let previous = driver
                .read(|tree| tree.attempt("c1").unwrap().attempt_id.clone())
                .unwrap();
            let mut stub = StubDeliverer::ok();
            stub.result = Err(MailDeliveryFailure::SlotBusy);
            let deliverer = Arc::new(stub);
            let tool = Arc::new(send_tool(
                driver.clone(),
                Some(Arc::new(FutureTail {
                    inner: Arc::clone(&deliverer),
                })),
            ));
            let cancellation = plakous::model::CancellationToken::new();
            let pending = tokio::spawn({
                let token = cancellation.clone();
                async move {
                    let request =
                        json!({"agent_id": "c1", "message": "again", "purpose": "followup"});
                    tool.dispatch_cancellable(
                        &ToolCallerLineage::Direct,
                        "future-tail-followup".to_owned(),
                        &request,
                        token.cancelled(),
                    )
                    .await
                }
            });
            until_calls(&deliverer, 1).await;
            if !expiry {
                cancellation.cancel();
            }
            let answer = tokio::time::timeout(std::time::Duration::from_secs(300), pending)
                .await
                .unwrap()
                .unwrap();
            if expiry {
                assert_eq!(answer.unwrap().kind(), ToolOutcomeKind::Failure);
            } else {
                assert_eq!(answer.unwrap_err().kind(), ToolRuntimeErrorKind::Cancelled);
            }
            let tree = driver.tree.lock().unwrap();
            assert_ne!(tree.attempt("c1").unwrap().attempt_id, previous);
            assert_eq!(
                tree.node("c1").unwrap().status,
                NodeStatus::RecoveryRequired
            );
            assert!(
                tree.messages()
                    .values()
                    .any(|mail| mail.to == "c1" && mail.durable == MailDurableState::Acknowledged)
            );
            assert!(tree.start_reservations.is_empty());
            assert_followup_views_rebuild(&store, &tree);
        }
    }

    /// The send budget never cuts a submission in flight: a Start admitted
    /// after the budget is spent is delivered late, its attempt keeps
    /// running and can still submit.
    #[tokio::test(start_paused = true)]
    async fn a_followup_start_admitted_after_the_send_budget_is_delivered_not_fenced() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        interrupted_unwinding_child(&driver).await;
        release_unwound_turn(&driver.tree);
        let (stub, gate) = StubDeliverer::ok().binding(&driver.tree).holding_from(0);
        let deliverer = Arc::new(stub);
        let tool = Arc::new(send_tool(driver.clone(), Some(deliverer.clone())));
        let request = json!({"agent_id": "c1", "message": "again", "purpose": "followup"});
        let mut pending = tokio::spawn(async move {
            tool.dispatch_cancellable(
                &ToolCallerLineage::Direct,
                "late-followup".to_owned(),
                &request,
                std::future::pending(),
            )
            .await
        });
        until_calls(&deliverer, 1).await;
        let opened = driver
            .read(|tree| tree.attempt("c1").unwrap().attempt_id.clone())
            .unwrap();
        tokio::time::advance(TURN_UNWIND_WAIT * 2).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, &mut pending)
                .await
                .is_err(),
            "an expired budget must not answer a submission in flight"
        );
        driver
            .read(|tree| {
                assert_ne!(
                    tree.node("c1").unwrap().status,
                    NodeStatus::RecoveryRequired
                );
                assert!(tree.fault_reason("c1").is_none());
            })
            .unwrap();
        gate.add_permits(1);
        let answer = pending.await.unwrap().unwrap();
        assert_eq!(answer.kind(), ToolOutcomeKind::Success, "{answer:?}");
        let tree = driver.tree.lock().unwrap();
        let attempt = tree.attempt("c1").unwrap();
        assert_eq!(attempt.attempt_id, opened);
        assert!(!attempt.status.is_stopped(), "{:?}", attempt.status);
        assert!(attempt.turn_id.is_some());
        assert_ne!(
            tree.node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(tree.fault_reason("c1").is_none());
        assert!(tree.start_reservations.is_empty());
        assert!(tree.delivery_claims.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn followup_cancel_during_unknown_submission_holds_recovery_and_rebuild() {
        {
            let root = tempfile::tempdir().unwrap();
            let (store, tree) = persist_fixture(&root);
            let driver = driver_on(tree, Arc::clone(&store));
            interrupted_unwinding_child(&driver).await;
            release_unwound_turn(&driver.tree);
            let (stub, _gate) = StubDeliverer::ok().holding_from(0);
            let deliverer = Arc::new(stub);
            let tool = Arc::new(send_tool(driver.clone(), Some(deliverer.clone())));
            let cancellation = plakous::model::CancellationToken::new();
            let request = serde_json::to_value(SendAgentMessageRequest {
                agent_id: "c1".to_owned(),
                message: "again".to_owned(),
                purpose: Some(MessagePurpose::Followup),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            })
            .unwrap();
            let pending = tokio::spawn({
                let token = cancellation.clone();
                async move {
                    tool.dispatch_cancellable(
                        &ToolCallerLineage::Direct,
                        "unknown-followup".to_owned(),
                        &request,
                        token.cancelled(),
                    )
                    .await
                }
            });
            until_calls(&deliverer, 1).await;
            cancellation.cancel();
            let answer = tokio::time::timeout(std::time::Duration::from_secs(300), pending)
                .await
                .expect("unknown submission has a bounded cancellation")
                .unwrap();
            assert_eq!(answer.unwrap_err().kind(), ToolRuntimeErrorKind::Cancelled);
            let mut tree = driver.tree.lock().unwrap();
            assert_eq!(
                tree.node("c1").unwrap().status,
                NodeStatus::RecoveryRequired
            );
            let expected = tree.attempt("c1").unwrap().clone();
            assert_eq!(
                expected.status,
                crate::protocol::AttemptStatus::RecoveryRequired
            );
            assert!(
                tree.operations()
                    .contains_key(&format!("plak:followup_opened:{}", expected.attempt_id))
            );
            assert!(tree.start_reservations.is_empty());
            assert!(tree.delivery_claims.is_empty());
            assert!(
                tree.messages()
                    .values()
                    .any(|mail| mail.to == "c1" && mail.durable == MailDurableState::Accepted)
            );
            assert_followup_views_rebuild(&store, &tree);
            tree.clear_feed_views();
            let session = tree.tree_id.clone();
            tree.project_feed(
                "root",
                &session,
                &[],
                &crate::tree::TreeFeedReads::default(),
            )
            .unwrap();
            tree.project_feed("c1", &session, &[], &crate::tree::TreeFeedReads::default())
                .unwrap();
            assert_eq!(
                tree.node("c1").unwrap().status,
                NodeStatus::RecoveryRequired
            );
            assert_eq!(tree.attempt("c1").unwrap(), &expected);
        }
    }

    /// A refused Start of an ordinary (coordinate) mail never rolls back the
    /// attempt a `followup` it did not send opened: only the send that
    /// opened an attempt may undo it.
    #[tokio::test]
    async fn a_refused_coordinate_start_never_rolls_back_another_sends_followup() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        release_unwound_turn(&tree);
        let opened = {
            let mut tree = tree.lock().unwrap();
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: "c1".into(),
                    message: "now do the v3 schema".into(),
                    purpose: Some(MessagePurpose::Followup),
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
            tree.attempt("c1").unwrap().attempt_id.clone()
        };
        let deliverer = Arc::new(StubDeliverer::refuse());
        let tool = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        let outcome = tool
            .send_with(
                "op-coordinate".into(),
                &send_request("c1", "also keep the old field", None),
            )
            .await
            .unwrap();
        let tree = tree.lock().unwrap();
        assert_eq!(
            tree.attempt("c1").unwrap().attempt_id,
            opened,
            "a coordinate's refused Start rolled back another send's followup attempt: {}",
            outcome_json(&outcome)
        );
    }

    /// A follow-up into a turn another command admitted that is still not
    /// live when the send's budget is spent leaves the mail unread: that
    /// turn's end parks or wakes the child with it. The send stands.
    #[tokio::test(start_paused = true)]
    async fn a_follow_up_whose_turn_is_not_live_in_time_is_left_for_its_end() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        {
            let mut tree = tree.lock().unwrap();
            tree.set_active_turn("c1", true).unwrap();
            if tree.attempt("c1").unwrap().turn_id.is_none() {
                tree.bind_attempt_turn("c1", "00000000-0000-0000-0000-000000000001")
                    .unwrap();
            }
        }
        let mut stub = StubDeliverer::ok();
        stub.result = Err(MailDeliveryFailure::SlotBusy);
        let deliverer = Arc::new(stub);
        let tool = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            tool.send_with(
                "op-late".into(),
                &send_request(
                    "c1",
                    "keep the old field too",
                    Some(MessagePriority::Deferred),
                ),
            ),
        )
        .await
        .expect("the budget ends the send")
        .unwrap();
        assert_eq!(
            outcome.kind(),
            ToolOutcomeKind::Success,
            "{}",
            outcome_json(&outcome)
        );
        let sent = outcome_json(&outcome);
        assert_eq!(sent["disposition"], json!(MailDisposition::Queued));
        assert!(deliverer.calls.lock().unwrap().len() > 1);
        let tree = tree.lock().unwrap();
        assert_eq!(
            tree.mail(sent["message_id"].as_str().unwrap())
                .map(|mail| mail.durable),
            Some(MailDurableState::Accepted),
            "the mail stays unread for the turn's end"
        );
        assert_eq!(tree.operations()["op-late"].terminal_code, None);
    }

    /// A Start whose deliverer bound the turn it admitted, and whose turn
    /// already ended when the delivery returns, is not marked active again:
    /// the send binds nothing.
    #[tokio::test]
    async fn an_admitted_start_whose_turn_already_ended_is_not_marked_active() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::bind_only()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        let (stub, gate) = StubDeliverer::ok().holding_from(0);
        let deliverer = Arc::new(stub);
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let pending = tokio::spawn({
            let tool = Arc::clone(&tool);
            async move {
                tool.send_with(
                    "op-start".into(),
                    &send_request("c1", "use the v2 schema", None),
                )
                .await
                .unwrap()
            }
        });
        until_calls(&deliverer, 1).await;
        {
            // The deliverer bound the turn it admitted, and the turn ended.
            let mut tree = tree.lock().unwrap();
            tree.bind_attempt_turn("c1", "turn-1").unwrap();
            tree.release_turn_owner("c1");
            tree.set_active_turn("c1", false).unwrap();
        }
        gate.add_permits(1);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        assert_eq!(
            outcome_json(&outcome)["disposition"],
            json!(MailDisposition::Started)
        );
        let tree = tree.lock().unwrap();
        assert!(
            !tree.node("c1").unwrap().active_turn,
            "the ended turn was marked active again"
        );
        assert_eq!(tree.attempt("c1").unwrap().usage.turns, 1);
    }

    /// Spawns `c1` with no turn and starts a send whose Start the deliverer
    /// holds at admission, so a test can change what the child's attempt
    /// binds before the delivery returns.
    async fn start_held_at_admission() -> (
        tempfile::TempDir,
        Arc<Mutex<ProductAgentTree>>,
        Arc<tokio::sync::Semaphore>,
        tokio::task::JoinHandle<ToolOutcome>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::bind_only()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        let (stub, gate) = StubDeliverer::ok().holding_from(0);
        let deliverer = Arc::new(stub);
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let pending = tokio::spawn({
            let tool = Arc::clone(&tool);
            async move {
                tool.send_with(
                    "op-start".into(),
                    &send_request("c1", "use the v2 schema", None),
                )
                .await
                .unwrap()
            }
        });
        until_calls(&deliverer, 1).await;
        (root, tree, gate, pending)
    }

    /// A Start whose turn was interrupted and released before the delivery
    /// returns is not bound or marked active again: the send binds nothing,
    /// so the child stays `interrupted` with no turn for the next mail to
    /// resume (spec 03 section 9).
    #[tokio::test]
    async fn an_admitted_start_whose_turn_was_released_is_not_bound_again() {
        let (_root, tree, gate, pending) = start_held_at_admission().await;
        {
            // The deliverer bound the turn it admitted; an interrupt ended
            // it and its waiter released it.
            let mut tree = tree.lock().unwrap();
            tree.bind_attempt_turn("c1", "turn-1").unwrap();
            tree.set_active_turn("c1", true).unwrap();
            tree.interrupt_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: "c1".into(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap();
            tree.release_attempt_turn("c1").unwrap();
        }
        gate.add_permits(1);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let tree = tree.lock().unwrap();
        let node = tree.node("c1").unwrap();
        assert_eq!(node.status, NodeStatus::Interrupted);
        assert!(!node.active_turn, "the released turn was marked active");
        let attempt = tree.attempt("c1").unwrap();
        assert_eq!(attempt.turn_id, None, "the released turn was bound again");
        assert_eq!(attempt.usage.turns, 1);
    }

    /// A Start whose turn a later turn replaced before the delivery returns
    /// keeps that later turn bound: the send never binds the turn its ack
    /// names over another one.
    #[tokio::test]
    async fn an_admitted_start_never_binds_over_a_later_turn() {
        let (_root, tree, gate, pending) = start_held_at_admission().await;
        {
            let mut tree = tree.lock().unwrap();
            tree.bind_attempt_turn("c1", "turn-1").unwrap();
            tree.set_active_turn("c1", true).unwrap();
            tree.bind_attempt_turn("c1", "turn-2").unwrap();
        }
        gate.add_permits(1);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let tree = tree.lock().unwrap();
        let attempt = tree.attempt("c1").unwrap();
        assert_eq!(attempt.turn_id.as_deref(), Some("turn-2"));
        assert_eq!(attempt.usage.turns, 2);
    }

    /// A send that waited behind another send's resume Start delivers its
    /// mail into the resumed turn as a follow-up once that Start is
    /// admitted. Its mail was claimed while the Start was composed, so the
    /// resumed turn never carried it; left unread, a resumed turn that
    /// submits would strand it under a `queued` success.
    #[tokio::test]
    async fn a_rider_reaches_the_resumed_turn() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        let stub = StubDeliverer::ok().binding(&tree);
        for _ in 0..2 {
            stub.first
                .lock()
                .unwrap()
                .push_back(Err(MailDeliveryFailure::TurnEnded));
        }
        let (stub, gate) = stub.holding_from(2);
        let deliverer = Arc::new(stub);
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let send = |operation: &'static str, text: &'static str| {
            let tool = Arc::clone(&tool);
            tokio::spawn(async move {
                tool.send_with(operation.into(), &send_request("c1", text, None))
                    .await
                    .unwrap()
            })
        };
        let first = send("op-mail-a", "use the v2 schema");
        until_calls(&deliverer, 1).await;
        let second = send("op-mail-b", "keep the old field too");
        until_calls(&deliverer, 2).await;
        release_unwound_turn(&tree);
        until_calls(&deliverer, 3).await;
        gate.add_permits(8);
        let mut ids = Vec::new();
        for pending in [first, second] {
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
                .await
                .unwrap()
                .unwrap();
            ids.push(
                outcome_json(&outcome)["message_id"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        let calls = deliverer.calls.lock().unwrap().clone();
        let rider = ids
            .iter()
            .find(|id| **id != calls[2].message_id)
            .unwrap()
            .clone();
        assert!(
            calls[3..]
                .iter()
                .any(|call| call.message_id == rider
                    && call.disposition == MailDisposition::Queued),
            "the rider's mail was never offered to the resumed turn; it stays unread ({:?}) \
             until that turn's unsubmitted end",
            tree.lock().unwrap().mail(&rider).map(|mail| mail.durable)
        );
        assert_eq!(
            tree.lock().unwrap().mail(&rider).map(|mail| mail.durable),
            Some(MailDurableState::Acknowledged),
            "the resumed turn's follow-up carried the rider's mail"
        );
    }

    #[tokio::test]
    async fn send_urgent_busy_child_steers() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        let deliverer = Arc::new(StubDeliverer::ok());
        let tool = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        tool.send_with("op-mail-1".into(), &send_request("c1", "hello", None))
            .await
            .unwrap();
        let body = "🦀".repeat(crate::protocol::MAX_MESSAGE_BYTES / 4);
        let refused = tool
            .send_with(
                "op-mail-too-big".into(),
                &send_request("c1", &(body.clone() + "a"), Some(MessagePriority::Urgent)),
            )
            .await
            .unwrap();
        assert_eq!(outcome_json(&refused)["code"], "bound_exceeded");
        assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
        let steered = tool
            .send_with(
                "op-mail-2".into(),
                &send_request("c1", &body, Some(MessagePriority::Urgent)),
            )
            .await
            .unwrap();
        assert_eq!(steered.kind(), ToolOutcomeKind::Success);
        assert_eq!(outcome_json(&steered)["disposition"], "steered");
        let calls = deliverer.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].disposition, MailDisposition::Steered);
        assert_eq!(calls[1].body, body);
        assert_eq!(calls[1].turn_token, 1);
        let tree = tree.lock().unwrap();
        assert_eq!(tree.attempt("c1").unwrap().turn_token, 1);
    }

    #[tokio::test]
    async fn abandoned_uncommitted_wait_receipt_restores_mail_even_after_history_pruning() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
            .spawn_with(
                "receipt-spawn".into(),
                &serde_json::to_value(spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        let mail = driver
            .tree
            .lock()
            .unwrap()
            .send_agent_message(
                "c1",
                &SendAgentMessageRequest {
                    agent_id: "root".into(),
                    message: "uncommitted question".into(),
                    purpose: Some(MessagePurpose::Question),
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        wait_tool(driver.clone())
            .wait_operation_with("abandoned-wait", &json!({}))
            .await
            .unwrap();
        {
            let mut tree = driver.tree.lock().unwrap();
            for _ in 0..70 {
                let next = tree
                    .send_agent_message(
                        "c1",
                        &SendAgentMessageRequest {
                            agent_id: "root".into(),
                            message: "later acknowledged mail".into(),
                            purpose: None,
                            priority: None,
                            in_reply_to: None,
                            output_schema_json: None,
                        },
                        None,
                    )
                    .unwrap();
                tree.acknowledge_mail(&next.message_id).unwrap();
            }
            driver.save(&mut tree).unwrap();
        }
        // Reopen the durable cut after receipt persistence but before the
        // tool result reached canonical history. History pruning is not loss.
        let mut reopened = reload(&store, "tree1");
        assert!(reopened.mail(&mail.message_id).is_none());
        assert_eq!(
            reopened
                .settle_wait_receipts("root", &[("abandoned-wait".into(), false)])
                .unwrap()
                .restored_mail,
            1
        );
        assert_eq!(
            reopened.mail(&mail.message_id).unwrap().durable,
            MailDurableState::Accepted
        );
        assert_eq!(
            reopened.pending_mail("root")[0].body,
            "uncommitted question"
        );
        assert_eq!(
            reopened
                .settle_wait_receipts("root", &[("abandoned-wait".into(), false)])
                .unwrap()
                .restored_mail,
            0
        );
    }

    /// Settlement follows canonical commit evidence per receipt: a committed
    /// wait is retired with its acknowledgements standing, an uncommitted
    /// one is rolled back, and settling again changes nothing.
    #[tokio::test]
    async fn committed_wait_evidence_retires_and_uncommitted_evidence_restores() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        tree.record_turn_completed("c1").unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let waiter = wait_tool(driver.clone());
        waiter
            .wait_operation_with("committed-wait", &json!({}))
            .await
            .unwrap();
        {
            let mut tree = driver.tree.lock().unwrap();
            tree.send_agent_message(
                "c1",
                &SendAgentMessageRequest {
                    agent_id: "root".into(),
                    message: "uncommitted note".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        }
        waiter
            .wait_operation_with("uncommitted-wait", &json!({"agent_ids": ["c1"]}))
            .await
            .unwrap();
        let mut tree = driver.tree.lock().unwrap();
        assert_eq!(
            tree.pending_mail("root"),
            [] as [crate::tree::MailRecord; 0]
        );
        let mut retained = tree.retained_wait_receipts("root");
        retained.sort();
        assert_eq!(retained, ["committed-wait", "uncommitted-wait"]);
        let evidence = [
            ("committed-wait".to_owned(), true),
            ("uncommitted-wait".to_owned(), false),
        ];
        let settled = tree.settle_wait_receipts("root", &evidence).unwrap();
        assert_eq!(
            settled,
            crate::tree::WaitReceiptSettlement {
                retired: 1,
                rolled_back: 1,
                restored_mail: 1,
            }
        );
        assert_eq!(tree.retained_wait_receipts("root"), [] as [String; 0]);
        assert!(tree.delivered_to("root", "c1"));
        let pending = tree.pending_mail("root");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "uncommitted note");
        assert_eq!(
            tree.settle_wait_receipts("root", &evidence).unwrap(),
            crate::tree::WaitReceiptSettlement::default()
        );
    }

    #[tokio::test]
    async fn wait_recovery_only_undoes_deliveries_consumed_by_that_operation() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        tree.record_turn_completed("c1").unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let waiter = wait_tool(driver.clone());
        waiter
            .wait_operation_with("uncommitted-settlement", &json!({}))
            .await
            .unwrap();
        {
            let mut tree = driver.tree.lock().unwrap();
            assert!(tree.delivered_to("root", "c1"));
            assert_eq!(
                tree.settle_wait_receipts("root", &[("uncommitted-settlement".into(), false)])
                    .unwrap()
                    .restored_mail,
                1
            );
            assert!(!tree.delivered_to("root", "c1"));
        }
        waiter
            .wait_operation_with("committed-settlement", &json!({}))
            .await
            .unwrap();
        waiter
            .wait_operation_with("uncommitted-reread", &json!({"agent_ids": ["c1"]}))
            .await
            .unwrap();
        let mut tree = driver.tree.lock().unwrap();
        assert_eq!(
            tree.settle_wait_receipts("root", &[("uncommitted-reread".into(), false)])
                .unwrap()
                .restored_mail,
            0
        );
        assert!(tree.delivered_to("root", "c1"));
        assert_eq!(
            tree.pending_mail("root"),
            [] as [crate::tree::MailRecord; 0]
        );
    }

    #[tokio::test]
    async fn five_hundred_waits_across_committed_turns_keep_receipts_bounded() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let waiter = wait_tool(driver.clone());
        for turn in 0..500 {
            {
                let mut tree = driver.tree.lock().unwrap();
                // The previous turn committed and its terminal retired it.
                tree.retire_wait_receipts("root");
                tree.send_agent_message(
                    "c1",
                    &SendAgentMessageRequest {
                        agent_id: "root".into(),
                        message: format!("finding {turn}"),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap();
            }
            let outcome = waiter
                .wait_operation_with(&format!("wait-{turn}"), &json!({}))
                .await
                .unwrap();
            assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
            let tree = driver.tree.lock().unwrap();
            assert_eq!(
                tree.operations()
                    .values()
                    .filter(|op| op.tool_name == "wait_agent")
                    .count(),
                1
            );
            assert!(tree.messages().len() <= 64);
        }
        let mut reopened = reload(&store, "tree1");
        assert_eq!(
            reopened
                .settle_wait_receipts("root", &[("wait-499".into(), false)])
                .unwrap()
                .restored_mail,
            1
        );
        assert_eq!(reopened.retire_wait_receipts("root"), 0);
    }

    #[tokio::test]
    async fn committed_child_completion_retires_only_its_own_wait_receipts() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.grants_mut_for_test("root").max_depth = 3;
        tree.spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        tree.spawn_agent("c1", &spawn_request(), None).unwrap();
        tree.bind_attempt_turn("c1", "child-turn").unwrap();
        for (sender, recipient) in [("c1", "root"), ("c2", "c1")] {
            tree.send_agent_message(
                sender,
                &SendAgentMessageRequest {
                    agent_id: recipient.into(),
                    message: "question".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        }
        let driver = driver_on(tree, Arc::clone(&store));
        wait_tool(driver.clone())
            .wait_operation_with("root-wait", &json!({}))
            .await
            .unwrap();
        let mut child_waiter = wait_tool(driver.clone());
        child_waiter.caller_id = Arc::new(Mutex::new("c1".into()));
        child_waiter
            .wait_operation_with("child-wait", &json!({}))
            .await
            .unwrap();
        let mut tree = driver.tree.lock().unwrap();
        assert!(tree.operations().contains_key("child-wait"));
        tree.set_active_turn("c1", true).unwrap();
        assert!(
            tree.operations().contains_key("child-wait"),
            "same bound-turn rejoin keeps rollback evidence"
        );
        tree.record_turn_completed("c1").unwrap();
        assert!(!tree.operations().contains_key("child-wait"));
        assert!(tree.operations().contains_key("root-wait"));
        driver.save(&mut tree).unwrap();
        let reopened = reload(&store, "tree1");
        assert!(!reopened.operations().contains_key("child-wait"));
        assert!(reopened.operations().contains_key("root-wait"));
    }

    /// Spec 03 section 4.4: a child terminal settles its own receipts. A
    /// cancelled turn closed its exchange, so releasing it retires them; a
    /// failure leaves them to the host's settlement, and neither the
    /// followup that opens the next attempt nor its first turn retires them
    /// blindly, so uncommitted evidence still restores the mail.
    #[tokio::test]
    async fn child_receipts_retire_by_terminal_kind_and_never_at_followup() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.grants_mut_for_test("root").max_depth = 3;
        tree.spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        tree.spawn_agent("c1", &spawn_request(), None).unwrap();
        tree.bind_attempt_turn("c1", "child-turn").unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let mut child_waiter = wait_tool(driver.clone());
        child_waiter.caller_id = Arc::new(Mutex::new("c1".into()));
        for wait in ["interrupted-wait", "failed-wait"] {
            driver
                .tree
                .lock()
                .unwrap()
                .send_agent_message(
                    "c2",
                    &SendAgentMessageRequest {
                        agent_id: "c1".into(),
                        message: wait.into(),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap();
            child_waiter
                .wait_operation_with(wait, &json!({ "agent_ids": ["c2"] }))
                .await
                .unwrap();
            let mut tree = driver.tree.lock().unwrap();
            assert_eq!(tree.pending_mail("c1"), [] as [crate::tree::MailRecord; 0]);
            if wait == "interrupted-wait" {
                tree.release_attempt_turn("c1").unwrap();
                assert_eq!(tree.retained_wait_receipts("c1"), [] as [String; 0]);
                tree.bind_attempt_turn("c1", "child-turn-2").unwrap();
            }
        }
        let mut tree = driver.tree.lock().unwrap();
        tree.record_turn_failure(
            "c1",
            crate::tree::turn_outcome::FailedTurnDisposition::Failed {
                kind: "turn_failed",
            },
            "contained panic".into(),
        )
        .unwrap();
        tree.settle_for_test();
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
                agent_id: "c1".into(),
                message: "again".into(),
                purpose: Some(MessagePurpose::Followup),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
        tree.set_active_turn("c1", true).unwrap();
        assert_eq!(tree.retained_wait_receipts("c1"), ["failed-wait"]);
        let settled = tree
            .settle_wait_receipts("c1", &[("failed-wait".into(), false)])
            .unwrap();
        assert_eq!(settled.restored_mail, 1);
        let unread: Vec<String> = tree
            .pending_mail("c1")
            .into_iter()
            .filter(|mail| mail.from == "c2")
            .map(|mail| mail.body)
            .collect();
        assert_eq!(unread, ["failed-wait"]);
    }

    #[tokio::test]
    async fn timeout_without_mail_does_not_publish_a_wait_receipt() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("timeout-no-write", root.path());
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::new(FailingStore));
        let outcome = wait_tool(driver.clone())
            .wait_operation_with("empty-wait", &json!({"timeout_ms": 1}))
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        assert_eq!(outcome_json(&outcome)["woke_by"], "timeout");
        assert!(driver.tree.lock().unwrap().parked_waits.is_empty());
        assert!(
            !driver
                .tree
                .lock()
                .unwrap()
                .operations()
                .contains_key("empty-wait")
        );
    }

    #[tokio::test]
    async fn child_retention_runtime_archive_delete_crash_cuts() {
        use plakous::sessions::*;
        use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
        for cut in 0..3 {
            let root = tempfile::tempdir().unwrap();
            let controller = BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                root.path().join("evidence.jsonl"),
            )
            .unwrap();
            let plan = Arc::new(
                serde_json::from_value(
                    json!({"version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "steps": [{"output": {"text": "done"}}]}),
                )
                .unwrap(),
            );
            let runtime = scripted_runtime_binding(root.path(), controller, plan).unwrap();
            let sessions = AgentSessions::new(
                SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
            )
            .unwrap();
            let CreateSessionOutcome::Created { session, .. } = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new("retention:create").unwrap(),
                    runtime,
                ))
                .await
                .unwrap()
            else {
                panic!("created session required")
            };
            let locator = session.release().await.unwrap();
            let mut tree = bootstrap_tree("child-retention-runtime", root.path());
            let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
            tree.bind_session_locator(&child.agent_id, locator.to_string())
                .unwrap();
            tree.record_turn_completed(&child.agent_id).unwrap();
            tree.settle_for_test();
            let result = tree
                .poll_wait("root", &AwaitAgents::Pending)
                .unwrap()
                .unwrap();
            tree.acknowledge_wait_mail("root", &result);
            assert_eq!(tree.prepare_child_retention(u64::MAX).unwrap().len(), 1);
            let store = Arc::new(MemoryTreeStore::new());
            let driver = driver_on(tree, store.clone());
            driver.mutate(|tree| driver.save(tree)).unwrap().0.unwrap();
            if cut > 0 {
                sessions.archive(locator).await.unwrap();
            }
            if cut > 1 {
                sessions
                    .delete_archived(locator.session_id())
                    .await
                    .unwrap();
            }
            driver.collect_dormant_children(&sessions).await.unwrap();
            driver.collect_dormant_children(&sessions).await.unwrap();
            assert!(
                driver
                    .read(|tree| tree.node(&child.agent_id).is_err())
                    .unwrap()
            );
            assert!(
                sessions
                    .delete_archived(locator.session_id())
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[derive(Default)]
    struct FixtureJobHost(Mutex<Option<plakous::tools::JoinedProcessSessions>>);

    impl plakous::tools::JobHost for FixtureJobHost {
        fn service(
            &self,
            _: plakous::model::SessionId,
            _: plakous::model::BranchId,
        ) -> Option<plakous::tools::JoinedProcessSessions> {
            self.0.lock().unwrap().clone()
        }
        fn attach(
            &self,
            _: plakous::tools::JobOwner,
            fresh: plakous::tools::JoinedProcessSessions,
        ) -> Result<plakous::tools::JoinedProcessSessions, plakous::tools::ExecutionBackendError>
        {
            *self.0.lock().unwrap() = Some(fresh.clone());
            Ok(fresh)
        }
    }

    async fn install_running_job(
        driver: &TreeDriver,
        caller: &str,
        root: &Path,
    ) -> (
        plakous::tools::JoinedProcessSessions,
        plakous::sessions::OpenedSession,
    ) {
        use plakous::sessions::*;
        use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
        let controller = BoundaryController::quiet_from_evidence_path(
            plakous_offline_crash_harness::protocol::CrashBoundary::None,
            root.join("job-evidence.jsonl"),
        )
        .unwrap();
        let plan = Arc::new(serde_json::from_value(json!({"version": plakous_offline_crash_harness::script::SCRIPT_VERSION, "process_tools": true, "steps": [
            {"output": {"tool": {"name": "process_session_start", "arguments": {"command": "read line", "workdir": ".", "login": false, "tty": false, "keep_stdin_open": true, "yield_time_ms": 0, "max_output_bytes": 8192}}}, "finish": "tool_calls"},
            {"output": {"text": "ready"}}
        ]})).unwrap());
        let host = Arc::new(FixtureJobHost::default());
        let runtime = scripted_runtime_binding(root, controller, plan)
            .unwrap()
            .with_job_host(host.clone());
        let sessions =
            AgentSessions::new(SessionBackend::sqlite(root.join("job-runtime.sqlite3")).unwrap())
                .unwrap();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("fixture-job:create").unwrap(),
                runtime,
            ))
            .await
            .unwrap()
        else {
            panic!("created job session required")
        };
        session
            .agent()
            .prompt("start a job")
            .await
            .unwrap()
            .result()
            .await
            .unwrap();
        let jobs = host.0.lock().unwrap().clone().unwrap();
        assert_eq!(jobs.jobs()[0].status, plakous::tools::JobStatus::Running);
        driver
            .tree
            .lock()
            .unwrap()
            .bind_session_locator(caller, session.locator().to_string())
            .unwrap();
        driver.bind_job_host(Some(host));
        (jobs, session)
    }

    #[tokio::test]
    async fn owned_process_wakes_only_waits_admitted_before_its_terminal_transition() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("process-no-write", root.path());
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::new(FailingStore));
        let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
        let waiter = wait_tool(driver.clone());
        let arguments = json!({"timeout_ms": 300_000});
        let mut pending = Box::pin(waiter.wait_operation_with("process-wait", &arguments));
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
        jobs.stop_jobs(
            plakous::tools::JobStopReason::Model,
            std::time::Duration::from_millis(250),
        )
        .unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome_json(&outcome)["woke_by"], "process");
        assert_eq!(outcome_json(&outcome)["processes"], json!(["p1"]));
        assert!(driver.tree.lock().unwrap().operations().is_empty());
        let next = waiter
            .wait_operation_with("next-process-wait", &json!({"timeout_ms": 1}))
            .await
            .unwrap();
        assert_eq!(outcome_json(&next)["woke_by"], "timeout");
    }

    #[tokio::test]
    async fn wait_resolves_jobs_that_attach_after_admission() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("late-job-service", root.path());
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::new(FailingStore));
        let waiter = wait_tool(driver.clone());
        let arguments = json!({"timeout_ms": 300_000});
        let mut pending = Box::pin(waiter.wait_operation_with("late-job-wait", &arguments));
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
        let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
        driver.read(|tree| tree.waiters().notify_waiters()).unwrap();
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
        jobs.stop_jobs(
            plakous::tools::JobStopReason::Model,
            std::time::Duration::from_millis(250),
        )
        .unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome_json(&outcome)["woke_by"], "process");
    }

    /// A fan-out whose results together outgrow the tool outcome limit
    /// used to fail every wait. The aggregate budget returns them in
    /// batches: each return stays within it, a withheld result stays
    /// pending with its result envelope unread, and every result arrives
    /// exactly once.
    #[tokio::test]
    async fn a_fan_out_wait_past_the_outcome_limit_returns_in_budgeted_batches() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let children = ["c1", "c2", "c3", "c4", "c5", "c6"];
        for (index, child) in children.iter().enumerate() {
            let mut request = spawn_request();
            request.request_key = Some(format!("budget-{index}"));
            request.output_schema_json = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"blob":{"type":"string"}},"required":["blob"],"additionalProperties":false}"#.into();
            spawn_tool(
                driver.clone(),
                Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
            )
            .spawn_with(
                format!("op-spawn-budget-{index}"),
                &serde_json::to_value(request).unwrap(),
            )
            .await
            .unwrap();
            let mut tree = tree.lock().unwrap();
            tree.submit_result(
                child,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: format!(r#"{{"blob":"{}"}}"#, child.repeat(100 * 1024)),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed(child).unwrap();
        }
        let mut seen = Vec::new();
        for wave in 0..children.len() {
            let outcome = wait_tool(driver.clone())
                .wait_operation_with(
                    &format!("budget-wait-{wave}"),
                    &json!({ "timeout_ms": 1000 }),
                )
                .await
                .unwrap();
            let ToolResultPart::Text(block) = &outcome.parts()[0] else {
                panic!("expected text tool outcome");
            };
            assert!(block.text().len() <= crate::tree::WAIT_OUTPUT_BUDGET_BYTES);
            let output = outcome_json(&outcome);
            let delivered: Vec<String> = output["agents"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["output_json"].is_string())
                .map(|row| row["agent_id"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(delivered.len(), 1, "wave {wave}: {}", output["omitted"]);
            let withheld = children.len() - wave - 1;
            if withheld == 0 {
                assert!(output.get("omitted").is_none());
            } else {
                assert_eq!(
                    output["omitted"]["outputs"].as_array().unwrap().len(),
                    withheld
                );
                assert_eq!(output["omitted"]["mail"], withheld);
                let tree = tree.lock().unwrap();
                for id in output["omitted"]["outputs"].as_array().unwrap() {
                    assert!(
                        tree.unread_mail("root")
                            .any(|mail| mail.from == id.as_str().unwrap()),
                        "a withheld result keeps its envelope unread"
                    );
                }
            }
            seen.extend(delivered);
        }
        seen.sort();
        assert_eq!(seen, children);
        let refused = wait_tool(driver.clone())
            .wait_with(&json!({ "timeout_ms": 1000 }))
            .await
            .unwrap();
        assert_eq!(outcome_json(&refused)["code"], "no_pending_children");
    }

    /// Mail the budget withheld from a child whose row the same return
    /// delivered stays watched: the next bare wait returns it at once instead
    /// of refusing `no_pending_children`.
    #[tokio::test]
    async fn withheld_mail_from_a_delivered_child_returns_on_the_next_wait() {
        withheld_mail_from_a_delivered_child_returns(true).await;
    }

    /// The same for a settled row without `output_json`: its Result envelope
    /// is plain budgeted mail, and when the budget withholds it the next bare
    /// wait still returns it.
    #[tokio::test]
    async fn a_withheld_result_envelope_of_a_delivered_row_returns_on_the_next_wait() {
        withheld_mail_from_a_delivered_child_returns(false).await;
    }

    async fn withheld_mail_from_a_delivered_child_returns(submit: bool) {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let mut request = spawn_request();
        request.request_key = Some("probe-mail".to_owned());
        request.output_schema_json = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"blob":{"type":"string"}},"required":["blob"],"additionalProperties":false}"#.into();
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-probe-mail".into(),
            &serde_json::to_value(request).unwrap(),
        )
        .await
        .unwrap();
        {
            let mut tree = tree.lock().unwrap();
            // Without a 200 KiB output, enough mail alone overruns the budget.
            let count = if submit { 40 } else { 140 };
            for index in 0..count {
                tree.send_agent_message(
                    "c1",
                    &SendAgentMessageRequest {
                        agent_id: "root".into(),
                        message: format!("{index:03}{}", "m".repeat(2000)),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap();
            }
            if submit {
                tree.submit_result(
                    "c1",
                    &SubmitResultRequest {
                        turn_token: 1,
                        output_json: format!(r#"{{"blob":"{}"}}"#, "c".repeat(200 * 1024)),
                    },
                    None,
                )
                .unwrap();
            }
            tree.record_turn_completed("c1").unwrap();
        }
        let first = wait_tool(driver.clone())
            .wait_operation_with("probe-mail-1", &json!({ "timeout_ms": 1000 }))
            .await
            .unwrap();
        let first = outcome_json(&first);
        assert_eq!(first["agents"][0]["output_json"].is_string(), submit);
        let withheld = first["omitted"]["mail"].as_u64().unwrap();
        assert!(withheld > 0, "{}", first["omitted"]);
        let second = wait_tool(driver.clone())
            .wait_operation_with("probe-mail-2", &json!({ "timeout_ms": 1000 }))
            .await
            .unwrap();
        let second = outcome_json(&second);
        assert_eq!(
            second["mail"].as_array().map(Vec::len),
            Some(usize::try_from(withheld).unwrap()),
            "the next wait returns the withheld mail: {second}"
        );
        if !submit {
            assert_eq!(
                second["mail"].as_array().unwrap().last().unwrap()["purpose"],
                "result"
            );
        }
    }

    /// Spec 03 section 4.4: a bare wait delivers each settled row once. It
    /// keeps watching a delivered child whose mail the budget withheld, and
    /// the next bare wait returns that mail without the delivered
    /// `output_json`, neither re-sent whole nor listed under
    /// `omitted.rereads`: re-reads exist only for named waits.
    #[tokio::test]
    async fn a_bare_wait_over_withheld_mail_does_not_resend_a_delivered_output() {
        // Forty withheld envelopes leave room to re-send the output whole;
        // ninety leave none, so it would be listed as a re-read instead.
        for count in [40, 90] {
            let root = tempfile::tempdir().unwrap();
            let (store, tree) = persist_fixture(&root);
            let driver = driver_on(tree, Arc::clone(&store));
            let tree = Arc::clone(&driver.tree);
            let mut request = spawn_request();
            request.request_key = Some("probe-reread".to_owned());
            request.output_schema_json = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"blob":{"type":"string"}},"required":["blob"],"additionalProperties":false}"#.into();
            spawn_tool(
                driver.clone(),
                Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
            )
            .spawn_with(
                "op-spawn-probe-reread".into(),
                &serde_json::to_value(request).unwrap(),
            )
            .await
            .unwrap();
            {
                let mut tree = tree.lock().unwrap();
                for index in 0..count {
                    tree.send_agent_message(
                        "c1",
                        &SendAgentMessageRequest {
                            agent_id: "root".into(),
                            message: format!("{index:03}{}", "m".repeat(2000)),
                            purpose: None,
                            priority: None,
                            in_reply_to: None,
                            output_schema_json: None,
                        },
                        None,
                    )
                    .unwrap();
                }
                tree.submit_result(
                    "c1",
                    &SubmitResultRequest {
                        turn_token: 1,
                        output_json: format!(r#"{{"blob":"{}"}}"#, "c".repeat(200 * 1024)),
                    },
                    None,
                )
                .unwrap();
                tree.record_turn_completed("c1").unwrap();
            }
            let first = wait_tool(driver.clone())
                .wait_operation_with("probe-reread-1", &json!({ "timeout_ms": 1000 }))
                .await
                .unwrap();
            let first = outcome_json(&first);
            assert!(first["agents"][0]["output_json"].is_string());
            assert!(first["omitted"]["mail"].as_u64().unwrap() > 0);
            let second = wait_tool(driver.clone())
                .wait_operation_with("probe-reread-2", &json!({ "timeout_ms": 1000 }))
                .await
                .unwrap();
            let second = outcome_json(&second);
            assert!(
                !second["agents"][0]["output_json"].is_string(),
                "{count} envelopes: a bare wait re-sent the delivered output ({} bytes)",
                second["agents"][0]["output_json"]
                    .as_str()
                    .map_or(0, str::len)
            );
            assert!(
                second["omitted"]["rereads"]
                    .as_array()
                    .is_none_or(Vec::is_empty),
                "{count} envelopes: a bare wait listed a delivered output as a re-read: {}",
                second["omitted"]
            );
        }
    }

    /// The await engine's process targets: a named own process wakes the
    /// await when it ends and is returned as a snapshot row; a finished one
    /// returns at once; an id outside the caller's session is refused.
    #[tokio::test]
    async fn an_await_naming_an_own_process_returns_its_row_when_it_ends() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("process-await", root.path());
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::new(FailingStore));
        let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
        let waiter = wait_tool(driver.clone());
        let named = |id: &str| AwaitRequest {
            agents: AwaitAgents::Unwatched,
            processes: vec![id.to_owned()],
            timeout_ms: 300_000,
        };
        let refused = waiter
            .await_operation("await-p9", &named("p9"), &json!({}), None)
            .await
            .unwrap();
        assert_eq!(outcome_json(&refused)["code"], "grant_denied");
        assert!(
            outcome_json(&refused)["message"]
                .as_str()
                .unwrap()
                .contains("p9 is not in this session")
        );
        let request = named("p1");
        let presented = json!({});
        let mut pending = Box::pin(waiter.await_operation("await-p1", &request, &presented, None));
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
        jobs.stop_jobs(
            plakous::tools::JobStopReason::Model,
            std::time::Duration::from_millis(250),
        )
        .unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap();
        let output = outcome_json(&outcome);
        assert_eq!(output["woke_by"], "process");
        assert_eq!(output["agents"], json!([]));
        assert_eq!(output["jobs"][0]["id"], "p1");
        assert_eq!(output["jobs"][0]["status"], "stopped");
        assert!(output["jobs"][0]["output"].is_string());
        assert!(driver.tree.lock().unwrap().operations().is_empty());
        let again = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            waiter.await_operation("await-p1-again", &request, &json!({}), None),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(outcome_json(&again)["jobs"][0]["status"], "stopped");
    }

    #[test]
    fn an_own_job_no_longer_listed_is_already_reported_not_a_refusal() {
        let ended = vec!["p2".to_owned()];
        assert!(unlisted_job_admitted("p2", &ended).is_ok());
        let refused = unlisted_job_admitted("p9", &ended).unwrap_err();
        assert_eq!(refused.code, TopologyErrorCode::GrantDenied);
        assert_eq!(refused.message, "process p9 is not in this session");
        let rows = await_job_rows(None, &["p2".to_owned()]);
        assert_eq!(rows[0].id, "p2");
        assert_eq!(rows[0].status, "already_reported");
        assert!(rows[0].output.is_none());
    }

    #[tokio::test]
    async fn idle_submit_with_live_job_refuses_without_leaf_only_unavailable_remedies() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, store);
        let (jobs, _opened) = install_running_job(&driver, &child.agent_id, root.path()).await;
        let outcome = invoke_granted_topology_tool_on(
            driver.clone(),
            child.agent_id,
            "submit_result",
            &json!({"turn_token": 1, "output_json": "{\"ok\":true}"}),
            "submit-live-job".into(),
            TopologyHostPorts::default(),
        )
        .await
        .unwrap();
        let payload = outcome_json(&outcome);
        assert_eq!(payload["code"], "live_work");
        let message = payload["message"].as_str().unwrap();
        assert!(message.contains("p1"));
        for unavailable in ["wait_agent", "close_agent", "interrupt_agent"] {
            assert!(!message.contains(unavailable));
        }
        jobs.stop_jobs(
            plakous::tools::JobStopReason::Model,
            std::time::Duration::from_millis(250),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn close_stops_child_jobs_but_interrupt_preserves_them() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, store);
        let (jobs, _opened) = install_running_job(&driver, &child.agent_id, root.path()).await;
        let interrupted = interrupt_tool(driver.clone(), None)
            .interrupt_with("interrupt-job".into(), &json!({"agent_id": child.agent_id}))
            .await
            .unwrap();
        assert_eq!(interrupted.kind(), ToolOutcomeKind::Success);
        assert_eq!(jobs.jobs()[0].status, plakous::tools::JobStatus::Running);
        let closed = close_tool(driver, None)
            .close_with("close-job".into(), &json!({"agent_id": child.agent_id}))
            .await
            .unwrap();
        assert_eq!(closed.kind(), ToolOutcomeKind::Success);
        assert_eq!(
            jobs.jobs()[0].status,
            plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::AttemptEnded)
        );
    }

    struct ExpiredCloseCanceller(TreeDriver);

    impl LiveTurnCanceller for ExpiredCloseCanceller {
        fn cancel_agents(
            &self,
            ids: Vec<String>,
        ) -> Pin<Box<dyn Future<Output = Result<LiveCancelOutcome, LiveCancelFailure>> + Send + '_>>
        {
            Box::pin(async move {
                self.0
                    .mutate_and_settle(|tree| {
                        tree.record_host_retirement(&ids[0], "owner deadline".into())
                            .and_then(|value| tree.fold_due(&ids[0]).map(|_| value))
                            .unwrap();
                        self.0.save(tree).unwrap();
                    })
                    .unwrap();
                Err(LiveCancelFailure {
                    message: "joiner deadline; owner retained".into(),
                    failed_ids: Vec::new(),
                    joiner_expired_ids: ids,
                    owner_failures: Vec::new(),
                    cancelled_ids: Vec::new(),
                    not_live_ids: Vec::new(),
                })
            })
        }
    }

    struct RecoverDuringJobStop {
        driver: TreeDriver,
        child: String,
    }

    impl plakous::tools::JobHost for RecoverDuringJobStop {
        fn service(
            &self,
            _: plakous::model::SessionId,
            _: plakous::model::BranchId,
        ) -> Option<plakous::tools::JoinedProcessSessions> {
            self.driver
                .mutate_and_settle(|tree| {
                    let attempt = tree.attempt(&self.child).unwrap().attempt_id.clone();
                    tree.release_turn_owner(&self.child);
                    tree.set_active_turn(&self.child, false).unwrap();
                    tree.recover_host_decision(
                        &self.child,
                        &attempt,
                        crate::tree::RecoveryResolution::RecoveredWithoutTurn {
                            message: "late release returned during job stop".into(),
                        },
                    )
                    .unwrap();
                    self.driver.save(tree).unwrap();
                })
                .unwrap();
            None
        }

        fn attach(
            &self,
            _: plakous::tools::JobOwner,
            fresh: plakous::tools::JoinedProcessSessions,
        ) -> Result<plakous::tools::JoinedProcessSessions, plakous::tools::ExecutionBackendError>
        {
            Ok(fresh)
        }
    }

    #[tokio::test]
    async fn close_joiner_expiry_does_not_refault_recovery_during_job_stop() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        let child = tree
            .spawn_agent("root", &spawn_request(), None)
            .unwrap()
            .agent_id;
        tree.set_active_turn(&child, true).unwrap();
        let driver = driver_on(tree, store);
        let (_jobs, _opened) = install_running_job(&driver, &child, root.path()).await;
        driver.bind_job_host(Some(Arc::new(RecoverDuringJobStop {
            driver: driver.clone(),
            child: child.clone(),
        })));
        let outcome = close_tool(
            driver.clone(),
            Some(Arc::new(ExpiredCloseCanceller(driver.clone()))),
        )
        .close_with("close-expired-joiner".into(), &json!({"agent_id": child}))
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        driver
            .read(|tree| {
                assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
                assert_eq!(
                    tree.attempt(&child).unwrap().status,
                    crate::protocol::AttemptStatus::Closed
                );
                assert_eq!(
                    tree.recovery_need(&child).unwrap(),
                    crate::tree::RecoveryNeed::NotRequired
                );
                assert_eq!(
                    tree.pending_result_mail("root"),
                    [] as [crate::tree::MailRecord; 0]
                );
            })
            .unwrap();
        driver.bind_job_host(None);
    }

    #[tokio::test]
    async fn settling_unrelated_effects_does_not_stop_historical_attempt_jobs() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, store);
        let (jobs, _opened) = install_running_job(&driver, &child.agent_id, root.path()).await;
        driver
            .mutate_and_settle(|tree| tree.record_turn_completed(&child.agent_id))
            .unwrap()
            .0
            .unwrap();
        assert_eq!(jobs.jobs()[0].status, plakous::tools::JobStatus::Running);
        jobs.stop_jobs(
            plakous::tools::JobStopReason::Model,
            std::time::Duration::from_millis(250),
        )
        .unwrap();
    }

    struct JobCheckingCanceller(plakous::tools::JoinedProcessSessions);

    struct LateJobCanceller {
        driver: TreeDriver,
        agent: String,
        path: std::path::PathBuf,
        job: Mutex<
            Option<(
                plakous::tools::JoinedProcessSessions,
                plakous::sessions::OpenedSession,
            )>,
        >,
    }
    impl LiveTurnCanceller for LateJobCanceller {
        fn cancel_agents(
            &self,
            agent_ids: Vec<String>,
        ) -> Pin<Box<dyn Future<Output = Result<LiveCancelOutcome, LiveCancelFailure>> + Send + '_>>
        {
            Box::pin(async move {
                assert_eq!(
                    self.driver
                        .read(|tree| tree.node(&self.agent).unwrap().status)
                        .unwrap(),
                    NodeStatus::Closed
                );
                let job = install_running_job(&self.driver, &self.agent, &self.path).await;
                *self.job.lock().unwrap() = Some(job);
                Ok(LiveCancelOutcome {
                    cancelled_ids: agent_ids,
                    not_live_ids: Vec::new(),
                })
            })
        }
    }

    #[tokio::test]
    async fn a_job_promoted_between_closure_and_cancel_is_stopped_not_parked() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, store);
        let canceller = Arc::new(LateJobCanceller {
            driver: driver.clone(),
            agent: child.agent_id.clone(),
            path: root.path().to_owned(),
            job: Mutex::new(None),
        });
        let outcome = close_tool(driver, Some(canceller.clone()))
            .close_with(
                "close-late-job".into(),
                &json!({"agent_id": child.agent_id}),
            )
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        assert_eq!(
            canceller.job.lock().unwrap().as_ref().unwrap().0.jobs()[0].status,
            plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::AttemptEnded)
        );
    }

    impl LiveTurnCanceller for JobCheckingCanceller {
        fn cancel_agents(
            &self,
            agent_ids: Vec<String>,
        ) -> Pin<Box<dyn Future<Output = Result<LiveCancelOutcome, LiveCancelFailure>> + Send + '_>>
        {
            Box::pin(async move {
                assert_eq!(
                    self.0.jobs()[0].status,
                    plakous::tools::JobStatus::Running,
                    "cancel live turns before stopping jobs"
                );
                Ok(LiveCancelOutcome {
                    cancelled_ids: agent_ids,
                    not_live_ids: Vec::new(),
                })
            })
        }
    }

    #[tokio::test]
    async fn close_cancels_live_turn_before_stopping_its_jobs() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, store);
        let (jobs, _opened) = install_running_job(&driver, &child.agent_id, root.path()).await;
        let closed = close_tool(driver, Some(Arc::new(JobCheckingCanceller(jobs.clone()))))
            .close_with("close-order".into(), &json!({"agent_id": child.agent_id}))
            .await
            .unwrap();
        assert_eq!(closed.kind(), ToolOutcomeKind::Success);
        assert_eq!(
            jobs.jobs()[0].status,
            plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::AttemptEnded)
        );
    }

    #[tokio::test]
    async fn steer_wakes_wait_without_mail_or_a_durable_receipt() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("steer-no-write", root.path());
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::new(FailingStore));
        let waiter = wait_tool(driver.clone());
        let (sender, receiver) = tokio::sync::watch::channel(plakous::tools::ToolPreempt::None);
        let arguments = json!({"timeout_ms": 300_000});
        let mut pending =
            Box::pin(waiter.wait_operation_preempt("steer-wait", &arguments, Some(receiver)));
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
        sender.send(plakous::tools::ToolPreempt::Steer).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap();
        let output = outcome_json(&result);
        assert_eq!(output["woke_by"], "steer");
        assert_eq!(output["mail"], json!([]));
        assert!(driver.tree.lock().unwrap().operations().is_empty());
    }

    #[tokio::test]
    async fn parked_hub_wait_is_the_only_consumer_of_a_child_question() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.grants_mut_for_test("root").max_depth = 3;
        let driver = driver_on(tree, Arc::clone(&store));
        spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
            .spawn_with(
                "hub-spawn".into(),
                &serde_json::to_value(SpawnAgentRequest::test_orchestrate()).unwrap(),
            )
            .await
            .unwrap();
        {
            let mut tree = driver.tree.lock().unwrap();
            tree.spawn_agent("c1", &spawn_request(), None).unwrap();
        }
        let mut waiter = wait_tool(driver.clone());
        waiter.caller_id = Arc::new(Mutex::new("c1".into()));
        let arguments = json!({"agent_ids": ["c2"], "timeout_ms": 300_000});
        let mut pending = Box::pin(waiter.wait_operation_with("hub-wait", &arguments));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        let deliverer = Arc::new(StubDeliverer::ok());
        let mut send = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        send.caller_id = Arc::new(Mutex::new("c2".into()));
        let arguments = json!({"agent_id": "c1", "message": "which file?", "purpose": "question", "priority": "urgent"});
        let (receipt, outcome) =
            tokio::join!(send.send_with("hub-question".into(), &arguments), pending);
        let receipt = receipt.unwrap();
        assert_eq!(receipt.kind(), ToolOutcomeKind::Success);
        let output = outcome_json(&outcome.unwrap());
        assert_eq!(output["woke_by"], "mail");
        assert_eq!(output["mail"][0]["body"], "which file?");
        assert!(deliverer.calls.lock().unwrap().is_empty());
        assert_eq!(
            driver.tree.lock().unwrap().pending_mail("c1"),
            [] as [crate::tree::MailRecord; 0]
        );
    }

    #[tokio::test]
    async fn watched_urgent_mail_beats_a_simultaneous_steer_flag() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.grants_mut_for_test("root").max_depth = 3;
        let driver = driver_on(tree, Arc::clone(&store));
        spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
            .spawn_with(
                "hub-spawn".into(),
                &serde_json::to_value(SpawnAgentRequest::test_orchestrate()).unwrap(),
            )
            .await
            .unwrap();
        {
            let mut tree = driver.tree.lock().unwrap();
            tree.spawn_agent("c1", &spawn_request(), None).unwrap();
        }
        let mut waiter = wait_tool(driver.clone());
        waiter.caller_id = Arc::new(Mutex::new("c1".into()));
        let arguments = json!({"agent_ids": ["c2"], "timeout_ms": 300_000});
        let (sender, receiver) = tokio::sync::watch::channel(plakous::tools::ToolPreempt::None);
        let mut pending =
            Box::pin(waiter.wait_operation_preempt("hub-wait", &arguments, Some(receiver)));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        let deliverer = Arc::new(StubDeliverer::ok());
        let mut send = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        send.caller_id = Arc::new(Mutex::new("c2".into()));
        let arguments = json!({"agent_id": "c1", "message": "which file?", "purpose": "question", "priority": "urgent"});
        sender.send(plakous::tools::ToolPreempt::Steer).unwrap();
        let (receipt, outcome) =
            tokio::join!(send.send_with("hub-question".into(), &arguments), pending);
        let receipt = receipt.unwrap();
        assert_eq!(receipt.kind(), ToolOutcomeKind::Success);
        let output = outcome_json(&outcome.unwrap());
        assert_eq!(output["woke_by"], "mail");
        assert_eq!(output["mail"][0]["body"], "which file?");
        assert!(deliverer.calls.lock().unwrap().is_empty());
        assert_eq!(
            driver.tree.lock().unwrap().pending_mail("c1"),
            [] as [crate::tree::MailRecord; 0]
        );
    }

    #[tokio::test]
    async fn cancelling_a_parked_hub_wait_releases_mail_to_the_deliverer() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.grants_mut_for_test("root").max_depth = 3;
        let driver = driver_on(tree, Arc::clone(&store));
        spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
            .spawn_with(
                "hub-spawn".into(),
                &serde_json::to_value(SpawnAgentRequest::test_orchestrate()).unwrap(),
            )
            .await
            .unwrap();
        driver
            .tree
            .lock()
            .unwrap()
            .spawn_agent("c1", &spawn_request(), None)
            .unwrap();
        let mut waiter = wait_tool(driver.clone());
        waiter.caller_id = Arc::new(Mutex::new("c1".into()));
        let wait_arguments = json!({"agent_ids": ["c2"]});
        let mut pending_wait =
            Box::pin(waiter.wait_operation_with("cancelled-hub-wait", &wait_arguments));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(pending_wait.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        let deliverer = Arc::new(StubDeliverer::ok());
        let mut send = send_tool(driver.clone(), Some(deliverer.clone()));
        send.caller_id = Arc::new(Mutex::new("c2".into()));
        let send_arguments = json!({"agent_id": "c1", "message": "which file?", "purpose": "question", "priority": "urgent"});
        let mut pending_send = Box::pin(send.send_with("held-question".into(), &send_arguments));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(pending_send.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        assert!(deliverer.calls.lock().unwrap().is_empty());
        drop(pending_wait);
        let receipt = tokio::time::timeout(std::time::Duration::from_secs(1), pending_send)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.kind(), ToolOutcomeKind::Success);
        assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
        assert_eq!(
            driver.tree.lock().unwrap().pending_mail("c1"),
            [] as [crate::tree::MailRecord; 0]
        );
    }

    #[tokio::test]
    async fn a_failed_wait_receipt_publish_leaves_mail_unread() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("refused-wait-receipt", root.path());
        let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let request = SendAgentMessageRequest {
            agent_id: "root".into(),
            message: "question".into(),
            purpose: Some(MessagePurpose::Question),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        };
        let receipt = tree
            .send_agent_message(&child.agent_id, &request, None)
            .unwrap();
        let driver = driver_on(tree, Arc::new(FailingStore));
        let output = wait_tool(driver.clone())
            .wait_with(&json!({}))
            .await
            .unwrap();
        assert_eq!(output.kind(), ToolOutcomeKind::Failure);
        driver
            .read(|tree| {
                assert_eq!(
                    tree.mail(&receipt.message_id).unwrap().durable,
                    crate::tree::MailDurableState::Accepted
                );
                assert!(
                    tree.poll_wait("root", &AwaitAgents::Pending)
                        .unwrap()
                        .is_some()
                );
            })
            .unwrap();
    }

    #[tokio::test]
    async fn wait_returns_full_mail_and_blocks_again_until_new_mail() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        spawn_tool(driver.clone(), Some(Arc::new(StubBinder::ok())))
            .spawn_with(
                "mail-wait-spawn".into(),
                &serde_json::to_value(spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        let body = "€".repeat(682) + "ab";
        let send = |message: &str| SendAgentMessageRequest {
            agent_id: "root".into(),
            message: message.into(),
            purpose: Some(MessagePurpose::Question),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        };
        let first = driver
            .tree
            .lock()
            .unwrap()
            .send_agent_message("c1", &send(&body), None)
            .unwrap();
        let waiter = wait_tool(driver.clone());
        let arguments = json!({"timeout_ms": 1_800_000});
        let output = outcome_json(&waiter.wait_with(&arguments).await.unwrap());
        assert_eq!(output["woke_by"], "mail");
        assert_eq!(output["mail"][0]["message_id"], first.message_id);
        assert_eq!(output["mail"][0]["body"], body);
        assert_eq!(output["mail"][0]["purpose"], "question");
        assert!(output["agents"][0].get("usage").is_none());
        assert_eq!(
            reload(&store, "tree1")
                .mail(&first.message_id)
                .unwrap()
                .durable,
            crate::tree::MailDurableState::Acknowledged
        );
        for agent_ids in [Value::Null, json!(["c1"])] {
            let arguments = json!({"agent_ids": agent_ids, "timeout_ms": 1_800_000});
            let mut pending = Box::pin(waiter.wait_with(&arguments));
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            let next = driver
                .tree
                .lock()
                .unwrap()
                .send_agent_message("c1", &send("new question"), None)
                .unwrap();
            let output = outcome_json(&pending.await.unwrap());
            assert_eq!(output["mail"].as_array().unwrap().len(), 1);
            assert_eq!(output["mail"][0]["message_id"], next.message_id);
            assert_eq!(output["timed_out"], false);
        }
    }

    #[tokio::test]
    async fn send_deferred_busy_child_queues_follow_up() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn-bind".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        let deliverer = Arc::new(StubDeliverer::ok());
        let tool = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        );
        tool.send_with("op-mail-1".into(), &send_request("c1", "hello", None))
            .await
            .unwrap();
        let queued = tool
            .send_with(
                "op-mail-2".into(),
                &send_request("c1", "later", Some(MessagePriority::Deferred)),
            )
            .await
            .unwrap();
        assert_eq!(queued.kind(), ToolOutcomeKind::Success);
        assert_eq!(outcome_json(&queued)["disposition"], "queued");
        let calls = deliverer.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].disposition, MailDisposition::Queued);
        assert_eq!(calls[1].body, "later");
    }

    #[tokio::test]
    async fn hub_working_copy_is_shared_across_compiles_and_persists() {
        let root = tempfile::tempdir().unwrap();
        let tree_id = format!("hub-{}", uuid::Uuid::new_v4().simple());
        let mut tree = bootstrap_tree(&tree_id, root.path());
        let store: Arc<dyn TreeStore> = Arc::new(MemoryTreeStore::new());
        tree.acknowledge_revision(store.save(&tree).unwrap());
        let runtime =
            crate::hub::insert_root_with(tree_id.clone(), tree, Arc::clone(&store), None).unwrap();
        let parent_tree = Arc::clone(&runtime.tree);
        let child_lookup = crate::hub::lookup(&tree_id).unwrap();
        assert!(Arc::ptr_eq(&parent_tree, &child_lookup.tree));
        let binder = Arc::new(StubBinder::ok());
        let outcome = spawn_tool(
            runtime.driver().clone(),
            Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-hub-spawn".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let names = granted_names(Arc::clone(&child_lookup.tree), "c1");
        assert!(names.iter().any(|name| name == "submit_result"));
        assert!(!names.iter().any(|name| name == "spawn_agent"));
        let loaded = reload(&store, &tree_id);
        assert_eq!(
            loaded.node("c1").unwrap().session_locator.as_deref(),
            Some(
                "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001"
            )
        );
        crate::hub::force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn interrupt_cancels_live_child_and_replays() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        tree.set_active_turn("c1", true).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let canceller = Arc::new(StubCanceller::ok());
        let tool = interrupt_tool(
            driver.clone(),
            Some(Arc::clone(&canceller) as Arc<dyn LiveTurnCanceller>),
        );
        let first = tool
            .interrupt_with("op-interrupt".into(), &agent_id_request("c1"))
            .await
            .unwrap();
        assert_eq!(first.kind(), ToolOutcomeKind::Success);
        assert_eq!(outcome_json(&first)["status"], "interrupted");
        {
            let calls = canceller.calls.lock().unwrap();
            assert_eq!(calls.as_slice(), &[vec!["c1".to_owned()]]);
        }
        {
            let tree = tree.lock().unwrap();
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
            // This stub has no terminal observer or release worker.
            assert!(tree.node("c1").unwrap().active_turn);
        }
        let replay = tool
            .interrupt_with("op-interrupt".into(), &agent_id_request("c1"))
            .await
            .unwrap();
        assert_eq!(replay.kind(), ToolOutcomeKind::Success);
        assert_eq!(outcome_json(&replay)["status"], "interrupted");
        assert_eq!(canceller.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn interrupt_without_canceller_persists_only() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        tree.set_active_turn("c1", true).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let outcome = interrupt_tool(driver.clone(), None)
            .interrupt_with("op-interrupt-store".into(), &agent_id_request("c1"))
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let tree = tree.lock().unwrap();
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
        // This stub has no terminal observer or release worker.
        assert!(tree.node("c1").unwrap().active_turn);
    }

    #[tokio::test]
    async fn interrupt_cancel_failure_is_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        tree.set_active_turn("c1", true).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let canceller = Arc::new(StubCanceller::fail());
        let outcome = interrupt_tool(
            driver.clone(),
            Some(Arc::clone(&canceller) as Arc<dyn LiveTurnCanceller>),
        )
        .interrupt_with("op-interrupt-fail".into(), &agent_id_request("c1"))
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(
            tree.lock().unwrap().node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
        let loaded = reload(&store, "tree1");
        assert_eq!(
            loaded.node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }

    /// A store that refuses every publish, for the paths that must still
    /// observe and report when persistence is gone.
    #[tokio::test]
    async fn cancellation_prepare_publish_refusal_changes_no_node_and_dispatches_nothing() {
        for close in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut tree = bootstrap_tree("refused-cancel-prepare", root.path());
            tree.spawn_agent("root", &spawn_request(), None).unwrap();
            tree.set_active_turn("c1", true).unwrap();
            let driver = driver_on(tree, Arc::new(FailingStore));
            let canceller = Arc::new(StubCanceller::ok());
            let tool = if close {
                close_tool(driver.clone(), Some(canceller.clone()))
            } else {
                interrupt_tool(driver.clone(), Some(canceller.clone()))
            };
            let outcome = if close {
                tool.close_with("refused-prepare".into(), &agent_id_request("c1"))
                    .await
            } else {
                tool.interrupt_with("refused-prepare".into(), &agent_id_request("c1"))
                    .await
            }
            .unwrap();
            assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
            assert!(canceller.calls.lock().unwrap().is_empty());
            driver
                .read(|tree| {
                    assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
                    assert!(tree.node("c1").unwrap().active_turn);
                    assert!(
                        tree.operations()["refused-prepare"]
                            .terminal_response
                            .is_none()
                    );
                })
                .unwrap();
        }
    }

    struct SecondSaveStore {
        inner: MemoryTreeStore,
        saves: AtomicUsize,
        fail_at: AtomicUsize,
        stale: bool,
    }

    #[test]
    fn deleted_fold_stays_deleted_without_a_live_refold_callback() {
        for refused in [false, true] {
            for next_entry in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let store = Arc::new(SecondSaveStore {
                    inner: MemoryTreeStore::new(),
                    saves: AtomicUsize::new(0),
                    fail_at: AtomicUsize::new(if refused { 2 } else { 0 }),
                    stale: false,
                });
                let mut tree = bootstrap_tree("refold-ack", tmp.path());
                let child = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .unwrap();
                tree.record_child_session(&child.agent_id,
                    "00000000-0000-4000-8000-000000000100/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001").unwrap();
                tree.acknowledge_revision(store.save(&tree).unwrap());
                let driver = driver_on(tree, store.clone());
                let calls = Arc::new(AtomicUsize::new(0));
                let observed = calls.clone();
                let shared = driver.tree.clone();
                driver.set_retirement_recovery(Arc::new(move || {
                    assert!(
                        shared.try_lock().is_ok(),
                        "callback must run outside the tree mutex"
                    );
                    observed.fetch_add(1, Ordering::SeqCst);
                }));
                let (result, effects) = driver
                    .mutate(|tree| {
                        driver.fold_and_commit(tree, |tree| {
                            tree.clear_feed_views();
                            Ok(())
                        })
                    })
                    .unwrap();
                assert_eq!(result.is_err(), refused);
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                // Crash after the clearing save and before any process-local callback.
                let loaded = store.load("refold-ack").unwrap().unwrap();
                assert_eq!(
                    loaded
                        .rows()
                        .node(&child.agent_id)
                        .unwrap()
                        .fold_order
                        .is_none(),
                    !refused
                );
                if !refused {
                    let reopened = ProductAgentTree::reopen(loaded);
                    assert!(
                        reopened
                            .session_recovery_targets()
                            .contains(&child.agent_id)
                    );
                }
                if next_entry {
                    driver.read(|_| ()).unwrap();
                }
                driver.settle(effects).unwrap();
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                driver.read(|_| ()).unwrap();
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            }
        }
    }

    #[tokio::test]
    async fn tool_consequence_second_save_refusal_restores_views_and_dispatches_nothing() {
        use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
        for stale in [false, true] {
            for kind in [
                TopologyKind::Close,
                TopologyKind::Interrupt,
                TopologyKind::Submit,
                TopologyKind::Discard,
            ] {
                let root = tempfile::tempdir().unwrap();
                let vcs = Arc::new(FakeVcs::new());
                let store = Arc::new(SecondSaveStore {
                    inner: MemoryTreeStore::new(),
                    saves: AtomicUsize::new(0),
                    fail_at: AtomicUsize::new(0),
                    stale,
                });
                let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
                    vcs.clone(),
                    "tool-second-save",
                    "root",
                    "owner",
                    root.path().to_owned(),
                    "selected-tuple",
                    WriterMode::SingleLease,
                )
                .unwrap();
                let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
                binding.max_tier = Tier::Mutate;
                tree.pin_template("implement", binding);
                let child = tree
                    .spawn_agent("root", &mint_spawn_request(), None)
                    .unwrap()
                    .agent_id;
                if kind == TopologyKind::Interrupt {
                    tree.set_active_turn(&child, true).unwrap();
                }
                if kind == TopologyKind::Discard {
                    tree.close_agent(
                        "root",
                        &AttemptScopedRequest {
                            agent_id: child.clone(),
                            expected_attempt_id: None,
                        },
                        None,
                    )
                    .unwrap();
                    tree.settle_for_test();
                }
                let before_node = tree.node(&child).unwrap().clone();
                let before_attempt = tree.attempt(&child).unwrap().clone();
                let before_workspaces = tree.workspaces().clone();
                let workspace = tree.node(&child).unwrap().workspace_id.clone();
                let driver = driver_on(tree, store.clone());
                driver.save(&mut driver.lock().unwrap()).unwrap();
                store
                    .fail_at
                    .store(store.saves.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
                let mut tool = close_tool(driver.clone(), None);
                tool.kind = kind;
                let outcome = match kind {
                    TopologyKind::Close => tool
                        .close_with("refused-consequence".into(), &agent_id_request(&child))
                        .await
                        .unwrap(),
                    TopologyKind::Interrupt => tool
                        .interrupt_with("refused-consequence".into(), &agent_id_request(&child))
                        .await
                        .unwrap(),
                    TopologyKind::Submit => {
                        *tool.caller_id.lock().unwrap() = child.clone();
                        tool.run_named(
                            "refused-consequence",
                            &json!({"turn_token": 1, "output_json": "{\"ok\":true}"}),
                        )
                        .unwrap()
                    }
                    TopologyKind::Discard => tool
                        .run_named("refused-consequence", &workspace_request(&workspace))
                        .unwrap(),
                    _ => unreachable!(),
                };
                assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
                assert_eq!(
                    store.saves.load(Ordering::SeqCst),
                    store.fail_at.load(Ordering::SeqCst) + 1,
                    "the consequence save must fail before the failure receipt is published",
                );
                driver
                    .read(|tree| {
                        assert_eq!(tree.node(&child).unwrap(), &before_node);
                        assert_eq!(tree.attempt(&child).unwrap(), &before_attempt);
                        assert_eq!(tree.workspaces(), &before_workspaces);
                        assert!(
                            tree.operations()["refused-consequence"]
                                .terminal_code
                                .is_some()
                        );
                        assert!(
                            !tree.operations().values().any(|op| op.tool_name == "close"
                                || op.tool_name == "interrupt"
                                || op.tool_name == "submitted")
                                || kind == TopologyKind::Discard
                        );
                    })
                    .unwrap();
                assert!(driver.lock().unwrap().take_effects().is_empty());
                let seals_before = usize::from(kind == TopologyKind::Discard);
                assert_eq!(vcs.calls_of(FakeOp::Seal).len(), seals_before);
                assert!(vcs.calls_of(FakeOp::Unmint).is_empty());
            }
        }
    }

    impl TreeStore for SecondSaveStore {
        fn exists(&self, id: &str) -> Result<bool, crate::tree::TreeStoreError> {
            self.inner.exists(id)
        }

        fn ids(&self) -> Result<Vec<String>, crate::tree::TreeStoreError> {
            self.inner.ids()
        }

        fn load(
            &self,
            id: &str,
        ) -> Result<Option<crate::tree::PersistedTree>, crate::tree::TreeStoreError> {
            self.inner.load(id)
        }

        fn save(
            &self,
            tree: &ProductAgentTree,
        ) -> Result<crate::tree::Revision, crate::tree::TreeStoreError> {
            let save = self.saves.fetch_add(1, Ordering::SeqCst) + 1;
            if save == self.fail_at.load(Ordering::SeqCst) {
                return Err(if self.stale {
                    crate::tree::TreeStoreError::stale(tree.stored_revision + 1, tree.revision)
                } else {
                    crate::tree::TreeStoreError::unavailable("second save refused")
                });
            }
            self.inner.save(tree)
        }
    }

    #[test]
    fn recorded_intent_second_save_refusal_reverts_and_reserves_once_on_retry() {
        use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
        for stale in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let vcs = Arc::new(FakeVcs::new());
            let store = Arc::new(SecondSaveStore {
                inner: MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(0),
                stale,
            });
            let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
                vcs.clone(),
                "second-save",
                "root",
                "owner",
                root.path().to_owned(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
            binding.max_tier = Tier::Mutate;
            tree.pin_template("implement", binding);
            let child = tree
                .spawn_agent("root", &mint_spawn_request(), None)
                .unwrap()
                .agent_id;
            let driver = driver_on(tree, store.clone());
            let mut tree = driver.lock().unwrap();
            driver.save(&mut tree).unwrap();
            tree.record_host_cancellation(&child).unwrap();
            let before_node = tree.node(&child).unwrap().clone();
            let before_attempt = tree.attempt(&child).unwrap().clone();
            store
                .fail_at
                .store(store.saves.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
            assert!(driver.apply_recorded(&mut tree, &child).is_err());
            assert_eq!(tree.node(&child).unwrap(), &before_node);
            assert_eq!(tree.attempt(&child).unwrap(), &before_attempt);
            assert_eq!(tree.revision, tree.stored_revision);
            assert!(tree.take_effects().is_empty());
            assert!(vcs.calls_of(FakeOp::Seal).is_empty());
            driver.apply_recorded(&mut tree, &child).unwrap();
            let effects = tree.take_effects();
            assert!(!effects.is_empty());
            drop(tree);
            driver.settle(effects).unwrap();
            assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
            let mut tree = driver.lock().unwrap();
            driver.apply_recorded(&mut tree, &child).unwrap();
            assert!(tree.take_effects().is_empty());
        }
    }

    #[test]
    fn runtime_second_save_refusal_reverts_and_dispatches_only_after_retry() {
        use crate::hub::ProductTopologyRuntime;
        use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
        for stale in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let vcs = Arc::new(FakeVcs::new());
            let store = Arc::new(SecondSaveStore {
                inner: MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(2),
                stale,
            });
            let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
                vcs.clone(),
                "runtime-second-save",
                "root",
                "owner",
                root.path().to_owned(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
            binding.max_tier = Tier::Mutate;
            tree.pin_template("implement", binding);
            let child = tree
                .spawn_agent("root", &mint_spawn_request(), None)
                .unwrap()
                .agent_id;
            tree.reserve_turn_owner(&child);
            tree.release_turn_owner(&child);
            let before_node = tree.node(&child).unwrap().clone();
            let before_attempt = tree.attempt(&child).unwrap().clone();
            let runtime = ProductTopologyRuntime::new(tree, store, vcs.clone(), None);
            let close = |tree: &mut ProductAgentTree| {
                tree.close_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: child.clone(),
                        expected_attempt_id: None,
                    },
                    None,
                )
                .map_err(|error| error.message)
            };
            assert!(runtime.mutate_then_emit(false, close).is_err());
            runtime
                .read(|tree| {
                    assert_eq!(tree.node(&child).unwrap(), &before_node);
                    assert_eq!(tree.attempt(&child).unwrap(), &before_attempt);
                    assert!(
                        !tree.has_turn_owner(&child),
                        "rollback must not resurrect released ownership"
                    );
                    assert_eq!(tree.revision, tree.stored_revision);
                })
                .unwrap();
            assert!(vcs.calls_of(FakeOp::Seal).is_empty());
            runtime.mutate_then_emit(false, close).unwrap();
            assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
            runtime.mutate_then_emit(false, |_| Ok(())).unwrap();
            assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
        }
    }

    #[test]
    fn fail_closed_second_save_refusal_dispatches_nothing_until_retry() {
        use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
        for stale in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let vcs = Arc::new(FakeVcs::new());
            let store = Arc::new(SecondSaveStore {
                inner: MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(2),
                stale,
            });
            let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
                vcs.clone(),
                "fail-closed-second-save",
                "root",
                "owner",
                root.path().to_owned(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
            binding.max_tier = Tier::Mutate;
            tree.pin_template("implement", binding);
            let child = tree
                .spawn_agent("root", &mint_spawn_request(), None)
                .unwrap()
                .agent_id;
            let before_node = tree.node(&child).unwrap().clone();
            let before_attempt = tree.attempt(&child).unwrap().clone();
            let driver = driver_on(tree, store);
            let tool = task_tool(driver.clone(), TopologyKind::Spawn);
            tool.fail_closed_session(&child, "materialization failed".into())
                .unwrap();
            driver
                .read(|tree| {
                    assert_eq!(tree.node(&child).unwrap(), &before_node);
                    assert_eq!(tree.attempt(&child).unwrap(), &before_attempt);
                    assert_eq!(tree.revision, tree.stored_revision);
                })
                .unwrap();
            assert!(vcs.calls_of(FakeOp::Seal).is_empty());
            tool.fail_closed_session(&child, "materialization failed".into())
                .unwrap();
            assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
        }
    }

    #[test]
    fn fold_commit_refuses_dirty_input_and_reverts_work_refusal() {
        let root = tempfile::tempdir().unwrap();
        let driver = driver_on(
            bootstrap_tree("fold-commit-refusals", root.path()),
            Arc::new(MemoryTreeStore::new()),
        );
        let mut tree = driver.lock().unwrap();
        assert!(
            driver
                .fold_and_commit::<()>(&mut tree, |_| panic!("dirty input must not run work"))
                .is_err()
        );
        driver.save(&mut tree).unwrap();
        let before = tree.revision;
        let result: Result<(), TopologyError> = driver.fold_and_commit(&mut tree, |tree| {
            tree.record_fault("root", "uncommitted diagnostic");
            Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "work refused",
            ))
        });
        assert_eq!(result.unwrap_err().code, TopologyErrorCode::GrantDenied);
        assert_eq!(tree.revision, before);
        assert_eq!(tree.fault_reason("root"), None);
    }

    #[test]
    fn recorded_runtime_intent_survives_consequence_refusal_without_resurrecting_owner() {
        use crate::hub::ProductTopologyRuntime;
        use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
        for stale in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let vcs = Arc::new(FakeVcs::new());
            let store = Arc::new(SecondSaveStore {
                inner: MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(2),
                stale,
            });
            let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
                vcs.clone(),
                "recorded-runtime-refusal",
                "root",
                "owner",
                root.path().to_owned(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
            binding.max_tier = Tier::Mutate;
            tree.pin_template("implement", binding);
            let child = tree
                .spawn_agent("root", &mint_spawn_request(), None)
                .unwrap()
                .agent_id;
            tree.reserve_turn_owner(&child);
            let before_node = tree.node(&child).unwrap().clone();
            let before_attempt = tree.attempt(&child).unwrap().clone();
            let runtime = ProductTopologyRuntime::new(tree, store.clone(), vcs.clone(), None);
            assert!(
                runtime
                    .record_then_apply(&child, |tree| {
                        tree.release_turn_owner(&child);
                        tree.record_host_cancellation(&child)
                    })
                    .is_err()
            );
            runtime
                .read(|tree| {
                    assert_eq!(tree.node(&child).unwrap(), &before_node);
                    assert_eq!(tree.attempt(&child).unwrap(), &before_attempt);
                    assert!(!tree.has_turn_owner(&child));
                    assert_eq!(tree.revision, tree.stored_revision);
                    assert_eq!(
                        tree.operations(),
                        store
                            .load(&tree.tree_id)
                            .unwrap()
                            .unwrap()
                            .rows()
                            .operations()
                    );
                })
                .unwrap();
            assert!(vcs.calls_of(FakeOp::Seal).is_empty());
            runtime
                .record_then_apply(&child, |tree| tree.record_host_cancellation(&child))
                .unwrap();
            assert_eq!(vcs.calls_of(FakeOp::Seal).len(), 1);
        }
    }

    #[test]
    fn refused_effect_result_save_keeps_follow_on_removal_queued_until_acknowledged() {
        use crate::hub::ProductTopologyRuntime;
        use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
        for stale in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let vcs = Arc::new(FakeVcs::new());
            let store = Arc::new(SecondSaveStore {
                inner: MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(2),
                stale,
            });
            let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
                vcs.clone(),
                "follow-on-save-refusal",
                "root",
                "owner",
                root.path().to_owned(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
            binding.max_tier = Tier::Mutate;
            binding.on_mint = Some("fixture-hook".into());
            tree.pin_template("implement", binding);
            tree.spawn_agent("root", &mint_spawn_request(), None)
                .unwrap();
            let runtime = ProductTopologyRuntime::new(tree, store, vcs.clone(), None);
            let driver = runtime.driver();
            vcs.hook_next_with(Err(VcsError::hook_failed("fixture hook failed")));
            let effects = {
                let mut tree = driver.lock().unwrap();
                driver.save(&mut tree).unwrap();
                tree.take_effects()
            };
            assert!(driver.settle(effects).is_err());
            assert_eq!(vcs.calls_of(FakeOp::Hook).len(), 1);
            assert!(vcs.calls_of(FakeOp::Unmint).is_empty());
            runtime.mutate_then_emit(false, |_| Ok(())).unwrap();
            assert_eq!(vcs.calls_of(FakeOp::Hook).len(), 1);
            assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
        }
    }

    struct FailingStore;

    #[test]
    fn followup_withdrawal_cas_refusal_restores_the_complete_opening() {
        for stale in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut tree = bootstrap_tree("tree1", root.path());
            let spawned = tree.spawn_agent("root", &spawn_request(), None).unwrap();
            tree.record_child_session(&spawned.agent_id,
                "00000000-0000-4000-8000-000000000100/00000000-0000-4000-8000-000000000200/00000000-0000-4000-8000-000000000300").unwrap();
            tree.record_host_cancellation(&spawned.agent_id).unwrap();
            tree.fold_due(&spawned.agent_id).unwrap();
            let opener = tree
                .send_agent_message(
                    "root",
                    &SendAgentMessageRequest {
                        agent_id: spawned.agent_id.clone(),
                        message: "again".to_owned(),
                        purpose: Some(MessagePurpose::Followup),
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    Some("withdraw-send"),
                )
                .unwrap()
                .message_id;
            let store = Arc::new(SecondSaveStore {
                inner: MemoryTreeStore::new(),
                saves: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(2),
                stale,
            });
            tree.acknowledge_revision(store.save(&tree).unwrap());
            let before_mail = tree.mail(&opener).unwrap().clone();
            let before_nodes = tree.nodes().clone();
            let before_attempts = tree.attempts().clone();
            let before_operations = tree.operations().clone();
            let revision = tree.revision;
            let driver = driver_on(tree, store.clone());
            assert!(
                driver
                    .withdraw_unstarted_followup(
                        &spawned.agent_id,
                        &opener,
                        "00000000-0000-4000-8000-000000000100",
                        0,
                        Some((
                            "withdraw-send".to_owned(),
                            TopologyErrorCode::RecipientAttemptTerminal
                        )),
                    )
                    .is_err()
            );
            driver
                .read(|tree| {
                    assert_eq!(tree.mail(&opener), Some(&before_mail));
                    assert_eq!(tree.nodes(), &before_nodes);
                    assert_eq!(tree.attempts(), &before_attempts);
                    assert_eq!(tree.operations(), &before_operations);
                    assert_eq!(tree.revision, revision);
                })
                .unwrap();
            driver
                .withdraw_unstarted_followup(
                    &spawned.agent_id,
                    &opener,
                    "00000000-0000-4000-8000-000000000100",
                    0,
                    Some((
                        "withdraw-send".to_owned(),
                        TopologyErrorCode::RecipientAttemptTerminal,
                    )),
                )
                .unwrap();
            let saved = store.load("tree1").unwrap().unwrap();
            assert_eq!(saved.rows().attempts().len(), before_attempts.len() - 1);
            assert_eq!(
                saved.rows().mail(&opener).unwrap().durable,
                crate::tree::MailDurableState::Acknowledged
            );
            assert_eq!(
                saved.rows().operations()["withdraw-send"].terminal_code,
                Some(TopologyErrorCode::RecipientAttemptTerminal)
            );
            assert_eq!(store.saves.load(Ordering::SeqCst), 3);
        }
    }

    impl TreeStore for FailingStore {
        fn exists(&self, _tree_id: &str) -> Result<bool, crate::tree::TreeStoreError> {
            Ok(false)
        }

        fn ids(&self) -> Result<Vec<String>, crate::tree::TreeStoreError> {
            Ok(Vec::new())
        }

        fn save(
            &self,
            _tree: &ProductAgentTree,
        ) -> Result<crate::tree::Revision, crate::tree::TreeStoreError> {
            Err(crate::tree::TreeStoreError::unavailable(
                "the test store refuses every save",
            ))
        }

        fn load(
            &self,
            _tree_id: &str,
        ) -> Result<Option<crate::tree::PersistedTree>, crate::tree::TreeStoreError> {
            Ok(None)
        }
    }

    /// A refused publish does not stop the drain: the seal the port ran is
    /// recorded — the node leaves `Sealing`, the candidate is in the tree —
    /// and the save error is reported after, so no effect is lost to a
    /// stale or absent store.
    #[tokio::test]
    async fn settle_records_every_effect_before_reporting_a_refused_publish() {
        use crate::protocol::NodeStatus;
        use plakous_workspace::occupancy_vcs::fake::FakeVcs;

        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            Arc::clone(&vcs) as Arc<dyn VcsOps>,
            "tree-settle",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        tree.pin_template("implement", binding);
        let store = Arc::new(SecondSaveStore {
            inner: MemoryTreeStore::new(),
            saves: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(0),
            stale: false,
        });
        let driver = driver_on(tree, store.clone());
        let child = driver
            .mutate(|tree| {
                tree.spawn_agent("root", &mint_spawn_request(), None)
                    .unwrap()
                    .agent_id
            })
            .unwrap()
            .0;
        let (closed, effects) = driver
            .mutate(|tree| {
                tree.close_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: child.clone(),
                        expected_attempt_id: None,
                    },
                    None,
                )
            })
            .unwrap();
        closed.unwrap();
        assert!(!effects.is_empty(), "closing a mutate copy seals it");
        driver.save(&mut driver.lock().unwrap()).unwrap();
        store
            .fail_at
            .store(store.saves.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
        let error = driver.settle(effects).unwrap_err();
        assert!(
            error.message.contains("second save refused"),
            "{}",
            error.message
        );
        let mut tree = driver.tree.lock().unwrap();
        assert_ne!(tree.node(&child).unwrap().status, NodeStatus::Sealing);
        assert!(
            tree.attempt(&child).unwrap().candidate_id.is_some(),
            "the seal that ran is recorded"
        );
        assert!(
            tree.take_effects().is_empty(),
            "the drain ran to completion"
        );
    }

    #[tokio::test]
    async fn settle_reports_a_seal_failure_after_recording_and_publishing_it() {
        use crate::protocol::NodeStatus;
        use plakous_workspace::occupancy_vcs::fake::FakeVcs;

        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            vcs.clone(),
            "tree-seal-failure",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        tree.pin_template("implement", binding);
        let store = Arc::new(MemoryTreeStore::default());
        let driver = driver_on(tree, store.clone());
        let child = driver
            .mutate_and_settle(|tree| {
                tree.spawn_agent("root", &mint_spawn_request(), None)
                    .unwrap()
                    .agent_id
            })
            .unwrap()
            .0;
        vcs.seal_next_with(Err(VcsError::seal_failed("injected seal failure")));
        let ((), effects) = driver
            .mutate(|tree| {
                tree.record_turn_completed(&child).unwrap();
                driver.save(tree).unwrap();
            })
            .unwrap();
        let settled = driver.settle(effects).unwrap();
        let error = settled
            .seal_failures
            .first()
            .expect("the seal failure is surfaced");
        assert_eq!(error.code, TopologyErrorCode::MintUnavailable);
        assert!(error.message.contains("injected seal failure"));
        driver
            .read(|tree| {
                assert_eq!(
                    tree.node(&child).unwrap().status,
                    NodeStatus::RecoveryRequired
                );
                assert_eq!(tree.revision, tree.stored_revision);
                let reopened =
                    ProductAgentTree::reopen(store.load(&tree.tree_id).unwrap().unwrap());
                assert_eq!(
                    reopened
                        .attempt(&child)
                        .unwrap()
                        .failure
                        .as_ref()
                        .unwrap()
                        .kind,
                    "seal_failed"
                );
            })
            .unwrap();
    }

    #[tokio::test]
    async fn close_cancels_closed_ids_in_domain_order() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let canceller = Arc::new(StubCanceller::ok());
        let outcome = close_tool(
            driver.clone(),
            Some(Arc::clone(&canceller) as Arc<dyn LiveTurnCanceller>),
        )
        .close_with("op-close".into(), &agent_id_request("c1"))
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        assert_eq!(outcome_json(&outcome)["agents"][0]["agent_id"], "c1");
        {
            let calls = canceller.calls.lock().unwrap();
            assert_eq!(calls.as_slice(), &[vec!["c1".to_owned()]]);
        }
        assert_eq!(
            tree.lock().unwrap().node("c1").unwrap().status,
            NodeStatus::Closed
        );
        let replay = close_tool(
            driver.clone(),
            Some(Arc::clone(&canceller) as Arc<dyn LiveTurnCanceller>),
        )
        .close_with("op-close".into(), &agent_id_request("c1"))
        .await
        .unwrap();
        assert_eq!(replay.kind(), ToolOutcomeKind::Success);
        assert_eq!(canceller.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn close_cancel_failure_keeps_close_intent_and_recovery_evidence() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        tree.set_active_turn("c1", true).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let outcome = close_tool(
            driver.clone(),
            Some(Arc::new(StubCanceller::fail()) as Arc<dyn LiveTurnCanceller>),
        )
        .close_with("op-close-fail".into(), &agent_id_request("c1"))
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(
            tree.lock().unwrap().node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
        assert!(matches!(
            tree.lock().unwrap().recovery_need("c1").unwrap(),
            crate::tree::RecoveryNeed::Session { .. }
        ));
    }

    #[tokio::test]
    async fn close_cancel_failure_without_ids_keeps_closed() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let outcome = close_tool(
            driver.clone(),
            Some(Arc::new(StubCanceller::fail_without_ids()) as Arc<dyn LiveTurnCanceller>),
        )
        .close_with("op-close-empty".into(), &agent_id_request("c1"))
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(
            tree.lock().unwrap().node("c1").unwrap().status,
            NodeStatus::Closed
        );
    }

    #[tokio::test]
    async fn sibling_send_persists_grant_denied() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let spawn = spawn_tool(driver.clone(), None);
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        spawn
            .spawn_with("op-sib-1".into(), &arguments)
            .await
            .unwrap();
        spawn
            .spawn_with("op-sib-2".into(), &arguments)
            .await
            .unwrap();
        let send = send_tool(driver.clone(), None);
        *send.caller_id.lock().unwrap() = "c1".into();
        let outcome = send
            .send_with("op-sib-mail".into(), &send_request("c2", "peer", None))
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&outcome)["code"], "grant_denied");
        let stored = tree.lock().unwrap();
        assert_eq!(
            stored
                .operations()
                .get("op-sib-mail")
                .expect("grant_denied must persist a terminal receipt")
                .terminal_code,
            Some(TopologyErrorCode::GrantDenied)
        );
    }

    /// Through the tool: a `mutate` ask on a read-only template
    /// clips, and the wire receipt says so.
    #[tokio::test]
    async fn spawn_tool_clips_tier_to_the_template_ceiling() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let mut request = spawn_request();
        request.tier = Tier::Mutate;
        let outcome = spawn_tool(driver.clone(), None)
            .spawn_with("op-clip".into(), &serde_json::to_value(request).unwrap())
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let json = outcome_json(&outcome);
        assert_eq!(json["tier"], "read_only");
        assert_eq!(json["workspace"]["mode"], "inherit");
        assert_eq!(json["attempt_id"], "c1-a1");
        assert!(json.get("path").is_none() && json.get("description").is_none());
    }

    /// `workspace=fork` on `execute` mints a snapshot copy and persists the
    /// base it was minted from.
    #[tokio::test]
    async fn spawn_tool_execute_tier_mints_a_snapshot_copy() {
        let (_root, store, tree) = git_persist_fixture();
        let driver = driver_on(tree, Arc::clone(&store));
        let mut request = mint_spawn_request();
        request.tier = Tier::Execute;
        request.workspace = SpawnWorkspace::Fork;
        let outcome = spawn_tool(driver.clone(), None)
            .spawn_with("op-exec".into(), &serde_json::to_value(request).unwrap())
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let json = outcome_json(&outcome);
        assert_eq!(json["tier"], "execute");
        assert_eq!(json["workspace"]["mode"], "snapshot");
        assert_eq!(
            json["workspace"]["base"].as_str().map(str::len),
            Some(40),
            "{json}"
        );
        let loaded = reload(&store, "tree1");
        assert_eq!(loaded.nodes().len(), 2);
        let workspace_id = json["workspace"]["id"].as_str().unwrap();
        let row = loaded.workspace(workspace_id).unwrap();
        assert_eq!(row.mode, WorkspaceMode::Snapshot);
        assert_eq!(row.base_commit.as_deref().map(str::len), Some(40));
    }

    #[tokio::test]
    async fn read_only_spawn_tool_binds_caller_copy_and_adopt_is_not_sealed() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let launch = root.path().to_path_buf();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let spawned = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-inherit".into(),
                &serde_json::to_value(spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(spawned.kind(), ToolOutcomeKind::Success);
        let json = outcome_json(&spawned);
        assert_eq!(json["workspace"]["id"], "ws-root");
        assert_eq!(
            std::fs::canonicalize(child_path(&tree, "c1")).unwrap(),
            std::fs::canonicalize(&launch).unwrap()
        );
        assert!(!launch.join(".plak/worktrees").exists());
        let adopt = adopt_tool(driver.clone())
            .adopt_with("op-adopt", &json!({ "candidate_id": "cand-c1-1" }))
            .unwrap();
        assert_eq!(adopt.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&adopt)["code"], "not_sealed");
        let loaded = reload(&store, "tree1");
        assert_eq!(loaded.node("c1").unwrap().workspace_id, "ws-root");
        assert_eq!(
            std::fs::canonicalize(&loaded.node("c1").unwrap().path).unwrap(),
            std::fs::canonicalize(&launch).unwrap()
        );
    }

    #[tokio::test]
    async fn git_mint_tool_forks_from_the_parent_and_reloads() {
        let (root, store, tree) = git_persist_fixture();
        std::fs::write(root.path().join("DIRTY.txt"), "dirty\n").unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let spawned = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-mint".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(spawned.kind(), ToolOutcomeKind::Success);
        let json = outcome_json(&spawned);
        assert_eq!(json["tier"], "mutate");
        assert_eq!(json["workspace"]["mode"], "fork");
        let workspace_id = json["workspace"]["id"].as_str().unwrap().to_owned();
        let child_path = child_path(&tree, "c1");
        assert!(child_path.starts_with(root.path().join(".plak/worktrees")));
        assert!(
            child_path.join("DIRTY.txt").is_file(),
            "parent dirty state is copied"
        );
        assert!(child_path.join("SEED.txt").is_file());
        let porcelain = Command::new("git")
            .args(["status", "--porcelain", "-uall"])
            .current_dir(root.path())
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&porcelain.stdout);
        assert!(
            porcelain.status.success() && !status.contains(".plak"),
            "mint copies must be gitignored, got:\n{status}"
        );
        let loaded = reload(&store, "tree1");
        let workspace = loaded.workspace(&workspace_id).unwrap();
        assert_eq!(workspace.mode, WorkspaceMode::Fork);
        assert_eq!(workspace.status, WorkspaceStatus::Active);
        assert_eq!(loaded.node("c1").unwrap().tier, Tier::Mutate);
        assert_eq!(
            loaded
                .node("c1")
                .unwrap()
                .template
                .as_ref()
                .map(TemplateName::as_str),
            Some("implement")
        );
    }

    #[tokio::test]
    async fn discard_wire_waits_for_cleanup_and_replays_only_its_acknowledged_outcome() {
        use plakous_workspace::occupancy_vcs::{
            VcsError,
            fake::{FakeOp, FakeVcs},
        };
        for fail in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let vcs = Arc::new(FakeVcs::new());
            let tree = ProductAgentTree::bootstrap_root_for_test_with(
                vcs.clone(),
                "cleanup-wire",
                "root",
                "owner",
                root.path().to_path_buf(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let store: Arc<dyn TreeStore> = Arc::new(MemoryTreeStore::new());
            let driver = driver_on(tree, store.clone());
            let spawned = spawn_tool(driver.clone(), None)
                .spawn_with(
                    "cleanup-spawn".into(),
                    &serde_json::to_value(mint_spawn_request()).unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(spawned.kind(), ToolOutcomeKind::Success);
            let id = "c1";
            let workspace_id = outcome_json(&spawned)["workspace"]["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let path = child_path(&driver.tree, id);
            vcs.seal_next_with(Ok(FakeVcs::base_for(id)));
            // Finished, not closed: a closed copy would already be pruned.
            driver
                .mutate_and_settle(|tree| {
                    tree.record_turn_completed(id).unwrap();
                    driver.save(tree).unwrap();
                })
                .unwrap();
            let request = WorkspaceIdRequest {
                workspace_id: workspace_id.clone(),
            };
            let (pending, effects) = driver
                .mutate(|tree| {
                    let pending = tree
                        .discard_workspace("root", &request, Some("cleanup-discard"))
                        .unwrap();
                    driver.save(tree).unwrap();
                    pending
                })
                .unwrap();
            assert_eq!(pending.status, WorkspaceStatus::Discarding);
            let tool = discard_tool(driver.clone());
            let arguments = workspace_request(&workspace_id);
            assert_eq!(
                tool.run_named("cleanup-discard", &arguments)
                    .unwrap()
                    .kind(),
                ToolOutcomeKind::Failure
            );
            assert!(
                driver.tree.lock().unwrap().operations()["cleanup-discard"]
                    .terminal_code
                    .is_none()
            );
            assert!(vcs.calls_of(FakeOp::Unmint).is_empty());
            assert!(path.exists());
            let paused = reload(&store, "cleanup-wire");
            assert_eq!(
                paused.workspace(&workspace_id).unwrap().status,
                WorkspaceStatus::Discarding
            );
            assert_eq!(
                paused.node(id).unwrap().status,
                NodeStatus::RecoveryRequired
            );
            if fail {
                vcs.unmint_next_with(Err(VcsError::mint_unavailable("injected cleanup failure")));
            }
            assert_eq!(driver.settle(effects).is_err(), fail);
            let replay = tool.run_named("cleanup-discard", &arguments).unwrap();
            assert_eq!(
                replay.kind(),
                if fail {
                    ToolOutcomeKind::Failure
                } else {
                    ToolOutcomeKind::Success
                }
            );
            let loaded = reload(&store, "cleanup-wire");
            assert_eq!(
                loaded.workspace(&workspace_id).unwrap().status,
                if fail {
                    WorkspaceStatus::Discarding
                } else {
                    WorkspaceStatus::Discarded
                }
            );
            assert_eq!(path.exists(), fail);
            assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
            if fail {
                assert_eq!(outcome_json(&replay)["code"], "workspace_recovery_required");
            } else {
                assert_eq!(outcome_json(&replay)["status"], "discarded");
            }
        }
    }

    struct CleanupCutStore {
        inner: MemoryTreeStore,
        remaining: Mutex<Option<usize>>,
    }

    impl TreeStore for CleanupCutStore {
        fn exists(&self, id: &str) -> Result<bool, crate::tree::TreeStoreError> {
            self.inner.exists(id)
        }
        fn ids(&self) -> Result<Vec<String>, crate::tree::TreeStoreError> {
            self.inner.ids()
        }
        fn load(
            &self,
            id: &str,
        ) -> Result<Option<crate::tree::PersistedTree>, crate::tree::TreeStoreError> {
            self.inner.load(id)
        }
        fn save(
            &self,
            tree: &ProductAgentTree,
        ) -> Result<crate::tree::Revision, crate::tree::TreeStoreError> {
            let mut remaining = self.remaining.lock().unwrap();
            if let Some(count) = remaining.as_mut() {
                if *count == 0 {
                    return Err(crate::tree::TreeStoreError::unavailable(
                        "cleanup publication cut",
                    ));
                }
                *count -= 1;
            }
            self.inner.save(tree)
        }
    }

    #[tokio::test]
    async fn discard_publication_cuts_never_remove_unprepared_or_replay_unpublished_success() {
        use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};
        for cut in [1, 2] {
            let root = tempfile::tempdir().unwrap();
            let vcs = Arc::new(FakeVcs::new());
            let tree = ProductAgentTree::bootstrap_root_for_test_with(
                vcs.clone(),
                "cleanup-cut",
                "root",
                "owner",
                root.path().to_path_buf(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap();
            let store = Arc::new(CleanupCutStore {
                inner: MemoryTreeStore::new(),
                remaining: Mutex::new(None),
            });
            let driver = driver_on(tree, store.clone());
            let spawned = spawn_tool(driver.clone(), None)
                .spawn_with(
                    "spawn-cut".into(),
                    &serde_json::to_value(mint_spawn_request()).unwrap(),
                )
                .await
                .unwrap();
            let workspace = outcome_json(&spawned)["workspace"]["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let path = child_path(&driver.tree, "c1");
            vcs.seal_next_with(Ok(FakeVcs::base_for("c1")));
            // Finished, not closed: a closed copy would already be pruned.
            driver
                .mutate_and_settle(|tree| {
                    tree.record_turn_completed("c1").unwrap();
                    driver.save(tree).unwrap();
                })
                .unwrap();
            *store.remaining.lock().unwrap() = Some(cut);
            let tool = discard_tool(driver.clone());
            let args = workspace_request(&workspace);
            assert_eq!(
                tool.run_named("discard-cut", &args).unwrap().kind(),
                ToolOutcomeKind::Failure
            );
            assert_eq!(
                tool.run_named("discard-cut", &args).unwrap().kind(),
                ToolOutcomeKind::Failure
            );
            assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), usize::from(cut == 2));
            assert_eq!(path.exists(), cut == 1);
            let durable = store.inner.load("cleanup-cut").unwrap().unwrap();
            let reopened = ProductAgentTree::reopen(durable);
            assert!(
                reopened.operations()["discard-cut"]
                    .terminal_response
                    .is_none()
            );
            *store.remaining.lock().unwrap() = None;
            let replay = tool.run_named("discard-cut", &args).unwrap();
            assert_eq!(
                replay.kind(),
                if cut == 2 {
                    ToolOutcomeKind::Success
                } else {
                    ToolOutcomeKind::Failure
                }
            );
            assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), usize::from(cut == 2));
        }
    }

    #[tokio::test]
    async fn closing_a_child_prunes_its_copy() {
        let (_root, store, tree) = git_persist_fixture();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let spawned = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-mint".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        let json = outcome_json(&spawned);
        let child_path = child_path(&tree, "c1");
        let workspace_id = json["workspace"]["id"].as_str().unwrap().to_owned();
        let closed = close_tool(driver.clone(), None)
            .close_with("op-close".into(), &agent_id_request("c1"))
            .await
            .unwrap();
        assert_eq!(closed.kind(), ToolOutcomeKind::Success);
        // The copy never diverged, so no candidate holds it: closing removes
        // the worktree and its branch without a discard.
        assert!(!child_path.exists());
        let loaded = reload(&store, "tree1");
        assert_eq!(
            loaded.workspace(&workspace_id).unwrap().status,
            WorkspaceStatus::Discarded
        );
    }

    #[tokio::test]
    async fn minted_child_policy_cannot_escape_to_parent_seed() {
        let (root, store, tree) = git_persist_fixture();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let spawned = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-mint".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(spawned.kind(), ToolOutcomeKind::Success);
        let child_path = child_path(&tree, "c1");
        let policy = WorkspacePolicy::host(&child_path, Path::new(".")).unwrap();
        assert!(policy.resolve_existing("SEED.txt").is_ok());
        assert!(policy.resolve_existing("../../SEED.txt").is_err());
        assert!(root.path().join("SEED.txt").is_file());
        assert!(
            child_path.join(".plak/bin/git").is_file(),
            "minted copy must install the git-redirect shim"
        );
    }

    #[tokio::test]
    async fn jj_mint_tool_mints_under_plak_workspaces_when_jj_exists() {
        if !Command::new("jj")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join(".jj")).unwrap();
            std::fs::write(root.path().join("SEED.txt"), "seed\n").unwrap();
            let (store, tree) = persist_fixture(&root);
            let driver = driver_on(tree, Arc::clone(&store));
            let first = spawn_tool(driver.clone(), None)
                .spawn_with(
                    "op-jj-mint".into(),
                    &serde_json::to_value(mint_spawn_request()).unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(first.kind(), ToolOutcomeKind::Failure);
            assert_eq!(outcome_json(&first)["code"], "mint_unavailable");
            let loaded = reload(&store, "tree1");
            assert_eq!(
                loaded.operations().get("op-jj-mint").unwrap().terminal_code,
                Some(TopologyErrorCode::MintUnavailable)
            );
            return;
        }
        let (repo, store, tree) = jj_persist_fixture();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let first = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-jj-mint".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.kind(), ToolOutcomeKind::Success);
        let child_path = child_path(&tree, "c1");
        assert!(
            child_path
                .display()
                .to_string()
                .contains(".plak/workspaces/"),
            "jj mint must land under .plak/workspaces/"
        );
        assert!(child_path.join(".plak/bin/jj").is_file());
        let replay = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-jj-mint".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replay.kind(), ToolOutcomeKind::Success);
        let loaded = reload(&store, "tree1");
        assert_eq!(loaded.nodes().len(), 2);
        assert!(repo.path().join("SEED.txt").is_file());
    }

    #[tokio::test]
    async fn no_vcs_mint_tool_is_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let first = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-novcs-mint".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&first)["code"], "mint_unavailable");
        let loaded = reload(&store, "tree1");
        assert_eq!(
            loaded
                .operations()
                .get("op-novcs-mint")
                .unwrap()
                .terminal_code,
            Some(TopologyErrorCode::MintUnavailable)
        );
    }

    #[tokio::test]
    async fn discard_tool_refuses_running_occupant() {
        let (_root, store, tree) = git_persist_fixture();
        let driver = driver_on(tree, Arc::clone(&store));
        let spawned = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-mint".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        let workspace_id = outcome_json(&spawned)["workspace"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let refused = discard_tool(driver.clone())
            .run_named("op-discard-running", &workspace_request(&workspace_id))
            .unwrap();
        assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&refused)["code"], "discard_not_applicable");
        let loaded = reload(&store, "tree1");
        assert_eq!(
            loaded.workspace(&workspace_id).unwrap().status,
            WorkspaceStatus::Active
        );
    }

    #[tokio::test]
    async fn invoke_granted_template_unknown_is_executor_failure() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.snapshot.roster.clear();
        pin_review(&mut tree, "grok", "grok-4", None);
        let driver = driver_on(tree, Arc::clone(&store));
        let explore = SpawnAgentRequest::test_named("explore", Tier::ReadOnly, "scout", "look");
        let outcome = invoke_granted_topology_tool_on(
            driver.clone(),
            "root".into(),
            "spawn_agent",
            &serde_json::to_value(explore).unwrap(),
            "op-unpin".into(),
            TopologyHostPorts::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&outcome)["code"], "template_unknown");
    }

    #[tokio::test]
    async fn invoke_granted_omits_spawn_on_empty_roster() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.snapshot.roster.clear();
        tree.snapshot.digest = tree.snapshot.compute_digest();
        let err = invoke_granted_topology_tool_on(
            driver_on(tree, store),
            "root".into(),
            "spawn_agent",
            &serde_json::to_value(SpawnAgentRequest::test_review()).unwrap(),
            "op-omit".into(),
            TopologyHostPorts::default(),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("not granted"), "{}", err.message());
    }

    #[tokio::test]
    async fn invoke_granted_list_agents_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let outcome = invoke_granted_topology_tool_on(
            driver_on(tree, store),
            "root".into(),
            "list_agents",
            &json!({}),
            "op-list".into(),
            TopologyHostPorts::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        assert!(outcome_json(&outcome)["agents"].is_array());
    }

    struct StubRecoverer {
        calls: AtomicUsize,
    }

    impl ChildRecoverer for StubRecoverer {
        fn recover(
            &self,
            agent_id: String,
            expected_attempt_id: Option<crate::protocol::AttemptId>,
        ) -> Pin<Box<dyn Future<Output = Result<RecoverAgentOutput, TopologyError>> + Send + '_>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(RecoverAgentOutput {
                    agent_id,
                    attempt_id: expected_attempt_id,
                    recovered: true,
                    status: NodeStatus::Failed,
                    attempt: Some(crate::protocol::AttemptStatus::Failed),
                    failure_kind: Some(crate::tree::TURN_ABANDONED.to_owned()),
                    workspace_recovery: None,
                })
            })
        }
    }

    /// `recover_agent` is authorized like `close_agent` before the host port
    /// is asked, and a host with no port refuses with a typed reason.
    #[tokio::test]
    async fn invoke_granted_recover_agent_authorizes_then_asks_the_host() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        let child = tree
            .spawn_agent("root", &spawn_request(), None)
            .unwrap()
            .agent_id;
        tree.settle_for_test();
        let driver = driver_on(tree, store);
        let recoverer = Arc::new(StubRecoverer {
            calls: AtomicUsize::new(0),
        });
        let invoke = |caller: &str, target: &str, operation: &str, with_port: bool| {
            let driver = driver.clone();
            let caller = caller.to_owned();
            let arguments = json!({ "agent_id": target });
            let operation = operation.to_owned();
            let ports = TopologyHostPorts {
                recoverer: with_port.then(|| Arc::clone(&recoverer) as Arc<dyn ChildRecoverer>),
                ..TopologyHostPorts::default()
            };
            async move {
                invoke_granted_topology_tool_on(
                    driver,
                    caller,
                    "recover_agent",
                    &arguments,
                    operation,
                    ports,
                )
                .await
            }
        };

        let unported = invoke("root", &child, "op-recover-1", false).await.unwrap();
        assert_eq!(unported.kind(), ToolOutcomeKind::Failure);
        assert_eq!(
            outcome_json(&unported)["code"],
            "recovery_session_unavailable"
        );

        let denied = invoke("root", "root", "op-recover-2", true).await.unwrap();
        assert_eq!(denied.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&denied)["code"], "grant_denied");
        assert_eq!(recoverer.calls.load(Ordering::SeqCst), 0);

        let receipt = invoke("root", &child, "op-recover-3", true).await.unwrap();
        assert_eq!(receipt.kind(), ToolOutcomeKind::Success);
        assert_eq!(outcome_json(&receipt)["agent_id"], child.as_str());
        assert_eq!(outcome_json(&receipt)["recovered"], true);
        assert_eq!(recoverer.calls.load(Ordering::SeqCst), 1);

        let leaf = invoke(&child, &child, "op-recover-4", true).await;
        assert!(leaf.is_err(), "a leaf is not granted recover_agent");
    }

    // ---- Per-cell agent budget ----------------------------

    /// The lineage L0 stamps on a nested call from a Code Mode cell.
    ///
    /// `execute_local_composition_nested` builds exactly this
    /// (`crates/plakous/agent/src/runtime/body.rs:21157`) for `native-ptc`,
    /// `js-ptc` and `py-ptc` alike; only `cell_id` differs between them.
    fn cell_lineage(cell_id: &str, sequence: u32) -> ToolCallerLineage {
        ToolCallerLineage::local_programmatic(
            41,
            7,
            cell_id,
            sequence,
            <plakous::model::CallId as std::str::FromStr>::from_str(
                &uuid::Uuid::from_u128(u128::from(sequence) + 900).to_string(),
            )
            .unwrap(),
            3,
        )
        .unwrap()
    }

    fn spawn_tool_with_budget(driver: TreeDriver, budget: Arc<CellAgentBudget>) -> TopologyTool {
        let mut tool = spawn_tool(driver.clone(), None);
        tool.cell_budget = budget;
        tool
    }

    fn failure_code(outcome: &ToolOutcome) -> String {
        outcome_json(outcome)
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[tokio::test]
    async fn per_cell_agent_budget_denies_the_ninth_spawn() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tool = spawn_tool_with_budget(
            driver.clone(),
            Arc::new(CellAgentBudget::new(DEFAULT_MAX_AGENTS_PER_CELL)),
        );
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        for sequence in 1..=u32::from(DEFAULT_MAX_AGENTS_PER_CELL) {
            let outcome = tool
                .dispatch(
                    &cell_lineage("cell-a", sequence),
                    format!("op-cell-a-{sequence}"),
                    &arguments,
                )
                .await
                .unwrap();
            assert_ne!(
                failure_code(&outcome),
                "cell_budget_exhausted",
                "admission {sequence} of {DEFAULT_MAX_AGENTS_PER_CELL} must not be budget-refused"
            );
        }
        let ninth = tool
            .dispatch(&cell_lineage("cell-a", 9), "op-cell-a-9".into(), &arguments)
            .await
            .unwrap();
        assert_eq!(ninth.kind(), ToolOutcomeKind::Failure);
        let body = outcome_json(&ninth);
        assert_eq!(body["code"], "cell_budget_exhausted");
        assert_eq!(body["budget"], 8);
        assert_eq!(body["used"], 8);
        assert_eq!(body["cell_id"], "root/41/7/cell-a");
    }

    /// `purpose=followup` is new agent work, so it spends the
    /// cell's budget; ordinary mail on a child the cell already has does not,
    /// or an exhausted cell could not steer what it started.
    #[tokio::test]
    async fn followup_spends_the_cell_budget_and_ordinary_mail_does_not() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let mut tool = send_tool(driver.clone(), None);
        tool.cell_budget = Arc::new(CellAgentBudget::new(1));

        let mut request = serde_json::json!({ "agent_id": "c1", "message": "steer" });
        assert!(
            !tool.consumes_cell_agent_budget_for(&request),
            "ordinary mail rides an agent that already exists"
        );
        request["purpose"] = serde_json::json!("followup");
        assert!(tool.consumes_cell_agent_budget_for(&request));

        // The one unit this cell has goes to the followup; a second is refused
        // by the budget before the tree ever sees it.
        let first = tool
            .dispatch(&cell_lineage("cell-f", 1), "op-cell-f-1".into(), &request)
            .await
            .unwrap();
        assert_ne!(failure_code(&first), "cell_budget_exhausted");
        let second = tool
            .dispatch(&cell_lineage("cell-f", 2), "op-cell-f-2".into(), &request)
            .await
            .unwrap();
        assert_eq!(failure_code(&second), "cell_budget_exhausted");
        // Steering is still available to the exhausted cell.
        let steer = serde_json::json!({ "agent_id": "c1", "message": "steer" });
        let steered = tool
            .dispatch(&cell_lineage("cell-f", 3), "op-cell-f-3".into(), &steer)
            .await
            .unwrap();
        assert_ne!(failure_code(&steered), "cell_budget_exhausted");
    }

    #[tokio::test]
    async fn nested_agent_budget_resets_per_cell() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let budget = Arc::new(CellAgentBudget::new(1));
        let tool = spawn_tool_with_budget(driver.clone(), Arc::clone(&budget));
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        assert_ne!(
            failure_code(
                &tool
                    .dispatch(&cell_lineage("cell-a", 1), "op-a-1".into(), &arguments)
                    .await
                    .unwrap()
            ),
            "cell_budget_exhausted"
        );
        assert_eq!(
            failure_code(
                &tool
                    .dispatch(&cell_lineage("cell-a", 2), "op-a-2".into(), &arguments)
                    .await
                    .unwrap()
            ),
            "cell_budget_exhausted"
        );
        // A second cell in the same evaluation starts at zero.
        assert_ne!(
            failure_code(
                &tool
                    .dispatch(&cell_lineage("cell-b", 1), "op-b-1".into(), &arguments)
                    .await
                    .unwrap()
            ),
            "cell_budget_exhausted"
        );
    }

    #[tokio::test]
    async fn agent_budget_is_keyed_on_local_programmatic_lineage_not_operation_id() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tool = spawn_tool_with_budget(driver.clone(), Arc::new(CellAgentBudget::new(1)));
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        // Every nested call carries a distinct derived child call id, so the
        // `operation_id` differs on each of these; the budget must still see
        // one cell. Keying on `operation_id` would give each call its own.
        tool.dispatch(
            &cell_lineage("cell-a", 1),
            "op-derived-1".into(),
            &arguments,
        )
        .await
        .unwrap();
        let second = tool
            .dispatch(
                &cell_lineage("cell-a", 2),
                "op-derived-2".into(),
                &arguments,
            )
            .await
            .unwrap();
        assert_eq!(failure_code(&second), "cell_budget_exhausted");
    }

    #[tokio::test]
    async fn direct_topology_calls_bypass_the_cell_budget() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tool = spawn_tool_with_budget(driver.clone(), Arc::new(CellAgentBudget::new(1)));
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        for operation in 1..=3 {
            let outcome = tool
                .dispatch(
                    &ToolCallerLineage::Direct,
                    format!("op-direct-{operation}"),
                    &arguments,
                )
                .await
                .unwrap();
            assert_ne!(failure_code(&outcome), "cell_budget_exhausted");
        }
        // A hosted-program caller is not a cell either.
        let provider = ToolCallerLineage::provider_programmatic(
            plakous::model::NamespaceId::new("hosted.program").unwrap(),
            "exec-1",
        )
        .unwrap();
        let outcome = tool
            .dispatch(&provider, "op-provider-1".into(), &arguments)
            .await
            .unwrap();
        assert_ne!(failure_code(&outcome), "cell_budget_exhausted");
    }

    #[tokio::test]
    async fn every_code_mode_variant_hits_the_same_budget_check() {
        // native-ptc, js-ptc and py-ptc share this host-call dispatch: L0
        // stamps the same LocalProgrammatic lineage whichever interpreter
        // produced the nested request, so the three differ only in `cell_id`.
        for cell_id in ["native-ptc-cell-1", "js-ptc-cell-1", "py-ptc-cell-1"] {
            let root = tempfile::tempdir().unwrap();
            let (store, tree) = persist_fixture(&root);
            let driver = driver_on(tree, Arc::clone(&store));
            let tool = spawn_tool_with_budget(driver.clone(), Arc::new(CellAgentBudget::new(1)));
            let arguments = serde_json::to_value(spawn_request()).unwrap();
            tool.dispatch(
                &cell_lineage(cell_id, 1),
                format!("op-{cell_id}-1"),
                &arguments,
            )
            .await
            .unwrap();
            let refused = tool
                .dispatch(
                    &cell_lineage(cell_id, 2),
                    format!("op-{cell_id}-2"),
                    &arguments,
                )
                .await
                .unwrap();
            assert_eq!(
                failure_code(&refused),
                "cell_budget_exhausted",
                "{cell_id} must be budgeted by the same host check"
            );
            assert_eq!(
                outcome_json(&refused)["cell_id"],
                format!("root/41/7/{cell_id}")
            );
        }
    }

    #[tokio::test]
    async fn an_exhausted_cell_can_still_wait_list_and_close() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let budget = Arc::new(CellAgentBudget::new(1));
        let spawn = spawn_tool_with_budget(driver.clone(), Arc::clone(&budget));
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        spawn
            .dispatch(
                &cell_lineage("cell-a", 1),
                "op-exhaust-1".into(),
                &arguments,
            )
            .await
            .unwrap();
        assert_eq!(
            failure_code(
                &spawn
                    .dispatch(
                        &cell_lineage("cell-a", 2),
                        "op-exhaust-2".into(),
                        &arguments
                    )
                    .await
                    .unwrap()
            ),
            "cell_budget_exhausted"
        );
        let mut close = close_tool(driver.clone(), None);
        close.cell_budget = Arc::clone(&budget);
        let closed = close
            .dispatch(
                &cell_lineage("cell-a", 3),
                "op-exhaust-close".into(),
                &agent_id_request("c1"),
            )
            .await
            .unwrap();
        assert_eq!(closed.kind(), ToolOutcomeKind::Success);
    }

    #[test]
    fn one_cell_budget_is_shared_by_every_topology_tool_on_a_tree() {
        // The extension set is rebuilt per caller and per turn; a counter owned
        // by one set would reset under the cell and enforce nothing. The tree
        // owns it, per epoch.
        let root = tempfile::tempdir().unwrap();
        let tree = bootstrap_tree("tree-w4-shared", root.path());
        let left = tree.cell_budget();
        let right = tree.cell_budget();
        assert!(Arc::ptr_eq(&left, &right));
        assert!(left.budget() <= CEILING_MAX_AGENTS_PER_CELL);
    }

    /// The design's headline oracle: no VCS subprocess
    /// runs under the tree mutex. A `FakeVcs` watching the tree's own mutex
    /// records, per call, whether the lock was held; every mint, occupancy
    /// probe, seal, unpin, adopt, hook, and unmint the tools drive must have
    /// found it released. There is no exception: the mint is a ticket
    /// `spawn_reserve` hands back, and the `list_agents` probes are a
    /// snapshot taken before the render.
    #[tokio::test]
    async fn no_vcs_subprocess_runs_under_the_tree_mutex() {
        use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};

        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
            Arc::clone(&vcs) as Arc<dyn VcsOps>,
            "tree-lock",
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        binding.on_mint = Some("true".into());
        tree.pin_template("implement", binding);
        let driver = driver_on(tree, Arc::new(MemoryTreeStore::new()));
        let tree = Arc::clone(&driver.tree);
        let watched = Arc::clone(&tree);
        vcs.watch_lock(move || {
            matches!(watched.try_lock(), Err(std::sync::TryLockError::WouldBlock))
        });

        // spawn: mint (under the lock, by design) then the hook (an effect).
        let spawned = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-spawn".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(spawned.kind(), ToolOutcomeKind::Success, "{spawned:?}");
        let child = outcome_json(&spawned)["agent_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let workspace_id = outcome_json(&spawned)["workspace"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        // close: the seal (an effect).
        let closed = close_tool(driver.clone(), None)
            .close_with("op-close".into(), &agent_id_request(&child))
            .await
            .unwrap();
        assert_eq!(closed.kind(), ToolOutcomeKind::Success, "{closed:?}");
        let candidate_id = tree
            .lock()
            .unwrap()
            .attempt(&child)
            .unwrap()
            .candidate_id
            .clone()
            .expect("the closed copy sealed");
        // adopt: the landing (an effect).
        let adopted = adopt_tool(driver.clone())
            .adopt_with(
                "op-adopt",
                &serde_json::to_value(AdoptCandidateRequest {
                    candidate_id,
                    expected_target: None,
                    evidence: None,
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(adopted.kind(), ToolOutcomeKind::Success, "{adopted:?}");
        // discard: the unmint (an effect).
        let discarded = discard_tool(driver.clone())
            .run_named("op-discard", &workspace_request(&workspace_id))
            .unwrap();
        assert_eq!(discarded.kind(), ToolOutcomeKind::Success, "{discarded:?}");

        // list: the occupancy probes (a snapshot taken before the render).
        let listed = list_tool(driver.clone())
            .run_named(
                "op-list",
                &serde_json::to_value(ListAgentsRequest {
                    include_read: None,
                    include_self: None,
                    include_completed: Some(true),
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(listed.kind(), ToolOutcomeKind::Success, "{listed:?}");

        let calls = vcs.calls();
        for op in [
            FakeOp::Mint,
            FakeOp::Hook,
            FakeOp::Seal,
            FakeOp::Adopt,
            FakeOp::Unmint,
            FakeOp::Dirty,
        ] {
            assert!(
                calls.iter().any(|call| call.op == op),
                "{op:?} ran: {calls:?}"
            );
        }
        let under_lock: Vec<_> = calls
            .iter()
            .filter(|call| call.op.is_effect() && call.lock_held)
            .collect();
        assert!(
            under_lock.is_empty(),
            "a VCS subprocess ran under the tree mutex: {under_lock:?}"
        );
        // The store saw every publish the settle made.
        let stored = driver
            .store
            .load("tree-lock")
            .unwrap()
            .expect("the settled tree was published");
        assert_eq!(stored.rows().candidates().len(), 1);
    }

    // ---- Deferred seals, split adoption, minting and followups ----

    /// At the tools layer: `close_agent` on an idle copy seals
    /// through the deferred seam and lands the candidate after the lock drops.
    #[tokio::test]
    async fn close_tool_seals_an_idle_copy_after_the_lock_drops() {
        let (_root, store, mut tree) = git_persist_fixture();
        let child = tree
            .spawn_agent("root", &mint_spawn_request(), None)
            .unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("H.txt"),
            "child\n",
        )
        .unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let outcome = close_tool(driver.clone(), None)
            .close_with("op-close-seal".into(), &agent_id_request(&child.agent_id))
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        {
            let mut tree = tree.lock().unwrap();
            assert_eq!(
                tree.node(&child.agent_id).unwrap().status,
                NodeStatus::Closed
            );
            assert_eq!(tree.candidates().len(), 1, "the idle closed copy sealed");
            assert!(tree.take_effects().is_empty(), "no ticket was stranded");
        }
        let loaded = reload(&store, "tree1");
        assert_eq!(loaded.candidates().len(), 1, "the seal was persisted");
    }

    /// A mutation cannot strand what it queued.
    ///
    /// `ProductTopologyRuntime.tree` and `TreeDriver.tree` used to be `pub`,
    /// so a caller above this crate could lock the tree, mutate it, and drop
    /// the guard with the seal that mutation queued still sitting on the
    /// tree — the node stays `Sealing` and reopens `RecoveryRequired`, the
    /// same fault a crash mid-subprocess leaves. The handle is private now
    /// and [`TreeDriver::mutate`] is the way through, which cannot lose the
    /// effects because it hands them back: the tree owes nothing the moment
    /// the mutation returns.
    #[test]
    fn a_mutation_through_the_driver_cannot_strand_its_effects() {
        let (root, store, mut tree) = git_persist_fixture();
        let child = tree
            .spawn_agent("root", &mint_spawn_request(), None)
            .unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("H.txt"),
            "child\n",
        )
        .unwrap();
        tree.settle_for_test();
        let driver = driver_on(tree, Arc::clone(&store));
        assert!(
            !driver.read(ProductAgentTree::owes_effects).unwrap(),
            "the fixture starts settled"
        );

        // A close queues the child's seal.
        let (closed, effects) = driver
            .mutate(|tree| {
                tree.close_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: child.agent_id.clone(),
                        expected_attempt_id: None,
                    },
                    Some("op-strand"),
                )
            })
            .unwrap();
        assert!(closed.is_ok());
        assert!(!effects.is_empty(), "the close owed the VCS a seal");
        assert!(
            !driver.read(ProductAgentTree::owes_effects).unwrap(),
            "the mutation handed its effects out; the tree kept none"
        );

        // Until they are settled the node is still mid-seal, which is exactly
        // what a dropped batch would have left behind forever.
        assert_eq!(
            driver
                .read(|tree| tree.node(&child.agent_id).unwrap().status)
                .unwrap(),
            NodeStatus::Sealing
        );
        driver.save(&mut driver.lock().unwrap()).unwrap();
        driver.settle(effects).unwrap();
        assert_eq!(
            driver
                .read(|tree| tree.node(&child.agent_id).unwrap().status)
                .unwrap(),
            NodeStatus::Closed
        );
        assert_eq!(
            driver.read(|tree| tree.candidates().len()).unwrap(),
            1,
            "the seal ran and left the candidate"
        );
        drop(root);
    }

    /// The settled pairing does both halves in one call, so the window in
    /// which a batch can be dropped does not exist for its callers.
    #[test]
    fn mutate_and_settle_leaves_nothing_owed() {
        let (root, store, mut tree) = git_persist_fixture();
        let child = tree
            .spawn_agent("root", &mint_spawn_request(), None)
            .unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("H.txt"),
            "child\n",
        )
        .unwrap();
        tree.settle_for_test();
        let driver = driver_on(tree, Arc::clone(&store));
        let (closed, _settled) = driver
            .mutate_and_settle(|tree| {
                tree.close_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: child.agent_id.clone(),
                        expected_attempt_id: None,
                    },
                    Some("op-settled"),
                )
            })
            .unwrap();
        assert!(closed.is_ok());
        assert!(!driver.read(ProductAgentTree::owes_effects).unwrap());
        assert_eq!(
            driver
                .read(|tree| tree.node(&child.agent_id).unwrap().status)
                .unwrap(),
            NodeStatus::Closed
        );
        assert_eq!(driver.read(|tree| tree.candidates().len()).unwrap(), 1);
        drop(root);
    }

    /// the tools-layer adopt lands with the lock released and
    /// records a replayable receipt; a refusal closes the operation too.
    #[tokio::test]
    async fn adopt_with_running_jobs_succeeds_and_warns_by_id() {
        let (root, store, mut tree) = git_persist_fixture();
        let child = tree
            .spawn_agent("root", &mint_spawn_request(), None)
            .unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("H.txt"),
            "child\n",
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = tree
            .attempt(&child.agent_id)
            .unwrap()
            .candidate_id
            .clone()
            .unwrap();
        let driver = driver_on(tree, store);
        let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
        let tool = adopt_tool(driver.clone());
        let outcome = tool
            .adopt_with("adopt-running-job", &json!({"candidate_id": candidate_id}))
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
        let ToolResultPart::Text(block) = &outcome.parts()[0] else {
            panic!("text receipt required")
        };
        let receipt: Value = serde_json::from_str(block.text()).unwrap();
        assert!(
            receipt["warning"]
                .as_str()
                .unwrap()
                .contains("running processes p1")
        );
        assert_eq!(receipt["candidate_id"], json!(candidate_id));
        assert_eq!(jobs.jobs()[0].status, plakous::tools::JobStatus::Running);
        jobs.stop_jobs(
            plakous::tools::JobStopReason::Model,
            std::time::Duration::from_millis(250),
        )
        .unwrap();
        let replay = tool
            .adopt_with("adopt-running-job", &json!({"candidate_id": candidate_id}))
            .unwrap();
        assert_eq!(
            outcome_json(&replay),
            receipt,
            "warning is the stored observation, not recomputed on replay"
        );
    }

    #[tokio::test]
    async fn adopt_tool_lands_outside_the_lock_and_records_the_receipt() {
        let (root, store, mut tree) = git_persist_fixture();
        let child = tree
            .spawn_agent("root", &mint_spawn_request(), None)
            .unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("H.txt"),
            "child\n",
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = tree
            .attempt(&child.agent_id)
            .unwrap()
            .candidate_id
            .clone()
            .unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let tool = adopt_tool(driver.clone());
        let request = serde_json::to_value(AdoptCandidateRequest {
            candidate_id: candidate_id.clone(),
            expected_target: None,
            evidence: None,
        })
        .unwrap();
        let outcome = tool.adopt_with("op-adopt", &request).unwrap();
        assert_eq!(
            outcome.kind(),
            ToolOutcomeKind::Success,
            "{}",
            outcome_json(&outcome)
        );
        assert_eq!(outcome_json(&outcome)["files_changed"], 1);
        assert_eq!(
            std::fs::read_to_string(root.path().join("H.txt")).unwrap(),
            "child\n"
        );
        let replay = tool.adopt_with("op-adopt", &request).unwrap();
        assert_eq!(outcome_json(&replay), outcome_json(&outcome));
        let refused = tool
            .adopt_with(
                "op-adopt-unknown",
                &serde_json::to_value(AdoptCandidateRequest {
                    candidate_id: CandidateId::from_persisted("cand-nope".to_owned()),
                    expected_target: None,
                    evidence: None,
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(refused.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&refused)["code"], "not_sealed");
        let tree = tree.lock().unwrap();
        assert_eq!(
            tree.operations()["op-adopt-unknown"].terminal_code,
            Some(TopologyErrorCode::NotSealed)
        );
        assert_eq!(
            tree.workspace(&child.workspace.id).unwrap().status,
            WorkspaceStatus::Adopted
        );
    }

    /// the template's `on_mint` runs once in the new copy; a
    /// non-zero exit unmints it and fails the spawn with `mint_hook_failed`.
    #[tokio::test]
    async fn spawn_tool_runs_on_mint_and_fails_closed_on_non_zero() {
        let (_root, store, mut tree) = git_persist_fixture();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        binding.on_mint = Some("echo minted > MARK.txt".into());
        tree.pin_template("implement", binding.clone());
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let outcome = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-hook-ok".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.kind(),
            ToolOutcomeKind::Success,
            "{}",
            outcome_json(&outcome)
        );
        let first = outcome_json(&outcome)["agent_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let first_path = child_path(&tree, &first);
        assert_eq!(
            std::fs::read_to_string(first_path.join("MARK.txt")).unwrap(),
            "minted\n"
        );

        binding.on_mint = Some("echo broken >&2; exit 1".into());
        tree.lock().unwrap().pin_template("implement", binding);
        let outcome = spawn_tool(driver.clone(), None)
            .spawn_with(
                "op-hook-fail".into(),
                &serde_json::to_value(mint_spawn_request()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure, "{outcome:?}");
        let json = outcome_json(&outcome);
        assert_eq!(json["code"], "mint_hook_failed");
        assert!(
            json["message"].as_str().unwrap().contains("broken"),
            "{json}"
        );
        let stored = tree.lock().unwrap();
        let failed = stored
            .nodes()
            .values()
            .find(|node| node.agent_id != first && node.parent_agent_id.is_some())
            .expect("the failed spawn keeps a row");
        assert_eq!(failed.status, NodeStatus::Failed);
        assert!(!failed.path.exists(), "the copy was unminted");
        assert_eq!(
            stored.operations()["op-hook-fail"].terminal_code,
            Some(TopologyErrorCode::MintHookFailed)
        );
        let loaded = reload(&store, "tree1");
        assert_eq!(
            loaded.node(&failed.agent_id).unwrap().status,
            NodeStatus::Failed,
            "the failure was persisted"
        );
    }

    /// Spawn → complete → `followup` opens attempt a2 on the
    /// same node, delivered as a Start, and that attempt completes.
    #[tokio::test]
    async fn followup_after_completion_opens_attempt_two_and_completes() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        let binder = Arc::new(StubBinder::ok());
        let spawned = spawn_tool(
            driver.clone(),
            Some(Arc::clone(&binder) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(spawned.kind(), ToolOutcomeKind::Success);
        {
            let mut tree = tree.lock().unwrap();
            tree.submit_result(
                "c1",
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed("c1").unwrap();
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Completed);
        }
        let deliverer = Arc::new(StubDeliverer::ok().binding(&tree));
        let body = "🦀".repeat(crate::protocol::MAX_MESSAGE_BYTES / 4);
        let followup = serde_json::to_value(SendAgentMessageRequest {
            agent_id: "c1".into(),
            message: body.clone(),
            purpose: Some(MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        })
        .unwrap();
        let mut too_big = followup.clone();
        too_big["message"] = json!(body.clone() + "a");
        let refused = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        )
        .send_with("op-followup-too-big".into(), &too_big)
        .await
        .unwrap();
        assert_eq!(outcome_json(&refused)["code"], "bound_exceeded");
        assert!(deliverer.calls.lock().unwrap().is_empty());
        let outcome = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        )
        .send_with("op-followup".into(), &followup)
        .await
        .unwrap();
        assert_eq!(
            outcome.kind(),
            ToolOutcomeKind::Success,
            "{}",
            outcome_json(&outcome)
        );
        assert_eq!(outcome_json(&outcome)["disposition"], "started");
        {
            let calls = deliverer.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].disposition, MailDisposition::Started);
            assert_eq!(calls[0].turn_token, 2, "the new attempt's token travels");
            assert_eq!(calls[0].body, body);
        }
        {
            let mut tree = tree.lock().unwrap();
            let attempt = tree.attempt("c1").unwrap();
            assert_eq!(attempt.attempt_id.as_str(), "c1-a2");
            assert_eq!(attempt.status, crate::protocol::AttemptStatus::Running);
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
            assert!(tree.node("c1").unwrap().active_turn);
            tree.submit_result(
                "c1",
                &SubmitResultRequest {
                    turn_token: 2,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed("c1").unwrap();
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Completed);
            assert_eq!(
                tree.attempt("c1").unwrap().status,
                crate::protocol::AttemptStatus::Completed
            );
        }
    }

    #[tokio::test]
    async fn owner_fenced_followup_refusal_fails_closed_without_erasing_evidence() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        let id = tree
            .spawn_agent("root", &spawn_request(), None)
            .unwrap()
            .agent_id;
        tree.submit_result(
            &id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&id).unwrap();
        tree.settle_for_test();
        let opener = tree
            .send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: id.clone(),
                    message: "again".into(),
                    purpose: Some(MessagePurpose::Followup),
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
            .message_id;
        tree.record_host_retirement(&id, "owner release failed".into())
            .and_then(|value| tree.fold_due(&id).map(|_| value))
            .unwrap();
        let pending = tree.attempt(&id).unwrap().attempt_id.clone();
        let driver = driver_on(tree, store);
        let tool = send_tool(driver.clone(), None);
        let outcome = tool
            .refuse_delivery(
                None,
                &id,
                &opener,
                MailDeliveryFailure::Refused("child is retiring".into()),
            )
            .await
            .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(
            outcome_json(&outcome)["code"],
            "operation_recovery_required"
        );
        driver
            .read(|tree| {
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
                let attempt = tree.attempt(&id).unwrap();
                assert_eq!(attempt.attempt_id, pending);
                assert_eq!(
                    attempt.status,
                    crate::protocol::AttemptStatus::RecoveryRequired
                );
                assert_eq!(attempt.failure.as_ref().unwrap().kind, "session_retirement");
                assert!(
                    attempt
                        .failure
                        .as_ref()
                        .unwrap()
                        .message
                        .contains("owner release failed")
                );
                assert!(matches!(
                    tree.recovery_need(&id).unwrap(),
                    crate::tree::RecoveryNeed::Session { .. }
                ));
            })
            .unwrap();
    }

    /// A `followup` whose Start the deliverer refuses leaves no attempt row.
    /// The refused Start left `c1-a2` in place, the node fell
    /// to `recovery_required`, and its partial-work seal minted `cand-c1-a2`
    /// — which moved the workspace head out from under the parent's adopt of
    /// `cand-c1-a1` (`target_changed`).
    #[tokio::test]
    async fn refused_followup_start_rolls_the_attempt_back() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        spawn_tool(
            driver.clone(),
            Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
        )
        .spawn_with(
            "op-spawn".into(),
            &serde_json::to_value(spawn_request()).unwrap(),
        )
        .await
        .unwrap();
        let (schema_before, candidates_before) = {
            let mut tree = tree.lock().unwrap();
            tree.submit_result(
                "c1",
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed("c1").unwrap();
            let node = tree.node("c1").unwrap();
            assert_eq!(node.status, NodeStatus::Completed);
            (node.output_schema_json.clone(), tree.candidates().len())
        };
        let deliverer = Arc::new(StubDeliverer::refuse());
        let followup = serde_json::to_value(SendAgentMessageRequest {
            agent_id: "c1".into(),
            message: "re-task the finished child".into(),
            purpose: Some(MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        })
        .unwrap();
        let outcome = send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        )
        .send_with("op-followup-refused".into(), &followup)
        .await
        .unwrap();
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(outcome_json(&outcome)["code"], "capacity");
        assert!(
            outcome_json(&outcome)["message"]
                .as_str()
                .unwrap()
                .contains("rolled back"),
            "{outcome:?}"
        );
        assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
        let tree = tree.lock().unwrap();
        let node = tree.node("c1").unwrap();
        assert_eq!(node.status, NodeStatus::Completed);
        assert_eq!(node.attempt_id.as_ref().unwrap().as_str(), "c1-a1");
        assert_eq!(node.output_schema_json, schema_before);
        assert!(!node.active_turn);
        assert_eq!(
            tree.attempt("c1").unwrap().status,
            crate::protocol::AttemptStatus::Completed
        );
        assert!(
            tree.attempts().keys().all(|id| id.as_str() != "c1-a2"),
            "the refused start must leave no attempt row"
        );
        assert_eq!(tree.candidates().len(), candidates_before);
        let opening = &deliverer.calls.lock().unwrap()[0].message_id;
        assert_eq!(
            tree.mail(opening).unwrap().durable,
            crate::tree::MailDurableState::Acknowledged,
        );
        assert_eq!(
            tree.operations()["op-followup-refused"].terminal_code,
            Some(TopologyErrorCode::Capacity),
        );
    }

    #[tokio::test]
    async fn spawn_close_emits_tree_node_spawned_and_detached() {
        let root = tempfile::tempdir().unwrap();
        let tree_id = uuid::Uuid::new_v4().to_string();
        let store: Arc<dyn TreeStore> = Arc::new(MemoryTreeStore::new());
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            tree_id.clone(),
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        tree.acknowledge_revision(store.save(&tree).unwrap());
        let driver = driver_on(tree, store);
        let recorder = Arc::new(RecordingObserver::default());
        let observer = Arc::clone(&recorder) as Arc<dyn TopologyObserver>;
        let spawn = TopologyTool {
            driver: driver.clone(),
            caller_id: Arc::new(Mutex::new("root".into())),
            kind: TopologyKind::Spawn,
            vocabulary: TopologyVocabulary::default(),
            binder: Some(Arc::new(StubBinder::ok()) as Arc<dyn SpawnedSessionBinder>),
            deliverer: None,
            canceller: None,
            recoverer: None,
            observer: Some(Arc::clone(&observer)),
            cell_budget: test_budget(),
        };
        let arguments = serde_json::to_value(spawn_request()).unwrap();
        spawn
            .spawn_with("op-spawn-telemetry".into(), &arguments)
            .await
            .unwrap();
        let close = TopologyTool {
            driver: driver.clone(),
            caller_id: Arc::new(Mutex::new("root".into())),
            kind: TopologyKind::Close,
            vocabulary: TopologyVocabulary::default(),
            binder: None,
            deliverer: None,
            canceller: None,
            recoverer: None,
            observer: Some(Arc::clone(&observer)),
            cell_budget: test_budget(),
        };
        close
            .close_with("op-close-telemetry".into(), &agent_id_request("c1"))
            .await
            .unwrap();
        let spawned = recorder.spawned.lock().unwrap().clone();
        let detached = recorder.detached.lock().unwrap().clone();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].0, tree_id);
        assert_eq!(spawned[0].1, "c1");
        assert_eq!(spawned[0].2.as_deref(), Some("root"));
        assert_eq!(detached.len(), 1);
        assert_eq!(detached[0].0, tree_id);
        assert_eq!(detached[0].1, "c1");
        assert_eq!(detached[0].2, NodeDetach::Closed);
    }

    #[tokio::test]
    async fn fail_closed_session_does_not_project_unsaved_materialization() {
        use plakous_workspace::occupancy_vcs::fake::{FakeOp, FakeVcs};

        let root = tempfile::tempdir().unwrap();
        let vcs = Arc::new(FakeVcs::new());
        let driver = driver_on(
            ProductAgentTree::bootstrap_root_for_test_with(
                vcs.clone(),
                uuid::Uuid::new_v4().to_string(),
                "root",
                "owner",
                root.path().to_path_buf(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap(),
            Arc::new(FailingStore),
        );
        let tree = Arc::clone(&driver.tree);
        let agent_id = {
            let mut tree = tree.lock().unwrap();
            let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
            binding.max_tier = Tier::Mutate;
            tree.pin_template("implement", binding);
            let spawned = tree
                .spawn_agent("root", &mint_spawn_request(), None)
                .unwrap();
            tree.bind_session_locator(&spawned.agent_id, "sess/fail")
                .unwrap();
            spawned.agent_id
        };
        let recorder = Arc::new(RecordingObserver::default());
        let observer = Arc::clone(&recorder) as Arc<dyn TopologyObserver>;
        let tool = TopologyTool {
            driver: driver.clone(),
            caller_id: Arc::new(Mutex::new("root".into())),
            kind: TopologyKind::Spawn,
            vocabulary: TopologyVocabulary::default(),
            binder: None,
            deliverer: None,
            canceller: None,
            recoverer: None,
            observer: Some(Arc::clone(&observer)),
            cell_budget: test_budget(),
        };
        let _ = tool.fail_closed_session(&agent_id, "materialization failed".into());
        driver
            .read(|tree| {
                assert_eq!(
                    tree.attempt(&agent_id).unwrap().status,
                    crate::protocol::AttemptStatus::Running
                );
                assert_eq!(tree.node(&agent_id).unwrap().status, NodeStatus::Running);
                assert!(tree.attempt(&agent_id).unwrap().failure.is_none());
                assert!(tree.attempt(&agent_id).unwrap().candidate_id.is_none());
            })
            .unwrap();
        assert!(vcs.calls_of(FakeOp::Seal).is_empty());
        let detached = recorder.detached.lock().unwrap().clone();
        assert_eq!(detached.len(), 1);
        assert_eq!(detached[0].1, agent_id);
        assert_eq!(detached[0].2, NodeDetach::RecoveryRequired);
    }

    fn task_tool(driver: TreeDriver, kind: TopologyKind) -> TopologyTool {
        let mut tool = wait_tool(driver);
        tool.kind = kind;
        tool.vocabulary = TopologyVocabulary::TaskTools;
        tool
    }

    fn task_output(task_ids: &[&str], timeout_ms: Option<u64>) -> TaskOutputRequest {
        TaskOutputRequest {
            task_ids: task_ids.iter().map(|id| (*id).to_owned()).collect(),
            timeout_ms,
        }
    }

    /// The await receipt inside the topology envelope a Grok call naming a
    /// child returns.
    fn enveloped(outcome: &ToolOutcome) -> Value {
        let outer = outcome_json(outcome);
        serde_json::from_str(outer["result"].as_str().expect("topology envelope")).unwrap()
    }

    #[test]
    fn task_ids_split_into_process_numbers_and_agent_ids() {
        for (id, number) in [
            ("p3", Some(3)),
            ("3", Some(3)),
            ("c3", None),
            ("p0", None),
            ("0", None),
            ("p", None),
            ("p-1", None),
        ] {
            assert_eq!(task_process_number(id), number, "{id}");
            assert_eq!(
                crate::protocol::is_process_task_id(id),
                number.is_some(),
                "{id}"
            );
        }
    }

    /// Under the Grok vocabulary the granted await and interrupt slots are
    /// presented as Grok's output and kill tools instead of `wait_agent` and
    /// `interrupt_agent`; the grant itself, and a leaf's lack of both, is
    /// unchanged.
    #[test]
    fn grok_vocabulary_presents_the_await_and_interrupt_under_grok_names() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("grok-names", root.path());
        let spawned = tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let tree = Arc::new(Mutex::new(tree));
        let registrations = |caller: &str, vocabulary| {
            topology_tool_registrations_in(Arc::clone(&tree), caller, vocabulary).unwrap()
        };
        let names = |registrations: &[ToolRegistration]| {
            registrations
                .iter()
                .map(|registration| registration.name().as_str().to_owned())
                .collect::<Vec<_>>()
        };
        let plak = names(&registrations("root", TopologyVocabulary::Plak));
        let grok_registrations = registrations("root", TopologyVocabulary::TaskTools);
        let grok = names(&grok_registrations);
        assert!(plak.iter().any(|name| name == "wait_agent"));
        assert!(plak.iter().any(|name| name == "interrupt_agent"));
        assert!(!plak.iter().any(|name| name.contains("command_or_subagent")));
        for name in [
            "get_command_or_subagent_output",
            "kill_command_or_subagent",
            "close_agent",
        ] {
            assert!(grok.iter().any(|presented| presented == name), "{name}");
        }
        for name in ["wait_agent", "interrupt_agent"] {
            assert!(!grok.iter().any(|presented| presented == name), "{name}");
        }
        assert_eq!(plak.len(), grok.len());
        let spawn = grok_registrations
            .iter()
            .find(|registration| registration.name().as_str() == "spawn_agent")
            .unwrap();
        assert!(
            spawn
                .definition()
                .description()
                .contains("get_command_or_subagent_output returns them")
        );
        assert!(!spawn.definition().description().contains("wait_agent"));
        let leaf = names(&registrations(
            &spawned.agent_id,
            TopologyVocabulary::TaskTools,
        ));
        assert!(
            !leaf.iter().any(|name| name.contains("command_or_subagent")
                || name.ends_with("_agent")
                    && name != "list_agents"
                    && name != "send_agent_message"),
            "{leaf:?}"
        );
    }

    /// Grok's output tool naming a child is the await: an omitted timeout is
    /// a snapshot that saves nothing, a settled child returns its row and the
    /// mail it consumed under the one await receipt kind, and the same call
    /// replays the receipt.
    #[tokio::test]
    async fn grok_output_over_a_child_is_the_await_with_its_receipt_and_replay() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tool = task_tool(driver.clone(), TopologyKind::TaskOutput);
        let snapshot = tool
            .task_output_with(
                "grok-snapshot",
                &task_output(&["c1"], None),
                None,
                &ToolCallerLineage::Direct,
            )
            .await
            .unwrap();
        let inner = enveloped(&snapshot);
        assert_eq!(inner["timed_out"], true);
        assert_eq!(inner["woke_by"], "timeout");
        assert_eq!(inner["agents"][0]["agent_id"], "c1");
        assert!(driver.tree.lock().unwrap().operations().is_empty());
        {
            let mut tree = driver.tree.lock().unwrap();
            tree.send_agent_message(
                "c1",
                &SendAgentMessageRequest {
                    agent_id: "root".into(),
                    message: "halfway there".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed("c1").unwrap();
        }
        let request = task_output(&["c1"], Some(5_000));
        let settled = tool
            .task_output_with("grok-await", &request, None, &ToolCallerLineage::Direct)
            .await
            .unwrap();
        let inner = enveloped(&settled);
        assert!(
            matches!(inner["woke_by"].as_str(), Some("settled" | "mail")),
            "{inner}"
        );
        assert_eq!(inner["mail"][0]["body"], "halfway there");
        {
            let tree = driver.tree.lock().unwrap();
            assert_eq!(
                tree.pending_mail("root"),
                [] as [crate::tree::MailRecord; 0]
            );
            assert_eq!(tree.retained_wait_receipts("root"), ["grok-await"]);
        }
        let replay = tool
            .task_output_with("grok-await", &request, None, &ToolCallerLineage::Direct)
            .await
            .unwrap();
        assert_eq!(replay.joined_text(), settled.joined_text());
        // After a reopen the same call replays its durable receipt, mail ack
        // included, without consuming or waking again.
        let reopened = task_tool(
            driver_on(reload(&store, "tree1"), Arc::clone(&store)),
            TopologyKind::TaskOutput,
        );
        let replay = reopened
            .task_output_with("grok-await", &request, None, &ToolCallerLineage::Direct)
            .await
            .unwrap();
        assert_eq!(replay.joined_text(), settled.joined_text());
        assert_eq!(
            reopened.driver.tree.lock().unwrap().pending_mail("root"),
            [] as [crate::tree::MailRecord; 0]
        );
        let unknown = tool
            .task_output_with(
                "grok-unknown",
                &task_output(&["c9"], None),
                None,
                &ToolCallerLineage::Direct,
            )
            .await
            .unwrap();
        assert_eq!(unknown.kind(), ToolOutcomeKind::Failure);
        assert!(outcome_json(&unknown)["code"].is_string());
    }

    /// A js-ptc cell on a Grok hub sees the raw await receipt, the same bytes
    /// `wait_agent` returns. Direct still gets the topology envelope. A
    /// process-shaped result is not wrapped for the cell either: process-only
    /// calls return the process tool's bytes, and the cell path does not add
    /// an envelope around them.
    #[tokio::test]
    async fn js_ptc_grok_hub_child_await_returns_the_raw_receipt() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, store);
        let tool = task_tool(driver, TopologyKind::TaskOutput);
        let request = task_output(&["c1"], None);
        let cell = tool
            .task_output_with("grok-cell", &request, None, &cell_lineage("js-ptc", 1))
            .await
            .unwrap();
        let raw = outcome_json(&cell);
        assert!(raw.get("result").is_none(), "{raw}");
        assert_eq!(raw["agents"][0]["agent_id"], "c1");
        assert_eq!(raw["woke_by"], "timeout");
        let direct = tool
            .task_output_with("grok-direct", &request, None, &ToolCallerLineage::Direct)
            .await
            .unwrap();
        assert_eq!(enveloped(&direct), raw);
        let process = ToolOutcome::success_text(
            r#"{"tasks":[{"task_id":"p1","status":"exited","output":"done"}]}"#,
        )
        .unwrap();
        let shown = present_task_receipt(&cell_lineage("js-ptc", 2), process).unwrap();
        assert_eq!(
            shown.joined_text(),
            r#"{"tasks":[{"task_id":"p1","status":"exited","output":"done"}]}"#
        );
    }

    /// A call naming a child and the caller's own process is one await: the
    /// process is a `jobs[]` snapshot row, its end wakes the await, and an id
    /// the caller does not own refuses the whole call.
    #[tokio::test]
    async fn grok_output_mixed_call_snapshots_jobs_and_wakes_on_their_end() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("grok-mixed", root.path());
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::new(FailingStore));
        let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
        let tool = task_tool(driver.clone(), TopologyKind::TaskOutput);
        let snapshot = tool
            .task_output_with(
                "mixed-snapshot",
                &task_output(&["c1", "p1"], Some(0)),
                None,
                &ToolCallerLineage::Direct,
            )
            .await
            .unwrap();
        let inner = enveloped(&snapshot);
        assert_eq!(inner["agents"][0]["agent_id"], "c1");
        assert_eq!(inner["jobs"][0]["id"], "p1");
        assert_eq!(inner["jobs"][0]["status"], "running");
        let refused = tool
            .task_output_with(
                "mixed-foreign",
                &task_output(&["c1", "p9"], None),
                None,
                &ToolCallerLineage::Direct,
            )
            .await
            .unwrap();
        assert_eq!(outcome_json(&refused)["code"], "grant_denied");
        let request = task_output(&["c1", "1"], Some(300_000));
        let mut pending = Box::pin(tool.task_output_with(
            "mixed-wait",
            &request,
            None,
            &ToolCallerLineage::Direct,
        ));
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
        jobs.stop_jobs(
            plakous::tools::JobStopReason::Model,
            std::time::Duration::from_millis(250),
        )
        .unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap();
        let inner = enveloped(&outcome);
        assert_eq!(inner["woke_by"], "process");
        assert_eq!(inner["jobs"][0]["id"], "p1");
        assert_ne!(inner["jobs"][0]["status"], "running");
        assert!(driver.tree.lock().unwrap().operations().is_empty());
    }

    /// Grok's kill tool naming a child interrupts it, under the interrupt
    /// receipt, and never closes it.
    #[tokio::test]
    async fn grok_kill_on_a_child_interrupts_and_never_closes() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        tree.set_active_turn("c1", true).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tool = task_tool(driver.clone(), TopologyKind::TaskKill);
        let outcome = tool
            .task_kill_child(
                "grok-kill".into(),
                &TaskKillRequest {
                    task_id: "c1".into(),
                },
                &ToolCallerLineage::Direct,
            )
            .await
            .unwrap();
        let inner = enveloped(&outcome);
        assert_eq!(inner["agent_id"], "c1");
        assert_eq!(inner["status"], "interrupted");
        {
            let tree = driver.tree.lock().unwrap();
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
            assert!(tree.operations().contains_key("grok-kill"));
        }
        let unknown = tool
            .task_kill_child(
                "grok-kill-unknown".into(),
                &TaskKillRequest {
                    task_id: "c9".into(),
                },
                &ToolCallerLineage::Direct,
            )
            .await
            .unwrap();
        assert_eq!(unknown.kind(), ToolOutcomeKind::Failure);
        assert!(outcome_json(&unknown)["code"].is_string());
    }

    /// A Grok hub's refusals name the tools that hub was given. A submit
    /// blocked on a live child offers the output and kill tools, never
    /// `wait_agent` or `interrupt_agent`.
    #[test]
    fn grok_hub_refusals_do_not_name_wait_agent_or_interrupt_agent() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("grok-refusal", root.path());
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::new(FailingStore));
        let tool = task_tool(driver, TopologyKind::Submit);
        let outcome = tool
            .run_named(
                "grok-submit-live",
                &json!({"turn_token": 1, "output_json": "{\"ok\":true}"}),
            )
            .unwrap();
        let payload = outcome_json(&outcome);
        assert_eq!(payload["code"], "live_work");
        let message = payload["message"].as_str().unwrap();
        for absent in ["wait_agent", "interrupt_agent"] {
            assert!(!message.contains(absent), "{message}");
        }
        assert!(
            message.contains("get_command_or_subagent_output"),
            "{message}"
        );
        assert!(message.contains("kill_command_or_subagent"), "{message}");
        assert!(message.contains("close_agent"), "{message}");
    }

    fn jobs_tool(driver: TreeDriver, kind: TopologyKind) -> TopologyTool {
        let mut tool = wait_tool(driver);
        tool.kind = kind;
        tool.vocabulary = TopologyVocabulary::WaitStop;
        tool
    }

    fn jobs_wait(ids: Option<&[&str]>, timeout_ms: Option<u64>) -> JobsWaitRequest {
        JobsWaitRequest {
            ids: ids.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect()),
            timeout_ms,
        }
    }

    /// Under pi's vocabulary the granted await and interrupt slots are
    /// presented as `wait` and `stop`; `wait_agent` and `interrupt_agent` are
    /// not, and a leaf has neither.
    #[test]
    fn wait_stop_vocabulary_presents_the_await_and_interrupt_as_wait_and_stop() {
        let root = tempfile::tempdir().unwrap();
        let mut tree = bootstrap_tree("pi-names", root.path());
        let spawned = tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let tree = Arc::new(Mutex::new(tree));
        let registrations = |caller: &str| {
            topology_tool_registrations_in(Arc::clone(&tree), caller, TopologyVocabulary::WaitStop)
                .unwrap()
        };
        let hub = registrations("root");
        let names = hub
            .iter()
            .map(|registration| registration.name().as_str().to_owned())
            .collect::<Vec<_>>();
        for name in ["wait", "stop", "close_agent", "spawn_agent"] {
            assert!(names.iter().any(|presented| presented == name), "{name}");
        }
        for name in ["wait_agent", "interrupt_agent"] {
            assert!(!names.iter().any(|presented| presented == name), "{name}");
        }
        let description = |name: &str| {
            hub.iter()
                .find(|registration| registration.name().as_str() == name)
                .unwrap()
                .definition()
                .description()
                .to_owned()
        };
        assert!(description("spawn_agent").contains("wait returns them"));
        assert!(!description("spawn_agent").contains("wait_agent"));
        assert_eq!(description("wait"), PLAK_TOPOLOGY_JOBS_WAIT_DESCRIPTION);
        assert_eq!(description("stop"), PLAK_TOPOLOGY_JOBS_STOP_DESCRIPTION);
        let leaf = registrations(&spawned.agent_id)
            .iter()
            .map(|registration| registration.name().as_str().to_owned())
            .collect::<Vec<_>>();
        assert!(
            !leaf.iter().any(|name| name == "wait" || name == "stop"),
            "{leaf:?}"
        );
    }

    /// The bare-wait rule (spec 03 §4.4) and the id split: process ids alone
    /// are the process tool's own await; a child id makes it the tree's
    /// await; a bare wait watches pending children and running own jobs, and
    /// is the process tool's bare await when no child is pending or an own job
    /// already ended unreported.
    #[tokio::test]
    async fn pi_wait_plans_split_processes_children_and_the_bare_wait() {
        let root = tempfile::tempdir().unwrap();
        let tree = bootstrap_tree("pi-plan", root.path());
        let driver = driver_on(tree, Arc::new(FailingStore));
        let tool = jobs_tool(driver.clone(), TopologyKind::JobsWait);
        // No children, no jobs: the process tool's bare await.
        assert_eq!(
            tool.jobs_wait_plan("pi-plan", &jobs_wait(None, None))
                .unwrap(),
            JobsWaitPlan::Processes(Vec::new())
        );
        assert_eq!(
            tool.jobs_wait_plan("pi-plan", &jobs_wait(Some(&["p2", "3"]), Some(10)))
                .unwrap(),
            JobsWaitPlan::Processes(vec![2, 3])
        );
        driver
            .tree
            .lock()
            .unwrap()
            .spawn_agent("root", &spawn_request(), None)
            .unwrap();
        // A pending child, no jobs: the tree's await over pending children,
        // 300000 ms by default.
        assert_eq!(
            tool.jobs_wait_plan("pi-plan", &jobs_wait(None, None))
                .unwrap(),
            JobsWaitPlan::Await(AwaitRequest {
                agents: AwaitAgents::Pending,
                processes: Vec::new(),
                timeout_ms: crate::protocol::DEFAULT_WAIT_TIMEOUT_MS,
            })
        );
        assert_eq!(
            tool.jobs_wait_plan("pi-plan", &jobs_wait(Some(&["c1", "p1"]), Some(9_000_000)))
                .unwrap(),
            JobsWaitPlan::Await(AwaitRequest {
                agents: AwaitAgents::Named(vec!["c1".into()]),
                processes: vec!["p1".into()],
                timeout_ms: crate::protocol::MAX_WAIT_TIMEOUT_MS,
            })
        );
        // A running own job joins the bare await.
        let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
        assert_eq!(
            tool.jobs_wait_plan("pi-plan", &jobs_wait(None, Some(0)))
                .unwrap(),
            JobsWaitPlan::Await(AwaitRequest {
                agents: AwaitAgents::Pending,
                processes: vec!["p1".into()],
                timeout_ms: 0,
            })
        );
        // An own job that ended unreported makes it the process tool's bare
        // await, which returns at once and consumes its notice.
        jobs.stop_jobs(
            plakous::tools::JobStopReason::Model,
            std::time::Duration::from_millis(250),
        )
        .unwrap();
        assert_eq!(
            tool.jobs_wait_plan("pi-plan", &jobs_wait(None, None))
                .unwrap(),
            JobsWaitPlan::Processes(Vec::new())
        );
    }

    /// Pi's `wait` over a child is the tree's await: its row and the mail it
    /// consumed come back in the topology envelope under the one await
    /// receipt, and the same call, planned from scratch on the rejoined
    /// tree, replays the receipt.
    #[tokio::test]
    async fn pi_wait_over_a_child_is_the_await_with_its_receipt_and_replay() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tool = jobs_tool(driver.clone(), TopologyKind::JobsWait);
        child_reports(&driver);
        let request = jobs_wait(None, Some(5_000));
        let JobsWaitPlan::Await(awaited) = tool.jobs_wait_plan("pi-await", &request).unwrap()
        else {
            panic!("a pending child makes a bare wait the tree's await");
        };
        let settled = tool
            .jobs_wait_with(
                "pi-await",
                &awaited,
                &request,
                None,
                &ToolCallerLineage::Direct,
            )
            .await
            .unwrap();
        let inner = enveloped(&settled);
        assert!(
            matches!(inner["woke_by"].as_str(), Some("settled" | "mail")),
            "{inner}"
        );
        assert_eq!(inner["mail"][0]["body"], "halfway there");
        {
            let tree = driver.tree.lock().unwrap();
            assert_eq!(
                tree.pending_mail("root"),
                [] as [crate::tree::MailRecord; 0]
            );
            assert_eq!(tree.retained_wait_receipts("root"), ["pi-await"]);
        }
        // A rejoined turn: the tree reopens from the store with the receipt
        // retained and nothing pending, and the same call is planned from
        // scratch, as `jobs_wait` does.
        let rejoined = driver_on(reload(&store, "tree1"), Arc::clone(&store));
        let tool = jobs_tool(rejoined, TopologyKind::JobsWait);
        let JobsWaitPlan::Await(again) = tool.jobs_wait_plan("pi-await", &request).unwrap() else {
            panic!("a call the ledger holds as an await replays as the await");
        };
        let replay = tool
            .jobs_wait_with(
                "pi-await",
                &again,
                &request,
                None,
                &ToolCallerLineage::Direct,
            )
            .await
            .unwrap();
        assert_eq!(replay.joined_text(), settled.joined_text());
        // A new call with nothing pending is the process tool's bare await.
        assert_eq!(
            tool.jobs_wait_plan("pi-next", &request).unwrap(),
            JobsWaitPlan::Processes(Vec::new())
        );
    }

    fn child_reports(driver: &TreeDriver) {
        let mut tree = driver.tree.lock().unwrap();
        tree.send_agent_message(
            "c1",
            &SendAgentMessageRequest {
                agent_id: "root".into(),
                message: "halfway there".into(),
                purpose: None,
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed("c1").unwrap();
    }

    /// A same-call replay of a bare pi `wait` that ran the
    /// tree's await replays its receipt (spec 03 §4.4) even though the first
    /// run delivered the only pending child, which would plan a new bare
    /// call onto the process path.
    #[tokio::test]
    async fn bare_pi_wait_same_call_replay_keeps_the_await() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tool = jobs_tool(driver.clone(), TopologyKind::JobsWait);
        child_reports(&driver);
        let request = jobs_wait(None, Some(5_000));
        let JobsWaitPlan::Await(awaited) = tool.jobs_wait_plan("pi-bare", &request).unwrap() else {
            panic!("a pending child makes a bare wait the tree's await");
        };
        let first = tool
            .jobs_wait_with(
                "pi-bare",
                &awaited,
                &request,
                None,
                &ToolCallerLineage::Direct,
            )
            .await
            .unwrap();
        assert_eq!(
            driver.tree.lock().unwrap().retained_wait_receipts("root"),
            ["pi-bare"]
        );
        let planned = tool.jobs_wait_plan("pi-bare", &request).unwrap();
        let JobsWaitPlan::Await(again) = planned else {
            panic!(
                "same-call replay of bare wait `pi-bare` re-planned to {planned:?}; its retained \
                 receipt {} is lost",
                first.joined_text()
            );
        };
        let replay = tool
            .jobs_wait_with(
                "pi-bare",
                &again,
                &request,
                None,
                &ToolCallerLineage::Direct,
            )
            .await
            .unwrap();
        assert_eq!(replay.joined_text(), first.joined_text());
    }

    /// A re-run cell repeats its nested call ids: a bare pi `wait` a cell ran
    /// as the tree's await replays its raw receipt under the same nested id.
    #[tokio::test]
    async fn bare_pi_wait_in_a_rerun_cell_replays_its_raw_receipt() {
        use std::str::FromStr;
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tool = jobs_tool(driver.clone(), TopologyKind::JobsWait);
        child_reports(&driver);
        let cell = ToolCallerLineage::local_programmatic(
            41,
            7,
            "cell-1",
            1,
            plakous::model::CallId::from_str(&uuid::Uuid::from_u128(9).to_string()).unwrap(),
            1,
        )
        .unwrap();
        let nested = uuid::Uuid::from_u128(10).to_string();
        let request = jobs_wait(None, Some(5_000));
        let mut shown = Vec::new();
        for _run in 0..2 {
            let JobsWaitPlan::Await(awaited) = tool.jobs_wait_plan(&nested, &request).unwrap()
            else {
                panic!("the cell's bare wait is the tree's await, then its replay");
            };
            let outcome = tool
                .jobs_wait_with(&nested, &awaited, &request, None, &cell)
                .await
                .unwrap();
            shown.push(outcome.joined_text());
        }
        assert_eq!(shown[0], shown[1]);
        let raw: Value = serde_json::from_str(&shown[0]).unwrap();
        assert!(raw.get("result").is_none(), "{raw}");
        assert_eq!(raw["mail"][0]["body"], "halfway there", "{raw}");
    }

    /// Pi's `stop` naming a child interrupts it under the interrupt receipt
    /// and never closes it.
    #[tokio::test]
    async fn pi_stop_on_a_child_interrupts_and_never_closes() {
        let root = tempfile::tempdir().unwrap();
        let (store, mut tree) = persist_fixture(&root);
        tree.spawn_agent("root", &spawn_request(), None).unwrap();
        tree.set_active_turn("c1", true).unwrap();
        let driver = driver_on(tree, Arc::clone(&store));
        let tool = jobs_tool(driver.clone(), TopologyKind::JobsStop);
        let request: JobsStopRequest = serde_json::from_value(json!({"id": "c1"})).unwrap();
        assert!(task_process_number(&request.id).is_none());
        let outcome = tool
            .task_kill_child(
                "pi-stop".into(),
                &TaskKillRequest {
                    task_id: request.id,
                },
                &ToolCallerLineage::Direct,
            )
            .await
            .unwrap();
        let inner = enveloped(&outcome);
        assert_eq!(inner["status"], "interrupted");
        let tree = driver.tree.lock().unwrap();
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
    }

    /// A send that heard `TurnEnded` while the interrupted turn's waiter was
    /// registered, whose waiter then ends without releasing the cancelled
    /// turn (a panicked or aborted waiter), fails closed at its next look
    /// instead of waiting out the budget for a release nobody will make
    /// (spec 03 section 9).
    #[tokio::test(start_paused = true)]
    async fn a_send_whose_turn_waiter_left_without_releasing_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        let deliverer = Arc::new(StubDeliverer::turn_ended());
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let started = tokio::time::Instant::now();
        let pending = tokio::spawn({
            let tool = Arc::clone(&tool);
            async move {
                tool.send_with(
                    "op-unwatched".into(),
                    &send_request("c1", "use the v2 schema", None),
                )
                .await
                .unwrap()
            }
        });
        until_calls(&deliverer, 1).await;
        // The waiter ends without releasing the turn.
        tree.lock().unwrap().leave_turn_waiter("c1");
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(300), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the send waited {:?} for a release no waiter will make",
            started.elapsed()
        );
        assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
        assert_eq!(
            outcome_json(&outcome)["code"],
            json!(TopologyErrorCode::OperationRecoveryRequired)
        );
        assert_eq!(deliverer.calls.lock().unwrap().len(), 1);
        let tree = tree.lock().unwrap();
        assert_eq!(
            tree.node("c1").unwrap().status,
            NodeStatus::RecoveryRequired
        );
    }

    /// A send waiting behind another send's resume Start keeps waiting when
    /// an interrupt lands while that Start is being admitted. The product
    /// Start marks the turn active (which makes the node `running`) before
    /// admission and binds the turn and registers its waiter only after it;
    /// an interrupt in between leaves the node `interrupted` with an active
    /// but unbound turn and no waiter yet. That is not a bound turn nobody
    /// will release, so the waiting send does not fail the child closed.
    #[tokio::test(start_paused = true)]
    async fn an_interrupt_during_a_resume_start_admission_is_not_unwatched() {
        let root = tempfile::tempdir().unwrap();
        let (store, tree) = persist_fixture(&root);
        let driver = driver_on(tree, Arc::clone(&store));
        let tree = Arc::clone(&driver.tree);
        interrupted_unwinding_child(&driver).await;
        let stub = StubDeliverer::ok().binding(&tree);
        for _ in 0..2 {
            stub.first
                .lock()
                .unwrap()
                .push_back(Err(MailDeliveryFailure::TurnEnded));
        }
        // The resume Start is held at admission.
        let (stub, gate) = stub.holding_from(2);
        let deliverer = Arc::new(stub);
        let tool = Arc::new(send_tool(
            driver.clone(),
            Some(Arc::clone(&deliverer) as Arc<dyn MailDeliverer>),
        ));
        let send = |operation: &'static str, text: &'static str| {
            let tool = Arc::clone(&tool);
            tokio::spawn(async move {
                tool.send_with(operation.into(), &send_request("c1", text, None))
                    .await
                    .unwrap()
            })
        };
        let first = send("op-resume", "use the v2 schema");
        until_calls(&deliverer, 1).await;
        let second = send("op-rider", "keep the old field too");
        until_calls(&deliverer, 2).await;
        release_unwound_turn(&tree);
        until_calls(&deliverer, 3).await;
        {
            // What the product Start does before admission, then an
            // interrupt before its bind.
            let mut tree = tree.lock().unwrap();
            tree.set_active_turn("c1", true).unwrap();
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Running);
            tree.interrupt_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: "c1".into(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap();
            assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
        }
        // The interrupt notified the waiting send; let it look, past a tick
        // of virtual time.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let (status, fault) = {
            let tree = tree.lock().unwrap();
            (
                tree.node("c1").unwrap().status,
                tree.fault_reason("c1").map(str::to_owned),
            )
        };
        // One send holds the resume Start; the other still waits for it.
        assert!(
            !first.is_finished() && !second.is_finished(),
            "the waiting send stopped waiting"
        );
        first.abort();
        second.abort();
        gate.add_permits(4);
        assert_ne!(
            status,
            NodeStatus::RecoveryRequired,
            "a resume Start still being admitted was read as an unwatched turn: {fault:?}"
        );
    }
}
