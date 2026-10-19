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

mod cancel;
mod driver;
mod jobs;
mod mail;
mod registrations;
mod spawn;
mod wait;

pub use cancel::{ChildRecoverer, LiveCancelFailure, LiveCancelOutcome, LiveTurnCanceller};
pub use driver::TreeDriver;
pub use mail::{MailDeliverer, MailDeliveryAck, MailDeliveryFailure, MailDeliveryRequest};
pub use registrations::{
    TopologyHostPorts, invoke_granted_topology_tool_on, topology_tool_registrations,
    topology_tool_registrations_in, topology_tool_registrations_on,
};
pub use spawn::{SpawnBindError, SpawnBound, SpawnStart, SpawnedSessionBinder};

use driver::driver_for_tree;
use jobs::{admit_awaited_jobs, await_job_rows, named_job_settled};
use mail::{MailConsumer, MailConsumerGuard, settle_followup_refusal};
use wait::leave_wait_facts;

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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
