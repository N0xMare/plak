//! Delivery/terminal rendezvous over a real child session, without a turn waiter.
#![allow(clippy::unwrap_used, reason = "offline follow-up fixtures")]

use super::*;
use crate::agent::children::ChildTurnIdentity;
use crate::agent::park::{self, TurnEnd};
use crate::agent_tree_tools::{MailDeliverer, MailDeliveryRequest};
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
};
use plakous_offline_crash_harness::gate::GateHandle;
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};

type Queue = std::collections::VecDeque<(TurnControl, TurnResultHandle)>;

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
        assert!(
            runtime
                .try_occupy_live(child.clone(), Box::new(live))
                .await
                .is_ok()
        );
        gate.entered().await;
        Self {
            _root: root,
            runtime,
            agent,
            child,
            first,
            gate,
            queued,
        }
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
        let prompt = crate::agent::result_context::prepare(
            self.runtime.driver(),
            &self.child,
            command.as_str(),
            "follow up",
        )
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
