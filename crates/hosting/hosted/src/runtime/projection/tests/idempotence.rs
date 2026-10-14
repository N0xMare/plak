use super::*;
use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};

#[test]
fn converged_memory_views_are_byte_identical_after_replaying_every_feed_page() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    run_prompts(config, &["first", "second"]);
    let views = caught_up(root.path(), &store, &id);
    let intents = store.load_intents(&id).unwrap();
    // Debug includes keyed maps that SessionViewState's SQL serialization skips.
    let before = format!("{views:?}").into_bytes();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(async {
            let mut cursor = FeedCursor::start(
                store
                    .load(&id)
                    .unwrap()
                    .plakous_locator
                    .unwrap()
                    .session_id(),
            );
            loop {
                let FeedPage::Records { records, tail, .. } = sessions
                    .feed_page(FeedPageRequest::after(cursor))
                    .await
                    .unwrap()
                else {
                    panic!("fixture feed was pruned")
                };
                let replayed =
                    crate::sessions::projector::run(&sessions, &views, &intents, &records)
                        .await
                        .unwrap();
                assert_eq!(replayed.views, views);
                assert_eq!(replayed.views.cursor, views.cursor);
                assert_eq!(format!("{:?}", replayed.views).into_bytes(), before);
                assert!(replayed.writes.is_empty());
                assert!(replayed.writes.successors.is_empty());
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
    assert_eq!(caught_up(root.path(), &store, &id), views);
    assert_eq!(
        format!("{:?}", store.load_views(&id).unwrap().unwrap()).into_bytes(),
        before
    );
}
