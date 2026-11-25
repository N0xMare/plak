use super::*;
use crate::surface_protocol::{ContentEvent, PlakExtension};
use plakous_host_protocol::{observation_channel, views::PresentationSnapshot};
use plakous_hosted::runtime::{Sender, test_kit};

#[test]
fn draft_attach_replays_exactly_and_pages_after_restart_or_prune() {
    use crate::host::protocol_13::{
        finish_first_page, finish_replay, prepare_first_page, prepare_replay, status,
    };
    use plakd_wire_types::v13::AttachTranscript;
    use plakous_host_protocol::intent_channel;
    use plakous_host_protocol::wire::NodeAddress;
    use plakous_hosted::host_service::wire::WireConnection;

    let fixture = Fixture::new();
    let (channel, observations) = observation_channel::<PlakExtension>();
    let (sender, root) = Sender::over_keyed_root(channel);
    let (actions, _intents) = intent_channel();
    let node = NodeAddress {
        tree_id: "tree".into(),
        agent_id: "root".into(),
    };
    let mut wire =
        WireConnection::new(node.clone(), Vec::new(), actions).with_attachment_status(status);
    let publish = wire.publisher();
    let pump = || {
        while let Ok(observation) = observations.try_recv() {
            publish.publish(observation);
        }
    };
    assert!(test_kit::publish_root_projection(
        &sender,
        &fixture.id,
        FeedSeq::ZERO,
        PresentationSnapshot::default(),
    ));
    pump();
    let attach = |wire: &mut WireConnection<PlakExtension, Vec<u8>>,
                  generation,
                  retained: Option<&RetainedRevision>| {
        let stamp = wire.begin_viewport(&node, generation).unwrap();
        if let Some(retained) = retained
            && let Some(replay) = prepare_replay(&fixture.paths, &fixture.id, generation, retained)
            && let Some(page) = finish_replay(wire, &stamp, 1, replay).unwrap()
        {
            return page;
        }
        let stamp = wire.begin_viewport(&node, generation).unwrap();
        let page = prepare_first_page(&fixture.paths, &fixture.id, 1, generation, &root).unwrap();
        finish_first_page(wire, &stamp, 1, page).unwrap().unwrap()
    };
    let first = attach(&mut wire, 13, None);
    let HostFrameV13::Snapshot(baseline) = &first.frames[1] else {
        panic!("expected first page")
    };
    let retained = baseline.revision.clone();
    let encode = |frames: &[HostFrameV13]| {
        frames
            .iter()
            .map(|frame| wire::encode_line(frame).unwrap())
            .collect::<String>()
    };
    let first_wire = encode(&first.frames);
    wire.write_viewport_prefix(baseline.attachment, |out| {
        assert_eq!(out.as_slice(), b"");
        out.extend_from_slice(first_wire.as_bytes());
        Ok(())
    })
    .unwrap();
    assert!(sender.send(ContentEvent::AssistantDelta("one".into())));
    assert!(sender.send(ContentEvent::AssistantDone));
    pump();
    let replay = attach(&mut wire, 13, Some(&retained));
    assert!(
        matches!(&replay.frames[0], HostFrameV13::Response { outcome: Outcome::Ok(answer), .. }
        if matches!(answer.as_ref(), Answer::Draft(AnswerV13::Attached {
            transcript: AttachTranscript::Replay { from, to }, ..
        }) if from == &retained && to.revision > from.revision && to.token == from.token))
    );
    assert!(matches!(
        &replay.frames[1],
        HostFrameV13::Event { seq: 1, .. }
    ));
    assert!(
        replay.frames[1..]
            .iter()
            .all(|frame| matches!(frame, HostFrameV13::Event { .. }))
    );
    // A ready live patch cannot pass the response and frozen replay.
    assert!(sender.send(ContentEvent::UserMessage {
        text: "two".into(),
        sent_at_ms: None,
    }));
    pump();
    wire.flush().unwrap();
    let attachment = wire.attachment().unwrap();
    let replay_wire = encode(&replay.frames);
    let replay_end = first_wire.len() + replay_wire.len();
    wire.write_viewport_prefix(attachment, |out| {
        assert_eq!(out.as_slice(), first_wire.as_bytes());
        out.extend_from_slice(replay_wire.as_bytes());
        Ok(())
    })
    .unwrap();
    wire.flush().unwrap();

    let mut store = ProductStore::open(&fixture.paths).unwrap();
    let views = SessionViews::empty();
    let mut pruned = views.clone();
    pruned.state.truncated = true;
    store
        .commit_projection_views(
            &fixture.id,
            &ViewWrites::between(&views, &pruned),
            &pruned.state,
        )
        .unwrap();
    let fallback = attach(&mut wire, 13, Some(&retained));
    let HostFrameV13::Snapshot(replacement) = &fallback.frames[1] else {
        panic!("prune must page")
    };
    assert_ne!(replacement.revision.token, retained.token);
    wire.write_viewport_prefix(replacement.attachment, |out| {
        assert_eq!(&out[first_wire.len()..replay_end], replay_wire.as_bytes());
        let live = std::str::from_utf8(&out[replay_end..]).unwrap();
        assert!(live.lines().any(|line| matches!(
            wire::decode_line::<wire::HostFrame<PlakExtension>>(line).unwrap(),
            wire::HostFrame::Event {
                attachment: id, seq,
                observation: plakous_host_protocol::SessionObservation::TranscriptPatch(_), ..
            } if id == attachment && seq >= replay.frames.len() as u64
        )));
        Ok(())
    })
    .unwrap();
    let (actions, _restarted_intents) = intent_channel();
    let mut restarted = WireConnection::new(node.clone(), Vec::new(), actions);
    restarted.publisher().publish(
        plakous_host_protocol::SessionObservation::PresentationSnapshot(
            PresentationSnapshot::empty(root.capture(&fixture.id).unwrap().revision),
        ),
    );
    let restarted = attach(&mut restarted, 14, Some(&replacement.revision));
    assert!(matches!(&restarted.frames[1], HostFrameV13::Snapshot(page)
        if page.revision.host_generation == 14 && page.revision.token != replacement.revision.token));
}

#[test]
fn first_page_and_replay_admit_during_an_unbroken_keyed_run() {
    use crate::host::protocol_13::{
        finish_first_page, finish_replay, prepare_first_page, prepare_replay, read_live_row,
    };
    use plakd_wire_types::v13::OpV13;
    use plakous_host_protocol::views::transcript::TranscriptStreamDelta;
    use plakous_host_protocol::{SessionObservation, intent_channel, wire::NodeAddress};
    use plakous_hosted::host_service::wire::WireConnection;

    let fixture = Fixture::new();
    let (events, observations) = observation_channel::<PlakExtension>();
    let (sender, root) = Sender::over_keyed_root(events);
    let (actions, _intents) = intent_channel();
    let node = NodeAddress {
        tree_id: "tree".into(),
        agent_id: "root".into(),
    };
    let mut wire = WireConnection::new(node.clone(), Vec::new(), actions);
    let publish = wire.publisher();
    let pump = || {
        let mut last = None;
        while let Ok(event) = observations.try_recv() {
            if let SessionObservation::TranscriptStream {
                delta: TranscriptStreamDelta::Keyed { row, offset, .. },
                ..
            } = &event
            {
                last = Some((*row, *offset));
            }
            publish.publish(event);
        }
        last
    };
    assert!(test_kit::publish_root_projection(
        &sender,
        &fixture.id,
        FeedSeq::ZERO,
        PresentationSnapshot::default(),
    ));
    assert!(sender.send(ContentEvent::AssistantDelta("first".into())));
    pump().unwrap();
    let read = wire.begin_viewport(&node, 13).unwrap();
    let page = prepare_first_page(&fixture.paths, &fixture.id, 1, 13, &root).unwrap();
    let missed = "é".repeat(64);
    for _ in 0..64 {
        assert!(sender.send(ContentEvent::AssistantDelta("é".into())));
        pump().unwrap();
    }
    // One page read suffices even though the run has never stopped appending.
    let first = finish_first_page(&mut wire, &read, 1, page)
        .unwrap()
        .unwrap();
    let HostFrameV13::Snapshot(baseline) = &first.frames[1] else {
        panic!("expected a first page");
    };
    let [LiveRowLength { row, len }] = baseline.live_rows.as_slice() else {
        panic!("expected one live row");
    };
    assert_eq!(*len, 5);
    let revision = &baseline.revision;
    wire.write_viewport_prefix(baseline.attachment, |_| Ok(()))
        .unwrap();
    let stamp = wire.begin_viewport(&node, 13).unwrap();
    let replay = prepare_replay(&fixture.paths, &fixture.id, 13, revision).unwrap();
    let replay = finish_replay(&mut wire, &stamp, 2, replay)
        .unwrap()
        .unwrap();
    assert_eq!(replay.frames.len(), 1); // appends don't advance the patch revision

    assert!(sender.send(ContentEvent::AssistantDelta("tail".into())));
    assert_eq!(pump(), Some((*row, *len + missed.len() as u64)));
    let attachment = wire.attachment().unwrap();
    let slice = read_live_row(
        &root,
        attachment,
        &OpV13::ReadTranscriptRow {
            attachment,
            row: *row,
            offset: *len,
        },
    );
    assert!(
        matches!(slice, Some(Outcome::Ok(answer)) if matches!(answer.as_ref(),
        Answer::Draft(AnswerV13::TranscriptRow { text, offset, last: true, .. })
        if *offset == *len && *text == missed + "tail"))
    );
}

#[test]
fn capture_refuses_store_ahead_then_replaces_provisional_rows_without_duplicates() {
    let fixture = Fixture::new();
    let (channel, _observations) = observation_channel::<PlakExtension>();
    let (sender, root) = Sender::over_keyed_root(channel);
    assert!(test_kit::publish_root_projection(
        &sender,
        &fixture.id,
        FeedSeq::ZERO,
        PresentationSnapshot::default()
    ));
    assert!(sender.send(ContentEvent::AssistantDelta("Café".into())));
    assert!(sender.send(ContentEvent::AssistantDelta("☕".into())));
    let live = attach(&fixture.paths, &fixture.id, 1, target(), Some(&root)).unwrap();
    let HostFrameV13::Snapshot(baseline) = &live.frames[1] else {
        panic!("no live baseline")
    };
    let provisional = baseline.items[0].key;
    assert!(RootTranscript::is_provisional(provisional));
    assert_eq!(
        baseline.live_rows,
        [LiveRowLength {
            row: provisional,
            len: 8
        }]
    );
    assert_eq!(baseline.items[0].row.0.text(), Some("Café☕"));
    assert_eq!(
        baseline.revision.revision,
        root.capture(&fixture.id).unwrap().revision
    );

    let canon = key(20, 0);
    fixture.seed(20, [(20, 0, "Café☕".into())]);
    assert!(
        matches!(attach(&fixture.paths, &fixture.id, 2, target(), Some(&root)),
        Err(RequestRefusal::Shared(reason)) if *reason.as_inner() == wire::RequestRefusal::Backpressure)
    );
    assert!(test_kit::publish_root_projection(
        &sender,
        &fixture.id,
        FeedSeq::new(20),
        PresentationSnapshot {
            items: vec![TranscriptItem {
                key: canon,
                row: TranscriptRow::assistant("Café☕")
            }],
            ..PresentationSnapshot::default()
        }
    ));
    let settled = attach(&fixture.paths, &fixture.id, 3, target(), Some(&root)).unwrap();
    let HostFrameV13::Snapshot(baseline) = &settled.frames[1] else {
        panic!("no settled baseline")
    };
    assert_eq!(baseline.items.len(), 1);
    assert_eq!(baseline.items[0].key, canon);
    assert_eq!(baseline.live_rows, [] as [LiveRowLength; 0]);
    assert!(root.read(provisional, 0).is_err());
}

#[test]
fn missing_runtime_feed_leaves_saved_history_readable_as_one_page() {
    let fixture = Fixture::new();
    let mut store = ProductStore::open(&fixture.paths).unwrap();
    let mut record = store.load(&fixture.id).unwrap();
    record.plakous_locator = Some(
        AgentSessions::genesis_locator_for(&SessionOperationId::new("missing-feed").unwrap())
            .unwrap(),
    );
    store.commit(&mut record).unwrap();
    fixture.seed(20, [(1, 0, "old".into()), (20, 0, "new".into())]);
    let saved = attach(&fixture.paths, &fixture.id, 1, target(), None).unwrap();
    assert!(matches!(&saved.frames[1], HostFrameV13::Snapshot(page)
        if page.items.len() == 2 && page.last && !page.has_older));
    let other = Fixture::new();
    assert_expired(&[read_row(&other.paths, &saved.binding, 2, key(20, 0), 0)]);
}
