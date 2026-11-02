//! One bounded exchange/retry owner for isolated side requests.

#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    reason = "closed side-request vocabulary and explicit replay authority"
)]

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use plakous_attempt::{
    FailurePoint, Jitter, ObservationSnapshot, ProviderAcceptance, ReplaySafety, RetryPolicy,
    TerminalFacts,
};
use plakous_http::{
    CancellationToken as HttpCancellationToken, Client, ExchangeControl, Failure, Phase, Request,
    Response, TransmissionFact,
};
use plakous_model::CancellationToken;
use tokio::time::Instant;

use crate::{cancellable, read_bounded_body};

/// One URL citation, in first-reported order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetrievalSource {
    /// Source URL.
    pub url: String,
    /// Provider-reported title.
    pub title: Option<String>,
}

/// Exact usage over the side request's exchanges.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetrievalUsage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Provider-hosted tool invocations.
    pub hosted_requests: u64,
}

/// The family-neutral result; it never enters canonical model history.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RetrievalOutcome {
    /// Answer text, unchanged from the family collector.
    pub text: String,
    /// Unique source URLs in first-reported order.
    pub sources: Vec<RetrievalSource>,
    /// Usage across continuations.
    pub usage: RetrievalUsage,
    /// Successful exchanges, including any pause-turn continuations.
    pub exchanges: usize,
}

/// Content-free failure classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RetrievalErrorKind {
    /// Request is outside the family's allowlist.
    InvalidRequest,
    /// Exact-route admission or credential failed.
    Unauthorized,
    /// Provider refused the request.
    Rejected,
    /// The bounded retry could not complete.
    Unavailable,
    /// Grammar or bounds failed.
    InvalidResponse,
    /// Caller cancelled.
    Cancelled,
}

/// Redaction-safe error with transient physical accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetrievalError {
    kind: RetrievalErrorKind,
    message: String,
    status: Option<u16>,
    physical_requests: u32,
}
impl RetrievalError {
    /// Creates a failure without inventing transport evidence.
    pub fn new(kind: RetrievalErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            physical_requests: 0,
        }
    }
    /// Local request rejection.
    pub fn invalid_request(message: &str) -> Self {
        Self::new(RetrievalErrorKind::InvalidRequest, message)
    }
    /// Family grammar or bound failure.
    pub fn invalid_response(message: impl Into<String>) -> Self {
        Self::new(RetrievalErrorKind::InvalidResponse, message)
    }
    /// Failure classification.
    pub const fn kind(&self) -> RetrievalErrorKind {
        self.kind
    }
    /// Final HTTP refusal, when one was received.
    pub const fn status(&self) -> Option<u16> {
        self.status
    }
    /// Attempted physical exchanges, retries included.
    pub const fn physical_requests(&self) -> u32 {
        self.physical_requests
    }
    /// Existing safe diagnostic text.
    pub fn message(&self) -> &str {
        &self.message
    }
    /// Retains the final response status.
    #[must_use]
    pub const fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }
    /// Retains physical accounting without changing the failure class or text.
    #[must_use]
    pub const fn with_physical_requests(mut self, physical: u32) -> Self {
        self.physical_requests = physical;
        self
    }
}
impl fmt::Display for RetrievalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for RetrievalError {}

/// A route-owned lease and dispatch permit, retained through the response head.
pub trait RetrievalGuard: Send {
    /// Settles the permit from typed transport proof.
    fn transport_failed(&mut self, failure: Failure);
}

/// Exactly authorized physical request, with optional lifetime authority.
pub struct RetrievalDispatch {
    request: Request,
    guard: Option<Box<dyn RetrievalGuard>>,
}
impl RetrievalDispatch {
    /// A request retaining no extra authority.
    pub fn new(request: Request) -> Self {
        Self {
            request,
            guard: None,
        }
    }
    /// Keeps a lease/permit until the response head.
    #[must_use]
    pub fn with_guard(mut self, guard: impl RetrievalGuard + 'static) -> Self {
        self.guard = Some(Box::new(guard));
        self
    }
}
impl From<Request> for RetrievalDispatch {
    fn from(request: Request) -> Self {
        Self::new(request)
    }
}
impl fmt::Debug for RetrievalDispatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetrievalDispatch")
            .field("guarded", &self.guard.is_some())
            .finish_non_exhaustive()
    }
}
/// Revalidation result for one physical request.
pub type RetrievalAuthorization<'a> =
    Pin<Box<dyn Future<Output = Result<RetrievalDispatch, RetrievalError>> + Send + 'a>>;
/// Whether the exact rejected login was renewed.
pub type CredentialRefresh<'a> = Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

/// Exact-route authorization; no fallback or cross-route credential lookup.
pub trait RetrievalTransport: Send + Sync {
    /// Revalidates before every physical request, including retries.
    fn authorize<'a>(&'a self, body: &'a [u8]) -> RetrievalAuthorization<'a>;
    /// Optional single claimed refresh after an HTTP 401 refusal.
    fn credential_rejected(&self) -> CredentialRefresh<'_> {
        Box::pin(std::future::ready(false))
    }

    /// Opts this route into retrying one proven overload that arrives wearing
    /// another status. The family's reader decides which body that is and
    /// which status it is retried as. Default: a 401 stays a credential
    /// rejection.
    fn retry_wrapped_overload(&self) -> bool {
        false
    }

    /// The route's operator remedy for a proven contract refusal (a rejected
    /// client version or sent beta), replacing the provider's text. Such a
    /// refusal is final: no retry and no claimed refresh. A 401 or 403 is
    /// never offered here; it keeps the credential-rejection path. Default:
    /// none.
    fn contract_refusal(&self, status: u16, body: &[u8]) -> Option<String> {
        let _ = (status, body);
        None
    }
}

/// Bounded transport and backoff policy for one side request.
#[derive(Clone, Copy, Debug)]
pub struct RetrievalLimits {
    /// Bound through the complete response body.
    pub exchange_timeout: Duration,
    /// Deterministic test sample; otherwise sample each wait.
    pub jitter: Option<Jitter>,
}
impl Default for RetrievalLimits {
    fn default() -> Self {
        Self {
            exchange_timeout: Duration::from_secs(120),
            jitter: None,
        }
    }
}

/// Distinct replay semantics of the two real side-request operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SideRequestPolicy {
    /// Isolated observational retrieval may retry a broken response.
    Retrieval,
    /// Image creation retries only proven-unsent requests and HTTP 429.
    Image,
}

/// Collector failure with explicit retry authority.
pub enum Broken {
    /// A bounded retrieval retry may start a new physical request.
    Transient(RetrievalError),
    /// End this logical request.
    Fatal(RetrievalError),
}
impl From<RetrievalError> for Broken {
    fn from(error: RetrievalError) -> Self {
        Self::Fatal(error)
    }
}

/// Family-owned result collector, not a transport or retry loop.
pub trait BodyCollector: Send {
    /// Completed family result.
    type Output: Send;
    /// Consume one body fragment under the family's bounds.
    fn push(&mut self, bytes: &[u8]) -> Result<(), Broken>;
    /// Validate the terminal grammar at clean EOF.
    fn finish(self) -> Result<Self::Output, Broken>;
    /// Family document readers may retain a partial body for their decoder.
    fn transport_failure(self, failure: Failure) -> Result<Self::Output, Broken>
    where
        Self: Sized,
    {
        Err(Broken::Transient(unavailable(failure)))
    }
}

/// Chooses a family collector and preserves its status-body interpretation.
pub trait ResponseReader: Sync {
    /// One fresh collector for each physical response.
    type Collector: BodyCollector;
    /// Chooses the response grammar, before consuming the body.
    fn start(&self, response: &Response) -> Self::Collector;
    /// Preserves the family's existing safe refusal text.
    fn status_error(&self, status: u16, body: &[u8]) -> RetrievalError;
    /// Status this refusal is retried as, when that is not its wire status.
    ///
    /// `Some` is a turn-policy retry even for an image request, honoring
    /// `Retry-After`, and is never a credential rejection. `None` keeps the
    /// wire status.
    fn retried_as(&self, status: u16, body: &[u8]) -> Option<u16> {
        let _ = (status, body);
        None
    }
    /// Replay semantics; images deliberately differ from retrieval.
    fn policy(&self) -> SideRequestPolicy {
        SideRequestPolicy::Retrieval
    }
}

/// A bounded JSON/raw document; parsing remains with the family.
pub struct Document {
    bytes: Vec<u8>,
    limit: usize,
    overflow: &'static str,
}
impl Document {
    /// Starts a document under its existing family byte bound and diagnostic.
    pub fn new(limit: usize, overflow: &'static str) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            overflow,
        }
    }
}
impl BodyCollector for Document {
    type Output = Vec<u8>;
    fn push(&mut self, bytes: &[u8]) -> Result<(), Broken> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            return Err(Broken::Fatal(RetrievalError::invalid_response(
                self.overflow,
            )));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn finish(self) -> Result<Vec<u8>, Broken> {
        Ok(self.bytes)
    }
}

enum Attempted<T> {
    Done(T),
    Retry {
        point: FailurePoint,
        retry_after: Option<Duration>,
        error: RetrievalError,
    },
    Rejected(RetrievalError),
}

/// Runs one logical side request under the existing bounded retry policy.
pub async fn exchange_with_retry<R: ResponseReader>(
    client: &Client,
    transport: &dyn RetrievalTransport,
    body: &[u8],
    reader: R,
    cancellation: &CancellationToken,
    limits: RetrievalLimits,
) -> Result<<R::Collector as BodyCollector>::Output, RetrievalError> {
    let mut physical = 0_u32;
    let mut waited = Duration::ZERO;
    let mut refreshed = false;
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        physical = physical.saturating_add(1);
        let (point, retry_after, error) =
            match exchange_once(client, transport, body, &reader, cancellation, limits).await {
                Ok(Attempted::Done(value)) => return Ok(value),
                Ok(Attempted::Rejected(error)) => {
                    if !refreshed {
                        refreshed = true;
                        if transport.credential_rejected().await {
                            continue;
                        }
                    }
                    return Err(error.with_physical_requests(physical));
                }
                Ok(Attempted::Retry {
                    point,
                    retry_after,
                    error,
                }) => (point, retry_after, error),
                Err(error) => return Err(error.with_physical_requests(physical)),
            };
        let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
            physical_attempts: physical,
            provider_acceptance: ProviderAcceptance::NotAccepted,
            visible_output: false,
            replay_safety: ReplaySafety::CostOnly,
            hosted_effects_possible: false,
            logical_success: false,
        });
        let jitter = limits.jitter.unwrap_or_else(Jitter::random);
        let Some(delay) = RetryPolicy::TURN.next_delay(facts, point, retry_after, waited, jitter)
        else {
            return Err(error.with_physical_requests(physical));
        };
        waited = waited.saturating_add(delay);
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancellation.clone().cancelled() => return Err(cancelled()),
        }
    }
}

async fn exchange_once<R: ResponseReader>(
    client: &Client,
    transport: &dyn RetrievalTransport,
    body: &[u8],
    reader: &R,
    cancellation: &CancellationToken,
    limits: RetrievalLimits,
) -> Result<Attempted<<R::Collector as BodyCollector>::Output>, RetrievalError> {
    let RetrievalDispatch { request, mut guard } = transport.authorize(body).await?;
    let http_cancellation = HttpCancellationToken::new();
    let started = Instant::now();
    let deadline = started
        .checked_add(limits.exchange_timeout)
        .unwrap_or(started);
    let exchanged = cancellable(
        client.execute(
            request,
            ExchangeControl::streaming(deadline.into_std(), http_cancellation.clone())
                .with_body_deadline(Some(deadline.into_std())),
        ),
        cancellation,
        &http_cancellation,
    )
    .await;
    if exchanged.cancelled {
        return Err(cancelled());
    }
    let response = match exchanged.value {
        Ok(response) => response,
        Err(failure) => {
            if let Some(guard) = guard.as_mut() {
                guard.transport_failed(failure);
            }
            let point = transport_point(failure);
            let error = unavailable(failure);
            if reader.policy() == SideRequestPolicy::Image
                && matches!(point, FailurePoint::AfterSend | FailurePoint::MidStream)
            {
                return Err(error);
            }
            return Ok(Attempted::Retry {
                point,
                retry_after: None,
                error,
            });
        }
    };
    drop(guard);
    let status = response.status().as_u16();
    if status != 200 {
        let retry_after = response
            .headers()
            .get(&plakous_http::HeaderName::from_static("retry-after"))
            .and_then(|value| value.to_str().ok())
            .and_then(plakous_attempt::retry_after_seconds);
        let body = read_bounded_body(
            response.into_body(),
            cancellation,
            &http_cancellation,
            64 * 1024,
        )
        .await
        .unwrap_or_default();
        if reader.policy() == SideRequestPolicy::Image && cancellation.is_cancelled() {
            return Err(cancelled());
        }
        if let Some(error) = claimed_contract_refusal(transport, status, &body) {
            return Err(error);
        }
        let error = reader.status_error(status, &body);
        // A proven overload wearing another status is retried as the status
        // the reader names, including an image request that retries no other
        // server error. It is not a credential rejection.
        if let Some(retried) = reader.retried_as(status, &body)
            && let Some(point) = RetryPolicy::status_failure_point(retried)
        {
            return Ok(Attempted::Retry {
                point,
                retry_after,
                error,
            });
        }
        if (reader.policy() == SideRequestPolicy::Retrieval || status == 429)
            && let Some(point) = RetryPolicy::status_failure_point(status)
        {
            return Ok(Attempted::Retry {
                point,
                retry_after,
                error,
            });
        }
        let error = error.with_status(status);
        return if status == 401 {
            Ok(Attempted::Rejected(error))
        } else {
            Err(error)
        };
    }
    let collector = reader.start(&response);
    read_stream(
        response.into_body(),
        collector,
        reader.policy(),
        cancellation,
        &http_cancellation,
    )
    .await
}

/// A route-claimed contract refusal; credential refusals are never offered.
fn claimed_contract_refusal(
    transport: &dyn RetrievalTransport,
    status: u16,
    body: &[u8],
) -> Option<RetrievalError> {
    if matches!(status, 401 | 403) {
        return None;
    }
    let remedy = transport.contract_refusal(status, body)?;
    Some(RetrievalError::new(RetrievalErrorKind::Rejected, remedy).with_status(status))
}

async fn read_stream<C: BodyCollector>(
    mut body: plakous_http::ResponseBody,
    mut collector: C,
    policy: SideRequestPolicy,
    cancellation: &CancellationToken,
    http_cancellation: &HttpCancellationToken,
) -> Result<Attempted<C::Output>, RetrievalError> {
    loop {
        let next = cancellable(body.next_chunk(), cancellation, http_cancellation).await;
        if next.cancelled {
            let _ = body.close().await;
            return Err(cancelled());
        }
        let failure = match next.value {
            Some(Ok(chunk)) => collector.push(&chunk).err(),
            Some(Err(failure)) => {
                let _ = body.close().await;
                return match collector.transport_failure(failure) {
                    Ok(value) => Ok(Attempted::Done(value)),
                    Err(failure) => collect_failure(failure, policy),
                };
            }
            None => break,
        };
        if let Some(failure) = failure {
            let _ = body.close().await;
            return collect_failure(failure, policy);
        }
    }
    match collector.finish() {
        Ok(value) => Ok(Attempted::Done(value)),
        Err(failure) => collect_failure(failure, policy),
    }
}

fn collect_failure<T>(
    failure: Broken,
    policy: SideRequestPolicy,
) -> Result<Attempted<T>, RetrievalError> {
    match failure {
        Broken::Transient(error) if policy == SideRequestPolicy::Retrieval => {
            Ok(Attempted::Retry {
                point: FailurePoint::MidStream,
                retry_after: None,
                error,
            })
        }
        Broken::Transient(error) | Broken::Fatal(error) => Err(error),
    }
}

/// Caller cancellation, without invented physical accounting.
fn cancelled() -> RetrievalError {
    RetrievalError::new(
        RetrievalErrorKind::Cancelled,
        "the side request was cancelled",
    )
}

/// Conservative replay point from transport proof, not from error prose.
fn transport_point(failure: Failure) -> FailurePoint {
    match failure.transmission() {
        TransmissionFact::NotStarted
            if matches!(
                failure.phase(),
                Phase::Resolution | Phase::Connect | Phase::Tls
            ) =>
        {
            FailurePoint::Unreachable
        }
        TransmissionFact::NotStarted | TransmissionFact::RequestIncomplete => FailurePoint::Refused,
        TransmissionFact::MayHaveTransmitted => FailurePoint::AfterSend,
    }
}

/// Existing safe transport diagnostic shared by the side-request families.
pub fn unavailable(failure: Failure) -> RetrievalError {
    let phase = match failure.phase() {
        Phase::Resolution => "DNS resolution",
        Phase::Connect => "connect",
        Phase::Tls => "TLS",
        Phase::RequestWrite | Phase::RequestWriteOrResponseHead => "request write",
        Phase::ResponseHead => "response head",
        _ => "response stream",
    };
    RetrievalError::new(
        RetrievalErrorKind::Unavailable,
        format!("the provider was unreachable ({phase} failed)"),
    )
}

/// Bounds provider error prose at a character boundary, preserving old text.
pub fn bounded_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.len() <= max {
        return line;
    }
    let mut end = max;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &line[..end])
}
