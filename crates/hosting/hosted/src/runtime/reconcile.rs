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
    save: impl FnOnce(&mut SessionRecord) -> Result<(), String>,
) -> Result<(), String> {
    let mut candidate = session.clone();
    candidate.append_transcript(TranscriptRow::error(
        NoticeScope::Turn,
        super::models::humanize_turn_error(error),
    ));
    candidate.touch();
    save(&mut candidate)?;
    *session = candidate;
    Ok(())
}

pub(super) fn commit_exceptional_terminal_projection(
    session: &mut SessionRecord,
    projection: ExceptionalTerminalProjection<'_>,
    save: impl FnOnce(&mut SessionRecord) -> Result<(), String>,
) -> Result<(), String> {
    let mut candidate = session.clone();
    match projection {
        ExceptionalTerminalProjection::RecoveryRequired(error)
        | ExceptionalTerminalProjection::UnresolvedError(error) => {
            candidate.queue_drain_state = SessionQueueDrainState::RestoredPaused;
            candidate.append_transcript(TranscriptRow::error(NoticeScope::Turn, error.to_string()));
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
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type Sender = crate::runtime::Sender<TestExtension>;

    use super::*;
    use crate::runtime::*;

    use crate::runtime::info::*;

    use crate::sessions::{QueueDrainState as SessionQueueDrainState, SessionRecord};

    #[test]
    fn definite_failure_projection_publishes_only_after_durable_save() {
        let root = tempfile::tempdir().unwrap();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        session.append_transcript(TranscriptRow::user("retained source"));
        let source = session.clone();

        let result = commit_definite_failure_projection(&mut session, "definite rejection", |_| {
            Err("injected store failure".into())
        });
        assert!(result.is_err());
        assert_eq!(session.transcript, source.transcript);
        assert_eq!(session.store_revision, source.store_revision);

        commit_definite_failure_projection(&mut session, "definite rejection", |_| Ok(())).unwrap();
        assert_eq!(
            session.transcript.last(),
            Some(&TranscriptRow::error(
                NoticeScope::Turn,
                "definite rejection"
            ))
        );
    }

    #[test]
    fn exceptional_terminal_projections_publish_candidate_first() {
        let root = tempfile::tempdir().unwrap();
        for projection in [
            ExceptionalTerminalProjection::RecoveryRequired("recovery"),
            ExceptionalTerminalProjection::Interrupted("partial"),
            ExceptionalTerminalProjection::UnresolvedError("unresolved"),
        ] {
            let mut session = SessionRecord::new(root.path().to_path_buf());
            session.append_transcript(TranscriptRow::user("retained source"));
            let source = session.clone();

            let result = commit_exceptional_terminal_projection(&mut session, projection, |_| {
                Err("injected store failure".into())
            });
            assert!(result.is_err());
            assert_eq!(session, source);

            commit_exceptional_terminal_projection(&mut session, projection, |_| Ok(())).unwrap();
            assert_ne!(session, source);
            match projection {
                ExceptionalTerminalProjection::Interrupted(_) => {
                    assert_eq!(session.transcript.last(), Some(&TranscriptRow::Interrupted));
                }
                ExceptionalTerminalProjection::RecoveryRequired(_)
                | ExceptionalTerminalProjection::UnresolvedError(_) => {
                    assert_eq!(
                        session.queue_drain_state,
                        SessionQueueDrainState::RestoredPaused
                    );
                }
            }
        }
    }

    /// Status is a latest-state observation, so an `idle` published after the
    /// settled-cancel `interrupted` phase would erase it before any consumer
    /// could read it; an interrupted turn leaves `interrupted` as its rest.
    #[test]
    fn an_interrupted_terminal_phase_publishes_no_status() {
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);

        emit_terminal_status(&events, TerminalPhase::Interrupted, 0);

        assert!(
            rx.try_iter()
                .all(|event| !matches!(event, HostEvent::Status { .. })),
            "the settled-cancel `interrupted` status must remain the resting phase"
        );
    }

    #[test]
    fn failed_exceptional_terminal_publication_never_emits_success_idle() {
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);

        emit_terminal_status(&events, TerminalPhase::RecoveryPending, 3);

        let observed = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(
            observed,
            [HostEvent::Status {
                phase: "error".into(),
                queue_len: 3,
                detail: Some("needs recovery · next prompt recovers".into()),
            }]
        );
        assert!(!observed.iter().any(|event| matches!(
            event,
            HostEvent::Status { phase, .. } if phase == "idle"
        )));
    }
}
