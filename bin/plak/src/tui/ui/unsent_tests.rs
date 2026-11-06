//! Actions the session did not take, learned after their send returned,
//! fed through the frame's host drain as the wire face gives them back.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use plakous_host_protocol::client::SendError;
use plakous_host_protocol::status::HostPhase;

use super::App;
use super::terminal::{commit_sent_action, drain_host_events};
use super::test_support::{blocks_contain, launch_basic};
use crate::tui::{HostEventReceiver, SurfaceIntent, UserAction, UserActionSendError};

/// Drains `unsent` through the frame's host drain, as the loop does.
fn drain(app: &mut App, unsent: Vec<crate::surface_protocol::tui_channel::Unsent>) {
    let events = HostEventReceiver::Refusing(Mutex::new(VecDeque::from(unsent)));
    let mut connected = true;
    drain_host_events(app, &events, &mut connected, 64);
    assert!(connected);
}

/// Sends `action` as a send that returned does: its pending record.
fn sent(app: &mut App, action: &UserAction) {
    if let UserAction::Prompt(_) | UserAction::FollowUp(_) | UserAction::Steer(_) = action {
        app.pending_drafts.push_back(action.clone());
    }
    commit_sent_action(app, action);
}

fn error_rows(app: &App) -> usize {
    app.pane
        .blocks
        .iter()
        .filter(|block| matches!(block, crate::tui::blocks::ScrollBlock::Error(_)))
        .count()
}

/// A refused draft comes back beside what the user is typing now, its
/// pending record gone; a second refused draft keeps both.
#[test]
fn refused_drafts_come_back_beside_the_composer() {
    let mut app = App::new(launch_basic(), true);
    let first = UserAction::Prompt("first draft".into());
    let second = UserAction::FollowUp("second draft".into());
    sent(&mut app, &first);
    sent(&mut app, &second);
    app.set_input("typing now".to_owned());
    drain(
        &mut app,
        vec![
            SendError::Refused(UserActionSendError::Saturated(first)),
            SendError::Refused(UserActionSendError::Disconnected(second)),
        ],
    );
    for text in ["typing now", "first draft", "second draft"] {
        assert!(app.input.contains(text), "{text} lost: {:?}", app.input);
    }
    assert!(app.pending_drafts.is_empty());
    assert!(blocks_contain(&app, "host control queue is full"));
    assert!(blocks_contain(&app, "product host is disconnected"));
}

/// A refused queued edit drops its pending record and comes back beside the
/// composer; a refusal for the face's role names its row.
#[test]
fn a_refused_queued_edit_comes_back_and_names_its_row() {
    let mut app = App::new(launch_basic(), true);
    app.pending_queue_edit = Some(("q-1".into(), "edited draft".into()));
    app.set_input("typing now".to_owned());
    drain(
        &mut app,
        vec![SendError::RefusedForRole {
            row: "queue".into(),
            intent: UserAction::EditQueued {
                id: "q-1".into(),
                revision: 4,
                text: "edited draft".into(),
            },
        }],
    );
    assert!(app.pending_queue_edit.is_none());
    assert!(app.input.contains("typing now") && app.input.contains("edited draft"));
    assert!(blocks_contain(&app, "this face may not send that (queue)"));
}

/// A `/new` the session never took rolls its pending clear back.
#[test]
fn a_refused_new_session_is_no_longer_pending() {
    let mut app = App::new(launch_basic(), true);
    sent(&mut app, &UserAction::NewSession);
    assert!(app.clear_pending);
    drain(
        &mut app,
        vec![SendError::Refused(UserActionSendError::Duplicate(
            UserAction::NewSession,
        ))],
    );
    assert!(!app.clear_pending);
    assert_eq!(app.phase, HostPhase::Idle);
    assert!(app.status_detail.is_none());
    assert!(blocks_contain(&app, "already queued or running"));
}

/// A draft with no verdict comes back with the warning that the session
/// may still run it.
#[test]
fn a_draft_with_no_verdict_says_the_session_may_still_run_it() {
    let mut app = App::new(launch_basic(), true);
    let draft = UserAction::Prompt("maybe sent".into());
    sent(&mut app, &draft);
    drain(&mut app, vec![SendError::OutcomeUnknown(draft)]);
    assert_eq!(app.input, "maybe sent");
    assert!(blocks_contain(&app, "the session may still run this"));
}

/// Background intents come back silently and are armed again only after a
/// pause: the composer chrome, the usage read, the agent view the host
/// would not serve, and a reload acknowledgement.
#[test]
fn background_intents_are_armed_again_silently_after_a_pause() {
    let mut app = App::new(launch_basic(), true);
    app.composer_chrome.refresh_pending = false;
    app.usage_refresh.pending_read = false;
    app.host_agent_transcript = Some(("a1".into(), 1));
    app.reload_requests.handed_over.insert("r1".into());
    drain(
        &mut app,
        vec![
            SendError::Refused(UserActionSendError::Saturated(UserAction::Extension(
                SurfaceIntent::RefreshComposerChrome,
            ))),
            SendError::OutcomeUnknown(UserAction::Extension(SurfaceIntent::RereadUsage)),
            SendError::RefusedForRole {
                row: "transcript".into(),
                intent: UserAction::OpenAgentTranscript {
                    agent_id: "a1".into(),
                },
            },
            SendError::Refused(UserActionSendError::Saturated(UserAction::Extension(
                SurfaceIntent::AcceptReloadRequest {
                    request_id: "r1".into(),
                },
            ))),
        ],
    );
    assert_eq!(error_rows(&app), 0);
    // Not on the next frame.
    assert!(!app.composer_chrome.refresh_pending);
    assert!(!app.usage_refresh.pending_read);
    assert!(app.host_agent_transcript.is_some());
    assert_eq!(app.reload_requests.accepts, Vec::<String>::new());
    assert!(app.next_rearm().is_some());
    app.rearm_due(Instant::now() + Duration::from_secs(2));
    assert!(app.composer_chrome.refresh_pending);
    assert!(app.usage_refresh.pending_read);
    assert!(app.host_agent_transcript.is_none());
    assert_eq!(app.reload_requests.accepts, ["r1"]);
    assert!(!app.reload_requests.handed_over.contains("r1"));
    assert!(app.next_rearm().is_none());
    assert_eq!(error_rows(&app), 0);
}
