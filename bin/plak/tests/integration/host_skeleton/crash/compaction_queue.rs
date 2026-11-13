//! Correlate durable queue identities with wire admission at a paid-fold cut.

use super::*;
use plak::store::session_records::SessionRecordStore as _;
use plakous_host_protocol::SessionObservation;
use plakous_host_protocol::wire::{ClientDraftId, RequestRefusal};
use plakous_hosted::sessions::QueueItem as StoredQueueItem;

const SEED: &str = "seed for compaction owner loss";
const TRIGGER: &str = "input waiting for the paid fold";
const DEFERRED: &str = "queued while owner compacts";

fn intent(
    attachment: u64,
    client_id: ClientDraftId,
    intent: SessionIntent<PlakExtension>,
) -> Op<PlakExtension> {
    Op::Intent {
        client_id: Some(client_id),
        attachment,
        intent,
        blobs: Vec::new(),
    }
}

fn raw_queue(fixture: &Fixture, session: &str) -> Vec<(String, i64, String)> {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let connection = rusqlite::Connection::open_with_flags(
        paths.product_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .unwrap();
    connection
        .prepare(
            "SELECT item_id,revision,text FROM queue_items WHERE session_id=?1 ORDER BY position",
        )
        .unwrap()
        .query_map([session], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn summary_dispatches(fixture: &Fixture) -> usize {
    match read_evidence(fixture.root.join("evidence.summary.jsonl")) {
        Ok(rows) => rows.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("invalid summary dispatch evidence: {error}"),
    }
}

fn compacting(fixture: Fixture, queued: bool) -> (Fixture, Host, Client, String, NodeAddress, u64) {
    let gate = fixture.root.join("seed-release");
    fixture.write_script(&json!({
        "version":1, "maximum_projected_input":100_000,
        "scheduled_compaction":true, "content_sensitive_projection":true,
        "fold":{"stall":300_000}, "steps":[
            {"output":{"text":format!("{}SEED_ONE", "detail ".repeat(10000))}, "usage":{}},
            {"stream":if queued { json!([{"text":"SEED_BUSY"},{"gate":{"file":gate}}]) } else { json!([]) },
             "output":{"text":"SEED_DONE"}, "usage":{"input":95000}},
            {"output":{"text":"TRIGGER_DONE"}, "usage":{}}
        ]
    }));
    let host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, SEED, "SEED_ONE");
    profiles::wait_idle(&mut client, &session);
    client.prompt(
        30,
        attachment,
        "protect the latest answer",
        if queued { "SEED_BUSY" } else { "SEED_DONE" },
    );
    client.retain_observations();
    if !queued {
        profiles::wait_idle(&mut client, &session);
    }
    let trigger_id = ClientDraftId::new();
    let trigger = intent(
        attachment,
        trigger_id,
        if queued {
            SessionIntent::FollowUp(TRIGGER.into())
        } else {
            SessionIntent::Prompt(TRIGGER.into())
        },
    );
    assert_eq!(
        client.request(4, trigger.clone()),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    if queued {
        loop {
            if let HostFrame::Event {
                observation: SessionObservation::DraftAccepted(SessionIntent::FollowUp(draft)),
                ..
            } = client.next()
                && draft.text == TRIGGER
            {
                break;
            }
        }
        assert_eq!(
            client.request(40, trigger.clone()),
            Outcome::Ok(Answer::Accepted { duplicate: true })
        );
        assert_eq!(
            raw_queue(&fixture, &session)
                .iter()
                .filter(|item| item.2 == TRIGGER)
                .count(),
            1
        );
        std::fs::write(gate, b"release").unwrap();
    }
    let deadline = Instant::now() + WAIT;
    while summary_dispatches(&fixture) == 0 {
        assert!(
            Instant::now() < deadline,
            "the paid summary never dispatched"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(summary_dispatches(&fixture), 1);
    assert_eq!(dispatches(&fixture), 3);
    assert_eq!(
        client.request(41, trigger),
        Outcome::Ok(Answer::Accepted { duplicate: true })
    );
    (fixture, host, client, session, node, attachment)
}

fn queue_snapshot(
    client: &mut Client,
) -> (
    Vec<plakous_host_protocol::QueueItem>,
    plakous_host_protocol::QueueDrainState,
) {
    loop {
        let frame = client.next();
        if let HostFrame::Event {
            observation: SessionObservation::QueueSnapshot { items, drain },
            ..
        } = frame
        {
            return (items, drain);
        }
    }
}

#[test]
fn the_small_owner_loss_fixture_does_not_dispatch_a_paid_summary() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version":1, "maximum_projected_input":20000,
        "scheduled_compaction":true, "content_sensitive_projection":true,
        "fold":{"stall":300_000}, "steps":[
            {"output":{"text":format!("{}SEED_DONE", "detail ".repeat(800))}, "usage":{"input":19000}},
            {"output":{"text":"TRIGGER_DONE"}, "usage":{}}
        ]
    }));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, SEED, "SEED_DONE");
    profiles::wait_idle(&mut client, &session);
    client.prompt(4, attachment, TRIGGER, "TRIGGER_DONE");
    profiles::wait_idle(&mut client, &session);
    assert_eq!(
        dispatches(&fixture),
        2,
        "two ordinary turns, not a turn plus a paid summary"
    );
    assert_eq!(summary_dispatches(&fixture), 0);
    consumed_followup(&fixture, &mut client, attachment, &session);
    let store = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    assert_eq!(
        store.load(&session).unwrap().pending_queue,
        [] as [StoredQueueItem; 0]
    );
    kill(&mut host);
    drop(client);
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    client.attach(1, &node);
    let record = store.load(&session).unwrap();
    assert_eq!(record.pending_queue, [] as [StoredQueueItem; 0]);
    assert_eq!(
        record
            .transcript
            .rows()
            .iter()
            .filter(|(row, _)| row.text() == Some(DEFERRED))
            .count(),
        1
    );
    assert_eq!(dispatches(&fixture), 3);
    stop(&mut host);
}

fn consumed_followup(fixture: &Fixture, client: &mut Client, attachment: u64, session: &str) {
    let op = intent(
        attachment,
        ClientDraftId::new(),
        SessionIntent::FollowUp(DEFERRED.into()),
    );
    client.retain_observations();
    assert_eq!(
        client.request(5, op.clone()),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    loop {
        if let HostFrame::Event { observation, .. } = client.next()
            && format!("{observation:?}").contains("loopback turn committed")
        {
            break;
        }
    }
    profiles::wait_idle(client, session);
    assert_eq!(
        client.request(6, op),
        Outcome::Ok(Answer::Accepted { duplicate: true })
    );
    let store = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    let record = store.load(session).unwrap();
    assert_eq!(record.pending_queue, [] as [StoredQueueItem; 0]);
    assert_eq!(
        record
            .transcript
            .rows()
            .iter()
            .filter(|(row, _)| row.text() == Some(DEFERRED))
            .count(),
        1,
        "an empty joined queue after command acceptance is consumption, not lost input"
    );
    assert_eq!(summary_dispatches(fixture), 0);
    assert_eq!(dispatches(fixture), 3);
}

#[test]
fn host_crash_during_paid_compaction_preserves_the_accepted_trigger_not_a_pending_followup() {
    let Some(fixture) = fixture() else { return };
    paid_fold_cut(fixture, false);
}

#[test]
fn host_crash_during_paid_compaction_preserves_a_durably_accepted_queued_followup() {
    let Some(fixture) = fixture() else { return };
    paid_fold_cut(fixture, true);
}

fn paid_fold_cut(fixture: Fixture, queued: bool) {
    let (fixture, mut host, mut client, session, node, attachment) = compacting(fixture, queued);
    let before = raw_queue(&fixture, &session);
    assert_eq!(before.len(), 1);
    assert_eq!(
        before[0].2, TRIGGER,
        "the existing chip is the triggering prompt, not a new follow-up"
    );
    let pending_id = ClientDraftId::new();
    let op = intent(
        attachment,
        pending_id,
        SessionIntent::FollowUp(DEFERRED.into()),
    );
    client.send(&ClientFrame::Request {
        id: 5,
        op: op.clone(),
    });
    assert_eq!(
        client.request(6, op),
        Outcome::Refused(RequestRefusal::DraftPending {
            client_id: pending_id
        })
    );
    assert!(
        client
            .retained
            .as_ref()
            .unwrap()
            .iter()
            .any(|frame| matches!(
                frame,
                HostFrame::Response {
                    id: 5,
                    outcome: Outcome::Ok(Answer::Accepted { duplicate: false })
                }
            )),
        "the first response certifies mailbox entry only"
    );
    assert!(!client.retained.as_ref().unwrap().iter().any(|frame| matches!(frame,
        HostFrame::Event { observation:SessionObservation::DraftAccepted(SessionIntent::FollowUp(draft)), .. } if draft.text == DEFERRED)), "the deferred follow-up has no durable admission verdict");
    assert_eq!(raw_queue(&fixture, &session), before);
    kill(&mut host); // Cut while the client is attached, not after graceful client shutdown.
    drop(client);
    assert_eq!(
        raw_queue(&fixture, &session),
        before,
        "SIGKILL must not erase committed intents"
    );
    let mut plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    plan["fold"] = json!("accept");
    plan["steps"] = json!([{"output":{"text":"AFTER_EXPLICIT_RESUME"}}]);
    fixture.write_script(&plan);
    let mut reopened = start(&fixture);
    let mut client = Client::connect(&reopened);
    client.retain_observations();
    let attachment = client.attach(1, &node);
    let (items, drain) = queue_snapshot(&mut client);
    eprintln!(
        "compaction cut: raw={:?}, reopened={items:?}, drain={drain:?}, dispatches={}",
        raw_queue(&fixture, &session),
        dispatches(&fixture)
    );
    assert_eq!(
        items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        [before[0].0.as_str()]
    );
    assert_eq!(i64::try_from(items[0].revision).unwrap(), before[0].1);
    assert_eq!(items[0].text, before[0].2);
    assert_eq!(
        drain,
        plakous_host_protocol::QueueDrainState::RestoredPaused
    );
    assert_eq!(raw_queue(&fixture, &session), before);
    assert_eq!(
        summary_dispatches(&fixture),
        1,
        "opening cannot replay the uncertain paid fold"
    );
    assert_eq!(
        dispatches(&fixture),
        3,
        "opening cannot admit a root continuation or pending draft"
    );
    resume_once(&fixture, &mut client, &session, attachment);
    stop(&mut reopened);
}

fn resume_once(fixture: &Fixture, client: &mut Client, session: &str, attachment: u64) {
    let resume = Op::Intent {
        attachment,
        client_id: None,
        intent: SessionIntent::ResumeQueueDrain,
        blobs: Vec::new(),
    };
    assert_eq!(
        client.request(10, resume.clone()),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    loop {
        if let HostFrame::Event { observation, .. } = client.next()
            && format!("{observation:?}").contains("AFTER_EXPLICIT_RESUME")
        {
            break;
        }
    }
    profiles::wait_idle(client, session);
    assert_eq!(
        client.request(11, resume),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    // Request a host-actor baseline behind the repeat in the same mailbox.
    // A directory read or mailbox acknowledgement would not be this barrier.
    assert_eq!(
        client.request(
            12,
            Op::Intent {
                attachment,
                client_id: None,
                intent: SessionIntent::ResyncTranscript {
                    scope: plakous_host_protocol::views::transcript::TranscriptScope::Root
                },
                blobs: Vec::new()
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    loop {
        if matches!(client.next(), HostFrame::Snapshot(page) if page.last) {
            break;
        }
    }
    let store = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    let record = store.load(session).unwrap();
    assert_eq!(record.pending_queue, [] as [StoredQueueItem; 0]);
    assert_eq!(
        record
            .prompt_history
            .iter()
            .filter(|text| text.as_str() == TRIGGER)
            .count(),
        1
    );
    assert!(!record.prompt_history.iter().any(|text| text == DEFERRED));
    assert_eq!(
        record
            .transcript
            .rows()
            .iter()
            .filter(|(row, _)| row.text() == Some(TRIGGER))
            .count(),
        1
    );
    assert_eq!(
        record
            .transcript
            .rows()
            .iter()
            .filter(|(row, _)| row.text() == Some("AFTER_EXPLICIT_RESUME"))
            .count(),
        1
    );
    let requests = store.load_intents(session).unwrap().workflow_requests;
    assert_eq!(
        requests.len(),
        2,
        "explicit Resume authorizes a new paid workflow"
    );
    assert_ne!(requests[0].operation_id, requests[1].operation_id);
    paid_workflows(fixture, &store, session, &requests, &record);
    assert_eq!(summary_dispatches(fixture), 2);
    assert_eq!(
        dispatches(fixture),
        5,
        "two seeds, one uncertain fold, one newly authorized fold and one queued turn"
    );
}

fn paid_workflows(
    fixture: &Fixture,
    store: &plak::store::session_records::SqliteSessionStore,
    session: &str,
    requests: &[plakous_hosted::sessions::projector::WorkflowRequest],
    record: &plakous_hosted::sessions::SessionRecord,
) {
    use plakous_hosted::sessions::projector::WorkflowSettlement;
    let views = store.load_views(session).unwrap().unwrap();
    let settled: Vec<_> = requests
        .iter()
        .map(|request| &views.state.workflows[&request.operation_id].settled)
        .collect();
    assert_eq!(
        settled
            .iter()
            .filter(|outcome| matches!(outcome, Some(WorkflowSettlement::Abandoned)))
            .count(),
        1
    );
    assert_eq!(
        settled
            .iter()
            .filter(|outcome| matches!(outcome, Some(WorkflowSettlement::Compacted { .. })))
            .count(),
        1
    );
    let database = plak::home::paths::PlakPaths::explicit(fixture.home()).runtime_database();
    let owners: Vec<_> = plakous_offline_crash_harness::ledger_signature::inspect_provider_owners(
        &database,
        record.plakous_locator.unwrap(),
        "AFTER_EXPLICIT_RESUME",
    )
    .unwrap()
    .into_iter()
    .filter(|provider| {
        matches!(
            provider.owner,
            plakous::model::ModelOperationRoot::SessionWorkflow(_)
        )
    })
    .map(|provider| provider.owner)
    .collect();
    assert_eq!(owners.len(), 2, "each workflow has one provider operation");
    assert_ne!(
        owners[0], owners[1],
        "the resumed fold has a new workflow owner"
    );
}
