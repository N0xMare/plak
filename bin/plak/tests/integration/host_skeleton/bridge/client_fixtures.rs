use super::*;
use plak::store::session_records::SessionRecordStore as _;
use plakous_host_protocol::wire::{ClientDraftId, RequestRefusal};
use plakous_host_protocol::{SessionObservation, views::transcript::TranscriptScope};

fn providers(fixture: &Fixture) -> usize {
    plakous_offline_crash_harness::evidence::read_evidence(fixture.root.join("evidence.jsonl"))
        .unwrap()
        .iter()
        .filter(|record| {
            record.kind == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
        })
        .count()
}

fn ready(path: &std::path::Path) {
    let until = Instant::now() + WAIT;
    while !std::fs::read(path).is_ok_and(|value| value == b"reserved\n") {
        assert!(
            Instant::now() < until,
            "fixture never reserved the original draft"
        );
        std::thread::yield_now();
    }
    assert_eq!(std::fs::read(path).unwrap(), b"reserved\n");
}

fn draft(attachment: u64, client_id: ClientDraftId, text: &str) -> Op<PlakExtension> {
    Op::Intent {
        attachment,
        client_id: Some(client_id),
        intent: SessionIntent::Prompt(Draft::from(text)),
        blobs: Vec::new(),
    }
}

#[test]
fn client_producer_pending_gate_keeps_reads_reconnect_and_same_id_checks_live() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({"version":1,"repeat_last_step":true,"steps":[{"output":{"text":"fixture answer"}}]}));
    let id = ClientDraftId::new();
    let reached = fixture.root.join("pending-ready");
    let release = fixture.root.join("pending-release");
    let mut host = start_env(
        &fixture,
        &[],
        &[
            ("PLAK_TEST_DRAFT_PENDING_ID", id.to_string()),
            (
                "PLAK_TEST_DRAFT_PENDING_READY",
                reached.display().to_string(),
            ),
            (
                "PLAK_TEST_DRAFT_PENDING_RELEASE",
                release.display().to_string(),
            ),
        ],
    );
    let mut origin = Client::connect(&host);
    let Outcome::Ok(Answer::Created { node, session, .. }) =
        origin.request(1, new_session(&fixture.workspace()))
    else {
        panic!("no fixture session")
    };
    let attachment = origin.attach(2, &node);
    origin.send(&ClientFrame::Request {
        id: 3,
        op: draft(attachment, id, "held original"),
    });
    ready(&reached);
    assert_eq!(providers(&fixture), 0);
    // Lose only the original reply. The actual reservation and original sender survive.
    drop(origin);
    let mut check = Client::connect(&host);
    assert_eq!(check.rows(1)[0].session, session);
    let attachment = check.attach(2, &node);
    for request in 3..6 {
        assert_eq!(
            check.request(request, draft(attachment, id, "held original")),
            Outcome::Refused(RequestRefusal::DraftPending { client_id: id })
        );
    }
    check.send(&ClientFrame::Request {
        id: 6,
        op: draft(attachment, id, "held original"),
    });
    drop(check);
    let mut check = Client::connect(&host);
    let attachment = check.attach(1, &node);
    check.retain_observations();
    assert!(matches!(
        check.request(6, draft(attachment, id, "conflicting edit")),
        Outcome::Refused(RequestRefusal::Malformed { .. })
    ));
    assert!(matches!(
        check.request(7, Op::Ping),
        Outcome::Ok(Answer::Pong)
    ));
    assert_eq!(providers(&fixture), 0);
    publish(&release, "release\n");
    duplicate(&mut check, attachment, id, "held original");
    assert!(matches!(
        check.request(9, draft(attachment, id, "held original")),
        Outcome::Ok(Answer::Accepted { duplicate: true })
    ));
    finished(&mut check);
    drop(check);
    stop(&mut host);
    assert_eq!(providers(&fixture), 1);
    let store = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    let record = store.load(&session).unwrap();
    assert_eq!(
        record
            .prompt_history
            .iter()
            .filter(|text| *text == "held original")
            .count(),
        1
    );
}

fn publish(path: &std::path::Path, value: &str) {
    let staging = path.with_extension("staging");
    std::fs::write(&staging, value).unwrap();
    std::fs::rename(staging, path).unwrap();
}

fn duplicate(client: &mut Client, attachment: u64, id: ClientDraftId, text: &str) {
    let until = Instant::now() + WAIT;
    let mut request = 100;
    loop {
        match client.request(request, draft(attachment, id, text)) {
            Outcome::Ok(Answer::Accepted { duplicate: true }) => return,
            Outcome::Refused(RequestRefusal::DraftPending { .. }) => {
                assert!(Instant::now() < until);
            }
            other => panic!("not durably admitted: {other:?}"),
        }
        request += 1;
    }
}

fn finished(client: &mut Client) {
    let until = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < until);
        if matches!(
            client.next(),
            HostFrame::Event {
                observation: SessionObservation::AssistantDone,
                ..
            }
        ) {
            return;
        }
    }
}

#[test]
fn client_producer_cancelled_pending_gate_frees_id_without_admitting_original() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({"version":1,"repeat_last_step":true,"steps":[{"output":{"text":"fixture answer"}}]}));
    let id = ClientDraftId::new();
    let reached = fixture.root.join("ready");
    let release = fixture.root.join("release");
    let mut host = start_env(
        &fixture,
        &[],
        &[
            ("PLAK_TEST_DRAFT_PENDING_ID", id.to_string()),
            (
                "PLAK_TEST_DRAFT_PENDING_READY",
                reached.display().to_string(),
            ),
            (
                "PLAK_TEST_DRAFT_PENDING_RELEASE",
                release.display().to_string(),
            ),
        ],
    );
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { node, session, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("no session")
    };
    let attachment = client.attach(2, &node);
    client.retain_observations();
    assert_eq!(
        client.request(3, draft(attachment, id, "never admitted")),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    ready(&reached);
    assert_eq!(providers(&fixture), 0);
    publish(&release, "cancel\n");
    let until = Instant::now() + WAIT;
    loop {
        match client.request(4, draft(attachment, id, "explicit replacement")) {
            Outcome::Ok(Answer::Accepted { duplicate: false }) => break,
            Outcome::Refused(RequestRefusal::Malformed { .. }) => {
                assert!(Instant::now() < until);
                std::thread::yield_now();
            }
            other => panic!("reservation was not released: {other:?}"),
        }
    }
    duplicate(&mut client, attachment, id, "explicit replacement");
    finished(&mut client);
    drop(client);
    stop(&mut host);
    let record = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    )
    .load(&session)
    .unwrap();
    assert!(
        !record
            .prompt_history
            .iter()
            .any(|text| text == "never admitted")
    );
    assert_eq!(
        record
            .prompt_history
            .iter()
            .filter(|text| *text == "explicit replacement")
            .count(),
        1
    );
    assert_eq!(providers(&fixture), 1);
}

mod recovery;
