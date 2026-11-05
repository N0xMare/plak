use std::io::{BufRead as _, BufReader};
use std::os::unix::net::UnixStream;

use super::*;
use crate::Core;
use crate::wire::{ClientLocality, WIRE_VERSION, decode_line};

fn hello() -> Hello {
    Hello {
        proto: WIRE_VERSION,
        binary_id: "client".into(),
        caps: Vec::new(),
        locality: ClientLocality::default(),
    }
}

fn node(tree: &str) -> NodeAddress {
    NodeAddress {
        tree_id: tree.into(),
        agent_id: tree.into(),
    }
}

fn welcome() -> Welcome {
    Welcome {
        proto: WIRE_VERSION,
        binary_id: "host".into(),
        host_generation: 1,
        caps: Vec::new(),
        session_id: "s1".into(),
        tree_id: "t1".into(),
        root: node("t1"),
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
        let transport = Transport::new(client.try_clone().unwrap(), client);
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
    assert_eq!(welcomed.session_id, "s1");
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
    assert!(receiver.transcript_resnapshot_required_for(&TranscriptScope::Root));
    for page in crate::wire::page_snapshot(crate::views::PresentationSnapshot::empty(3), 2).unwrap()
    {
        host.send(&HostFrame::Snapshot(page));
    }
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
        SessionObservation::PresentationSnapshot(_)
    ));
    assert!(!receiver.transcript_resnapshot_required_for(&TranscriptScope::Root));

    let sending = thread::spawn({
        let sender = sender.clone();
        move || sender.send(SessionIntent::Interrupt)
    });
    let (id, op) = host.request();
    assert!(matches!(op, Op::Intent { attachment: 2, .. }));
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Refused(RequestRefusal::Backpressure),
    });
    assert!(matches!(
        sending.join().unwrap(),
        Err(SendError::Refused(IntentSendError::Saturated(
            SessionIntent::Interrupt
        )))
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
    let sending = thread::spawn({
        let large = large.clone();
        move || sender.send(large)
    });
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
        outcome: Outcome::Ok(Answer::Accepted),
    });
    assert!(sending.join().unwrap().is_ok());
    let (_, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t2") });
}

/// A face that falls [`QUEUE_BOUND`] observations behind loses them: the
/// client lets its attachment go and attaches again, the root transcript
/// read as lost.
#[test]
fn a_face_that_falls_behind_is_attached_again() {
    let (mut host, _sender, receiver) = attached(1);
    for _ in 0..=QUEUE_BOUND {
        host.send(&event(1, true));
    }
    let (_, op) = host.request();
    assert_eq!(op, Op::Detach { attachment: 1 });
    let (id, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t1") });
    assert!(receiver.transcript_resnapshot_required_for(&TranscriptScope::Root));
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment: 2 }),
    });
    host.send(&event(2, false));
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
        SessionObservation::SetBusy(false)
    );
}

/// A root attach refused `backpressure` is asked once more; a second
/// refusal ends the connection, naming it.
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
        receiver.recv_timeout(Duration::from_secs(5)),
        Err(RecvTimeoutError::Disconnected)
    ));
    let ended = receiver.ended().unwrap();
    assert!(ended.contains("Backpressure"), "{ended}");
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
    let (mut host, sender, _receiver) = attached(1);
    let sending = thread::spawn(move || sender.send(SessionIntent::Interrupt));
    let (_, op) = host.request();
    assert!(matches!(op, Op::Intent { .. }));
    host.stream.shutdown(std::net::Shutdown::Both).unwrap();
    assert!(matches!(
        sending.join().unwrap(),
        Err(SendError::OutcomeUnknown(SessionIntent::Interrupt))
    ));
}
