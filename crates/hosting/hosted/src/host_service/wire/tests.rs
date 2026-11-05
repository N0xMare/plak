use plakous_host_protocol::intent::Draft;

use super::*;

fn root() -> NodeAddress {
    NodeAddress {
        tree_id: "t1".into(),
        agent_id: "t1".into(),
    }
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
            (7, Outcome::Ok(Answer::Accepted)),
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
    let (client, lines) = mpsc::sync_channel(8);
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

fn welcome() -> Welcome {
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
    let (_client, lines) = mpsc::sync_channel(1);
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
        Some(Outcome::Refused(RequestRefusal::UnknownNode))
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
        HostFrame::Moved { attachment: 1, node } if *node == root_named("tree-s2")
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
        Some(Outcome::Refused(RequestRefusal::UnknownNode)),
        "a connection that followed the switch knows the new root"
    );
}

/// A process asked to stop ends the connection as a leaving client does,
/// with the session quit.
#[test]
fn a_stop_ends_the_connection() {
    use plakous_host_protocol::Core;
    let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
    let (published, events) = plakous_host_protocol::observation_channel::<Core>();
    let (_client, lines) = mpsc::sync_channel::<Result<String, WireError>>(1);
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
                matches!(frame, HostFrame::Moved { attachment: held, node }
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
