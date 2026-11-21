//! Responses' raw terminal and observation hooks for the shared exchange.

use plakous_exchange::attempt::PhysicalRequestLedger;
use plakous_exchange::family::{
    FamilyExchangeHooks, Generation, GenerationEnd, Reauthorization, RetryFailure,
    TransportRejection, transport_fault,
};
use plakous_exchange::retry_phase;
use plakous_model::{ModelRetry, ModelRetryDeclineReason, ModelRetryPhase, ModelTransportFault};
use tokio::time::Instant;

use super::dispatch::{execute_admitted, refusal_status, status_outcome};
use super::{
    Arc, Client, PhysicalRequestOutcome, ResponsesAttempt, ResponsesAttemptDialectData,
    ResponsesAttemptError, ResponsesAttemptResponse, ResponsesAuthorizationAdapter,
    ResponsesFamilyStreamError, ResponsesHttpStatusClass, ResponsesObservation, StatusCode,
    StreamProgress,
};

pub(super) struct ResponsesExchange<D, G> {
    pub(super) client: Client,
    pub(super) dialect: Arc<D>,
    pub(super) data: ResponsesAttemptDialectData,
    pub(super) authorization: ResponsesAuthorizationAdapter<D, G>,
    pub(super) attempt: ResponsesAttempt,
}

type AttemptResult = Result<ResponsesAttemptResponse, ResponsesAttemptError>;

impl<D: Send + Sync + 'static, G: Send + 'static> FamilyExchangeHooks for ResponsesExchange<D, G> {
    type Result = AttemptResult;
    type Terminal = AttemptResult;

    fn ledger(&self) -> &PhysicalRequestLedger {
        self.attempt.core.observations_ref()
    }

    async fn exchange(&mut self, generation: Generation) -> GenerationEnd<Self::Terminal> {
        let attempt = &mut self.attempt;
        attempt.deadline = generation.deadline.into_std();
        attempt.stream.physical_started = Some(generation.started.into_std());
        let terminal = execute_admitted(
            self.client.clone(),
            Arc::clone(&self.dialect),
            self.data.clone(),
            self.authorization,
            attempt,
            generation.fresh_connection,
        )
        .await;
        if let Err(ResponsesAttemptError::HttpStatus {
            retry_disabled: true,
            ..
        }) = &terminal
        {
            attempt.stream.retry_disabled = true;
        }
        if attempt.retry_permitted
            && matches!(
                &terminal,
                Err(ResponsesAttemptError::HttpStatus {
                    status: StatusCode::UNAUTHORIZED,
                    class: ResponsesHttpStatusClass::Authentication,
                    retry_disabled: false,
                    ..
                })
            )
        {
            return GenerationEnd::Renew(terminal);
        }
        let stream = attempt.stream;
        let failure = match &terminal {
            Ok(_) => stream
                .refused_in_band
                .map(|fault| stream_failure(stream, fault, format!("in-band refusal ({fault})"))),
            Err(error) => retry_failure(error, stream),
        };
        let Some(mut failure) = failure else {
            return GenerationEnd::Finished(terminal);
        };
        if stream.retry_disabled {
            failure = failure.with_decline(ModelRetryDeclineReason::RetryDisabled);
        }
        if let Some(request) = attempt.request_log.take() {
            failure = failure.with_request(request);
        }
        GenerationEnd::Retry { terminal, failure }
    }

    async fn announce_retry(&mut self, retry: ModelRetry) -> bool {
        if let Some(sender) = &self.attempt.observations {
            if retry.loss().declined_reason.is_some() {
                let _ = sender.try_send(ResponsesObservation::Retry(retry));
            } else {
                let _ = sender.send(ResponsesObservation::Retry(retry)).await;
            }
        }
        true
    }

    async fn reauthorize(&mut self, renew: bool) -> Reauthorization {
        if !renew {
            // Each new preflight obtains fresh authority rather than reusing a lease.
            return Reauthorization::Send(None);
        }
        let Some(hook) = self.authorization.credential_rejected else {
            return Reauthorization::End;
        };
        let attempt = &self.attempt;
        let started = Instant::now();
        let deadline = started
            .checked_add(attempt.dispatch_timeout)
            .unwrap_or(started);
        let deadline = attempt
            .hard_deadline
            .map_or(deadline, |hard| deadline.min(Instant::from_std(hard)));
        tokio::select! {
            biased;
            () = attempt.cancellation.cancelled() => Reauthorization::Cancelled,
            () = tokio::time::sleep_until(deadline) => Reauthorization::Deadline,
            refreshed = hook(self.dialect.as_ref()) => match refreshed {
                Ok(()) => Reauthorization::Send(Some(started)),
                Err(_) => Reauthorization::End,
            },
        }
    }

    async fn finish(&mut self, terminal: Self::Terminal, deferred: bool) -> Self::Result {
        let attempt = &self.attempt;
        match terminal {
            Ok(response) if deferred => {
                attempt.core.mark_failed();
                Ok(ResponsesAttemptResponse {
                    facts: attempt.facts(),
                    ..response
                })
            }
            Err(error) => {
                attempt.core.mark_failed();
                Err(error.with_summary(attempt.core.observations_ref().attempt_summary()))
            }
            response => response,
        }
    }

    fn reset(&mut self) {
        self.attempt.physical = None;
        self.attempt.reset_stream();
    }

    fn cancelled(&mut self) -> Self::Result {
        self.attempt.core.mark_failed();
        Err(ResponsesAttemptError::Cancelled {
            physical: self.attempt.physical(),
            facts: self.attempt.facts(),
        })
    }

    async fn deadline(&mut self, terminal: Self::Terminal) -> Self::Result {
        match terminal {
            Ok(_) => self.finish(terminal, true).await,
            Err(_) => {
                self.attempt.core.mark_failed();
                Err(ResponsesAttemptError::Deadline {
                    physical: self.attempt.physical(),
                    facts: self.attempt.facts(),
                })
            }
        }
    }
}

fn stream_failure(
    stream: StreamProgress,
    fault: ModelTransportFault,
    detail: String,
) -> RetryFailure {
    RetryFailure::interrupted(fault, detail, stream.delivered, stream.loss)
        .with_phase(Some(ModelRetryPhase::ResponseBody))
}

fn retry_failure(error: &ResponsesAttemptError, stream: StreamProgress) -> Option<RetryFailure> {
    match error {
        ResponsesAttemptError::HttpStatus {
            status,
            class,
            retry_after,
            ..
        } => {
            let refused = refusal_status(*status, *class);
            let detail = if refused == *status {
                format!("HTTP {}", status.as_u16())
            } else {
                format!(
                    "HTTP {} wrapping upstream HTTP {}",
                    status.as_u16(),
                    refused.as_u16()
                )
            };
            RetryFailure::status(
                refused.as_u16(),
                status_outcome(refused) == PhysicalRequestOutcome::DefiniteRejection,
                *retry_after,
                detail,
            )
        }
        ResponsesAttemptError::Transport { source, .. } if !stream.started => {
            RetryFailure::transport(*source, TransportRejection::from_failure(*source))
        }
        _ if !stream.interrupted => None,
        ResponsesAttemptError::Transport { source, .. } => Some(
            stream_failure(stream, transport_fault(*source)?, format!("{source:?}"))
                .with_phase(retry_phase(source.phase())),
        ),
        ResponsesAttemptError::Deadline { .. } => Some(stream_failure(
            stream,
            ModelTransportFault::Timeout,
            "stream idle timeout".to_owned(),
        )),
        ResponsesAttemptError::Stream {
            source:
                source @ (ResponsesFamilyStreamError::Truncated
                | ResponsesFamilyStreamError::MissingTerminal),
            ..
        } => Some(stream_failure(
            stream,
            ModelTransportFault::EndOfStream,
            format!("{source:?}"),
        )),
        _ => None,
    }
}
