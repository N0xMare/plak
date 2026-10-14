//! JSON projections of the tree: `list_agents` rows, wait rows, and the
//! per-node shapes.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{
    AttemptStatus, ListAgentsOutput, ListAgentsRequest, NodeRecord, NodeStatus, ProductAgentTree,
    RecoveryNeed, TopologyError, TopologyErrorCode, TopologyGrant, VcsOps, WaitAgentRow,
    bounded_text,
};

/// The most prompt or result text one `list_agents` row carries; a wait on
/// the id returns the whole result.
const LIST_TEXT_PREVIEW_BYTES: usize = 1024;

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
    rows: Vec<(String, PathBuf, Option<PathBuf>)>,
}

impl OccupancyProbe {
    /// Probes every listed copy with the mutex released.
    #[must_use]
    pub fn run(self, vcs: &dyn VcsOps) -> Occupancy {
        Occupancy(
            self.rows
                .into_iter()
                .map(|(agent_id, path, parent_path)| {
                    let dirty = vcs.dirty(&path);
                    let ahead_behind =
                        parent_path.and_then(|parent| vcs.ahead_behind(&path, &parent));
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
            probe.rows.push((id, node.path.clone(), parent));
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
                if let Some(commit) = self
                    .candidates
                    .get(candidate)
                    .and_then(|row| row.merged_at.as_ref())
                {
                    value["merged_at"] = serde_json::Value::String(commit.clone());
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
                    && candidate.content != candidate.base_content
            })
            .map(|candidate| &candidate.candidate_id)
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
mod tests {
    use super::NodeOccupancy;
    use crate::tree::test_support::*;
    use crate::tree::vcs_fixtures::*;
    use crate::tree::*;

    #[test]
    fn failed_candidate_rows_name_the_partial_candidate_and_fresh_spawn_recipe() {
        for kind in ["turn_failed", "context_exhausted"] {
            let (_tmp, mut tree) = tree();
            let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
            let candidate = CandidateId::from_persisted(format!("cand-{}", child.attempt_id));
            let attempt = tree.attempts.get_mut(&child.attempt_id).unwrap();
            attempt.candidate_id = Some(candidate.clone());
            attempt.failure = Some(AttemptFailure {
                kind: kind.into(),
                message: "original cause".into(),
            });
            let expected = format!(
                "partial candidate {candidate}; spawn(inputs=[\"{candidate}\"]) with a handoff of completed work and the remaining task"
            );
            let wait = tree.wait_row(&child.agent_id).unwrap();
            assert!(wait.failure.unwrap().message.contains(&expected));
            let list = tree
                .full_list_json(
                    tree.node(&child.agent_id).unwrap(),
                    NodeOccupancy::default(),
                )
                .unwrap();
            assert!(
                list["failure"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(&expected)
            );
            assert_eq!(
                tree.attempt(&child.agent_id)
                    .unwrap()
                    .failure
                    .as_ref()
                    .unwrap()
                    .message,
                "original cause"
            );
            tree.attempts.get_mut(&child.attempt_id).unwrap().status = AttemptStatus::Failed;
            tree.nodes.get_mut(&child.agent_id).unwrap().status = NodeStatus::Failed;
            tree.enqueue_result_mail(&child.agent_id);
            let notice: serde_json::Value =
                serde_json::from_str(&tree.pending_result_mail("root")[0].body).unwrap();
            assert_eq!(notice["continuation"], expected);
        }
    }

    /// A failed child must say why. `recovery_required` with a null output and
    /// no message left the operator nothing to act on.
    #[test]
    fn a_failed_attempt_carries_a_typed_reason_on_the_wait_row() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_fault(
            &spawned.agent_id,
            "child turn failed (Model): 429 from codex",
        );
        tree.record_host_materialization(&spawned.agent_id)
            .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();

        let failure = tree
            .attempt(&spawned.agent_id)
            .unwrap()
            .failure
            .clone()
            .expect("the attempt carries why it failed");
        assert_eq!(failure.kind, "session_materialization");
        assert!(failure.message.contains("429"), "{}", failure.message);

        let settled = tree
            .poll_wait("root", &wait_for(&spawned.agent_id))
            .unwrap()
            .unwrap();
        assert_eq!(settled.agents[0].failure.as_ref(), Some(&failure));
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: None,
                    include_completed: Some(true),
                },
            )
            .unwrap();
        assert_eq!(
            listed.agents[0]["failure"]["kind"],
            "session_materialization"
        );
    }

    /// A list row previews a long prompt and result; the wait row keeps the
    /// whole result.
    #[test]
    fn list_rows_preview_long_prompts_and_results() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let long = format!("{{\"text\":\"{}\"}}", "y".repeat(3000));
        for attempt in tree.attempts.values_mut() {
            attempt.output_json = Some(long.clone());
        }
        tree.nodes.get_mut(&spawned.agent_id).unwrap().prompt = "p".repeat(2000);
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: None,
                    include_completed: Some(true),
                },
            )
            .unwrap();
        let row = &listed.agents[0];
        assert!(row.get("output_json").is_none());
        assert_eq!(row["output_bytes"], long.len());
        assert_eq!(row["output_preview"].as_str().unwrap().len(), 1024);
        assert!(row.get("prompt").is_none());
        assert_eq!(row["prompt_bytes"], 2000);
        let waited = tree.wait_row(&spawned.agent_id).unwrap();
        assert_eq!(waited.output_json.as_deref(), Some(long.as_str()));
    }

    #[test]
    fn empty_working_set_does_not_advertise_spawn_agent() {
        let (_tmp, mut tree) = tree();
        assert!(
            tree.compiled_tool_names("root")
                .unwrap()
                .contains(&"spawn_agent")
        );
        tree.snapshot.roster.clear();
        tree.snapshot.digest = tree.snapshot.compute_digest();
        let names = tree.compiled_tool_names("root").unwrap();
        assert!(!names.contains(&"spawn_agent"));
        assert!(names.contains(&"list_agents"));
    }

    #[test]
    fn list_agents_reports_dirty_worktree_and_template() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&spawned.agent_id).unwrap().path.clone();
        std::fs::write(path.join("DIRTY_LIST.txt"), "dirty\n").unwrap();
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: Some(false),
                    include_completed: Some(false),
                },
            )
            .unwrap();
        let child = listed
            .agents
            .iter()
            .find(|row| row["agent_id"] == spawned.agent_id)
            .unwrap();
        assert_eq!(child["dirty"], true);
        assert_eq!(child["template"], "implement");
        assert_eq!(child["tier"], "mutate");
        assert_eq!(child["workspace_mode"], "fork");
        assert_eq!(child["attempt"], "running");
        assert_eq!(child["attempt_id"], spawned.attempt_id.as_str());
        assert!(child.get("candidate").is_none());
        assert!(child.get("lease_holder").is_none());
    }

    #[test]
    fn list_agents_reports_ahead_behind_on_git_worktree() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&spawned.agent_id).unwrap().path.clone();
        std::fs::write(path.join("AHEAD.txt"), "ahead\n").unwrap();
        git_commit(&path, &["AHEAD.txt"], "child ahead");
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: Some(false),
                    include_completed: Some(false),
                },
            )
            .unwrap();
        let child = listed
            .agents
            .iter()
            .find(|row| row["agent_id"] == spawned.agent_id)
            .unwrap();
        assert!(child.get("ahead").is_some());
        assert!(child.get("behind").is_some());
        assert!(child["ahead"].as_u64().unwrap_or(0) >= 1);
    }

    #[test]
    fn list_agents_reports_ahead_behind_on_jj_workspace() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&spawned.agent_id).unwrap().path.clone();
        std::fs::write(path.join("AHEAD.txt"), "ahead\n").unwrap();
        jj_commit(&path, &["AHEAD.txt"], "child ahead");
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: Some(false),
                    include_completed: Some(false),
                },
            )
            .unwrap();
        let child = listed
            .agents
            .iter()
            .find(|row| row["agent_id"] == spawned.agent_id)
            .unwrap();
        assert!(child.get("ahead").is_some());
        assert!(child.get("behind").is_some());
        assert!(child.get("workspace_generation").is_none());
        assert!(child["ahead"].as_u64().unwrap_or(0) >= 1);
    }

    #[test]
    fn jj_request_key_replays_the_admitted_input_without_reminting() {
        if !jj_available() {
            return;
        }
        let repo = jj_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let producer = tree.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(
            tree.node(&producer.agent_id)
                .unwrap()
                .path
                .join("result.txt"),
            "result\n",
        )
        .unwrap();
        tree.record_turn_completed(&producer.agent_id).unwrap();
        tree.settle_for_test();
        let candidate = candidate_of(&tree, &producer.agent_id);
        let mut request = mint_req();
        request.request_key = Some("jj-input".into());
        request.inputs = Some(vec![candidate.to_string()]);
        let first = tree.spawn_agent("root", &request, None).unwrap();
        let before = tree.nodes().len();
        assert_ne!(
            tree.workspace(&first.workspace.id)
                .unwrap()
                .base_commit
                .as_deref(),
            Some(tree.candidates.get(&candidate).unwrap().commit.as_str())
        );
        assert_eq!(
            tree.spawn_agent("root", &request, None).unwrap().agent_id,
            first.agent_id
        );
        request.inputs = None;
        assert_eq!(
            tree.spawn_agent("root", &request, None).unwrap_err().code,
            TopologyErrorCode::RequestKeyConflict
        );
        assert_eq!(tree.nodes().len(), before);
    }

    /// B5: `request_key` identity covers `inputs`, and a key spent by another
    /// caller is refused without naming that caller's child.
    #[test]
    fn request_key_covers_inputs_and_hides_other_callers_children() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let producer = tree.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(
            tree.node(&producer.agent_id).unwrap().path.join("H.txt"),
            "child\n",
        )
        .unwrap();
        tree.record_turn_completed(&producer.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &producer.agent_id);
        let mut keyed = mint_req();
        keyed.request_key = Some("k1".into());
        keyed.inputs = Some(vec![candidate_id.to_string()]);
        let first = tree.spawn_agent("root", &keyed, None).unwrap();
        let repeat = tree.spawn_agent("root", &keyed, None).unwrap();
        assert_eq!(repeat.agent_id, first.agent_id, "same brief replays");
        let mut without_inputs = keyed.clone();
        without_inputs.inputs = None;
        let error = tree.spawn_agent("root", &without_inputs, None).unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::RequestKeyConflict);
        // JJ uses a separate working-copy commit; later rewrites and candidate
        // retirement must not mutate the original admitted input identity.
        let workspace_id = tree.node(&first.agent_id).unwrap().workspace_id.clone();
        tree.workspaces.get_mut(&workspace_id).unwrap().base_commit =
            Some("different-working-copy".into());
        tree.candidates.remove(&candidate_id);
        assert_eq!(
            tree.spawn_agent("root", &keyed, None).unwrap().agent_id,
            first.agent_id
        );
        assert_eq!(
            tree.spawn_agent("root", &without_inputs, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::RequestKeyConflict
        );
        // Another caller reusing the key learns only that it is spent.
        tree.grants.get_mut("root").unwrap().max_depth = 2;
        let hub = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        let mut hub_binding = RosterBinding::leaf("test-account", "selected-tuple");
        hub_binding.spawns = vec![TemplateName::parse("review").unwrap()];
        tree.pin_template("orchestrate", hub_binding);
        tree.grants.get_mut(&hub.agent_id).unwrap().may_spawn = true;
        let mut from_hub = spawn_req();
        from_hub.request_key = Some("k1".into());
        let error = tree
            .spawn_agent(&hub.agent_id, &from_hub, None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::RequestKeyConflict);
        assert!(
            !error.message.contains(&first.agent_id),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("another caller"),
            "{}",
            error.message
        );
    }
}
