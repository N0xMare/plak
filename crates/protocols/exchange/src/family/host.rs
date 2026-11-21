//! One logical HTTP exchange driver over a family's typed generation hooks.

use std::{future::Future, time::Duration};

use plakous_model::{CancellationToken, ModelRetry};
use tokio::time::Instant;

use crate::attempt::{PhysicalRequestLedger, TerminalFacts};

use super::retry::{RetryBudget, RetryFailure, RetryWindow};

/// The fresh bounds and connection selection for a physical generation.
#[derive(Clone, Copy)]
pub struct Generation {
    /// Start of its dispatch budget, including any credential renewal.
    pub started: Instant,
    /// Dispatch deadline through the response head.
    pub deadline: Instant,
    /// Whether it must not reuse an earlier connection.
    pub fresh_connection: bool,
}

/// A physical generation has ended; only the host can start another.
#[allow(
    clippy::large_enum_variant,
    reason = "consumed once at the generation boundary"
)]
pub enum GenerationEnd<T> {
    /// No retry evaluation: the family's terminal work is already done.
    Finished(T),
    /// A refusal or loss that must pass the physical retry policy.
    Retry {
        /// Terminal to use if replay is not allowed.
        terminal: T,
        /// The evidence for the retry decision.
        failure: RetryFailure,
    },
    /// A definite credential refusal eligible for the one renewal hook.
    Renew(T),
}

/// Delivery semantics of the family's existing observation channel.
#[derive(Clone, Copy)]
pub enum RetryDelivery {
    /// Publish before starting the delay; losing the sink cancels the attempt.
    BeforeDelay,
    /// Publish during the delay. If a live sink cannot be told to discard
    /// output, do not replay it. A closed sink does not block replay.
    DuringDelay,
}

/// Hooks for the work specific to a wire family. A generation closes its
/// physical ledger entry before returning a retry or renewal candidate.
pub trait FamilyExchangeHooks: Send {
    /// The result of the logical attempt, including its typed error.
    type Result: Send;
    /// A terminal candidate held during the wait or credential renewal.
    type Terminal: Send;

    /// The admitted attempt's exact shared ledger.
    fn ledger(&self) -> &PhysicalRequestLedger;

    /// Authorize and execute one generation, including its wire decoding.
    fn exchange(
        &mut self,
        generation: Generation,
    ) -> impl Future<Output = GenerationEnd<Self::Terminal>> + Send;

    /// Emit a retry decision in the family's observation vocabulary.
    fn announce_retry(&mut self, retry: ModelRetry) -> impl Future<Output = bool> + Send;

    /// Revalidate before replay or renew after 401. A successful hook returns
    /// the start of the next generation's budget, if it already began.
    fn reauthorize(&mut self, renew: bool) -> impl Future<Output = Reauthorization> + Send;

    /// Finish without another dispatch. A deferred terminal may need sealing
    /// or publication using the final ledger facts.
    fn finish(
        &mut self,
        terminal: Self::Terminal,
        deferred: bool,
    ) -> impl Future<Output = Self::Result> + Send;

    /// Prepare family-local state for another generation.
    fn reset(&mut self) {}

    /// End a cancelled retry.
    fn cancelled(&mut self) -> Self::Result;

    /// End a retry whose hard deadline expired.
    fn deadline(&mut self, terminal: Self::Terminal) -> impl Future<Output = Self::Result> + Send;
}

/// Result of the family's fresh credential/dispatch fence.
pub enum Reauthorization {
    /// Send another generation; any time spent renewing counts against it.
    Send(Option<Instant>),
    /// No valid reauthorization, so keep the current terminal.
    End,
    /// The attempt was cancelled while reauthorizing.
    Cancelled,
    /// The exchange budget expired while reauthorizing.
    Deadline,
}

/// Runs the physical generations of one admitted, route-bound operation.
pub struct FamilyExchange {
    /// Budget for each fresh dispatch.
    pub timeout: Duration,
    /// The caller's independent hard deadline, if any.
    pub hard_deadline: Option<Instant>,
    /// The admitted operation permits ordinary turn replay.
    pub turn_rooted: bool,
    /// Another generation can be reauthorized.
    pub redispatch_available: bool,
    /// The family's ordered live retry delivery contract.
    pub retry_delivery: RetryDelivery,
}

impl FamilyExchange {
    /// Drives one admitted attempt to its terminal. The same budget applies
    /// to all re-sends, including those after credential renewal.
    pub async fn run<H: FamilyExchangeHooks>(
        self,
        hooks: &mut H,
        cancellation: &CancellationToken,
    ) -> H::Result {
        let mut budget = RetryBudget::default();
        let mut renewed = false;
        let mut next_started = None;
        let mut fresh_connection = false;
        loop {
            let started = next_started.take().unwrap_or_else(Instant::now);
            let deadline = started.checked_add(self.timeout).unwrap_or(started);
            let generation = Generation {
                started,
                deadline: self
                    .hard_deadline
                    .map_or(deadline, |hard| hard.min(deadline)),
                fresh_connection: std::mem::take(&mut fresh_connection),
            };
            let (terminal, renew) = match hooks.exchange(generation).await {
                GenerationEnd::Finished(terminal) => return hooks.finish(terminal, false).await,
                GenerationEnd::Renew(terminal) => {
                    let facts = TerminalFacts::from_snapshot(hooks.ledger().snapshot());
                    if renewed || !budget.permits_renewal(facts) {
                        return hooks.finish(terminal, true).await;
                    }
                    renewed = true;
                    (terminal, true)
                }
                GenerationEnd::Retry { terminal, failure } => {
                    fresh_connection = failure.needs_fresh_connection();
                    let retry = budget.evaluate(
                        hooks.ledger(),
                        RetryWindow {
                            remaining: self
                                .hard_deadline
                                .map(|hard| hard.saturating_duration_since(Instant::now())),
                            elapsed: started.elapsed(),
                            redispatch_available: self.redispatch_available,
                            turn_rooted: self.turn_rooted,
                        },
                        failure,
                    );
                    if retry.loss().declined_reason.is_some() {
                        let _ = hooks.announce_retry(retry).await;
                        return hooks.finish(terminal, true).await;
                    }
                    let delay = retry.delay();
                    match self.wait(hooks, cancellation, retry).await {
                        Reauthorization::Send(_) => budget.waited(delay),
                        Reauthorization::End => return hooks.finish(terminal, true).await,
                        Reauthorization::Cancelled => return hooks.cancelled(),
                        Reauthorization::Deadline => return hooks.deadline(terminal).await,
                    }
                    (terminal, false)
                }
            };
            match hooks.reauthorize(renew).await {
                Reauthorization::Send(started) => next_started = started,
                Reauthorization::End => return hooks.finish(terminal, true).await,
                Reauthorization::Cancelled => return hooks.cancelled(),
                Reauthorization::Deadline => return hooks.deadline(terminal).await,
            }
            hooks.reset();
        }
    }

    async fn wait<H: FamilyExchangeHooks>(
        &self,
        hooks: &mut H,
        cancellation: &CancellationToken,
        retry: ModelRetry,
    ) -> Reauthorization {
        let delay = retry.delay();
        match self.retry_delivery {
            RetryDelivery::BeforeDelay => {
                if !hooks.announce_retry(retry).await {
                    return Reauthorization::Cancelled;
                }
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => Reauthorization::Cancelled,
                    () = tokio::time::sleep(delay) => Reauthorization::Send(None),
                }
            }
            RetryDelivery::DuringDelay => {
                let discards = retry.output_discarded();
                let resume = Instant::now() + delay;
                let pause = async {
                    let announced = tokio::time::timeout(
                        delay.max(Duration::from_secs(1)),
                        hooks.announce_retry(retry),
                    )
                    .await
                    .is_ok_and(|sent| sent);
                    tokio::time::sleep_until(resume).await;
                    if announced || !discards {
                        Reauthorization::Send(None)
                    } else {
                        Reauthorization::End
                    }
                };
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => Reauthorization::Cancelled,
                    () = wait_deadline(self.hard_deadline) => Reauthorization::Deadline,
                    result = pause => result,
                }
            }
        }
    }
}

pub(super) async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
