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
        environment: SessionEnvironment::HostLoginShell,
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
            loop {
                match intents.recv_timeout(Duration::from_millis(10)) {
                    Ok(delivery) => match &*delivery {
                        SessionIntent::Quit => break,
                        SessionIntent::Prompt(_) => {
                            let _ = busy.send(SessionObservation::SetBusy(true));
                        }
                        _ => {}
                    },
                    // The actor's idle release, asked to release now.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if !script.stay.load(Ordering::SeqCst)
                            && count.release_now()
                            && count.begin_release(count.seen())
                        {
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
            environment: SessionEnvironment::HostLoginShell,
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
        connection.watch.as_mut().unwrap().dirty = true;
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
    connection.watch.as_mut().unwrap().dirty = true;
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
        environment: SessionEnvironment::HostLoginShell,
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

/// The requests this host does not do yet are refused unsupported, naming
/// the op.
#[test]
fn stop_is_refused_unsupported_for_now() {
    let (host, _) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(
        &mut connection,
        1,
        Op::StopSession {
            session: "s1".into(),
        },
    );
    let frames = out.take();
    assert_eq!(
        outcome(&frames, 1),
        Outcome::Refused(RequestRefusal::Unsupported {
            detail: "stop_session is not available on this host yet".into()
        })
    );
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
