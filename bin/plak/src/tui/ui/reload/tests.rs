use super::*;
use crate::config::user_action_channel;
use crate::tui::{SurfaceIntent, UserAction};

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

fn bind(app: &mut App, session: &str) {
    app.apply_host_event(crate::tui::HostEvent::Binding {
        session_id: Some(session.to_owned()),
        account: app.account.clone(),
        model: app.model.clone(),
        effort: app.effort.clone(),
        extra: app.model_extra.clone(),
        composition: app.composition,
        protocol: app.protocol,
        effective_presentation: app.effective_presentation.clone(),
    });
}

/// A reload request belongs to its session: one the face holds, waiting
/// for an idle boundary or about to run, is dropped when the face switches
/// to another session, and no reload starts there.
#[test]
fn a_held_reload_request_is_dropped_when_the_session_switches() {
    for started in [false, true] {
        let source = uuid::Uuid::new_v4().to_string();
        let next = uuid::Uuid::new_v4().to_string();
        let mut app = App::new(super::super::test_support::launch_basic(), true);
        bind(&mut app, &source);
        let request: crate::reload_request::ReloadRequest =
            serde_json::from_value(serde_json::json!({
                "request_id": uuid::Uuid::new_v4().to_string(),
                "session_id": source,
                "expect_binary": "build-1",
            }))
            .unwrap();
        app.apply_reload_request(request);
        assert!(app.reload_requests.pending.is_some());
        if started {
            app.service_reload_request();
            assert!(app.reload_requests.active.is_some() && app.pending_reload);
        }
        // A round that hands the terminal back reports what it still held.
        assert_eq!(app.held_reload_requests().len(), 1, "{started}");
        bind(&mut app, &next);
        assert!(app.held_reload_requests().is_empty(), "{started}");
        assert!(app.reload_requests.pending.is_none(), "{started}");
        assert!(app.reload_requests.active.is_none(), "{started}");
        assert!(app.reload_requests.accepts.is_empty(), "{started}");
        app.service_reload_request();
        assert!(
            !app.pending_reload,
            "{started}: no reload starts for the session switched to"
        );
    }
}
