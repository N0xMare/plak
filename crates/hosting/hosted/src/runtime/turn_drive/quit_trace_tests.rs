//! Which quit branch each session took and the order of its durable writes,
//! plus injected cancel refusals and join expiry. Test builds only.

use std::sync::{Mutex, PoisonError};
use std::{future::Future, pin::Pin};

type ResultValue = Result<plakous::agent::TurnResult, plakous::agent::AgentError>;
pub(super) type ResultSlot = tokio::sync::watch::Receiver<Option<ResultValue>>;
type ResultFuture = Pin<Box<dyn Future<Output = ResultValue> + Send>>;

static STEPS: Mutex<Vec<(String, &'static str)>> = Mutex::new(Vec::new());
static CANCEL_REFUSALS: Mutex<Vec<(String, plakous::agent::ControlError)>> = Mutex::new(Vec::new());
static EXPIRED_JOINS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Resume the next quit join with its result ready and its deadline expired.
/// Arm before the turn starts so the fixture can observe result readiness.
pub(in crate::runtime) fn expire_next_join(session: &str) {
    EXPIRED_JOINS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(session.to_owned());
}

/// Cancellation settlement can wake before the result future is notified.
/// Forward the selected result before expiring the join, so the drive's own
/// receiver is genuinely ready, not merely inside terminal publication.
pub(super) fn observe_result(
    session: &str,
    result: impl Future<Output = ResultValue> + Send + 'static,
) -> (ResultFuture, Option<ResultSlot>) {
    if !EXPIRED_JOINS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|id| id == session)
    {
        return (Box::pin(result), None);
    }
    let (published, receiver) = tokio::sync::watch::channel(None);
    drop(tokio::spawn(async move {
        published.send_replace(Some(result.await));
    }));
    let ready = receiver.clone();
    (Box::pin(ready_result(Some(receiver))), Some(ready))
}

pub(super) async fn ready_result(receiver: Option<ResultSlot>) -> ResultValue {
    let mut receiver =
        receiver.unwrap_or_else(|| panic!("join expiry must be armed before the turn starts"));
    match receiver.wait_for(Option::is_some).await {
        Ok(value) => value
            .clone()
            .unwrap_or_else(|| unreachable!("the ready predicate requires a result")),
        Err(error) => panic!("the selected result publisher ended without its result: {error}"),
    }
}

pub(super) fn take_expired_join(session: &str) -> bool {
    let mut joins = EXPIRED_JOINS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(index) = joins.iter().position(|id| id == session) {
        joins.remove(index);
        return true;
    }
    false
}

/// The quit path of `session` reached `step`.
pub(super) fn step(session: &str, step: &'static str) {
    STEPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((session.to_owned(), step));
}

/// The quit steps `session` reached, in order.
pub(in crate::runtime) fn steps(session: &str) -> Vec<&'static str> {
    STEPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|(stepped, _)| stepped == session)
        .map(|(_, step)| *step)
        .collect()
}

/// The next interrupt of `session` is refused at ingress with `error`.
pub(in crate::runtime) fn refuse_next_cancel(session: &str, error: plakous::agent::ControlError) {
    CANCEL_REFUSALS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((session.to_owned(), error));
}

pub(super) fn take_cancel_refusal(session: &str) -> Option<plakous::agent::ControlError> {
    let mut refusals = CANCEL_REFUSALS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let index = refusals
        .iter()
        .position(|(refused, _)| refused == session)?;
    Some(refusals.remove(index).1)
}
