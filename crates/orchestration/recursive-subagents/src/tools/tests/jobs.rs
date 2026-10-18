//! Process jobs and the task/jobs presentations.

use super::*;
use crate::tools::jobs::{
    JobsStopRequest, JobsWaitPlan, JobsWaitRequest, TaskKillRequest, TaskOutputRequest,
    task_process_number, unlisted_job_admitted,
};

/// The await engine's process targets: a named own process wakes the
/// await when it ends and is returned as a snapshot row; a finished one
/// returns at once; an id outside the caller's session is refused.
#[tokio::test]
async fn an_await_naming_an_own_process_returns_its_row_when_it_ends() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("process-await", root.path());
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::new(FailingStore));
    let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
    let waiter = wait_tool(driver.clone());
    let named = |id: &str| AwaitRequest {
        agents: AwaitAgents::Unwatched,
        processes: vec![id.to_owned()],
        timeout_ms: 300_000,
    };
    let refused = waiter
        .await_operation("await-p9", &named("p9"), &json!({}), None)
        .await
        .unwrap();
    assert_eq!(outcome_json(&refused)["code"], "grant_denied");
    assert!(
        outcome_json(&refused)["message"]
            .as_str()
            .unwrap()
            .contains("p9 is not in this session")
    );
    let request = named("p1");
    let presented = json!({});
    let mut pending = Box::pin(waiter.await_operation("await-p1", &request, &presented, None));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    jobs.stop_jobs(
        plakous::tools::JobStopReason::Model,
        std::time::Duration::from_millis(250),
    )
    .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap();
    let output = outcome_json(&outcome);
    assert_eq!(output["woke_by"], "process");
    assert_eq!(output["agents"], json!([]));
    assert_eq!(output["jobs"][0]["id"], "p1");
    assert_eq!(output["jobs"][0]["status"], "stopped");
    assert!(output["jobs"][0]["output"].is_string());
    assert!(driver.tree.lock().unwrap().operations().is_empty());
    let again = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        waiter.await_operation("await-p1-again", &request, &json!({}), None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome_json(&again)["jobs"][0]["status"], "stopped");
}

#[test]
fn an_own_job_no_longer_listed_is_already_reported_not_a_refusal() {
    let ended = vec!["p2".to_owned()];
    assert!(unlisted_job_admitted("p2", &ended).is_ok());
    let refused = unlisted_job_admitted("p9", &ended).unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::GrantDenied);
    assert_eq!(refused.message, "process p9 is not in this session");
    let rows = await_job_rows(None, &["p2".to_owned()]);
    assert_eq!(rows[0].id, "p2");
    assert_eq!(rows[0].status, "already_reported");
    assert!(rows[0].output.is_none());
}

#[tokio::test]
async fn idle_submit_with_live_job_refuses_without_leaf_only_unavailable_remedies() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, store);
    let (jobs, _opened) = install_running_job(&driver, &child.agent_id, root.path()).await;
    let outcome = invoke_granted_topology_tool_on(
        driver.clone(),
        child.agent_id,
        "submit_result",
        &json!({"turn_token": 1, "output_json": "{\"ok\":true}"}),
        "submit-live-job".into(),
        TopologyHostPorts::default(),
    )
    .await
    .unwrap();
    let payload = outcome_json(&outcome);
    assert_eq!(payload["code"], "live_work");
    let message = payload["message"].as_str().unwrap();
    assert!(message.contains("p1"));
    for unavailable in ["wait_agent", "close_agent", "interrupt_agent"] {
        assert!(!message.contains(unavailable));
    }
    jobs.stop_jobs(
        plakous::tools::JobStopReason::Model,
        std::time::Duration::from_millis(250),
    )
    .unwrap();
}

#[tokio::test]
async fn close_stops_child_jobs_but_interrupt_preserves_them() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, store);
    let (jobs, _opened) = install_running_job(&driver, &child.agent_id, root.path()).await;
    let interrupted = interrupt_tool(driver.clone(), None)
        .interrupt_with("interrupt-job".into(), &json!({"agent_id": child.agent_id}))
        .await
        .unwrap();
    assert_eq!(interrupted.kind(), ToolOutcomeKind::Success);
    assert_eq!(jobs.jobs()[0].status, plakous::tools::JobStatus::Running);
    let closed = close_tool(driver, None)
        .close_with("close-job".into(), &json!({"agent_id": child.agent_id}))
        .await
        .unwrap();
    assert_eq!(closed.kind(), ToolOutcomeKind::Success);
    assert_eq!(
        jobs.jobs()[0].status,
        plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::AttemptEnded)
    );
}

struct ExpiredCloseCanceller(TreeDriver);

impl LiveTurnCanceller for ExpiredCloseCanceller {
    fn cancel_agents(
        &self,
        ids: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<LiveCancelOutcome, LiveCancelFailure>> + Send + '_>>
    {
        Box::pin(async move {
            self.0
                .mutate_and_settle(|tree| {
                    tree.record_host_retirement(&ids[0], "owner deadline".into())
                        .and_then(|value| tree.fold_due(&ids[0]).map(|_| value))
                        .unwrap();
                    self.0.save(tree).unwrap();
                })
                .unwrap();
            Err(LiveCancelFailure {
                message: "joiner deadline; owner retained".into(),
                failed_ids: Vec::new(),
                joiner_expired_ids: ids,
                owner_failures: Vec::new(),
                cancelled_ids: Vec::new(),
                not_live_ids: Vec::new(),
            })
        })
    }
}

struct RecoverDuringJobStop {
    driver: TreeDriver,
    child: String,
}

impl plakous::tools::JobHost for RecoverDuringJobStop {
    fn service(
        &self,
        _: plakous::model::SessionId,
        _: plakous::model::BranchId,
    ) -> Option<plakous::tools::JoinedProcessSessions> {
        self.driver
            .mutate_and_settle(|tree| {
                let attempt = tree.attempt(&self.child).unwrap().attempt_id.clone();
                tree.release_turn_owner(&self.child);
                tree.set_active_turn(&self.child, false).unwrap();
                tree.recover_host_decision(
                    &self.child,
                    &attempt,
                    crate::tree::RecoveryResolution::RecoveredWithoutTurn {
                        message: "late release returned during job stop".into(),
                    },
                )
                .unwrap();
                self.driver.save(tree).unwrap();
            })
            .unwrap();
        None
    }

    fn attach(
        &self,
        _: plakous::tools::JobOwner,
        fresh: plakous::tools::JoinedProcessSessions,
    ) -> Result<plakous::tools::JoinedProcessSessions, plakous::tools::ExecutionBackendError> {
        Ok(fresh)
    }
}

#[tokio::test]
async fn close_joiner_expiry_does_not_refault_recovery_during_job_stop() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    let child = tree
        .spawn_agent("root", &spawn_request(), None)
        .unwrap()
        .agent_id;
    tree.set_active_turn(&child, true).unwrap();
    let driver = driver_on(tree, store);
    let (_jobs, _opened) = install_running_job(&driver, &child, root.path()).await;
    driver.bind_job_host(Some(Arc::new(RecoverDuringJobStop {
        driver: driver.clone(),
        child: child.clone(),
    })));
    let outcome = close_tool(
        driver.clone(),
        Some(Arc::new(ExpiredCloseCanceller(driver.clone()))),
    )
    .close_with("close-expired-joiner".into(), &json!({"agent_id": child}))
    .await
    .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    driver
        .read(|tree| {
            assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
            assert_eq!(
                tree.attempt(&child).unwrap().status,
                crate::protocol::AttemptStatus::Closed
            );
            assert_eq!(
                tree.recovery_need(&child).unwrap(),
                crate::tree::RecoveryNeed::NotRequired
            );
            assert_eq!(
                tree.pending_result_mail("root"),
                [] as [crate::tree::MailRecord; 0]
            );
        })
        .unwrap();
    driver.bind_job_host(None);
}

#[tokio::test]
async fn settling_unrelated_effects_does_not_stop_historical_attempt_jobs() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, store);
    let (jobs, _opened) = install_running_job(&driver, &child.agent_id, root.path()).await;
    driver
        .mutate_and_settle(|tree| tree.record_turn_completed(&child.agent_id))
        .unwrap()
        .0
        .unwrap();
    assert_eq!(jobs.jobs()[0].status, plakous::tools::JobStatus::Running);
    jobs.stop_jobs(
        plakous::tools::JobStopReason::Model,
        std::time::Duration::from_millis(250),
    )
    .unwrap();
}

struct JobCheckingCanceller(plakous::tools::JoinedProcessSessions);

struct LateJobCanceller {
    driver: TreeDriver,
    agent: String,
    path: std::path::PathBuf,
    job: Mutex<
        Option<(
            plakous::tools::JoinedProcessSessions,
            plakous::sessions::OpenedSession,
        )>,
    >,
}

impl LiveTurnCanceller for LateJobCanceller {
    fn cancel_agents(
        &self,
        agent_ids: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<LiveCancelOutcome, LiveCancelFailure>> + Send + '_>>
    {
        Box::pin(async move {
            assert_eq!(
                self.driver
                    .read(|tree| tree.node(&self.agent).unwrap().status)
                    .unwrap(),
                NodeStatus::Closed
            );
            let job = install_running_job(&self.driver, &self.agent, &self.path).await;
            *self.job.lock().unwrap() = Some(job);
            Ok(LiveCancelOutcome {
                cancelled_ids: agent_ids,
                not_live_ids: Vec::new(),
            })
        })
    }
}

#[tokio::test]
async fn a_job_promoted_between_closure_and_cancel_is_stopped_not_parked() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, store);
    let canceller = Arc::new(LateJobCanceller {
        driver: driver.clone(),
        agent: child.agent_id.clone(),
        path: root.path().to_owned(),
        job: Mutex::new(None),
    });
    let outcome = close_tool(driver, Some(canceller.clone()))
        .close_with(
            "close-late-job".into(),
            &json!({"agent_id": child.agent_id}),
        )
        .await
        .unwrap();
    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    assert_eq!(
        canceller.job.lock().unwrap().as_ref().unwrap().0.jobs()[0].status,
        plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::AttemptEnded)
    );
}

impl LiveTurnCanceller for JobCheckingCanceller {
    fn cancel_agents(
        &self,
        agent_ids: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<LiveCancelOutcome, LiveCancelFailure>> + Send + '_>>
    {
        Box::pin(async move {
            assert_eq!(
                self.0.jobs()[0].status,
                plakous::tools::JobStatus::Running,
                "cancel live turns before stopping jobs"
            );
            Ok(LiveCancelOutcome {
                cancelled_ids: agent_ids,
                not_live_ids: Vec::new(),
            })
        })
    }
}

#[tokio::test]
async fn close_cancels_live_turn_before_stopping_its_jobs() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    let child = tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, store);
    let (jobs, _opened) = install_running_job(&driver, &child.agent_id, root.path()).await;
    let closed = close_tool(driver, Some(Arc::new(JobCheckingCanceller(jobs.clone()))))
        .close_with("close-order".into(), &json!({"agent_id": child.agent_id}))
        .await
        .unwrap();
    assert_eq!(closed.kind(), ToolOutcomeKind::Success);
    assert_eq!(
        jobs.jobs()[0].status,
        plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::AttemptEnded)
    );
}

fn task_output(task_ids: &[&str], timeout_ms: Option<u64>) -> TaskOutputRequest {
    TaskOutputRequest {
        task_ids: task_ids.iter().map(|id| (*id).to_owned()).collect(),
        timeout_ms,
    }
}

/// The await receipt inside the topology envelope a Grok call naming a
/// child returns.
fn enveloped(outcome: &ToolOutcome) -> Value {
    let outer = outcome_json(outcome);
    serde_json::from_str(outer["result"].as_str().expect("topology envelope")).unwrap()
}

#[test]
fn task_ids_split_into_process_numbers_and_agent_ids() {
    for (id, number) in [
        ("p3", Some(3)),
        ("3", Some(3)),
        ("c3", None),
        ("p0", None),
        ("0", None),
        ("p", None),
        ("p-1", None),
    ] {
        assert_eq!(task_process_number(id), number, "{id}");
        assert_eq!(
            crate::protocol::is_process_task_id(id),
            number.is_some(),
            "{id}"
        );
    }
}

/// Under the Grok vocabulary the granted await and interrupt slots are
/// presented as Grok's output and kill tools instead of `wait_agent` and
/// `interrupt_agent`; the grant itself, and a leaf's lack of both, is
/// unchanged.
#[test]
fn grok_vocabulary_presents_the_await_and_interrupt_under_grok_names() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("grok-names", root.path());
    let spawned = tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let tree = Arc::new(Mutex::new(tree));
    let registrations = |caller: &str, vocabulary| {
        topology_tool_registrations_in(Arc::clone(&tree), caller, vocabulary).unwrap()
    };
    let names = |registrations: &[ToolRegistration]| {
        registrations
            .iter()
            .map(|registration| registration.name().as_str().to_owned())
            .collect::<Vec<_>>()
    };
    let plak = names(&registrations("root", TopologyVocabulary::Plak));
    let grok_registrations = registrations("root", TopologyVocabulary::TaskTools);
    let grok = names(&grok_registrations);
    assert!(plak.iter().any(|name| name == "wait_agent"));
    assert!(plak.iter().any(|name| name == "interrupt_agent"));
    assert!(!plak.iter().any(|name| name.contains("command_or_subagent")));
    for name in [
        "get_command_or_subagent_output",
        "kill_command_or_subagent",
        "close_agent",
    ] {
        assert!(grok.iter().any(|presented| presented == name), "{name}");
    }
    for name in ["wait_agent", "interrupt_agent"] {
        assert!(!grok.iter().any(|presented| presented == name), "{name}");
    }
    assert_eq!(plak.len(), grok.len());
    let spawn = grok_registrations
        .iter()
        .find(|registration| registration.name().as_str() == "spawn_agent")
        .unwrap();
    assert!(
        spawn
            .definition()
            .description()
            .contains("get_command_or_subagent_output returns them")
    );
    assert!(!spawn.definition().description().contains("wait_agent"));
    let leaf = names(&registrations(
        &spawned.agent_id,
        TopologyVocabulary::TaskTools,
    ));
    assert!(
        !leaf.iter().any(|name| name.contains("command_or_subagent")
            || name.ends_with("_agent") && name != "list_agents" && name != "send_agent_message"),
        "{leaf:?}"
    );
}

/// Grok's output tool naming a child is the await: an omitted timeout is
/// a snapshot that saves nothing, a settled child returns its row and the
/// mail it consumed under the one await receipt kind, and the same call
/// replays the receipt.
#[tokio::test]
async fn grok_output_over_a_child_is_the_await_with_its_receipt_and_replay() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tool = task_tool(driver.clone(), TopologyKind::TaskOutput);
    let snapshot = tool
        .task_output_with(
            "grok-snapshot",
            &task_output(&["c1"], None),
            None,
            &ToolCallerLineage::Direct,
        )
        .await
        .unwrap();
    let inner = enveloped(&snapshot);
    assert_eq!(inner["timed_out"], true);
    assert_eq!(inner["woke_by"], "timeout");
    assert_eq!(inner["agents"][0]["agent_id"], "c1");
    assert!(driver.tree.lock().unwrap().operations().is_empty());
    {
        let mut tree = driver.tree.lock().unwrap();
        tree.send_agent_message(
            "c1",
            &SendAgentMessageRequest {
                agent_id: "root".into(),
                message: "halfway there".into(),
                purpose: None,
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed("c1").unwrap();
    }
    let request = task_output(&["c1"], Some(5_000));
    let settled = tool
        .task_output_with("grok-await", &request, None, &ToolCallerLineage::Direct)
        .await
        .unwrap();
    let inner = enveloped(&settled);
    assert!(
        matches!(inner["woke_by"].as_str(), Some("settled" | "mail")),
        "{inner}"
    );
    assert_eq!(inner["mail"][0]["body"], "halfway there");
    {
        let tree = driver.tree.lock().unwrap();
        assert_eq!(
            tree.pending_mail("root"),
            [] as [crate::tree::MailRecord; 0]
        );
        assert_eq!(tree.retained_wait_receipts("root"), ["grok-await"]);
    }
    let replay = tool
        .task_output_with("grok-await", &request, None, &ToolCallerLineage::Direct)
        .await
        .unwrap();
    assert_eq!(replay.joined_text(), settled.joined_text());
    // After a reopen the same call replays its durable receipt, mail ack
    // included, without consuming or waking again.
    let reopened = task_tool(
        driver_on(reload(&store, "tree1"), Arc::clone(&store)),
        TopologyKind::TaskOutput,
    );
    let replay = reopened
        .task_output_with("grok-await", &request, None, &ToolCallerLineage::Direct)
        .await
        .unwrap();
    assert_eq!(replay.joined_text(), settled.joined_text());
    assert_eq!(
        reopened.driver.tree.lock().unwrap().pending_mail("root"),
        [] as [crate::tree::MailRecord; 0]
    );
    let unknown = tool
        .task_output_with(
            "grok-unknown",
            &task_output(&["c9"], None),
            None,
            &ToolCallerLineage::Direct,
        )
        .await
        .unwrap();
    assert_eq!(unknown.kind(), ToolOutcomeKind::Failure);
    assert!(outcome_json(&unknown)["code"].is_string());
}

/// A js-ptc cell on a Grok hub sees the raw await receipt, the same bytes
/// `wait_agent` returns. Direct still gets the topology envelope. A
/// process-shaped result is not wrapped for the cell either: process-only
/// calls return the process tool's bytes, and the cell path does not add
/// an envelope around them.
#[tokio::test]
async fn js_ptc_grok_hub_child_await_returns_the_raw_receipt() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, store);
    let tool = task_tool(driver, TopologyKind::TaskOutput);
    let request = task_output(&["c1"], None);
    let cell = tool
        .task_output_with("grok-cell", &request, None, &cell_lineage("js-ptc", 1))
        .await
        .unwrap();
    let raw = outcome_json(&cell);
    assert!(raw.get("result").is_none(), "{raw}");
    assert_eq!(raw["agents"][0]["agent_id"], "c1");
    assert_eq!(raw["woke_by"], "timeout");
    let direct = tool
        .task_output_with("grok-direct", &request, None, &ToolCallerLineage::Direct)
        .await
        .unwrap();
    assert_eq!(enveloped(&direct), raw);
    let process = ToolOutcome::success_text(
        r#"{"tasks":[{"task_id":"p1","status":"exited","output":"done"}]}"#,
    )
    .unwrap();
    let shown = present_task_receipt(&cell_lineage("js-ptc", 2), process).unwrap();
    assert_eq!(
        shown.joined_text(),
        r#"{"tasks":[{"task_id":"p1","status":"exited","output":"done"}]}"#
    );
}

/// A call naming a child and the caller's own process is one await: the
/// process is a `jobs[]` snapshot row, its end wakes the await, and an id
/// the caller does not own refuses the whole call.
#[tokio::test]
async fn grok_output_mixed_call_snapshots_jobs_and_wakes_on_their_end() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("grok-mixed", root.path());
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::new(FailingStore));
    let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
    let tool = task_tool(driver.clone(), TopologyKind::TaskOutput);
    let snapshot = tool
        .task_output_with(
            "mixed-snapshot",
            &task_output(&["c1", "p1"], Some(0)),
            None,
            &ToolCallerLineage::Direct,
        )
        .await
        .unwrap();
    let inner = enveloped(&snapshot);
    assert_eq!(inner["agents"][0]["agent_id"], "c1");
    assert_eq!(inner["jobs"][0]["id"], "p1");
    assert_eq!(inner["jobs"][0]["status"], "running");
    let refused = tool
        .task_output_with(
            "mixed-foreign",
            &task_output(&["c1", "p9"], None),
            None,
            &ToolCallerLineage::Direct,
        )
        .await
        .unwrap();
    assert_eq!(outcome_json(&refused)["code"], "grant_denied");
    let request = task_output(&["c1", "1"], Some(300_000));
    let mut pending =
        Box::pin(tool.task_output_with("mixed-wait", &request, None, &ToolCallerLineage::Direct));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    jobs.stop_jobs(
        plakous::tools::JobStopReason::Model,
        std::time::Duration::from_millis(250),
    )
    .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap();
    let inner = enveloped(&outcome);
    assert_eq!(inner["woke_by"], "process");
    assert_eq!(inner["jobs"][0]["id"], "p1");
    assert_ne!(inner["jobs"][0]["status"], "running");
    assert!(driver.tree.lock().unwrap().operations().is_empty());
}

/// Grok's kill tool naming a child interrupts it, under the interrupt
/// receipt, and never closes it.
#[tokio::test]
async fn grok_kill_on_a_child_interrupts_and_never_closes() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    tree.set_active_turn("c1", true).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tool = task_tool(driver.clone(), TopologyKind::TaskKill);
    let outcome = tool
        .task_kill_child(
            "grok-kill".into(),
            &TaskKillRequest {
                task_id: "c1".into(),
            },
            &ToolCallerLineage::Direct,
        )
        .await
        .unwrap();
    let inner = enveloped(&outcome);
    assert_eq!(inner["agent_id"], "c1");
    assert_eq!(inner["status"], "interrupted");
    {
        let tree = driver.tree.lock().unwrap();
        assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
        assert!(tree.operations().contains_key("grok-kill"));
    }
    let unknown = tool
        .task_kill_child(
            "grok-kill-unknown".into(),
            &TaskKillRequest {
                task_id: "c9".into(),
            },
            &ToolCallerLineage::Direct,
        )
        .await
        .unwrap();
    assert_eq!(unknown.kind(), ToolOutcomeKind::Failure);
    assert!(outcome_json(&unknown)["code"].is_string());
}

/// A Grok hub's refusals name the tools that hub was given. A submit
/// blocked on a live child offers the output and kill tools, never
/// `wait_agent` or `interrupt_agent`.
#[test]
fn grok_hub_refusals_do_not_name_wait_agent_or_interrupt_agent() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("grok-refusal", root.path());
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::new(FailingStore));
    let tool = task_tool(driver, TopologyKind::Submit);
    let outcome = tool
        .run_named(
            "grok-submit-live",
            &json!({"turn_token": 1, "output_json": "{\"ok\":true}"}),
        )
        .unwrap();
    let payload = outcome_json(&outcome);
    assert_eq!(payload["code"], "live_work");
    let message = payload["message"].as_str().unwrap();
    for absent in ["wait_agent", "interrupt_agent"] {
        assert!(!message.contains(absent), "{message}");
    }
    assert!(
        message.contains("get_command_or_subagent_output"),
        "{message}"
    );
    assert!(message.contains("kill_command_or_subagent"), "{message}");
    assert!(message.contains("close_agent"), "{message}");
}

fn jobs_tool(driver: TreeDriver, kind: TopologyKind) -> TopologyTool {
    let mut tool = wait_tool(driver);
    tool.kind = kind;
    tool.vocabulary = TopologyVocabulary::WaitStop;
    tool
}

fn jobs_wait(ids: Option<&[&str]>, timeout_ms: Option<u64>) -> JobsWaitRequest {
    JobsWaitRequest {
        ids: ids.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect()),
        timeout_ms,
    }
}

/// Under pi's vocabulary the granted await and interrupt slots are
/// presented as `wait` and `stop`; `wait_agent` and `interrupt_agent` are
/// not, and a leaf has neither.
#[test]
fn wait_stop_vocabulary_presents_the_await_and_interrupt_as_wait_and_stop() {
    let root = tempfile::tempdir().unwrap();
    let mut tree = bootstrap_tree("pi-names", root.path());
    let spawned = tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let tree = Arc::new(Mutex::new(tree));
    let registrations = |caller: &str| {
        topology_tool_registrations_in(Arc::clone(&tree), caller, TopologyVocabulary::WaitStop)
            .unwrap()
    };
    let hub = registrations("root");
    let names = hub
        .iter()
        .map(|registration| registration.name().as_str().to_owned())
        .collect::<Vec<_>>();
    for name in ["wait", "stop", "close_agent", "spawn_agent"] {
        assert!(names.iter().any(|presented| presented == name), "{name}");
    }
    for name in ["wait_agent", "interrupt_agent"] {
        assert!(!names.iter().any(|presented| presented == name), "{name}");
    }
    let description = |name: &str| {
        hub.iter()
            .find(|registration| registration.name().as_str() == name)
            .unwrap()
            .definition()
            .description()
            .to_owned()
    };
    assert!(description("spawn_agent").contains("wait returns them"));
    assert!(!description("spawn_agent").contains("wait_agent"));
    assert_eq!(description("wait"), PLAK_TOPOLOGY_JOBS_WAIT_DESCRIPTION);
    assert_eq!(description("stop"), PLAK_TOPOLOGY_JOBS_STOP_DESCRIPTION);
    let leaf = registrations(&spawned.agent_id)
        .iter()
        .map(|registration| registration.name().as_str().to_owned())
        .collect::<Vec<_>>();
    assert!(
        !leaf.iter().any(|name| name == "wait" || name == "stop"),
        "{leaf:?}"
    );
}

/// The bare-wait rule (spec 03 §4.4) and the id split: process ids alone
/// are the process tool's own await; a child id makes it the tree's
/// await; a bare wait watches pending children and running own jobs, and
/// is the process tool's bare await when no child is pending or an own job
/// already ended unreported.
#[tokio::test]
async fn pi_wait_plans_split_processes_children_and_the_bare_wait() {
    let root = tempfile::tempdir().unwrap();
    let tree = bootstrap_tree("pi-plan", root.path());
    let driver = driver_on(tree, Arc::new(FailingStore));
    let tool = jobs_tool(driver.clone(), TopologyKind::JobsWait);
    // No children, no jobs: the process tool's bare await.
    assert_eq!(
        tool.jobs_wait_plan("pi-plan", &jobs_wait(None, None))
            .unwrap(),
        JobsWaitPlan::Processes(Vec::new())
    );
    assert_eq!(
        tool.jobs_wait_plan("pi-plan", &jobs_wait(Some(&["p2", "3"]), Some(10)))
            .unwrap(),
        JobsWaitPlan::Processes(vec![2, 3])
    );
    driver
        .tree
        .lock()
        .unwrap()
        .spawn_agent("root", &spawn_request(), None)
        .unwrap();
    // A pending child, no jobs: the tree's await over pending children,
    // 300000 ms by default.
    assert_eq!(
        tool.jobs_wait_plan("pi-plan", &jobs_wait(None, None))
            .unwrap(),
        JobsWaitPlan::Await(AwaitRequest {
            agents: AwaitAgents::Pending,
            processes: Vec::new(),
            timeout_ms: crate::protocol::DEFAULT_WAIT_TIMEOUT_MS,
        })
    );
    assert_eq!(
        tool.jobs_wait_plan("pi-plan", &jobs_wait(Some(&["c1", "p1"]), Some(9_000_000)))
            .unwrap(),
        JobsWaitPlan::Await(AwaitRequest {
            agents: AwaitAgents::Named(vec!["c1".into()]),
            processes: vec!["p1".into()],
            timeout_ms: crate::protocol::MAX_WAIT_TIMEOUT_MS,
        })
    );
    // A running own job joins the bare await.
    let (jobs, _opened) = install_running_job(&driver, "root", root.path()).await;
    assert_eq!(
        tool.jobs_wait_plan("pi-plan", &jobs_wait(None, Some(0)))
            .unwrap(),
        JobsWaitPlan::Await(AwaitRequest {
            agents: AwaitAgents::Pending,
            processes: vec!["p1".into()],
            timeout_ms: 0,
        })
    );
    // An own job that ended unreported makes it the process tool's bare
    // await, which returns at once and consumes its notice.
    jobs.stop_jobs(
        plakous::tools::JobStopReason::Model,
        std::time::Duration::from_millis(250),
    )
    .unwrap();
    assert_eq!(
        tool.jobs_wait_plan("pi-plan", &jobs_wait(None, None))
            .unwrap(),
        JobsWaitPlan::Processes(Vec::new())
    );
}

/// Pi's `wait` over a child is the tree's await: its row and the mail it
/// consumed come back in the topology envelope under the one await
/// receipt, and the same call, planned from scratch on the rejoined
/// tree, replays the receipt.
#[tokio::test]
async fn pi_wait_over_a_child_is_the_await_with_its_receipt_and_replay() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tool = jobs_tool(driver.clone(), TopologyKind::JobsWait);
    child_reports(&driver);
    let request = jobs_wait(None, Some(5_000));
    let JobsWaitPlan::Await(awaited) = tool.jobs_wait_plan("pi-await", &request).unwrap() else {
        panic!("a pending child makes a bare wait the tree's await");
    };
    let settled = tool
        .jobs_wait_with(
            "pi-await",
            &awaited,
            &request,
            None,
            &ToolCallerLineage::Direct,
        )
        .await
        .unwrap();
    let inner = enveloped(&settled);
    assert!(
        matches!(inner["woke_by"].as_str(), Some("settled" | "mail")),
        "{inner}"
    );
    assert_eq!(inner["mail"][0]["body"], "halfway there");
    {
        let tree = driver.tree.lock().unwrap();
        assert_eq!(
            tree.pending_mail("root"),
            [] as [crate::tree::MailRecord; 0]
        );
        assert_eq!(tree.retained_wait_receipts("root"), ["pi-await"]);
    }
    // A rejoined turn: the tree reopens from the store with the receipt
    // retained and nothing pending, and the same call is planned from
    // scratch, as `jobs_wait` does.
    let rejoined = driver_on(reload(&store, "tree1"), Arc::clone(&store));
    let tool = jobs_tool(rejoined, TopologyKind::JobsWait);
    let JobsWaitPlan::Await(again) = tool.jobs_wait_plan("pi-await", &request).unwrap() else {
        panic!("a call the ledger holds as an await replays as the await");
    };
    let replay = tool
        .jobs_wait_with(
            "pi-await",
            &again,
            &request,
            None,
            &ToolCallerLineage::Direct,
        )
        .await
        .unwrap();
    assert_eq!(replay.joined_text(), settled.joined_text());
    // A new call with nothing pending is the process tool's bare await.
    assert_eq!(
        tool.jobs_wait_plan("pi-next", &request).unwrap(),
        JobsWaitPlan::Processes(Vec::new())
    );
}

fn child_reports(driver: &TreeDriver) {
    let mut tree = driver.tree.lock().unwrap();
    tree.send_agent_message(
        "c1",
        &SendAgentMessageRequest {
            agent_id: "root".into(),
            message: "halfway there".into(),
            purpose: None,
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed("c1").unwrap();
}

/// A same-call replay of a bare pi `wait` that ran the
/// tree's await replays its receipt (spec 03 §4.4) even though the first
/// run delivered the only pending child, which would plan a new bare
/// call onto the process path.
#[tokio::test]
async fn bare_pi_wait_same_call_replay_keeps_the_await() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tool = jobs_tool(driver.clone(), TopologyKind::JobsWait);
    child_reports(&driver);
    let request = jobs_wait(None, Some(5_000));
    let JobsWaitPlan::Await(awaited) = tool.jobs_wait_plan("pi-bare", &request).unwrap() else {
        panic!("a pending child makes a bare wait the tree's await");
    };
    let first = tool
        .jobs_wait_with(
            "pi-bare",
            &awaited,
            &request,
            None,
            &ToolCallerLineage::Direct,
        )
        .await
        .unwrap();
    assert_eq!(
        driver.tree.lock().unwrap().retained_wait_receipts("root"),
        ["pi-bare"]
    );
    let planned = tool.jobs_wait_plan("pi-bare", &request).unwrap();
    let JobsWaitPlan::Await(again) = planned else {
        panic!(
            "same-call replay of bare wait `pi-bare` re-planned to {planned:?}; its retained \
             receipt {} is lost",
            first.joined_text()
        );
    };
    let replay = tool
        .jobs_wait_with(
            "pi-bare",
            &again,
            &request,
            None,
            &ToolCallerLineage::Direct,
        )
        .await
        .unwrap();
    assert_eq!(replay.joined_text(), first.joined_text());
}

/// A re-run cell repeats its nested call ids: a bare pi `wait` a cell ran
/// as the tree's await replays its raw receipt under the same nested id.
#[tokio::test]
async fn bare_pi_wait_in_a_rerun_cell_replays_its_raw_receipt() {
    use std::str::FromStr;
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tool = jobs_tool(driver.clone(), TopologyKind::JobsWait);
    child_reports(&driver);
    let cell = ToolCallerLineage::local_programmatic(
        41,
        7,
        "cell-1",
        1,
        plakous::model::CallId::from_str(&uuid::Uuid::from_u128(9).to_string()).unwrap(),
        1,
    )
    .unwrap();
    let nested = uuid::Uuid::from_u128(10).to_string();
    let request = jobs_wait(None, Some(5_000));
    let mut shown = Vec::new();
    for _run in 0..2 {
        let JobsWaitPlan::Await(awaited) = tool.jobs_wait_plan(&nested, &request).unwrap() else {
            panic!("the cell's bare wait is the tree's await, then its replay");
        };
        let outcome = tool
            .jobs_wait_with(&nested, &awaited, &request, None, &cell)
            .await
            .unwrap();
        shown.push(outcome.joined_text());
    }
    assert_eq!(shown[0], shown[1]);
    let raw: Value = serde_json::from_str(&shown[0]).unwrap();
    assert!(raw.get("result").is_none(), "{raw}");
    assert_eq!(raw["mail"][0]["body"], "halfway there", "{raw}");
}

/// Pi's `stop` naming a child interrupts it under the interrupt receipt
/// and never closes it.
#[tokio::test]
async fn pi_stop_on_a_child_interrupts_and_never_closes() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut tree) = persist_fixture(&root);
    tree.spawn_agent("root", &spawn_request(), None).unwrap();
    tree.set_active_turn("c1", true).unwrap();
    let driver = driver_on(tree, Arc::clone(&store));
    let tool = jobs_tool(driver.clone(), TopologyKind::JobsStop);
    let request: JobsStopRequest = serde_json::from_value(json!({"id": "c1"})).unwrap();
    assert!(task_process_number(&request.id).is_none());
    let outcome = tool
        .task_kill_child(
            "pi-stop".into(),
            &TaskKillRequest {
                task_id: request.id,
            },
            &ToolCallerLineage::Direct,
        )
        .await
        .unwrap();
    let inner = enveloped(&outcome);
    assert_eq!(inner["status"], "interrupted");
    let tree = driver.tree.lock().unwrap();
    assert_eq!(tree.node("c1").unwrap().status, NodeStatus::Interrupted);
}
