use std::sync::atomic::AtomicUsize;

use plakous_host_protocol::wire::{LiveWork, RequestId, encode_line};
use plakous_host_protocol::{Core, SessionIntent, SessionObservation};

use super::*;
use crate::runtime::HostWorkerHandle;

/// The frames written so far, shared between the connection and the test.
#[derive(Clone, Default)]
struct Out(Arc<Mutex<Vec<u8>>>);

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Out {
    /// The frames written since the last take.
    fn take(&self) -> Vec<HostFrame<Core>> {
        let bytes = std::mem::take(&mut *self.0.lock().unwrap());
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| decode_line(line).unwrap())
            .collect()
    }
}

/// What the fake session does with a prompt: `approve` asks for an
/// approval, `process` runs a background process, `flood` sends more
/// required events than an attachment holds, and anything else is busy.
fn prompted(
    script: &Script,
    busy: &plakous_host_protocol::ObservationSender<Core>,
    draft: &plakous_host_protocol::intent::Draft,
) {
    script
        .images
        .lock()
        .unwrap()
        .push(draft.images.iter().map(|image| image.bytes.len()).sum());
    match draft.text.as_str() {
        "approve" => {
            let _ = busy.send(SessionObservation::ApprovalRequired {
                approval_id: "a1".into(),
                call_id: None,
                tool_name: "shell".into(),
                subject: None,
                prompt: None,
                expires_at_ms: 1,
            });
        }
        "process" => {
            let _ = busy.send(SessionObservation::Processes {
                views: Vec::new(),
                running: 1,
            });
        }
        // More required events than an attachment holds.
        "flood" => {
            for index in 0..5_000 {
                let _ = busy.send(SessionObservation::ApprovalResolved {
                    approval_id: format!("f{index}"),
                    resolution: "denied".into(),
                });
                if index % 200 == 0 {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            script.flooded.store(true, Ordering::SeqCst);
        }
        _ => {
            let _ = busy.send(SessionObservation::SetBusy(true));
        }
    }
}

fn node(session: &str) -> NodeAddress {
    NodeAddress {
        tree_id: session.into(),
        agent_id: session.into(),
    }
}

fn row(session: &str, updated_at: u64) -> SessionRow {
    SessionRow {
        session: session.into(),
        node: node(session),
        title: None,
        workspace: "/w".into(),
        profile: None,
        updated_at,
        residency: SessionResidency::Dormant,
        activity: SessionActivity::Idle,
        live_work: LiveWork::default(),
        environment: SessionEnvironment::Host,
        hosted_elsewhere: None,
    }
}

/// The stores as a test scripts them, and what the host asked of them.
#[derive(Default)]
struct Script {
    rows: Mutex<Vec<SessionRow>>,
    opens: AtomicUsize,
    /// Sessions whose open fails, with the failure.
    refuse: Mutex<BTreeMap<String, RequestRefusal>>,
    /// The fake sessions' observation senders, to end one.
    ends: Mutex<BTreeMap<String, plakous_host_protocol::ObservationSender<Core>>>,
    creates: Mutex<Vec<String>>,
    /// The tree each session's root is in, when it is not its own.
    trees: Mutex<BTreeMap<String, String>>,
    /// The sessions keep themselves resident: nothing releases.
    stay: std::sync::atomic::AtomicBool,
    /// An open waits for this to be sent (or dropped) before it opens.
    gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// How long a releasing session's worker lingers before it ends.
    linger: AtomicU64,
    /// The image byte counts of every prompt a session took.
    images: Mutex<Vec<usize>>,
    /// A flood of required events was sent whole.
    flooded: std::sync::atomic::AtomicBool,
    /// The sessions take stops and settle none until this is cleared.
    hold_stops: std::sync::atomic::AtomicBool,
}

#[derive(Clone, Default)]
struct Fake(Arc<Script>);

impl HostSessions<Core> for Fake {
    fn rows(&self, workspace: Option<&str>) -> Result<Vec<SessionRow>, String> {
        Ok(self
            .0
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|row| workspace.is_none_or(|workspace| row.workspace == workspace))
            .cloned()
            .collect())
    }

    fn session_of(&self, node: &NodeAddress) -> Result<Option<String>, String> {
        Ok(self
            .0
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|row| row.node.tree_id == node.tree_id)
            .map(|row| row.session.clone()))
    }

    fn open(&self, session: &str) -> CliResult<OpenedSession<Core>> {
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        let gate = self.0.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.recv();
        }
        if self.0.refuse.lock().unwrap().contains_key(session) {
            return Err(CliFailure::new("refused by the script"));
        }
        let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
        let (observations, events) = plakous_host_protocol::observation_channel::<Core>();
        self.0
            .ends
            .lock()
            .unwrap()
            .insert(session.to_owned(), observations.clone());
        let busy = observations.clone();
        let attachments = crate::host_service::AttachmentCount::default();
        let count = attachments.clone();
        let script = Arc::clone(&self.0);
        let worker = HostWorkerHandle::spawn(move || {
            drop(observations);
            let mut stops = Vec::new();
            loop {
                if !script.hold_stops.load(Ordering::SeqCst) {
                    for ticket in stops.drain(..) {
                        intents.settle_stop(ticket, Ok(()));
                    }
                }
                match intents.recv_timeout(Duration::from_millis(10)) {
                    Ok(delivery) => match &*delivery {
                        SessionIntent::Quit => break,
                        SessionIntent::StopSession { ticket } => stops.push(*ticket),
                        SessionIntent::Prompt(draft) => prompted(&script, &busy, draft),
                        _ => {}
                    },
                    // The actor's idle release, asked to release now.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if !script.stay.load(Ordering::SeqCst)
                            && count.release_now()
                            && count.begin_release(count.seen())
                        {
                            let linger = script.linger.load(Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(linger));
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            Ok(())
        });
        let tree = self
            .0
            .trees
            .lock()
            .unwrap()
            .get(session)
            .cloned()
            .unwrap_or_else(|| session.to_owned());
        Ok(OpenedSession {
            root: node(&tree),
            session_id: session.to_owned(),
            actions,
            events,
            worker,
            attachments,
            roots: Box::new(|id| Some(node(id))),
        })
    }

    fn refusal(&self, session: &str, failure: &CliFailure) -> RequestRefusal {
        self.0
            .refuse
            .lock()
            .unwrap()
            .get(session)
            .cloned()
            .unwrap_or_else(|| RequestRefusal::SessionUnavailable {
                detail: failure.message().to_owned(),
            })
    }

    fn create(
        &self,
        workspace: &str,
        _template: Option<&str>,
        request_id: &str,
    ) -> Result<Created, RequestRefusal> {
        let session = format!("new-{request_id}");
        let mut rows = self.0.rows.lock().unwrap();
        if !rows.iter().any(|row| row.session == session) {
            self.0.creates.lock().unwrap().push(session.clone());
            rows.insert(
                0,
                SessionRow {
                    workspace: workspace.to_owned(),
                    ..row(&session, 100)
                },
            );
        }
        Ok(Created {
            node: node(&session),
            session,
            environment: SessionEnvironment::Host,
        })
    }
}

/// Hands every resident session off, each in time.
fn handed_off(host: &SessionHost<Core>) {
    let late: Vec<_> = host
        .hand_off_all(Duration::from_secs(10))
        .into_iter()
        .map(|(session, failure)| format!("{session}: {}", failure.message()))
        .collect();
    assert_eq!(late, Vec::<String>::new());
}

fn host(rows: &[&str]) -> (Arc<SessionHost<Core>>, Fake) {
    let fake = Fake::default();
    *fake.0.rows.lock().unwrap() = rows
        .iter()
        .enumerate()
        .map(|(at, session)| row(session, u64::try_from(at).unwrap()))
        .collect();
    (SessionHost::new(fake.clone(), "b1", 7), fake)
}

fn request(connection: &mut HostConnection<Core, Out>, id: RequestId, op: Op<Core>) -> bool {
    let line = encode_line(&ClientFrame::<Core>::Request { id, op }).unwrap();
    connection.frame(Ok(line)).unwrap()
}

fn outcome(frames: &[HostFrame<Core>], id: RequestId) -> Outcome {
    frames
        .iter()
        .find_map(|frame| match frame {
            HostFrame::Response {
                id: answered,
                outcome,
            } if *answered == id => Some(outcome.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no answer to {id}: {frames:?}"))
}

fn attached(frames: &[HostFrame<Core>], id: RequestId) -> AttachmentId {
    match outcome(frames, id) {
        Outcome::Ok(Answer::Attached { attachment }) => attachment,
        other => panic!("not attached: {other:?}"),
    }
}

/// The welcome names no session, and a node request before any attach is
/// refused not attached: a host connection serves no session by default.
#[test]
fn the_welcome_names_no_session_and_nothing_is_attached_by_default() {
    let (host, fake) = host(&["s1"]);
    assert_eq!(host.welcome().session, None);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(
        &mut connection,
        1,
        Op::Intent {
            attachment: 1,
            intent: SessionIntent::Interrupt,
            blobs: Vec::new(),
        },
    );
    request(&mut connection, 2, Op::Detach { attachment: 1 });
    let frames = out.take();
    for id in [1, 2] {
        assert_eq!(
            outcome(&frames, id),
            Outcome::Refused(RequestRefusal::NotAttached)
        );
    }
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 0);
}

/// An attach of a dormant session's root opens it once and attaches it; a
/// second attach to its tree on the same connection opens nothing more,
/// and neither does another connection's attach while it is resident. A
/// node no session's tree holds is unknown, and opens nothing.
#[test]
fn an_attach_opens_a_dormant_session_once_and_routes_by_tree() {
    let (host, fake) = host(&["s1", "s2"]);
    let out = Out::default();
    let mut first = host.connect(out.clone(), Face::Remote, || false);
    request(&mut first, 1, Op::Attach { node: node("s1") });
    request(&mut first, 2, Op::Attach { node: node("s1") });
    let frames = out.take();
    attached(&frames, 1);
    attached(&frames, 2);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    let other = Out::default();
    let mut second = host.connect(other.clone(), Face::Remote, || false);
    request(&mut second, 1, Op::Attach { node: node("s1") });
    attached(&other.take(), 1);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    request(
        &mut second,
        2,
        Op::Attach {
            node: node("nowhere"),
        },
    );
    assert_eq!(
        outcome(&other.take(), 2),
        Outcome::Refused(RequestRefusal::UnknownNode)
    );
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    // An attach of another session moves the node attachment there.
    request(&mut first, 3, Op::Attach { node: node("s2") });
    attached(&out.take(), 3);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 2);
    first.leave();
    second.leave();
    handed_off(&host);
}

/// An attach of a tree a resident session serves goes to that session,
/// even when the stores name another, older session of the tree first (a
/// session a rebind or a switch left behind): nothing else opens.
#[test]
fn an_attach_goes_to_the_resident_session_of_its_tree() {
    let (host, fake) = host(&["old", "new"]);
    for session in ["old", "new"] {
        fake.0
            .trees
            .lock()
            .unwrap()
            .insert(session.into(), "tree".into());
    }
    for row in fake.0.rows.lock().unwrap().iter_mut() {
        row.node = node("tree");
    }
    // The newer session is resident, opened by its id.
    let resident = host.slot("new");
    let (_session, hold) = resident.resident().unwrap();
    drop(hold);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("tree") });
    attached(&out.take(), 1);
    assert_eq!(
        fake.0.opens.load(Ordering::SeqCst),
        1,
        "the stores' older session of the tree was opened beside the resident one"
    );
    assert_eq!(
        connection.node.as_ref().unwrap().session.session_id(),
        "new"
    );
    connection.leave();
    handed_off(&host);
}

/// A session that does not open refuses the attach with the stores' own
/// refusal (a live holder elsewhere is `tree_hosted_elsewhere`), and the
/// connection holds nothing after it.
#[test]
fn an_attach_whose_session_does_not_open_is_refused_as_the_stores_say() {
    let (host, fake) = host(&["s1"]);
    let held = RequestRefusal::TreeHostedElsewhere {
        host_name: "desk".into(),
        binary: Some("0123456789ab".into()),
        pid: 42,
    };
    fake.0
        .refuse
        .lock()
        .unwrap()
        .insert("s1".into(), held.clone());
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    assert_eq!(outcome(&out.take(), 1), Outcome::Refused(held));
    assert!(connection.node.is_none());
}

/// While a connection holds a node attachment the session is held
/// resident; once it detaches, the connection lets the session go, so it
/// may release when idle.
#[test]
fn a_detach_lets_the_session_go() {
    let (host, _) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    assert!(connection.node.is_some());
    request(&mut connection, 2, Op::Detach { attachment });
    assert_eq!(outcome(&out.take(), 2), Outcome::Ok(Answer::Detached));
    assert!(
        connection.node.is_none(),
        "a connection that holds no attachment kept its session"
    );
    handed_off(&host);
}

/// One host attachment per connection: a second watch is one in flight, an
/// unwatch names it or is not attached, and after it the connection may
/// watch again.
#[test]
fn a_connection_holds_at_most_one_host_attachment() {
    let (host, _) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::WatchSessions);
    request(&mut connection, 2, Op::WatchSessions);
    let frames = out.take();
    let Outcome::Ok(Answer::Watching { attachment }) = outcome(&frames, 1) else {
        panic!("watching: {frames:?}");
    };
    assert_eq!(
        outcome(&frames, 2),
        Outcome::Refused(RequestRefusal::OneInFlight)
    );
    request(
        &mut connection,
        3,
        Op::UnwatchSessions {
            attachment: attachment + 1,
        },
    );
    request(&mut connection, 4, Op::UnwatchSessions { attachment });
    request(&mut connection, 5, Op::WatchSessions);
    let frames = out.take();
    assert_eq!(
        outcome(&frames, 3),
        Outcome::Refused(RequestRefusal::NotAttached)
    );
    assert_eq!(outcome(&frames, 4), Outcome::Ok(Answer::Detached));
    assert!(matches!(
        outcome(&frames, 5),
        Outcome::Ok(Answer::Watching { attachment: next }) if next != attachment
    ));
}

fn directory(frames: &[HostFrame<Core>]) -> Vec<(u64, bool, Vec<SessionRow>)> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            HostFrame::Directory {
                seq,
                snapshot,
                rows,
                ..
            } => Some((*seq, *snapshot, rows.clone())),
            _ => None,
        })
        .collect()
}

/// The host attachment takes a snapshot of every row first, then only the
/// rows that changed (a session the host opened reads resident, and busy
/// once its turn runs), numbered on from 1, and nothing when nothing
/// changed; a row gone from the stores is said by a fresh snapshot.
#[test]
fn the_directory_sends_a_snapshot_then_the_rows_that_changed() {
    let (host, fake) = host(&["s1", "s2"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::WatchSessions);
    connection.flush().unwrap();
    let frames = directory(&out.take());
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, 1);
    assert!(frames[0].1, "the first directory frame is a snapshot");
    assert_eq!(frames[0].2, vec![row("s1", 0), row("s2", 1)]);
    // Read again only when the host changed or the poll is due.
    connection.flush().unwrap();
    assert_eq!(directory(&out.take()), Vec::new());
    let other = Out::default();
    let mut worker = host.connect(other.clone(), Face::Remote, || false);
    request(&mut worker, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&other.take(), 1);
    // The attach changed the host: the next flush reads again.
    assert!(connection.host_changes.has_changed().unwrap());
    connection.flush().unwrap();
    let frames = directory(&out.take());
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].0, 2);
    assert!(!frames[0].1);
    assert_eq!(
        frames[0].2,
        vec![SessionRow {
            residency: SessionResidency::Resident,
            ..row("s1", 0)
        }]
    );
    request(
        &mut worker,
        2,
        Op::Intent {
            attachment,
            intent: SessionIntent::Prompt("go".into()),
            blobs: Vec::new(),
        },
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let busy = loop {
        host.changed();
        connection.flush().unwrap();
        let frames = directory(&out.take());
        if let Some(frame) = frames.into_iter().next() {
            break frame;
        }
        assert!(Instant::now() < deadline, "the turn never showed");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(busy.0, 3);
    assert_eq!(busy.2[0].activity, SessionActivity::Busy);
    fake.0.rows.lock().unwrap().remove(1);
    host.changed();
    connection.flush().unwrap();
    let frames = directory(&out.take());
    assert_eq!(frames.len(), 1);
    assert!(frames[0].1, "a removed row is said by a snapshot");
    assert_eq!(frames[0].2.len(), 1);
    worker.leave();
    handed_off(&host);
}

/// A node attachment whose session ended on its own ends with the host
/// frame `detached`, and the connection lets the session go; the next
/// attach opens it again.
#[test]
fn an_attachment_whose_session_ended_is_detached() {
    let (host, fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    // The session ends on its own: its worker stops and its observations
    // end.
    drop(fake.0.ends.lock().unwrap().remove("s1").unwrap());
    let session = Arc::clone(&connection.node.as_ref().unwrap().session);
    session.quit();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !session.ended() {
        assert!(Instant::now() < deadline, "the session never ended");
        std::thread::sleep(Duration::from_millis(5));
    }
    connection.flush().unwrap();
    assert!(
        out.take().contains(&HostFrame::Detached { attachment }),
        "no detached frame"
    );
    assert!(connection.node.is_none());
    request(&mut connection, 2, Op::Attach { node: node("s1") });
    attached(&out.take(), 2);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 2);
    connection.leave();
    handed_off(&host);
}

/// `new_session` answers the session the stores created, and a replay of
/// the same request id answers the same session without a second create;
/// the directory reads again after it.
#[test]
fn new_session_answers_created_and_a_replay_answers_the_same() {
    let (host, fake) = host(&[]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    let new = |id| Op::NewSession {
        workspace: "/w".into(),
        template: None,
        request_id: id,
    };
    let mut changes = host.changes();
    request(&mut connection, 1, new("r1".into()));
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    request(&mut connection, 2, new("r1".into()));
    let frames = out.take();
    let created = Outcome::Ok(Answer::Created {
        session: "new-r1".into(),
        node: node("new-r1"),
        environment: SessionEnvironment::Host,
    });
    assert_eq!(outcome(&frames, 1), created);
    assert_eq!(outcome(&frames, 2), created);
    assert_eq!(*fake.0.creates.lock().unwrap(), ["new-r1"]);
}

/// `release_session` detaches every attachment of a resident session, the
/// asker's own and another connection's (each ended with `detached`), and
/// answers `released` once it began to release; the next attach opens it
/// again. A dormant session is released already, and an unknown one is
/// refused.
#[test]
fn release_session_detaches_every_attachment_and_releases() {
    let (host, fake) = host(&["s1", "s2"]);
    let (one, two) = (Out::default(), Out::default());
    let mut asker = host.connect(one.clone(), Face::Remote, || false);
    let mut other = host.connect(two.clone(), Face::Remote, || false);
    request(&mut asker, 1, Op::Attach { node: node("s1") });
    let mine = attached(&one.take(), 1);
    request(&mut other, 1, Op::Attach { node: node("s1") });
    let theirs = attached(&two.take(), 1);
    let releasing = std::thread::spawn(move || {
        request(
            &mut asker,
            2,
            Op::ReleaseSession {
                session: "s1".into(),
            },
        );
        asker
    });
    // The other connection detaches when the host's change reaches it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while other.node.is_some() {
        assert!(
            Instant::now() < deadline,
            "the other connection kept its attachment"
        );
        other.flush().unwrap();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        two.take()
            .contains(&HostFrame::Detached { attachment: theirs })
    );
    let asker = releasing.join().unwrap();
    let frames = one.take();
    assert!(
        frames.contains(&HostFrame::Detached { attachment: mine }),
        "{frames:?}"
    );
    assert_eq!(outcome(&frames, 2), Outcome::Ok(Answer::Released));
    assert!(asker.node.is_none());
    let mut asker = asker;
    request(&mut asker, 3, Op::Attach { node: node("s1") });
    attached(&one.take(), 3);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 2);
    for (session, answer) in [
        ("s2", Outcome::Ok(Answer::Released)),
        ("nowhere", Outcome::Refused(RequestRefusal::UnknownSession)),
    ] {
        request(
            &mut asker,
            4,
            Op::ReleaseSession {
                session: session.into(),
            },
        );
        assert_eq!(outcome(&one.take(), 4), answer);
    }
    asker.leave();
    other.leave();
    handed_off(&host);
}

/// A session that something else keeps resident is answered `resident`
/// with its reason once the wait is over, and the request is withdrawn: it
/// does not release later by itself.
#[test]
fn release_session_answers_resident_and_withdraws_when_nothing_releases() {
    let fake = Fake::default();
    *fake.0.rows.lock().unwrap() = vec![row("s1", 0)];
    fake.0.stay.store(true, Ordering::SeqCst);
    let host = SessionHost::with_release_wait(fake.clone(), Duration::from_millis(200));
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    request(
        &mut connection,
        2,
        Op::ReleaseSession {
            session: "s1".into(),
        },
    );
    assert_eq!(
        outcome(&out.take(), 2),
        Outcome::Ok(Answer::Resident {
            reason: ResidentReason::Work
        })
    );
    // The session would release now, but the request was withdrawn.
    fake.0.stay.store(false, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(200));
    assert!(matches!(host.slot("s1").peek(), SlotView::Resident(_)));
    handed_off(&host);
}

/// A host that releases nothing within the wait, for the session `s1`.
fn staying_host() -> (Arc<SessionHost<Core>>, Fake) {
    let fake = Fake::default();
    *fake.0.rows.lock().unwrap() = vec![row("s1", 0)];
    fake.0.stay.store(true, Ordering::SeqCst);
    let host = SessionHost::with_release_wait(fake.clone(), Duration::from_millis(200));
    (host, fake)
}

fn release_s1(connection: &mut HostConnection<Core, Out>, id: RequestId) {
    request(
        connection,
        id,
        Op::ReleaseSession {
            session: "s1".into(),
        },
    );
}

/// What keeps a session resident is named: an attachment made since (one
/// that has not seen the request yet), or a background process.
#[test]
fn release_session_names_an_attachment_or_a_process_that_keeps_it() {
    let (host, _fake) = staying_host();
    let other = Out::default();
    let mut attached_elsewhere = host.connect(other.clone(), Face::Remote, || false);
    request(&mut attached_elsewhere, 1, Op::Attach { node: node("s1") });
    attached(&other.take(), 1);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    release_s1(&mut connection, 1);
    assert_eq!(
        outcome(&out.take(), 1),
        Outcome::Ok(Answer::Resident {
            reason: ResidentReason::Attachment
        })
    );
    attached_elsewhere.leave();

    request(&mut connection, 2, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 2);
    prompt(&mut connection, 3, attachment, "process");
    let deadline = Instant::now() + Duration::from_secs(10);
    while row_of(&host, "s1").live_work.processes == 0 {
        assert!(Instant::now() < deadline, "the process never showed");
        std::thread::sleep(Duration::from_millis(5));
    }
    release_s1(&mut connection, 4);
    assert_eq!(
        outcome(&out.take(), 4),
        Outcome::Ok(Answer::Resident {
            reason: ResidentReason::Processes
        })
    );
    handed_off(&host);
}

/// An attach made after a withdrawn release is not ended by that release.
#[test]
fn an_attach_after_a_withdrawn_release_stays_attached() {
    let (host, _fake) = staying_host();
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    release_s1(&mut connection, 2);
    assert!(matches!(
        outcome(&out.take(), 2),
        Outcome::Ok(Answer::Resident { .. })
    ));
    request(&mut connection, 3, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 3);
    connection.flush().unwrap();
    assert!(
        !out
            .take()
            .iter()
            .any(|frame| matches!(frame, HostFrame::Detached { attachment: ended } if *ended == attachment)),
        "the new attachment was detached"
    );
    assert!(connection.node.is_some());
    handed_off(&host);
}

/// A release request on a session already releasing waits for that release
/// to commit before it answers `released`, so another face may open the
/// session at once.
#[test]
fn release_session_of_a_releasing_session_answers_once_it_released() {
    let (host, fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    connection.leave();
    // The release lingers in its worker for a while.
    fake.0.linger.store(1_000, Ordering::SeqCst);
    let first = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || {
            let out = Out::default();
            let mut connection = host.connect(out.clone(), Face::Remote, || false);
            release_s1(&mut connection, 1);
            outcome(&out.take(), 1)
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while row_of(&host, "s1").residency != SessionResidency::Releasing {
        assert!(Instant::now() < deadline, "the release never showed");
        std::thread::sleep(Duration::from_millis(5));
    }
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    release_s1(&mut connection, 2);
    assert_eq!(outcome(&out.take(), 2), Outcome::Ok(Answer::Released));
    assert!(
        matches!(host.slot("s1").peek(), SlotView::Dormant(_)),
        "released was answered before the release committed"
    );
    assert_eq!(first.join().unwrap(), Outcome::Ok(Answer::Released));
}

/// A bye, a second hello or a line that is not a frame is the client
/// leaving: its node attachment detaches and its host attachment closes.
#[test]
fn a_leaving_client_detaches_everything() {
    for leaving in [
        encode_line(&ClientFrame::<Core>::Bye).unwrap(),
        "not a frame".to_owned(),
    ] {
        let (host, _) = host(&["s1"]);
        let out = Out::default();
        let mut connection = host.connect(out.clone(), Face::Remote, || false);
        request(&mut connection, 1, Op::Attach { node: node("s1") });
        request(&mut connection, 2, Op::WatchSessions);
        assert!(!connection.frame(Ok(leaving)).unwrap());
        assert!(connection.node.is_none());
        assert!(connection.watch.is_none());
        handed_off(&host);
    }
}

/// A stop of the host hands every resident session off; a dormant one has
/// nothing to hand off.
#[test]
fn hand_off_all_releases_every_resident_session() {
    let (host, fake) = host(&["s1", "s2"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    connection.leave();
    let _dormant = host.slot("s2");
    handed_off(&host);
    for slot in host.slots() {
        assert!(matches!(slot.peek(), SlotView::Dormant(_)));
    }
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
}

fn prompt(
    connection: &mut HostConnection<Core, Out>,
    id: RequestId,
    attachment: AttachmentId,
    text: &str,
) {
    request(
        connection,
        id,
        Op::Intent {
            attachment,
            intent: SessionIntent::Prompt(text.into()),
            blobs: Vec::new(),
        },
    );
}

fn row_of(host: &SessionHost<Core>, session: &str) -> SessionRow {
    host.changed();
    host.directory()
        .unwrap()
        .iter()
        .find(|row| row.session == session)
        .cloned()
        .unwrap()
}

/// An attach of a node in the tree the connection already serves stays on
/// that session's connection: nothing on the host changes, so no host
/// attachment reads the directory again.
#[test]
fn an_attach_in_the_served_tree_changes_nothing_on_the_host() {
    let (host, fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    let mut changes = host.changes();
    changes.borrow_and_update();
    request(&mut connection, 2, Op::Attach { node: node("s1") });
    attached(&out.take(), 2);
    assert!(
        !changes.has_changed().unwrap(),
        "the same tree's attach went through the host"
    );
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    connection.leave();
    handed_off(&host);
}

/// A refused attach of a node in another tree leaves the connection's node
/// attachment as it was.
#[test]
fn a_refused_cross_tree_attach_keeps_the_attachment() {
    let (host, _) = host(&["s1", "s2"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    // A node of s2's tree that is not its root: no such child.
    request(
        &mut connection,
        2,
        Op::Attach {
            node: NodeAddress {
                tree_id: "s2".into(),
                agent_id: "no-such-child".into(),
            },
        },
    );
    assert!(matches!(outcome(&out.take(), 2), Outcome::Refused(_)));
    assert_eq!(
        connection
            .node
            .as_ref()
            .and_then(|link| link.connection.attachment()),
        Some(attachment),
        "a refused attach ended the attachment"
    );
    connection.leave();
    handed_off(&host);
}

/// An attachment the session's service ended by itself (it fell behind on
/// required events) lets the session go: the connection holds nothing, and
/// no hold keeps the session resident.
#[test]
fn an_attachment_its_service_ended_lets_the_session_go() {
    let (host, fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    let session = Arc::clone(&connection.node.as_ref().unwrap().session);
    prompt(&mut connection, 2, attachment, "flood");
    // Nothing takes the attachment's queue while the flood fills it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fake.0.flooded.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the flood was never sent");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));
    let deadline = Instant::now() + Duration::from_secs(10);
    while connection.node.is_some() {
        assert!(
            Instant::now() < deadline,
            "the flooded attachment was never ended"
        );
        connection.flush().unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        out.take()
            .contains(&HostFrame::ResyncRequired { attachment })
    );
    assert_eq!(session.attached(), 0, "an ended attachment kept its hold");
    connection.leave();
    handed_off(&host);
}

/// A resident session awaiting an approval reads awaiting_operator.
#[test]
fn a_session_awaiting_an_approval_reads_awaiting_operator() {
    let (host, _) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    prompt(&mut connection, 2, attachment, "approve");
    let deadline = Instant::now() + Duration::from_secs(10);
    while row_of(&host, "s1").activity != SessionActivity::AwaitingOperator {
        assert!(Instant::now() < deadline, "the approval never showed");
        std::thread::sleep(Duration::from_millis(20));
    }
    connection.leave();
    handed_off(&host);
}

/// `stop_session` of the attached session is its own connection's, answered
/// once the stop is durable; one this connection is not attached to is
/// refused, and one the host does not know is `unknown_session`.
#[test]
fn stop_session_stops_the_attached_session() {
    let (host, _fake) = host(&["s1", "s2"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    let stop = |session: &str| Op::StopSession {
        session: session.into(),
    };
    request(&mut connection, 1, stop("s1"));
    assert_eq!(
        outcome(&out.take(), 1),
        Outcome::Refused(RequestRefusal::Unsupported {
            detail: "stop_session stops the session this connection is attached to; attach it \
                     first"
                .into()
        })
    );
    request(&mut connection, 2, stop("nope"));
    assert_eq!(
        outcome(&out.take(), 2),
        Outcome::Refused(RequestRefusal::UnknownSession)
    );
    request(&mut connection, 3, Op::Attach { node: node("s1") });
    attached(&out.take(), 3);
    // Attached to s1, a stop of s2 is not this connection's.
    request(&mut connection, 4, stop("s2"));
    assert!(matches!(
        outcome(&out.take(), 4),
        Outcome::Refused(RequestRefusal::Unsupported { .. })
    ));
    request(&mut connection, 5, stop("s1"));
    assert_eq!(
        answer(&mut connection, &out, 5),
        Outcome::Ok(Answer::Stopped)
    );
}

/// Flushes `connection` until request `id` is answered, within 10 s.
fn answer(connection: &mut HostConnection<Core, Out>, out: &Out, id: RequestId) -> Outcome {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut frames = Vec::new();
    loop {
        connection.flush().unwrap();
        frames.extend(out.take());
        if frames.iter().any(
            |frame| matches!(frame, HostFrame::Response { id: answered, .. } if *answered == id),
        ) {
            return outcome(&frames, id);
        }
        assert!(
            Instant::now() < deadline,
            "request {id} was never answered: {frames:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A stop is answered even when its node attachment ends first, by a
/// detach or by an attach of another session: the stop's own session
/// connection outlives the attachment until the stop settles.
#[test]
fn a_stop_is_answered_after_its_attachment_ends() {
    let (host, fake) = host(&["s1", "s2"]);
    for then in ["detach", "attach another"] {
        // The stop settles only after the attachment ended.
        fake.0.hold_stops.store(true, Ordering::SeqCst);
        let out = Out::default();
        let mut connection = host.connect(out.clone(), Face::Remote, || false);
        request(&mut connection, 1, Op::Attach { node: node("s1") });
        let attachment = attached(&out.take(), 1);
        request(
            &mut connection,
            2,
            Op::StopSession {
                session: "s1".into(),
            },
        );
        if then == "detach" {
            request(&mut connection, 3, Op::Detach { attachment });
        } else {
            request(&mut connection, 3, Op::Attach { node: node("s2") });
        }
        connection.flush().unwrap();
        fake.0.hold_stops.store(false, Ordering::SeqCst);
        assert_eq!(
            answer(&mut connection, &out, 2),
            Outcome::Ok(Answer::Stopped),
            "then {then}"
        );
        connection.leave();
    }
}

/// The settle of a stop whose attachment ended wakes a connection that
/// waits for something to write, though it attaches and watches nothing.
#[test]
fn a_draining_stops_settle_wakes_the_connection() {
    let (host, fake) = host(&["s1"]);
    fake.0.hold_stops.store(true, Ordering::SeqCst);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    request(
        &mut connection,
        2,
        Op::StopSession {
            session: "s1".into(),
        },
    );
    request(&mut connection, 3, Op::Detach { attachment });
    connection.flush().unwrap();
    let release = {
        let fake = fake.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            fake.0.hold_stops.store(false, Ordering::SeqCst);
        })
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let woke = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), connection.changed())
            .await
            .is_ok()
    });
    release.join().unwrap();
    assert!(woke, "the settle did not wake the connection");
    assert_eq!(
        answer(&mut connection, &out, 2),
        Outcome::Ok(Answer::Stopped)
    );
}

/// A stop still to be answered after its attachment ended is the
/// connection's one stop in flight: another is refused until it is
/// answered.
#[test]
fn a_draining_stop_is_the_connections_one_in_flight() {
    let (host, fake) = host(&["s1"]);
    fake.0.hold_stops.store(true, Ordering::SeqCst);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    let stop = Op::StopSession {
        session: "s1".into(),
    };
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    request(&mut connection, 2, stop.clone());
    request(&mut connection, 3, Op::Detach { attachment });
    request(&mut connection, 4, Op::Attach { node: node("s1") });
    attached(&out.take(), 4);
    request(&mut connection, 5, stop);
    assert_eq!(
        outcome(&out.take(), 5),
        Outcome::Refused(RequestRefusal::OneInFlight)
    );
    fake.0.hold_stops.store(false, Ordering::SeqCst);
    assert_eq!(
        answer(&mut connection, &out, 2),
        Outcome::Ok(Answer::Stopped)
    );
}

/// A client that leaves owes no stop an answer: its draining session
/// connection goes too, and with it its hold on the session's residency.
#[test]
fn a_leaving_client_lets_a_draining_stops_session_go() {
    let fake = Fake::default();
    *fake.0.rows.lock().unwrap() = vec![row("s1", 0)];
    fake.0.hold_stops.store(true, Ordering::SeqCst);
    let host = SessionHost::with_release_wait(fake.clone(), Duration::from_millis(500));
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    request(
        &mut connection,
        2,
        Op::StopSession {
            session: "s1".into(),
        },
    );
    request(&mut connection, 3, Op::Detach { attachment });
    connection.leave();
    let other = Out::default();
    let mut releasing = host.connect(other.clone(), Face::Remote, || false);
    release_s1(&mut releasing, 1);
    assert_eq!(outcome(&other.take(), 1), Outcome::Ok(Answer::Released));
}

/// A session request pending when its attachment ends is answered
/// `not_attached`, as on a connection that serves one session.
#[test]
fn a_pending_session_request_is_not_attached_once_its_attachment_ends() {
    let (host, _fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    request(
        &mut connection,
        2,
        Op::ResumeSession {
            attachment,
            request_id: "r1".into(),
            session: "s9".into(),
        },
    );
    request(&mut connection, 3, Op::Detach { attachment });
    assert_eq!(
        answer(&mut connection, &out, 2),
        Outcome::Refused(RequestRefusal::NotAttached)
    );
}

/// A session resident here is held by this host, whatever the directory's
/// read said of it: a read that raced its open names its last holder.
#[test]
fn a_resident_sessions_row_names_no_holder_elsewhere() {
    let (host, fake) = host(&["s1"]);
    fake.0.rows.lock().unwrap()[0].hosted_elsewhere =
        Some(plakous_host_protocol::wire::HostedElsewhere {
            host_name: "elsewhere".into(),
            binary: None,
            pid: 7,
        });
    assert!(row_of(&host, "s1").hosted_elsewhere.is_some());
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    let row = row_of(&host, "s1");
    assert_eq!(row.residency, SessionResidency::Resident);
    assert_eq!(row.hosted_elsewhere, None);
}

/// A session reads opening while its open runs, and releasing while its
/// release runs, both when its actor began to release and while a trigger
/// joins that release.
#[test]
fn a_rows_residency_follows_the_open_and_the_release() {
    let (host, fake) = host(&["s1"]);
    let (open, gate) = std::sync::mpsc::channel();
    *fake.0.gate.lock().unwrap() = Some(gate);
    let opening = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || {
            let out = Out::default();
            let mut connection = host.connect(out.clone(), Face::Remote, || false);
            request(&mut connection, 1, Op::Attach { node: node("s1") });
            attached(&out.take(), 1);
            connection.leave();
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while fake.0.opens.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the open never began");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(row_of(&host, "s1").residency, SessionResidency::Opening);
    open.send(()).unwrap();
    opening.join().unwrap();
    assert_eq!(row_of(&host, "s1").residency, SessionResidency::Resident);
    // The release lingers in its worker for a while.
    fake.0.linger.store(1_500, Ordering::SeqCst);
    let releasing = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || {
            let out = Out::default();
            let mut connection = host.connect(out.clone(), Face::Remote, || false);
            request(
                &mut connection,
                1,
                Op::ReleaseSession {
                    session: "s1".into(),
                },
            );
            outcome(&out.take(), 1)
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while row_of(&host, "s1").residency != SessionResidency::Releasing {
        assert!(Instant::now() < deadline, "the release never showed");
        std::thread::sleep(Duration::from_millis(5));
    }
    // A trigger now joins the lingering release under the slot's lock; its
    // open after the join is refused, so the slot stays dormant.
    fake.0.refuse.lock().unwrap().insert(
        "s1".into(),
        RequestRefusal::SessionUnavailable {
            detail: "refused by the script".into(),
        },
    );
    let joining = {
        let slot = host.slot("s1");
        std::thread::spawn(move || drop(slot.resident()))
    };
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(row_of(&host, "s1").residency, SessionResidency::Releasing);
    assert_eq!(releasing.join().unwrap(), Outcome::Ok(Answer::Released));
    joining.join().unwrap();
    handed_off(&host);
}

/// A blob sent before the intent that names it reaches the session with
/// the intent's image.
#[test]
fn a_blob_reaches_the_session_with_its_intent() {
    let (host, fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    let mut intent = SessionIntent::<Core>::Prompt(plakous_host_protocol::Draft {
        text: "look".into(),
        images: vec![plakous_host_protocol::intent::DraftImage {
            name: "a.png".into(),
            media_type: "image/png".into(),
            bytes: vec![7; 4096],
            source_dimensions: None,
        }],
    });
    let taken = plakous_host_protocol::wire::take_images(&mut intent, "c1");
    for chunk in taken.chunks {
        let line = encode_line(&ClientFrame::<Core>::Blob(chunk)).unwrap();
        assert!(connection.frame(Ok(line)).unwrap());
    }
    request(
        &mut connection,
        2,
        Op::Intent {
            attachment,
            intent,
            blobs: taken.ids,
        },
    );
    assert_eq!(outcome(&out.take(), 2), Outcome::Ok(Answer::Accepted));
    let deadline = Instant::now() + Duration::from_secs(10);
    while fake.0.images.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the prompt never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*fake.0.images.lock().unwrap(), [4096]);
    connection.leave();
    handed_off(&host);
}

/// A stopping host opens nothing more and creates nothing: an attach of a
/// dormant session and a new session are refused, and an open already
/// asked for is refused too.
#[test]
fn a_closed_host_opens_and_creates_nothing() {
    let (host, fake) = host(&["s1"]);
    let dormant = host.slot("s1");
    assert_eq!(host.hand_off_all(Duration::from_secs(1)), Vec::new());
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    request(
        &mut connection,
        2,
        Op::NewSession {
            workspace: "/w".into(),
            template: None,
            request_id: "r9".into(),
        },
    );
    let frames = out.take();
    for id in [1, 2] {
        assert!(
            matches!(
                outcome(&frames, id),
                Outcome::Refused(RequestRefusal::SessionUnavailable { .. })
            ),
            "{frames:?}"
        );
    }
    assert!(
        dormant.resident().is_err(),
        "a slot opened on a stopping host"
    );
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 0);
    assert!(fake.0.creates.lock().unwrap().is_empty());
}
