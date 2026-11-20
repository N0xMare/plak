//! One-shot facade read failures, isolated by runtime session and page cursor.

#![allow(clippy::unwrap_used, reason = "test-only fault injection")]

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use plakous::model::SessionId;
use plakous::sessions::feed::FeedSeq;

fn faults() -> &'static Mutex<BTreeMap<SessionId, FeedSeq>> {
    static FAULTS: OnceLock<Mutex<BTreeMap<SessionId, FeedSeq>>> = OnceLock::new();
    FAULTS.get_or_init(Mutex::default)
}

pub(crate) fn arm(session: SessionId, after: FeedSeq) {
    faults().lock().unwrap().insert(session, after);
}

pub(super) fn session_for_read(session: SessionId, cursor: FeedSeq, has_reads: bool) -> SessionId {
    let mut faults = faults().lock().unwrap();
    if has_reads && faults.get(&session).is_some_and(|after| cursor >= *after) {
        faults.remove(&session);
        // The real facade rejects a checkpoint read under a foreign session.
        return "00000000-0000-4000-8000-000000000001".parse().unwrap();
    }
    session
}
