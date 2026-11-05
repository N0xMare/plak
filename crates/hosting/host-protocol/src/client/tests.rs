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
        Err(IntentSendError::Saturated(SessionIntent::Interrupt))
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
