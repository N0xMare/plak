//! Process-and-agent task presentations: Grok output/kill and pi wait/stop.

use super::*;

/// The job number of a process task id ([`crate::protocol::is_process_task_id`]);
/// `None` for an id that names an agent.
pub(super) fn task_process_number(id: &str) -> Option<u64> {
    crate::protocol::is_process_task_id(id)
        .then(|| id.strip_prefix('p').unwrap_or(id).parse::<u64>().ok())
        .flatten()
}

/// `get_command_or_subagent_output` on a node granted agent tools.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskOutputRequest {
    pub(super) task_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) timeout_ms: Option<u64>,
}

/// `kill_command_or_subagent` on a node granted agent tools.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskKillRequest {
    pub(super) task_id: String,
}

/// Pi's `wait` on a node granted agent tools.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JobsWaitRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) timeout_ms: Option<u64>,
}

/// Pi's `stop` on a node granted agent tools.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JobsStopRequest {
    pub(super) id: String,
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
pub(super) enum JobsWaitPlan {
    /// The process tool's own first-terminal await over these job numbers,
    /// or over every own job still listed when empty.
    Processes(Vec<u64>),
    /// The tree's await over children, their mail and named own processes.
    Await(AwaitRequest),
}

/// Named processes must be the caller's own promoted jobs in this session.
pub(super) fn admit_awaited_jobs(
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
pub(super) fn unlisted_job_admitted(id: &str, ended: &[String]) -> Result<(), TopologyError> {
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
pub(super) fn named_job_settled(
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

pub(super) fn await_job_rows(
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

impl TopologyTool {
    /// Grok's output and kill tools on a node granted agent tools. A call
    /// naming only process ids is the process tool's own call, through the
    /// process service's pack-neutral entry points, so its bytes match the
    /// process-only presentation; a call naming a child is the await (or the
    /// interrupt), and its receipt comes back in the topology envelope.
    pub(super) async fn dispatch_task(
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
    pub(super) async fn task_output_with(
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
    pub(super) async fn jobs_wait(
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
    pub(super) fn jobs_wait_plan(
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
    pub(super) async fn jobs_wait_with(
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
    pub(super) async fn task_kill_child(
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

    pub(super) fn running_jobs(&self, caller: &str) -> Vec<String> {
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
}
