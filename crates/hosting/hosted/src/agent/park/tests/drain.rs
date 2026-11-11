use super::*;

async fn drain_without_a_terminal(fixture: &Fixture) {
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.runtime.cancel_live(&fixture.child),
    )
    .await
    .unwrap()
    .unwrap();
    fixture
        .until("drained turn waiter", |tree, child| {
            !tree.has_turn_waiter(child)
        })
        .await;
    fixture
        .runtime
        .read(|tree| {
            assert_eq!(
                tree.node(&fixture.child).unwrap().status,
                NodeStatus::Running
            );
            let attempt = tree.attempt(&fixture.child).unwrap();
            assert_eq!(attempt.status, AttemptStatus::Running);
            assert_eq!(attempt.usage.turns, 1);
            assert_eq!(tree.pending_mail("root").len(), 0);
            assert!(tree.fault_reason(&fixture.child).is_none());
        })
        .unwrap();
}

#[tokio::test]
async fn placement_hold_cut_nudge_drain_keeps_owner_loss_debt_without_result_mail() {
    let vcs = Arc::new(crate::occupancy_vcs::fake::FakeVcs::new());
    let fixture = fixture(json!([held("unsubmitted"), held("corrective")]), |tree| {
        tree.set_vcs(vcs.clone());
        leaf(tree)
    })
    .await;
    fixture.held_reached().await;
    let path = fixture
        .runtime
        .read(|tree| tree.node(&fixture.child).unwrap().path.clone())
        .unwrap();
    vcs.set_placement_ready(path, false);
    let reached = Arc::new(tokio::sync::Notify::new());
    hooks::set_before_submit_nudge(
        &fixture.child,
        Arc::new({
            let reached = reached.clone();
            move || {
                reached.notify_one();
                Box::pin(async {})
            }
        }),
    );
    fixture.provider_gates[0].release();
    tokio::time::timeout(Duration::from_secs(5), reached.notified())
        .await
        .unwrap();
    drain_without_a_terminal(&fixture).await;
}

#[tokio::test]
async fn placement_hold_cut_transient_instruction_drain_keeps_owner_loss_debt_without_result_mail()
{
    let probing = Arc::new(tokio::sync::Notify::new());
    let online = Arc::new(tokio::sync::Notify::new());
    let probe = plakous::model::ReachabilityProbe::new(
        plakous::model::RouteId::new("offline.drain").unwrap(),
        {
            let probing = probing.clone();
            let online = online.clone();
            move |_, _| {
                probing.notify_one();
                let online = online.clone();
                Box::pin(async move {
                    online.notified().await;
                    Ok(())
                })
            }
        },
    );
    let vcs = Arc::new(crate::occupancy_vcs::fake::FakeVcs::new());
    let fixture = fixture_with_probe(json!([
        {"fault": {"at": "pre_stream", "kind": "transport", "phase": "Connect", "retryability": "Exhausted", "message": "offline network failure"}, "output": {"text": "unused"}},
        held("continued")
    ]), |tree| {
        tree.set_vcs(vcs.clone());
        let mut epoch = tree.current_epoch().clone();
        epoch.transient_resumes = 1;
        tree.install_snapshot(epoch);
        leaf(tree)
    }, Some(probe)).await;
    tokio::time::timeout(Duration::from_secs(5), probing.notified())
        .await
        .unwrap();
    let path = fixture
        .runtime
        .read(|tree| tree.node(&fixture.child).unwrap().path.clone())
        .unwrap();
    let reached = Arc::new(tokio::sync::Notify::new());
    hooks::set_before_instruction_admission(
        &fixture.child,
        Arc::new({
            let reached = reached.clone();
            move || {
                vcs.set_placement_ready(path.clone(), false);
                reached.notify_one();
                Box::pin(async {})
            }
        }),
    );
    online.notify_one();
    tokio::time::timeout(Duration::from_secs(5), reached.notified())
        .await
        .unwrap();
    drain_without_a_terminal(&fixture).await;
}
