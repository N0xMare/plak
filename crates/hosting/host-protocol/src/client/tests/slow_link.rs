use super::*;

struct GatedWriter {
    stream: UnixStream,
    gate: Arc<(Mutex<bool>, Condvar)>,
    entered: mpsc::Sender<()>,
}

impl Write for GatedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let (gate, changed) = &*self.gate;
        let mut blocked = lock(gate);
        if *blocked {
            self.entered.send(()).unwrap();
            while *blocked {
                blocked = changed.wait(blocked).unwrap();
            }
        }
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

type Gate = Arc<(Mutex<bool>, Condvar)>;
type Gated = (
    Host,
    WireSender<Core>,
    WireReceiver<Core>,
    Gate,
    mpsc::Receiver<()>,
);

fn gated() -> Gated {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, writing) = mpsc::channel();
    let (host_socket, client_socket) = UnixStream::pair().unwrap();
    let mut host = Host {
        lines: BufReader::new(host_socket.try_clone().unwrap()),
        stream: host_socket,
    };
    host.stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let shutdown = client_socket.try_clone().unwrap();
    let transport = Transport::new(
        client_socket.try_clone().unwrap(),
        GatedWriter {
            stream: client_socket,
            gate: Arc::clone(&gate),
            entered,
        },
    )
    .with_write_shutdown(move || shutdown.shutdown(std::net::Shutdown::Write));
    let connecting = thread::spawn(move || connect::<Core>(transport, hello()).unwrap());
    assert!(matches!(host.read(), ClientFrame::Hello(_)));
    host.send(&HostFrame::Welcome(welcome()));
    let (sender, receiver, _) = connecting.join().unwrap();
    let (attach, _) = host.request();
    host.send(&HostFrame::Response {
        id: attach,
        outcome: Outcome::Ok(Answer::Attached { attachment: 1 }),
    });
    host.send(&event(1, false));
    receiver.recv_timeout(Duration::from_secs(3)).unwrap();
    (host, sender, receiver, gate, writing)
}

fn release(gate: &Gate) {
    *lock(&gate.0) = false;
    gate.1.notify_all();
}

#[test]
fn last_handle_drop_is_bounded_when_the_writer_is_stalled() {
    let (mut host, sender, receiver, gate, writing) = gated();
    *lock(&gate.0) = true;
    sender.request_operation(Op::Ping).unwrap();
    writing.recv_timeout(Duration::from_secs(3)).unwrap();
    let (report, complete) = mpsc::channel();
    let dropping = thread::spawn(move || {
        let started = Instant::now();
        drop((sender, receiver));
        report.send(started.elapsed()).unwrap();
    });
    let elapsed = complete
        .recv_timeout(DROP_DRAIN_TIMEOUT + Duration::from_secs(1))
        .unwrap();
    assert!(elapsed >= DROP_DRAIN_TIMEOUT);
    assert!(elapsed < DROP_DRAIN_TIMEOUT + Duration::from_secs(1));
    release(&gate);
    dropping.join().unwrap();
    assert_eq!(host.request().1, Op::Ping);
    assert_eq!(host.read(), ClientFrame::Bye);
    assert_eq!(host.lines.read_line(&mut String::new()).unwrap(), 0);
}

#[test]
fn goodbye_preserves_unwritten_and_started_write_dispositions() {
    let (mut host, sender, receiver, gate, writing) = gated();
    *lock(&gate.0) = true;
    sender
        .send(SessionIntent::Prompt("started".into()))
        .unwrap();
    writing.recv_timeout(Duration::from_secs(3)).unwrap();
    sender
        .send(SessionIntent::Prompt("unwritten".into()))
        .unwrap();
    sender.goodbye();
    for sent in lock(&sender.connection.inner.state).sent.values_mut() {
        sent.at -= ANSWER_DEADLINE;
    }
    assert!(
        matches!(receiver.try_unsent(), Some(SendError::OutcomeUnknown {
        intent: SessionIntent::Prompt(ref draft), client_id: Some(_), host_generation: 1,
    }) if draft.text == "started")
    );
    assert!(matches!(receiver.try_unsent(), Some(SendError::Refused(
        IntentSendError::Disconnected(SessionIntent::Prompt(ref draft)))) if draft.text == "unwritten"));
    release(&gate);
    assert!(matches!(host.request().1, Op::Intent {
        intent: SessionIntent::Prompt(ref draft), .. } if draft.text == "started"));
    assert_eq!(host.read(), ClientFrame::Bye);
    assert_eq!(host.lines.read_line(&mut String::new()).unwrap(), 0);
    host.send(&HostFrame::Bye);
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(3)),
        Err(RecvTimeoutError::Disconnected)
    ));
    assert!(receiver.try_unsent().is_none());
}

#[test]
fn goodbye_has_a_terminal_slot_even_when_controls_fill_the_writer() {
    let (mut host, sender, receiver, gate, writing) = gated();
    let _last = full_controls(&sender, &gate, &writing);
    receiver.goodbye();
    sender.goodbye();
    release(&gate);
    for _ in 0..writer::MAX_FRAMES {
        assert_eq!(host.request().1, Op::Ping);
    }
    assert_eq!(host.read(), ClientFrame::Bye);
    assert_eq!(host.lines.read_line(&mut String::new()).unwrap(), 0);
}

fn full_controls(
    sender: &WireSender<Core>,
    gate: &Gate,
    writing: &mpsc::Receiver<()>,
) -> writer::Ticket {
    *lock(&gate.0) = true;
    sender.request_operation(Op::Ping).unwrap();
    writing.recv_timeout(Duration::from_secs(3)).unwrap();
    for _ in 1..writer::MAX_FRAMES - 1 {
        sender.request_operation(Op::Ping).unwrap();
    }
    sender
        .connection
        .inner
        .outbox
        .push(
            encode_line(&ClientFrame::<Core>::Request {
                id: u64::MAX,
                op: Op::Ping,
            })
            .unwrap(),
        )
        .unwrap()
}

fn pending_control(sender: &WireSender<Core>, expected: impl Fn(&Control) -> bool) {
    let inner = &sender.connection.inner;
    let until = Instant::now() + Duration::from_secs(3);
    let mut state = lock(&inner.state);
    while !state.control_retry.as_ref().is_some_and(&expected) {
        let left = until
            .checked_duration_since(Instant::now())
            .expect("control retry not recorded");
        let (next, wait) = inner.changed.wait_timeout(state, left).unwrap();
        assert!(
            !wait.timed_out(),
            "control state change did not wake waiters"
        );
        state = next;
    }
}

#[test]
fn slow_link_deliver_overflow_replays_detach_before_attach_without_leaking() {
    let (mut host, sender, receiver, gate, writing) = gated();
    let _last = full_controls(&sender, &gate, &writing);
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
    pending_control(&sender, |control| matches!(control, Control::Detach(1)));
    assert_eq!(receiver.ended(), None);
    assert!(receiver.transcript_resnapshot_required_for(&TranscriptScope::Root));
    receiver.try_iter().for_each(drop);
    release(&gate);
    for _ in 0..writer::MAX_FRAMES {
        assert_eq!(host.request().1, Op::Ping);
    }
    assert_eq!(
        host.request().1,
        Op::Detach { attachment: 1 },
        "release the live host attachment first"
    );
    let (id, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t1") });
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment: 2 }),
    });
    host.send(&event(2, true));
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
        SessionObservation::SetBusy(true)
    );
    let sentinel = sender.request_operation(Op::Ping).unwrap();
    assert_eq!(
        host.request().0,
        sentinel,
        "no extra attachment after recovery"
    );
}

#[test]
fn slow_link_transcript_overflow_replays_open_on_current_attachment() {
    let (mut host, sender, receiver, gate, writing) = gated();
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
    assert!(sender.settle(Duration::from_secs(3)));
    host.send(&HostFrame::ResyncRequired { attachment: 1 });
    let (attach, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t1") });
    let _last = full_controls(&sender, &gate, &writing);
    host.send(&HostFrame::Response {
        id: attach,
        outcome: Outcome::Ok(Answer::Attached { attachment: 2 }),
    });
    pending_control(
        &sender,
        |control| matches!(control, Control::OpenTranscript(agent) if agent == "a1"),
    );
    release(&gate);
    for _ in 0..writer::MAX_FRAMES {
        assert_eq!(host.request().1, Op::Ping);
    }
    assert_eq!(
        host.request().1,
        Op::Intent {
            client_id: None,
            attachment: 2,
            intent: SessionIntent::OpenAgentTranscript {
                agent_id: "a1".into()
            },
            blobs: Vec::new(),
        }
    );
    let sentinel = sender.request_operation(Op::Ping).unwrap();
    assert_eq!(
        host.request().0,
        sentinel,
        "opening a transcript must not create another attachment"
    );
    assert!(receiver.try_control_error().is_none());
}

#[test]
fn slow_link_completed_selection_supersedes_pending_transcript_recovery() {
    let (mut host, sender, receiver, gate, writing) = gated();
    sender
        .send(SessionIntent::OpenAgentTranscript {
            agent_id: "a1".into(),
        })
        .unwrap();
    let (open, _) = host.request();
    host.send(&HostFrame::Response {
        id: open,
        outcome: Outcome::Ok(Answer::Accepted { duplicate: false }),
    });
    assert!(sender.settle(Duration::from_secs(3)));
    host.send(&HostFrame::ResyncRequired { attachment: 1 });
    let (recovery, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t1") });
    let selection = sender.attach_to(node("chosen")).unwrap();
    assert_eq!(
        host.request(),
        (
            selection,
            Op::Attach {
                node: node("chosen")
            }
        )
    );
    let _last = full_controls(&sender, &gate, &writing);
    host.send(&HostFrame::Response {
        id: recovery,
        outcome: Outcome::Ok(Answer::Attached { attachment: 2 }),
    });
    pending_control(
        &sender,
        |control| matches!(control, Control::OpenTranscript(agent) if agent == "a1"),
    );
    host.send(&HostFrame::Response {
        id: selection,
        outcome: Outcome::Ok(Answer::Attached { attachment: 3 }),
    });
    assert_eq!(
        receiver.recv_response(Duration::from_secs(3)).unwrap().0,
        selection
    );
    release(&gate);
    for _ in 0..writer::MAX_FRAMES {
        assert_eq!(host.request().1, Op::Ping);
    }
    let sentinel = sender.request_operation(Op::Ping).unwrap();
    assert_eq!(
        host.request(),
        (sentinel, Op::Ping),
        "completed selection must not reopen the old transcript or attach again"
    );
    assert!(receiver.try_control_error().is_none());
    assert_eq!(receiver.ended(), None);
}

#[test]
fn slow_link_overflow_notifies_waiters_and_queued_selection_clears_retry_state() {
    let (mut host, sender, receiver, gate, writing) = gated();
    let last = full_controls(&sender, &gate, &writing);
    let inner = Arc::clone(&sender.connection.inner);
    let (ready, registered) = mpsc::channel();
    let watch = thread::spawn(move || {
        let mut state = lock(&inner.state);
        ready.send(()).unwrap();
        while state.control_retry.is_none() {
            let (next, wait) = inner
                .changed
                .wait_timeout(state, Duration::from_secs(3))
                .unwrap();
            assert!(!wait.timed_out(), "WouldBlock must notify changed waiters");
            state = next;
        }
    });
    registered.recv_timeout(Duration::from_secs(3)).unwrap();
    host.send(&HostFrame::ResyncRequired { attachment: 1 });
    watch.join().unwrap();
    assert!(lock(&sender.connection.inner.state).control_error.is_some());
    assert!(last.withdraw());
    let selection = sender.attach_to(node("chosen")).unwrap();
    {
        let state = lock(&sender.connection.inner.state);
        assert!(
            state.control_retry.is_none(),
            "any queued attach supersedes the old attach retry"
        );
        assert!(
            state.control_error.is_none(),
            "queued attach clears the old overflow reason"
        );
    }
    release(&gate);
    for _ in 0..writer::MAX_FRAMES - 1 {
        assert_eq!(host.request().1, Op::Ping);
    }
    assert_eq!(
        host.request(),
        (
            selection,
            Op::Attach {
                node: node("chosen")
            }
        )
    );
    host.send(&HostFrame::Response {
        id: selection,
        outcome: Outcome::Ok(Answer::Attached { attachment: 2 }),
    });
    assert_eq!(
        receiver.recv_response(Duration::from_secs(3)).unwrap().0,
        selection
    );
    let sentinel = sender.request_operation(Op::Ping).unwrap();
    assert_eq!(
        host.request().0,
        sentinel,
        "stale retry must not leak another attachment"
    );
}

#[test]
fn slow_link_attached_intent_is_serialized_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SERIALIZATIONS: AtomicUsize = AtomicUsize::new(0);
    #[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
    struct Counted(u8);
    impl Serialize for Counted {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            SERIALIZATIONS.fetch_add(1, Ordering::SeqCst);
            serializer.serialize_u8(self.0)
        }
    }
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Counting;
    impl ProtocolExtension for Counting {
        type Intent = Counted;
        type Observation = crate::Never;
        type HostRequest = crate::Never;
        type HostResponse = crate::Never;
        type HostStreamEvent = crate::Never;
        type WelcomeExtension = Option<crate::Never>;
    }
    let inner = Inner::<Counting>::new();
    lock(&inner.state).attachment = Some(1);
    let sender = WireSender {
        connection: Arc::new(Connection {
            inner: Arc::clone(&inner),
        }),
    };
    sender.send(SessionIntent::Extension(Counted(7))).unwrap();
    assert_eq!(SERIALIZATIONS.load(Ordering::SeqCst), 1);
    inner.outbox.finish();
    let (read, write) = UnixStream::pair().unwrap();
    inner.outbox.run(Box::new(write), || {}).unwrap();
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).unwrap();
    assert!(matches!(
        decode_line::<ClientFrame<Counting>>(&line).unwrap(),
        ClientFrame::Request {
            op: Op::Intent {
                intent: SessionIntent::Extension(Counted(7)),
                ..
            },
            ..
        }
    ));
}

#[test]
fn slow_link_answer_deadline_withdraws_unwritten_draft_before_pipe_drains() {
    let (mut host, sender, receiver, gate, writing) = gated();
    *lock(&gate.0) = true;
    let ping = sender.request_operation(Op::Ping).unwrap();
    writing.recv_timeout(Duration::from_secs(3)).unwrap();
    let intent = SessionIntent::Prompt(crate::Draft::from("one admission"));
    sender.send(intent.clone()).unwrap();
    for sent in lock(&sender.connection.inner.state).sent.values_mut() {
        sent.at -= ANSWER_DEADLINE;
    }
    assert!(
        matches!(receiver.try_unsent(), Some(SendError::Refused(IntentSendError::Disconnected(ref draft))) if *draft == intent)
    );
    release(&gate);
    assert_eq!(host.request().0, ping);
    sender.send(intent.clone()).unwrap();
    let (_, op) = host.request();
    assert!(matches!(op, Op::Intent { intent: ref draft, .. } if *draft == intent));
    let sentinel = sender.request_operation(Op::Ping).unwrap();
    assert_eq!(
        host.request().0,
        sentinel,
        "withdrawn line must never land after draining"
    );
}

#[test]
fn slow_link_explicit_same_id_check_reports_duplicate_or_pending_and_fences_generation() {
    for pending in [false, true] {
        let (mut host, sender, receiver) = attached(1);
        host.send(&event(1, false));
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
            SessionObservation::SetBusy(false)
        );
        let client_id = crate::wire::ClientDraftId::new();
        let outcome = if pending {
            Outcome::Refused(RequestRefusal::DraftPending { client_id })
        } else {
            Outcome::Ok(Answer::Accepted { duplicate: true })
        };
        let draft = SessionIntent::Prompt(crate::Draft::from("same payload"));
        let first = sender.send_draft(1, client_id, draft.clone()).unwrap();
        let (_, original) = host.request();
        for sent in lock(&sender.connection.inner.state).sent.values_mut() {
            sent.at -= ANSWER_DEADLINE;
        }
        assert!(matches!(
            receiver.try_unsent(),
            Some(SendError::OutcomeUnknown { client_id: Some(id), host_generation: 1, .. }) if id == client_id
        ));
        let retry = sender.send_draft(1, client_id, draft.clone()).unwrap();
        assert_ne!(first, retry);
        assert_eq!(
            host.request().1,
            original,
            "same id, target and payload; no new admission identity"
        );
        host.send(&HostFrame::Response {
            id: retry,
            outcome: outcome.clone(),
        });
        assert_eq!(
            receiver.recv_response(Duration::from_secs(3)).unwrap(),
            (retry, outcome)
        );
        assert!(sender.send_draft(2, client_id, draft).is_none());
        let sentinel = sender.request_operation(Op::Ping).unwrap();
        assert_eq!(host.request().0, sentinel);
    }
}

#[test]
fn slow_link_writer_reserves_complete_batches_under_byte_and_frame_bounds() {
    let queue = Arc::new(writer::Queue::default());
    let batch = "{}\n".repeat(writer::MAX_FRAMES);
    let reserved = queue.push(batch).unwrap();
    assert_eq!(
        queue.push("{}\n{}\n".into()).err().unwrap().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(reserved.withdraw());
    let reserved = queue
        .push(format!("{}\n", "x".repeat(writer::MAX_BYTES - 1)))
        .unwrap();
    assert_eq!(
        queue.push("{}\n".into()).err().unwrap().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(reserved.withdraw());
    assert!(queue.push("{}\n{}\n".into()).is_ok());
}

#[test]
fn slow_link_writer_withdraws_expired_batch_even_before_receiver_polls() {
    let queue = Arc::new(writer::Queue::default());
    let expired = queue
        .reserve("must not land\n".into(), Some(Instant::now()))
        .unwrap();
    let later = queue
        .reserve(
            "later must not overtake\n".into(),
            Some(Instant::now() + ANSWER_DEADLINE),
        )
        .unwrap();
    queue.push("sentinel\n".into()).unwrap();
    queue.finish();
    let (mut read, write) = UnixStream::pair().unwrap();
    queue.run(Box::new(write), || {}).unwrap();
    let mut bytes = String::new();
    read.read_to_string(&mut bytes).unwrap();
    assert_eq!(bytes, "sentinel\n");
    assert!(expired.withdraw());
    assert!(later.withdrawn());
}

#[test]
fn slow_link_control_headroom_keeps_resync_alive_at_intent_capacity() {
    let (mut host, sender, receiver, gate, writing) = gated();
    *lock(&gate.0) = true;
    sender.send(SessionIntent::Interrupt).unwrap();
    writing.recv_timeout(Duration::from_secs(3)).unwrap();
    for _ in 1..writer::INTENT_FRAMES {
        sender.send(SessionIntent::Quit).unwrap();
    }
    let draft_id = crate::wire::ClientDraftId::new();
    assert!(
        sender
            .send_draft(1, draft_id, SessionIntent::Prompt("full".into()))
            .is_none()
    );
    assert!(
        receiver.try_unsent().is_none(),
        "report sends have one unsent signal, not two"
    );
    host.send(&HostFrame::ResyncRequired { attachment: 1 });
    let until = Instant::now() + Duration::from_secs(3);
    while lock(&sender.connection.inner.state).attaching.is_none() {
        assert!(Instant::now() < until);
        thread::yield_now();
    }
    assert_eq!(receiver.ended(), None);
    assert!(receiver.try_control_error().is_none());
    release(&gate);
    for _ in 0..writer::INTENT_FRAMES {
        assert!(matches!(host.request().1, Op::Intent { .. }));
    }
    let (id, op) = host.request();
    assert_eq!(op, Op::Attach { node: node("t1") });
    host.send(&HostFrame::Response {
        id,
        outcome: Outcome::Ok(Answer::Attached { attachment: 2 }),
    });
    host.send(&event(2, true));
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
        SessionObservation::SetBusy(true)
    );
}

#[test]
fn slow_link_control_overflow_is_distinct_and_retries_without_disconnect() {
    let (mut host, sender, receiver, gate, writing) = gated();
    *lock(&gate.0) = true;
    sender.request_operation(Op::Ping).unwrap();
    writing.recv_timeout(Duration::from_secs(3)).unwrap();
    for _ in 1..writer::MAX_FRAMES {
        sender.request_operation(Op::Ping).unwrap();
    }
    let error = sender.request_operation(Op::Ping).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(error.to_string().contains("control"));
    host.send(&HostFrame::ResyncRequired { attachment: 1 });
    let until = Instant::now() + Duration::from_secs(3);
    while receiver.try_control_error().is_none() {
        assert!(Instant::now() < until);
        thread::yield_now();
    }
    assert_eq!(receiver.ended(), None);
    release(&gate);
    for _ in 0..writer::MAX_FRAMES {
        assert_eq!(host.request().1, Op::Ping);
    }
    assert_eq!(host.request().1, Op::Attach { node: node("t1") });
}

#[test]
fn slow_link_expiry_withdraws_entire_queued_intent_suffix_in_fifo_order() {
    let (mut host, sender, receiver, gate, writing) = gated();
    *lock(&gate.0) = true;
    let ping = sender.request_operation(Op::Ping).unwrap();
    writing.recv_timeout(Duration::from_secs(3)).unwrap();
    for text in ["A", "B", "C"] {
        sender.send(SessionIntent::Prompt(text.into())).unwrap();
    }
    lock(&sender.connection.inner.state)
        .sent
        .first_entry()
        .unwrap()
        .get_mut()
        .at -= ANSWER_DEADLINE;
    for text in ["A", "B", "C"] {
        let error = receiver
            .try_unsent()
            .expect("whole suffix returned without waiting for later deadlines");
        assert!(
            matches!(error, SendError::Refused(IntentSendError::Disconnected(SessionIntent::Prompt(ref draft))) if draft.text == text)
        );
    }
    assert!(receiver.try_unsent().is_none());
    release(&gate);
    assert_eq!(host.request().0, ping);
    let sentinel = sender.request_operation(Op::Ping).unwrap();
    assert_eq!(host.request().0, sentinel);
}

#[test]
fn slow_link_mid_write_expiry_keeps_identity_and_later_line_intact() {
    let (mut host, sender, receiver, gate, writing) = gated();
    *lock(&gate.0) = true;
    let draft = SessionIntent::Prompt("writing".into());
    sender.send(draft.clone()).unwrap();
    writing.recv_timeout(Duration::from_secs(3)).unwrap();
    sender.send(SessionIntent::Interrupt).unwrap();
    lock(&sender.connection.inner.state)
        .sent
        .first_entry()
        .unwrap()
        .get_mut()
        .at -= ANSWER_DEADLINE;
    let Some(SendError::OutcomeUnknown {
        intent,
        client_id: Some(client_id),
        host_generation,
    }) = receiver.try_unsent()
    else {
        panic!("writing batch is unknown, not never-sent");
    };
    assert_eq!(intent, draft);
    assert!(receiver.try_unsent().is_none());
    release(&gate);
    assert!(matches!(host.request().1, Op::Intent { client_id: Some(id), .. } if id == client_id));
    assert!(matches!(
        host.request().1,
        Op::Intent {
            intent: SessionIntent::Interrupt,
            ..
        }
    ));
    let check = sender
        .send_draft(host_generation, client_id, intent)
        .unwrap();
    assert_eq!(host.request().0, check);
    host.send(&HostFrame::Response {
        id: check,
        outcome: Outcome::Ok(Answer::Accepted { duplicate: true }),
    });
    assert_eq!(
        receiver.recv_response(Duration::from_secs(3)).unwrap(),
        (check, Outcome::Ok(Answer::Accepted { duplicate: true }))
    );
}

#[test]
fn slow_link_byte_headroom_is_available_only_to_controls() {
    let queue = Arc::new(writer::Queue::default());
    queue
        .reserve(
            format!("{}\n", "x".repeat(writer::INTENT_BYTES - 1)),
            Some(Instant::now() + ANSWER_DEADLINE),
        )
        .unwrap();
    assert_eq!(
        queue
            .reserve("x\n".into(), Some(Instant::now() + ANSWER_DEADLINE))
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(
        queue
            .push(format!(
                "{}\n",
                "x".repeat(writer::MAX_BYTES - writer::INTENT_BYTES - 1)
            ))
            .is_ok()
    );
}
