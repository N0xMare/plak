use std::cell::RefCell;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use plakous::sessions::feed::FeedTerminalOrigin;

use crate::agent_tree::ProductAgentTree;
use crate::agent_tree_protocol::WriterMode;
use crate::runtime::projection::{RootTreePass, TreePassOutcome, probe, root_tree_pass};
use crate::runtime::topology_state::{TopologyRows, TreeRows, TreeWriteError};
use crate::sessions::SessionRecord;

use super::{MemoryStore, SessionRecordStore as _, fresh_injected_attached_host, run_prompts};

struct RegressingRows {
    tree: ProductAgentTree,
    session: String,
    first_read: AtomicBool,
}

impl TreeRows for RegressingRows {
    fn tree(&self, tree_id: &str) -> Result<Option<ProductAgentTree>, String> {
        if tree_id != self.tree.tree_id {
            return Ok(None);
        }
        let mut tree = self.tree.clone();
        if self.first_read.swap(false, Ordering::Relaxed) {
            tree.seed_feed_cursor(&self.session, 1);
        }
        Ok(Some(tree))
    }

    fn tree_exists(&self, tree_id: &str) -> Result<bool, String> {
        Ok(tree_id == self.tree.tree_id)
    }

    fn save_tree(&self, tree: &mut ProductAgentTree) -> Result<(), TreeWriteError> {
        assert!(tree.feed_cursor(&self.session) > 1);
        Ok(())
    }
}

impl TopologyRows for RegressingRows {
    fn session(&self, _: &str) -> Result<SessionRecord, String> {
        Err("the fixture only serves tree rows".into())
    }

    fn tree_source(&self, _: &str) -> Result<Option<String>, String> {
        Err("the fixture only serves tree rows".into())
    }
}

#[test]
fn wake_admission_keeps_its_terminal_origin_on_refold() {
    let dir = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(dir.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    run_prompts(config, &["publish a feed prefix"]);
    let locator = store.load(&id).unwrap().plakous_locator.unwrap();

    let (config, _) = fresh_injected_attached_host(dir.path());
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let tree_id = "wake-origin-refold";
    let tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id,
        "root",
        "owner",
        config.cwd,
        "selected",
        WriterMode::SingleLease,
    )
    .unwrap();
    let rows = RegressingRows {
        tree,
        session: locator.session_id().to_string(),
        first_read: AtomicBool::new(true),
    };
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(probe::TREE_FEED_READS.scope(RefCell::default(), async {
            let pass = root_tree_pass(
                &sessions,
                &rows,
                tree_id,
                locator.session_id(),
                RootTreePass::WakeAdmission,
            )
            .await
            .unwrap();
            assert_eq!(pass, TreePassOutcome::Applied);
            // The cursor regressed between reading it and applying the page.
            // Only a real retry reaches the retained prefix at cursor zero.
            probe::TREE_FEED_READS.with(|reads| {
                assert_eq!(
                    reads.borrow().as_slice(),
                    &[
                        (1, FeedTerminalOrigin::WakeAdmission),
                        (0, FeedTerminalOrigin::WakeAdmission),
                    ]
                );
            });
        }))
        .unwrap();
}
