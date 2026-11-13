//! The producer captures a sender-owned ID minted only after host startup.

use super::*;

#[test]
fn client_producer_holds_the_first_native_id_without_replacing_or_reusing_it() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({"version":1,"repeat_last_step":true,"steps":[{"output":{"text":"fixture answer"}}]}));
    let reached = fixture.root.join("native-ready");
    let release = fixture.root.join("native-release");
    let mut host = start_env(
        &fixture,
        &[],
        &[
            ("PLAK_TEST_DRAFT_PENDING_ID", "first".into()),
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
    let id = ClientDraftId::new();
    let marker = format!("reserved {id}\n");
    origin.send(&ClientFrame::Request {
        id: 3,
        op: draft(attachment, id, "native original"),
    });
    ready_value(&reached, marker.as_bytes());
    assert_eq!(providers(&fixture), 0);
    drop(origin);
    let mut check = Client::connect(&host);
    assert_eq!(check.rows(1)[0].session, session);
    let attachment = check.attach(2, &node);
    check.retain_observations();
    for request in 3..6 {
        assert_eq!(
            check.request(request, draft(attachment, id, "native original")),
            Outcome::Refused(RequestRefusal::DraftPending { client_id: id })
        );
    }
    assert!(matches!(
        check.request(6, draft(attachment, id, "edited original")),
        Outcome::Refused(RequestRefusal::Malformed { .. })
    ));
    assert_eq!(providers(&fixture), 0);
    publish(&release, "release\n");
    duplicate(&mut check, attachment, id, "native original");
    finished(&mut check);
    let next_id = ClientDraftId::new();
    assert_eq!(
        check.request(7, draft(attachment, next_id, "next native write")),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    duplicate(&mut check, attachment, next_id, "next native write");
    finished(&mut check);
    assert_eq!(std::fs::read(&reached).unwrap(), marker.as_bytes());
    drop(check);
    stop(&mut host);
    assert_eq!(providers(&fixture), 2);
    let store = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    let record = store.load(&session).unwrap();
    for prompt in ["native original", "next native write"] {
        assert_eq!(
            record
                .prompt_history
                .iter()
                .filter(|text| *text == prompt)
                .count(),
            1
        );
    }
}
