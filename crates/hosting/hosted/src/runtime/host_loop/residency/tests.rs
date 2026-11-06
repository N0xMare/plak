//! The idle release's decision, one residency condition at a time (spec 02
//! §4.4): each keeps the session resident while it holds, and the next
//! decision after it clears releases the session, marking it releasing.
//! The host releases at once here (an idle release of zero), so every
//! decision is the condition's alone; nothing waits on time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::{RootActor, RootRecovery, Sender};
use crate::agent_tree_protocol::SpawnAgentRequest;
use crate::host_service::AttachmentCount;
use crate::runtime::recover_tests::{script, scripted_host_with};
use crate::runtime::test_kit::{BlockOn as _, TestExtension, install_idle_topology_hub};
use crate::runtime::{HostConfig, IdleRelease};

/// Opens a scripted session whose host releases it the moment nothing
/// keeps it resident, after `prepare`, and hands its actor to `check`.
fn with_actor(
    prepare: impl FnOnce(&mut HostConfig<TestExtension>),
    check: impl FnOnce(&mut RootActor<'_, TestExtension>, &AttachmentCount),
) {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([{"output": {"text": "resident"}}])),
    );
    let attachments = AttachmentCount::default();
    config.idle_release = Some(IdleRelease {
        after: Duration::ZERO,
        attachments: attachments.clone(),
    });
    prepare(&mut config);
    let (_controls, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (sent, _observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(sent), None).unwrap();
    check(&mut host, &attachments);
}

/// The decision at a receive's timeout, from the word read just before.
fn due(host: &mut RootActor<'_, TestExtension>) -> bool {
    host.block(async |host| {
        let seen = host.residency_seen();
        host.idle_release_due(seen).await
    })
}

/// Asserts the session stays resident, then that clearing the condition
/// releases it.
fn held_then_released(
    host: &mut RootActor<'_, TestExtension>,
    attachments: &AttachmentCount,
    clear: impl FnOnce(&mut RootActor<'_, TestExtension>),
) {
    assert!(!due(host), "released while the condition held");
    assert!(!attachments.releasing());
    clear(host);
    assert!(due(host), "not released once the condition cleared");
    assert!(attachments.releasing());
}

#[test]
fn a_hold_keeps_the_session_and_a_release_refuses_the_next() {
    with_actor(
        |_| {},
        |host, attachments| {
            let hold = attachments.hold().expect("a resident session takes a hold");
            held_then_released(host, attachments, |_| drop(hold));
            assert!(
                attachments.hold().is_none(),
                "a releasing session took a hold"
            );
        },
    );
}

#[test]
fn a_paused_queued_item_keeps_the_session() {
    with_actor(
        |config| {
            config
                .session
                .pending_queue
                .push(crate::sessions::QueueItem::new("still queued"));
            config.session.queue_drain_state = crate::sessions::QueueDrainState::RestoredPaused;
        },
        |host, attachments| {
            held_then_released(host, attachments, |host| {
                host.role.config.session.pending_queue.clear();
            });
        },
    );
}

#[test]
fn a_pending_continuation_keeps_the_session() {
    with_actor(
        |_| {},
        |host, attachments| {
            host.role.config.continuation = Some("carry on".into());
            held_then_released(host, attachments, |host| {
                host.role.config.continuation = None;
            });
        },
    );
}

/// A queued item the actor took to start next keeps the session. The loop
/// starts it before it would receive, so only this decision reads it.
#[test]
fn a_queued_item_about_to_start_keeps_the_session() {
    with_actor(
        |_| {},
        |host, attachments| {
            host.role.pending_start = Some(crate::sessions::QueueItem::new("next up"));
            held_then_released(host, attachments, |host| {
                host.role.pending_start = None;
            });
        },
    );
}

/// An action a watched step drained and deferred keeps the session. The
/// loop takes it before it would receive, so only this decision reads it.
#[test]
fn a_deferred_action_keeps_the_session() {
    with_actor(
        |_| {},
        |host, attachments| {
            let (sender, receiver) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
            sender
                .send(crate::surface_protocol::UserAction::Prompt(
                    "deferred".into(),
                ))
                .unwrap();
            let delivery = receiver.try_recv().unwrap();
            host.role
                .interrupt_state
                .borrow_mut()
                .restore_front(delivery);
            held_then_released(host, attachments, |host| {
                drop(host.role.interrupt_state.borrow_mut().take_deferred());
            });
        },
    );
}

/// A transient resume waiting for its gate keeps the session. The loop
/// runs it at a receive's timeout before deciding, so only this decision
/// reads it.
#[test]
fn a_pending_transient_resume_keeps_the_session() {
    with_actor(
        |_| {},
        |host, attachments| {
            let plan = crate::driver::transient::plan(
                crate::driver::transient::ResumeGate::Delay(Duration::from_secs(3600)),
                crate::driver::transient::ResumeOwner::Root {
                    failed_turn: plakous::model::TurnId::try_from(uuid::Uuid::from_u128(1))
                        .unwrap(),
                },
                "offline",
                0,
                1,
            )
            .unwrap();
            host.block(async |host| {
                let id = host.role.config.session.id.clone();
                host.role.pending_resume = Some(super::super::transient::PendingResume::spawn(
                    host.role.executor.tasks(),
                    id,
                    plan,
                    None,
                ));
            });
            held_then_released(host, attachments, |host| {
                host.role.pending_resume = None;
            });
        },
    );
}

/// A lost host's continuation the operator consented to keeps the session.
/// The loop runs it at a receive's timeout before deciding, so only this
/// decision reads it.
#[test]
fn a_due_loss_continuation_keeps_the_session() {
    with_actor(
        |_| {},
        |host, attachments| {
            host.role.root_loss_resume_due = true;
            held_then_released(host, attachments, |host| {
                host.role.root_loss_resume_due = false;
            });
        },
    );
}

/// An attach and its detach between the read and the mark keep the
/// session, though nothing is counted at the mark: the release waits
/// afresh, and the next decision releases it.
#[test]
fn an_attach_gone_again_before_the_mark_keeps_the_session() {
    with_actor(
        |_| {},
        |host, attachments| {
            let seen = host.residency_seen();
            drop(attachments.hold().unwrap());
            assert_eq!(attachments.get(), 0);
            assert!(
                !host.block(async |host| host.idle_release_due(seen).await),
                "released over an attach since the read"
            );
            assert!(!attachments.releasing());
            assert!(due(host), "not released at the next decision");
            assert!(attachments.releasing());
        },
    );
}

#[test]
fn recovery_debt_keeps_the_session() {
    with_actor(
        |_| {},
        |host, attachments| {
            host.role.recovery = RootRecovery::Retrying;
            held_then_released(host, attachments, |host| {
                host.role.recovery = RootRecovery::Clear;
            });
        },
    );
}

#[test]
fn a_live_child_keeps_the_tree_resident_until_it_retires() {
    let hub = std::sync::Mutex::new(None);
    with_actor(
        |config| {
            let runtime =
                install_idle_topology_hub(&config.session.id, &config.session.workspace_root);
            let child = runtime
                .mutate_then_emit(false, |tree| {
                    Ok(tree
                        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                        .unwrap()
                        .agent_id)
                })
                .block()
                .unwrap();
            let executor = plakous::application::ApplicationExecutor::new().unwrap();
            assert!(
                executor
                    .block_on(runtime.try_occupy_live(child.clone(), Box::new(QuietChild)))
                    .unwrap()
                    .is_ok()
            );
            *hub.lock().unwrap() = Some((runtime, child));
        },
        |host, attachments| {
            let (runtime, child) = hub.lock().unwrap().take().unwrap();
            assert!(!due(host), "released with a live child");
            assert!(!attachments.releasing());
            let _ = host.block(async |_| runtime.cancel_live(&child).await);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !due(host) {
                assert!(
                    Instant::now() < deadline,
                    "not released once the child retired"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(attachments.releasing());
        },
    );
}

/// A live child that retires at once when asked.
struct QuietChild;

impl crate::topology_runtime::LiveChildHandle for QuietChild {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        None
    }
    fn turn_id(&self) -> plakous::model::TurnId {
        "00000000-0000-0000-0000-000000000001".parse().unwrap()
    }
    fn turn_control(&self) -> plakous::agent::TurnControl {
        panic!("unused")
    }
    fn follow_up(
        &self,
        _: plakous::agent::CommandId,
        _: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        Box::pin(async { Err("unused".into()) })
    }
    fn follow_up_barrier(&self) -> Option<Arc<tokio::sync::RwLock<()>>> {
        None
    }
    fn restart(
        &mut self,
        _: crate::agent_tree_tools::TreeDriver,
        _: plakous::agent::CommandId,
        _: String,
        _delivery: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<plakous::agent::TurnResultHandle, String>> + Send + '_>,
    > {
        Box::pin(async { Err("unused".into()) })
    }
    fn release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        self.cancel_and_release(driver, None)
    }
    fn cancel_and_release(
        self: Box<Self>,
        _driver: crate::agent_tree_tools::TreeDriver,
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        if let Some(dispatched) = dispatched {
            let _ = dispatched.send(());
        }
        Box::pin(async { crate::topology_runtime::LiveRelease::default() })
    }
}

/// A stop drops a transient resume, a host continuation and a pending
/// start. Any one of them left in place would run after the stop.
#[test]
fn a_stop_drops_a_resume_a_continuation_and_a_pending_start() {
    with_actor(
        |_| {},
        |host, _| {
            host.role.config.continuation = Some("carry on".into());
            host.role.pending_start = Some(crate::sessions::QueueItem::new("next up"));
            host.block(async |host| {
                let plan = crate::driver::transient::plan(
                    crate::driver::transient::ResumeGate::Delay(Duration::from_secs(3600)),
                    crate::driver::transient::ResumeOwner::Root {
                        failed_turn: plakous::model::TurnId::try_from(uuid::Uuid::from_u128(1))
                            .unwrap(),
                    },
                    "offline",
                    0,
                    1,
                )
                .unwrap();
                let id = host.role.config.session.id.clone();
                host.role.pending_resume = Some(super::super::transient::PendingResume::spawn(
                    host.role.executor.tasks(),
                    id,
                    plan,
                    None,
                ));
            });
            let (sender, _receiver) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
            let ticket = sender.request_stop().unwrap();
            host.block(async |host| host.stop_session(ticket, None).await);
            assert!(host.role.pending_resume.is_none(), "the resume would retry");
            assert!(host.role.config.continuation.is_none());
            assert!(host.role.pending_start.is_none());
        },
    );
}

/// A queued item's live drain stops with the session. A later open pauses
/// any queue still outstanding; this pins the pause the stop itself writes,
/// which a load of a non-empty queue would report either way.
#[test]
fn a_stop_pauses_a_queued_item() {
    with_actor(
        |config| {
            config
                .session
                .pending_queue
                .push(crate::sessions::QueueItem::new("must not run"));
            config.session.queue_drain_state = crate::sessions::QueueDrainState::LiveAutoDrain;
        },
        |host, _| {
            let (sender, _receiver) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
            let ticket = sender.request_stop().unwrap();
            host.block(async |host| host.stop_session(ticket, None).await);
            assert_eq!(
                host.role.config.session.queue_drain_state,
                crate::sessions::QueueDrainState::RestoredPaused,
                "the queued item would still drain"
            );
            assert_eq!(host.role.config.session.pending_queue.len(), 1);
            assert!(host.role.pending_start.is_none());
        },
    );
}

/// A stop a running turn took, not yet finished, keeps the session. An
/// idle release of zero would otherwise drop it before the settle.
#[test]
fn a_stop_taken_by_a_running_turn_keeps_the_session() {
    with_actor(
        |_| {},
        |host, attachments| {
            let (sender, _receiver) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
            let ticket = sender.request_stop().unwrap();
            host.role
                .interrupt_state
                .borrow_mut()
                .stop_after_turn(ticket, None);
            assert!(!due(host), "released while the stop was unfinished");
            assert!(!attachments.releasing());
            let _ = host.role.interrupt_state.borrow_mut().take_stops();
            assert!(due(host), "not released once the stop was taken");
            assert!(attachments.releasing());
        },
    );
}

/// The tree's retirement bound is past, and the stop is already durable.
/// The answer is still stopped.
#[test]
fn a_slow_child_retirement_does_not_fail_a_recorded_stop() {
    let _guard = crate::runtime::test_seams::Guard;
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([{"output": {"text": "resident"}}])),
    );
    let attachments = AttachmentCount::default();
    config.idle_release = Some(IdleRelease {
        after: Duration::ZERO,
        attachments: attachments.clone(),
    });
    let (sender, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (sent, _observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(sent), None).unwrap();
    let id = host.role.config.session.id.clone();
    let _runtime = install_idle_topology_hub(&id, &host.role.config.cwd);
    crate::runtime::test_seams::force_retirement_timeout(&id);
    let ticket = sender.request_stop().unwrap();
    host.block(async |host| host.stop_session(ticket, None).await);
    assert_eq!(sender.stop_settled(ticket), Some(Ok(())));
}

/// A running child and a child held after a loss both read `stopped`, and
/// the held one has no continuation left.
#[test]
fn a_stop_interrupts_a_running_child_and_a_held_child() {
    use crate::agent_tree::{AgentCommand, AgentCommandKind, InterruptCause, interrupt_cause};
    use crate::agent_tree_protocol::{NodeStatus, SpawnAgentRequest};
    with_actor(
        |_| {},
        |host, _| {
            let id = host.role.config.session.id.clone();
            let runtime = install_idle_topology_hub(&id, &host.role.config.cwd);
            let (running, held) = host.block(async |_host| {
                runtime
                    .mutate_then_emit(false, |tree| {
                        let spawn = |tree: &mut crate::agent_tree::ProductAgentTree| {
                            tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                                .unwrap()
                                .agent_id
                        };
                        let running = spawn(tree);
                        let held = spawn(tree);
                        for (child, turn) in [
                            (running.as_str(), "00000000-0000-0000-0000-0000000000a1"),
                            (held.as_str(), "00000000-0000-0000-0000-0000000000a2"),
                        ] {
                            tree.bind_session_locator(child, "locator".to_owned())
                                .unwrap();
                            tree.set_active_turn(child, true).unwrap();
                            tree.bind_attempt_turn(child, turn).unwrap();
                            let command = format!("start:{child}");
                            let attempt = tree.node(child).unwrap().attempt_id.clone();
                            tree.record_command(AgentCommand::intent(
                                command,
                                child,
                                attempt,
                                AgentCommandKind::Start,
                                "assignment",
                                Vec::new(),
                            ))
                            .unwrap();
                        }
                        tree.release_attempt_turn(&held).unwrap();
                        tree.park_child(&held, Vec::new()).unwrap();
                        tree.mark_owner_lost(&held);
                        Ok((running, held))
                    })
                    .await
                    .unwrap()
            });
            let (sender, _receiver) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
            let ticket = sender.request_stop().unwrap();
            host.block(async |host| host.stop_session(ticket, None).await);
            runtime
                .read(|tree| {
                    assert_eq!(tree.node(&running).unwrap().status, NodeStatus::Interrupted);
                    assert_eq!(tree.loss_continuation(&held), None);
                    let causes: Vec<_> = tree
                        .operations()
                        .values()
                        .filter_map(|operation| interrupt_cause(operation).unwrap())
                        .collect();
                    assert!(
                        causes.contains(&Some(InterruptCause::Stopped)),
                        "{causes:?}"
                    );
                })
                .unwrap();
        },
    );
}

/// A stopped session keeps nothing resident: the stop ends what would
/// continue, so the next decision releases it.
#[test]
fn a_stopped_session_is_released_when_idle() {
    with_actor(
        |_| {},
        |host, attachments| {
            let (sender, receiver) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
            let ticket = sender.request_stop().unwrap();
            drop(receiver);
            host.block(async |host| host.stop_session(ticket, None).await);
            let role = &host.role;
            let state = format!(
                "start={} resume={} loss={} continuation={} blocked={} deferred={} stops={} queue={} jobs={}",
                role.pending_start.is_some(),
                role.pending_resume.is_some(),
                role.root_loss_resume_due,
                role.config.continuation.is_some(),
                role.recovery.blocked(),
                role.interrupt_state.borrow().has_deferred(),
                role.interrupt_state.borrow().has_stops(),
                role.config.session.pending_queue.len(),
                role.config.state_roots.job_registry().running_count(),
            );
            assert!(due(host), "a stopped session stayed resident: {state}");
            assert!(attachments.releasing());
        },
    );
}
