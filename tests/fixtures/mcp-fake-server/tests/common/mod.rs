//! Helpers shared by the HTTP integration tests.

use std::str::FromStr;
use std::sync::Arc;

use plakous_model::{
    AttemptId, BranchId, CallId, CancellationToken, Clock, Digest, DigestDomain, FixedClock, RunId,
    SessionId, Timestamp, ToolCorrelation, ToolName, TurnId,
};
use plakous_tools::{
    EffectAuthorizationGrantSource, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentScope, FrozenToolRegistry, JobOutputRoot, ProgressSink,
    ToolInvocationRequest, ToolOutcome, ToolPolicyRevision,
};

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Runs one registered tool call through its executor, as a turn would.
pub async fn execute(
    registry: &FrozenToolRegistry,
    environment: Arc<ExecutionEnvironment>,
    tool: &str,
    arguments: &str,
) -> TestResult<ToolOutcome> {
    let run = RunId::from_str("00000000-0000-0000-0000-000000000201")?;
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000202")?;
    let attempt = AttemptId::from_str("00000000-0000-0000-0000-000000000203")?;
    let call = CallId::from_str("00000000-0000-0000-0000-000000000204")?;
    let invocation = registry.prepare(ToolInvocationRequest::new(
        run,
        turn,
        attempt,
        call,
        ToolName::new(tool)?,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default())?,
        Arc::<str>::from(arguments),
    ))?;
    let intent = Digest::sha256(DigestDomain::Snapshot, 1, &[b"mcp-http-intent"])?;
    let claim = Digest::sha256(DigestDomain::Snapshot, 1, &[b"mcp-http-claim"])?;
    let invocation = plakous_tools::internal::bind_effect_dispatch(invocation, intent, claim);
    let binding = plakous_tools::internal::authorization_binding(
        &invocation,
        environment.descriptor().clone(),
        ToolPolicyRevision::new("mcp-http-test", 1)?,
    )?;
    let grant = plakous_tools::internal::authorization_grant(
        binding,
        EffectAuthorizationGrantSource::Policy,
    );
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(71, 0)?));
    let (progress, _receiver) = ProgressSink::channel(8, call, Arc::clone(&clock))?;
    let context = plakous_tools::internal::tool_context(
        run,
        turn,
        attempt,
        CancellationToken::new(),
        None,
        clock,
        environment,
        grant,
        progress,
    );
    let executor = plakous_tools::internal::executor(registry, &invocation)?;
    Ok(executor.execute(invocation, context).await?)
}

/// A workspace and job-output root for one test.
pub struct Workspace {
    pub environment: Arc<ExecutionEnvironment>,
    _root: tempfile::TempDir,
    _state: tempfile::TempDir,
}

pub fn workspace() -> TestResult<Workspace> {
    let workspace = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000200")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    environment.joined_process_sessions()?.bind_job_outputs(
        JobOutputRoot::open(&state.path().join("jobs"))?,
        SessionId::from_str("00000000-0000-0000-0000-0000000002b0")?,
        BranchId::from_str("00000000-0000-0000-0000-0000000002c0")?,
    );
    Ok(Workspace {
        environment,
        _root: workspace,
        _state: state,
    })
}
