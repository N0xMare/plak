//! Interrupt, close and recover: the cancel outcomes and the fail-closed receipts.

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
        let tool = self.clone();
        let (writer_caller, writer_operation, writer_request) =
            (caller.clone(), operation_id.clone(), request.clone());
        let (status, effects) = self
            .driver
            .mutate(move |tree| {
                tool.dispatch_mutating(
                    tree,
                    &writer_caller,
                    &writer_operation,
                    "interrupt_agent",
                    &writer_request,
                    |tree, caller, request, operation_id| {
                        tree.interrupt_agent(caller, request, operation_id)
                    },
                )
            })
            .await
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
        let Some(canceller) = self.canceller() else {
            return ToolOutcome::success_text(payload.to_string());
        };
        let canceller = canceller.clone();
        let payload_text = payload.to_string();
        self.run_retirement(
            vec![request.agent_id.clone()],
            move |tool, signalled| async move {
                let cancellation = canceller
                    .cancel_agents(vec![request.agent_id.clone()], signalled)
                    .await;
                tool.finish_cancel_receipt(
                    &operation_id,
                    &caller,
                    "interrupt_agent",
                    request.clone(),
                    status,
                    cancellation.is_ok(),
                )
                .await?;
                match cancellation {
                    Ok(_) => ToolOutcome::success_text(payload.to_string()),
                    Err(failure) => {
                        tool.fail_closed_cancel(failure, std::slice::from_ref(&request.agent_id))
                            .await
                    }
                }
            },
        )
        .await?;
        ToolOutcome::success_text(payload_text)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "close receipts retain the captured targets through cancellation, job stop and effect settlement"
    )]
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
        let tool = self.clone();
        let (writer_caller, writer_operation, writer_request) =
            (caller.clone(), operation_id.clone(), request.clone());
        let (closed, effects) = self
            .driver
            .mutate(move |tree| {
                tool.dispatch_mutating(
                    tree,
                    &writer_caller,
                    &writer_operation,
                    "close_agent",
                    &writer_request,
                    |tree, caller, request, operation_id| {
                        tree.close_agent(caller, request, operation_id)
                    },
                )
            })
            .await
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
                .map(|(agent_id, status)| json!({ "agent_id": agent_id, "status": status }))
                .collect::<Vec<_>>()
        });
        if replayed {
            self.settle(effects).await?;
            return ToolOutcome::success_text(payload.to_string());
        }
        let payload_text = payload.to_string();
        self.run_retirement(ids.clone(), move |tool, signalled| async move {
            let mut cancellation = if let Some(canceller) = tool.canceller() {
                canceller.cancel_agents(ids.clone(), signalled).await
            } else {
                let _ = signalled.send(());
                Ok(LiveCancelOutcome {
                    cancelled_ids: Vec::new(),
                    not_live_ids: ids.clone(),
                })
            };
            let stop = tool.driver.stop_agent_jobs(&ids).await;
            // Closing owns these effects even if cancellation or process cleanup failed.
            let settled = tool.settle(effects).await;
            if let Err(error) = stop {
                let failed_ids = tool
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
                let observer = tool.observer.clone();
                let detached = closed.clone();
                tool.driver
                    .write(move |tree| {
                        for (agent_id, status) in &detached {
                            let detach =
                                NodeDetach::from_node_status(*status).unwrap_or(NodeDetach::Closed);
                            tree.try_emit_tree_node_detached(observer.as_ref(), agent_id, detach);
                        }
                    })
                    .await
                    .map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "topology tree lock is poisoned",
                        )
                    })?;
            }
            tool.finish_cancel_receipt(
                &operation_id,
                &caller,
                "close_agent",
                request,
                closed,
                cancellation.is_ok(),
            )
            .await?;
            settled?;
            match cancellation {
                Ok(_) => ToolOutcome::success_text(payload.to_string()),
                Err(failure) => tool.fail_closed_cancel(failure, &ids).await,
            }
        })
        .await?;
        ToolOutcome::success_text(payload_text)
    }

    /// The independently owned join settles the prepared receipt, not the
    /// requesting tool future. A returned intent is not a release receipt.
    async fn run_retirement<F, Fut>(
        &self,
        target_ids: Vec<String>,
        work: F,
    ) -> Result<(), ToolRuntimeError>
    where
        F: FnOnce(Self, tokio::sync::oneshot::Sender<()>) -> Fut,
        Fut: Future<Output = Result<ToolOutcome, ToolRuntimeError>> + Send + 'static,
    {
        let (signal, signalled) = tokio::sync::oneshot::channel();
        let owners = self
            .driver
            .read(|tree| {
                target_ids
                    .iter()
                    .map(|id| {
                        (
                            id.clone(),
                            tree.attempt(id).map(|attempt| attempt.attempt_id.clone()),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|_| target_ids.into_iter().map(|id| (id, None)).collect());
        let work = work(self.clone(), signal);
        self.driver.spawn_retirement(async move {
            if let Err(error) = work.await {
                tracing::error!(target: "plak.topology", agent_attempts = ?owners,
                    error_kind = ?error.kind(), "retirement completion failed");
            }
        });
        // The saved intent is acknowledged even if dispatch is still pending;
        // only the owner can establish release or publish its failure.
        let _ = tokio::time::timeout(
            plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT,
            signalled,
        )
        .await;
        Ok(())
    }

    async fn finish_cancel_receipt<Req, Out>(
        &self,
        operation_id: &str,
        caller: &str,
        tool_name: &'static str,
        request: Req,
        output: Out,
        cancelled: bool,
    ) -> Result<(), ToolRuntimeError>
    where
        Req: serde::Serialize + Send + 'static,
        Out: serde::Serialize + Send + 'static,
    {
        let driver = self.driver.clone();
        let (operation_id, caller) = (operation_id.to_owned(), caller.to_owned());
        let (result, effects) = self
            .driver
            .mutate(move |tree| {
                if cancelled {
                    tree.remember_success(&operation_id, &caller, tool_name, &request, &output)?;
                } else {
                    tree.remember_failure(
                        &operation_id,
                        &caller,
                        tool_name,
                        &request,
                        TopologyErrorCode::OperationRecoveryRequired,
                    )?;
                }
                if let Err(error) = driver.save(tree) {
                    // A refused publish cannot leave an in-memory success replay.
                    tree.remember_failure(
                        &operation_id,
                        &caller,
                        tool_name,
                        &request,
                        TopologyErrorCode::OperationRecoveryRequired,
                    )?;
                    return Err(error);
                }
                Ok(())
            })
            .await
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        self.driver.settle(effects).await.map_err(|error| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        })?;
        result
            .map_err(|error| ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message))
    }

    async fn fail_closed_cancel(
        &self,
        failure: LiveCancelFailure,
        targets: &[String],
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let mut recorded = false;
        for owner in &failure.owner_failures {
            if !targets.contains(&owner.agent_id) {
                continue;
            }
            let driver = self.driver.clone();
            let observer = self.observer.clone();
            let owner = owner.clone();
            let (result, effects) = self
                .driver
                .mutate(move |tree| {
                    let id = &owner.agent_id;
                    if !owner.matches(tree) {
                        return Ok(false);
                    }
                    let changed = tree
                        .record_host_retirement_for_attempt(
                            id,
                            owner.expected_attempt_id.as_ref(),
                            owner.message.clone(),
                        )
                        .and_then(|value| driver.apply_recorded(tree, id).map(|()| value))?;
                    if !changed {
                        return Ok(false);
                    }
                    tree.try_emit_tree_node_detached(
                        observer.as_ref(),
                        id,
                        NodeDetach::RecoveryRequired,
                    );
                    driver.save(tree)?;
                    Ok(true)
                })
                .await
                .map_err(|error| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                })?;
            self.driver.settle(effects).await.map_err(|error| {
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

    pub(super) async fn fail_closed_session(
        &self,
        agent_id: &str,
        message: impl Into<plakous_host_protocol::status::HostFailure>,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let failure = message.into();
        let message = failure.diagnostic.as_str().to_owned();
        let driver = self.driver.clone();
        let observer = self.observer.clone();
        let (agent, fault) = (agent_id.to_owned(), message.clone());
        let recorded = self
            .driver
            .write(move |tree| {
                tree.record_fault(&agent, fault);
                // the partial-work seal runs with the lock released.
                let materialized = tree
                    .record_host_materialization(&agent)
                    .and_then(|value| driver.apply_recorded_guarded(tree, &agent).map(|()| value));
                let status = tree
                    .node(&agent)
                    .ok()
                    .and_then(|node| NodeDetach::from_node_status(node.status))
                    .unwrap_or(NodeDetach::RecoveryRequired);
                tree.try_emit_tree_node_detached(observer.as_ref(), &agent, status);
                let pending = if materialized.is_ok() {
                    tree.take_effects()
                } else {
                    Effects::default()
                };
                (materialized, pending)
            })
            .await;
        let Ok((persist_error, pending)) = recorded else {
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
        if let Err(error) = self.driver.settle(pending).await {
            return Self::topology_failure(&error);
        }
        let error = match persist_error {
            Ok(()) => TopologyError::new(TopologyErrorCode::OperationRecoveryRequired, message),
            Err(error) => TopologyError::new(
                error.code,
                format!("{message}; persist failed: {}", error.message),
            ),
        };
        Self::topology_failure(&error.with_failure(&failure))
    }
}
