//! A running child's promoted process remains on its original allocation after cutover.
use super::*;

#[test]
fn host_move_live_running_child_job_survives_reconciliation_crashes_with_the_same_attempt() {
    for cut in [
        "move-after-owner-witnesses",
        "move-after-all-transfers",
        "move-after-ready",
    ] {
        let Some(fixture) = fixture() else { return };
        let _cleanup = HomeHost(fixture.home());
        let (mut source, session, node) = runner::start_child_job_variant(
            &fixture,
            true,
            &[
                (ENABLED, "1".into()),
                ("PLAK_TEST_HOST_UPGRADE_CUT", "move-after-commit".into()),
            ],
        );
        let pid = runner::job(&fixture).0;
        let identity = allocation(&fixture);
        let original = tree(&fixture, &plak::home::state::state_key(), &node.tree_id);
        let child = original
            .nodes()
            .values()
            .find(|child| child.parent_agent_id.is_some())
            .unwrap()
            .clone();
        assert!(child.active_turn);
        let before: plakous::sessions::SessionLocator =
            child.session_locator.as_ref().unwrap().parse().unwrap();
        commit_source(&fixture, &mut source);
        let parked = tree(&fixture, &plak::home::state::state_key(), &node.tree_id);
        assert!(parked.loss_continuation(&child.agent_id).is_some());
        let commands: Vec<_> = parked.commands().keys().cloned().collect();
        crash_destination(&fixture, cut);
        assert!(runner::alive(pid));
        assert_eq!(allocation(&fixture), identity);
        let mut target = destination(&fixture);
        retained::wait_group(&fixture, &session, "ready");
        let imported = tree(&fixture, DESTINATION, &node.tree_id);
        let next = imported.node(&child.agent_id).unwrap();
        assert_eq!(next.attempt_id, child.attempt_id);
        assert_eq!(next.line_id, child.line_id);
        let after: plakous::sessions::SessionLocator =
            next.session_locator.as_ref().unwrap().parse().unwrap();
        assert_eq!(after.session_id(), before.session_id());
        assert_eq!(after.branch_id(), before.branch_id());
        assert!(
            commands
                .iter()
                .all(|id| imported.commands().contains_key(id))
        );
        assert_eq!(allocation(&fixture), identity);
        assert!(runner::alive(pid));
        assert_eq!(runner::job(&fixture).0, pid);
        let mut client = Client::connect(&target);
        client.attach(80, &node);
        assert_eq!(
            client.request(81, Op::StopSession { session }),
            Outcome::Ok(Answer::Stopped {
                runner_pending: false
            })
        );
        stopped(pid);
        stop(&mut target);
    }
}

fn tree(
    fixture: &Fixture,
    key: &str,
    id: &str,
) -> plakous_recursive_subagents::tree::ProductAgentTree {
    plakous_recursive_subagents_store::SqliteTreeStore::inspect_move_tree(
        &fixture
            .home()
            .join("state")
            .join(key)
            .join("subagents.sqlite3"),
        id,
    )
    .unwrap()
    .unwrap()
    .0
    .rows()
    .clone()
}
