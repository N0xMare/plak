use super::*;
use plak::store::session_records::{SessionRecordStore, SqliteSessionStore};
use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};

#[test]
fn converged_sqlite_views_are_byte_identical_after_replaying_every_feed_page() {
    for shape in [
        "direct_tool_then_final",
        "hosted_tool_call_final",
        "steer_accepted_mid_turn",
    ] {
        let (seed, id) = setup(shape);
        let fixture = seed.fork(&shapes::script(shape));
        let outcome = lifecycle(&fixture, &id, shape, None, None);
        assert!(outcome.status.success(), "{shape}: {}", outcome.stderr);
        summary::read_converged(&fixture);
        assert_idempotent(&fixture, &id, shape);
    }
}

pub(super) fn assert_idempotent(fixture: &Fixture, id: &str, shape: &str) {
    let paths = summary::paths(fixture);
    let store = SqliteSessionStore::new(paths.clone());
    let views = store.load_views(id).unwrap().unwrap();
    let intents = store.load_intents(id).unwrap();
    let record = store.load(id).unwrap();
    let sessions = plakous::sessions::AgentSessions::new(
        plakous::sessions::SessionBackend::sqlite(paths.runtime_database()).unwrap(),
    )
    .unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let before = view_snapshot::snapshot(&paths);
    executor
        .block_on(async {
            let mut cursor = FeedCursor::start(record.plakous_locator.unwrap().session_id());
            loop {
                let FeedPage::Records { records, tail, .. } = sessions
                    .feed_page(FeedPageRequest::after(cursor))
                    .await
                    .unwrap()
                else {
                    panic!("fixture feed was pruned")
                };
                let replayed =
                    plakous_hosted::sessions::projector::run(&sessions, &views, &intents, &records)
                        .await
                        .unwrap();
                assert_eq!(
                    replayed.views, views,
                    "{shape}: replay changed complete views"
                );
                assert_eq!(replayed.views.cursor, views.cursor);
                assert!(replayed.writes.is_empty(), "{shape}: replay planned writes");
                assert_eq!(
                    replayed.writes.successors,
                    [] as [plakous_hosted::sessions::projector::Successor; 0]
                );
                let applied = records
                    .last()
                    .map_or(cursor.applied(), |record| record.header().seq);
                if applied == tail {
                    break;
                }
                cursor = FeedCursor::new(cursor.session_id(), applied);
            }
        })
        .unwrap();
    let records: std::sync::Arc<dyn SessionRecordStore> = std::sync::Arc::new(store);
    executor
        .block_on(plakous_hosted::runtime::project_session(
            &sessions, &records, &record,
        ))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(view_snapshot::snapshot(&paths), before);
}
