//! The placement hold at the runner-call boundary: which calls wait, what
//! wakes them, and that nothing is sent twice or under a new key.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use plakous_model::{AttemptId, BranchId, CallId, LineId};

use super::*;

/// What one call sent: its verb, payload, key and budget.
type Sent = (RunnerVerb, Value, RunnerClass, Option<Timestamp>);

/// Answers each call from a script, after recording what it was sent.
#[derive(Default)]
struct Scripted {
    answers: Mutex<VecDeque<Result<Value, RunnerFailure>>>,
    sent: Mutex<Vec<Sent>>,
}

impl Scripted {
    fn answering(answers: impl IntoIterator<Item = Result<Value, RunnerFailure>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into_iter().collect()),
            sent: Mutex::default(),
        })
    }

    fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }
}

impl RunnerLink for Scripted {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        self.sent.lock().unwrap().push((
            call.verb(),
            call.payload().clone(),
            call.class().clone(),
            call.budget(),
        ));
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(json!({"ok": true})));
        Box::pin(std::future::ready(answer))
    }
}

fn held_services(link: Arc<Scripted>) -> RunnerServices {
    let source = ExecutionEnvironment::host(scope(), std::env::temp_dir(), ".", config()).unwrap();
    let descriptor = runner_descriptor(&source, RunnerId::new("held").unwrap()).unwrap();
    let backend = RunnerBackend::new(descriptor, link).unwrap();
    RunnerServices::new(Arc::new(backend), tokio::runtime::Handle::current())
}

fn owner() -> LineOwner {
    LineOwner::new(
        LineId::from_str("00000000-0000-0000-0000-0000000000a1").unwrap(),
        1,
        BranchId::from_str("00000000-0000-0000-0000-0000000000b1").unwrap(),
        1,
    )
}

fn model(cancellation: &CancellationToken) -> OperationContext {
    OperationContext::new(cancellation.clone())
        .with_owner(owner())
        .with_class(OperationClass::EffectCall {
            call_id: CallId::from_str("00000000-0000-0000-0000-0000000000c1").unwrap(),
            attempt_id: AttemptId::from_str("00000000-0000-0000-0000-0000000000d1").unwrap(),
        })
}

fn model_call(verb: RunnerVerb) -> RunnerCall {
    RunnerCall::new(verb, json!({"step": 1}), &model(&CancellationToken::new()))
}

fn model_poll(cursor: u64) -> RunnerCall {
    model_call(RunnerVerb::ProcessPoll).with_class(RunnerClass::Observation { job: 1, cursor })
}

async fn settled<T: Send + 'static>(task: &tokio::task::JoinHandle<T>) -> bool {
    tokio::time::sleep(Duration::from_millis(150)).await;
    task.is_finished()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_model_effect_waits_unsent_and_is_sent_once_on_release() {
    let link = Scripted::answering([]);
    let services = held_services(Arc::clone(&link));
    services.set_available(false);
    let call = tokio::spawn(
        services
            .backend()
            .link()
            .call(model_call(RunnerVerb::ProcessStart)),
    );
    assert!(!settled(&call).await, "a held effect waits");
    assert_eq!(link.sent().len(), 0, "nothing is sent while held");
    services.set_available(true);
    call.await.unwrap().unwrap();
    let sent = link.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, RunnerVerb::ProcessStart);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_not_sent_is_resent_under_its_key_only_after_a_fresh_release() {
    let link = Scripted::answering([Err(RunnerFailure::NotSent("no connection".into()))]);
    let services = held_services(Arc::clone(&link));
    let requested = Arc::new(AtomicUsize::new(0));
    {
        let requested = Arc::clone(&requested);
        services.request_reconciliation_with(move || {
            requested.fetch_add(1, Ordering::SeqCst);
            true
        });
    }
    let call = tokio::spawn(
        services
            .backend()
            .link()
            .call(model_call(RunnerVerb::ExecuteLiteralProcess)),
    );
    assert!(
        !settled(&call).await,
        "the stale open admission does not send again"
    );
    assert_eq!(link.sent().len(), 1);
    assert_eq!(
        requested.load(Ordering::SeqCst),
        1,
        "a refusal under an admission that still reads open asks for reconciliation"
    );
    services.set_available(false);
    services.set_available(true);
    call.await.unwrap().unwrap();
    let sent = link.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].1, sent[1].1, "the same payload");
    assert_eq!(sent[0].2, sent[1].2, "the same call key");
    assert!(matches!(sent[1].2, RunnerClass::Effect { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permanent_refusals_and_possible_effects_are_never_sent_again() {
    for failure in [
        RunnerFailure::Unavailable("the call could not be recorded".into()),
        RunnerFailure::StaleOwner,
        RunnerFailure::OutcomeUnknown("the link was lost after send".into()),
        RunnerFailure::ReadInterrupted("an effect is not a read".into()),
    ] {
        let link = Scripted::answering([Err(failure.clone())]);
        let services = held_services(Arc::clone(&link));
        let answer = tokio::time::timeout(
            Duration::from_secs(10),
            services
                .backend()
                .link()
                .call(model_call(RunnerVerb::MutateWorkspace)),
        )
        .await
        .unwrap();
        assert_eq!(answer, Err(failure));
        assert_eq!(link.sent().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controls_tree_steps_and_host_calls_never_wait_on_a_hold() {
    let link = Scripted::answering([]);
    let services = held_services(Arc::clone(&link));
    services.set_available(false);
    let admitted = services.backend().link();
    let controlled = OperationContext::new(CancellationToken::new()).with_owner(owner());
    let calls = [
        RunnerCall::new(RunnerVerb::ProcessStop, json!({}), &controlled)
            .with_class(RunnerClass::Control),
        RunnerCall::recorded_control(
            RunnerVerb::JobsStop,
            json!({}),
            owner(),
            "plak:stop:1".into(),
        ),
        RunnerCall::tree(RunnerVerb::VcsEffect, json!({}), "tree", "operation"),
        RunnerCall::new(RunnerVerb::ProcessStart, json!({}), &controlled),
        model_call(RunnerVerb::ProcessWrite),
        model_call(RunnerVerb::ProcessPromote),
        model_call(RunnerVerb::ServiceSpawn),
        model_call(RunnerVerb::VcsEffect),
    ];
    for call in calls {
        let verb = call.verb();
        let answer = tokio::time::timeout(Duration::from_secs(10), admitted.call(call))
            .await
            .unwrap();
        assert!(
            matches!(answer, Err(RunnerFailure::NotSent(_))),
            "{verb:?}: {answer:?}"
        );
    }
    assert_eq!(link.sent().len(), 0);
    // A host's own read still passes a held placement.
    admitted
        .call(RunnerCall::unkeyed(RunnerVerb::JobsSnapshot, json!({})))
        .await
        .unwrap();
    assert_eq!(link.sent().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_and_refusal_end_a_held_wait_unsent() {
    let link = Scripted::answering([]);
    let services = held_services(Arc::clone(&link));
    services.set_available(false);
    let cancellation = CancellationToken::new();
    let cancelled = tokio::spawn(services.backend().link().call(RunnerCall::new(
        RunnerVerb::ProcessStart,
        json!({}),
        &model(&cancellation),
    )));
    let refused = tokio::spawn(
        services
            .backend()
            .link()
            .call(model_call(RunnerVerb::MutateWorkspace)),
    );
    assert!(!settled(&cancelled).await);
    cancellation.cancel();
    let answer = tokio::time::timeout(Duration::from_secs(10), cancelled)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(answer, Err(RunnerFailure::NotSent(_))),
        "{answer:?}"
    );
    assert!(!refused.is_finished());
    // Identity replacement or a refused placement ends the remaining holds.
    services.refuse();
    let answer = tokio::time::timeout(Duration::from_secs(10), refused)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(answer, Err(RunnerFailure::Unavailable(_))),
        "{answer:?}"
    );
    assert_eq!(link.sent().len(), 0);
    // A new call refuses at once until admission opens again.
    let answer = services
        .backend()
        .link()
        .call(model_call(RunnerVerb::ProcessStart))
        .await;
    assert!(matches!(answer, Err(RunnerFailure::Unavailable(_))));
    services.set_available(true);
    services
        .backend()
        .link()
        .call(model_call(RunnerVerb::ProcessStart))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_model_read_is_read_again_at_its_cursor_after_release() {
    let link = Scripted::answering([Err(RunnerFailure::ReadInterrupted("lost".into()))]);
    let services = held_services(Arc::clone(&link));
    let poll = tokio::spawn(services.backend().link().call(model_poll(7)));
    assert!(!settled(&poll).await, "an interrupted read waits");
    assert_eq!(link.sent().len(), 1);
    // The link's owner holds, then releases, the placement.
    services.set_available(false);
    services.set_available(true);
    poll.await.unwrap().unwrap();
    let sent = link.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].1, sent[1].1);
    assert_eq!(sent[1].2, RunnerClass::Observation { job: 1, cursor: 7 });

    // A host's own read is not the hold's to repeat.
    let link = Scripted::answering([Err(RunnerFailure::ReadInterrupted("lost".into()))]);
    let services = held_services(Arc::clone(&link));
    let answer = services
        .backend()
        .link()
        .call(RunnerCall::unkeyed(RunnerVerb::JobsSnapshot, json!({})))
        .await;
    assert!(matches!(answer, Err(RunnerFailure::ReadInterrupted(_))));
    assert_eq!(link.sent().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_unsent_call_budget_excludes_its_hold() {
    let link = Scripted::answering([]);
    let services = held_services(Arc::clone(&link));
    services.set_available(false);
    let budget = Timestamp::new(plakous_model::now_unix_secs() + 60, 0).unwrap();
    let mut call = services.backend().link().call(RunnerCall::new(
        RunnerVerb::ProcessStart,
        json!({}),
        &model(&CancellationToken::new()).with_budget(budget),
    ));
    // Enter the hold before advancing its clock; task scheduling time is
    // not time spent held, and timer wakeups need not be exactly 300 ms.
    std::future::poll_fn(|context| {
        assert!(call.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(link.sent().len(), 0);
    tokio::time::advance(Duration::from_millis(300)).await;
    services.set_available(true);
    call.await.unwrap();
    let sent = link.sent()[0].3.unwrap();
    let moved = (u128::from(sent.seconds()) * 1_000_000_000 + u128::from(sent.nanoseconds()))
        - u128::from(budget.seconds()) * 1_000_000_000;
    assert_eq!(moved, 300_000_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_driver_gate_reads_held_until_release_or_refusal() {
    let services = held_services(Scripted::answering([]));
    let mut gate = services.backend().link().placement().unwrap();
    assert!(!gate.is_held());
    services.set_available(false);
    assert!(gate.is_held());
    let released = tokio::spawn(async move { gate.released().await });
    assert!(!settled(&released).await);
    services.refuse();
    tokio::time::timeout(Duration::from_secs(10), released)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_coalesced_reconciliation_request_never_strands_its_call() {
    let link = Scripted::answering([Err(RunnerFailure::NotSent("no connection".into()))]);
    let services = held_services(Arc::clone(&link));
    // The owner coalesced this request away: it starts no reconciliation,
    // so no fresh revision would ever come.
    services.request_reconciliation_with(|| false);
    let answer = tokio::time::timeout(
        Duration::from_secs(10),
        services
            .backend()
            .link()
            .call(model_call(RunnerVerb::ExecuteLiteralProcess)),
    )
    .await
    .unwrap();
    answer.unwrap();
    let sent = link.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].2, sent[1].2, "the same call key");
}
