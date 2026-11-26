//! A turn whose result is ready before its drive observes any event.

use super::*;
use crate::runtime::{HostBoundary, HostWorkflow};
use crate::sessions::NoticeScope;
use crate::surface_protocol::UserAction;

/// Holds the root's delivery after its acknowledgement until the turn's
/// terminal is durable in the feed. The runtime installs the result right
/// after that commit, while admission still has its candidate save to make,
/// so the result is normally ready before the drive takes its first event; a
/// result that is not yet ready only weakens the check, never fails it.
fn hold_until_terminal(database: &std::path::Path, session: plakous::model::SessionId) {
    use plakous::sessions::AgentSessions;
    use plakous::sessions::feed::{FeedCursor, FeedFact, FeedPage, FeedPageRequest, FeedSeq};

    let deadline = Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let terminal = AgentSessions::inspect_feed_sqlite(
            database,
            FeedPageRequest::after(FeedCursor::new(session, FeedSeq::new(0))),
        )
        .is_ok_and(|page| match page {
            FeedPage::Records { records, .. } => records.iter().any(|record| {
                record
                    .facts()
                    .iter()
                    .any(|fact| matches!(fact, FeedFact::TurnTerminal { .. }))
            }),
            _ => false,
        });
        if terminal {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the scripted runtime never committed the turn's terminal"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn a_result_ready_before_the_drive_still_commits_the_retry_notice() {
    let root = tempfile::tempdir().unwrap();
    let plan = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "steps": [
                {"output": "reasoning_only", "finish": "length"},
                {"output": {"text": "retried answer"}, "finish": "stop"}
            ]
        }))
        .unwrap(),
    );
    let (mut config, _) = crate::runtime::recover_tests::scripted_host_with(root.path(), plan);
    let database = config.state_roots.runtime_database().to_path_buf();
    let session = Arc::new(std::sync::OnceLock::new());
    let held = Arc::clone(&session);
    crate::runtime::test_kit::scripted_mut(&mut config).observe =
        Arc::new(move |workflow, boundary| {
            if workflow == HostWorkflow::RootDelivery && boundary == HostBoundary::ProductCommitted
            {
                hold_until_terminal(&database, *held.get().unwrap());
            }
        });
    let (action_tx, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        RootActor::open_blocking(config, &actions, crate::runtime::Sender::over(tx), None).unwrap();
    let locator = host.role.config.session.plakous_locator.unwrap();
    session.set(locator.session_id()).unwrap();
    action_tx
        .send(UserAction::Prompt("retry once".into()))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap()
    else {
        panic!("prompt")
    };
    host.block(async |host| {
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .await
    })
    .unwrap();
    let retry = |row: &TranscriptRow| matches!(row, TranscriptRow::Notice(notice) if notice.scope == NoticeScope::Retry);
    assert!(
        events
            .try_iter()
            .any(|event| matches!(&event, HostEvent::Row(row) if retry(row))),
        "the retry notice delivered before the result was not shown"
    );
    let notices = host
        .role
        .config
        .store
        .load_intents(&host.role.config.session.id)
        .unwrap()
        .notices;
    assert!(
        notices.iter().any(|notice| retry(&notice.row)),
        "the retry notice delivered before the result was not committed: {notices:?}"
    );
    host.block(async |host| host.release().await).unwrap();
}
