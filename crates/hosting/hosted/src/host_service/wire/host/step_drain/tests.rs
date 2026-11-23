use super::*;
use crate::agent_tree::{ContinuationCause, InterruptCause};

async fn detached_tree(id: &str) -> (crate::agent::RecoveryFixture, HostStepDrain, String) {
    let fixture = crate::agent::RecoveryFixture::new(id);
    let [_, parked, _] = fixture.handoff_children().await;
    let runtime = crate::topology_runtime::lookup(id).unwrap();
    let hold = runtime.pause_step_admissions().await.unwrap();
    let drain = HostStepDrain {
        trees: BTreeMap::from([(id.into(), hold)]),
        drivers: BTreeMap::new(),
        owners: Vec::new(),
    };
    (fixture, drain, parked)
}

#[tokio::test]
async fn host_step_drain_records_a_tree_without_a_resident_root() {
    let id = "step-drain-detached-tree";
    let (fixture, drain, child) = detached_tree(id).await;
    assert_eq!(
        drain.record_child_handoffs(Duration::from_secs(5)).await,
        Vec::<(String, String)>::new()
    );
    assert_eq!(
        fixture.stored_continuation_causes()[&child],
        Some(ContinuationCause::Handoff)
    );
    drop(drain);
    crate::topology_runtime::force_release(id).await;
}

#[tokio::test]
async fn host_step_drain_zero_child_record_budget_writes_nothing() {
    let id = "step-drain-zero-record-budget";
    let (fixture, drain, child) = detached_tree(id).await;
    let before = fixture.stored_continuation_causes();
    assert_eq!(before[&child], Some(ContinuationCause::OwnerLost));
    let failures = drain.record_child_handoffs(Duration::ZERO).await;
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, id);
    assert!(failures[0].1.contains("budget"));
    assert_eq!(fixture.stored_continuation_causes(), before);
    drop(drain);
    crate::topology_runtime::force_release(id).await;
}

#[tokio::test]
async fn late_root_cascade_preserves_only_the_committed_upgrade_handoff() {
    let id = "step-drain-late-root-cascade";
    let (fixture, drain, child) = detached_tree(id).await;
    assert_eq!(
        drain.record_child_handoffs(Duration::from_secs(5)).await,
        Vec::<(String, String)>::new()
    );
    crate::runtime::cascade_root_for_step_drain_test(id)
        .await
        .unwrap();
    assert_eq!(
        fixture.stored_continuation_causes()[&child],
        Some(ContinuationCause::Handoff)
    );
    drop(drain);
    crate::runtime::cascade_root_for_step_drain_test(id)
        .await
        .unwrap();
    let runtime = crate::topology_runtime::lookup(id).unwrap();
    assert_eq!(
        runtime
            .read(|tree| tree.interrupt_cause_of(&child))
            .unwrap(),
        Some(InterruptCause::Cascade),
    );
    crate::topology_runtime::force_release(id).await;
}
