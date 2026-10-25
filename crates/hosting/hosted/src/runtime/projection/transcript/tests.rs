#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;
use plakous_host_protocol::limits::OBSERVATION_QUEUE_CAPACITY;
use plakous_host_protocol::views::transcript::{Lane, NoticeScope, RowKey, TranscriptRow};
use plakous_host_protocol::{Core, observation_channel};

fn snapshot(scope: TranscriptScope, rows: &[(u64, &str)]) -> PresentationSnapshot {
    PresentationSnapshot {
        scope,
        items: rows
            .iter()
            .map(|(anchor, text)| TranscriptItem {
                key: RowKey::new(*anchor, Lane::Runtime, 0),
                row: TranscriptRow::assistant(*text),
            })
            .collect(),
        ..PresentationSnapshot::default()
    }
}

#[test]
fn attach_is_full_and_ordinary_publication_is_only_the_changed_keys() {
    let (sink, receiver) = observation_channel::<Core>();
    let mut publisher = Presentations::default();
    assert!(publisher.publish(
        snapshot(TranscriptScope::Root, &[(1, "old"), (2, "kept")]),
        &sink
    ));
    let HostEvent::PresentationSnapshot(first) = receiver.try_recv().unwrap() else {
        panic!("attach snapshot");
    };
    let mut folded = TranscriptState::default();
    folded.replace(first).unwrap();
    assert!(publisher.publish(
        snapshot(TranscriptScope::Root, &[(2, "kept"), (7, "new")]),
        &sink
    ));
    let HostEvent::TranscriptPatch(patch) = receiver.try_recv().unwrap() else {
        panic!("ordinary patch");
    };
    assert_eq!(patch.upserts.len(), 1);
    assert_eq!(patch.upserts[0].key.anchor, 7);
    assert_eq!(patch.deletes, [RowKey::new(1, Lane::Runtime, 0)]);
    folded.apply(patch).unwrap();
    assert!(publisher.publish(
        snapshot(TranscriptScope::Root, &[(2, "kept"), (7, "new")]),
        &sink
    ));
    let HostEvent::TranscriptPatch(empty) = receiver.try_recv().unwrap() else {
        panic!("reconciliation patch");
    };
    assert!(empty.upserts.is_empty() && empty.deletes.is_empty());
    folded.apply(empty).unwrap();
    assert!(publisher.resync(&TranscriptScope::Root, &sink));
    let HostEvent::PresentationSnapshot(resync) = receiver.try_recv().unwrap() else {
        panic!("resync snapshot");
    };
    assert_eq!(
        resync
            .items
            .into_iter()
            .map(|item| (item.key, item.row))
            .collect::<BTreeMap<_, _>>(),
        *folded.items()
    );
}

#[test]
fn a_dropped_patch_is_recovered_by_a_full_overflow_snapshot() {
    let (sink, receiver) = observation_channel::<Core>();
    let mut publisher = Presentations::default();
    let scope = TranscriptScope::Root;
    assert!(publisher.publish(snapshot(scope.clone(), &[(1, "before")]), &sink));
    let _ = receiver.try_recv().unwrap();
    for index in 0..OBSERVATION_QUEUE_CAPACITY {
        sink.send(HostEvent::Row(TranscriptRow::error(
            NoticeScope::Host,
            format!("error {index}"),
        )))
        .unwrap();
    }
    assert!(publisher.publish(snapshot(scope, &[(1, "after")]), &sink));
    let resync = receiver
        .try_iter()
        .find_map(|event| match event {
            HostEvent::PresentationSnapshot(snapshot) => Some(snapshot),
            _ => None,
        })
        .unwrap();
    assert_eq!(resync.items[0].row, TranscriptRow::assistant("after"));
    assert!(!sink.transcript_resnapshot_required());
}

#[test]
fn scopes_share_the_publisher_but_never_each_others_baseline() {
    let (sink, receiver) = observation_channel::<Core>();
    let mut publisher = Presentations::default();
    let root = TranscriptScope::Root;
    let child = TranscriptScope::Agent("c1".into());
    assert!(publisher.publish(snapshot(root.clone(), &[(1, "root")]), &sink));
    assert!(publisher.publish(snapshot(child.clone(), &[(1, "child")]), &sink));
    assert!(publisher.publish(snapshot(child.clone(), &[(1, "child changed")]), &sink));
    let events = receiver.try_iter().collect::<Vec<_>>();
    assert_eq!(events.len(), 3);
    let HostEvent::TranscriptPatch(patch) = &events[2] else {
        panic!("child patch");
    };
    assert_eq!(patch.scope, child);
    assert!(publisher.resync(&root, &sink));
    let HostEvent::PresentationSnapshot(root_snapshot) = receiver.try_recv().unwrap() else {
        panic!("root snapshot");
    };
    assert_eq!(root_snapshot.scope, root);
    assert_eq!(root_snapshot.items[0].row, TranscriptRow::assistant("root"));
    assert!(publisher.publish(snapshot(TranscriptScope::Agent("c2".into()), &[]), &sink));
    assert!(
        !publisher.resync(&child, &sink),
        "only one agent view is retained"
    );
}
