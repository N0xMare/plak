use plakous_host_protocol::intent::Draft;

use super::*;

fn root() -> NodeAddress {
    NodeAddress {
        tree_id: "t1".into(),
        agent_id: "t1".into(),
    }
}

#[test]
fn runner_aliases_decorate_each_wire_node_without_changing_another_attachments_facts() {
    use plakous_host_protocol::Core;
    use plakous_host_protocol::wire::RunnerLabel;

    let temporary = tempfile::tempdir().unwrap();
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        "t1",
        "t1",
        "root",
        temporary.path().to_owned(),
        "tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let mut view = crate::topology_navigator::project_snapshot(&tree);
    view.nodes[0].runner = RunnerLabel {
        name: "root-id".into(),
        runner_id: "root-id".into(),
    };
    let mut child = view.nodes[0].clone();
    child.agent_id = "child".into();
    child.parent_agent_id = Some("t1".into());
    child.runner = RunnerLabel {
        name: "child-id".into(),
        runner_id: "child-id".into(),
    };
    view.nodes.push(child);
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let service = HostService::new(actions.clone(), root(), ATTACHMENT_CAPACITY);
    let attachment = service.attach(&root(), Face::Remote).unwrap();
    let another = service.attach(&root(), Face::Remote).unwrap();
    service.observe(SessionObservation::TopologyNavigatorSnapshot(view.clone()));
    let mut connection =
        WireConnection::on_service(service.clone(), Vec::new(), actions, Leave::Detach)
            .with_runner_labels(std::sync::Arc::new(|id| RunnerLabel {
                name: format!("alias-{id}"),
                runner_id: id.to_owned(),
            }));
    connection.attachment = Some(attachment);
    connection.flush().unwrap();
    let shown = host_frames(&connection.stdout)
        .into_iter()
        .find_map(|frame| match frame {
            HostFrame::Event {
                observation: SessionObservation::TopologyNavigatorSnapshot(tree),
                ..
            } => Some(tree),
            _ => None,
        })
        .unwrap();
    assert_eq!(shown.nodes[0].runner.name, "alias-root-id");
    assert_eq!(shown.nodes[1].runner.name, "alias-child-id");
    assert_eq!(shown.nodes[1].runner.runner_id, "child-id");
    let cached = service
        .take(another)
        .into_iter()
        .find_map(|event| match event {
            Outbound::Event {
                observation: SessionObservation::TopologyNavigatorSnapshot(tree),
                ..
            } => Some(tree),
            _ => None,
        })
        .unwrap();
    assert_eq!(cached, view);
    connection.leave();
}

#[test]
fn host_draft_ids_survive_reconnect_only_after_durable_admission_for_every_submission_kind() {
    use crate::host_service::drafts::DraftRegistry;
    use plakous_host_protocol::{Core, wire::ClientDraftId};

    for intent in [
        SessionIntent::<Core>::Prompt(Draft::from("same input")),
        SessionIntent::FollowUp(Draft::from("same input")),
        SessionIntent::Steer(Draft::from("same input")),
        SessionIntent::MailTopologyNode {
            agent_id: "peer".to_owned(),
            message: "same input".to_owned(),
            urgent: true,
        },
    ] {
        let registry = DraftRegistry::default();
        let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
        let service = HostService::new(actions.clone(), root(), ATTACHMENT_CAPACITY);
        let connect = || {
            let mut connection = WireConnection::on_service(
                service.clone(),
                Vec::new(),
                actions.clone(),
                Leave::Detach,
            )
            .with_drafts(registry.clone());
            connection.session = Some("session-a".to_owned());
            connection.attachment = Some(service.attach(&root(), Face::Remote).unwrap());
            connection
        };
        let mut first = connect();
        let attachment = first.attachment.unwrap();
        let id = ClientDraftId::new();
        assert!(matches!(
            first.intent_request(0, attachment, intent.clone(), &[], None),
            Some(Outcome::Refused(RequestRefusal::Malformed { .. }))
        ));
        assert_eq!(
            first.intent_request(1, attachment, intent.clone(), &[], Some(id)),
            Some(Outcome::Ok(Answer::Accepted { duplicate: false }))
        );
        first.leave();
        let mut second = connect();
        let reattached = second.attachment.unwrap();
        assert_eq!(
            second.intent_request(2, reattached, intent.clone(), &[], Some(id)),
            Some(Outcome::Refused(RequestRefusal::DraftPending {
                client_id: id
            }))
        );
        service.observe(SessionObservation::IntentStarted(Some(format!(
            "attachment:{attachment}:{id}"
        ))));
        service.observe(SessionObservation::DraftAccepted(intent.clone()));
        assert_eq!(
            second.intent_request(3, reattached, intent.clone(), &[], Some(id)),
            Some(Outcome::Ok(Answer::Accepted { duplicate: true }))
        );
        service
            .lock()
            .attachments
            .get_mut(&reattached)
            .unwrap()
            .node = Some("other-node".into());
        assert!(matches!(
            second.intent_request(4, reattached, intent.clone(), &[], Some(id)),
            Some(Outcome::Refused(RequestRefusal::Malformed { .. }))
        ));
        service
            .lock()
            .attachments
            .get_mut(&reattached)
            .unwrap()
            .node = None;
        second.session = Some("session-b".to_owned());
        assert!(matches!(
            second.intent_request(4, reattached, intent, &[], Some(id)),
            Some(Outcome::Refused(RequestRefusal::Malformed { .. }))
        ));
        second.leave();
    }
}

#[test]
fn not_attached_refusal_frees_an_identified_draft_for_a_new_attachment() {
    use crate::host_service::drafts::DraftRegistry;
    use plakous_host_protocol::Core;
    use plakous_host_protocol::wire::ClientDraftId;
    let registry = DraftRegistry::default();
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let service = HostService::new(actions.clone(), root(), ATTACHMENT_CAPACITY);
    let mut connection =
        WireConnection::on_service(service.clone(), Vec::new(), actions, Leave::Detach)
            .with_drafts(registry);
    let attachment = service.attach(&root(), Face::Remote).unwrap();
    connection.attachment = Some(attachment);
    let id = ClientDraftId::new();
    let intent = SessionIntent::Prompt(Draft::from("retry only after refusal"));
    service.detach(attachment).unwrap();
    assert_eq!(
        connection.intent_request(1, attachment, intent.clone(), &[], Some(id)),
        Some(Outcome::Refused(RequestRefusal::NotAttached))
    );
    let next = service.attach(&root(), Face::Remote).unwrap();
    connection.attachment = Some(next);
    assert_eq!(
        connection.intent_request(2, next, intent, &[], Some(id)),
        Some(Outcome::Ok(Answer::Accepted { duplicate: false }))
    );
}

#[test]
fn answering_mail_records_durable_admission_before_a_reconnected_retry() {
    answering_draft(
        SessionIntent::MailTopologyNode {
            agent_id: "peer".into(),
            message: "deliver once".into(),
            urgent: true,
        },
        false,
    );
}

#[test]
fn interleaved_answer_keeps_steer_pending_until_its_own_admission() {
    answering_draft(SessionIntent::Steer(Draft::from("steer once")), false);
}

#[test]
fn review2_probe_stray_error_row_before_async_mail_admission() {
    answering_draft(
        SessionIntent::MailTopologyNode {
            agent_id: "peer".into(),
            message: "one mail".into(),
            urgent: false,
        },
        true,
    );
}

fn answering_draft(intent: SessionIntent<plakous_host_protocol::Core>, stray_error: bool) {
    use crate::host_service::drafts::DraftRegistry;
    use plakous_host_protocol::wire::ClientDraftId;
    use plakous_host_protocol::{ContentObservation, Core};
    let registry = DraftRegistry::default();
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let service = HostService::new(actions.clone(), root(), ATTACHMENT_CAPACITY);
    let connect = || {
        let mut wire =
            WireConnection::on_service(service.clone(), Vec::new(), actions.clone(), Leave::Detach)
                .with_drafts(registry.clone());
        wire.session = Some("session".into());
        wire.attachment = Some(service.attach(&root(), Face::Remote).unwrap());
        wire
    };
    let mut first = connect();
    let attachment = first.attachment.unwrap();
    let id = ClientDraftId::new();
    assert_eq!(
        first.intent_request(1, attachment, intent.clone(), &[], Some(id)),
        Some(Outcome::Ok(Answer::Accepted { duplicate: false }))
    );
    let (channel, events) = crate::surface_protocol::host_event_channel::<Core>();
    let sender = crate::runtime::Sender::over(channel);
    let drain = || {
        while let Ok(event) = events.try_recv() {
            service.observe(event);
        }
    };
    sender.send(ContentObservation::IntentStarted(Some(
        "attachment:999:detail".into(),
    )));
    let detail = sender.answering();
    sender.send(ContentObservation::IntentStarted(Some(format!(
        "attachment:{attachment}:{id}"
    ))));
    let admission = sender.answering();
    drain();
    detail.send(ContentObservation::SetBusy(false));
    drain();
    if stray_error {
        sender.send(ContentObservation::Row(TranscriptRow::error(
            plakous_host_protocol::views::transcript::NoticeScope::Host,
            "an unrelated background failure",
        )));
        drain();
    }
    first.leave();
    let mut retry = connect();
    let reattached = retry.attachment.unwrap();
    assert_eq!(
        retry.intent_request(2, reattached, intent.clone(), &[], Some(id)),
        Some(Outcome::Refused(RequestRefusal::DraftPending {
            client_id: id
        }))
    );
    admission.send(ContentObservation::DraftAccepted(intent.clone()));
    drain();
    assert_eq!(
        retry.intent_request(3, reattached, intent, &[], Some(id)),
        Some(Outcome::Ok(Answer::Accepted { duplicate: true }))
    );
}

#[test]
fn review2_probe_navigator_unavailable_mail_frees_its_reservation() {
    terminal_draft_verdict(
        SessionIntent::MailTopologyNode {
            agent_id: "peer".into(),
            message: "not delivered".into(),
            urgent: false,
        },
        plakous_host_protocol::ContentObservation::TopologyNavigatorUnavailable {
            reason: "no navigator".into(),
        },
        false,
    );
}

#[test]
fn every_handled_draft_kind_settles_accepted_rejected_or_addressed_error() {
    use plakous_host_protocol::ContentObservation as C;
    for intent in [
        SessionIntent::Prompt("prompt".into()),
        SessionIntent::FollowUp("follow".into()),
        SessionIntent::Steer("steer".into()),
        SessionIntent::MailTopologyNode {
            agent_id: "peer".into(),
            message: "mail".into(),
            urgent: true,
        },
    ] {
        terminal_draft_verdict(intent.clone(), C::DraftAccepted(intent.clone()), true);
        terminal_draft_verdict(intent.clone(), C::DraftRejected(intent.clone()), false);
        terminal_draft_verdict(
            intent,
            C::Row(TranscriptRow::error(
                plakous_host_protocol::views::transcript::NoticeScope::Host,
                "addressed failure",
            )),
            false,
        );
    }
}

fn terminal_draft_verdict(
    intent: SessionIntent<plakous_host_protocol::Core>,
    verdict: plakous_host_protocol::ContentObservation<plakous_host_protocol::Core>,
    duplicate: bool,
) {
    use crate::host_service::drafts::DraftRegistry;
    use plakous_host_protocol::{ContentObservation as C, Core, wire::ClientDraftId};
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let service = HostService::new(actions.clone(), root(), ATTACHMENT_CAPACITY);
    let mut wire = WireConnection::on_service(service.clone(), Vec::new(), actions, Leave::Detach)
        .with_drafts(DraftRegistry::default());
    let attachment = service.attach(&root(), Face::Remote).unwrap();
    wire.attachment = Some(attachment);
    let id = ClientDraftId::new();
    assert_eq!(
        wire.intent_request(1, attachment, intent.clone(), &[], Some(id)),
        Some(Outcome::Ok(Answer::Accepted { duplicate: false }))
    );
    let (channel, events) = crate::surface_protocol::host_event_channel::<Core>();
    let sender = crate::runtime::Sender::over(channel);
    sender.send(C::IntentStarted(Some(format!(
        "attachment:{attachment}:{id}"
    ))));
    sender.answering().send(verdict);
    while let Ok(event) = events.try_recv() {
        service.observe(event);
    }
    assert_eq!(
        wire.intent_request(2, attachment, intent, &[], Some(id)),
        Some(Outcome::Ok(Answer::Accepted { duplicate }))
    );
}

/// A child's mail request is answered from its own answer window: accepted
/// once the session reports the mail written, refused with the tree's
/// reason or the navigator's, and not attached when its attachment ends
/// first.
#[test]
fn a_childs_mail_request_is_answered_once_the_mail_is_written() {
    let (actions, _intents) =
        plakous_host_protocol::intent_channel::<plakous_host_protocol::Core>();
    let mut stdout = Vec::new();
    let mut adapter = WireConnection::new(root(), &mut stdout, actions);
    adapter.mails.insert("mail-1".into(), 7);
    adapter.mails.insert("mail-2".into(), 8);
    adapter.mails.insert("mail-3".into(), 9);
    adapter.mails.insert("mail-4".into(), 10);
    let refusal = TranscriptRow::error(
        plakous_host_protocol::views::transcript::NoticeScope::Tree,
        "mail refused: recipient is not a message peer",
    );
    for observation in [
        SessionObservation::IntentStarted(Some("mail-2".into())),
        SessionObservation::SetBusy(true),
        SessionObservation::IntentStarted(Some("mail-1".into())),
        SessionObservation::DraftAccepted(SessionIntent::Steer(Draft::from("look"))),
        SessionObservation::IntentStarted(Some("mail-2".into())),
        SessionObservation::Row(refusal),
        SessionObservation::IntentStarted(Some("mail-4".into())),
        SessionObservation::TopologyNavigatorUnavailable {
            reason: "no topology hub is attached".into(),
        },
    ] {
        adapter.mail_answer(&observation);
    }
    adapter.unanswered();
    assert_eq!(
        adapter.settled,
        [
            (7, Outcome::Ok(Answer::Accepted { duplicate: false })),
            (
                8,
                Outcome::Refused(RequestRefusal::MailRefused {
                    detail: "mail refused: recipient is not a message peer".into()
                })
            ),
            (
                10,
                Outcome::Refused(RequestRefusal::MailRefused {
                    detail: "no topology hub is attached".into()
                })
            ),
            (9, Outcome::Refused(RequestRefusal::NotAttached)),
        ]
    );
}

/// Every attach is a fresh attachment answered before any event of it: over
/// a session that already published a state, `attached` comes first and the
/// attachment's events are numbered from 1; a second attach names another
/// attachment and numbers its own events from 1 again.
#[test]
fn an_attach_is_answered_before_its_events_and_numbers_them_from_one() {
    use plakous_host_protocol::Core;
    let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
    let (observations, events) = plakous_host_protocol::observation_channel::<Core>();
    observations
        .send(SessionObservation::SetBusy(true))
        .unwrap();
    let (client, lines) = lines(8);
    for frame in [
        ClientFrame::<Core>::Request {
            id: 1,
            op: Op::Attach { node: root() },
        },
        ClientFrame::Request {
            id: 2,
            op: Op::Attach { node: root() },
        },
        ClientFrame::Bye,
    ] {
        client.send(Ok(encode_line(&frame).unwrap())).unwrap();
    }
    let session = std::thread::spawn(move || {
        while let Ok(delivery) = intents.recv() {
            if matches!(*delivery, SessionIntent::Quit) {
                break;
            }
        }
        drop(observations);
    });
    let mut stdout = Vec::new();
    WireConnection::new(root(), &mut stdout, actions)
        .serve(&lines, &events)
        .unwrap();
    session.join().unwrap();
    let frames: Vec<HostFrame<Core>> = String::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| decode_line(line).unwrap())
        .collect();
    let mut attached = Vec::new();
    let mut first_seq = BTreeMap::new();
    for frame in &frames {
        match frame {
            HostFrame::Response {
                outcome: Outcome::Ok(Answer::Attached { attachment }),
                ..
            } => attached.push(*attachment),
            HostFrame::Event {
                attachment, seq, ..
            } => {
                assert!(
                    attached.contains(attachment),
                    "an event before its attachment's answer: {frames:?}"
                );
                first_seq.entry(*attachment).or_insert(*seq);
            }
            _ => {}
        }
    }
    assert_eq!(attached.len(), 2, "{frames:?}");
    assert_ne!(
        attached[0], attached[1],
        "every attach is a fresh attachment"
    );
    assert_eq!(
        first_seq.into_iter().collect::<Vec<_>>(),
        [(attached[0], 1), (attached[1], 1)]
    );
}

fn welcome() -> Welcome<plakous_host_protocol::Core> {
    session_welcome("b".into(), 1, "s1".into(), root())
}

fn host_frames(stdout: &[u8]) -> Vec<HostFrame<plakous_host_protocol::Core>> {
    String::from_utf8(stdout.to_vec())
        .unwrap()
        .lines()
        .map(|line| decode_line(line).unwrap())
        .collect()
}

/// A client that sends no hello in time is refused, so a silent peer never
/// holds the opened session; a process asked to stop ends the wait with no
/// frame, and its caller quits the session.
#[test]
fn the_hello_wait_ends_at_its_deadline_or_a_stop() {
    use plakous_host_protocol::Core;
    let (_client, lines) = lines(1);
    let mut stdout = Vec::new();
    assert!(
        !handshake_within::<Core>(
            &lines,
            welcome(),
            &mut stdout,
            &|| false,
            Duration::from_millis(50)
        )
        .unwrap()
    );
    assert!(
        matches!(
            host_frames(&stdout).as_slice(),
            [HostFrame::Refused(HandshakeRefusal::Malformed { detail })]
                if detail.starts_with("no hello within")
        ),
        "{stdout:?}"
    );
    let mut stdout = Vec::new();
    assert!(!handshake::<Core>(&lines, welcome(), &mut stdout, &|| true).unwrap());
    assert!(stdout.is_empty(), "a stopped wait writes nothing");
}

#[test]
fn protocol_four_is_explicitly_refused_before_any_attachment() {
    use plakous_host_protocol::Core;
    use plakous_host_protocol::wire::Hello;
    let (sender, lines) = lines(1);
    sender
        .send(Ok(encode_line(&ClientFrame::<Core>::Hello(Hello {
            proto: 4,
            binary_id: "old-client".into(),
            caps: Vec::new(),
        }))
        .unwrap()))
        .unwrap();
    let mut stdout = Vec::new();
    assert!(!handshake::<Core>(&lines, welcome(), &mut stdout, &|| false).unwrap());
    assert!(matches!(
        host_frames(&stdout).as_slice(),
        [HostFrame::Refused(HandshakeRefusal::VersionMismatch {
            host: 6,
            client: 4
        })]
    ));
}

fn binding(session: &str) -> SessionObservation<plakous_host_protocol::Core> {
    SessionObservation::Binding {
        session_id: Some(session.to_owned()),
        account: None,
        model: None,
        effort: None,
        extra: None,
        composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
        protocol: plakous_host_protocol::ProtocolSelection::Auto,
        effective_presentation: "direct".to_owned(),
    }
}

fn root_named(tree: &str) -> NodeAddress {
    NodeAddress {
        tree_id: tree.into(),
        agent_id: tree.into(),
    }
}

/// A binding that names the served session again changes nothing; one that
/// names another (the session switched) ends the attachment with `moved`,
/// naming the new session's root, which the client then attaches fresh. A
/// switch whose root cannot be found ends the connection.
#[test]
fn a_session_switch_moves_the_attachment_to_the_new_root() {
    use plakous_host_protocol::Core;
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions).serving("s1", |id| {
        (id != "lost").then(|| root_named(&format!("tree-{id}")))
    });
    assert!(matches!(
        connection.request(1, Op::Attach { node: root() }),
        Some(Outcome::Ok(Answer::Attached { attachment: 1 }))
    ));
    assert!(connection.observe(binding("s1")).unwrap());
    assert_eq!(connection.attachment, Some(1), "the same session stays");
    assert!(connection.observe(binding("s2")).unwrap());
    assert_eq!(connection.attachment, None);
    assert!(matches!(
        connection.request(2, Op::Attach { node: root() }),
        Some(Outcome::Refused(RequestRefusal::Moved { node })) if node == root_named("tree-s2")
    ));
    assert!(matches!(
        connection.request(
            3,
            Op::Attach {
                node: root_named("tree-s2")
            }
        ),
        Some(Outcome::Ok(Answer::Attached { attachment: 2 }))
    ));
    assert!(!connection.observe(binding("lost")).unwrap());
    drop(connection);
    assert!(host_frames(&stdout).iter().any(|frame| matches!(
        frame,
        HostFrame::Moved { attachment: 1, node, .. } if *node == root_named("tree-s2")
    )));
}

/// A connection welcomed before a switch that attaches the root its welcome
/// named is refused `moved` with the served session's root, which it then
/// attaches; a root it never knew is still unknown.
#[test]
fn an_attach_of_the_welcomed_root_after_a_switch_is_refused_moved() {
    use plakous_host_protocol::Core;
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let (published, events) = plakous_host_protocol::observation_channel::<Core>();
    let session = WireSession::start(root(), "s1".to_owned(), actions, events, |id| {
        Some(root_named(&format!("tree-{id}")))
    });
    let mut stdout = Vec::new();
    let mut connection = session.connect(&mut stdout, Face::Remote, || false);
    assert_eq!(connection.welcomed(), Some(("s1".to_owned(), root())));
    published.send(binding("s2")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.session_id() != "s2" {
        assert!(Instant::now() < deadline, "the switch");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        connection.request(1, Op::Attach { node: root() }),
        Some(Outcome::Refused(RequestRefusal::Moved {
            node: root_named("tree-s2")
        }))
    );
    assert_eq!(
        connection.request(
            2,
            Op::Attach {
                node: root_named("tree-s3")
            }
        ),
        Some(Outcome::Refused(RequestRefusal::UnknownNode))
    );
    assert!(matches!(
        connection.request(
            3,
            Op::Attach {
                node: root_named("tree-s2")
            }
        ),
        Some(Outcome::Ok(Answer::Attached { .. }))
    ));
    assert_eq!(
        connection.request(4, Op::Attach { node: root() }),
        Some(Outcome::Refused(RequestRefusal::Moved {
            node: root_named("tree-s2")
        })),
        "the welcomed root stays one the client was told"
    );
}

/// After two switches before the client attaches again, the root its one
/// `moved` named is refused `moved` with the root now, which it attaches.
#[test]
fn a_root_told_before_another_switch_is_refused_moved() {
    use plakous_host_protocol::Core;
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions)
        .serving("s1", |id| Some(root_named(&format!("tree-{id}"))));
    assert!(connection.request(1, Op::Attach { node: root() }).is_some());
    assert!(connection.observe(binding("s2")).unwrap());
    assert!(connection.observe(binding("s3")).unwrap());
    assert_eq!(
        connection.welcomed(),
        Some(("s3".to_owned(), root_named("tree-s3")))
    );
    for told in [root_named("tree-s2"), root()] {
        assert_eq!(
            connection.request(2, Op::Attach { node: told }),
            Some(Outcome::Refused(RequestRefusal::Moved {
                node: root_named("tree-s3")
            }))
        );
    }
    assert!(matches!(
        connection.request(
            3,
            Op::Attach {
                node: root_named("tree-s3")
            }
        ),
        Some(Outcome::Ok(Answer::Attached { .. }))
    ));
    drop(connection);
    let moved: Vec<_> = host_frames(&stdout)
        .into_iter()
        .filter_map(|frame| match frame {
            HostFrame::Moved { node, .. } => Some(node),
            _ => None,
        })
        .collect();
    assert_eq!(
        moved,
        [root_named("tree-s2")],
        "one moved, at the first switch"
    );
}

/// A process asked to stop ends the connection as a leaving client does,
/// with the session quit.
#[test]
fn a_stop_ends_the_connection() {
    use plakous_host_protocol::Core;
    let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
    let (published, events) = plakous_host_protocol::observation_channel::<Core>();
    let (_client, lines) = lines(1);
    published.send(binding("s1")).unwrap();
    let session = std::thread::spawn(move || {
        let quit = intents
            .recv()
            .is_ok_and(|delivery| matches!(*delivery, SessionIntent::Quit));
        drop(published);
        quit
    });
    let mut stdout = Vec::new();
    WireConnection::new(root(), &mut stdout, actions)
        .with_stop(|| true)
        .serving("s1", |_| None)
        .serve(&lines, &events)
        .unwrap();
    assert!(session.join().unwrap(), "the session was quit");
    assert!(matches!(host_frames(&stdout).last(), Some(HostFrame::Bye)));
}

/// A switch's opening comes before the binding that names its session: it
/// is held, so the old attachment gets none of it, and the new session's
/// events an attach snapshot does not carry (an open notice, an addressed
/// answer before the next session's first window) reach the next
/// attachment after its `attached` answer.
#[test]
fn a_switchs_opening_reaches_the_next_attachment_not_the_old_one() {
    use plakous_host_protocol::Core;
    use plakous_host_protocol::views::transcript::NoticeScope;
    let notice = SessionObservation::<Core>::Row(TranscriptRow::info(
        NoticeScope::Host,
        "the next session opened",
    ));
    let draft = SessionObservation::<Core>::DraftRestored(Draft::from("next draft"));
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions)
        .serving("s1", |id| Some(root_named(&format!("tree-{id}"))));
    connection.request(1, Op::Attach { node: root() });
    for observation in [
        SessionObservation::SessionOpening,
        notice.clone(),
        draft.clone(),
        binding("s2"),
    ] {
        assert!(connection.observe(observation).unwrap());
    }
    connection.flush().unwrap();
    assert!(matches!(
        connection.request(
            2,
            Op::Attach {
                node: root_named("tree-s2")
            }
        ),
        Some(Outcome::Ok(Answer::Attached { attachment: 2 }))
    ));
    connection.flush().unwrap();
    drop(connection);
    let frames = host_frames(&stdout);
    let moved = frames
        .iter()
        .position(|frame| matches!(frame, HostFrame::Moved { attachment: 1, .. }))
        .expect("moved");
    let events = |attachment: AttachmentId| -> Vec<&SessionObservation<Core>> {
        frames
            .iter()
            .filter_map(|frame| match frame {
                HostFrame::Event {
                    attachment: held,
                    observation,
                    ..
                } if *held == attachment => Some(observation),
                _ => None,
            })
            .collect()
    };
    assert!(
        frames[..moved].iter().all(|frame| !matches!(
            frame,
            HostFrame::Event { observation, .. }
                if *observation == notice || *observation == draft
                    || matches!(observation, SessionObservation::SessionOpening)
        )),
        "{frames:?}"
    );
    let next = events(2);
    assert!(next.contains(&&notice), "{frames:?}");
    assert!(next.contains(&&draft), "{frames:?}");
}

/// A shared session moves every attachment when it switches: each
/// connection's attachment ends with `moved` naming the new root, after
/// none of the opening; the service then serves the new session, so a later
/// connection attaches it too; the first attach after the switch gets the
/// opening's events after its snapshot; and a switch whose root cannot be
/// found ends the service.
#[test]
fn a_shared_session_moves_every_attachment_on_a_switch() {
    use plakous_host_protocol::Core;
    use plakous_host_protocol::views::transcript::NoticeScope;
    let notice = SessionObservation::<Core>::Row(TranscriptRow::info(
        NoticeScope::Host,
        "the next session opened",
    ));
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let (published, events) = plakous_host_protocol::observation_channel::<Core>();
    let session = WireSession::start(root(), "s1".to_owned(), actions, events, |id| {
        (id != "lost").then(|| root_named(&format!("tree-{id}")))
    });
    let mut changes = session.changes();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let (mut first, mut second, mut later) = (Vec::new(), Vec::new(), Vec::new());
    let mut one = session.connect(&mut first, Face::Local, || false);
    let mut two = session.connect(&mut second, Face::Local, || false);
    assert!(matches!(
        one.request(1, Op::Attach { node: root() }),
        Some(Outcome::Ok(Answer::Attached { attachment: 1 }))
    ));
    assert!(matches!(
        two.request(1, Op::Attach { node: root() }),
        Some(Outcome::Ok(Answer::Attached { attachment: 2 }))
    ));
    published.send(binding("s1")).unwrap();
    for observation in [
        SessionObservation::SessionOpening,
        notice.clone(),
        binding("s2"),
    ] {
        published.send(observation).unwrap();
    }
    while session.session_id() != "s2" {
        runtime.block_on(changes.changed()).unwrap();
    }
    assert_eq!(session.root(), root_named("tree-s2"));
    assert!(!session.ended());
    one.flush().unwrap();
    two.flush().unwrap();
    assert!(matches!(
        one.request(
            2,
            Op::Attach {
                node: root_named("tree-s2")
            }
        ),
        Some(Outcome::Ok(Answer::Attached { attachment: 3 }))
    ));
    one.flush().unwrap();
    let mut three = session.connect(&mut later, Face::Local, || false);
    assert!(matches!(
        three.request(
            1,
            Op::Attach {
                node: root_named("tree-s2")
            }
        ),
        Some(Outcome::Ok(Answer::Attached { attachment: 4 }))
    ));
    published.send(binding("lost")).unwrap();
    while !session.ended() {
        runtime.block_on(changes.changed()).unwrap();
    }
    drop((one, two, three));
    for (frames, attachment) in [(host_frames(&first), 1), (host_frames(&second), 2)] {
        let moved = frames
            .iter()
            .position(|frame| {
                matches!(frame, HostFrame::Moved { attachment: held, node, .. }
                    if *held == attachment && *node == root_named("tree-s2"))
            })
            .expect("moved");
        assert!(
            frames[..moved].iter().all(|frame| !matches!(
                frame,
                HostFrame::Event { observation, .. } if *observation == notice
            )),
            "{frames:?}"
        );
    }
    assert!(host_frames(&first).iter().any(|frame| matches!(
        frame,
        HostFrame::Event { attachment: 3, observation, .. } if *observation == notice
    )));
}

/// Every connection a switch moved gets the opening's replay on its next
/// root attach, whichever attaches first: both get the open notice, and an
/// answer to one connection's intent reaches that connection alone, with
/// its own correlation. A connection that arrives after the switch was
/// never moved and gets the attach snapshot only.
#[test]
fn each_moved_connection_gets_the_replay_on_its_next_root_attach() {
    use plakous_host_protocol::Core;
    use plakous_host_protocol::views::transcript::NoticeScope;
    let notice = SessionObservation::<Core>::Row(TranscriptRow::info(
        NoticeScope::Host,
        "the next session opened",
    ));
    let draft = SessionObservation::<Core>::DraftRestored(Draft::from("first's draft"));
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let (published, events) = plakous_host_protocol::observation_channel::<Core>();
    let session = WireSession::start(root(), "s1".to_owned(), actions, events, |id| {
        Some(root_named(&format!("tree-{id}")))
    });
    let (mut first_out, mut second_out, mut later_out) = (Vec::new(), Vec::new(), Vec::new());
    let mut first = session.connect(&mut first_out, Face::Local, || false);
    let mut second = session.connect(&mut second_out, Face::Local, || false);
    assert!(matches!(
        first.request(1, Op::Attach { node: root() }),
        Some(Outcome::Ok(Answer::Attached { attachment: 1 }))
    ));
    assert!(matches!(
        second.request(1, Op::Attach { node: root() }),
        Some(Outcome::Ok(Answer::Attached { attachment: 2 }))
    ));
    for observation in [
        binding("s1"),
        SessionObservation::SessionOpening,
        notice.clone(),
        SessionObservation::IntentStarted(Some("attachment:1:mine".into())),
        draft.clone(),
        SessionObservation::IntentStarted(None),
        binding("s2"),
    ] {
        published.send(observation).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.session_id() != "s2" {
        assert!(Instant::now() < deadline, "the switch");
        std::thread::sleep(Duration::from_millis(2));
    }
    let next = root_named("tree-s2");
    // The second attaches first, before either has sent its `moved`.
    assert!(matches!(
        second.request(2, Op::Attach { node: next.clone() }),
        Some(Outcome::Ok(Answer::Attached { attachment: 3 }))
    ));
    first.flush().unwrap();
    assert!(matches!(
        first.request(2, Op::Attach { node: next.clone() }),
        Some(Outcome::Ok(Answer::Attached { attachment: 4 }))
    ));
    let mut later = session.connect(&mut later_out, Face::Local, || false);
    assert!(matches!(
        later.request(1, Op::Attach { node: next }),
        Some(Outcome::Ok(Answer::Attached { attachment: 5 }))
    ));
    for connection in [&mut first, &mut second, &mut later] {
        connection.flush().unwrap();
    }
    drop((first, second, later));
    let events_of = |frames: Vec<HostFrame<Core>>, attachment: AttachmentId| {
        frames
            .into_iter()
            .filter_map(|frame| match frame {
                HostFrame::Event {
                    attachment: held,
                    observation,
                    ..
                } if held == attachment => Some(observation),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let first_events = events_of(host_frames(&first_out), 4);
    let second_events = events_of(host_frames(&second_out), 3);
    let later_events = events_of(host_frames(&later_out), 5);
    assert!(first_events.contains(&notice), "{first_events:?}");
    assert!(second_events.contains(&notice), "{second_events:?}");
    assert!(!later_events.contains(&notice), "{later_events:?}");
    assert!(first_events.contains(&draft), "{first_events:?}");
    assert!(
        first_events.contains(&SessionObservation::IntentStarted(Some("mine".into()))),
        "{first_events:?}"
    );
    assert!(!second_events.contains(&draft), "{second_events:?}");
    assert!(!later_events.contains(&draft), "{later_events:?}");
}

/// An opening too long to hold until the binding that names its session
/// ends the served session's attachments with `resync_required`, after
/// what they held and before any of the opening: none of it reaches them,
/// whichever session it turns out to be. An attachment made before the
/// binding takes nothing, not even a snapshot; the binding ends it with
/// `moved` when it names another session, or `resync_required` when it
/// names the served one, and the next attach asks the session for a fresh
/// baseline.
#[test]
fn an_opening_too_long_to_hold_resyncs_the_old_attachments() {
    for (next, root_after) in [("s2", root_named("tree-s2")), ("s1", root())] {
        overflowed_opening_resolved_by(next, &root_after);
    }
}

fn overflowed_opening_resolved_by(next: &str, root_after: &NodeAddress) {
    use plakous_host_protocol::Core;
    use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptScope};
    let before = SessionObservation::<Core>::Row(TranscriptRow::info(NoticeScope::Host, "before"));
    let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions)
        .serving("s1", |id| Some(root_named(&format!("tree-{id}"))));
    connection.request(1, Op::Attach { node: root() });
    assert!(connection.observe(before.clone()).unwrap());
    let opening = |index: usize| {
        SessionObservation::Row(TranscriptRow::info(
            NoticeScope::Host,
            format!("opening {index}"),
        ))
    };
    assert!(
        connection
            .observe(SessionObservation::SessionOpening)
            .unwrap()
    );
    for index in 0..=ATTACHMENT_CAPACITY {
        assert!(connection.observe(opening(index)).unwrap());
    }
    connection.flush().unwrap();
    while intents.try_recv().is_ok() {}
    assert!(matches!(
        connection.request(2, Op::Attach { node: root() }),
        Some(Outcome::Ok(Answer::Attached { attachment: 2 }))
    ));
    assert!(intents.try_recv().is_err(), "a waiting attach asks nothing");
    assert!(
        connection
            .observe(opening(ATTACHMENT_CAPACITY + 1))
            .unwrap()
    );
    assert!(connection.observe(binding(next)).unwrap());
    connection.flush().unwrap();
    assert!(matches!(
        connection.request(
            3,
            Op::Attach {
                node: root_after.clone()
            }
        ),
        Some(Outcome::Ok(Answer::Attached { attachment: 3 }))
    ));
    assert!(
        matches!(
            intents.try_recv().as_deref(),
            Ok(SessionIntent::ResyncTranscript {
                scope: TranscriptScope::Root
            })
        ),
        "{next}: the next attach asks for a fresh baseline"
    );
    drop(connection);
    let frames = host_frames(&stdout);
    let resync = frames
        .iter()
        .position(|frame| matches!(frame, HostFrame::ResyncRequired { attachment: 1 }))
        .expect("resync_required");
    assert!(
        frames[..resync].iter().any(|frame| matches!(
            frame,
            HostFrame::Event { observation, .. } if *observation == before
        )),
        "{frames:?}"
    );
    assert!(
        !frames.iter().any(|frame| matches!(
            frame,
            HostFrame::Event {
                observation: SessionObservation::Row(row),
                ..
            } if row.text().is_some_and(|text| text.starts_with("opening"))
        )),
        "{next}: {frames:?}"
    );
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, HostFrame::Event { attachment: 2, .. })),
        "{next}: {frames:?}"
    );
    let ended = frames.iter().any(|frame| match next {
        "s1" => matches!(frame, HostFrame::ResyncRequired { attachment: 2 }),
        _ => matches!(frame, HostFrame::Moved { attachment: 2, node, .. } if node == root_after),
    });
    assert!(ended, "{next}: {frames:?}");
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, HostFrame::Moved { attachment: 1, .. })),
        "{frames:?}"
    );
}

/// Overflows the held opening of `connection`, attached as attachment 1,
/// and flushes the resync.
fn overflow_the_opening<W: Write>(connection: &mut WireConnection<plakous_host_protocol::Core, W>) {
    use plakous_host_protocol::views::transcript::NoticeScope;
    assert!(
        connection
            .observe(SessionObservation::SessionOpening)
            .unwrap()
    );
    for index in 0..=ATTACHMENT_CAPACITY {
        let row = TranscriptRow::info(NoticeScope::Host, format!("opening {index}"));
        assert!(connection.observe(SessionObservation::Row(row)).unwrap());
    }
    connection.flush().unwrap();
}

/// A child attachment made before an overflowed opening's binding waits as
/// a root one does, and the session's end with the opening still
/// unresolved ends every waiting attachment with `resync_required`.
#[test]
fn waiting_attachments_include_children_and_end_with_the_session() {
    use plakous_host_protocol::Core;
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions)
        .serving("s1", |id| Some(root_named(&format!("tree-{id}"))));
    assert!(connection.request(1, Op::Attach { node: root() }).is_some());
    overflow_the_opening(&mut connection);
    connection.service = connection
        .service
        .clone()
        .with_nodes(std::sync::Arc::new(OpenChildren));
    let child = NodeAddress {
        tree_id: "t1".into(),
        agent_id: "c1".into(),
    };
    assert!(matches!(
        connection.request(2, Op::Attach { node: child }),
        Some(Outcome::Ok(Answer::Attached { attachment: 2 }))
    ));
    let served = connection.served.clone().unwrap();
    served::lock(&served).end();
    connection.flush().unwrap();
    drop(connection);
    let frames = host_frames(&stdout);
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, HostFrame::Event { attachment: 2, .. })),
        "{frames:?}"
    );
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, HostFrame::ResyncRequired { attachment: 2 })),
        "{frames:?}"
    );
}

/// A connection remembers the last 16 roots it told its client; an attach
/// of an older one is unknown.
#[test]
fn a_connection_remembers_its_last_told_roots() {
    use plakous_host_protocol::Core;
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::<Core, _>::new(root(), &mut stdout, actions);
    for index in 0..TOLD_ROOTS {
        connection.tell(&root_named(&format!("tree-{index}")));
    }
    assert!(
        !connection.told.contains(&root()),
        "the welcome's went first"
    );
    assert!(connection.told.contains(&root_named("tree-1")));
    assert_eq!(connection.told.len(), TOLD_ROOTS);
}

/// Every child of the tree takes attachments.
struct OpenChildren;

impl super::super::ChildNodes for OpenChildren {
    fn state(&self, _agent_id: &str) -> super::super::NodeState {
        super::super::NodeState::Open
    }

    fn interrupt(&self, _agent_id: &str, done: super::super::ControlDone) {
        done(Ok(()));
    }
}

/// A connection moved twice before it attaches the root again, its child
/// attachment between, is owed only the last switch's replay: once it
/// attaches the root, no connection is owed one and it is dropped.
#[test]
fn a_connection_moved_twice_is_owed_one_replay() {
    use plakous_host_protocol::Core;
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions)
        .serving("s1", |id| Some(root_named(&format!("tree-{id}"))));
    assert!(connection.request(1, Op::Attach { node: root() }).is_some());
    assert!(connection.observe(binding("s2")).unwrap());
    let served = connection.served.clone().unwrap();
    assert!(served::lock(&served).owes(1));
    // The new tree's children, reached as its hub would.
    connection.service = connection
        .service
        .clone()
        .with_nodes(std::sync::Arc::new(OpenChildren));
    let child = NodeAddress {
        tree_id: "tree-s2".into(),
        agent_id: "c1".into(),
    };
    assert!(matches!(
        connection.request(2, Op::Attach { node: child }),
        Some(Outcome::Ok(Answer::Attached { attachment: 2 }))
    ));
    assert!(served::lock(&served).owes(1), "a child attach owes on");
    assert!(connection.observe(binding("s3")).unwrap());
    assert!(
        !served::lock(&served).owes(1),
        "the first switch's is forgotten"
    );
    assert!(served::lock(&served).owes(2));
    assert!(matches!(
        connection.request(
            3,
            Op::Attach {
                node: root_named("tree-s3")
            }
        ),
        Some(Outcome::Ok(Answer::Attached { .. }))
    ));
    assert!(!served::lock(&served).owes(2));
}

/// Events past the capacity between `moved` and the next attach.
fn overflow_after_a_switch<W: Write>(
    connection: &mut WireConnection<plakous_host_protocol::Core, W>,
    next: &str,
) {
    use plakous_host_protocol::views::transcript::NoticeScope;
    assert!(connection.observe(binding(next)).unwrap());
    for index in 0..=ATTACHMENT_CAPACITY {
        let row = TranscriptRow::info(NoticeScope::Host, format!("event {index}"));
        assert!(connection.observe(SessionObservation::Row(row)).unwrap());
    }
}

fn attach_line(id: RequestId, tree: &str) -> Result<String, WireError> {
    let attach = ClientFrame::<plakous_host_protocol::Core>::Request {
        id,
        op: Op::Attach {
            node: root_named(tree),
        },
    };
    Ok(encode_line(&attach).unwrap())
}

/// More events between `moved` and the next attach than an attachment holds
/// are not a silent gap: the next attachment is answered `attached` and
/// then told to resync, as a slow consumer is, with no event of it between.
#[test]
fn a_replay_past_the_capacity_ends_in_a_resync() {
    use plakous_host_protocol::Core;
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions)
        .serving("s1", |id| Some(root_named(&format!("tree-{id}"))));
    connection.request(1, Op::Attach { node: root() });
    overflow_after_a_switch(&mut connection, "s2");
    assert!(connection.frame(attach_line(2, "tree-s2")).unwrap());
    connection.flush().unwrap();
    drop(connection);
    let frames = host_frames(&stdout);
    let attached = frames
        .iter()
        .position(|frame| {
            matches!(
                frame,
                HostFrame::Response {
                    id: 2,
                    outcome: Outcome::Ok(Answer::Attached { attachment: 2 })
                }
            )
        })
        .expect("attached");
    assert!(
        matches!(
            frames.get(attached + 1),
            Some(HostFrame::ResyncRequired { attachment: 2 })
        ),
        "{:?}",
        &frames[attached..]
    );
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, HostFrame::Event { attachment: 2, .. })),
        "no event of the resynced attachment"
    );
}

/// A replay given up after one switch does not outlive it: after another
/// switch, the moved connection's next attachment gets that switch's
/// replay and no resync.
#[test]
fn a_later_switch_replays_again_after_an_overflow() {
    use plakous_host_protocol::Core;
    use plakous_host_protocol::views::transcript::NoticeScope;
    let notice = SessionObservation::<Core>::Row(TranscriptRow::info(
        NoticeScope::Host,
        "the third session opened",
    ));
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions)
        .serving("s1", |id| Some(root_named(&format!("tree-{id}"))));
    connection.request(1, Op::Attach { node: root() });
    overflow_after_a_switch(&mut connection, "s2");
    for observation in [
        SessionObservation::SessionOpening,
        notice.clone(),
        binding("s3"),
    ] {
        assert!(connection.observe(observation).unwrap());
    }
    assert!(connection.frame(attach_line(2, "tree-s3")).unwrap());
    connection.flush().unwrap();
    drop(connection);
    let frames = host_frames(&stdout);
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, HostFrame::ResyncRequired { .. })),
        "{frames:?}"
    );
    assert!(frames.iter().any(|frame| matches!(
        frame,
        HostFrame::Event { observation, .. } if *observation == notice
    )));
}

/// The `IntentStarted` the session announces as it takes `delivery`.
fn started_of(
    delivery: &plakous_host_protocol::IntentDelivery<plakous_host_protocol::Core>,
) -> SessionObservation<plakous_host_protocol::Core> {
    match delivery.started() {
        Some(plakous_host_protocol::ContentObservation::IntentStarted(started)) => {
            SessionObservation::IntentStarted(started)
        }
        other => panic!("no start: {other:?}"),
    }
}

/// A resume request goes to the session as its own resume, correlated to
/// the request: the switch it causes answers it `moved` with the new root
/// before the requester's `moved` frame, which names the request. An error
/// in its window refuses it `session_unavailable`, the session unchanged. A
/// second request while one is in flight is refused; a retry or a rebind,
/// which only a launcher can do, is refused `session_unavailable` at once;
/// a child attachment is refused them for its role.
#[test]
fn a_resume_request_is_answered_by_its_switch_or_its_error() {
    use plakous_host_protocol::Core;
    use plakous_host_protocol::views::transcript::NoticeScope;
    let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions)
        .serving("s1", |id| Some(root_named(&format!("tree-{id}"))));
    assert!(matches!(
        connection.request(1, Op::Attach { node: root() }),
        Some(Outcome::Ok(Answer::Attached { attachment: 1 }))
    ));
    while intents.try_recv().is_ok() {}
    let resume = |request_id: &str, session: &str| Op::ResumeSession {
        attachment: 1,
        request_id: request_id.into(),
        session: session.into(),
    };
    assert_eq!(connection.request(2, resume("r1", "lost")), None);
    let refused = intents.try_recv().expect("the resume reached the session");
    assert_eq!(
        *refused,
        SessionIntent::ResumeSession {
            id: Some("lost".into())
        }
    );
    assert_eq!(
        connection.request(3, resume("r2", "s2")),
        Some(Outcome::Refused(RequestRefusal::OneInFlight))
    );
    for observation in [
        started_of(&refused),
        SessionObservation::Row(TranscriptRow::error(
            NoticeScope::Session,
            "resume: no such session",
        )),
    ] {
        assert!(connection.observe(observation).unwrap());
    }
    connection.flush().unwrap();
    assert_eq!(connection.attachment, Some(1), "the session is unchanged");
    for (id, op) in [
        (
            4,
            Op::RetryAttach {
                attachment: 1,
                request_id: "r3".into(),
            },
        ),
        (
            5,
            Op::Rebind {
                attachment: 1,
                request_id: "r4".into(),
                cause: "asked".into(),
            },
        ),
    ] {
        assert!(matches!(
            connection.request(id, op),
            Some(Outcome::Refused(RequestRefusal::SessionUnavailable { .. }))
        ));
    }
    assert_eq!(connection.request(6, resume("r5", "s2")), None);
    let switching = intents.try_recv().expect("the resume reached the session");
    for observation in [
        started_of(&switching),
        SessionObservation::SessionOpening,
        binding("s2"),
    ] {
        assert!(connection.observe(observation).unwrap());
    }
    connection.flush().unwrap();
    drop(connection);
    let frames = host_frames(&stdout);
    assert!(frames.iter().any(|frame| matches!(
        frame,
        HostFrame::Response {
            id: 2,
            outcome: Outcome::Refused(RequestRefusal::SessionUnavailable { detail })
        } if detail == "resume: no such session"
    )));
    let answered = frames
        .iter()
        .position(|frame| {
            matches!(frame, HostFrame::Response {
                id: 6,
                outcome: Outcome::Ok(Answer::Moved { node })
            } if *node == root_named("tree-s2"))
        })
        .expect("answered moved");
    assert!(
        matches!(
            frames.get(answered + 1),
            Some(HostFrame::Moved { attachment: 1, node, request_id: Some(request) })
                if *node == root_named("tree-s2") && request == "r5"
        ),
        "{:?}",
        &frames[answered..]
    );
}

/// Every connection's `moved` names the session request that caused the
/// switch, so a client tells another's request from its own; a switch no
/// request caused names none.
#[test]
fn every_moved_names_the_request_that_switched() {
    use plakous_host_protocol::Core;
    let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
    let (published, events) = plakous_host_protocol::observation_channel::<Core>();
    let session = WireSession::start(root(), "s1".to_owned(), actions, events, |id| {
        Some(root_named(&format!("tree-{id}")))
    });
    let (mut first_out, mut second_out) = (Vec::new(), Vec::new());
    let mut first = session.connect(&mut first_out, Face::Remote, || false);
    let mut second = session.connect(&mut second_out, Face::Local, || false);
    assert!(first.request(1, Op::Attach { node: root() }).is_some());
    assert!(second.request(1, Op::Attach { node: root() }).is_some());
    while intents.try_recv().is_ok() {}
    assert_eq!(
        first.request(
            2,
            Op::ResumeSession {
                attachment: 1,
                request_id: "r1".into(),
                session: "s2".into(),
            }
        ),
        None
    );
    let resume = intents.try_recv().expect("the resume reached the session");
    let switch = |observations: [SessionObservation<Core>; 3], next: &str| {
        for observation in observations {
            published.send(observation).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.session_id() != next {
            assert!(Instant::now() < deadline, "the switch to {next}");
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    switch(
        [
            started_of(&resume),
            SessionObservation::SessionOpening,
            binding("s2"),
        ],
        "s2",
    );
    first.flush().unwrap();
    second.flush().unwrap();
    assert!(
        second
            .request(
                2,
                Op::Attach {
                    node: root_named("tree-s2")
                }
            )
            .is_some()
    );
    switch(
        [
            SessionObservation::IntentStarted(None),
            SessionObservation::SessionOpening,
            binding("s3"),
        ],
        "s3",
    );
    second.flush().unwrap();
    drop((first, second));
    let moved = |frames: Vec<HostFrame<Core>>| {
        frames
            .into_iter()
            .filter_map(|frame| match frame {
                HostFrame::Moved { request_id, .. } => Some(request_id),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(moved(host_frames(&first_out)), [Some("r1".to_owned())]);
    assert_eq!(
        moved(host_frames(&second_out)),
        [Some("r1".to_owned()), None]
    );
    assert!(host_frames(&first_out).iter().any(|frame| matches!(
        frame,
        HostFrame::Response {
            id: 2,
            outcome: Outcome::Ok(Answer::Moved { .. })
        }
    )));
}

/// Client request ids are only unique per connection: when two
/// connections use the same one, a switch answers only the connection whose
/// attachment asked; the other's request is answered `not_attached` as its
/// attachment ends, and may still switch later.
#[test]
fn a_switch_answers_only_the_connection_that_asked() {
    use plakous_host_protocol::Core;
    let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
    let (published, events) = plakous_host_protocol::observation_channel::<Core>();
    let session = WireSession::start(root(), "s1".to_owned(), actions, events, |id| {
        Some(root_named(&format!("tree-{id}")))
    });
    let (mut first_out, mut second_out) = (Vec::new(), Vec::new());
    let mut first = session.connect(&mut first_out, Face::Remote, || false);
    let mut second = session.connect(&mut second_out, Face::Remote, || false);
    assert!(first.request(1, Op::Attach { node: root() }).is_some());
    assert!(second.request(1, Op::Attach { node: root() }).is_some());
    while intents.try_recv().is_ok() {}
    for (connection, attachment, target) in [(&mut first, 1, "s2"), (&mut second, 2, "s3")] {
        assert_eq!(
            connection.request(
                2,
                Op::ResumeSession {
                    attachment,
                    request_id: "1".into(),
                    session: target.into(),
                }
            ),
            None
        );
    }
    let first_resume = intents.try_recv().expect("the first resume");
    for observation in [
        started_of(&first_resume),
        SessionObservation::SessionOpening,
        binding("s2"),
    ] {
        published.send(observation).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.session_id() != "s2" {
        assert!(Instant::now() < deadline, "the switch");
        std::thread::sleep(Duration::from_millis(2));
    }
    first.flush().unwrap();
    second.flush().unwrap();
    drop((first, second));
    let answer = |frames: Vec<HostFrame<Core>>| {
        frames.into_iter().find_map(|frame| match frame {
            HostFrame::Response { id: 2, outcome } => Some(outcome),
            _ => None,
        })
    };
    assert_eq!(
        answer(host_frames(&first_out)),
        Some(Outcome::Ok(Answer::Moved {
            node: root_named("tree-s2")
        }))
    );
    assert_eq!(
        answer(host_frames(&second_out)),
        Some(Outcome::Refused(RequestRefusal::NotAttached))
    );
}

/// A transcript the session's mailbox lost content of ends the attachments
/// holding it with `resync_required` once, and drops the root baseline
/// kept for attaches, so the next attach asks the session for a fresh
/// one; an attach during the same loss is not ended again, and a later
/// loss ends its holders again.
#[test]
fn a_lost_transcript_resyncs_its_holders_once_per_loss() {
    use plakous_host_protocol::Core;
    use plakous_host_protocol::limits::OBSERVATION_QUEUE_CAPACITY;
    use plakous_host_protocol::views::PresentationSnapshot;
    use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptPatch};
    let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
    let (published, events) = plakous_host_protocol::observation_channel::<Core>();
    let service = HostService::new(actions, root(), ATTACHMENT_CAPACITY);
    service.observe(SessionObservation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let first = service.attach(&root(), Face::Remote).unwrap();
    assert!(intents.try_recv().is_err(), "a kept baseline needs no ask");
    let drop_root_patch = || {
        for index in 0..OBSERVATION_QUEUE_CAPACITY {
            let row = TranscriptRow::error(NoticeScope::Host, format!("error {index}"));
            let _ = published.send(SessionObservation::Row(row));
        }
        let patch = TranscriptPatch {
            scope: TranscriptScope::Root,
            from_revision: 1,
            to_revision: 2,
            upserts: Vec::new(),
            deletes: Vec::new(),
        };
        assert!(
            published
                .send(SessionObservation::TranscriptPatch(patch))
                .is_err()
        );
        assert!(events.transcript_resnapshot_required_for(&TranscriptScope::Root));
    };
    let ended = |attachment| {
        service
            .take(attachment)
            .iter()
            .any(|item| matches!(item, Outbound::ResyncRequired))
    };
    let mut losses = LostTranscripts::default();
    losses.check(&service, &events);
    assert!(!ended(first), "no loss, no resync");
    drop_root_patch();
    losses.check(&service, &events);
    assert!(ended(first));
    let second = service.attach(&root(), Face::Remote).unwrap();
    assert!(
        matches!(
            intents.try_recv().as_deref(),
            Ok(SessionIntent::ResyncTranscript {
                scope: TranscriptScope::Root
            })
        ),
        "the attach asks for a fresh baseline"
    );
    losses.check(&service, &events);
    assert!(!ended(second), "the same loss ends no later attach");
    let _ = events.try_iter().count();
    published
        .send(SessionObservation::PresentationSnapshot(
            PresentationSnapshot::empty(2),
        ))
        .unwrap();
    losses.check(&service, &events);
    let _ = events.try_iter().count();
    drop_root_patch();
    losses.check(&service, &events);
    assert!(ended(second), "a later loss ends its holders again");
}

/// What one `quit` intent on the root attachment answers, and whether it
/// asked the session to quit.
fn quit_on_root(leave: Leave) -> (Option<Outcome<plakous_host_protocol::Core>>, bool) {
    let (actions, intents) = plakous_host_protocol::intent_channel::<plakous_host_protocol::Core>();
    let service = HostService::new(actions.clone(), root(), ATTACHMENT_CAPACITY)
        .with_nodes(std::sync::Arc::new(TreeNodes::new(root().tree_id)));
    let mut stdout = Vec::new();
    let mut connection = WireConnection::on_service(service, &mut stdout, actions, leave);
    let Some(Outcome::Ok(Answer::Attached { attachment })) =
        connection.request(1, Op::Attach { node: root() })
    else {
        panic!("attached");
    };
    let outcome = connection.request(
        2,
        Op::Intent {
            client_id: None,
            attachment,
            intent: SessionIntent::Quit,
            blobs: Vec::new(),
        },
    );
    if outcome == Some(Outcome::Ok(Answer::Detached)) {
        assert_eq!(connection.attachment, None);
    }
    drop(connection);
    let mut quit = false;
    while let Ok(delivery) = intents.try_recv() {
        quit |= matches!(*delivery, SessionIntent::Quit);
    }
    (outcome, quit)
}

/// In a host (connections that detach when they leave) the session outlives
/// its clients: quitting the root attachment detaches it and the session is
/// never asked to quit. On stdio, the connection's only client, quitting
/// quits the session as before.
#[test]
fn a_quit_on_the_root_detaches_in_a_host_and_quits_on_stdio() {
    let (hosted, quit) = quit_on_root(Leave::Detach);
    assert_eq!(hosted, Some(Outcome::Ok(Answer::Detached)));
    assert!(!quit, "the host's session was asked to quit");
    let (stdio, quit) = quit_on_root(Leave::Quit);
    assert_eq!(
        stdio,
        Some(Outcome::Ok(Answer::Accepted { duplicate: false }))
    );
    assert!(quit, "stdio's session was not asked to quit");
}

/// `stop_session` names a session: one the connection does not serve is
/// refused `unknown_session` and nothing reaches the session it serves.
#[test]
fn a_stop_of_a_session_not_served_is_refused_unknown() {
    let (actions, intents) = plakous_host_protocol::intent_channel::<plakous_host_protocol::Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions).serving("s1", |_| None);
    assert_eq!(
        connection.request(
            1,
            Op::StopSession {
                session: "s2".to_owned()
            }
        ),
        Some(Outcome::Refused(RequestRefusal::UnknownSession))
    );
    assert!(intents.try_recv().is_err());
}

/// A stop of the served session needs no attachment: it enters the
/// session's mailbox as its stop, a second one while it is in flight is
/// refused `one_in_flight`, and the request is answered `stopped` only once
/// the session settled the stop, and then again for a repeated stop.
#[test]
fn a_stop_is_answered_once_the_session_settled_it() {
    let (actions, intents) = plakous_host_protocol::intent_channel::<plakous_host_protocol::Core>();
    let mut stdout = Vec::new();
    let pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = pending.clone();
    let mut connection = WireConnection::new(root(), &mut stdout, actions)
        .serving("s1", |_| None)
        .with_runner_pending(Some(std::sync::Arc::new(move || {
            observed.load(std::sync::atomic::Ordering::SeqCst)
        })));
    let stop = |id| ClientFrame::<plakous_host_protocol::Core>::Request {
        id,
        op: Op::StopSession {
            session: "s1".to_owned(),
        },
    };
    let line = |frame| Ok(encode_line(&frame).unwrap().trim_end().to_owned());
    assert!(connection.frame(line(stop(1))).unwrap());
    assert_eq!(
        connection.request(
            2,
            Op::StopSession {
                session: "s1".to_owned()
            }
        ),
        Some(Outcome::Refused(RequestRefusal::OneInFlight))
    );
    let SessionIntent::StopSession { ticket } = &*intents.try_recv().unwrap() else {
        panic!("the stop did not reach the session");
    };
    let ticket = *ticket;
    connection.flush().unwrap();
    pending.store(true, std::sync::atomic::Ordering::SeqCst);
    intents.settle_stop(ticket, Ok(()));
    connection.flush().unwrap();
    assert!(connection.frame(line(stop(3))).unwrap());
    let SessionIntent::StopSession { ticket } = &*intents.try_recv().unwrap() else {
        panic!("the repeated stop did not reach the session");
    };
    pending.store(false, std::sync::atomic::Ordering::SeqCst);
    intents.settle_stop(*ticket, Ok(()));
    connection.flush().unwrap();
    drop(connection);
    let responses: Vec<serde_json::Value> = String::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|frame: &serde_json::Value| frame["frame"] == "response")
        .collect();
    assert_eq!(
        responses,
        [
            serde_json::json!({"frame": "response", "id": 1, "outcome": {"ok": {"answer": "stopped", "runner_pending": true}}}),
            serde_json::json!({"frame": "response", "id": 3, "outcome": {"ok": {"answer": "stopped", "runner_pending": false}}}),
        ]
    );
}

/// A stop the session ends before settling is answered with why, never
/// `stopped`.
#[test]
fn a_stop_whose_session_ended_first_is_not_answered_stopped() {
    let (actions, intents) = plakous_host_protocol::intent_channel::<plakous_host_protocol::Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions).serving("s1", |_| None);
    assert_eq!(
        connection.request(
            1,
            Op::StopSession {
                session: "s1".to_owned()
            }
        ),
        None
    );
    drop(intents);
    connection.flush().unwrap();
    drop(connection);
    let text = String::from_utf8(stdout).unwrap();
    assert!(text.contains("session_unavailable"), "{text}");
    assert!(!text.contains("stopped\""), "{text}");
}

/// A connection that serves one session has no session directory: each of
/// its requests is refused unsupported, naming the op, and nothing reaches
/// the session.
#[test]
fn a_single_session_connection_refuses_the_directory_requests() {
    let (actions, intents) = plakous_host_protocol::intent_channel::<plakous_host_protocol::Core>();
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions);
    for (op, name) in [
        (
            Op::ListSessions {
                runner: None,
                include_archived: false,
                workspace: None,
            },
            "list_sessions",
        ),
        (
            Op::WatchSessions {
                include_archived: false,
            },
            "watch_sessions",
        ),
        (Op::UnwatchSessions { attachment: 1 }, "unwatch_sessions"),
        (
            Op::NewSession {
                runner: plakous_host_protocol::wire::RunnerLabel::local(),
                profile: None,
                model: None,
                workspace: "/w".to_owned(),
                template: None,
                request_id: "r".to_owned(),
            },
            "new_session",
        ),
        (
            Op::ReleaseSession {
                session: "s1".to_owned(),
            },
            "release_session",
        ),
    ] {
        assert_eq!(
            connection.request(1, op),
            Some(Outcome::Refused(RequestRefusal::Unsupported {
                detail: format!("{name} needs a host of many sessions"),
            }))
        );
    }
    assert!(intents.try_recv().is_err());
}
/// `detached` as a host's root quit does: one answer for one meaning.
#[test]
fn a_childs_quit_answers_detached() {
    use plakous_host_protocol::Core;
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let service = HostService::new(actions.clone(), root(), ATTACHMENT_CAPACITY)
        .with_nodes(std::sync::Arc::new(OpenChildren));
    let mut stdout = Vec::new();
    let mut connection = WireConnection::on_service(service, &mut stdout, actions, Leave::Quit);
    let child = NodeAddress {
        tree_id: "t1".into(),
        agent_id: "c1".into(),
    };
    let Some(Outcome::Ok(Answer::Attached { attachment })) =
        connection.request(1, Op::Attach { node: child })
    else {
        panic!("attached");
    };
    assert_eq!(
        connection.request(
            2,
            Op::Intent {
                client_id: None,
                attachment,
                intent: SessionIntent::Quit,
                blobs: Vec::new(),
            },
        ),
        Some(Outcome::Ok(Answer::Detached))
    );
    assert_eq!(connection.attachment, None);
}

/// A clear (its binding, then `ClearView`, with no opening) empties the
/// process list every attachment holds as a state: a connection that
/// attaches after it gets an empty list in its attach snapshot, not the
/// cleared session's rows.
#[test]
fn an_attach_after_a_clear_holds_no_process_of_the_cleared_session() {
    use plakous_host_protocol::Core;
    use plakous_host_protocol::views::{ProcessStatus, ProcessView};
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let (observations, events) = plakous_host_protocol::observation_channel::<Core>();
    let sender = crate::runtime::Sender::over(observations);
    sender.publish_processes(vec![ProcessView {
        id: "s1:main:p1".into(),
        command: "make".into(),
        owner: "root".into(),
        started_at: 1,
        elapsed_ms: 100,
        status: ProcessStatus::Running,
        output: String::new(),
    }]);
    let _ = sender.send(crate::surface_protocol::ContentEvent::ClearView);
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions);
    while let Ok(event) = events.try_recv() {
        assert!(connection.observe(event).unwrap());
    }
    assert!(connection.frame(attach_line(1, "t1")).unwrap());
    connection.flush().unwrap();
    drop(connection);
    let held: Vec<_> = host_frames(&stdout)
        .into_iter()
        .filter_map(|frame| match frame {
            HostFrame::Event {
                observation: SessionObservation::Processes { views, running },
                ..
            } => Some((views.len(), running)),
            _ => None,
        })
        .collect();
    assert_eq!(held, [(0, 0)], "{held:?}");
}

/// A wire face is shown a job's lifecycle redacted as a process row is: a
/// heredoc's token in the displayed command, and the fragment of a tail
/// cut inside its only line, never reach the client.
#[test]
fn a_jobs_lifecycle_reaches_a_wire_face_redacted() {
    use plakous_host_protocol::Core;
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let token = "ghp_abcdefghijklmnopqrstuvwxyz0123";
    // A tail the runtime cut inside its only line, inside the token.
    let tail = format!("{}{}", plakous::tools::JOB_TAIL_FRAGMENT, &token[4..]);
    let mut stdout = Vec::new();
    let mut connection = WireConnection::new(root(), &mut stdout, actions);
    assert!(connection.frame(attach_line(1, "t1")).unwrap());
    for observation in [
        SessionObservation::JobStarted {
            job_id: "p1".into(),
            owner: "root".into(),
            command: format!("gh auth login --with-token <<EOF\\n{token}\\nEOF"),
        },
        SessionObservation::JobTerminal {
            job_id: "p1".into(),
            owner: "root".into(),
            status: "exited".into(),
            exit_code: Some(0),
            stop_reason: None,
            elapsed_ms: 10,
            output: tail,
        },
    ] {
        assert!(connection.observe(observation).unwrap());
    }
    connection.flush().unwrap();
    drop(connection);
    let mut shown = Vec::new();
    for frame in host_frames(&stdout) {
        match frame {
            HostFrame::Event {
                observation: SessionObservation::JobStarted { command, .. },
                ..
            } => shown.push(command),
            HostFrame::Event {
                observation: SessionObservation::JobTerminal { output, .. },
                ..
            } => shown.push(output),
            _ => {}
        }
    }
    assert_eq!(shown.len(), 2, "{shown:?}");
    assert_eq!(
        shown[0],
        "gh auth login --with-token <<EOF\\n[redacted]\\nEOF"
    );
    assert_eq!(shown[1], plakous::tools::JOB_TAIL_FRAGMENT);
}

/// A session that ends while its client still reads ends the connection
/// with `bye` at once: the connection does not wait for the client to
/// leave first.
#[test]
fn a_session_that_ends_ends_its_connection() {
    use plakous_host_protocol::Core;
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let (observations, events) = plakous_host_protocol::observation_channel::<Core>();
    let (client, lines) = lines(4);
    client.send(attach_line(1, "t1")).unwrap();
    drop(observations);
    let mut stdout = Vec::new();
    WireConnection::new(root(), &mut stdout, actions)
        .serve(&lines, &events)
        .unwrap();
    assert!(matches!(host_frames(&stdout).last(), Some(HostFrame::Bye)));
    drop(client);
}
