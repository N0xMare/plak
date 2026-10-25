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
use crate::agent::live::ProductLiveChild;
use crate::agent_tree::ProductAgentTree;
use crate::agent_tree_protocol::{
    AttemptStatus, NodeStatus, SendAgentMessageRequest, SpawnAgentRequest, SubmitResultRequest,
    TemplateName,
};
use crate::topology_runtime::ProductTopologyRuntime;

struct Fixture {
    provider_gates: Vec<plakous_offline_crash_harness::gate::GateHandle>,
    root: tempfile::TempDir,
    runtime: Arc<ProductTopologyRuntime>,
    agent: plakous::agent::Agent,
    registry: Arc<crate::jobs::JobRegistry>,
    sessions: Arc<AgentSessions>,
    binding: Box<dyn Fn() -> plakous::sessions::SessionRuntimeBinding + Send + Sync>,
    child: String,
}

impl Fixture {
    async fn until(&self, what: &str, done: impl Fn(&ProductAgentTree, &str) -> bool) {
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

    fn mutate<R>(&self, work: impl FnOnce(&mut ProductAgentTree) -> R) -> R {
        let mut out = None;
        self.runtime
            .mutate_then_emit(false, |tree| {
                out = Some(work(tree));
                Ok(())
            })
            .unwrap();
        out.unwrap()
    }

    fn submit(&self) {
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
        });
        for gate in &self.provider_gates {
            gate.release();
        }
    }
}

fn parked(tree: &ProductAgentTree, child: &str) -> bool {
    tree.waiting_on(child).is_some()
}

fn turns(count: u32) -> impl Fn(&ProductAgentTree, &str) -> bool {
    // Acceptance is durable; the projected live turn can already be gone
    // when a loaded test next polls, even though the wake ran successfully.
    move |tree, child| {
        tree.attempt(child)
            .is_some_and(|attempt| attempt.usage.turns >= count)
    }
}

fn settled(tree: &ProductAgentTree, child: &str) -> bool {
    tree.attempt(child)
        .is_some_and(|attempt| attempt.status.is_stopped())
}

/// One child with a real scripted session whose first turn is admitted
/// and handed to the production turn waiter, exactly as a spawn does.
async fn fixture(
    steps: serde_json::Value,
    setup: impl FnOnce(&mut ProductAgentTree) -> String,
) -> Fixture {
    fixture_with_probe(steps, setup, None).await
}

async fn fixture_with_probe(
    mut steps: serde_json::Value,
    setup: impl FnOnce(&mut ProductAgentTree) -> String,
    probe: Option<plakous::model::ReachabilityProbe>,
) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        crate::topology_runtime::test_support::bootstrap("park-wake", root.path().to_owned()),
        crate::topology_runtime::test_support::memory_store(),
        Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let mut child = None;
    runtime
        .mutate_then_emit(false, |tree| {
            child = Some(setup(tree));
            Ok(())
        })
        .unwrap();
    let child = child.unwrap();
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
    let plan: Arc<plakous_offline_crash_harness::script::ScriptPlan> = Arc::new(
        serde_json::from_value(json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "process_tools": true,
            "steps": steps,
        }))
        .unwrap(),
    );
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
            binding.unwrap().with_job_host(registry.clone())
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
        .mutate_then_emit(false, |tree| {
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
        .unwrap();
    let agent = session.agent().clone();
    let registration =
        super::super::children::TurnWaiterRegistration::register(runtime.driver(), &child).unwrap();
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
        .mutate_then_emit(false, |tree| {
            initial_feed
                .apply(tree, &child)
                .map(|_| ())
                .map_err(|error| error.message)
        })
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
    runtime
        .try_occupy_live(
            child.clone(),
            Box::new(ProductLiveChild::new(attached, turn, control)),
        )
        .await
        .map_err(|_| "live slot refused")
        .unwrap();
    super::super::children::spawn_topology_turn_waiter(
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

fn start_job(command: &str) -> serde_json::Value {
    json!({"output": {"tool": {"name": "process_session_start", "arguments": {
        "command": command, "workdir": ".", "login": false, "tty": false,
        "keep_stdin_open": true, "yield_time_ms": 0, "max_output_bytes": 8192
    }}}, "finish": "tool_calls"})
}

fn leaf(tree: &mut ProductAgentTree) -> String {
    tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id
}

/// Hold the final stream until the test submits or cancels the model.
fn held(text: &str) -> serde_json::Value {
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
    ]), leaf, Some(probe)).await;
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
            assert!(command.text.contains("Do not repeat completed actions"));
        })
        .unwrap();
    fixture.submit();
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
async fn reopened_transient_resume_rederives_gate_and_exact_command_identity() {
    assert_reopened_transient_resume(false).await;
}

#[tokio::test]
async fn reopened_transient_resume_retries_prepared_command_without_withdrawal() {
    assert_reopened_transient_resume(true).await;
}

async fn assert_reopened_transient_resume(journaled: bool) {
    let route = if journaled {
        "offline.reopen.prepared"
    } else {
        "offline.reopen.fresh"
    };
    let entered = Arc::new(tokio::sync::Notify::new());
    let probe =
        plakous::model::ReachabilityProbe::new(plakous::model::RouteId::new(route).unwrap(), {
            let entered = entered.clone();
            move |_, _| {
                entered.notify_one();
                Box::pin(std::future::pending())
            }
        });
    let fixture = fixture_with_probe(json!([
        {"fault": {"at": "pre_stream", "kind": "transport", "phase": "Connect", "retryability": "Exhausted", "message": "retained original outage"}, "output": {"text": "unused"}}, held("reopened continuation")
    ]), leaf, Some(probe)).await;
    entered.notified().await;
    let attempt = fixture
        .runtime
        .read(|tree| tree.attempt(&fixture.child).unwrap().attempt_id.clone())
        .unwrap();
    let mut attached = lose_and_reopen(&fixture).await;
    attached.reachability_probe = Some(plakous::model::ReachabilityProbe::new(
        plakous::model::RouteId::new(route).unwrap(),
        |_, _| Box::pin(async { Ok(()) }),
    ));
    if journaled {
        let (turn, _) = fixture
            .runtime
            .read(|tree| {
                tree.pending_transient_resume(&fixture.child)
                    .unwrap()
                    .unwrap()
            })
            .unwrap();
        let locator = attached.locator();
        let plakous::sessions::feed::FeedTerminalOutcome::Terminal(terminal) = fixture
            .sessions
            .feed_terminal(plakous::sessions::feed::FeedTerminalRequest {
                session: locator.session_id(),
                branch: locator.branch_id(),
                checkpoint: locator.checkpoint_id(),
                turn_id: turn.parse().unwrap(),
            })
            .await
            .unwrap()
        else {
            panic!("canonical failure required")
        };
        let plakous::agent::TurnResultOutcome::Failed(error) = terminal.result().outcome() else {
            panic!("failed turn required")
        };
        let prompt = crate::child_prompts::TransientResume {
            failure: error.message(),
        }
        .render();
        ChildLedger::new(fixture.runtime.driver(), &fixture.child)
            .prepare(CommandIntent {
                command_id: &format!("plak:resume-transient:{}:{attempt}:1", fixture.child),
                kind: CommandKind::Resume,
                base_text: &prompt,
                batch: BatchRule::WakeAll,
            })
            .unwrap();
    }
    wake_held(&fixture, attached).await;
    fixture.provider_gates[0].entered().await;
    fixture
        .until("reopened continuation acceptance", turns(2))
        .await;
    fixture
        .runtime
        .read(|tree| {
            let command = tree
                .command(&format!(
                    "plak:resume-transient:{}:{attempt}:1",
                    fixture.child
                ))
                .unwrap();
            assert_eq!(command.kind, crate::agent_tree::AgentCommandKind::Resume);
            assert_eq!(command.fact, Some(crate::agent_tree::CommandFact::Accepted));
            assert!(command.text.contains("retained original outage"));
            assert_eq!(
                tree.transient_resume_count(&fixture.child, &attempt)
                    .unwrap(),
                1
            );
        })
        .unwrap();
    fixture.submit();
    fixture.provider_gates[0].release();
    fixture.until("reopened submission", settled).await;
}

#[tokio::test]
async fn reopened_transient_wait_interrupt_and_close_cancel_startup_without_dispatch() {
    for close in [false, true] {
        let route = if close {
            "offline.reopen.close"
        } else {
            "offline.reopen.interrupt"
        };
        let entered = Arc::new(tokio::sync::Notify::new());
        let probe =
            plakous::model::ReachabilityProbe::new(plakous::model::RouteId::new(route).unwrap(), {
                let entered = entered.clone();
                move |_, _| {
                    entered.notify_one();
                    Box::pin(std::future::pending())
                }
            });
        let fixture = fixture_with_probe(json!([
            {"fault": {"at": "pre_stream", "kind": "transport", "phase": "Connect", "retryability": "Exhausted"}, "output": {"text": "unused"}}, held("must not dispatch")
        ]), leaf, Some(probe.clone())).await;
        entered.notified().await;
        let mut attached = lose_and_reopen(&fixture).await;
        attached.reachability_probe = Some(probe);
        wake_held(&fixture, attached).await;
        entered.notified().await;
        assert!(
            fixture
                .runtime
                .live
                .lock()
                .await
                .contains_key(&fixture.child),
            "the pending gate has a hub-owned startup reservation"
        );
        fixture.mutate(|tree| {
            let request = crate::agent_tree_protocol::AttemptScopedRequest {
                agent_id: fixture.child.clone(),
                expected_attempt_id: None,
            };
            if close {
                tree.close_agent("root", &request, None).unwrap();
            } else {
                tree.interrupt_agent("root", &request, None).unwrap();
            }
        });
        fixture.runtime.cancel_live(&fixture.child).await.unwrap();
        fixture
            .runtime
            .read(|tree| {
                assert_eq!(
                    tree.node(&fixture.child).unwrap().status,
                    if close {
                        NodeStatus::Closed
                    } else {
                        NodeStatus::Interrupted
                    }
                );
                assert_eq!(tree.attempt(&fixture.child).unwrap().usage.turns, 1);
                assert!(!tree.commands().values().any(|command| command.kind
                    == crate::agent_tree::AgentCommandKind::Resume
                    && command.accepted_seq.is_some()));
                assert!(!tree.is_parked(&fixture.child));
            })
            .unwrap();
    }
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
    fixture.submit();
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
            let mut snapshot = tree.snapshot.clone();
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
    fixture.mutate(|tree| {
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
    });
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
    fixture.submit();
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
    parent_mail(&fixture, "also check the docs");
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
    let sent = fixture.mutate(|tree| {
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
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
    });
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
    let status = fixture.mutate(|tree| {
        tree.interrupt_agent(
            "root",
            &crate::agent_tree_protocol::AttemptScopedRequest {
                agent_id: fixture.child.clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap()
    });
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
            let mut binding = tree.snapshot.binding(&name).unwrap().clone();
            binding.max_turns = Some(2);
            tree.pin_template("review", binding);
            leaf(tree)
        },
    )
    .await;
    fixture.until("the park", parked).await;
    fixture.mutate(|tree| {
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
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
    });
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

/// The reopen wake as reattach runs it: after the resume held the child.
async fn wake_held(fixture: &Fixture, attached: AttachedCodingSession) {
    // Rebuild the allocation seed before resume takes its hold. The wake
    // preparation still owns the first retained feed/terminal pass.
    fixture.mutate(|tree| {
        tree.project_feed(
            &fixture.child,
            &attached.locator().session_id().to_string(),
            &[],
            &crate::agent_tree::TreeFeedReads::default(),
        )
        .unwrap()
    });
    fixture.mutate(|tree| tree.resume_tree().unwrap());
    assert_eq!(
        wake_reopened(&fixture.runtime, &fixture.child, attached)
            .await
            .unwrap(),
        Reopened::Done,
        "reopen state: {:?}",
        fixture
            .runtime
            .read(|tree| (
                tree.node(&fixture.child).unwrap().clone(),
                tree.attempt(&fixture.child).unwrap().clone(),
                tree.fault_reason(&fixture.child).map(str::to_owned),
                tree.commands().clone(),
            ))
            .unwrap(),
    );
}

/// A turn the lost process admitted before it bound, rejoined as
/// reattach rejoins it: the resume holds the child, the child reopen
/// reconciles its journal with the recovered turn unbound, and the
/// rejoin binds it. Returns what the rejoin did.
async fn rejoin_held(
    fixture: &Fixture,
    attached: AttachedCodingSession,
    control: plakous::agent::TurnControl,
    result: TurnResultHandle,
    before_rejoin: impl FnOnce(&Fixture),
) -> Reopened {
    let turn = result.turn_id();
    fixture.mutate(|tree| tree.resume_tree().unwrap());
    before_rejoin(fixture);
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

fn parent_mail(fixture: &Fixture, text: &str) {
    fixture.mutate(|tree| {
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
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
    });
}

#[tokio::test]
async fn a_reopen_wakes_a_parked_child_once_with_its_stopped_process() {
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
    assert!(
        attached
            .agent()
            .unwrap()
            .jobs()
            .iter()
            .all(|job| job.status != plakous::tools::JobStatus::Running),
        "the released process is a terminal notice for the wake"
    );
    wake_held(&fixture, attached).await;
    fixture.until("the wake turn", turns(2)).await;
    assert_eq!(
        fixture.wake_prompts(),
        [crate::child_prompts::PLAK_CHILD_WAKE_PROMPT]
    );
    assert!(
        fixture
            .runtime
            .read(|tree| tree.parked_reopen_targets().is_empty())
            .unwrap()
    );
    fixture.submit();
    fixture.until("the attempt to settle", settled).await;
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

/// A wake the lost process journaled but never got to the runtime is
/// retried under its own command and exact prompt: one Start, not a
/// second wake, and not `recovery_required`.
#[tokio::test]
async fn a_lost_wake_start_is_retried_exactly_after_a_reopen() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    parent_mail(&fixture, "status?");
    let lost = fixture
        .runtime
        .read(|tree| wake_command(tree, &fixture.child))
        .unwrap()
        .unwrap()
        .replace(":1:", ":7:");
    let prepared = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &lost,
            kind: CommandKind::Wake,
            base_text: crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
            batch: BatchRule::WakeAll,
        })
        .unwrap();
    wake_held(&fixture, attached).await;
    fixture.until("the wake turn", turns(2)).await;
    let wakes = fixture
        .runtime
        .read(|tree| {
            tree.commands()
                .values()
                .filter(|command| command.kind == crate::agent_tree::AgentCommandKind::Wake)
                .map(|command| {
                    (
                        command.command_id.clone(),
                        if command.withdrawn {
                            "withdrawn".to_owned()
                        } else {
                            format!("{:?}", command.fact)
                        },
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(wakes.len(), 1, "no second wake beside the journaled one");
    assert!(wakes[0].0.ends_with(&lost));
    assert_eq!(wakes[0].1, "Some(Accepted)");
    assert_eq!(fixture.wake_prompts(), [prepared.prompt]);
    fixture.submit();
    fixture.until("the attempt to settle", settled).await;
}

#[tokio::test]
async fn behind_retention_retry_survives_a_consumed_command_and_a_caught_up_cursor() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let first = fixture
        .runtime
        .read(|tree| wake_command(tree, &fixture.child))
        .unwrap()
        .unwrap();
    let stem = first.rsplit_once(':').unwrap().0;
    let ids = [
        format!("{stem}:7"),
        format!("{stem}:8"),
        format!("{stem}:9"),
    ];
    for id in &ids {
        ChildLedger::new(fixture.runtime.driver(), &fixture.child)
            .prepare(CommandIntent {
                command_id: id,
                kind: CommandKind::Wake,
                base_text: "exact lost wake",
                batch: BatchRule::WakeAll,
            })
            .unwrap();
    }
    fixture.mutate(ProductAgentTree::clear_feed_views);
    // Retain only the release/open suffix, not the assignment's facts.
    let connection = rusqlite::Connection::open(fixture.root.path().join("park.sqlite3")).unwrap();
    connection
        .execute_batch(
            "DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions);\
             UPDATE sessions SET feed_retained_from=feed_tail,\
             feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);",
        )
        .unwrap();
    let expected_commands =
        std::iter::once("park:assignment".to_owned()).chain(ids.iter().cloned());
    for expected in expected_commands {
        let ReopenedWake::Start(Some(command)) =
            prepare_reopened_wake(&fixture.runtime, &fixture.child, &attached)
                .await
                .unwrap()
        else {
            panic!("the unresolved command must be retried")
        };
        assert_eq!(command.command_id, expected);
        command.settle(fixture.runtime.driver(), true).unwrap();
        assert!(
            fixture
                .runtime
                .read(|tree| ids.iter().all(|id| !tree.command(id).unwrap().withdrawn))
                .unwrap()
        );
    }
    attached.release_opened().await.unwrap();
}

#[tokio::test]
async fn reopened_wake_consumes_the_whole_retention_queue_on_one_attachment() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}},
            {"output": {"text": "lost a"}}, {"output": {"text": "lost b"}},
            {"output": {"text": "lost c"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let base = fixture
        .runtime
        .read(|tree| wake_command(tree, &fixture.child))
        .unwrap()
        .unwrap();
    let stem = base.rsplit_once(':').unwrap().0;
    let ids = [base.clone(), format!("{stem}:2"), format!("{stem}:3")];
    for id in &ids {
        let context = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
            .prepare(CommandIntent {
                command_id: id,
                kind: CommandKind::Wake,
                base_text: "exact lost wake",
                batch: BatchRule::WakeAll,
            })
            .unwrap();
        let submission = attached
            .agent()
            .unwrap()
            .start_instruction(CommandId::new(id.clone()).unwrap(), context.prompt)
            .await
            .unwrap();
        submission.result().unwrap().clone().result().await.unwrap();
    }
    fixture.mutate(ProductAgentTree::clear_feed_views);
    let connection = rusqlite::Connection::open(fixture.root.path().join("park.sqlite3")).unwrap();
    connection
        .execute_batch(
            "DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions);\
             UPDATE sessions SET feed_retained_from=feed_tail,\
             feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);",
        )
        .unwrap();
    wake_held(&fixture, attached).await;
    fixture
        .until("the fresh wake after consumed commands", turns(1))
        .await;
    fixture
        .runtime
        .read(|tree| {
            for id in &ids {
                let intent = tree.command(id).unwrap();
                assert_eq!(intent.fact, Some(crate::agent_tree::CommandFact::Accepted));
                assert!(intent.turn_id.is_none());
                assert!(!intent.withdrawn);
            }
            assert_eq!(tree.attempt(&fixture.child).unwrap().usage.turns, 1);
        })
        .unwrap();
    fixture.submit();
    fixture.until("the attempt to settle", settled).await;
}

#[tokio::test]
async fn reopened_wake_consumes_a_reclaimed_command_without_binding_it() {
    let mut steps = vec![
        start_job("read line"),
        json!({"output": {"text": "started"}}),
    ];
    steps.extend((0..18).map(|_| json!({"output": {"text": "retained history"}})));
    steps.push(held("woke"));
    let fixture = fixture(serde_json::Value::Array(steps), leaf).await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let id = fixture
        .runtime
        .read(|tree| wake_command(tree, &fixture.child))
        .unwrap()
        .unwrap();
    let context = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &id,
            kind: CommandKind::Wake,
            base_text: "exact lost wake",
            batch: BatchRule::WakeAll,
        })
        .unwrap();
    attached
        .agent()
        .unwrap()
        .start_instruction(CommandId::new(id.clone()).unwrap(), context.prompt.clone())
        .await
        .unwrap()
        .result()
        .unwrap()
        .clone()
        .result()
        .await
        .unwrap();
    for round in 0..17 {
        attached
            .agent()
            .unwrap()
            .start_instruction(
                CommandId::new(format!("prune:{round}")).unwrap(),
                "untracked round",
            )
            .await
            .unwrap()
            .result()
            .unwrap()
            .clone()
            .result()
            .await
            .unwrap();
    }
    let locator = attached.release_opened().await.unwrap();
    assert!(matches!(
        fixture.sessions.prune_history(locator).await.unwrap(),
        plakous::sessions::PruneHistoryOutcome::Pruned(_)
    ));
    let plakous::sessions::OpenSessionOutcome::Opened { session, .. } = fixture
        .sessions
        .open(plakous::sessions::OpenSessionRequest::new(
            SessionOperationId::new("park:pruned:reopen").unwrap(),
            locator,
            (fixture.binding)(),
        ))
        .await
        .unwrap()
    else {
        panic!("pruned parked session must reopen");
    };
    let replay = session
        .agent()
        .command(plakous::agent::TurnCommand::StartInstruction {
            command_id: CommandId::new(id.clone()).unwrap(),
            text: context.prompt,
        })
        .await
        .unwrap_or_else(|error| panic!("reclaimed replay: {}", error.message()));
    assert_eq!(
        replay
            .acknowledgement()
            .rejection()
            .map(plakous::agent::ControlError::kind),
        Some(plakous::agent::ControlErrorKind::Reclaimed)
    );
    let attached = AttachedCodingSession {
        output_bound_rederived: None,
        reachability_probe: None,
        sessions: Arc::clone(&fixture.sessions),
        locator: session.locator(),
        opened: Some(session),
        recovered_turn: None,
        topology_owner: None,
    };
    fixture.mutate(ProductAgentTree::clear_feed_views);
    let connection = rusqlite::Connection::open(fixture.root.path().join("park.sqlite3")).unwrap();
    connection.execute_batch("DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions);\
            UPDATE sessions SET feed_retained_from=feed_tail, feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);").unwrap();
    wake_held(&fixture, attached).await;
    fixture
        .until("the fresh wake after the reclaimed command", turns(1))
        .await;
    fixture
        .runtime
        .read(|tree| {
            let command = tree.command(&id).unwrap();
            assert_eq!(command.fact, Some(crate::agent_tree::CommandFact::Accepted));
            assert!(command.turn_id.is_none());
            assert!(!command.withdrawn);
            assert_eq!(tree.attempt(&fixture.child).unwrap().usage.turns, 1);
        })
        .unwrap();
    fixture.submit();
    fixture.until("the attempt to settle", settled).await;
}

/// A corrective round the lost process wrote but never got to the
/// runtime is withdrawn, not retried: its round is re-derived from the
/// attempt's state, so nothing is spent for it and one fresh wake runs.
#[tokio::test]
async fn an_unrecorded_corrective_round_is_withdrawn_at_the_reopen_wake() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let (corrective, rounds) = fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            (
                format!(
                    "plak:submit-harness:{}:{}:{}:1",
                    fixture.child, tree.host_generation, attempt.attempt_id
                ),
                attempt.submit_harness_nudge.rounds(),
            )
        })
        .unwrap();
    ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &corrective,
            kind: CommandKind::Corrective,
            base_text: "submit your result",
            batch: BatchRule::Results,
        })
        .unwrap();
    wake_held(&fixture, attached).await;
    fixture.until("the wake turn", turns(2)).await;
    let (command, spent) = fixture
        .runtime
        .read(|tree| {
            (
                tree.command(&corrective).unwrap().clone(),
                tree.attempt(&fixture.child)
                    .unwrap()
                    .submit_harness_nudge
                    .rounds(),
            )
        })
        .unwrap();
    assert_eq!(
        command.kind,
        crate::agent_tree::AgentCommandKind::Corrective
    );
    assert!(command.withdrawn && command.fact.is_none(), "{command:?}");
    assert_eq!(spent, rounds, "the withdrawn round spends nothing");
    assert_eq!(
        fixture.wake_prompts(),
        [crate::child_prompts::PLAK_CHILD_WAKE_PROMPT.to_owned()],
        "one fresh wake, not the corrective text"
    );
    fixture.submit();
    fixture.until("the attempt to settle", settled).await;
}

/// A wait the lost process acknowledged mail through, but which never
/// reached the child's committed history, settles before the reopen wake
/// dispatches: the mail is unread again and that wake carries it.
#[tokio::test]
async fn a_reopen_settles_lost_await_receipts_before_the_wake() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    parent_mail(&fixture, "use the v2 schema");
    fixture.mutate(|tree| {
        let mail = tree.wake_mail(&fixture.child).pop().unwrap();
        tree.remember_success(
            "lost-wait",
            &fixture.child,
            "wait_agent",
            &json!({}),
            &json!({"output": {}, "acknowledged": [mail], "newly_delivered": []}),
        )
        .unwrap();
        tree.acknowledge_mail(&mail.message_id).unwrap();
        assert_eq!(
            tree.wake_mail(&fixture.child),
            [] as [plakous_recursive_subagents::tree::MailRecord; 0]
        );
    });
    wake_held(&fixture, attached).await;
    fixture.until("the wake turn", turns(2)).await;
    assert!(
        fixture
            .runtime
            .read(|tree| tree.retained_wait_receipts(&fixture.child).is_empty())
            .unwrap()
    );
    assert_eq!(
        fixture.wake_prompts(),
        [format!(
            "{}\n\nMessage:\nuse the v2 schema",
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT
        )]
    );
    fixture.submit();
    fixture.until("the attempt to settle", settled).await;
}

#[tokio::test]
async fn a_reopen_with_max_turns_spent_ends_the_attempt_blocked() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}]),
        |tree| {
            let name = TemplateName::parse("review").unwrap();
            let mut binding = tree.snapshot.binding(&name).unwrap().clone();
            binding.max_turns = Some(2);
            tree.pin_template("review", binding);
            leaf(tree)
        },
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    // A second turn the lost process bound and spent before it parked.
    fixture.mutate(|tree| {
        tree.bind_attempt_turn(&fixture.child, "00000000-0000-0000-0000-0000000000f2")
            .unwrap();
        tree.release_attempt_turn(&fixture.child).unwrap();
    });
    wake_held(&fixture, attached).await;
    fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.status, AttemptStatus::Blocked);
            assert_eq!(attempt.usage.turns, 2);
        })
        .unwrap();
    assert_eq!(fixture.wake_prompts(), [] as [String; 0]);
}

/// Mail that is unread when a turn ends wakes the child although nothing
/// it started still runs: an arrival is not a nudge.
#[tokio::test]
async fn an_arrival_without_live_work_wakes_instead_of_nudging() {
    let fixture = fixture(json!([held("thinking"), held("woke")]), leaf).await;
    parent_mail(&fixture, "use the v2 schema");
    fixture.provider_gates[0].release();
    fixture.until("the wake turn", turns(2)).await;
    assert_eq!(
        fixture.wake_prompts(),
        [format!(
            "{}\n\nMessage:\nuse the v2 schema",
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT
        )]
    );
    fixture.submit();
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

/// A mail Start the lost process journaled is retried exactly on reopen
/// (spec 03 §14.1) and acknowledges the mail its prompt carries, so the
/// child's next unsubmitted end does not wake it with the same message a
/// second time.
#[tokio::test]
async fn a_retried_mail_start_acknowledges_its_mail() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("on it")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    // The lost process accepted parent mail as a Start, journaled that
    // Start and died before the runtime recorded it.
    let sent = fixture.mutate(|tree| {
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
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
    });
    assert_eq!(
        sent.disposition,
        crate::agent_tree::MailDisposition::Started
    );
    let command = format!("plak:mail:{}:start", sent.message_id);
    ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::MailStart,
            base_text: "assignment\n\nMessage:\nuse the v2 schema",
            batch: BatchRule::MailStart(&sent.message_id),
        })
        .unwrap();
    wake_held(&fixture, attached).await;
    fixture.until("the retried Start", turns(2)).await;
    let unread = fixture
        .runtime
        .read(|tree| {
            tree.wake_mail(&fixture.child)
                .iter()
                .any(|mail| mail.message_id == sent.message_id)
        })
        .unwrap();
    fixture.submit();
    fixture.until("the attempt to settle", settled).await;
    assert!(
        !unread,
        "the retried mail Start delivered `use the v2 schema`, yet the mail is still \
             unread for the next wake to deliver again"
    );
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
    let status = fixture.mutate(|tree| {
        tree.interrupt_agent(
            "root",
            &crate::agent_tree_protocol::AttemptScopedRequest {
                agent_id: fixture.child.clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap()
    });
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
    let sent = fixture.mutate(|tree| {
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
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
    });
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
                    .mutate_then_emit(false, |tree| {
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
                    })
                    .unwrap();
                runtime.cancel_live(&child).await.unwrap();
            })
        }),
    );
    parent_mail(&fixture, "status?");
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
    let resumed = fixture.mutate(|tree| {
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
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
    });
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
    fixture.submit();
    let observed = Arc::new(std::sync::Mutex::new(None));
    let runtime = Arc::clone(&fixture.runtime);
    let child = fixture.child.clone();
    let seen = Arc::clone(&observed);
    hooks::set_before_failed_wake_end(
        &fixture.child,
        Arc::new(move || {
            let (runtime, child, seen) = (Arc::clone(&runtime), child.clone(), Arc::clone(&seen));
            Box::pin(async move {
                let mut sent = None;
                let mut status = None;
                runtime
                    .mutate_then_emit(false, |tree| {
                        sent = Some(
                            tree.send_agent_message(
                                "root",
                                &SendAgentMessageRequest {
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
                        status = tree
                            .interrupt_agent(
                                "root",
                                &crate::agent_tree_protocol::AttemptScopedRequest {
                                    agent_id: child.clone(),
                                    expected_attempt_id: None,
                                },
                                None,
                            )
                            .ok();
                        Ok(())
                    })
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
    parent_mail(&fixture, "status?");
    fixture.until("the attempt to settle", settled).await;
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
    fixture.submit();
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
    fixture.mutate(|tree| {
        tree.interrupt_agent(
            "root",
            &crate::agent_tree_protocol::AttemptScopedRequest {
                agent_id: fixture.child.clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap()
    });
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
    parent_mail(&fixture, "use the v2 schema");
    let message = fixture.mutate(|tree| {
        let mail = tree.wake_mail(&fixture.child).pop().unwrap();
        tree.set_delivery_claim_for_test(&mail.message_id, true);
        mail.message_id
    });
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
    fixture.mutate(|tree| tree.set_delivery_claim_for_test(&message, false));
    fixture.until("the wake turn", turns(2)).await;
    assert_eq!(
        fixture.wake_prompts(),
        [format!(
            "{}\n\nMessage:\nuse the v2 schema",
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT
        )]
    );
    fixture.submit();
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

/// A wake an earlier attempt journaled is never retried onto the
/// current attempt after a reopen; it settles, and a fresh wake runs.
#[tokio::test]
async fn a_reopen_does_not_retry_another_attempts_wake() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    let current = fixture
        .runtime
        .read(|tree| wake_command(tree, &fixture.child))
        .unwrap()
        .unwrap();
    let attempt = fixture
        .runtime
        .read(|tree| {
            tree.attempt(&fixture.child)
                .unwrap()
                .attempt_id
                .as_str()
                .to_owned()
        })
        .unwrap();
    assert_eq!(
        crate::agent::ledger::command_attempt(&current),
        Some(attempt.as_str())
    );
    let stale = current.replace(&format!(":{attempt}:"), ":c1-a0:");
    ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &stale,
            kind: CommandKind::Wake,
            base_text: crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
            batch: BatchRule::WakeAll,
        })
        .unwrap();
    wake_held(&fixture, attached).await;
    fixture.until("the wake turn", turns(2)).await;
    let wakes = fixture
        .runtime
        .read(|tree| {
            tree.commands()
                .values()
                .filter(|command| command.kind == crate::agent_tree::AgentCommandKind::Wake)
                .map(|command| {
                    (
                        command.command_id.clone(),
                        if command.withdrawn {
                            "withdrawn".to_owned()
                        } else {
                            format!("{:?}", command.fact)
                        },
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(wakes.len(), 2);
    for (operation, response) in wakes {
        if operation.ends_with(&stale) {
            assert_eq!(response, "withdrawn");
        } else {
            assert!(operation.ends_with(&current));
            assert_eq!(response, "Some(Accepted)");
        }
    }
    fixture.submit();
    fixture.until("the attempt to settle", settled).await;
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
    let sent = fixture.mutate(|tree| {
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
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
    });
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
    let rejoined = rejoin_held(&fixture, attached, control, result, |_| {}).await;
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
    fixture.submit();
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
    let send = |text: &str| {
        fixture.mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
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
    };
    let journaled = send("use the v2 schema");
    let command = format!("plak:mail:{}:start", journaled.message_id);
    let context = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::MailStart,
            base_text: "assignment\n\nMessage:\nuse the v2 schema",
            batch: BatchRule::MailStart(&journaled.message_id),
        })
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
    let late = send("keep the old field too");
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
    fixture.submit();
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
    let send = |text: &str| {
        fixture.mutate(|tree| {
            tree.send_agent_message(
                "root",
                &SendAgentMessageRequest {
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
    };
    let journaled = send("use the v2 schema");
    let command = format!("plak:mail:{}:start", journaled.message_id);
    let context = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::MailStart,
            base_text: "assignment\n\nMessage:\nuse the v2 schema",
            batch: BatchRule::MailStart(&journaled.message_id),
        })
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
    let late = send("keep the old field too");
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
                    .mutate_then_emit(false, |tree| {
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
                    })
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
    let sent = fixture.mutate(|tree| {
        tree.send_agent_message(
            "root",
            &SendAgentMessageRequest {
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
    });
    let command = format!("plak:mail:{}:start", sent.message_id);
    let context = ChildLedger::new(fixture.runtime.driver(), &fixture.child)
        .prepare(CommandIntent {
            command_id: &command,
            kind: CommandKind::MailStart,
            base_text: "assignment\n\nMessage:\nuse the v2 schema",
            batch: BatchRule::MailStart(&sent.message_id),
        })
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
    fixture.mutate(|tree| {
        tree.bind_attempt_turn(&fixture.child, other).unwrap();
        tree.set_active_turn(&fixture.child, true).unwrap();
    });
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
    parent_mail(&fixture, "status?");
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
    let rejoined = rejoin_held(&fixture, attached, control, result, |_| {}).await;
    assert_eq!(rejoined, Reopened::Done);
    fixture.until("the rejoined wake", turns(2)).await;
    assert!(
        fixture
            .runtime
            .read(|tree| tree.wake_mail(&fixture.child).is_empty())
            .unwrap(),
        "the rejoined wake acknowledged its batch"
    );
    fixture.submit();
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
    parent_mail(&fixture, "status?");
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
    let rejoined = rejoin_held(&fixture, attached, control, result, |fixture| {
        let status = fixture.mutate(|tree| {
            tree.interrupt_agent(
                "root",
                &crate::agent_tree_protocol::AttemptScopedRequest {
                    agent_id: fixture.child.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap()
        });
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

/// A resume holds a child a reopen finds parked, and an interrupt ends
/// that hold: the reopen wake, which walks the targets reattach read
/// before the interrupt, starts nothing and leaves the child
/// `interrupted`.
#[tokio::test]
async fn an_interrupt_of_a_held_child_stops_its_reopen_wake() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    fixture.mutate(|tree| tree.resume_tree().unwrap());
    assert!(
        fixture
            .runtime
            .read(|tree| tree.is_parked(&fixture.child))
            .unwrap(),
        "the resume holds the child for its reopen wake"
    );
    fixture.mutate(|tree| {
        tree.interrupt_agent(
            "root",
            &crate::agent_tree_protocol::AttemptScopedRequest {
                agent_id: fixture.child.clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap()
    });
    let interrupted = fixture
        .runtime
        .read(|tree| {
            (
                tree.node(&fixture.child).unwrap().status,
                tree.is_parked(&fixture.child),
            )
        })
        .unwrap();
    assert_eq!(interrupted, (NodeStatus::Interrupted, false));
    let woke = wake_reopened(&fixture.runtime, &fixture.child, attached).await;
    let (status, turn, turns) = fixture
        .runtime
        .read(|tree| {
            let attempt = tree.attempt(&fixture.child).unwrap();
            (
                tree.node(&fixture.child).unwrap().status,
                attempt.turn_id.clone(),
                attempt.usage.turns,
            )
        })
        .unwrap();
    fixture
        .registry
        .stop_all(Duration::from_millis(250))
        .await
        .unwrap();
    assert_eq!(woke, Ok(Reopened::HoldEnded));
    assert_eq!(status, NodeStatus::Interrupted);
    assert!(
        turn.is_none() && turns == 1,
        "the reopen wake started a turn on a child the operator interrupted while it \
             was held (bound turn {turn:?}, turns {turns})"
    );
}

/// An interrupt that lands after the reopen wake began, while its Start
/// is being reserved, refuses the Start: the reservation re-checks the
/// hold under the live lock, so it never writes `running` over the
/// interrupt, and the wake's batch stays unread.
#[tokio::test]
async fn an_interrupt_during_a_reopen_wake_start_is_not_overwritten() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    parent_mail(&fixture, "status?");
    fixture.mutate(|tree| {
        tree.resume_tree().unwrap();
        assert!(tree.begin_reopen_wake(&fixture.child));
        tree.interrupt_agent(
            "root",
            &crate::agent_tree_protocol::AttemptScopedRequest {
                agent_id: fixture.child.clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .unwrap();
    });
    let command = fixture
        .runtime
        .read(|tree| wake_command(tree, &fixture.child))
        .unwrap()
        .unwrap();
    let started = super::super::children::start_child_turn_as(
        &fixture.runtime,
        &fixture.child,
        attached,
        CommandId::new(command).unwrap(),
        crate::agent::children::ChildCommand::fresh(
            crate::child_prompts::PLAK_CHILD_WAKE_PROMPT.to_owned(),
            CommandKind::Wake,
        ),
        super::super::children::StartAdmission::ReopenWake,
    )
    .await;
    assert!(matches!(
        started,
        Err(super::super::children::ChildStartFailure::Refused(ref message))
            if message == REOPEN_HOLD_ENDED
    ));
    fixture
        .runtime
        .read(|tree| {
            let node = tree.node(&fixture.child).unwrap();
            assert_eq!(node.status, NodeStatus::Interrupted);
            assert!(!node.active_turn);
            assert_eq!(tree.attempt(&fixture.child).unwrap().usage.turns, 1);
            assert_eq!(tree.wake_mail(&fixture.child).len(), 1);
            assert!(tree.fault_reason(&fixture.child).is_none());
        })
        .unwrap();
    fixture
        .registry
        .stop_all(Duration::from_millis(250))
        .await
        .unwrap();
}

/// An interrupt that lands after the runtime admitted a reopen wake's
/// Start, before its turn binds, ends the hold: nothing binds, but the
/// runtime put the wake's batch in history, so its mail settles read (as
/// a rejoin whose hold ended settles its batch) instead of staying unread
/// for the resume to deliver a second time.
#[tokio::test]
async fn an_interrupt_after_a_reopen_wake_was_admitted_settles_its_batch_read() {
    let fixture = fixture(
        json!([start_job("read line"), {"output": {"text": "started"}}, held("woke")]),
        leaf,
    )
    .await;
    fixture.until("the park", parked).await;
    let attached = lose_and_reopen(&fixture).await;
    parent_mail(&fixture, "status?");
    let runtime = Arc::clone(&fixture.runtime);
    let child = fixture.child.clone();
    hooks::set_after_start_admission(
        &fixture.child,
        Arc::new(move || {
            let (runtime, child) = (Arc::clone(&runtime), child.clone());
            Box::pin(async move {
                runtime
                    .mutate_then_emit(false, |tree| {
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
                    })
                    .unwrap();
            })
        }),
    );
    fixture.mutate(|tree| tree.resume_tree().unwrap());
    let woke = wake_reopened(&fixture.runtime, &fixture.child, attached).await;
    let (status, turn, unread) = fixture
        .runtime
        .read(|tree| {
            (
                tree.node(&fixture.child).unwrap().status,
                tree.attempt(&fixture.child).unwrap().turn_id.clone(),
                tree.wake_mail(&fixture.child).len(),
            )
        })
        .unwrap();
    let prepared =
        crate::agent::ledger::prepared_commands(fixture.runtime.driver(), &fixture.child)
            .unwrap()
            .len();
    fixture
        .registry
        .stop_all(Duration::from_millis(250))
        .await
        .unwrap();
    assert_eq!(woke, Ok(Reopened::HoldEnded));
    assert_eq!(status, NodeStatus::Interrupted);
    assert_eq!(turn, None, "nothing bound the admitted wake");
    assert_eq!(prepared, 0);
    assert_eq!(
        unread, 0,
        "the admitted wake's mail is in history, yet it is unread for the resume to \
             deliver again"
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
                    .unwrap();
                ChildLedger::settlement_only(runtime.driver())
                    .settle(
                        &command,
                        crate::driver::ledger::Settlement::Refused { pause: None },
                    )
                    .map_err(String::from)
                    .unwrap();
            })
        }),
    );
    parent_mail(&fixture, "status?");
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
    fixture.submit();
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
    let after = fixture
        .runtime
        .read(|tree| tree.has_turn_waiter(&fixture.child))
        .unwrap();
    // Session handles may need a reactor to drop.
    std::mem::forget(fixture);
    assert!(registered, "the waiter registered");
    assert!(!after, "the aborted waiter's registration leaked");
}
