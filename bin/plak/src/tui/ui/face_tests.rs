//! The TUI as an attachment of its session's host service (spec 02 §4.4):
//! its intents pass through the service and go out on the session's own
//! mailbox sender, tagged with its attachment, and its observations come
//! back through the service. The urgent lanes, the one-in-flight admission
//! and the shutdown signals stay the mailbox's.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::KeyCode;
use plakous_host_protocol::{ApprovalChoice, observation_channel};

use super::App;
use super::terminal::{dispatch_action, drain_host_events};
use super::test_support::{key, key_alt, key_ctrl, launch_basic};
use crate::tui::{HostEvent, HostEventReceiver, UserAction, UserActionSendError};

/// A TUI app on its face over a session's mailbox ends, with the host's
/// side of them.
struct Pilot {
    app: App,
    actions: crate::tui::FaceActionSender,
    events: HostEventReceiver,
    intents: crate::surface_protocol::UserActionReceiver,
    observations: plakous_host_protocol::ObservationSender<crate::surface_protocol::PlakExtension>,
    shut_down: Arc<AtomicBool>,
    storage_interrupted: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl Pilot {
    fn new() -> Self {
        let (worker_actions, intents) = crate::surface_protocol::user_action_channel();
        let (observations, worker_events) = observation_channel();
        let (actions, events) = plakous_hosted::host_service::face(worker_actions, worker_events);
        let shut_down = Arc::new(AtomicBool::new(false));
        intents.on_shutdown({
            let shut_down = Arc::clone(&shut_down);
            move || shut_down.store(true, Ordering::Release)
        });
        let storage_interrupted = intents.storage_interrupt_probe();
        Self {
            app: App::new(launch_basic(), true),
            actions: crate::surface_protocol::TuiActionSender::InProcess(actions),
            events: HostEventReceiver::InProcess(events),
            intents,
            observations,
            shut_down,
            storage_interrupted,
        }
    }

    /// The host publishes `observations`.
    fn publish(&self, observations: impl IntoIterator<Item = HostEvent>) {
        for observation in observations {
            self.observations.send(observation).unwrap();
        }
    }

    /// Everything the face holds now, applied to the app; what it was.
    fn drain(&mut self) -> Vec<HostEvent> {
        let seen: Vec<HostEvent> = self.events.try_iter().collect();
        for event in seen.clone() {
            self.app.apply_host_event(event);
        }
        seen
    }

    /// Presses `keys` in order and dispatches what they produce, as the
    /// terminal loop does; what that was.
    fn press(&mut self, keys: &[crossterm::event::KeyEvent]) -> Vec<UserAction> {
        let sent: Vec<UserAction> = keys
            .iter()
            .filter_map(|pressed| self.app.handle_key(*pressed))
            .collect();
        for action in &sent {
            dispatch_action(&mut self.app, action, Some(&self.actions));
        }
        sent
    }
}

/// Approval: asked, allowed from the keyboard, and resolved in the window
/// the face's intent opened, which it sees with its own (absent)
/// correlation.
fn approve(pilot: &mut Pilot) {
    pilot.publish([
        HostEvent::SetBusy(true),
        HostEvent::ApprovalRequired {
            approval_id: "approval-1".into(),
            call_id: Some("call-1".into()),
            tool_name: "exec_command".into(),
            subject: Some("rm -rf build".into()),
            prompt: None,
            expires_at_ms: 0,
        },
    ]);
    pilot.drain();
    assert_eq!(pilot.app.pending_approval(), Some("approval-1"));
    let resolve = UserAction::ResolveApproval {
        approval_id: "approval-1".into(),
        choice: ApprovalChoice::Allow,
    };
    let sent = pilot.press(&[key_alt(KeyCode::Char('a')), key(KeyCode::Enter)]);
    assert_eq!(sent, vec![resolve.clone()]);
    let delivery = pilot.intents.try_recv().unwrap();
    assert_eq!(delivery, resolve);
    assert!(
        !delivery.correlated(),
        "the TUI names no command of its own"
    );
    let started = delivery.started().unwrap();
    assert_eq!(
        HostEvent::from(started.clone()),
        HostEvent::IntentStarted(Some("attachment:1".into())),
        "the session sees the face's attachment"
    );
    pilot.publish([
        started.into(),
        HostEvent::ApprovalResolved {
            approval_id: "approval-1".into(),
            resolution: "allowed".into(),
        },
    ]);
    let seen = pilot.drain();
    assert_eq!(
        seen.first(),
        Some(&HostEvent::IntentStarted(None)),
        "{seen:?}"
    );
    assert_eq!(pilot.app.pending_approval(), None);
}

/// Credential wait: parked, shown, cleared.
fn wait_for_login(pilot: &mut Pilot) {
    let waiting = Some("waiting for login: run `plak login claude`");
    pilot.publish([HostEvent::CredentialWait {
        account: Some("claude".into()),
    }]);
    pilot.drain();
    assert_eq!(pilot.app.turn_status_label().as_deref(), waiting);
    pilot.publish([HostEvent::CredentialWait { account: None }]);
    pilot.drain();
    assert_ne!(pilot.app.turn_status_label().as_deref(), waiting);
}

/// Interrupt: the urgent lane, coalesced, seen by the storage probe; then
/// one compaction in flight at a time.
fn interrupt(pilot: &mut Pilot) {
    let sent = pilot.press(&[key(KeyCode::Esc), key(KeyCode::Right), key(KeyCode::Enter)]);
    assert_eq!(sent, vec![UserAction::Interrupt]);
    pilot.actions.send(UserAction::Interrupt).unwrap();
    assert!(
        (pilot.storage_interrupted)(),
        "the probe sees the interrupt"
    );
    let interrupt = pilot.intents.try_recv().unwrap();
    assert_eq!(interrupt, UserAction::Interrupt);
    assert_eq!(interrupt.started(), None, "the urgent lane opens no window");
    drop(interrupt);
    assert!(pilot.intents.try_recv().is_err(), "two interrupts are one");
    pilot.publish([HostEvent::SetBusy(false)]);
    pilot.drain();

    pilot.actions.send(UserAction::Compact).unwrap();
    assert!(matches!(
        pilot.actions.send(UserAction::Compact),
        Err(UserActionSendError::Duplicate(UserAction::Compact))
    ));
    drop(pilot.intents.try_recv().unwrap());
    pilot.actions.send(UserAction::Compact).unwrap();
    drop(pilot.intents.try_recv().unwrap());
}

/// Quit: the TUI ends itself, then the launcher's quit runs the shutdown
/// hooks before the host takes it; the session going away disconnects the
/// face.
fn quit(mut pilot: Pilot) {
    let sent = pilot.press(&[key_ctrl(KeyCode::Char('q'))]);
    assert_eq!(sent, vec![UserAction::Quit]);
    assert!(
        !pilot.shut_down.load(Ordering::Acquire),
        "the UI only ends itself"
    );
    pilot.actions.send(UserAction::Quit).unwrap();
    assert!(pilot.shut_down.load(Ordering::Acquire));
    assert!((pilot.storage_interrupted)());
    assert_eq!(pilot.intents.try_recv().unwrap(), UserAction::Quit);
    drop(pilot.observations);
    let mut connected = true;
    drain_host_events(&mut pilot.app, &pilot.events, &mut connected, 8);
    assert!(!connected);
}

#[test]
fn the_tui_drives_approval_credential_wait_interrupt_and_quit_through_its_face() {
    let mut pilot = Pilot::new();
    approve(&mut pilot);
    wait_for_login(&mut pilot);
    interrupt(&mut pilot);
    quit(pilot);
}

/// The TUI's agent view is its attachment's: the session is asked by the
/// face's attachment, and the TUI takes the root and the agent transcript it
/// opened, not another agent's.
#[test]
fn the_tui_takes_only_the_agent_transcript_it_opened() {
    use plakous_host_protocol::views::transcript::{TranscriptScope, TranscriptStreamDelta};
    let stream = |agent: &str| HostEvent::TranscriptStream {
        scope: TranscriptScope::Agent(agent.into()),
        delta: TranscriptStreamDelta::AssistantDone,
    };
    let mut pilot = Pilot::new();
    let open = UserAction::OpenAgentTranscript {
        agent_id: "c1".into(),
    };
    dispatch_action(&mut pilot.app, &open, Some(&pilot.actions));
    let delivery = pilot.intents.try_recv().unwrap();
    assert_eq!(delivery, open);
    assert_eq!(
        delivery.started().map(HostEvent::from),
        Some(HostEvent::IntentStarted(Some("attachment:1".into())))
    );
    pilot.publish([stream("c2"), stream("c1")]);
    assert_eq!(pilot.drain(), [stream("c1")]);
}

/// The face's sender is the session's own, re-tagged, and the service is
/// not one of its senders: dropping it is the last sender leaving, which
/// runs the shutdown hooks as it always did.
#[test]
fn dropping_the_face_sender_is_the_last_sender_leaving() {
    let (worker_actions, intents) = crate::surface_protocol::user_action_channel();
    let (_observations, worker_events) = observation_channel();
    let (actions, _events) = plakous_hosted::host_service::face(worker_actions, worker_events);
    let shut_down = Arc::new(AtomicBool::new(false));
    intents.on_shutdown({
        let shut_down = Arc::clone(&shut_down);
        move || shut_down.store(true, Ordering::Release)
    });
    let storage_interrupted = intents.storage_interrupt_probe();
    let clone = actions.clone();
    drop(actions);
    assert!(!shut_down.load(Ordering::Acquire), "a clone still holds it");
    assert!(!storage_interrupted());
    drop(clone);
    assert!(shut_down.load(Ordering::Acquire));
    assert!(storage_interrupted());
}
