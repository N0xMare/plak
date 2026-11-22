use super::super::queue::MAX_QUEUE_BYTES;
use super::*;
use plakous_host_protocol::views::transcript::{
    Lane, RowKey, TranscriptItem, TranscriptRow, TranscriptState,
};

fn row(bytes: usize, value: char) -> TranscriptItem {
    TranscriptItem {
        key: RowKey::new(1, Lane::Runtime, 0),
        row: TranscriptRow::assistant(value.to_string().repeat(bytes)),
    }
}

fn snapshot(revision: u64, bytes: usize, value: char) -> Observation {
    Observation::PresentationSnapshot(PresentationSnapshot {
        items: vec![row(bytes, value)],
        ..PresentationSnapshot::empty(revision)
    })
}

fn patch(from_revision: u64, bytes: usize, value: char) -> Observation {
    Observation::TranscriptPatch(TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision,
        to_revision: from_revision + 1,
        upserts: vec![row(bytes, value)],
        deletes: Vec::new(),
    })
}

#[test]
fn a_snapshot_supersedes_its_scope_but_not_queued_required_events() {
    let (service, _intents) = service(16);
    service.observe(snapshot(1, 16, 'a'));
    let id = service.attach(&root(), Face::Remote).unwrap();
    service.observe(approval("before"));
    service.observe(patch(1, 16, 'b'));
    service.observe(snapshot(3, 16, 'c'));
    service.observe(approval("after"));
    service.observe(snapshot(4, 16, 'd'));
    service.observe(patch(4, 16, 'e'));
    assert_eq!(
        observations(service.take(id)),
        [
            approval("before"),
            approval("after"),
            snapshot(4, 16, 'd'),
            patch(4, 16, 'e')
        ]
    );
}

#[test]
fn droppable_loss_does_not_replay_an_intact_keyed_chain() {
    let (service, intents) = service(2);
    service.observe(snapshot(1, 16, 'a'));
    let id = service.attach(&root(), Face::Remote).unwrap();
    service.take(id);
    service.observe(Observation::AssistantDelta("partial".into()));
    service.observe(patch(1, 16, 'b'));
    service.observe(patch(2, 16, 'c'));
    assert_eq!(
        observations(service.take(id)),
        [patch(1, 16, 'b'), patch(2, 16, 'c')]
    );
    assert!(intents.try_recv().is_err());
}

#[test]
fn one_root_repair_serves_waiters_without_resetting_a_healthy_attachment() {
    let (service, intents) = service(32);
    service.observe(snapshot(1, 16, 'a'));
    let fast = service.attach(&root(), Face::Remote).unwrap();
    service.take(fast);
    let wide = 8 * 1024 * 1024;
    service.observe(patch(1, wide, 'b'));
    assert_eq!(service.take(fast).len(), 1);
    let waiters = [(); 3].map(|()| service.attach(&root(), Face::Remote).unwrap());
    assert!(intents.try_recv().unwrap().is_root_attachment_repair());
    assert!(intents.try_recv().is_err());
    service.observe(snapshot(2, wide, 'b'));
    service.observe(patch(2, 16, 'c'));
    assert_eq!(observations(service.take(fast)), [patch(2, 16, 'c')]);
    for id in waiters {
        let events = observations(service.take(id));
        assert!(
            matches!(&events[..], [Observation::PresentationSnapshot(page), Observation::TranscriptPatch(patch)] if page.revision == 2 && patch.from_revision == 2)
        );
    }
}

#[test]
fn a_slow_keyed_view_survives_byte_pressure_and_reaches_the_published_revision() {
    let (service, intents) = service(4096);
    let bytes = 5 * 1024 * 1024;
    service.observe(snapshot(1, bytes, 'a'));
    let slow = service.attach(&root(), Face::Remote).unwrap();
    let fast = service.attach(&root(), Face::Remote).unwrap();
    let mut current = TranscriptState::default();
    let mut apply = |events: Vec<Observation>| {
        for event in events {
            match event {
                Observation::PresentationSnapshot(snapshot) => current.replace(snapshot).unwrap(),
                Observation::TranscriptPatch(patch) => current.apply(patch).unwrap(),
                _ => {}
            }
        }
    };
    apply(observations(service.take(slow)));
    service.take(fast);
    // The fast client drains; the slow client stops reading while the
    // session publishes more than twice the queue's byte budget.
    for revision in 1..=28 {
        service.observe(Observation::AssistantDelta("streaming".into()));
        service.observe(patch(revision, bytes, 'b'));
        service.take(fast);
        assert!(service.lock().attachments[&slow].queue.bytes() <= MAX_QUEUE_BYTES);
        assert!(service.attached(slow));
    }
    assert!(intents.try_recv().unwrap().is_root_attachment_repair());
    assert!(intents.try_recv().is_err());
    service.observe(snapshot(29, bytes, 'b'));
    service.observe(patch(29, 32, 'z'));
    apply(observations(service.take(slow)));
    assert_eq!(current.revision(), 30);
    assert_eq!(
        current.items().get(&row(32, 'z').key),
        Some(&row(32, 'z').row)
    );
    assert_eq!(observations(service.take(fast)), [patch(29, 32, 'z')]);
    assert!(service.attached(slow));
}

#[test]
fn retained_repairs_wait_for_the_previous_baseline_and_the_cooldown() {
    let (service, intents) = service(8);
    let scope = TranscriptScope::Root;
    let baseline = snapshot(1, 32, 'a');
    service.observe(baseline.clone());
    let id = service.attach(&root(), Face::Remote).unwrap();
    let note_gap = || {
        service
            .lock()
            .attachments
            .get_mut(&id)
            .unwrap()
            .gapped
            .insert(scope.clone());
    };
    let now = std::time::Instant::now();
    note_gap();
    service.repair_gaps_at(now);
    assert_eq!(
        observations(service.take(id)),
        std::slice::from_ref(&baseline)
    );
    note_gap();
    service.repair_gaps_at(now);
    assert_eq!(
        observations(service.take(id)),
        std::slice::from_ref(&baseline)
    );
    let after = service.lock().attachments[&id].retained_after.unwrap();
    note_gap();
    service.repair_gaps_at(now);
    assert_eq!(service.take(id), [] as [Outbound<Core>; 0]);
    service.repair_gaps_at(after);
    assert_eq!(observations(service.take(id)), [baseline]);
    assert!(intents.try_recv().is_err());
}
