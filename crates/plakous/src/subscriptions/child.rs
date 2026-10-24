//! Child resolution retries begin only after the failed operation releases its authority.

use std::{future::Future, time::Duration};

use super::model_catalog::OPERATION_DEADLINE_CAUSE;
use super::{MODEL_CATALOG_DEADLINE, ModelId, NativeModelResolutionError, NativeSubscriptionError};

const STARTUP_MARGIN: Duration = Duration::from_secs(10);
const CATALOG_ATTEMPT: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(15);

#[cfg(test)]
pub(super) type BackoffHook =
    std::sync::Arc<dyn Fn() -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

impl super::NativeSubscriptions {
    pub(super) async fn child_lookup_backoff(&self, delay: Duration) {
        #[cfg(test)]
        if let Some(hook) = &self.child_backoff {
            hook().await;
            return;
        }
        tokio::time::sleep(delay).await;
    }
}

pub(super) async fn resolve<T, F, Fut, W, Wait>(
    mut operation: F,
    model: &ModelId,
    caller_deadline: Option<tokio::time::Instant>,
    mut wait: W,
) -> Result<T, NativeModelResolutionError>
where
    F: FnMut(tokio::time::Instant) -> Fut,
    Fut: Future<Output = Result<T, NativeModelResolutionError>>,
    W: FnMut(Duration) -> Wait,
    Wait: Future<Output = ()>,
{
    let now = tokio::time::Instant::now();
    let startup_deadline =
        (now + MODEL_CATALOG_DEADLINE).min(caller_deadline.unwrap_or(now + MODEL_CATALOG_DEADLINE));
    // Return a lookup failure before the retained startup owner's outer cut,
    // leaving session creation, Start acknowledgement and registration time.
    let deadline = startup_deadline.checked_sub(STARTUP_MARGIN).unwrap_or(now);
    let mut previous_attempts = 0_u8;
    for operation_attempt in 1..=2 {
        // Awaiting the whole operation drops its compatibility guard, manager,
        // lifecycle ownership and Ready lease before we consider sleeping.
        let error = match operation(deadline).await {
            Ok(resolved) => return Ok(resolved),
            Err(error) => error,
        };
        let NativeSubscriptionError::ModelLookup {
            attempts,
            cause,
            retry_after,
        } = error.kind()
        else {
            return Err(error);
        };
        let attempts = previous_attempts.saturating_add(attempts);
        let failure = NativeModelResolutionError::after_selection(
            model.clone(),
            NativeSubscriptionError::ModelLookup {
                attempts,
                cause,
                retry_after,
            },
        );
        if operation_attempt == 2 || cause == OPERATION_DEADLINE_CAUSE {
            return Err(failure);
        }
        let delay = plakous_exchange::lookup::retry_backoff(1, retry_after);
        if delay > MAX_BACKOFF
            || startup_deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .saturating_sub(delay)
                < CATALOG_ATTEMPT + STARTUP_MARGIN
        {
            return Err(failure);
        }
        previous_attempts = attempts;
        wait(delay).await;
    }
    unreachable!("the second resolution returns its result")
}

#[cfg(test)]
mod tests;
