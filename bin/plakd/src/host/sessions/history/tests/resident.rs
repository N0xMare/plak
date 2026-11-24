use super::*;
use crate::surface_protocol::{ContentEvent, PlakExtension};
use plakous_host_protocol::{observation_channel, views::PresentationSnapshot};
use plakous_hosted::runtime::{Sender, test_kit};

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
    assert!(baseline.live_rows.is_empty());
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
