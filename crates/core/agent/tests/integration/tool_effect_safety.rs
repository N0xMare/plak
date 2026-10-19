//! Deterministic conformance tests for durably claimed mutating dispatch.

use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use plakous_agent::internal::{
    AuthorizedToolInvocation, ClaimedToolInvocation, EffectAuthorization, EffectDispatchIntent,
    ToolDispatchState, ToolExecutionControl, ToolRuntime,
};
use plakous_model::{
    AttemptId, CallId, CancellationToken, Clock, Digest, DigestDomain, FixedClock, RunId,
    Timestamp, ToolConcurrency, ToolCorrelation, ToolDefinition, ToolEffect, ToolInputContract,
    ToolName, ToolOutputContract, TurnId,
};
use plakous_tools::{
    ArgumentLimits, ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    ProgressSink, RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorId, ToolFuture,
    ToolInvocation, ToolInvocationRequest, ToolOutcome, ToolOutcomeKind, ToolPolicy,
    ToolPolicyDecision, ToolPolicyFuture, ToolPolicyRevision, ToolRegistration,
    ToolRegistryBuilder, ToolRegistryLimits, ToolRuntimeError, ToolRuntimeErrorKind,
};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
type SeenEffectBinding = Arc<Mutex<Option<(Digest, Digest)>>>;

#[derive(Clone)]
struct AllowPolicy;

impl ToolPolicy for AllowPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        match ToolPolicyRevision::new("test.effect-policy", 1) {
            Ok(revision) => revision,
            Err(error) => panic!("static test policy revision failed: {error}"),
        }
    }

    fn evaluate(&self, _request: plakous_tools::ToolPolicyRequest) -> ToolPolicyFuture {
        Box::pin(std::future::ready(Ok(ToolPolicyDecision::Allowed)))
    }
}

struct ProbeTool {
    result: Result<ToolOutcome, ToolRuntimeError>,
    execute_calls: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
    seen_binding: SeenEffectBinding,
}

impl ToolExecutor for ProbeTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        self.execute_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(binding) = invocation.effect_dispatch() {
            let value = (binding.intent_digest(), binding.claim_evidence_digest());
            match self.seen_binding.lock() {
                Ok(mut seen) => *seen = Some(value),
                Err(poisoned) => *poisoned.into_inner() = Some(value),
            }
        }
        let polls = Arc::clone(&self.polls);
        let mut result = Some(self.result.clone());
        Box::pin(std::future::poll_fn(move |_| {
            polls.fetch_add(1, Ordering::SeqCst);
            match result.take() {
                Some(result) => Poll::Ready(result),
                None => Poll::Pending,
            }
        }))
    }
}

struct Fixture {
    _workspace: tempfile::TempDir,
    runtime: ToolRuntime,
    request: ToolInvocationRequest,
    control: ToolExecutionControl,
    cancellation: CancellationToken,
    calls: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
    seen: SeenEffectBinding,
}

fn block_on<F: Future>(future: F) -> F::Output {
    match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime.block_on(future),
        Err(error) => panic!("test runtime failed: {error}"),
    }
}

fn definition(effect: ToolEffect) -> TestResult<ToolDefinition> {
    definition_with_output(effect, ToolOutputContract::portable_model_content())
}

fn definition_with_output(
    effect: ToolEffect,
    output: ToolOutputContract,
) -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new("mutate")?,
        "1.0.0",
        "Mutates an external test resource.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        }))?,
        output,
        ToolConcurrency::Serial,
        effect,
    )?)
}

fn fixture(suffix: u8, result: Result<ToolOutcome, ToolRuntimeError>) -> TestResult<Fixture> {
    fixture_with_output(suffix, result, ToolOutputContract::portable_model_content())
}

fn fixture_with_output(
    suffix: u8,
    result: Result<ToolOutcome, ToolRuntimeError>,
    output: ToolOutputContract,
) -> TestResult<Fixture> {
    let calls = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(None));
    let definition = definition_with_output(ToolEffect::Mutating, output)?;
    let tool = Arc::new(ProbeTool {
        result,
        execute_calls: Arc::clone(&calls),
        polls: Arc::clone(&polls),
        seen_binding: Arc::clone(&seen),
    });
    let mut builder = ToolRegistryBuilder::default();
    builder.register(ToolRegistration::new(
        RegisteredToolId::new("test.mutate")?,
        ToolExecutorId::new("test.mutate.v1")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"test.mutate.v1"])?,
        definition,
        tool,
    ))?;
    let run = RunId::from_str(&format!("00000000-0000-0000-0000-0000000000{suffix:02x}"))?;
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000101")?;
    let attempt = AttemptId::from_str("00000000-0000-0000-0000-000000000102")?;
    let call = CallId::from_str("00000000-0000-0000-0000-000000000103")?;
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let runtime = ToolRuntime::new(
        builder.freeze(),
        Arc::new(environment),
        Arc::new(AllowPolicy),
    );
    let request = ToolInvocationRequest::new(
        run,
        turn,
        attempt,
        call,
        ToolName::new("mutate")?,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)?,
        Arc::<str>::from(r#"{"value":"x"}"#),
    );
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(10, 0)?));
    let (progress, _) = ProgressSink::channel(4, call, Arc::clone(&clock))?;
    let cancellation = CancellationToken::new();
    let control = ToolExecutionControl::new(cancellation.clone(), None, clock, progress);
    Ok(Fixture {
        _workspace: workspace,
        runtime,
        request,
        control,
        cancellation,
        calls,
        polls,
        seen,
    })
}

async fn authorized(fixture: &Fixture) -> Result<AuthorizedToolInvocation, ToolRuntimeError> {
    authorize_request(&fixture.runtime, fixture.request.clone()).await
}

async fn authorize_request(
    runtime: &ToolRuntime,
    request: ToolInvocationRequest,
) -> Result<AuthorizedToolInvocation, ToolRuntimeError> {
    let invocation = runtime.prepare(request)?;
    match runtime.authorize(invocation).await? {
        EffectAuthorization::Allowed(authorized) => Ok(authorized),
        _ => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Policy,
            "test policy did not authorize invocation",
        )),
    }
}

fn runtime_with_argument_limits(
    tool: Arc<dyn ToolExecutor>,
    environment: Arc<ExecutionEnvironment>,
    arguments: ArgumentLimits,
) -> TestResult<ToolRuntime> {
    let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits {
        arguments,
        ..ToolRegistryLimits::default()
    });
    builder.register(ToolRegistration::new(
        RegisteredToolId::new("test.mutate")?,
        ToolExecutorId::new("test.mutate.v1")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"test.mutate.v1"])?,
        definition(ToolEffect::Mutating)?,
        tool,
    ))?;
    Ok(ToolRuntime::new(
        builder.freeze(),
        environment,
        Arc::new(AllowPolicy),
    ))
}

fn effect_control(call: CallId) -> TestResult<ToolExecutionControl> {
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(10, 0)?));
    let (progress, _) = ProgressSink::channel(4, call, Arc::clone(&clock))?;
    Ok(ToolExecutionControl::new(
        CancellationToken::new(),
        None,
        clock,
        progress,
    ))
}

struct ArgumentLimitPair {
    _workspace: tempfile::TempDir,
    baseline: ToolRuntime,
    changed: ToolRuntime,
    request: ToolInvocationRequest,
    call: CallId,
    calls: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
}

fn argument_limit_pair(
    index: usize,
    defaults: ArgumentLimits,
    changed_limits: ArgumentLimits,
) -> TestResult<ArgumentLimitPair> {
    let calls = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn ToolExecutor> = Arc::new(ProbeTool {
        result: ToolOutcome::success_text("ok"),
        execute_calls: Arc::clone(&calls),
        polls: Arc::clone(&polls),
        seen_binding: Arc::new(Mutex::new(None)),
    });
    let suffix = u8::try_from(index + 0x20)?;
    let run = RunId::from_str(&format!("00000000-0000-0000-0000-0000000000{suffix:02x}"))?;
    let call = CallId::from_str("00000000-0000-0000-0000-000000000103")?;
    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let baseline =
        runtime_with_argument_limits(Arc::clone(&tool), Arc::clone(&environment), defaults)?;
    let changed = runtime_with_argument_limits(tool, environment, changed_limits)?;
    let request = ToolInvocationRequest::new(
        run,
        TurnId::from_str("00000000-0000-0000-0000-000000000101")?,
        AttemptId::from_str("00000000-0000-0000-0000-000000000102")?,
        call,
        ToolName::new("mutate")?,
        ToolCorrelation::default(),
        Arc::<str>::from(r#"{"value":"x"}"#),
    );
    Ok(ArgumentLimitPair {
        _workspace: workspace,
        baseline,
        changed,
        request,
        call,
        calls,
        polls,
    })
}

fn assert_argument_limit_case(field: &str, pair: ArgumentLimitPair) -> TestResult {
    let baseline_authorized = block_on(authorize_request(&pair.baseline, pair.request.clone()))?;
    let changed_authorized = block_on(authorize_request(&pair.changed, pair.request))?;
    assert_ne!(
        baseline_authorized
            .invocation()
            .identity()
            .argument_limits_digest(),
        changed_authorized
            .invocation()
            .identity()
            .argument_limits_digest(),
        "{field} did not change the registered argument-limit identity"
    );
    let baseline_decision = baseline_authorized.grant().binding().digest();
    let changed_decision = changed_authorized.grant().binding().digest();
    assert_eq!(baseline_decision.version(), 6);
    assert_eq!(changed_decision.version(), 6);
    assert_ne!(baseline_decision, changed_decision, "{field} decision");

    let baseline_intent = pair.baseline.effect_dispatch_intent(&baseline_authorized)?;
    let changed_intent = pair.changed.effect_dispatch_intent(&changed_authorized)?;
    assert_eq!(baseline_intent.digest().version(), 6);
    assert_eq!(changed_intent.digest().version(), 6);
    assert_ne!(baseline_intent, changed_intent, "{field} effect intent");

    let baseline_evidence = claim_evidence(&baseline_intent)?;
    let changed_evidence = claim_evidence(&changed_intent)?;
    let baseline_claimed =
        pair.baseline
            .claim_effect(baseline_authorized, baseline_intent, baseline_evidence)?;
    let changed_claimed =
        pair.changed
            .claim_effect(changed_authorized, changed_intent, changed_evidence)?;

    let baseline_report = block_on(
        pair.baseline
            .execute_claimed(baseline_claimed, effect_control(pair.call)?),
    );
    let changed_report = block_on(
        pair.changed
            .execute_claimed(changed_claimed, effect_control(pair.call)?),
    );
    for report in [&baseline_report, &changed_report] {
        assert_eq!(report.state(), ToolDispatchState::Started, "{field}");
        assert_eq!(
            report.outcome().map(ToolOutcome::kind),
            Some(ToolOutcomeKind::Success),
            "{field}"
        );
    }
    assert_eq!(pair.calls.load(Ordering::SeqCst), 2, "{field}");
    assert_eq!(pair.polls.load(Ordering::SeqCst), 2, "{field}");
    Ok(())
}

fn claim_evidence(intent: &EffectDispatchIntent) -> TestResult<Digest> {
    let identity = intent.digest().identity_bytes();
    Ok(Digest::sha256(
        DigestDomain::Snapshot,
        1,
        &[b"test/persisted-effect-claim", &identity],
    )?)
}

fn claim(
    runtime: &ToolRuntime,
    authorized: AuthorizedToolInvocation,
) -> TestResult<ClaimedToolInvocation> {
    let intent = runtime.effect_dispatch_intent(&authorized)?;
    let evidence = claim_evidence(&intent)?;
    Ok(runtime.claim_effect(authorized, intent, evidence)?)
}

#[test]
fn every_argument_limit_changes_decision_and_intent_without_changing_dispatch() -> TestResult {
    let defaults = ArgumentLimits::default();
    let cases = [
        (
            "max_bytes",
            ArgumentLimits {
                max_bytes: defaults.max_bytes - 1,
                ..defaults
            },
        ),
        (
            "max_depth",
            ArgumentLimits {
                max_depth: defaults.max_depth - 1,
                ..defaults
            },
        ),
        (
            "max_nodes",
            ArgumentLimits {
                max_nodes: defaults.max_nodes - 1,
                ..defaults
            },
        ),
        (
            "max_string_bytes",
            ArgumentLimits {
                max_string_bytes: defaults.max_string_bytes - 1,
                ..defaults
            },
        ),
    ];

    for (index, (field, changed_limits)) in cases.into_iter().enumerate() {
        assert_argument_limit_case(field, argument_limit_pair(index, defaults, changed_limits)?)?;
    }
    Ok(())
}

#[test]
fn durable_claim_precedes_poll_and_delivers_exact_binding() -> TestResult {
    let fixture = fixture(1, ToolOutcome::success_text("ok"))?;
    let authorized = block_on(authorized(&fixture))?;
    let expected = fixture.runtime.effect_dispatch_intent(&authorized)?;
    let evidence = claim_evidence(&expected)?;
    let claimed = fixture
        .runtime
        .claim_effect(authorized, expected.clone(), evidence)?;
    assert_eq!(claimed.intent(), &expected);
    assert_eq!(claimed.claim_evidence_digest(), evidence);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.polls.load(Ordering::SeqCst), 0);

    let report = block_on(fixture.runtime.execute_claimed(claimed, fixture.control));
    assert_eq!(report.state(), ToolDispatchState::Started);
    assert_eq!(
        report.outcome().map(ToolOutcome::kind),
        Some(ToolOutcomeKind::Success)
    );
    let seen = match fixture.seen.lock() {
        Ok(seen) => *seen,
        Err(poisoned) => *poisoned.into_inner(),
    };
    assert_eq!(seen, Some((expected.digest(), evidence)));
    Ok(())
}

#[test]
fn mismatched_claim_and_predispatch_cancel_never_poll_executor() -> TestResult {
    let failed = fixture(2, ToolOutcome::success_text("unused"))?;
    let foreign = fixture(3, ToolOutcome::success_text("unused"))?;
    let failed_authorized = block_on(authorized(&failed))?;
    let foreign_authorized = block_on(authorized(&foreign))?;
    let foreign_intent = foreign
        .runtime
        .effect_dispatch_intent(&foreign_authorized)?;
    let error = failed
        .runtime
        .claim_effect(
            failed_authorized,
            foreign_intent.clone(),
            claim_evidence(&foreign_intent)?,
        )
        .err()
        .ok_or("mismatched claim unexpectedly succeeded")?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::EffectSafety);
    assert_eq!(failed.calls.load(Ordering::SeqCst), 0);
    assert_eq!(failed.polls.load(Ordering::SeqCst), 0);

    let cancelled = fixture(4, ToolOutcome::success_text("unused"))?;
    let cancelled_authorized = block_on(authorized(&cancelled))?;
    let claimed = claim(&cancelled.runtime, cancelled_authorized)?;
    cancelled.cancellation.cancel();
    let report = block_on(
        cancelled
            .runtime
            .execute_claimed(claimed, cancelled.control),
    );
    assert_eq!(report.state(), ToolDispatchState::NotStarted);
    assert_eq!(
        report.error().map(ToolRuntimeError::kind),
        Some(ToolRuntimeErrorKind::Cancelled)
    );
    assert_eq!(cancelled.calls.load(Ordering::SeqCst), 0);
    assert_eq!(cancelled.polls.load(Ordering::SeqCst), 0);
    let replay = cancelled.runtime.prepare(cancelled.request)?;
    assert!(block_on(cancelled.runtime.authorize(replay)).is_err());
    Ok(())
}

#[test]
fn started_executor_no_effect_error_is_definite() -> TestResult {
    for error in [
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "size is not available on this route; omit it",
        ),
        ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, "request was not sent")
            .with_proven_no_effect(true),
    ] {
        let rejected = fixture(15, Err(error.clone()))?;
        let authorized = block_on(authorized(&rejected))?;
        let claimed = claim(&rejected.runtime, authorized)?;
        let report = block_on(rejected.runtime.execute_claimed(claimed, rejected.control));
        assert_eq!(report.state(), ToolDispatchState::Started);
        assert_eq!(report.error(), Some(&error));
        assert!(report.uncertainty_cause().is_none());
        assert_eq!(rejected.calls.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[test]
fn started_executor_uncertain_disposition_overrides_argument_error_category() -> TestResult {
    let uncertain = fixture(
        16,
        Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "validation failed after a possible effect",
        )
        .with_proven_no_effect(false)),
    )?;
    let authorized = block_on(authorized(&uncertain))?;
    let claimed = claim(&uncertain.runtime, authorized)?;
    let report = block_on(
        uncertain
            .runtime
            .execute_claimed(claimed, uncertain.control),
    );
    assert_eq!(report.state(), ToolDispatchState::Started);
    assert_eq!(
        report.outcome().map(ToolOutcome::kind),
        Some(ToolOutcomeKind::OutcomeUnknown)
    );
    assert!(report.uncertainty_cause().is_some());
    Ok(())
}

#[test]
fn started_error_becomes_unknown_and_known_failure_is_preserved() -> TestResult {
    let uncertain = fixture(
        4,
        Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::CleanupTimeout,
            "test executor lost its terminal result",
        )),
    )?;
    let uncertain_authorized = block_on(authorized(&uncertain))?;
    let claimed = claim(&uncertain.runtime, uncertain_authorized)?;
    let report = block_on(
        uncertain
            .runtime
            .execute_claimed(claimed, uncertain.control),
    );
    assert_eq!(report.state(), ToolDispatchState::Started);
    assert_eq!(
        report.outcome().map(ToolOutcome::kind),
        Some(ToolOutcomeKind::OutcomeUnknown)
    );
    assert_eq!(
        report.uncertainty_cause().map(ToolRuntimeError::kind),
        Some(ToolRuntimeErrorKind::CleanupTimeout)
    );

    let failed = fixture(5, ToolOutcome::failure_text("known failure"))?;
    let failed_authorized = block_on(authorized(&failed))?;
    let claimed = claim(&failed.runtime, failed_authorized)?;
    let report = block_on(failed.runtime.execute_claimed(claimed, failed.control));
    assert_eq!(report.state(), ToolDispatchState::Started);
    assert_eq!(
        report.outcome().map(ToolOutcome::kind),
        Some(ToolOutcomeKind::Failure)
    );
    assert!(report.uncertainty_cause().is_none());
    Ok(())
}

#[test]
fn started_executor_that_joins_cancellation_is_a_definite_cancelled_terminal() -> TestResult {
    // The executor observed the interrupt and classified its own stop. That is
    // a joined outcome (spec 04 §3): it must not be widened
    // to `OutcomeUnknown`, which would turn every observed interrupt of a
    // running mutating tool into a recovery obligation.
    let joined = fixture(
        6,
        Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Cancelled,
            "test executor joined the cancel",
        )),
    )?;
    let joined_authorized = block_on(authorized(&joined))?;
    let claimed = claim(&joined.runtime, joined_authorized)?;
    let report = block_on(joined.runtime.execute_claimed(claimed, joined.control));
    assert_eq!(report.state(), ToolDispatchState::Started);
    assert!(report.outcome().is_none());
    assert!(report.uncertainty_cause().is_none());
    assert_eq!(
        report.error().map(ToolRuntimeError::kind),
        Some(ToolRuntimeErrorKind::Cancelled)
    );
    Ok(())
}

#[test]
fn completed_contract_mismatch_is_definite_failure_and_cannot_redispatch() -> TestResult {
    let mismatched = fixture_with_output(
        12,
        ToolOutcome::success(vec![
            plakous_model::ToolResultPart::Text(plakous_model::TextBlock::new("first", None)?),
            plakous_model::ToolResultPart::Text(plakous_model::TextBlock::new("second", None)?),
        ]),
        ToolOutputContract::model_content(1, 1_048_576)?,
    )?;
    let authorized = block_on(authorized(&mismatched))?;
    let claimed = claim(&mismatched.runtime, authorized)?;

    let report = block_on(
        mismatched
            .runtime
            .execute_claimed(claimed, mismatched.control),
    );
    assert_eq!(report.state(), ToolDispatchState::Started);
    assert_eq!(
        report.outcome().map(ToolOutcome::kind),
        Some(ToolOutcomeKind::Failure)
    );
    assert!(report.uncertainty_cause().is_none());
    assert_eq!(mismatched.calls.load(Ordering::SeqCst), 1);
    assert_eq!(mismatched.polls.load(Ordering::SeqCst), 1);
    let replay = mismatched.runtime.prepare(mismatched.request)?;
    assert!(block_on(mismatched.runtime.authorize(replay)).is_err());
    assert_eq!(mismatched.calls.load(Ordering::SeqCst), 1);
    Ok(())
}
