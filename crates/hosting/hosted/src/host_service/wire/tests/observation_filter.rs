use super::*;
use plakous_host_protocol::views::transcript::{ToolItem, TranscriptStreamDelta as Delta};
use plakous_host_protocol::{Core, intent_channel, observation_channel};

#[test]
fn unservable_previews_are_never_numbered_for_child_or_agent_viewers() {
    let (actions, _intents) = intent_channel::<Core>();
    let (published, events) = observation_channel::<Core>();
    let session = WireSession::start_counted_filtered(
        root(),
        "session".into(),
        actions,
        events,
        |_| None,
        &super::super::super::AttachmentCount::default(),
        Some(|event| !event.is_unkeyed_text_preview()),
    );
    let service = served::lock(&session.served)
        .service()
        .with_nodes(std::sync::Arc::new(OpenChildren));
    let viewer = service.attach(&root(), Face::Remote).unwrap();
    let child = service
        .attach(
            &NodeAddress {
                agent_id: "child".into(),
                ..root()
            },
            Face::Remote,
        )
        .unwrap();
    service
        .intent(
            viewer,
            SessionIntent::OpenAgentTranscript {
                agent_id: "child".into(),
            },
        )
        .unwrap();
    for attachment in [viewer, child] {
        assert!(service.take(attachment).is_empty());
        service.enable_attachment_status(attachment, Instant::now(), 0);
    }
    let stream = |delta| SessionObservation::TranscriptStream {
        scope: TranscriptScope::Agent("child".into()),
        delta,
    };
    let tool = |state: &str| -> ToolItem {
        serde_json::from_value(serde_json::json!({
            "call_id": "call", "name": "tool", "activity_kind": "other", "state": state
        }))
        .unwrap()
    };
    for event in [
        stream(Delta::Tool(Box::new(tool("running")))),
        stream(Delta::Assistant("preview".into())),
        stream(Delta::Reasoning {
            location_key: "reasoning".into(),
            text: "preview".into(),
        }),
        stream(Delta::AssistantDone),
        stream(Delta::Tool(Box::new(tool("done")))),
    ] {
        published.send(event).unwrap();
    }
    drop(published);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !session.ended() {
        assert!(Instant::now() < deadline, "observation pump did not drain");
        std::thread::yield_now();
    }
    for attachment in [viewer, child] {
        let mut connection = session
            .connect(Vec::new(), Face::Remote, || false)
            .with_attachment_status(|_, _, _, _| Ok(()));
        connection.attachment = Some(attachment);
        connection.flush().unwrap();
        let frames = host_frames(&connection.stdout.inner);
        let numbers: Vec<_> = frames
            .iter()
            .filter_map(|frame| match frame {
                HostFrame::Event {
                    seq,
                    observation:
                        SessionObservation::TranscriptStream {
                            delta: Delta::Tool(_),
                            ..
                        },
                    ..
                } => Some(*seq),
                _ => None,
            })
            .collect();
        assert_eq!(numbers, [1, 2], "{frames:?}");
        let status = service
            .attachment_status(attachment, Instant::now(), true)
            .unwrap();
        assert_eq!((status.seq_gaps, status.deltas_evicted), (0, 0));
    }
}
