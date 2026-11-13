//! Exercise the real unavailable endpoint, not a fabricated compact outcome.

use super::*;

#[test]
fn an_unavailable_compaction_endpoint_preserves_the_item_and_names_the_queue_remedy() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let executor = ApplicationExecutor::new().unwrap();
    let runtime = host_runtime_binding(&config, &mut Vec::new(), RuntimeRole::Primary)
        .block_here()
        .unwrap();
    let opened = executor
        .block_on(crate::agent::build_or_open_bound_session(
            config.state_roots.runtime_sessions().unwrap(),
            None,
            None,
            SessionOperationId::new("unavailable:create").unwrap(),
            runtime,
        ))
        .unwrap()
        .unwrap_or_else(|error| panic!("{}", error.failure.message()));
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    let item = config
        .session
        .enqueue_live("keep this exact input")
        .unwrap();
    config.store.save(&mut config.session).unwrap();
    let locator = opened.locator();
    // A normal public shutdown removes the actual driver endpoint. No store
    // rows, compact results or outgoing notices are substituted by the test.
    executor
        .block_on(opened.agent().unwrap().shutdown())
        .unwrap()
        .unwrap();
    let (events, rx) = host_event_sink();
    let (_actions, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let outcome = executor
        .block_on(super::super::super::run_scheduled_compaction_until_fits(
            InterruptWatch::new(&actions, &state),
            &mut config,
            &opened,
            &mut Vec::new(),
            &events,
            None,
            &UserInput::from(item.text.clone()),
            None,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(outcome, ScheduledPrompt::Parked);
    assert_eq!(
        config.session.pending_queue.as_slice(),
        std::slice::from_ref(&item)
    );
    assert_eq!(
        config.store.load(&config.session.id).unwrap().pending_queue,
        [item]
    );
    assert_eq!(
        config.session.queue_drain_state,
        SessionQueueDrainState::RestoredPaused
    );
    assert_eq!(opened.locator(), locator);
    assert!(
        !plakous_offline_crash_harness::evidence::read_evidence(
            root.path().join("immediate-quit-evidence.jsonl")
        )
        .unwrap()
        .iter()
        .any(|row| row.kind
            == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched)
    );
    let notes: Vec<_> = rx
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(TranscriptRow::Notice(row)) => Some(row),
            _ => None,
        })
        .collect();
    assert_eq!(
        notes
            .iter()
            .filter(|note| note.remedy
                == Some(
                    plakous_host_protocol::notice_remedy::NoticeRemedy::ResumeQueueOrStartFresh {}
                ))
            .count(),
        1,
        "{notes:?}"
    );
}
