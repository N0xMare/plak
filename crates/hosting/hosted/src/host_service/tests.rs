use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::transcript::{TranscriptPatch, TranscriptStreamDelta};
use plakous_host_protocol::{Core, Draft, IntentReceiver, intent_channel};

use super::*;

mod child;

type Observation = SessionObservation<Core>;

fn root() -> NodeAddress {
    NodeAddress {
        tree_id: "t1".into(),
        agent_id: "t1".into(),
    }
}

fn service(capacity: usize) -> (HostService<Core>, IntentReceiver<Core>) {
    let (actions, intents) = intent_channel();
    (HostService::new(actions, root(), capacity), intents)
}

fn observations(outbound: Vec<Outbound<Core>>) -> Vec<Observation> {
    outbound
        .into_iter()
        .map(|item| match item {
            Outbound::Event { observation, .. } => observation,
            Outbound::ResyncRequired => panic!("dropped"),
        })
        .collect()
}

fn approval(id: &str) -> Observation {
    Observation::ApprovalRequired {
        approval_id: id.into(),
        call_id: None,
        tool_name: "shell".into(),
        subject: None,
        prompt: None,
        expires_at_ms: 1,
    }
}

fn root_patch(from: u64) -> Observation {
    Observation::TranscriptPatch(TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: from,
        to_revision: from + 1,
        upserts: Vec::new(),
        deletes: Vec::new(),
    })
}

fn stream(scope: TranscriptScope) -> Observation {
    Observation::TranscriptStream {
        scope,
        delta: TranscriptStreamDelta::AssistantDone,
    }
}

/// A new attachment starts with every state (the latest of each kind), the
/// root baseline, every approval still pending and the credential wait that
/// holds, each as itself; a resolved approval and a cleared wait are not
/// re-emitted.
#[test]
fn the_attach_snapshot_re_emits_states_baseline_pending_approvals_and_the_wait() {
    let (service, intents) = service(16);
    for observation in [
        Observation::SetBusy(true),
        Observation::Activity {
            label: "thinking".into(),
            active: true,
        },
        Observation::SetBusy(false),
        approval("a1"),
        approval("a2"),
        Observation::ApprovalResolved {
            approval_id: "a1".into(),
            resolution: "approved".into(),
        },
        Observation::CredentialWait {
            account: Some("main".into()),
        },
        Observation::PresentationSnapshot(PresentationSnapshot::empty(1)),
        root_patch(1),
    ] {
        service.observe(observation);
    }
    let id = service.attach(&root()).unwrap();
    assert_eq!(
        observations(service.take(id)),
        [
            Observation::SetBusy(false),
            Observation::Activity {
                label: "thinking".into(),
                active: true,
            },
            Observation::PresentationSnapshot(PresentationSnapshot::empty(1)),
            root_patch(1),
            approval("a2"),
            Observation::CredentialWait {
                account: Some("main".into()),
            },
        ]
    );
    assert!(intents.try_recv().is_err(), "a kept baseline asks for none");

    service.observe(Observation::CredentialWait { account: None });
    service.observe(Observation::ApprovalResolved {
        approval_id: "a2".into(),
        resolution: "denied".into(),
    });
    let later = service.attach(&root()).unwrap();
    let snapshot = observations(service.take(later));
    assert!(!snapshot.iter().any(|item| matches!(
        item,
        Observation::ApprovalRequired { .. } | Observation::CredentialWait { .. }
    )));
}

/// Without a kept baseline, a new attachment asks the session for a fresh
/// root snapshot, correlated to itself.
#[test]
fn an_attach_without_a_baseline_asks_for_one() {
    let (service, intents) = service(16);
    let id = service.attach(&root()).unwrap();
    let delivery = intents.try_recv().unwrap();
    assert!(matches!(
        *delivery,
        SessionIntent::ResyncTranscript {
            scope: TranscriptScope::Root
        }
    ));
    assert_eq!(
        delivery.started(),
        Some(plakous_host_protocol::ContentObservation::IntentStarted(
            Some(format!("attachment:{id}"))
        ))
    );
}

/// Answers go to the attachment whose intent the session answers, broadcast
/// goes to all, and an agent transcript only to the attachments that opened
/// it.
#[test]
fn observations_reach_the_attachments_their_class_names() {
    let (service, intents) = service(16);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let first = service.attach(&root()).unwrap();
    let second = service.attach(&root()).unwrap();
    service.take(first);
    service.take(second);

    service
        .intent(first, SessionIntent::Prompt(Draft::from("hi")))
        .unwrap();
    service
        .intent(
            second,
            SessionIntent::OpenAgentTranscript {
                agent_id: "c1".into(),
            },
        )
        .unwrap();
    // The session starts the first intent and answers it.
    let started = intents.try_recv().unwrap().started().unwrap();
    service.observe(started.into());
    service.observe(Observation::DraftAccepted(SessionIntent::Prompt(
        Draft::from("hi"),
    )));
    service.observe(Observation::SetBusy(true));
    service.observe(stream(TranscriptScope::Agent("c1".into())));
    service.observe(stream(TranscriptScope::Root));

    let kinds = |items: Vec<Observation>| -> Vec<&'static str> {
        items
            .iter()
            .map(plakous_host_protocol::jsonl::host_event_kind)
            .collect()
    };
    assert_eq!(
        kinds(observations(service.take(first))),
        [
            "intent_started",
            "draft_accepted",
            "busy",
            "transcript_stream"
        ]
    );
    assert_eq!(
        kinds(observations(service.take(second))),
        ["busy", "transcript_stream", "transcript_stream"]
    );
}

/// An attachment that does not take falls behind deterministically: a
/// droppable event makes room by dropping the oldest droppable one, a state
/// supersedes its queued kind, and a required event that finds no room drops
/// the attachment, which yields one resync-required and then refuses
/// everything; another attachment is untouched.
#[test]
fn a_slow_attachment_is_dropped_only_for_what_it_may_not_lose() {
    let (service, _intents) = service(3);
    let slow = service.attach(&root()).unwrap();
    let fast = service.attach(&root()).unwrap();

    service.observe(Observation::AssistantDelta("a".into()));
    service.observe(Observation::SetBusy(true));
    service.observe(approval("p1"));
    // Full: the delta makes room for the next approval.
    service.observe(approval("p2"));
    // A state of a queued kind supersedes it in place, no room needed.
    service.observe(Observation::SetBusy(false));
    assert_eq!(service.take(fast).len(), 3);
    // A droppable event finding no droppable one to replace is dropped.
    service.observe(Observation::AssistantDelta("b".into()));
    service.take(fast);
    // A required event finding no room drops the slow attachment.
    service.observe(approval("p3"));
    service.take(fast);

    assert_eq!(service.take(slow), [Outbound::ResyncRequired]);
    assert_eq!(service.take(slow), []);
    assert_eq!(
        service.intent(slow, SessionIntent::Interrupt),
        Err(RequestRefusal::NotAttached)
    );
    assert_eq!(service.detach(slow), Err(RequestRefusal::NotAttached));
    service.observe(Observation::SetBusy(true));
    assert_eq!(service.take(fast).len(), 1);
    assert_eq!(service.detach(fast), Ok(()));
}

/// The queue keeps its sequence: a droppable event evicted to make room
/// leaves a gap the client sees.
#[test]
fn an_evicted_droppable_event_leaves_a_sequence_gap() {
    let (service, _intents) = service(3);
    let id = service.attach(&root()).unwrap();
    service.observe(Observation::SetBusy(true));
    service.observe(approval("p1"));
    service.observe(Observation::AssistantDelta("lost".into()));
    service.observe(Observation::AssistantDelta("kept".into()));
    let seqs: Vec<u64> = service
        .take(id)
        .into_iter()
        .map(|item| match item {
            Outbound::Event { seq, .. } => seq,
            Outbound::ResyncRequired => 0,
        })
        .collect();
    assert_eq!(seqs, [1, 2, 4]);
}

/// Only the session's root is attachable here, and the mailbox's refusals
/// are the request's.
#[test]
fn attach_and_intent_refusals_are_typed() {
    let (service, intents) = service(4);
    assert_eq!(
        service.attach(&NodeAddress {
            tree_id: "t1".into(),
            agent_id: "c1".into(),
        }),
        Err(RequestRefusal::UnknownNode)
    );
    let id = service.attach(&root()).unwrap();
    drop(intents);
    assert_eq!(
        service.intent(id, SessionIntent::Prompt(Draft::from("x"))),
        Err(RequestRefusal::Disconnected)
    );
}

/// A client's own correlation travels through the service and comes back
/// in its `IntentStarted`; an answer to no attachment's intent reaches all.
#[test]
fn a_client_correlation_comes_back_and_unowned_answers_reach_all() {
    let (service, intents) = service(16);
    let first = service.attach(&root()).unwrap();
    let second = service.attach(&root()).unwrap();
    service.take(first);
    service.take(second);
    while intents.try_recv().is_ok() {}
    service
        .intent_correlated(
            first,
            SessionIntent::Prompt(Draft::from("hi")),
            Some("req-7"),
        )
        .unwrap();
    let started = intents.try_recv().unwrap().started().unwrap();
    service.observe(started.into());
    assert_eq!(
        observations(service.take(first)),
        [Observation::IntentStarted(Some("req-7".into()))]
    );
    assert_eq!(service.take(second), []);
    service.observe(Observation::IntentStarted(None));
    service.observe(Observation::DraftRestored(Draft::from("back")));
    assert_eq!(observations(service.take(first)).len(), 2);
    assert_eq!(observations(service.take(second)).len(), 2);
}

/// Transcript content lost to a full queue is repaid with the kept root
/// baseline at the next take, after the loss; a scope without one is asked
/// of the session.
#[test]
fn a_lost_transcript_is_repaid_with_a_fresh_baseline() {
    let (service, intents) = service(2);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(5),
    ));
    let id = service.attach(&root()).unwrap();
    service.take(id);
    service
        .intent(
            id,
            SessionIntent::OpenAgentTranscript {
                agent_id: "c1".into(),
            },
        )
        .unwrap();
    while intents.try_recv().is_ok() {}
    service.observe(Observation::AssistantDelta("a".into()));
    service.observe(stream(TranscriptScope::Agent("c1".into())));
    // Full: this delta evicts the first, a root loss.
    service.observe(Observation::AssistantDelta("b".into()));
    let taken = observations(service.take(id));
    assert!(
        taken.contains(&Observation::PresentationSnapshot(
            PresentationSnapshot::empty(5)
        )),
        "{taken:?}"
    );
    // The agent stream lost on a further overflow is asked of the session.
    service.observe(Observation::AssistantDelta("c".into()));
    service.observe(Observation::AssistantDelta("d".into()));
    service.observe(stream(TranscriptScope::Agent("c1".into())));
    service.observe(Observation::AssistantDelta("e".into()));
    let _ = service.take(id);
    let mut asked = Vec::new();
    while let Ok(delivery) = intents.try_recv() {
        asked.push((*delivery).clone());
    }
    assert!(asked.iter().any(|intent| matches!(
        intent,
        SessionIntent::ResyncTranscript {
            scope: TranscriptScope::Agent(agent)
        } if agent == "c1"
    )));
}

/// The attach snapshot does not count against the capacity: a snapshot
/// larger than it is delivered whole, and the capacity applies after it.
#[test]
fn a_snapshot_larger_than_the_capacity_is_delivered_whole() {
    let (service, _intents) = service(2);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    for id in ["a1", "a2", "a3", "a4"] {
        service.observe(approval(id));
    }
    let id = service.attach(&root()).unwrap();
    service.observe(approval("a5"));
    service.observe(approval("a6"));
    let taken = observations(service.take(id));
    assert_eq!(taken.len(), 7, "{taken:?}");
}

/// A session that opens again starts the attach snapshot afresh: an
/// approval or credential wait of the earlier session is not re-emitted.
#[test]
fn a_reopened_session_forgets_what_the_earlier_one_held() {
    let (service, _intents) = service(16);
    service.observe(approval("old"));
    service.observe(Observation::CredentialWait {
        account: Some("main".into()),
    });
    service.observe(Observation::SetBusy(true));
    service.observe(Observation::SessionOpening);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let id = service.attach(&root()).unwrap();
    assert_eq!(
        observations(service.take(id)),
        [
            Observation::SessionOpening,
            Observation::PresentationSnapshot(PresentationSnapshot::empty(1)),
        ]
    );
}

/// A superseded state leaves its old place and comes after what was queued
/// before it, with a fresh sequence number.
#[test]
fn a_superseding_state_keeps_the_order_of_events() {
    let (service, _intents) = service(16);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let id = service.attach(&root()).unwrap();
    service.take(id);
    service.observe(Observation::SetBusy(true));
    service.observe(approval("p1"));
    service.observe(Observation::SetBusy(false));
    let order: Vec<(u64, Observation)> = service
        .take(id)
        .into_iter()
        .map(|item| match item {
            Outbound::Event {
                seq, observation, ..
            } => (seq, observation),
            Outbound::ResyncRequired => panic!("dropped"),
        })
        .collect();
    assert_eq!(
        order,
        [(3, approval("p1")), (4, Observation::SetBusy(false))]
    );
}

/// An agent transcript is opened in the session by its first holder and
/// closed by its last; a later holder gets a fresh snapshot instead; a
/// scope change the session refused does not take effect.
#[test]
fn agent_transcripts_are_shared_across_attachments() {
    let (service, intents) = service(16);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let first = service.attach(&root()).unwrap();
    let second = service.attach(&root()).unwrap();
    let open = || SessionIntent::OpenAgentTranscript {
        agent_id: "c1".into(),
    };
    service.intent(first, open()).unwrap();
    service.intent(second, open()).unwrap();
    service
        .intent(first, SessionIntent::CloseAgentTranscript)
        .unwrap();
    service
        .intent(second, SessionIntent::CloseAgentTranscript)
        .unwrap();
    let mut sent = Vec::new();
    while let Ok(delivery) = intents.try_recv() {
        sent.push((*delivery).clone());
    }
    assert_eq!(
        sent,
        [
            open(),
            SessionIntent::ResyncTranscript {
                scope: TranscriptScope::Agent("c1".into())
            },
            SessionIntent::CloseAgentTranscript,
        ]
    );

    drop(intents);
    assert_eq!(
        service.intent(first, open()),
        Err(RequestRefusal::Disconnected)
    );
    service.take(first);
    service.observe(stream(TranscriptScope::Agent("c1".into())));
    assert_eq!(service.take(first), []);
}

/// An attach whose baseline the session cannot be asked for fails, and
/// leaves no attachment behind.
#[test]
fn an_attach_that_cannot_ask_for_a_baseline_fails() {
    let (service, intents) = service(4);
    drop(intents);
    assert_eq!(service.attach(&root()), Err(RequestRefusal::Disconnected));
    assert_eq!(service.detach(1), Err(RequestRefusal::NotAttached));
}

/// A face sends through the session's own sender, tagged with its
/// attachment, and takes everything the session published, in order and in
/// every scope, with its own correlation back in `IntentStarted`; its
/// service asks the session nothing.
#[test]
fn a_face_takes_everything_in_order_with_its_own_correlation() {
    let (actions, intents) = intent_channel::<Core>();
    let (published, events) = plakous_host_protocol::observation_channel::<Core>();
    let (actions, face) = face(actions, events);
    actions
        .send_correlated(SessionIntent::Prompt(Draft::from("hi")), Some("c7".into()))
        .unwrap();
    let delivery = intents.try_recv().unwrap();
    assert!(delivery.correlated());
    let started = delivery.started().unwrap();
    assert_eq!(
        Observation::from(started.clone()),
        Observation::IntentStarted(Some("attachment:1:c7".into()))
    );
    let foreign = Observation::IntentStarted(Some("c9".into()));
    for observation in [
        started.into(),
        stream(TranscriptScope::Agent("c1".into())),
        Observation::DraftRestored(Draft::from("back")),
        foreign.clone(),
        approval("a1"),
    ] {
        published.send(observation).unwrap();
    }
    assert_eq!(
        face.try_iter().collect::<Vec<_>>(),
        [
            Observation::IntentStarted(Some("c7".into())),
            stream(TranscriptScope::Agent("c1".into())),
            Observation::DraftRestored(Draft::from("back")),
            foreign,
            approval("a1"),
        ]
    );
    assert!(intents.try_recv().is_err(), "a face's service asks nothing");
    drop(published);
    assert_eq!(
        face.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    );
}

/// A face's mailbox may carry a correlation that only looks like an
/// attachment's (an untagged sender, another face on the same mailbox):
/// its window and the answers in it reach the face as no attachment's,
/// unchanged.
#[test]
fn a_face_passes_a_foreign_attachment_window_through() {
    let (actions, _intents) = intent_channel::<Core>();
    let (published, events) = plakous_host_protocol::observation_channel::<Core>();
    let (_actions, face) = face(actions, events);
    let foreign = Observation::IntentStarted(Some("attachment:7".into()));
    let answer = Observation::DraftRestored(Draft::from("back"));
    published.send(foreign.clone()).unwrap();
    published.send(answer.clone()).unwrap();
    assert_eq!(face.try_iter().collect::<Vec<_>>(), [foreign, answer]);
}
