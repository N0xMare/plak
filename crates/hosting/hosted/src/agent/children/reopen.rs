//! Reopening and reattaching children after a restart.

use super::{
    AgentSessionsError, AgentSessionsErrorKind, Arc, AttachedCodingSession, ChildBoundBind,
    ChildFeedSource, Duration, FromStr, LiveRelease, OpenSessionOutcome, OpenSessionRequest,
    RecoverEffectEvidenceOutcome, RecoverEffectEvidenceRequest, RecoveredTurnHandle,
    RecoveryRequiredReport, SessionEffectEvidence, SessionLocator, SessionOperationId,
    SessionRecoveryAction, SessionToolFence, SpawnParent, ToolExecutorId, ToolOutcome, TurnId,
    TurnResultHandle, TurnWaiterRegistration, child_session_runtime_binding, mail_command_id,
    mutate_child, occupancy_session_fence, persist_or_fault,
    require_reconstructed_coding_projection, spawn_parent_tree_id, spawn_topology_turn_waiter,
};

/// A child reopen follows one stale-locator answer without a root predicate.
/// A second stale answer is a fault, not another adoption opportunity.
#[doc(hidden)]
#[must_use]
pub fn child_reopen_head(
    error: &AgentSessionsError,
    already_adopted: bool,
) -> Option<SessionLocator> {
    if already_adopted || error.kind() != AgentSessionsErrorKind::StaleLocator {
        return None;
    }
    error.current_locator()
}

/// Rejoins the turn a mail Start's reopen recovered unbound: a Start the
/// lost process journaled and the runtime admitted before the tree bound its
/// turn (spec 03 section 14.1). The reopen left that command prepared; it
/// settles from its record before the turn is joined. The turn binds to the
/// attempt before its waiter can observe it, so a turn that ends at once
/// settles as the bound turn and is never marked active again; a turn that
/// loses the live slot is unbound. An attempt that already binds another
/// turn refuses the rejoin (`RecoveryRequired`): the recovered turn is
/// cancelled and released and that binding stays. Only a turn this mail's own Start
/// admitted carried the mail. A turn another command admitted (a lost wake,
/// another mail's Start) is bound and occupied all the same, and the send
/// hears `TurnEnded` and follows up into it, so the mail is never
/// acknowledged by a turn that did not carry it.
pub(in crate::agent) async fn rejoin_recovered_start(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    message_id: &str,
    attached: AttachedCodingSession,
    turn: TurnId,
    control: plakous::agent::TurnControl,
    result: TurnResultHandle,
) -> Result<crate::agent_tree_tools::MailDeliveryAck, crate::agent_tree_tools::MailDeliveryFailure>
{
    use crate::topology_runtime::LiveChildHandle as _;
    let admitted = match crate::agent::park::settle_recovered_start(
        runtime, agent_id, &attached, turn,
    )
    .await
    {
        Ok(admitted) => admitted,
        Err(error) => {
            let _ = Box::new(crate::agent::live::ProductLiveChild::new(
                attached, turn, control,
            ))
            .cancel_and_release(runtime.driver().clone())
            .await;
            return Err(error.into());
        }
    };
    // Only the expected state binds: an attempt with no turn binds the
    // recovered one, an attempt that already binds it is left as it is, and
    // an attempt that binds any other turn refuses the rejoin, which cancels
    // and releases the recovered turn below without touching that binding.
    // The turn's waiter registers with the bind and is dropped only after a
    // lost occupy unbound it, so the bound turn never reads as unwatched.
    let mut registration = None;
    let bound = runtime.prepare_then_emit(|tree| {
        let current = tree
            .attempt(agent_id)
            .ok_or_else(|| "recovered child turn has no attempt to bind".to_owned())?
            .turn_id
            .clone();
        match current {
            None => {
                registration = TurnWaiterRegistration::enter(runtime.driver(), tree, agent_id);
                Ok(true)
            }
            Some(current) if current == turn.to_string() => {
                registration = TurnWaiterRegistration::enter(runtime.driver(), tree, agent_id);
                Ok(false)
            }
            Some(current) => Err(format!(
                "recovered child turn {turn} does not match the turn {current} the attempt binds"
            )),
        }
    });
    match bound {
        Ok(_) => {}
        Err(error) => {
            let _ = Box::new(crate::agent::live::ProductLiveChild::new(
                attached, turn, control,
            ))
            .cancel_and_release(runtime.driver().clone())
            .await;
            return Err(error.into());
        }
    }
    let occupied = occupy_recovered_turn(
        runtime,
        agent_id.to_owned(),
        attached,
        turn,
        control,
        result,
        &mut registration,
    )
    .await;
    drop(registration);
    let occupied = occupied?;
    #[cfg(test)]
    crate::agent::park::hooks::after_recovered_occupy(agent_id).await;
    if admitted == mail_command_id(message_id, "start")?.as_str() && occupied == turn {
        return Ok(crate::agent_tree_tools::MailDeliveryAck {
            turn_id: Some(turn.to_string()),
        });
    }
    Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded)
}

pub(super) async fn occupy_recovered_child(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: String,
    attached: AttachedCodingSession,
    recovered: RecoveredTurnHandle,
) -> Result<TurnId, String> {
    let (turn, control, result) = (recovered.turn_id(), recovered.control(), recovered.result());
    let mut registration = TurnWaiterRegistration::register(runtime.driver(), &agent_id);
    occupy_recovered_turn(
        runtime,
        agent_id,
        attached,
        turn,
        control,
        result,
        &mut registration,
    )
    .await
}

/// Occupies the live slot with a recovered turn and hands it, with the
/// waiter `registration` taken before the slot became visible, to a turn
/// waiter. A slot another turn won answers that turn and leaves the
/// registration with the caller.
async fn occupy_recovered_turn(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: String,
    attached: AttachedCodingSession,
    turn: TurnId,
    control: plakous::agent::TurnControl,
    result: TurnResultHandle,
    registration: &mut Option<TurnWaiterRegistration>,
) -> Result<TurnId, String> {
    let feed = ChildFeedSource::of(&attached);
    let child: Box<dyn crate::topology_runtime::LiveChildHandle> = Box::new(
        crate::agent::live::ProductLiveChild::new(attached, turn, control),
    );
    match runtime.try_occupy_live(agent_id.clone(), child).await {
        Ok(turn_id) => {
            if let Some(registration) = registration.take() {
                spawn_topology_turn_waiter(
                    registration,
                    Arc::clone(&runtime.live),
                    runtime.observer(),
                    Some(feed),
                    result,
                );
            }
            Ok(turn_id)
        }
        Err(loser) => {
            let occupied = runtime
                .live
                .lock()
                .await
                .get(&agent_id)
                .map(crate::topology_runtime::LiveChildHandle::turn_id);
            if let Some(failure) = loser.release(runtime.driver().clone()).await.failure {
                return Err(failure);
            }
            match occupied {
                Some(existing) => Ok(existing),
                None => Err("topology hub is shutting down".into()),
            }
        }
    }
}

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

pub(in crate::agent) async fn reattach_inflight_children(
    parent: &SpawnParent,
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
) {
    let Ok(paused) =
        runtime.read(|tree| tree.state == crate::agent_tree::TreeAdmissionState::RestoredPaused)
    else {
        return;
    };
    if paused {
        // RestoredPaused: do not occupy recovered child turns until ResumeTree.
        return;
    }
    let _reattach = runtime.lock_reattach().await;
    let Ok(paused) =
        runtime.read(|tree| tree.state == crate::agent_tree::TreeAdmissionState::RestoredPaused)
    else {
        return;
    };
    if paused {
        return;
    }
    let Ok(children) = runtime.read(inflight_children) else {
        return;
    };
    let mut failed = Vec::new();
    let mut joined = Vec::new();
    let mut abandoned_children = Vec::new();
    let mut failure_reasons: Vec<(String, String)> = Vec::new();
    for child in children {
        if runtime.live.lock().await.contains_key(&child.agent_id)
            || runtime.is_recycling(&child.agent_id)
        {
            joined.push(child.agent_id);
            continue;
        }
        let _stored_turn = if let Ok(turn_id) = TurnId::from_str(&child.turn_id) {
            turn_id
        } else {
            failed.push(child.agent_id);
            continue;
        };
        match reopen_spawned_child(
            parent,
            &child.agent_id,
            &child.path,
            &child.locator,
            "reattach",
            ChildReopenMode::OWNER,
        )
        .await
        {
            Ok(ChildReopen::Attached {
                session: attached,
                recovered: Some(recovered),
            }) if bound_child_turn_matches(runtime, &child.agent_id, recovered.turn_id()) => {
                match occupy_recovered_child(runtime, child.agent_id.clone(), *attached, recovered)
                    .await
                {
                    Ok(_) => joined.push(child.agent_id),
                    Err(_) => failed.push(child.agent_id),
                }
            }
            Ok(ChildReopen::Attached {
                session: attached, ..
            }) => {
                // Process death dropped the live turn. The lifecycle contract
                // forbids a replacement Start (restart drain). A reopen that
                // abandoned the turn the lost process left settles it as an
                // ordinary failure the parent can re-task; a quiescent head
                // with nothing abandoned while the tree still lists the child
                // inflight stays RecoveryRequired, not a silent Completed.
                let abandoned = attached
                    .agent()
                    .and_then(plakous::agent::Agent::abandoned_turn)
                    .cloned();
                // Read before the release, at the head the abandonment left;
                // an unreadable history restores every receipt's mail.
                let retained = runtime
                    .read(|tree| tree.retained_wait_receipts(&child.agent_id))
                    .unwrap_or_default();
                let evidence = match crate::agent::committed_wait_evidence(
                    attached.sessions(),
                    attached.locator(),
                    retained.clone(),
                )
                .await
                {
                    Ok(evidence) => evidence,
                    Err(_) => retained.into_iter().map(|id| (id, false)).collect(),
                };
                let _ = attached.release_opened().await;
                match abandoned {
                    Some(report) => abandoned_children.push((child.agent_id, report, evidence)),
                    None => failed.push(child.agent_id),
                }
            }
            Ok(ChildReopen::RecoveryRequired(report)) => {
                failure_reasons.push((
                    child.agent_id.clone(),
                    format!(
                        "the child's session is recovery-required after the process was lost \
                         ({:?})",
                        report.phase()
                    ),
                ));
                match recover_reattach_effect_evidence(parent, &child.agent_id, &report).await {
                    Ok(RecoverEffectEvidenceOutcome::Finalized { target, .. }) => {
                        let locator = target.to_string();
                        let projected = match parent.state_roots().runtime_sessions() {
                            Ok(sessions) => {
                                crate::agent::result_context::project_feed(
                                    runtime,
                                    &child.agent_id,
                                    &sessions,
                                    target.session_id(),
                                )
                                .await
                            }
                            Err(error) => Err(error.to_string()),
                        };
                        if let Err(error) = projected {
                            failure_reasons.push((child.agent_id.clone(), error));
                            failed.push(child.agent_id);
                            continue;
                        }
                        match reopen_spawned_child(
                            parent,
                            &child.agent_id,
                            &child.path,
                            &locator,
                            "reattach-finalized",
                            ChildReopenMode::OWNER,
                        )
                        .await
                        {
                            Ok(ChildReopen::Attached {
                                session: attached,
                                recovered: Some(recovered),
                            }) if bound_child_turn_matches(
                                runtime,
                                &child.agent_id,
                                recovered.turn_id(),
                            ) =>
                            {
                                match occupy_recovered_child(
                                    runtime,
                                    child.agent_id.clone(),
                                    *attached,
                                    recovered,
                                )
                                .await
                                {
                                    Ok(_) => joined.push(child.agent_id),
                                    Err(_) => failed.push(child.agent_id),
                                }
                            }
                            Ok(ChildReopen::Attached {
                                session: attached, ..
                            }) => {
                                let _ = attached.release_opened().await;
                                failed.push(child.agent_id);
                            }
                            _ => failed.push(child.agent_id),
                        }
                    }
                    _ => failed.push(child.agent_id),
                }
            }
            Err(reason) => {
                failure_reasons.push((child.agent_id.clone(), reason.message));
                failed.push(child.agent_id);
            }
        }
    }
    // Spec 03 §14.1: a child the lost process left parked wakes once, with
    // whatever its stopped processes and its mailbox now hold.
    let owner = runtime
        .read(|tree| tree.tree_id.clone())
        .ok()
        .and_then(|tree_id| crate::topology_runtime::lookup(&tree_id));
    let parked = owner
        .as_ref()
        .and_then(|owner| {
            owner
                .read(crate::agent_tree::ProductAgentTree::parked_reopen_targets)
                .ok()
        })
        .unwrap_or_default();
    for (agent_id, locator, path) in parked {
        // The resume held this child for its reopen wake; whatever happens
        // to that wake below, the hold ends with this iteration.
        let held = agent_id.clone();
        let release_hold = || {
            mutate_child(runtime.driver(), &held, |tree| {
                tree.release_reopen_hold(&held);
            });
        };
        let Some(owner) = owner.as_ref() else {
            release_hold();
            continue;
        };
        if runtime.live.lock().await.contains_key(&agent_id) || runtime.is_recycling(&agent_id) {
            release_hold();
            continue;
        }
        match reopen_spawned_child(
            parent,
            &agent_id,
            &path,
            &locator,
            "park-wake",
            ChildReopenMode::REJOIN,
        )
        .await
        {
            Ok(ChildReopen::Attached {
                session,
                recovered: None,
            }) => {
                if let Some(report) = session
                    .agent()
                    .and_then(plakous::agent::Agent::abandoned_turn)
                    .cloned()
                {
                    // Same evidence read as the in-flight reattach above.
                    let retained = runtime
                        .read(|tree| tree.retained_wait_receipts(&agent_id))
                        .unwrap_or_default();
                    let evidence = match crate::agent::committed_wait_evidence(
                        session.sessions(),
                        session.locator(),
                        retained.clone(),
                    )
                    .await
                    {
                        Ok(evidence) => evidence,
                        Err(_) => retained.into_iter().map(|id| (id, false)).collect(),
                    };
                    let _ = session.release_opened().await;
                    abandoned_children.push((agent_id, report, evidence));
                    release_hold();
                    continue;
                }
                // An interrupt that ended the hold leaves the child
                // `interrupted`: nothing to wake and nothing to recover.
                if crate::agent::park::wake_reopened(owner, &agent_id, *session)
                    .await
                    .is_err()
                {
                    failed.push(agent_id);
                }
            }
            // A wake or mail Start the lost process journaled was admitted
            // before its turn bound: that turn rejoins (spec 03 §14.1). The
            // rejoin binds the turn active itself, and never over an
            // interrupt that ended the hold, so it is not in `joined`.
            Ok(ChildReopen::Attached {
                session,
                recovered: Some(recovered),
            }) => {
                if crate::agent::park::rejoin_reopened(owner, &agent_id, *session, recovered)
                    .await
                    .is_err()
                {
                    failed.push(agent_id);
                }
            }
            Ok(ChildReopen::RecoveryRequired(_)) => failed.push(agent_id),
            Err(reason) => {
                failure_reasons.push((agent_id.clone(), reason.message));
                failed.push(agent_id);
            }
        }
        release_hold();
    }
    let live_ids: std::collections::HashSet<String> =
        runtime.live.lock().await.keys().cloned().collect();
    mutate_child(
        runtime.driver(),
        &runtime
            .read(|tree| tree.root_agent_id.clone())
            .unwrap_or_default(),
        |tree| {
            let observer = parent.state_roots().topology_observer();
            for (agent_id, report, evidence) in &abandoned_children {
                if live_ids.contains(agent_id) {
                    continue;
                }
                let recorded = tree.record_host_turn_failure(
                    agent_id,
                    crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                        kind: crate::agent_tree::TURN_ABANDONED,
                    },
                    crate::runtime::events::abandoned_turn_operator_line(report).to_owned(),
                    evidence.clone(),
                );
                if recorded.is_err() || runtime.driver().apply_recorded(tree, agent_id).is_err() {
                    continue;
                }
                if let Some(status) = tree
                    .node(agent_id)
                    .ok()
                    .and_then(|node| crate::agent_tree::NodeDetach::from_node_status(node.status))
                {
                    tree.try_emit_tree_node_detached(observer.as_ref(), agent_id, status);
                }
            }
            for (agent_id, reason) in &failure_reasons {
                if failed.contains(agent_id) && !live_ids.contains(agent_id) {
                    tree.record_fault(agent_id, reason.clone());
                }
            }
            for agent_id in &failed {
                if live_ids.contains(agent_id) {
                    continue;
                }
                let terminal = tree
                    .node(agent_id)
                    .is_ok_and(|node| node.status.is_terminal())
                    || tree
                        .attempt(agent_id)
                        .is_some_and(|assignment| assignment.status.is_stopped());
                if terminal {
                    continue;
                }
                let _ = tree
                    .record_host_materialization(agent_id)
                    .and_then(|value| {
                        runtime
                            .driver()
                            .apply_recorded(tree, agent_id)
                            .map(|()| value)
                    });
                let status = tree
                    .node(agent_id)
                    .ok()
                    .and_then(|node| crate::agent_tree::NodeDetach::from_node_status(node.status))
                    .unwrap_or(crate::agent_tree::NodeDetach::RecoveryRequired);
                tree.try_emit_tree_node_detached(observer.as_ref(), agent_id, status);
            }
            let root_id = tree.root_agent_id.clone();
            persist_or_fault(runtime.driver(), tree, &root_id);
        },
    );
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
    /// The caller rejoins a recovered turn its attempt does not bind (a
    /// Start the lost process journaled and the runtime admitted before the
    /// bind), so the result reconcile leaves that turn's command prepared
    /// for the rejoin to settle. Every other caller settles it here.
    pub(in crate::agent) rejoin: bool,
}

impl ChildReopenMode {
    /// A reopen by the process that owns the child: waits out a lease the
    /// lost owner still held and records what it finds on the node.
    pub(super) const OWNER: Self = Self {
        lease_wait: CHILD_LEASE_WAIT,
        recovery: false,
        rejoin: false,
    };

    /// An owner reopen that rejoins an unbound recovered turn itself.
    pub(super) const REJOIN: Self = Self {
        rejoin: true,
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
}

impl ChildReopenError {
    pub(in crate::agent) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: ChildReopenErrorKind::SessionUnavailable,
            message: message.into(),
        }
    }

    pub(in crate::agent) fn from_sessions(prefix: &str, error: &AgentSessionsError) -> Self {
        Self {
            kind: ChildReopenErrorKind::of(error),
            message: format_session_reopen_error(prefix, error),
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
        Self::RecoveryRequired(error.message)
    }
}

pub(in crate::agent) const SESSION_LEASE_HOLD_HINT: &str =
    "session lease is still held; retry after 30s. This is not RecoveryRequired.";

pub(in crate::agent) fn format_session_reopen_error(
    prefix: &str,
    error: &AgentSessionsError,
) -> String {
    match error.kind() {
        AgentSessionsErrorKind::IncompatibleBuild => incompatible_session_message(error),
        AgentSessionsErrorKind::Ownership => {
            format!("{prefix} (Ownership): {SESSION_LEASE_HOLD_HINT} {error}")
        }
        kind => format!("{prefix} ({kind:?}): {error}"),
    }
}

/// The one refusal every surface prints for a session another build wrote.
pub(crate) fn incompatible_session_message(error: &AgentSessionsError) -> String {
    error
        .to_string()
        .replace("an incompatible build", "an incompatible plak build")
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
    reason = "one reopen: stale-head adoption, lease wait, and the outcome's reconciliation"
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
        .runtime_sessions()
        .map_err(|_| ChildReopenError::unavailable("plak runtime service is unavailable"))?;
    let mut locator = SessionLocator::from_str(locator).map_err(|error| {
        ChildReopenError::unavailable(format!("child session locator is invalid: {error}"))
    })?;
    let generation = topology.read(|tree| tree.host_generation).unwrap_or(1);
    // The tree's locator can lag the branch head (a turn that ended
    // without the node hearing about it). Open the head
    // the store names instead and record it, once; a second stale answer is a
    // real fault.
    let mut opened = None;
    // A lease the lost parent still held expires on its own clock; waiting
    // it out is how the reopen gets to abandon the turn that parent left.
    let lease_wait_until = tokio::time::Instant::now() + mode.lease_wait;
    let mut lease_waits = 0_u32;
    let mut attempt = 0_u32;
    while attempt < 2 {
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
                Err(error) if let Some(current) = child_reopen_head(&error, attempt != 0) => {
                    attempt += 1;
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
        )
        .await
        .map_err(ChildReopenError::unavailable)?;
        let operation_id = SessionOperationId::new(format!(
            "plak:{operation_kind}:{agent_id}:{generation}:{attempt}:{lease_waits}"
        ))
        .map_err(|_| ChildReopenError::unavailable("child reopen operation identity is invalid"))?;
        match Box::pin(sessions.open(OpenSessionRequest::new(operation_id, locator, runtime))).await
        {
            Ok(OpenSessionOutcome::RecoveryRequired {
                retry_after_seconds: Some(retry_after),
                ..
            }) => {
                let now = tokio::time::Instant::now();
                if now >= lease_wait_until {
                    return Err(ChildReopenError {
                        kind: ChildReopenErrorKind::LeaseHeld {
                            retry_after_seconds: Some(retry_after),
                        },
                        message: format!(
                            "child session lease is still held by another owner; retry after \
                             {retry_after}s"
                        ),
                    });
                }
                lease_waits = lease_waits.saturating_add(1);
                tokio::time::sleep(CHILD_LEASE_POLL.min(lease_wait_until - now)).await;
            }
            Ok(outcome) => {
                opened = Some(outcome);
                break;
            }
            Err(error) if let Some(current) = child_reopen_head(&error, attempt != 0) => {
                attempt += 1;
                locator = current;
            }
            Err(error) => {
                return Err(ChildReopenError::from_sessions(
                    "child session could not be reopened",
                    &error,
                ));
            }
        }
    }
    let Some(opened) = opened else {
        return Err(ChildReopenError::unavailable(
            "child session could not be reopened at its current head",
        ));
    };
    let incompatible = |error: crate::agent::CodingSessionOpenFailure| ChildReopenError {
        kind: ChildReopenErrorKind::Incompatible,
        message: error.failure.message().to_owned(),
    };
    let reopened = match opened {
        OpenSessionOutcome::Opened { session, .. } => {
            require_reconstructed_coding_projection(&session).map_err(incompatible)?;
            let locator = session.locator();
            ChildReopen::Attached {
                session: Box::new(AttachedCodingSession {
                    output_bound_rederived: None,
                    reachability_probe: None,
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: None,
                    locator,
                    topology_owner: None,
                }),
                recovered: None,
            }
        }
        OpenSessionOutcome::RestoredWorking { session, turn, .. } => {
            require_reconstructed_coding_projection(&session).map_err(incompatible)?;
            let locator = session.locator();
            ChildReopen::Attached {
                session: Box::new(AttachedCodingSession {
                    output_bound_rederived: None,
                    reachability_probe: None,
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: Some(turn.clone()),
                    locator,
                    topology_owner: None,
                }),
                recovered: Some(turn),
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

pub(super) fn bound_child_turn_matches(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    turn: TurnId,
) -> bool {
    runtime
        .read(|tree| {
            tree.attempt(agent_id)
                .and_then(|attempt| attempt.turn_id.as_deref())
                == Some(turn.to_string().as_str())
        })
        .unwrap_or(false)
}

/// Settles a reopened child's command intents from its feed's
/// `CommandAccepted` and `CommandRejected` facts through the tree projector.
/// `recovered` is the turn the caller rejoins, if any. A rejoined turn on an
/// attempt that binds none is a Start the lost process wrote and the runtime
/// admitted before the tree bound it: the rejoin (or the mail Start that
/// reopened the child) applies the feed itself, under its reopen hold, and
/// finds that command by its fact (spec 03 section 14.1).
pub(in crate::agent) async fn reconcile_reopened_results(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    agent_id: &str,
    session: &AttachedCodingSession,
    recovered: Option<TurnId>,
) -> Result<(), String> {
    let expected = runtime
        .read(|tree| {
            tree.attempt(agent_id)
                .and_then(|attempt| attempt.turn_id.clone())
        })
        .map_err(|error| error.message)?
        .map(|turn| {
            turn.parse::<TurnId>()
                .map_err(|_| "bound child turn is invalid".to_owned())
        })
        .transpose()?;
    if recovered.is_some() && expected.is_none() {
        return Ok(());
    }
    // Only unresolved command intents need their facts here; with none, the
    // reopen's own feed pass (recovery's release, the wake's preparation)
    // applies the feed with the terminal reads it needs.
    if crate::agent::result_context::prepared_commands(runtime.driver(), agent_id)?.is_empty() {
        return Ok(());
    }
    crate::agent::result_context::project_child_feed(runtime, agent_id, session)
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
    // Only a caller that rejoins the recovered turn keeps its command
    // prepared; a recovery or recycle reopen settles everything from the
    // records.
    let rejoining = recovered
        .as_ref()
        .filter(|_| mode.rejoin)
        .map(RecoveredTurnHandle::turn_id);
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
        reconcile_reopened_results(runtime, agent_id, &session, rejoining).await
    }
    .await;
    if let Err(error) = reconciled {
        let sessions = Arc::clone(&session.sessions);
        let session_id = session.locator().session_id();
        let release = if let Some(recovered) = recovered {
            Box::new(crate::agent::live::ProductLiveChild::new(
                *session,
                recovered.turn_id(),
                recovered.control(),
            ))
            .cancel_and_release(runtime.driver().clone())
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
        crate::agent::result_context::project_feed(runtime, agent_id, &sessions, session_id)
            .await?;
        runtime.record_then_apply(agent_id, |tree| {
            tree.record_host_retirement(agent_id, error.clone())
        })?;
        return Err(error);
    }
    Ok(ChildReopen::Attached { session, recovered })
}
