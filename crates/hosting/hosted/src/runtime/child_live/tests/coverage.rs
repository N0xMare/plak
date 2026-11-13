//! The canonical attach cursor separates unavailable prefixes from live appends.

use super::*;
use plakous_offline_crash_harness::gate::GateHandle;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coverage_starts_at_the_history_cursor_and_retires_at_terminal() {
    let before = Arc::new(GateHandle::new());
    let during = Arc::new(GateHandle::new());
    let finish = GateHandle::new();
    let (_root, sessions, attached, turn, live) = fixture_with(serde_json::json!([
        {"output":{"text":"committed prefix"}},
        {"output":{"text":"before-attachduring-attachafter-attach"}, "stream":[
            {"text":"before-attach"}, {"gate":before.script()},
            {"text":"during-attach"}, {"gate":during.script()},
            {"text":"after-attach"}, {"gate":finish.script()}
        ]}
    ]))
    .await;
    before.entered().await;
    let release = Arc::clone(&before);
    let reached = Arc::clone(&during);
    hooks::set_before_attach(
        "coverage",
        Arc::new(move || {
            let release = Arc::clone(&release);
            let reached = Arc::clone(&reached);
            Box::pin(async move {
                release.release();
                reached.entered().await;
            })
        }),
    );
    let ready = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let notified = Arc::clone(&ready);
    let resumed = Arc::clone(&resume);
    hooks::set_before_follow(
        "coverage",
        Arc::new(move || {
            let ready = Arc::clone(&notified);
            let resume = Arc::clone(&resumed);
            Box::pin(async move {
                ready.notify_one();
                resume.notified().await;
            })
        }),
    );
    let (wire, received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let mut observers = ChildObservers::default();
    observers.start_agent_view(
        &crate::TaskSet::new(),
        false,
        "s1",
        "coverage",
        attached.locator(),
        sessions,
        Some((
            turn.id(),
            attached.agent().clone(),
            live.owner.observation.clone(),
        )),
        &sender,
    );
    ready.notified().await;
    let mut state = plakous_host_protocol::views::transcript::TranscriptState::default();
    let mut ops = Vec::new();
    let first = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert!(
        first
            .rows
            .contains(&TranscriptRow::assistant("committed prefix"))
    );
    assert!(
        !first
            .rows
            .iter()
            .any(|row| row.text().is_some_and(|text| text.contains("-attach")))
    );
    let HostEvent::TranscriptPatch(marker) = next_event(&received).await else {
        panic!("the coverage marker is an addressed keyed row");
    };
    assert_eq!(marker.scope, TranscriptScope::Agent("coverage".into()));
    assert_eq!(marker.upserts.len(), 1);
    assert_eq!(
        marker.upserts[0].row,
        TranscriptRow::info(
            NoticeScope::Tree,
            "Live view starts here · earlier streaming output not shown"
        )
    );
    state.apply(marker).unwrap();
    assert!(sender.resync_transcript(&TranscriptScope::Agent("coverage".into())));
    let resynced = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert_eq!(
        resynced
            .rows
            .iter()
            .filter(|row| row.text() == Some(GAP))
            .count(),
        1,
        "resync preserves the segment instead of adding a notice"
    );

    during.release();
    finish.entered().await;
    resume.notify_one();
    let streamed = next_event(&received).await;
    assert!(matches!(&streamed, HostEvent::TranscriptStream {scope, ..}
                    if *scope == TranscriptScope::Agent("coverage".into())));
    assert!(carries_text(&streamed, "after-attach"), "{streamed:?}");
    assert!(!carries_text(&streamed, "before-attach"));
    assert!(!carries_text(&streamed, "during-attach"));
    finish.release();
    turn.result().await.unwrap();
    let ended = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert!(
        !ops.iter().any(|event| carries_text(event, GAP)),
        "one marker per segment"
    );
    assert!(!ops.iter().any(|event| matches!(event,
        HostEvent::TranscriptStream { delta: plakous_host_protocol::views::transcript::TranscriptStreamDelta::Assistant(text), .. }
        if text.contains("after-attach"))), "the live append arrives only once");
    assert_eq!(
        ended
            .rows
            .iter()
            .filter(|row| row.text() == Some("before-attachduring-attachafter-attach"))
            .count(),
        1,
        "canonical refresh replaces the provisional tail exactly once"
    );
    assert!(!ended.rows.iter().any(|row| row.text() == Some(GAP)));
    drop(observers);
    attached.release().await.unwrap();
}
