//! Completed publication is a root terminal fact, not a fixture text marker.

use serde_json::Value;

pub(super) fn completed_publications(
    answers: &[(String, String)],
    submissions: &[Value],
    transcript: &[Value],
) -> Vec<String> {
    answers
        .iter()
        .filter(|(command, text)| {
            submissions.iter().any(|row| {
                row["command"] == *command
                    && row["kind"] == "root_start"
                    && row["state"] == "accepted"
            }) && transcript
                .iter()
                .filter(|row| row["kind"] == "assistant" && row["text"] == *text)
                .count()
                == answers.iter().filter(|(_, answer)| answer == text).count()
        })
        .map(|(command, _)| command.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a2_completed_publication_uses_the_joined_root_answer()
    -> Result<(), Box<dyn std::error::Error>> {
        use plakous::agent::CommandId;
        use plakous::sessions::{
            AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
            SessionOperationId,
        };
        use plakous_offline_crash_harness::{BoundaryController, protocol::CrashBoundary};
        let root = tempfile::tempdir()?;
        let database = root.path().join("runtime.sqlite3");
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            root.path().join("blocks.jsonl"),
        )?;
        let runtime = plakous_offline_crash_harness::text_blocks_runtime_binding(
            root.path(),
            controller,
            &["first", "second"],
        )
        .map_err(|error| -> Box<dyn std::error::Error> { error })?;
        let sessions = AgentSessions::new(SessionBackend::sqlite(&database)?)?;
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("blocks-create")?,
                runtime,
            ))
            .await?
        else {
            return Err("created session".into());
        };
        session
            .agent()
            .start(CommandId::new("original")?, "blocks")
            .await?
            .into_turn()
            .ok_or("turn")?
            .result()
            .await?;
        let head = session.release().await?;
        let answers =
            plakous_offline_crash_harness::ledger_signature::inspect_completed_root_answers(
                &database, head,
            )?;
        assert_eq!(
            answers,
            [("original".to_owned(), "first\n\nsecond".to_owned())]
        );
        let submissions =
            vec![json!({"command":"original","kind":"root_start","state":"accepted"})];
        let transcript = vec![json!({"kind":"assistant","text":"first\n\nsecond"})];
        assert_eq!(
            completed_publications(&answers, &submissions, &transcript),
            ["original"]
        );
        for wrong in ["firstsecond", "first\n\n\n\nsecond\n\n"] {
            assert!(
                completed_publications(
                    &answers,
                    &submissions,
                    &[json!({"kind":"assistant","text":wrong})],
                )
                .is_empty()
            );
        }
        Ok(())
    }

    #[test]
    fn two_round_publication_counts_only_the_root_terminal_answer() {
        let answers = vec![("original".into(), super::super::shapes::ANSWER.into())];
        let submissions =
            vec![json!({"command":"original","kind":"root_start","state":"accepted"})];
        let transcript = vec![
            json!({"kind":"assistant","text":"intermediate round"}),
            json!({"kind":"assistant","text":super::super::shapes::ANSWER}),
        ];
        assert_eq!(
            completed_publications(&answers, &submissions, &transcript),
            ["original"]
        );
    }

    #[test]
    fn first_provider_completion_counts_without_the_pilot_answer_marker() {
        let answers = vec![("original".into(), "intermediate round".into())];
        let submissions =
            vec![json!({"command":"original","kind":"root_start","state":"accepted"})];
        let transcript = vec![json!({"kind":"assistant","text":"intermediate round"})];
        assert_eq!(
            completed_publications(&answers, &submissions, &transcript),
            ["original"]
        );
    }

    #[test]
    fn completed_root_requires_its_accepted_submission_and_assistant_row() {
        let answers = vec![("original".into(), "final text".into())];
        let submission = json!({"command":"original","kind":"root_start","state":"accepted"});
        let row = json!({"kind":"assistant","text":"final text"});
        assert!(
            completed_publications(&answers, std::slice::from_ref(&submission), &[]).is_empty()
        );
        assert!(completed_publications(&answers, &[], std::slice::from_ref(&row)).is_empty());
        assert!(completed_publications(&answers, &[submission], &[row.clone(), row]).is_empty());
    }

    #[test]
    fn distinct_completed_roots_can_publish_identical_text_once_each() {
        let answers = vec![
            ("older".into(), "same answer".into()),
            ("original".into(), "same answer".into()),
        ];
        let submissions = vec![
            json!({"command":"older","kind":"root_start","state":"accepted"}),
            json!({"command":"original","kind":"root_start","state":"accepted"}),
        ];
        let row = json!({"kind":"assistant","text":"same answer"});
        assert_eq!(
            completed_publications(&answers, &submissions, &[row.clone(), row]),
            ["older", "original"]
        );
    }
}
