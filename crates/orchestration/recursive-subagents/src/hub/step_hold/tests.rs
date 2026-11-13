use super::*;
use crate::hub::{LiveRelease, test_support};
use crate::protocol::SpawnAgentRequest;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

async fn fixture(root: &std::path::Path) -> TestResult<(Arc<ProductTopologyRuntime>, String)> {
    let runtime = Arc::new(ProductTopologyRuntime::new(
        test_support::bootstrap(&test_support::unique_tree_id(), root.to_path_buf()),
        test_support::memory_store(),
        Arc::new(plakous_workspace::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let id = runtime
        .mutate_then_emit(false, |tree| {
            tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .map(|created| created.agent_id)
                .map_err(|error| error.message)
        })
        .await?;
    Ok((runtime, id))
}

#[tokio::test]
async fn tree_step_pause_allows_dispatched_startup_and_rollback_reopens_the_same_tree() -> TestResult
{
    let root = tempfile::tempdir()?;
    let (runtime, id) = fixture(root.path()).await?;
    let pause = runtime.pause_step_admissions().await?;
    let startup = runtime.reserve_startup(&id).await?;
    assert!(!startup.is_cancelled());
    assert!(runtime.pause_step_admissions().await.is_err());
    assert!(!runtime.is_closed());
    assert!(pause.agents().await?.transitioning);
    drop(pause);
    runtime
        .finish_startup(&id, &startup, LiveRelease::default())
        .await;
    assert!(!runtime.is_closed());
    assert!(runtime.children_quiet().await);
    Ok(())
}

#[tokio::test]
async fn tree_step_pause_joins_existing_startup_without_cancelling_its_owner() -> TestResult {
    let root = tempfile::tempdir()?;
    let (runtime, id) = fixture(root.path()).await?;
    let startup = runtime.reserve_startup(&id).await?;
    let pause = runtime.pause_step_admissions().await?;
    assert!(pause.agents().await?.transitioning);
    assert!(!startup.is_cancelled());
    runtime
        .finish_startup(&id, &startup, LiveRelease::default())
        .await;
    assert!(!pause.agents().await?.transitioning);
    assert!(pause.effects_joined());
    drop(pause);
    Ok(())
}

#[tokio::test]
async fn tree_step_pause_sees_a_recovery_claim_before_its_task_is_registered() -> TestResult {
    let root = tempfile::tempdir()?;
    let (runtime, id) = fixture(root.path()).await?;
    let retries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    runtime
        .set_retirement_recovery(Arc::new({
            let retries = Arc::clone(&retries);
            move || {
                retries.fetch_add(1, Ordering::AcqRel);
            }
        }))
        .await;
    let claim = runtime
        .claim_recovery(&id)
        .map_err(|error| format!("{error:?}"))?;
    let pause = runtime.pause_step_admissions().await?;
    assert!(!pause.effects_joined());
    assert!(matches!(
        runtime.claim_recovery("later-child"),
        Err(crate::hub::RecoveryClaimRefusal::StepHeld)
    ));
    drop(claim);
    assert!(pause.effects_joined());
    drop(pause);
    assert_eq!(
        retries.load(Ordering::Acquire),
        1,
        "rollback must retain the deferred host retry"
    );
    assert!(runtime.claim_recovery(&id).is_ok());
    Ok(())
}

#[tokio::test]
async fn tree_step_pause_effect_joins_survive_caller_loss_but_do_not_wait_for_actor_lifetimes()
-> TestResult {
    let root = tempfile::tempdir()?;
    let (runtime, _) = fixture(root.path()).await?;
    let pause = runtime.pause_step_admissions().await?;
    let (keep, kept) = tokio::sync::oneshot::channel::<()>();
    let actor = runtime.driver().spawn_release_owner(async move {
        let _ = kept.await;
    });
    assert!(
        pause.effects_joined(),
        "retained owners alone are not executing effects"
    );
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let (done, finished) = tokio::sync::oneshot::channel();
    let driver = runtime.driver().clone();
    let caller = tokio::spawn(async move {
        driver
            .owned(async move {
                let _ = entered.send(());
                let _ = released.await;
                let _ = done.send(());
                Ok(())
            })
            .await
    });
    entering.await?;
    assert!(!pause.effects_joined());
    caller.abort();
    let _ = caller.await;
    assert!(
        !pause.effects_joined(),
        "caller cancellation cannot hide still-owned work"
    );
    release.send(()).map_err(|()| "effect owner lost")?;
    finished.await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !pause.effects_joined() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    keep.send(()).map_err(|()| "retained actor lost")?;
    actor.await?;
    Ok(())
}

#[tokio::test]
async fn tree_step_pause_waits_for_release_without_faulting_or_losing_a_startup() -> TestResult {
    let root = tempfile::tempdir()?;
    let (runtime, id) = fixture(root.path()).await?;
    let pause = runtime.pause_step_admissions().await?;
    let mut wait = Box::pin(
        runtime.wait_step_release(tokio::time::Instant::now() + std::time::Duration::from_secs(10)),
    );
    assert!(futures_util::poll!(wait.as_mut()).is_pending());
    assert_eq!(
        runtime.wait_step_release(tokio::time::Instant::now()).await,
        Err(crate::hub::RecoveryClaimRefusal::StepHeld)
    );
    let startup = runtime.reserve_startup(&id).await?;
    assert!(!startup.is_cancelled());
    drop(pause);
    wait.await.map_err(|_| "release was not observed")?;
    runtime
        .finish_startup(&id, &startup, LiveRelease::default())
        .await;
    assert!(runtime.children_quiet().await);
    Ok(())
}
