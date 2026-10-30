//! Reconciliation of prepared workflows on session open.

use super::ExceptionalTerminalProjection;
use crate::session_store::SessionRecordStore;
use crate::sessions::{NoticeScope, TranscriptRow};
use crate::sessions::{QueueDrainState as SessionQueueDrainState, SessionRecord};

pub(super) fn persist(store: &dyn SessionRecordStore, session: &mut SessionRecord) {
    session.touch();
    let _ = store.save(session);
}

/// Commits a definite turn failure with the error row the operator is shown:
/// the humanized text, which the caller publishes as a row observation.
pub(super) fn commit_definite_failure_projection(
    session: &mut SessionRecord,
    error: &str,
    save: impl FnOnce(&mut SessionRecord) -> Result<(), crate::session_store::RecordStoreError>,
    remedy: Option<&plakous_host_protocol::notice_remedy::NoticeRemedy>,
) -> Result<(), String> {
    let mut candidate = session.clone();
    candidate.append_transcript(
        TranscriptRow::error(NoticeScope::Turn, error).with_optional_remedy(remedy),
    );
    candidate.touch();
    save(&mut candidate)?;
    *session = candidate;
    Ok(())
}

pub(super) fn commit_exceptional_terminal_projection(
    session: &mut SessionRecord,
    projection: ExceptionalTerminalProjection<'_>,
    save: impl FnOnce(&mut SessionRecord) -> Result<(), crate::session_store::RecordStoreError>,
    remedy: Option<&plakous_host_protocol::notice_remedy::NoticeRemedy>,
) -> Result<(), String> {
    let mut candidate = session.clone();
    match projection {
        ExceptionalTerminalProjection::RecoveryRequired(error)
        | ExceptionalTerminalProjection::UnresolvedError(error) => {
            candidate.queue_drain_state = SessionQueueDrainState::RestoredPaused;
            candidate.append_transcript(
                TranscriptRow::error(NoticeScope::Turn, error.to_string())
                    .with_optional_remedy(remedy),
            );
        }
        ExceptionalTerminalProjection::Interrupted(assistant) => {
            candidate.child_wake_suppressed = true;
            if !assistant.trim().is_empty() {
                candidate.append_transcript(TranscriptRow::assistant(assistant));
            }
            candidate.append_transcript(TranscriptRow::Interrupted);
        }
    }
    candidate.touch();
    save(&mut candidate)?;
    *session = candidate;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
