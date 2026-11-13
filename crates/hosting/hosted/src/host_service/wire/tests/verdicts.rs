//! A verdict joins durable observation and an actual wire write, not mailbox ack.

use super::*;
use crate::host_service::drafts::{DraftRegistry, DraftReservation};
use plakous_host_protocol::wire::ClientDraftId;
use plakous_host_protocol::{Core, IntentReceiver, intent_channel};
use std::sync::atomic::AtomicBool;

fn setup() -> (
    WireConnection<Core, Vec<u8>>,
    IntentReceiver<Core>,
    DraftRegistry,
) {
    let (sender, receiver) = intent_channel();
    let service = HostService::new(sender.clone(), root(), ATTACHMENT_CAPACITY);
    let attachment = service.attach(&root(), Face::Remote).unwrap();
    while receiver.try_recv().is_ok() {}
    let mut wire = WireConnection::on_service(service, Vec::new(), sender, Leave::Detach);
    wire.attachment = Some(attachment);
    wire.flush().unwrap();
    (wire, receiver, DraftRegistry::default())
}

fn reserve<W: Write>(
    wire: &WireConnection<Core, W>,
    registry: &DraftRegistry,
    text: &str,
) -> ClientDraftId {
    let id = ClientDraftId::new();
    let DraftReservation::Pending(pending) = registry.reserve(id, [1; 32]).unwrap() else {
        panic!("new id")
    };
    let attachment = wire.attachment.unwrap();
    let intent = SessionIntent::Prompt(Draft::from(text));
    wire.service
        .begin_wire_draft(id, pending, attachment, intent.clone());
    wire.service
        .intent_correlated(attachment, intent, Some(&id.to_string()))
        .unwrap();
    id
}

#[test]
fn draft_verdict_barrier_requester_loss_withdraws_the_wait_not_the_admission_verdict() {
    let (mut wire, receiver, registry) = setup();
    reserve(&wire, &registry, "already claimed");
    let delivery = receiver.try_recv().unwrap();
    let cancelled = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let waiting = scope
            .spawn(|| registry.wait_resolved(Instant::now() + Duration::from_secs(30), &cancelled));
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        assert!(waiting.join().unwrap().unwrap_err().contains("cancelled"));
    });
    wire.service.observe(delivery.started().unwrap().into());
    wire.service
        .observe(SessionObservation::DraftAccepted(SessionIntent::Prompt(
            Draft::from("already claimed"),
        )));
    wire.flush().unwrap();
    registry
        .wait_resolved(Instant::now(), &AtomicBool::new(false))
        .unwrap();
}

#[test]
fn draft_barrier_waits_while_the_verdict_write_is_in_flight() {
    use std::io::{self, Write};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    struct GatedWriter {
        armed: Arc<AtomicBool>,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Write for GatedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if String::from_utf8_lossy(bytes).contains("\"draft_rejected\"")
                && self.armed.swap(false, Ordering::SeqCst)
            {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let armed = Arc::new(AtomicBool::new(false));
    let (entered, entering) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let (sender, receiver) = intent_channel();
    let service = HostService::new(sender.clone(), root(), ATTACHMENT_CAPACITY);
    let attachment = service.attach(&root(), Face::Remote).unwrap();
    while receiver.try_recv().is_ok() {}
    let mut wire = WireConnection::on_service(
        service,
        GatedWriter {
            armed: Arc::clone(&armed),
            entered,
            release: released,
        },
        sender,
        Leave::Detach,
    );
    wire.attachment = Some(attachment);
    wire.flush().unwrap();
    let registry = DraftRegistry::default();
    reserve(&wire, &registry, "pending write");
    wire.service.reject_queued_wire_drafts();
    armed.store(true, Ordering::SeqCst);
    let worker = std::thread::spawn(move || wire.flush());
    entering.recv_timeout(Duration::from_secs(5)).unwrap();
    let pending = registry.wait_resolved(Instant::now(), &AtomicBool::new(false));
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert!(pending.is_err(), "taking an output is not writing it");
    registry
        .wait_resolved(Instant::now(), &AtomicBool::new(false))
        .unwrap();
}

#[test]
fn draft_barrier_withdraws_only_unclaimed_inputs_and_joins_verdict_writes() {
    let (mut wire, receiver, registry) = setup();
    let accepted = reserve(&wire, &registry, "accepted first");
    let delivery = receiver.try_recv().unwrap();
    wire.service.observe(delivery.started().unwrap().into());
    wire.service
        .observe(SessionObservation::DraftAccepted(SessionIntent::Prompt(
            Draft::from("accepted first"),
        )));
    let rejected = reserve(&wire, &registry, "not yet claimed");
    wire.service.reject_queued_wire_drafts();
    assert!(
        receiver.try_recv().is_err(),
        "withdrawn input must never reach the actor"
    );
    assert!(
        registry
            .wait_resolved(Instant::now(), &AtomicBool::new(false))
            .is_err(),
        "not written yet"
    );
    wire.flush().unwrap();
    registry
        .wait_resolved(Instant::now(), &AtomicBool::new(false))
        .unwrap();
    let verdicts: Vec<_> = host_frames(&wire.stdout)
        .into_iter()
        .filter_map(|frame| match frame {
            HostFrame::Event {
                client_id,
                observation: SessionObservation::DraftAccepted(_),
                ..
            } => Some((client_id, true)),
            HostFrame::Event {
                client_id,
                observation: SessionObservation::DraftRejected(_),
                ..
            } => Some((client_id, false)),
            _ => None,
        })
        .collect();
    assert_eq!(verdicts, [(Some(accepted), true), (Some(rejected), false)]);
    assert!(matches!(
        registry.reserve(accepted, [1; 32]),
        Ok(DraftReservation::Duplicate)
    ));
}

#[test]
fn draft_barrier_disconnect_is_not_a_verdict_for_an_already_claimed_input() {
    let (wire, receiver, registry) = setup();
    let id = reserve(&wire, &registry, "in flight");
    let delivery = receiver.try_recv().unwrap();
    let service = wire.service.clone();
    service.detach(wire.attachment.unwrap()).unwrap();
    drop(wire);
    service.reject_queued_wire_drafts();
    assert!(
        registry
            .wait_resolved(Instant::now(), &AtomicBool::new(false))
            .is_err(),
        "a lost reply proves no admission outcome"
    );
    service.observe(delivery.started().unwrap().into());
    service.observe(SessionObservation::DraftAccepted(SessionIntent::Prompt(
        Draft::from("in flight"),
    )));
    registry
        .wait_resolved(Instant::now(), &AtomicBool::new(false))
        .unwrap();
    assert!(matches!(
        registry.reserve(id, [1; 32]),
        Ok(DraftReservation::Duplicate)
    ));
    assert!(
        matches!(
            DraftRegistry::default().reserve(id, [1; 32]),
            Ok(DraftReservation::Pending(_))
        ),
        "a new generation carries no registry"
    );
}

#[test]
fn draft_barrier_uses_explicit_rejection_id_not_another_child_mail_window() {
    let (mut wire, _receiver, registry) = setup();
    let id = reserve(&wire, &registry, "child input");
    wire.mails.insert(id.to_string(), 99);
    wire.window = Some("another-window".into());
    wire.mails.insert("another-window".into(), 100);
    wire.service.reject_queued_wire_drafts();
    wire.flush().unwrap();
    registry
        .wait_resolved(Instant::now(), &AtomicBool::new(false))
        .unwrap();
    let frames = host_frames(&wire.stdout);
    assert!(frames.iter().any(|frame| matches!(frame, HostFrame::Response { id: 99, outcome: Outcome::Refused(RequestRefusal::MailRefused { detail }) } if detail.contains("upgrading"))));
    assert!(wire.mails.contains_key("another-window"));
}

#[test]
fn draft_barrier_refuses_unknown_service_end_instead_of_surrendering() {
    let (wire, receiver, registry) = setup();
    reserve(&wire, &registry, "claimed without a verdict");
    let _claimed = receiver.try_recv().unwrap();
    wire.service.clear_wire_drafts();
    assert!(
        registry
            .wait_resolved(Instant::now(), &AtomicBool::new(false))
            .unwrap_err()
            .contains("verdict_unknown")
    );
}
