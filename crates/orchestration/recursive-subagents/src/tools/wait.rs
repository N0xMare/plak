//! The await every wait presentation drives and its receipts.

use super::*;

/// Leaves the facts the host reads from an await's whole result at the tool
/// boundary, never from the bounded preview a fan-out wait outgrows, and
/// reports whether a steer woke it.
pub(super) fn leave_wait_facts(operation_id: &str, outcome: &ToolOutcome) -> bool {
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

impl TopologyTool {
    #[cfg(test)]
    pub(super) async fn wait_with(&self, value: &Value) -> Result<ToolOutcome, ToolRuntimeError> {
        self.wait_operation_with(&format!("test-wait-{}", uuid::Uuid::new_v4()), value)
            .await
    }

    #[cfg(test)]
    pub(super) async fn wait_operation_with(
        &self,
        operation_id: &str,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        self.wait_operation_preempt(operation_id, value, None).await
    }

    pub(super) async fn wait_operation_preempt(
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
    pub(super) async fn await_operation<R: serde::Serialize>(
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
