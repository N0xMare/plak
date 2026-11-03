//! The await every wait presentation drives and its receipts.

use super::*;

/// Leaves the facts the host reads from an await's whole result at the tool
/// boundary, never from the bounded preview a fan-out wait outgrows, and
/// reports whether a steer or host notice yielded it.
pub(super) fn leave_wait_facts(operation_id: &str, outcome: &ToolOutcome) -> bool {
    let result = outcome.parts().iter().find_map(|part| match part {
        plakous::model::ToolResultPart::Text(text) => {
            serde_json::from_str::<Value>(text.text()).ok()
        }
        _ => None,
    });
    let yielded = result.as_ref().is_some_and(|value| {
        matches!(
            value.get("woke_by").and_then(Value::as_str),
            Some("steer" | "notice")
        )
    });
    if let Some(result) = result {
        crate::call_facts::leave(
            operation_id,
            crate::call_facts::TopologyCallFacts::Wait(result),
        );
    }
    yielded
}

/// The content-free wait fact of one await: how many children and processes
/// it watched and what ended it.
fn await_wait_fact(output: &AwaitOutput) -> plakous::tools::WaitFact {
    use crate::tree::WaitWakeReason;
    use plakous::tools::WaitWake;
    let wake = match output.woke_by {
        WaitWakeReason::Settled | WaitWakeReason::Process => WaitWake::Settled,
        WaitWakeReason::Mail => WaitWake::Mail,
        WaitWakeReason::Timeout => WaitWake::Timeout,
        WaitWakeReason::Steer | WaitWakeReason::Notice => WaitWake::Interrupted,
    };
    let count = |rows: usize| u32::try_from(rows).unwrap_or(u32::MAX);
    plakous::tools::WaitFact::new(count(output.agents.len()), count(output.jobs.len()), wake)
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
    #[allow(
        clippy::too_many_lines,
        reason = "wait admission retains its provisional receipt through wake, timeout, cancellation and publication"
    )]
    pub(super) async fn await_operation<R>(
        &self,
        operation_id: &str,
        request: &AwaitRequest,
        presented: &R,
        preempt: Option<tokio::sync::watch::Receiver<plakous::tools::ToolPreempt>>,
    ) -> Result<ToolOutcome, ToolRuntimeError>
    where
        R: serde::Serialize + Clone + Send + Sync + 'static,
    {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let jobs = self.driver.job_service(&caller);
        // Process terminals count from the call itself, before its admission
        // waits its writer turn.
        let generation = jobs.as_ref().map_or(
            0,
            plakous::tools::JoinedSessionsPort::job_terminal_generation,
        );
        let admitted = {
            let (operation, caller, request, presented, jobs) = (
                operation_id.to_owned(),
                caller.clone(),
                request.clone(),
                presented.clone(),
                jobs.clone(),
            );
            let parked = (caller.clone(), operation.clone());
            self.driver
                .write_or_undo(
                    move |tree| {
                        Self::admit_await(
                            tree,
                            &operation,
                            &caller,
                            &request,
                            &presented,
                            jobs.as_ref(),
                        )
                    },
                    // A wait dropped before it parked releases what it reserved.
                    move |tree, admitted| {
                        if admitted.is_ok() {
                            tree.parked_waits.remove(&parked);
                            tree.waiters().notify_waiters();
                        }
                    },
                )
                .await
                .map_err(|_| poisoned_tree())?
        };
        if let Err(done) = admitted {
            return *done;
        }
        let waiting = MailConsumerGuard::new(
            self.driver.clone(),
            MailConsumer::Wait {
                caller: caller.clone(),
                operation: operation_id.to_owned(),
            },
        );
        let outcome = self
            .await_admitted(
                operation_id,
                &caller,
                request,
                presented,
                generation,
                preempt,
            )
            .await;
        waiting.release().await;
        outcome
    }

    /// The await's admission under the tree lock: an exact replay answers
    /// from its receipt; otherwise the wait parks on its targets. `Err` is
    /// the call's whole answer.
    fn admit_await<R: serde::Serialize>(
        tree: &mut ProductAgentTree,
        operation_id: &str,
        caller: &str,
        request: &AwaitRequest,
        presented: &R,
        jobs: Option<&plakous::tools::JoinedSessionsPort>,
    ) -> Result<(), Answered> {
        if tree
            .operations()
            .get(operation_id)
            .is_some_and(|receipt| receipt.caller_agent_id != caller)
        {
            return Err(Box::new(Self::topology_failure(&TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "retry caller conflicts with the wait receipt",
            ))));
        }
        if tree.operations().contains_key(operation_id)
            && let Some(response) = tree
                .prepare_operation(operation_id, caller, AWAIT_RECEIPT_KIND, presented)
                .map_err(|error| {
                    Box::new(Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        error.message,
                    )))
                })?
        {
            let receipt: crate::tree::WaitReceipt =
                serde_json::from_str(&response).map_err(|_| {
                    Box::new(Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "wait receipt could not be reconstructed",
                    )))
                })?;
            return Err(Box::new(ToolOutcome::success_text(
                receipt.output.to_string(),
            )));
        }
        if let Err(error) = admit_awaited_jobs(jobs, request) {
            return Err(Box::new(Self::topology_failure(&error)));
        }
        // An empty or cancelled wait has no durable receipt.
        let targets = match tree.wait_target_ids(caller, &request.agents) {
            Ok(targets) => targets,
            Err(error) => return Err(Box::new(Self::topology_failure(&error))),
        };
        tree.parked_waits
            .insert((caller.to_owned(), operation_id.to_owned()), targets);
        Ok(())
    }

    async fn await_admitted<R>(
        &self,
        operation_id: &str,
        caller: &str,
        request: &AwaitRequest,
        presented: &R,
        generation: u64,
        mut preempt: Option<tokio::sync::watch::Receiver<plakous::tools::ToolPreempt>>,
    ) -> Result<ToolOutcome, ToolRuntimeError>
    where
        R: serde::Serialize + Clone + Send + Sync + 'static,
    {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(request.timeout_ms);
        let call = AwaitCall {
            tool: self.clone(),
            operation: operation_id.to_owned(),
            caller: caller.to_owned(),
            request: request.clone(),
            presented: presented.clone(),
        };
        loop {
            let jobs = self.driver.job_service(caller);
            let mut job_changes = jobs
                .as_ref()
                .map(plakous::tools::JoinedSessionsPort::job_terminal_changes);
            // the wait stays poll-driven. The notifier is
            // subscribed before the poll so a settle, seal, question, mail, or
            // steer that lands during it still wakes the next park instead of
            // being lost until the tick expires. This is not an event loop.
            let (output, notify) = self.poll_admitted(call.clone(), jobs.clone()).await?;
            if let Some(outcome) = output {
                return outcome;
            }
            let processes = jobs
                .as_ref()
                .map_or_else(Vec::new, |service| service.terminal_jobs_since(generation));
            if preempted(preempt.as_ref())
                || !processes.is_empty()
                || named_job_settled(jobs.as_ref(), &request.processes)
            {
                let outcome = self
                    .finish_woken(call.clone(), jobs, processes, preempt.as_ref())
                    .await?;
                let reason = preempt
                    .as_ref()
                    .and_then(|flag| flag.borrow().notice_reason());
                return outcome.and_then(|outcome| with_preempt_notice(outcome, reason));
            }
            if tokio::time::Instant::now() >= deadline {
                return self.finish_timed_out(call.clone(), jobs).await?;
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

    /// One poll of an admitted await, with the tree's notifier subscribed
    /// before it: the outcome once the wait is satisfied.
    async fn poll_admitted<R>(
        &self,
        call: AwaitCall<R>,
        jobs: Option<plakous::tools::JoinedSessionsPort>,
    ) -> Result<(Option<Result<ToolOutcome, ToolRuntimeError>>, OwnedNotified), ToolRuntimeError>
    where
        R: serde::Serialize + Send + 'static,
    {
        self.driver
            .write(move |tree| {
                let mut notified = Box::pin(tree.waiters().notified_owned());
                notified.as_mut().enable();
                let output = match tree.poll_wait(&call.caller, &call.request.agents) {
                    Ok(Some(output)) => Some(call.finish(tree, jobs.as_ref(), output)),
                    Ok(None) => None,
                    Err(error) => Some(Self::topology_failure(&error)),
                };
                if output.is_some() {
                    tree.parked_waits
                        .remove(&(call.caller.clone(), call.operation.clone()));
                    tree.waiters().notify_waiters();
                }
                (output, notified)
            })
            .await
            .map_err(|_| poisoned_tree())
    }

    /// Ends an await a steer, a notice or a process woke.
    async fn finish_woken<R>(
        &self,
        call: AwaitCall<R>,
        jobs: Option<plakous::tools::JoinedSessionsPort>,
        processes: Vec<String>,
        preempt: Option<&tokio::sync::watch::Receiver<plakous::tools::ToolPreempt>>,
    ) -> Result<Result<ToolOutcome, ToolRuntimeError>, ToolRuntimeError>
    where
        R: serde::Serialize + Send + 'static,
    {
        let steer =
            preempt.is_some_and(|flag| *flag.borrow() == plakous::tools::ToolPreempt::Steer);
        let notice = preempted(preempt);
        self.driver
            .write(move |tree| {
                let mut output = tree
                    .timeout_wait(&call.caller, &call.request.agents)
                    .map_err(|error| {
                        ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                    })?;
                // Recheck under the lock: mail may have arrived after the first poll.
                output.processes = processes;
                if output.woke_by == crate::tree::WaitWakeReason::Timeout {
                    output.timed_out = false;
                    output.woke_by = if steer {
                        crate::tree::WaitWakeReason::Steer
                    } else if !output.processes.is_empty() {
                        crate::tree::WaitWakeReason::Process
                    } else if notice {
                        crate::tree::WaitWakeReason::Notice
                    } else {
                        crate::tree::WaitWakeReason::Process
                    };
                }
                call.finish(tree, jobs.as_ref(), output)
            })
            .await
            .map_err(|error| ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message))
    }

    /// Ends an await whose deadline passed.
    async fn finish_timed_out<R>(
        &self,
        call: AwaitCall<R>,
        jobs: Option<plakous::tools::JoinedSessionsPort>,
    ) -> Result<Result<ToolOutcome, ToolRuntimeError>, ToolRuntimeError>
    where
        R: serde::Serialize + Send + 'static,
    {
        self.driver
            .write(move |tree| {
                let output = tree.timeout_wait(&call.caller, &call.request.agents);
                tree.parked_waits
                    .remove(&(call.caller.clone(), call.operation.clone()));
                tree.waiters().notify_waiters();
                match output {
                    Ok(output) => call.finish(tree, jobs.as_ref(), output),
                    Err(error) => Self::topology_failure(&error),
                }
            })
            .await
            .map_err(|_| poisoned_tree())
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
        jobs: Option<&plakous::tools::JoinedSessionsPort>,
        mut output: AwaitOutput,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        output.jobs = await_job_rows(jobs, &request.processes);
        output.fit_budget();
        let subject = output.presentation_subject();
        let fact = await_wait_fact(&output);
        let outcome = ToolOutcome::success_text(json!(output).to_string())
            .map(|outcome| match subject {
                Some(subject) => outcome.with_presentation_subject(subject),
                None => outcome,
            })
            .map(|outcome| outcome.with_wait_fact(fact));
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

fn poisoned_tree() -> ToolRuntimeError {
    ToolRuntimeError::new(
        ToolRuntimeErrorKind::Execution,
        "topology tree lock is poisoned",
    )
}

/// What one admitted await carries into each of its tree writers.
#[derive(Clone)]
struct AwaitCall<R> {
    tool: TopologyTool,
    operation: String,
    caller: String,
    request: AwaitRequest,
    presented: R,
}

impl<R: serde::Serialize> AwaitCall<R> {
    fn finish(
        &self,
        tree: &mut ProductAgentTree,
        jobs: Option<&plakous::tools::JoinedSessionsPort>,
        output: AwaitOutput,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        self.tool.finish_await(
            tree,
            &self.caller,
            &self.operation,
            &self.presented,
            &self.request,
            jobs,
            output,
        )
    }
}

type OwnedNotified = Pin<Box<tokio::sync::futures::OwnedNotified>>;

/// Whether a steer or an operator notice preempts the await.
fn preempted(preempt: Option<&tokio::sync::watch::Receiver<plakous::tools::ToolPreempt>>) -> bool {
    preempt.is_some_and(|flag| {
        matches!(
            *flag.borrow(),
            plakous::tools::ToolPreempt::Steer | plakous::tools::ToolPreempt::Notice(_)
        )
    })
}

/// Names the notice that preempted an await in its outcome.
fn with_preempt_notice(
    outcome: ToolOutcome,
    reason: Option<&'static str>,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let Some(reason) = reason else {
        return Ok(outcome);
    };
    let mut value: Value = serde_json::from_str(&outcome.joined_text()).map_err(|error| {
        ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.to_string())
    })?;
    value["notice"] = json!(reason);
    let mut result = ToolOutcome::success_text(value.to_string())?.with_wait_observation(true);
    if let Some(fact) = outcome.wait_fact() {
        result = result.with_wait_fact(fact);
    }
    if let Some(subject) = outcome.presentation_subject() {
        result = result.with_presentation_subject(subject);
    }
    Ok(result)
}
