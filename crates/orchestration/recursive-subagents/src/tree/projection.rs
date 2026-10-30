//! JSON projections of the tree: `list_agents` rows, wait rows, and the
//! per-node shapes.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{
    AttemptStatus, CandidateRecord, ListAgentsOutput, ListAgentsRequest, NodeRecord, NodeStatus,
    ProductAgentTree, RecoveryNeed, TopologyError, TopologyErrorCode, TopologyGrant, VcsOps,
    WaitAgentRow, bounded_text,
};

/// The most prompt or result text one `list_agents` row carries; a wait on
/// the id returns the whole result.
const LIST_TEXT_PREVIEW_BYTES: usize = 1024;

fn candidate_details(candidate: &CandidateRecord) -> serde_json::Value {
    let mut value = serde_json::json!({
        "candidate_id": candidate.candidate_id,
        "candidate_kind": candidate.kind,
    });
    for (key, id) in [
        ("candidate_commit", &candidate.commit),
        ("candidate_base", &candidate.base_commit),
    ] {
        if !id.is_empty() {
            value[key] = serde_json::json!(id);
        }
    }
    for (key, id) in [
        ("source_candidate", &candidate.source_candidate_id),
        ("resolves", &candidate.resolves),
        ("adopted_via", &candidate.adopted_via),
    ] {
        if let Some(id) = id {
            value[key] = serde_json::json!(id);
        }
    }
    if let Some(target) = &candidate.conflict_target {
        value["conflict_target"] = serde_json::json!(target);
    }
    if let Some(snapshot) = &candidate.reverted_at
        && candidate.adopted_by.is_none()
    {
        value["reverted_at"] = serde_json::json!(snapshot);
        value["reverted_by"] = serde_json::json!(candidate.reverted_by);
    }
    if !candidate.conflict_paths.is_empty() {
        value["conflict_paths"] =
            serde_json::json!(candidate.conflict_paths.iter().take(16).collect::<Vec<_>>());
        if candidate.conflict_paths.len() > 16 {
            value["conflict_paths_truncated"] = serde_json::Value::Bool(true);
        }
    }
    value
}

/// The occupancy one `list_agents` render wants for one node: whether its
/// copy is dirty, and how far it has moved from its parent's.
#[derive(Clone, Copy, Debug, Default)]
pub struct NodeOccupancy {
    pub dirty: bool,
    pub ahead_behind: Option<(u64, u64)>,
}

/// The paths one `list_agents` render owes the VCS, read from the rows under
/// the tree mutex. Holds no tree reference, so
/// [`Self::run`] cannot be called with the guard alive by construction of
/// its arguments — the same shape the seal, adopt, and mint tickets use.
#[derive(Clone, Debug, Default)]
pub struct OccupancyProbe {
    rows: Vec<(String, PathBuf, Option<PathBuf>, Option<String>)>,
}

impl OccupancyProbe {
    /// Probes every listed copy with the mutex released.
    #[must_use]
    pub fn run(self, vcs: &dyn VcsOps) -> Occupancy {
        let mut parents = plakous_workspace::occupancy_vcs::ParentBehindCache::new();
        Occupancy(
            self.rows
                .into_iter()
                .map(|(agent_id, path, parent_path, base)| {
                    let dirty = vcs.dirty(&path);
                    let ahead_behind = parent_path.and_then(|parent| {
                        vcs.ahead_behind_cached(&path, &parent, base.as_deref(), &mut parents)
                    });
                    (
                        agent_id,
                        NodeOccupancy {
                            dirty,
                            ahead_behind,
                        },
                    )
                })
                .collect(),
        )
    }
}

/// What [`OccupancyProbe::run`] read, by agent id. A node the probe did not
/// see — one spawned between the two phases — renders as a clean copy with
/// no ahead/behind, which is what a copy minted since the probe is.
#[derive(Clone, Debug, Default)]
pub struct Occupancy(BTreeMap<String, NodeOccupancy>);

impl Occupancy {
    fn get(&self, agent_id: &str) -> NodeOccupancy {
        self.0.get(agent_id).copied().unwrap_or_default()
    }
}

impl ProductAgentTree {
    /// Disposable compaction counters rebuilt by the child-feed projector.
    pub fn attempt_compaction(
        &self,
        attempt_id: &super::AttemptId,
    ) -> Option<plakous_host_protocol::views::AttemptCompaction> {
        self.compaction_views.get(attempt_id).copied()
    }

    /// Renders `list_agents` through the tree's own port: the in-process
    /// convenience for the tree's own tests. Production drives
    /// [`Self::occupancy_probe`] / [`Self::list_agents_with`] through the
    /// runtime so the probes run with the tree mutex released.
    pub fn list_agents(
        &self,
        caller_id: &str,
        request: &ListAgentsRequest,
    ) -> Result<ListAgentsOutput, TopologyError> {
        let occupancy = self.occupancy_probe(caller_id, request)?.run(self.vcs());
        self.list_agents_with(caller_id, request, &occupancy)
    }

    /// The paths the render will ask the VCS about, read under the lock.
    /// Same visibility rules as the render itself, so a caller never probes
    /// a copy it may not see.
    pub fn occupancy_probe(
        &self,
        caller_id: &str,
        request: &ListAgentsRequest,
    ) -> Result<OccupancyProbe, TopologyError> {
        let mut probe = OccupancyProbe::default();
        for id in self.listed_ids(caller_id, request)? {
            let Ok(node) = self.node(&id) else {
                continue;
            };
            let parent = node
                .parent_agent_id
                .as_ref()
                .and_then(|parent| self.nodes.get(parent))
                .map(|parent| parent.path.clone());
            probe.rows.push((
                id,
                node.path.clone(),
                parent,
                self.workspaces
                    .get(&node.workspace_id)
                    .and_then(|row| row.base_commit.clone()),
            ));
        }
        Ok(probe)
    }

    /// The render half: rows under the lock, occupancy from a snapshot the
    /// caller took with the lock released.
    pub fn list_agents_with(
        &self,
        caller_id: &str,
        request: &ListAgentsRequest,
        occupancy: &Occupancy,
    ) -> Result<ListAgentsOutput, TopologyError> {
        let grant = self.grant(caller_id)?;
        let full = grant.may_spawn;
        let mut agents = Vec::new();
        for id in self.listed_ids(caller_id, request)? {
            let node = self.node(&id)?;
            if full {
                agents.push(self.full_list_json(node, occupancy.get(&id))?);
            } else {
                agents.push(self.short_list_json(node, grant)?);
            }
        }
        Ok(ListAgentsOutput {
            revision: self.revision,
            agents,
            inbox: self.inbox_for(caller_id, request.include_read.unwrap_or(false)),
        })
    }

    /// The rows one `list_agents` call renders, in tree order: the single
    /// visibility rule both the probe and the render read.
    fn listed_ids(
        &self,
        caller_id: &str,
        request: &ListAgentsRequest,
    ) -> Result<Vec<String>, TopologyError> {
        let grant = self.grant(caller_id)?;
        let include_self = request.include_self.unwrap_or(false);
        let include_completed = request.include_completed.unwrap_or(false);
        let full = grant.may_spawn;
        if !full && grant.message_peers.is_empty() {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "caller has no list grant",
            ));
        }
        let mut ordered = Vec::new();
        self.preorder(self.root_agent_id.as_str(), &mut ordered);
        let mut listed = Vec::new();
        for id in ordered {
            if id == caller_id && !include_self {
                continue;
            }
            let node = self.node(&id)?;
            // A node waiting on recovery needs its manager's action, so it is
            // listed like a live one.
            if node.status.is_finished()
                && node.status != NodeStatus::RecoveryRequired
                && !include_completed
            {
                continue;
            }
            let visible = grant.visible_ids.iter().any(|item| item == &id)
                || grant.message_peers.iter().any(|item| item == &id)
                || (full && grant.managed_ids.iter().any(|item| item == &id));
            if !visible {
                continue;
            }
            listed.push(id);
        }
        Ok(listed)
    }

    pub(super) fn wait_row(&self, id: &str) -> Result<WaitAgentRow, TopologyError> {
        let node = self.node(id)?;
        let attempt = self.current_attempt(id);
        Ok(WaitAgentRow {
            agent_id: id.to_owned(),
            status: node.presentation_status(self.state),
            attempt_id: node.attempt_id.clone(),
            attempt: attempt.map_or(AttemptStatus::Closed, |row| row.status),
            candidate: attempt.and_then(|row| row.candidate_id.clone()),
            output_json: attempt.and_then(|row| row.output_json.clone()),
            blocked: attempt.and_then(|row| row.blocked.clone()),
            failure: attempt.and_then(super::AttemptRecord::presented_failure),
            usage: attempt
                .filter(|row| row.status.is_stopped())
                .map(|row| row.usage),
            waiting_on: self.waiting_on(id).cloned(),
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "agent list presentation projects bounded nodes, mail, workspace and attempt evidence in one response"
    )]
    pub(super) fn full_list_json(
        &self,
        node: &NodeRecord,
        occupancy: NodeOccupancy,
    ) -> Result<serde_json::Value, TopologyError> {
        let workspace = self.workspaces.get(&node.workspace_id);
        let mut value = serde_json::json!({
            "agent_id": node.agent_id,
            "status": node.presentation_status(self.state),
            "description": node.description,
            "prompt_bytes": node.prompt.len(),
            "tuple_id": node.tuple_id,
            "workspace_id": node.workspace_id,
            "path": node.path.display().to_string(),
            "tier": node.tier,
            "dirty": occupancy.dirty,
        });
        if let Some(template) = &node.template {
            value["template"] = serde_json::Value::String(template.as_str().to_owned());
        }
        if let Some(attempt) = self.current_attempt(&node.agent_id) {
            value["attempt_id"] = serde_json::Value::String(attempt.attempt_id.to_string());
            value["attempt"] = serde_json::json!(attempt.status);
            if let Some(compaction) = self.attempt_compaction(&attempt.attempt_id) {
                value["compaction"] = serde_json::json!(compaction);
            }
            value["transient_resumes"] = serde_json::json!(
                self.transient_resume_count(&node.agent_id, &attempt.attempt_id)?
            );
            // per-attempt usage only. No cost, no quota balance,
            // and no subtree rollup reaches a prompt.
            if attempt.status.is_stopped() {
                value["usage"] = serde_json::json!(attempt.usage);
            }
            if let Some(blocked) = &attempt.blocked {
                value["blocked"] = serde_json::json!(blocked);
            }
            if let Some(failure) = attempt.presented_failure() {
                value["failure"] = serde_json::json!(failure);
            }
            if let Some(candidate) = &attempt.candidate_id {
                value["candidate"] = serde_json::Value::String(candidate.to_string());
                if let Some(row) = self.candidates.get(candidate) {
                    if let Some(details) = candidate_details(row).as_object() {
                        for (key, detail) in details {
                            if key != "candidate_id" {
                                value[key] = detail.clone();
                            }
                        }
                    }
                    if let Some(commit) = row.merged_at.as_ref() {
                        value["merged_at"] = serde_json::Value::String(commit.clone());
                    }
                }
            }
            if let Some(output) = &attempt.output_json {
                if output.len() <= LIST_TEXT_PREVIEW_BYTES {
                    value["output_json"] = serde_json::Value::String(output.clone());
                } else {
                    value["output_preview"] =
                        serde_json::Value::String(bounded_text(output, LIST_TEXT_PREVIEW_BYTES));
                    value["output_bytes"] = serde_json::Value::from(output.len());
                }
            }
        }
        let retained: Vec<_> = self
            .candidates
            .values()
            .filter(|candidate| {
                candidate.producing_agent_id == node.agent_id
                    && candidate.adopted_by.is_none()
                    && candidate.adopted_via.is_none()
                    && candidate.content != candidate.base_content
            })
            .map(candidate_details)
            .collect();
        if !retained.is_empty() {
            value["candidates"] = serde_json::json!(retained);
        }
        if self.conversation_retired(&node.agent_id) {
            value["conversation_retired"] = serde_json::Value::Bool(true);
        }
        if let Some(waiting_on) = self.waiting_on(&node.agent_id) {
            value["waiting_on"] = serde_json::json!(waiting_on);
        }
        if let Some(parent) = &node.parent_agent_id {
            value["parent_agent_id"] = serde_json::Value::String(parent.clone());
        }
        if let Some(branch) = &node.branch {
            value["branch"] = serde_json::Value::String(branch.clone());
        }
        if let Some((ahead, behind)) = occupancy.ahead_behind {
            value["ahead"] = serde_json::Value::from(ahead);
            value["behind"] = serde_json::Value::from(behind);
        }
        if !node.account.is_empty() {
            value["account"] = serde_json::Value::String(node.account.clone());
        }
        if let Some(workspace) = workspace {
            value["workspace_mode"] = serde_json::json!(workspace.mode);
        }
        match self.recovery_need(&node.agent_id)? {
            RecoveryNeed::NotRequired => {}
            RecoveryNeed::Session { .. } => match self.terminal_recovery_refusal(&node.agent_id) {
                Some(refusal) => {
                    value["remedy"] = serde_json::Value::String("close_agent".to_owned());
                    value["recovery"] = serde_json::Value::String(format!(
                        "{}: {}; close_agent ends the node",
                        refusal.kind, refusal.message
                    ));
                }
                None => {
                    value["remedy"] = serde_json::Value::String("recover_agent".to_owned());
                }
            },
            RecoveryNeed::Workspace { reason, .. } => {
                value["remedy"] = serde_json::Value::String("close_agent".to_owned());
                value["recovery"] = serde_json::Value::String(reason);
            }
        }
        Ok(value)
    }

    pub(super) fn short_list_json(
        &self,
        node: &NodeRecord,
        grant: &TopologyGrant,
    ) -> Result<serde_json::Value, TopologyError> {
        Ok(serde_json::json!({
            "agent_id": node.agent_id,
            "description": node.description,
            "prompt_bytes": node.prompt.len(),
            "status": node.status,
            "path": node.path.display().to_string(),
            "can_message": grant.message_peers.iter().any(|id| id == &node.agent_id),
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
