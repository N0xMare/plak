#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;

use crate::runtime::events::model::{project_live_usage, publish_model_diagnostic};
use crate::session_home::SessionHome;
use crate::session_log::SessionLog;
use crate::sessions::{NoticeScope, SessionRecord, TranscriptRow};

#[test]
fn a_history_projection_diagnostic_goes_to_the_session_log_only() {
    use plakous::model::{Digest, DigestDomain, ModelDiagnostic};

    let dir = tempfile::tempdir().unwrap();
    let home = SessionHome::new(dir.path().join("sessions"));
    let session = SessionRecord::new(dir.path().to_path_buf());
    let log = SessionLog::open(&home, &session.id).unwrap();
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let identity = Digest::sha256(DigestDomain::HistoryProjection, 1, &[b"first"]).unwrap();
    publish_model_diagnostic(
        &ModelDiagnostic::historical_projection(identity, "projected history"),
        Some(&log),
        &session.id,
        &events,
    );
    assert!(rx.try_iter().next().is_none(), "never rendered");
    assert!(session.transcript.is_empty(), "never persisted");
    let logged = crate::session_log::read_tail_for_kind(
        &home,
        &session.id,
        4,
        Some("model.history_projection"),
    )
    .unwrap();
    assert_eq!(logged.len(), 1);
    assert!(logged[0].contains(&identity.to_string()), "{logged:?}");
}

#[test]
fn a_generic_model_diagnostic_is_a_live_turn_notice_and_never_persisted() {
    use plakous::model::ModelDiagnostic;

    let dir = tempfile::tempdir().unwrap();
    let session = SessionRecord::new(dir.path().to_path_buf());
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    for _ in 0..2 {
        publish_model_diagnostic(
            &ModelDiagnostic::new("same text"),
            None,
            &session.id,
            &events,
        );
    }
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![
            HostEvent::Row(TranscriptRow::info(NoticeScope::Turn, "same text")),
            HostEvent::Row(TranscriptRow::info(NoticeScope::Turn, "same text")),
        ]
    );
    assert!(session.transcript.is_empty());
}

/// A live usage report is a snapshot, never a turn total.
#[test]
fn live_usage_projects_as_a_non_terminal_snapshot() {
    let update = plakous::model::UsageUpdate::new(plakous::model::Usage::new(11, 3, 2, 7, 0));
    assert_eq!(
        project_live_usage(&update),
        crate::surface_protocol::ContentEvent::<TestExtension>::TurnUsage {
            input_tokens: 11,
            output_tokens: 3,
            reasoning_tokens: 2,
            cached_input_tokens: 7,
            cache_write_tokens: 0,
            // Coverage is unknown: a live report is one provider-reported
            // cumulative snapshot over an unknown number of operations, and
            // the host never fabricates a `1/1`.
            model_operations: None,
            reporting_model_operations: None,
            peak_prepared_input_tokens: None,
            terminal: false,
        }
    );
}
