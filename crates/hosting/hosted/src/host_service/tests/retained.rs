use super::*;
use plakous_host_protocol::views::transcript::{Lane, RowKey, TranscriptItem, TranscriptRow};

#[test]
fn attachments_share_cached_snapshot_and_patch_payloads() {
    let (service, intents) = service(16);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    service.observe(root_patch(1));
    let ids = [(); 3].map(|()| service.attach(&root(), Face::Remote).unwrap());
    service.observe(root_patch(2));
    let [first, second, third] = ids.map(|id| service.take_shared(id));
    assert!(intents.try_recv().is_err());
    assert_eq!(first.len(), 3);
    for other in [second, third] {
        assert_eq!(first, other);
        for (a, b) in first.iter().zip(other) {
            match (a, b) {
                (
                    Pending::Event {
                        observation: Payload::Snapshot(a),
                        ..
                    },
                    Pending::Event {
                        observation: Payload::Snapshot(b),
                        ..
                    },
                ) => assert!(Arc::ptr_eq(a, &b)),
                (
                    Pending::Event {
                        observation: Payload::Patch(a),
                        ..
                    },
                    Pending::Event {
                        observation: Payload::Patch(b),
                        ..
                    },
                ) => assert!(Arc::ptr_eq(a, &b)),
                _ => panic!("expected only transcript payloads"),
            }
        }
    }
}

#[test]
fn crossing_the_byte_cap_requests_a_fresh_baseline_only_for_later_attaches() {
    let (service, intents) = service(16);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let large = |from_revision, scope| {
        Observation::TranscriptPatch(TranscriptPatch {
            scope,
            from_revision,
            to_revision: from_revision + 1,
            upserts: vec![TranscriptItem {
                key: RowKey::new(1, Lane::Runtime, 0),
                row: TranscriptRow::assistant("x".repeat(4 * 1024 * 1024)),
            }],
            deletes: Vec::new(),
        })
    };
    service.observe(large(1, TranscriptScope::Root));
    // Agent scope has no retained replay and cannot use up the root's cap.
    service.observe(large(1, TranscriptScope::Agent("child".into())));
    let old = service.attach(&root(), Face::Remote).unwrap();
    assert!(intents.try_recv().is_err());

    // Each root patch fits individually; their encoded lengths add to >8 MiB.
    service.observe(large(2, TranscriptScope::Root));
    let new = service.attach(&root(), Face::Remote).unwrap();
    assert!(matches!(
        &*intents.try_recv().unwrap(),
        SessionIntent::ResyncTranscript {
            scope: TranscriptScope::Root
        }
    ));
    assert!(intents.try_recv().is_err());
    service.observe(root_patch(3));
    assert_eq!(
        service.take(new),
        [] as [Outbound<Core>; 0],
        "no root patch without a baseline"
    );

    let queued = observations(service.take(old));
    assert!(
        matches!(queued.as_slice(), [Observation::PresentationSnapshot(snapshot),
        Observation::TranscriptPatch(first), Observation::TranscriptPatch(second),
        Observation::TranscriptPatch(third)] if snapshot.revision == 1
        && first.to_revision == 2 && second.to_revision == 3 && third.to_revision == 4)
    );

    let fresh = Observation::PresentationSnapshot(PresentationSnapshot::empty(4));
    service.observe(fresh.clone());
    service.observe(root_patch(4));
    assert_eq!(
        observations(service.take(new)),
        [fresh.clone(), root_patch(4)]
    );
    let later = service.attach(&root(), Face::Remote).unwrap();
    assert_eq!(observations(service.take(later)), [fresh, root_patch(4)]);
    assert!(intents.try_recv().is_err());
}

#[test]
fn the_patch_count_limit_still_applies_to_small_patches() {
    let (service, intents) = service(16);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    for revision in 1..=64 {
        service.observe(root_patch(revision));
    }
    let kept = service.attach(&root(), Face::Remote).unwrap();
    assert_eq!(service.take(kept).len(), 65);
    assert!(intents.try_recv().is_err());
    service.observe(root_patch(65));
    let new = service.attach(&root(), Face::Remote).unwrap();
    assert_eq!(service.take(new), [] as [Outbound<Core>; 0]);
    assert!(matches!(
        &*intents.try_recv().unwrap(),
        SessionIntent::ResyncTranscript {
            scope: TranscriptScope::Root
        }
    ));
}
