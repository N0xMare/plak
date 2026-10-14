//! The product's live child turn: `plakous-hosted`'s implementation of the tree's
//! [`crate::topology_runtime::LiveChildHandle`].
//!
//! One attached child session and the turn it is running. Everything the hub
//! asks of a live child — follow up, steer, restart, release, cancel — is a
//! session operation, and this is the only place that says so.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use plakous::agent::{CommandId, TurnControl, TurnResultHandle};
use plakous::model::TurnId;

use crate::agent::AttachedCodingSession;
use crate::topology_runtime::{LiveChildHandle, LiveRelease};

/// A follow-up or steer reached the child after the turn it targeted ended.
pub(super) const TURN_ENDED: &str = "the child's turn ended before this mail reached it";

pub(super) const RESTART_RECONCILIATION_REQUIRED: &str =
    "duplicate child restart has no live turn; result reconciliation required";

/// Prefix of a restart the runtime definitely rejected: nothing was admitted.
pub(super) const RESTART_REJECTED: &str = "child restart was rejected";

#[derive(Debug)]
enum ChildRestartError {
    ReconciliationRequired,
    /// The command was recorded below the feed's retention (spec 03 §14.1).
    Consumed,
    Runtime(String),
}

impl From<String> for ChildRestartError {
    fn from(message: String) -> Self {
        Self::Runtime(message)
    }
}

impl std::fmt::Display for ChildRestartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReconciliationRequired => formatter.write_str(RESTART_RECONCILIATION_REQUIRED),
            Self::Consumed => formatter.write_str(super::children::CHILD_COMMAND_CONSUMED),
            Self::Runtime(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ChildRestartError {}

/// One live child turn: the session it is attached to, the turn it is
/// running, and that turn's control.
pub struct ProductLiveChild {
    pub attached: AttachedCodingSession,
    pub turn_id: TurnId,
    pub control: TurnControl,
    /// Follow-up turns admitted behind the running turn, in admission order.
    pub follow_ups: std::sync::Mutex<std::collections::VecDeque<(TurnControl, TurnResultHandle)>>,
}

impl ProductLiveChild {
    /// One live child on its running turn, with no follow-up queued.
    #[must_use]
    pub fn new(attached: AttachedCodingSession, turn_id: TurnId, control: TurnControl) -> Self {
        Self {
            attached,
            turn_id,
            control,
            follow_ups: std::sync::Mutex::default(),
        }
    }

    // The offline fixture delays host acknowledgement while the admitted turn
    // completes, exercising registration independently of provider scheduling.
    async fn restart_with_ack(
        &mut self,
        driver: &crate::agent_tree_tools::TreeDriver,
        command_id: CommandId,
        prompt: String,
        acknowledged: impl Future<Output = ()> + Send,
    ) -> Result<TurnResultHandle, ChildRestartError> {
        let agent = self
            .attached
            .agent()
            .ok_or_else(|| "opened child has no agent".to_owned())?;
        let command_text = command_id.as_str().to_owned();
        let submission = agent
            .command(plakous::agent::TurnCommand::StartInstruction {
                command_id,
                text: prompt,
            })
            .await
            .map_err(|error| error.to_string())?;
        acknowledged.await;
        if super::result_context::consumed_below_retention(
            driver,
            &self.attached.sessions,
            self.attached.locator().session_id(),
            &command_text,
            submission.acknowledgement().disposition(),
            submission
                .acknowledgement()
                .rejection()
                .is_some_and(|rejection| {
                    rejection.kind() == plakous::agent::ControlErrorKind::Reclaimed
                }),
        )
        .await?
        {
            return Err(ChildRestartError::Consumed);
        }
        if let Some(rejection) = submission.acknowledgement().rejection() {
            return Err(ChildRestartError::Runtime(format!(
                "{RESTART_REJECTED}: {rejection}"
            )));
        }
        let turn_id = submission
            .acknowledgement()
            .turn_id()
            .ok_or_else(|| "submit harness start produced no turn identity".to_owned())?;
        let disposition = submission.acknowledgement().disposition();
        let control = match disposition {
            plakous::agent::CommandAckDisposition::Accepted => {
                submission.turn().map(plakous::agent::Turn::control)
            }
            plakous::agent::CommandAckDisposition::Duplicate => agent.turn_control(turn_id),
            _ => None,
        }
        .ok_or_else(|| {
            if disposition == plakous::agent::CommandAckDisposition::Duplicate {
                ChildRestartError::ReconciliationRequired
            } else {
                ChildRestartError::Runtime("child turn control is unavailable".to_owned())
            }
        })?;
        let result = submission
            .result()
            .cloned()
            .ok_or_else(|| "submit harness start produced no result handle".to_owned())?;
        // The table must name the turn that is actually running, or a
        // later follow-up steers a turn that has already ended.
        self.turn_id = turn_id;
        self.control = control;
        Ok(result)
    }
}

pub(super) fn release_observation(
    driver: &crate::agent_tree_tools::TreeDriver,
    locator: plakous::sessions::SessionLocator,
) -> (u64, bool) {
    driver
        .read(|tree| {
            (
                tree.feed_read_cursor(&locator.session_id().to_string()),
                tree.nodes().values().any(|node| {
                    node.session_locator.as_deref() == Some(locator.to_string().as_str())
                }),
            )
        })
        .unwrap_or((0, false))
}

pub(super) fn live_feed_cursor(
    driver: &crate::agent_tree_tools::TreeDriver,
    agent_id: &str,
) -> Result<u64, String> {
    driver
        .read(|tree| {
            let locator = tree
                .node(agent_id)?
                .session_locator
                .as_deref()
                .and_then(|locator| locator.parse::<plakous::sessions::SessionLocator>().ok());
            Ok::<_, crate::agent_tree::TopologyError>(locator.map_or(0, |locator| {
                tree.feed_read_cursor(&locator.session_id().to_string())
            }))
        })
        .map_err(|error| error.message)?
        .map_err(|error| error.message)
}

/// Loads the released session's feed before returning its construction locator.
pub(crate) async fn released_with_feed(
    sessions: &plakous::sessions::AgentSessions,
    locator: plakous::sessions::SessionLocator,
    failure: Option<String>,
    cursor: u64,
    recorded: bool,
) -> LiveRelease {
    match super::read_tree_feed(sessions, locator.session_id(), cursor).await {
        Ok(feed) => {
            let reached = recorded || (feed.records.is_empty() && !feed.went_behind_retention()) || feed.heads.contains(&locator) || feed.records.iter().any(|record| {
                let header = record.header();
                header.session_id == locator.session_id()
                    && header.branch_id == locator.branch_id()
                    && matches!(header.change, plakous::sessions::feed::FeedChange::LeaseReleased { head, .. } if head == locator.checkpoint_id())
            });
            let failure = if reached {
                failure
            } else {
                let debt =
                    "child release feed did not reach the released head; projection required";
                Some(
                    failure.map_or_else(|| debt.to_owned(), |failure| format!("{failure}; {debt}")),
                )
            };
            LiveRelease {
                feed: Some(feed),
                locator: Some(locator.to_string()),
                failure,
            }
        }
        Err(error) => LiveRelease {
            feed: None,
            locator: Some(locator.to_string()),
            failure: Some(failure.map_or(error.clone(), |failure| format!("{failure}; {error}"))),
        },
    }
}

impl LiveChildHandle for ProductLiveChild {
    fn reachability_probe(&self) -> Option<plakous::model::ReachabilityProbe> {
        self.attached.reachability_probe.clone()
    }

    fn agent(&self) -> Option<plakous::agent::Agent> {
        self.attached.agent().cloned()
    }
    fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    fn turn_control(&self) -> TurnControl {
        self.control.clone()
    }

    fn feed(
        &self,
        cursor: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<crate::agent_tree::TreeFeed>, String>> + Send>>
    {
        let sessions = Arc::clone(&self.attached.sessions);
        let session = self.attached.locator().session_id();
        Box::pin(async move {
            super::read_tree_feed(&sessions, session, cursor)
                .await
                .map(Some)
        })
    }

    fn follow_up(
        &self,
        command_id: CommandId,
        body: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let submission = self
                .attached
                .agent()
                .ok_or_else(|| "opened child has no agent".to_owned())?
                .follow_up_with_id(command_id, self.turn_id, body)
                .await
                .map_err(|error| {
                    if error.kind() == plakous::agent::ControlErrorKind::WrongPhase {
                        TURN_ENDED.to_owned()
                    } else {
                        error.to_string()
                    }
                })?;
            // A newly admitted follow-up is the child's next turn; an exact
            // duplicate was queued by the first delivery.
            if let (Some(turn), Some(result)) = (submission.turn(), submission.result()) {
                self.follow_ups
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_back((turn.control(), result.clone()));
            }
            Ok(())
        })
    }

    fn take_follow_up(&mut self) -> Option<TurnResultHandle> {
        let (control, result) = self
            .follow_ups
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()?;
        self.turn_id = result.turn_id();
        self.control = control;
        Some(result)
    }

    fn restart(
        &mut self,
        driver: crate::agent_tree_tools::TreeDriver,
        command_id: CommandId,
        prompt: String,
    ) -> Pin<Box<dyn Future<Output = Result<TurnResultHandle, String>> + Send + '_>> {
        Box::pin(async move {
            self.restart_with_ack(&driver, command_id, prompt, std::future::ready(()))
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        Box::pin(async move {
            let sessions = Arc::clone(&self.attached.sessions);
            match self.attached.release_opened_for_transfer().await {
                Ok(locator) => {
                    let (cursor, recorded) = release_observation(&driver, locator);
                    released_with_feed(&sessions, locator, None, cursor, recorded).await
                }
                Err(error) => LiveRelease {
                    feed: None,
                    locator: None,
                    failure: Some(format!(
                        "child session could not be released ({:?}): {error}",
                        error.kind()
                    )),
                },
            }
        })
    }

    fn cancel_and_release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> Pin<Box<dyn Future<Output = LiveRelease> + Send>> {
        Box::pin(async move {
            let cancel = match crate::interrupt::settle_cancel(
                self.control.cancel(),
                plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT,
            )
            .await
            {
                crate::interrupt::CancelSettlement::Settled => None,
                crate::interrupt::CancelSettlement::Unsettled(detail) => Some(detail),
            };
            let sessions = Arc::clone(&self.attached.sessions);
            match self.attached.release_opened_for_transfer().await {
                Ok(locator) => {
                    let (cursor, recorded) = release_observation(&driver, locator);
                    released_with_feed(&sessions, locator, cancel, cursor, recorded).await
                }
                Err(error) => LiveRelease {
                    feed: None,
                    locator: None,
                    failure: cancel.or_else(|| {
                        Some(format!(
                            "child session could not be released after cancel ({:?}): {error}",
                            error.kind()
                        ))
                    }),
                },
            }
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "offline release fixture")]
mod job_release_tests {
    use super::*;
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
        OpenSessionRequest, SessionBackend, SessionOperationId,
    };
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
    use std::{sync::Arc, time::Duration};

    #[tokio::test]
    async fn retiring_interrupted_child_transfers_job_and_crash_settles_unknown() {
        terminal_release_fixture(true).await;
    }

    #[tokio::test]
    async fn direct_attachment_release_stops_job_and_reopens_with_terminal() {
        terminal_release_fixture(false).await;
    }

    async fn terminal_release_fixture(child: bool) {
        let root = tempfile::tempdir().unwrap();
        let tree_id = uuid::Uuid::new_v4().to_string();
        let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            &tree_id,
            "root",
            "owner",
            root.path().to_owned(),
            "selected",
            crate::agent_tree_protocol::WriterMode::SingleLease,
        )
        .unwrap();
        let runtime = crate::topology_runtime::insert_root_with(
            tree_id,
            tree,
            Arc::new(crate::agent_tree::MemoryTreeStore::new()),
            None,
        )
        .unwrap();
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            root.path().join("evidence.jsonl"),
        )
        .unwrap();
        let plan: Arc<plakous_offline_crash_harness::script::ScriptPlan> = Arc::new(
            serde_json::from_value(serde_json::json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "process_tools": true,
                "steps": [
                    {"output": {"tool": {"name": "process_session_start", "arguments": {
                        "command": "read line", "workdir": ".", "login": false,
                        "tty": false, "keep_stdin_open": true, "yield_time_ms": 0,
                        "max_output_bytes": 8192
                    }}}, "finish": "tool_calls"},
                    {"stream": [{"text": "waiting"}, {"stall": 30000}]}
                ]
            }))
            .unwrap(),
        );
        let registry = Arc::new(crate::jobs::JobRegistry::default());
        let binding = || {
            scripted_runtime_binding(root.path(), controller.clone(), plan.clone())
                .unwrap()
                .with_job_host(registry.clone())
        };
        let sessions = Arc::new(
            AgentSessions::new(
                SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
            )
            .unwrap(),
        );
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("child-job:create").unwrap(),
                binding(),
            ))
            .await
            .unwrap()
        else {
            panic!("created session required")
        };
        let agent = session.agent().clone();
        let turn = agent.prompt("start").await.unwrap();
        let control = turn.control();
        tokio::time::timeout(Duration::from_secs(5), async {
            while agent.jobs().is_empty()
                || plakous_offline_crash_harness::evidence::read_evidence(
                    root.path().join("evidence.jsonl"),
                )
                .unwrap()
                .iter()
                .filter(|record| {
                    record.kind
                        == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
                })
                .count()
                    < 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        control.cancel().await.unwrap();
        let result = turn.result().await.unwrap();
        assert!(matches!(
            result.outcome(),
            plakous::agent::TurnResultOutcome::Cancelled(_)
        ));
        assert_eq!(agent.jobs()[0].status, plakous::tools::JobStatus::Running);
        let attached = AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions: sessions.clone(),
            locator: session.locator(),
            opened: Some(session),
            recovered_turn: None,
            topology_owner: None,
        };
        let locator = if child {
            let released = Box::new(ProductLiveChild::new(attached, result.turn_id(), control))
                .release(runtime.driver().clone())
                .await;
            assert!(released.failure.is_none(), "{:?}", released.failure);
            let feed = released.feed.as_ref().unwrap();
            assert!(feed.records.iter().any(|record| matches!(
                record.header().change,
                plakous::sessions::feed::FeedChange::LeaseReleased { .. }
            )));
            released.locator.unwrap().parse().unwrap()
        } else {
            attached.release().await.unwrap()
        };
        if child {
            let number = agent.jobs()[0].number;
            assert_eq!(agent.jobs()[0].status, plakous::tools::JobStatus::Running);
            let OpenSessionOutcome::Opened { session, .. } = sessions
                .open(OpenSessionRequest::new(
                    SessionOperationId::new("child-job:transfer").unwrap(),
                    locator,
                    binding(),
                ))
                .await
                .unwrap()
            else {
                panic!("transferred session required")
            };
            assert_eq!(session.agent().jobs()[0].number, number);
            assert_eq!(
                session.agent().jobs()[0].status,
                plakous::tools::JobStatus::Running
            );
            let locator = session.release_for_transfer().await.unwrap();
            // A new process has no exact registry authority: never reconstruct a PID.
            let fresh = scripted_runtime_binding(root.path(), controller.clone(), plan.clone())
                .unwrap()
                .with_job_host(Arc::new(crate::jobs::JobRegistry::default()));
            let OpenSessionOutcome::Opened { session, .. } = sessions
                .open(OpenSessionRequest::new(
                    SessionOperationId::new("child-job:after-crash").unwrap(),
                    locator,
                    fresh,
                ))
                .await
                .unwrap()
            else {
                panic!("crash-settled session required")
            };
            assert_eq!(session.agent().jobs()[0].number, number);
            assert_eq!(
                session.agent().jobs()[0].status,
                plakous::tools::JobStatus::OutcomeUnknown
            );
            session.release().await.unwrap();
            registry.stop_all(Duration::from_millis(250)).await.unwrap();
            return;
        }
        assert_eq!(
            agent.jobs()[0].status,
            plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::SessionExit)
        );
        // A fresh registry cannot infer a stop: only a pre-release durable terminal proves it.
        let fresh = scripted_runtime_binding(root.path(), controller, plan)
            .unwrap()
            .with_job_host(Arc::new(crate::jobs::JobRegistry::default()));
        let OpenSessionOutcome::Opened { session, .. } = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new("child-job:reopen").unwrap(),
                locator,
                fresh,
            ))
            .await
            .unwrap()
        else {
            panic!("released locator must open exactly")
        };
        assert_eq!(
            session.agent().jobs()[0].status,
            plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::SessionExit)
        );
        session.release().await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous::agent::{CommandAckDisposition, TurnResultOutcome};
    use plakous::model::ModelErrorKind;
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
        SessionOperationId,
    };
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, terminal_failure_runtime_binding};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn fresh_restart_retains_control_after_terminal_before_ack() {
        restart_control_fixture(false, false).await;
    }

    #[tokio::test]
    async fn duplicate_wake_restart_below_retention_is_consumed_without_rebinding() {
        restart_control_fixture(true, false).await;
    }

    #[tokio::test]
    async fn reclaimed_wake_restart_is_consumed_without_rebinding() {
        restart_control_fixture(false, true).await;
    }

    async fn restart_control_fixture(below_retention: bool, reclaimed: bool) {
        let root = tempfile::tempdir().unwrap();
        let tree_id = uuid::Uuid::new_v4().to_string();
        let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            &tree_id,
            "root",
            "owner",
            root.path().to_owned(),
            "selected",
            crate::agent_tree_protocol::WriterMode::SingleLease,
        )
        .unwrap();
        let runtime = crate::topology_runtime::insert_root_with(
            tree_id,
            tree,
            Arc::new(crate::agent_tree::MemoryTreeStore::new()),
            None,
        )
        .unwrap();
        super::super::result_context::prepare(
            runtime.driver(),
            "root",
            "restart-next",
            "submit result",
        )
        .unwrap();
        let evidence = root.path().join("restart-evidence.jsonl");
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
        let sessions = Arc::new(
            AgentSessions::new(
                SessionBackend::sqlite(root.path().join("restart.sqlite3")).unwrap(),
            )
            .unwrap(),
        );
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("restart-create").unwrap(),
                terminal_failure_runtime_binding(root.path(), controller, ModelErrorKind::Protocol)
                    .unwrap(),
            ))
            .await
            .unwrap()
        else {
            panic!("created session required")
        };
        let agent = session.agent().clone();
        let first = agent
            .start(CommandId::new("restart-first").unwrap(), "work")
            .await
            .unwrap();
        let first_turn = first.acknowledgement().turn_id().unwrap();
        let control = first.turn().unwrap().control();
        first.result().unwrap().clone().result().await.unwrap();
        let mut child = ProductLiveChild::new(
            AttachedCodingSession {
                output_bound_rederived: None,
                reachability_probe: None,
                sessions,
                locator: session.locator(),
                opened: Some(session),
                recovered_turn: None,
                topology_owner: None,
            },
            first_turn,
            control,
        );
        let command = CommandId::new("restart-next").unwrap();
        let result = child
            .restart_with_ack(
                runtime.driver(),
                command.clone(),
                "submit result".into(),
                async {
                    tokio::time::timeout(Duration::from_secs(20), async {
                        let replay = agent
                            .start_instruction(command.clone(), "submit result")
                            .await
                            .unwrap();
                        assert_eq!(
                            replay.acknowledgement().disposition(),
                            CommandAckDisposition::Duplicate
                        );
                        let result = replay.result().unwrap().clone();
                        let turn = result.turn_id();
                        assert!(matches!(
                            result.result().await.unwrap().outcome(),
                            TurnResultOutcome::Failed(_)
                        ));
                        while agent.turn_control(turn).is_some() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                },
            )
            .await
            .unwrap();
        assert_ne!(result.turn_id(), first_turn);
        assert_eq!(child.turn_id(), result.turn_id());
        assert_eq!(child.turn_control().turn_id(), result.turn_id());
        assert!(matches!(
            result.result().await.unwrap().outcome(),
            TurnResultOutcome::Failed(_)
        ));
        if reclaimed {
            for round in 0..17 {
                child
                    .attached
                    .agent()
                    .unwrap()
                    .start_instruction(
                        CommandId::new(format!("prune:{round}")).unwrap(),
                        "submit result",
                    )
                    .await
                    .unwrap()
                    .result()
                    .unwrap()
                    .clone()
                    .result()
                    .await
                    .unwrap();
            }
            let sessions = Arc::clone(&child.attached.sessions);
            let locator = child.attached.release_opened().await.unwrap();
            assert!(matches!(
                sessions.prune_history(locator).await.unwrap(),
                plakous::sessions::PruneHistoryOutcome::Pruned(_)
            ));
            let controller =
                BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)
                    .unwrap();
            let plakous::sessions::OpenSessionOutcome::Opened { session, .. } = sessions
                .open(plakous::sessions::OpenSessionRequest::new(
                    SessionOperationId::new("restart:pruned:reopen").unwrap(),
                    locator,
                    terminal_failure_runtime_binding(
                        root.path(),
                        controller,
                        ModelErrorKind::Protocol,
                    )
                    .unwrap(),
                ))
                .await
                .unwrap()
            else {
                panic!("pruned session must reopen");
            };
            child.attached = AttachedCodingSession {
                output_bound_rederived: None,
                reachability_probe: None,
                sessions,
                locator: session.locator(),
                opened: Some(session),
                recovered_turn: None,
                topology_owner: None,
            };
            let submission = child
                .attached
                .agent()
                .unwrap()
                .command(plakous::agent::TurnCommand::StartInstruction {
                    command_id: command.clone(),
                    text: "submit result".to_owned(),
                })
                .await
                .unwrap_or_else(|error| panic!("reclaimed replay: {}", error.message()));
            assert_eq!(
                submission
                    .acknowledgement()
                    .rejection()
                    .map(plakous::agent::ControlError::kind),
                Some(plakous::agent::ControlErrorKind::Reclaimed)
            );
        }
        if below_retention {
            let connection =
                rusqlite::Connection::open(root.path().join("restart.sqlite3")).unwrap();
            connection
                .execute_batch(
                    "DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions);\
                 UPDATE sessions SET feed_retained_from=feed_tail,\
                 feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);",
                )
                .unwrap();
        }
        let bound = child.turn_id();
        let Err(error) = child
            .restart_with_ack(
                runtime.driver(),
                command,
                "submit result".into(),
                std::future::ready(()),
            )
            .await
        else {
            panic!("retired duplicate cannot start another turn");
        };
        if below_retention || reclaimed {
            assert!(matches!(error, ChildRestartError::Consumed));
            assert_eq!(child.turn_id(), bound);
        } else {
            assert!(matches!(error, ChildRestartError::ReconciliationRequired));
        }
        let locator = child.attached.locator();
        let cursor = child
            .attached
            .sessions
            .subscribe_feed(locator.session_id())
            .await
            .unwrap()
            .seen()
            .get();
        let observed =
            released_with_feed(&child.attached.sessions, locator, None, cursor, false).await;
        assert!(observed.failure.is_none());
        assert_eq!(
            observed.feed.unwrap().records,
            [] as [plakous::sessions::feed::FeedRecord; 0]
        );
        assert!(
            Box::new(child)
                .release(runtime.driver().clone())
                .await
                .failure
                .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(evidence)
                .unwrap()
                .lines()
                .filter(|line| {
                    serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]
                        == "provider_dispatched"
                })
                .count(),
            if reclaimed { 19 } else { 2 },
            "one dispatch per admitted turn, none for the duplicate"
        );
    }
}
