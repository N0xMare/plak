#![allow(
    clippy::too_many_lines,
    reason = "the differential scenario exercises every derived view mutation against its full scratch-clone oracle"
)]

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
