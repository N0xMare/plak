//! Adversarial conformance for fail-closed tool authorization.

use std::future::Future;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::task::noop_waker_ref;
use plakous_agent::internal::{
    AuthorizedToolInvocation, EffectAuthorization, PendingEffectDecision, ToolExecutionControl,
    ToolRuntime,
};
use plakous_model::{
    AttemptId, CallId, CancellationToken, Clock, FixedClock, RunId, Timestamp, ToolConcurrency,
    ToolCorrelation, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
    TurnId,
};
use plakous_tools::{
    EffectAuthorizationGrantSource, EffectDecision, EffectDecisionAuditId, ExecutionEnvironment,
    ExecutionEnvironmentConfig, ExecutionEnvironmentScope, FrozenToolRegistry, ProgressSink,
    RegisteredToolId, ToolExecutorId, ToolInvocation, ToolInvocationRequest, ToolOutcome,
    ToolPolicy, ToolPolicyDecision, ToolPolicyDenial, ToolPolicyFuture, ToolPolicyRevision,
    ToolRegistration, ToolRegistryBuilder, ToolRuntimeErrorKind,
};
use serde_json::json;
use tempfile::TempDir;

fn test_block_on<F: Future>(future: F) -> F::Output {
    match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime.block_on(future),
        Err(error) => panic!("test Tokio runtime failed: {error}"),
    }
}

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy)]
struct Ids {
    run: RunId,
    turn: TurnId,
    attempt: AttemptId,
    call: CallId,
}

fn ids(call: u128) -> TestResult<Ids> {
    Ok(Ids {
        run: RunId::from_str("00000000-0000-0000-0000-000000000101")?,
        turn: TurnId::from_str("00000000-0000-0000-0000-000000000102")?,
        attempt: AttemptId::from_str("00000000-0000-0000-0000-000000000103")?,
        call: CallId::from_str(&format!("00000000-0000-0000-0000-{call:012x}"))?,
    })
}

fn definition(name: &str, effect: ToolEffect) -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new(name)?,
        "1.0.0",
        "Policy-test tool.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        effect,
    )?)
}

fn registry() -> TestResult<(
    FrozenToolRegistry,
    Arc<plakous_test_support::ScriptedTool>,
    Arc<plakous_test_support::ScriptedTool>,
)> {
    let read = Arc::new(plakous_test_support::ScriptedTool::new([
        ToolOutcome::success_text("read"),
    ]));
    let write = Arc::new(plakous_test_support::ScriptedTool::new([
        ToolOutcome::success_text("write"),
    ]));
    let mut builder = ToolRegistryBuilder::default();
    builder.register(ToolRegistration::new(
        RegisteredToolId::new("test.read")?,
        ToolExecutorId::new("test.read.v1")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"test.read.v1"])?,
        definition("read", ToolEffect::ReadOnly)?,
        read.clone(),
    ))?;
    builder.register(ToolRegistration::new(
        RegisteredToolId::new("test.write")?,
        ToolExecutorId::new("test.write.v1")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"test.write.v1"])?,
        definition("write", ToolEffect::Mutating)?,
        write.clone(),
    ))?;
    Ok((builder.freeze(), read, write))
}

fn invocation(
    runtime: &ToolRuntime,
    ids: Ids,
    tool: &str,
    value: &str,
) -> TestResult<ToolInvocation> {
    Ok(runtime.prepare(ToolInvocationRequest::new(
        ids.run,
        ids.turn,
        ids.attempt,
        ids.call,
        ToolName::new(tool)?,
        ToolCorrelation::default(),
        Arc::<str>::from(format!(r#"{{"value":"{value}"}}"#)),
    ))?)
}

fn environment(ids: Ids, workspace: &TempDir) -> TestResult<Arc<ExecutionEnvironment>> {
    Ok(Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?))
}

fn control(ids: Ids) -> TestResult<ToolExecutionControl> {
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(10, 0)?));
    let (progress, _receiver) = ProgressSink::channel(4, ids.call, Arc::clone(&clock))?;
    Ok(ToolExecutionControl::new(
        CancellationToken::new(),
        None,
        clock,
        progress,
    ))
}

#[derive(Clone)]
struct MutablePolicy {
    revision: Arc<Mutex<ToolPolicyRevision>>,
    decision: ToolPolicyDecision,
}

impl MutablePolicy {
    fn new(revision: u64, decision: ToolPolicyDecision) -> TestResult<Self> {
        Ok(Self {
            revision: Arc::new(Mutex::new(ToolPolicyRevision::new(
                "test.policy",
                revision,
            )?)),
            decision,
        })
    }

    fn set_revision(&self, revision: u64) -> TestResult {
        let replacement = ToolPolicyRevision::new("test.policy", revision)?;
        match self.revision.lock() {
            Ok(mut current) => *current = replacement,
            Err(poisoned) => *poisoned.into_inner() = replacement,
        }
        Ok(())
    }
}

impl ToolPolicy for MutablePolicy {
    fn revision(&self) -> ToolPolicyRevision {
        match self.revision.lock() {
            Ok(revision) => revision.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn evaluate(&self, _request: plakous_tools::ToolPolicyRequest) -> ToolPolicyFuture {
        let decision = self.decision.clone();
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |wake| {
            if yielded {
                Poll::Ready(Ok(decision.clone()))
            } else {
                yielded = true;
                wake.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}

fn runtime(
    registry: FrozenToolRegistry,
    environment: Arc<ExecutionEnvironment>,
    policy: MutablePolicy,
) -> ToolRuntime {
    ToolRuntime::new(registry, environment, Arc::new(policy))
}

fn pending(authorization: EffectAuthorization) -> TestResult<PendingEffectDecision> {
    match authorization {
        EffectAuthorization::DecisionRequired(pending) => Ok(pending),
        _ => Err("expected a pending tool decision".into()),
    }
}

fn authorized(authorization: EffectAuthorization) -> TestResult<AuthorizedToolInvocation> {
    let EffectAuthorization::Allowed(authorized) = authorization else {
        return Err("expected an authorized tool invocation".into());
    };
    Ok(authorized)
}

#[test]
fn allow_mints_one_runtime_bound_capability_and_dispatches() -> TestResult {
    let ids = ids(0x104)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let policy = MutablePolicy::new(1, ToolPolicyDecision::Allowed)?;
    let runtime = runtime(registry, environment(ids, &workspace)?, policy);
    let invocation = invocation(&runtime, ids, "read", "allowed")?;
    let authorized = authorized(test_block_on(runtime.authorize(invocation))?)?;
    assert_eq!(
        authorized.grant().source(),
        &EffectAuthorizationGrantSource::Policy
    );

    let outcome = test_block_on(runtime.execute(authorized, control(ids)?))?;
    assert_eq!(outcome.parts().len(), 1);
    assert_eq!(read.captured().len(), 1);
    Ok(())
}

#[test]
fn denial_and_pending_decision_never_dispatch() -> TestResult {
    let ids = ids(0x105)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let denied_runtime = runtime(
        registry.clone(),
        environment(ids, &workspace)?,
        MutablePolicy::new(
            1,
            ToolPolicyDecision::Denied(ToolPolicyDenial::new("denied")),
        )?,
    );
    let denied = test_block_on(denied_runtime.authorize(invocation(
        &denied_runtime,
        ids,
        "read",
        "denied",
    )?))?;
    assert!(matches!(denied, EffectAuthorization::Denied(_)));
    assert!(read.captured().is_empty());

    let decision_runtime = runtime(
        registry,
        environment(ids, &workspace)?,
        MutablePolicy::new(
            1,
            ToolPolicyDecision::DecisionRequired(plakous_tools::EffectDecisionPrompt::new(
                "approve read",
            )),
        )?,
    );
    let pending = pending(test_block_on(decision_runtime.authorize(invocation(
        &decision_runtime,
        ids,
        "read",
        "pending",
    )?))?)?;
    let binding = pending.binding().digest();
    let denied = decision_runtime.resolve_effect_decision(
        pending,
        EffectDecision::deny(binding, ToolPolicyDenial::new("operator denied")),
    )?;
    assert!(matches!(denied, EffectAuthorization::Denied(_)));
    assert!(read.captured().is_empty());
    Ok(())
}

#[test]
fn mutating_dispatch_fails_closed_without_an_effect_journal() -> TestResult {
    let ids = ids(0x110)?;
    let workspace = tempfile::tempdir()?;
    let (registry, _read, write) = registry()?;
    let runtime = runtime(
        registry,
        environment(ids, &workspace)?,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let authorized = authorized(test_block_on(runtime.authorize(invocation(
        &runtime,
        ids,
        "write",
        "must-not-run",
    )?))?)?;
    let result = test_block_on(runtime.execute(authorized, control(ids)?));
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Policy)
    );
    assert!(write.captured().is_empty());
    Ok(())
}

#[test]
fn explicit_decision_is_digest_bound_and_clone_runtime_dispatches() -> TestResult {
    let ids = ids(0x106)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let runtime = runtime(
        registry,
        environment(ids, &workspace)?,
        MutablePolicy::new(
            1,
            ToolPolicyDecision::DecisionRequired(plakous_tools::EffectDecisionPrompt::new(
                "approve exact invocation",
            )),
        )?,
    );
    let clone = runtime.clone();
    let pending = pending(test_block_on(
        runtime.authorize(invocation(&runtime, ids, "read", "allowed")?),
    )?)?;
    let binding = pending.binding().digest();
    let authorized = authorized(clone.resolve_effect_decision(
        pending,
        EffectDecision::allow(binding, EffectDecisionAuditId::new("decision-106")?),
    )?)?;
    assert_eq!(authorized.grant().binding().digest(), binding);
    assert!(matches!(
        authorized.grant().source(),
        EffectAuthorizationGrantSource::Operator(id) if id.as_str() == "decision-106"
    ));
    test_block_on(clone.execute(authorized, control(ids)?))?;
    assert_eq!(read.captured().len(), 1);
    Ok(())
}

#[test]
fn decision_response_for_another_binding_fails_closed() -> TestResult {
    let ids = ids(0x10e)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let runtime = runtime(
        registry,
        environment(ids, &workspace)?,
        MutablePolicy::new(
            1,
            ToolPolicyDecision::DecisionRequired(plakous_tools::EffectDecisionPrompt::new(
                "approve exact invocation",
            )),
        )?,
    );
    let pending = pending(test_block_on(runtime.authorize(invocation(
        &runtime,
        ids,
        "read",
        "binding-a",
    )?))?)?;
    let wrong_binding = pending.binding().argument_digest();
    let result = runtime.resolve_effect_decision(
        pending,
        EffectDecision::allow(wrong_binding, EffectDecisionAuditId::new("wrong-binding")?),
    );
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Policy)
    );
    assert!(read.captured().is_empty());
    Ok(())
}

#[test]
fn environment_scope_must_cover_invocation_run_and_attempt() -> TestResult {
    let environment_ids = ids(0x10f)?;
    let foreign_run_ids = Ids {
        run: RunId::from_str("00000000-0000-0000-0000-000000000201")?,
        ..environment_ids
    };
    let foreign_attempt_ids = Ids {
        attempt: AttemptId::from_str("00000000-0000-0000-0000-000000000202")?,
        call: CallId::from_str("00000000-0000-0000-0000-000000000203")?,
        ..environment_ids
    };
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let run_runtime = runtime(
        registry.clone(),
        environment(environment_ids, &workspace)?,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let foreign_run = test_block_on(run_runtime.authorize(invocation(
        &run_runtime,
        foreign_run_ids,
        "read",
        "foreign-run",
    )?));
    assert_eq!(
        foreign_run.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Policy)
    );

    let attempt_environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Attempt {
            run_id: environment_ids.run,
            attempt_id: environment_ids.attempt,
        },
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let attempt_runtime = runtime(
        registry,
        attempt_environment,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let foreign_attempt = test_block_on(attempt_runtime.authorize(invocation(
        &attempt_runtime,
        foreign_attempt_ids,
        "read",
        "foreign-attempt",
    )?));
    assert_eq!(
        foreign_attempt.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Policy)
    );
    assert!(read.captured().is_empty());
    Ok(())
}

#[test]
fn authorization_cannot_cross_runtime_authority_even_with_identical_inputs() -> TestResult {
    let ids = ids(0x107)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let environment = environment(ids, &workspace)?;
    let first = runtime(
        registry.clone(),
        Arc::clone(&environment),
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let second = runtime(
        registry,
        environment,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let authorized = authorized(test_block_on(
        first.authorize(invocation(&first, ids, "read", "bound")?),
    )?)?;
    let result = test_block_on(second.execute(authorized, control(ids)?));
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Policy)
    );
    assert!(read.captured().is_empty());
    Ok(())
}

#[test]
fn duplicate_authorization_for_one_call_attempt_is_rejected_before_dispatch() -> TestResult {
    let ids = ids(0x10d)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let runtime = runtime(
        registry,
        environment(ids, &workspace)?,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let exact = invocation(&runtime, ids, "read", "once")?;
    let first = authorized(test_block_on(runtime.authorize(exact.clone()))?)?;
    let duplicate = test_block_on(runtime.authorize(exact));
    assert_eq!(
        duplicate.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Policy)
    );

    test_block_on(runtime.execute(first, control(ids)?))?;
    assert_eq!(read.captured().len(), 1);
    Ok(())
}

#[test]
fn dropped_evaluation_releases_the_bounded_live_attempt_slot() -> TestResult {
    let ids = ids(0x114)?;
    let workspace = tempfile::tempdir()?;
    let (registry, _read, _write) = registry()?;
    let runtime = runtime(
        registry,
        environment(ids, &workspace)?,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let exact = invocation(&runtime, ids, "read", "dropped-evaluation")?;
    let mut evaluation = Box::pin(runtime.authorize(exact.clone()));
    let mut context = Context::from_waker(noop_waker_ref());
    assert!(evaluation.as_mut().poll(&mut context).is_pending());
    drop(evaluation);

    let replacement = authorized(test_block_on(runtime.authorize(exact))?)?;
    drop(replacement);
    Ok(())
}

#[test]
fn dropped_pending_and_authorized_capabilities_release_live_slots() -> TestResult {
    let ids = ids(0x115)?;
    let workspace = tempfile::tempdir()?;
    let (registry, _read, _write) = registry()?;
    let decision_runtime = runtime(
        registry.clone(),
        environment(ids, &workspace)?,
        MutablePolicy::new(
            1,
            ToolPolicyDecision::DecisionRequired(plakous_tools::EffectDecisionPrompt::new(
                "drop pending",
            )),
        )?,
    );
    let exact_pending = invocation(&decision_runtime, ids, "read", "drop-pending")?;
    drop(pending(test_block_on(
        decision_runtime.authorize(exact_pending.clone()),
    )?)?);
    drop(pending(test_block_on(
        decision_runtime.authorize(exact_pending),
    )?)?);

    let allow_runtime = runtime(
        registry,
        environment(ids, &workspace)?,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let exact_authorized = invocation(&allow_runtime, ids, "read", "drop-authorized")?;
    drop(authorized(test_block_on(
        allow_runtime.authorize(exact_authorized.clone()),
    )?)?);
    drop(authorized(test_block_on(
        allow_runtime.authorize(exact_authorized),
    )?)?);
    Ok(())
}

#[test]
fn completed_read_only_dispatch_retains_terminal_replay_protection() -> TestResult {
    let ids = ids(0x116)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let runtime = runtime(
        registry,
        environment(ids, &workspace)?,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let exact = invocation(&runtime, ids, "read", "terminal-read")?;
    let first = authorized(test_block_on(runtime.authorize(exact.clone()))?)?;
    test_block_on(runtime.execute(first, control(ids)?))?;
    assert_eq!(read.captured().len(), 1);

    let Err(error) = test_block_on(runtime.authorize(exact)) else {
        return Err("completed canonical attempt was unexpectedly reauthorized".into());
    };
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Policy);
    assert_eq!(read.captured().len(), 1);
    Ok(())
}

#[test]
fn conflicting_binding_for_one_call_attempt_is_rejected_before_policy_or_dispatch() -> TestResult {
    let ids = ids(0x112)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let runtime = runtime(
        registry,
        environment(ids, &workspace)?,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let first = authorized(test_block_on(
        runtime.authorize(invocation(&runtime, ids, "read", "first")?),
    )?)?;
    let conflicting =
        test_block_on(runtime.authorize(invocation(&runtime, ids, "read", "different")?));
    assert_eq!(
        conflicting.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Policy)
    );

    test_block_on(runtime.execute(first, control(ids)?))?;
    assert_eq!(read.captured().len(), 1);
    Ok(())
}

#[test]
fn concurrent_conflicting_bindings_for_one_call_attempt_have_one_winner() -> TestResult {
    let ids = ids(0x113)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let runtime = runtime(
        registry,
        environment(ids, &workspace)?,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let first = invocation(&runtime, ids, "read", "first")?;
    let second = invocation(&runtime, ids, "read", "second")?;
    let (first, second) = test_block_on(async {
        futures::join!(runtime.authorize(first), runtime.authorize(second))
    });
    let ((Ok(EffectAuthorization::Allowed(winner)), Err(loser))
    | (Err(loser), Ok(EffectAuthorization::Allowed(winner)))) = (first, second)
    else {
        return Err("conflicting authorization did not produce one winner".into());
    };
    assert_eq!(loser.kind(), ToolRuntimeErrorKind::Policy);
    test_block_on(runtime.execute(winner, control(ids)?))?;
    assert_eq!(read.captured().len(), 1);
    Ok(())
}

#[test]
fn concurrent_duplicate_authorization_has_exactly_one_winner() -> TestResult {
    let ids = ids(0x111)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let runtime = runtime(
        registry,
        environment(ids, &workspace)?,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let exact = invocation(&runtime, ids, "read", "concurrent")?;
    let (first, second) = test_block_on(async {
        futures::join!(runtime.authorize(exact.clone()), runtime.authorize(exact))
    });
    let ((Ok(EffectAuthorization::Allowed(winner)), Err(loser))
    | (Err(loser), Ok(EffectAuthorization::Allowed(winner)))) = (first, second)
    else {
        return Err("concurrent authorization did not produce one winner".into());
    };
    assert_eq!(loser.kind(), ToolRuntimeErrorKind::Policy);
    test_block_on(runtime.execute(winner, control(ids)?))?;
    assert_eq!(read.captured().len(), 1);
    Ok(())
}

#[test]
fn policy_revision_change_invalidates_an_existing_grant() -> TestResult {
    let ids = ids(0x108)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let policy = MutablePolicy::new(1, ToolPolicyDecision::Allowed)?;
    let runtime = runtime(registry, environment(ids, &workspace)?, policy.clone());
    let authorized = authorized(test_block_on(
        runtime.authorize(invocation(&runtime, ids, "read", "stale")?),
    )?)?;
    policy.set_revision(2)?;
    let result = test_block_on(runtime.execute(authorized, control(ids)?));
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Policy)
    );
    assert!(read.captured().is_empty());
    Ok(())
}

#[test]
fn binding_changes_with_arguments_tool_environment_and_policy_revision() -> TestResult {
    let first_ids = ids(0x109)?;
    let first_workspace = tempfile::tempdir()?;
    let second_workspace = tempfile::tempdir()?;
    let (registry, _read, _write) = registry()?;
    let decision = ToolPolicyDecision::DecisionRequired(plakous_tools::EffectDecisionPrompt::new(
        "compare binding",
    ));
    let baseline = runtime(
        registry.clone(),
        environment(first_ids, &first_workspace)?,
        MutablePolicy::new(1, decision.clone())?,
    );
    let baseline_binding = pending(test_block_on(
        baseline.authorize(invocation(&baseline, first_ids, "read", "first")?),
    )?)?
    .binding()
    .digest();

    let argument_runtime = runtime(
        registry.clone(),
        environment(first_ids, &first_workspace)?,
        MutablePolicy::new(1, decision.clone())?,
    );
    let changed_arguments = pending(test_block_on(argument_runtime.authorize(invocation(
        &argument_runtime,
        first_ids,
        "read",
        "second",
    )?))?)?
    .binding()
    .digest();
    let tool_runtime = runtime(
        registry.clone(),
        environment(first_ids, &first_workspace)?,
        MutablePolicy::new(1, decision.clone())?,
    );
    let changed_tool = pending(test_block_on(tool_runtime.authorize(invocation(
        &tool_runtime,
        first_ids,
        "write",
        "first",
    )?))?)?
    .binding()
    .digest();

    let other_environment = runtime(
        registry.clone(),
        environment(first_ids, &second_workspace)?,
        MutablePolicy::new(1, decision.clone())?,
    );
    let changed_environment = pending(test_block_on(other_environment.authorize(invocation(
        &other_environment,
        first_ids,
        "read",
        "first",
    )?))?)?
    .binding()
    .digest();

    let other_policy = runtime(
        registry,
        environment(first_ids, &first_workspace)?,
        MutablePolicy::new(2, decision)?,
    );
    let changed_policy = pending(test_block_on(other_policy.authorize(invocation(
        &other_policy,
        first_ids,
        "read",
        "first",
    )?))?)?
    .binding()
    .digest();

    assert_ne!(baseline_binding, changed_arguments);
    assert_ne!(baseline_binding, changed_tool);
    assert_ne!(baseline_binding, changed_environment);
    assert_ne!(baseline_binding, changed_policy);
    Ok(())
}

#[test]
fn control_call_mismatch_fails_before_executor_dispatch() -> TestResult {
    let invocation_ids = ids(0x10b)?;
    let control_ids = ids(0x10c)?;
    let workspace = tempfile::tempdir()?;
    let (registry, read, _write) = registry()?;
    let runtime = runtime(
        registry,
        environment(invocation_ids, &workspace)?,
        MutablePolicy::new(1, ToolPolicyDecision::Allowed)?,
    );
    let authorized = authorized(test_block_on(runtime.authorize(invocation(
        &runtime,
        invocation_ids,
        "read",
        "mismatch",
    )?))?)?;
    let result = test_block_on(runtime.execute(authorized, control(control_ids)?));
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Execution)
    );
    assert!(read.captured().is_empty());
    Ok(())
}
