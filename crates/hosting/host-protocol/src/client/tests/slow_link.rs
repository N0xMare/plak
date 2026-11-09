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

#[test]
fn slow_link_answer_deadline_withdraws_unwritten_draft_before_pipe_drains() {
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
    *lock(&gate.0) = false;
    gate.1.notify_all();
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
            Some(SendError::OutcomeUnknown(_))
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
    queue.push("sentinel\n".into()).unwrap();
    queue.finish();
    let (mut read, write) = UnixStream::pair().unwrap();
    queue.run(Box::new(write)).unwrap();
    let mut bytes = String::new();
    read.read_to_string(&mut bytes).unwrap();
    assert_eq!(bytes, "sentinel\n");
    assert!(expired.withdraw());
}
