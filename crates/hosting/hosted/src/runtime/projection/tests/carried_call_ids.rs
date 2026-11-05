//! Committed rows can share a call id (a carried session's imported rows and
//! its own tree's results); every root presentation keeps both rows at their
//! own keys: a reopen, a steer's mid-turn refresh and the turn end
//! (spec 02 §2.6).

use super::*;
use crate::runtime::host_loop::transcript_keys_tests::{child_result, fold, is_repair, results};
use crate::runtime::recover_tests::{script, scripted_host_with};
use crate::sessions::projector::{Lane, RowKey};
use crate::sessions::{ProductTranscript, SessionRecord};
use plakous_host_protocol::views::transcript::TranscriptState;
use std::path::PathBuf;

const STEER: &str = "steer after the tree result";

#[test]
fn a_steer_over_a_reused_committed_call_id_publishes_only_valid_presentations() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([{"output": {"text": "first answer"}}])),
    );
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    let reopen_with = |steps: serde_json::Value| {
        let mut config = reopened(root.path(), &store, &id);
        let (fresh, _) = scripted_host_with(root.path(), script(steps));
        config.runtime_factory = fresh.runtime_factory;
        config
    };
    // The earlier result stands for a carried session's imported row, the
    // later one for the first result of the tree the session runs now.
    let commit_result = |notice_id: &str, subject: &str| {
        let mut record = store.load(&id).unwrap();
        record
            .transcript
            .record_named_notice(notice_id.into(), child_result(subject));
        store.save(&mut record).unwrap();
    };
    run_prompts(config, &["before the carry"]);
    commit_result("tree:carried:result:m1", "m1 · c1 completed");
    run_prompts(
        reopen_with(serde_json::json!([{"output": {"text": "second answer"}}])),
        &["after the carry"],
    );
    commit_result("tree:successor:result:m1", "m1 · c2 completed");

    let config = reopen_with(serde_json::json!([
        {"stream": [{"stall": 1500}], "output": {"text": "the steered round"}},
        {"output": {"text": "the steered answer"}}
    ]));
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    send_attached_action(
        &actions,
        UserAction::Prompt("start the turn".to_owned().into()),
    );
    let mut seen = Vec::new();
    let mut next = |what: &str, matches: &dyn Fn(&HostEvent) -> bool| loop {
        let event = events
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| panic!("waiting for {what}"));
        let hit = matches(&event);
        seen.push(event);
        if hit {
            return;
        }
    };
    next("the admitted turn", &|event| {
        matches!(event, HostEvent::TurnLifecycle(lifecycle)
            if lifecycle.phase == plakous_host_protocol::TurnPhase::Admitted)
    });
    send_attached_action(&actions, UserAction::Steer(STEER.to_owned().into()));
    next("the finished turn", &|event| {
        matches!(event, HostEvent::TurnLifecycle(lifecycle)
            if lifecycle.phase != plakous_host_protocol::TurnPhase::Admitted)
    });
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    seen.extend(events.try_iter());

    assert!(seen.iter().any(|event| {
        match event {
            HostEvent::TranscriptPatch(patch) => patch.upserts.iter().any(
                |item| matches!(&item.row, TranscriptRow::Steer { text, .. } if text == STEER),
            ),
            _ => false,
        }
    }));
    for event in &seen {
        if let HostEvent::PresentationSnapshot(snapshot) = event {
            assert_eq!(snapshot.validate(), Ok(()));
            assert!(!snapshot.items.iter().any(|item| is_repair(&item.row)));
        }
    }
    let mut folded = TranscriptState::default();
    fold(seen, &mut folded);
    assert_eq!(
        results(&folded),
        [
            child_result("m1 · c1 completed"),
            child_result("m1 · c2 completed")
        ]
    );
}

#[test]
fn a_truncated_stored_view_presents_its_marker_beside_the_session_header() {
    let mut session = SessionRecord::new(PathBuf::from("/work"));
    session.transcript = ProductTranscript::stored(vec![
        (RowKey::TRUNCATION, TranscriptRow::Truncated),
        (
            RowKey::new(4, Lane::Runtime, 0),
            TranscriptRow::user("kept"),
        ),
    ]);
    let snapshot = crate::runtime::logging::project_session_snapshot(&session);
    assert_eq!(snapshot.validate(), Ok(()));
    let keys: Vec<_> = snapshot.items.iter().map(|item| item.key).collect();
    assert_eq!(
        keys,
        [
            RowKey::TRUNCATION,
            RowKey::new(0, Lane::Runtime, 1),
            RowKey::new(4, Lane::Runtime, 0)
        ]
    );
    assert!(matches!(
        snapshot.items[0].row,
        TranscriptRow::SessionHeader { .. }
    ));
    assert_eq!(snapshot.items[1].row, TranscriptRow::Truncated);
}
