use std::collections::VecDeque;
use std::future::ready;

use futures_channel::mpsc;
use futures_util::SinkExt as _;
use plakous_model::{ModelRetryCause, ModelRetryLoss, ModelTransportFault};

use super::*;
use crate::attempt::{LedgerPolicy, PhysicalRequestOutcome};

type Terminal = &'static str;
type Step = (PhysicalRequestOutcome, GenerationEnd<Terminal>);

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Finished(Terminal, bool),
    Cancelled,
    Deadline(Terminal),
}

struct Script {
    ledger: PhysicalRequestLedger,
    steps: VecDeque<Step>,
    generations: Vec<Generation>,
    reauthorizations: Vec<bool>,
    renewal_started: Option<Instant>,
    notices: Vec<ModelRetry>,
    sink: Option<mpsc::Sender<()>>,
}

impl Script {
    fn new<const N: usize>(steps: [Step; N]) -> Self {
        let ledger = PhysicalRequestLedger::new(LedgerPolicy::RESPONSES_FAMILY);
        assert!(ledger.admit_service().is_ok());
        Self {
            ledger,
            steps: steps.into(),
            generations: Vec::new(),
            reauthorizations: Vec::new(),
            renewal_started: None,
            notices: Vec::new(),
            sink: None,
        }
    }
}

impl FamilyExchangeHooks for Script {
    type Result = Outcome;
    type Terminal = Terminal;

    fn ledger(&self) -> &PhysicalRequestLedger {
        &self.ledger
    }

    fn exchange(
        &mut self,
        generation: Generation,
    ) -> impl Future<Output = GenerationEnd<Terminal>> + Send {
        let Some((outcome, end)) = self.steps.pop_front() else {
            panic!("unexpected extra dispatch")
        };
        self.generations.push(generation);
        assert!(self.ledger.physical_request_started().is_ok());
        assert!(self.ledger.physical_request_finished(outcome).is_ok());
        ready(end)
    }

    async fn announce_retry(&mut self, retry: ModelRetry) -> bool {
        self.notices.push(retry);
        if let Some(sink) = &mut self.sink {
            // As in Responses, a closed receiver is not a stalled observer.
            let _ = sink.send(()).await;
        }
        true
    }

    async fn reauthorize(&mut self, renew: bool) -> Reauthorization {
        self.reauthorizations.push(renew);
        if renew {
            self.renewal_started = Some(Instant::now());
            tokio::time::sleep(Duration::from_secs(2)).await;
            return Reauthorization::Send(self.renewal_started);
        }
        Reauthorization::Send(None)
    }

    fn finish(
        &mut self,
        terminal: Terminal,
        deferred: bool,
    ) -> impl Future<Output = Outcome> + Send {
        ready(Outcome::Finished(terminal, deferred))
    }

    fn cancelled(&mut self) -> Outcome {
        Outcome::Cancelled
    }

    fn deadline(&mut self, terminal: Terminal) -> impl Future<Output = Outcome> + Send {
        ready(Outcome::Deadline(terminal))
    }
}

const fn host(retry_delivery: RetryDelivery) -> FamilyExchange {
    FamilyExchange {
        timeout: Duration::from_secs(5),
        hard_deadline: None,
        turn_rooted: true,
        redispatch_available: true,
        retry_delivery,
    }
}

const fn renewed(terminal: Terminal) -> Step {
    (
        PhysicalRequestOutcome::DefiniteRejection,
        GenerationEnd::Renew(terminal),
    )
}

const fn finished() -> Step {
    (
        PhysicalRequestOutcome::AcceptedDisconnect,
        GenerationEnd::Finished("terminal"),
    )
}

fn interrupted() -> Step {
    (
        PhysicalRequestOutcome::AcceptedDisconnect,
        GenerationEnd::Retry {
            terminal: "stream cut",
            failure: RetryFailure::interrupted(
                ModelTransportFault::EndOfStream,
                String::new(),
                true,
                ModelRetryLoss::default(),
            ),
        },
    )
}

fn busy(retry_after: Duration) -> Step {
    let Some(failure) = RetryFailure::status(503, true, Some(retry_after), String::new()) else {
        panic!("503 must be replayable")
    };
    (
        PhysicalRequestOutcome::DefiniteRejection,
        GenerationEnd::Retry {
            terminal: "busy",
            failure,
        },
    )
}

#[tokio::test(start_paused = true)]
async fn renewal_runs_once_and_the_next_dispatch_keeps_its_start_time() {
    let mut script = Script::new([renewed("first 401"), renewed("second 401")]);
    let driver = host(RetryDelivery::BeforeDelay);
    let timeout = driver.timeout;
    let outcome = driver.run(&mut script, &CancellationToken::new()).await;

    assert_eq!(outcome, Outcome::Finished("second 401", true));
    assert_eq!(script.reauthorizations, [true]);
    assert_eq!(script.generations.len(), 2);
    let next = script.generations[1];
    assert_eq!(Some(next.started), script.renewal_started);
    assert_eq!(next.deadline, next.started + timeout);
    assert!(next.started.elapsed() >= Duration::from_secs(2));
}

#[tokio::test(start_paused = true)]
async fn a_401_does_not_renew_after_the_physical_retry_budget_is_spent() {
    let mut script = Script::new([
        busy(Duration::ZERO),
        busy(Duration::ZERO),
        busy(Duration::ZERO),
        renewed("last 401"),
    ]);
    let outcome = host(RetryDelivery::BeforeDelay)
        .run(&mut script, &CancellationToken::new())
        .await;

    assert_eq!(outcome, Outcome::Finished("last 401", true));
    assert_eq!(script.generations.len(), 4);
    assert_eq!(script.reauthorizations, [false; 3]);
}

#[tokio::test(start_paused = true)]
async fn a_saturated_sink_cannot_miss_a_discard_and_then_see_a_replay() {
    let (mut sink, receiver) = mpsc::channel(0);
    assert!(sink.try_send(()).is_ok());
    let mut script = Script::new([interrupted()]);
    script.sink = Some(sink);
    let outcome = host(RetryDelivery::DuringDelay)
        .run(&mut script, &CancellationToken::new())
        .await;
    drop(receiver);

    assert_eq!(outcome, Outcome::Finished("stream cut", true));
    assert_eq!(script.notices.len(), 1);
    assert!(script.notices[0].output_discarded());
    assert_eq!(script.reauthorizations, [false; 0]);
}

#[tokio::test(start_paused = true)]
async fn a_closed_sink_does_not_prevent_discarding_replay() {
    let (sink, receiver) = mpsc::channel(0);
    drop(receiver);
    let mut script = Script::new([interrupted(), finished()]);
    script.sink = Some(sink);
    let outcome = host(RetryDelivery::DuringDelay)
        .run(&mut script, &CancellationToken::new())
        .await;

    assert_eq!(outcome, Outcome::Finished("terminal", false));
    assert!(script.notices[0].output_discarded());
    assert_eq!(script.reauthorizations, [false]);
}

#[tokio::test(start_paused = true)]
async fn the_hard_deadline_can_expire_while_a_retry_notice_waits_for_capacity() {
    let (mut sink, receiver) = mpsc::channel(0);
    assert!(sink.try_send(()).is_ok());
    let retry_delay = Duration::from_millis(100);
    let mut script = Script::new([busy(retry_delay)]);
    let mut driver = host(RetryDelivery::DuringDelay);
    driver.hard_deadline = Some(Instant::now() + Duration::from_millis(500));
    script.sink = Some(sink);
    let outcome = driver.run(&mut script, &CancellationToken::new()).await;
    drop(receiver);

    assert_eq!(outcome, Outcome::Deadline("busy"));
    assert_eq!(script.notices.len(), 1);
    assert_eq!(script.notices[0].delay(), retry_delay);
    assert_eq!(script.reauthorizations, [false; 0]);
}

#[tokio::test(start_paused = true)]
async fn record_rejection_forces_a_fresh_connection_for_only_the_next_generation() {
    let rejected_record = (
        PhysicalRequestOutcome::DefiniteRejection,
        GenerationEnd::Retry {
            terminal: "record rejected",
            failure: RetryFailure::record_rejection_fixture(),
        },
    );
    let mut script = Script::new([rejected_record, busy(Duration::ZERO), finished()]);
    let outcome = host(RetryDelivery::BeforeDelay)
        .run(&mut script, &CancellationToken::new())
        .await;

    assert_eq!(outcome, Outcome::Finished("terminal", false));
    assert_eq!(
        script.notices[0].cause(),
        ModelRetryCause::RecordRejectedInTransit
    );
    assert_eq!(
        script
            .generations
            .iter()
            .map(|generation| generation.fresh_connection)
            .collect::<Vec<_>>(),
        [false, true, false]
    );
}
