//! Opt-in release-profile long-history hosted cold-path measurements.

use super::*;
use plakous::model::{
    Actor, ContentBlock, ConversationItem, EntryId, HistorySegment, HistorySegmentKind, ModelId,
    Provenance, TextBlock,
};
use plakous::sessions::{AgentSessions, PortableSessionHistory};
use std::time::Instant;

#[test]
#[ignore = "informational release-profile benchmark; no inference"]
#[allow(clippy::print_stderr, reason = "opt-in informational timing output")]
fn long_lineage_host_ready_fork_and_status() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let database = config.product_home.join("runtime.sqlite3");
    let payload = "x".repeat(4_600);
    let segments: Vec<_> = (0..4_096_u64)
        .map(|index| {
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![
                    ConversationItem::new(
                        EntryId::try_from(uuid::Uuid::from_u128(u128::from(index) + 1)).unwrap(),
                        Actor::User,
                        vec![ContentBlock::Text(
                            TextBlock::new(payload.clone(), None).unwrap(),
                        )],
                        Provenance::UserInput,
                    )
                    .unwrap(),
                ],
            )
            .unwrap()
        })
        .collect();
    let history_canonical_bytes: usize = segments
        .iter()
        .map(|segment| segment.canonical_bytes().len())
        .sum();
    config.seed_history = Some(PortableSessionHistory::new(
        segments,
        "synthetic-no-replay".into(),
        ModelId::new("offline-loopback-v1").unwrap(),
    ));
    let (source, _) = crate::runtime::startup::create_seeded_session(config).unwrap();
    let objects =
        crate::runtime::test_support::durable_authority_count(&database, "runtime_objects");
    assert!(objects > 8_000);
    let config = reopened(root.path(), &store, &source.id);
    let started = Instant::now();
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    loop {
        if matches!(
            events.recv_timeout(Duration::from_secs(90)).unwrap(),
            HostEvent::AgentAttached(true)
        ) {
            break;
        }
    }
    let resume_ready_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    assert!(
        AgentSessions::inspect_recovery_sqlite(&database, source.plakous_locator.unwrap())
            .unwrap()
            .is_none()
    );
    let status_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    send_attached_action(
        &actions,
        UserAction::ForkSession {
            account: None,
            compact: false,
        },
    );
    loop {
        let event = events.recv_timeout(Duration::from_secs(90)).unwrap();
        if let HostEvent::Row(TranscriptRow::Notice(notice)) = event {
            assert_ne!(
                notice.tone,
                crate::sessions::NoticeTone::Error,
                "{}",
                notice.text
            );
            if notice.text.starts_with("forked session") {
                break;
            }
        }
    }
    let fork_now_ns = started.elapsed().as_nanos();
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    eprintln!(
        "{}",
        serde_json::json!({
            "kind": "hosted-long-lineage-cold-path", "history_segments": 4096,
        "history_canonical_bytes": history_canonical_bytes, "objects": objects,
            "resume_host_ready_ns": resume_ready_ns, "fork_now_ns": fork_now_ns,
            "recovery_status_ns": status_ns,
            "scope": "real hosted loop and SQLite runtime; memory product store; status excludes CLI startup",
        })
    );
}
