use super::{DispatchPhase, Disposition, IntentClass, take_next};
use crate::interrupt::{InterruptWatch, InterruptWatchState};
use crate::runtime::test_kit::TestExtension;
use crate::surface_protocol::user_action_channel;
use std::cell::RefCell;

type UserAction = crate::surface_protocol::UserAction<TestExtension>;

/// The test's own copy of the table. Exhaustive, so a new intent must be
/// classified here too, and a reclassified one fails
/// `every_intent_has_its_class_and_disposition`.
fn expected_class(action: &UserAction) -> IntentClass {
    match action {
        UserAction::Interrupt => IntentClass::Urgent,
        UserAction::Quit => IntentClass::Quit,
        UserAction::ClearSession => IntentClass::IdleOnly { noun: "clear" },
        UserAction::Compact => IntentClass::IdleOnly { noun: "compact" },
        UserAction::ResumeSession { .. } | UserAction::ResumeSessionHandoff { .. } => {
            IntentClass::IdleOnly { noun: "resume" }
        }
        UserAction::ForkSession { .. } => IntentClass::IdleOnly { noun: "fork" },
        UserAction::InvokeTool { .. } => IntentClass::IdleOnly {
            noun: "invoke_tool",
        },
        UserAction::RetryAttach => IntentClass::IdleOnly { noun: "binding" },
        UserAction::Recover => IntentClass::IdleOnly { noun: "recover" },
        UserAction::Prompt(_)
        | UserAction::FollowUp(_)
        | UserAction::Steer(_)
        | UserAction::Background
        | UserAction::StopProcesses { .. }
        | UserAction::DropQueued
        | UserAction::SendQueuedNow { .. }
        | UserAction::DeleteQueued { .. }
        | UserAction::EditQueued { .. }
        | UserAction::MoveQueued { .. }
        | UserAction::ResumeQueueDrain
        | UserAction::ResolveApproval { .. }
        | UserAction::RefreshReadiness
        | UserAction::OpenTopologyNavigator
        | UserAction::OpenAgentTranscript { .. }
        | UserAction::CloseAgentTranscript
        | UserAction::ResyncTranscript { .. }
        | UserAction::RefreshTopologyTree
        | UserAction::ResumeTopologyTree
        | UserAction::InterruptTopologyNode { .. }
        | UserAction::CloseTopologyNode { .. }
        | UserAction::StopAllTopology
        | UserAction::RecoverTopologyNode { .. }
        | UserAction::RefreshTemplates
        | UserAction::RequestCallDetail { .. }
        | UserAction::Extension(_) => IntentClass::Ordinary,
    }
}

/// One intent of every variant `expected_class` names.
fn every_intent() -> Vec<UserAction> {
    let id = || "x".to_owned();
    vec![
        UserAction::Prompt("p".into()),
        UserAction::FollowUp("f".into()),
        UserAction::Steer("s".into()),
        UserAction::Background,
        UserAction::Interrupt,
        UserAction::StopProcesses {
            id: None,
            interrupt: true,
        },
        UserAction::DropQueued,
        UserAction::SendQueuedNow {
            id: id(),
            revision: 1,
        },
        UserAction::DeleteQueued {
            id: id(),
            revision: 1,
        },
        UserAction::EditQueued {
            id: id(),
            revision: 1,
            text: id(),
        },
        UserAction::MoveQueued {
            id: id(),
            revision: 1,
            new_index: 0,
        },
        UserAction::ResumeQueueDrain,
        UserAction::ResolveApproval {
            approval_id: id(),
            choice: plakous_host_protocol::ApprovalChoice::Allow,
        },
        UserAction::Quit,
        UserAction::ClearSession,
        UserAction::Compact,
        UserAction::ResumeSession { id: None },
        UserAction::ResumeSessionHandoff { id: id() },
        UserAction::ForkSession {
            account: None,
            compact: false,
        },
        UserAction::RefreshReadiness,
        UserAction::RetryAttach,
        UserAction::Recover,
        UserAction::OpenTopologyNavigator,
        UserAction::OpenAgentTranscript { agent_id: id() },
        UserAction::CloseAgentTranscript,
        UserAction::ResyncTranscript {
            scope: plakous_host_protocol::views::transcript::TranscriptScope::Root,
        },
        UserAction::RefreshTopologyTree,
        UserAction::ResumeTopologyTree,
        UserAction::InterruptTopologyNode { agent_id: id() },
        UserAction::CloseTopologyNode { agent_id: id() },
        UserAction::StopAllTopology,
        UserAction::RecoverTopologyNode {
            tree_id: id(),
            revision: 1,
            agent_id: id(),
            expected_attempt_id: id(),
        },
        UserAction::RefreshTemplates,
        UserAction::InvokeTool {
            correlation_id: None,
            name: id(),
            arguments: id(),
            agent_id: None,
        },
        UserAction::RequestCallDetail {
            call_id: id(),
            agent_id: None,
            session_id: id(),
        },
        UserAction::Extension(crate::runtime::test_kit::ProbeIntent::InspectUsage),
    ]
}

/// The number of `SessionIntent` variants `expected_class` matches.
const INTENT_VARIANTS: usize = 36;

#[test]
fn every_intent_has_its_class_and_disposition() {
    use DispatchPhase::{Busy, Workflow};
    let intents = every_intent();
    let variants = intents
        .iter()
        .map(std::mem::discriminant)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(variants.len(), intents.len(), "one intent per variant");
    assert_eq!(variants.len(), INTENT_VARIANTS, "every variant is listed");
    for action in &intents {
        let class = IntentClass::of(action);
        assert_eq!(class, expected_class(action), "{action:?}");
        let (busy, workflow) = match class {
            IntentClass::Urgent => (Disposition::Handle, Disposition::ObserveInterrupt),
            IntentClass::Quit | IntentClass::Ordinary => (Disposition::Handle, Disposition::Defer),
            IntentClass::IdleOnly { noun } => {
                (Disposition::RefuseWhileBusy { noun }, Disposition::Defer)
            }
        };
        assert_eq!(class.disposition(Busy), busy, "{action:?}");
        assert_eq!(class.disposition(Workflow), workflow, "{action:?}");
    }
}

/// Deferred first, one action per pass: a pass takes the oldest deferred
/// action and leaves the rest, and the mailbox only once nothing is deferred.
#[test]
fn each_pass_takes_one_action_deferred_first_in_arrival_order() {
    let (sender, receiver) = user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&receiver, &state);
    watch.begin();
    sender.send(UserAction::FollowUp("first".into())).unwrap();
    sender.send(UserAction::Interrupt).unwrap();
    sender.send(UserAction::Compact).unwrap();
    watch.poll();
    assert!(
        watch.interrupted(),
        "the urgent lane is observed, not deferred"
    );
    sender.send(UserAction::FollowUp("later".into())).unwrap();

    let first = take_next(&state, || receiver.try_recv()).unwrap();
    assert_eq!(first.into_parts().0, UserAction::FollowUp("first".into()));
    assert!(state.borrow().has_deferred(), "one action per pass");
    let second = take_next(&state, || receiver.try_recv()).unwrap();
    assert_eq!(second.into_parts().0, UserAction::Compact);
    let third = take_next(&state, || receiver.try_recv()).unwrap();
    assert_eq!(third.into_parts().0, UserAction::FollowUp("later".into()));
    assert!(take_next(&state, || receiver.try_recv()).is_err());
}

/// Quit keeps its FIFO place behind work already accepted, yet preparation
/// stops for it before starting a turn.
#[test]
fn a_deferred_quit_keeps_its_place_and_blocks_preparation() {
    let (sender, receiver) = user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&receiver, &state);
    watch.begin();
    assert!(!watch.preparation_stopped());
    sender
        .send(UserAction::FollowUp("accepted".into()))
        .unwrap();
    sender.send(UserAction::Quit).unwrap();

    assert!(watch.preparation_stopped());
    assert!(!watch.interrupted(), "Quit is not an interrupt");
    let first = take_next(&state, || receiver.try_recv()).unwrap();
    assert_eq!(
        first.into_parts().0,
        UserAction::FollowUp("accepted".into())
    );
    let second = take_next(&state, || receiver.try_recv()).unwrap();
    assert_eq!(second.into_parts().0, UserAction::Quit);
}

/// An Interrupt that arrives during local work between round-trips is seen
/// at the next checkpoint, ahead of ordinary work queued before it.
#[test]
fn interrupt_is_observed_at_the_next_checkpoint() {
    let (sender, receiver) = user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&receiver, &state);
    watch.begin();
    assert!(!watch.observe_now());
    sender.send(UserAction::Prompt("queued".into())).unwrap();
    sender.send(UserAction::Interrupt).unwrap();

    assert!(watch.observe_now());
    assert!(watch.preparation_stopped());
    let deferred = take_next(&state, || receiver.try_recv()).unwrap();
    assert_eq!(deferred.into_parts().0, UserAction::Prompt("queued".into()));
    assert!(
        take_next(&state, || receiver.try_recv()).is_err(),
        "the interrupt is never deferred"
    );
}
