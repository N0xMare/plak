#![allow(clippy::unwrap_used, reason = "unit tests")]

#[cfg(test)]
mod terminal_storage_tests;

#[test]
fn live_host_counts_provider_hosted_calls_from_the_projected_views() {
    assert_hosted_calls_projected(false);
}

#[test]
fn hosted_call_counts_catch_up_after_deferred_projection() {
    assert_hosted_calls_projected(true);
}

#[test]
fn live_patch_fold_and_quiet_settlement_keep_the_same_keys_after_exact_reopen() {
    use crate::runtime::Sender;
    use crate::surface_protocol::UserAction;
    use plakous_host_protocol::views::transcript::TranscriptState;
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let store = config.store.clone();
    let (action_tx, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = HostLoop::open(config, &actions, Sender::over(wire), None).unwrap();
    let id = host.config.session.id.clone();
    let mut folded = TranscriptState::default();
    for event in events.try_iter() {
        if let HostEvent::PresentationSnapshot(snapshot) = event {
            folded.replace(snapshot).unwrap();
        }
    }
    assert!(folded.revision() > 0);
    action_tx
        .send(UserAction::Prompt("check live and restored keys".into()))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt");
    };
    host.run_prompt(text, queued_delivery, user_echoed, None)
        .unwrap();
    host.publish_projected_transcript(true);
    let mut patches = 0;
    for event in events.try_iter() {
        match event {
            HostEvent::TranscriptPatch(patch) => {
                folded.apply(patch).unwrap();
                patches += 1;
            }
            HostEvent::PresentationSnapshot(_) => {
                panic!("ordinary turn settlement must not replace its baseline")
            }
            _ => {}
        }
    }
    assert!(patches > 0);
    let expected = super::super::super::logging::project_session_snapshot(&host.config.session);
    assert_eq!(
        folded.items(),
        &expected
            .items
            .into_iter()
            .map(|item| (item.key, item.row))
            .collect()
    );
    host.publish_projected_transcript(true);
    let quiet: Vec<_> = events
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::TranscriptPatch(patch) => Some(patch),
            HostEvent::PresentationSnapshot(_) => panic!("quiet settlement must publish a patch"),
            _ => None,
        })
        .collect();
    assert_eq!(quiet.len(), 1);
    assert!(quiet[0].upserts.is_empty() && quiet[0].deletes.is_empty());
    folded.apply(quiet.into_iter().next().unwrap()).unwrap();
    host.release().unwrap();

    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.session = store.load(&id).unwrap();
    config.store = store;
    let (wire, restored) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let reopened = HostLoop::open(config, &actions, Sender::over(wire), None).unwrap();
    let snapshot = restored
        .try_iter()
        .find_map(|event| match event {
            HostEvent::PresentationSnapshot(snapshot) => Some(snapshot),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        folded.items(),
        &snapshot
            .items
            .into_iter()
            .map(|item| (item.key, item.row))
            .collect()
    );
    reopened.release().unwrap();
}

fn assert_hosted_calls_projected(defer_projection: bool) {
    use crate::surface_protocol::UserAction;

    let root = tempfile::tempdir().unwrap();
    let plan = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "hosted_tool_call_final": true,
            "steps": [{"output": {"text": "hosted answer"}}]
        }))
        .unwrap(),
    );
    let (config, _) = crate::runtime::recover_tests::scripted_host_with(root.path(), plan);
    let (action_tx, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        HostLoop::open(config, &actions, crate::runtime::Sender::over(tx), None).unwrap();
    let store = host.config.store.clone();
    if defer_projection {
        host.live.take();
        host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
            store.clone(),
            crate::runtime::test_support::SlowProjection {
                delay: std::time::Duration::ZERO,
                refuse: true,
            },
        ));
    }
    action_tx
        .send(UserAction::Prompt("hosted prompt".into()))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt")
    };
    host.run_prompt(text, queued_delivery, user_echoed, None)
        .unwrap();
    if defer_projection {
        assert_eq!(host.config.session.tool_call_count, 0);
    }
    host.config.store = store;
    // Interactive publication can leave projection debt at its bounded
    // pass; this fixture asserts the settled view, not that wall clock.
    host.publish_projected_transcript(true);
    assert_eq!(host.config.session.tool_call_count, 1);
    let stored = host.config.store.load(&host.config.session.id).unwrap();
    assert_eq!(host.config.session.tool_call_count, stored.tool_call_count);
    assert!(events.try_iter().any(|event| match event {
        HostEvent::PresentationSnapshot(snapshot) =>
            snapshot.items.iter().map(|item| &item.row).any(
                |row| matches!(row, TranscriptRow::Tool(tool) if tool.name == "hosted_interpreter")
            ),
        HostEvent::TranscriptPatch(patch) => patch.upserts.iter().any(|item| matches!(&item.row, TranscriptRow::Tool(tool) if tool.name == "hosted_interpreter")),
        HostEvent::Row(TranscriptRow::Tool(tool)) => tool.name == "hosted_interpreter",
        _ => false,
    }));
    host.release().unwrap();
}

use super::*;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::{actual_ingress_backpressure, fresh_injected_attached_host};
use crate::sessions::SessionRecord;
use std::sync::{Arc, Mutex};

struct CapacityProbe(Arc<Mutex<crate::session_store::StorageAdmission>>);

impl crate::runtime::fault_store::StoreFaults for CapacityProbe {
    fn storage_admission(&self) -> Option<crate::session_store::StorageAdmission> {
        Some(self.0.lock().unwrap().clone())
    }

    fn before(
        &self,
        _: crate::runtime::fault_store::StoreCall,
        _: Option<&SessionRecord>,
    ) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn low_storage_keeps_the_prompt_queued_without_admission_and_rechecks_on_retry() {
    use crate::session_store::StorageAdmission;
    let root = tempfile::tempdir().unwrap();
    let plan = crate::runtime::recover_tests::script(serde_json::json!([
        {"output": {"text": "only response"}}
    ]));
    let (config, _) = crate::runtime::recover_tests::scripted_host_with(root.path(), plan);
    let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (event_tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = HostLoop::open(
        config,
        &action_rx,
        crate::runtime::Sender::over(event_tx),
        None,
    )
    .unwrap();
    let capacity = Arc::new(Mutex::new(StorageAdmission::Refused(
        "test volume: 0 MiB free".into(),
    )));
    host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        host.config.store.clone(),
        CapacityProbe(capacity.clone()),
    ));
    actions
        .send(crate::surface_protocol::UserAction::Prompt(
            "retained prompt".into(),
        ))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt");
    };
    let _ = events.try_iter().collect::<Vec<_>>();
    host.transient_resumes_used = 2;
    host.run_prompt(text.clone(), queued_delivery.clone(), false, None)
        .unwrap();
    assert_eq!(host.config.session.pending_queue.len(), 1);
    assert_eq!(
        host.config.session.queue_drain_state,
        SessionQueueDrainState::RestoredPaused
    );
    assert!(!host.config.session.queue_commands.has_pending());
    assert_eq!(host.transient_resumes_used, 2);
    assert!(!events.try_iter().any(|event| matches!(
        event,
        HostEvent::PromptStarted(_) | HostEvent::TurnLifecycle(_)
    )));
    *capacity.lock().unwrap() = StorageAdmission::Available;
    host.run_prompt(text, queued_delivery, user_echoed, None)
        .unwrap();
    assert!(events.try_iter().any(|event| matches!(event,
        HostEvent::TurnLifecycle(lifecycle) if lifecycle.phase == plakous_host_protocol::TurnPhase::Admitted)));
    host.release().unwrap();
}

struct SubmissionFailure {
    fail_prepare: bool,
    fail_discard: bool,
    fail_join: Option<Arc<std::sync::atomic::AtomicBool>>,
    join_gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
    hold_join: Option<Arc<std::sync::atomic::AtomicBool>>,
    reason: &'static str,
    attempted_revision: Mutex<Option<u64>>,
}

struct CapacityWriteFault(Arc<std::sync::atomic::AtomicBool>);

impl crate::runtime::fault_store::StoreFaults for CapacityWriteFault {
    fn refusal(
        &self,
        call: crate::runtime::fault_store::StoreCall,
    ) -> Option<crate::error::StoreRefusal> {
        (call == crate::runtime::fault_store::StoreCall::Save
            && self.0.load(std::sync::atomic::Ordering::SeqCst))
        .then_some(crate::error::StoreRefusal::StorageFull)
    }

    fn before(
        &self,
        call: crate::runtime::fault_store::StoreCall,
        _: Option<&SessionRecord>,
    ) -> Result<(), String> {
        if call == crate::runtime::fault_store::StoreCall::Save
            && self.0.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(std::io::Error::from_raw_os_error(28).to_string());
        }
        Ok(())
    }
}

#[test]
fn enospc_during_busy_follow_up_preserves_the_committed_queue_and_rejects_only_the_new_draft() {
    assert_busy_follow_up_storage_full(false);
}

#[test]
fn enospc_then_failed_root_never_merges_the_retained_queue_with_the_next_prompt() {
    assert_busy_follow_up_storage_full(true);
}

fn assert_busy_follow_up_storage_full(failed_root: bool) {
    use crate::surface_protocol::UserAction;
    use std::sync::atomic::{AtomicBool, Ordering};
    let root = tempfile::tempdir().unwrap();
    let gate = plakous_offline_crash_harness::gate::GateHandle::new();
    let first = if failed_root {
        serde_json::json!({"gate": gate.script(), "fault": {"at": "pre_stream", "kind": "protocol", "retryability": "Permanent", "message": "injected definite root failure"}})
    } else {
        serde_json::json!({"gate": gate.script(), "output": {"text": "root response"}})
    };
    let plan = crate::runtime::recover_tests::script(serde_json::json!([
        first, {"output": {"text": "next response"}}, {"output": {"text": "queued response"}}
    ]));
    let (mut config, _) = crate::runtime::recover_tests::scripted_host_with(root.path(), plan);
    let durable = config.store.clone();
    let id = config.session.id.clone();
    let armed = Arc::new(AtomicBool::new(false));
    config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        durable.clone(),
        CapacityWriteFault(armed.clone()),
    ));
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    actions
        .send(UserAction::Prompt("root prompt".into()))
        .unwrap();
    crate::runtime::recover_tests::wait_for(&events, &mut seen, "busy root", |event| {
        matches!(event,
        HostEvent::TurnLifecycle(lifecycle) if lifecycle.phase == plakous_host_protocol::TurnPhase::Admitted)
    });
    actions
        .send(UserAction::FollowUp("committed follow-up".into()))
        .unwrap();
    crate::runtime::recover_tests::wait_for(&events, &mut seen, "durable follow-up", |event| {
        matches!(event,
        HostEvent::QueueSnapshot { items, .. } if items.iter().any(|item| item.text == "committed follow-up"))
    });
    let before = durable.load(&id).unwrap();
    let queued = before
        .pending_queue
        .iter()
        .find(|item| item.text == "committed follow-up")
        .unwrap()
        .clone();
    armed.store(true, Ordering::SeqCst);
    actions
        .send(UserAction::FollowUp("refused follow-up".into()))
        .unwrap();
    crate::runtime::recover_tests::wait_for(&events, &mut seen, "capacity rejection", |event| {
        matches!(event,
        HostEvent::DraftRejected(UserAction::FollowUp(draft)) if draft.text == "refused follow-up")
    });
    let after = durable.load(&id).unwrap();
    assert!(after.pending_queue.contains(&queued));
    assert!(
        !after
            .pending_queue
            .iter()
            .any(|item| item.text == "refused follow-up")
    );
    armed.store(false, Ordering::SeqCst);
    gate.release();
    if failed_root {
        assert_failed_root_keeps_queue_distinct(
            &actions, &events, &mut seen, &*durable, &id, &queued,
        );
    }
    crate::runtime::recover_tests::wait_for(&events, &mut seen, "queued delivery", |event| {
        matches!(event,
        HostEvent::UserMessage { text, .. } if text == "committed follow-up")
    });
    crate::runtime::recover_tests::wait_for(&events, &mut seen, "queued terminal", |event| {
        matches!(event, HostEvent::SetBusy(false))
    });
    assert_busy_follow_up_delivered(&seen, failed_root);
    assert_eq!(
        durable.load(&id).unwrap().pending_queue,
        [] as [queue::QueueItem; 0]
    );
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
}

fn assert_busy_follow_up_delivered(seen: &[HostEvent<TestExtension>], failed_root: bool) {
    assert!(!seen.iter().any(|event| matches!(event,
        HostEvent::UserMessage { text, .. } if text.contains("refused follow-up"))));
    let delivered: Vec<_> = seen
        .iter()
        .filter_map(|event| match event {
            HostEvent::UserMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let expected = if failed_root {
        vec!["root prompt", "distinct next prompt", "committed follow-up"]
    } else {
        vec!["root prompt", "committed follow-up"]
    };
    assert_eq!(delivered, expected, "never merge or redeliver queue text");
    let last_snapshot = seen
        .iter()
        .rev()
        .find_map(|event| match event {
            HostEvent::QueueSnapshot { items, .. } => Some(items),
            _ => None,
        })
        .unwrap();
    assert!(
        last_snapshot.is_empty(),
        "the TUI must not show a consumed item as queued"
    );
}

fn assert_failed_root_keeps_queue_distinct(
    actions: &crate::surface_protocol::UserActionSender<TestExtension>,
    events: &crate::surface_protocol::HostEventReceiver<TestExtension>,
    seen: &mut Vec<HostEvent<TestExtension>>,
    durable: &dyn crate::session_store::SessionRecordStore,
    id: &str,
    queued: &queue::QueueItem,
) {
    use crate::surface_protocol::UserAction;
    crate::runtime::recover_tests::wait_for(
        events,
        seen,
        "failed root queue committed paused",
        |event| matches!(event, HostEvent::QueueSnapshot { items, drain } if *drain == plakous_host_protocol::QueueDrainState::RestoredPaused && items.len() == 1 && items[0].id == queued.id),
    );
    assert!(seen.iter().any(|event| matches!(event, HostEvent::TurnLifecycle(turn) if turn.phase == plakous_host_protocol::TurnPhase::Failed)));
    let retained = durable.load(id).unwrap();
    assert_eq!(retained.pending_queue, std::slice::from_ref(queued));
    assert_eq!(
        retained.queue_drain_state,
        SessionQueueDrainState::RestoredPaused
    );
    actions
        .send(UserAction::Prompt("distinct next prompt".into()))
        .unwrap();
    crate::runtime::recover_tests::wait_for(
        events,
        seen,
        "new distinct prompt",
        |event| matches!(event, HostEvent::UserMessage { text, .. } if text == "distinct next prompt"),
    );
    crate::runtime::recover_tests::wait_for(events, seen, "new prompt idle", |event| {
        matches!(event, HostEvent::SetBusy(false))
    });
    assert_eq!(
        durable.load(id).unwrap().pending_queue,
        std::slice::from_ref(queued)
    );
    actions.send(UserAction::ResumeQueueDrain).unwrap();
}

#[test]
fn busy_send_now_recovers_after_ack_projection_debt_without_waiting_for_idle() {
    use crate::surface_protocol::UserAction;
    use plakous_offline_crash_harness::gate::GateHandle;
    use std::sync::atomic::{AtomicBool, Ordering};

    let root = tempfile::tempdir().unwrap();
    let gate = GateHandle::new();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test_with(
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        "busy-steer-wait",
        "root",
        "owner",
        config.cwd.clone(),
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent(
            "root",
            &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap();
    let tree = Arc::new(Mutex::new(tree));
    let plan = crate::runtime::recover_tests::script(serde_json::json!([
        {"output": {"tool": {"name": "wait_agent", "arguments": {"agent_ids": [child.agent_id], "timeout_ms": 2_400_000}}}, "finish": "tool_calls"},
        {"stream": [{"gate": gate.script()}], "output": {"text": "busy response"}},
        {"output": {"text": "steered response"}}
    ]));
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        root.path().join("wait-steer-evidence.jsonl"),
    )
    .unwrap();
    let workspace = config.cwd.clone();
    let cursor = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let binding = move |_| {
        plakous_offline_crash_harness::scripted_topology_runtime_binding(
            &workspace,
            controller.clone(),
            plan.clone(),
            cursor.clone(),
            crate::agent_tree_tools::topology_tool_registrations(tree.clone(), "root").unwrap(),
        )
        .map_err(|error| error.to_string())
    };
    let armed = Arc::new(AtomicBool::new(false));
    let arm = armed.clone();
    let hold = Arc::new(AtomicBool::new(false));
    let hold_after_ack = hold.clone();
    let injection = crate::runtime::test_kit::scripted_mut(&mut config);
    injection.rebind_target = binding(crate::runtime::RuntimeRole::Primary)
        .unwrap()
        .target_binding();
    injection.runtime = Arc::new(binding);
    let inject_once = AtomicBool::new(true);
    injection.observe = Arc::new(move |workflow, boundary| {
        if workflow == HostWorkflow::RootDelivery
            && boundary == HostBoundary::RuntimeReturned
            && inject_once.swap(false, Ordering::SeqCst)
        {
            // Later auto-drained roots must not re-arm the recovered fault.
            arm.store(true, Ordering::SeqCst);
            hold_after_ack.store(true, Ordering::SeqCst);
        }
    });
    let durable = config.store.clone();
    let id = config.session.id.clone();
    let home = config.home.clone();
    let join_gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        durable.clone(),
        SubmissionFailure {
            fail_prepare: false,
            fail_discard: false,
            fail_join: Some(armed.clone()),
            join_gate: Some(join_gate.clone()),
            hold_join: Some(hold.clone()),
            reason: "injected after-ack projection timeout",
            attempted_revision: Mutex::new(None),
        },
    ));
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    let seen = std::cell::RefCell::new(Vec::new());
    let next = |predicate: &dyn Fn(&HostEvent<TestExtension>) -> bool| loop {
        let event = events
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap_or_else(|error| panic!("host observation: {error}; seen: {:?}", seen.borrow()));
        seen.borrow_mut().push(format!("{event:?}"));
        if predicate(&event) {
            break event;
        }
    };
    actions
        .send(UserAction::Prompt("start busy root".into()))
        .unwrap();
    next(&|event| {
        matches!(event, HostEvent::TurnLifecycle(lifecycle)
        if lifecycle.phase == plakous_host_protocol::TurnPhase::Admitted)
    });
    assert!(
        !armed.load(Ordering::SeqCst),
        "after-ack pass entered the unreleased projection gate"
    );
    assert!(
        crate::session_log::read_tail(&home, &id, 64)
            .unwrap()
            .iter()
            .any(|event| event.contains("projector.deferred")
                && event.contains("after-ack projector pass exceeded its bound")),
        "the unreleased gate must force the actual projection timeout"
    );
    *join_gate.0.lock().unwrap() = true;
    join_gate.1.notify_all();
    next(
        &|event| matches!(event, HostEvent::ToolLine { name, state, .. } if name == "wait_agent" && state == "running"),
    );

    // Ordinary queue writes and a direct steer still work with the debt latched.
    actions
        .send(UserAction::FollowUp("send this now".into()))
        .unwrap();
    let HostEvent::QueueSnapshot { items, .. } = next(&|event| {
        matches!(event,
        HostEvent::QueueSnapshot { items, .. } if items.iter().any(|item| item.text == "send this now"))
    }) else {
        unreachable!()
    };
    let item = items
        .into_iter()
        .find(|item| item.text == "send this now")
        .unwrap();
    actions
        .send(UserAction::Steer("direct steer is independent".into()))
        .unwrap();
    next(&|event| {
        matches!(event, HostEvent::DraftAccepted(UserAction::Steer(draft))
        if draft.text == "direct steer is independent")
    });
    next(&|event| {
        matches!(event, HostEvent::ToolLine { name, wait, .. }
        if name == "wait_agent" && wait.as_ref().and_then(|wait| wait.woke_by.as_deref()) == Some("steer"))
    });
    for index in 0..4 {
        let text = format!("unrelated queued follow-up {index}");
        actions
            .send(UserAction::FollowUp(text.clone().into()))
            .unwrap();
        next(&|event| {
            matches!(event, HostEvent::QueueSnapshot { items, .. }
            if items.iter().any(|item| item.text == text))
        });
    }
    actions
        .send(UserAction::EditQueued {
            id: item.id.clone(),
            revision: item.revision,
            text: "edited send-now".into(),
        })
        .unwrap();
    let HostEvent::QueueSnapshot { items, .. } = next(&|event| {
        matches!(event,
        HostEvent::QueueSnapshot { items, .. } if items.iter().any(|item| item.text == "edited send-now"))
    }) else {
        unreachable!()
    };
    let edited = items
        .into_iter()
        .find(|queued| queued.id == item.id)
        .unwrap();
    for _ in 0..2 {
        actions
            .send(UserAction::SendQueuedNow {
                id: edited.id.clone(),
                revision: edited.revision,
            })
            .unwrap();
        actions
            .send(UserAction::FollowUp("notice barrier".into()))
            .unwrap();
        next(&|event| {
            matches!(event, HostEvent::QueueSnapshot { items, .. }
            if items.iter().any(|item| item.text == "notice barrier"))
        });
        actions.send(UserAction::DropQueued).unwrap();
        next(&|event| {
            matches!(event, HostEvent::QueueSnapshot { items, .. }
            if !items.iter().any(|item| item.text == "notice barrier"))
        });
    }
    assert_eq!(
        seen.borrow()
            .iter()
            .filter(|event| event.starts_with("Row(")
                && event.contains("NoticeRow")
                && event.contains("queue projection is catching up"))
            .count(),
        1
    );
    hold.store(false, Ordering::SeqCst);
    // A busy projector tick must publish the caught-up queue without settling the turn.

    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let (fresh, _) = fresh_injected_attached_host(root.path());
    executor
        .block_on(super::super::super::projection::project_session(
            &fresh.state_roots.runtime_sessions().unwrap(),
            &durable,
            &durable.load(&id).unwrap(),
        ))
        .unwrap()
        .unwrap();
    next(&|event| {
        matches!(event, HostEvent::QueueSnapshot { items, .. }
        if !items.iter().any(|item| item.text == "start busy root"))
    });
    actions
        .send(UserAction::EditQueued {
            id: item.id.clone(),
            revision: item.revision,
            text: "stale edit".into(),
        })
        .unwrap();
    next(&|event| matches!(event, HostEvent::QueueEditRefused { id } if id == &item.id));
    actions
        .send(UserAction::SendQueuedNow {
            id: edited.id,
            revision: edited.revision,
        })
        .unwrap();
    let response = next(&|event| match event {
        HostEvent::Row(TranscriptRow::Steer { .. }) => true,
        HostEvent::Status { detail, .. } => detail.as_deref() == Some("steering..."),
        HostEvent::Row(TranscriptRow::Notice(notice)) => {
            notice.text == "queue changed; refreshed from host snapshot"
        }
        _ => false,
    });
    assert!(
        matches!(response, HostEvent::Status { detail, .. } if detail.as_deref() == Some("steering...")),
        "acceptance publishes chrome, not an unapplied transcript row"
    );
    gate.release();
    let update = next(&|event| {
        let items = match event {
            HostEvent::PresentationSnapshot(snapshot) => &snapshot.items,
            HostEvent::TranscriptPatch(patch) => &patch.upserts,
            _ => return false,
        };
        items.iter().any(|item| matches!(&item.row, TranscriptRow::Steer { text, .. } if text == "edited send-now"))
    });
    let items = match update {
        HostEvent::PresentationSnapshot(snapshot) => snapshot.items,
        HostEvent::TranscriptPatch(patch) => patch.upserts,
        _ => unreachable!("presentation update"),
    };
    let steers: Vec<_> = items
        .iter()
        .map(|item| &item.row)
        .filter(|row| {
            matches!(row,
        TranscriptRow::Steer { text, .. } if text == "edited send-now")
        })
        .collect();
    assert_eq!(steers.len(), 1);
    assert!(matches!(
        steers[0],
        TranscriptRow::Steer {
            sent_at_ms: None,
            ..
        }
    ));
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
}

impl crate::runtime::fault_store::StoreFaults for SubmissionFailure {
    fn before(
        &self,
        call: crate::runtime::fault_store::StoreCall,
        session: Option<&SessionRecord>,
    ) -> Result<(), String> {
        use crate::runtime::fault_store::StoreCall;
        match call {
            StoreCall::Save => {
                let session = session.expect("save hook receives the session");
                if !self.fail_prepare
                    && !self.fail_discard
                    && self.fail_join.is_none()
                    && session
                        .pending_queue
                        .iter()
                        .any(|item| session.queue_commands.is_consumed(item))
                {
                    *self.attempted_revision.lock().unwrap() = Some(session.store_revision);
                    return Err(self.reason.into());
                }
            }
            StoreCall::SaveSubmission => {
                let session = session.expect("save hook receives the session");
                if self.fail_prepare {
                    *self.attempted_revision.lock().unwrap() = Some(session.store_revision);
                    return Err(self.reason.into());
                }
            }
            StoreCall::DiscardSubmission => {
                if self.fail_discard {
                    return Err(self.reason.into());
                }
            }
            StoreCall::LoadIntents
                if self
                    .hold_join
                    .as_ref()
                    .is_some_and(|hold| hold.load(std::sync::atomic::Ordering::SeqCst)) =>
            {
                if let Some(armed) = &self.fail_join {
                    armed.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                if let Some(gate) = &self.join_gate {
                    let mut released = gate.0.lock().unwrap();
                    while !*released {
                        released = gate.1.wait(released).unwrap();
                    }
                }
                return Err(self.reason.into());
            }
            _ => {}
        }
        Ok(())
    }
}

#[test]
fn ingress_backpressure_keeps_previously_submitted_command_locked() {
    for retry in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &action_rx,
            crate::runtime::Sender::over(event_tx),
            None,
        )
        .unwrap();
        actions
            .send(crate::surface_protocol::UserAction::Prompt(
                "earlier dispatch".into(),
            ))
            .unwrap();
        let HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = host.next_prompt(None).unwrap()
        else {
            panic!("prompt")
        };
        let item = queued_delivery.as_ref().unwrap();
        let operation = format!("plak:deliver:{}:{}", item.id, item.revision);
        let command = format!("plak.queue.{}.{}", item.id, item.revision);
        let submission = sessions::projector::Submission {
            command_id: command.clone(),
            kind: sessions::projector::SubmissionKind::RootStart,
            queue_item_id: item.id.clone(),
            queue_item_revision: item.revision,
            target_turn_id: None,
            canonical_command: text.clone(),
            submitted_at_ms: item.submitted_at_ms,
        };
        host.config
            .store
            .save_submission(&mut host.config.session, &submission)
            .unwrap();
        if retry {
            host.config
                .store
                .save_submission(&mut host.config.session, &submission)
                .unwrap();
        }
        let intents = host
            .config
            .store
            .load_intents(&host.config.session.id)
            .unwrap()
            .submissions;
        crate::runtime::turn_drive::stub_ingress_backpressure(
            operation.clone(),
            actual_ingress_backpressure(),
        );
        host.run_prompt(text, queued_delivery.clone(), user_echoed, None)
            .unwrap();
        assert_eq!(
            host.config
                .store
                .load_intents(&host.config.session.id)
                .unwrap()
                .submissions,
            intents
        );
        assert!(host.pending_start.is_none());
        assert!(!host.recovery_blocked);
        assert!(queue::queue_item_is_delivery_locked(
            &host.config.session,
            &item.id
        ));
        let loaded = host.config.store.load(&host.config.session.id).unwrap();
        assert!(loaded.queue_commands.is_locked(&item.id));
        let conflicting = sessions::projector::Submission {
            canonical_command: "changed command".into(),
            ..submission
        };
        assert!(
            host.config
                .store
                .save_submission(&mut host.config.session, &conflicting)
                .is_err()
        );
        assert!(
            !event_rx
                .try_iter()
                .any(|event| matches!(event, HostEvent::DraftRejected(_)))
        );
        host.release().unwrap();
    }
}

#[test]
fn transient_refusal_leaves_only_the_queue_and_allows_a_second_prompt() {
    for kind in [
        plakous::agent::ControlErrorKind::QueueFull,
        plakous::agent::ControlErrorKind::Backpressure,
    ] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &action_rx,
            crate::runtime::Sender::over(event_tx),
            None,
        )
        .unwrap();
        actions
            .send(crate::surface_protocol::UserAction::Prompt("first".into()))
            .unwrap();
        let HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = host.next_prompt(None).unwrap()
        else {
            panic!("prompt")
        };
        let item = queued_delivery.as_ref().unwrap();
        let operation = format!("plak:deliver:{}:{}", item.id, item.revision);
        let first_item = item.clone();
        if kind == plakous::agent::ControlErrorKind::Backpressure {
            crate::runtime::turn_drive::stub_ingress_backpressure(
                operation,
                actual_ingress_backpressure(),
            );
        } else {
            crate::runtime::turn_drive::stub_start_refusal(
                operation,
                plakous::agent::CommandAckDisposition::Rejected,
                kind,
            );
        }
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .unwrap();
        assert!(!host.recovery_blocked);
        assert!(!host.config.session.queue_commands.has_pending());
        // Withdrawal releases the pending row, not its permanent content binding.
        let command = format!("plak.queue.{}.{}", first_item.id, first_item.revision);
        let conflicting = sessions::projector::Submission {
            command_id: command,
            kind: sessions::projector::SubmissionKind::RootStart,
            queue_item_id: first_item.id.clone(),
            queue_item_revision: first_item.revision,
            target_turn_id: None,
            canonical_command: "changed unadmitted command".into(),
            submitted_at_ms: first_item.submitted_at_ms,
        };
        assert!(
            host.config
                .store
                .save_submission(&mut host.config.session, &conflicting)
                .is_err()
        );
        assert!(host.pending_start.is_none());
        assert_eq!(
            host.config.session.queue_drain_state,
            SessionQueueDrainState::LiveAutoDrain
        );
        assert!(
            !event_rx
                .try_iter()
                .any(|event| matches!(event, HostEvent::DraftRejected(_)))
        );
        actions
            .send(crate::surface_protocol::UserAction::Prompt("second".into()))
            .unwrap();
        let HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = host.next_prompt(None).unwrap()
        else {
            panic!("second prompt")
        };
        assert_eq!(text, "second");
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .unwrap();
        assert!(!host.recovery_blocked);
        assert_eq!(host.config.session.root_turn_count, 1);
        assert!(!host.config.session.queue_commands.has_pending());
        // The remaining first prompt may now drain, but was never copied to a draft.
        let first = host.pending_start.take().unwrap();
        host.run_prompt(first.text.clone(), Some(first), false, None)
            .unwrap();
        assert_eq!(host.config.session.root_turn_count, 2);
        host.release().unwrap();
    }
}

#[test]
fn stopped_or_recovery_gated_actor_fences_instead_of_rejecting_a_draft() {
    for kind in [
        None,
        Some(plakous::agent::ControlErrorKind::Shutdown),
        Some(plakous::agent::ControlErrorKind::LedgerFull),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &action_rx,
            crate::runtime::Sender::over(event_tx),
            None,
        )
        .unwrap();
        actions
            .send(crate::surface_protocol::UserAction::Prompt(
                "kept for recovery".into(),
            ))
            .unwrap();
        let HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = host.next_prompt(None).unwrap()
        else {
            panic!("prompt")
        };
        if let Some(kind) = kind {
            let item = queued_delivery.as_ref().unwrap();
            crate::runtime::turn_drive::stub_start_refusal(
                format!("plak:deliver:{}:{}", item.id, item.revision),
                plakous::agent::CommandAckDisposition::Rejected,
                kind,
            );
        } else {
            host.executor
                .block_on(host.opened.agent().unwrap().shutdown())
                .unwrap()
                .unwrap();
        }
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .unwrap();
        assert!(host.recovery_blocked);
        assert!(host.config.session.queue_commands.has_pending());
        assert_eq!(host.config.session.root_turn_count, 0);
        assert!(
            !event_rx
                .try_iter()
                .any(|event| matches!(event, HostEvent::DraftRejected(_)))
        );
        host.release().unwrap();
    }
}

#[test]
fn duplicate_refusals_after_save_failure_never_consume_unrun_text() {
    use plakous::agent::{CommandAckDisposition, ControlErrorKind};
    for kind in [ControlErrorKind::InvalidInput, ControlErrorKind::WrongPhase] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &action_rx,
            crate::runtime::Sender::over(event_tx),
            None,
        )
        .unwrap();
        let durable_store = host.config.store.clone();
        host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
            durable_store.clone(),
            SubmissionFailure {
                fail_prepare: false,
                fail_discard: true,
                fail_join: None,
                join_gate: None,
                hold_join: None,
                reason: "injected rejection save failure",
                attempted_revision: Mutex::new(None),
            },
        ));
        actions
            .send(crate::surface_protocol::UserAction::Prompt(
                "never drop me".into(),
            ))
            .unwrap();
        let HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = host.next_prompt(None).unwrap()
        else {
            panic!("prompt");
        };
        let item = queued_delivery.as_ref().unwrap();
        let operation = format!("plak:deliver:{}:{}", item.id, item.revision);
        crate::runtime::turn_drive::stub_start_refusal(
            operation.clone(),
            CommandAckDisposition::Rejected,
            kind,
        );
        host.run_prompt(text.clone(), queued_delivery.clone(), user_echoed, None)
            .unwrap();
        assert!(host.recovery_blocked);
        assert!(host.config.session.queue_commands.has_pending());
        host.config.store = durable_store;
        host.recover_session(super::super::recover::RecoveryTrigger::Explicit)
            .unwrap();
        crate::runtime::turn_drive::stub_start_refusal(
            operation,
            CommandAckDisposition::Duplicate,
            kind,
        );
        host.run_prompt(text.clone(), queued_delivery.clone(), true, None)
            .unwrap();
        assert!(!host.recovery_blocked);
        assert_eq!(host.config.session.pending_queue.len(), 1);
        assert_eq!(host.config.session.root_turn_count, 0);
        assert_eq!(
            host.config.session.queue_drain_state,
            SessionQueueDrainState::RestoredPaused
        );
        assert!(!host.config.session.transcript.iter().any(
            |line| matches!(line, TranscriptRow::User { text, .. } if text == "never drop me")
        ));
        assert_eq!(
            event_rx
                .try_iter()
                .filter(|event| matches!(event, HostEvent::DraftRejected(_)))
                .count(),
            0
        );
        assert!(!host.config.session.queue_commands.has_pending());
        assert!(!host.config.session.queue_commands.has_pending());
        host.release().unwrap();
    }
}

#[test]
fn reclaimed_refusals_consume_once_and_continue_the_live_drain() {
    use plakous::agent::{CommandAckDisposition, ControlErrorKind};
    for disposition in [
        CommandAckDisposition::Rejected,
        CommandAckDisposition::Duplicate,
    ] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &action_rx,
            crate::runtime::Sender::over(event_tx),
            None,
        )
        .unwrap();
        actions
            .send(crate::surface_protocol::UserAction::Prompt(
                "already ran".into(),
            ))
            .unwrap();
        let HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = host.next_prompt(None).unwrap()
        else {
            panic!("prompt");
        };
        let item = queued_delivery.as_ref().unwrap();
        queue::enqueue(&mut host.config.session.pending_queue, "next item").unwrap();
        host.config.store.save(&mut host.config.session).unwrap();
        crate::runtime::turn_drive::stub_start_refusal(
            format!("plak:deliver:{}:{}", item.id, item.revision),
            disposition,
            ControlErrorKind::Reclaimed,
        );
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .unwrap();
        assert!(!host.recovery_blocked);
        assert_eq!(host.config.session.root_turn_count, 0);
        assert_eq!(host.config.session.pending_queue.len(), 1);
        assert_eq!(host.pending_start.as_ref().unwrap().text, "next item");
        assert_eq!(event_rx.try_iter().filter(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note == "queued prompt already ran; its result is no longer kept")).count(), 1);
        let next = host.pending_start.take().unwrap();
        host.run_prompt(next.text.clone(), Some(next), false, None)
            .unwrap();
        assert_eq!(
            host.config.session.pending_queue,
            [] as [queue::QueueItem; 0]
        );
        assert_eq!(host.config.session.root_turn_count, 1);
        assert!(!host.recovery_blocked);
        host.release().unwrap();
    }
}

#[test]
fn rejection_projection_save_failure_keeps_the_item_retryable() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = HostLoop::open(
        config,
        &action_rx,
        crate::runtime::Sender::over(event_tx),
        None,
    )
    .unwrap();
    let durable_store = host.config.store.clone();
    host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        durable_store.clone(),
        SubmissionFailure {
            fail_prepare: false,
            fail_discard: true,
            fail_join: None,
            join_gate: None,
            hold_join: None,
            reason: "injected rejected projection save failure",
            attempted_revision: Mutex::new(None),
        },
    ));
    let injector = rusqlite::Connection::open(root.path().join("home/runtime.sqlite3")).unwrap();
    injector.execute_batch("CREATE TRIGGER reject_admission BEFORE INSERT ON commit_receipts BEGIN SELECT RAISE(IGNORE); END;").unwrap();
    actions
        .send(crate::surface_protocol::UserAction::Prompt(
            "retry exact item".into(),
        ))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt");
    };
    host.run_prompt(text.clone(), queued_delivery.clone(), user_echoed, None)
        .unwrap();
    assert!(host.recovery_blocked);
    assert_eq!(host.config.session.pending_queue.len(), 1);
    assert!(host.config.session.queue_commands.has_pending());
    assert_eq!(host.config.session.root_turn_count, 0);
    assert_eq!(
        host.config.session.queue_drain_state,
        SessionQueueDrainState::RestoredPaused
    );
    assert_eq!(
        event_rx
            .try_iter()
            .filter(|event| matches!(event, HostEvent::DraftRejected(_)))
            .count(),
        0
    );
    injector
        .execute_batch("DROP TRIGGER reject_admission;")
        .unwrap();
    host.config.store = durable_store;
    host.recover_session(super::super::recover::RecoveryTrigger::Explicit)
        .unwrap();
    let submitted_at_ms = queued_delivery.as_ref().unwrap().submitted_at_ms;
    host.run_prompt(text, queued_delivery, true, None).unwrap();
    assert!(!host.recovery_blocked);
    assert_eq!(
        host.config.session.pending_queue,
        [] as [queue::QueueItem; 0]
    );
    assert_eq!(host.config.session.root_turn_count, 1);
    // The admitted row shows the operator's submit time, not the retry's.
    assert_eq!(
        host.config
            .session
            .transcript
            .iter()
            .filter(|line| matches!(
                line,
                TranscriptRow::User { text, sent_at_ms }
                    if text == "retry exact item" && *sent_at_ms == Some(submitted_at_ms)
            ))
            .count(),
        1
    );
    host.release().unwrap();
}

#[test]
fn lost_acknowledgement_keeps_fenced_recovery_and_never_restores_as_a_refusal() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = HostLoop::open(
        config,
        &action_rx,
        crate::runtime::Sender::over(event_tx),
        None,
    )
    .unwrap();
    actions
        .send(crate::surface_protocol::UserAction::Prompt(
            "uncertain prompt".into(),
        ))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt");
    };
    let item = queued_delivery.as_ref().unwrap();
    crate::runtime::turn_drive::stub_start_refusal(
        format!("plak:deliver:{}:{}", item.id, item.revision),
        plakous::agent::CommandAckDisposition::Rejected,
        plakous::agent::ControlErrorKind::OutcomeUnknown,
    );
    host.run_prompt(text, queued_delivery, user_echoed, None)
        .unwrap();
    assert!(host.recovery_blocked);
    assert_eq!(host.config.session.pending_queue.len(), 1);
    assert_eq!(host.config.session.root_turn_count, 0);
    assert!(host.config.session.queue_commands.has_pending());
    assert_eq!(
        host.config.session.queue_drain_state,
        SessionQueueDrainState::RestoredPaused
    );
    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(!events.iter().any(|event| matches!(
        event,
        HostEvent::DraftRejected(_) | HostEvent::DraftRestored(_)
    )));
    assert!(events.iter().any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note == super::super::publication::RECOVERY_PENDING_NOTE)));
    host.release().unwrap();
}

/// Spec 02 §5.4: a terminal turn error has one owner. The projection
/// stores exactly the row the operator is shown (the humanized text on
/// the definite-failure path) and the notice drain adds no second copy.
/// A replayed delivery's batch is bound to its command, so an over-window
/// replay parks rather than being withheld after acknowledging its mail;
/// a fresh batch is dropped instead.
#[test]
fn an_over_window_replay_with_bound_mail_parks() {
    assert_eq!(
        mail_after_scheduled_send(true, false, true),
        ScheduledMail::Park
    );
    assert_eq!(
        mail_after_scheduled_send(false, false, true),
        ScheduledMail::Keep
    );
    assert_eq!(
        mail_after_scheduled_send(true, true, false),
        ScheduledMail::Drop
    );
    assert_eq!(
        mail_after_scheduled_send(false, true, false),
        ScheduledMail::Drop
    );
    assert_eq!(
        mail_after_scheduled_send(true, false, false),
        ScheduledMail::Keep
    );
}

#[test]
fn unverified_context_capacity_source_publishes_protected_recovery_without_compaction() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (_, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = HostLoop::open(
        config,
        &action_rx,
        crate::runtime::Sender::over(event_tx),
        None,
    )
    .unwrap();
    let source = plakous::model::CheckpointId::try_from(uuid::Uuid::new_v4()).unwrap();
    assert_ne!(source, host.opened.locator().checkpoint_id());
    host.publish_terminal(
        Ok(TurnHostOutcome::ContextCapacity {
            source_checkpoint: source,
            recovery: plakous_host_protocol::ContextCapacityRecovery::Compactable,
            error: "injected overflow".into(),
        }),
        Instant::now(),
        super::super::publication::TerminalErrorPresentation::Events,
    );
    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(events.iter().any(|event| matches!(
        event,
        HostEvent::ContextCapacityRecovery(Some(
            plakous_host_protocol::ContextCapacityRecovery::Protected
        ))
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        HostEvent::ContextCapacityRecovery(Some(
            plakous_host_protocol::ContextCapacityRecovery::Compactable
        ))
    )));
    host.release().unwrap();
}

#[test]
fn context_capacity_restores_the_admitted_draft_without_a_rejection_receipt() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let workspace = config.cwd.clone();
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        root.path().join("capacity-events.jsonl"),
    )
    .unwrap();
    let binding = plakous_offline_crash_harness::projected_terminal_failure_runtime_binding(
        &workspace,
        controller.clone(),
        plakous::model::ModelErrorKind::ContextLimit,
    )
    .unwrap();
    config.runtime_factory = Arc::new(crate::runtime::ScriptedRuntimeFactory {
        rebind_target: binding.target_binding(),
        runtime: Arc::new(move |_| {
            plakous_offline_crash_harness::projected_terminal_failure_runtime_binding(
                &workspace,
                controller.clone(),
                plakous::model::ModelErrorKind::ContextLimit,
            )
            .map_err(|error| error.to_string())
        }),
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: Mutex::new(Vec::new()),
    });
    let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = HostLoop::open(
        config,
        &action_rx,
        crate::runtime::Sender::over(event_tx),
        None,
    )
    .unwrap();
    actions
        .send(crate::surface_protocol::UserAction::Prompt(
            "withheld prompt".into(),
        ))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt");
    };
    host.run_prompt(text, queued_delivery, user_echoed, None)
        .unwrap();
    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .filter(
                |event| matches!(event, HostEvent::DraftRestored(text) if text == "withheld prompt")
            )
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::DraftRejected(_)))
    );
    assert!(!host.recovery_blocked);
    host.release().unwrap();
}

#[test]
fn logical_pressure_root_refusal_keeps_remedies_and_non_admission_copy() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = HostLoop::open(
        config,
        &action_rx,
        crate::runtime::Sender::over(event_tx),
        None,
    )
    .unwrap();
    let injector = rusqlite::Connection::open(root.path().join("home/runtime.sqlite3")).unwrap();
    let checkpoints: i64 = injector
        .query_row("SELECT checkpoints FROM store_meta WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    injector
        .execute(
            "UPDATE store_meta SET checkpoints=?1 WHERE id=1",
            [524_288 * 7 / 8],
        )
        .unwrap();
    actions
        .send(crate::surface_protocol::UserAction::Prompt(
            "kept under logical pressure".into(),
        ))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt");
    };
    queue::enqueue(
        &mut host.config.session.pending_queue,
        "second under pressure",
    )
    .unwrap();
    host.config.store.save(&mut host.config.session).unwrap();
    host.run_prompt(text, queued_delivery, user_echoed, None)
        .unwrap();
    assert!(!host.recovery_blocked);
    assert!(host.pending_start.is_none());
    assert_eq!(host.config.session.pending_queue.len(), 2);
    assert!(!host.config.session.queue_commands.has_pending());
    assert_eq!(
        host.config
            .store
            .load_intents(&host.config.session.id)
            .unwrap()
            .pause_causes
            .len(),
        1
    );
    assert_eq!(host.config.session.root_turn_count, 0);
    assert_eq!(
        host.config.session.queue_drain_state,
        SessionQueueDrainState::RestoredPaused
    );
    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::DraftRejected(_)))
    );
    assert!(events.iter().any(|event| matches!(event, HostEvent::QueueSnapshot { items, .. } if items.len() == 2 && !items[0].rejected && !items[1].rejected)));
    assert!(events.iter().any(|event| matches!(event, HostEvent::Status { detail: Some(detail), .. } if detail == "prompt not admitted")));
    assert!(events.iter().any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: message, .. })) if message.starts_with("session store is full (checkpoints ") && message.contains("plak session gc --all") && message.contains("plak session delete <id>") && message.contains("prompt was not admitted"))));
    injector
        .execute(
            "UPDATE store_meta SET checkpoints=?1 WHERE id=1",
            [checkpoints],
        )
        .unwrap();
    host.release().unwrap();
}

#[test]
fn rejected_root_fact_binds_only_with_its_projector_page() {
    for (shutdown, fail_projection) in [(false, false), (true, false), (false, true)] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &rx, crate::runtime::Sender::over(tx), None).unwrap();
        let store = host.config.store.clone();
        let id = host.config.session.id.clone();
        let item = queue::enqueue(&mut host.config.session.pending_queue, "queued item").unwrap();
        store.save(&mut host.config.session).unwrap();
        let source = host.opened.locator();
        host.opened.close_admission();
        let command_id =
            plakous::agent::CommandId::new(format!("plak.queue.{}.{}", item.id, item.revision))
                .unwrap();
        let receipt =
            host.executor
                .block_on(host.opened.agent().unwrap().command(
                    plakous::agent::TurnCommand::Start {
                        command_id,
                        input: item.text.clone().into(),
                    },
                ))
                .unwrap()
                .unwrap();
        assert_eq!(
            receipt.acknowledgement().rejection().unwrap().kind(),
            plakous::agent::ControlErrorKind::Shutdown
        );
        if !shutdown {
            // Product text validation cannot reach InvalidInput today; exercise its projection over a real rejection checkpoint.
            crate::runtime::turn_drive::stub_start_refusal(
                format!("plak:deliver:{}:{}", item.id, item.revision),
                plakous::agent::CommandAckDisposition::Rejected,
                plakous::agent::ControlErrorKind::InvalidInput,
            );
        }
        if fail_projection {
            host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
                store.clone(),
                SubmissionFailure {
                    fail_prepare: false,
                    fail_discard: true,
                    fail_join: None,
                    join_gate: None,
                    hold_join: None,
                    reason: "injected rejected projection failure",
                    attempted_revision: Mutex::new(None),
                },
            ));
        }
        host.run_prompt(item.text.clone(), Some(item), true, None)
            .unwrap();
        let head = host.opened.locator();
        assert_ne!(source, head, "rejection receipt must have committed");
        assert!(
            host.config
                .session
                .queue_commands
                .is_rejected(&host.config.session.pending_queue[0])
        );
        let projected = store.load_views(&id).unwrap().unwrap();
        assert!(projected.state.deliveries.contains_key(&format!(
            "plak.queue.{}.{}",
            host.config.session.pending_queue[0].id, host.config.session.pending_queue[0].revision
        )));
        let notices = events
            .try_iter()
            .filter(|event| matches!(event, HostEvent::Row(_)))
            .count();
        assert!(notices > 0);
        host.config.store = store.clone();
        host.release().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store = store.clone();
        config.session = store.load(&id).unwrap();
        let (tx, reopened_events) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let reopened = HostLoop::open(config, &rx, crate::runtime::Sender::over(tx), None).unwrap();
        assert_eq!(
            reopened.config.session.plakous_locator,
            Some(reopened.opened.locator())
        );
        assert!(!reopened_events.try_iter().any(|event| matches!(event, HostEvent::Row(row) if row.text().is_some_and(|text| text.contains("prompt text must") || text.contains("agent is shutting down")))));
        reopened.release().unwrap();
    }
}

#[test]
fn definite_admission_rejection_keeps_text_queue_and_host_available() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let database = root.path().join("home/runtime.sqlite3");
    let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = HostLoop::open(
        config,
        &action_rx,
        crate::runtime::Sender::over(event_tx),
        None,
    )
    .unwrap();
    let injector = rusqlite::Connection::open(database).unwrap();
    injector
        .execute_batch(
            "CREATE TRIGGER reject_admission BEFORE INSERT ON commit_receipts
            BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
    let _ = event_rx.try_iter().collect::<Vec<_>>();
    actions
        .send(crate::surface_protocol::UserAction::Prompt(
            "keep my text".into(),
        ))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt must enter the product queue");
    };
    let head = host.opened.locator();
    host.run_prompt(text, queued_delivery, user_echoed, None)
        .unwrap();
    assert!(!host.recovery_blocked);
    assert_eq!(host.opened.locator(), head);
    assert_eq!(
        host.config.session.queue_drain_state,
        SessionQueueDrainState::RestoredPaused
    );
    assert_eq!(host.config.session.pending_queue.len(), 1);
    assert_eq!(host.config.session.pending_queue[0].text, "keep my text");
    assert!(!host.config.session.queue_commands.has_pending());
    assert_eq!(
        host.config
            .store
            .load_intents(&host.config.session.id)
            .unwrap()
            .submissions,
        [] as [sessions::projector::Submission; 0]
    );
    let saved = host.config.store.load(&host.config.session.id).unwrap();
    assert_eq!(saved.pending_queue, host.config.session.pending_queue);
    assert!(
        !saved
            .transcript
            .iter()
            .any(|line| matches!(line, TranscriptRow::User { text, .. } if text == "keep my text"))
    );
    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(events.iter().any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: message, .. })) if message.contains("prompt was not admitted"))));
    assert!(!events.iter().any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: message, .. })) if message == super::super::publication::RECOVERY_PENDING_NOTE)));
    injector
        .execute_batch("DROP TRIGGER reject_admission;")
        .unwrap();
    actions
        .send(crate::surface_protocol::UserAction::Prompt(
            "new prompt".into(),
        ))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("host must accept a new prompt after definite rejection");
    };
    assert!(
        host.config
            .session
            .pending_queue
            .iter()
            .any(|item| item.text == "new prompt")
    );
    assert!(
        event_rx
            .try_iter()
            .any(|event| matches!(event, HostEvent::PromptAdmitted(_)))
    );
    host.run_prompt(text, queued_delivery, user_echoed, None)
        .unwrap();
    assert!(!host.recovery_blocked);
    assert!(
        host.pending_start.is_none(),
        "a rejected delivery is not automatically retried"
    );
    assert_eq!(host.config.session.pending_queue.len(), 1);
    assert_eq!(host.config.session.pending_queue[0].text, "keep my text");
    host.release().unwrap();
}

#[test]
fn root_delivery_prepare_and_ack_failures_log_actual_revision_without_inventing_queue() {
    for fail_prepare in [true, false] {
        for (reason, class) in [
            ("product session changed concurrently", "concurrent_change"),
            ("product store is busy", "store_busy"),
            ("private backend path /secret/token", "store_failure"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (config, _) = fresh_injected_attached_host(root.path());
            let durable_store = Arc::clone(&config.store);
            let (actions, action_rx) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
            let (event_tx, event_rx) =
                crate::surface_protocol::host_event_channel::<TestExtension>();
            let mut host = HostLoop::open(
                config,
                &action_rx,
                crate::runtime::Sender::over(event_tx),
                None,
            )
            .unwrap();
            let failing_store = Arc::new(crate::runtime::fault_store::FaultStore::new(
                Arc::clone(&durable_store),
                SubmissionFailure {
                    fail_prepare,
                    fail_discard: false,
                    fail_join: None,
                    join_gate: None,
                    hold_join: None,
                    reason,
                    attempted_revision: Mutex::new(None),
                },
            ));
            host.config.store = failing_store.clone();
            let _ = event_rx.try_iter().collect::<Vec<_>>();
            actions
                .send(crate::surface_protocol::UserAction::Prompt(
                    "private queued prompt".into(),
                ))
                .unwrap();
            let HostStep::Prompt {
                text,
                queued_delivery,
                user_echoed,
            } = host.next_prompt(None).unwrap()
            else {
                panic!("queue admission must succeed before the injected delivery failure");
            };
            let admitted = durable_store.load(&host.config.session.id).unwrap();
            assert_eq!(admitted.pending_queue.len(), 1);
            assert!(!admitted.queue_commands.has_pending());
            assert!(
                event_rx
                    .try_iter()
                    .any(|event| matches!(event, HostEvent::PromptAdmitted(_)))
            );
            assert!(matches!(
                host.run_prompt(text, queued_delivery, user_echoed, None)
                    .unwrap(),
                HostStep::Continue
            ));
            let attempted_revision = failing_store
                .faults
                .attempted_revision
                .lock()
                .unwrap()
                .unwrap();
            assert_eq!(
                attempted_revision,
                admitted.store_revision + u64::from(!fail_prepare)
            );
            let records = crate::session_log::read_tail_for_kind(
                &host.config.home,
                &admitted.id,
                10,
                Some("queue.persist_failed"),
            )
            .unwrap();
            assert_eq!(records.len(), 1);
            let raw = &records[0];
            let event: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(event["action"], "root_delivery");
            assert_eq!(event["failure_class"], class);
            assert_eq!(event["attempted_store_revision"], attempted_revision);
            assert_eq!(event.as_object().unwrap().len(), 6);
            assert!(!raw.contains("private"));
            assert!(!raw.contains("/secret/token"));
            let stored = durable_store.load(&admitted.id).unwrap();
            if fail_prepare {
                assert_eq!(stored.pending_queue, admitted.pending_queue);
                assert!(!stored.queue_commands.has_pending());
            } else {
                assert_eq!(stored.pending_queue, [] as [queue::QueueItem; 0]);
                assert!(
                    stored
                        .queue_commands
                        .is_consumed(&admitted.pending_queue[0])
                );
                assert!(!stored.queue_commands.has_pending());
                assert_eq!(
                    durable_store
                        .load_intents(&admitted.id)
                        .unwrap()
                        .submissions
                        .len(),
                    1
                );
            }
            host.release().unwrap();
        }
    }
}
