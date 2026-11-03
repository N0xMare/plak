//! Follow-up catch-up ownership and the persisted prepare/apply boundary.

use super::{
    MessagePurpose, OperationRecord, ProductAgentTree, SendAgentMessageRequest, TopologyError,
    TopologyErrorCode, VcsError, VcsErrorKind, WorkspaceMode, bounded_text, canonical_json,
};
use plakous_workspace::occupancy_vcs::{
    ConflictPath, RefreshApplied, RefreshPlan, RefreshPreparation, RefreshRequest, VcsOps,
};

const KIND: &str = "workspace_refresh";
const MAX_START_LINE_BYTES: usize = 1024;

#[cfg(test)]
mod tests;

/// The outcome of an attempted live-fork catch-up.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshStatus {
    /// The copy now includes its parent's clean current state.
    Refreshed,
    /// A state or backend restriction left the old base unchanged.
    Skipped,
    /// The parent's state conflicted with the child; nothing was applied.
    Conflict,
}

/// Parent receipt and child Start share this exact refresh result.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RefreshReport {
    /// Whether catch-up succeeded, was skipped or conflicted.
    pub status: RefreshStatus,
    /// New base after a successful catch-up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Conflicting paths and kinds, at most sixteen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<ConflictPath>,
    /// Full number of conflicting paths.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub paths_total: u32,
    /// Further paths were omitted from the receipt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paths_truncated: bool,
    /// A bounded reason when the copy was not refreshed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if takes a reference"
)]
fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl RefreshReport {
    fn skipped(reason: &str) -> Self {
        Self {
            status: RefreshStatus::Skipped,
            base: None,
            paths: Vec::new(),
            paths_total: 0,
            paths_truncated: false,
            reason: Some(bounded_text(reason, 240)),
        }
    }

    pub(super) fn start_line(&self) -> String {
        let line = match self.status {
            RefreshStatus::Refreshed => format!(
                "Your workspace was refreshed onto the parent's current state (base {}).",
                self.base
                    .as_deref()
                    .unwrap_or_default()
                    .chars()
                    .take(12)
                    .collect::<String>()
            ),
            RefreshStatus::Skipped => format!(
                "Your workspace was not refreshed: {}; your base is unchanged.",
                self.reason.as_deref().unwrap_or("refresh was skipped")
            ),
            RefreshStatus::Conflict => format!(
                "Your workspace was not refreshed: merging the parent's current state conflicts in {} path(s): {}; your base is unchanged.",
                self.paths_total,
                bounded_text(
                    &self
                        .paths
                        .iter()
                        .map(|p| format!("{} ({})", p.path, p.kind))
                        .collect::<Vec<_>>()
                        .join(", "),
                    768
                )
            ),
        };
        bounded_text(&line, MAX_START_LINE_BYTES)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Intent {
    operation: String,
    request_digest: String,
    #[serde(flatten)]
    phase: Phase,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Phase {
    Preparing { request: RefreshRequest },
    Applying { plan: Box<RefreshPlan> },
    Done { report: Option<RefreshReport> },
}

#[derive(Clone, Debug)]
pub(crate) struct RefreshTicket {
    workspace: String,
    operation: String,
    recover: bool,
    pub(super) attach: bool,
    step: Step,
}

#[derive(Clone, Debug)]
enum Step {
    Prepare(RefreshRequest),
    Apply(Box<RefreshPlan>),
}

#[derive(Debug)]
pub(crate) enum RefreshAnswer {
    Prepared(RefreshPreparation),
    Applied(RefreshApplied),
}

impl RefreshTicket {
    pub(crate) async fn run(&self, vcs: &dyn VcsOps) -> Result<RefreshAnswer, VcsError> {
        match &self.step {
            Step::Prepare(request) => vcs
                .prepare_refresh(request)
                .await
                .map(RefreshAnswer::Prepared),
            Step::Apply(plan) => vcs
                .apply_refresh(plan, self.recover)
                .await
                .map(RefreshAnswer::Applied),
        }
    }
}

fn corrupt() -> TopologyError {
    TopologyError::new(
        TopologyErrorCode::WorkspaceRecoveryRequired,
        "workspace refresh ownership changed; inspect the copy before following up",
    )
}

impl ProductAgentTree {
    fn refresh_key(workspace: &str) -> String {
        format!("plak:{KIND}:{workspace}")
    }

    fn refresh_result(&self, operation: &str) -> Result<Option<Intent>, TopologyError> {
        self.operations
            .get(&format!("plak:workspace_refresh_result:{operation}"))
            .map(|op| serde_json::from_str(&op.canonical_request).map_err(|_| corrupt()))
            .transpose()
    }

    fn refresh_intent(&self, workspace: &str) -> Result<Option<Intent>, TopologyError> {
        self.operations
            .get(&Self::refresh_key(workspace))
            .map(|op| serde_json::from_str(&op.canonical_request).map_err(|_| corrupt()))
            .transpose()
    }

    pub(super) fn refresh_pending(&self, workspace: &str) -> bool {
        self.refresh_intent(workspace).map_or(true, |intent| {
            intent.is_some_and(|intent| !matches!(intent.phase, Phase::Done { .. }))
        })
    }

    pub(crate) fn abandon_queued_refresh_dispatches(&mut self) {
        for ticket in std::mem::take(&mut self.effects.refreshes) {
            self.inflight_refreshes.remove(&ticket.workspace);
        }
    }

    fn write_refresh(&mut self, workspace: &str, intent: &Intent) -> Result<(), TopologyError> {
        let id = Self::refresh_key(workspace);
        let canonical = canonical_json(intent)?;
        let owner = self.workspace_producer(workspace)?;
        let record = OperationRecord {
            operation_id: id.clone(),
            caller_agent_id: owner,
            tool_name: KIND.into(),
            request_digest: super::digest_text(&canonical),
            canonical_request: canonical,
            fold: None,
            terminal_code: None,
            terminal_response: matches!(intent.phase, Phase::Done { .. }).then(|| "settled".into()),
        };
        if matches!(intent.phase, Phase::Done { .. }) {
            let result_id = format!("plak:workspace_refresh_result:{}", intent.operation);
            self.operations.insert(
                result_id.clone(),
                OperationRecord {
                    operation_id: result_id,
                    tool_name: "workspace_refresh_result".into(),
                    ..record.clone()
                },
            );
        }
        self.operations.insert(id.clone(), record);
        self.bump();
        Ok(())
    }

    pub(crate) fn prepare_followup_refresh(
        &mut self,
        caller: &str,
        request: &SendAgentMessageRequest,
        operation: &str,
        jobs_running: bool,
    ) -> Result<(), TopologyError> {
        if self.operations.contains_key(operation) {
            return Ok(());
        }
        self.require_live()?;
        request
            .validate(caller)
            .map_err(|code| TopologyError::new(code, "invalid followup refresh request"))?;
        if request.purpose != Some(MessagePurpose::Followup) {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "refresh is followup only",
            ));
        }
        if !self
            .grant(caller)?
            .message_peers
            .contains(&request.agent_id)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "recipient is not a message peer",
            ));
        }
        let node = self.node(&request.agent_id)?.clone();
        let request_digest = super::digest_text(&canonical_json(request)?);
        if let Some(prior) = self.refresh_result(operation)? {
            return if prior.request_digest == request_digest {
                Ok(())
            } else {
                Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "retry content conflicts with the completed refresh",
                ))
            };
        }
        if let Some(intent) = self.refresh_intent(&node.workspace_id)? {
            if intent.operation == operation && intent.request_digest != request_digest {
                return Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "retry content conflicts with the prepared refresh",
                ));
            }
            if intent.operation == operation && matches!(intent.phase, Phase::Done { .. }) {
                return Ok(());
            }
            if !matches!(intent.phase, Phase::Done { .. }) {
                if self.inflight_refreshes.contains(&node.workspace_id) {
                    return Err(TopologyError::new(
                        TopologyErrorCode::WorkspaceRecoveryRequired,
                        "workspace refresh is already in flight; wait for its outcome",
                    ));
                }
                match intent.phase {
                    Phase::Applying { plan } => {
                        self.inflight_refreshes.insert(node.workspace_id.clone());
                        self.effects.refreshes.push(RefreshTicket {
                            workspace: node.workspace_id,
                            operation: intent.operation,
                            recover: true,
                            attach: false,
                            step: Step::Apply(plan),
                        });
                        return Ok(());
                    }
                    Phase::Preparing { .. } => {
                        self.write_refresh(
                            &node.workspace_id,
                            &Intent {
                                operation: intent.operation,
                                request_digest: intent.request_digest,
                                phase: Phase::Done {
                                    report: Some(RefreshReport::skipped(
                                        "refresh preparation was interrupted",
                                    )),
                                },
                            },
                        )?;
                        return self.prepare_followup_refresh(
                            caller,
                            request,
                            operation,
                            jobs_running,
                        );
                    }
                    Phase::Done { .. } => {}
                }
            }
        }
        self.admit_followup(
            &node.agent_id,
            node.status,
            !self
                .current_attempt(&node.agent_id)
                .is_some_and(|attempt| attempt.status.is_stopped())
                && self.has_current_turn_owner(&node.agent_id),
            request.output_schema_json.as_deref(),
        )?;
        self.queue_new_refresh(node, request, operation, request_digest, jobs_running)
    }

    fn queue_new_refresh(
        &mut self,
        node: super::NodeRecord,
        request: &SendAgentMessageRequest,
        operation: &str,
        request_digest: String,
        jobs_running: bool,
    ) -> Result<(), TopologyError> {
        let row = self
            .workspaces
            .get(&node.workspace_id)
            .ok_or_else(corrupt)?;
        let eligible = row.mode == WorkspaceMode::Fork && row.input_candidate_id.is_none();
        if request.refresh == Some(false) {
            return Ok(());
        }
        if !eligible {
            return if request.refresh == Some(true) {
                Err(TopologyError::new(
                    TopologyErrorCode::SchemaMismatch,
                    "refresh requires a fork from the parent's live state, not a candidate or snapshot copy",
                ))
            } else {
                Ok(())
            };
        }
        // Refuse predictable send failures before publishing a refresh intent
        // or touching the copy, including the largest possible host Start line.
        self.check_mailbox_budget(
            &node.agent_id,
            request
                .message
                .len()
                .saturating_add(2 + MAX_START_LINE_BYTES),
        )?;
        if jobs_running {
            return self.write_refresh(
                &node.workspace_id,
                &Intent {
                    operation: operation.into(),
                    request_digest,
                    phase: Phase::Done {
                        report: Some(RefreshReport::skipped("child has running background jobs")),
                    },
                },
            );
        }
        let parent = self.node(node.parent_agent_id.as_deref().ok_or_else(corrupt)?)?;
        let refresh = RefreshRequest {
            parent: parent.path.clone(),
            child: row.path.clone(),
            base: row.base_commit.clone().ok_or_else(corrupt)?,
            branch: node.branch.clone().unwrap_or_default(),
            namespace: super::digest_text(&self.tree_id),
            agent: node.agent_id,
        };
        self.write_refresh(
            &node.workspace_id,
            &Intent {
                operation: operation.into(),
                request_digest,
                phase: Phase::Preparing {
                    request: refresh.clone(),
                },
            },
        )?;
        self.inflight_refreshes.insert(node.workspace_id.clone());
        self.effects.refreshes.push(RefreshTicket {
            workspace: node.workspace_id,
            operation: operation.into(),
            recover: false,
            attach: false,
            step: Step::Prepare(refresh),
        });
        Ok(())
    }

    pub(super) fn refresh_report(
        &self,
        workspace: &str,
        operation: Option<&str>,
    ) -> Option<RefreshReport> {
        let intent = operation
            .and_then(|id| self.refresh_result(id).ok().flatten())
            .or_else(|| self.refresh_intent(workspace).ok().flatten())?;
        if Some(intent.operation.as_str()) != operation {
            return None;
        }
        match intent.phase {
            Phase::Done { report } => report,
            _ => None,
        }
    }

    pub(super) fn queue_refresh_recovery(&mut self) {
        let pending: Vec<_> = self
            .workspaces
            .keys()
            .filter_map(|ws| {
                self.refresh_intent(ws)
                    .ok()
                    .flatten()
                    .map(|intent| (ws.clone(), intent))
            })
            .collect();
        for (workspace, intent) in pending {
            match intent.phase {
                Phase::Preparing { .. } => {
                    let finished = Intent {
                        operation: intent.operation,
                        request_digest: intent.request_digest,
                        phase: Phase::Done {
                            report: Some(RefreshReport::skipped(
                                "refresh preparation was interrupted",
                            )),
                        },
                    };
                    if let Err(error) = self.write_refresh(&workspace, &finished) {
                        self.record_fault(&self.root_agent_id.clone(), error.message);
                    }
                }
                Phase::Applying { plan } => {
                    if self.inflight_refreshes.insert(workspace.clone()) {
                        self.effects.refreshes.push(RefreshTicket {
                            workspace,
                            operation: intent.operation,
                            recover: true,
                            attach: true,
                            step: Step::Apply(plan),
                        });
                    }
                }
                Phase::Done { .. } => {}
            }
        }
    }

    pub(super) fn complete_refresh(
        &mut self,
        ticket: RefreshTicket,
        answer: Result<RefreshAnswer, VcsError>,
    ) -> Result<(), TopologyError> {
        let mut intent = self
            .refresh_intent(&ticket.workspace)?
            .ok_or_else(corrupt)?;
        if intent.operation != ticket.operation {
            return Err(corrupt());
        }
        let matches = match (&intent.phase, &ticket.step) {
            (Phase::Preparing { request }, Step::Prepare(expected)) => request == expected,
            (Phase::Applying { plan }, Step::Apply(expected)) => plan == expected,
            _ => false,
        };
        if !matches {
            return Err(corrupt());
        }
        self.inflight_refreshes.remove(&ticket.workspace);
        let report = match (&ticket.step, answer) {
            (
                Step::Prepare(expected),
                Ok(RefreshAnswer::Prepared(RefreshPreparation::Ready(plan))),
            ) => {
                return self.record_prepared_refresh(&ticket, expected, plan, &mut intent);
            }
            (Step::Prepare(_), Ok(RefreshAnswer::Prepared(RefreshPreparation::Unchanged))) => None,
            (
                Step::Prepare(_),
                Ok(RefreshAnswer::Prepared(RefreshPreparation::Skipped(reason))),
            ) => Some(RefreshReport::skipped(&reason)),
            (
                Step::Prepare(_),
                Ok(RefreshAnswer::Prepared(RefreshPreparation::Conflict(paths))),
            ) => Some(RefreshReport {
                status: RefreshStatus::Conflict,
                base: None,
                paths_total: u32::try_from(paths.len()).unwrap_or(u32::MAX),
                paths_truncated: paths.len() > 16,
                paths: paths.into_iter().take(16).collect(),
                reason: None,
            }),
            (Step::Apply(plan), Ok(RefreshAnswer::Applied(RefreshApplied::Refreshed))) => {
                Some(self.record_refreshed_copy(&ticket.workspace, plan)?)
            }
            (Step::Apply(_), Ok(RefreshAnswer::Applied(RefreshApplied::Abandoned(reason)))) => {
                self.settle_workspace_faults(&ticket.workspace);
                Some(RefreshReport::skipped(&reason))
            }
            (Step::Apply(_), Err(error))
                if error.kind == VcsErrorKind::RecoveryRequired
                    || self.workspace_has_vcs_fault(&ticket.workspace) =>
            {
                self.record_refresh_fault(&ticket.workspace, &error.detail);
                return Err(TopologyError::new(
                    TopologyErrorCode::WorkspaceRecoveryRequired,
                    error.detail,
                ));
            }
            (_, Err(error)) => Some(RefreshReport::skipped(&error.detail)),
            _ => return Err(corrupt()),
        };
        intent.phase = Phase::Done { report };
        self.write_refresh(&ticket.workspace, &intent)
    }

    fn record_prepared_refresh(
        &mut self,
        ticket: &RefreshTicket,
        expected: &RefreshRequest,
        plan: Box<RefreshPlan>,
        intent: &mut Intent,
    ) -> Result<(), TopologyError> {
        if plan.request != *expected
            || self
                .workspaces
                .get(&ticket.workspace)
                .and_then(|row| row.base_commit.as_deref())
                != Some(plan.request.base.as_str())
        {
            return Err(corrupt());
        }
        intent.phase = Phase::Applying { plan: plan.clone() };
        self.write_refresh(&ticket.workspace, intent)?;
        self.inflight_refreshes.insert(ticket.workspace.clone());
        self.effects.refreshes.push(RefreshTicket {
            step: Step::Apply(plan),
            recover: false,
            attach: false,
            ..ticket.clone()
        });
        Ok(())
    }

    fn record_refreshed_copy(
        &mut self,
        workspace: &str,
        plan: &RefreshPlan,
    ) -> Result<RefreshReport, TopologyError> {
        let row = self.workspaces.get_mut(workspace).ok_or_else(corrupt)?;
        row.base_commit = Some(plan.parent.clone());
        row.base_content = Some(plan.parent_content.clone());
        row.base_change_id = None;
        self.bump_workspace_generation(workspace);
        self.settle_workspace_faults(workspace);
        Ok(RefreshReport {
            status: RefreshStatus::Refreshed,
            base: Some(plan.parent.clone()),
            paths: Vec::new(),
            paths_total: 0,
            paths_truncated: false,
            reason: None,
        })
    }
}
