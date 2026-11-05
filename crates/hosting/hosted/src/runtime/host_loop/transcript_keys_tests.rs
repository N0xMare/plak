//! Committed rows that share a call id keep their own keys in every root
//! presentation the host publishes (spec 02 §2.6).

#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::{HostStep, RootActor};
use crate::runtime::Sender;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::fresh_injected_attached_host;
use crate::sessions::{ToolItem, TranscriptRow};
use crate::surface_protocol::{HostEvent, UserAction, UserActionSender};
use plakous_host_protocol::views::transcript::TranscriptState;

/// A tree result row, as `tree.rs` records it. A carried session's successor
/// tree numbers its mail from the start again, so its result rows can name a
/// call id an imported row already uses.
pub(in crate::runtime) fn child_result(subject: &str) -> TranscriptRow {
    TranscriptRow::Tool(ToolItem {
        call_id: "child-result:m1".into(),
        name: "child_result".into(),
        exposed_name: None,
        kind: crate::activity::tool_activity_kind("child_result"),
        state: "done".into(),
        subject: Some(subject.into()),
        detail: None,
        started_at_ms: None,
        finished_at_ms: None,
        parent_call_id: None,
        cell_id: None,
        wait: None,
    })
}

pub(in crate::runtime) fn results(folded: &TranscriptState) -> Vec<TranscriptRow> {
    folded
        .items()
        .values()
        .filter(|row| row.call_id() == Some("child-result:m1"))
        .cloned()
        .collect()
}

pub(in crate::runtime) fn is_repair(row: &TranscriptRow) -> bool {
    row.text()
        .is_some_and(|text| text.contains("invalid transcript presentation"))
}

/// Applies every presentation the host sent, as a face does, refusing any
/// invalid one and any repair notice.
pub(in crate::runtime) fn fold(seen: Vec<HostEvent<TestExtension>>, folded: &mut TranscriptState) {
    for event in seen {
        match event {
            HostEvent::PresentationSnapshot(snapshot) => folded.replace(snapshot).unwrap(),
            HostEvent::TranscriptPatch(patch) => folded.apply(patch).unwrap(),
            HostEvent::Row(row) => assert!(!is_repair(&row), "{row:?}"),
            _ => {}
        }
    }
    assert!(!folded.items().values().any(is_repair));
}

fn run_turn(
    host: &mut RootActor<'_, TestExtension>,
    actions: &UserActionSender<TestExtension>,
    text: &str,
) {
    actions.send(UserAction::Prompt(text.into())).unwrap();
    // Idle passes that handle no prompt answer `Continue`.
    let (text, queued_delivery, user_echoed) = (0..8)
        .find_map(|_| {
            match host
                .block(async |host| host.next_prompt(None).await)
                .unwrap()
            {
                HostStep::Prompt {
                    text,
                    queued_delivery,
                    user_echoed,
                } => Some((text, queued_delivery, user_echoed)),
                HostStep::Continue => None,
                HostStep::Quit => panic!("the host quit before the prompt"),
            }
        })
        .unwrap();
    host.block(async |host| {
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .await
    })
    .unwrap();
}

/// Commits a tree result as `tree.rs` does.
fn commit_result(
    host: &mut RootActor<'_, TestExtension>,
    notice_ids: &mut std::collections::HashSet<String>,
    id: &str,
    subject: &str,
) {
    let role = &mut host.role;
    assert!(
        crate::runtime::tree::publish_notice_once(
            &*role.config.store,
            &role.config.home,
            &mut role.config.session,
            &role.events,
            notice_ids,
            id,
            child_result(subject),
        )
        .unwrap()
    );
}

#[test]
fn a_tree_result_reusing_a_committed_call_id_publishes_both_rows_in_key_order() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (action_tx, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(wire), None).unwrap();
    let mut folded = TranscriptState::default();
    let mut notice_ids = std::collections::HashSet::new();
    // The earlier result stands for a carried session's imported row, the
    // later one for the first result of the tree the session runs now.
    commit_result(
        &mut host,
        &mut notice_ids,
        "tree:carried:result:m1",
        "m1 · c1 completed",
    );
    run_turn(&mut host, &action_tx, "after the carry");
    fold(events.try_iter().collect(), &mut folded);
    commit_result(
        &mut host,
        &mut notice_ids,
        "tree:successor:result:m1",
        "m1 · c2 completed",
    );
    fold(events.try_iter().collect(), &mut folded);
    assert_eq!(
        results(&folded),
        [
            child_result("m1 · c1 completed"),
            child_result("m1 · c2 completed")
        ],
        "the later result is its own row live, not an update of the earlier one"
    );
    // Turn end refreshes the projection from the store.
    run_turn(&mut host, &action_tx, "the next turn");
    fold(events.try_iter().collect(), &mut folded);

    let expected = crate::runtime::logging::project_session_snapshot(&host.role.config.session);
    assert_eq!(expected.validate(), Ok(()));
    assert_eq!(
        folded.items(),
        &expected
            .items
            .into_iter()
            .map(|item| (item.key, item.row))
            .collect()
    );
    assert_eq!(
        results(&folded),
        [
            child_result("m1 · c1 completed"),
            child_result("m1 · c2 completed")
        ]
    );
    host.block(async |host| host.release().await).unwrap();
}
