//! A slow attachment cannot turn transcript loss into a mailbox feedback loop.

use super::*;

fn resyncs(intents: &IntentReceiver<Core>, scope: &TranscriptScope) -> usize {
    let mut count = 0;
    while let Ok(delivery) = intents.try_recv() {
        if matches!(&*delivery, SessionIntent::ResyncTranscript { scope: found } if found == scope)
        {
            count += 1;
        }
    }
    count
}

fn attach_with_no_kept_replay(service: &HostService<Core>) -> AttachmentId {
    let id = service.attach(&root(), Face::Remote).unwrap();
    // Finish the initial baseline wait, then discard the replay cache.
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    service.observe(Observation::ClearView);
    service.take(id);
    id
}

#[test]
fn an_attachment_retries_its_missing_initial_baseline_and_resumes_delivery() {
    for ask in [true, false] {
        let (service, intents) = service(4);
        let scope = TranscriptScope::Root;
        let id = service.attach_asking(&root(), Face::Remote, ask).unwrap();
        // Take the initial ask without publishing an answer.
        assert_eq!(resyncs(&intents, &scope), usize::from(ask));
        service.observe(root_patch(1));
        assert_eq!(service.take(id), [] as [Outbound<Core>; 0]);
        let deadline = service.lock().attachments[&id].repairing[&scope];
        service.repair_gaps_at(
            deadline
                .checked_sub(std::time::Duration::from_millis(1))
                .unwrap(),
        );
        assert_eq!(resyncs(&intents, &scope), 0);
        service.repair_gaps_at(deadline);
        assert_eq!(resyncs(&intents, &scope), 1);

        let baseline = Observation::PresentationSnapshot(PresentationSnapshot::empty(2));
        service.observe(baseline.clone());
        service.observe(root_patch(2));
        assert_eq!(observations(service.take(id)), [baseline, root_patch(2)]);
        service.repair_gaps_at(deadline + std::time::Duration::from_secs(10));
        assert_eq!(resyncs(&intents, &scope), 0);
    }
}

#[test]
fn a_saturated_attachment_bounds_gap_repair_until_a_snapshot_arrives() {
    for scope in [
        TranscriptScope::Root,
        TranscriptScope::Agent("child".into()),
    ] {
        let (service, intents) = service(4);
        let id = attach_with_no_kept_replay(&service);
        if let TranscriptScope::Agent(agent_id) = &scope {
            service
                .intent(
                    id,
                    SessionIntent::OpenAgentTranscript {
                        agent_id: agent_id.clone(),
                    },
                )
                .unwrap();
        }
        service.take(id);
        resyncs(&intents, &scope);
        let mut asks = 0;
        for _ in 0..1000 {
            service.observe(stream(scope.clone()));
            asks += resyncs(&intents, &scope);
        }
        assert_eq!(asks, 0, "a saturated queue must wait for consumer progress");
        service.take(id);
        asks += resyncs(&intents, &scope);
        assert_eq!(asks, 1);
        for _ in 0..20 {
            for _ in 0..16 {
                service.observe(stream(scope.clone()));
            }
            service.take(id);
            asks += resyncs(&intents, &scope);
        }
        assert_eq!(asks, 1, "only one repair may await its snapshot per scope");
        let mut snapshot = PresentationSnapshot::empty(2);
        snapshot.scope = scope.clone();
        service.observe(Observation::PresentationSnapshot(snapshot.clone()));
        assert!(
            observations(service.take(id)).contains(&Observation::PresentationSnapshot(snapshot))
        );
        // Root attaches no longer have a retained baseline after ClearView.
        service.observe(Observation::ClearView);
        service.take(id);
        for _ in 0..16 {
            service.observe(stream(scope.clone()));
        }
        service.take(id);
        asks += resyncs(&intents, &scope);
        assert_eq!(asks, 2, "a later loss still receives a fresh repair");
        assert!(service.attached(id));
    }
}

#[test]
fn an_unanswered_gap_repair_retries_after_its_deadline_without_new_loss() {
    let (service, intents) = service(4);
    let id = attach_with_no_kept_replay(&service);
    let scope = TranscriptScope::Root;
    service.take(id);
    resyncs(&intents, &scope);
    for _ in 0..16 {
        service.observe(stream(scope.clone()));
    }
    service.take(id);
    assert_eq!(resyncs(&intents, &scope), 1);
    let deadline = service.lock().attachments[&id].repairing[&scope];
    service.repair_gaps_at(
        deadline
            .checked_sub(std::time::Duration::from_millis(1))
            .unwrap(),
    );
    assert_eq!(resyncs(&intents, &scope), 0);
    service.repair_gaps_at(deadline);
    assert_eq!(resyncs(&intents, &scope), 1);
    service.repair_gaps_at(deadline);
    assert_eq!(resyncs(&intents, &scope), 0);
}

#[test]
fn a_snapshot_clears_losses_recorded_while_its_repair_was_outstanding() {
    for scope in [
        TranscriptScope::Root,
        TranscriptScope::Agent("child".into()),
    ] {
        let (service, intents) = service(4);
        let id = attach_with_no_kept_replay(&service);
        if let TranscriptScope::Agent(agent_id) = &scope {
            service
                .intent(
                    id,
                    SessionIntent::OpenAgentTranscript {
                        agent_id: agent_id.clone(),
                    },
                )
                .unwrap();
        }
        service.take(id);
        resyncs(&intents, &scope);
        for _ in 0..16 {
            service.observe(stream(scope.clone()));
        }
        service.take(id);
        assert_eq!(resyncs(&intents, &scope), 1);
        for _ in 0..16 {
            service.observe(stream(scope.clone()));
        }
        let mut snapshot = PresentationSnapshot::empty(2);
        snapshot.scope = scope.clone();
        service.observe(Observation::PresentationSnapshot(snapshot));
        service.take(id);
        service.observe(Observation::ClearView);
        service.take(id);
        assert_eq!(resyncs(&intents, &scope), 0);
    }
}

#[test]
fn patch_pressure_asks_for_a_fresh_revision_without_ending_the_attachment() {
    use plakous_host_protocol::views::transcript::TranscriptPatch;
    let (service, intents) = service(2);
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(1),
    ));
    let id = service.attach(&root(), Face::Remote).unwrap();
    service.take(id);
    let patch = |from_revision| {
        Observation::TranscriptPatch(TranscriptPatch {
            scope: TranscriptScope::Root,
            from_revision,
            to_revision: from_revision + 1,
            upserts: Vec::new(),
            deletes: Vec::new(),
        })
    };
    for revision in 1..=4 {
        service.observe(patch(revision));
    }
    assert!(service.attached(id));
    assert_eq!(service.take(id), [] as [Outbound<Core>; 0]);
    assert_eq!(resyncs(&intents, &TranscriptScope::Root), 1);
    let snapshot = Observation::PresentationSnapshot(PresentationSnapshot::empty(5));
    service.observe(snapshot.clone());
    service.observe(patch(5));
    assert_eq!(observations(service.take(id)), vec![snapshot, patch(5)]);
    assert!(service.attached(id));
}
