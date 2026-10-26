use super::*;
use plakous::sessions::SessionBackend;

tokio::task_local! {
    pub(in crate::agent) static REOPEN_OPERATIONS: std::cell::RefCell<Vec<String>>;
}

pub(super) fn record_operation(operation: &SessionOperationId) {
    let _untracked = REOPEN_OPERATIONS.try_with(|operations| {
        operations.borrow_mut().push(operation.as_str().to_owned());
    });
}

#[test]
fn reopen_formatter_collapses_nested_chains_without_hiding_the_distinct_refusal() {
    let error = SessionOperationId::new("").unwrap_err();
    let cause = "initiating Session failure: session store disk is full";
    let prefix = format!(
        "{cause}; cleanup disposition: {cause}; cleanup disposition: {cause}; exact session reopen failed"
    );
    let rendered = format_session_reopen_error(&prefix, &error);
    assert_eq!(rendered.matches("session store disk is full").count(), 1);
    assert!(rendered.contains(&error.to_string()));
}

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
