//! A face's controls of a child the reopen law owes a `recover_node`
//! (spec 02 §4.4, spec 03 §14): its mail is refused by the slot, stays
//! durable and reaches the child that `recover_node` rebinds; its
//! interrupt takes effect at once, with the operator as its cause.

use std::sync::Arc;

use plakous_host_protocol::wire::{NodeAddress, RequestRefusal};
use plakous_host_protocol::{
    Core, Draft, ObservationReceiver, SessionIntent, SessionObservation, intent_channel,
    observation_channel,
};

use super::Fixture;
use crate::agent_tree::InterruptCause;
use crate::agent_tree_protocol::{AttemptScopedRequest, AttemptStatus, NodeStatus};
use crate::host_service::{HostService, TreeNodes};

/// A child whose bound turn the lost owner left: owed its `recover_node`.
async fn owed_child(name: &str) -> (Fixture, String) {
    let fixture = Fixture::new(name);
    // A native account, so the product composes the mail's Start.
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.pin_bind_hints_for_test(crate::orchestration::BindHints::from_names(
                &["claude"],
                &[],
            ));
            tree.pin_template(
                "review",
                crate::orchestration::RosterBinding::leaf("claude", "test-model"),
            );
            Ok(())
        })
        .await
        .unwrap();
    let id = fixture.spawn();
    let holder = fixture.sessions();
    let (held, handle, _) = fixture.start_failing_turn(&holder, &id).await;
    Fixture::await_unknown_outcome(&handle).await;
    fixture.bind_turn(&id, &held.locator(), handle.turn_id());
    held.release().await.unwrap();
    fixture
        .runtime
        .mutate_then_emit(false, |tree| {
            tree.apply_reopen_law();
            Ok(())
        })
        .await
        .unwrap();
    assert!(owed(&fixture, &id), "a bound turn is owed a recover_node");
    (fixture, id)
}

fn owed(fixture: &Fixture, id: &str) -> bool {
    fixture.runtime.read(|tree| tree.recovery_owed(id)).unwrap()
}

/// A wire host service on the fixture's tree, and the child's address.
fn face_service(
    fixture: &Fixture,
    id: &str,
) -> (
    HostService<Core>,
    plakous_host_protocol::IntentReceiver<Core>,
    NodeAddress,
) {
    let (actions, intents) = intent_channel::<Core>();
    let service = HostService::new(
        actions,
        NodeAddress {
            tree_id: fixture.tree_id.clone(),
            agent_id: "root".into(),
        },
        64,
    )
    .with_nodes(Arc::new(TreeNodes::new(fixture.tree_id.clone())));
    let node = NodeAddress {
        tree_id: fixture.tree_id.clone(),
        agent_id: id.to_owned(),
    };
    (service, intents, node)
}

/// The next observation the face's mailbox holds, waiting on the tree's
/// tasks without a clock.
async fn answer(received: &ObservationReceiver<Core>) -> SessionObservation<Core> {
    loop {
        if let Ok(observation) = received.try_recv() {
            return observation;
        }
        tokio::task::yield_now().await;
    }
}

/// A face attached to an owed child steers it: the steer is operator mail
/// to that node, written and answered at once, which starts nothing (the
/// slot refuses it, nothing dispatches) and stays durable. Once `recover_node` rebinds the child,
/// the same mail starts its turn, the face's answer arrives, and the face
/// is still attached to the node.
#[tokio::test]
async fn a_face_steer_to_an_owed_child_stays_durable_until_recover_node() {
    let (fixture, id) = owed_child("face-owed-mail").await;
    let dispatched = fixture.dispatches();
    let (service, intents, node) = face_service(&fixture, &id);
    let child = service.attach(&node).expect("an owed child is open");
    let _transcript = intents.try_recv().unwrap();
    service
        .intent(child, SessionIntent::Steer(Draft::from("while owed")))
        .unwrap();
    let (steer, _) = intents.try_recv().unwrap().into_parts();
    let SessionIntent::MailTopologyNode {
        agent_id,
        message,
        urgent,
    } = steer
    else {
        panic!("a child's steer is operator mail to it: {steer:?}");
    };
    let (channel, received) = observation_channel::<Core>();
    let events = crate::runtime::Sender::over(channel);
    crate::topology_navigator::spawn_operator_mail(
        &fixture.runtime,
        &fixture.tree_id,
        &events,
        &agent_id,
        &message,
        urgent,
    );
    while fixture
        .runtime
        .read(|tree| tree.pending_mail(&id).len())
        .unwrap()
        == 0
    {
        tokio::task::yield_now().await;
    }
    assert!(
        !fixture.runtime.live.lock().await.contains_key(&id),
        "the mail started no turn"
    );
    assert_eq!(fixture.dispatches(), dispatched);
    assert!(owed(&fixture, &id));
    assert_eq!(
        answer(&received).await,
        SessionObservation::DraftAccepted(SessionIntent::MailTopologyNode {
            agent_id: id.clone(),
            message: "while owed".into(),
            urgent: true,
        }),
        "the face is answered once the mail is written"
    );
    assert!(
        received.try_recv().is_err(),
        "its delivery waits on the child"
    );

    crate::agent::recover_owed_children(&fixture.parent, &fixture.runtime).await;
    assert!(!owed(&fixture, &id));
    assert_eq!(
        answer(&received).await,
        SessionObservation::SteerAck {
            state: "queued".into(),
            position: None,
        },
        "the mail started the rebound child's turn"
    );
    assert!(service.attached(child), "the face stays on the node");

    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

/// The face's interrupt of an owed child takes effect at once: the child
/// reads interrupted, its attempt stays open, and the tree records the
/// operator as the cause. Once the node is closed, it takes no attachment
/// and no control.
#[tokio::test]
async fn a_face_interrupt_of_an_owed_child_takes_effect() {
    let (fixture, id) = owed_child("face-owed-interrupt").await;
    let status = fixture
        .runtime
        .mutate_then_emit(false, {
            let id = id.clone();
            move |tree| crate::topology_navigator::operator_interrupt(tree, &id, "face-interrupt")
        })
        .await
        .unwrap();
    assert_eq!(status, NodeStatus::Interrupted);
    let runtime = Arc::clone(&fixture.runtime);
    runtime
        .read(|tree| {
            assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Interrupted);
            assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Running);
            let causes: Vec<_> = tree
                .operations()
                .values()
                .filter_map(|operation| crate::agent_tree::interrupt_cause(operation).unwrap())
                .collect();
            assert!(
                causes.contains(&Some(InterruptCause::Operator)),
                "{causes:?}"
            );
        })
        .unwrap();
    let (service, _intents, node) = face_service(&fixture, &id);
    let child = service.attach(&node).expect("an interrupted child is open");

    // A closed node takes no attachment and no control.
    fixture
        .runtime
        .mutate_then_emit(false, {
            let id = id.clone();
            move |tree| {
                tree.close_agent(
                    "root",
                    &AttemptScopedRequest {
                        agent_id: id,
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
    assert_eq!(service.attach(&node), Err(RequestRefusal::NodeClosed));
    assert_eq!(
        service.intent(child, SessionIntent::Interrupt),
        Err(RequestRefusal::NodeClosed)
    );
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}
