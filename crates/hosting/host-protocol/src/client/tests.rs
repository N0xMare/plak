use std::io::{BufRead as _, BufReader};
use std::os::unix::net::UnixStream;

use super::*;
use crate::Core;
use crate::wire::{WIRE_VERSION, WelcomeSession, decode_line};

mod slow_link;

#[test]
fn product_host_responses_route_by_id_without_attaching_a_session() {
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Reads;
    impl ProtocolExtension for Reads {
        type Intent = crate::Never;
        type Observation = crate::Never;
        type HostRequest = u8;
        type HostResponse = u8;
        type WelcomeExtension = Option<crate::Never>;
    }

    let (mut peer, client) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut reader = BufReader::new(peer.try_clone().unwrap());
    let transport = Transport::new(client.try_clone().unwrap(), client);
    let connecting = thread::spawn(move || connect_directory::<Reads>(transport, hello()));
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(matches!(
        decode_line::<ClientFrame<Reads>>(&line).unwrap(),
        ClientFrame::Hello(_)
    ));
    peer.write_all(
        encode_line(&HostFrame::<Reads>::Welcome(Welcome {
            extension: None,
            proto: WIRE_VERSION,
            binary_id: "host".into(),
            host_generation: 1,
            caps: Vec::new(),
            session: None,
        }))
        .unwrap()
        .as_bytes(),
    )
    .unwrap();
    let (sender, receiver, welcome) = connecting.join().unwrap().unwrap();
    assert!(welcome.session.is_none());
    let first = sender.request_operation(Op::Host { request: 1 }).unwrap();
    let second = sender.request_operation(Op::Host { request: 2 }).unwrap();
    for (id, request) in [(first, 1), (second, 2)] {
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            decode_line::<ClientFrame<Reads>>(&line).unwrap(),
            ClientFrame::Request {
                id,
                op: Op::Host { request }
            }
        );
    }
    for (id, response) in [(second, 42), (first, 41)] {
        peer.write_all(
            encode_line(&HostFrame::<Reads>::Response {
                id,
                outcome: Outcome::Ok(Answer::Host { response }),
            })
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            receiver.recv_response(Duration::from_secs(3)).unwrap(),
            (id, Outcome::Ok(Answer::Host { response }))
        );
    }
    peer.shutdown(std::net::Shutdown::Both).unwrap();
}

fn hello() -> Hello {
    Hello {
        proto: WIRE_VERSION,
        binary_id: "client".into(),
        caps: Vec::new(),
    }
}

fn node(tree: &str) -> NodeAddress {
    NodeAddress {
        tree_id: tree.into(),
        agent_id: tree.into(),
    }
}

fn welcome() -> Welcome<Core> {
    Welcome {
        extension: None,
        proto: WIRE_VERSION,
        binary_id: "host".into(),
        host_generation: 1,
        caps: Vec::new(),
        session: Some(WelcomeSession {
            session_id: "s1".into(),
            tree_id: "t1".into(),
            root: node("t1"),
        }),
    }
}

/// A scripted host on the other end of a socket pair.
struct Host {
    lines: BufReader<UnixStream>,
    stream: UnixStream,
}

impl Host {
    fn pair() -> (Self, Transport) {
        let (host, client) = UnixStream::pair().unwrap();
        let shutdown = client.try_clone().unwrap();
        let transport = Transport::new(client.try_clone().unwrap(), client)
            .with_write_shutdown(move || shutdown.shutdown(std::net::Shutdown::Write));
        (
            Self {
                lines: BufReader::new(host.try_clone().unwrap()),
                stream: host,
            },
            transport,
        )
    }

    fn read(&mut self) -> ClientFrame<Core> {
        let mut line = String::new();
        self.lines.read_line(&mut line).unwrap();
        decode_line(&line).unwrap()
    }

    fn send(&mut self, frame: &HostFrame<Core>) {
        self.stream
            .write_all(encode_line(frame).unwrap().as_bytes())
            .unwrap();
    }

    /// Reads the next request: its id and op.
    fn request(&mut self) -> (RequestId, Op<Core>) {
        match self.read() {
            ClientFrame::Request { id, op } => (id, op),
            other => panic!("not a request: {other:?}"),
        }
    }
}

#[test]
fn the_bye_line_is_the_bye_frame() {
    assert_eq!(encode_line(&ClientFrame::<Core>::Bye).unwrap(), BYE_LINE);
}

#[test]
fn goodbye_half_closes_once_and_keeps_the_receiver_draining() {
    let (mut host, sender, receiver) = attached(1);
    host.stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let cloned = sender.clone();
    sender.goodbye();
    receiver.goodbye();
    cloned.goodbye();
    assert_eq!(host.read(), ClientFrame::Bye);
    assert_eq!(host.lines.read_line(&mut String::new()).unwrap(), 0);
    assert!(matches!(
        sender.send(SessionIntent::Interrupt),
        Err(SendError::Refused(IntentSendError::Disconnected(_)))
    ));
    assert_eq!(
        sender.request_operation(Op::Ping).unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
    host.send(&event(1, true));
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
        SessionObservation::SetBusy(true)
    );
    host.send(&HostFrame::Bye);
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(3)),
        Err(RecvTimeoutError::Disconnected)
    ));
    drop((sender, cloned, receiver));
    assert_eq!(host.lines.read_line(&mut String::new()).unwrap(), 0);
}

#[test]
fn goodbye_does_not_send_waiting_intents_when_an_attach_answers_late() {
    let (mut host, transport) = Host::pair();
    host.stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let client = thread::spawn(move || connect::<Core>(transport, hello()).unwrap());
    assert!(matches!(host.read(), ClientFrame::Hello(_)));
    host.send(&HostFrame::Welcome(welcome()));
    let (sender, receiver, _) = client.join().unwrap();
    let (attach, _) = host.request();
    sender
        .send(SessionIntent::Prompt("waiting".into()))
        .unwrap();
    receiver.goodbye();
    assert_eq!(host.read(), ClientFrame::Bye);
    assert_eq!(host.lines.read_line(&mut String::new()).unwrap(), 0);
    host.send(&HostFrame::Response {
        id: attach,
        outcome: Outcome::Ok(Answer::Attached { attachment: 1 }),
    });
    host.send(&event(1, true));
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
        SessionObservation::SetBusy(true)
    );
    host.send(&HostFrame::Bye);
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(3)),
        Err(RecvTimeoutError::Disconnected)
    ));
    assert!(matches!(receiver.try_unsent(), Some(SendError::Refused(
        IntentSendError::Disconnected(SessionIntent::Prompt(ref draft)))) if draft.text == "waiting"));
    assert!(receiver.try_unsent().is_none());
}

#[test]
fn a_directory_client_selects_a_root_without_a_default_session() {
    let (mut host, transport) = Host::pair();
    let client = thread::spawn(move || connect_directory::<Core>(transport, hello()).unwrap());
    assert!(matches!(host.read(), ClientFrame::Hello(_)));
    host.send(&HostFrame::Welcome(Welcome {
        extension: None,
        session: None,
        ..welcome()
    }));
    let (sender, receiver, _) = client.join().unwrap();
    let id = sender
        .request_operation(Op::ListSessions {
            runner: None,
            include_archived: false,
            workspace: Some("/workspace".into()),
        })
        .unwrap();
    assert_eq!(
        host.request(),
        (
            id,
            Op::ListSessions {
                runner: None,
                include_archived: false,
                workspace: Some("/workspace".into())
            }
        )
    );
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Sessions { rows: Vec::new() }),
    });
    assert_eq!(
        receiver.recv_response(Duration::from_secs(5)).unwrap(),
        (id, Outcome::Ok(Answer::Sessions { rows: Vec::new() }))
    );
    sender.attach_to(node("chosen")).unwrap();
    let (id, op) = host.request();
    assert_eq!(
        op,
        Op::Attach {
            node: node("chosen")
        }
    );
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment: 7 }),
    });
    host.send(&HostFrame::Event {
        client_id: None,
        attachment: 7,
        seq: 1,
        class: crate::observation::ObservationClass::State,
        observation: SessionObservation::SetBusy(true),
        blobs: Vec::new(),
    });
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
        SessionObservation::SetBusy(true)
    );
    drop(sender);
    drop(receiver);
    assert!(matches!(host.read(), ClientFrame::Bye));
}

#[test]
fn directory_responses_keep_refusals_and_disconnect_is_terminal() {
    let (mut host, transport) = Host::pair();
    let client = thread::spawn(move || connect_directory::<Core>(transport, hello()).unwrap());
    assert!(matches!(host.read(), ClientFrame::Hello(_)));
    host.send(&HostFrame::Welcome(Welcome {
        extension: None,
        session: None,
        ..welcome()
    }));
    let (sender, receiver, _) = client.join().unwrap();
    assert_eq!(
        receiver.recv_response(Duration::ZERO),
        Err(RecvTimeoutError::Timeout)
    );
    let id = sender
        .request_operation(Op::ListSessions {
            runner: None,
            include_archived: false,
            workspace: None,
        })
        .unwrap();
    assert_eq!(host.request().0, id);
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Refused(RequestRefusal::Backpressure),
    });
    assert_eq!(
        receiver.recv_response(Duration::from_secs(5)).unwrap(),
        (id, Outcome::Refused(RequestRefusal::Backpressure))
    );
    drop(host);
    assert_eq!(
        receiver.recv_response(Duration::from_secs(5)),
        Err(RecvTimeoutError::Disconnected)
    );
    assert!(sender.request_operation(Op::Ping).is_err());
}

#[test]
fn a_refused_selection_keeps_the_old_attachment_and_can_select_again() {
    let (mut host, transport) = Host::pair();
    let client = thread::spawn(move || connect_directory::<Core>(transport, hello()).unwrap());
    assert!(matches!(host.read(), ClientFrame::Hello(_)));
    host.send(&HostFrame::Welcome(Welcome {
        extension: None,
        session: None,
        ..welcome()
    }));
    let (sender, receiver, _) = client.join().unwrap();
    let first = sender.attach_to(node("old")).unwrap();
    assert_eq!(host.request().0, first);
    host.send(&HostFrame::Response {
        id: first,
        outcome: Outcome::Ok(Answer::Attached { attachment: 7 }),
    });
    assert_eq!(
        receiver.recv_response(Duration::from_secs(2)).unwrap().0,
        first
    );
    let refused = sender.attach_to(node("held")).unwrap();
    assert_eq!(host.request().0, refused);
    let reason = RequestRefusal::TreeHostedElsewhere {
        host_name: "holder".into(),
        binary: None,
        pid: 4242,
    };
    host.send(&HostFrame::Response {
        id: refused,
        outcome: Outcome::Refused(reason.clone()),
    });
    assert_eq!(
        receiver.recv_response(Duration::from_secs(2)).unwrap(),
        (refused, Outcome::Refused(reason))
    );
    assert_eq!(receiver.ended(), None);
    assert_eq!(lock(&sender.connection.inner.state).root, node("old"));
    sender.send(SessionIntent::Quit).unwrap();
    assert!(matches!(host.request().1, Op::Intent { attachment: 7, .. }));
    let next = sender.attach_to(node("next")).unwrap();
    assert_eq!(host.request().0, next);
    host.send(&HostFrame::Response {
        id: next,
        outcome: Outcome::Ok(Answer::Attached { attachment: 9 }),
    });
    assert_eq!(
        receiver.recv_response(Duration::from_secs(2)).unwrap().0,
        next
    );
    assert_eq!(receiver.ended(), None);
}

#[test]
fn the_welcome_deadline_interrupts_only_a_missing_welcome() {
    for welcome_now in [false, true] {
        let (mut host, transport) = Host::pair();
        let shutdown = host.stream.try_clone().unwrap();
        let transport = transport.with_welcome_deadline(Duration::from_millis(1000), move || {
            shutdown.shutdown(std::net::Shutdown::Both).unwrap();
        });
        let (deliver, result) = mpsc::channel();
        let client = thread::spawn(move || {
            let _ = deliver.send(connect_directory::<Core>(transport, hello()));
        });
        assert!(matches!(host.read(), ClientFrame::Hello(_)));
        if !welcome_now {
            assert!(matches!(
                result
                    .recv_timeout(Duration::from_secs(3))
                    .expect("the welcome deadline did not unblock the reader"),
                Err(ConnectError::HandshakeTimeout(_))
            ));
            client.join().unwrap();
            continue;
        }
        host.send(&HostFrame::Welcome(Welcome {
            extension: None,
            session: None,
            ..welcome()
        }));
        let (sender, receiver, _) = result
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        client.join().unwrap();
        let id = sender
            .request_operation(Op::ListSessions {
                runner: None,
                include_archived: false,
                workspace: None,
            })
            .unwrap();
        assert_eq!(host.request().0, id);
        thread::sleep(Duration::from_millis(1100));
        host.send(&HostFrame::Response {
            id,
            outcome: Outcome::Ok(Answer::Sessions { rows: Vec::new() }),
        });
        assert_eq!(
            receiver.recv_response(Duration::from_secs(2)).unwrap().0,
            id
        );
        assert_eq!(receiver.ended(), None);
    }
}

#[test]
fn a_refused_hello_is_the_connect_error() {
    let (mut host, transport) = Host::pair();
    let client = thread::spawn(move || connect::<Core>(transport, hello()).err());
    assert!(matches!(host.read(), ClientFrame::Hello(_)));
    host.send(&HostFrame::Refused(HandshakeRefusal::Busy));
    assert!(matches!(
        client.join().unwrap(),
        Some(ConnectError::Refused(HandshakeRefusal::Busy))
    ));
}

/// A host of many sessions welcomes the client to none: `connect` refuses
/// it, and `connect_to` attaches the root it was given, whatever the
/// welcome names.
#[test]
fn a_welcome_that_names_no_session_attaches_the_root_the_client_names() {
    let unnamed = || Welcome {
        extension: None,
        session: None,
        ..welcome()
    };
    let (mut host, transport) = Host::pair();
    let client = thread::spawn(move || connect::<Core>(transport, hello()).err());
    assert!(matches!(host.read(), ClientFrame::Hello(_)));
    host.send(&HostFrame::Welcome(unnamed()));
    assert!(matches!(
        client.join().unwrap(),
        Some(ConnectError::NoSession)
    ));
    for (welcome, attached) in [(unnamed(), node("t9")), (welcome(), node("t9"))] {
        let (mut host, transport) = Host::pair();
        let client =
            thread::spawn(move || connect_to::<Core>(transport, hello(), node("t9")).unwrap());
        assert!(matches!(host.read(), ClientFrame::Hello(_)));
        host.send(&HostFrame::Welcome(welcome));
        let _connected = client.join().unwrap();
        let (_, op) = host.request();
        assert_eq!(op, Op::Attach { node: attached });
    }
}

/// A `detached` for an attachment the client no longer holds is ignored;
/// one for its own attachment ends the connection.
#[test]
fn only_its_own_attachments_detached_ends_the_connection() {
    let (mut host, transport) = Host::pair();
    let client = thread::spawn(move || connect::<Core>(transport, hello()).unwrap());
    assert!(matches!(host.read(), ClientFrame::Hello(_)));
    host.send(&HostFrame::Welcome(welcome()));
    let (_sender, receiver, _) = client.join().unwrap();
    let (id, _) = host.request();
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment: 4 }),
    });
    host.send(&HostFrame::Detached { attachment: 3 });
    host.send(&HostFrame::Event {
        client_id: None,
        attachment: 4,
        seq: 1,
        class: SessionObservation::<Core>::SetBusy(true).class(),
        observation: SessionObservation::SetBusy(true),
        blobs: Vec::new(),
    });
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
        SessionObservation::SetBusy(true)
    );
    host.send(&HostFrame::Detached { attachment: 4 });
    let deadline = Instant::now() + Duration::from_secs(10);
    while receiver.ended().is_none() {
        assert!(Instant::now() < deadline, "its own detached did not end it");
        thread::sleep(Duration::from_millis(10));
    }
}

/// The client attaches the welcome's root, follows a refusal `moved` to
/// the root it names, takes only its own attachment's events, marks the
/// root transcript lost across a `moved` until the next root snapshot,
/// gives a refused intent back as the mailbox would, and says goodbye when
/// both halves go.
#[test]
fn the_client_follows_the_host() {
    let (mut host, transport) = Host::pair();
    let client = thread::spawn(move || connect::<Core>(transport, hello()).unwrap());
    assert!(matches!(host.read(), ClientFrame::Hello(_)));
    host.send(&HostFrame::Welcome(welcome()));
    let (sender, receiver, welcomed) = client.join().unwrap();
    assert_eq!(welcomed.session.unwrap().session_id, "s1");
    let (id, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t1") });
    // The session switched before the attach reached it.
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Refused(RequestRefusal::Moved { node: node("t2") }),
    });
    let (id, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t2") });
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment: 2 }),
    });
    for (attachment, busy) in [(1, false), (2, true)] {
        host.send(&HostFrame::Event {
            client_id: None,
            attachment,
            seq: 1,
            class: SessionObservation::<Core>::SetBusy(busy).class(),
            observation: SessionObservation::SetBusy(busy),
            blobs: Vec::new(),
        });
    }
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
        SessionObservation::SetBusy(true)
    );
    // Nothing was shown before the first attachment, so nothing was lost.
    assert!(!receiver.transcript_resnapshot_required_for(&TranscriptScope::Root));
    for page in crate::wire::page_snapshot(crate::views::PresentationSnapshot::empty(3), 2).unwrap()
    {
        host.send(&HostFrame::Snapshot(page));
    }
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
        SessionObservation::PresentationSnapshot(_)
    ));
    assert!(!receiver.transcript_resnapshot_required_for(&TranscriptScope::Root));

    sender.send(SessionIntent::Interrupt).unwrap();
    let (id, op) = host.request();
    assert!(matches!(op, Op::Intent { attachment: 2, .. }));
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Refused(RequestRefusal::Backpressure),
    });
    assert!(matches!(
        unsent(&receiver),
        SendError::Refused(IntentSendError::Saturated(SessionIntent::Interrupt))
    ));

    host.send(&HostFrame::Moved {
        attachment: 2,
        node: node("t3"),
        request_id: None,
    });
    let (id, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t3") });
    assert!(receiver.transcript_resnapshot_required_for(&TranscriptScope::Root));
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment: 3 }),
    });
    drop((sender, receiver));
    assert!(matches!(host.read(), ClientFrame::Bye));
}

/// The next intent the host did not take, within a liveness bound.
fn unsent(receiver: &WireReceiver<Core>) -> SendError<Core> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(unsent) = receiver.try_unsent() {
            return unsent;
        }
        assert!(Instant::now() < deadline, "no intent came back");
        thread::sleep(Duration::from_millis(1));
    }
}

/// A connected client and its scripted host, attached as `attachment`.
fn attached(attachment: AttachmentId) -> (Host, WireSender<Core>, WireReceiver<Core>) {
    let (mut host, transport) = Host::pair();
    let client = thread::spawn(move || connect::<Core>(transport, hello()).unwrap());
    assert!(matches!(host.read(), ClientFrame::Hello(_)));
    host.send(&HostFrame::Welcome(welcome()));
    let (sender, receiver, _) = client.join().unwrap();
    let (id, _) = host.request();
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment }),
    });
    (host, sender, receiver)
}

fn event(attachment: AttachmentId, busy: bool) -> HostFrame<Core> {
    let observation = SessionObservation::SetBusy(busy);
    HostFrame::Event {
        client_id: None,
        attachment,
        seq: 1,
        class: observation.class(),
        observation,
        blobs: Vec::new(),
    }
}

/// A large send blocked on a host that is not reading holds up neither the
/// face nor the reader: what the host says meanwhile (an event, a
/// `moved`) reaches the face at once, and once the host reads again it
/// gets the intent and then the re-attach.
#[test]
fn a_large_send_does_not_hold_up_the_face() {
    let (mut host, sender, receiver) = attached(1);
    let large = SessionIntent::Prompt(crate::Draft::from("x".repeat(4 * 1024 * 1024)));
    let started = Instant::now();
    sender.send(large).unwrap();
    // Sending never waits for the transport.
    assert!(started.elapsed() < Duration::from_secs(1));
    // The writer is blocked on the full transport by now.
    thread::sleep(Duration::from_secs(1));
    host.send(&event(1, true));
    host.send(&HostFrame::Moved {
        attachment: 1,
        node: node("t2"),
        request_id: None,
    });
    let started = Instant::now();
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
        SessionObservation::SetBusy(true)
    );
    while !receiver.transcript_resnapshot_required_for(&TranscriptScope::Root) {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the face waited {:?}",
        started.elapsed()
    );
    // Once the host reads again it takes the intent, then the re-attach.
    let (id, op) = host.request();
    assert!(matches!(op, Op::Intent { attachment: 1, .. }), "{op:?}");
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Accepted { duplicate: false }),
    });
    assert!(sender.settle(Duration::from_secs(10)));
    assert!(receiver.try_unsent().is_none());
    let (_, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t2") });
}

/// Transcript rows a slow face cannot hold are dropped as a baseline
/// repays them: the root transcript reads as lost, and the client keeps its
/// attachment.
#[test]
fn rows_a_slow_face_cannot_hold_leave_the_transcript_lost() {
    let (mut host, sender, receiver) = attached(1);
    for index in 0..=crate::limits::OBSERVATION_QUEUE_CAPACITY {
        let observation = SessionObservation::Row(TranscriptRow::user(format!("row {index}")));
        host.send(&HostFrame::Event {
            client_id: None,
            attachment: 1,
            seq: 1,
            class: observation.class(),
            observation,
            blobs: Vec::new(),
        });
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !receiver.transcript_resnapshot_required_for(&TranscriptScope::Root) {
        assert!(
            Instant::now() < deadline,
            "the dropped row read as nothing lost"
        );
        thread::sleep(Duration::from_millis(1));
    }
    // The next request is the face's own, on the same attachment.
    sender.send(SessionIntent::Interrupt).unwrap();
    let (_, op) = host.request();
    assert!(matches!(op, Op::Intent { attachment: 1, .. }), "{op:?}");
}

/// An observation the mailbox cannot hold or drop is a real overflow: the
/// client lets its attachment go and attaches again, every transcript the
/// attachment held (the root, and the agent the face opened) read as lost,
/// and the agent's transcript opened again on the new attachment.
#[test]
fn a_real_overflow_attaches_again_and_opens_the_agent_again() {
    let (mut host, sender, receiver) = attached(1);
    sender
        .send(SessionIntent::OpenAgentTranscript {
            agent_id: "a1".into(),
        })
        .unwrap();
    let (id, _) = host.request();
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Accepted { duplicate: false }),
    });
    assert!(sender.settle(Duration::from_secs(10)));
    for _ in 0..=crate::limits::OBSERVATION_QUEUE_CAPACITY {
        let observation = SessionObservation::DraftRejected(SessionIntent::Interrupt);
        host.send(&HostFrame::Event {
            client_id: None,
            attachment: 1,
            seq: 1,
            class: observation.class(),
            observation,
            blobs: Vec::new(),
        });
    }
    let (_, op) = host.request();
    assert_eq!(op, Op::Detach { attachment: 1 });
    let (id, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t1") });
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("a1".into())] {
        assert!(
            receiver.transcript_resnapshot_required_for(&scope),
            "{scope:?}"
        );
    }
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment: 2 }),
    });
    let (_, op) = host.request();
    assert_eq!(
        op,
        Op::Intent {
            client_id: None,
            attachment: 2,
            intent: SessionIntent::OpenAgentTranscript {
                agent_id: "a1".into()
            },
            blobs: Vec::new(),
        }
    );
}

/// A `resync_required` re-attach reads the agent transcript the face had
/// open as lost too and opens it again; a `moved` names another session,
/// whose agent view the face never opened.
#[test]
fn a_resync_reads_every_held_transcript_as_lost() {
    let (mut host, sender, receiver) = attached(1);
    sender
        .send(SessionIntent::OpenAgentTranscript {
            agent_id: "a1".into(),
        })
        .unwrap();
    let (id, _) = host.request();
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Accepted { duplicate: false }),
    });
    assert!(sender.settle(Duration::from_secs(10)));
    host.send(&HostFrame::ResyncRequired { attachment: 1 });
    let (id, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t1") });
    let agent = TranscriptScope::Agent("a1".into());
    assert!(receiver.transcript_resnapshot_required_for(&agent));
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment: 2 }),
    });
    let (_, op) = host.request();
    assert!(
        matches!(
            &op,
            Op::Intent {
                attachment: 2,
                intent: SessionIntent::OpenAgentTranscript { .. },
                ..
            }
        ),
        "{op:?}"
    );
    let mut snapshot = crate::views::PresentationSnapshot::empty(1);
    snapshot.scope = agent.clone();
    for page in crate::wire::page_snapshot(snapshot, 2).unwrap() {
        host.send(&HostFrame::Snapshot(page));
    }
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
        SessionObservation::PresentationSnapshot(_)
    ));
    assert!(!receiver.transcript_resnapshot_required_for(&agent));

    host.send(&HostFrame::Moved {
        attachment: 2,
        node: node("t2"),
        request_id: None,
    });
    let (id, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t2") });
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment: 3 }),
    });
    sender.send(SessionIntent::Interrupt).unwrap();
    let (_, op) = host.request();
    assert!(
        matches!(
            &op,
            Op::Intent {
                attachment: 3,
                intent: SessionIntent::Interrupt,
                ..
            }
        ),
        "{op:?}"
    );
}

/// An intent sent while the client re-attaches waits for the attachment
/// and goes on it, in order.
#[test]
fn an_intent_sent_while_attaching_goes_on_the_next_attachment() {
    let (mut host, sender, _receiver) = attached(1);
    host.send(&HostFrame::ResyncRequired { attachment: 1 });
    let (id, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t1") });
    sender.send(SessionIntent::Interrupt).unwrap();
    sender.send(SessionIntent::Quit).unwrap();
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment: 2 }),
    });
    for expected in [SessionIntent::Interrupt, SessionIntent::Quit] {
        let (_, op) = host.request();
        assert!(
            matches!(&op, Op::Intent { attachment: 2, intent, .. } if *intent == expected),
            "{op:?}"
        );
    }
}

/// A root attach refused `backpressure` is asked once more; a second
/// refusal is a per-request outcome and leaves the connection usable.
#[test]
fn a_backpressured_attach_is_asked_once_more() {
    let (mut host, transport) = Host::pair();
    let client = thread::spawn(move || connect::<Core>(transport, hello()).unwrap());
    assert!(matches!(host.read(), ClientFrame::Hello(_)));
    host.send(&HostFrame::Welcome(welcome()));
    let (_sender, receiver, _) = client.join().unwrap();
    for _ in 0..2 {
        let (id, op) = host.request();
        assert_eq!(op, Op::Attach { node: node("t1") });
        host.send(&HostFrame::Response {
            id,
            outcome: Outcome::Refused(RequestRefusal::Backpressure),
        });
    }
    assert!(matches!(
        receiver.recv_response(Duration::from_secs(5)).unwrap().1,
        Outcome::Refused(RequestRefusal::Backpressure)
    ));
    assert!(
        format!(
            "{:?}",
            receiver.recv_timeout(Duration::from_secs(2)).unwrap()
        )
        .contains("attach refused")
    );
    assert_eq!(receiver.ended(), None);
}

/// An event for an attachment that is no longer the client's drops the
/// blobs it names, however many come.
#[test]
fn a_stale_events_blobs_are_dropped() {
    let (mut host, _sender, receiver) = attached(2);
    for index in 0..2 * crate::wire::MAX_PENDING_BLOBS {
        let id = format!("b{index}");
        host.send(&HostFrame::Blob(crate::wire::BlobChunk {
            id: id.clone(),
            offset: 0,
            data: String::new(),
            last: true,
        }));
        let observation = SessionObservation::SetBusy(true);
        host.send(&HostFrame::Event {
            client_id: None,
            attachment: 1,
            seq: 1,
            class: observation.class(),
            observation,
            blobs: vec![id],
        });
    }
    host.send(&event(2, false));
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
        SessionObservation::SetBusy(false)
    );
    assert_eq!(receiver.ended(), None);
}

/// An intent the host read but never answered before the connection ended
/// may have run: its outcome is unknown, not a refusal.
#[test]
fn an_unanswered_send_has_an_unknown_outcome() {
    let (mut host, sender, receiver) = attached(1);
    sender.send(SessionIntent::Interrupt).unwrap();
    let (_, op) = host.request();
    assert!(matches!(op, Op::Intent { .. }));
    host.stream.shutdown(std::net::Shutdown::Both).unwrap();
    assert!(matches!(
        unsent(&receiver),
        SendError::OutcomeUnknown {
            intent: SessionIntent::Interrupt,
            ..
        }
    ));
    assert!(matches!(
        sender.send(SessionIntent::Interrupt),
        Err(SendError::Refused(IntentSendError::Disconnected(_)))
    ));
}

/// A refusal for the attachment's role names its row.
#[test]
fn a_role_refusal_names_its_row() {
    let (mut host, sender, receiver) = attached(1);
    sender.send(SessionIntent::Interrupt).unwrap();
    let (id, _) = host.request();
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Refused(RequestRefusal::RefusedForRole {
            row: "control".into(),
        }),
    });
    assert!(matches!(
        unsent(&receiver),
        SendError::RefusedForRole { row, intent: SessionIntent::Interrupt } if row == "control"
    ));
}
