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
    let transport = Transport::new(
        client_socket.try_clone().unwrap(),
        GatedWriter {
            stream: client_socket,
            gate: Arc::clone(&gate),
            entered,
        },
    );
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
