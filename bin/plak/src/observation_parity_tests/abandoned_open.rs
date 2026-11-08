//! Recovery evidence independent of a terminal renderer or its feature.

use super::*;
use plakous_hosted::sessions::QueueItem;

pub(crate) struct AbandonedOpen {
    _fixture: Fixture,
    pub(crate) events: Vec<HostEvent>,
    store: Arc<dyn crate::store::session_records::SessionRecordStore>,
    id: String,
}

impl AbandonedOpen {
    pub(crate) fn record(&self) -> SessionRecord {
        self.store.load(&self.id).unwrap()
    }
}

pub(crate) fn recover(with_submission: bool) -> AbandonedOpen {
    let fixture = Fixture::new(Script::Effect);
    let mut config = fixture.host_config();
    let store = config.store.clone();
    let id = config.session.id.clone();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    actions
        .send(UserAction::Prompt("seed terminal".into()))
        .unwrap();
    let mut started = false;
    loop {
        let event = events.recv_timeout(STEP_TIMEOUT).unwrap();
        if matches!(event, HostEvent::PromptStarted(_)) {
            started = true;
        }
        if started
            && matches!(event, HostEvent::Status { ref phase, .. } if matches!(phase, HostPhase::Idle))
        {
            break;
        }
    }
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
    config = fixture.host_config();
    config.session = store.load(&id).unwrap();
    let source = config.session.plakous_locator.unwrap();
    let database = PlakPaths::explicit(config.product_home.clone()).runtime_database();
    let executor = tokio::runtime::Runtime::new().unwrap();
    executor
        .block_on(plakous_offline_crash_harness::seed_fenced_code_cell(
            &database,
            source,
            "face-fenced-root",
            b"fenced face prompt",
            false,
        ))
        .unwrap();
    if with_submission {
        let item = QueueItem::new("fenced face prompt");
        config.session.pending_queue.push(item.clone());
        let submission = plakous_hosted::sessions::projector::Submission {
            command_id: "face-fenced-root".into(),
            kind: plakous_hosted::sessions::projector::SubmissionKind::RootStart,
            queue_item_id: item.id.clone(),
            queue_item_revision: item.revision,
            target_turn_id: None,
            canonical_command: item.text.clone(),
            submitted_at_ms: item.submitted_at_ms,
        };
        store
            .save_submission(&mut config.session, &submission)
            .unwrap();
    }
    store.save(&mut config.session).unwrap();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
    AbandonedOpen {
        store,
        id,
        events: events.try_iter().collect(),
        _fixture: fixture,
    }
}

#[test]
fn abandoned_recovery_reaches_host_and_jsonl_once_without_persisting_a_duplicate() {
    for with_submission in [false, true] {
        let recovered = recover(with_submission);
        assert_eq!(
            recovered
                .events
                .iter()
                .filter(|event| matches!(event, HostEvent::Recovered { .. }))
                .count(),
            1
        );
        let recovery_row = |event: &&HostEvent| matches!(event, HostEvent::Row(row) if row.text().is_some_and(|text| text.contains("recovered an interrupted turn")));
        assert_eq!(recovered.events.iter().filter(recovery_row).count(), 1);
        assert_eq!(
            recovered
                .events
                .iter()
                .filter(recovery_row)
                .filter_map(|event| project_host_event(event, JsonlCorrelation::default(), None))
                .filter(|line| line.contains("recovered an interrupted turn"))
                .count(),
            1
        );
        assert_eq!(
            recovered
                .record()
                .transcript
                .iter()
                .filter(|row| row
                    .text()
                    .is_some_and(|text| text.contains("recovered an interrupted turn")))
                .count(),
            0,
            "abandonment derives no stored notice (submission: {with_submission})"
        );
    }
}
