//! Delivery/terminal rendezvous over a real child session, without a turn waiter.
#![allow(clippy::unwrap_used, reason = "offline follow-up fixtures")]

use super::*;
use crate::agent::children::ChildTurnIdentity;
use crate::agent::ledger::ChildLedger;
use crate::agent::park::{self, TurnEnd};
use crate::agent_tree_tools::MailDeliveryRequest;
use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind, CommandLedger};
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
};
use plakous_offline_crash_harness::gate::GateHandle;
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};

type Queue = std::collections::VecDeque<(
    TurnControl,
    TurnResultHandle,
    crate::runtime::child_projection::ChildTurnProjection,
)>;

struct Canceller {
    runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
    returned: Arc<tokio::sync::Notify>,
    finish: Option<Arc<tokio::sync::Notify>>,
}

impl crate::agent_tree_tools::ChildController for Canceller {
    fn services(&self) -> crate::agent_tree_tools::ChildServices {
        crate::agent_tree_tools::ChildServices::CANCEL
    }
    fn cancel_agents(
        &self,
        ids: Vec<String>,
        signalled: tokio::sync::oneshot::Sender<()>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        crate::agent_tree_tools::LiveCancelOutcome,
                        crate::agent_tree_tools::LiveCancelFailure,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let join = self.runtime.begin_cancel_live_many(&ids).await;
            let _ = signalled.send(());
            let report = join.await;
            assert!(report.is_clean());
            self.returned.notify_one();
            if let Some(finish) = &self.finish {
                finish.notified().await;
            }
            Ok(crate::agent_tree_tools::LiveCancelOutcome {
                cancelled_ids: report.cancelled,
                not_live_ids: report.not_live,
            })
        })
    }
}

async fn close(
    fixture: &Fixture,
    finish: Option<Arc<tokio::sync::Notify>>,
    returned: Arc<tokio::sync::Notify>,
) -> plakous::tools::ToolOutcome {
    crate::agent_tree_tools::invoke_granted_topology_tool_on(
        fixture.runtime.driver().clone(),
        "root".into(),
        "close_agent",
        &serde_json::json!({"agent_id": fixture.child}),
        "fixture:close".into(),
        crate::agent_tree_tools::TopologyHostPorts {
            controller: Some(Arc::new(Canceller {
                runtime: fixture.runtime.clone(),
                returned,
                finish,
            })),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn close_acknowledges_actual_cancel_after_delivery_lock_and_stops_the_child() {
    let fixture = Fixture::new().await;
    let control = fixture
        .runtime
        .live
        .lock()
        .await
        .get(&fixture.child)
        .unwrap()
        .turn_control();
    let barrier = fixture
        .runtime
        .live
        .lock()
        .await
        .get(&fixture.child)
        .unwrap()
        .follow_up_barrier()
        .unwrap();
    let delivery = barrier.clone().read_owned().await;
    let returned = Arc::new(tokio::sync::Notify::new());
    let mut closing = Box::pin(close(&fixture, None, returned.clone()));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(closing.as_mut().poll(cx).is_pending()))
            .await
    );
    while barrier.try_read().is_ok() {
        tokio::task::yield_now().await;
    }
    tokio::task::yield_now().await;
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(closing.as_mut().poll(cx).is_pending()))
            .await
    );
    drop(delivery);
    assert_eq!(
        closing.await.kind(),
        plakous::tools::ToolOutcomeKind::Success
    );
    assert!(
        control.is_cancelled(),
        "the actor must have received cancel before acknowledgement"
    );
    let result = fixture.first.clone().result().await.unwrap();
    assert!(matches!(
        result.outcome(),
        plakous::agent::TurnResultOutcome::Cancelled(_)
    ));
    returned.notified().await;
    fixture
        .runtime
        .driver()
        .join_retirement_tasks()
        .await
        .unwrap();
    assert!(fixture.runtime.live.lock().await.is_empty());
    crate::topology_runtime::force_release(
        &fixture.runtime.read(|tree| tree.tree_id.clone()).unwrap(),
    )
    .await;
}

struct DispatchWake {
    control: TurnControl,
    observed: std::sync::atomic::AtomicU8,
    fired: tokio::sync::Notify,
}

impl std::task::Wake for DispatchWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.observed.store(
            if self.control.is_cancelled() { 2 } else { 1 },
            std::sync::atomic::Ordering::SeqCst,
        );
        self.fired.notify_one();
    }
}

#[tokio::test]
async fn dispatch_signal_observes_actor_cancellation_at_the_instant_it_fires() {
    let (fixture, live) = Fixture::unoccupied().await;
    let probe = Arc::new(DispatchWake {
        control: live.turn_control(),
        observed: std::sync::atomic::AtomicU8::new(0),
        fired: tokio::sync::Notify::new(),
    });
    assert!(!probe.control.is_cancelled());
    let (signal, mut signalled) = tokio::sync::oneshot::channel();
    let waker = std::task::Waker::from(probe.clone());
    assert!(
        Pin::new(&mut signalled)
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending()
    );
    let release = tokio::spawn(
        Box::new(live).cancel_and_release(fixture.runtime.driver().clone(), Some(signal)),
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), probe.fired.notified())
        .await
        .expect("dispatch must wake its registered receiver");
    assert_eq!(
        probe.observed.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "cancel must precede the signal, not merely the tool's eventual return"
    );
    assert!(signalled.await.is_ok());
    assert!(release.await.unwrap().failure.is_none());
    crate::topology_runtime::force_release(
        &fixture.runtime.read(|tree| tree.tree_id.clone()).unwrap(),
    )
    .await;
}

#[tokio::test]
async fn headless_exit_joins_tool_receipt_after_the_live_owner_has_retired() {
    assert_exit_joins_tool_receipt(false).await;
}

#[tokio::test]
async fn shutdown_joins_tool_receipt_after_the_live_owner_has_retired() {
    assert_exit_joins_tool_receipt(true).await;
}

async fn assert_exit_joins_tool_receipt(shutdown: bool) {
    let fixture = Fixture::new().await;
    let finish = Arc::new(tokio::sync::Notify::new());
    let returned = Arc::new(tokio::sync::Notify::new());
    assert_eq!(
        close(&fixture, Some(finish.clone()), returned.clone())
            .await
            .kind(),
        plakous::tools::ToolOutcomeKind::Success
    );
    returned.notified().await;
    assert!(fixture.runtime.live.lock().await.is_empty());
    assert!(
        fixture
            .runtime
            .read(|tree| tree.operations()["fixture:close"]
                .terminal_response
                .is_none())
            .unwrap()
    );
    let tree_id = fixture.runtime.read(|tree| tree.tree_id.clone()).unwrap();
    let mut exiting = Box::pin(async {
        if shutdown {
            crate::topology_runtime::force_release(&tree_id).await;
        } else {
            crate::runtime::mark_root_interrupted_for_run(&tree_id)
                .await
                .unwrap();
        }
    });
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(exiting.as_mut().poll(cx).is_pending()))
            .await
    );
    // Shutdown also joins an empty startup worker. Let that unrelated join
    // finish before proving the receipt owner still keeps exit pending.
    tokio::task::yield_now().await;
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(exiting.as_mut().poll(cx).is_pending()))
            .await
    );
    finish.notify_one();
    exiting.await;
    assert!(
        fixture
            .runtime
            .read(|tree| tree.operations()["fixture:close"]
                .terminal_response
                .is_some())
            .unwrap()
    );
    crate::topology_runtime::force_release(&tree_id).await;
}

struct Fixture {
    _root: tempfile::TempDir,
    runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
    agent: plakous::agent::Agent,
    child: String,
    first: TurnResultHandle,
    gate: GateHandle,
    queued: Arc<std::sync::Mutex<Queue>>,
}

impl Fixture {
    async fn new() -> Self {
        let (fixture, live) = Self::unoccupied().await;
        assert!(
            fixture
                .runtime
                .try_occupy_live(fixture.child.clone(), Box::new(live))
                .await
                .is_ok()
        );
        fixture
    }

    async fn unoccupied() -> (Self, ProductLiveChild) {
        let root = tempfile::tempdir().unwrap();
        let tree_id = uuid::Uuid::new_v4().to_string();
        let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            &tree_id,
            "root",
            "owner",
            root.path().to_owned(),
            "selected",
            crate::agent_tree_protocol::WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent(
                "root",
                &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .unwrap()
            .agent_id;
        let runtime = crate::topology_runtime::insert_root_with(
            tree_id,
            tree,
            Arc::new(crate::agent_tree::MemoryTreeStore::new()),
            None,
        )
        .unwrap();
        let gate = GateHandle::new();
        let plan = Arc::new(
            serde_json::from_value(serde_json::json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "steps": [
                    {"stream": [{"text": "first"}, {"gate": gate.script()}]},
                    {"stream": [{"text": "follow-up"}, {"stall": 30000}]}
                ]
            }))
            .unwrap(),
        );
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            root.path().join("evidence.jsonl"),
        )
        .unwrap();
        let sessions = Arc::new(
            AgentSessions::new(
                SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
            )
            .unwrap(),
        );
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("follow-up:create").unwrap(),
                scripted_runtime_binding(root.path(), controller, plan).unwrap(),
            ))
            .await
            .unwrap()
        else {
            panic!("created session required")
        };
        runtime
            .mutate_then_emit(false, |tree| {
                tree.record_child_session(&child, &session.locator().to_string())
                    .unwrap();
                let attempt = tree.attempt(&child).unwrap().attempt_id.clone();
                tree.record_command(crate::agent_tree::AgentCommand::intent(
                    "fixture:start",
                    &child,
                    Some(attempt),
                    crate::agent_tree::AgentCommandKind::Start,
                    "start",
                    Vec::new(),
                ))
                .map_err(|error| error.message)
            })
            .unwrap();
        let agent = session.agent().clone();
        let submission = agent
            .start(CommandId::new("fixture:start").unwrap(), "start")
            .await
            .unwrap();
        let first = submission.result().unwrap().clone();
        let feed = crate::agent::read_tree_feed(&sessions, session.locator().session_id(), 0)
            .await
            .unwrap();
        runtime
            .mutate_then_emit(false, |tree| {
                feed.apply(tree, &child)
                    .map(|_| ())
                    .map_err(|error| error.message)
            })
            .unwrap();
        let live = ProductLiveChild::new(
            AttachedCodingSession {
                output_bound_rederived: None,
                reachability_probe: None,
                sessions,
                locator: session.locator(),
                opened: Some(session),
                recovered_turn: None,
                topology_owner: None,
            },
            first.turn_id(),
            submission.turn().unwrap().control(),
        );
        let queued = Arc::clone(&live.follow_ups);
        gate.entered().await;
        (
            Self {
                _root: root,
                runtime,
                agent,
                child,
                first,
                gate,
                queued,
            },
            live,
        )
    }

    fn identity(&self) -> ChildTurnIdentity {
        ChildTurnIdentity {
            attempt: self
                .runtime
                .read(|tree| tree.attempt(&self.child).unwrap().attempt_id.clone())
                .unwrap(),
            turn: self.first.turn_id(),
        }
    }

    fn pause(&self) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let admitted = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        park::hooks::set_after_follow_up_admission(
            &self.agent.id().to_string(),
            Arc::new({
                let admitted = admitted.clone();
                let finish = finish.clone();
                move || {
                    let admitted = admitted.clone();
                    let finish = finish.clone();
                    Box::pin(async move {
                        admitted.notify_one();
                        finish.notified().await;
                    })
                }
            }),
        );
        (admitted, finish)
    }

    async fn direct_delivery(
        &self,
    ) -> (
        tokio::task::JoinHandle<Result<(), String>>,
        CommandId,
        String,
    ) {
        let command = CommandId::new("fixture:follow-up").unwrap();
        let prompt = ChildLedger::new(self.runtime.driver(), &self.child)
            .prepare(CommandIntent {
                command_id: command.as_str(),
                kind: CommandKind::Followup,
                base_text: "follow up",
                batch: BatchRule::Results,
            })
            .unwrap()
            .prompt;
        let delivery = self
            .runtime
            .live
            .lock()
            .await
            .get(&self.child)
            .unwrap()
            .follow_up(command.clone(), prompt.clone());
        (tokio::spawn(delivery), command, prompt)
    }

    fn handoff(&self) -> impl Future<Output = TurnEnd> + Send + 'static {
        let runtime = self.runtime.clone();
        let child = self.child.clone();
        let identity = self.identity();
        async move {
            park::turn_end(
                runtime.driver(),
                &runtime.live,
                &child,
                &identity,
                &park::Arrivals::new(),
            )
            .await
        }
    }

    async fn terminal(&self) {
        self.gate.release();
        self.first.clone().result().await.unwrap();
    }
}

async fn assert_pending(future: Pin<&mut impl Future>) {
    let mut future = future;
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx).is_pending()))
            .await
    );
}

#[tokio::test]
async fn terminal_handoff_drains_registration_and_revalidates_concurrent_close() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for close in [false, true] {
            let fixture = Fixture::new().await;
            let (admitted, finish) = fixture.pause();
            let (delivery, command, prompt) = fixture.direct_delivery().await;
            admitted.notified().await;
            let admitted_turn = fixture
                .agent
                .follow_up_with_id(command, fixture.first.turn_id(), prompt)
                .await
                .unwrap()
                .result()
                .unwrap()
                .turn_id();
            fixture.terminal().await;
            let mut handoff = Box::pin(fixture.handoff());
            assert_pending(handoff.as_mut()).await;
            assert!(fixture.runtime.live.try_lock().is_ok());
            assert!(fixture.queued.lock().unwrap().is_empty());
            let handoff = tokio::spawn(handoff);
            let mut retirement = Box::pin(fixture.runtime.cancel_live(&fixture.child));
            if close {
                assert_pending(retirement.as_mut()).await;
            }
            finish.notify_one();
            delivery.await.unwrap().unwrap();
            let result = handoff.await.unwrap();
            if close {
                assert!(!matches!(result, TurnEnd::FollowUp(_)));
                retirement.await.unwrap();
                assert!(
                    !fixture
                        .runtime
                        .live
                        .lock()
                        .await
                        .contains_key(&fixture.child)
                );
            } else {
                let TurnEnd::FollowUp(result) = result else {
                    panic!("follow-up handoff required")
                };
                assert_eq!(result.turn_id(), admitted_turn);
                {
                    let live = fixture.runtime.live.lock().await;
                    let child = live.get(&fixture.child).unwrap();
                    assert!(
                        crate::runtime::child_projection::of(child).is_some(),
                        "the opaque projection must rebind to the queued follow-up turn"
                    );
                }
                assert_eq!(
                    fixture
                        .runtime
                        .live
                        .lock()
                        .await
                        .get(&fixture.child)
                        .unwrap()
                        .turn_id(),
                    admitted_turn
                );
                retirement.await.unwrap();
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn restart_drains_follow_up_without_holding_the_live_map() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let fixture = Fixture::new().await;
        let (admitted, finish) = fixture.pause();
        let (delivery, _, _) = fixture.direct_delivery().await;
        admitted.notified().await;
        fixture.terminal().await;
        let identity = fixture.identity();
        let mut restart = Box::pin(crate::agent::children::start_instruction_turn(
            fixture.runtime.driver(),
            &fixture.runtime.live,
            &fixture.child,
            &identity,
            CommandId::new("fixture:restart").unwrap(),
            "restart".into(),
            CommandKind::Corrective,
        ));
        assert_pending(restart.as_mut()).await;
        assert!(fixture.runtime.live.try_lock().is_ok());
        finish.notify_one();
        delivery.await.unwrap().unwrap();
        let restarted = restart.await.unwrap();
        assert_eq!(fixture.queued.lock().unwrap().len(), 1);
        assert_eq!(
            fixture
                .runtime
                .live
                .lock()
                .await
                .get(&fixture.child)
                .unwrap()
                .turn_id(),
            restarted.turn_id(),
        );
        fixture.runtime.cancel_live(&fixture.child).await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn mail_entry_detaches_registration_from_cancelled_send_and_allows_close() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for cancel_send in [false, true] {
            let fixture = Fixture::new().await;
            let (admitted, finish) = fixture.pause();
            let delivery = tokio::spawn({
                let runtime = fixture.runtime.clone();
                let child = fixture.child.clone();
                async move {
                    let deliverer = crate::agent::children::ProductMailDeliverer {
                        parent: crate::agent::SpawnParent::Api(
                            crate::agent::tests::openrouter_test_parent(),
                        ),
                        runtime,
                    };
                    deliverer
                        .deliver(MailDeliveryRequest {
                            agent_id: child,
                            session_locator: String::new(),
                            disposition: crate::agent_tree::MailDisposition::Queued,
                            message_id: "fixture-mail".into(),
                            body: "follow up".into(),
                            description: String::new(),
                            prompt: String::new(),
                            output_schema_json: String::new(),
                            path: String::new(),
                            turn_token: 1,
                        })
                        .await
                }
            });
            admitted.notified().await;
            assert!(fixture.runtime.live.try_lock().is_ok());
            assert!(fixture.queued.lock().unwrap().is_empty());
            if cancel_send {
                delivery.abort();
                assert!(delivery.await.unwrap_err().is_cancelled());
                fixture.terminal().await;
                let mut handoff = Box::pin(fixture.handoff());
                assert_pending(handoff.as_mut()).await;
                finish.notify_one();
                let TurnEnd::FollowUp(result) = handoff.await else {
                    panic!("cancelled send lost its admitted follow-up")
                };
                assert_eq!(
                    fixture
                        .runtime
                        .live
                        .lock()
                        .await
                        .get(&fixture.child)
                        .unwrap()
                        .turn_id(),
                    result.turn_id()
                );
                fixture.runtime.cancel_live(&fixture.child).await.unwrap();
            } else {
                let control = fixture
                    .runtime
                    .live
                    .lock()
                    .await
                    .get(&fixture.child)
                    .unwrap()
                    .turn_control();
                assert!(
                    control
                        .steer_with_id(CommandId::new("fixture:steer").unwrap(), "steer")
                        .await
                        .unwrap()
                        .rejection()
                        .is_none()
                );
                let mut close = Box::pin(fixture.runtime.cancel_live(&fixture.child));
                assert_pending(close.as_mut()).await;
                {
                    let live = fixture.runtime.live.try_lock().unwrap();
                    assert!(live.contains_key(&fixture.child));
                    assert!(
                        live.get(&fixture.child).is_none(),
                        "close reserved retirement"
                    );
                }
                assert!(fixture.queued.lock().unwrap().is_empty());
                finish.notify_one();
                let ack = delivery.await.unwrap().unwrap();
                assert_eq!(ack.turn_id, Some(fixture.first.turn_id().to_string()));
                assert_eq!(fixture.queued.lock().unwrap().len(), 1);
                close.await.unwrap();
            }
        }
    })
    .await
    .unwrap();
}
