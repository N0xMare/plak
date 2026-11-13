//! Directory timestamps are durable product facts, not attachment times.

use super::*;
use plak::store::session_records::SessionRecordStore as _;

#[test]
fn directory_creation_time_survives_a_turn_and_host_reopen() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({"version":1,"steps":[{"output":{"text":ANSWER}}]}));
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let store = plak::store::session_records::SqliteSessionStore::new(
        plak::home::paths::PlakPaths::explicit(fixture.home()),
    );
    let created_at = store.load(&session).unwrap().created_at_unix;
    assert!(
        created_at > 1_500_000_000,
        "a new session must not report an epoch sentinel"
    );
    assert_eq!(client.rows(2)[0].created_at, created_at);
    let attachment = client.attach(3, &node);
    client.prompt(4, attachment, "exercise the directory timestamp", ANSWER);
    profiles::wait_idle(&mut client, &session);
    assert_eq!(client.rows(5)[0].created_at, created_at);
    drop(client);
    stop(&mut host);
    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    assert_eq!(client.rows(1)[0].created_at, created_at);
    client.attach(2, &node);
    assert_eq!(client.rows(3)[0].created_at, created_at);
    stop(&mut host);
}
