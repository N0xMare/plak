//! Recovering `recovery_required` children through one supported path.
//!
//! A child whose turn outcome, retirement, or startup is unresolved is settled
//! the way a relaunch would settle it: its session is reopened exactly, which
//! abandons what a lost or fenced owner left (Plakous §13.1), and the child is
//! then interrupted with cause `owner_lost`, its attempt kept (law L). Nothing
//! a child ran is replayed: no model call, no tool. A child whose startup
//! never published a session locator is settled by replaying the spawn's own
//! deterministic create, which either returns the session that create already
//! made or makes the one it was admitted to make.
//!
//! The same [`recover_child`] serves the operator's retry of one child (the
//! Agents list's `u`, or JSONL `invoke_tool` of `recover_agent`), which may
//! wait out a held lease, and each `recover_node` run over a tree (a custody
//! take, `/recover`, a release with a recovery need), which never waits on a
//! lease. Explicit parent acknowledgement in a followup uses the same exact
//! recovery without waiting on a lease, before its separate fresh Start.
//! `recover_agent` remains operator-only; no model receives a recovery tool.
//! Each child is held by one run at a time ([`claim_child`]). A refusal
//! may advance (or, for a replayed create, record) the node's session locator;
//! it never settles the attempt. The session part of an interrupted seal or
//! adopt is settled like any other; what the VCS step did is never decided
//! here (`recovery_vcs_ambiguous`), and `close_agent` ends such a node.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionOperationId,
    SessionRecoveryAction, SessionRuntimeBinding,
};

use crate::agent_tree::{RecoverAgentOutput, RecoveryNeed, RecoveryResolution, TopologyError};
use crate::agent_tree_protocol::{AttemptId, AttemptStatus, TopologyErrorCode};
use crate::topology_runtime::ProductTopologyRuntime;

use super::children::{
    CHILD_LEASE_POLL, CHILD_LEASE_WAIT, ChildBoundBind, ChildReopen, ChildReopenError,
    ChildReopenErrorKind, ChildReopenMode, child_session_runtime_binding, mutate_child,
    reopen_spawned_child, spawn_child_operation_id,
};
use super::{AttachedCodingSession, SpawnParent, occupancy_session_fence, spawn_parent_tree_id};
use crate::driver::reopen::wait_evidence;

/// A child session reopened for recovery, not yet released.
pub(super) struct ChildReopened {
    pub(super) session: Box<AttachedCodingSession>,
    /// Set when the head was a resumable working turn rather than one the
    /// reopen abandoned.
    pub(super) recovered: Option<plakous::agent::RecoveredTurnHandle>,
}

/// A port's refusal, and whether any retry could get past it.
pub(super) struct PortRefusal {
    pub(super) error: TopologyError,
    /// An incompatible session, or a head that needs effect evidence the host
    /// does not hold: retrying recovers nothing, so `close_agent` may end the
    /// node instead.
    pub(super) terminal: bool,
}

impl From<TopologyError> for PortRefusal {
    fn from(error: TopologyError) -> Self {
        Self {
            error,
            terminal: false,
        }
    }
}

type PortFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, PortRefusal>> + Send + 'a>>;

/// The session operations recovery needs, so the settlement rules below are
/// one implementation over the product's real reopen.
pub(super) trait ChildSessionPort: Send + Sync {
    /// Reopens the child at its head.
    fn reopen<'a>(
        &'a self,
        agent_id: &'a str,
        path: &'a str,
        locator: &'a str,
    ) -> PortFuture<'a, ChildReopened>;

    /// Replays the spawn's deterministic create.
    fn replay_create<'a>(
        &'a self,
        runtime: &'a ProductTopologyRuntime,
        agent_id: &'a str,
    ) -> PortFuture<'a, ReplayedCreate>;
}

/// What replaying a spawn's deterministic create found.
#[derive(Debug)]
pub(super) enum ReplayedCreate {
    /// The create's session, released at this locator.
    Session(String),
    /// No session exists for the create and the fresh create, or the binding
    /// it needs, was refused: the spawn never produced a session, so there is
    /// no session outcome to settle and the attempt settles `spawn_refused`.
    NeverCreated(String),
}

/// The product's port: the reopen every child path uses.
pub(super) struct ParentSessionPort {
    pub(super) parent: SpawnParent,
}

impl ChildSessionPort for ParentSessionPort {
    fn reopen<'a>(
        &'a self,
        agent_id: &'a str,
        path: &'a str,
        locator: &'a str,
    ) -> PortFuture<'a, ChildReopened> {
        Box::pin(async move {
            // Never waits on a held lease: a `recover_node` run skips such a
            // child and the operator's retry polls between tries.
            let mode = ChildReopenMode {
                lease_wait: Duration::ZERO,
                recovery: true,
                project: true,
            };
            let kind = recovery_operation_kind();
            match reopen_spawned_child(&self.parent, agent_id, path, locator, &kind, mode).await {
                Ok(ChildReopen::Attached { session, recovered }) => {
                    Ok(ChildReopened { session, recovered })
                }
                Ok(ChildReopen::RecoveryRequired(report)) => {
                    let code = recovery_required_code(
                        report
                            .obligations()
                            .iter()
                            .map(|obligation| obligation.action()),
                    );
                    let reason = if code == TopologyErrorCode::RecoveryEffectEvidenceRefused {
                        "the child's head needs external effect evidence this host does not hold"
                    } else {
                        "the child's recorded recovery scope cannot be settled by this build"
                    };
                    Err(PortRefusal {
                        error: refusal(code, &format!("{reason} ({:?})", report.phase())),
                        terminal: true,
                    })
                }
                Err(error) => Err(PortRefusal {
                    terminal: matches!(error.kind, ChildReopenErrorKind::Incompatible),
                    error: reopen_refusal(error),
                }),
            }
        })
    }

    fn replay_create<'a>(
        &'a self,
        runtime: &'a ProductTopologyRuntime,
        agent_id: &'a str,
    ) -> PortFuture<'a, ReplayedCreate> {
        Box::pin(async move {
            let (bind, path, fence) = runtime
                .read(|tree| {
                    tree.node(agent_id).map(|node| {
                        (
                            ChildBoundBind::of(node),
                            node.path.display().to_string(),
                            occupancy_session_fence(node.tier),
                        )
                    })
                })
                .map_err(|error| session_refusal(&error.message))?
                .map_err(|error| session_refusal(&error.message))?;
            let operation_id =
                spawn_child_operation_id(spawn_parent_tree_id(&self.parent), agent_id)
                    .map_err(|reason| session_refusal(&reason))?;
            let sessions = self
                .parent
                .state_roots()
                .child_runtime_sessions()
                .map_err(|_| session_refusal("plak runtime service is unavailable"))?;
            let accepted = sessions
                .accepted_create(&operation_id)
                .await
                .map_err(|error| {
                    reopen_refusal(ChildReopenError::from_sessions(
                        "the spawn's accepted create could not be inspected",
                        &error,
                    ))
                })?;
            let mut parent = self.parent.clone();
            let created_output_bound = if let Some(locator) = accepted {
                let roots = parent
                    .state_roots()
                    .clone()
                    .for_exact_reopen(&sessions, locator)
                    .await
                    .map_err(|error| {
                        reopen_refusal(ChildReopenError::from_sessions(
                            "child recorded construction inputs are unavailable",
                            &error,
                        ))
                    })?;
                match &mut parent {
                    SpawnParent::Native(spec) => spec.state_roots = roots,
                    SpawnParent::Api(spec) => spec.state_roots = roots,
                }
                Some(
                    sessions
                        .created_request_output_bound(locator)
                        .await
                        .map_err(|error| {
                            reopen_refusal(ChildReopenError::from_sessions(
                                "child created output bound is unavailable",
                                &error,
                            ))
                        })?,
                )
            } else {
                None
            };
            // The same binding the spawn's create used, so the replay names
            // the create the spawn admitted rather than a different one.
            let binding = match child_session_runtime_binding(
                &parent,
                agent_id,
                &path,
                bind.account.as_deref(),
                bind.tuple_id.as_deref(),
                bind.effort.as_deref(),
                accepted.is_some(),
                fence,
                created_output_bound,
                None,
            )
            .await
            {
                Ok(binding) => binding,
                Err(reason) if accepted.is_none() => {
                    return Ok(ReplayedCreate::NeverCreated(format!(
                        "child session could not be created: {reason}"
                    )));
                }
                Err(reason) => {
                    return Err(session_refusal(reason.message())
                        .with_failure(&reason.host_failure())
                        .into());
                }
            };
            Ok(replay_create_with(&sessions, operation_id, binding, accepted.is_none()).await?)
        })
    }
}

/// Replays one deterministic create and releases what it attached.
///
/// `fresh` says the exact accepted-create lookup found no session. A fresh
/// create refused for anything but storage or a held lease, after which the
/// lookup still finds no session, wrote nothing: it is `NeverCreated`.
pub(super) async fn replay_create_with(
    sessions: &Arc<AgentSessions>,
    operation_id: SessionOperationId,
    binding: SessionRuntimeBinding,
    fresh: bool,
) -> Result<ReplayedCreate, TopologyError> {
    // `create` resolves an already-accepted deterministic receipt first. If
    // none exists, this is still the ordinary create the spawn was admitted
    // to make, not blanket recovery work that may consume the settlement
    // reserve.
    match sessions
        .create(CreateSessionRequest::new(operation_id.clone(), binding))
        .await
    {
        Ok(CreateSessionOutcome::Created { session, .. }) => session
            .release()
            .await
            .map(|locator| ReplayedCreate::Session(locator.to_string()))
            .map_err(|error| {
                reopen_refusal(ChildReopenError::from_sessions(
                    "the replayed create did not release",
                    &error,
                ))
            }),
        Ok(_) => Err(session_refusal(
            "the replayed create requires operation recovery",
        )),
        Err(error) => {
            let refused =
                ChildReopenError::from_sessions("the spawn's create could not be replayed", &error);
            if fresh
                && refused.kind.settles_never_created()
                && matches!(sessions.accepted_create(&operation_id).await, Ok(None))
            {
                return Ok(ReplayedCreate::NeverCreated(format!(
                    "child session could not be created ({:?}): {error}",
                    error.kind()
                )));
            }
            Err(reopen_refusal(refused))
        }
    }
}

/// What one `recover_node` run over a tree's unresolved sessions did.
#[derive(Debug, Default)]
pub(crate) struct ChildRecoveryReport {
    /// Children this run settled: each is interrupted, its attempt kept.
    pub(crate) recovered: Vec<String>,
    /// Children still waiting, with the typed refusal that kept them waiting.
    /// A child whose session settled but whose seal or adopt is still
    /// ambiguous is here (`recovery_vcs_ambiguous`), not in `recovered`.
    pub(crate) refused: Vec<(String, TopologyError)>,
}

impl ChildRecoveryReport {
    /// The one operator line for this run, or `None` when it did nothing.
    #[must_use]
    pub(crate) fn note(&self) -> Option<String> {
        if self.recovered.is_empty() && self.refused.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if !self.recovered.is_empty() {
            parts.push(format!(
                "recovered {} child agent(s) ({}) as interrupted, attempt kept; followup \
                 continues and close_agent ends each",
                self.recovered.len(),
                self.recovered.join(", ")
            ));
        }
        if !self.refused.is_empty() {
            parts.push(format!(
                "{} child agent(s) still need recovery ({}); retry recovery for each obligation; root recovery only selects unresolved child sessions, not an already-settled child's pending tree publication; close a node only when its reported remedy is closure",
                self.refused.len(),
                self.refused
                    .iter()
                    .map(|(id, error)| format!("{id}: {}", code_name(error.code)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Some(format!("plak: {}", parts.join("; ")))
    }
}

fn code_name(code: TopologyErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{code:?}"))
}

/// A reopen operation id is a workflow identity the session store may keep a
/// receipt for, so each recovery reopen names its own: a retry after a refused
/// reopen must not be answered with the refused one.
pub(super) fn recovery_operation_kind() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    format!("recover-{millis}-{nonce}")
}

fn refusal(code: TopologyErrorCode, reason: &str) -> TopologyError {
    TopologyError::new(code, format!("{reason}; the attempt is not settled"))
}

fn session_refusal(reason: &str) -> TopologyError {
    refusal(TopologyErrorCode::RecoverySessionUnavailable, reason)
}

/// The one mapping from a typed reopen refusal to the recovery codes.
pub(super) fn reopen_refusal(error: ChildReopenError) -> TopologyError {
    let code = match error.kind {
        ChildReopenErrorKind::LeaseHeld { .. } => TopologyErrorCode::RecoveryLeaseHeld,
        ChildReopenErrorKind::StorageUnavailable => TopologyErrorCode::RecoveryStorageUnavailable,
        ChildReopenErrorKind::Incompatible | ChildReopenErrorKind::SessionUnavailable => {
            TopologyErrorCode::RecoverySessionUnavailable
        }
    };
    let mut result = refusal(code, &error.message);
    result.advice = error.advice;
    result.remedy = error.remedy;
    result
}

/// [`wait_evidence`] read from the committed history of a turn's
/// terminal checkpoint instead of the store: a receipt counts as committed
/// when that history holds its call and result.
pub(crate) fn wait_evidence_in(
    terminal: &plakous::agent::TurnResult,
    ids: Vec<String>,
) -> Vec<(String, bool)> {
    use plakous::model::{ContentBlock, ConversationItem, HistorySegment};
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for block in terminal
        .conversation()
        .history()
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
    {
        match block {
            ContentBlock::ToolCall(call) => calls.push(call.call_id()),
            ContentBlock::ToolResult(result) => results.push(result.call_id()),
            _ => {}
        }
    }
    ids.into_iter()
        .map(|id| {
            let committed = uuid::Uuid::parse_str(&id)
                .ok()
                .and_then(|uuid| plakous::model::CallId::try_from(uuid).ok())
                .is_some_and(|call_id| calls.contains(&call_id) && results.contains(&call_id));
            (id, committed)
        })
        .collect()
}

/// What a turn's terminal checkpoint did with its tool exchange, which
/// decides how the caller's await receipts settle when that terminal is
/// published (spec 03 section 4.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalExchange {
    /// Completed, cancelled, or failed and closed by the driver (spec 01
    /// section 3.5): canonical history holds every result the turn's awaits
    /// returned (a nested one inside its cell's result), so they retire.
    Closed,
    /// An `Internal` failure, which the driver does not close: each receipt
    /// settles by whether committed history holds its call.
    Unclosed,
    /// `RecoveryRequired`, or no authoritative terminal at all: the receipts
    /// wait for the settlement of the next open, recovery or admission.
    Unresolved,
}

impl TerminalExchange {
    pub(crate) fn of(result: &plakous::agent::TurnResult) -> Self {
        use plakous::agent::{AgentErrorKind, TurnResultOutcome};
        match result.outcome() {
            TurnResultOutcome::Completed(_) | TurnResultOutcome::Cancelled(_) => Self::Closed,
            TurnResultOutcome::Failed(error) if error.kind() == AgentErrorKind::Internal => {
                Self::Unclosed
            }
            TurnResultOutcome::Failed(_) => Self::Closed,
            TurnResultOutcome::RecoveryRequired(_) => Self::Unresolved,
        }
    }
}

/// A run of one session's feed past the tree's cursor, with the
/// `feed_terminal` reads the tree projector needs for it (spec 03 section
/// 14.3). Read without a lease; applied under the tree's lock.
use crate::agent_tree::TreeFeed;

async fn read_tree_feed_pages<F, Fut, E>(
    session_id: plakous::model::SessionId,
    cursor: u64,
    mut read_page: F,
) -> Result<TreeFeed, E>
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = Result<plakous::sessions::feed::FeedPage, E>>,
    E: From<&'static str>,
{
    use plakous::sessions::feed::FeedPage;
    let mut feed = TreeFeed {
        session_id: session_id.to_string(),
        retained_from: 1,
        below_retained: None,
        heads: Vec::new(),
        head_turns: std::collections::BTreeMap::new(),
        records: Vec::new(),
        reads: crate::agent_tree::TreeFeedReads::default(),
    };
    let mut applied = cursor;
    let mut tail = None;
    while (feed.records.len() as u64) < plakous::sessions::feed::MAX_FEED_RECORDS_PER_SESSION {
        let page = read_page(applied).await?;
        match page {
            FeedPage::Records {
                records,
                retained_from,
                tail: page_tail,
            } => {
                let target = *tail.get_or_insert(page_tail.get());
                feed.retained_from = retained_from.get();
                let Some(last) = records.last() else {
                    break;
                };
                let last_seq = last.header().seq.get();
                if last_seq <= applied {
                    return Err(E::from("the tree projector feed read made no progress"));
                }
                applied = last_seq;
                feed.records.extend(records);
                if applied >= target {
                    break;
                }
            }
            FeedPage::BehindRetention {
                retained_from,
                heads,
                tail: page_tail,
            } => {
                tail.get_or_insert(page_tail.get());
                feed.retained_from = retained_from.get();
                let below = retained_from.get().saturating_sub(1);
                if below <= applied {
                    break;
                }
                feed.below_retained = Some(below);
                feed.heads = heads;
                applied = below;
            }
            _ => break,
        }
    }
    if tail.is_some_and(|target| applied < target) {
        return Err(E::from(
            "the tree projector feed read did not reach its initial tail",
        ));
    }
    Ok(feed)
}

pub(crate) const TREE_FEED_READ_FAILED: &str = "the tree projector could not read the feed";

/// Reads `session_id`'s feed after `cursor` through its initial tail and the abandonment
/// and failure terminals the tree projector reads (spec 03 section 14.3): the entry the
/// host loop's open and live tick use to run the tree projector for the
/// root or a child. A cursor behind the feed's retention resumes at the
/// current tail; older command identities settle only by exact resubmission.
///
/// # Errors
///
/// Returns a refused feed read.
pub(crate) async fn read_tree_feed(
    sessions: &AgentSessions,
    session_id: plakous::model::SessionId,
    cursor: u64,
) -> Result<TreeFeed, crate::error::ReadFailure> {
    use crate::error::ReadFailure;
    use plakous::sessions::feed::{
        FeedCursor, FeedPageRequest, FeedSeq, FeedTerminalOutcome, FeedTerminalRequest,
    };
    let mut feed = read_tree_feed_pages(session_id, cursor, |after| async move {
        #[cfg(test)]
        if let Some(failure) = crate::error::read_faults::take(TREE_FEED_READ_FAILED) {
            return Err(failure);
        }
        sessions
            .feed_page(FeedPageRequest::after(FeedCursor::new(
                session_id,
                FeedSeq::new(after),
            )))
            .await
            .map_err(|error| ReadFailure::store(TREE_FEED_READ_FAILED, &error))
    })
    .await?;
    for head in &feed.heads {
        let turn = sessions.head_running_turn(*head).await.map_err(|error| {
            ReadFailure::store(
                "the tree projector could not read a head's running turn",
                &error,
            )
        })?;
        feed.head_turns.insert(*head, turn);
    }
    let needed: Vec<_> = crate::agent_tree::terminal_reads_needed(&feed.records)
        .into_iter().filter_map(|(index, turn_id)| {
            let record = &feed.records[index];
            let checkpoint = crate::sessions::projector::record_checkpoint(record)?;
            let closure = !record.facts().iter().any(|fact| {
                matches!(fact, plakous::sessions::feed::FeedFact::TurnTerminal { turn_id: ended, .. } if *ended == turn_id)
            });
            Some((FeedTerminalRequest {
                session: session_id, branch: record.header().branch_id,
                checkpoint, turn_id,
            }, closure))
        }).collect();
    let outcomes = sessions
        .feed_terminals(needed.iter().map(|(request, _)| *request).collect())
        .await;
    for ((request, closure_candidate), outcome) in needed.into_iter().zip(outcomes) {
        let turn_id = request.turn_id;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error)
                if closure_candidate
                    && error.kind() == plakous::sessions::AgentSessionsErrorKind::InvalidInput =>
            {
                continue;
            }
            Err(error) => {
                return Err(ReadFailure::store(
                    "the tree projector could not read a terminal",
                    &error,
                ));
            }
        };
        let FeedTerminalOutcome::Terminal(terminal) = outcome else {
            continue;
        };
        if let plakous::agent::TurnResultOutcome::Failed(error) = terminal.result().outcome() {
            let context = error
                .model_error()
                .and_then(plakous::model::ModelError::context);
            feed.reads.insert_failure(
                turn_id.to_string(),
                crate::agent_tree::FailureRead {
                    kind: error.kind(),
                    retryability: error
                        .model_error()
                        .and_then(plakous::model::ModelError::context)
                        .map(plakous::model::ModelErrorContext::retryability),
                    phase: context.map(plakous::model::ModelErrorContext::phase),
                    http_status: context.and_then(plakous::model::ModelErrorContext::http_status),
                    provider_code: context
                        .and_then(plakous::model::ModelErrorContext::provider_code)
                        .map(str::to_owned),
                    message: error.message().to_owned(),
                },
            );
        }
        let Some(report) = terminal.abandoned_turn() else {
            continue;
        };
        feed.reads.insert_terminal(
            turn_id.to_string(),
            crate::agent_tree::TerminalRead {
                message: crate::runtime::events::abandoned_turn_operator_line(report).to_owned(),
                abandoned: Some(crate::agent_tree::AbandonedChildTurn {
                    operations: report.operations().to_vec(),
                    claim: report.claim(),
                }),
            },
        );
    }
    Ok(feed)
}

/// Who in this process already owns a child [`claim_child`] refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChildOwner {
    /// A live turn, startup, or retirement, or its turn waiter's recycle.
    Process,
    /// Another `recover_node` run.
    Recovery,
}

/// Holds `agent_id` for one recovery. The live table is read before the
/// claim: a turn waiter marks its recycle before the live entry goes, so a
/// child seen without a live entry is either recycling (and refused here)
/// or owned by nobody in this process.
pub(super) async fn claim_child(
    runtime: &Arc<ProductTopologyRuntime>,
    agent_id: &str,
) -> Result<crate::topology_runtime::RecoveryClaim, ChildOwner> {
    if runtime.live.lock().await.contains_key(agent_id) {
        return Err(ChildOwner::Process);
    }
    runtime
        .claim_recovery(agent_id)
        .map_err(|refusal| match refusal {
            crate::topology_runtime::RecoveryClaimRefusal::Recycling => ChildOwner::Process,
            crate::topology_runtime::RecoveryClaimRefusal::Recovering => ChildOwner::Recovery,
        })
}

/// Settles one child, or refuses without settling.
///
/// The child is held for the whole call ([`claim_child`]), so no other
/// recovery or fenced-turn recycle of it runs concurrently.
pub(super) async fn recover_child(
    runtime: &Arc<ProductTopologyRuntime>,
    port: &dyn ChildSessionPort,
    agent_id: &str,
    expected_attempt_id: Option<&AttemptId>,
) -> Result<RecoverAgentOutput, TopologyError> {
    // A child this process still owns (a live turn, a startup, a retirement,
    // a turn waiter's recycle, or another recovery) settles on its own;
    // recovery never reopens under it and never waits for it.
    let _claim = claim_child(runtime, agent_id)
        .await
        .map_err(|_| owner_live())?;
    recover_claimed_child(runtime, port, agent_id, expected_attempt_id).await
}

fn owner_live() -> TopologyError {
    refusal(
        TopologyErrorCode::RecoveryOwnerLive,
        "this process still owns the child's turn, startup, retirement, or recovery; it \
         settles on its own; retry the acknowledged followup or operator recovery after that",
    )
}

/// [`recover_child`] for a child its caller holds.
#[allow(
    clippy::too_many_lines,
    reason = "Child recovery reconciles tree ownership and canonical terminal state before reopening or retiring the existing attempt."
)]
async fn recover_claimed_child(
    runtime: &ProductTopologyRuntime,
    port: &dyn ChildSessionPort,
    agent_id: &str,
    expected_attempt_id: Option<&AttemptId>,
) -> Result<RecoverAgentOutput, TopologyError> {
    if !crate::agent::placement::ready(runtime.driver(), agent_id) {
        return Err(session_refusal(
            "child placement is unavailable; recovery is deferred",
        ));
    }
    let rebuilding = runtime
        .read(|tree| {
            tree.node(agent_id)
                .is_ok_and(|node| node.fold_order.is_none())
        })
        .map_err(|error| session_refusal(&error.message))?;
    let need = runtime
        .read(|tree| {
            if let Some(expected) = expected_attempt_id {
                let current = tree.node(agent_id)?.attempt_id.as_ref();
                if current != Some(expected) {
                    return Err(TopologyError::new(
                        TopologyErrorCode::AttemptMismatch,
                        format!("expected_attempt_id {expected} is not the current attempt"),
                    ));
                }
            }
            tree.recovery_need(agent_id)
        })
        .map_err(|error| session_refusal(&error.message))??;
    let (attempt_id, locator, path) = match need {
        RecoveryNeed::NotRequired => {
            // A previous effect completion may be recorded only in memory
            // after a refused publish. A no-op must not claim it is durable.
            let (id, expected) = (agent_id.to_owned(), expected_attempt_id.cloned());
            return mutate_and_publish(runtime, agent_id, move |tree| {
                tree.recover_receipt(&id, expected.as_ref(), false)
            })
            .await;
        }
        RecoveryNeed::Workspace { code, reason } => {
            return Err(TopologyError::new(code, reason));
        }
        RecoveryNeed::Session {
            attempt_id,
            locator,
            path,
        } => (attempt_id, locator, path),
    };
    if rebuilding && locator.is_none() {
        let (id, attempt) = (agent_id.to_owned(), attempt_id.clone());
        return mutate_and_publish(runtime, agent_id, move |tree| {
            tree.rebuild_sessionless_fold(&id)?;
            tree.recover_receipt(&id, Some(&attempt), true)
        })
        .await;
    }
    let refused = async |refusal: PortRefusal| {
        if refusal.terminal {
            let code = code_name(refusal.error.code);
            let (id, attempt, message) = (
                agent_id.to_owned(),
                attempt_id.clone(),
                refusal.error.message.clone(),
            );
            mutate_child(runtime.driver(), agent_id, move |tree| {
                tree.record_terminal_recovery_refusal(&id, &attempt, &code, &message);
            })
            .await;
        }
        refusal.error
    };
    let locator = if let Some(locator) = locator {
        locator
    } else {
        let replayed = match port.replay_create(runtime, agent_id).await {
            Ok(replayed) => replayed,
            Err(refusal) => return Err(refused(refusal).await),
        };
        match replayed {
            ReplayedCreate::Session(created) => {
                let (id, session) = (agent_id.to_owned(), created.clone());
                mutate_and_publish(runtime, agent_id, move |tree| {
                    tree.record_child_session(&id, &session)
                })
                .await?;
                created
            }
            ReplayedCreate::NeverCreated(message) => {
                let recorded = runtime
                    .record_then_apply(agent_id, {
                        let (agent_id, attempt_id) = (agent_id.to_owned(), attempt_id.clone());
                        move |tree| {
                            let (agent_id, attempt_id) = (agent_id.as_str(), &attempt_id);
                            tree.record_host_recovery(
                                agent_id,
                                attempt_id,
                                RecoveryResolution::NeverCreated { message },
                            )
                        }
                    })
                    .await
                    .map_err(|error| session_refusal(&error))?;
                return runtime
                    .read(|tree| tree.recover_receipt(agent_id, Some(&attempt_id), recorded))
                    .map_err(|error| session_refusal(&error.message))?;
            }
        }
    };
    let reopened = match port.reopen(agent_id, &path, &locator).await {
        Ok(reopened) => reopened,
        Err(refusal) => return Err(refused(refusal).await),
    };
    let retained = runtime
        .read(|tree| tree.retained_wait_receipts(agent_id))
        .map_err(|error| session_refusal(&error.message))?;
    let cursor = runtime
        .read(|tree| tree.feed_read_cursor(&reopened.session.locator().session_id().to_string()))
        .map_err(|error| session_refusal(&error.message))?;
    let sessions = Arc::clone(&reopened.session.sessions);
    let (resolution, feed) = release_reopened(agent_id, reopened, retained, cursor).await?;
    let (projection, _feed) =
        super::feed_projection::retry_tree_feed(&sessions, feed, |feed| async move {
            let refusal = super::feed_projection::RefusalSlot::default();
            let (seen, id) = (refusal.clone(), agent_id.to_owned());
            mutate_and_publish(runtime, agent_id, move |tree| {
                feed.apply_checked(tree, &id)
                    .inspect_err(|error| seen.record(error))
                    .map_err(TopologyError::from)
            })
            .await
            .map_err(|error| {
                refusal
                    .take()
                    .unwrap_or(crate::agent_tree::TreeProjectorError::Tree(error))
            })
        })
        .await?;
    // No turn of the lost owner is left to lose on an attempt that already
    // ended or submitted: only its session's release was unresolved, and
    // that settles as before, a submitted result completing the attempt.
    let ended = runtime
        .read(|tree| {
            tree.attempt(agent_id).is_some_and(|attempt| {
                attempt.output_json.is_some()
                    || (attempt.status.is_stopped()
                        && attempt.status != AttemptStatus::RecoveryRequired)
            })
        })
        .map_err(|error| session_refusal(&error.message))?;
    if ended && rebuilding {
        // The rebuild already settled the attempt from its feed.
        return runtime
            .read(|tree| tree.recover_receipt(agent_id, Some(&attempt_id), true))
            .map_err(|error| session_refusal(&error.message))?;
    }
    if ended {
        let recorded = runtime
            .record_then_apply(agent_id, {
                let (agent_id, attempt_id) = (agent_id.to_owned(), attempt_id.clone());
                move |tree| tree.record_host_recovery(&agent_id, &attempt_id, resolution)
            })
            .await
            .map_err(|error| session_refusal(&error))?;
        return runtime
            .read(|tree| tree.recover_receipt(agent_id, Some(&attempt_id), recorded))
            .map_err(|error| session_refusal(&error.message))?;
    }
    // Law L: the projection already interrupted a child whose abandoned
    // or cancelled turn it folded; one that reopened quiescent with no turn
    // to fold is interrupted here. Either way the attempt is kept. A child
    // whose views were rebuilt is judged on the rebuilt rows the same way.
    if !projection.abandoned.iter().any(|id| id == agent_id) {
        let (id, attempt) = (agent_id.to_owned(), attempt_id.clone());
        mutate_and_publish(runtime, agent_id, move |tree| {
            tree.lose_owner(&id, Some(attempt));
            Ok(())
        })
        .await?;
    }
    let recovered = true;
    runtime
        .read(|tree| tree.recover_receipt(agent_id, Some(&attempt_id), recovered))
        .map_err(|error| session_refusal(&error.message))?
}

/// Releases the reopened session and says what the reopen settled. A
/// resumable working turn is cancelled rather than rejoined: the attempt is
/// already ended, and cancelling publishes that turn's terminal exactly once.
/// An abandonment is remembered before the release, so a refusal after it
/// does not turn a retry into `recovered_without_turn`.
async fn release_reopened(
    agent_id: &str,
    reopened: ChildReopened,
    retained_waits: Vec<String>,
    cursor: u64,
) -> Result<(RecoveryResolution, TreeFeed), TopologyError> {
    let ChildReopened { session, recovered } = reopened;
    if let Some(turn) = recovered {
        let message = format!(
            "plak: {agent_id} recovered a resumable turn and cancelled it; nothing from it is \
             replayed"
        );
        let cancellation = crate::interrupt::settle_cancel(
            turn.control().cancel(),
            plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT,
        )
        .await;
        // Read once no turn is live and before the release, at the head the
        // cancellation (or, below, the abandonment) left.
        let evidence = wait_evidence(session.sessions(), session.locator(), retained_waits)
            .await
            .map_err(|error| session_refusal(&error))?;
        let mut feed = read_tree_feed(session.sessions(), session.locator().session_id(), cursor)
            .await
            .map_err(|failure| session_refusal(&failure.to_string()))?;
        feed.reads.insert_waits(session.locator(), evidence);
        session
            .release_opened_for_transfer()
            .await
            .map_err(|error| {
                session_refusal(&format!("the reopened child did not release: {error}"))
            })?;
        if let crate::interrupt::CancelSettlement::Unsettled(failure) = cancellation {
            return Err(session_refusal(&format!(
                "the reopened child's resumable turn did not cancel and release: {failure}"
            )));
        }
        return Ok((RecoveryResolution::TurnAbandoned { message }, feed));
    }
    let observed = session
        .agent()
        .and_then(plakous::agent::Agent::abandoned_turn)
        .map(|report| crate::runtime::events::abandoned_turn_operator_line(report).to_owned());
    let evidence = wait_evidence(session.sessions(), session.locator(), retained_waits)
        .await
        .map_err(|error| session_refusal(&error))?;
    let mut feed = read_tree_feed(session.sessions(), session.locator().session_id(), cursor)
        .await
        .map_err(|failure| session_refusal(&failure.to_string()))?;
    feed.reads.insert_waits(session.locator(), evidence);
    session
        .release_opened_for_transfer()
        .await
        .map_err(|error| {
            session_refusal(&format!("the reopened child did not release: {error}"))
        })?;
    let resolution = match observed {
        Some(message) => RecoveryResolution::TurnAbandoned { message },
        None => RecoveryResolution::RecoveredWithoutTurn {
            message: format!(
                "plak: {agent_id}'s session reopened quiescent and no abandonment evidence \
                     is available; the attempt left no result"
            ),
        },
    };
    Ok((resolution, feed))
}

/// One tree mutation, applied to a copy that is published first and swapped
/// in only once the store took it, so a refused publish leaves the tree as it
/// was and is reported as `recovery_storage_unavailable`. Effect completions
/// are recorded after that publication; their failures are returned too.
async fn mutate_and_publish<R: Send + 'static>(
    runtime: &ProductTopologyRuntime,
    agent_id: &str,
    work: impl FnOnce(&mut crate::agent_tree::ProductAgentTree) -> Result<R, TopologyError>
    + Send
    + 'static,
) -> Result<R, TopologyError> {
    let driver = &runtime.driver().for_agent(agent_id);
    let publish_refusal = |error: TopologyError| {
        refusal(
            TopologyErrorCode::RecoveryStorageUnavailable,
            &format!(
                "the recovered state could not be published to the tree store: {}",
                error.message
            ),
        )
    };
    let (result, effects) = driver
        .mutate({
            let driver = driver.clone();
            move |tree| -> Result<R, TopologyError> {
                let driver = &driver;
                if tree.revision != tree.stored_revision {
                    driver.save(tree).map_err(publish_refusal)?;
                }
                let mut work_refusal = None;
                driver
                    .fold_and_commit(tree, |tree| {
                        work(tree).inspect_err(|error| work_refusal = Some(error.clone()))
                    })
                    .map_err(|error| work_refusal.unwrap_or_else(|| publish_refusal(error)))
            }
        })
        .await
        .map_err(|error| session_refusal(&error.message))?;
    let result = result?;
    let settled = driver.settle(effects).await.map_err(|error| {
        let code = match error.code {
            TopologyErrorCode::OperationRecoveryRequired => {
                TopologyErrorCode::RecoveryStorageUnavailable
            }
            code => code,
        };
        TopologyError::new(
            code,
            format!(
                "{agent_id}'s recovery effects were recorded but settlement failed: {}",
                error.message,
            ),
        )
    })?;
    if let Some(error) = settled.seal_failures.into_iter().next() {
        return Err(error);
    }
    Ok(result)
}

/// Runs `recover_node` for every child whose session outcome is still
/// unresolved, descendants first: law L interrupts each one whose lost
/// turn the exact reopen closed, keeping its attempt. Reopen refusals are
/// reported without changing the attempt; effect failures retain their
/// recorded evidence.
pub(super) async fn recover_unresolved_children(
    runtime: &Arc<ProductTopologyRuntime>,
    port: &dyn ChildSessionPort,
) -> ChildRecoveryReport {
    let mut report = ChildRecoveryReport::default();
    let Ok(targets) = runtime.read(crate::agent_tree::ProductAgentTree::session_recovery_targets)
    else {
        return report;
    };
    for agent_id in targets {
        // A child another run holds is that run's to report.
        let claim = match claim_child(runtime, &agent_id).await {
            Ok(claim) => claim,
            Err(ChildOwner::Recovery) => continue,
            Err(ChildOwner::Process) => {
                report.refused.push((agent_id, owner_live()));
                continue;
            }
        };
        let outcome = recover_claimed_child(runtime, port, &agent_id, None).await;
        drop(claim);
        match outcome {
            Ok(output) if output.recovered => match output.workspace_recovery {
                Some(reason) => report.refused.push((
                    agent_id,
                    TopologyError::new(TopologyErrorCode::RecoveryVcsAmbiguous, reason),
                )),
                None => report.recovered.push(agent_id),
            },
            Ok(_) => {}
            Err(error) => report.refused.push((agent_id, error)),
        }
    }
    report
}

/// How long the operator's retry waits out a held lease.
fn operator_recover_lease_wait() -> Duration {
    #[cfg(test)]
    if let Some(wait) = test_lease_wait::get() {
        return wait;
    }
    CHILD_LEASE_WAIT
}

/// A shorter operator-retry lease wait for tests that hold a lease on
/// purpose. Each nextest test is its own process.
#[cfg(test)]
pub(crate) mod test_lease_wait {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static MILLIS: AtomicU64 = AtomicU64::new(u64::MAX);

    /// Sets the wait for the rest of this process.
    pub(crate) fn set(wait: Duration) {
        MILLIS.store(
            u64::try_from(wait.as_millis()).unwrap_or(u64::MAX - 1),
            Ordering::SeqCst,
        );
    }

    pub(super) fn get() -> Option<Duration> {
        match MILLIS.load(Ordering::SeqCst) {
            u64::MAX => None,
            millis => Some(Duration::from_millis(millis)),
        }
    }
}

/// The operator's retry port over the attached hub.
pub(super) struct ProductChildRecoverer {
    pub(super) parent: SpawnParent,
    pub(super) runtime: Arc<ProductTopologyRuntime>,
}

impl ProductChildRecoverer {
    pub(super) fn recover(
        &self,
        agent_id: String,
        expected_attempt_id: Option<AttemptId>,
    ) -> Pin<Box<dyn Future<Output = Result<RecoverAgentOutput, TopologyError>> + Send + '_>> {
        Box::pin(async move {
            // Each try holds the child without waiting on the lease, and
            // releases it between tries, so a held lease never stalls a
            // recycle or another recovery of it.
            let deadline = tokio::time::Instant::now() + operator_recover_lease_wait();
            let port = ParentSessionPort {
                parent: self.parent.clone(),
            };
            loop {
                let outcome = recover_child(
                    &self.runtime,
                    &port,
                    &agent_id,
                    expected_attempt_id.as_ref(),
                )
                .await;
                let lease_held = matches!(
                    &outcome,
                    Err(error) if error.code == TopologyErrorCode::RecoveryLeaseHeld
                );
                let now = tokio::time::Instant::now();
                if !lease_held || now >= deadline {
                    return outcome;
                }
                tokio::time::sleep(CHILD_LEASE_POLL.min(deadline - now)).await;
            }
        })
    }
}

/// Why an operator `u` must not call [`ProductChildRecoverer::recover`].
///
/// Eligibility is [`operator_child_recover_eligible`]: failed-idle and
/// `recovery_required` children, on a live, interrupted or terminal-cleanup
/// tree, with no model-grant check. The revision fence stays here.
pub(crate) fn refuse_operator_child_recover(
    revision: u64,
    expected_revision: u64,
    state: crate::agent_tree_protocol::TreeAdmissionState,
    is_root: bool,
    status: crate::agent_tree_protocol::NodeStatus,
    attempt_id: Option<&str>,
    expected_attempt_id: &str,
) -> Option<&'static str> {
    use crate::agent_tree_protocol::TreeAdmissionState;
    if revision != expected_revision {
        return Some("stale tree revision");
    }
    if state == TreeAdmissionState::Closed {
        return Some("tree is closed");
    }
    if is_root {
        return Some("the root is not recovered from the agent list");
    }
    if !plakous_host_protocol::topology::operator_child_recover_eligible(
        state.as_str(),
        status.as_str(),
        is_root,
        attempt_id.is_some(),
    ) {
        return Some("this agent is not waiting on recovery");
    }
    match attempt_id {
        Some(id) if id == expected_attempt_id => None,
        Some(_) => Some("attempt mismatch"),
        None => Some("this agent has no attempt to recover"),
    }
}

/// One operator child recover from the Agents list: the operator's retry
/// port, not a `recover_node` run over the tree. The tree mutex is not held
/// across the lease wait.
pub(crate) async fn operator_child_recover(
    runtime: &Arc<ProductTopologyRuntime>,
    expected_revision: u64,
    agent_id: &str,
    expected_attempt_id: &str,
) -> Result<RecoverAgentOutput, String> {
    let parent = {
        let refusal = runtime
            .read(|tree| -> Result<Option<&'static str>, String> {
                let node = tree.node(agent_id).map_err(|error| error.message.clone())?;
                Ok(refuse_operator_child_recover(
                    tree.revision,
                    expected_revision,
                    tree.state,
                    node.parent_agent_id.is_none(),
                    node.presentation_status(tree.state),
                    node.attempt_id.as_ref().map(AttemptId::as_str),
                    expected_attempt_id,
                ))
            })
            .map_err(|error| error.message)??;
        if let Some(reason) = refusal {
            return Err(reason.to_owned());
        }
        super::build::rejoin_parent(runtime)
            .ok_or_else(|| "no parent session is attached".to_owned())?
    };
    let attempt = AttemptId::from_persisted(expected_attempt_id.to_owned());
    ProductChildRecoverer {
        parent,
        runtime: Arc::clone(runtime),
    }
    .recover(agent_id.to_owned(), Some(attempt))
    .await
    .map_err(|error| format!("{}: {}", code_name(error.code), error.message))
}

/// The JSONL operator's `invoke_tool` of `recover_agent`: the Agents list's
/// retry for one child, without the confirmation's revision fence. A child
/// that is not waiting on recovery answers its state with `recovered: false`,
/// so a repeat is idempotent.
pub(crate) async fn operator_recover_tool(
    runtime: &Arc<ProductTopologyRuntime>,
    arguments: &serde_json::Value,
) -> Result<RecoverAgentOutput, TopologyError> {
    let request: crate::agent_tree_protocol::AttemptScopedRequest =
        serde_json::from_value(arguments.clone()).map_err(|error| {
            TopologyError::new(TopologyErrorCode::SchemaMismatch, error.to_string())
        })?;
    runtime.read(|tree| tree.authorize_operator_recover(&request))??;
    let parent = super::build::rejoin_parent(runtime).ok_or_else(|| {
        TopologyError::new(
            TopologyErrorCode::RecoverySessionUnavailable,
            "this host cannot reopen child sessions; nothing changed",
        )
    })?;
    ProductChildRecoverer {
        parent,
        runtime: Arc::clone(runtime),
    }
    .recover(request.agent_id, request.expected_attempt_id)
    .await
}

/// One `recover_node` run over the tree's children (design section 6.3),
/// for a custody take, `/recover`, an operator retry, or a release with a
/// recovery need: the children owed a recovery first, then every child
/// whose session outcome is unresolved, each held for its own recovery. It
/// never waits on a lease. `None` when this tree has no attached hub or no
/// parent to reopen under.
pub(crate) async fn recover_tree(tree_id: &str) -> Option<ChildRecoveryReport> {
    let runtime = crate::topology_runtime::lookup(tree_id)?;
    let parent = super::build::rejoin_parent(&runtime)?;
    super::children::recover_owed_children(&parent, &runtime).await;
    let port = ParentSessionPort { parent };
    let report = recover_unresolved_children(&runtime, &port).await;
    // The continuation pass runs after every recovery the run made, so a
    // node resumed here is one its recover_node already settled.
    super::children::continue_after_loss(&port.parent, &runtime).await;
    Some(report)
}

/// What [`start_tree_recovery`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TreeRecoveryStart {
    /// A run started; it reports through `notify` when it finishes.
    /// `pending` is how many children it has to try.
    Started { pending: usize },
    /// No hub is attached for this tree, or its hub is shutting down.
    NoTree,
}

/// Starts one [`recover_tree`] run on the tree's tasks; the hub tracks it,
/// so a shutdown waits for (or aborts) it before releasing leases. Runs
/// may overlap: each child is held by one of them at a time, and a run
/// skips a child another one holds. `notify` receives the report when the
/// run finishes, `None` when it had nothing to run on.
pub(crate) fn start_tree_recovery(
    tree_id: String,
    notify: impl FnOnce(Option<ChildRecoveryReport>) + Send + 'static,
) -> TreeRecoveryStart {
    let Some(runtime) = crate::topology_runtime::lookup(&tree_id) else {
        return TreeRecoveryStart::NoTree;
    };
    let pending = runtime
        .read(|tree| tree.session_recovery_targets().len() + tree.recovery_owed_targets().len())
        .unwrap_or(0);
    let task = runtime.driver().tasks().spawn(async move {
        let report = recover_tree(&tree_id).await;
        if let Some(report) = report.as_ref().filter(|report| report.note().is_some()) {
            tracing::info!(
                target: "plak.topology",
                tree_id,
                recovered = ?report.recovered,
                refused = ?report.refused,
                "child recovery finished"
            );
        }
        notify(report);
    });
    if runtime.track_recovery(task) {
        TreeRecoveryStart::Started { pending }
    } else {
        TreeRecoveryStart::NoTree
    }
}

pub(super) async fn install_retirement_recovery(runtime: &ProductTopologyRuntime, tree_id: &str) {
    let tree_id = tree_id.to_owned();
    let first = runtime
        .set_retirement_recovery(Arc::new(move || {
            start_tree_recovery(tree_id.clone(), |_| {});
        }))
        .await;
    if !first {
        return;
    }
    let Some(owner) = crate::topology_runtime::lookup(
        &runtime
            .read(|tree| tree.tree_id.clone())
            .unwrap_or_default(),
    ) else {
        return;
    };
    crate::agent::placement::watch_debt(&owner);
}

#[cfg(test)]
mod operator_recover_tests {
    use super::refuse_operator_child_recover;

    #[test]
    fn operator_recover_refuses_a_stale_revision_a_mismatch_and_a_paused_tree() {
        use crate::agent_tree_protocol::{NodeStatus, TreeAdmissionState};
        let live = TreeAdmissionState::Live;
        let recover = NodeStatus::RecoveryRequired;
        assert_eq!(
            refuse_operator_child_recover(4, 3, live, false, recover, Some("a1"), "a1"),
            Some("stale tree revision")
        );
        assert_eq!(
            refuse_operator_child_recover(3, 3, live, false, recover, Some("a2"), "a1"),
            Some("attempt mismatch")
        );
        assert_eq!(
            refuse_operator_child_recover(
                3,
                3,
                TreeAdmissionState::Closed,
                false,
                recover,
                Some("a1"),
                "a1"
            ),
            Some("tree is closed")
        );
        assert_eq!(
            refuse_operator_child_recover(3, 3, live, true, recover, Some("a1"), "a1"),
            Some("the root is not recovered from the agent list")
        );
        assert_eq!(
            refuse_operator_child_recover(3, 3, live, false, NodeStatus::Running, Some("a1"), "a1"),
            Some("this agent is not waiting on recovery")
        );
        assert_eq!(
            refuse_operator_child_recover(3, 3, live, false, recover, Some("a1"), "a1"),
            None
        );
        assert_eq!(
            refuse_operator_child_recover(3, 3, live, false, NodeStatus::Failed, Some("a1"), "a1"),
            None,
            "a failed-idle child is eligible"
        );
    }
}

#[cfg(test)]
pub(super) mod tests;

#[cfg(all(test, unix))]
mod crash_tests;

fn recovery_required_code(
    actions: impl IntoIterator<Item = SessionRecoveryAction>,
) -> TopologyErrorCode {
    if actions.into_iter().any(|action| {
        matches!(
            action,
            SessionRecoveryAction::ProviderEvidenceRequired
                | SessionRecoveryAction::EffectEvidenceRequired
        )
    }) {
        TopologyErrorCode::RecoveryEffectEvidenceRefused
    } else {
        TopologyErrorCode::RecoverySessionUnavailable
    }
}
