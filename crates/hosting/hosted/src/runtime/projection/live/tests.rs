#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;
use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
use crate::session_store::MemoryStore;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct TargetCheckGate {
    entered: Notify,
    release: Notify,
}

fn target_gates()
-> &'static std::sync::Mutex<std::collections::BTreeMap<String, Arc<TargetCheckGate>>> {
    static GATES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, Arc<TargetCheckGate>>>,
    > = std::sync::OnceLock::new();
    GATES.get_or_init(std::sync::Mutex::default)
}

pub(super) async fn pause_noncurrent(id: &str, forwarder_alive: bool) {
    let gate = target_gates().lock().unwrap().remove(id);
    if let Some(gate) = gate {
        gate.entered.notify_one();
        assert!(
            forwarder_alive,
            "the A subscription must survive coalescing back to A"
        );
        gate.release.notified().await;
    }
}

#[derive(Default)]
struct PausePass {
    armed: AtomicBool,
    entered: Notify,
    released: std::sync::Mutex<bool>,
    release: std::sync::Condvar,
}

impl PausePass {
    fn unblock(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

impl StoreFaults for Arc<PausePass> {
    fn before(
        &self,
        call: StoreCall,
        _: Option<&crate::sessions::SessionRecord>,
    ) -> Result<(), String> {
        if call == StoreCall::LoadInheritedSubmissions && self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.release.wait(released).unwrap();
            }
        }
        Ok(())
    }
}

struct UnblockOnDrop(Arc<PausePass>);

impl Drop for UnblockOnDrop {
    fn drop(&mut self) {
        self.0.unblock();
    }
}

#[test]
fn coalescing_a_b_a_during_a_pass_preserves_new_settled_rows() {
    use crate::runtime::recover_tests::{script, scripted_host_with};
    let root = tempfile::tempdir().unwrap();
    let plan = || {
        script(serde_json::json!([
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": "the effect ran"}}
        ]))
    };
    let store = Arc::new(MemoryStore::new());
    let (mut config, _) = scripted_host_with(root.path(), plan());
    config.store = store.clone();
    let id = config.session.id.clone();
    super::super::tests::run_prompts(config, &["first effect"]);
    let (mut config, _) = scripted_host_with(root.path(), plan());
    config.store = store.clone();
    config.session = store.load(&id).unwrap();
    let pause = Arc::new(PausePass::default());
    let emitted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&emitted);
    let target = LiveTarget {
        sessions: config.state_roots.runtime_sessions().unwrap(),
        store: Arc::new(FaultStore::new(store.clone(), Arc::clone(&pause))),
        roots: config.state_roots.clone(),
        product_id: id.clone(),
        followed: config.session.plakous_locator.unwrap(),
        tree_id: id.clone(),
        emit_settled: Arc::new(move |row| sink.lock().unwrap().push(row)),
    };
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let _unblock = UnblockOnDrop(Arc::clone(&pause));
    let mut live = executor
        .block_on(async {
            let mut live = LiveProjector::spawn();
            live.tick(target.clone());
            live.applied
                .wait_for(|applied| applied.generation == 1)
                .await
                .unwrap();
            assert!(live.take_refusal().is_none());
            live
        })
        .unwrap();
    assert_eq!(emitted.lock().unwrap().len(), 1);
    pause.armed.store(true, Ordering::SeqCst);
    live.tick(target.clone());
    executor.block_on(pause.entered.notified()).unwrap();
    super::super::tests::run_prompts(config, &["second effect"]);
    executor
        .block_on(async {
            let gate = Arc::new(TargetCheckGate::default());
            target_gates()
                .lock()
                .unwrap()
                .insert(id.clone(), Arc::clone(&gate));
            let mut b = target.clone();
            b.product_id = "coalesced-other-target".into();
            live.tick(b);
            pause.unblock();
            gate.entered.notified().await;
            live.tick(target.clone());
            gate.release.notify_one();
            let views = live.finish(target, None).await.unwrap();
            let expected: Vec<_> = views
                .rows
                .values()
                .filter(|row| row.as_tool().is_some_and(|tool| tool.state != "running"))
                .cloned()
                .collect();
            assert_eq!(expected.len(), 2);
            assert_eq!(*emitted.lock().unwrap(), expected);
        })
        .unwrap();
}

struct RefuseCommit(Arc<AtomicBool>);

impl StoreFaults for RefuseCommit {
    fn before(
        &self,
        call: StoreCall,
        _: Option<&crate::sessions::SessionRecord>,
    ) -> Result<(), String> {
        if call == StoreCall::CommitProjectionViews && self.0.load(Ordering::SeqCst) {
            return Err("injected projection commit failure".into());
        }
        Ok(())
    }
}

#[test]
fn a_failed_live_page_drops_its_views_and_the_next_pass_reloads() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = crate::runtime::test_support::fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    super::super::tests::run_prompts(config, &["recorded turn"]);
    let record = store.load(&id).unwrap();
    let (config, _) = crate::runtime::test_support::fresh_injected_attached_host(root.path());
    let refuse = Arc::new(AtomicBool::new(true));
    let target = LiveTarget {
        sessions: config.state_roots.runtime_sessions().unwrap(),
        store: Arc::new(FaultStore::new(
            store.clone(),
            RefuseCommit(Arc::clone(&refuse)),
        )),
        roots: config.state_roots,
        product_id: id.clone(),
        followed: record.plakous_locator.unwrap(),
        tree_id: id,
        emit_settled: Arc::new(|_| {}),
    };
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let records: Arc<dyn SessionRecordStore> = store.clone();
    let expected = executor
        .block_on(super::super::project_to_tail(
            &target.sessions,
            &records,
            &target.product_id,
            target.followed,
        ))
        .unwrap()
        .unwrap();
    store.delete_views(&target.product_id);
    executor
        .block_on(async {
            let mut live = LiveProjector::spawn();
            let reads = store.view_read_count();
            live.tick(target.clone());
            live.applied
                .wait_for(|applied| applied.generation == 1)
                .await
                .unwrap();
            assert_eq!(
                live.take_refusal().as_deref(),
                Some("injected projection commit failure")
            );
            assert_eq!(store.view_read_count(), reads + 1);
            refuse.store(false, Ordering::SeqCst);
            let actual = live.finish(target.clone(), None).await.unwrap();
            assert_eq!(
                store.view_read_count(),
                reads + 2,
                "the failed page cannot retain mutated views"
            );
            assert_eq!(actual, expected);
            let mut live = LiveProjector::spawn();
            live.tick(target.clone());
            live.applied
                .wait_for(|applied| applied.generation == 1)
                .await
                .unwrap();
            let reads = store.view_read_count();
            let records = store.record_read_count();
            assert_eq!(live.finish(target, None).await.unwrap(), expected);
            assert_eq!(
                store.view_read_count(),
                reads,
                "finish reuses the live views"
            );
            assert_eq!(
                store.record_read_count(),
                records,
                "finish needs no full record"
            );
        })
        .unwrap();
}

#[test]
fn a_facade_read_failure_on_page_two_reloads_the_last_committed_page() {
    use crate::sessions::projector::{ProductIntents, read_fault, run};
    use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = crate::runtime::test_support::fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    super::super::tests::run_prompts(config, &["recorded turn"; 80]);
    let followed = store.load(&id).unwrap().plakous_locator.unwrap();
    let (config, _) = crate::runtime::test_support::fresh_injected_attached_host(root.path());
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let records = super::super::tests::feed_records(&sessions, followed.session_id());
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(async {
            let FeedPage::Records { records: first, .. } = sessions
                .feed_page(FeedPageRequest::after(FeedCursor::start(
                    followed.session_id(),
                )))
                .await
                .unwrap()
            else {
                panic!("retained feed required")
            };
            assert!(records.len() > first.len(), "fixture must span pages");
            let mut intents: ProductIntents = store.load_intents(&id).unwrap();
            intents
                .submissions
                .extend(store.load_inherited_submissions(&id).unwrap());
            let prefix = run(&sessions, SessionViews::empty(), &intents, &first)
                .await
                .unwrap()
                .views;
            let expected = run(&sessions, SessionViews::empty(), &intents, &records)
                .await
                .unwrap()
                .views;
            store.delete_views(&id);
            read_fault::arm(followed.session_id(), prefix.cursor);
            let target = LiveTarget {
                sessions,
                store: store.clone(),
                roots: config.state_roots,
                product_id: id.clone(),
                followed,
                tree_id: id.clone(),
                emit_settled: Arc::new(|_| {}),
            };
            let reads = store.view_read_count();
            let mut live = LiveProjector::spawn();
            live.tick(target.clone());
            live.applied
                .wait_for(|applied| applied.generation == 1)
                .await
                .unwrap();
            assert!(
                live.take_refusal()
                    .unwrap()
                    .starts_with("projector read failed:")
            );
            assert_eq!(store.view_read_count(), reads + 1);
            assert_eq!(store.load_views(&id).unwrap(), Some(prefix));
            let reads = store.view_read_count();
            assert_eq!(live.finish(target, None).await.unwrap(), expected);
            assert_eq!(
                store.view_read_count(),
                reads + 1,
                "read error must discard live cached views"
            );
            assert_eq!(store.load_views(&id).unwrap(), Some(expected));
        })
        .unwrap();
}

#[test]
fn changed_keys_emit_the_same_settled_rows_as_a_full_scan() {
    use crate::sessions::projector::{Lane, ViewWrites};
    use std::cell::RefCell;
    let tool = |call: &str, state: &str| {
        TranscriptRow::Tool(ToolItem {
            call_id: call.into(),
            name: "bash".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("bash"),
            state: state.into(),
            subject: None,
            detail: None,
            started_at_ms: None,
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: None,
            wait: None,
        })
    };
    let a = RowKey::new(1, Lane::Runtime, 0);
    let b = RowKey::new(2, Lane::Runtime, 0);
    let c = RowKey::new(3, Lane::Runtime, 0);
    let mut views = SessionViews::empty();
    let mut shown = SettledRows::default();
    let mut scanned = std::collections::HashMap::new();
    for changes in [
        vec![(a, Some(tool("a", "running")))],
        vec![(b, Some(tool("b", "succeeded")))],
        vec![(a, Some(tool("a", "succeeded")))],
        vec![(a, None), (c, Some(tool("a", "succeeded")))],
        vec![(b, Some(tool("b", "failed")))],
        vec![(c, None)],
        vec![(b, Some(tool("b", "running")))],
        vec![(b, Some(tool("b", "succeeded")))],
    ] {
        let before = views.clone();
        for (key, row) in changes {
            if let Some(row) = row {
                views.rows.insert(key, row);
            } else {
                views.rows.remove(&key);
            }
        }
        let writes = ViewWrites::between(&before, &views);
        let changed = writes
            .upserts
            .iter()
            .map(|(key, _)| *key)
            .chain(writes.deletes)
            .collect();
        let mut retained = std::collections::HashMap::new();
        let mut expected = Vec::new();
        for row in views.rows.values() {
            if let Some(tool) = row.as_tool().filter(|tool| tool.state != "running") {
                if scanned.get(&tool.call_id) != Some(tool) {
                    expected.push(row.clone());
                }
                retained.insert(tool.call_id.clone(), tool.clone());
            }
        }
        scanned = retained;
        let emitted = RefCell::new(Vec::new());
        emit_settled_changes(&views, Some(changed), &mut shown, &|row| {
            emitted.borrow_mut().push(row);
        });
        assert_eq!(*emitted.borrow(), expected);
        emitted.borrow_mut().clear();
        emit_settled_changes(&views, None, &mut shown, &|row| {
            emitted.borrow_mut().push(row);
        });
        assert!(
            emitted.borrow().is_empty(),
            "reloading cannot duplicate settled rows"
        );
    }
    let emitted = RefCell::new(Vec::new());
    shown = SettledRows::default();
    emit_settled_changes(
        &views,
        Some(std::collections::BTreeSet::new()),
        &mut shown,
        &|row| {
            emitted.borrow_mut().push(row);
        },
    );
    assert_eq!(
        *emitted.borrow(),
        vec![tool("b", "succeeded")],
        "a replacement subscription reconciles settled rows even at an unchanged cursor"
    );
}
