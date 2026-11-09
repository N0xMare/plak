//! A foreground command through the process tool on a runner placement
//! survives the placement's hold: its observation waits out the lost link
//! and reads again after release, and the process completes on its own.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    environment: ExecutionEnvironment,
    sessions: JoinedProcessSessions,
    /// Lost once the start is answered, so the model's first observation
    /// meets the lost link.
    lost: Arc<AtomicBool>,
    interrupted: AtomicUsize,
    stops: AtomicUsize,
}

impl RunnerLink for LosableRunner {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        if self.lost.load(Ordering::SeqCst) {
            let failure = if matches!(call.class(), RunnerClass::Observation { .. }) {
                self.interrupted.fetch_add(1, Ordering::SeqCst);
                RunnerFailure::ReadInterrupted("lost".into())
            } else {
                RunnerFailure::NotSent("lost".into())
            };
            return Box::pin(std::future::ready(Err(failure)));
        }
        if matches!(
            call.verb(),
            RunnerVerb::ProcessStop | RunnerVerb::ForegroundStop | RunnerVerb::JobsStop
        ) {
            self.stops.fetch_add(1, Ordering::SeqCst);
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
        let lose = (call.verb() == RunnerVerb::ProcessStart).then(|| Arc::clone(&self.lost));
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

/// Start a foreground command, lose the placement's link as it starts, and
/// release it after the command ended: the command completes naturally, its result comes
/// through, and nothing stops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreground_command_survives_a_placement_hold() -> TestResult {
    let root = tempfile::tempdir()?;
    let runner_side = host(root.path())?;
    let runner = Arc::new(LosableRunner {
        sessions: line_sessions(&runner_side, &job_capacity(8))?,
        environment: runner_side.clone(),
        lost: Arc::new(AtomicBool::new(false)),
        interrupted: AtomicUsize::new(0),
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
    let outage = {
        let (runner, services) = (Arc::clone(&runner), services.clone());
        tokio::spawn(async move {
            while runner.interrupted.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // The link's owner holds the placement, the command finishes
            // on the runner meanwhile, and the barrier then releases it.
            services.set_available(false);
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            runner.lost.store(false, Ordering::SeqCst);
            services.set_available(true);
        })
    };
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        ProcessSessionTools::posix_sh()?.start_canonical(
            &environment,
            CanonicalProcessStartRequest {
                command: String::from("sleep 1; printf done"),
                workdir: std::path::PathBuf::from("."),
                login: false,
                tty: false,
                keep_stdin_open: false,
                _model_description: None,
                yield_time_ms: 30_000,
                max_output_bytes: 4_096,
            },
            control,
        ),
    )
    .await??;
    outage.await?;
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
