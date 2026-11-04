#![allow(
    clippy::too_many_lines,
    reason = "Park and wake scenarios force interrupt and mail races across durable admission and rejoin boundaries."
)]
#![allow(clippy::unwrap_used, reason = "offline park fixtures")]

use crate::agent::ledger::ChildLedger;
use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind, CommandLedger};
use std::time::Duration;

use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
use serde_json::json;

use super::*;
use crate::agent::AttachedCodingSession;
use crate::agent::live::ChildActorHandle;
use crate::agent_tree::ProductAgentTree;
use crate::agent_tree_protocol::{
    AttemptStatus, NodeStatus, SendAgentMessageRequest, SpawnAgentRequest, SubmitResultRequest,
    TemplateName,
};
use crate::topology_runtime::ProductTopologyRuntime;

pub(in crate::agent) struct Fixture {
    provider_gates: Vec<plakous_offline_crash_harness::gate::GateHandle>,
    pub(in crate::agent) root: tempfile::TempDir,
    pub(in crate::agent) runtime: Arc<ProductTopologyRuntime>,
    pub(in crate::agent) agent: plakous::agent::Agent,
    pub(in crate::agent) registry: Arc<crate::jobs::JobRegistry>,
    sessions: Arc<AgentSessions>,
    binding: Box<dyn Fn() -> plakous::sessions::SessionRuntimeBinding + Send + Sync>,
    pub(in crate::agent) child: String,
}

impl Fixture {
    pub(in crate::agent) async fn until(
        &self,
        what: &str,
        done: impl Fn(&ProductAgentTree, &str) -> bool,
    ) {
        let changed = self.runtime.read(ProductAgentTree::waiters).unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let notified = changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.runtime.read(|tree| done(tree, &self.child)).unwrap() {
                    break;
                }
                notified.await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {what}: {:?}",
                self.runtime.read(|tree| (
                    tree.attempt(&self.child).cloned(),
                    tree.waiting_on(&self.child).cloned(),
                    tree.fault_reason(&self.child).map(str::to_owned),
                ))
            )
        });
    }

    /// The exact wake prompts the result-context journal admitted.
    fn wake_prompts(&self) -> Vec<String> {
        self.runtime
            .read(|tree| {
                tree.commands()
                    .values()
                    .filter(|command| {
                        command.recipient == self.child
                            && command.kind == crate::agent_tree::AgentCommandKind::Wake
                    })
                    .map(|command| command.text.clone())
                    .collect()
            })
            .unwrap()
    }

    pub(in crate::agent) async fn mutate<R>(
        &self,
        work: impl FnOnce(&mut ProductAgentTree) -> R,
    ) -> R {
        self.runtime
            .mutate_then_emit_borrowed_for_test(false, |tree| Ok(work(tree)))
            .await
            .unwrap()
    }

    pub(in crate::agent) async fn submit(&self) {
        self.mutate(|tree| {
            tree.submit_result(
                &self.child,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.to_owned(),
                },
                None,
            )
            .unwrap();
        })
        .await;
        for gate in &self.provider_gates {
            gate.release();
        }
    }

    /// Waits until the child's model reaches its first held stream.
    pub(in crate::agent) async fn held_reached(&self) {
        tokio::time::timeout(Duration::from_secs(30), self.provider_gates[0].entered())
            .await
            .expect("the held stream is reached");
    }
}

pub(in crate::agent) fn parked(tree: &ProductAgentTree, child: &str) -> bool {
    tree.waiting_on(child).is_some()
}

pub(in crate::agent) fn turns(count: u32) -> impl Fn(&ProductAgentTree, &str) -> bool {
    // Acceptance is durable; the projected live turn can already be gone
    // when a loaded test next polls, even though the wake ran successfully.
    move |tree, child| {
        tree.attempt(child)
            .is_some_and(|attempt| attempt.usage.turns >= count)
    }
}

pub(in crate::agent) fn settled(tree: &ProductAgentTree, child: &str) -> bool {
    tree.attempt(child)
        .is_some_and(|attempt| attempt.status.is_stopped())
}

/// One child with a real scripted session whose first turn is admitted
/// and handed to the production turn waiter, exactly as a spawn does.
pub(in crate::agent) async fn fixture(
    steps: serde_json::Value,
    setup: impl FnOnce(&mut ProductAgentTree) -> String,
) -> Fixture {
    fixture_with_probe(steps, setup, None).await
}

/// A fixture whose child's tool calls wait for an operator decision.
pub(in crate::agent) async fn fixture_deciding(
    steps: serde_json::Value,
    setup: impl FnOnce(&mut ProductAgentTree) -> String,
) -> Fixture {
    fixture_with(steps, setup, None, Some("decision_required")).await
}

async fn fixture_with_probe(
    steps: serde_json::Value,
    setup: impl FnOnce(&mut ProductAgentTree) -> String,
    probe: Option<plakous::model::ReachabilityProbe>,
) -> Fixture {
    fixture_with(steps, setup, probe, None).await
}

async fn fixture_with(
    mut steps: serde_json::Value,
    setup: impl FnOnce(&mut ProductAgentTree) -> String,
    probe: Option<plakous::model::ReachabilityProbe>,
    tool_policy: Option<&str>,
) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        crate::topology_runtime::test_support::bootstrap("park-wake", root.path().to_owned()),
        crate::topology_runtime::test_support::memory_store(),
        Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let child = runtime
        .mutate_then_emit_borrowed_for_test(false, |tree| Ok(setup(tree)))
        .await
        .unwrap();
    let mut provider_gates = Vec::new();
    for step in steps.as_array_mut().unwrap() {
        if let Some(stream) = step
            .get_mut("stream")
            .and_then(serde_json::Value::as_array_mut)
        {
            for event in stream {
                if event.get("gate").is_some() {
                    let provider_gate = plakous_offline_crash_harness::gate::GateHandle::new();
                    event["gate"] = serde_json::to_value(provider_gate.script()).unwrap();
                    provider_gates.push(provider_gate);
                }
            }
        }
    }
    let mut plan = json!({
        "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
        "process_tools": true,
        "steps": steps,
    });
    if let Some(tool_policy) = tool_policy {
        plan["tool_policy"] = json!(tool_policy);
    }
    let plan: Arc<plakous_offline_crash_harness::script::ScriptPlan> =
        Arc::new(serde_json::from_value(plan).unwrap());
    let registry = Arc::new(crate::jobs::JobRegistry::default());
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("park.jsonl"),
    )
    .unwrap();
    let sessions = Arc::new(
        AgentSessions::new(SessionBackend::sqlite(root.path().join("park.sqlite3")).unwrap())
            .unwrap(),
    );
    let binding = {
        let (path, registry) = (root.path().to_owned(), registry.clone());
        // The child's own processes, as production attributes them.
        let jobs = registry.for_agent(child.clone(), "park-wake");
        let cursor = probe
            .as_ref()
            .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(1)));
        move || {
            let binding = if let Some(cursor) = &cursor {
                plakous_offline_crash_harness::scripted_runtime_binding_with_cursor(
                    &path,
                    controller.clone(),
                    plan.clone(),
                    cursor.clone(),
                )
            } else {
                scripted_runtime_binding(&path, controller.clone(), plan.clone())
            };
            binding.unwrap().with_job_host(jobs.clone())
        }
    };
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("park:create").unwrap(),
            binding(),
        ))
        .await
        .unwrap()
    else {
        panic!("created child session required")
    };
    runtime
        .mutate_then_emit_borrowed_for_test(false, |tree| {
            tree.record_child_session(&child, &session.locator().to_string())
                .map_err(|error| error.message)?;
            let attempt = tree.attempt(&child).unwrap().attempt_id.clone();
            tree.record_command(crate::agent_tree::AgentCommand::intent(
                "park:assignment",
                &child,
                Some(attempt),
                crate::agent_tree::AgentCommandKind::Start,
                "assignment",
                Vec::new(),
            ))
            .map_err(|error| error.message)
        })
        .await
        .unwrap();
    let agent = session.agent().clone();
    let registration =
        super::super::children::TurnWaiterRegistration::register(runtime.driver(), &child)
            .await
            .unwrap();
    let submission = agent
        .start(CommandId::new("park:assignment").unwrap(), "assignment")
        .await
        .unwrap();
    let result = submission.result().unwrap().clone();
    let turn = result.turn_id();
    let control = submission.turn().unwrap().control();
    let locator = session.locator();
    let initial_feed = super::super::read_tree_feed(&sessions, locator.session_id(), 0)
        .await
        .unwrap();
    runtime
        .mutate_then_emit_borrowed_for_test(false, |tree| {
            initial_feed
                .apply(tree, &child)
                .map(|_| ())
                .map_err(|error| error.message)
        })
        .await
        .unwrap();
    let attached = AttachedCodingSession {
        output_bound_rederived: None,
        reachability_probe: probe,
        sessions: sessions.clone(),
        locator,
        opened: Some(session),
        recovered_turn: None,
        topology_owner: None,
    };
    let feed = super::super::children::ChildFeedSource::of(&attached);
    let live = ChildActorHandle::new(&runtime.driver().release_owners(), attached, turn, control);
    let watcher = live.watcher();
    runtime
        .try_occupy_live(child.clone(), Box::new(live))
        .await
        .map_err(|_| "live slot refused")
        .unwrap();
    watcher.watch(
        registration,
        Arc::clone(&runtime.live),
        None,
        Some(feed),
        result,
    );
    Fixture {
        provider_gates,
        root,
        runtime,
        agent,
        registry,
        sessions,
        binding: Box::new(binding),
        child,
    }
}

pub(in crate::agent) fn start_job(command: &str) -> serde_json::Value {
    json!({"output": {"tool": {"name": "process_session_start", "arguments": {
        "command": command, "workdir": ".", "login": false, "tty": false,
        "keep_stdin_open": true, "yield_time_ms": 0, "max_output_bytes": 8192
    }}}, "finish": "tool_calls"})
}

pub(in crate::agent) fn leaf(tree: &mut ProductAgentTree) -> String {
    tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id
}

/// Hold the final stream until the test submits or cancels the model.
pub(in crate::agent) fn held(text: &str) -> serde_json::Value {
    json!({"stream": [{"gate": {"registry": "test-held-response"}}], "output": {"text": text}})
}

#[tokio::test]
async fn transient_failure_continues_same_attempt_after_exact_route_probe() {
    use plakous::model::{ReachabilityProbe, RouteId};
    let entered = Arc::new(tokio::sync::Notify::new());
    let online = Arc::new(tokio::sync::Notify::new());
    let probe = ReachabilityProbe::new(RouteId::new("offline.loopback").unwrap(), {
        let entered = entered.clone();
        let online = online.clone();
        move |_, _| {
            let online = online.clone();
            entered.notify_one();
            Box::pin(async move {
                online.notified().await;
                Ok(())
            })
        }
    });
    let fixture = fixture_with_probe(json!([
        {"fault": {"at": "pre_stream", "kind": "transport", "phase": "Connect", "retryability": "Exhausted", "message": "offline network failure"}, "output": {"text": "unused"}},
        held("continued")
    ]), |tree| {
        let mut epoch = tree.current_epoch().clone();
        epoch.transient_resumes = 1;
        tree.install_snapshot(epoch);
        leaf(tree)
    }, Some(probe)).await;
    entered.notified().await;
    let (attempt_id, token) = fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.status, AttemptStatus::Running);
            assert!(
                tree.is_parked(&fixture.child),
                "mail must not start a second turn beside a pending continuation"
            );
            assert!(tree.waiting_on(&fixture.child).unwrap().network);
            let listed = tree
                .list_agents(
                    "root",
                    &crate::agent_tree_protocol::ListAgentsRequest {
                        include_read: None,
                        include_self: None,
                        include_completed: None,
                    },
                )
                .unwrap();
            assert_eq!(listed.agents[0]["waiting_on"]["network"], true);
            assert_eq!(
                tree.transient_resume_count(&fixture.child, &attempt.attempt_id)
                    .unwrap(),
                1
            );
            assert_eq!(attempt.submit_harness_nudge.rounds(), 0);
            (attempt.attempt_id.clone(), attempt.turn_token)
        })
        .unwrap();
    online.notify_one();
    fixture.provider_gates[0].entered().await;
    fixture.until("continuation acceptance", turns(2)).await;
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.attempt_id, attempt_id);
            assert_eq!(attempt.turn_token, token);
            let id = format!("plak:resume-transient:{}:{attempt_id}:1", fixture.child);
            let command = tree.command(&id).unwrap();
            assert_eq!(command.kind, crate::agent_tree::AgentCommandKind::Resume);
            assert_eq!(command.fact, Some(crate::agent_tree::CommandFact::Accepted));
            assert!(command.text.contains("offline network failure"));
            let bytes = serde_json::to_string(&json!({
                "command_id": command.command_id,
                "recipient": command.recipient,
                "attempt_id": command.attempt_id,
                "kind": command.kind,
                "text": command.text,
                "message_ids": command.message_ids,
            }))
            .unwrap();
            assert_eq!(
                bytes.as_bytes(),
                include_str!("fixtures/child-resume.json")
                    .trim_end()
                    .as_bytes()
            );
            assert!(command.text.contains("Do not repeat completed actions"));
        })
        .unwrap();
    fixture.submit().await;
    fixture.provider_gates[0].release();
    fixture.until("submitted continuation", settled).await;
    assert_eq!(
        fixture
            .runtime
            .read(|tree| tree.attempt(&fixture.child).unwrap().status)
            .unwrap(),
        AttemptStatus::Completed
    );
}

#[tokio::test]
async fn transient_resume_exhaustion_preserves_failure_without_corrective_rounds() {
    let fault = json!({"fault": {"at": "pre_stream", "kind": "transport", "phase": "Connect", "retryability": "Exhausted", "message": "original outage"}, "output": {"text": "unused"}});
    let probe = plakous::model::ReachabilityProbe::new(
        plakous::model::RouteId::new("offline.exhaustion").unwrap(),
        |_, _| Box::pin(async { Ok(()) }),
    );
    let fixture = fixture_with_probe(json!([fault.clone(), fault]), leaf, Some(probe)).await;
    fixture.until("exhausted failure", settled).await;
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.status, AttemptStatus::Failed);
            assert!(
                attempt
                    .failure
                    .as_ref()
                    .unwrap()
                    .message
                    .contains("original outage")
            );
            assert_eq!(attempt.submit_harness_nudge.rounds(), 0);
            assert_eq!(attempt.usage.turns, 2);
            assert_eq!(
                tree.transient_resume_count(&fixture.child, &attempt.attempt_id)
                    .unwrap(),
                1
            );
            assert_eq!(
                tree.commands()
                    .values()
                    .filter(|command| command.kind == crate::agent_tree::AgentCommandKind::Resume)
                    .count(),
                1
            );
        })
        .unwrap();
}

#[tokio::test]
async fn a_live_job_parks_the_child_and_its_terminal_wakes_it_to_submit() {
    let fixture = fixture(
        json!([start_job("while [ ! -e release-job ]; do sleep 0.01; done"),
            {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.status, AttemptStatus::Running);
            assert_eq!(
                attempt.submit_harness_nudge.rounds(),
                0,
                "a park is not a nudge"
            );
            assert_eq!(tree.waiting_on(&fixture.child).unwrap().processes, ["p1"]);
            // The parent's rows say what the child waits on.
            let listed = tree
                .list_agents(
                    "root",
                    &crate::agent_tree_protocol::ListAgentsRequest {
                        include_read: None,
                        include_self: None,
                        include_completed: None,
                    },
                )
                .unwrap();
            assert_eq!(
                listed.agents[0]["waiting_on"],
                json!({"processes": ["p1"], "agents": []})
            );
        })
        .unwrap();
    std::fs::write(fixture.root.path().join("release-job"), b"release").unwrap();
    fixture.until("the wake turn", turns(2)).await;
    let wakes = fixture.wake_prompts();
    assert_eq!(wakes, [crate::child_prompts::PLAK_CHILD_WAKE_PROMPT]);
    fixture
        .runtime
        .read(|tree| {
            let command = tree
                .commands()
                .values()
                .find(|row| row.kind == crate::agent_tree::AgentCommandKind::Wake)
                .unwrap();
            let bytes = serde_json::to_string(&json!({
                "command_id": command.command_id, "recipient": command.recipient,
                "attempt_id": command.attempt_id, "kind": command.kind,
                "text": command.text, "message_ids": command.message_ids,
            }))
            .unwrap();
            assert_eq!(
                bytes.as_bytes(),
                include_str!("fixtures/child-wake.json")
                    .trim_end()
                    .as_bytes()
            );
        })
        .unwrap();
    fixture.submit().await;
    fixture.until("the attempt to settle", settled).await;
    fixture
        .runtime
        .read(|tree| {
            assert_eq!(
                tree.attempt(&fixture.child).unwrap().status,
                AttemptStatus::Completed
            );
            assert_eq!(
                tree.attempt(&fixture.child)
                    .unwrap()
                    .submit_harness_nudge
                    .rounds(),
                0
            );
        })
        .unwrap();
}

#[tokio::test]
async fn a_live_child_parks_the_hub_its_result_wakes_it_and_an_empty_end_is_nudged() {
    let mut grandchild = String::new();
    let fixture = fixture(
        json!([{"output": {"text": "delegated"}}, {"output": {"text": "read it"}}, held("reminded")]),
        |tree| {
            let mut snapshot = tree.current_epoch().clone();
            snapshot.max_depth = 2;
            snapshot.digest = snapshot.compute_digest();
            tree.install_snapshot(snapshot);
            let hub = tree
                .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
                .unwrap()
                .agent_id;
            grandchild = tree
                .spawn_agent(&hub, &SpawnAgentRequest::test_review(), None)
                .unwrap()
                .agent_id;
            hub
        },
    )
    .await;
    fixture.until("the park", parked).await;
    let waiting = fixture
        .runtime
        .read(|tree| tree.waiting_on(&fixture.child).cloned().unwrap())
        .unwrap();
    assert_eq!(waiting.agents, [grandchild.clone()]);
    assert_eq!(waiting.processes, [] as [String; 0]);
    fixture
        .mutate(|tree| {
            tree.submit_result(
                &grandchild,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.to_owned(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed(&grandchild).unwrap();
        })
        .await;
    fixture.until("the wake turn", turns(2)).await;
    let wakes = fixture.wake_prompts();
    assert_eq!(wakes.len(), 1);
    assert!(wakes[0].starts_with(crate::child_prompts::PLAK_CHILD_WAKE_PROMPT));
    assert!(wakes[0].contains("untrusted data from sub-agents"));
    assert!(wakes[0].contains(&format!("from=\"{grandchild}\" purpose=\"result\"")));
    // The wake consumed the result; with nothing left the end is nudged.
    fixture
        .until("the reminder", |tree, child| {
            tree.attempt(child)
                .is_some_and(|attempt| attempt.submit_harness_nudge.rounds() == 1)
        })
        .await;
    assert!(
        fixture
            .runtime
            .read(|tree| tree.wake_mail(&fixture.child).is_empty())
            .unwrap()
    );
    fixture.submit().await;
    fixture.until("the attempt to settle", settled).await;
}

#[tokio::test]
async fn a_completed_wake_is_observable_after_its_live_turn_ends() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}},
            {"output": {"text": "noted"}}]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    parent_mail(&fixture, "also check the docs").await;
    // Deliberately observe only after the fast wake has ended and parked.
    fixture
        .until("the completed wake's park", |tree, child| {
            parked(tree, child)
                && tree
                    .attempt(child)
                    .is_some_and(|attempt| attempt.usage.turns == 2)
                && tree.wake_mail(child).is_empty()
        })
        .await;
    assert!(
        fixture
            .runtime
            .read(|tree| turns(2)(tree, &fixture.child))
            .unwrap()
    );
    fixture
        .registry
        .stop_all(Duration::from_millis(250))
        .await
        .unwrap();
}

#[tokio::test]
async fn parent_mail_wakes_a_parked_child_and_an_interrupt_keeps_its_job() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, {"output": {"text": "noted"}}]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let sent = fixture
        .mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    refresh: None,
                    agent_id: fixture.child.clone(),
                    message: "also check the docs".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
        })
        .await;
    assert_eq!(sent.disposition, crate::agent_tree::MailDisposition::Queued);
    fixture.until("the wake turn", turns(2)).await;
    assert_eq!(
        fixture.wake_prompts(),
        [format!(
            "{}\n\nMessage:\nalso check the docs",
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT
        )]
    );
    // The job still runs, so the wake turn's end parks again.
    fixture.until("the second park", parked).await;
    let status = fixture
        .mutate(|tree| {
            tree.interrupt_agent(
                "root",
                &crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: fixture.child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap()
        })
        .await;
    assert_eq!(status, NodeStatus::Interrupted);
    fixture.runtime.cancel_live(&fixture.child).await.unwrap();
    fixture
        .until("the park to end", |tree, child| !tree.is_parked(child))
        .await;
    assert_eq!(
        fixture.agent.jobs()[0].status,
        plakous::tools::JobStatus::Running,
        "an interrupt keeps the child's processes (spec 03 §5.2)"
    );
    fixture
        .registry
        .stop_all(Duration::from_millis(250))
        .await
        .unwrap();
}

/// A follow-up sent as the turn ended (the tree still saw it active)
/// reaches the ended turn: it is not a delivery failure, and its mail is
/// left unread for the wake.
#[tokio::test]
async fn a_follow_up_that_reaches_an_ended_turn_is_left_for_the_wake() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let error = {
        let live = fixture.runtime.live.lock().await;
        let child = live.get(&fixture.child).unwrap();
        child
            .follow_up(
                CommandId::new("plak:mail:late:follow-up").unwrap(),
                "one more thing".to_owned(),
            )
            .await
            .unwrap_err()
    };
    assert_eq!(error, super::super::live::TURN_ENDED);
    assert_eq!(
        super::super::children::turn_ended_or(error),
        crate::agent_tree_tools::MailDeliveryFailure::TurnEnded
    );
    assert!(
        fixture
            .runtime
            .read(|tree| tree.is_parked(&fixture.child))
            .unwrap(),
        "nothing was admitted behind the ended turn"
    );
    fixture
        .registry
        .stop_all(Duration::from_millis(250))
        .await
        .unwrap();
}

#[tokio::test]
async fn wake_turns_count_toward_max_turns() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, {"output": {"text": "noted"}}]),
        |tree| {
            let name = TemplateName::parse("review").unwrap();
            let mut binding = tree.current_epoch().binding(&name).unwrap().clone();
            binding.max_turns = Some(2);
            tree.pin_template("review", binding);
            leaf(tree)
        },
    )
    .await;
    fixture.until("the park", parked).await;
    fixture
        .mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    refresh: None,
                    agent_id: fixture.child.clone(),
                    message: "status?".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        })
        .await;
    // The wake is the second turn; its end cannot park past the ceiling.
    fixture.until("the attempt to settle", settled).await;
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.usage.turns, 2);
            assert_eq!(attempt.status, AttemptStatus::Blocked);
            assert!(!tree.is_parked(&fixture.child));
        })
        .unwrap();
    fixture
        .registry
        .stop_all(Duration::from_millis(250))
        .await
        .unwrap();
}

/// The owning process goes away: the parked child's live session is
/// released and its processes stop; only durable rows remain. Returns the
/// session reopened as the next process would.
async fn lose_and_reopen(fixture: &Fixture) -> AttachedCodingSession {
    crate::topology_runtime::retire_live(
        fixture.runtime.driver(),
        &fixture.runtime.live,
        &fixture.child,
        None,
        false,
    )
    .await
    .unwrap();
    fixture
        .until("the park to end", |tree, child| !tree.is_parked(child))
        .await;
    fixture
        .registry
        .stop_all(Duration::from_millis(250))
        .await
        .unwrap();
    let locator = fixture
        .runtime
        .read(|tree| tree.node(&fixture.child).unwrap().session_locator.clone())
        .unwrap();
    let plakous::sessions::OpenSessionOutcome::Opened { session, .. } = fixture
        .sessions
        .open(plakous::sessions::OpenSessionRequest::new(
            SessionOperationId::new("park:reopen").unwrap(),
            locator.unwrap().parse().unwrap(),
            (fixture.binding)(),
        ))
        .await
        .unwrap()
    else {
        panic!("parked child session must reopen")
    };
    AttachedCodingSession {
        output_bound_rederived: None,
        reachability_probe: None,
        sessions: fixture.sessions.clone(),
        locator: session.locator(),
        opened: Some(session),
        recovered_turn: None,
        topology_owner: None,
    }
}

/// A turn the lost process admitted before it bound, rejoined as
/// recover_node rejoins it: the reopen law finds the parked child's start
/// unfacted and owes it a recovery instead of marking it, the child reopen
/// reconciles its journal with the recovered turn unbound, and the rejoin
/// binds it. Returns what the rejoin did.
async fn rejoin_held(
    fixture: &Fixture,
    attached: AttachedCodingSession,
    control: plakous::agent::TurnControl,
    result: TurnResultHandle,
    before_rejoin: impl AsyncFnOnce(&Fixture),
) -> Reopened {
    let turn = result.turn_id();
    fixture.mutate(ProductAgentTree::apply_reopen_law).await;
    assert!(
        fixture
            .runtime
            .read(|tree| tree.recovery_owed(&fixture.child)
                && tree.node(&fixture.child).unwrap().status == NodeStatus::Running)
            .unwrap(),
        "an unfacted start exempts the parked child from the mark"
    );
    before_rejoin(fixture).await;
    super::super::children::reconcile_reopened_results(
        &fixture.runtime,
        &fixture.child,
        &attached,
        Some(turn),
    )
    .await
    .unwrap();
    rejoin_turn(
        &fixture.runtime,
        &fixture.child,
        attached,
        turn,
        control,
        result,
    )
    .await
    .unwrap()
}

async fn parent_mail(fixture: &Fixture, text: &str) {
    fixture
        .mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    refresh: None,
                    agent_id: fixture.child.clone(),
                    message: text.into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap();
        })
        .await;
}

#[tokio::test]
async fn a_reopen_marks_a_parked_child_owner_lost_and_wakes_nothing() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let targets = fixture
        .runtime
        .read(ProductAgentTree::parked_reopen_targets)
        .unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].0, fixture.child);
    fixture.mutate(ProductAgentTree::apply_reopen_law).await;
    fixture
        .runtime
        .read(|tree| {
            // The rows prove it parked between turns: interrupted, attempt
            // and turn count kept, owed nothing, and no longer a target.
            assert_eq!(
                tree.node(&fixture.child).unwrap().status,
                NodeStatus::Interrupted
            );
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.status, AttemptStatus::Running);
            assert_eq!(attempt.usage.turns, 1);
            assert!(!tree.recovery_owed(&fixture.child));
            assert_eq!(
                tree.parked_reopen_targets(),
                [] as [(String, String, String); 0]
            );
        })
        .unwrap();
    attached.release_opened().await.unwrap();
    assert_eq!(fixture.wake_prompts(), [] as [String; 0], "nothing woke it");
}

#[tokio::test]
async fn late_finalize_reads_after_acceptance_without_replaying_old_history() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let (accepted, revision, usage, cursor) = fixture
        .runtime
        .read(|tree| {
            let command = tree.command("park:assignment").unwrap();
            (
                command.accepted_seq.unwrap(),
                tree.revision,
                tree.attempt(&fixture.child).unwrap().usage,
                tree.feed_cursor(&attached.locator().session_id().to_string()),
            )
        })
        .unwrap();
    let db = rusqlite::Connection::open(fixture.root.path().join("park.sqlite3")).unwrap();
    db.execute(
        "DELETE FROM session_feed WHERE seq <= ?1",
        [i64::try_from(accepted).unwrap()],
    )
    .unwrap();
    db.execute("UPDATE sessions SET feed_retained_from=?1, feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed)", [i64::try_from(accepted + 1).unwrap()]).unwrap();
    let terminal = super::super::children::read_finalize_terminal(
        fixture.runtime.driver(),
        &fixture.sessions,
        attached.locator().session_id(),
        &fixture.child,
    )
    .await
    .unwrap();
    assert!(terminal.seq > accepted);
    assert!(terminal.seq <= cursor);
    fixture
        .runtime
        .read(|tree| {
            assert_eq!(tree.revision, revision);
            assert_eq!(tree.attempt(&fixture.child).unwrap().usage, usage);
            assert_eq!(
                tree.feed_cursor(&attached.locator().session_id().to_string()),
                cursor
            );
        })
        .unwrap();
    db.execute(
        "DELETE FROM session_feed WHERE seq=?1",
        [i64::try_from(terminal.seq).unwrap()],
    )
    .unwrap();
    assert!(
        super::super::children::read_finalize_terminal(
            fixture.runtime.driver(),
            &fixture.sessions,
            attached.locator().session_id(),
            &fixture.child,
        )
        .await
        .is_err()
    );
    assert_eq!(
        fixture
            .runtime
            .read(|tree| tree.attempt(&fixture.child).unwrap().status)
            .unwrap(),
        AttemptStatus::Running
    );
    attached.release_opened().await.unwrap();
}

/// Mail that is unread when a turn ends wakes the child although nothing
/// it started still runs: an arrival is not a nudge.
#[tokio::test]
async fn child_deadline_fire_is_admitted_by_the_park_adapter() {
    let fixture = fixture(json!([held("thinking"), held("woke")]), leaf).await;
    parent_mail(&fixture, "wake at the coalescing deadline").await;
    let facts = read_facts(
        fixture.runtime.driver(),
        &fixture.runtime.live,
        &fixture.child,
    )
    .await
    .unwrap();
    let mut coalescer = WakeCoalescer::new(WakePolicy::Child);
    let now = std::time::Instant::now();
    let WakeDecision::Wait { until } = coalescer.observe(&facts, now) else {
        panic!("an arrival starts the coalescing window");
    };
    assert_eq!(
        coalescer.observe(&facts, until),
        WakeDecision::Fire(facts.arrivals.clone())
    );
    assert_eq!(
        coalescer.observe(&facts, until + Duration::from_secs(1)),
        WakeDecision::Fire(facts.arrivals)
    );
    fixture.provider_gates[0].release();
    fixture
        .until("the deadline fire's wake acceptance", turns(2))
        .await;
    assert_eq!(fixture.wake_prompts().len(), 1);
    assert!(fixture.wake_prompts()[0].contains("wake at the coalescing deadline"));
    fixture.submit().await;
    fixture.until("the awakened child to settle", settled).await;
}

#[tokio::test]
async fn an_arrival_without_live_work_wakes_instead_of_nudging() {
    let fixture = fixture(json!([held("thinking"), held("woke")]), leaf).await;
    parent_mail(&fixture, "use the v2 schema").await;
    fixture.provider_gates[0].release();
    fixture.until("the wake turn", turns(2)).await;
    assert_eq!(
        fixture.wake_prompts(),
        [format!(
            "{}\n\nMessage:\nuse the v2 schema",
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT
        )]
    );
    fixture.submit().await;
    fixture.until("the attempt to settle", settled).await;
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.status, AttemptStatus::Completed);
            assert_eq!(attempt.submit_harness_nudge.rounds(), 0);
        })
        .unwrap();
}

/// An interrupt that lands after the turn ended but before the waiter
/// parked takes the interrupted-turn path: the bound turn is released,
/// so the next mail starts the resume turn (spec 03 §9) instead of
/// queueing behind a turn that no longer exists.
#[tokio::test]
async fn an_interrupt_racing_the_park_decision_releases_the_bound_turn() {
    let fixture = fixture(json!([start_job("read line"), held("started")]), leaf).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !fixture
            .agent
            .jobs()
            .iter()
            .any(|job| job.status == plakous::tools::JobStatus::Running)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    // Holding the live table stops the waiter at its park decision, after
    // it classified the ended turn as an ordinary completion.
    fixture.provider_gates[0].entered().await;
    let held = fixture.runtime.live.lock().await;
    fixture.provider_gates[0].release();
    fixture
        .until("the turn end", |tree, child| {
            tree.attempt(child)
                .is_some_and(|attempt| attempt.usage.tool_calls >= 1)
        })
        .await;
    let status = fixture
        .mutate(|tree| {
            tree.interrupt_agent(
                "root",
                &crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: fixture.child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap()
        })
        .await;
    assert_eq!(status, NodeStatus::Interrupted);
    drop(held);
    fixture
        .until("the park to end", |tree, child| {
            !tree.has_turn_owner(child) && !tree.is_parked(child)
        })
        .await;
    let bound = fixture
        .runtime
        .read(|tree| tree.attempt(&fixture.child).unwrap().turn_id.clone())
        .unwrap();
    let sent = fixture
        .mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    refresh: None,
                    agent_id: fixture.child.clone(),
                    message: "resume".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
        })
        .await;
    fixture
        .registry
        .stop_all(Duration::from_millis(250))
        .await
        .unwrap();
    assert_eq!(
        bound, None,
        "the interrupted child still binds the turn that already ended"
    );
    assert_eq!(
        sent.disposition,
        crate::agent_tree::MailDisposition::Started,
        "the resume mail must start a fresh turn on the interrupted attempt"
    );
}

/// An interrupt that lands after a wake began but before its Start is
/// admitted ends the park instead of the attempt: nothing starts, the
/// batch the wake prepared stays unread, the attempt stays open, and the
/// next mail resumes the child (spec 03 §9).
#[tokio::test]
async fn an_interrupt_racing_the_wake_admission_keeps_the_attempt() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let runtime = Arc::clone(&fixture.runtime);
    let child = fixture.child.clone();
    hooks::set_before_wake_admission(
        &fixture.child,
        Arc::new(move || {
            let (runtime, child) = (Arc::clone(&runtime), child.clone());
            Box::pin(async move {
                runtime
                    .mutate_then_emit(false, {
                        let child = child.clone();
                        move |tree| {
                            tree.interrupt_agent(
                                "root",
                                &crate::agent_tree_protocol::AttemptScopedRequest {
                                    agent_id: child.clone(),
                                    expected_attempt_id: None,
                                },
                                None,
                            )
                            .map(|_| ())
                            .map_err(|error| error.message)
                        }
                    })
                    .await
                    .unwrap();
                runtime.cancel_live(&child).await.unwrap();
            })
        }),
    );
    parent_mail(&fixture, "status?").await;
    fixture
        .until("the wake batch to be released", |tree, child| {
            tree.commands().values().any(|command| {
                command.recipient == child
                    && command.kind == crate::agent_tree::AgentCommandKind::Wake
                    && command.withdrawn
            })
        })
        .await;
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(
                tree.node(&fixture.child).unwrap().status,
                NodeStatus::Interrupted
            );
            assert_eq!(attempt.status, AttemptStatus::Running);
            assert_eq!(attempt.turn_id, None);
            assert_eq!(attempt.usage.turns, 1, "no wake turn was admitted");
            assert_eq!(
                tree.wake_mail(&fixture.child).len(),
                1,
                "the batch stays unread"
            );
        })
        .unwrap();
    assert!(
        crate::agent::ledger::prepared_commands(fixture.runtime.driver(), &fixture.child)
            .unwrap()
            .is_empty(),
        "the released wake batch blocks no later command"
    );
    assert_eq!(
        fixture.agent.jobs()[0].status,
        plakous::tools::JobStatus::Running,
        "an interrupt keeps the child's processes"
    );
    let resumed = fixture
        .mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    refresh: None,
                    agent_id: fixture.child.clone(),
                    message: "resume".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
        })
        .await;
    assert_eq!(
        resumed.disposition,
        crate::agent_tree::MailDisposition::Started
    );
    fixture
        .registry
        .stop_all(Duration::from_millis(250))
        .await
        .unwrap();
}

/// While a wake that was not admitted ends the attempt, the child is
/// marked ending: new mail is refused as terminal rather than started,
/// an interrupt no longer applies, and a Start that reached the live
/// session is not joined to a turn the tree no longer binds.
#[tokio::test]
async fn mail_during_a_failed_wake_end_is_refused_not_started() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    // A result recorded while parked: the next wake is refused, as for a
    // spent or terminal attempt.
    fixture.submit().await;
    let observed = Arc::new(std::sync::Mutex::new(None));
    let runtime = Arc::clone(&fixture.runtime);
    let child = fixture.child.clone();
    let seen = Arc::clone(&observed);
    hooks::set_before_failed_wake_end(
        &fixture.child,
        Arc::new(move || {
            let (runtime, child, seen) = (Arc::clone(&runtime), child.clone(), Arc::clone(&seen));
            Box::pin(async move {
                let (sent, status) = runtime
                    .mutate_then_emit(false, {
                        let child = child.clone();
                        move |tree| {
                            let sent = Some(
                                tree.send_agent_message(
                                    "root",
                                    &SendAgentMessageRequest {
                                        refresh: None,
                                        agent_id: child.clone(),
                                        message: "one more thing".into(),
                                        purpose: None,
                                        priority: None,
                                        in_reply_to: None,
                                        output_schema_json: None,
                                    },
                                    None,
                                )
                                .map_err(|error| error.code),
                            );
                            let status = tree
                                .interrupt_agent(
                                    "root",
                                    &crate::agent_tree_protocol::AttemptScopedRequest {
                                        agent_id: child.clone(),
                                        expected_attempt_id: None,
                                    },
                                    None,
                                )
                                .ok();
                            Ok((sent, status))
                        }
                    })
                    .await
                    .unwrap();
                let (locator, path) = runtime
                    .read(|tree| {
                        let node = tree.node(&child).unwrap();
                        (
                            node.session_locator.clone().unwrap(),
                            node.path.display().to_string(),
                        )
                    })
                    .unwrap();
                let deliverer = super::super::children::ProductMailDeliverer {
                    parent: crate::agent::SpawnParent::Api(
                        super::super::tests::openrouter_test_parent(),
                    ),
                    runtime: Arc::clone(&runtime),
                };
                let started = crate::agent::children::ProductMailDeliverer::deliver(
                    &deliverer,
                    crate::agent_tree_tools::MailDeliveryRequest {
                        agent_id: child.clone(),
                        session_locator: locator,
                        disposition: crate::agent_tree::MailDisposition::Started,
                        message_id: "m-late".into(),
                        body: "late".into(),
                        description: String::new(),
                        prompt: String::new(),
                        output_schema_json: String::new(),
                        path,
                        turn_token: 1,
                    },
                )
                .await;
                *seen.lock().unwrap() = Some((sent, status, started));
            })
        }),
    );
    parent_mail(&fixture, "status?").await;
    // The ending wake unparks the child after the attempt settles.
    fixture
        .until("the attempt to settle unparked", |tree, child| {
            settled(tree, child) && !tree.is_parked(child)
        })
        .await;
    let (sent, status, started) = observed.lock().unwrap().take().unwrap();
    assert_eq!(
        sent.unwrap().unwrap_err(),
        crate::agent_tree_protocol::TopologyErrorCode::RecipientAttemptTerminal
    );
    assert_eq!(
        status,
        Some(NodeStatus::Running),
        "an ending child is not interrupted"
    );
    assert_eq!(
        started.unwrap_err(),
        crate::agent_tree_tools::MailDeliveryFailure::TurnEnded
    );
    fixture
        .runtime
        .read(|tree| {
            assert_eq!(
                tree.attempt(&fixture.child).unwrap().status,
                AttemptStatus::Completed
            );
            assert!(!tree.is_parked(&fixture.child));
            // The mail that waited for the wake is settled and named,
            // not left unread on an attempt nothing will wake.
            assert_eq!(
                tree.wake_mail(&fixture.child),
                [] as [plakous_recursive_subagents::tree::MailRecord; 0]
            );
            let fault = tree.fault_reason(&fixture.child).unwrap_or_default();
            assert!(fault.contains("was not delivered"), "{fault}");
        })
        .unwrap();
}

/// Delivers `message_id` to the fixture's child as a Started mail, as
/// a send to a child with no turn does.
async fn deliver_started(
    fixture: &Fixture,
    message_id: &str,
) -> Result<crate::agent_tree_tools::MailDeliveryAck, crate::agent_tree_tools::MailDeliveryFailure>
{
    let (locator, path) = fixture
        .runtime
        .read(|tree| {
            let node = tree.node(&fixture.child).unwrap();
            (
                node.session_locator.clone().unwrap(),
                node.path.display().to_string(),
            )
        })
        .unwrap();
    let deliverer = super::super::children::ProductMailDeliverer {
        parent: crate::agent::SpawnParent::Api(super::super::tests::openrouter_test_parent()),
        runtime: Arc::clone(&fixture.runtime),
    };
    crate::agent::children::ProductMailDeliverer::deliver(
        &deliverer,
        crate::agent_tree_tools::MailDeliveryRequest {
            agent_id: fixture.child.clone(),
            session_locator: locator,
            disposition: crate::agent_tree::MailDisposition::Started,
            message_id: message_id.into(),
            body: "use the v2 schema".into(),
            description: String::new(),
            prompt: String::new(),
            output_schema_json: String::new(),
            path,
            turn_token: 1,
        },
    )
    .await
}

/// A Started mail never joins a turn another command admitted as a
/// Start: it would be acknowledged, and its mail settled, without ever
/// reaching that turn. The send hears `TurnEnded` and delivers the mail
/// into that turn as a follow-up.
#[tokio::test]
async fn a_started_mail_never_joins_a_turn_another_command_admitted() {
    let fixture = fixture(json!([held("working")]), leaf).await;
    let delivered = deliver_started(&fixture, "m-late-start").await;
    assert_eq!(
        delivered.map(|ack| ack.turn_id),
        Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded),
        "a Started mail was acknowledged by a turn its command did not admit"
    );
    fixture.submit().await;
    fixture.until("the attempt to settle", settled).await;
}

/// A Started mail that reaches an interrupted child while another Start
/// holds its live slot answers `SlotBusy`, so the send retries once the
/// slot is vacant, instead of reopening the session and faulting the
/// node when its own Start finds the slot taken.
#[tokio::test]
async fn a_mail_start_on_a_starting_slot_is_retried_not_reopened() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    fixture
        .mutate(|tree| {
            tree.interrupt_agent(
                "root",
                &crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: fixture.child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap()
        })
        .await;
    fixture.runtime.cancel_live(&fixture.child).await.unwrap();
    fixture
        .until("the interrupted turn to unwind", |tree, child| {
            !tree.is_parked(child)
                && tree
                    .attempt(child)
                    .is_some_and(|attempt| attempt.turn_id.is_none())
        })
        .await;
    let startup = fixture
        .runtime
        .reserve_startup(&fixture.child)
        .await
        .unwrap();
    let delivered = deliver_started(&fixture, "m-resume").await;
    let fault = fixture
        .runtime
        .read(|tree| tree.fault_reason(&fixture.child).map(str::to_owned))
        .unwrap();
    fixture
        .registry
        .stop_all(Duration::from_millis(250))
        .await
        .unwrap();
    assert_eq!(
        delivered.map(|ack| ack.turn_id),
        Err(crate::agent_tree_tools::MailDeliveryFailure::SlotBusy)
    );
    assert_eq!(fault, None, "the starting slot faulted the node");
    drop(startup);
}

/// A Start whose reservation finds the child's slot held by another
/// Start (taken while a mail Start reopened the session) is refused as
/// `SlotOccupied`: its attachment is released and the child is not
/// faulted, so the mail Start answers `SlotBusy` and its send retries.
#[tokio::test]
async fn a_start_whose_slot_is_taken_before_its_reservation_does_not_fault_the_child() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let startup = fixture
        .runtime
        .reserve_startup(&fixture.child)
        .await
        .unwrap();
    let started = super::super::children::start_child_turn(
        &fixture.runtime,
        &fixture.child,
        attached,
        CommandId::new("plak:mail:m-taken:start").unwrap(),
        crate::agent::children::ChildCommand::fresh("assignment".to_owned(), CommandKind::Start),
    )
    .await;
    let (fault, status) = fixture
        .runtime
        .read(|tree| {
            (
                tree.fault_reason(&fixture.child).map(str::to_owned),
                tree.node(&fixture.child).unwrap().status,
            )
        })
        .unwrap();
    assert!(
        matches!(
            started,
            Err(super::super::children::ChildStartFailure::SlotOccupied(_))
        ),
        "the taken slot was not a distinct refusal"
    );
    assert_eq!(fault, None, "the taken slot faulted the node");
    assert_ne!(status, NodeStatus::RecoveryRequired);
    drop(startup);
}

/// Mail whose live delivery is still in flight when the turn ends (it
/// targeted the turn that just ended) parks the child instead of nudging
/// it, and the wake runs once the delivery returns, carrying the mail.
#[tokio::test]
async fn mail_claimed_at_turn_end_parks_and_wakes_when_its_delivery_returns() {
    let fixture = fixture(json!([held("thinking"), held("woke")]), leaf).await;
    parent_mail(&fixture, "use the v2 schema").await;
    let message = fixture
        .mutate(|tree| {
            let mail = tree.wake_mail(&fixture.child).pop().unwrap();
            tree.set_delivery_claim_for_test(&mail.message_id, true);
            mail.message_id
        })
        .await;
    fixture.provider_gates[0].release();
    fixture.until("the park", parked).await;
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.usage.turns, 1);
            assert_eq!(attempt.submit_harness_nudge.rounds(), 0);
        })
        .unwrap();
    // The delivery found the turn ended and left the mail unread.
    fixture
        .mutate(|tree| tree.set_delivery_claim_for_test(&message, false))
        .await;
    fixture.until("the wake turn", turns(2)).await;
    assert_eq!(
        fixture.wake_prompts(),
        [format!(
            "{}\n\nMessage:\nuse the v2 schema",
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT
        )]
    );
    fixture.submit().await;
    fixture.until("the attempt to settle", settled).await;
    assert_eq!(
        fixture
            .runtime
            .read(|tree| tree
                .attempt(&fixture.child)
                .unwrap()
                .submit_harness_nudge
                .rounds())
            .unwrap(),
        0
    );
}

/// A mail Start the lost process admitted before it bound its turn
/// rejoins after the child reopen (spec 03 section 14.1): the reopen's
/// result reconcile leaves its command prepared, so the rejoin finds it,
/// binds the turn and acknowledges the mail it carried.
#[tokio::test]
async fn an_admitted_mail_start_rejoins_after_the_reopen_reconcile() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("on it")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let sent = fixture
        .mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    refresh: None,
                    agent_id: fixture.child.clone(),
                    message: "use the v2 schema".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
        })
        .await;
    assert_eq!(
        sent.disposition,
        crate::agent_tree::MailDisposition::Started
    );
    let command = format!("plak:mail:{}:start", sent.message_id);
    let context = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::MailStart,
            base_text: "assignment\n\nMessage:\nuse the v2 schema",
            batch: BatchRule::MailStart(&sent.message_id),
        })
        .await
        .unwrap();
    // The lost process admitted the Start and died before binding its turn.
    let submission = attached
        .agent()
        .unwrap()
        .start(
            CommandId::new(command.clone()).unwrap(),
            context.prompt.clone(),
        )
        .await
        .unwrap();
    let control = submission.turn().unwrap().control();
    let result = submission.result().unwrap().clone();
    let turn = result.turn_id();
    let rejoined = rejoin_held(&fixture, attached, control, result, async |_| {}).await;
    assert_eq!(rejoined, Reopened::Done);
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.turn_id, Some(turn.to_string()));
            assert_eq!(attempt.usage.turns, 2);
            assert!(!tree.is_parked(&fixture.child));
            assert!(
                tree.wake_mail(&fixture.child)
                    .iter()
                    .all(|mail| mail.message_id != sent.message_id),
                "the rejoined Start acknowledged the mail it carried"
            );
        })
        .unwrap();
    assert!(
        crate::agent::ledger::prepared_commands(fixture.runtime.driver(), &fixture.child)
            .unwrap()
            .is_empty()
    );
    fixture.submit().await;
    fixture.until("the attempt to settle", settled).await;
}

/// A mail Start whose reopen recovers a turn another command admitted
/// (here another mail's Start the lost process journaled) never claims
/// that turn: the turn binds and goes live with its own batch settled,
/// and the delivery answers `TurnEnded`, so the send follows up into it
/// and the mail is not acknowledged by a turn that did not carry it.
#[tokio::test]
async fn a_mail_start_never_claims_a_recovered_turn_another_command_admitted() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("on it")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let send = async |text: &str| {
        fixture
            .mutate(|tree| {
                tree.send_agent_message(
                    "root",
                    &SendAgentMessageRequest {
                        refresh: None,
                        agent_id: fixture.child.clone(),
                        message: text.into(),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap()
            })
            .await
    };
    let journaled = send("use the v2 schema").await;
    let command = format!("plak:mail:{}:start", journaled.message_id);
    let context = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::MailStart,
            base_text: "assignment\n\nMessage:\nuse the v2 schema",
            batch: BatchRule::MailStart(&journaled.message_id),
        })
        .await
        .unwrap();
    // The lost process admitted that Start and died before binding it.
    let submission = attached
        .agent()
        .unwrap()
        .start(CommandId::new(command).unwrap(), context.prompt.clone())
        .await
        .unwrap();
    let control = submission.turn().unwrap().control();
    let result = submission.result().unwrap().clone();
    let turn = result.turn_id();
    let late = send("keep the old field too").await;
    assert_eq!(
        late.disposition,
        crate::agent_tree::MailDisposition::Started
    );
    super::super::children::reconcile_reopened_results(
        &fixture.runtime,
        &fixture.child,
        &attached,
        Some(turn),
    )
    .await
    .unwrap();
    let rejoined = super::super::children::rejoin_recovered_start(
        &fixture.runtime,
        &fixture.child,
        &late.message_id,
        attached,
        turn,
        control,
        result,
    )
    .await;
    assert_eq!(
        rejoined.map(|ack| ack.turn_id),
        Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded),
        "the late mail's Start was acknowledged by a turn its command did not admit"
    );
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.turn_id, Some(turn.to_string()));
            assert!(tree.node(&fixture.child).unwrap().active_turn);
            let unread: Vec<_> = tree
                .wake_mail(&fixture.child)
                .into_iter()
                .map(|mail| mail.message_id)
                .collect();
            assert_eq!(unread, std::slice::from_ref(&late.message_id));
        })
        .unwrap();
    fixture.submit().await;
    fixture.until("the attempt to settle", settled).await;
}

/// A recovered turn binds to the attempt before its waiter can observe
/// it: a turn that ends while the rejoin is still returning settles as
/// the bound turn, and the rejoin never marks the ended turn active
/// again (spec 03 section 14.1).
#[tokio::test]
async fn a_recovered_turn_that_ends_before_the_rejoin_returns_stays_ended() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("on it")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let send = async |text: &str| {
        fixture
            .mutate(|tree| {
                tree.send_agent_message(
                    "root",
                    &SendAgentMessageRequest {
                        refresh: None,
                        agent_id: fixture.child.clone(),
                        message: text.into(),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap()
            })
            .await
    };
    let journaled = send("use the v2 schema").await;
    let command = format!("plak:mail:{}:start", journaled.message_id);
    let context = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::MailStart,
            base_text: "assignment\n\nMessage:\nuse the v2 schema",
            batch: BatchRule::MailStart(&journaled.message_id),
        })
        .await
        .unwrap();
    let submission = attached
        .agent()
        .unwrap()
        .start(CommandId::new(command).unwrap(), context.prompt.clone())
        .await
        .unwrap();
    let control = submission.turn().unwrap().control();
    let result = submission.result().unwrap().clone();
    let turn = result.turn_id();
    let late = send("keep the old field too").await;
    super::super::children::reconcile_reopened_results(
        &fixture.runtime,
        &fixture.child,
        &attached,
        Some(turn),
    )
    .await
    .unwrap();
    // The turn ends, and its waiter settles it, before the rejoin returns.
    let (runtime, child) = (Arc::clone(&fixture.runtime), fixture.child.clone());
    hooks::set_after_recovered_occupy(
        &fixture.child,
        Arc::new(move || {
            let (runtime, child) = (Arc::clone(&runtime), child.clone());
            Box::pin(async move {
                runtime
                    .mutate_then_emit(false, {
                        let child = child.clone();
                        move |tree| {
                            tree.submit_result(
                                &child,
                                &SubmitResultRequest {
                                    turn_token: 1,
                                    output_json: r#"{"ok":true}"#.to_owned(),
                                },
                                None,
                            )
                            .map(|_| ())
                            .map_err(|error| error.message)
                        }
                    })
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(30), async {
                    while !runtime.read(|tree| settled(tree, &child)).unwrap() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("the recovered turn settles");
            })
        }),
    );
    let rejoined = super::super::children::rejoin_recovered_start(
        &fixture.runtime,
        &fixture.child,
        &late.message_id,
        attached,
        turn,
        control,
        result,
    )
    .await;
    assert_eq!(
        rejoined.map(|ack| ack.turn_id),
        Err(crate::agent_tree_tools::MailDeliveryFailure::TurnEnded),
        "the rejoin faulted on a turn that ended before it returned"
    );
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert!(attempt.status.is_stopped());
            assert_eq!(attempt.turn_id, None);
            assert!(
                !tree.node(&fixture.child).unwrap().active_turn,
                "the ended turn was marked active again"
            );
        })
        .unwrap();
}

/// A recovered mail-Start turn on an attempt that already binds another
/// turn is refused (`RecoveryRequired`): the rejoin never binds over that
/// turn, and the recovered turn is cancelled and released instead of
/// going live (spec 03 section 14.1).
#[tokio::test]
async fn a_recovered_turn_on_an_attempt_bound_to_another_turn_is_refused() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("on it")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let sent = fixture
        .mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    refresh: None,
                    agent_id: fixture.child.clone(),
                    message: "use the v2 schema".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .unwrap()
        })
        .await;
    let command = format!("plak:mail:{}:start", sent.message_id);
    let context = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::MailStart,
            base_text: "assignment\n\nMessage:\nuse the v2 schema",
            batch: BatchRule::MailStart(&sent.message_id),
        })
        .await
        .unwrap();
    let submission = attached
        .agent()
        .unwrap()
        .start(CommandId::new(command).unwrap(), context.prompt.clone())
        .await
        .unwrap();
    let control = submission.turn().unwrap().control();
    let result = submission.result().unwrap().clone();
    let turn = result.turn_id();
    super::super::children::reconcile_reopened_results(
        &fixture.runtime,
        &fixture.child,
        &attached,
        Some(turn),
    )
    .await
    .unwrap();
    let other = "00000000-0000-0000-0000-0000000000b2";
    fixture
        .mutate(|tree| {
            tree.bind_attempt_turn(&fixture.child, other).unwrap();
            tree.set_active_turn(&fixture.child, true).unwrap();
        })
        .await;
    let rejoined = super::super::children::rejoin_recovered_start(
        &fixture.runtime,
        &fixture.child,
        &sent.message_id,
        attached,
        turn,
        control,
        result,
    )
    .await;
    assert!(
        matches!(
            rejoined,
            Err(crate::agent_tree_tools::MailDeliveryFailure::RecoveryRequired(_))
        ),
        "{rejoined:?}"
    );
    fixture
        .runtime
        .read(|tree| {
            assert_eq!(
                tree.attempt(&fixture.child).unwrap().turn_id.as_deref(),
                Some(other),
                "the rejoin bound over the attempt's turn"
            );
        })
        .unwrap();
    assert!(
        !fixture
            .runtime
            .live
            .lock()
            .await
            .contains_key(&fixture.child),
        "the refused turn went live"
    );
}

/// The lost process's wake carrying mail, admitted before it bound,
/// rejoins the same way.
#[tokio::test]
async fn an_admitted_wake_with_mail_rejoins_after_the_reopen_reconcile() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    parent_mail(&fixture, "status?").await;
    let command = fixture
        .runtime
        .read(|tree| wake_command(tree, &fixture.child))
        .unwrap()
        .unwrap();
    let context = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::Wake,
            base_text: crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
            batch: BatchRule::WakeAll,
        })
        .await
        .unwrap();
    assert!(context.prompt.ends_with("\n\nMessage:\nstatus?"));
    let submission = attached
        .agent()
        .unwrap()
        .command(plakous::agent::TurnCommand::StartInstruction {
            command_id: CommandId::new(command).unwrap(),
            text: context.prompt.clone(),
        })
        .await
        .unwrap();
    let control = submission.turn().unwrap().control();
    let result = submission.result().unwrap().clone();
    let rejoined = rejoin_held(&fixture, attached, control, result, async |_| {}).await;
    assert_eq!(rejoined, Reopened::Done);
    fixture.until("the rejoined wake", turns(2)).await;
    assert!(
        fixture
            .runtime
            .read(|tree| tree.wake_mail(&fixture.child).is_empty())
            .unwrap(),
        "the rejoined wake acknowledged its batch"
    );
    fixture.submit().await;
    fixture.until("the attempt to settle", settled).await;
}

/// An interrupt that ends a held child's hold before its reopen rejoin
/// wins: the recovered turn is cancelled instead of bound and the node
/// stays `interrupted`. The runtime admitted that turn, so the batch it
/// carried is in history and settles accepted: the resume does not
/// deliver it again.
#[tokio::test]
async fn an_interrupt_of_a_held_child_cancels_its_rejoined_turn() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    parent_mail(&fixture, "status?").await;
    let command = fixture
        .runtime
        .read(|tree| wake_command(tree, &fixture.child))
        .unwrap()
        .unwrap();
    let context = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::Wake,
            base_text: crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
            batch: BatchRule::WakeAll,
        })
        .await
        .unwrap();
    let submission = attached
        .agent()
        .unwrap()
        .command(plakous::agent::TurnCommand::StartInstruction {
            command_id: CommandId::new(command).unwrap(),
            text: context.prompt.clone(),
        })
        .await
        .unwrap();
    let control = submission.turn().unwrap().control();
    let result = submission.result().unwrap().clone();
    let rejoined = rejoin_held(&fixture, attached, control, result, async |fixture| {
        let status = fixture
            .mutate(|tree| {
                tree.interrupt_agent(
                    "root",
                    &crate::agent_tree_protocol::AttemptScopedRequest {
                        agent_id: fixture.child.clone(),
                        expected_attempt_id: None,
                    },
                    None,
                )
                .unwrap()
            })
            .await;
        assert_eq!(status, NodeStatus::Interrupted);
    })
    .await;
    assert_eq!(rejoined, Reopened::HoldEnded);
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(
                tree.node(&fixture.child).unwrap().status,
                NodeStatus::Interrupted
            );
            assert_eq!(attempt.status, AttemptStatus::Running);
            assert_eq!(attempt.turn_id, None);
            assert_eq!(
                attempt.usage.turns, 2,
                "an admitted turn is never refunded when registration loses the hold"
            );
            assert!(
                tree.wake_mail(&fixture.child).is_empty(),
                "the cancelled rejoin's batch is still unread, so the resume would \
                     deliver mail its admitted turn already carried"
            );
        })
        .unwrap();
    assert!(
        !fixture
            .runtime
            .live
            .lock()
            .await
            .contains_key(&fixture.child)
    );
    assert!(
        crate::agent::ledger::prepared_commands(fixture.runtime.driver(), &fixture.child)
            .unwrap()
            .is_empty(),
        "the released batch blocks no later command"
    );
}

/// A fact committed after a local withdrawal still admits the wake. Its
/// waiter remains live and its batch settles from the feed exactly once.
#[tokio::test]
async fn a_wake_fact_outranks_a_pre_admission_withdrawal() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let runtime = Arc::clone(&fixture.runtime);
    let child = fixture.child.clone();
    hooks::set_before_wake_admission(
        &fixture.child,
        Arc::new(move || {
            let runtime = Arc::clone(&runtime);
            let child = child.clone();
            Box::pin(async move {
                let command = runtime
                    .read(|tree| wake_command(tree, &child))
                    .unwrap()
                    .unwrap();
                ChildLedger::new(runtime.driver(), &child)
                    .prepare(CommandIntent {
                        command_id: &command,
                        kind: CommandKind::Wake,
                        base_text: crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
                        batch: BatchRule::WakeAll,
                    })
                    .await
                    .unwrap();
                ChildLedger::settlement_only(runtime.driver())
                    .settle(
                        &command,
                        crate::driver::ledger::Settlement::Refused { pause: None },
                    )
                    .await
                    .map_err(String::from)
                    .unwrap();
            })
        }),
    );
    parent_mail(&fixture, "status?").await;
    fixture.until("the admitted wake", turns(2)).await;
    fixture
        .runtime
        .read(|tree| {
            assert!(tree.has_turn_owner(&fixture.child));
            assert!(
                tree.commands()
                    .values()
                    .any(|command| command.recipient == fixture.child
                        && command.kind == crate::agent_tree::AgentCommandKind::Wake
                        && command.withdrawn
                        && command.fact == Some(crate::agent_tree::CommandFact::Accepted))
            );
        })
        .unwrap();
    fixture.submit().await;
    fixture.until("the attempt to settle", settled).await;
}

/// A turn waiter aborted by runtime shutdown (its task dropped mid-turn,
/// never polled to completion) still leaves its registration: the
/// registration's `Drop` runs with the task and the count does not leak.
#[test]
fn an_aborted_turn_waiter_leaves_its_registration() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let fixture = rt.block_on(fixture(json!([held("working")]), leaf));
    let registered = fixture
        .runtime
        .read(|tree| tree.has_turn_waiter(&fixture.child))
        .unwrap();
    rt.shutdown_timeout(Duration::from_secs(5));
    // The dropped registration leaves through a writer of its own.
    // A liveness bound only: generous, so load never trips it.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let after = loop {
        let after = fixture
            .runtime
            .read(|tree| tree.has_turn_waiter(&fixture.child))
            .unwrap();
        if !after || std::time::Instant::now() > deadline {
            break after;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Session handles may need a reactor to drop.
    std::mem::forget(fixture);
    assert!(registered, "the waiter registered");
    assert!(!after, "the aborted waiter's registration leaked");
}
