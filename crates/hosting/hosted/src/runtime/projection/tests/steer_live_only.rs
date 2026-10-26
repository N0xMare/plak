//! Accepted steering stays in chrome until committed history proves application.

use super::*;
use crate::runtime::recover_tests::{script, scripted_host_with};

const STEER: &str = "steer toward the fixture answer";

#[test]
fn an_accepted_steer_commits_no_revision_and_rebuilds_only_after_application() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([
            {"stream": [{"stall": 1500}], "output": {"text": "the steered round"}},
            {"output": {"text": "the steered answer"}}
        ])),
    );
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
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
        seen.push(event.clone());
        if hit {
            return;
        }
    };
    next("the admitted turn", &|event| {
        matches!(event, HostEvent::TurnLifecycle(lifecycle)
            if lifecycle.phase == plakous_host_protocol::TurnPhase::Admitted)
    });
    let before = store.load(&id).unwrap().store_revision;
    send_attached_action(&actions, UserAction::Steer(STEER.to_owned().into()));
    next(
        "queued steering chrome",
        &|event| matches!(event, HostEvent::SteerAck { state, .. } if state == "queued"),
    );
    assert_eq!(
        store.load(&id).unwrap().store_revision,
        before,
        "accepting a steer commits no product revision"
    );
    next("the finished turn", &|event| {
        matches!(event, HostEvent::TurnLifecycle(lifecycle)
            if lifecycle.phase != plakous_host_protocol::TurnPhase::Admitted)
    });
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, HostEvent::Row(TranscriptRow::Steer { .. })))
    );
    let applied = seen
        .iter()
        .position(|event| match event {
            HostEvent::TranscriptPatch(patch) => patch.upserts.iter().any(
                |item| matches!(&item.row, TranscriptRow::Steer { text, .. } if text == STEER),
            ),
            _ => false,
        })
        .unwrap();
    let following = seen.iter().position(|event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("the steered answer"))).unwrap();
    assert!(
        applied < following,
        "publish applied history before following model output"
    );
    let HostEvent::TranscriptPatch(patch) = &seen[applied] else {
        unreachable!()
    };
    let preceding = patch
        .upserts
        .iter()
        .find(|item| item.row.text() == Some("the steered round"))
        .unwrap();
    let steer = patch
        .upserts
        .iter()
        .find(|item| matches!(item.row, TranscriptRow::Steer { .. }))
        .unwrap();
    assert!(
        preceding.key < steer.key,
        "preceding prose must not move after the steer"
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();

    let steers = |views: &SessionViews| {
        views
            .rows
            .values()
            .filter(|row| matches!(row, TranscriptRow::Steer { text, .. } if text == STEER))
            .count()
    };
    let live = caught_up(root.path(), &store, &id);
    assert_eq!(steers(&live), 1, "the projector derived the steer row");
    store.delete_views(&id);
    let mut config = reopened(root.path(), &store, &id);
    let (fresh, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
    config.runtime_factory = fresh.runtime_factory;
    run_prompts(config, &[]);
    let rebuilt = caught_up(root.path(), &store, &id);
    assert_eq!(rebuilt.rows, live.rows, "the rebuild keeps the steer row");
}
