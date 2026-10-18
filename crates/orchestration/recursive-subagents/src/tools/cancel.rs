//! Interrupt, close and recover: the cancel ports and the fail-closed receipts.

use super::*;

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

impl TopologyTool {
    pub(super) async fn interrupt_with(
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

    pub(super) async fn close_with(
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
    pub(super) async fn recover_with(
        &self,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
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

    pub(super) fn fail_closed_session(
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
}
