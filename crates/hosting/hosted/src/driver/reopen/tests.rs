use super::*;

#[tokio::test(start_paused = true)]
async fn stale_adoption_does_not_restart_the_lease_budget_or_poll_identity() {
    let start = tokio::time::Instant::now();
    let mut retry = ExactRetry::with_lease_wait(Duration::from_secs(3));
    assert_eq!(
        retry.lease_delay(Duration::from_secs(1)),
        Some(Duration::from_secs(1))
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(retry.adopt());
    assert!(!retry.adopt());
    for expected in [2, 3] {
        assert_eq!(
            retry.lease_delay(Duration::from_secs(1)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(retry.lease_waits, expected);
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert_eq!(retry.lease_delay(Duration::from_secs(1)), None);
    assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(3));
}
