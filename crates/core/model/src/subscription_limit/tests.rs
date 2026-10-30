use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use super::*;

#[derive(Default)]
struct ManualClock(AtomicU64);

impl ManualClock {
    fn advance(&self, seconds: u64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

impl SubscriptionGateClock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_secs(self.0.load(Ordering::SeqCst))
    }
}

fn gate(clock: Arc<ManualClock>) -> InMemorySubscriptionRequestGate {
    InMemorySubscriptionRequestGate::new(
        SubscriptionRequestLimits::new(0, 1, 5, 4_096, Some(Duration::from_secs(300)), 0)
            .expect("fixed limits are valid"),
        clock,
    )
}

#[test]
fn proven_not_started_refunds_count_and_input_without_reusing_authorization() {
    let gate = gate(Arc::new(ManualClock::default()));
    let before = gate.snapshot().expect("snapshot").inference;
    let mut permit = gate
        .acquire_inference(4_096)
        .expect("whole input allowance");
    permit.authorize_dispatch().expect("dispatch");
    permit.refund_not_started().expect("proven unsent refund");
    assert_eq!(gate.snapshot().expect("snapshot").inference, before);
    assert_eq!(
        permit.authorize_dispatch(),
        Err(SubscriptionLimitError::PermitAlreadyUsed)
    );
    assert_eq!(
        permit.refund_not_started(),
        Err(SubscriptionLimitError::PermitAlreadyUsed)
    );
    drop(permit);
    assert_eq!(gate.snapshot().expect("snapshot").inference, before);
    assert!(gate.acquire_inference(4_096).is_ok());
}

#[test]
fn zero_refresh_one_catalog_and_five_inference_are_exact() {
    let gate = gate(Arc::new(ManualClock::default()));
    assert_eq!(
        gate.acquire(SubscriptionRequestClass::AuthRefresh).err(),
        Some(SubscriptionLimitError::ClassLimitExceeded {
            class: SubscriptionRequestClass::AuthRefresh,
            reserved: 0,
            allowance: 0,
        })
    );
    let mut catalog = gate
        .acquire(SubscriptionRequestClass::Catalog)
        .expect("catalog permit");
    catalog.authorize_dispatch().expect("catalog dispatch");
    assert_eq!(
        catalog.authorize_dispatch(),
        Err(SubscriptionLimitError::PermitAlreadyUsed)
    );
    assert!(gate.acquire(SubscriptionRequestClass::Catalog).is_err());
    for _ in 0..5 {
        gate.acquire_inference(100)
            .expect("inference permit")
            .authorize_dispatch()
            .expect("inference dispatch");
    }
    assert_eq!(
        gate.acquire(SubscriptionRequestClass::Inference).err(),
        Some(SubscriptionLimitError::InferenceBoundRequired)
    );
    assert_eq!(
        gate.acquire_inference(100).err(),
        Some(SubscriptionLimitError::ClassLimitExceeded {
            class: SubscriptionRequestClass::Inference,
            reserved: 5,
            allowance: 5,
        })
    );

    let snapshot = gate.snapshot().expect("snapshot");
    assert_eq!(snapshot.retries(), 0);
    assert_eq!(
        snapshot.class(SubscriptionRequestClass::AuthRefresh),
        SubscriptionClassSnapshot {
            allowance: 0,
            reserved: 0,
            dispatched: 0,
            abandoned: 0,
            denied: 1,
            input_token_upper_bound: 0,
        }
    );
    assert_eq!(
        snapshot
            .class(SubscriptionRequestClass::Inference)
            .dispatched(),
        5
    );
    assert_eq!(
        snapshot
            .class(SubscriptionRequestClass::Inference)
            .input_token_upper_bound(),
        500
    );
}

#[test]
fn abandoned_permit_consumes_capacity_and_cannot_mint_retry() {
    let gate = gate(Arc::new(ManualClock::default()));
    drop(
        gate.acquire(SubscriptionRequestClass::Catalog)
            .expect("catalog permit"),
    );
    assert_eq!(
        gate.acquire(SubscriptionRequestClass::Catalog).err(),
        Some(SubscriptionLimitError::ClassLimitExceeded {
            class: SubscriptionRequestClass::Catalog,
            reserved: 1,
            allowance: 1,
        })
    );
    let state = gate
        .snapshot()
        .expect("snapshot")
        .class(SubscriptionRequestClass::Catalog);
    assert_eq!(state.reserved(), 1);
    assert_eq!(state.dispatched(), 0);
    assert_eq!(state.abandoned(), 1);
    assert_eq!(state.denied(), 1);
}

#[test]
fn one_absolute_deadline_applies_to_existing_and_new_permits() {
    let clock = Arc::new(ManualClock::default());
    let gate = gate(clock.clone());
    let mut permit = gate.acquire_inference(1).expect("pre-deadline permit");
    clock.advance(300);
    assert_eq!(
        permit.authorize_dispatch(),
        Err(SubscriptionLimitError::DeadlineExceeded)
    );
    assert_eq!(
        gate.acquire_inference(1).err(),
        Some(SubscriptionLimitError::DeadlineExceeded)
    );
    drop(permit);
    let snapshot = gate.snapshot().expect("snapshot");
    assert!(snapshot.deadline_reached());
    assert_eq!(snapshot.elapsed(), Duration::from_secs(300));
    assert_eq!(snapshot.max_elapsed(), Some(Duration::from_secs(300)));
}

#[test]
fn concurrent_reservations_never_overshoot() {
    let gate = Arc::new(gate(Arc::new(ManualClock::default())));
    let threads = (0..32)
        .map(|_| {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                gate.acquire_inference(1)
                    .map(|mut permit| permit.authorize_dispatch())
            })
        })
        .collect::<Vec<_>>();
    let successes = threads
        .into_iter()
        .map(thread::JoinHandle::join)
        .filter(|result| matches!(result, Ok(Ok(Ok(())))))
        .count();
    assert_eq!(successes, 5);
    let state = gate
        .snapshot()
        .expect("snapshot")
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(state.reserved(), 5);
    assert_eq!(state.dispatched(), 5);
    assert_eq!(state.denied(), 27);
}

#[test]
fn scope_handles_prevent_a_b_a_renewal_and_keep_truthful_deadlines() {
    let clock = Arc::new(ManualClock::default());
    let limits = SubscriptionRequestLimits::new(0, 1, 2, 10, Some(Duration::from_secs(5)), 0)
        .expect("limits");
    let gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
        limits,
        clock.clone(),
    ));
    let first = SubscriptionInferenceScope::turn([1; 16]).expect("first scope");
    let second = SubscriptionInferenceScope::turn([2; 16]).expect("second scope");

    assert_eq!(
        gate.acquire_inference(1).err(),
        Some(SubscriptionLimitError::InferenceScopeUnavailable),
        "inference before a claimed root must fail closed"
    );
    let first_lease = gate.claim_scope(first).expect("claim first");
    gate.acquire_inference(1)
        .expect("the sole claimed root is unambiguous without a poll-local enter")
        .authorize_dispatch()
        .expect("sole-root dispatch");
    let first_handle = first_lease.handle();
    let first_active = first_handle.enter();
    gate.acquire_inference(5)
        .expect("count permit")
        .authorize_dispatch()
        .expect("count dispatch");
    assert_eq!(
        gate.acquire_inference(1).err(),
        Some(SubscriptionLimitError::ClassLimitExceeded {
            class: SubscriptionRequestClass::Inference,
            reserved: 2,
            allowance: 2,
        })
    );
    drop(first_active);
    let second_lease = gate.claim_scope(second).expect("claim second");
    let second_handle = second_lease.handle();
    let second_active = second_handle.enter();
    gate.acquire_inference(1)
        .expect("second scope is independent")
        .authorize_dispatch()
        .expect("second dispatch");
    drop(second_active);

    let first_again = gate.bind_scope(first).expect("rebind live first");
    assert_eq!(first_again.scope(), first);
    let first_again_active = first_again.enter();
    assert_eq!(
        gate.acquire_inference(1).err(),
        Some(SubscriptionLimitError::ClassLimitExceeded {
            class: SubscriptionRequestClass::Inference,
            reserved: 2,
            allowance: 2,
        })
    );
    drop(first_again_active);

    let _second_active = second_handle.enter();
    assert_eq!(
        gate.acquire_inference(11).err(),
        Some(SubscriptionLimitError::InferenceInputLimitExceeded)
    );
    let mut before_deadline = gate.acquire_inference(1).expect("deadline permit");
    clock.advance(5);
    assert_eq!(
        before_deadline.authorize_dispatch(),
        Err(SubscriptionLimitError::DeadlineExceeded)
    );
    assert_eq!(
        gate.acquire_inference(1).err(),
        Some(SubscriptionLimitError::DeadlineExceeded)
    );

    let snapshot = second_handle.snapshot().expect("scope snapshot");
    assert!(snapshot.deadline_reached());
    assert_eq!(snapshot.elapsed(), Duration::from_secs(5));
    let state = snapshot.class(SubscriptionRequestClass::Inference);
    assert_eq!(state.reserved(), 2);
    assert_eq!(state.dispatched(), 1);
    assert_eq!(state.denied(), 2);
    assert_eq!(state.input_token_upper_bound(), 2);

    drop(first_lease);
    assert_eq!(
        gate.claim_scope(first).err(),
        Some(SubscriptionLimitError::InferenceScopeUnavailable)
    );
    drop(second_lease);
}

#[test]
fn concurrent_scope_activation_never_cross_charges() {
    let clock = Arc::new(ManualClock::default());
    let limits = SubscriptionRequestLimits::new(0, 1, 1, 10, Some(Duration::from_secs(5)), 0)
        .expect("limits");
    let gate = Arc::new(TurnScopedSubscriptionRequestGate::new(limits, clock));
    let first_lease = gate
        .claim_scope(SubscriptionInferenceScope::turn([1; 16]).expect("first"))
        .expect("claim first");
    let second_lease = gate
        .claim_scope(SubscriptionInferenceScope::turn([2; 16]).expect("second"))
        .expect("claim second");
    let first = first_lease.handle();
    let second = second_lease.handle();
    let threads = [first.clone(), second.clone()].map(|scope| {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            let _active = scope.enter();
            gate.acquire_inference(3)?.authorize_dispatch()
        })
    });
    for thread in threads {
        assert_eq!(thread.join().expect("thread"), Ok(()));
    }
    for scope in [first, second] {
        let inference = scope
            .snapshot()
            .expect("scope snapshot")
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 1);
        assert_eq!(inference.dispatched(), 1);
        assert_eq!(inference.input_token_upper_bound(), 3);
    }
}

#[test]
fn dyn_gate_acquires_after_typed_claim_without_tls_and_after_lease_drop() {
    let clock = Arc::new(ManualClock::default());
    let limits =
        SubscriptionRequestLimits::new(1, 1, 8, 1_000_000, Some(Duration::from_secs(30)), 0)
            .expect("limits");
    let typed = Arc::new(TurnScopedSubscriptionRequestGate::new(limits, clock));
    let dyn_gate: Arc<dyn SubscriptionRequestGate> = typed.clone();
    let scope = SubscriptionInferenceScope::turn([9; 16]).expect("scope");
    let lease = typed.claim_scope(scope).expect("claim");
    dyn_gate
        .acquire_inference(100_000)
        .expect("manager-style dyn acquire must succeed on the claimed root without TLS")
        .authorize_dispatch()
        .expect("dispatch");
    drop(lease);
    assert_eq!(
        dyn_gate.acquire_inference(1).err(),
        Some(SubscriptionLimitError::InferenceScopeUnavailable)
    );
}

#[test]
fn claimed_root_inference_is_independent_of_a_saturated_process_class() {
    let clock = Arc::new(ManualClock::default());
    let limits =
        SubscriptionRequestLimits::new(1, 1, 8, 1_000_000, Some(Duration::from_secs(30)), 0)
            .expect("limits");
    let typed = Arc::new(TurnScopedSubscriptionRequestGate::new(limits, clock));
    let dyn_gate: Arc<dyn SubscriptionRequestGate> = typed.clone();
    let global_permits = (0..8)
        .map(|_| {
            typed
                .global
                .acquire_inference(1)
                .expect("process class reservation")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        typed.global.acquire_inference(1).err(),
        Some(SubscriptionLimitError::ClassLimitExceeded {
            class: SubscriptionRequestClass::Inference,
            reserved: 8,
            allowance: 8,
        })
    );
    let lease = typed
        .claim_scope(SubscriptionInferenceScope::turn([3; 16]).expect("scope"))
        .expect("claim");
    dyn_gate
        .acquire_inference(1)
        .expect("claimed root must not inherit a saturated process inference class")
        .authorize_dispatch()
        .expect("root dispatch");
    drop(lease);
    drop(global_permits);
}

/// A product gate has no elapsed clock, and a turn's refreshes are the
/// turn's: an attached session spanning hours keeps its auth and catalog
/// traffic, and a turn longer than any former deadline keeps dispatching.
#[test]
fn unclocked_gate_charges_refreshes_to_the_live_root_for_hours() {
    let clock = Arc::new(ManualClock::default());
    let limits = SubscriptionRequestLimits::new(1, 1, 8, 1_000_000, None, 0).expect("limits");
    let gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
        limits,
        clock.clone(),
    ));
    let mut resolution = gate
        .acquire(SubscriptionRequestClass::Catalog)
        .expect("resolution catalog");
    resolution
        .authorize_dispatch()
        .expect("resolution dispatch");
    for (turn, seed) in [(1_u64, 1_u8), (2, 2), (3, 3)] {
        clock.advance(turn * 3_600);
        let lease = gate
            .claim_scope(SubscriptionInferenceScope::turn([seed; 16]).expect("scope"))
            .expect("claim");
        for class in [
            SubscriptionRequestClass::AuthRefresh,
            SubscriptionRequestClass::Catalog,
        ] {
            gate.acquire(class)
                .expect("in-turn refresh is charged to the root")
                .authorize_dispatch()
                .expect("in-turn refresh dispatch");
        }
        clock.advance(3_600);
        gate.acquire_inference(1)
            .expect("an hour-long turn keeps inference")
            .authorize_dispatch()
            .expect("inference dispatch");
        assert!(!gate.snapshot().expect("snapshot").deadline_reached());
        drop(lease);
    }
    let global = gate.global.snapshot().expect("global snapshot");
    assert_eq!(global.max_elapsed(), None);
    assert_eq!(
        global.class(SubscriptionRequestClass::Catalog).reserved(),
        1
    );
    assert_eq!(
        global
            .class(SubscriptionRequestClass::AuthRefresh)
            .reserved(),
        0
    );
}

#[test]
fn two_live_roots_do_not_cross_charge_without_tls() {
    let clock = Arc::new(ManualClock::default());
    let limits =
        SubscriptionRequestLimits::new(1, 1, 8, 1_000_000, Some(Duration::from_secs(30)), 0)
            .expect("limits");
    let gate = Arc::new(TurnScopedSubscriptionRequestGate::new(limits, clock));
    let first = gate
        .claim_scope(SubscriptionInferenceScope::turn([1; 16]).expect("first"))
        .expect("claim first");
    let _second = gate
        .claim_scope(SubscriptionInferenceScope::turn([2; 16]).expect("second"))
        .expect("claim second");
    assert_eq!(
        gate.acquire_inference(1).err(),
        Some(SubscriptionLimitError::InferenceScopeUnavailable)
    );
    let _active = first.handle().enter();
    gate.acquire_inference(1)
        .expect("TLS selects the entered root")
        .authorize_dispatch()
        .expect("dispatch");
}

#[test]
fn eight_abandoned_inference_permits_report_reserved_and_allowance() {
    let limits =
        SubscriptionRequestLimits::new(1, 2, 8, 1_000_000, Some(Duration::from_secs(300)), 0)
            .expect("limits");
    let gate = InMemorySubscriptionRequestGate::new(limits, Arc::new(ManualClock::default()));
    let permits = (0..8)
        .map(|_| gate.acquire_inference(1).expect("inference reservation"))
        .collect::<Vec<_>>();
    assert_eq!(
        gate.acquire_inference(1).err(),
        Some(SubscriptionLimitError::ClassLimitExceeded {
            class: SubscriptionRequestClass::Inference,
            reserved: 8,
            allowance: 8,
        })
    );
    let state = gate
        .snapshot()
        .expect("snapshot")
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(state.reserved(), 8);
    assert_eq!(state.dispatched(), 0);
    assert_eq!(state.abandoned(), 0);
    assert_eq!(state.denied(), 1);
    drop(permits);
    let abandoned = gate
        .snapshot()
        .expect("snapshot")
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(abandoned.reserved(), 8);
    assert_eq!(abandoned.abandoned(), 8);
    assert_eq!(
        gate.acquire_inference(1).err(),
        Some(SubscriptionLimitError::ClassLimitExceeded {
            class: SubscriptionRequestClass::Inference,
            reserved: 8,
            allowance: 8,
        })
    );
}
