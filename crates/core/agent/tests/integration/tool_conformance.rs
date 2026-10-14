//! Public-boundary conformance tests for the minimal frozen tool runtime.

use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::Poll;

use futures::join;
use plakous_agent::internal::{
    AuthorizedToolInvocation, EffectAuthorization, ToolDispatchObserver,
    ToolDispatchObserverFuture, ToolExecutionControl, ToolRuntime,
};
use plakous_model::{
    AttemptId, CallId, CancellationToken, Clock, FixedClock, RunId, SemanticJsonV1, TextBlock,
    Timestamp, ToolConcurrency, ToolCorrelation, ToolDefinition, ToolEffect, ToolInputContract,
    ToolName, ToolOutputContract, ToolResultKind, ToolResultPart, TurnId,
};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    FrozenToolRegistry, IsolationLevel, ProgressReceiver, ProgressSink, ReadOnlyToolPolicy,
    RegisteredToolId, ToolContext, ToolExecutorId, ToolExposure, ToolExposureBinding,
    ToolExposureMap, ToolFuture, ToolInvocation, ToolInvocationRequest, ToolOutcome,
    ToolOutcomeKind, ToolOutcomeLimits, ToolRegistration, ToolRegistryBuilder, ToolRuntimeError,
    ToolRuntimeErrorKind, ValidatedArguments,
};
use proptest::prelude::*;
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

struct Ids {
    run: RunId,
    turn: TurnId,
    attempt: AttemptId,
    call: CallId,
}

#[derive(Default)]
struct RejectingDispatchObserver {
    calls: AtomicUsize,
}

impl ToolDispatchObserver for RejectingDispatchObserver {
    fn dispatch_started(&self) -> ToolDispatchObserverFuture<'_> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Box::pin(async {
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "test observer rejected dispatch",
            ))
        })
    }
}

fn ids() -> Result<Ids, Box<dyn std::error::Error>> {
    Ok(Ids {
        run: RunId::from_str("00000000-0000-0000-0000-000000000001")?,
        turn: TurnId::from_str("00000000-0000-0000-0000-000000000002")?,
        attempt: AttemptId::from_str("00000000-0000-0000-0000-000000000003")?,
        call: CallId::from_str("00000000-0000-0000-0000-000000000004")?,
    })
}

fn definition(name: &str) -> Result<ToolDefinition, Box<dyn std::error::Error>> {
    Ok(ToolDefinition::new(
        ToolName::new(name)?,
        "1.0.0",
        "Returns ordered test content.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?)
}

fn frozen(
    tool: Arc<plakous_test_support::ScriptedTool>,
) -> Result<FrozenToolRegistry, Box<dyn std::error::Error>> {
    let mut builder = ToolRegistryBuilder::default();
    builder.register(ToolRegistration::new(
        RegisteredToolId::new("test.lookup")?,
        ToolExecutorId::new("test.lookup.v1")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"test.lookup.v1"])?,
        definition("lookup")?,
        tool,
    ))?;
    Ok(builder.freeze())
}

fn request(ids: &Ids, raw: &str) -> Result<ToolInvocationRequest, Box<dyn std::error::Error>> {
    Ok(ToolInvocationRequest::new(
        ids.run,
        ids.turn,
        ids.attempt,
        ids.call,
        ToolName::new("lookup")?,
        ToolCorrelation::new(
            Some(String::from("remote-7")),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )?,
        Arc::<str>::from(raw),
    ))
}

fn context(
    ids: &Ids,
    workspace: &TempDir,
    cancellation: CancellationToken,
    capacity: usize,
) -> Result<(ToolExecutionControl, ProgressReceiver, ProgressSink), Box<dyn std::error::Error>> {
    context_for_call(ids, ids.call, workspace, cancellation, capacity)
}

fn context_for_call(
    _ids: &Ids,
    call_id: CallId,
    _workspace: &TempDir,
    cancellation: CancellationToken,
    capacity: usize,
) -> Result<(ToolExecutionControl, ProgressReceiver, ProgressSink), Box<dyn std::error::Error>> {
    let timestamp = Timestamp::new(42, 7)?;
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(timestamp));
    let (progress, receiver) = ProgressSink::channel(capacity, call_id, Arc::clone(&clock))?;
    let control = ToolExecutionControl::new(cancellation, None, clock, progress.clone());
    Ok((control, receiver, progress))
}

fn runtime(
    registry: FrozenToolRegistry,
    ids: &Ids,
    workspace: &TempDir,
) -> Result<ToolRuntime, Box<dyn std::error::Error>> {
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    Ok(ToolRuntime::new(
        registry,
        Arc::new(environment),
        Arc::new(ReadOnlyToolPolicy),
    ))
}

fn runtime_with_limits(
    registry: FrozenToolRegistry,
    ids: &Ids,
    workspace: &TempDir,
    limits: ToolOutcomeLimits,
) -> Result<ToolRuntime, Box<dyn std::error::Error>> {
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(ids.run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    Ok(ToolRuntime::with_outcome_limits(
        registry,
        Arc::new(environment),
        Arc::new(ReadOnlyToolPolicy),
        limits,
    )?)
}

async fn admitted(
    runtime: &ToolRuntime,
    invocation: ToolInvocation,
) -> Result<AuthorizedToolInvocation, ToolRuntimeError> {
    match runtime.authorize(invocation).await? {
        EffectAuthorization::Allowed(authorized) => Ok(authorized),
        EffectAuthorization::Denied(_) => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Policy,
            "test invocation was unexpectedly denied",
        )),
        EffectAuthorization::DecisionRequired(_) => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Policy,
            "test invocation unexpectedly required decision",
        )),
        _ => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Policy,
            "test invocation returned an unknown authorization state",
        )),
    }
}

async fn execute_admitted(
    runtime: &ToolRuntime,
    invocation: ToolInvocation,
    control: ToolExecutionControl,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let authorized = admitted(runtime, invocation).await?;
    runtime.execute(authorized, control).await
}

#[test]
fn dispatch_observer_rejection_prevents_executor_poll_and_closes_progress()
-> Result<(), Box<dyn std::error::Error>> {
    let ids = ids()?;
    let workspace = TempDir::new()?;
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps([
        plakous_test_support::ScriptedToolStep::Complete(Ok(ToolOutcome::success_text(
            "unexpected",
        )?)),
    ]));
    let runtime = runtime(frozen(tool.clone())?, &ids, &workspace)?;
    let invocation = runtime.prepare(request(&ids, r#"{"query":"x"}"#)?)?;
    let authorized = test_block_on(admitted(&runtime, invocation))?;
    let observer = Arc::new(RejectingDispatchObserver::default());
    let (control, receiver, progress) = context(&ids, &workspace, CancellationToken::new(), 2)?;
    let result = test_block_on(
        runtime.execute(authorized, control.with_dispatch_observer(observer.clone())),
    );

    assert!(matches!(
        result,
        Err(error) if error.kind() == ToolRuntimeErrorKind::Execution
    ));
    assert_eq!(observer.calls.load(Ordering::Acquire), 1);
    assert!(tool.captured().is_empty());
    assert!(test_block_on(receiver.recv()).is_none());
    progress.report("late", None, None)?;
    assert!(receiver.try_recv().is_none());
    Ok(())
}

#[test]
fn conformance_validates_dispatches_and_preserves_result_order_and_correlation()
-> Result<(), Box<dyn std::error::Error>> {
    let parts = vec![
        ToolResultPart::Text(TextBlock::new("first", None)?),
        ToolResultPart::Text(TextBlock::new("second", None)?),
    ];
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps([
        plakous_test_support::ScriptedToolStep::ProgressThenComplete {
            messages: vec![String::from("started"), String::from("finished")],
            result: Ok(ToolOutcome::success(parts.clone())?),
        },
    ]));
    let ids = ids()?;
    let workspace = tempfile::tempdir()?;
    let runtime = runtime(frozen(Arc::clone(&tool))?, &ids, &workspace)?;
    let request = request(&ids, r#"{"query":"needle"}"#)?;
    let request_debug = format!("{request:?}");
    assert!(!request_debug.contains("needle"));
    assert!(!request_debug.contains("remote-7"));
    let invocation = runtime.prepare(request)?;
    let invocation_debug = format!("{invocation:?}");
    assert!(!invocation_debug.contains("needle"));
    assert!(!invocation_debug.contains("remote-7"));
    let (context, progress, sink) = context(&ids, &workspace, CancellationToken::new(), 1)?;
    let context_debug = format!("{context:?}");
    assert!(!context_debug.contains(&workspace.path().to_string_lossy().to_string()));
    assert_eq!(runtime.environment().isolation(), IsolationLevel::Host);
    let outcome = test_block_on(execute_admitted(&runtime, invocation.clone(), context))?;

    assert_eq!(outcome.kind(), ToolOutcomeKind::Success);
    assert_eq!(outcome.parts(), parts);
    let canonical = outcome.to_model_result(&invocation)?;
    assert_eq!(canonical.call_id(), ids.call);
    assert_eq!(canonical.kind(), ToolResultKind::Success);
    assert_eq!(canonical.correlation().remote_call_id(), Some("remote-7"));
    assert_eq!(canonical.parts(), parts);
    assert_eq!(tool.captured().len(), 1);
    assert_eq!(tool.remaining_steps(), 0);

    let retained = progress
        .try_recv()
        .ok_or("expected one bounded progress event")?;
    assert_eq!(retained.sequence(), 0);
    assert_eq!(retained.call_id(), ids.call);
    assert_eq!(retained.timestamp(), Timestamp::new(42, 7)?);
    assert_eq!(retained.message(), "started");
    assert!(!format!("{retained:?}").contains("started"));
    sink.report("after terminal", None, None)?;
    assert!(progress.try_recv().is_none());
    Ok(())
}

#[test]
fn conformance_rejects_invalid_arguments_without_dispatch() -> Result<(), Box<dyn std::error::Error>>
{
    let tool = Arc::new(plakous_test_support::ScriptedTool::new([
        ToolOutcome::success_text("unused"),
    ]));
    let ids = ids()?;
    let workspace = tempfile::tempdir()?;
    let runtime = runtime(frozen(Arc::clone(&tool))?, &ids, &workspace)?;

    let malformed = runtime.prepare(request(&ids, r#"{"query":"a","query":"b"}"#)?);
    assert_eq!(
        malformed.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::InvalidArguments)
    );
    let schema_invalid = runtime.prepare(request(&ids, r#"{"query":1}"#)?);
    assert_eq!(
        schema_invalid.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::InvalidArguments)
    );
    assert!(tool.captured().is_empty());
    assert_eq!(tool.remaining_steps(), 1);
    Ok(())
}

#[test]
fn conformance_propagates_active_cancellation_and_closes_progress()
-> Result<(), Box<dyn std::error::Error>> {
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps([
        plakous_test_support::ScriptedToolStep::AwaitCancellation,
    ]));
    let ids = ids()?;
    let workspace = tempfile::tempdir()?;
    let runtime = runtime(frozen(Arc::clone(&tool))?, &ids, &workspace)?;
    let token = CancellationToken::new();
    let (context, progress, sink) = context(&ids, &workspace, token.clone(), 2)?;
    let invocation = runtime.prepare(request(&ids, r#"{"query":"wait"}"#)?)?;

    let (result, ()) = test_block_on(async {
        join!(
            execute_admitted(&runtime, invocation, context),
            async move {
                token.cancel();
            }
        )
    });
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Cancelled)
    );
    assert_eq!(tool.captured().len(), 1);
    sink.report("suppressed", None, None)?;
    assert!(progress.try_recv().is_none());
    Ok(())
}

#[test]
fn prepared_invocation_cannot_cross_frozen_registry_boundaries()
-> Result<(), Box<dyn std::error::Error>> {
    let first = Arc::new(plakous_test_support::ScriptedTool::new([
        ToolOutcome::success_text("first"),
    ]));
    let second = Arc::new(plakous_test_support::ScriptedTool::new([
        ToolOutcome::success_text("second"),
    ]));
    let ids = ids()?;
    let workspace = tempfile::tempdir()?;
    let first_runtime = runtime(frozen(first)?, &ids, &workspace)?;
    let second_runtime = runtime(frozen(Arc::clone(&second))?, &ids, &workspace)?;
    let invocation = first_runtime.prepare(request(&ids, r#"{"query":"bound"}"#)?)?;

    let result = test_block_on(second_runtime.authorize(invocation));
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Execution)
    );
    assert!(second.captured().is_empty());
    assert_eq!(second.remaining_steps(), 1);
    Ok(())
}

#[test]
fn runtime_reapplies_its_own_outcome_limits() -> Result<(), Box<dyn std::error::Error>> {
    let parts = vec![
        ToolResultPart::Text(TextBlock::new("first", None)?),
        ToolResultPart::Text(TextBlock::new("second", None)?),
    ];
    let oversized = ToolOutcome::new(
        ToolOutcomeKind::Success,
        parts,
        ToolOutcomeLimits {
            max_parts: 2,
            max_bytes: 1_024,
        },
    )?;
    let tool = Arc::new(plakous_test_support::ScriptedTool::new([Ok(oversized)]));
    let ids = ids()?;
    let workspace = tempfile::tempdir()?;
    let runtime = runtime_with_limits(
        frozen(tool)?,
        &ids,
        &workspace,
        ToolOutcomeLimits {
            max_parts: 1,
            max_bytes: 1_024,
        },
    )?;
    let invocation = runtime.prepare(request(&ids, r#"{"query":"bounded"}"#)?)?;
    let (context, progress, sink) = context(&ids, &workspace, CancellationToken::new(), 1)?;

    let result = test_block_on(execute_admitted(&runtime, invocation, context));
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::ResourceLimit)
    );
    sink.report("must stay closed", None, None)?;
    assert!(progress.try_recv().is_none());
    Ok(())
}

#[test]
fn context_call_binding_and_prepare_failures_close_progress()
-> Result<(), Box<dyn std::error::Error>> {
    let tool = Arc::new(plakous_test_support::ScriptedTool::new([
        ToolOutcome::success_text("unused"),
    ]));
    let ids = ids()?;
    let workspace = tempfile::tempdir()?;
    let runtime = runtime(frozen(Arc::clone(&tool))?, &ids, &workspace)?;
    let invocation = runtime.prepare(request(&ids, r#"{"query":"bound"}"#)?)?;
    let other_call = CallId::from_str("00000000-0000-0000-0000-000000000005")?;
    let (wrong_context, _wrong_progress, _wrong_sink) =
        context_for_call(&ids, other_call, &workspace, CancellationToken::new(), 1)?;
    let mismatch = test_block_on(execute_admitted(&runtime, invocation, wrong_context));
    assert_eq!(
        mismatch.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Execution)
    );
    assert!(tool.captured().is_empty());

    let (_control, progress, sink) = context(&ids, &workspace, CancellationToken::new(), 1)?;
    let invalid = runtime.prepare(request(&ids, r#"{"query":1}"#)?);
    sink.close();
    assert_eq!(
        invalid.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::InvalidArguments)
    );
    sink.report("must stay closed", None, None)?;
    assert!(progress.try_recv().is_none());
    assert!(tool.captured().is_empty());
    Ok(())
}

#[test]
fn runtime_serializes_overlapping_serial_calls() -> Result<(), Box<dyn std::error::Error>> {
    let tool = Arc::new(plakous_test_support::ScriptedTool::from_steps([
        plakous_test_support::ScriptedToolStep::AwaitCancellation,
        plakous_test_support::ScriptedToolStep::Complete(ToolOutcome::success_text("unused")),
    ]));
    let first_ids = ids()?;
    let second_ids = Ids {
        run: first_ids.run,
        turn: first_ids.turn,
        attempt: first_ids.attempt,
        call: CallId::from_str("00000000-0000-0000-0000-000000000005")?,
    };
    let workspace = tempfile::tempdir()?;
    let runtime = runtime(frozen(Arc::clone(&tool))?, &first_ids, &workspace)?;
    let first = runtime.prepare(request(&first_ids, r#"{"query":"first"}"#)?)?;
    let second = runtime.prepare(request(&second_ids, r#"{"query":"second"}"#)?)?;
    let cancellation = CancellationToken::new();
    let (first_context, _first_progress, _first_sink) =
        context(&first_ids, &workspace, cancellation.clone(), 1)?;
    let (second_context, _second_progress, _second_sink) =
        context(&second_ids, &workspace, CancellationToken::new(), 1)?;

    let (first_result, second_result, ()) = test_block_on(async {
        futures::join!(
            execute_admitted(&runtime, first, first_context),
            execute_admitted(&runtime, second, second_context),
            async move { cancellation.cancel() }
        )
    });
    assert_eq!(
        first_result.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Cancelled)
    );
    assert!(second_result.is_ok());
    assert_eq!(tool.captured().len(), 2);
    assert_eq!(tool.remaining_steps(), 0);
    Ok(())
}

#[test]
fn public_runtime_values_preserve_send_sync_contracts() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send + ?Sized>() {}

    assert_send_sync::<FrozenToolRegistry>();
    assert_send_sync::<ToolRuntime>();
    assert_send_sync::<ToolInvocationRequest>();
    assert_send_sync::<ToolInvocation>();
    assert_send_sync::<ValidatedArguments>();
    assert_send_sync::<ToolContext>();
    assert_send_sync::<ToolExecutionControl>();
    assert_send_sync::<ProgressSink>();
    assert_send_sync::<ToolOutcome>();
    assert_send_sync::<ToolExposure>();
    assert_send_sync::<ToolExposureMap>();
    assert_send_sync::<ToolExposureBinding>();
    assert_send::<ToolFuture>();
}

struct CleanupTool {
    release_cleanup: Arc<AtomicBool>,
    cleaned: Arc<AtomicBool>,
}

impl plakous_tools::ToolExecutor for CleanupTool {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.cancellation().cancelled().await;
            std::future::poll_fn(|wake| {
                if self.release_cleanup.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    wake.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            self.cleaned.store(true, Ordering::Release);
            ToolOutcome::success_text("cleaned")
        })
    }
}

#[test]
fn cancellation_preserves_executor_result_after_cleanup() -> Result<(), Box<dyn std::error::Error>>
{
    let release_cleanup = Arc::new(AtomicBool::new(false));
    let cleaned = Arc::new(AtomicBool::new(false));
    let tool = Arc::new(CleanupTool {
        release_cleanup: Arc::clone(&release_cleanup),
        cleaned: Arc::clone(&cleaned),
    });
    let mut builder = ToolRegistryBuilder::default();
    builder.register(ToolRegistration::new(
        RegisteredToolId::new("cleanup.tool")?,
        ToolExecutorId::new("cleanup.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"cleanup.executor"])?,
        definition("lookup")?,
        tool,
    ))?;
    let ids = ids()?;
    let workspace = tempfile::tempdir()?;
    let runtime = runtime(builder.freeze(), &ids, &workspace)?;
    let invocation = runtime.prepare(request(&ids, r#"{"query":"cleanup"}"#)?)?;
    let cancellation = CancellationToken::new();
    let (context, _progress, _sink) = context(&ids, &workspace, cancellation.clone(), 1)?;

    let (result, ()) = test_block_on(async {
        futures::join!(
            execute_admitted(&runtime, invocation, context),
            async move {
                cancellation.cancel();
                let mut yielded = false;
                std::future::poll_fn(|wake| {
                    if yielded {
                        Poll::Ready(())
                    } else {
                        yielded = true;
                        wake.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                release_cleanup.store(true, Ordering::Release);
            }
        )
    });
    assert!(result.is_ok());
    assert!(cleaned.load(Ordering::Acquire));
    Ok(())
}

proptest! {
    #[test]
    fn canonicalization_is_independent_of_object_insertion_order(
        left in -9_007_199_254_740_991_i64..=9_007_199_254_740_991_i64,
        right in -9_007_199_254_740_991_i64..=9_007_199_254_740_991_i64,
    ) {
        let first = json!({"z": right, "a": left});
        let second = json!({"a": left, "z": right});
        let first_bytes = SemanticJsonV1::from_value(first)
            .map(|value| value.canonical_bytes().to_vec());
        let second_bytes = SemanticJsonV1::from_value(second)
            .map(|value| value.canonical_bytes().to_vec());
        prop_assert!(first_bytes.is_ok());
        prop_assert_eq!(first_bytes, second_bytes);
    }
}
