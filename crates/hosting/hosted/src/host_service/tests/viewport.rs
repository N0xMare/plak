use std::time::{Duration, Instant};

use super::super::attachment::{AttachmentStatus, SentBatch, SentTranscript};
use super::super::retained::ViewportBase;
use super::*;

#[test]
fn viewport_catchup_freezes_before_new_publications_and_eviction() {
    let (service, _intents) = service(2);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let stamp = service.viewport_read(&root(), 3).unwrap();
    let line = Arc::<str>::from("current");
    let attach = |base| {
        service
            .install_viewport(&stamp, line.clone(), base, Face::Remote)
            .unwrap()
    };
    service.observe(root_patch(1));
    let first = attach(ViewportBase::FirstPage(1)).unwrap();
    assert_eq!(first.revision.revision, 1);
    assert_eq!(first.patches[0].to_revision, 2);
    service.detach(first.attachment).unwrap();

    service.observe(root_patch(2));
    let replay = attach(ViewportBase::Replay(&first.revision)).unwrap();
    assert_eq!(
        replay
            .patches
            .iter()
            .map(|patch| (patch.from_revision, patch.to_revision))
            .collect::<Vec<_>>(),
        [(1, 2), (2, 3)]
    );
    service.observe(root_patch(3));
    assert!(
        matches!(&service.take(replay.attachment)[..], [Outbound::Event { seq: 3, observation, .. }]
        if *observation == root_patch(3))
    );
    service.detach(replay.attachment).unwrap();

    for from in 4..=70 {
        service.observe(root_patch(from));
    }
    assert!(attach(ViewportBase::Replay(&first.revision)).is_none());
    assert!(attach(ViewportBase::FirstPage(71)).is_some());
}

#[test]
fn viewport_rejects_wrong_authority_gap_and_reset() {
    let (service, _intents) = service(8);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let line = Arc::<str>::from("head and prune fence");
    let stamp = service.viewport_read(&root(), 3).unwrap();
    let first = service
        .install_viewport(
            &stamp,
            line.clone(),
            ViewportBase::FirstPage(1),
            Face::Remote,
        )
        .unwrap()
        .unwrap();
    let start = first.revision;
    service.detach(first.attachment).unwrap();
    let replay = |read: &wire::ViewportRead, line: Arc<str>, start: &wire::ViewportRevision| {
        service
            .install_viewport(read, line, ViewportBase::Replay(start), Face::Remote)
            .unwrap()
            .is_some()
    };
    assert!(
        service
            .viewport_read(
                &NodeAddress {
                    agent_id: "child".into(),
                    ..root()
                },
                3
            )
            .is_none()
    );
    assert!(!replay(
        &service.viewport_read(&root(), 4).unwrap(),
        line.clone(),
        &start
    ));
    assert!(!replay(&stamp, "changed head or prune".into(), &start));
    assert!(!replay(
        &stamp,
        line.clone(),
        &wire::ViewportRevision {
            token: uuid::Uuid::nil(),
            ..start.clone()
        }
    ));
    assert!(!replay(
        &stamp,
        line.clone(),
        &wire::ViewportRevision {
            revision: 2,
            ..start.clone()
        }
    ));

    service.observe(root_patch(2)); // producer gap invalidates the whole chain
    assert!(!replay(&stamp, line.clone(), &start));
    let current = service.viewport_read(&root(), 3).unwrap();
    assert!(!replay(&current, line.clone(), &start));
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(4),
    ));
    assert!(
        service
            .install_viewport(
                &current,
                line.clone(),
                ViewportBase::FirstPage(3),
                Face::Remote
            )
            .unwrap()
            .is_none()
    ); // a capture started before the reset
    let reset = service.viewport_read(&root(), 3).unwrap();
    let first = service
        .install_viewport(&reset, line, ViewportBase::FirstPage(4), Face::Remote)
        .unwrap()
        .unwrap();
    assert_eq!(first.patches, [] as [Arc<TranscriptPatch>; 0]);
    assert_eq!(service.take(first.attachment), [] as [Outbound<Core>; 0]);
}

#[test]
fn same_revision_repair_preserves_tokens_and_an_in_flight_page() {
    let (service, _intents) = service(8);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let stamp = service.viewport_read(&root(), 3).unwrap();
    let attach = |base| {
        service
            .install_viewport(&stamp, "line".into(), base, Face::Remote)
            .unwrap()
            .unwrap()
    };
    let first = attach(ViewportBase::FirstPage(1));
    service.observe(root_patch(1));
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(2),
    ));
    let second = attach(ViewportBase::FirstPage(1));
    let replay = attach(ViewportBase::Replay(&first.revision));
    assert_eq!(second.patches, replay.patches);
    assert_eq!(replay.patches.len(), 1);
    assert_eq!(replay.patches[0].to_revision, 2);
}

#[test]
fn opening_clear_and_holding_boundaries_require_new_viewports() {
    for boundary in [
        Some(Observation::SessionOpened {
            session_open_ms: None,
        }),
        Some(Observation::ClearView),
        None,
    ] {
        let (service, _intents) = service(8);
        service.observe(Observation::PresentationSnapshot(
            PresentationSnapshot::empty(1),
        ));
        let stamp = service.viewport_read(&root(), 3).unwrap();
        let attach = |read, base| {
            service
                .install_viewport(read, "line".into(), base, Face::Remote)
                .unwrap()
        };
        let first = attach(&stamp, ViewportBase::FirstPage(1)).unwrap();
        if let Some(boundary) = boundary {
            service.observe(boundary);
        } else {
            service.hold_attachments(true);
            assert!(attach(&stamp, ViewportBase::Replay(&first.revision)).is_none());
            service.hold_attachments(false);
        }
        assert!(attach(&stamp, ViewportBase::FirstPage(1)).is_none());
        service.observe(Observation::PresentationSnapshot(
            PresentationSnapshot::empty(2),
        ));
        let fresh = service.viewport_read(&root(), 3).unwrap();
        assert!(attach(&fresh, ViewportBase::Replay(&first.revision)).is_none());
        assert!(attach(&fresh, ViewportBase::FirstPage(2)).is_some());
    }
}

#[test]
fn viewport_byte_budget_evicts_old_patches_and_oversize_resets_the_epoch() {
    use super::super::{fanout::Publication, retained::MAX_PATCH_BYTES};
    use plakous_host_protocol::views::transcript::{Lane, RowKey, TranscriptItem, TranscriptRow};

    let (service, _intents) = service(8);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let stamp = service.viewport_read(&root(), 3).unwrap();
    let attach = |read, base| {
        service
            .install_viewport(read, "line".into(), base, Face::Remote)
            .unwrap()
    };
    let first = attach(&stamp, ViewportBase::FirstPage(1)).unwrap();
    for from in 1..=2 {
        service.publish(Publication {
            observation: root_patch(from),
            size: MAX_PATCH_BYTES / 2 + 1,
        });
    }
    assert!(attach(&stamp, ViewportBase::Replay(&first.revision)).is_none());
    let at_two = wire::ViewportRevision {
        revision: 2,
        ..first.revision.clone()
    };
    assert_eq!(
        attach(&stamp, ViewportBase::Replay(&at_two))
            .unwrap()
            .patches
            .len(),
        1
    );
    // Escaped text fits the presentation bound, but not the wire replay budget.
    service.observe(Observation::TranscriptPatch(TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: 3,
        to_revision: 4,
        upserts: vec![TranscriptItem {
            key: RowKey::new(3, Lane::Runtime, 0),
            row: TranscriptRow::assistant("\n".repeat(MAX_PATCH_BYTES / 2)),
        }],
        deletes: Vec::new(),
    }));
    assert!(attach(&stamp, ViewportBase::FirstPage(4)).is_none());
    let fresh = service.viewport_read(&root(), 3).unwrap();
    let at_four = wire::ViewportRevision {
        revision: 4,
        ..first.revision
    };
    assert!(attach(&fresh, ViewportBase::Replay(&at_four)).is_none());
    let page = attach(&fresh, ViewportBase::FirstPage(4)).unwrap();
    assert_eq!(page.patches, [] as [Arc<TranscriptPatch>; 0]);
}

#[test]
fn attachment_meter_counts_queue_loss_and_wire_gaps_without_idle_reports() {
    let (service, _intents) = service(2);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let id = service.attach(&root(), Face::Remote).unwrap();
    let other = service.attach(&root(), Face::Remote).unwrap();
    let now = Instant::now();
    for id in [id, other] {
        service.enable_attachment_status(id, now, 0);
        service.take(id);
    }
    let at = |seconds| now + Duration::from_secs(seconds);
    let status = |seconds, explicit| service.attachment_status(id, at(seconds), explicit);
    assert_eq!(status(0, true), Some(AttachmentStatus::default()));
    assert_eq!(service.status_deadline(id), None);
    let mut initial = SentBatch::default();
    initial.record(SentTranscript::Baseline(Some(1)));
    service.transcript_sent(id, initial);
    service.observe(Observation::AssistantDelta("queued".into()));
    service.observe(Observation::SetBusy(true));
    service.observe(root_patch(1));
    service.observe(root_patch(2));
    service.observe(root_patch(3));
    assert_eq!(service.status_deadline(id), Some(at(10)));
    let queued = status(5, true).unwrap();
    assert_eq!(
        (
            queued.deltas_evicted,
            queued.patches_skipped,
            queued.baselines_sent
        ),
        (1, 3, 1)
    );
    assert_eq!(queued.queue_len, 1);
    assert!(queued.bytes_queued > 0);
    assert_eq!(status(9, false), None);
    let mut batch = SentBatch::default();
    for event in service.take(id) {
        if let Outbound::Event { seq, .. } = event {
            batch.record(SentTranscript::Sequence(seq));
        }
    }
    service.transcript_sent(id, batch);
    let drained = status(10, false).unwrap();
    assert_eq!(
        drained,
        AttachmentStatus {
            bytes_queued: 0,
            queue_len: 0,
            seq_gaps: 1,
            ..queued
        }
    );
    assert_eq!(status(10, true), Some(drained));
    assert_eq!(status(20, false), None);
    assert_eq!(service.status_deadline(id), None);
    let isolated = service.attachment_status(other, at(20), true).unwrap();
    assert_eq!(
        (
            isolated.baselines_sent,
            isolated.seq_gaps,
            isolated.queue_len
        ),
        (0, 0, 1)
    );
}
