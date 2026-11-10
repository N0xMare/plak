//! A foreground command through the process tool on a runner placement
//! survives the placement's hold: its observation waits out the lost link
//! and reads again after release, and the process completes on its own.

use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, RunId};
use plakous_tools::internal::runner::{
    RunnerBackend, RunnerCall, RunnerCallFuture, RunnerClass, RunnerFailure, RunnerLink,
    RunnerServices, RunnerVerb, RunnerVerbFamily, ServeFailure, job_capacity, line_sessions,
    runner_descriptor, runner_environment, serve, serve_probe, serve_sessions,
};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    JoinedProcessSessions, LineOwner, OperationClass, OperationContext, RunnerId,
};

use crate::ProcessSessionTools;
use crate::canonical::CanonicalProcessStartRequest;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The runner side in process, over a link the test can lose: while lost,
/// a read sent before the loss is answered interrupted and anything else
/// is refused unsent, as the host client answers them.
struct LosableRunner {
    release_poll: Mutex<Option<(std::path::PathBuf, usize)>>,
    environment: ExecutionEnvironment,
    sessions: JoinedProcessSessions,
    /// The link is lost.
    lost: Arc<AtomicBool>,
    /// Lose the link once the start is answered, so the model's first
    /// observation meets the lost link.
    lose_after_start: bool,
    /// Refuse the next promotion unsent and lose the link with it.
    refuse_promotion: AtomicBool,
    interrupted: AtomicUsize,
    promotions: AtomicUsize,
    stops: AtomicUsize,
}

impl RunnerLink for LosableRunner {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        if call.verb() == RunnerVerb::ProcessPromote
            && self.refuse_promotion.swap(false, Ordering::SeqCst)
        {
            self.lost.store(true, Ordering::SeqCst);
        }
        if self.lost.load(Ordering::SeqCst) {
            let failure = if matches!(call.class(), RunnerClass::Observation { .. }) {
                self.interrupted.fetch_add(1, Ordering::SeqCst);
                RunnerFailure::ReadInterrupted("lost".into())
            } else {
                RunnerFailure::NotSent("lost".into())
            };
            return Box::pin(std::future::ready(Err(failure)));
        }
        if call.verb() == RunnerVerb::ProcessPoll {
            let mut release = self
                .release_poll
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((path, polls)) = release.as_mut() {
                *polls += 1;
                if *polls == 2 {
                    std::fs::write(path, b"release")
                        .unwrap_or_else(|error| panic!("release marker: {error}"));
                }
            }
        }
        match call.verb() {
            RunnerVerb::ProcessStop | RunnerVerb::ForegroundStop | RunnerVerb::JobsStop => {
                self.stops.fetch_add(1, Ordering::SeqCst);
            }
            RunnerVerb::ProcessPromote => {
                self.promotions.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        let control = OperationContext::new(call.cancellation().clone());
        let served = match call.verb().family() {
            RunnerVerbFamily::Probe => serve_probe(call.payload().clone(), &|_| true),
            RunnerVerbFamily::Environment => serve(
                &self.environment,
                call.verb().as_str(),
                call.payload().clone(),
                control,
            ),
            RunnerVerbFamily::Sessions => serve_sessions(
                &self.environment,
                &self.sessions,
                "instance",
                call.verb().as_str(),
                call.payload().clone(),
                control,
                false,
            ),
            _ => return Box::pin(std::future::ready(Err(RunnerFailure::Unsupported))),
        };
        let lose = (self.lose_after_start && call.verb() == RunnerVerb::ProcessStart)
            .then(|| Arc::clone(&self.lost));
        Box::pin(async move {
            let answer = served.await.map_err(|failure| match failure {
                ServeFailure::Backend { error, .. } => RunnerFailure::Backend(error),
                ServeFailure::Unsupported => RunnerFailure::Unsupported,
            });
            if let Some(lost) = lose {
                lost.store(true, Ordering::SeqCst);
            }
            answer
        })
    }
}

fn host(root: &Path) -> TestResult<ExecutionEnvironment> {
    Ok(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000581")?),
        root,
        ".",
        ExecutionEnvironmentConfig::default(),
    )?)
}

fn owner() -> TestResult<LineOwner> {
    Ok(LineOwner::new(
        LineId::from_str("00000000-0000-0000-0000-0000000000a1")?,
        1,
        BranchId::from_str("00000000-0000-0000-0000-0000000000b1")?,
        1,
    ))
}

/// A placed environment over a losable runner, and its tool call context.
struct Placed {
    root: tempfile::TempDir,
    runner: Arc<LosableRunner>,
    services: RunnerServices,
    environment: ExecutionEnvironment,
    control: OperationContext,
}

fn placed(lose_after_start: bool, refuse_promotion: bool) -> TestResult<Placed> {
    let root = tempfile::tempdir()?;
    let runner_side = host(root.path())?;
    let runner = Arc::new(LosableRunner {
        release_poll: Mutex::default(),
        sessions: line_sessions(&runner_side, &job_capacity(8))?,
        environment: runner_side.clone(),
        lost: Arc::new(AtomicBool::new(false)),
        lose_after_start,
        refuse_promotion: AtomicBool::new(refuse_promotion),
        interrupted: AtomicUsize::new(0),
        promotions: AtomicUsize::new(0),
        stops: AtomicUsize::new(0),
    });
    let descriptor = runner_descriptor(&runner_side, RunnerId::new("held")?)?;
    let link: Arc<dyn RunnerLink> = runner.clone();
    let services = RunnerServices::new(
        Arc::new(RunnerBackend::new(descriptor, link)?),
        tokio::runtime::Handle::current(),
    );
    let environment = runner_environment(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000581")?),
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
        services.backend(),
    )?
    .with_line_owner(owner()?);
    let control = OperationContext::new(CancellationToken::new())
        .with_owner(owner()?)
        .with_class(OperationClass::EffectCall {
            call_id: CallId::from_str("00000000-0000-0000-0000-0000000000c1")?,
            attempt_id: AttemptId::from_str("00000000-0000-0000-0000-0000000000d1")?,
        });
    Ok(Placed {
        root,
        runner,
        services,
        environment,
        control,
    })
}

impl Placed {
    /// Once the link is lost, the owner holds the placement for `outage`,
    /// then the barrier releases it.
    fn outage(&self, outage: Duration) -> tokio::task::JoinHandle<()> {
        let (runner, services) = (Arc::clone(&self.runner), self.services.clone());
        tokio::spawn(async move {
            // A liveness bound only: generous, so load never trips it.
            let lost = tokio::time::timeout(Duration::from_secs(60), async {
                while !runner.lost.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            assert!(
                lost.is_ok(),
                "the link was never lost: the outage did not start"
            );
            services.set_available(false);
            tokio::time::sleep(outage).await;
            runner.lost.store(false, Ordering::SeqCst);
            services.set_available(true);
        })
    }

    async fn start(
        &self,
        command: &str,
        yield_time_ms: u64,
    ) -> TestResult<crate::session::ProcessExecutionResult> {
        Ok(tokio::time::timeout(
            Duration::from_secs(60),
            ProcessSessionTools::posix_sh()?.start_canonical(
                &self.environment,
                CanonicalProcessStartRequest {
                    command: command.to_owned(),
                    workdir: std::path::PathBuf::from("."),
                    login: false,
                    tty: false,
                    keep_stdin_open: false,
                    _model_description: None,
                    yield_time_ms,
                    max_output_bytes: 4_096,
                },
                self.control.clone(),
            ),
        )
        .await??)
    }
}

/// Start a foreground command, lose the placement's link as it starts, and
/// release it after the command ended: the command completes naturally,
/// its result comes through, and nothing stops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placement_hold_cut_yield_deadline_does_not_promote_on_rebind() -> TestResult {
    let placed = placed(true, false)?;
    let path = placed.root.path().join("release");
    *placed
        .runner
        .release_poll
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((path.clone(), 0));
    let outage = placed.outage(Duration::from_secs(3));
    let result = placed
        .start(
            "n=0; while test ! -f release; do [ \"$n\" -lt 3000 ] || exit 1; n=$((n + 1)); sleep 0.01; done; sleep 0.1; printf done",
            2_000,
        )
        .await;
    // Also release a process a mutant promoted, before any assertion can fail.
    std::fs::write(&path, b"release")?;
    outage.await?;
    let result = result?;
    assert!(
        result.joined,
        "the elapsed hold spent the yield window and promoted a live process"
    );
    assert_eq!(placed.runner.promotions.load(Ordering::SeqCst), 0);
    assert_eq!(placed.runner.stops.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreground_command_survives_a_placement_hold() -> TestResult {
    let placed = placed(true, false)?;
    let outage = placed.outage(Duration::from_millis(1_500));
    let result = placed.start("sleep 1; printf done", 30_000).await?;
    outage.await?;
    let runner = &placed.runner;
    assert!(
        runner.interrupted.load(Ordering::SeqCst) > 0,
        "the hold was exercised"
    );
    assert_eq!(
        result.projection.exit_code,
        Some(0),
        "{:?}",
        result.projection
    );
    assert!(
        result.projection.output.contains("done"),
        "{:?}",
        result.projection
    );
    assert_eq!(
        runner.stops.load(Ordering::SeqCst),
        0,
        "nothing stopped the process"
    );
    Ok(())
}

/// A yield whose promotion the lost link refused unsent does not terminate
/// the process: the observation waits out the hold and the promotion is
/// made once after release, the process running on as a job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_promotion_refused_by_a_lost_link_is_made_once_after_release() -> TestResult {
    let placed = placed(false, true)?;
    let outage = placed.outage(Duration::from_millis(500));
    let result = placed.start("sleep 20; printf done", 300).await?;
    outage.await?;
    let runner = &placed.runner;
    assert!(
        runner.interrupted.load(Ordering::SeqCst) > 0,
        "the hold was exercised"
    );
    assert!(
        result.projection.session_id.is_some(),
        "{:?}",
        result.projection
    );
    assert!(result.first_promotion);
    assert_eq!(
        runner.promotions.load(Ordering::SeqCst),
        1,
        "one promotion completed"
    );
    assert_eq!(
        runner.stops.load(Ordering::SeqCst),
        0,
        "nothing stopped the process"
    );
    Ok(())
}

/// When the process ended during the hold that refused its promotion, the
/// tool reports its end rather than a stale running handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_ended_during_a_refused_promotions_hold_reports_its_end() -> TestResult {
    let placed = placed(false, true)?;
    // The process outlives the yield by far and ends well inside the
    // outage, so load can delay either without changing the outcome.
    let outage = placed.outage(Duration::from_millis(6_000));
    let result = placed.start("sleep 2; printf done", 300).await?;
    outage.await?;
    let runner = &placed.runner;
    assert!(
        runner.interrupted.load(Ordering::SeqCst) > 0,
        "the hold was exercised"
    );
    assert_eq!(
        result.projection.session_id, None,
        "{:?}",
        result.projection
    );
    assert_eq!(
        result.projection.exit_code,
        Some(0),
        "{:?}",
        result.projection
    );
    assert!(
        result.projection.output.contains("done"),
        "{:?}",
        result.projection
    );
    assert_eq!(runner.promotions.load(Ordering::SeqCst), 0);
    assert_eq!(runner.stops.load(Ordering::SeqCst), 0);
    Ok(())
}
