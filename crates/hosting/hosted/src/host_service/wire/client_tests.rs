//! The wire v2 client ([`plakous_host_protocol::client`]) against the real
//! shared session service, over a socket pair.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use plakous_host_protocol::client::{self, SessionAnswer, SessionChange, Transport};
use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptScope};
use plakous_host_protocol::wire::{Hello, LineReader};
use plakous_host_protocol::{Core, IntentReceiver, ObservationSender};

use super::*;

const WAIT: Duration = Duration::from_secs(10);

fn root_of(session: &str) -> NodeAddress {
    NodeAddress {
        tree_id: format!("tree-{session}"),
        agent_id: format!("tree-{session}"),
    }
}

fn binding(session: &str) -> SessionObservation<Core> {
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

fn hello() -> Hello {
    Hello {
        proto: WIRE_VERSION,
        binary_id: "client".into(),
        caps: Vec::new(),
    }
}

/// A session served to wire clients, as an acceptor serves it.
struct Served {
    session: WireSession<Core>,
    published: ObservationSender<Core>,
    intents: IntentReceiver<Core>,
    /// While set, no connection writes what its attachment queued.
    paused: Arc<AtomicBool>,
    /// Why the last connection's server side ended.
    exited: Arc<std::sync::Mutex<Option<String>>>,
}

impl Served {
    fn start() -> Self {
        let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
        let (published, events) = plakous_host_protocol::observation_channel::<Core>();
        published
            .send(SessionObservation::PresentationSnapshot(
                PresentationSnapshot::empty(1),
            ))
            .unwrap();
        let session = WireSession::start(root_of("s1"), "s1".to_owned(), actions, events, |id| {
            Some(root_of(id))
        });
        Self {
            session,
            published,
            intents,
            paused: Arc::default(),
            exited: Arc::default(),
        }
    }

    /// A transport to a new connection of the session, served on a thread
    /// until the client leaves; setting the returned flag cuts it.
    fn transport(&self, face: Face) -> (Transport, Arc<AtomicBool>) {
        let (host, client) = UnixStream::pair().unwrap();
        let cut = Arc::new(AtomicBool::new(false));
        let mut connection = self
            .session
            .connect(host.try_clone().unwrap(), face, || false);
        let (session, root) = connection.welcomed().unwrap();
        let welcome = session_welcome("host".into(), 1, session, root);
        let lines = lines_of(host.try_clone().unwrap());
        let paused = Arc::clone(&self.paused);
        let stop = Arc::clone(&cut);
        let exited = Arc::clone(&self.exited);
        std::thread::spawn(move || {
            let mut out = host.try_clone().unwrap();
            let reason = drive(&mut connection, &lines, welcome, &mut out, &stop, &paused);
            *exited.lock().unwrap() = Some(format!("{reason:?}"));
            // Whatever ended the connection ends its transport.
            let _ = host.shutdown(std::net::Shutdown::Both);
        });
        (Transport::new(client.try_clone().unwrap(), client), cut)
    }

    /// Publishes `observations`, then waits for the session to be `session`.
    fn switch(&self, observations: Vec<SessionObservation<Core>>, session: &str) {
        for observation in observations {
            self.published.send(observation).unwrap();
        }
        let deadline = Instant::now() + WAIT;
        while self.session.session_id() != session {
            assert!(Instant::now() < deadline, "the switch to {session}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Answers the next intent the session takes with its start.
    fn started(&self) -> (SessionIntent<Core>, SessionObservation<Core>) {
        let delivery = self.intents.recv_timeout(WAIT).expect("an intent");
        let started = match delivery.started() {
            Some(plakous_host_protocol::ContentObservation::IntentStarted(started)) => {
                SessionObservation::IntentStarted(started)
            }
            other => panic!("no start: {other:?}"),
        };
        ((*delivery).clone(), started)
    }
}

/// Serves `connection` as an acceptor does: the handshake, then its client's
/// lines and its flushes, until it ends; why it ended.
fn drive(
    connection: &mut WireConnection<Core, UnixStream>,
    lines: &Lines,
    welcome: Welcome,
    out: &mut UnixStream,
    stop: &AtomicBool,
    paused: &AtomicBool,
) -> Result<&'static str, String> {
    if !handshake::<Core>(lines, welcome, out, &|| false).map_err(|error| error.to_string())? {
        return Ok("refused");
    }
    loop {
        if stop.load(Ordering::Acquire) {
            connection.leave();
            return Ok("cut");
        }
        match lines.recv_timeout(Duration::from_millis(2)) {
            Ok(line) => {
                if !connection.frame(line).map_err(|error| error.to_string())? {
                    let _ = connection.flush();
                    return Ok("the client left");
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                connection.leave();
                return Ok("the client's transport ended");
            }
        }
        if !paused.load(Ordering::Acquire) {
            connection.flush().map_err(|error| error.to_string())?;
        }
    }
}

fn lines_of(stream: UnixStream) -> Lines {
    use std::io::Read as _;
    let (sender, lines) = lines(64);
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut reader = LineReader::default();
        let mut buffer = vec![0_u8; 64 * 1024];
        while let Ok(read) = stream.read(&mut buffer) {
            if read == 0 {
                return;
            }
            for line in reader.push(&buffer[..read]) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        }
    });
    lines
}

/// Waits for an observation `wanted` accepts, skipping the rest.
fn wait_for(
    receiver: &client::WireReceiver<Core>,
    wanted: impl Fn(&SessionObservation<Core>) -> bool,
) -> SessionObservation<Core> {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("the observation in time");
        let observation = receiver
            .recv_timeout(left)
            .unwrap_or_else(|error| panic!("no observation ({error:?}): {:?}", receiver.ended()));
        if wanted(&observation) {
            return observation;
        }
    }
}

fn busy(on: bool) -> impl Fn(&SessionObservation<Core>) -> bool {
    move |observation| *observation == SessionObservation::SetBusy(on)
}

/// A client attaches through the welcome, takes the session's
/// observations, sends an intent the session takes, and leaves with a
/// bye; a new connection of the same session starts from its attach
/// snapshot, and a transport that ends is the receiver's disconnect.
#[test]
fn a_client_connects_sends_and_reconnects() {
    let served = Served::start();
    let (transport, _) = served.transport(Face::Local);
    let (sender, receiver, welcome) = client::connect::<Core>(transport, hello()).unwrap();
    assert_eq!(welcome.root, root_of("s1"));
    wait_for(&receiver, |observation| {
        matches!(observation, SessionObservation::PresentationSnapshot(_))
    });
    assert!(!receiver.transcript_resnapshot_required_for(&TranscriptScope::Root));
    served
        .published
        .send(SessionObservation::SetBusy(true))
        .unwrap();
    wait_for(&receiver, busy(true));
    let prompt = SessionIntent::Prompt(plakous_host_protocol::Draft::from("go"));
    sender.send(prompt.clone()).unwrap();
    let (taken, _) = served.started();
    assert_eq!(taken, prompt);
    drop((sender, receiver));

    let (transport, cut) = served.transport(Face::Local);
    let (_sender, receiver, _) = client::connect::<Core>(transport, hello()).unwrap();
    // The busy state is part of the attach snapshot.
    wait_for(&receiver, busy(true));
    cut.store(true, Ordering::Release);
    let deadline = Instant::now() + WAIT;
    loop {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            _ => assert!(Instant::now() < deadline, "the disconnect"),
        }
    }
}

/// Two switches before the client attaches again: its `moved` names the
/// first new root, whose attach is refused `moved` with the second, which
/// the client attaches; it takes the second session's attach snapshot and
/// then its observations.
#[test]
fn a_client_follows_two_switches() {
    let served = Served::start();
    let (transport, _) = served.transport(Face::Local);
    let (_sender, receiver, _) = client::connect::<Core>(transport, hello()).unwrap();
    wait_for(&receiver, |observation| {
        matches!(observation, SessionObservation::PresentationSnapshot(_))
    });
    served.paused.store(true, Ordering::Release);
    served.switch(vec![binding("s2")], "s2");
    served.switch(vec![binding("s3")], "s3");
    served.paused.store(false, Ordering::Release);
    // The third session's attach snapshot, then its own observation.
    wait_for(&receiver, |observation| {
        matches!(observation, SessionObservation::PresentationSnapshot(_))
    });
    served
        .published
        .send(SessionObservation::SetBusy(true))
        .unwrap();
    wait_for(&receiver, busy(true));
    assert_eq!(receiver.ended(), None);
}

/// A client that fell behind on what it may not lose is ended with
/// `resync_required`; it attaches again on its own and takes the attach
/// snapshot, its root transcript read as lost until that snapshot.
#[test]
fn a_client_resyncs_after_an_overflow() {
    let served = Served::start();
    let (transport, _) = served.transport(Face::Local);
    let (_sender, receiver, _) = client::connect::<Core>(transport, hello()).unwrap();
    wait_for(&receiver, |observation| {
        matches!(observation, SessionObservation::PresentationSnapshot(_))
    });
    served.paused.store(true, Ordering::Release);
    for index in 0..=ATTACHMENT_CAPACITY {
        let row = TranscriptRow::error(NoticeScope::Host, format!("error {index}"));
        served.published.send(SessionObservation::Row(row)).unwrap();
        // The session's own mailbox is smaller than the attachment's queue.
        if index % 256 == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    served
        .published
        .send(SessionObservation::SetBusy(true))
        .unwrap();
    std::thread::sleep(Duration::from_millis(50));
    served.paused.store(false, Ordering::Release);
    // Its queue was replaced by the resync: it attaches again and takes
    // the new attachment's snapshot.
    let deadline = Instant::now() + WAIT;
    loop {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(SessionObservation::PresentationSnapshot(_)) => break,
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("{error:?}: {:?}", receiver.ended()),
        }
        assert!(
            Instant::now() < deadline,
            "no snapshot; client {:?}, server {:?}",
            receiver.ended(),
            served.exited.lock().unwrap()
        );
    }
    assert!(!receiver.transcript_resnapshot_required_for(&TranscriptScope::Root));
    // The new attachment takes what the session says next.
    served
        .published
        .send(SessionObservation::SetBusy(false))
        .unwrap();
    wait_for(&receiver, busy(false));
    assert_eq!(receiver.ended(), None);
}

/// A resume asked through the client goes to the session as its own
/// resume; the switch answers it with the new root, and the client follows
/// the switch. A retry is refused by a host that serves one session.
#[test]
fn a_session_request_is_answered_and_followed() {
    let served = Served::start();
    let (transport, _) = served.transport(Face::Remote);
    let (sender, receiver, _) = client::connect::<Core>(transport, hello()).unwrap();
    wait_for(&receiver, |observation| {
        matches!(observation, SessionObservation::PresentationSnapshot(_))
    });
    while served.intents.try_recv().is_ok() {}
    let asked = sender
        .request_session(SessionChange::Resume("s2".into()))
        .unwrap();
    let (resume, started) = served.started();
    assert_eq!(
        resume,
        SessionIntent::ResumeSession {
            id: Some("s2".into())
        }
    );
    served.switch(
        vec![started, SessionObservation::SessionOpening, binding("s2")],
        "s2",
    );
    let answer = wait_answer(&receiver);
    assert_eq!(
        answer,
        SessionAnswer {
            request_id: asked,
            outcome: Ok(root_of("s2")),
        }
    );
    served
        .published
        .send(SessionObservation::SetBusy(true))
        .unwrap();
    wait_for(&receiver, busy(true));
    let retry = sender.request_session(SessionChange::RetryAttach).unwrap();
    let answer = wait_answer(&receiver);
    assert_eq!(answer.request_id, retry);
    assert!(matches!(
        answer.outcome,
        Err(RequestRefusal::SessionUnavailable { .. })
    ));
}

fn wait_answer(receiver: &client::WireReceiver<Core>) -> SessionAnswer {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(answer) = receiver.try_session_answer() {
            return answer;
        }
        assert!(Instant::now() < deadline, "the session answer");
        let _ = receiver.recv_timeout(Duration::from_millis(5));
    }
}
