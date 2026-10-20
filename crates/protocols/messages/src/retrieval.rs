//! Isolated Messages side requests for hosted retrieval.
//!
//! One wrapper tool call is one logical side request: a fresh one-tool
//! Messages body on the session's own route, its own bounded retry, and at
//! most [`MAX_MESSAGES_RETRIEVAL_CONTINUATIONS`] `pause_turn` continuations.
//! The side request has its own grammar: it accepts exactly the server-tool
//! blocks its body asked for, keeps the paused assistant content verbatim
//! only to send it back inside the same side request, and never produces a
//! model event, a transcript item or a canonical history entry. Main-exchange
//! streams keep refusing every hosted block.

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
use plakous_model::CancellationToken;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

/// The hosted web search tool type a side request names.
pub const MESSAGES_WEB_SEARCH_TOOL_TYPE: &str = "web_search_20260318";
/// The hosted web fetch tool type a side request names.
pub const MESSAGES_WEB_FETCH_TOOL_TYPE: &str = "web_fetch_20260318";
/// Output cap of one side request.
pub const MESSAGES_RETRIEVAL_MAX_TOKENS: u32 = 4096;
/// Content tokens one fetched page may contribute.
pub const MESSAGES_WEB_FETCH_MAX_CONTENT_TOKENS: u32 = 20_000;
/// `pause_turn` continuations one side request may send.
pub const MAX_MESSAGES_RETRIEVAL_CONTINUATIONS: usize = 3;

/// The most physical requests one side request can send: the first exchange
/// and its [`MAX_MESSAGES_RETRIEVAL_CONTINUATIONS`] `pause_turn`
/// continuations, each retried within [`RetryPolicy::TURN`]'s physical
/// budget. A connect failure counts, since its authorization was spent.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the continuation bound is a small constant"
)]
pub const MAX_MESSAGES_RETRIEVAL_PHYSICAL_REQUESTS: u32 =
    (MAX_MESSAGES_RETRIEVAL_CONTINUATIONS as u32 + 1) * RetryPolicy::TURN.max_physical();

const ALLOWED_CALLERS: [&str; 1] = ["direct"];
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_RETAINED_CONTENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_BLOCKS: usize = 512;
const SSE_LIMITS: SseLimits = SseLimits {
    max_line_bytes: 1024 * 1024,
    max_event_bytes: 2 * 1024 * 1024,
};

/// What one side request retrieves.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessagesRetrievalQuery {
    /// Hosted web search.
    WebSearch {
        /// The query.
        query: String,
        /// Only results from these domains.
        allowed_domains: Vec<String>,
        /// No results from these domains.
        blocked_domains: Vec<String>,
        /// Searches the side request may run.
        max_uses: u8,
    },
    /// Hosted fetch of one page; the URL travels in the user message,
    /// because the provider fetches only URLs present in the conversation.
    PageFetch {
        /// The absolute URL.
        url: String,
        /// What to extract from the page.
        prompt: Option<String>,
    },
}

impl MessagesRetrievalQuery {
    /// The provider's name for the one tool this side request offers.
    #[must_use]
    pub const fn tool_name(&self) -> &'static str {
        match self {
            Self::WebSearch { .. } => "web_search",
            Self::PageFetch { .. } => "web_fetch",
        }
    }

    /// The one user message.
    #[must_use]
    pub fn prompt(&self) -> String {
        match self {
            Self::WebSearch { query, .. } => {
                format!("Search the web for: {query}. Answer concisely and cite sources.")
            }
            Self::PageFetch {
                url,
                prompt: Some(prompt),
            } => format!("Fetch {url} and answer: {prompt}. Answer concisely and cite sources."),
            Self::PageFetch { url, prompt: None } => {
                format!("Fetch {url} and summarize it. Answer concisely and cite sources.")
            }
        }
    }

    fn tool(&self) -> Value {
        match self {
            Self::WebSearch {
                allowed_domains,
                blocked_domains,
                max_uses,
                ..
            } => {
                let mut tool = json!({
                    "type": MESSAGES_WEB_SEARCH_TOOL_TYPE,
                    "name": "web_search",
                    "allowed_callers": ALLOWED_CALLERS,
                    "max_uses": max_uses,
                });
                if !allowed_domains.is_empty() {
                    tool["allowed_domains"] = json!(allowed_domains);
                }
                if !blocked_domains.is_empty() {
                    tool["blocked_domains"] = json!(blocked_domains);
                }
                tool
            }
            Self::PageFetch { .. } => json!({
                "type": MESSAGES_WEB_FETCH_TOOL_TYPE,
                "name": "web_fetch",
                "allowed_callers": ALLOWED_CALLERS,
                "max_uses": 1,
                "citations": {"enabled": true},
                "max_content_tokens": MESSAGES_WEB_FETCH_MAX_CONTENT_TOKENS,
            }),
        }
    }
}

/// One side request's exact inputs: the session's model, the route's
/// costume system text, and the query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagesRetrievalRequest {
    model: String,
    system: Vec<String>,
    metadata: Option<Value>,
    effort: Option<&'static str>,
    query: MessagesRetrievalQuery,
}

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<SystemBlock<'a>>,
    messages: Vec<Value>,
    tools: [Value; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a Value>,
    stream: bool,
}

#[derive(Serialize)]
struct SystemBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

impl MessagesRetrievalRequest {
    /// A side request on `model` without route costume.
    #[must_use]
    pub fn new(model: impl Into<String>, query: MessagesRetrievalQuery) -> Self {
        Self {
            model: model.into(),
            system: Vec::new(),
            metadata: None,
            effort: None,
            query,
        }
    }

    /// Requests adaptive thinking at `effort`, for a model that admits no
    /// side request without thinking; pass the lowest effort its ladder
    /// admits.
    #[must_use]
    pub const fn with_adaptive_thinking(mut self, effort: &'static str) -> Self {
        self.effort = Some(effort);
        self
    }

    /// Prepends the route's required system text blocks, in order.
    #[must_use]
    pub fn with_system(mut self, system: Vec<String>) -> Self {
        self.system = system;
        self
    }

    /// Attaches the route's request metadata object.
    #[must_use]
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// The query.
    #[must_use]
    pub const fn query(&self) -> &MessagesRetrievalQuery {
        &self.query
    }

    /// The exact body: the user message, then the paused assistant content
    /// of earlier exchanges unchanged, when continuing.
    ///
    /// # Errors
    ///
    /// Fails only when serialization fails.
    pub fn body(&self, paused: &[Value]) -> Result<Vec<u8>, MessagesRetrievalError> {
        let mut messages = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": self.query.prompt()}],
        })];
        if !paused.is_empty() {
            messages.push(json!({"role": "assistant", "content": paused}));
        }
        serde_json::to_vec(&Body {
            model: &self.model,
            max_tokens: MESSAGES_RETRIEVAL_MAX_TOKENS,
            system: self
                .system
                .iter()
                .map(|text| SystemBlock { kind: "text", text })
                .collect(),
            messages,
            tools: [self.query.tool()],
            thinking: self.effort.map(|_| json!({"type": "adaptive"})),
            output_config: self.effort.map(|effort| json!({"effort": effort})),
            metadata: self.metadata.as_ref(),
            stream: true,
        })
        .map_err(|_| MessagesRetrievalError::invalid_request("side request could not be encoded"))
    }
}

/// Classifies a side-request body by the typed per-tool allowlist: exactly
/// one `web_search_20260318` or `web_fetch_20260318` tool callable only
/// directly, and only known top-level fields, is [`ReplaySafety::CostOnly`].
/// Anything else, including a tool type nobody has classified, is
/// [`ReplaySafety::ServerEffect`].
#[must_use]
pub fn classify_messages_retrieval_body(body: &[u8]) -> ReplaySafety {
    const FIELDS: [&str; 9] = [
        "model",
        "max_tokens",
        "system",
        "messages",
        "tools",
        "thinking",
        "output_config",
        "metadata",
        "stream",
    ];
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(body) else {
        return ReplaySafety::ServerEffect;
    };
    if body.keys().any(|key| !FIELDS.contains(&key.as_str())) {
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
        Some(MESSAGES_WEB_SEARCH_TOOL_TYPE) => &[
            "type",
            "name",
            "allowed_callers",
            "max_uses",
            "allowed_domains",
            "blocked_domains",
        ],
        Some(MESSAGES_WEB_FETCH_TOOL_TYPE) => &[
            "type",
            "name",
            "allowed_callers",
            "max_uses",
            "citations",
            "max_content_tokens",
        ],
        _ => return ReplaySafety::ServerEffect,
    };
    let direct_only = tool.get("allowed_callers") == Some(&json!(ALLOWED_CALLERS));
    if !direct_only || tool.keys().any(|key| !allowed.contains(&key.as_str())) {
        return ReplaySafety::ServerEffect;
    }
    ReplaySafety::CostOnly
}

/// One source the provider reported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagesRetrievalSource {
    /// The URL.
    pub url: String,
    /// The title, when reported.
    pub title: Option<String>,
}

/// What a side request consumed over all its exchanges.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MessagesRetrievalUsage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// `server_tool_use.web_search_requests` plus `web_fetch_requests`.
    pub hosted_requests: u64,
}

/// The completed side request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MessagesRetrievalOutcome {
    /// The answer: text after the last server-tool result.
    pub text: String,
    /// Sources from tool results and citations, first-reported order, URLs
    /// unique.
    pub sources: Vec<MessagesRetrievalSource>,
    /// Usage over every exchange.
    pub usage: MessagesRetrievalUsage,
    /// Exchanges sent: one plus the `pause_turn` continuations.
    pub exchanges: usize,
    /// Physical requests sent over every exchange, retries included.
    pub physical_requests: u32,
}

/// Why a side request failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MessagesRetrievalErrorKind {
    /// The body is not an allowlisted side request or could not be built.
    InvalidRequest,
    /// Admission or the credential is no longer current, or HTTP 401/403.
    Unauthorized,
    /// The provider refused the request (a non-retryable status or refusal).
    Rejected,
    /// The provider stayed unreachable or failing through the retry budget.
    Unavailable,
    /// The stream broke the side-request grammar or its bounds.
    InvalidResponse,
    /// The caller cancelled.
    Cancelled,
}

/// A failed side request with a redaction-safe message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagesRetrievalError {
    kind: MessagesRetrievalErrorKind,
    message: String,
    status: Option<u16>,
    physical_requests: u32,
}

impl MessagesRetrievalError {
    /// A failure of `kind`.
    #[must_use]
    pub fn new(kind: MessagesRetrievalErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            physical_requests: 0,
        }
    }

    fn invalid_request(message: &str) -> Self {
        Self::new(MessagesRetrievalErrorKind::InvalidRequest, message)
    }

    fn invalid_response(message: impl Into<String>) -> Self {
        Self::new(MessagesRetrievalErrorKind::InvalidResponse, message)
    }

    /// The failure class.
    #[must_use]
    pub const fn kind(&self) -> MessagesRetrievalErrorKind {
        self.kind
    }

    /// The HTTP status of the final refusal, when there was one.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    /// Physical requests the failed exchange sent, retries included.
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

impl fmt::Display for MessagesRetrievalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MessagesRetrievalError {}

/// The future [`MessagesRetrievalTransport::authorize`] returns.
pub type MessagesRetrievalAuthorization<'a> =
    Pin<Box<dyn Future<Output = Result<HttpRequest, MessagesRetrievalError>> + Send + 'a>>;

/// The route's half of a side request.
pub trait MessagesRetrievalTransport: Send + Sync {
    /// Revalidates admission and the credential, then builds the exact
    /// authorized request carrying `body`. Called before every physical
    /// request, retries included.
    fn authorize<'a>(&'a self, body: &'a [u8]) -> MessagesRetrievalAuthorization<'a>;
}

/// Exchange bounds of one side request.
#[derive(Clone, Copy, Debug)]
pub struct MessagesRetrievalLimits {
    /// Bound of one physical exchange up to its response head and of its
    /// streamed body.
    pub exchange_timeout: Duration,
    /// The backoff sample; `None` draws one per wait.
    pub jitter: Option<Jitter>,
}

impl Default for MessagesRetrievalLimits {
    fn default() -> Self {
        Self {
            exchange_timeout: Duration::from_secs(120),
            jitter: None,
        }
    }
}

/// Runs one side request to its answer: the bounded retry of
/// [`RetryPolicy::TURN`] inside each exchange, and up to
/// [`MAX_MESSAGES_RETRIEVAL_CONTINUATIONS`] `pause_turn` continuations.
///
/// # Errors
///
/// Refuses a body outside the allowlist before dispatch; otherwise returns
/// the final failure after retries.
pub async fn run_messages_retrieval(
    client: &Client,
    transport: &dyn MessagesRetrievalTransport,
    request: &MessagesRetrievalRequest,
    cancellation: &CancellationToken,
    limits: MessagesRetrievalLimits,
) -> Result<MessagesRetrievalOutcome, MessagesRetrievalError> {
    let mut content = Vec::<Value>::new();
    let mut usage = MessagesRetrievalUsage::default();
    let mut physical_requests = 0_u32;
    for exchange in 0..=MAX_MESSAGES_RETRIEVAL_CONTINUATIONS {
        let body = request.body(&content)?;
        if classify_messages_retrieval_body(&body) != ReplaySafety::CostOnly {
            return Err(MessagesRetrievalError::invalid_request(
                "side request body is outside the hosted retrieval allowlist",
            ));
        }
        let thinking = request.effort.is_some();
        let (finished, physical) =
            exchange_with_retry(client, transport, &body, thinking, cancellation, limits).await?;
        physical_requests = physical_requests.saturating_add(physical);
        usage.input_tokens = usage
            .input_tokens
            .saturating_add(finished.usage.input_tokens);
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(finished.usage.output_tokens);
        usage.hosted_requests = usage
            .hosted_requests
            .saturating_add(finished.usage.hosted_requests);
        content.extend(finished.content);
        let retained = content
            .iter()
            .map(|block| block.to_string().len())
            .sum::<usize>();
        if retained > MAX_RETAINED_CONTENT_BYTES {
            return Err(MessagesRetrievalError::invalid_response(
                "side request content exceeded its retained-byte bound",
            ));
        }
        match finished.stop.as_str() {
            "pause_turn" => {}
            "end_turn" | "max_tokens" | "stop_sequence" => {
                return assemble(&content, usage, exchange + 1).map(|outcome| {
                    MessagesRetrievalOutcome {
                        physical_requests,
                        ..outcome
                    }
                });
            }
            "refusal" => {
                return Err(MessagesRetrievalError::new(
                    MessagesRetrievalErrorKind::Rejected,
                    "the provider refused the side request",
                ));
            }
            other => {
                return Err(MessagesRetrievalError::invalid_response(format!(
                    "side request ended with unexpected stop reason {other:?}"
                )));
            }
        }
    }
    Err(MessagesRetrievalError::new(
        MessagesRetrievalErrorKind::Unavailable,
        format!(
            "the provider paused the side request more than {MAX_MESSAGES_RETRIEVAL_CONTINUATIONS} times"
        ),
    ))
}

/// One exchange that reached a terminal stop.
struct Finished {
    content: Vec<Value>,
    stop: String,
    usage: MessagesRetrievalUsage,
}

enum Attempted {
    Done(Finished),
    Retry {
        point: FailurePoint,
        retry_after: Option<Duration>,
        error: MessagesRetrievalError,
    },
}

async fn exchange_with_retry(
    client: &Client,
    transport: &dyn MessagesRetrievalTransport,
    body: &[u8],
    thinking: bool,
    cancellation: &CancellationToken,
    limits: MessagesRetrievalLimits,
) -> Result<(Finished, u32), MessagesRetrievalError> {
    let mut physical = 0_u32;
    let mut waited = Duration::ZERO;
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        physical = physical.saturating_add(1);
        let attempted =
            exchange_once(client, transport, body, thinking, cancellation, limits).await;
        let (point, retry_after, error) = match attempted {
            Ok(Attempted::Done(finished)) => return Ok((finished, physical)),
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
            let mut error = error;
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

fn cancelled() -> MessagesRetrievalError {
    MessagesRetrievalError::new(
        MessagesRetrievalErrorKind::Cancelled,
        "the side request was cancelled",
    )
}

async fn exchange_once(
    client: &Client,
    transport: &dyn MessagesRetrievalTransport,
    body: &[u8],
    thinking: bool,
    cancellation: &CancellationToken,
    limits: MessagesRetrievalLimits,
) -> Result<Attempted, MessagesRetrievalError> {
    let request = transport.authorize(body).await?;
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
            return Ok(Attempted::Retry {
                point: transport_point(failure),
                retry_after: None,
                error: unavailable(failure),
            });
        }
    };
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
    let mut body = response.into_body();
    let mut collector = Collector::new(thinking);
    loop {
        let next = cancellable(body.next_chunk(), cancellation, &http_cancellation).await;
        if next.cancelled {
            let _ = body.close().await;
            return Err(cancelled());
        }
        match next.value {
            Some(Ok(chunk)) => match collector.push(&chunk) {
                Ok(()) => {}
                Err(Broken::Overloaded(error)) => {
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
        Ok(finished) => Ok(Attempted::Done(finished)),
        Err(error) => Ok(Attempted::Retry {
            point: FailurePoint::MidStream,
            retry_after: None,
            error,
        }),
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

fn unavailable(failure: HttpFailure) -> MessagesRetrievalError {
    let phase = match failure.phase() {
        Phase::Resolution => "DNS resolution",
        Phase::Connect => "connect",
        Phase::Tls => "TLS",
        Phase::RequestWrite | Phase::RequestWriteOrResponseHead => "request write",
        Phase::ResponseHead => "response head",
        _ => "response stream",
    };
    MessagesRetrievalError::new(
        MessagesRetrievalErrorKind::Unavailable,
        format!("the provider was unreachable ({phase} failed)"),
    )
}

/// One bounded line from a provider error body: its `error.type` and
/// `error.message`, never the raw body.
fn status_error(status: u16, body: &[u8]) -> MessagesRetrievalError {
    let detail = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            let error = value.get("error")?;
            let kind = error.get("type").and_then(Value::as_str).unwrap_or("error");
            let message = error.get("message").and_then(Value::as_str).unwrap_or("");
            Some(format!("{kind}: {}", bounded_line(message, 300)))
        });
    let kind = match status {
        401 | 403 => MessagesRetrievalErrorKind::Unauthorized,
        429 | 500..=599 => MessagesRetrievalErrorKind::Unavailable,
        _ => MessagesRetrievalErrorKind::Rejected,
    };
    let message = match detail {
        Some(detail) => format!("the provider refused the side request (HTTP {status}, {detail})"),
        None => format!("the provider refused the side request (HTTP {status})"),
    };
    MessagesRetrievalError::new(kind, message)
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
    /// The provider reported a transient overload mid-stream.
    Overloaded(MessagesRetrievalError),
    Fatal(MessagesRetrievalError),
}

impl From<MessagesRetrievalError> for Broken {
    fn from(error: MessagesRetrievalError) -> Self {
        Self::Fatal(error)
    }
}

/// One open content block.
struct OpenBlock {
    block: Map<String, Value>,
    text: String,
    citations: Vec<Value>,
    input_json: String,
    signature: String,
}

/// The side-request grammar over one exchange's SSE events.
#[derive(Default)]
struct Collector {
    sse: Option<SseDecoder>,
    started: bool,
    stopped: bool,
    blocks: Vec<Value>,
    open: Option<OpenBlock>,
    stop: Option<String>,
    usage: MessagesRetrievalUsage,
    server_tool_ids: BTreeSet<String>,
    thinking: bool,
}

impl Collector {
    fn new(thinking: bool) -> Self {
        Self {
            thinking,
            ..Self::default()
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), Broken> {
        let messages = self
            .sse
            .get_or_insert_with(|| SseDecoder::new(SSE_LIMITS))
            .push(bytes)
            .map_err(|_| {
                MessagesRetrievalError::invalid_response("side request SSE was malformed")
            })?;
        for message in messages {
            self.event(message.event(), message.data())?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Finished, MessagesRetrievalError> {
        if let Some(sse) = self.sse.as_mut() {
            let messages = sse.finish().map_err(|_| {
                MessagesRetrievalError::invalid_response("side request SSE was truncated")
            })?;
            for message in messages {
                self.event(message.event(), message.data())
                    .map_err(|broken| match broken {
                        Broken::Overloaded(error) | Broken::Fatal(error) => error,
                    })?;
            }
        }
        if !self.stopped || self.open.is_some() {
            return Err(MessagesRetrievalError::new(
                MessagesRetrievalErrorKind::Unavailable,
                "the side request stream ended before message_stop",
            ));
        }
        let stop = self.stop.ok_or_else(|| {
            MessagesRetrievalError::invalid_response("side request omitted its stop reason")
        })?;
        Ok(Finished {
            content: self.blocks,
            stop,
            usage: self.usage,
        })
    }

    fn event(&mut self, name: Option<&str>, data: &str) -> Result<(), Broken> {
        let value: Value = serde_json::from_str(data).map_err(|_| {
            MessagesRetrievalError::invalid_response("side request event was not JSON")
        })?;
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if name.is_some_and(|name| name != kind) {
            return Err(MessagesRetrievalError::invalid_response(
                "side request event name disagreed with its type",
            )
            .into());
        }
        if kind == "ping" {
            return Ok(());
        }
        if kind == "error" {
            let error_type = value
                .pointer("/error/type")
                .and_then(Value::as_str)
                .unwrap_or("error");
            let error = MessagesRetrievalError::new(
                MessagesRetrievalErrorKind::Unavailable,
                format!("the provider failed the side request mid-stream ({error_type})"),
            );
            return Err(if error_type == "overloaded_error" {
                Broken::Overloaded(error)
            } else {
                Broken::Fatal(error)
            });
        }
        if self.stopped {
            return Err(MessagesRetrievalError::invalid_response(
                "side request event after message_stop",
            )
            .into());
        }
        match kind {
            "message_start" if !self.started => {
                self.started = true;
                self.usage.input_tokens = u64_at(&value, "/message/usage/input_tokens");
                self.usage.output_tokens = u64_at(&value, "/message/usage/output_tokens");
                Ok(())
            }
            _ if !self.started => Err(MessagesRetrievalError::invalid_response(
                "side request event before message_start",
            )
            .into()),
            "content_block_start" => self.block_start(&value).map_err(Broken::from),
            "content_block_delta" => self.block_delta(&value).map_err(Broken::from),
            "content_block_stop" => self.block_stop().map_err(Broken::from),
            "message_delta" => {
                if let Some(stop) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(stop.to_owned());
                }
                if let Some(input) = value.pointer("/usage/input_tokens").and_then(Value::as_u64) {
                    self.usage.input_tokens = input;
                }
                if let Some(output) = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                {
                    self.usage.output_tokens = output;
                }
                let hosted = u64_at(&value, "/usage/server_tool_use/web_search_requests")
                    .saturating_add(u64_at(&value, "/usage/server_tool_use/web_fetch_requests"));
                if hosted > 0 {
                    self.usage.hosted_requests = hosted;
                }
                Ok(())
            }
            "message_stop" => {
                self.stopped = true;
                Ok(())
            }
            _ => Err(MessagesRetrievalError::invalid_response(format!(
                "side request carried unexpected event {kind:?}"
            ))
            .into()),
        }
    }

    fn block_start(&mut self, value: &Value) -> Result<(), MessagesRetrievalError> {
        if self.open.is_some() {
            return Err(MessagesRetrievalError::invalid_response(
                "side request block started before the previous one stopped",
            ));
        }
        if value.get("index").and_then(Value::as_u64) != u64::try_from(self.blocks.len()).ok()
            || self.blocks.len() >= MAX_BLOCKS
        {
            return Err(MessagesRetrievalError::invalid_response(
                "side request block indices are not contiguous or exceed the bound",
            ));
        }
        let Some(Value::Object(block)) = value.get("content_block").cloned() else {
            return Err(MessagesRetrievalError::invalid_response(
                "side request block is not an object",
            ));
        };
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text" | "web_search_tool_result" | "web_fetch_tool_result" => {}
            "thinking" | "redacted_thinking" if self.thinking => {}
            "server_tool_use" => {
                let name = block.get("name").and_then(Value::as_str);
                let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                if !matches!(name, Some("web_search" | "web_fetch"))
                    || id.is_empty()
                    || !self.server_tool_ids.insert(id.to_owned())
                {
                    return Err(MessagesRetrievalError::invalid_response(
                        "side request used a server tool it did not offer",
                    ));
                }
            }
            other => {
                return Err(MessagesRetrievalError::invalid_response(format!(
                    "side request carried unexpected block {other:?}"
                )));
            }
        }
        let text = block
            .get(if kind == "thinking" {
                "thinking"
            } else {
                "text"
            })
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let citations = block
            .get("citations")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.open = Some(OpenBlock {
            block,
            text,
            citations,
            input_json: String::new(),
            signature: String::new(),
        });
        Ok(())
    }

    fn block_delta(&mut self, value: &Value) -> Result<(), MessagesRetrievalError> {
        let index = u64::try_from(self.blocks.len()).ok();
        let open = self
            .open
            .as_mut()
            .filter(|_| value.get("index").and_then(Value::as_u64) == index)
            .ok_or_else(|| {
                MessagesRetrievalError::invalid_response("side request delta for no open block")
            })?;
        let kind = open.block.get("type").and_then(Value::as_str).unwrap_or("");
        match (kind, value.pointer("/delta/type").and_then(Value::as_str)) {
            ("text", Some("text_delta")) => {
                open.text.push_str(
                    value
                        .pointer("/delta/text")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
            }
            ("text", Some("citations_delta")) => {
                if let Some(citation) = value.pointer("/delta/citation") {
                    open.citations.push(citation.clone());
                }
            }
            ("thinking", Some("thinking_delta")) => {
                open.text.push_str(
                    value
                        .pointer("/delta/thinking")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
            }
            ("thinking", Some("signature_delta")) => {
                open.signature.push_str(
                    value
                        .pointer("/delta/signature")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
            }
            ("server_tool_use", Some("input_json_delta")) => {
                open.input_json.push_str(
                    value
                        .pointer("/delta/partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
            }
            _ => {
                return Err(MessagesRetrievalError::invalid_response(
                    "side request delta does not fit its block",
                ));
            }
        }
        Ok(())
    }

    fn block_stop(&mut self) -> Result<(), MessagesRetrievalError> {
        let OpenBlock {
            mut block,
            text,
            citations,
            input_json,
            signature,
        } = self.open.take().ok_or_else(|| {
            MessagesRetrievalError::invalid_response("side request stopped no open block")
        })?;
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                block.insert("text".to_owned(), Value::String(text));
                if citations.is_empty() {
                    block.remove("citations");
                } else {
                    block.insert("citations".to_owned(), Value::Array(citations));
                }
            }
            Some("thinking") => {
                block.insert("thinking".to_owned(), Value::String(text));
                if !signature.is_empty() {
                    block.insert("signature".to_owned(), Value::String(signature));
                }
            }
            Some("server_tool_use") if !input_json.is_empty() => {
                let input = serde_json::from_str(&input_json).map_err(|_| {
                    MessagesRetrievalError::invalid_response("side request tool input was not JSON")
                })?;
                block.insert("input".to_owned(), input);
            }
            _ => {}
        }
        self.blocks.push(Value::Object(block));
        Ok(())
    }
}

fn u64_at(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

/// The answer and sources of the whole side request.
fn assemble(
    content: &[Value],
    usage: MessagesRetrievalUsage,
    exchanges: usize,
) -> Result<MessagesRetrievalOutcome, MessagesRetrievalError> {
    let mut sources = Vec::<MessagesRetrievalSource>::new();
    let mut seen = BTreeSet::new();
    let mut add = |url: Option<&str>, title: Option<&str>| {
        if let Some(url) = url.filter(|url| !url.is_empty())
            && seen.insert(url.to_owned())
        {
            sources.push(MessagesRetrievalSource {
                url: url.to_owned(),
                title: title.filter(|title| !title.is_empty()).map(str::to_owned),
            });
        }
    };
    let mut errors = Vec::new();
    let mut answer_from = 0;
    for (index, block) in content.iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("web_search_tool_result") => {
                answer_from = index + 1;
                match block.get("content") {
                    Some(Value::Array(results)) => {
                        for result in results {
                            add(
                                result.get("url").and_then(Value::as_str),
                                result.get("title").and_then(Value::as_str),
                            );
                        }
                    }
                    Some(error) => errors.push(error_code(error)),
                    None => {}
                }
            }
            Some("web_fetch_tool_result") => {
                answer_from = index + 1;
                match block.pointer("/content/type").and_then(Value::as_str) {
                    Some("web_fetch_result") => add(
                        block.pointer("/content/url").and_then(Value::as_str),
                        block
                            .pointer("/content/content/title")
                            .and_then(Value::as_str),
                    ),
                    _ => errors.push(error_code(block.get("content").unwrap_or(&Value::Null))),
                }
            }
            Some("text") => {
                for citation in block
                    .get("citations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    add(
                        citation.get("url").and_then(Value::as_str),
                        citation.get("title").and_then(Value::as_str),
                    );
                }
            }
            _ => {}
        }
    }
    let text = content[answer_from.min(content.len())..]
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<String>();
    if text.trim().is_empty()
        && let Some(code) = errors.first()
    {
        return Err(MessagesRetrievalError::new(
            MessagesRetrievalErrorKind::Rejected,
            format!("the provider's retrieval failed ({code})"),
        ));
    }
    Ok(MessagesRetrievalOutcome {
        text,
        sources,
        usage,
        exchanges,
        physical_requests: 0,
    })
}

fn error_code(error: &Value) -> String {
    bounded_line(
        error
            .get("error_code")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error"),
        64,
    )
}

#[cfg(test)]
mod tests;
