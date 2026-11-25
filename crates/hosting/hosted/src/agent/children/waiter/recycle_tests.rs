use super::*;
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::agent) enum Fault {
    LeaseFence,
    RestoredTurn,
    Release,
}

static FAULTS: Mutex<BTreeMap<String, Fault>> = Mutex::new(BTreeMap::new());

pub(in crate::agent) fn install(driver: &TreeDriver, fault: Fault) {
    FAULTS
        .lock()
        .unwrap()
        .insert(driver.read(|tree| tree.tree_id.clone()).unwrap(), fault);
}

fn take(driver: &TreeDriver, fault: Fault) -> bool {
    let tree = driver.read(|tree| tree.tree_id.clone()).unwrap();
    let mut faults = FAULTS.lock().unwrap();
    if faults.get(&tree) != Some(&fault) {
        return false;
    }
    faults.remove(&tree);
    true
}

pub(super) fn lease_fence(
    driver: &TreeDriver,
    turn: TurnId,
    observed: Option<TurnId>,
) -> Option<TurnId> {
    take(driver, Fault::LeaseFence).then_some(turn).or(observed)
}

pub(super) fn restored_turn(driver: &TreeDriver, observed: bool) -> bool {
    take(driver, Fault::RestoredTurn) || observed
}

pub(super) fn released<T>(driver: &TreeDriver, observed: Result<T, String>) -> Result<T, String> {
    if take(driver, Fault::Release) {
        return Err("injected reopened child release refusal".into());
    }
    observed
}

#[test]
fn a_fence_terminal_for_a_different_turn_cannot_complete_the_submission() {
    let waited = "00000000-0000-4000-8000-000000000001".parse().unwrap();
    let terminal = crate::agent_tree::ChildTerminal {
        attempt_id: crate::agent_tree_protocol::AttemptId::new("child", 1),
        turn_id: "00000000-0000-4000-8000-000000000002".into(),
        committed_at_ms: 1,
        outcome: plakous::sessions::feed::FeedTurnOutcome::RecoveryRequired,
        seq: 1,
    };
    assert!(
        recyclable_fence_terminal(
            Some(&FencedChildRecycle::Quiescent {
                restored_turn: false
            }),
            None,
            waited,
            Some(terminal),
        )
        .is_none()
    );
}
