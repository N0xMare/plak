//! Capacity parking retries publication, never accepted work.

use super::io::{StoreCallError, StorePhase, capacity_store_call};
use crate::SessionStoreError;
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn capacity_outage_retains_one_mutation_until_an_authoritative_write() {
    let mut attempts = 0;
    let started = tokio::time::Instant::now();
    let result = capacity_store_call(
        StorePhase::Commit,
        Duration::from_secs(30),
        || {
            attempts += 1;
            std::future::ready(if attempts < 10 {
                Err(SessionStoreError::StorageFull)
            } else {
                Ok("the retained receipt")
            })
        },
        || false,
    )
    .await;
    assert!(matches!(result, Ok("the retained receipt")));
    assert_eq!(attempts, 10);
    assert_eq!(started.elapsed(), Duration::from_secs(45));
}

#[tokio::test(start_paused = true)]
async fn capacity_outage_stops_only_on_owner_cancellation_without_losing_its_class() {
    let mut attempts = 0;
    let started = tokio::time::Instant::now();
    let result: Result<(), _> = capacity_store_call(
        StorePhase::Commit,
        Duration::from_secs(12),
        || {
            attempts += 1;
            std::future::ready(Err(SessionStoreError::StorageFull))
        },
        || started.elapsed() >= Duration::from_secs(45),
    )
    .await;
    assert!(matches!(
        result,
        Err(StoreCallError::Store(SessionStoreError::StorageFull))
    ));
    assert!(attempts >= 9);
}

#[tokio::test(start_paused = true)]
async fn ambiguous_or_fenced_writes_are_never_capacity_retried() {
    for failure in [
        SessionStoreError::BackendUnavailable,
        SessionStoreError::FenceSuperseded,
        SessionStoreError::corrupt(),
    ] {
        let mut attempts = 0;
        let result: Result<(), _> = capacity_store_call(
            StorePhase::Commit,
            Duration::from_secs(30),
            || {
                attempts += 1;
                std::future::ready(Err(failure))
            },
            || false,
        )
        .await;
        assert!(matches!(result, Err(StoreCallError::Store(error)) if error == failure));
        assert_eq!(attempts, 1);
    }
}
