//! Reopening and reattaching children after a restart.

use crate::driver::reopen::classify_opened;

use super::{
    AgentSessionsError, AgentSessionsErrorKind, Arc, AttachedCodingSession, ChildBoundBind,
    Duration, FromStr, LiveRelease, OpenSessionOutcome, OpenSessionRequest,
    RecoverEffectEvidenceOutcome, RecoverEffectEvidenceRequest, RecoveredTurnHandle,
    RecoveryRequiredReport, SessionEffectEvidence, SessionLocator, SessionOperationId,
    SessionRecoveryAction, SessionToolFence, SpawnParent, ToolExecutorId, ToolOutcome, TurnId,
    child_session_runtime_binding, mutate_child, occupancy_session_fence, persist_or_fault,
    require_reconstructed_coding_projection, spawn_parent_tree_id,
};

struct InflightChild {
    pub(super) agent_id: String,
    pub(super) locator: String,
    pub(super) path: String,
    pub(super) turn_id: String,
}

fn inflight_children(tree: &crate::agent_tree::ProductAgentTree) -> Vec<InflightChild> {
    tree.inflight_join_targets()
        .into_iter()
        .filter(|(agent_id, ..)| {
            tree.pending_transient_resume(agent_id)
                .ok()
                .flatten()
                .is_none()
        })
        .map(|(agent_id, locator, path, turn_id)| InflightChild {
            agent_id,
            locator,
            path,
            turn_id,
        })
        .collect()
}

/// Runs `recover_node` (design section 6.3) for every child this custody
/// take owes one, descendants first: each child the reopen law found with a
/// bound or active turn, or parked with a start the lost process prepared.
///
/// Every turn the open finds was the lost owner's, one it restores (a
/// decision barrier, or an admitted start not yet dispatched) included: that
/// turn is cancelled, never rejoined. Right after the open the child is
/// interrupted with cause `owner_lost`, attempt and subtree kept, and the
/// feed the open left is projected after that mark. A child whose open
/// refuses stays owed, takes no new turn, and records the refusal as its
/// fault. Each child is held for its own recovery; one another run holds is
/// skipped.
#[allow(
    clippy::too_many_lines,
    reason = "Child recovery reconciles each owed attempt with its live owner, the exact reopen and the owner-lost mark."
)]
pub(in crate::agent) async fn recover_owed_children(
    parent: &SpawnParent,
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
) {
    let Ok((owed, bound)) = runtime.read(|tree| {
        let bound: std::collections::BTreeMap<String, InflightChild> = inflight_children(tree)
            .into_iter()
            .map(|child| (child.agent_id.clone(), child))
            .collect();
        (tree.recovery_owed_targets(), bound)
    }) else {
        return;
    };
    let parked = runtime
        .read(|tree| {
            tree.nodes()
                .values()
                .filter_map(|node| {
                    Some((
                        node.agent_id.clone(),
                        (
                            node.session_locator.clone()?,
                            node.path.display().to_string(),
                        ),
                    ))
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut faults: Vec<(String, String)> = Vec::new();
    for agent_id in owed {
        if !crate::agent::placement::ready(runtime.driver(), &agent_id) {
            continue;
        }
        let _claim = match crate::agent::recovery::claim_child(runtime, &agent_id).await {
            Ok(claim) => claim,
            Err(crate::agent::recovery::ChildOwner::Process) => {
                // This process owns the child's turn already; nothing was lost.
                settle_owed(runtime, &agent_id).await;
                continue;
            }
            Err(
                crate::agent::recovery::ChildOwner::Recovery
                | crate::agent::recovery::ChildOwner::StepHeld
                | crate::agent::recovery::ChildOwner::Closed,
            ) => continue,
        };
        if let Some(child) = bound.get(&agent_id) {
            match recover_bound_child(parent, runtime, child).await {
                Ok(()) => {}
                Err(reason) => faults.push((agent_id, reason)),
            }
            continue;
        }
        let Some((locator, path)) = parked.get(&agent_id).cloned() else {
            mark_owner_lost(runtime, &agent_id, None).await;
            continue;
        };
        match reopen_spawned_child(
            parent,
            &agent_id,
            &path,
            &locator,
            "park-wake",
            ChildReopenMode::TAKE,
        )
        .await
        {
            // A wake or mail Start the lost process journaled and the
            // runtime admitted before its turn bound is the lost owner's
            // work: never rejoined after a take (spec 03 §14.1).
            Ok(ChildReopen::Attached {
                session,
                recovered: Some(recovered),
            }) => mark_restored_lost(runtime, &agent_id, *session, recovered).await,
            Ok(ChildReopen::Attached { session, .. }) => {
                mark_owner_lost(runtime, &agent_id, Some(*session)).await;
            }
            Ok(ChildReopen::RecoveryRequired(report)) => faults.push((
                agent_id,
                format!(
                    "the child's session is recovery-required after the process was lost ({:?})",
                    report.phase()
                ),
            )),
            Err(reason) => faults.push((agent_id, reason.message)),
        }
    }
    if faults.is_empty() {
        return;
    }
    let driver = runtime.driver().clone();
    mutate_child(
        runtime.driver(),
        &runtime
            .read(|tree| tree.root_agent_id.clone())
            .unwrap_or_default(),
        move |tree| {
            for (agent_id, reason) in &faults {
                tree.record_fault(agent_id, reason.clone());
            }
            let root_id = tree.root_agent_id.clone();
            persist_or_fault(&driver, tree, &root_id);
        },
    )
    .await;
}

/// recover_node for a child with a bound turn: the exact open, then the
/// owner-lost mark, after cancelling a turn the open restored.
async fn recover_bound_child(
    parent: &SpawnParent,
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    child: &InflightChild,
) -> Result<(), String> {
    if TurnId::from_str(&child.turn_id).is_err() {
        mark_owner_lost(runtime, &child.agent_id, None).await;
        return Ok(());
    }
    let reopened = reopen_spawned_child(
        parent,
        &child.agent_id,
        &child.path,
        &child.locator,
        "reattach",
        ChildReopenMode::TAKE,
    )
    .await
    .map_err(|reason| reason.message)?;
    match reopened {
        // A turn the open restored (a decision barrier, an admitted start
        // not yet dispatched) is the lost owner's: never rejoined.
        ChildReopen::Attached {
            session: attached,
            recovered: Some(recovered),
        } => {
            mark_restored_lost(runtime, &child.agent_id, *attached, recovered).await;
            Ok(())
        }
        ChildReopen::Attached {
            session: attached, ..
        } => {
            mark_owner_lost(runtime, &child.agent_id, Some(*attached)).await;
            Ok(())
        }
        ChildReopen::RecoveryRequired(report) => {
            match recover_reattach_effect_evidence(parent, &child.agent_id, &report).await {
                Ok(RecoverEffectEvidenceOutcome::Finalized { target, .. }) => {
                    let locator = target.to_string();
                    let sessions = parent
                        .state_roots()
                        .runtime_sessions()
                        .map_err(|error| error.to_string())?;
                    crate::agent::ledger::project_feed(
                        runtime,
                        &child.agent_id,
                        &sessions,
                        target.session_id(),
                    )
                    .await?;
                    let finalized = InflightChild {
                        agent_id: child.agent_id.clone(),
                        locator,
                        path: child.path.clone(),
                        turn_id: child.turn_id.clone(),
                    };
                    Box::pin(recover_bound_child(parent, runtime, &finalized)).await
                }
                _ => Err(format!(
                    "the child's session is recovery-required after the process was lost ({:?})",
                    report.phase()
                )),
            }
        }
    }
}

/// The child's owed recovery ran and nothing was lost.
async fn settle_owed(runtime: &crate::topology_runtime::ProductTopologyRuntime, agent_id: &str) {
    let id = agent_id.to_owned();
    mutate_child(runtime.driver(), agent_id, move |tree| {
        tree.recovery_settled(&id);
    })
    .await;
}

/// The owner-lost mark, written right after the exact open (design section
/// 6.3, step 5): the anchored interrupt first, then the release of the
/// reopened session and the projection of the feed its open left, so the
/// abandoned turn's terminal folds as the interrupted turn's. A command the
/// lost process prepared that the feed does not name never ran: it is
/// withdrawn and its mail returns to unread. The owed mark settles last,
/// so no delivery sees the child before its lost turn is folded.
async fn mark_owner_lost(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    reopened: Option<AttachedCodingSession>,
) {
    mark_and_project(runtime, agent_id, reopened, None).await;
    settle_owed(runtime, agent_id).await;
}

/// The owner-lost mark for a child whose exact open restored a turn the
/// lost owner left. A child's open closes an effect-decision barrier and a
/// promoted server pause itself, so only a hosted tool batch or a pause the
/// lost owner never promoted is restored here (spec 03 §14.1). The turn is
/// abandoned first, with [`restored_loss_cause`], so its one terminal (the
/// abandonment report and note, never a requested cancel's) is in the feed
/// the mark projects. On a child
/// already interrupted (a continuation, wake or mail Start the lost owner
/// admitted before binding it) the restored turn is lost again: the mark is
/// recorded anew on that turn, and the child's continuation is decided on it
/// (spec 03 §14.1).
pub(in crate::agent) async fn mark_restored_lost(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    session: AttachedCodingSession,
    recovered: RecoveredTurnHandle,
) {
    // The release's shutdown would cancel the turn too; the explicit abandon
    // bounds the wait for its terminal by the host's cancel bound.
    let cause = restored_loss_cause(runtime, agent_id);
    let control = recovered.control();
    let _ = crate::interrupt::settle_cancel(
        async {
            let abandoned = control.abandon(cause).await;
            if let Err(error) = &abandoned
                && error.kind() == plakous::agent::ControlErrorKind::Internal
            {
                warn_unreported_abandonment(agent_id, recovered.turn_id(), cause, error);
                return Ok(plakous::agent::CancellationStatus::Requested);
            }
            abandoned
        },
        plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT,
    )
    .await;
    let turn = recovered.turn_id().to_string();
    mark_and_project(runtime, agent_id, Some(session), Some(turn)).await;
    settle_owed(runtime, agent_id).await;
}

/// The restored turn was cancelled, but its abandonment report could not be
/// built (a root queued behind it is work no closure row admits), so its
/// terminal is a plain cancel's and its history carries the operator note.
fn warn_unreported_abandonment(
    agent_id: &str,
    turn_id: TurnId,
    cause: plakous::agent::AbandonmentCause,
    error: &plakous::agent::ControlError,
) {
    tracing::warn!(target: "plak.topology", agent_id, %turn_id, ?cause,
        error_kind = ?error.kind(),
        "restored child turn cancelled without its abandonment report; it carries the operator note");
}

/// Why a restored turn's previous owner let it go: a planned handoff
/// interrupted it; any other loss is the process's.
pub(in crate::agent) fn restored_loss_cause(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
) -> plakous::agent::AbandonmentCause {
    if runtime
        .read(|tree| tree.turn_handed_off(agent_id))
        .unwrap_or(false)
    {
        plakous::agent::AbandonmentCause::Interrupted
    } else {
        plakous::agent::AbandonmentCause::ProcessLost
    }
}

async fn mark_and_project(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    reopened: Option<AttachedCodingSession>,
    restored: Option<String>,
) {
    let driver = runtime.driver().for_agent(agent_id);
    let id = agent_id.to_owned();
    mutate_child(&driver, agent_id, {
        let writer = driver.clone();
        move |tree| {
            // A running child is marked; an interrupted one whose restored
            // turn the lost owner admitted after its last interrupt is
            // marked again on that turn.
            if let Some(turn) = restored.as_deref()
                && tree.node(&id).is_ok_and(|node| {
                    node.status == crate::agent_tree_protocol::NodeStatus::Interrupted
                })
            {
                tree.mark_restored_turn_lost(&id, turn);
            } else {
                tree.mark_owner_lost(&id);
            }
            persist_or_fault(&writer, tree, &id);
        }
    })
    .await;
    let Some(session) = reopened else {
        return;
    };
    let sessions = Arc::clone(&session.sessions);
    let session_id = session.locator().session_id();
    let _ = session.release_opened().await;
    if let Err(error) =
        crate::agent::ledger::project_feed(runtime, agent_id, &sessions, session_id).await
    {
        record_mark_fault(
            &driver,
            agent_id,
            format!("the feed did not project: {error}"),
        )
        .await;
        return;
    }
    let withdrawn = match crate::agent::ledger::prepared_commands(&driver, agent_id) {
        Ok(commands) => {
            let mut withdrawn = Ok(());
            // A continuation consent already recorded is resubmitted under
            // its own command id, never withdrawn.
            let kept = driver
                .read(|tree| {
                    commands
                        .iter()
                        .filter(|command| {
                            tree.is_loss_continuation_command(agent_id, &command.command_id)
                        })
                        .map(|command| command.command_id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for command in commands
                .into_iter()
                .filter(|command| !kept.contains(&command.command_id))
            {
                if let Err(error) = command.settle(&driver, false).await {
                    withdrawn = Err(error);
                }
            }
            withdrawn
        }
        Err(error) => Err(error),
    };
    if let Err(error) = withdrawn {
        record_mark_fault(
            &driver,
            agent_id,
            format!("an unadmitted command was not withdrawn: {error}"),
        )
        .await;
    }
    // A turn the open neither restored nor closed never ran: no terminal
    // will release its binding.
    let id = agent_id.to_owned();
    mutate_child(&driver, agent_id, {
        let writer = driver.clone();
        move |tree| {
            let stale = tree.node(&id).is_ok_and(|node| {
                node.status == crate::agent_tree_protocol::NodeStatus::Interrupted
            }) && tree
                .attempt(&id)
                .is_some_and(|attempt| !attempt.status.is_stopped() && attempt.turn_id.is_some());
            if stale && tree.release_attempt_turn(&id).is_ok() {
                persist_or_fault(&writer, tree, &id);
            }
        }
    })
    .await;
}

/// What an owner-lost mark could not finish, as the child's fault reason.
async fn record_mark_fault(
    driver: &crate::agent_tree_tools::TreeDriver,
    agent_id: &str,
    reason: String,
) {
    let id = agent_id.to_owned();
    mutate_child(driver, agent_id, {
        let writer = driver.clone();
        move |tree| {
            tree.record_fault(&id, format!("owner-lost recovery: {reason}"));
            persist_or_fault(&writer, tree, &id);
        }
    })
    .await;
}

/// How long a child reopen waits for a lease its lost parent still held: one
/// lease period plus slack, since the dead holder renews for at most that long.
pub(in crate::agent) const CHILD_LEASE_WAIT: Duration = Duration::from_secs(35);

pub(in crate::agent) const CHILD_LEASE_POLL: Duration = Duration::from_secs(2);

pub(in crate::agent) enum ChildReopen {
    Attached {
        session: Box<AttachedCodingSession>,
        recovered: Option<RecoveredTurnHandle>,
    },
    RecoveryRequired(RecoveryRequiredReport),
}

/// How one child reopen behaves around the parts a caller may not want.
#[derive(Clone, Copy, Debug)]
pub(in crate::agent) struct ChildReopenMode {
    /// How long to wait out a lease another owner still holds before
    /// refusing with [`ChildReopenErrorKind::LeaseHeld`].
    pub(in crate::agent) lease_wait: Duration,
    /// A recovery reopen records nothing on the node but a moved session
    /// locator: no abandoned-turn fact (its settlement carries that), and no
    /// retirement failure when the result reconciliation refuses.
    pub(in crate::agent) recovery: bool,
    /// Whether the reopen projects the feed the open left before it
    /// returns. A custody take's recover_node projects it itself, after its
    /// owner-lost mark: a lost turn's terminal folded before the mark would
    /// interrupt the child with no cause, and so with no continuation.
    pub(in crate::agent) project: bool,
}

impl ChildReopenMode {
    /// A reopen by the process that owns the child: waits out a lease the
    /// lost owner still held and records what it finds on the node.
    pub(in crate::agent) const OWNER: Self = Self {
        lease_wait: CHILD_LEASE_WAIT,
        recovery: false,
        project: true,
    };

    /// A custody take's reopen of a child it owes recover_node, and a loss
    /// continuation's: as [`Self::OWNER`], with the feed left for the mark
    /// (or the caller) to project.
    pub(in crate::agent) const TAKE: Self = Self {
        project: false,
        ..Self::OWNER
    };
}

/// Why a child reopen refused, typed so callers map it without reading text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::agent) enum ChildReopenErrorKind {
    /// Another owner still holds the child session's lease.
    LeaseHeld {
        /// When the store said the lease expires, if it did.
        retry_after_seconds: Option<u64>,
    },
    /// The session store is saturated or unavailable.
    StorageUnavailable,
    /// The session was written by, or reconstructs differently under,
    /// another plak build.
    Incompatible,
    /// The tree, the binding, or the session could not be reopened for any
    /// other reason.
    SessionUnavailable,
}

impl ChildReopenErrorKind {
    /// Whether a refused create of this kind, after which the exact
    /// accepted-create lookup finds no session, proves the spawn never
    /// produced a session. Storage and a held lease do not: both are
    /// transient, and the create stays retryable work.
    pub(in crate::agent) const fn settles_never_created(&self) -> bool {
        matches!(self, Self::SessionUnavailable | Self::Incompatible)
    }

    pub(super) fn of(error: &AgentSessionsError) -> Self {
        match error.kind() {
            AgentSessionsErrorKind::Ownership => Self::LeaseHeld {
                retry_after_seconds: None,
            },
            AgentSessionsErrorKind::StorageFull
            | AgentSessionsErrorKind::StorageBusy
            | AgentSessionsErrorKind::StorageUnavailable
            | AgentSessionsErrorKind::StoragePressure => Self::StorageUnavailable,
            AgentSessionsErrorKind::IncompatibleBuild => Self::Incompatible,
            _ => Self::SessionUnavailable,
        }
    }
}

#[derive(Clone, Debug)]
pub(in crate::agent) struct ChildReopenError {
    pub(in crate::agent) kind: ChildReopenErrorKind,
    pub(in crate::agent) message: String,
    pub(in crate::agent) advice: Option<Box<plakous_host_protocol::refusal::RefusalAdvice>>,
    pub(in crate::agent) remedy: Option<plakous_host_protocol::notice_remedy::NoticeRemedy>,
}

impl ChildReopenError {
    pub(in crate::agent) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: ChildReopenErrorKind::SessionUnavailable,
            message: message.into(),
            advice: None,
            remedy: None,
        }
    }

    pub(in crate::agent) fn from_sessions(prefix: &str, error: &AgentSessionsError) -> Self {
        Self {
            kind: ChildReopenErrorKind::of(error),
            message: format_session_reopen_error(prefix, error),
            advice: crate::error::incompatible_session_advice(error).map(Box::new),
            remedy: crate::storage_pressure::sessions_remedy(error),
        }
    }
}

impl std::fmt::Display for ChildReopenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl From<ChildReopenError> for String {
    fn from(error: ChildReopenError) -> Self {
        error.message
    }
}

impl From<ChildReopenError> for crate::agent_tree_tools::MailDeliveryFailure {
    fn from(error: ChildReopenError) -> Self {
        let mut failure = plakous_host_protocol::status::HostFailure::from(error.message);
        failure.advice = error.advice;
        failure.remedy = error.remedy;
        Self::RecoveryRequired(failure)
    }
}

fn child_reopen_from_binding(failure: crate::CliFailure) -> ChildReopenError {
    ChildReopenError {
        kind: ChildReopenErrorKind::SessionUnavailable,
        message: failure.message().to_owned(),
        advice: failure.advice().cloned().map(Box::new),
        remedy: failure.remedy().cloned(),
    }
}

fn child_reopen_from_open(error: crate::agent::CodingSessionOpenFailure) -> ChildReopenError {
    ChildReopenError {
        kind: ChildReopenErrorKind::Incompatible,
        message: error.failure.message().to_owned(),
        advice: error.failure.advice().cloned().map(Box::new),
        remedy: error.failure.remedy().cloned(),
    }
}

pub(in crate::agent) const SESSION_LEASE_HOLD_HINT: &str =
    "session lease is still held; retry after 30s. This is not RecoveryRequired.";

pub(in crate::agent) fn format_session_reopen_error(
    prefix: &str,
    error: &AgentSessionsError,
) -> String {
    let message = match error.kind() {
        AgentSessionsErrorKind::IncompatibleBuild => incompatible_session_message(error),
        AgentSessionsErrorKind::Ownership => {
            format!("{prefix} (Ownership): {SESSION_LEASE_HOLD_HINT} {error}")
        }
        kind => format!("{prefix} ({kind:?}): {error}"),
    };
    crate::error::collapse_repeated_causes(&message)
}

/// The one refusal every surface prints for a session another build wrote.
pub(crate) fn incompatible_session_message(error: &AgentSessionsError) -> String {
    let message = error
        .to_string()
        .replace("an incompatible build", "an incompatible plak build");
    match error.incompatible_session() {
        Some(_) => {
            format!("{message}; export using the writing build, then import with this build")
        }
        None => message,
    }
}

/// Product does not persist executor outcomes outside Plakous. SIGKILL during
/// a Working mutate therefore has no caller-owned evidence to supply.
fn product_owned_child_effect_evidence(
    report: &RecoveryRequiredReport,
) -> Option<(ToolExecutorId, SessionEffectEvidence, ToolOutcome)> {
    let _ = report;
    None
}

async fn recover_reattach_effect_evidence(
    parent: &SpawnParent,
    agent_id: &str,
    report: &RecoveryRequiredReport,
) -> Result<RecoverEffectEvidenceOutcome, String> {
    let Some(obligation) =
        report.obligations().iter().copied().find(|obligation| {
            obligation.action() == SessionRecoveryAction::EffectEvidenceRequired
        })
    else {
        return Err("child reopen has no effect-evidence obligation".into());
    };
    let Some((executor, evidence, outcome)) = product_owned_child_effect_evidence(report) else {
        return Err("no executor-owned effect evidence; RecoveryRequired".into());
    };
    let generation = crate::topology_runtime::lookup(spawn_parent_tree_id(parent))
        .and_then(|runtime| runtime.read(|tree| tree.host_generation).ok())
        .unwrap_or(1);
    let operation_id =
        SessionOperationId::new(format!("plak:recover-effect:{agent_id}:{generation}"))
            .map_err(|_| "child recover-effect operation identity is invalid".to_owned())?;
    let sessions = parent
        .state_roots()
        .runtime_sessions()
        .map_err(|_| "plak runtime service is unavailable".to_owned())?;
    sessions
        .recover_effect_evidence(RecoverEffectEvidenceRequest::new(
            operation_id,
            report.locator(),
            obligation.operation_id(),
            evidence,
            executor,
            outcome,
        ))
        .await
        .map_err(|error| error.to_string())
}

pub(in crate::agent) async fn reopen_spawned_child(
    parent: &SpawnParent,
    agent_id: &str,
    path: &str,
    locator: &str,
    operation_kind: &str,
    mode: ChildReopenMode,
) -> Result<ChildReopen, ChildReopenError> {
    Box::pin(reopen_spawned_child_at_head(
        parent,
        agent_id,
        path,
        locator,
        operation_kind,
        mode,
    ))
    .await
}

#[allow(
    clippy::too_many_lines,
    reason = "Child reopen reconstructs the stored binding and frozen tools before admitting its existing canonical head into the topology runtime."
)]
async fn reopen_spawned_child_at_head(
    parent: &SpawnParent,
    agent_id: &str,
    path: &str,
    locator: &str,
    operation_kind: &str,
    mode: ChildReopenMode,
) -> Result<ChildReopen, ChildReopenError> {
    let tree_id = spawn_parent_tree_id(parent).to_owned();
    let topology = crate::topology_runtime::lookup(&tree_id).ok_or_else(|| {
        ChildReopenError::unavailable("topology hub is not attached; cannot reopen child")
    })?;
    let bind = topology
        .read(|tree| {
            tree.node(agent_id)
                .map(ChildBoundBind::of)
                .map_err(|error| error.message)
        })
        .map_err(|error| error.message)
        .and_then(|bind| bind)
        .map_err(ChildReopenError::unavailable)?;
    let (account, tuple_id, effort) = (bind.account, bind.tuple_id, bind.effort);
    let fence = topology
        .read(|tree| {
            tree.node(agent_id)
                .ok()
                .map(|node| occupancy_session_fence(node.tier))
        })
        .ok()
        .flatten()
        .unwrap_or(SessionToolFence::OccupancyReadOnly);
    let sessions = parent
        .state_roots()
        .child_runtime_sessions()
        .map_err(|_| ChildReopenError::unavailable("plak runtime service is unavailable"))?;
    let mut locator = SessionLocator::from_str(locator).map_err(|error| {
        ChildReopenError::unavailable(format!("child session locator is invalid: {error}"))
    })?;
    let generation = topology.read(|tree| tree.host_generation).unwrap_or(1);
    // A turn a planned handoff let go of was not lost with its process.
    let barrier_cause = if topology
        .read(|tree| tree.turn_handed_off(agent_id))
        .unwrap_or(false)
    {
        plakous::agent::AbandonmentCause::Interrupted
    } else {
        plakous::agent::AbandonmentCause::ProcessLost
    };
    let mut retry = crate::driver::reopen::ExactRetry::with_lease_wait(mode.lease_wait);
    let opened = loop {
        // Read before the child's configuration is compiled, and re-read on the
        // second attempt because adoption moved the locator. A reopened child
        // keeps the output bound it was created with for the same reason a
        // reopened root does: the figure reaches the configuration identity and
        // from there the child's persisted plan, projection target, request
        // prefix and admission evidence, none of which can be re-signed.
        let created_output_bound =
            match Box::pin(sessions.created_request_output_bound(locator)).await {
                Ok(bound) => bound,
                // The head moved past the recorded locator, typically because
                // the lost turn advanced it. Adopt it exactly as a stale open does.
                Err(error)
                    if let Some(current) = crate::driver::reopen::stale_head(&error)
                        && retry.adopt() =>
                {
                    locator = current;
                    continue;
                }
                Err(error) => {
                    return Err(ChildReopenError::from_sessions(
                        "child created output bound is unavailable",
                        &error,
                    ));
                }
            };
        let roots = parent
            .state_roots()
            .clone()
            .for_exact_reopen(&sessions, locator)
            .await
            .map_err(|error| {
                ChildReopenError::from_sessions("child recorded evaluator is unavailable", &error)
            })?;
        let mut scoped_parent = parent.clone();
        match &mut scoped_parent {
            SpawnParent::Native(spec) => spec.state_roots = roots,
            SpawnParent::Api(spec) => spec.state_roots = roots,
        }
        let runtime = child_session_runtime_binding(
            &scoped_parent,
            agent_id,
            path,
            account.as_deref(),
            tuple_id.as_deref(),
            effort.as_deref(),
            true,
            fence,
            Some(created_output_bound),
            None,
        )
        .await
        .map_err(child_reopen_from_binding)?;
        let operation_id = SessionOperationId::new(format!(
            "plak:{operation_kind}:{agent_id}:{generation}:{}:{}",
            u8::from(retry.adoptions),
            retry.lease_waits
        ))
        .map_err(|_| ChildReopenError::unavailable("child reopen operation identity is invalid"))?;
        #[cfg(test)]
        super::reopen_error_tests::record_operation(&operation_id);
        // A child's reopen never takes over a pending decision: the turn the
        // previous owner left at one is closed as abandoned, never restored,
        // so nothing tells the child its request was cancelled (spec 03
        // §14.1).
        let request = OpenSessionRequest::new(operation_id, locator, runtime)
            .closing_effect_decision_barriers(barrier_cause);
        match Box::pin(sessions.open(request)).await {
            Ok(OpenSessionOutcome::RecoveryRequired {
                retry_after_seconds: Some(retry_after),
                ..
            }) => {
                let Some(delay) = retry.lease_delay(CHILD_LEASE_POLL) else {
                    return Err(ChildReopenError {
                        advice: None,
                        remedy: None,
                        kind: ChildReopenErrorKind::LeaseHeld {
                            retry_after_seconds: Some(retry_after),
                        },
                        message: format!(
                            "child session lease is still held by another owner; retry after \
                             {retry_after}s"
                        ),
                    });
                };
                tokio::time::sleep(delay).await;
            }
            Ok(outcome) => break outcome,
            Err(error)
                if let Some(current) = crate::driver::reopen::stale_head(&error)
                    && retry.adopt() =>
            {
                locator = current;
            }
            Err(error) => {
                return Err(ChildReopenError::from_sessions(
                    "child session could not be reopened",
                    &error,
                ));
            }
        }
    };
    let incompatible = child_reopen_from_open;
    let recovered = classify_opened(&opened);
    let reopened = match opened {
        OpenSessionOutcome::Opened { session, .. }
        | OpenSessionOutcome::RestoredWorking { session, .. } => {
            require_reconstructed_coding_projection(&session).map_err(incompatible)?;
            let locator = session.locator();
            ChildReopen::Attached {
                session: Box::new(AttachedCodingSession {
                    output_bound_rederived: None,
                    reachability_probe: None,
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: recovered.clone(),
                    locator,
                    topology_owner: None,
                }),
                recovered,
            }
        }
        OpenSessionOutcome::RecoveryRequired { recovery, .. } => {
            ChildReopen::RecoveryRequired(recovery)
        }
        _ => {
            return Err(ChildReopenError::unavailable(
                "child session reopen was unavailable",
            ));
        }
    };
    reconcile_reopened_child_results(&topology, agent_id, reopened, mode)
        .await
        .map_err(ChildReopenError::unavailable)
}

/// Settles a reopened child's command intents from its feed's
/// `CommandAccepted` and `CommandRejected` facts through the tree projector.
pub(in crate::agent) async fn reconcile_reopened_results(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    session: &AttachedCodingSession,
) -> Result<(), String> {
    // Only unresolved command intents need their facts here; with none, the
    // reopen's own feed pass (recovery's release, the wake's preparation)
    // applies the feed with the terminal reads it needs.
    if crate::agent::ledger::prepared_commands(runtime.driver(), agent_id)?.is_empty() {
        return Ok(());
    }
    crate::agent::ledger::project_child_feed(runtime, agent_id, session)
        .await
        .map(|_| ())
}

pub(super) async fn reconcile_reopened_child_results(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    reopened: ChildReopen,
    mode: ChildReopenMode,
) -> Result<ChildReopen, String> {
    use crate::topology_runtime::LiveChildHandle as _;
    let ChildReopen::Attached { session, recovered } = reopened else {
        return Ok(reopened);
    };
    if !mode.project {
        return Ok(ChildReopen::Attached { session, recovered });
    }
    let expected_attempt = runtime
        .read(|tree| {
            tree.attempt(agent_id)
                .map(|attempt| attempt.attempt_id.clone())
        })
        .map_err(|error| error.message)?;
    let reconciled = async {
        if !mode.recovery {
            let cursor = runtime
                .read(|tree| tree.feed_read_cursor(&session.locator().session_id().to_string()))
                .map_err(|error| error.message)?;
            let feed = crate::agent::read_tree_feed(
                &session.sessions,
                session.locator().session_id(),
                cursor,
            )
            .await?;
            crate::agent::feed_projection::retry_tree_feed(&session.sessions, feed, |feed| {
                crate::agent::feed_projection::apply_on_runtime(runtime, feed, agent_id)
            })
            .await
            .map_err(|error| error.message)?;
        }
        reconcile_reopened_results(runtime, agent_id, &session).await
    }
    .await;
    if let Err(error) = reconciled {
        let sessions = Arc::clone(&session.sessions);
        let session_id = session.locator().session_id();
        let release = if let Some(recovered) = recovered {
            Box::new(crate::agent::live::ChildActorHandle::new(
                &runtime.driver().release_owners(),
                *session,
                recovered.turn_id(),
                recovered.control(),
            ))
            .cancel_and_release(runtime.driver().clone(), None)
            .await
        } else {
            match session.release_opened().await {
                Ok(locator) => LiveRelease {
                    feed: None,
                    locator: Some(locator.to_string()),
                    failure: None,
                },
                Err(release) => LiveRelease {
                    feed: None,
                    locator: None,
                    failure: Some(release.to_string()),
                },
            }
        };
        let error = release
            .failure
            .map_or(error.clone(), |release| format!("{error}; {release}"));
        if mode.recovery {
            return Err(error);
        }
        crate::agent::ledger::project_feed(runtime, agent_id, &sessions, session_id).await?;
        record_reopen_failure(runtime, agent_id, expected_attempt.as_ref(), &error).await?;
        return Err(error);
    }
    Ok(ChildReopen::Attached { session, recovered })
}

pub(super) async fn record_reopen_failure(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    expected: Option<&crate::agent_tree_protocol::AttemptId>,
    error: &str,
) -> Result<(), String> {
    let (id, expected, error) = (agent_id.to_owned(), expected.cloned(), error.to_owned());
    runtime
        .record_then_apply(agent_id, move |tree| {
            tree.record_host_retirement_for_attempt(&id, expected.as_ref(), error)
                .map(|_| ())
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::test_kit::BlockOn as _;

    /// A child running when its host let go: handed off with `handoff`,
    /// otherwise lost with it.
    fn released_child_tree(
        id: &str,
        handoff: bool,
    ) -> (crate::agent_tree::ProductAgentTree, String) {
        let dir = std::env::temp_dir();
        let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            id,
            "root",
            "owner",
            dir,
            "selected-tuple",
            crate::agent_tree_protocol::WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent(
                "root",
                &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .unwrap()
            .agent_id;
        tree.bind_session_locator(&child, "locator".to_owned())
            .unwrap();
        tree.set_active_turn(&child, true).unwrap();
        tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
            .unwrap();
        if handoff {
            tree.record_handoff().unwrap();
        } else {
            tree.mark_owner_lost(&child);
        }
        (tree, child)
    }

    /// A restored turn a planned handoff let go is abandoned as interrupted;
    /// one the lost process left, as process lost.
    #[test]
    fn a_restored_turn_is_abandoned_with_its_previous_owners_cause() {
        for (handoff, expected) in [
            (true, plakous::agent::AbandonmentCause::Interrupted),
            (false, plakous::agent::AbandonmentCause::ProcessLost),
        ] {
            let id = uuid::Uuid::new_v4().to_string();
            let (tree, child) = released_child_tree(&id, handoff);
            let runtime = crate::topology_runtime::insert_root(id.clone(), tree, None)
                .block()
                .unwrap();
            assert_eq!(restored_loss_cause(&runtime, &child), expected);
            drop(runtime);
            crate::topology_runtime::force_release(&id).block();
        }
    }

    #[test]
    fn mail_delivery_keeps_reopen_advice_and_remedy() {
        use plakous_host_protocol::notice_remedy::NoticeRemedy;
        use plakous_host_protocol::refusal::RefusalAdvice;
        let error = ChildReopenError {
            kind: ChildReopenErrorKind::SessionUnavailable,
            message: "child session could not be reopened".into(),
            advice: Some(Box::new(RefusalAdvice::CheckStorage)),
            remedy: Some(NoticeRemedy::Rebind {}),
        };
        let failure = crate::agent_tree_tools::MailDeliveryFailure::from(error);
        let crate::agent_tree_tools::MailDeliveryFailure::RecoveryRequired(host) = failure else {
            panic!("a reopen failure is recovery-required");
        };
        assert_eq!(host.advice.as_deref(), Some(&RefusalAdvice::CheckStorage));
        assert_eq!(host.remedy, Some(NoticeRemedy::Rebind {}));
        assert!(host.diagnostic.as_str().contains("could not be reopened"));
    }

    #[test]
    fn binding_and_open_failures_keep_advice_and_remedy_on_mail_delivery() {
        use plakous_host_protocol::notice_remedy::NoticeRemedy;
        use plakous_host_protocol::refusal::RefusalAdvice;
        let failure = crate::CliFailure::new("child recorded runtime is unavailable")
            .with_advice(RefusalAdvice::CheckStorage)
            .with_remedy(NoticeRemedy::FreeDiskSpace {});
        let reopened = child_reopen_from_binding(failure);
        let opened = child_reopen_from_open(crate::agent::CodingSessionOpenFailure::from(
            crate::CliFailure::new("child session projection is incompatible")
                .with_advice(RefusalAdvice::CheckStorage)
                .with_remedy(NoticeRemedy::FreeDiskSpace {}),
        ));
        for error in [reopened, opened] {
            let delivered = crate::agent_tree_tools::MailDeliveryFailure::from(error);
            let crate::agent_tree_tools::MailDeliveryFailure::RecoveryRequired(host) = delivered
            else {
                panic!("a reopen failure is recovery-required");
            };
            assert_eq!(host.advice.as_deref(), Some(&RefusalAdvice::CheckStorage));
            assert_eq!(host.remedy, Some(NoticeRemedy::FreeDiskSpace {}));
        }
    }
}
