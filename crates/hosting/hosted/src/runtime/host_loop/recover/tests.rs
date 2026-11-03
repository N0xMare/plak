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
    let mut host = RootActor::open_blocking(config, &actions, emitter.clone(), None).unwrap();
    let turn_id = host
        .role
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
    let target = host.role.config.session.id.clone();
    if switch {
        host.role
            .config
            .store
            .save(&mut host.role.config.session)
            .unwrap();
        let (mut source, _) = scripted_host_with(root.path(), script);
        source.store = host.role.config.store.clone();
        source.runtime_factory = host.role.config.runtime_factory.clone();
        host = RootActor::open_blocking(source, &actions, emitter, None).unwrap();
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
    host.role.recovery = RootRecovery::Due;
    let outcome = if switch {
        host.role
            .executor
            .block_on(crate::runtime::clear::resume_session_switch(
                &mut host.role.config,
                &mut host.opened,
                &mut host.role.stderr_buf,
                &host.role.events,
                &target,
            ))
            .expect("the executor drives the call")
            .and_then(|()| host.block(async |host| host.drive_restored_turn().await))
    } else {
        host.block(async |host| host.recover_session(RecoveryTrigger::Explicit).await)
    };
    let pending = host.opened.has_recovered_turn();
    let feed = host
        .role
        .executor
        .block_on(crate::agent::read_tree_feed(
            host.opened.sessions(),
            host.opened.locator().session_id(),
            0,
        ))
        .unwrap()
        .unwrap();
    host.block(async |host| host.release().await).unwrap();
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
        let mut host =
            RootActor::open_blocking(config, &rx, super::super::Sender::over(tx), None).unwrap();
        if with_submission {
            let item = sessions::QueueItem::new("interrupted prompt");
            host.role.config.session.pending_queue.push(item.clone());
            let submission = sessions::projector::Submission {
                command_id: "recover-root".into(),
                kind: sessions::projector::SubmissionKind::RootStart,
                queue_item_id: item.id.clone(),
                queue_item_revision: item.revision,
                target_turn_id: None,
                canonical_command: item.text.clone(),
                submitted_at_ms: item.submitted_at_ms,
            };
            host.role
                .config
                .store
                .save_submission(&mut host.role.config.session, &submission)
                .unwrap();
        }
        let result = host
            .role
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
        host.block(async |host| host.recover_session(RecoveryTrigger::Explicit).await)
            .unwrap();
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
        let saved = host
            .role
            .config
            .store
            .load(&host.role.config.session.id)
            .unwrap();
        assert_eq!(
            saved
                .transcript
                .iter()
                .filter(|row| row
                    .text()
                    .is_some_and(|text| text.contains("recovered an interrupted turn")))
                .count(),
            1,
            "the projector derives the fenced turn's recovery line once from its closure (with submission: {with_submission})"
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
        host.block(async |host| host.release().await).unwrap();
    }
}

/// Refuses only the views read the in-place reopen starts with.
struct RefuseViews;

impl crate::runtime::fault_store::StoreFaults for RefuseViews {
    fn before(
        &self,
        call: crate::runtime::fault_store::StoreCall,
        _: Option<&sessions::SessionRecord>,
    ) -> Result<(), String> {
        if matches!(call, crate::runtime::fault_store::StoreCall::LoadViews) {
            Err("injected unreadable views".to_owned())
        } else {
            Ok(())
        }
    }
}

/// A fenced root reopens itself at idle entry with no action. When that
/// eager reopen fails it is reported and left retrying, and the next action
/// that needs the session reopens it first and then runs.
#[test]
fn a_failed_eager_reopen_is_retried_by_the_next_action() {
    use crate::runtime::host_loop::RootRecovery;
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([{"output": {"text": "after the retry"}}])),
    );
    let (controls, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (sent, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        RootActor::open_blocking(config, &actions, super::super::Sender::over(sent), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    let durable = std::sync::Arc::clone(&host.role.config.store);
    host.role.config.store = std::sync::Arc::new(crate::runtime::fault_store::FaultStore::new(
        std::sync::Arc::clone(&durable),
        RefuseViews,
    ));
    host.role.recovery = RootRecovery::Due;
    observed.try_iter().for_each(drop);

    assert!(matches!(
        host.block(async |host| host.next_prompt(None).await)
            .unwrap(),
        HostStep::Continue
    ));
    assert_eq!(host.role.recovery, RootRecovery::Retrying);
    let failed = observed.try_iter().collect::<Vec<_>>();
    assert!(
        failed.iter().any(|event| matches!(event,
            HostEvent::Row(TranscriptRow::Notice(notice))
                if notice.text.contains("could not be reopened"))),
        "{failed:#?}"
    );
    assert!(
        !failed
            .iter()
            .any(|event| matches!(event, HostEvent::Recovered { .. }))
    );

    host.role.config.store = durable;
    controls
        .send(crate::surface_protocol::UserAction::Prompt(
            "after the retry".into(),
        ))
        .unwrap();
    let step = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();
    assert!(
        matches!(step, HostStep::Prompt { .. }),
        "the prompt still runs"
    );
    assert_eq!(host.role.recovery, RootRecovery::Clear);
    assert!(
        observed
            .try_iter()
            .any(|event| matches!(event, HostEvent::Recovered { .. })),
        "the action reopened the session first"
    );
    host.block(async |host| host.release().await).unwrap();
}

/// A quit already deferred when the root enters idle fenced is answered
/// first: the actor quits without the eager reopen, which the next open does.
#[test]
fn a_deferred_quit_skips_the_eager_reopen() {
    use crate::runtime::host_loop::RootRecovery;
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
    let (controls, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (sent, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        RootActor::open_blocking(config, &actions, super::super::Sender::over(sent), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    host.role.recovery = RootRecovery::Due;
    // The fenced turn's own watch drained the quit.
    controls
        .send(crate::surface_protocol::UserAction::Quit)
        .unwrap();
    crate::interrupt::InterruptWatch::new(&actions, &host.role.interrupt_state).poll();
    assert!(host.role.interrupt_state.borrow().has_deferred_quit());
    observed.try_iter().for_each(drop);

    let step = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();

    assert!(matches!(step, HostStep::Quit));
    assert_eq!(host.role.recovery, RootRecovery::Due, "no eager reopen ran");
    assert!(
        !observed
            .try_iter()
            .any(|event| matches!(event, HostEvent::Recovered { .. }))
    );
    host.block(async |host| host.release().await).unwrap();
}

/// Sends an interrupt and a quit once, while the in-place reopen reads the
/// views it starts with.
struct SendDuringReopen(
    std::sync::Mutex<Option<crate::surface_protocol::UserActionSender<TestExtension>>>,
);

impl crate::runtime::fault_store::StoreFaults for SendDuringReopen {
    fn before(
        &self,
        call: crate::runtime::fault_store::StoreCall,
        _: Option<&sessions::SessionRecord>,
    ) -> Result<(), String> {
        if matches!(call, crate::runtime::fault_store::StoreCall::LoadViews)
            && let Some(controls) = self.0.lock().unwrap().take()
        {
            controls
                .send(crate::surface_protocol::UserAction::Quit)
                .unwrap();
            controls
                .send(crate::surface_protocol::UserAction::Interrupt)
                .unwrap();
        }
        Ok(())
    }
}

/// The eager reopen is watched: an interrupt and a quit sent while it runs
/// are drained and left for the idle dispatcher, which answers them after the
/// reopen, the urgent interrupt first.
#[test]
fn an_interrupt_and_quit_sent_during_the_eager_reopen_are_answered_after_it() {
    use crate::runtime::host_loop::RootRecovery;
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
    let (controls, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (sent, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        RootActor::open_blocking(config, &actions, super::super::Sender::over(sent), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    let durable = std::sync::Arc::clone(&host.role.config.store);
    host.role.config.store = std::sync::Arc::new(crate::runtime::fault_store::FaultStore::new(
        std::sync::Arc::clone(&durable),
        SendDuringReopen(std::sync::Mutex::new(Some(controls))),
    ));
    host.role.recovery = RootRecovery::Due;
    observed.try_iter().for_each(drop);

    let step = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();

    assert_eq!(host.role.recovery, RootRecovery::Clear, "the reopen ran");
    assert!(
        observed
            .try_iter()
            .any(|event| matches!(event, HostEvent::Recovered { .. }))
    );
    // The interrupt was answered at idle; the quit waits, drained, behind it.
    assert!(matches!(step, HostStep::Continue));
    assert!(host.role.interrupt_state.borrow().has_deferred_quit());
    let step = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();
    assert!(matches!(step, HostStep::Quit));
    host.role.config.store = durable;
    host.block(async |host| host.release().await).unwrap();
}
