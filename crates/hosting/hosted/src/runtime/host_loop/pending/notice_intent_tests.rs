//! Tree milestones and the auto-continue and tree pause lines are operator
//! notice intents (spec 02 §2.6, row provenance (d)): they are written as
//! intents when the host observes them and survive a rebuild of the views
//! from empty at their anchors.

use std::sync::Arc;

use super::tests::{pause_rows, pause_tree, reopen, snapshots, spawn_child, with_configured_tree};
use super::{AUTO_CONTINUE_PAUSE_TEXT, TREE_PAUSE_TEXT};
use crate::runtime::host_loop::HostLoop;
use crate::runtime::test_kit::TestExtension;
use crate::session_store::{MemoryStore, SessionRecordStore as _};
use crate::sessions::TranscriptRow;
use crate::surface_protocol::UserAction;

fn notice_texts(store: &MemoryStore, id: &str) -> Vec<String> {
    store
        .load_intents(id)
        .unwrap()
        .notices
        .iter()
        .filter_map(|notice| match &notice.row {
            TranscriptRow::Notice(notice) => Some(notice.text.clone()),
            TranscriptRow::Milestone(_) => Some("milestone".to_owned()),
            _ => None,
        })
        .collect()
}

/// The transcript a load shows: the projector's view rows merged with the
/// notice intents at their anchors (notices are never view rows).
fn loaded_rows(store: &MemoryStore, id: &str) -> Vec<TranscriptRow> {
    store
        .load(id)
        .unwrap()
        .transcript
        .rows()
        .into_iter()
        .map(|(row, _)| row.clone())
        .collect()
}

#[test]
fn pause_lines_and_milestones_are_notice_intents_that_survive_a_rebuild() {
    let store = Arc::new(MemoryStore::new());
    let memory = Arc::clone(&store);
    with_configured_tree(
        0,
        move |config| config.store = memory,
        |host, runtime, actions, observed| {
            let id = host.config.session.id.clone();
            let observe_tree = |host: &mut HostLoop<'_, TestExtension>| {
                crate::runtime::tree::emit_tree_snapshot_if_revision_advanced(
                    &crate::runtime::HostRows::new(
                        &host.config.state_roots,
                        host.config.store.as_ref(),
                    ),
                    &*host.config.store,
                    &host.config.home,
                    &mut host.config.session,
                    &mut host.last_tree_revision,
                    &host.events,
                    false,
                );
            };
            observe_tree(host);
            spawn_child(runtime);
            observe_tree(host);
            actions.send(UserAction::Interrupt).unwrap();
            assert!(matches!(
                host.next_prompt(None).unwrap(),
                crate::runtime::host_loop::HostStep::Continue
            ));
            pause_tree(runtime);
            host.publish_pending_continuation();
            let _ = snapshots(observed);

            let intents = notice_texts(&store, &id);
            for expected in [AUTO_CONTINUE_PAUSE_TEXT, TREE_PAUSE_TEXT] {
                assert_eq!(
                    intents.iter().filter(|text| *text == expected).count(),
                    1,
                    "{expected} is one notice intent: {intents:?}"
                );
            }
            let notices: Vec<TranscriptRow> = host
                .config
                .session
                .transcript
                .rows()
                .into_iter()
                .map(|(row, _)| row.clone())
                .filter(|row| {
                    matches!(row, TranscriptRow::Milestone(_))
                        || matches!(row, TranscriptRow::Notice(notice)
                            if notice.text == AUTO_CONTINUE_PAUSE_TEXT
                                || notice.text == TREE_PAUSE_TEXT)
                })
                .collect();
            let milestones = notices
                .iter()
                .filter(|row| matches!(row, TranscriptRow::Milestone(_)))
                .count();
            assert!(milestones >= 1, "the spawn is a live milestone row");
            assert_eq!(
                intents.iter().filter(|text| *text == "milestone").count(),
                milestones,
                "every live milestone is one notice intent: {intents:?}"
            );

            host.config.store.save(&mut host.config.session).unwrap();
            store.delete_views(&id);
            reopen(host, |restored| {
                let sessions = restored.config.state_roots.runtime_sessions().unwrap();
                let records: Arc<dyn crate::session_store::SessionRecordStore> = store.clone();
                let locator = restored.config.session.plakous_locator.unwrap();
                restored
                    .executor
                    .block_on(crate::runtime::projection::project_to_tail(
                        &sessions, &records, &id, locator,
                    ))
                    .unwrap()
                    .unwrap();
                let rebuilt = loaded_rows(&store, &id);
                for row in &notices {
                    assert_eq!(
                        rebuilt.iter().filter(|stored| *stored == row).count(),
                        1,
                        "the rebuild keeps {row:?} once: {rebuilt:#?}"
                    );
                }
                assert_eq!(
                    pause_rows(&restored.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                    1
                );
                assert_eq!(pause_rows(&restored.config.session, TREE_PAUSE_TEXT), 1);
            });
        },
    );
}
