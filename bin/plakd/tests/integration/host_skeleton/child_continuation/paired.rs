use std::time::{Duration, Instant};

use super::*;
use plakd::store::session_records::{SessionRecordStore as _, SqliteSessionStore};
use plakous_hosted::agent_tree_protocol::NodeStatus;
use plakous_hosted::runtime::{ResumeAfterLoss, operator_attention::PendingTreeWork};
use plakous_recursive_subagents::tree::{CommandFact, ContinuationCause, LossContinuationState};
use plakous_recursive_subagents_store::TreeInspection;

#[test]
fn paired_child_continuation_does_not_report_an_operator_hold() {
    let Some(fixture) = fixture() else { return };
    let (_daemon, path, runner) = runner::remote_fixture(&fixture);
    let (mut host, session, node) = start_idle_parent_on(&fixture, Some((&path, &runner)));
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let read = || match SqliteTreeStore::inspect(&paths.subagents_database(), &node.tree_id) {
        Ok(TreeInspection::Tree(tree)) => tree,
        other => panic!("inspect child tree: {other:?}"),
    };
    let before = read();
    let child = before
        .rows()
        .nodes()
        .values()
        .find(|node| node.depth > 0)
        .unwrap();
    assert_eq!(child.placement.runner_id.as_deref(), Some(runner.as_str()));
    let child = child.agent_id.clone();

    stop(&mut host);
    let handed_off = read();
    let loss = handed_off.rows().loss_continuation(&child).unwrap();
    assert_eq!(loss.cause, ContinuationCause::Handoff);
    assert_eq!(loss.state, LossContinuationState::Held);
    assert!(
        PendingTreeWork::read(handed_off.rows(), ResumeAfterLoss::Hold)
            .attention(None, 0, 0)
            .is_none()
    );

    let mut host = start_env(&fixture, &[], &[("PATH", path)]);
    wait_child_calls(&fixture, 2);
    wait_for_child(read, "continuation admission", |tree| {
        tree.commands().values().any(|command| {
            command
                .command_id
                .starts_with(ProductAgentTree::LOSS_RESUME_COMMAND_PREFIX)
                && command.fact == Some(CommandFact::Accepted)
        })
    });

    let mut client = Client::connect(&host);
    let attachment = client.attach(10, &node);
    let prompt = "check the continuing child";
    client.prompt(11, attachment, prompt, ANSWER);
    let intents = SqliteSessionStore::new(paths.clone())
        .load_intents(&session)
        .unwrap();
    let parent = intents
        .submissions
        .iter()
        .find(|command| command.canonical_command.starts_with(prompt))
        .expect("root command persisted");
    assert!(
        !parent.canonical_command.contains("<agent_notice "),
        "false child loss notice"
    );
    let seen = read();
    let tree = seen.rows();
    assert_eq!(tree.nodes()[&child].status, NodeStatus::Running);
    let row = client
        .rows(12)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap();
    assert!(!row.needs_operator && row.live_work.held_children == 0);
    std::fs::write(fixture.root.join("first-gate"), "").unwrap();
    wait_for_child(read, "continuation terminal", |tree| {
        tree.loss_continuation(&child).is_none()
    });
    let row = client
        .rows(13)
        .into_iter()
        .find(|row| row.session == session)
        .unwrap();
    assert!(!row.needs_operator && row.live_work.held_children == 0);
    stop(&mut host);
}

fn wait_for_child(
    read: impl Fn() -> Box<plakous_recursive_subagents::tree::PersistedTree>,
    what: &str,
    condition: impl Fn(&ProductAgentTree) -> bool,
) {
    let deadline = Instant::now() + WAIT;
    while !condition(read().rows()) {
        assert!(Instant::now() < deadline, "{what} was not folded");
        std::thread::sleep(Duration::from_millis(10));
    }
}
