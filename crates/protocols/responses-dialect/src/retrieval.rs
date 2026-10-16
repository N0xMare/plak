//! Isolated Responses side requests for hosted retrieval.
//!
//! One wrapper tool call is one logical side request: a fresh one-tool
//! Responses body on the session's own route with its own bounded retry. The
//! side request has its own grammar: its terminal response may carry only
//! assistant messages, reasoning items and the call items of the one hosted
//! tool its body offered. It never produces a model event, a transcript item
//! or a canonical history entry. Main-exchange streams keep refusing every
//! hosted item.

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use plakous_attempt::{
    FailurePoint, Jitter, ObservationSnapshot, ProviderAcceptance, ReplaySafety, RetryPolicy,
    TerminalFacts,
};
use plakous_exchange::{cancellable, read_bounded_body};
use plakous_http::{
    CancellationToken as HttpCancellationToken, Client, ExchangeControl, Failure as HttpFailure,
    Phase, Request as HttpRequest, SseDecoder, SseLimits, TransmissionFact,
};
use plakous_model::{CancellationToken, ReasoningEffort};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::time::Instant;

/// The hosted web search tool type a side request names.
pub const RESPONSES_WEB_SEARCH_TOOL_TYPE: &str = "web_search";
/// The hosted post search tool type an xAI side request names.
pub const RESPONSES_X_SEARCH_TOOL_TYPE: &str = "x_search";
/// The include that asks for every consulted source of a web search call.
pub const RESPONSES_WEB_SEARCH_SOURCES_INCLUDE: &str = "web_search_call.action.sources";
/// Output cap of one side request.
pub const RESPONSES_RETRIEVAL_MAX_OUTPUT_TOKENS: u32 = 4096;

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_OUTPUT_ITEMS: usize = 256;
const SSE_LIMITS: SseLimits = SseLimits {
    max_line_bytes: 4 * 1024 * 1024,
    max_event_bytes: 8 * 1024 * 1024,
};

/// The route family whose hosted tool shapes a side request uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesRetrievalFlavor {
    /// OpenAI Platform: `web_search` with `filters.allowed_domains`.
    OpenAi,
    /// xAI API and SuperGrok: `web_search` with top-level domain lists, and
    /// `x_search`.
    Xai,
}

/// What one side request retrieves.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResponsesRetrievalQuery {
    /// Hosted web search.
    WebSearch {
        /// The query.
        query: String,
        /// Only results from these domains.
        allowed_domains: Vec<String>,
        /// No results from these domains.
        blocked_domains: Vec<String>,
    },
    /// Hosted search of X posts (xAI only).
    PostSearch {
        /// The query.
        query: String,
        /// Only posts by these handles.
        handles: Vec<String>,
        /// Earliest post date, `YYYY-MM-DD`.
        from_date: Option<String>,
        /// Latest post date, `YYYY-MM-DD`.
        to_date: Option<String>,
    },
}

impl ResponsesRetrievalQuery {
    /// The one user message.
    #[must_use]
    pub fn prompt(&self) -> String {
        match self {
            Self::WebSearch { query, .. } => {
                format!("Search the web for: {query}. Answer concisely and cite sources.")
            }
            Self::PostSearch { query, .. } => {
                format!("Search X posts for: {query}. Answer concisely and cite sources.")
            }
        }
    }

    /// The output item type the provider reports for this query's tool.
    const fn call_item(&self) -> &'static str {
        match self {
            Self::WebSearch { .. } => "web_search_call",
            Self::PostSearch { .. } => "x_search_call",
        }
    }

    fn tool(&self, flavor: ResponsesRetrievalFlavor) -> Result<Value, ResponsesRetrievalError> {
        match (self, flavor) {
            (
                Self::WebSearch {
                    allowed_domains,
                    blocked_domains,
                    ..
                },
                ResponsesRetrievalFlavor::OpenAi,
            ) => {
                if !blocked_domains.is_empty() {
                    return Err(ResponsesRetrievalError::invalid_request(
                        "this route's web search cannot exclude domains",
                    ));
                }
                let mut tool = json!({
                    "type": RESPONSES_WEB_SEARCH_TOOL_TYPE,
                    "search_context_size": "medium",
                });
                if !allowed_domains.is_empty() {
                    tool["filters"] = json!({"allowed_domains": allowed_domains});
                }
                Ok(tool)
            }
            (
                Self::WebSearch {
                    allowed_domains,
                    blocked_domains,
                    ..
                },
                ResponsesRetrievalFlavor::Xai,
            ) => {
                if !allowed_domains.is_empty() && !blocked_domains.is_empty() {
                    return Err(ResponsesRetrievalError::invalid_request(
                        "this route's web search takes allowed or excluded domains, not both",
                    ));
                }
                let mut tool = json!({"type": RESPONSES_WEB_SEARCH_TOOL_TYPE});
                if !allowed_domains.is_empty() {
                    tool["allowed_domains"] = json!(allowed_domains);
                }
                if !blocked_domains.is_empty() {
                    tool["excluded_domains"] = json!(blocked_domains);
                }
                Ok(tool)
            }
            (
                Self::PostSearch {
                    handles,
                    from_date,
                    to_date,
                    ..
                },
                ResponsesRetrievalFlavor::Xai,
            ) => {
                let mut tool = json!({"type": RESPONSES_X_SEARCH_TOOL_TYPE});
                if !handles.is_empty() {
                    tool["allowed_x_handles"] = json!(handles);
                }
                if let Some(from_date) = from_date {
                    tool["from_date"] = json!(from_date);
                }
                if let Some(to_date) = to_date {
                    tool["to_date"] = json!(to_date);
                }
                Ok(tool)
            }
            (Self::PostSearch { .. }, ResponsesRetrievalFlavor::OpenAi) => Err(
                ResponsesRetrievalError::invalid_request("this route does not search posts"),
            ),
        }
    }
}

/// The lowest effort a model's ladder admits, by its Responses name.
#[must_use]
pub fn lowest_responses_effort(efforts: &[ReasoningEffort]) -> Option<&'static str> {
    [
        (ReasoningEffort::None, "none"),
        (ReasoningEffort::Low, "low"),
        (ReasoningEffort::Medium, "medium"),
        (ReasoningEffort::High, "high"),
        (ReasoningEffort::XHigh, "xhigh"),
        (ReasoningEffort::Max, "max"),
        (ReasoningEffort::Ultra, "ultra"),
    ]
    .into_iter()
    .find(|(effort, _)| efforts.contains(effort))
    .map(|(_, name)| name)
}

/// One side request's exact inputs: the session's model, the route family,
/// the effort and the query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesRetrievalRequest {
    model: String,
    flavor: ResponsesRetrievalFlavor,
    effort: Option<&'static str>,
    query: ResponsesRetrievalQuery,
}

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    input: [Value; 1],
    tools: [Value; 1],
    tool_choice: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<[&'static str; 1]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Value>,
    max_output_tokens: u32,
    store: bool,
    stream: bool,
}

impl ResponsesRetrievalRequest {
    /// A side request on `model` for the route family `flavor`.
    #[must_use]
    pub fn new(
        model: impl Into<String>,
        flavor: ResponsesRetrievalFlavor,
        query: ResponsesRetrievalQuery,
    ) -> Self {
        Self {
            model: model.into(),
            flavor,
            effort: None,
            query,
        }
    }

    /// Requests `effort`; pass the lowest effort the model's ladder admits.
    #[must_use]
    pub const fn with_effort(mut self, effort: Option<&'static str>) -> Self {
        self.effort = effort;
        self
    }

    /// The query.
    #[must_use]
    pub const fn query(&self) -> &ResponsesRetrievalQuery {
        &self.query
    }

    /// The exact body.
    ///
    /// # Errors
    ///
    /// Refuses a query the route family cannot express.
    pub fn body(&self) -> Result<Vec<u8>, ResponsesRetrievalError> {
        let tool = self.query.tool(self.flavor)?;
        serde_json::to_vec(&Body {
            model: &self.model,
            input: [json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": self.query.prompt()}],
            })],
            tools: [tool],
            tool_choice: "required",
            include: matches!(self.query, ResponsesRetrievalQuery::WebSearch { .. })
                .then_some([RESPONSES_WEB_SEARCH_SOURCES_INCLUDE]),
            reasoning: self.effort.map(|effort| json!({"effort": effort})),
            max_output_tokens: RESPONSES_RETRIEVAL_MAX_OUTPUT_TOKENS,
            store: false,
            stream: true,
        })
        .map_err(|_| ResponsesRetrievalError::invalid_request("side request could not be encoded"))
    }
}

/// Classifies a side-request body by the typed per-tool allowlist: exactly
/// one `web_search` or `x_search` tool with only its known fields, a
/// required tool choice, `store: false`, and only known top-level fields is
/// [`ReplaySafety::CostOnly`]. Anything else, including a tool type nobody
/// has classified, is [`ReplaySafety::ServerEffect`].
#[must_use]
pub fn classify_responses_retrieval_body(body: &[u8]) -> ReplaySafety {
    const FIELDS: [&str; 9] = [
        "model",
        "input",
        "tools",
        "tool_choice",
        "include",
        "reasoning",
        "max_output_tokens",
        "store",
        "stream",
    ];
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(body) else {
        return ReplaySafety::ServerEffect;
    };
    if body.keys().any(|key| !FIELDS.contains(&key.as_str()))
        || body.get("store") != Some(&Value::Bool(false))
        || body.get("tool_choice") != Some(&json!("required"))
    {
        return ReplaySafety::ServerEffect;
    }
    if body
        .get("include")
        .is_some_and(|include| include != &json!([RESPONSES_WEB_SEARCH_SOURCES_INCLUDE]))
    {
        return ReplaySafety::ServerEffect;
    }
    let Some([Value::Object(tool)]) = body
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    else {
        return ReplaySafety::ServerEffect;
    };
    let allowed: &[&str] = match tool.get("type").and_then(Value::as_str) {
        Some(RESPONSES_WEB_SEARCH_TOOL_TYPE) => &[
            "type",
            "search_context_size",
            "filters",
            "allowed_domains",
            "excluded_domains",
        ],
        Some(RESPONSES_X_SEARCH_TOOL_TYPE) => {
            &["type", "allowed_x_handles", "from_date", "to_date"]
        }
        _ => return ReplaySafety::ServerEffect,
    };
    if tool.keys().any(|key| !allowed.contains(&key.as_str())) {
        return ReplaySafety::ServerEffect;
    }
    if let Some(filters) = tool.get("filters")
        && !filters
            .as_object()
            .is_some_and(|filters| filters.keys().all(|key| key == "allowed_domains"))
    {
        return ReplaySafety::ServerEffect;
    }
    ReplaySafety::CostOnly
}

/// One source the provider reported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesRetrievalSource {
    /// The URL.
    pub url: String,
    /// The title, when reported.
    pub title: Option<String>,
}

/// What a side request consumed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResponsesRetrievalUsage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Hosted tool calls the response reported.
    pub hosted_requests: u64,
}

/// The completed side request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResponsesRetrievalOutcome {
    /// The answer: the assistant message text.
    pub text: String,
    /// Sources from tool call actions and URL citations, first-reported
    /// order, URLs unique.
    pub sources: Vec<ResponsesRetrievalSource>,
    /// Usage of the side request.
    pub usage: ResponsesRetrievalUsage,
}

/// Why a side request failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponsesRetrievalErrorKind {
    /// The body is not an allowlisted side request or could not be built.
    InvalidRequest,
    /// Admission or the credential is no longer current, or HTTP 401/403.
    Unauthorized,
    /// The provider refused the request (a non-retryable status or failure).
    Rejected,
    /// The provider stayed unreachable or failing through the retry budget.
    Unavailable,
    /// The response broke the side-request grammar or its bounds.
    InvalidResponse,
    /// The caller cancelled.
    Cancelled,
}

/// A failed side request with a redaction-safe message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesRetrievalError {
    kind: ResponsesRetrievalErrorKind,
    message: String,
    status: Option<u16>,
    physical_requests: u32,
}

impl ResponsesRetrievalError {
    /// A failure of `kind`.
    #[must_use]
    pub fn new(kind: ResponsesRetrievalErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            physical_requests: 0,
        }
    }

    fn invalid_request(message: &str) -> Self {
        Self::new(ResponsesRetrievalErrorKind::InvalidRequest, message)
    }

    fn invalid_response(message: impl Into<String>) -> Self {
        Self::new(ResponsesRetrievalErrorKind::InvalidResponse, message)
    }

    /// The failure class.
    #[must_use]
    pub const fn kind(&self) -> ResponsesRetrievalErrorKind {
        self.kind
    }

    /// The HTTP status of the final refusal, when there was one.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    /// Physical requests the side request sent, retries included.
    #[must_use]
    pub const fn physical_requests(&self) -> u32 {
        self.physical_requests
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ResponsesRetrievalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ResponsesRetrievalError {}

/// What a route retains from authorization through the response head of one
/// physical side request, such as a credential lease and an inference permit.
pub trait ResponsesRetrievalGuard: Send {
    /// The physical request failed before its response head.
    fn transport_failed(&mut self, failure: HttpFailure);
}

/// One authorized physical side request and what the route retains until
/// its response head.
pub struct ResponsesRetrievalDispatch {
    request: HttpRequest,
    guard: Option<Box<dyn ResponsesRetrievalGuard>>,
}

impl ResponsesRetrievalDispatch {
    /// A request that retains nothing.
    #[must_use]
    pub fn new(request: HttpRequest) -> Self {
        Self {
            request,
            guard: None,
        }
    }

    /// Retains `guard` until the response head, or reports a transport
    /// failure before it to the guard.
    #[must_use]
    pub fn with_guard(mut self, guard: impl ResponsesRetrievalGuard + 'static) -> Self {
        self.guard = Some(Box::new(guard));
        self
    }
}

impl From<HttpRequest> for ResponsesRetrievalDispatch {
    fn from(request: HttpRequest) -> Self {
        Self::new(request)
    }
}

impl fmt::Debug for ResponsesRetrievalDispatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesRetrievalDispatch")
            .field("guarded", &self.guard.is_some())
            .finish_non_exhaustive()
    }
}

/// The future [`ResponsesRetrievalTransport::authorize`] returns.
pub type ResponsesRetrievalAuthorization<'a> = Pin<
    Box<
        dyn Future<Output = Result<ResponsesRetrievalDispatch, ResponsesRetrievalError>>
            + Send
            + 'a,
    >,
>;

/// The route's half of a side request.
pub trait ResponsesRetrievalTransport: Send + Sync {
    /// Revalidates admission and the credential, then builds the exact
    /// authorized request carrying `body`. Called before every physical
    /// request, retries included.
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ResponsesRetrievalAuthorization<'a>;
}

/// Exchange bounds of one side request.
#[derive(Clone, Copy, Debug)]
pub struct ResponsesRetrievalLimits {
    /// Bound of one physical exchange up to its response head and of its
    /// streamed body.
    pub exchange_timeout: Duration,
    /// The backoff sample; `None` draws one per wait.
    pub jitter: Option<Jitter>,
}

impl Default for ResponsesRetrievalLimits {
    fn default() -> Self {
        Self {
            exchange_timeout: Duration::from_secs(120),
            jitter: None,
        }
    }
}

/// Runs one side request to its answer under the bounded retry of
/// [`RetryPolicy::TURN`].
///
/// # Errors
///
/// Refuses a body outside the allowlist before dispatch; otherwise returns
/// the final failure after retries.
pub async fn run_responses_retrieval(
    client: &Client,
    transport: &dyn ResponsesRetrievalTransport,
    request: &ResponsesRetrievalRequest,
    cancellation: &CancellationToken,
    limits: ResponsesRetrievalLimits,
) -> Result<ResponsesRetrievalOutcome, ResponsesRetrievalError> {
    let body = request.body()?;
    if classify_responses_retrieval_body(&body) != ReplaySafety::CostOnly {
        return Err(ResponsesRetrievalError::invalid_request(
            "side request body is outside the hosted retrieval allowlist",
        ));
    }
    let Received::Terminal(terminal) =
        exchange_with_retry(client, transport, &body, Reply::Sse, cancellation, limits).await?
    else {
        return Err(ResponsesRetrievalError::invalid_response(
            "side request reply was not a stream",
        ));
    };
    assemble(&terminal, request.query.call_item())
}

/// Runs one side request whose reply is a single JSON document, under the
/// same bounded retry as [`run_responses_retrieval`], and returns the
/// bounded success body for the route to decode.
///
/// # Errors
///
/// Refuses a body `classify` does not call [`ReplaySafety::CostOnly`] before
/// dispatch; otherwise returns the final failure after retries.
pub async fn run_responses_json_side_request(
    client: &Client,
    transport: &dyn ResponsesRetrievalTransport,
    body: &[u8],
    classify: fn(&[u8]) -> ReplaySafety,
    cancellation: &CancellationToken,
    limits: ResponsesRetrievalLimits,
) -> Result<Vec<u8>, ResponsesRetrievalError> {
    if classify(body) != ReplaySafety::CostOnly {
        return Err(ResponsesRetrievalError::invalid_request(
            "side request body is outside the hosted retrieval allowlist",
        ));
    }
    match exchange_with_retry(client, transport, body, Reply::Json, cancellation, limits).await? {
        Received::Document(document) => Ok(document),
        Received::Terminal(_) => Err(ResponsesRetrievalError::invalid_response(
            "side request reply was not a document",
        )),
    }
}

/// How a side request's success body is read.
#[derive(Clone, Copy)]
enum Reply {
    /// A Responses event stream up to its terminal response.
    Sse,
    /// One bounded JSON document.
    Json,
}

enum Received {
    Terminal(Value),
    Document(Vec<u8>),
}

enum Attempted {
    Done(Received),
    Retry {
        point: FailurePoint,
        retry_after: Option<Duration>,
        error: ResponsesRetrievalError,
    },
}

async fn exchange_with_retry(
    client: &Client,
    transport: &dyn ResponsesRetrievalTransport,
    body: &[u8],
    reply: Reply,
    cancellation: &CancellationToken,
    limits: ResponsesRetrievalLimits,
) -> Result<Received, ResponsesRetrievalError> {
    let mut physical = 0_u32;
    let mut waited = Duration::ZERO;
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        physical = physical.saturating_add(1);
        let (point, retry_after, mut error) =
            match exchange_once(client, transport, body, reply, cancellation, limits).await {
                Ok(Attempted::Done(terminal)) => return Ok(terminal),
                Ok(Attempted::Retry {
                    point,
                    retry_after,
                    error,
                }) => (point, retry_after, error),
                Err(mut error) => {
                    error.physical_requests = physical;
                    return Err(error);
                }
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
            error.physical_requests = physical;
            return Err(error);
        };
        waited = waited.saturating_add(delay);
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancellation.clone().cancelled() => return Err(cancelled()),
        }
    }
}

fn cancelled() -> ResponsesRetrievalError {
    ResponsesRetrievalError::new(
        ResponsesRetrievalErrorKind::Cancelled,
        "the side request was cancelled",
    )
}

async fn exchange_once(
    client: &Client,
    transport: &dyn ResponsesRetrievalTransport,
    body: &[u8],
    reply: Reply,
    cancellation: &CancellationToken,
    limits: ResponsesRetrievalLimits,
) -> Result<Attempted, ResponsesRetrievalError> {
    let ResponsesRetrievalDispatch { request, mut guard } = transport.authorize(body).await?;
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
            return Ok(Attempted::Retry {
                point: transport_point(failure),
                retry_after: None,
                error: unavailable(failure),
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
            MAX_ERROR_BODY_BYTES,
        )
        .await
        .unwrap_or_default();
        let mut error = status_error(status, &body);
        if let Some(point) = RetryPolicy::status_failure_point(status) {
            return Ok(Attempted::Retry {
                point,
                retry_after,
                error,
            });
        }
        error.status = Some(status);
        return Err(error);
    }
    let body = response.into_body();
    match reply {
        Reply::Sse => read_stream(body, cancellation, &http_cancellation).await,
        Reply::Json => read_document(body, cancellation, &http_cancellation).await,
    }
}

async fn read_document(
    mut body: plakous_http::ResponseBody,
    cancellation: &CancellationToken,
    http_cancellation: &HttpCancellationToken,
) -> Result<Attempted, ResponsesRetrievalError> {
    let mut document = Vec::new();
    loop {
        let next = cancellable(body.next_chunk(), cancellation, http_cancellation).await;
        if next.cancelled {
            let _ = body.close().await;
            return Err(cancelled());
        }
        match next.value {
            Some(Ok(chunk)) => {
                if document.len().saturating_add(chunk.len()) > MAX_DOCUMENT_BYTES {
                    let _ = body.close().await;
                    return Err(ResponsesRetrievalError::invalid_response(
                        "side request reply exceeded its bound",
                    ));
                }
                document.extend_from_slice(&chunk);
            }
            Some(Err(failure)) => {
                return Ok(Attempted::Retry {
                    point: FailurePoint::MidStream,
                    retry_after: None,
                    error: unavailable(failure),
                });
            }
            None => return Ok(Attempted::Done(Received::Document(document))),
        }
    }
}

async fn read_stream(
    mut body: plakous_http::ResponseBody,
    cancellation: &CancellationToken,
    http_cancellation: &HttpCancellationToken,
) -> Result<Attempted, ResponsesRetrievalError> {
    let mut collector = Collector::default();
    loop {
        let next = cancellable(body.next_chunk(), cancellation, http_cancellation).await;
        if next.cancelled {
            let _ = body.close().await;
            return Err(cancelled());
        }
        match next.value {
            Some(Ok(chunk)) => match collector.push(&chunk) {
                Ok(()) => {}
                Err(Broken::Transient(error)) => {
                    let _ = body.close().await;
                    return Ok(Attempted::Retry {
                        point: FailurePoint::MidStream,
                        retry_after: None,
                        error,
                    });
                }
                Err(Broken::Fatal(error)) => {
                    let _ = body.close().await;
                    return Err(error);
                }
            },
            Some(Err(failure)) => {
                return Ok(Attempted::Retry {
                    point: FailurePoint::MidStream,
                    retry_after: None,
                    error: unavailable(failure),
                });
            }
            None => break,
        }
    }
    match collector.finish() {
        Ok(terminal) => Ok(Attempted::Done(Received::Terminal(terminal))),
        Err(Broken::Transient(error)) => Ok(Attempted::Retry {
            point: FailurePoint::MidStream,
            retry_after: None,
            error,
        }),
        Err(Broken::Fatal(error)) => Err(error),
    }
}

fn transport_point(failure: HttpFailure) -> FailurePoint {
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

fn unavailable(failure: HttpFailure) -> ResponsesRetrievalError {
    let phase = match failure.phase() {
        Phase::Resolution => "DNS resolution",
        Phase::Connect => "connect",
        Phase::Tls => "TLS",
        Phase::RequestWrite | Phase::RequestWriteOrResponseHead => "request write",
        Phase::ResponseHead => "response head",
        _ => "response stream",
    };
    ResponsesRetrievalError::new(
        ResponsesRetrievalErrorKind::Unavailable,
        format!("the provider was unreachable ({phase} failed)"),
    )
}

/// One bounded line from a provider error body, never the raw body: the
/// OpenAI shape `error.{type|code}: error.message` or the xAI shape
/// `code: error`.
fn status_error(status: u16, body: &[u8]) -> ResponsesRetrievalError {
    let detail = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| match value.get("error")? {
            Value::Object(error) => {
                let kind = error
                    .get("type")
                    .or_else(|| error.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                let message = error.get("message").and_then(Value::as_str).unwrap_or("");
                Some(format!("{kind}: {}", bounded_line(message, 300)))
            }
            Value::String(message) => {
                let kind = value.get("code").and_then(Value::as_str).unwrap_or("error");
                Some(format!("{kind}: {}", bounded_line(message, 300)))
            }
            _ => None,
        });
    let kind = match status {
        401 | 403 => ResponsesRetrievalErrorKind::Unauthorized,
        429 | 500..=599 => ResponsesRetrievalErrorKind::Unavailable,
        _ => ResponsesRetrievalErrorKind::Rejected,
    };
    let message = match detail {
        Some(detail) => format!("the provider refused the side request (HTTP {status}, {detail})"),
        None => format!("the provider refused the side request (HTTP {status})"),
    };
    ResponsesRetrievalError::new(kind, message)
}

fn bounded_line(text: &str, max: usize) -> String {
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

enum Broken {
    /// The provider reported a transient failure or the stream was cut.
    Transient(ResponsesRetrievalError),
    Fatal(ResponsesRetrievalError),
}

impl From<ResponsesRetrievalError> for Broken {
    fn from(error: ResponsesRetrievalError) -> Self {
        Self::Fatal(error)
    }
}

/// The side-request grammar over one exchange's SSE events: progress events
/// are ignored, and the terminal response object is kept for [`assemble`].
#[derive(Default)]
struct Collector {
    sse: Option<SseDecoder>,
    terminal: Option<Value>,
}

impl Collector {
    fn push(&mut self, bytes: &[u8]) -> Result<(), Broken> {
        let messages = self
            .sse
            .get_or_insert_with(|| SseDecoder::new(SSE_LIMITS))
            .push(bytes)
            .map_err(|_| {
                ResponsesRetrievalError::invalid_response("side request SSE was malformed")
            })?;
        for message in messages {
            self.event(message.event(), message.data())?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Value, Broken> {
        if let Some(sse) = self.sse.as_mut() {
            let messages = sse.finish().map_err(|_| {
                ResponsesRetrievalError::invalid_response("side request SSE was truncated")
            })?;
            for message in messages {
                self.event(message.event(), message.data())?;
            }
        }
        self.terminal.ok_or_else(|| {
            Broken::Transient(ResponsesRetrievalError::new(
                ResponsesRetrievalErrorKind::Unavailable,
                "the side request stream ended before its terminal response",
            ))
        })
    }

    fn event(&mut self, name: Option<&str>, data: &str) -> Result<(), Broken> {
        if data == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(data).map_err(|_| {
            ResponsesRetrievalError::invalid_response("side request event was not JSON")
        })?;
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if name.is_some_and(|name| name != kind) {
            return Err(ResponsesRetrievalError::invalid_response(
                "side request event name disagreed with its type",
            )
            .into());
        }
        if self.terminal.is_some() {
            return Err(ResponsesRetrievalError::invalid_response(
                "side request event after its terminal response",
            )
            .into());
        }
        match kind {
            "response.completed" | "response.incomplete" => {
                let response = value.get("response").cloned().ok_or_else(|| {
                    ResponsesRetrievalError::invalid_response(
                        "side request terminal event carried no response",
                    )
                })?;
                if kind == "response.incomplete"
                    && response.pointer("/incomplete_details/reason")
                        != Some(&json!("max_output_tokens"))
                {
                    return Err(ResponsesRetrievalError::new(
                        ResponsesRetrievalErrorKind::Rejected,
                        "the provider left the side request incomplete",
                    )
                    .into());
                }
                self.terminal = Some(response);
                Ok(())
            }
            "response.failed" => Err(provider_failure(value.pointer("/response/error"))),
            "error" => Err(provider_failure(Some(value.get("error").unwrap_or(&value)))),
            _ if kind.starts_with("response.") => Ok(()),
            _ => Err(ResponsesRetrievalError::invalid_response(format!(
                "side request carried unexpected event {:?}",
                bounded_line(kind, 64)
            ))
            .into()),
        }
    }
}

/// A provider-reported failure: a server error or rate limit is transient,
/// anything else is a refusal.
fn provider_failure(error: Option<&Value>) -> Broken {
    let code = error
        .and_then(|error| error.get("code").or_else(|| error.get("type")))
        .and_then(Value::as_str)
        .unwrap_or("error");
    let code = bounded_line(code, 64);
    if matches!(
        code.as_str(),
        "server_error" | "rate_limit_exceeded" | "overloaded" | "internal_error"
    ) {
        return Broken::Transient(ResponsesRetrievalError::new(
            ResponsesRetrievalErrorKind::Unavailable,
            format!("the provider failed the side request mid-stream ({code})"),
        ));
    }
    Broken::Fatal(ResponsesRetrievalError::new(
        ResponsesRetrievalErrorKind::Rejected,
        format!("the provider failed the side request ({code})"),
    ))
}

fn u64_at(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

/// The answer and sources of the terminal response. Only assistant
/// messages, reasoning items and calls of the offered tool are admitted.
/// Appends one assistant message's text and adds its URL citations.
fn message(
    item: &Value,
    text: &mut String,
    add: &mut impl FnMut(Option<&str>, Option<&str>),
) -> Result<(), ResponsesRetrievalError> {
    if item.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(ResponsesRetrievalError::invalid_response(
            "side request message was not the assistant's",
        ));
    }
    for part in item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match part.get("type").and_then(Value::as_str) {
            Some("output_text") => {
                text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                for annotation in part
                    .get("annotations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|annotation| {
                        annotation.get("type").and_then(Value::as_str) == Some("url_citation")
                    })
                {
                    add(
                        annotation.get("url").and_then(Value::as_str),
                        annotation.get("title").and_then(Value::as_str),
                    );
                }
            }
            Some("refusal") => {
                return Err(ResponsesRetrievalError::new(
                    ResponsesRetrievalErrorKind::Rejected,
                    "the provider refused the side request",
                ));
            }
            _ => {
                return Err(ResponsesRetrievalError::invalid_response(
                    "side request message carried an unexpected part",
                ));
            }
        }
    }
    Ok(())
}

fn assemble(
    terminal: &Value,
    call_item: &str,
) -> Result<ResponsesRetrievalOutcome, ResponsesRetrievalError> {
    let items = terminal
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ResponsesRetrievalError::invalid_response("side request response carried no output")
        })?;
    if items.len() > MAX_OUTPUT_ITEMS {
        return Err(ResponsesRetrievalError::invalid_response(
            "side request output exceeds its item bound",
        ));
    }
    let mut sources = Vec::<ResponsesRetrievalSource>::new();
    let mut seen = BTreeSet::new();
    let mut add = |url: Option<&str>, title: Option<&str>| {
        if let Some(url) = url.filter(|url| !url.is_empty())
            && seen.insert(url.to_owned())
        {
            sources.push(ResponsesRetrievalSource {
                url: url.to_owned(),
                title: title.filter(|title| !title.is_empty()).map(str::to_owned),
            });
        }
    };
    let mut text = String::new();
    let mut calls = 0_u64;
    let mut failed_calls = 0_u64;
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {}
            Some("message") => message(item, &mut text, &mut add)?,
            Some(kind) if kind == call_item => {
                calls = calls.saturating_add(1);
                if item.get("status").and_then(Value::as_str) == Some("failed") {
                    failed_calls = failed_calls.saturating_add(1);
                }
                for source in item
                    .pointer("/action/sources")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    add(
                        source.get("url").and_then(Value::as_str),
                        source.get("title").and_then(Value::as_str),
                    );
                }
            }
            other => {
                return Err(ResponsesRetrievalError::invalid_response(format!(
                    "side request carried unexpected item {:?}",
                    other.map(|kind| bounded_line(kind, 64))
                )));
            }
        }
    }
    if text.trim().is_empty() && failed_calls > 0 {
        return Err(ResponsesRetrievalError::new(
            ResponsesRetrievalErrorKind::Rejected,
            format!("the provider's retrieval failed ({call_item} failed)"),
        ));
    }
    Ok(ResponsesRetrievalOutcome {
        text,
        sources,
        usage: ResponsesRetrievalUsage {
            input_tokens: u64_at(terminal, "/usage/input_tokens"),
            output_tokens: u64_at(terminal, "/usage/output_tokens"),
            hosted_requests: calls,
        },
    })
}

#[cfg(test)]
mod tests;
