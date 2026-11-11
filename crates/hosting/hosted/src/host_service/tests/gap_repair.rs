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

#[test]
fn a_saturated_attachment_bounds_gap_repair_until_a_snapshot_arrives() {
    for scope in [
        TranscriptScope::Root,
        TranscriptScope::Agent("child".into()),
    ] {
        let (service, intents) = service(4);
        let id = service.attach(&root(), Face::Remote).unwrap();
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
