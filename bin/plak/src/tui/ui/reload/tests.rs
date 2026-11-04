use super::*;
use crate::tui::{SurfaceIntent, UserAction, user_action_channel};

fn outcome(paths: &crate::home::paths::PlakPaths, request_id: &str) -> Option<String> {
    std::fs::read_to_string(crate::reload_request::outcome_path(paths, request_id)).ok()
}

/// A refusal never overtakes its own acknowledgement, and one of a request
/// this face's host did not hand over is recorded by the face itself.
#[test]
fn a_reload_refusal_follows_its_acknowledgement_or_is_recorded_here() {
    let root = tempfile::tempdir().unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(root.path().join("home"));
    paths.ensure_layout().unwrap();
    let foreign = uuid::Uuid::new_v4().to_string();
    let own = uuid::Uuid::new_v4().to_string();
    let mut app = App::new(super::super::test_support::launch_basic(), true);
    app.reload_requests = ReloadRequests::new(Some(paths.clone()));
    let (actions, intents) = user_action_channel();

    app.reload_requests
        .refusals
        .push((foreign.clone(), "never handed over".to_owned()));
    app.send_reload_answers(Some(&actions));
    assert!(intents.try_recv().is_err(), "the host is not asked");
    assert!(
        outcome(&paths, &foreign)
            .unwrap()
            .contains("never handed over")
    );

    for index in 0..plakous_host_protocol::limits::INTENT_QUEUE_CAPACITY {
        actions
            .send(UserAction::Prompt(index.to_string().into()))
            .unwrap();
    }
    app.reload_requests.accepts.push(own.clone());
    app.reload_requests
        .refusals
        .push((own.clone(), "sub-agents are live".to_owned()));
    app.send_reload_answers(Some(&actions));
    assert_eq!(app.reload_requests.accepts, std::slice::from_ref(&own));
    assert_eq!(app.reload_requests.refusals.len(), 1, "the refusal waits");
    assert!(outcome(&paths, &own).is_none());

    while intents.try_recv().is_ok() {}
    app.send_reload_answers(Some(&actions));
    assert_eq!(
        intents.try_recv().unwrap(),
        UserAction::Extension(SurfaceIntent::AcceptReloadRequest {
            request_id: own.clone()
        })
    );
    assert_eq!(
        intents.try_recv().unwrap(),
        UserAction::Extension(SurfaceIntent::RefuseReloadRequest {
            request_id: own.clone(),
            reason: "sub-agents are live".to_owned()
        })
    );
    assert!(outcome(&paths, &own).is_none(), "the host records it");
}
