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
