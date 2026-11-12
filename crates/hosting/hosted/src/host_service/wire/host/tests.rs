use std::sync::atomic::AtomicUsize;

use plakous_host_protocol::wire::{LiveWork, RequestId, encode_line};
use plakous_host_protocol::{Core, SessionIntent, SessionObservation};

use super::*;
use crate::runtime::HostWorkerHandle;

mod lifecycle;

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
            let _ = busy.send(SessionObservation::SetBusy(true));
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
        runner: plakous_host_protocol::wire::RunnerLabel::local(),
        runner_status: plakous_host_protocol::wire::RunnerStatus::Unknown,
        needs_operator: false,
        activity_seq: 0,
        archived: false,
        differences: Vec::new(),
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
    successors: Mutex<BTreeMap<String, String>>,
    active_workers: AtomicUsize,
    unrelated_lifecycle_workers: AtomicUsize,
    lifecycle_mutations: AtomicUsize,
    lease: Mutex<Option<plakous::sessions::LeaseOnlyHead>>,
    processes: std::sync::atomic::AtomicBool,
    hold_mailbox: std::sync::atomic::AtomicBool,
    queued: AtomicUsize,
    rows: Mutex<Vec<SessionRow>>,
    row_limit: AtomicUsize,
    unreadable_rows: std::sync::atomic::AtomicBool,
    pending_stops: std::sync::atomic::AtomicBool,
    opens: AtomicUsize,
    dormant_stops: AtomicUsize,
    stop_intents: AtomicUsize,
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
    /// Each opened session's attachment count.
    counts: Mutex<BTreeMap<String, crate::host_service::AttachmentCount>>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Script>);

impl Script {
    fn root(&self, session: &str) -> NodeAddress {
        node(
            self.trees
                .lock()
                .unwrap()
                .get(session)
                .map_or(session, String::as_str),
        )
    }

    fn release_reopen(
        &self,
        intents: &plakous_host_protocol::IntentReceiver<Core>,
        ticket: plakous_host_protocol::StopTicket,
        session: &str,
        identity: &str,
        active: bool,
    ) -> bool {
        let current = self
            .successors
            .lock()
            .unwrap()
            .get(identity)
            .cloned()
            .unwrap_or_else(|| identity.to_owned());
        let outcome = if session != current {
            Err("the session moved".into())
        } else if active || self.processes.load(Ordering::SeqCst) {
            Err("work is running".into())
        } else {
            Ok(())
        };
        let quit = outcome.is_ok();
        if intents.settle_release_for_reopen(ticket, outcome) && quit {
            std::thread::sleep(Duration::from_millis(self.linger.load(Ordering::SeqCst)));
            return true;
        }
        false
    }
}

impl HostSessions<Core> for Fake {
    fn host_read_limits(&self) -> HostReadLimits {
        HostReadLimits {
            request_bytes: 64 * 1024,
            per_connection: 2,
            per_principal: 4,
            host: 32,
            deadline: Duration::from_secs(10),
            messages_per_second: 12,
            burst: 12,
        }
    }
    fn welcome_extension(
        &self,
        _: &HostPrincipal,
        _: &HostReadLimits,
    ) -> Option<plakous_host_protocol::Never> {
        None
    }
    fn contains(&self, session: &str) -> Result<bool, String> {
        Ok(self
            .0
            .rows
            .lock()
            .unwrap()
            .iter()
            .any(|row| row.session == session))
    }
    fn archive(&self, session: &str, archived: bool) -> Result<(), RequestRefusal> {
        self.0.lifecycle_mutations.fetch_add(1, Ordering::SeqCst);
        if archived && !self.0.successors.lock().unwrap().contains_key(session) {
            assert_eq!(
                self.0.active_workers.load(Ordering::SeqCst),
                self.0.unrelated_lifecycle_workers.load(Ordering::SeqCst),
                "lifecycle raced the resident worker"
            );
        }
        if let Some(row) = self
            .0
            .rows
            .lock()
            .unwrap()
            .iter_mut()
            .find(|row| row.session == session)
        {
            row.archived = archived;
        }
        Ok(())
    }

    fn delete(&self, session: &str, _abandon_children: bool) -> Result<(), RequestRefusal> {
        self.0.lifecycle_mutations.fetch_add(1, Ordering::SeqCst);
        if !self.0.successors.lock().unwrap().contains_key(session) {
            assert_eq!(
                self.0.active_workers.load(Ordering::SeqCst),
                self.0.unrelated_lifecycle_workers.load(Ordering::SeqCst),
                "delete raced the resident worker"
            );
        }
        self.0
            .rows
            .lock()
            .unwrap()
            .retain(|row| row.session != session);
        Ok(())
    }
    fn options(
        &self,
        _workspace: Option<&plakous_host_protocol::wire::WorkspaceProbe>,
        progress: &mut dyn FnMut(PickerProgress) -> Result<(), String>,
    ) -> Result<plakous_host_protocol::wire::SessionOptions, RequestRefusal> {
        use plakous_host_protocol::wire::{
            RunnerLabel, RunnerOptions, RunnerPhase, RunnerStatus, SessionOptions,
        };
        let row = RunnerOptions {
            runner: RunnerLabel::local(),
            aliases: Vec::new(),
            status: RunnerStatus::Ready,
            checked_at: Some(1),
            blocked_since: None,
            roots: None,
            default_workspace: Some("/w".into()),
            recent_workspaces: Vec::new(),
            discovered_workspaces: Vec::new(),
            discovery_truncated: false,
        };
        for event in [
            PickerProgress {
                runner: row.runner.clone(),
                phase: None,
                row: None,
            },
            PickerProgress {
                runner: row.runner.clone(),
                phase: Some(RunnerPhase::Discovery),
                row: Some(row.clone()),
            },
        ] {
            progress(event).map_err(|detail| RequestRefusal::SessionUnavailable { detail })?;
        }
        Ok(SessionOptions {
            runners: vec![row],
            models: plakous_host_protocol::views::ReadyCatalog::default(),
            profiles: plakous_host_protocol::views::ProfilesView::default(),
            templates: Vec::new(),
            workspace: None,
        })
    }

    fn stored_view(
        &self,
        session: &str,
        include_unarchived: bool,
    ) -> Result<Option<StoredSessionView<Core>>, RequestRefusal> {
        let archived = self
            .0
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|row| row.session == session)
            .map(|row| row.archived)
            .filter(|archived| *archived || include_unarchived);
        Ok(archived.map(|archived| StoredSessionView {
            archived,
            session: session.to_owned(),
            root: node(session),
            observations: vec![SessionObservation::PresentationSnapshot(
                plakous_host_protocol::views::PresentationSnapshot {
                    schema: plakous_host_protocol::views::presentation::PRESENTATION_SCHEMA_V3,
                    scope: plakous_host_protocol::views::transcript::TranscriptScope::Root,
                    revision: 1,
                    items: Vec::new(),
                },
            )],
        }))
    }

    fn rows(
        &self,
        workspace: Option<&str>,
        runner: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<SessionRow>, String> {
        if self.0.unreadable_rows.load(Ordering::SeqCst) {
            return Err("directory unavailable".into());
        }
        Ok(self
            .0
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|row| workspace.is_none_or(|workspace| row.workspace == workspace))
            .filter(|row| runner.is_none_or(|id| row.runner.runner_id == id))
            .filter(|row| include_archived || !row.archived)
            .take(match self.0.row_limit.load(Ordering::SeqCst) {
                0 => usize::MAX,
                limit => limit,
            })
            .cloned()
            .collect())
    }

    fn runner_pending(&self, _session: &str) -> bool {
        self.0.pending_stops.load(Ordering::SeqCst)
    }

    fn durable_idle_exit_blocked(&self) -> bool {
        self.0.unreadable_rows.load(Ordering::SeqCst)
            || self.0.pending_stops.load(Ordering::SeqCst)
            || self
                .0
                .rows
                .lock()
                .unwrap()
                .iter()
                .any(|row| row.activity == SessionActivity::Continuing)
    }

    fn session_of(&self, node: &NodeAddress) -> Result<Option<String>, String> {
        let rows = self.0.rows.lock().unwrap();
        let successors = self.0.successors.lock().unwrap();
        Ok(rows
            .iter()
            .find(|row| {
                row.node.tree_id == node.tree_id
                    && !successors.get(&row.session).is_some_and(|id| {
                        rows.iter()
                            .any(|next| &next.session == id && next.node == row.node)
                    })
            })
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
        self.0
            .counts
            .lock()
            .unwrap()
            .insert(session.to_owned(), count.clone());
        let script = Arc::clone(&self.0);
        let lease = script.lease.lock().unwrap().take();
        let identity = session.to_owned();
        script.active_workers.fetch_add(1, Ordering::SeqCst);
        let worker = HostWorkerHandle::spawn(move || {
            drop(observations);
            let mut stops = Vec::new();
            let mut active = false;
            loop {
                if script.hold_mailbox.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                if !script.hold_stops.load(Ordering::SeqCst) {
                    for ticket in stops.drain(..) {
                        intents.settle_stop(ticket, Ok(()));
                    }
                }
                match intents.recv_timeout(Duration::from_millis(10)) {
                    Ok(delivery) => match &*delivery {
                        SessionIntent::Quit => break,
                        SessionIntent::StopSession { ticket } => {
                            script.stop_intents.fetch_add(1, Ordering::SeqCst);
                            stops.push(*ticket);
                        }
                        SessionIntent::ReleaseForReopen { ticket, session } => {
                            if script.release_reopen(&intents, *ticket, session, &identity, active)
                            {
                                break;
                            }
                        }
                        SessionIntent::Prompt(draft) => {
                            active = true;
                            prompted(&script, &busy, draft);
                        }
                        SessionIntent::FollowUp(_) => {
                            script.queued.fetch_add(1, Ordering::SeqCst);
                        }
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
            if let Some(lease) = lease {
                crate::runtime::test_kit::BlockOn::block(lease.release()).unwrap();
            }
            script.active_workers.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        });
        Ok(OpenedSession {
            runner_pending: None,
            root: self.0.root(session),
            session_id: session.to_owned(),
            actions,
            events,
            worker,
            attachments,
            roots: {
                let script = Arc::clone(&self.0);
                Box::new(move |id| Some(script.root(id)))
            },
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

    fn create(&self, request: &Creation<'_>) -> Result<Created, RequestRefusal> {
        let workspace = request.workspace;
        let request_id = request.request_id;
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

    fn rebind(&self, session: &str, _cause: &str) -> CliResult<Created> {
        let target = format!("{session}-rebound");
        self.0
            .trees
            .lock()
            .unwrap()
            .insert(target.clone(), session.to_owned());
        let mut target_row = row(&target, 100);
        target_row.node = node(session);
        self.0.rows.lock().unwrap().insert(0, target_row);
        Ok(Created {
            session: target,
            node: node(session),
            environment: SessionEnvironment::Host,
        })
    }

    fn stop_dormant(&self, session: &str) -> CliResult<()> {
        self.0.dormant_stops.fetch_add(1, Ordering::SeqCst);
        let mut rows = self.0.rows.lock().unwrap();
        rows.iter_mut()
            .find(|row| row.session == session)
            .unwrap()
            .activity = SessionActivity::Stopped;
        Ok(())
    }
}

#[test]
fn host_requests_reopen_only_after_actor_release_and_move_every_viewer() {
    for rebind in [false, true] {
        let (host, fake) = host(&["s1"]);
        let out = Out::default();
        let mut connection = host.connect(
            out.clone(),
            Face::Remote,
            HostPrincipal::Socket(501),
            || false,
        );
        request(&mut connection, 1, Op::Attach { node: node("s1") });
        let attachment = attached(&out.take(), 1);
        let other_out = Out::default();
        let mut other = host.connect(
            other_out.clone(),
            Face::Remote,
            HostPrincipal::Socket(501),
            || false,
        );
        request(&mut other, 1, Op::Attach { node: node("s1") });
        let other_attachment = attached(&other_out.take(), 1);
        let op = if rebind {
            Op::Rebind {
                attachment,
                request_id: "repair".into(),
                cause: "binding repair".into(),
            }
        } else {
            Op::RetryAttach {
                attachment,
                request_id: "repair".into(),
            }
        };
        let source_slot = host.slot("s1");
        request(&mut connection, 2, op);
        assert_eq!(
            outcome(&out.take(), 2),
            Outcome::Ok(Answer::Moved { node: node("s1") })
        );
        other.flush().unwrap();
        assert!(other_out.take().iter().any(|frame| matches!(frame, HostFrame::Moved { attachment, request_id: Some(id), .. } if *attachment == other_attachment && id == "repair")));
        request(&mut connection, 3, Op::Attach { node: node("s1") });
        attached(&out.take(), 3);
        assert_eq!(fake.0.opens.load(Ordering::SeqCst), 2);
        assert_eq!(host.slots().len(), 1, "a reattach created a competing slot");
        if rebind {
            assert!(
                source_slot
                    .stop_target("s1", |_| panic!("a moved stop reached the successor"))
                    .is_err()
            );
        }
        connection.leave();
        other.leave();
        handed_off(&host);
    }
}

#[test]
fn a_busy_actor_refuses_reopen_without_stopping_or_redispatching() {
    let (host, fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    prompt(&mut connection, 2, attachment, "running");
    request(
        &mut connection,
        3,
        Op::RetryAttach {
            attachment,
            request_id: "retry".into(),
        },
    );
    assert!(matches!(
        outcome(&out.take(), 3),
        Outcome::Refused(RequestRefusal::SessionUnavailable { .. })
    ));
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    assert_eq!(fake.0.images.lock().unwrap().len(), 1);
    connection.leave();
    handed_off(&host);
}

#[test]
fn a_reopen_ack_timeout_cancels_the_fence_and_keeps_the_actor() {
    for mode in 0..3 {
        cancelled_reopen(mode);
    }
}

#[test]
fn a_committed_reopen_joins_a_late_idle_quit_before_opening() {
    let root = tempfile::tempdir().unwrap();
    let (challenger, lease) = held_lease(root.path());
    let line = lease.line();
    let fake = Fake::default();
    *fake.0.lease.lock().unwrap() = Some(lease);
    fake.0.stay.store(true, Ordering::SeqCst);
    fake.0.linger.store(50, Ordering::SeqCst);
    let opener = fake.clone();
    let slot = SessionSlot::new(fake.open("s1").unwrap(), move |id| opener.open(id));
    let (session, _hold) = slot.resident().unwrap();
    slot.reopen(&session, "s1", 1, "repair", |_| {
        // The actor acknowledged before its delayed teardown released the
        // lease. Carry must run only after that exact worker was joined.
        assert_lease_available(&challenger, line, "lease:carry");
        Ok(("s1".into(), node("s1")))
    })
    .unwrap();
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 2);
    slot.hand_off(Duration::from_secs(2)).unwrap();
}

fn actor_request(connection: &mut WireConnection<Core, Out>, id: RequestId, op: Op<Core>) {
    connection
        .frame(Ok(
            encode_line(&ClientFrame::<Core>::Request { id, op }).unwrap()
        ))
        .unwrap();
}

fn held_lease(
    root: &std::path::Path,
) -> (
    plakous::sessions::AgentSessions,
    plakous::sessions::LeaseOnlyHead,
) {
    use crate::runtime::test_kit::BlockOn as _;
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
        SessionOperationId,
    };
    async {
        let database = root.join("runtime.sqlite3");
        let sessions = AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap();
        let script =
            Arc::new(serde_json::from_value(serde_json::json!({"version":1,"steps":[]})).unwrap());
        let controller =
            plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                root.join("evidence.jsonl"),
            )
            .unwrap();
        let binding =
            plakous_offline_crash_harness::scripted_runtime_binding(root, controller, script)
                .unwrap();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("lease:create").unwrap(),
                binding,
            ))
            .await
            .unwrap()
        else {
            panic!("not created")
        };
        let locator = session.release().await.unwrap();
        let line = sessions.line_head_of(locator).await.unwrap().line();
        let head = sessions
            .open_lease_only(&SessionOperationId::new("lease:hold").unwrap(), line)
            .await
            .unwrap();
        let challenger = AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap();
        (challenger, head)
    }
    .block()
}

fn cancelled_reopen(mode: u8) {
    use crate::runtime::test_kit::BlockOn as _;
    let root = tempfile::tempdir().unwrap();
    let (challenger, lease) = held_lease(root.path());
    let line = lease.line();
    let fake = Fake::default();
    *fake.0.lease.lock().unwrap() = Some(lease);
    fake.0.processes.store(mode == 2, Ordering::SeqCst);
    fake.0.stay.store(true, Ordering::SeqCst);
    fake.0.hold_mailbox.store(true, Ordering::SeqCst);
    let opener = fake.clone();
    let slot = SessionSlot::new(fake.open("s1").unwrap(), move |id| opener.open(id))
        .with_release_join(Duration::from_millis(50));
    let (session, _hold) = slot.resident().unwrap();
    let out = Out::default();
    let mut viewer = session.connect(out.clone(), Face::Remote, || false);
    actor_request(&mut viewer, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    if mode == 1 {
        actor_request(
            &mut viewer,
            2,
            Op::Intent {
                attachment,
                intent: SessionIntent::Prompt("busy".into()),
                blobs: Vec::new(),
                client_id: None,
            },
        );
    }
    let changed = AtomicUsize::new(0);
    let (send, receive) = std::sync::mpsc::channel();
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            let result = slot.reopen(&session, "s1", 1, "repair", |_| {
                changed.fetch_add(1, Ordering::SeqCst);
                Ok(("s1".into(), node("s1")))
            });
            send.send(result).unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        let served = matches!(slot.peek(), SlotView::Resident(_));
        fake.0.hold_mailbox.store(false, Ordering::SeqCst);
        (result, served)
    });
    let timed_out = result
        .0
        .expect("an unread mailbox held the slot indefinitely");
    assert!(timed_out.unwrap_err().message().contains("acknowledge"));
    assert!(result.1, "expiry stranded a live actor");
    assert!(!session.reopening());
    assert!(!session.ended());
    assert_eq!(
        changed.load(Ordering::SeqCst),
        0,
        "a timeout performed a carry"
    );
    let (next, _hold) = slot.resident().unwrap();
    assert!(Arc::ptr_eq(&session, &next));
    let mut attached_again = next.connect(out.clone(), Face::Remote, || false);
    actor_request(&mut attached_again, 3, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 3);
    actor_request(
        &mut attached_again,
        4,
        Op::Intent {
            attachment,
            intent: SessionIntent::FollowUp("still served".into()),
            blobs: Vec::new(),
            client_id: None,
        },
    );
    let until = Instant::now() + Duration::from_secs(2);
    while fake.0.queued.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < until,
            "the actor quit on a cancelled idle fence"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    let held = challenger
        .open_lease_only(
            &plakous::sessions::SessionOperationId::new("lease:contend").unwrap(),
            line,
        )
        .block()
        .unwrap_err();
    assert_eq!(
        held.kind(),
        plakous::sessions::AgentSessionsErrorKind::Ownership
    );
    viewer.leave();
    attached_again.leave();
    slot.hand_off(Duration::from_secs(2)).unwrap();
    assert_lease_available(&challenger, line, "lease:after");
}

fn assert_lease_available(
    sessions: &plakous::sessions::AgentSessions,
    line: plakous::model::LineId,
    operation: &str,
) {
    use crate::runtime::test_kit::BlockOn as _;
    sessions
        .open_lease_only(
            &plakous::sessions::SessionOperationId::new(operation).unwrap(),
            line,
        )
        .block()
        .unwrap()
        .release()
        .block()
        .unwrap();
}

#[test]
fn directory_rows_count_live_and_held_children_without_counting_the_root() {
    use crate::agent_tree::{AgentCommand, AgentCommandKind};
    use crate::agent_tree_protocol::SpawnAgentRequest;
    use crate::runtime::test_kit::BlockOn as _;
    let dir = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let runtime = crate::runtime::test_kit::install_idle_topology_hub(&id, dir.path());
    runtime
        .mutate_then_emit(false, |tree| {
            tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .unwrap();
            let held = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .unwrap()
                .agent_id;
            tree.bind_session_locator(&held, "locator").unwrap();
            tree.set_active_turn(&held, true).unwrap();
            tree.bind_attempt_turn(&held, "00000000-0000-0000-0000-000000000001")
                .unwrap();
            let attempt = tree.node(&held).unwrap().attempt_id.clone();
            tree.record_command(AgentCommand::intent(
                "start",
                &held,
                attempt,
                AgentCommandKind::Start,
                "assignment",
                Vec::new(),
            ))
            .unwrap();
            tree.release_attempt_turn(&held).unwrap();
            tree.park_child(&held, Vec::new()).unwrap();
            tree.mark_owner_lost(&held);
            Ok(())
        })
        .block()
        .unwrap();
    let (host, _) = host(&[&id]);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node(&id) });
    attached(&out.take(), 1);
    let row = row_of(&host, &id);
    assert_eq!(row.live_work.children, 1);
    assert_eq!(row.live_work.held_children, 1);
    assert_eq!(row.activity, SessionActivity::Busy);
    connection.leave();
    handed_off(&host);
    crate::topology_runtime::force_release(&id).block();
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

#[test]
fn idle_exit_refuses_unreadable_state_pending_stops_and_a_resident_worker() {
    let (host, fake) = host(&["s1"]);
    assert!(!(host.idle_residents_blocked() || host.durable_idle_exit_blocked()));
    fake.0.unreadable_rows.store(true, Ordering::SeqCst);
    assert!(host.durable_idle_exit_blocked());
    fake.0.unreadable_rows.store(false, Ordering::SeqCst);
    fake.0.pending_stops.store(true, Ordering::SeqCst);
    assert!(host.durable_idle_exit_blocked());
    fake.0.pending_stops.store(false, Ordering::SeqCst);
    let (session, hold) = host.slot("s1").resident().unwrap();
    assert!(!session.ended());
    assert!(host.idle_residents_blocked());
    drop(hold);
    let failures = host.hand_off_all(Duration::from_secs(2));
    assert!(
        failures.is_empty(),
        "idle fixture did not hand off: {failures:?}"
    );
    assert!(!(host.idle_residents_blocked() || host.durable_idle_exit_blocked()));
    assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 0);
}

#[test]
fn idle_exit_keeps_a_dormant_continuation_without_opening_it() {
    let (host, fake) = host(&["s1"]);
    fake.0.rows.lock().unwrap()[0].activity = SessionActivity::Continuing;
    assert!(host.durable_idle_exit_blocked());
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 0);
    fake.0.refuse.lock().unwrap().insert(
        "s1".into(),
        RequestRefusal::SessionUnavailable {
            detail: "startup open failed".into(),
        },
    );
    assert!(host.slot("s1").resident().is_err());
    assert!(matches!(host.slot("s1").peek(), SlotView::Dormant(_)));
    assert!(host.durable_idle_exit_blocked());
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    fake.0.rows.lock().unwrap()[0].activity = SessionActivity::Idle;
    assert!(!(host.idle_residents_blocked() || host.durable_idle_exit_blocked()));
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
}

fn request<W: Write + Clone>(
    connection: &mut HostConnection<Core, W>,
    id: RequestId,
    mut op: Op<Core>,
) -> bool {
    if let Op::Intent {
        intent, client_id, ..
    } = &mut op
        && client_id.is_none()
        && matches!(
            intent,
            SessionIntent::Prompt(_)
                | SessionIntent::FollowUp(_)
                | SessionIntent::Steer(_)
                | SessionIntent::MailTopologyNode { .. }
        )
    {
        *client_id = Some(plakous_host_protocol::wire::ClientDraftId::new());
    }
    let line = encode_line(&ClientFrame::<Core>::Request { id, op }).unwrap();
    connection.frame(Ok(line)).unwrap()
}

fn outcome(frames: &[HostFrame<Core>], id: RequestId) -> Outcome<Core> {
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
    assert_eq!(host.welcome_for(&HostPrincipal::Socket(501)).session, None);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(
        &mut connection,
        1,
        Op::Intent {
            client_id: None,
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

#[test]
fn picker_progress_is_scoped_and_precedes_exactly_one_final_without_opening_a_session() {
    let (host, fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 7, Op::SessionOptions { workspace: None });
    let frames = out.take();
    assert_eq!(frames.len(), 3);
    assert!(matches!(
        frames[0],
        HostFrame::OptionsProgress {
            request_id: 7,
            phase: None,
            row: None,
            ..
        }
    ));
    assert!(matches!(
        frames[1],
        HostFrame::OptionsProgress {
            request_id: 7,
            row: Some(_),
            ..
        }
    ));
    assert!(matches!(
        frames[2],
        HostFrame::Response {
            id: 7,
            outcome: Outcome::Ok(Answer::Options { .. })
        }
    ));
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 0);
}

#[test]
fn directory_filters_precede_the_cap_and_watch_visibility_has_separate_caches() {
    let (host, fake) = host(&[]);
    let mut archived = row("archived", 3);
    archived.archived = true;
    let mut remote = row("remote", 2);
    remote.runner = plakous_host_protocol::wire::RunnerLabel {
        name: "machine".into(),
        runner_id: "pinned-id".into(),
    };
    *fake.0.rows.lock().unwrap() = vec![archived, remote, row("local", 1)];
    fake.0.row_limit.store(1, Ordering::SeqCst);
    let active_out = Out::default();
    let all_out = Out::default();
    let mut active = host.connect(
        active_out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    let mut all = host.connect(
        all_out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(
        &mut active,
        1,
        Op::WatchSessions {
            include_archived: false,
        },
    );
    active.flush().unwrap();
    assert_eq!(directory(&active_out.take())[0].2[0].session, "remote");
    request(
        &mut all,
        2,
        Op::WatchSessions {
            include_archived: true,
        },
    );
    all.flush().unwrap();
    assert_eq!(directory(&all_out.take())[0].2[0].session, "archived");
    for (id, runner, expected) in [(3, None, "local"), (4, Some("pinned-id"), "remote")] {
        request(
            &mut active,
            id,
            Op::ListSessions {
                workspace: Some("/w".into()),
                runner: runner.map(str::to_owned),
                include_archived: false,
            },
        );
        let Outcome::Ok(Answer::Sessions { rows }) = outcome(&active_out.take(), id) else {
            panic!("directory answer")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session, expected);
    }
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 0);
}

fn assert_saved_history(frames: &[HostFrame<Core>]) {
    assert!(frames.iter().any(|frame| {
        let HostFrame::Snapshot(page) = frame else {
            return false;
        };
        page.items.iter().any(|item| {
            matches!(&item.row,
            plakous_host_protocol::views::transcript::TranscriptRow::Notice(notice)
                if notice.text.contains("Saved history only; session unavailable")
                    && notice.text.contains("read-only")
                    && notice.text.contains("refused by the script"))
        })
    }));
    let facts = format!("{frames:?}");
    assert!(facts.contains("AgentAttached(false)"), "{facts}");
    assert!(facts.contains("SetBusy(false)"), "{facts}");
}

#[test]
fn failed_open_keeps_history_readable_during_scan_and_plain_attach_retries_after_stop() {
    let (host, fake) = host(&["s1"]);
    fake.0.stay.store(true, Ordering::SeqCst);
    let refusal = RequestRefusal::SessionUnavailable {
        detail: "inspection interrupted".into(),
    };
    fake.0
        .refuse
        .lock()
        .unwrap()
        .insert("s1".into(), refusal.clone());
    fake.0.rows.lock().unwrap()[0].activity = SessionActivity::Continuing;
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    assert!(row_of(&host, "s1").needs_operator);
    host.slot("s1").set_startup_retrying(true);
    host.changed();
    assert!(!row_of(&host, "s1").needs_operator);
    // The startup scan owns the retry; reconnect does not open in parallel.
    request(&mut connection, 2, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 2);
    connection.flush().unwrap();
    assert_saved_history(&out.take());
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    for (id, op) in [
        (
            3,
            Op::Intent {
                attachment,
                intent: SessionIntent::Prompt("do not run".into()),
                client_id: None,
                blobs: Vec::new(),
            },
        ),
        (
            4,
            Op::Rebind {
                attachment,
                request_id: "blocked".into(),
                cause: "repair".into(),
            },
        ),
    ] {
        request(&mut connection, id, op);
        assert_eq!(outcome(&out.take(), id), Outcome::Refused(refusal.clone()));
    }
    request(
        &mut connection,
        5,
        Op::RetryAttach {
            attachment: attachment + 1,
            request_id: "stale".into(),
        },
    );
    assert_eq!(
        outcome(&out.take(), 5),
        Outcome::Refused(RequestRefusal::NotAttached)
    );
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    request(
        &mut connection,
        6,
        Op::StopSession {
            session: "s1".into(),
        },
    );
    assert_eq!(
        outcome(&out.take(), 6),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    assert_eq!(fake.0.dormant_stops.load(Ordering::SeqCst), 1);
    assert_eq!(row_of(&host, "s1").activity, SessionActivity::Stopped);
    host.slot("s1").set_startup_retrying(false);
    host.changed();
    assert!(!row_of(&host, "s1").needs_operator);
    fake.0.refuse.lock().unwrap().clear();
    request(&mut connection, 8, Op::Attach { node: node("s1") });
    attached(&out.take(), 8);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 2);
    assert!(!row_of(&host, "s1").needs_operator);
    connection.leave();
    handed_off(&host);
}

#[test]
fn an_archived_attach_serves_persisted_facts_without_a_runtime_and_refuses_every_intent() {
    let (host, fake) = host(&["s1"]);
    fake.0.rows.lock().unwrap()[0].archived = true;
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 0);
    connection.flush().unwrap();
    assert!(
        out.take()
            .iter()
            .any(|frame| matches!(frame, HostFrame::Snapshot(_)))
    );
    for (index, intent) in [
        SessionIntent::Prompt("not admitted".into()),
        SessionIntent::Interrupt,
        SessionIntent::Compact,
    ]
    .into_iter()
    .enumerate()
    {
        request(
            &mut connection,
            10 + index as u64,
            Op::Intent {
                attachment,
                intent,
                client_id: None,
                blobs: Vec::new(),
            },
        );
        assert_eq!(
            outcome(&out.take(), 10 + index as u64),
            Outcome::Refused(RequestRefusal::SessionArchived {
                session: "s1".into()
            })
        );
    }
    request(
        &mut connection,
        19,
        Op::StopSession {
            session: "s1".into(),
        },
    );
    assert_eq!(
        outcome(&out.take(), 19),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    request(&mut connection, 20, Op::Detach { attachment });
    assert_eq!(outcome(&out.take(), 20), Outcome::Ok(Answer::Detached));
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 0);
    assert_eq!(fake.0.dormant_stops.load(Ordering::SeqCst), 0);
    assert_eq!(fake.0.stop_intents.load(Ordering::SeqCst), 0);
    assert_eq!(
        fake.0.rows.lock().unwrap()[0].activity,
        SessionActivity::Idle
    );
}

/// An attach of a dormant session's root opens it once and attaches it; a
/// second attach to its tree on the same connection opens nothing more,
/// and neither does another connection's attach while it is resident. A
/// node no session's tree holds is unknown, and opens nothing.
#[test]
fn an_attach_opens_a_dormant_session_once_and_routes_by_tree() {
    let (host, fake) = host(&["s1", "s2"]);
    let out = Out::default();
    let mut first = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut first, 1, Op::Attach { node: node("s1") });
    request(&mut first, 2, Op::Attach { node: node("s1") });
    let frames = out.take();
    attached(&frames, 1);
    attached(&frames, 2);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    let other = Out::default();
    let mut second = host.connect(
        other.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(
        &mut connection,
        1,
        Op::WatchSessions {
            include_archived: false,
        },
    );
    request(
        &mut connection,
        2,
        Op::WatchSessions {
            include_archived: false,
        },
    );
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
    request(
        &mut connection,
        5,
        Op::WatchSessions {
            include_archived: false,
        },
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(
        &mut connection,
        1,
        Op::WatchSessions {
            include_archived: false,
        },
    );
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
    let mut worker = host.connect(
        other.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
            client_id: None,
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    let new = |id| Op::NewSession {
        runner: plakous_host_protocol::wire::RunnerLabel::local(),
        profile: None,
        model: None,
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

#[test]
fn profile_creation_refuses_both_advanced_overrides_before_product_creation() {
    let (host, fake) = host(&[]);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    for model in [false, true] {
        request(
            &mut connection,
            1,
            Op::NewSession {
                runner: plakous_host_protocol::wire::RunnerLabel::local(),
                workspace: "/w".into(),
                profile: Some("claude-default".into()),
                template: (!model).then(|| "template".into()),
                model: model.then(|| plakous_host_protocol::wire::CreationModel {
                    account: "claude".into(),
                    model: "model".into(),
                }),
                request_id: "r1".into(),
            },
        );
        assert!(matches!(
            outcome(&out.take(), 1),
            Outcome::Refused(RequestRefusal::Malformed { .. })
        ));
    }
    assert!(fake.0.creates.lock().unwrap().is_empty());
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
    let mut asker = host.connect(
        one.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    let mut other = host.connect(
        two.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut attached_elsewhere = host.connect(
        other.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut attached_elsewhere, 1, Op::Attach { node: node("s1") });
    attached(&other.take(), 1);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    connection.leave();
    // The release lingers in its worker for a while.
    fake.0.linger.store(1_000, Ordering::SeqCst);
    let first = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || {
            let out = Out::default();
            let mut connection = host.connect(
                out.clone(),
                Face::Remote,
                HostPrincipal::Socket(501),
                || false,
            );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
        let mut connection = host.connect(
            out.clone(),
            Face::Remote,
            HostPrincipal::Socket(501),
            || false,
        );
        request(&mut connection, 1, Op::Attach { node: node("s1") });
        request(
            &mut connection,
            2,
            Op::WatchSessions {
                include_archived: false,
            },
        );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
            client_id: None,
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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

/// Attention is independent of busy activity; neither state hides the other.
#[test]
fn a_busy_session_awaiting_an_approval_independently_needs_an_operator() {
    let (host, fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    prompt(&mut connection, 2, attachment, "approve");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !row_of(&host, "s1").needs_operator {
        assert!(Instant::now() < deadline, "the approval never showed");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(row_of(&host, "s1").activity, SessionActivity::Busy);
    // Attention does not replace activity; busy submissions still queue.
    let intent = match row_of(&host, "s1").activity {
        SessionActivity::Busy => SessionIntent::FollowUp("queued".into()),
        _ => SessionIntent::Prompt("unexpected idle".into()),
    };
    request(
        &mut connection,
        3,
        Op::Intent {
            attachment,
            intent,
            blobs: Vec::new(),
            client_id: None,
        },
    );
    while fake.0.queued.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "attention submitted a new prompt instead of queueing"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        fake.0.images.lock().unwrap().len(),
        1,
        "a second prompt was dispatched"
    );
    connection.leave();
    handed_off(&host);
}

/// Directory stops never attach or open dormant work; resident stops use
/// the same actor even when this connection views another session.
#[test]
fn stop_session_stops_the_attached_session() {
    let (host, fake) = host(&["s1", "s2"]);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    let stop = |session: &str| Op::StopSession {
        session: session.into(),
    };
    request(&mut connection, 1, stop("s1"));
    assert_eq!(
        outcome(&out.take(), 1),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 0);
    request(&mut connection, 2, stop("nope"));
    assert_eq!(
        outcome(&out.take(), 2),
        Outcome::Refused(RequestRefusal::UnknownSession)
    );
    request(&mut connection, 3, Op::Attach { node: node("s1") });
    attached(&out.take(), 3);
    // Attached to s1, a stop of dormant s2 leaves that attachment alone.
    request(&mut connection, 4, stop("s2"));
    assert_eq!(
        outcome(&out.take(), 4),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    request(&mut connection, 5, stop("s1"));
    assert_eq!(
        answer(&mut connection, &out, 5),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
}

/// Flushes `connection` until request `id` is answered, within 10 s.
fn answer(connection: &mut HostConnection<Core, Out>, out: &Out, id: RequestId) -> Outcome<Core> {
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
        let mut connection = host.connect(
            out.clone(),
            Face::Remote,
            HostPrincipal::Socket(501),
            || false,
        );
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
            Outcome::Ok(Answer::Stopped {
                runner_pending: false
            }),
            "then {then}"
        );
        connection.leave();
    }
}

#[test]
fn a_background_open_moves_readonly_links_to_a_fresh_live_attachment() {
    let (host, fake) = host(&["s1"]);
    fake.0.stay.store(true, Ordering::SeqCst);
    fake.0.refuse.lock().unwrap().insert(
        "s1".into(),
        RequestRefusal::SessionUnavailable {
            detail: "offline".into(),
        },
    );
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    connection.flush().unwrap();
    out.take();
    fake.0.refuse.lock().unwrap().clear();
    let (_resident, _hold) = host.slot("s1").resident().unwrap();
    host.changed();
    connection.flush().unwrap();
    assert!(out.take().iter().any(|frame| matches!(frame,
        HostFrame::Moved { attachment: held, node: root, request_id: None }
            if *held == attachment && *root == node("s1"))));
    request(&mut connection, 2, Op::Attach { node: node("s1") });
    attached(&out.take(), 2);
    assert!(connection.node.is_some());
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 2);
    connection.leave();
}

#[test]
fn a_child_attach_preserves_a_failed_opens_diagnostic() {
    let (host, fake) = host(&["s1"]);
    let refusal = RequestRefusal::SessionUnavailable {
        detail: "offline".into(),
    };
    fake.0
        .refuse
        .lock()
        .unwrap()
        .insert("s1".into(), refusal.clone());
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(
        &mut connection,
        1,
        Op::Attach {
            node: NodeAddress {
                tree_id: "s1".into(),
                agent_id: "child".into(),
            },
        },
    );
    assert_eq!(outcome(&out.take(), 1), Outcome::Refused(refusal));
}

/// The settle of a stop whose attachment ended wakes a connection that
/// waits for something to write, though it attaches and watches nothing.
#[test]
fn a_draining_stops_settle_wakes_the_connection() {
    let (host, fake) = host(&["s1"]);
    fake.0.hold_stops.store(true, Ordering::SeqCst);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut releasing = host.connect(
        other.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    release_s1(&mut releasing, 1);
    assert_eq!(outcome(&other.take(), 1), Outcome::Ok(Answer::Released));
}

/// A session request pending when its attachment ends is answered
/// `not_attached`, as on a connection that serves one session.
#[test]
fn a_pending_session_request_is_not_attached_once_its_attachment_ends() {
    let (host, _fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
            let mut connection = host.connect(
                out.clone(),
                Face::Remote,
                HostPrincipal::Socket(501),
                || false,
            );
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
            let mut connection = host.connect(
                out.clone(),
                Face::Remote,
                HostPrincipal::Socket(501),
                || false,
            );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
            client_id: None,
            attachment,
            intent,
            blobs: taken.ids,
        },
    );
    assert_eq!(
        outcome(&out.take(), 2),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    request(
        &mut connection,
        2,
        Op::NewSession {
            runner: plakous_host_protocol::wire::RunnerLabel::local(),
            profile: None,
            model: None,
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

/// A host whose slot opened `s1` and now serves `s9`.
struct Switched {
    host: Arc<SessionHost<Core>>,
    fake: Fake,
    slot: Arc<SessionSlot<Core>>,
    connection: HostConnection<Core, Out>,
    out: Out,
}

/// A service handoff to an independent tree, as for a fork.
fn switched() -> Switched {
    switched_on_node(false)
}

/// A service handoff with either a continued or an independent root node.
fn switched_on_node(same_node: bool) -> Switched {
    let (host, fake) = host(&["s1", "s9"]);
    if same_node {
        fake.0
            .trees
            .lock()
            .unwrap()
            .insert("s9".into(), "s1".into());
        fake.0.rows.lock().unwrap()[1].node = node("s1");
    }
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    let slot = host.slot("s1");
    fake.0
        .successors
        .lock()
        .unwrap()
        .insert("s1".into(), "s9".into());
    fake.0
        .ends
        .lock()
        .unwrap()
        .get("s1")
        .unwrap()
        .send(binding("s9"))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while row_of(&host, "s9").residency != SessionResidency::Resident {
        assert!(Instant::now() < deadline, "the switch never showed");
        std::thread::sleep(Duration::from_millis(5));
    }
    Switched {
        host,
        fake,
        slot,
        connection,
        out,
    }
}

#[test]
fn lifecycle_releases_a_successor_served_through_its_predecessor_slot() {
    for delete in [false, true] {
        let Switched {
            host,
            fake,
            slot,
            mut connection,
            out,
        } = switched();
        let op = if delete {
            Op::DeleteSession {
                session: "s9".into(),
                abandon_children: false,
            }
        } else {
            Op::ArchiveSession {
                session: "s9".into(),
                archived: true,
            }
        };
        request(&mut connection, 2, op);
        assert!(matches!(outcome(&out.take(), 2), Outcome::Ok(_)));
        assert!(!matches!(slot.peek(), SlotView::Resident(_)));
        assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 0);
        assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
        assert_eq!(fake.0.lifecycle_mutations.load(Ordering::SeqCst), 1);
        connection.leave();
        handed_off(&host);
    }
}

#[test]
fn lifecycle_on_a_dormant_predecessor_does_not_release_its_resident_successor() {
    for delete in [false, true] {
        let Switched {
            host,
            fake,
            slot,
            mut connection,
            out,
        } = switched();
        fake.0
            .unrelated_lifecycle_workers
            .store(1, Ordering::SeqCst);
        let op = if delete {
            Op::DeleteSession {
                session: "s1".into(),
                abandon_children: false,
            }
        } else {
            Op::ArchiveSession {
                session: "s1".into(),
                archived: true,
            }
        };
        request(&mut connection, 2, op);
        assert!(matches!(outcome(&out.take(), 2), Outcome::Ok(_)));
        let SlotView::Resident(served) = slot.peek() else {
            panic!("successor released");
        };
        assert_eq!(served.session_id(), "s9");
        assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 1);
        assert_eq!(fake.0.lifecycle_mutations.load(Ordering::SeqCst), 1);
        connection.leave();
        handed_off(&host);
    }
}

fn release_session(connection: &mut HostConnection<Core, Out>, id: RequestId, session: &str) {
    request(
        connection,
        id,
        Op::ReleaseSession {
            session: session.into(),
        },
    );
}

/// `release_session` of the session a switch moved the slot onto releases
/// that session. The slot's `known` id is still the one it opened.
#[test]
fn release_of_the_switched_to_session_releases_it() {
    let Switched {
        host,
        fake,
        slot,
        mut connection,
        out,
    } = switched();
    release_session(&mut connection, 2, "s9");
    assert_eq!(outcome(&out.take(), 2), Outcome::Ok(Answer::Released));
    assert!(
        !matches!(slot.peek(), SlotView::Resident(_)),
        "the switched-to session was still resident"
    );
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    connection.leave();
    handed_off(&host);
}

/// `release_session` of the session a switch left behind does not release
/// the one the slot serves now.
#[test]
fn release_of_the_switched_from_session_keeps_the_other() {
    let Switched {
        host,
        fake,
        slot,
        mut connection,
        out,
    } = switched();
    release_session(&mut connection, 2, "s1");
    assert_eq!(outcome(&out.take(), 2), Outcome::Ok(Answer::Released));
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        match slot.peek() {
            SlotView::Resident(served) => assert_eq!(served.session_id(), "s9"),
            SlotView::Dormant(_) | SlotView::Opening(_) | SlotView::Releasing(_) => {
                panic!("the switched-to session was released");
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    connection.leave();
    handed_off(&host);
}

/// One thread attaches `s1` while its open is gated. The sender opens the
/// gate. The thread detaches once the release does, and returns what it
/// wrote.
fn gated_open() -> (
    Arc<SessionHost<Core>>,
    Fake,
    std::sync::mpsc::Sender<()>,
    std::thread::JoinHandle<Out>,
) {
    let (host, fake) = host(&["s1"]);
    let (open_tx, gate) = std::sync::mpsc::channel();
    *fake.0.gate.lock().unwrap() = Some(gate);
    let attaching = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || {
            let out = Out::default();
            let mut connection = host.connect(
                out.clone(),
                Face::Remote,
                HostPrincipal::Socket(501),
                || false,
            );
            request(&mut connection, 1, Op::Attach { node: node("s1") });
            let deadline = Instant::now() + Duration::from_secs(10);
            while connection.node.is_some() {
                assert!(Instant::now() < deadline, "the attach was not detached");
                connection.flush().unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            connection.leave();
            out
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while fake.0.opens.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the open never began");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(row_of(&host, "s1").residency, SessionResidency::Opening);
    (host, fake, open_tx, attaching)
}

/// `release_session` of a session that is still opening waits for the open.
/// Nothing is answered before it finishes; then the attach is detached and
/// the answer is `released`, from that one open.
#[test]
fn release_of_an_opening_session_waits_then_releases_it() {
    let (host, fake, open_tx, attaching) = gated_open();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let releasing = {
        let host = Arc::clone(&host);
        let entered = Arc::clone(&entered);
        std::thread::spawn(move || {
            let out = Out::default();
            let mut connection = host.connect(
                out.clone(),
                Face::Remote,
                HostPrincipal::Socket(501),
                || false,
            );
            entered.store(true, Ordering::SeqCst);
            release_session(&mut connection, 1, "s1");
            let answer = outcome(&out.take(), 1);
            connection.leave();
            done_tx.send(answer).unwrap();
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !entered.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the release never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(150));
    match done_rx.try_recv() {
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
        other => panic!("answered while the open was still running: {other:?}"),
    }
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    open_tx.send(()).unwrap();
    assert_eq!(
        done_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Outcome::Ok(Answer::Released)
    );
    let frames = attaching.join().unwrap().take();
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, HostFrame::Detached { .. })),
        "the attach was not detached: {frames:?}"
    );
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    releasing.join().unwrap();
    handed_off(&host);
}

/// An open still running after the release join is `resident {work}`, not
/// `released`. Nothing is answered before the join.
#[test]
fn release_of_an_open_past_the_join_answers_resident() {
    let fake = Fake::default();
    *fake.0.rows.lock().unwrap() = vec![row("s1", 0)];
    let host = SessionHost::with_release_bounds(
        fake.clone(),
        Duration::from_secs(10),
        Duration::from_millis(300),
    );
    let (open_tx, gate) = std::sync::mpsc::channel();
    *fake.0.gate.lock().unwrap() = Some(gate);
    let attaching = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || {
            let out = Out::default();
            let mut connection = host.connect(
                out.clone(),
                Face::Remote,
                HostPrincipal::Socket(501),
                || false,
            );
            request(&mut connection, 1, Op::Attach { node: node("s1") });
            connection.leave();
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while row_of(&host, "s1").residency != SessionResidency::Opening {
        assert!(Instant::now() < deadline, "the open never showed");
        std::thread::sleep(Duration::from_millis(5));
    }
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let releasing = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || {
            let out = Out::default();
            let mut connection = host.connect(
                out.clone(),
                Face::Remote,
                HostPrincipal::Socket(501),
                || false,
            );
            let started = Instant::now();
            release_session(&mut connection, 1, "s1");
            let answer = (outcome(&out.take(), 1), started.elapsed());
            connection.leave();
            done_tx.send(answer).unwrap();
        })
    };
    let (answer, elapsed) = done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        elapsed >= Duration::from_millis(250),
        "answered before the join: {elapsed:?}"
    );
    assert_eq!(
        answer,
        Outcome::Ok(Answer::Resident {
            reason: ResidentReason::Work
        })
    );
    assert_eq!(row_of(&host, "s1").residency, SessionResidency::Opening);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    open_tx.send(()).unwrap();
    attaching.join().unwrap();
    releasing.join().unwrap();
    handed_off(&host);
}

/// A release withdrawn after the actor has read it, and before the actor
/// marks, does not begin: the answer is `resident` and the slot stays
/// resident.
#[test]
fn a_release_withdrawn_before_the_actors_mark_stays_resident() {
    let fake = Fake::default();
    *fake.0.rows.lock().unwrap() = vec![row("s1", 0)];
    let host = SessionHost::with_release_wait(fake.clone(), Duration::from_secs(1));
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    let count = fake.0.counts.lock().unwrap().get("s1").unwrap().clone();
    connection.leave();
    assert_eq!(count.get(), 0, "the attach still held the session");
    let (marked_tx, marked_rx) = std::sync::mpsc::channel();
    let waiting = count.clone();
    count.around_next_mark(
        move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while waiting.release_now() {
                assert!(Instant::now() < deadline, "the release was never withdrawn");
                std::thread::sleep(Duration::from_millis(5));
            }
        },
        move |marked| {
            let _ = marked_tx.send(marked);
        },
    );
    let out = Out::default();
    let mut releasing = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    release_session(&mut releasing, 1, "s1");
    assert_eq!(
        outcome(&out.take(), 1),
        Outcome::Ok(Answer::Resident {
            reason: ResidentReason::Work
        })
    );
    assert!(
        !marked_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the actor never decided"),
        "the release began after it was withdrawn"
    );
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        assert!(
            matches!(host.slot("s1").peek(), SlotView::Resident(_)),
            "the session released after the withdrawal"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!count.releasing());
    releasing.leave();
    handed_off(&host);
}

/// A writer that fails once `fail` is set.
#[derive(Clone)]
struct FallibleOut {
    bytes: Arc<Mutex<Vec<u8>>>,
    fail: Arc<std::sync::atomic::AtomicBool>,
}

impl Write for FallibleOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("lost"));
        }
        self.bytes.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A lost output while `release_session` waits withdraws the request
/// before the error propagates, so the session does not release later.
#[test]
fn a_lost_output_during_release_withdraws_the_request() {
    let fake = Fake::default();
    *fake.0.rows.lock().unwrap() = vec![row("s1", 0)];
    fake.0.hold_stops.store(true, Ordering::SeqCst);
    let host = SessionHost::with_release_wait(fake.clone(), Duration::from_secs(2));
    let out = FallibleOut {
        bytes: Arc::default(),
        fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    request(
        &mut connection,
        2,
        Op::StopSession {
            session: "s1".into(),
        },
    );
    let session = Arc::clone(&connection.node.as_ref().unwrap().session);
    let fail = Arc::clone(&out.fail);
    let watcher = {
        let fake = fake.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while session.releases() == 0 {
                assert!(Instant::now() < deadline, "the release never asked");
                std::thread::sleep(Duration::from_millis(5));
            }
            fail.store(true, Ordering::SeqCst);
            fake.0.hold_stops.store(false, Ordering::SeqCst);
        })
    };
    let line = encode_line(&ClientFrame::<Core>::Request {
        id: 3,
        op: Op::ReleaseSession {
            session: "s1".into(),
        },
    })
    .unwrap();
    let error = connection
        .frame(Ok(line))
        .expect_err("the lost output was not reported");
    assert!(
        error.message().contains("stdout failed"),
        "{}",
        error.message()
    );
    watcher.join().unwrap();
    let count = fake.0.counts.lock().unwrap().get("s1").unwrap().clone();
    assert!(
        !count.release_now(),
        "the release request leaked past the lost output"
    );
    connection.leave();
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        assert!(
            matches!(host.slot("s1").peek(), SlotView::Resident(_)),
            "the session released after the lost output"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    handed_off(&host);
}

#[test]
fn a_running_root_turn_on_an_unreachable_placement_needs_the_operator() {
    use plakous_host_protocol::wire::{PlacementReason, RunnerPhase, RunnerStatus};
    let unreachable = |phase| RunnerStatus::Blocked {
        reason: PlacementReason::RunnerUnreachable {
            phase,
            timed_out: false,
        },
    };
    let busy = crate::host_service::KeptActivity {
        busy: true,
        turn_busy: true,
        ..Default::default()
    };
    let idle = crate::host_service::KeptActivity::default();
    // Children alone, live or parked, keep the row busy but are not the
    // root turn waiting on its runner.
    let children_only = crate::host_service::KeptActivity {
        busy: true,
        children: 2,
        held_children: 1,
        ..Default::default()
    };
    assert!(!needs_operator(
        &children_only,
        &unreachable(RunnerPhase::LinkLost)
    ));
    assert!(needs_operator(&busy, &unreachable(RunnerPhase::LinkLost)));
    assert!(needs_operator(&busy, &unreachable(RunnerPhase::Reconcile)));
    assert!(!needs_operator(&idle, &unreachable(RunnerPhase::LinkLost)));
    assert!(!needs_operator(&busy, &RunnerStatus::Ready));
    assert!(!needs_operator(
        &busy,
        &RunnerStatus::Blocked {
            reason: PlacementReason::RunnerNotPaired
        }
    ));
    let awaiting = crate::host_service::KeptActivity {
        awaiting_operator: true,
        ..Default::default()
    };
    assert!(needs_operator(&awaiting, &RunnerStatus::Ready));
}
