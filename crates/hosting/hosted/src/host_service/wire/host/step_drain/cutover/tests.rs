use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};

#[tokio::test]
async fn host_step_drain_slow_join_keeps_its_tree_lock_future_across_progress_ticks() {
    let lock = tokio::sync::Mutex::new(());
    let acquisitions = AtomicUsize::new(0);
    let cancelled = AtomicBool::new(false);
    let result = join(
        async {
            let _guard = lock.lock().await;
            acquisitions.fetch_add(1, Ordering::AcqRel);
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok(())
        },
        Some(Instant::now() + Duration::from_secs(2)),
        &StepDrainMonitor {
            cancelled: &cancelled,
            progress: &|_| Ok(()),
        },
        || false,
    )
    .await;
    assert!(result.is_ok());
    assert_eq!(acquisitions.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn host_step_drain_slow_join_still_observes_cancellation_and_the_freeze_bound() {
    for cancel in [false, true] {
        let cancelled = AtomicBool::new(cancel);
        let result = join(
            std::future::pending::<CliResult<()>>(),
            Some(Instant::now() + Duration::from_millis(30)),
            &StepDrainMonitor {
                cancelled: &cancelled,
                progress: &|_| Ok(()),
            },
            || false,
        )
        .await
        .unwrap_err();
        assert!(result.message().contains(if cancel {
            "cancelled"
        } else {
            "host_step_drain_blocked"
        }));
    }
}
