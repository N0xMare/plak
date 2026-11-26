//! A turn whose result is ready before its drive observes any event.

use super::*;
use crate::runtime::{HostBoundary, HostWorkflow};
use crate::sessions::NoticeScope;
use crate::surface_protocol::UserAction;

/// Holds the root's delivery after its acknowledgement until the scripted
/// runtime has answered the retried round, so the turn's result is ready
/// before the drive takes its first event.
fn hold_until_answered(evidence: &std::path::Path) {
    let deadline = Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let answered = std::fs::read_to_string(evidence).is_ok_and(|text| {
            text.lines().any(|line| {
                let row: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
                row["kind"] == "provider_outcome" && row["round"] == 2
            })
        });
        if answered {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the scripted runtime never answered its retried round"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // The terminal commit and result publication follow the answer.
    std::thread::sleep(std::time::Duration::from_millis(300));
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
    let (mut config, evidence) =
        crate::runtime::recover_tests::scripted_host_with(root.path(), plan);
    crate::runtime::test_kit::scripted_mut(&mut config).observe =
        Arc::new(move |workflow, boundary| {
            if workflow == HostWorkflow::RootDelivery && boundary == HostBoundary::ProductCommitted
            {
                hold_until_answered(&evidence);
            }
        });
    let (action_tx, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        RootActor::open_blocking(config, &actions, crate::runtime::Sender::over(tx), None).unwrap();
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
