//! The direct steer row is live only (spec 02 §2.6, row provenance (e) for
//! the host's copy): accepting a steer commits no product revision, and the
//! projector derives the stored steer row from its `CommandAccepted`, so a
//! rebuild from empty shows it too.

use super::*;
use crate::runtime::recover_tests::{script, scripted_host_with};

const STEER: &str = "steer toward the fixture answer";

#[test]
fn an_accepted_steer_commits_no_revision_and_rebuilds_from_its_acceptance() {
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
        seen.push(format!("{event:?}"));
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
        "the live steer row",
        &|event| matches!(event, HostEvent::Row(TranscriptRow::Steer { text, .. }) if text == STEER),
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
    config.test_injection = fresh.test_injection;
    run_prompts(config, &[]);
    let rebuilt = caught_up(root.path(), &store, &id);
    assert_eq!(rebuilt.rows, live.rows, "the rebuild keeps the steer row");
}
