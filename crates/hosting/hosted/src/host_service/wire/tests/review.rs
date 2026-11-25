use super::*;
use plakous_host_protocol::Core;
use plakous_host_protocol::views::transcript::{Lane, RowKey, TranscriptItem};

struct RacedHistory;

impl HistoryBinding for RacedHistory {
    fn repair(&self, _: &mut dyn Write, _: &PresentationSnapshot) -> CliResult<bool> {
        Ok(false)
    }

    fn read(
        &self,
        _: &mut dyn Write,
        _: RequestId,
        _: Option<&crate::runtime::RootTranscript>,
        _: TranscriptRead,
    ) -> CliResult<()> {
        panic!("the expired binding must not be read")
    }
}

#[test]
fn an_unstable_root_repair_does_not_send_a_truncated_history_as_complete() {
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut connection = WireConnection::new(root(), Vec::new(), actions).with_snapshot_output(
        Some(std::sync::Arc::new(|_, _| {
            panic!("a bounded snapshot cannot replace paged root history")
        })),
    );
    connection
        .service
        .observe(SessionObservation::PresentationSnapshot(
            PresentationSnapshot::empty(1),
        ));
    let stamp = connection.begin_viewport(&root(), 3).unwrap();
    let start = connection
        .install_viewport_page(&stamp, "line".into(), 1)
        .unwrap()
        .unwrap();
    let attachment = start.attachment;
    connection
        .write_viewport_prefix(attachment, |_| Ok(()))
        .unwrap();
    connection.history = Some((attachment, Box::new(RacedHistory)));
    connection
        .service
        .lose_wire_patch(attachment, &TranscriptScope::Root);
    connection
        .service
        .observe(SessionObservation::PresentationSnapshot(
            PresentationSnapshot {
                // This is a recent resident window, not the entire transcript.
                items: (10_001..11_025)
                    .map(|anchor| TranscriptItem {
                        key: RowKey::new(anchor, Lane::Runtime, 0),
                        row: TranscriptRow::assistant("recent output"),
                    })
                    .collect(),
                ..PresentationSnapshot::empty(1)
            },
        ));
    connection
        .service
        .observe(SessionObservation::SetBusy(false));
    connection.flush().unwrap();
    assert_eq!(
        host_frames(&connection.stdout.inner),
        [HostFrame::ResyncRequired { attachment }]
    );
    assert!(!connection.service.attached(attachment));
    assert!(connection.history.is_none());
}

#[test]
fn detaching_before_prefix_delivery_settles_deferred_status() {
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut connection = WireConnection::new(root(), Vec::new(), actions)
        .with_buffered_output(true)
        .with_attachment_status(test_status_output);
    connection
        .service
        .observe(SessionObservation::PresentationSnapshot(
            PresentationSnapshot::empty(1),
        ));
    let stamp = connection.begin_viewport(&root(), 3).unwrap();
    let attachment = connection
        .install_viewport_page(&stamp, "line".into(), 1)
        .unwrap()
        .unwrap()
        .attachment;
    connection.history = Some((attachment, Box::new(RacedHistory)));
    assert_eq!(
        connection.reply_attachment_status(4, attachment).unwrap(),
        None
    );
    assert_eq!(
        connection.request(5, Op::Detach { attachment }),
        Some(Outcome::Ok(Answer::Detached))
    );
    connection.flush().unwrap();
    assert_eq!(
        host_frames(&connection.stdout.inner),
        [HostFrame::Response {
            id: 4,
            outcome: Outcome::Refused(RequestRefusal::NotAttached)
        }]
    );
    assert!(connection.history.is_none());
}

#[test]
fn a_child_attach_on_the_same_connection_drops_root_history_authority() {
    let (actions, _intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut connection = WireConnection::new(root(), Vec::new(), actions);
    connection.service = connection
        .service
        .with_nodes(std::sync::Arc::new(OpenChildren));
    connection
        .service
        .observe(SessionObservation::PresentationSnapshot(
            PresentationSnapshot::empty(1),
        ));
    let stamp = connection.begin_viewport(&root(), 3).unwrap();
    let attachment = connection
        .install_viewport_page(&stamp, "line".into(), 1)
        .unwrap()
        .unwrap()
        .attachment;
    connection
        .write_viewport_prefix(attachment, |_| Ok(()))
        .unwrap();
    connection.history = Some((attachment, Box::new(RacedHistory)));
    let child = NodeAddress {
        agent_id: "child".into(),
        ..root()
    };
    assert_eq!(
        connection.request(2, Op::Attach { node: child }),
        Some(Outcome::Ok(Answer::Attached {
            attachment: attachment + 1
        }))
    );
    assert!(connection.history.is_none());
    assert!(!connection.service.attached(attachment));
}
