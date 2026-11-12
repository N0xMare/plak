//! Upgrade callers use one paused-host barrier, including zero remaining budget.

use super::*;

struct MailboxHold(Fake);

impl Drop for MailboxHold {
    fn drop(&mut self) {
        self.0.0.hold_mailbox.store(false, Ordering::SeqCst);
    }
}

#[test]
fn draft_verdict_barrier_requires_its_own_pause_and_empty_zero_budget_succeeds() {
    let (first, _) = host(&[]);
    let (second, _) = host(&[]);
    let pause = first.pause_admissions().unwrap();
    assert!(
        second
            .resolve_paused_drafts(&pause, Instant::now())
            .is_err()
    );
    first.resolve_paused_drafts(&pause, Instant::now()).unwrap();
    drop(pause);
    assert!(first.pause_admissions().is_ok());
}

#[test]
fn draft_verdict_barrier_withdraws_under_pause_and_joins_the_real_connection_write() {
    let (host, fake) = host(&["s1"]);
    fake.0.hold_mailbox.store(true, Ordering::SeqCst);
    let hold = MailboxHold(fake.clone());
    let out = Out::default();
    let mut connection = host
        .connect(
            out.clone(),
            Face::Remote,
            HostPrincipal::Socket(501),
            || false,
        )
        .buffered_output();
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = match outcome(&out.take(), 1) {
        Outcome::Ok(Answer::Attached { attachment }) => attachment,
        other => panic!("{other:?}"),
    };
    let client_id = plakous_host_protocol::wire::ClientDraftId::new();
    request(
        &mut connection,
        2,
        Op::Intent {
            attachment,
            intent: SessionIntent::FollowUp(plakous_host_protocol::Draft::from("unclaimed input")),
            client_id: Some(client_id),
            blobs: Vec::new(),
        },
    );
    assert_eq!(
        outcome(&out.take(), 2),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    let pause = host.pause_admissions().unwrap();
    assert!(host.resolve_paused_drafts(&pause, Instant::now()).is_err());
    connection.flush().unwrap();
    assert!(
        host.resolve_paused_drafts(&pause, Instant::now()).is_err(),
        "buffered output is not a transport send"
    );
    let frames = out.take();
    assert!(frames.iter().any(|frame| matches!(frame, HostFrame::Event {
        client_id: Some(id), observation: SessionObservation::DraftRejected(_), ..
    } if *id == client_id)));
    connection.output_sent();
    host.resolve_paused_drafts(&pause, Instant::now()).unwrap();
    assert_eq!(
        fake.0.queued.load(Ordering::SeqCst),
        0,
        "mailbox ack was not durable admission"
    );
    drop(hold);
    drop(pause);
    let failures = host.hand_off_all(Duration::from_secs(2));
    assert_eq!(failures.len(), 0, "{failures:?}");
}
