#![allow(
    clippy::too_many_lines,
    reason = "Recovery scenarios reconstruct a retained approval after owner loss and verify the original admission survives."
)]

use super::*;
use crate::runtime::recover_tests::{script, scripted_host_with};
use crate::runtime::test_kit::TestExtension;
use crate::sessions;
use crate::surface_protocol::HostEvent;
use plakous_host_protocol::status::HostPhase;

#[test]
fn in_place_recovery_drives_a_retained_approval_after_owner_loss() {
    retained_approval_after_owner_loss(false);
}

#[test]
fn session_switch_drives_a_retained_approval_without_idle_or_ready() {
    retained_approval_after_owner_loss(true);
}

fn retained_approval_after_owner_loss(switch: bool) {
    let root = tempfile::tempdir().unwrap();
    let script = std::sync::Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": 1, "tool_policy": "decision_required", "steps": [
                {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls"},
                {"output":{"text":"retained approval completed"}}
            ]
        }))
        .unwrap(),
    );
    let (config, evidence) = scripted_host_with(root.path(), std::sync::Arc::clone(&script));
    let (controls, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (sent, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut emitter = super::super::Sender::over(sent);
    emitter.recorded = Some(recorded.clone());
    let mut host = HostLoop::open(config, &actions, emitter.clone(), None).unwrap();
    let turn_id = host
        .executor
        .block_on(async {
            let mut turn = host
                .opened
                .agent()
                .unwrap()
                .prompt("retain this decision")
                .await
                .unwrap();
            let mut events = turn.take_events().unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if matches!(
                        events.recv().await.unwrap().kind(),
                        plakous::agent::AgentEventKind::EffectDecisionRequired { .. }
                    ) {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            let id = turn.id();
            host.opened
                .agent()
                .unwrap()
                .abandon_and_release()
                .await
                .unwrap();
            id
        })
        .unwrap();
    let target = host.config.session.id.clone();
    if switch {
        host.config.store.save(&mut host.config.session).unwrap();
        let (mut source, _) = scripted_host_with(root.path(), script);
        source.store = host.config.store.clone();
        source.runtime_factory = host.config.runtime_factory.clone();
        host = HostLoop::open(source, &actions, emitter, None).unwrap();
    }
    observed.try_iter().for_each(drop);
    recorded.lock().unwrap().clear();
    let responder = controls.clone();
    let approval_worker = std::thread::spawn(move || {
        let mut offered = false;
        let mut before_offer = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Ok(event) = observed.recv_timeout(std::time::Duration::from_millis(20)) {
                let HostEvent::ApprovalRequired { approval_id, .. } = event else {
                    before_offer.push(event);
                    continue;
                };
                responder
                    .send(crate::surface_protocol::UserAction::ResolveApproval {
                        approval_id,
                        choice: plakous_host_protocol::ApprovalChoice::Allow,
                    })
                    .unwrap();
                offered = true;
                break;
            }
        }
        (offered, observed, before_offer)
    });
    host.recovery_blocked = true;
    let outcome = if switch {
        crate::runtime::clear::resume_session_switch(
            &host.executor,
            &mut host.config,
            &mut host.opened,
            &mut host.stderr_buf,
            &host.events,
            &target,
        )
        .and_then(|()| host.drive_restored_turn())
    } else {
        host.recover_session(RecoveryTrigger::Explicit)
    };
    let pending = host.opened.has_recovered_turn();
    let feed = host
        .executor
        .block_on(crate::agent::read_tree_feed(
            host.opened.sessions(),
            host.opened.locator().session_id(),
            0,
        ))
        .unwrap()
        .unwrap();
    host.release().unwrap();
    let (offered, observed, before_offer) = approval_worker.join().unwrap();
    let published = recorded.lock().unwrap();
    let offer = published
        .iter()
        .position(|event| matches!(event, ContentEvent::ApprovalRequired { .. }))
        .expect("restored decision publication");
    assert!(
        !published[..offer]
            .iter()
            .any(|event| matches!(event, ContentEvent::SetBusy(false))
                || matches!(event, ContentEvent::Status { phase, .. } if matches!(phase, HostPhase::Idle))),
        "restored decision publication was announced idle: {:?}",
        &published[..offer]
    );
    drop(published);
    assert!(
        !before_offer
            .iter()
            .any(|event| matches!(event, HostEvent::SetBusy(false))
                || matches!(event, HostEvent::Status { phase, .. } if matches!(phase, HostPhase::Idle))),
        "restored decision was announced idle: {before_offer:?}"
    );
    let tail = observed.try_iter().collect::<Vec<_>>();
    assert!(
        !tail.iter().any(|event| matches!(event,
            HostEvent::Row(TranscriptRow::Notice(notice))
                if notice.text.contains("approval accepted")
        )),
        "uncorrelated decision received a command reply: {tail:?}"
    );
    assert!(outcome.is_ok(), "{:?}", outcome.as_ref().err());
    assert!(
        offered && !pending,
        "in-place recovery left its decision unobserved"
    );
    assert_eq!(feed.records.iter().flat_map(plakous::sessions::feed::FeedRecord::facts).filter(|fact| matches!(fact,
        plakous::sessions::feed::FeedFact::TurnTerminal { turn_id: id, outcome: plakous::sessions::feed::FeedTurnOutcome::Completed { .. }, .. } if *id == turn_id
    )).count(), 1);
    assert_eq!(
        tail.iter()
            .filter(|event| matches!(event, HostEvent::TurnLifecycle(_)))
            .count(),
        1
    );
    let evidence = plakous_offline_crash_harness::evidence::read_evidence(evidence).unwrap();
    assert_eq!(
        evidence
            .iter()
            .filter(|row| row.kind
                == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched)
            .count(),
        2
    );
    assert_eq!(
        evidence
            .iter()
            .filter(|row| row.kind
                == plakous_offline_crash_harness::evidence::EvidenceKind::EffectDispatched)
            .count(),
        1
    );
}

fn assert_one_jsonl_recovery_note(events: &[HostEvent<TestExtension>], with_submission: bool) {
    let notices = events
        .iter()
        .filter(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(row)) if row.text.contains("recovered an interrupted turn")))
        .count();
    assert_eq!(
        notices, 1,
        "JSONL recovery has one status note (with submission: {with_submission})"
    );
}

#[test]
fn in_process_recovery_has_one_notice_with_or_without_submission() {
    for with_submission in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (config, evidence) = scripted_host_with(
            root.path(),
            script(serde_json::json!([
                {"fault": {"at": "in_band", "kind": "outcome_unknown", "after_events": 2}}
            ])),
        );
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &rx, super::super::Sender::over(tx), None).unwrap();
        if with_submission {
            let item = sessions::QueueItem::new("interrupted prompt");
            host.config.session.pending_queue.push(item.clone());
            let submission = sessions::projector::Submission {
                command_id: "recover-root".into(),
                kind: sessions::projector::SubmissionKind::RootStart,
                queue_item_id: item.id.clone(),
                queue_item_revision: item.revision,
                target_turn_id: None,
                canonical_command: item.text.clone(),
                submitted_at_ms: item.submitted_at_ms,
            };
            host.config
                .store
                .save_submission(&mut host.config.session, &submission)
                .unwrap();
        }
        let result = host
            .executor
            .block_on(async {
                host.opened
                    .agent()
                    .unwrap()
                    .start(
                        plakous::agent::CommandId::new("recover-root").unwrap(),
                        "interrupted prompt",
                    )
                    .await
                    .unwrap()
                    .into_turn()
                    .unwrap()
                    .result()
                    .await
                    .unwrap()
            })
            .unwrap();
        assert!(matches!(
            result.outcome(),
            plakous::agent::TurnResultOutcome::RecoveryRequired(_)
        ));
        observed.try_iter().for_each(drop);
        host.recover_session(RecoveryTrigger::Explicit).unwrap();
        let events: Vec<_> = observed.try_iter().collect();
        assert_one_jsonl_recovery_note(&events, with_submission);
        assert_eq!(events.iter().filter(|event| matches!(event,
            HostEvent::Row(row) if row.text().is_some_and(|text| text.contains("recovered an interrupted turn"))
        )).count(), 1, "recovery must publish exactly one operator notice (with submission: {with_submission})");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, HostEvent::Recovered { claim: Some(_), .. }))
                .count(),
            1
        );
        let saved = host.config.store.load(&host.config.session.id).unwrap();
        assert_eq!(
            saved
                .transcript
                .iter()
                .filter(|row| row
                    .text()
                    .is_some_and(|text| text.contains("recovered an interrupted turn")))
                .count(),
            0,
            "the recovery-required terminal derived its row; abandoning it derives none (with submission: {with_submission})"
        );
        let dispatches = plakous_offline_crash_harness::evidence::read_evidence(&evidence)
            .unwrap()
            .iter()
            .filter(|record| {
                record.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
            })
            .count();
        assert_eq!(dispatches, 1, "recover must not redispatch the request");
        host.release().unwrap();
    }
}
