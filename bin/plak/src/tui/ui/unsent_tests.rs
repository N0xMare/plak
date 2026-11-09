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
    drain(
        &mut app,
        vec![SendError::OutcomeUnknown {
            intent: draft,
            client_id: None,
            host_generation: 1,
        }],
    );
    assert_eq!(app.input, "maybe sent");
    assert!(blocks_contain(&app, "the session may still run this"));
}

/// Background intents come back silently and are armed again only after a
/// pause: the composer chrome, the usage read, and the agent view the host
/// would not serve. A reload acknowledgement is put back on this frame.
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
            SendError::OutcomeUnknown {
                intent: UserAction::Extension(SurfaceIntent::RereadUsage),
                client_id: None,
                host_generation: 1,
            },
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
    // The reload answer is already back. The rest is not on this frame.
    assert_eq!(app.reload_requests.accepts, ["r1"]);
    assert!(!app.reload_requests.handed_over.contains("r1"));
    assert!(!app.composer_chrome.refresh_pending);
    assert!(!app.usage_refresh.pending_read);
    assert!(app.host_agent_transcript.is_some());
    assert!(app.next_rearm().is_some());
    app.rearm_due(Instant::now() + Duration::from_secs(2));
    assert!(app.composer_chrome.refresh_pending);
    assert!(app.usage_refresh.pending_read);
    assert!(app.host_agent_transcript.is_none());
    assert_eq!(app.reload_requests.accepts, ["r1"]);
    assert!(app.next_rearm().is_none());
    assert_eq!(error_rows(&app), 0);
}

/// A reload answer the session keeps refusing is requeued at once each
/// time. The wait does not double, and it does not advance another kind's.
#[test]
fn a_refused_reload_answer_is_requeued_without_a_growing_wait() {
    let mut app = App::new(launch_basic(), true);
    super::test_support::pin_clock(&mut app);
    let accept = UserAction::Extension(SurfaceIntent::AcceptReloadRequest {
        request_id: "r1".into(),
    });
    for _ in 0..4 {
        app.reload_requests.handed_over.insert("r1".into());
        app.reload_requests.accepts.clear();
        refuse(&mut app, accept.clone());
        assert_eq!(rearm_wait(&app), Duration::ZERO);
        app.rearm_due(app.now());
        assert_eq!(app.reload_requests.accepts, ["r1"]);
        assert!(app.next_rearm().is_none());
    }
    app.reload_requests.accepts.clear();
    app.reload_requests.handed_over.insert("r1".into());
    refuse(
        &mut app,
        UserAction::Extension(SurfaceIntent::RefuseReloadRequest {
            request_id: "r1".into(),
            reason: "changed".into(),
        }),
    );
    assert_eq!(rearm_wait(&app), Duration::ZERO);
    app.rearm_due(app.now());
    assert!(app.next_rearm().is_none());
    let (actions, intents) = crate::config::user_action_channel();
    app.send_reload_answers(Some(&actions));
    assert_eq!(
        intents.try_recv().expect("the refusal is sent again"),
        UserAction::Extension(SurfaceIntent::RefuseReloadRequest {
            request_id: "r1".into(),
            reason: "changed".into(),
        })
    );

    refuse(
        &mut app,
        UserAction::Extension(SurfaceIntent::RefreshComposerChrome),
    );
    assert_eq!(rearm_wait(&app), Duration::from_secs(1));
}

fn refuse(app: &mut App, action: UserAction) {
    app.unsent_action(super::unsent::Unsent::Refused(
        UserActionSendError::Saturated(action),
    ));
}

fn rearm_wait(app: &App) -> Duration {
    app.next_rearm()
        .expect("a background intent is waiting to be armed again")
        .saturating_duration_since(app.now())
}

/// A refused background intent waits 1s, then 2s, 4s, 8s, 16s and 30s.
/// Another kind keeps its own wait. Success cancels a pending retry and
/// starts that kind over. A draft the session refused is not part of this.
#[test]
fn background_rearm_doubles_to_thirty_seconds_and_resets_when_it_succeeds() {
    let mut app = App::new(launch_basic(), true);
    super::test_support::pin_clock(&mut app);
    app.next_build_check = app.now() + Duration::from_secs(3_600);

    refuse(&mut app, UserAction::Prompt("keep".into()));
    assert!(app.next_rearm().is_none());
    assert!(app.input.contains("keep"));
    app.input.clear();

    let chrome = UserAction::Extension(SurfaceIntent::RefreshComposerChrome);
    for secs in [1_u64, 2, 4, 8, 16, 30, 30] {
        app.composer_chrome.refresh_pending = false;
        refuse(&mut app, chrome.clone());
        assert_eq!(rearm_wait(&app), Duration::from_secs(secs), "{secs}");
        assert_eq!(
            app.next_deadline(app.now()),
            Some(Duration::from_secs(secs)),
            "{secs}"
        );
        super::test_support::pass_time(&mut app, secs * 1_000);
        app.rearm_due(app.now());
        assert!(app.next_rearm().is_none(), "{secs}");
    }

    // A different kind is not stuck on the chrome wait.
    let usage = UserAction::Extension(SurfaceIntent::RereadUsage);
    refuse(&mut app, usage);
    assert_eq!(rearm_wait(&app), Duration::from_secs(1));
    super::test_support::pass_time(&mut app, 1_000);
    app.rearm_due(app.now());

    refuse(&mut app, chrome.clone());
    assert_eq!(rearm_wait(&app), Duration::from_secs(30));
    app.apply_host_event(crate::tui::HostEvent::Extension(
        crate::tui::SurfaceObservation::WorkspaceChrome(crate::tui::WorkspaceChrome::default()),
    ));
    assert!(app.next_rearm().is_none());
    refuse(&mut app, chrome);
    assert_eq!(rearm_wait(&app), Duration::from_secs(1));

    let build = UserAction::Extension(SurfaceIntent::CheckBuild);
    refuse(&mut app, build.clone());
    super::test_support::pass_time(&mut app, 1_000);
    app.rearm_due(app.now());
    refuse(&mut app, build);
    assert_eq!(rearm_wait(&app), Duration::from_secs(2));
    app.apply_host_event(crate::tui::HostEvent::Extension(
        crate::tui::SurfaceObservation::BuildStatus {
            newer_installed: false,
        },
    ));
    assert!(app.next_rearm().is_none());
    refuse(&mut app, UserAction::Extension(SurfaceIntent::CheckBuild));
    assert_eq!(rearm_wait(&app), Duration::from_secs(1));
}

/// Closing or switching an agent view drops a refused open's wait. Renaming
/// the view that is already open does not.
#[test]
fn closing_or_reopening_an_agent_view_resets_its_rearm_backoff() {
    let mut app = App::new(launch_basic(), true);
    super::test_support::pin_clock(&mut app);
    app.open_agent_view("a1".into(), "one".into());

    let open = || UserAction::OpenAgentTranscript {
        agent_id: "a1".into(),
    };
    for secs in [1_u64, 2] {
        refuse(&mut app, open());
        assert_eq!(rearm_wait(&app), Duration::from_secs(secs));
        super::test_support::pass_time(&mut app, secs * 1_000);
        app.rearm_due(app.now());
    }
    app.open_agent_view("a1".into(), "renamed".into());
    refuse(&mut app, open());
    assert_eq!(rearm_wait(&app), Duration::from_secs(4));
    assert!(app.close_agent_view());
    assert!(app.next_rearm().is_none());
    refuse(&mut app, open());
    assert_eq!(rearm_wait(&app), Duration::from_secs(1));
    super::test_support::pass_time(&mut app, 1_000);
    app.rearm_due(app.now());

    app.open_agent_view("a1".into(), "one".into());
    refuse(&mut app, open());
    super::test_support::pass_time(&mut app, 1_000);
    app.rearm_due(app.now());
    refuse(&mut app, open());
    assert_eq!(rearm_wait(&app), Duration::from_secs(2));
    app.open_agent_view("a2".into(), "two".into());
    assert!(app.next_rearm().is_none());
    refuse(&mut app, open());
    assert_eq!(rearm_wait(&app), Duration::from_secs(1));
}
