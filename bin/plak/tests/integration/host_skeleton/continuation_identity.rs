//! Persisted loss decisions survive identity changes without replay or cross-root aliasing.

use super::*;
use plak::store::session_records::{SessionRecordStore as _, SqliteSessionStore};
use plakous_hosted::sessions::projector::{
    Delivery, RootContinuation, RootContinuationCause, RootContinuationDecision, RootLossIntent,
};
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};

fn store(fixture: &Fixture) -> SqliteSessionStore {
    SqliteSessionStore::new(plak::home::paths::PlakPaths::explicit(fixture.home()))
}

fn calls(fixture: &Fixture) -> usize {
    read_evidence(fixture.root.join("evidence.jsonl"))
        .unwrap_or_default()
        .iter()
        .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
        .count()
}

fn wait_calls(fixture: &Fixture, count: usize) {
    let deadline = std::time::Instant::now() + WAIT;
    while calls(fixture) < count {
        assert!(
            std::time::Instant::now() < deadline,
            "expected {count} physical calls, got {}",
            calls(fixture)
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(calls(fixture), count);
}

fn handed_off(fixture: &Fixture, count: usize) -> Vec<String> {
    fixture.write_script(&json!({"version": 1, "steps": [{"output": {"text": ANSWER}}]}));
    let mut host = start_env(
        fixture,
        &[],
        &[(
            "PLAK_TEST_PROVIDER_RELEASE",
            fixture
                .root
                .join("unreleased-provider")
                .display()
                .to_string(),
        )],
    );
    let mut clients = Vec::new();
    let mut sessions = Vec::new();
    for index in 0..count {
        let mut client = Client::connect(&host);
        let mut create = new_session(&fixture.workspace());
        if let Op::NewSession { request_id, .. } = &mut create {
            *request_id = uuid::Uuid::new_v4().to_string();
        }
        let Outcome::Ok(Answer::Created { session, node, .. }) = client.request(1, create) else {
            panic!("session not created")
        };
        let attachment = client.attach(2, &node);
        assert_eq!(
            client.request(
                3,
                Op::Intent {
                    client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                    attachment,
                    intent: SessionIntent::Prompt(Draft::from("continue this distinct session")),
                    blobs: Vec::new(),
                }
            ),
            Outcome::Ok(Answer::Accepted { duplicate: false })
        );
        wait_calls(fixture, index + 1);
        sessions.push(session);
        clients.push(client);
    }
    stop(&mut host);
    if sessions.len() == 2 {
        assert_ne!(sessions[0], sessions[1]);
        equal_loss_positions(fixture, &sessions);
    }
    sessions
}

fn equal_loss_positions(fixture: &Fixture, sessions: &[String]) {
    let losses: Vec<_> = sessions
        .iter()
        .map(|id| {
            store(fixture)
                .load_intents(id)
                .unwrap()
                .root_interrupts
                .last()
                .unwrap()
                .clone()
        })
        .collect();
    let anchor = losses.iter().map(|loss| loss.anchor).max().unwrap();
    for id in sessions {
        assert!(store(fixture).load_views(id).unwrap().unwrap().cursor.get() >= anchor);
    }
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let mut connection = rusqlite::Connection::open(paths.product_database()).unwrap();
    let transaction = connection.transaction().unwrap();
    let sql_anchor = i64::try_from(anchor).unwrap();
    let ordinal: u32 = transaction
        .query_row(
            "SELECT COALESCE(MAX(ordinal),0)+1 FROM host_intents WHERE anchor=?1",
            [sql_anchor],
            |row| row.get(0),
        )
        .unwrap();
    // Construct the equal-position persisted cut after real source releases.
    // Projection timing otherwise gives only one root an extra control intent.
    // No runtime turn, stop anchor or canonical command identity is edited.
    for (id, loss) in sessions.iter().zip(losses) {
        assert_eq!(transaction.execute(
            "UPDATE host_intents SET anchor=?1,ordinal=?2 WHERE session_id=?3 AND kind='interrupt_cause' AND anchor=?4 AND ordinal=?5",
            rusqlite::params![sql_anchor, ordinal, id, i64::try_from(loss.anchor).unwrap(), loss.ordinal],
        ).unwrap(), 1);
    }
    transaction.commit().unwrap();
}

fn legacy_decision(fixture: &Fixture, session: &str) -> String {
    let rows = store(fixture);
    let intents = rows.load_intents(session).unwrap();
    let loss = intents.root_interrupts.last().unwrap();
    let digest =
        plakous::model::sha256_hex(&[format!("{}:{}", loss.anchor, loss.ordinal).as_bytes()]);
    let command = format!("plak:resume-loss:root:{}", &digest[..32]);
    rows.write_root_loss(
        session,
        RootLossIntent::Continuation(RootContinuation {
            anchor: loss.value.anchor.clone(),
            host_generation: 1,
            cause: RootContinuationCause::Handoff,
            round: 1,
            decided_at_ms: 1,
            decision: RootContinuationDecision::NewTurn {
                command_id: command.clone(),
                withdrawn: Vec::new(),
            },
        }),
    )
    .unwrap();
    command
}

fn settled(fixture: &Fixture, session: &str, command: &str) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let views = store(fixture).load_views(session).unwrap().unwrap();
        if views.state.deliveries.get(command) == Some(&Delivery::Accepted)
            && views.state.open_turns.is_empty()
            && views.state.counters.root_turns == 2
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "continuation did not settle: {session}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn scan_complete(host: &Host) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let line = host
            .lines
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .unwrap();
        if line == "plak host: continuation scan complete" {
            return;
        }
    }
}

#[test]
fn root_continuation_equal_feed_positions_restart_each_session_exactly_once() {
    let Some(fixture) = fixture() else { return };
    let sessions = handed_off(&fixture, 2);
    let mut host = start(&fixture);
    scan_complete(&host);
    wait_calls(&fixture, 4);
    let commands: Vec<_> = sessions
        .iter()
        .map(|id| {
            let intents = store(&fixture).load_intents(id).unwrap();
            assert_eq!(intents.root_continuations.len(), 1);
            let RootContinuationDecision::NewTurn { command_id, .. } =
                &intents.root_continuations[0].value.decision
            else {
                panic!("not a continuation")
            };
            settled(&fixture, id, command_id);
            command_id.clone()
        })
        .collect();
    assert_ne!(commands[0], commands[1]);
    stop(&mut host);
    let mut host = start(&fixture);
    scan_complete(&host);
    assert_eq!(calls(&fixture), 4);
    stop(&mut host);
}

#[test]
fn root_continuation_legacy_pending_then_consumed_restarts_without_rederivation() {
    let Some(fixture) = fixture() else { return };
    let session = handed_off(&fixture, 1).remove(0);
    let legacy = legacy_decision(&fixture, &session);
    for _ in 0..2 {
        let mut host = start(&fixture);
        scan_complete(&host);
        wait_calls(&fixture, 2);
        settled(&fixture, &session, &legacy);
        assert_eq!(
            store(&fixture)
                .load_intents(&session)
                .unwrap()
                .root_continuations
                .len(),
            1
        );
        stop(&mut host);
    }
}

#[test]
fn root_continuation_legacy_foreign_submission_repairs_only_the_owed_session_once() {
    let Some(fixture) = fixture() else { return };
    let sessions = handed_off(&fixture, 2);
    let legacy = legacy_decision(&fixture, &sessions[0]);
    assert_eq!(legacy_decision(&fixture, &sessions[1]), legacy);
    let mut first = embedded(&fixture, &sessions[0]).expect("old decision opened");
    wait_calls(&fixture, 3);
    settled(&fixture, &sessions[0], &legacy);
    first
        .child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", quit_frame()).as_bytes())
        .unwrap();
    assert!(first.child.wait().unwrap().success());
    assert_eq!(
        store(&fixture).submission_owner(&legacy).unwrap(),
        Some(sessions[0].clone())
    );
    for _ in 0..2 {
        let mut host = start(&fixture);
        scan_complete(&host);
        wait_calls(&fixture, 4);
        let rows = store(&fixture);
        let intents = rows.load_intents(&sessions[1]).unwrap();
        assert_eq!(intents.root_continuations.len(), 2);
        let RootContinuationDecision::NewTurn {
            command_id,
            withdrawn,
        } = &intents.root_continuations[1].value.decision
        else {
            panic!("not repaired")
        };
        assert_ne!(command_id, &legacy);
        assert_eq!(withdrawn, std::slice::from_ref(&legacy));
        settled(&fixture, &sessions[1], command_id);
        assert_eq!(
            rows.submission_owner(&legacy).unwrap(),
            Some(sessions[0].clone())
        );
        assert_eq!(
            rows.load_intents(&sessions[0])
                .unwrap()
                .root_continuations
                .len(),
            1
        );
        stop(&mut host);
    }
}
