use super::*;
use crate::host_service::{HostService, Outbound, wire::Face};
use crate::runtime::{LiveRowReadError, RootTranscript, Sender, test_kit};
use plakous_host_protocol::views::transcript::TranscriptStreamDelta;
use plakous_host_protocol::wire::NodeAddress;
use plakous_host_protocol::{ObservationReceiver, intent_channel};

fn reasoning(text: &str) -> ContentEvent {
    ContentEvent::ReasoningDelta {
        location_key: "same-location".into(),
        text: text.into(),
    }
}

fn append(event: HostEvent) -> (RowKey, u64, u64, String) {
    let HostEvent::TranscriptStream {
        scope: TranscriptScope::Root,
        delta:
            TranscriptStreamDelta::Keyed {
                row,
                offset,
                end,
                text,
            },
    } = event
    else {
        panic!("expected keyed root append, got {event:?}")
    };
    (row, offset, end, text)
}

fn patch(event: HostEvent) -> TranscriptPatch {
    let HostEvent::TranscriptPatch(patch) = event else {
        panic!("expected patch, got {event:?}")
    };
    patch
}

fn opening(events: &ObservationReceiver, previous: Option<(RowKey, u64)>) -> RowKey {
    let patch = patch(events.try_recv().unwrap());
    let mut items = patch.upserts.iter();
    if let Some((row, len)) = previous {
        let prior = items.next().unwrap();
        assert_eq!(prior.key, row);
        assert_eq!(RootTranscript::live_row_len(prior), Some(len));
    }
    let empty = items.next().unwrap();
    assert_eq!(RootTranscript::live_row_len(empty), Some(0));
    assert!(items.next().is_none());
    empty.key
}

#[test]
fn keyed_runs_open_before_streaming_and_reconcile_at_interleaving() {
    let (channel, events) = observation_channel();
    let (sender, root) = Sender::over_keyed_root(channel);
    assert!(sender.replace_transcript(snapshot(TranscriptScope::Root, &[])));
    events.try_recv().unwrap();
    sender.send(ContentEvent::AssistantDelta("Café".into()));
    let first = opening(&events, None);
    assert_eq!(
        append(events.try_recv().unwrap()),
        (first, 0, 5, "Café".into())
    );
    sender.send(ContentEvent::AssistantDelta("☕".into()));
    assert_eq!(
        append(events.try_recv().unwrap()),
        (first, 5, 8, "☕".into())
    );

    sender.send(reasoning("α"));
    let r1 = opening(&events, Some((first, 8)));
    assert_eq!(append(events.try_recv().unwrap()), (r1, 0, 2, "α".into()));
    sender.send(reasoning("β"));
    assert_eq!(append(events.try_recv().unwrap()), (r1, 2, 4, "β".into()));
    sender.send(ContentEvent::AssistantDelta("尾".into()));
    let second = opening(&events, Some((r1, 4)));
    assert_eq!(
        append(events.try_recv().unwrap()),
        (second, 0, 3, "尾".into())
    );
    sender.send(reasoning("γ"));
    let r2 = opening(&events, Some((second, 3)));
    assert_eq!(append(events.try_recv().unwrap()), (r2, 0, 2, "γ".into()));
    assert!(first < r1 && r1 < second && second < r2);

    let repair = root.read(first, 3).unwrap();
    assert_eq!(
        (repair.offset, repair.end, repair.text.as_str(), repair.last),
        (3, 8, "é☕", true)
    );
    assert_eq!(root.read(first, 4), Err(LiveRowReadError::InvalidOffset));
    assert_eq!(root.read(r1, 2).unwrap().text, "β");
    assert!(
        events.try_recv().is_err(),
        "row repair must not publish a baseline"
    );

    assert!(sender.resync_transcript(&TranscriptScope::Root));
    let HostEvent::PresentationSnapshot(baseline) = events.try_recv().unwrap() else {
        panic!("expected baseline")
    };
    assert_eq!(
        baseline
            .items
            .iter()
            .map(|item| (item.key, RootTranscript::live_row_len(item)))
            .collect::<Vec<_>>(),
        [
            (first, Some(8)),
            (r1, Some(4)),
            (second, Some(3)),
            (r2, Some(2))
        ]
    );
    assert!(sender.publish_transcript(snapshot(TranscriptScope::Root, &[(12, "Café☕尾")])));
    let canonical = patch(events.try_recv().unwrap());
    assert_eq!(canonical.deletes, [first, r1, second, r2]);
    assert_eq!(
        canonical.upserts[0].row,
        TranscriptRow::assistant("Café☕尾")
    );
    assert_eq!(root.read(first, 0), Err(LiveRowReadError::RowGone));
    assert_eq!(RootTranscript::live_row_len(&canonical.upserts[0]), None);
}

#[test]
fn keyed_live_rows_keep_a_framing_safe_prefix_and_repair_in_scalar_slices() {
    use super::super::repair::MAX_LIVE_ASSISTANT_BYTES;

    let (channel, events) = observation_channel();
    let (sender, root) = Sender::over_keyed_root(channel);
    assert!(sender.replace_transcript(snapshot(TranscriptScope::Root, &[])));
    events.try_recv().unwrap();
    let boundary = 64 * 1024 - 1;
    let prefix = format!(
        "{}é{}é",
        "\0".repeat(boundary),
        "\0".repeat(MAX_LIVE_ASSISTANT_BYTES - boundary - 5)
    );
    sender.send(ContentEvent::AssistantDelta(format!("{prefix}☕")));
    let row = opening(&events, None);
    let (_, start, end, emitted) = append(events.try_recv().unwrap());
    assert_eq!(
        (start, end, emitted),
        (0, prefix.len() as u64, prefix.clone())
    );
    sender.send(ContentEvent::AssistantDelta("z".into()));
    assert!(
        events.try_recv().is_err(),
        "must not resume after dropping a scalar"
    );
    let first = root.read(row, 0).unwrap();
    assert!(!first.last);
    assert_eq!(first.end, boundary as u64);
    let next = root.read(row, first.end).unwrap();
    assert_eq!(next.offset, first.end);
    assert!(next.text.starts_with('é'));
    assert_eq!(root.read(row, end - 2).unwrap().text, "é");
    assert_eq!(
        root.read(row, end + 1),
        Err(LiveRowReadError::InvalidOffset)
    );
    assert_eq!(root.read(row, end).unwrap().text, "");
    assert!(sender.resync_transcript(&TranscriptScope::Root));
    let HostEvent::PresentationSnapshot(baseline) = events.try_recv().unwrap() else {
        panic!("expected capped baseline")
    };
    assert_eq!(RootTranscript::live_row_len(&baseline.items[0]), Some(end));
    assert_eq!(baseline.items[0].row, TranscriptRow::assistant(prefix));
}

#[test]
fn losing_an_opening_patch_suppresses_its_queued_deltas_until_baseline() {
    use plakous_host_protocol::status::HostPhase;

    let (channel, events) = observation_channel();
    let (sender, _) = Sender::over_keyed_root(channel.clone());
    assert!(sender.replace_transcript(snapshot(TranscriptScope::Root, &[])));
    events.try_recv().unwrap();
    sender.send(ContentEvent::AssistantDelta("é".into()));
    for _ in 2..OBSERVATION_QUEUE_CAPACITY {
        channel.send(HostEvent::IntentStarted(None)).unwrap();
    }
    channel
        .send(HostEvent::Status {
            phase: HostPhase::Idle,
            queue_len: 0,
            detail: None,
        })
        .unwrap();
    assert!(events.transcript_resnapshot_required_for(&TranscriptScope::Root));
    assert!(!sender.send(ContentEvent::AssistantDelta("☕".into())));
    assert!(
        !events
            .try_iter()
            .any(|event| matches!(event, HostEvent::TranscriptStream { .. }))
    );
    assert!(sender.resync_transcript(&TranscriptScope::Root));
    let HostEvent::PresentationSnapshot(baseline) = events.try_recv().unwrap() else {
        panic!("gap requires a new baseline")
    };
    let row = baseline.items[0].key;
    assert_eq!(baseline.items[0].row, TranscriptRow::assistant("é☕"));
    sender.send(ContentEvent::AssistantDelta("!".into()));
    assert_eq!(append(events.try_recv().unwrap()), (row, 5, 6, "!".into()));
}

fn relay(events: &ObservationReceiver, service: &HostService<Core>) {
    for event in events.try_iter() {
        service.observe(event);
    }
}

fn observations(outbound: Vec<Outbound<Core>>) -> Vec<HostEvent> {
    outbound
        .into_iter()
        .map(|item| match item {
            Outbound::Event { observation, .. } => observation,
            other => panic!("unexpected queue boundary {other:?}"),
        })
        .collect()
}

#[test]
fn keyed_queue_loss_is_repairable_and_late_attach_fences_the_same_row() {
    let node = NodeAddress {
        tree_id: "tree".into(),
        agent_id: "root".into(),
    };
    let (actions, intents) = intent_channel();
    let service = HostService::new(actions, node.clone(), 2);
    let (channel, events) = observation_channel();
    let (sender, root) = Sender::over_keyed_root(channel);
    assert!(sender.replace_transcript(snapshot(TranscriptScope::Root, &[])));
    relay(&events, &service);
    let first = service.attach_keyed_root(&node, Face::Remote).unwrap();
    assert!(test_kit::repair_root_attachment(
        &sender,
        &intents.try_recv().unwrap()
    ));
    relay(&events, &service);
    assert!(matches!(
        observations(service.take(first)).as_slice(),
        [HostEvent::PresentationSnapshot(_)]
    ));

    sender.send(ContentEvent::AssistantDelta("é".into()));
    relay(&events, &service);
    sender.send(ContentEvent::AssistantDelta("☕".into()));
    relay(&events, &service);
    let frames = observations(service.take(first));
    let row = patch(frames[0].clone()).upserts[0].key;
    assert_eq!(append(frames[1].clone()), (row, 2, 5, "☕".into()));
    assert_eq!(root.read(row, 0).unwrap().text, "é☕");
    assert!(
        intents.try_recv().is_err(),
        "droppable loss is not a lost revision"
    );
    assert_eq!(service.take(first).as_slice(), &[]);

    let late = service.attach_keyed_root(&node, Face::Remote).unwrap();
    sender.send(ContentEvent::AssistantDelta("!".into()));
    relay(&events, &service);
    assert!(service.take(late).is_empty(), "no suffix before baseline");
    // Drain the healthy viewer so the late viewer's repair does not overflow it.
    assert_eq!(
        append(observations(service.take(first)).remove(0)),
        (row, 5, 6, "!".into())
    );
    assert!(test_kit::repair_root_attachment(
        &sender,
        &intents.try_recv().unwrap()
    ));
    relay(&events, &service);
    let frames = observations(service.take(late));
    let [HostEvent::PresentationSnapshot(baseline)] = frames.as_slice() else {
        panic!("expected one current baseline: {frames:?}")
    };
    assert_eq!(baseline.items[0].key, row);
    assert_eq!(RootTranscript::live_row_len(&baseline.items[0]), Some(6));
    assert_eq!(baseline.items[0].row, TranscriptRow::assistant("é☕!"));
    assert_eq!(
        patch(observations(service.take(first)).remove(0)).to_revision,
        baseline.revision
    );
    sender.send(ContentEvent::AssistantDelta("後".into()));
    relay(&events, &service);
    assert_eq!(
        append(observations(service.take(late)).remove(0)),
        (row, 6, 9, "後".into())
    );
}
