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

/// A connection serves the session its welcome named: a binding that names
/// that session again changes nothing, and one that names another (the
/// session switched) ends the connection as a leaving client does, with the
/// session quit; a process asked to stop does the same.
#[test]
fn a_session_switch_or_a_stop_ends_the_connection() {
    use plakous_host_protocol::Core;
    let binding = |session: &str| SessionObservation::<Core>::Binding {
        session_id: Some(session.to_owned()),
        account: None,
        model: None,
        effort: None,
        extra: None,
        composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
        protocol: plakous_host_protocol::ProtocolSelection::Auto,
        effective_presentation: "direct".to_owned(),
    };
    for (observations, stop) in [
        (vec![binding("s1"), binding("s2")], false),
        (vec![binding("s1")], true),
    ] {
        let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
        let (published, events) = plakous_host_protocol::observation_channel::<Core>();
        let (_client, lines) = mpsc::sync_channel::<Result<String, WireError>>(1);
        for observation in observations {
            published.send(observation).unwrap();
        }
        let session = std::thread::spawn(move || {
            let quit = intents
                .recv()
                .is_ok_and(|delivery| matches!(*delivery, SessionIntent::Quit));
            drop(published);
            quit
        });
        let mut stdout = Vec::new();
        WireConnection::new(root(), &mut stdout, actions)
            .with_stop(move || stop)
            .serving("s1")
            .serve(&lines, &events)
            .unwrap();
        assert!(session.join().unwrap(), "the session was quit");
        assert!(matches!(host_frames(&stdout).last(), Some(HostFrame::Bye)));
    }
}
