//! Product gate limit tests.

use super::*;
use plakous::subscriptions::{
    SubscriptionInferenceScope, SubscriptionRequestClass, SubscriptionRequestGate,
    SystemSubscriptionGateClock, TurnScopedSubscriptionRequestGate,
};
use std::sync::Arc;

/// 2026-09-03: a 60-round Code Mode turn on a 272k model summed 1.02M
/// input tokens and died at the old 1M smoke budget with
/// `Responses fence-admission`. The product budget covers every admitted
/// round at a full envelope, so only the envelope decides a round.
#[test]
fn turn_input_budget_covers_every_admitted_round_at_full_envelope() {
    let limits = product_subscription_request_limits().unwrap();
    assert_eq!(
        limits.allowance(SubscriptionRequestClass::Inference),
        u32::try_from(NATIVE_RUN_TURN_INFERENCE_ALLOWANCE).unwrap()
    );
    assert_eq!(
        limits.max_inference_input_tokens(),
        NATIVE_RUN_TURN_INPUT_LIABILITY_BUDGET
    );
    assert!(
        limits.max_inference_input_tokens()
            >= NATIVE_RUN_TURN_INFERENCE_ALLOWANCE as u64
                * NATIVE_RUN_ROUND_INPUT_LIABILITY_CEILING
    );
    assert!(limits.max_inference_input_tokens() > 1_021_307);
}

/// The wrap-up round the operation budget buys must survive the gate.
///
/// The gate's inference scope is the root turn and it denies once `reserved`
/// reaches the allowance. The wrap-up round is request
/// `cap + 1` for the largest configurable cap, so an allowance equal to that cap
/// would refuse exactly the round that exists to rescue the turn — on every
/// OAuth account, and invisibly to the direct API tests, which have no gate.
#[test]
fn the_operation_budget_wrap_up_round_is_admitted_by_the_turn_gate() {
    let limits = product_subscription_request_limits().unwrap();
    let gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
        limits,
        Arc::new(SystemSubscriptionGateClock::default()),
    ));
    let scope = SubscriptionInferenceScope::turn([7; 16]).expect("turn scope");
    let lease = gate.claim_scope(scope).expect("claim the root turn");
    let handle = lease.handle();
    let _active = handle.enter();

    for round in 1..=NATIVE_RUN_MAX_MODEL_ROUNDS_CEILING {
        gate.acquire_inference(1)
            .unwrap_or_else(|error| panic!("budgeted round {round} was denied: {error:?}"))
            .authorize_dispatch()
            .unwrap_or_else(|error| panic!("budgeted round {round} did not dispatch: {error:?}"));
    }
    gate.acquire_inference(1)
        .expect("the wrap-up round must be admitted after the budgeted rounds")
        .authorize_dispatch()
        .expect("the wrap-up round must dispatch");
    // Generations replayed under the turn retry policy draw on the reserve,
    // not on the rounds.
    for retry in 1..=NATIVE_RUN_TURN_RETRY_RESERVE {
        gate.acquire_inference(1)
            .unwrap_or_else(|error| panic!("retry permit {retry} was denied: {error:?}"))
            .authorize_dispatch()
            .unwrap_or_else(|error| panic!("retry permit {retry} did not dispatch: {error:?}"));
    }

    // And the allowance still bounds the turn.
    assert!(
        gate.acquire_inference(1).is_err(),
        "the allowance must close the turn after its rounds and retry reserve"
    );
}

/// A product gate carries no elapsed clock: a turn has none, and the gate's
/// former 20-minute bound also stopped auth and catalog refreshes on every
/// session attached for longer than that.
#[test]
fn the_product_gate_has_no_elapsed_clock() {
    let limits = product_subscription_request_limits().unwrap();
    assert_eq!(limits.max_elapsed(), None);
}
