//! Closed semantic tree cuts; no VCS journal, live owner or execution grant crosses.

use super::{
    AgentCommand, AttemptId, AttemptRecord, AttemptStatus, CandidateId, CandidateRecord,
    ContinuationIntent, FoldAnchor, ForkedFrom, MailRecord, NodeRecord, NodeStatus,
    OperationRecord, OrchestrationSnapshot, PersistedTree, ProductAgentTree, TopologyGrant,
    TreeAdmissionState, TreeEpochs, WorkspaceRecord, canonical_json, digest_text, valid_id,
};
use crate::protocol::{MAX_MAILBOX_RECORDS, MAX_TREE_ATTEMPTS, MAX_TREE_NODES, WriterMode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

mod child_session;
mod memory;
mod refold;

#[cfg(test)]
mod tests;

/// Semantic ownership-move payload, embedded only in export document v13.
/// Changes to its encoding require the export version bump too.
#[doc(hidden)]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DormantTree {
    tree_id: String,
    root_agent_id: String,
    revision: u64,
    host_generation: u64,
    state: TreeAdmissionState,
    writer_mode: WriterMode,
    selected_tuple: String,
    launch_checkout: PathBuf,
    snapshot: OrchestrationSnapshot,
    epochs: TreeEpochs,
    forked_from: Option<ForkedFrom>,
    runner_ancestors: BTreeMap<String, BTreeSet<String>>,
    nodes: BTreeMap<String, NodeRecord>,
    grants: BTreeMap<String, TopologyGrant>,
    attempts: BTreeMap<AttemptId, AttemptRecord>,
    candidates: BTreeMap<CandidateId, CandidateRecord>,
    workspaces: BTreeMap<String, WorkspaceRecord>,
    messages: BTreeMap<String, MailRecord>,
    commands: BTreeMap<String, AgentCommand>,
    feed_cursors: BTreeMap<String, u64>,
    decisions: Vec<HostDecision>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostDecision {
    operation_id: String,
    agent_id: String,
    fold: FoldAnchor,
    body: DecisionBody,
    effects_reserved: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum DecisionBody {
    Continuation(ContinuationIntent),
    Operator(serde_json::Value),
}

impl HostDecision {
    fn operation(&self) -> Result<OperationRecord, String> {
        let mut row = match &self.body {
            DecisionBody::Continuation(intent) => intent
                .to_operation(&self.agent_id, Some(self.fold))
                .map_err(|error| error.message)?,
            DecisionBody::Operator(value) => {
                let intent: super::operator_intents::OperatorIntent =
                    serde_json::from_value(value.clone())
                        .map_err(|_| "invalid move operator decision")?;
                let canonical_request = canonical_json(&intent).map_err(|error| error.message)?;
                OperationRecord {
                    operation_id: self.operation_id.clone(),
                    caller_agent_id: self.agent_id.clone(),
                    tool_name: intent.kind().into(),
                    request_digest: digest_text(&canonical_request),
                    canonical_request,
                    fold: Some(self.fold),
                    terminal_code: None,
                    terminal_response: None,
                }
            }
        };
        if row.operation_id != self.operation_id {
            return Err("move continuation identity changed".into());
        }
        row.fold = Some(self.fold);
        if self.effects_reserved {
            row.terminal_response = Some("effects_reserved".into());
        }
        Ok(row)
    }
}

impl DormantTree {
    /// Captures a quiescent tree under the caller's custody/admission guards.
    ///
    /// # Errors
    /// Refuses unfinished effects, startup reservations, VCS work or active turns.
    pub fn capture(tree: &ProductAgentTree) -> Result<Self, String> {
        if tree.owes_effects()
            || !tree.outstanding_seals.is_empty()
            || !tree.inflight_adopts.is_empty()
            || !tree.inflight_refreshes.is_empty()
            || !tree.minting.is_empty()
            || tree.pending_spawns != 0
            || !tree.delivery_claims.is_empty()
            || !tree.start_reservations.is_empty()
            || !tree.parked_waits.is_empty()
            || tree.nodes.values().any(|node| {
                node.fold_order.is_none()
                    || node.active_turn
                    || matches!(node.status, NodeStatus::Pending | NodeStatus::Sealing)
            })
            || tree
                .workspaces
                .values()
                .any(|workspace| workspace.status.is_in_flight())
        {
            return Err("tree still has startup, turn, mailbox or workspace work in flight".into());
        }
        let mut decisions = Vec::new();
        for operation in tree.operations.values() {
            if child_session::resolved(tree, operation)? {
                continue;
            }
            if matches!(operation.tool_name.as_str(), "vcs_stage" | "epoch_pending") {
                return Err("tree still has an unresolved workspace or epoch journal".into());
            }
            if let Some(intent) =
                ContinuationIntent::from_operation(operation).map_err(|error| error.message)?
            {
                decisions.push(HostDecision {
                    operation_id: operation.operation_id.clone(),
                    agent_id: operation.caller_agent_id.clone(),
                    fold: operation
                        .fold
                        .ok_or("continuation has no semantic anchor")?,
                    body: DecisionBody::Continuation(intent),
                    effects_reserved: operation.terminal_response.as_deref()
                        == Some("effects_reserved"),
                });
            } else if matches!(
                operation.tool_name.as_str(),
                "submitted" | "close" | "interrupt" | "recovery_abandoned"
            ) {
                let value = serde_json::from_str(&operation.canonical_request)
                    .map_err(|_| "invalid move operator decision")?;
                let decision = HostDecision {
                    operation_id: operation.operation_id.clone(),
                    agent_id: operation.caller_agent_id.clone(),
                    fold: operation
                        .fold
                        .ok_or("operator decision has no semantic anchor")?,
                    body: DecisionBody::Operator(value),
                    effects_reserved: operation.terminal_response.as_deref()
                        == Some("effects_reserved"),
                };
                if decision.operation()?.canonical_request != operation.canonical_request
                    || digest_text(&operation.canonical_request) != operation.request_digest
                {
                    return Err("move operator decision is not canonical".into());
                }
                decisions.push(decision);
            } else {
                let joined = operation
                    .fold
                    .is_some_and(|fold| tree.fold_anchor_applied(&operation.caller_agent_id, fold));
                let settled =
                    operation.terminal_code.is_some() || operation.terminal_response.is_some();
                if !joined && !settled {
                    return Err("tree has an unresolved operation or host intent".into());
                }
            }
        }
        let cut = Self {
            tree_id: tree.tree_id.clone(),
            root_agent_id: tree.root_agent_id.clone(),
            revision: tree.revision,
            host_generation: tree.host_generation,
            state: tree.state,
            writer_mode: tree.writer_mode,
            selected_tuple: tree.selected_tuple.clone(),
            launch_checkout: tree.launch_checkout.clone(),
            snapshot: tree.header_snapshot().clone(),
            epochs: tree.epochs().clone(),
            forked_from: tree.forked_from().cloned(),
            runner_ancestors: tree.runner_ancestors().clone(),
            nodes: tree.nodes.clone(),
            grants: tree.grants.clone(),
            attempts: tree.attempts.clone(),
            candidates: tree.candidates.clone(),
            workspaces: tree.workspaces.clone(),
            messages: tree.messages.clone(),
            commands: tree.commands.clone(),
            feed_cursors: tree.feed_cursors.clone(),
            decisions,
        };
        cut.validate()?;
        Ok(cut)
    }

    /// The addressable source tree, unchanged by the move.
    #[must_use]
    pub fn tree_id(&self) -> &str {
        &self.tree_id
    }

    /// Source node identities and settled semantic data, without reopening a tree.
    #[must_use]
    pub const fn nodes(&self) -> &BTreeMap<String, NodeRecord> {
        &self.nodes
    }

    /// Exact semantic baseline in the destination's native tree schema.
    /// No custody is taken and no reopen law or execution driver runs.
    ///
    /// # Errors
    /// Refuses an invalid semantic base instead of dropping malformed decisions.
    pub fn installed_rows(&self) -> Result<PersistedTree, String> {
        self.validate()?;
        let mut rows = self.clone().into_rows()?;
        rows.0.move_base = Some(std::sync::Arc::new(self.clone()));
        Ok(rows)
    }

    /// Retained-empty runtime feed floors covered by this semantic base.
    #[must_use]
    pub const fn feed_floors(&self) -> &BTreeMap<String, u64> {
        &self.feed_cursors
    }

    /// Content identity of this closed semantic base, without native codec bytes.
    ///
    /// # Errors
    /// Refuses malformed semantic data or an encoding failure.
    pub fn digest(&self) -> Result<String, String> {
        self.validate()?;
        serde_json::to_string(self)
            .map(|text| digest_text(&text))
            .map_err(|_| "could not encode semantic tree base".into())
    }

    /// Validates bounded identities, dormant states and semantic references.
    ///
    /// # Errors
    /// Refuses malformed or non-dormant data before writing any native store.
    pub fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.tree_id)
            || !valid_id(&self.root_agent_id)
            || self.revision == u64::MAX
            || self.host_generation == u64::MAX
            || self.nodes.is_empty()
            || self.nodes.len() > MAX_TREE_NODES
            || self.attempts.len() > MAX_TREE_ATTEMPTS
            || self.candidates.len() > MAX_TREE_ATTEMPTS
            || self.workspaces.len() > MAX_TREE_ATTEMPTS
            || self.commands.len() > MAX_MAILBOX_RECORDS
            || self.decisions.len() > MAX_MAILBOX_RECORDS
            || self.messages.len() > MAX_MAILBOX_RECORDS
            || !self.snapshot.digest_matches()
            || !self.epochs.is_coherent()
            || self
                .nodes
                .get(&self.root_agent_id)
                .is_none_or(|root| root.parent_agent_id.is_some() || root.depth != 0)
        {
            return Err("invalid ownership-move tree inventory".into());
        }
        for (id, node) in &self.nodes {
            if id != &node.agent_id
                || !valid_id(id)
                || node.active_turn
                || node.fold_order.is_none()
                || matches!(node.status, NodeStatus::Pending | NodeStatus::Sealing)
                || !self.workspaces.contains_key(&node.workspace_id)
                || !self.epochs.holds(node.epoch_no)
                || node.parent_agent_id.as_ref().is_some_and(|parent| {
                    self.nodes
                        .get(parent)
                        .is_none_or(|parent| parent.depth.checked_add(1) != Some(node.depth))
                })
                || node.attempt_id.as_ref().is_some_and(|attempt| {
                    self.attempts
                        .get(attempt)
                        .is_none_or(|attempt| attempt.agent_id != *id)
                })
            {
                return Err("invalid or active ownership-move node".into());
            }
        }
        for (id, attempt) in &self.attempts {
            if id != &attempt.attempt_id
                || !self.nodes.contains_key(&attempt.agent_id)
                || attempt.status == AttemptStatus::Submitted
            {
                return Err("invalid or sealing ownership-move attempt".into());
            }
        }
        if self.workspaces.iter().any(|(id, workspace)| {
            id != &workspace.workspace_id
                || workspace.status.is_in_flight()
                || !self.nodes.contains_key(&workspace.owner_agent_id)
        }) || self.messages.iter().any(|(id, message)| {
            id != &message.message_id
                || !self.nodes.contains_key(&message.from)
                || !self.nodes.contains_key(&message.to)
        }) || self.commands.iter().any(|(id, command)| {
            id != &command.command_id || !self.nodes.contains_key(&command.recipient)
        }) {
            return Err("invalid ownership-move workspace, mailbox or command reference".into());
        }
        let mut decisions = BTreeSet::new();
        for decision in &self.decisions {
            if !self.nodes.contains_key(&decision.agent_id)
                || !decisions.insert(&decision.operation_id)
            {
                return Err("unknown move decision owner".into());
            }
            decision.operation()?;
        }
        Ok(())
    }

    /// Reconstructs rows with new native checkpoint heads and no VCS or driver work.
    ///
    /// # Errors
    /// Refuses a missing head mapping or any change to a public runtime identity.
    pub fn reconstruct(
        mut self,
        heads: &BTreeMap<String, String>,
    ) -> Result<PersistedTree, String> {
        self.validate()?;
        for node in self.nodes.values_mut() {
            if let Some(locator) = &mut node.session_locator {
                let target = heads.get(locator).ok_or("missing move node checkpoint")?;
                let source: plakous::sessions::SessionLocator =
                    locator.parse().map_err(|_| "invalid move locator")?;
                let next: plakous::sessions::SessionLocator =
                    target.parse().map_err(|_| "invalid move target locator")?;
                if source.session_id() != next.session_id()
                    || source.branch_id() != next.branch_id()
                    || source.checkpoint_id() == next.checkpoint_id()
                {
                    return Err("move changed a node's runtime identity".into());
                }
                locator.clone_from(target);
            }
        }
        self.revision += 1;
        self.host_generation += 1;
        self.installed_rows()
    }

    fn into_rows(self) -> Result<PersistedTree, String> {
        let mut operations = self
            .decisions
            .iter()
            .map(|decision| {
                let row = decision.operation()?;
                Ok((row.operation_id.clone(), row))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        for (id, node) in &self.nodes {
            if id != &self.root_agent_id
                && let Some(locator) = &node.session_locator
            {
                let marker = child_session::record(id, locator);
                operations.insert(marker.operation_id.clone(), marker);
            }
        }
        Ok(PersistedTree::from_rows(
            self.tree_id,
            self.root_agent_id,
            self.revision,
            self.host_generation,
            self.state,
            self.writer_mode,
            self.selected_tuple,
            self.launch_checkout,
            self.snapshot,
            self.nodes,
            self.grants,
            self.attempts,
            self.candidates,
            self.workspaces,
            self.messages,
            operations,
        )
        .with_feed_rows(self.commands, self.feed_cursors)
        .with_lineage_rows(self.epochs, self.forked_from, self.runner_ancestors))
    }
}
