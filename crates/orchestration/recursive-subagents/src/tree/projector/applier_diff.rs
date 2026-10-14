use std::collections::BTreeMap;

use crate::tree::{MessagePurpose, OperationRecord, ProductAgentTree, operator_intents};

impl ProductAgentTree {
    #[allow(
        clippy::too_many_lines,
        reason = "one diagnostic invariant covers every foreign view and queue"
    )]
    pub(super) fn assert_agent_local_applier(
        &self,
        before_operations: &BTreeMap<String, OperationRecord>,
        joined: &Self,
        agent: &str,
        intent: &str,
    ) {
        assert!(
            self.nodes
                .iter()
                .filter(|(id, _)| id.as_str() != agent)
                .eq(joined.nodes.iter().filter(|(id, _)| id.as_str() != agent)),
            "applier {intent} changed non-target nodes"
        );
        assert!(
            self.attempts
                .iter()
                .filter(|(_, row)| row.agent_id != agent)
                .eq(joined
                    .attempts
                    .iter()
                    .filter(|(_, row)| row.agent_id != agent)),
            "applier {intent} changed non-target attempts"
        );
        let workspace = self.nodes.get(agent).map(|node| node.workspace_id.as_str());
        assert!(
            self.workspaces
                .iter()
                .filter(|(id, _)| Some(id.as_str()) != workspace)
                .eq(joined
                    .workspaces
                    .iter()
                    .filter(|(id, _)| Some(id.as_str()) != workspace)),
            "applier {intent} changed non-target workspaces or generations"
        );
        // A result envelope is a reserved external effect, not another agent's view.
        let fresh_result = joined.operations.values().any(|row| {
            row.caller_agent_id == agent
                && row.tool_name == "result_effect"
                && !before_operations.contains_key(&row.operation_id)
        });
        assert!(
            self.messages
                .iter()
                .filter(|(_, row)| row.to != agent)
                .eq(joined.messages.iter().filter(|(id, row)| row.to != agent
                    && !(fresh_result
                        && row.from == agent
                        && row.purpose == MessagePurpose::Result
                        && !self.messages.contains_key(*id)))),
            "applier {intent} changed non-target messages"
        );
        assert!(
            self.commands
                .iter()
                .filter(|(_, row)| row.recipient != agent)
                .eq(joined
                    .commands
                    .iter()
                    .filter(|(_, row)| row.recipient != agent)),
            "applier {intent} changed non-target commands"
        );
        let session = self.fold_session(agent).ok().flatten();
        assert!(
            self.feed_cursors
                .iter()
                .filter(|(id, _)| Some(id.as_str()) != session.as_deref())
                .eq(joined
                    .feed_cursors
                    .iter()
                    .filter(|(id, _)| Some(id.as_str()) != session.as_deref())),
            "applier {intent} changed non-target feed cursors"
        );
        let foreign_inbound = |tree: &Self, exempt_results: bool| {
            tree.inbound
                .iter()
                .filter(|(recipient, _)| recipient.as_str() != agent)
                .map(|(recipient, ids)| {
                    let ids: Vec<_> = ids
                        .iter()
                        .filter(|id| {
                            !(exempt_results
                                && fresh_result
                                && !self.messages.contains_key(*id)
                                && tree.messages.get(*id).is_some_and(|row| {
                                    row.from == agent && row.purpose == MessagePurpose::Result
                                }))
                        })
                        .cloned()
                        .collect();
                    (recipient.clone(), ids)
                })
                .filter(|(_, ids)| !ids.is_empty())
                .collect::<Vec<_>>()
        };
        assert!(
            foreign_inbound(self, false) == foreign_inbound(joined, true),
            "applier {intent} changed non-target inbound queues"
        );
        assert!(
            foreign_effects(self, agent, workspace) == foreign_effects(joined, agent, workspace),
            "applier {intent} changed non-target effects queue"
        );
        assert!(
            self.effects.launch_checkout == joined.effects.launch_checkout
                || self.effects.launch_checkout.as_os_str().is_empty()
                    && joined
                        .effects
                        .unmints
                        .iter()
                        .any(|row| row.workspace_id.as_deref() == workspace),
            "applier {intent} changed foreign effects checkout"
        );
        assert!(
            self.fault_reasons
                .iter()
                .filter(|(id, _)| id.as_str() != agent)
                .eq(joined
                    .fault_reasons
                    .iter()
                    .filter(|(id, _)| id.as_str() != agent)),
            "applier {intent} changed non-target fault reasons"
        );
        // A cascade may append captured target inputs under its own reservation,
        // but may not apply them or alter any already-recorded foreign operation.
        assert!(
            before_operations
                .iter()
                .filter(|(_, row)| row.caller_agent_id != agent)
                .eq(joined.operations.iter().filter(|(id, row)| {
                    if row.caller_agent_id == agent {
                        return false;
                    }
                    let captured_close = row.tool_name == operator_intents::CLOSE_KIND
                        && !before_operations.contains_key(*id)
                        && joined.operations.values().any(|fence| {
                            fence.caller_agent_id == agent
                                && fence.tool_name == "descendant_close_effect"
                                && !before_operations.contains_key(&fence.operation_id)
                                && serde_json::from_str::<Vec<String>>(&fence.canonical_request)
                                    .is_ok_and(|targets| targets.contains(&row.caller_agent_id))
                        });
                    !captured_close
                })),
            "applier {intent} changed non-target operations"
        );
    }
}

fn foreign_effects(tree: &ProductAgentTree, agent: &str, workspace: Option<&str>) -> String {
    format!(
        "{:?}\n{:?}\n{:?}\n{:?}",
        tree.effects
            .seals
            .iter()
            .filter(|row| row.agent_id != agent)
            .map(|row| (row, std::sync::Arc::as_ptr(&row.ownership)))
            .collect::<Vec<_>>(),
        tree.effects
            .adopts
            .iter()
            .filter(|row| row.caller_id != agent)
            .collect::<Vec<_>>(),
        tree.effects
            .hooks
            .iter()
            .filter(|row| row.agent_id != agent)
            .collect::<Vec<_>>(),
        tree.effects
            .unmints
            .iter()
            .filter(|row| row.workspace_id.as_deref() != workspace)
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "deterministic offline law fixtures")]
mod tests {
    use crate::tree::test_support::{fake_tree, spawn_req};

    #[test]
    fn applier_diff_rejects_non_target_rows_and_accepts_target_rows() {
        let (_tmp, _vcs, mut before) = fake_tree();
        let child = before.spawn_agent("root", &spawn_req(), None).unwrap();
        let other = before
            .spawn_agent("root", &crate::tree::test_support::mint_req(), None)
            .unwrap();
        for field in 0..10 {
            let mut changed = before.clone();
            match field {
                0 => {
                    changed.nodes.get_mut("root").unwrap().active_turn = true;
                }
                1 => {
                    changed
                        .attempts
                        .get_mut(&child.attempt_id)
                        .unwrap()
                        .output_json = Some("changed".into());
                }
                2 => {
                    changed
                        .workspaces
                        .get_mut(&other.workspace.id)
                        .unwrap()
                        .generation += 1;
                }
                3 => {
                    changed.record_host_cancellation(&child.agent_id).unwrap();
                }
                4 => {
                    let mut mail = before.clone();
                    mail.record_host_cancellation(&child.agent_id).unwrap();
                    mail.fold_due(&child.agent_id).unwrap();
                    changed.messages = mail.messages;
                }
                5 => {
                    changed.commands.insert(
                        "foreign".into(),
                        crate::tree::AgentCommand::intent(
                            "foreign",
                            "root",
                            None,
                            crate::tree::AgentCommandKind::RootBatch,
                            "foreign",
                            Vec::new(),
                        ),
                    );
                }
                6 => {
                    changed.feed_cursors.insert(changed.tree_id.clone(), 99);
                }
                7 => {
                    changed
                        .inbound
                        .entry("root".into())
                        .or_default()
                        .push("foreign".into());
                }
                8 => {
                    changed.effects.hooks.push(crate::tree::MintHook {
                        agent_id: "root".into(),
                        command: "foreign".into(),
                        env: std::collections::BTreeMap::new(),
                        cwd: changed.launch_checkout.clone(),
                        caller_id: "root".into(),
                        request: spawn_req(),
                        operation_id: None,
                    });
                }
                _ => {
                    changed
                        .fault_reasons
                        .insert("root".into(), "foreign".into());
                }
            }
            let agent = if field == 0 || field == 2 || field >= 4 {
                &child.agent_id
            } else {
                "root"
            };
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    before.assert_agent_local_applier(
                        &before.operations,
                        &changed,
                        agent,
                        "deliberately foreign",
                    );
                }))
                .is_err(),
                "field={field}"
            );
        }
        let mut changed = before.clone();
        changed.nodes.get_mut(&child.agent_id).unwrap().active_turn = true;
        changed.commands.insert(
            "own".into(),
            crate::tree::AgentCommand::intent(
                "own",
                &child.agent_id,
                Some(child.attempt_id.clone()),
                crate::tree::AgentCommandKind::Start,
                "own",
                Vec::new(),
            ),
        );
        changed
            .inbound
            .entry(child.agent_id.clone())
            .or_default()
            .push("own".into());
        changed
            .fault_reasons
            .insert(child.agent_id.clone(), "own".into());
        before.assert_agent_local_applier(&before.operations, &changed, &child.agent_id, "own row");
    }
}
