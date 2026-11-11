//! Host-owned binding repair, exact reopen and their process-loss boundaries.

use super::crash::dispatches;
use super::*;
use std::time::Instant;

fn seeded(fixture: &Fixture, host: &Host) -> (Client, String, NodeAddress, u64) {
    let mut client = Client::connect(host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "one admitted turn", ANSWER);
    let deadline = Instant::now() + WAIT;
    while client
        .rows(4)
        .iter()
        .find(|row| row.session == session)
        .unwrap()
        .activity
        == SessionActivity::Busy
    {
        assert!(Instant::now() < deadline, "the completed turn stayed busy");
    }
    (client, session, node, attachment)
}

fn change(attachment: u64, rebind: bool) -> Op<PlakExtension> {
    if rebind {
        Op::Rebind {
            attachment,
            request_id: "repair".into(),
            cause: "explicit binding repair".into(),
        }
    } else {
        Op::RetryAttach {
            attachment,
            request_id: "retry".into(),
        }
    }
}

fn one_terminal(fixture: &Fixture, source: &str) {
    use plak::store::session_records::SessionRecordStore as _;
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let store = plak::store::session_records::SqliteSessionStore::new(paths.clone());
    let locator = store.load(source).unwrap().plakous_locator.unwrap();
    let (_, turns) = plakous_offline_crash_harness::ledger_signature::inspect_root_lifecycles(
        &paths.runtime_database(),
        locator,
    )
    .unwrap();
    assert_eq!(turns.len(), 1, "a reopen invented another root lifecycle");
    assert!(matches!(
        turns[0]["state"].as_str(),
        Some("completed" | "finalized")
    ));
}

fn history_preserved(fixture: &Fixture, target: &str) {
    let document = fixture.root.join("rebound-export.json");
    let exported = fixture
        .command()
        .args(["session", "export", target, "--out"])
        .arg(&document)
        .output()
        .unwrap();
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert!(
        std::fs::read_to_string(document).unwrap().contains(ANSWER),
        "the carried canonical history lost the answer"
    );
}

#[test]
fn host_binding_requests_preserve_history_without_another_paid_turn() {
    for rebind in [false, true] {
        let Some(fixture) = fixture() else { return };
        fixture.write_script(&json!({"version":1,"steps":[{"output":{"text":ANSWER}}]}));
        let mut host = start(&fixture);
        let (mut client, source, _, attachment) = seeded(&fixture, &host);
        let outcome = client.request(5, change(attachment, rebind));
        let Outcome::Ok(Answer::Moved { node }) = outcome else {
            panic!(
                "rebind={rebind}: the host refused binding repair: {outcome:?}; {:?}",
                host.lines.try_iter().collect::<Vec<_>>()
            )
        };
        client.attach(6, &node);
        let rows = client.rows(7);
        assert_eq!(
            rows.iter()
                .filter(|row| row.residency == SessionResidency::Resident)
                .count(),
            1
        );
        let target = rows
            .iter()
            .find(|row| row.residency == SessionResidency::Resident)
            .unwrap();
        assert_eq!(target.session == source, !rebind);
        history_preserved(&fixture, &target.session);
        assert_eq!(dispatches(&fixture), 1);
        one_terminal(&fixture, &source);
        drop(client);
        stop(&mut host);
    }
}

#[test]
fn host_plain_attach_reopens_after_a_failed_placement_is_fixed() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(
        &json!({"version":1,"repeat_last_step":true,"steps":[{"output":{"text":ANSWER}}]}),
    );
    let mut host = start(&fixture);
    let (mut client, session, node, _) = seeded(&fixture, &host);
    assert_eq!(
        client.request(
            5,
            Op::ReleaseSession {
                session: session.clone()
            }
        ),
        Outcome::Ok(Answer::Released)
    );
    let unavailable = fixture.root.join("workspace-unavailable");
    std::fs::rename(fixture.workspace(), &unavailable).unwrap();
    let attachment = client.attach(6, &node);
    assert!(matches!(
        client.request(
            7,
            Op::Intent {
                attachment,
                intent: SessionIntent::Prompt("not admitted".into()),
                client_id: None,
                blobs: Vec::new(),
            }
        ),
        Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::PlacementRefused { .. })
    ));
    assert_eq!(dispatches(&fixture), 1);
    std::fs::rename(unavailable, fixture.workspace()).unwrap();
    let attachment = client.attach(8, &node);
    client.prompt(9, attachment, "plain attach is live again", ANSWER);
    assert_eq!(dispatches(&fixture), 2);
    drop(client);
    stop(&mut host);
}

#[test]
fn host_binding_request_crash_cuts_keep_one_owner_and_never_redispatch() {
    for rebind in [false, true] {
        for cut in ["fenced", "released", "carried", "opened"] {
            let Some(fixture) = fixture() else { return };
            fixture.write_script(&json!({"version":1,"steps":[{"output":{"text":ANSWER}}]}));
            let mut host = start_env(
                &fixture,
                &[],
                &[("PLAK_TEST_HOST_READOPT_CUT", format!("reopen:{cut}"))],
            );
            let (mut client, source, _, attachment) = seeded(&fixture, &host);
            client.send(&ClientFrame::Request {
                id: 5,
                op: change(attachment, rebind),
            });
            client.stream.set_read_timeout(Some(WAIT)).unwrap();
            loop {
                let mut line = String::new();
                if client.reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
            }
            assert!(
                !host.child.take().unwrap().wait().unwrap().success(),
                "{cut}"
            );
            drop(client);
            let mut next = start(&fixture);
            let mut client = Client::connect(&next);
            let rows = client.rows(6);
            assert!(
                rows.iter().all(|row| row.hosted_elsewhere.is_none()),
                "{cut}: custody stayed live"
            );
            let target = rows
                .iter()
                .find(|row| row.session != source)
                .unwrap_or_else(|| rows.first().unwrap());
            if rebind && matches!(cut, "carried" | "opened") {
                assert_ne!(
                    target.session, source,
                    "a committed carry lost its successor"
                );
            }
            client.attach(7, &target.node);
            history_preserved(&fixture, &target.session);
            assert_eq!(dispatches(&fixture), 1, "{cut}: redispatched");
            one_terminal(&fixture, &source);
            assert_eq!(
                root_record(&fixture, &source).1,
                0,
                "{cut}: invented a continuation"
            );
            assert_eq!(
                client
                    .rows(8)
                    .iter()
                    .filter(|row| row.residency == SessionResidency::Resident)
                    .count(),
                1,
                "{cut}: duplicate actor"
            );
            drop(client);
            stop(&mut next);
        }
    }
}
