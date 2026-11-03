//! Spec 03 section 4.4 inside a running root turn: the per-round tree pass
//! settles a committed wait from the root's feed, publishes nothing without
//! a record past the cursor, and leaves a refused read or a page that needs
//! a refold as debt for a later pass.

#![allow(clippy::unwrap_used, reason = "unit tests")]
use crate::runtime::test_kit::BlockOn as _;

use super::HostLoop;
use super::wait_settlement_tests::{receipts_and_unread, wait_outside_history, with_unread_result};
use crate::agent_tree::{TreeFeed, TreeFeedReads, TreeProjectorError};
use crate::runtime::projection::{
    TreePass, TreePassOutcome, apply_running_root_feed, project_tree_as, running_pass_outcome,
};
use crate::runtime::test_kit::TestExtension;
use crate::topology_runtime::ProductTopologyRuntime;
use plakous::sessions::feed::{FeedFact, FeedRecord};
use std::sync::Arc;

fn root_session(host: &HostLoop<'_, TestExtension>) -> String {
    host.opened.locator().session_id().to_string()
}

fn cursor_and_revision(
    host: &HostLoop<'_, TestExtension>,
    runtime: &Arc<ProductTopologyRuntime>,
) -> (u64, u64) {
    let session = root_session(host);
    runtime
        .read(|tree| (tree.feed_cursor(&session), tree.revision))
        .unwrap()
}

fn receipt(runtime: &Arc<ProductTopologyRuntime>) -> String {
    let ids = runtime
        .read(|tree| tree.retained_wait_receipts("root"))
        .unwrap();
    assert_eq!(ids.len(), 1, "{ids:?}");
    ids[0].clone()
}

/// A root feed page with one record at `seq` whose history commits `call`.
fn history_page(host: &HostLoop<'_, TestExtension>, seq: u64, call: &str) -> TreeFeed {
    let locator = host.opened.locator();
    let header = serde_json::from_value(serde_json::json!({
        "session_id": locator.session_id(), "seq": seq,
        "branch_id": locator.branch_id(), "committed_at_ms": seq,
        "change": {"advanced": {"parent": locator.checkpoint_id(),
            "checkpoint": locator.checkpoint_id(), "revision": seq, "head": "quiescent"}}
    }))
    .unwrap();
    let entry = "00000000-0000-4000-8000-000000004000".parse().unwrap();
    let record = FeedRecord::from_store(
        header,
        vec![FeedFact::HistoryAppended {
            turn_id: Some("00000000-0000-4000-8000-000000000001".parse().unwrap()),
            segment: plakous::model::HistorySegmentKind::ToolExchange,
            first_ordinal: 0,
            count: 1,
            first_entry: entry,
            last_entry: entry,
            tool_results: vec![call.parse().unwrap()],
        }],
    );
    page(host, vec![record])
}

fn page(host: &HostLoop<'_, TestExtension>, records: Vec<FeedRecord>) -> TreeFeed {
    TreeFeed {
        session_id: root_session(host),
        retained_from: 1,
        below_retained: None,
        heads: Vec::new(),
        head_turns: std::collections::BTreeMap::new(),
        records,
        reads: TreeFeedReads::default(),
    }
}

#[test]
fn a_committed_wait_retires_its_receipt_and_advances_the_cursor() {
    with_unread_result(|host, runtime| {
        wait_outside_history(host, runtime);
        let call = receipt(runtime);
        let (cursor, _) = cursor_and_revision(host, runtime);
        let outcome = apply_running_root_feed(
            runtime,
            &history_page(host, cursor + 1, &call),
            "root",
            cursor + 1,
        )
        .block();
        assert_eq!(outcome, Ok(TreePassOutcome::Applied));
        assert_eq!(receipts_and_unread(runtime).0, 0);
        assert_eq!(cursor_and_revision(host, runtime).0, cursor + 1);
    });
}

#[test]
fn nothing_past_the_cursor_publishes_nothing() {
    with_unread_result(|host, runtime| {
        wait_outside_history(host, runtime);
        let before = cursor_and_revision(host, runtime);
        let outcome =
            apply_running_root_feed(runtime, &page(host, Vec::new()), "root", u64::MAX).block();
        assert_eq!(outcome, Ok(TreePassOutcome::Idle));
        assert_eq!(cursor_and_revision(host, runtime), before);
        assert_eq!(receipts_and_unread(runtime).0, 1);
    });
}

#[test]
fn a_refused_read_leaves_the_receipt_and_the_next_pass_retires_it_once() {
    with_unread_result(|host, runtime| {
        wait_outside_history(host, runtime);
        let call = receipt(runtime);
        let before = cursor_and_revision(host, runtime);
        crate::error::read_faults::inject(crate::agent::TREE_FEED_READ_FAILED, None, 1);
        let rows = host.config.topology_rows();
        let tree_id = runtime.read(|tree| tree.tree_id.clone()).unwrap();
        let refused = host
            .executor
            .block_on(project_tree_as(
                host.opened.sessions(),
                &rows,
                &tree_id,
                host.opened.locator().session_id(),
                TreePass::RunningTurn { through: u64::MAX },
            ))
            .unwrap();
        assert!(refused.is_err(), "the injected read refusal must surface");
        assert_eq!(cursor_and_revision(host, runtime), before);
        assert_eq!(receipt(runtime), call);

        let committed = history_page(host, before.0 + 1, &call);
        assert_eq!(
            apply_running_root_feed(runtime, &committed, "root", before.0 + 1).block(),
            Ok(TreePassOutcome::Applied)
        );
        let after = cursor_and_revision(host, runtime);
        assert_eq!(after.0, before.0 + 1);
        assert_eq!(receipts_and_unread(runtime).0, 0);
        // The same record again is behind the cursor: the pass is inert.
        apply_running_root_feed(runtime, &committed, "root", before.0 + 1)
            .block()
            .unwrap();
        assert_eq!(cursor_and_revision(host, runtime).0, after.0);
        assert_eq!(receipts_and_unread(runtime).0, 0);
    });
}

#[test]
fn a_page_that_needs_a_refold_is_deferred_not_refolded() {
    assert!(matches!(
        running_pass_outcome(Err(TreeProjectorError::RefoldRequired)),
        Ok(TreePassOutcome::Deferred(_))
    ));
    with_unread_result(|host, runtime| {
        wait_outside_history(host, runtime);
        let call = receipt(runtime);
        let before = cursor_and_revision(host, runtime);
        // A page that skips a record after the cursor needs a reread from
        // the feed's start; the running pass takes no session to reread with.
        let outcome = apply_running_root_feed(
            runtime,
            &history_page(host, before.0 + 2, &call),
            "root",
            u64::MAX,
        )
        .block();
        assert!(
            matches!(outcome, Ok(TreePassOutcome::Deferred(_))),
            "{outcome:?}"
        );
        assert_eq!(cursor_and_revision(host, runtime), before);
        assert_eq!(receipt(runtime), call);
    });
}

#[test]
fn a_delayed_round_observation_cannot_absorb_a_later_rounds_feed() {
    with_unread_result(|host, runtime| {
        wait_outside_history(host, runtime);
        let call = receipt(runtime);
        let (cursor, _) = cursor_and_revision(host, runtime);
        let mut feed = history_page(host, cursor + 1, &call);
        feed.records
            .extend(history_page(host, cursor + 2, &call).records);
        assert_eq!(
            apply_running_root_feed(runtime, &feed, "root", cursor + 1).block(),
            Ok(TreePassOutcome::Applied)
        );
        assert_eq!(cursor_and_revision(host, runtime).0, cursor + 1);
        assert_eq!(receipts_and_unread(runtime).0, 0);
        assert_eq!(
            apply_running_root_feed(runtime, &feed, "root", cursor + 2).block(),
            Ok(TreePassOutcome::Applied)
        );
        assert_eq!(cursor_and_revision(host, runtime).0, cursor + 2);
    });
}

#[test]
fn a_running_prefix_behind_retention_is_deferred_without_a_newer_head_seed() {
    with_unread_result(|host, runtime| {
        wait_outside_history(host, runtime);
        let before = cursor_and_revision(host, runtime);
        let mut feed = page(host, Vec::new());
        feed.below_retained = Some(before.0 + 2);
        assert!(matches!(
            apply_running_root_feed(runtime, &feed, "root", before.0 + 1).block(),
            Ok(TreePassOutcome::Deferred(_))
        ));
        assert_eq!(cursor_and_revision(host, runtime), before);
        assert_eq!(receipts_and_unread(runtime).0, 1);
    });
}
