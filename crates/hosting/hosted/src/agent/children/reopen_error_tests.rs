use super::*;
use plakous::sessions::SessionBackend;

#[test]
fn an_incompatible_store_reads_as_the_one_incompatible_build_refusal() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("runtime.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .pragma_update(None, "application_id", 0x504c_4b35_i64)
        .unwrap();
    connection
        .pragma_update(None, "user_version", 13_i64)
        .unwrap();
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .unwrap();
    drop(connection);
    let error = SessionBackend::sqlite(&database).expect_err("an older schema opened");
    assert_eq!(
        format_session_reopen_error("exact session reopen failed", &error),
        "this session was created by an incompatible plak build; start a new session"
    );
}
