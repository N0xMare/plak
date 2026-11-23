use plakous_hosted::sessions::TranscriptRow;
use plakous_offline_crash_harness::commit_trace;

pub(crate) fn is_stop_notice(row: &TranscriptRow) -> bool {
    row.text()
        == Some(
            "stopped: the session's work ended, and nothing continues it until it is prompted again",
        )
}

pub(crate) fn commit_session(
    transaction: rusqlite::Transaction<'_>,
    wrote_stop_notice: bool,
) -> rusqlite::Result<()> {
    // Name the write, not the notice content or session. A transaction that
    // matched a previously committed notice never reaches this with the flag.
    let traced_stop = wrote_stop_notice && std::env::var_os(commit_trace::TRACE_ENV).is_some();
    let _label = traced_stop.then(|| commit_trace::label("stop_notice"));
    transaction.commit()?;
    if traced_stop {
        // The WAL callback synced the indexed row before commit returned.
        eprintln!("commit-trace: stop_notice");
    }
    Ok(())
}
