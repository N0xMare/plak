//! Isolated Chat Completions side requests for hosted web search.
//!
//! One wrapper tool call is one logical side request: a fresh one-message
//! Chat body that enables exactly one gateway `web` plugin, on the session's
//! own route, under its own bounded retry. The side request has its own
//! grammar: it accepts `url_citation` annotations (streamed in `delta`, or in
//! `message` of a non-streamed body) and refuses tool calls, and it never
//! produces a model event, a transcript item or a canonical history entry.
//! Main-exchange streams keep refusing annotations, and main-exchange bodies
//! keep classifying an enabled plugin as a server effect.

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
use serde_json::{Map, Value};
use tokio::time::Instant;

/// The gateway plugin a web search side request enables.
pub const CHAT_WEB_PLUGIN_ID: &str = "web";
/// The search engine the `web` plugin is pinned to. Left unset, the gateway
/// picks one per model and, for some models, runs no search at all.
pub const CHAT_WEB_PLUGIN_ENGINE: &str = "exa";
/// Results the `web` plugin may add to one side request.
pub const CHAT_WEB_PLUGIN_MAX_RESULTS: u32 = 5;
/// Output cap of one side request.
pub const CHAT_RETRIEVAL_MAX_TOKENS: u32 = 4096;

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_JSON_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_ANSWER_BYTES: usize = 1024 * 1024;
const MAX_SOURCES: usize = 256;
const SSE_LIMITS: SseLimits = SseLimits {
    max_line_bytes: 1024 * 1024,
    max_event_bytes: 2 * 1024 * 1024,
};

/// How the side request states reasoning, from the model's attested facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatRetrievalReasoning {
    /// No `reasoning` field: the model does not reason by default.
    Omit,
    /// `reasoning: {enabled: false}`: optional reasoning the model enables
    /// by default is switched off.
    Disabled,
    /// `reasoning: {effort, exclude: true}` at the lowest effort a model
    /// that must reason admits.
    Lowest(&'static str),
}

/// The provider-data restriction the route's main exchange states.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChatRetrievalRouting {
    /// `provider.data_collection`, when constrained.
    pub data_collection: Option<String>,
    /// `provider.zdr`, when constrained.
    pub zdr: Option<bool>,
}

/// One web search side request's exact inputs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatRetrievalRequest {
    model: String,
    query: String,
    include_domains: Vec<String>,
    exclude_domains: Vec<String>,
    reasoning: ChatRetrievalReasoning,
    routing: ChatRetrievalRouting,
}

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    messages: [Message; 1],
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Value>,
    provider: Provider<'a>,
    plugins: [Plugin<'a>; 1],
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct Message {
    role: &'static str,
    content: String,
}

#[derive(Serialize)]
struct Provider<'a> {
    allow_fallbacks: bool,
    require_parameters: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_collection: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    zdr: Option<bool>,
}

#[derive(Serialize)]
struct Plugin<'a> {
    id: &'static str,
    engine: &'static str,
    max_results: u32,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    include_domains: &'a [String],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    exclude_domains: &'a [String],
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

impl ChatRetrievalRequest {
    /// A web search side request on `model` without domain filters,
    /// reasoning or routing restrictions.
    #[must_use]
    pub fn new(model: impl Into<String>, query: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            query: query.into(),
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
            reasoning: ChatRetrievalReasoning::Omit,
            routing: ChatRetrievalRouting::default(),
        }
    }

    /// Restricts results to `include` and away from `exclude`.
    #[must_use]
    pub fn with_domains(mut self, include: Vec<String>, exclude: Vec<String>) -> Self {
        self.include_domains = include;
        self.exclude_domains = exclude;
        self
    }

    /// States reasoning as the model's facts require.
    #[must_use]
    pub const fn with_reasoning(mut self, reasoning: ChatRetrievalReasoning) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Carries the route's provider-data restriction.
    #[must_use]
    pub fn with_routing(mut self, routing: ChatRetrievalRouting) -> Self {
        self.routing = routing;
        self
    }

    /// The one user message.
    #[must_use]
    pub fn prompt(&self) -> String {
        format!(
            "Search the web for: {}. Answer concisely and cite sources.",
            self.query
        )
    }

    /// The exact body.
    ///
    /// # Errors
    ///
    /// Fails only when serialization fails.
    pub fn body(&self) -> Result<Vec<u8>, ChatRetrievalError> {
        serde_json::to_vec(&Body {
            model: &self.model,
            messages: [Message {
                role: "user",
                content: self.prompt(),
            }],
            max_tokens: CHAT_RETRIEVAL_MAX_TOKENS,
            reasoning: match self.reasoning {
                ChatRetrievalReasoning::Omit => None,
                ChatRetrievalReasoning::Disabled => Some(serde_json::json!({"enabled": false})),
                ChatRetrievalReasoning::Lowest(effort) => {
                    Some(serde_json::json!({"effort": effort, "exclude": true}))
                }
            },
            provider: Provider {
                allow_fallbacks: false,
                require_parameters: true,
                data_collection: self.routing.data_collection.as_deref(),
                zdr: self.routing.zdr,
            },
            plugins: [Plugin {
                id: CHAT_WEB_PLUGIN_ID,
                engine: CHAT_WEB_PLUGIN_ENGINE,
                max_results: CHAT_WEB_PLUGIN_MAX_RESULTS,
                include_domains: &self.include_domains,
                exclude_domains: &self.exclude_domains,
            }],
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        })
        .map_err(|_| ChatRetrievalError::invalid_request("side request could not be encoded"))
    }
}

/// Classifies a side-request body by the typed per-tool allowlist: exactly
/// one enabled `web` plugin with only the fields the builder emits, no
/// tools, and only known top-level fields, is [`ReplaySafety::CostOnly`].
/// Anything else, including a plugin nobody has classified, is
/// [`ReplaySafety::ServerEffect`].
#[must_use]
pub fn classify_chat_retrieval_body(body: &[u8]) -> ReplaySafety {
    const FIELDS: [&str; 8] = [
        "model",
        "messages",
        "max_tokens",
        "reasoning",
        "provider",
        "plugins",
        "stream",
        "stream_options",
    ];
    const PLUGIN_FIELDS: [&str; 5] = [
        "id",
        "engine",
        "max_results",
        "include_domains",
        "exclude_domains",
    ];
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(body) else {
        return ReplaySafety::ServerEffect;
    };
    if body.keys().any(|key| !FIELDS.contains(&key.as_str())) {
        return ReplaySafety::ServerEffect;
    }
    let Some([Value::Object(plugin)]) = body
        .get("plugins")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    else {
        return ReplaySafety::ServerEffect;
    };
    if plugin.get("id").and_then(Value::as_str) != Some(CHAT_WEB_PLUGIN_ID)
        || plugin.get("engine").and_then(Value::as_str) != Some(CHAT_WEB_PLUGIN_ENGINE)
        || plugin
            .keys()
            .any(|key| !PLUGIN_FIELDS.contains(&key.as_str()))
    {
        return ReplaySafety::ServerEffect;
    }
    ReplaySafety::CostOnly
}

/// One source the provider cited.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatRetrievalSource {
    /// The URL.
    pub url: String,
    /// The title, when reported.
    pub title: Option<String>,
}

/// What a side request consumed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChatRetrievalUsage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Web searches: `usage.server_tool_use.web_search_requests` when the
    /// gateway reports it, otherwise the plugin's one search per request.
    pub hosted_requests: u64,
}

/// The completed side request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChatRetrievalOutcome {
    /// The answer text.
    pub text: String,
    /// Sources from `url_citation` annotations, first-reported order, URLs
    /// unique.
    pub sources: Vec<ChatRetrievalSource>,
    /// Usage of the side request.
    pub usage: ChatRetrievalUsage,
    /// Physical requests sent, retries included.
    pub physical_requests: u32,
}

/// Why a side request failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChatRetrievalErrorKind {
    /// The body is not an allowlisted side request or could not be built.
    InvalidRequest,
    /// Admission or the credential is no longer current, or HTTP 401/403.
    Unauthorized,
    /// The provider refused the request (a non-retryable status or refusal).
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
pub struct ChatRetrievalError {
    kind: ChatRetrievalErrorKind,
    message: String,
    status: Option<u16>,
    physical_requests: u32,
}

impl ChatRetrievalError {
    /// A failure of `kind`.
    #[must_use]
    pub fn new(kind: ChatRetrievalErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            physical_requests: 0,
        }
    }

    fn invalid_request(message: &str) -> Self {
        Self::new(ChatRetrievalErrorKind::InvalidRequest, message)
    }

    fn invalid_response(message: impl Into<String>) -> Self {
        Self::new(ChatRetrievalErrorKind::InvalidResponse, message)
    }

    /// The failure class.
    #[must_use]
    pub const fn kind(&self) -> ChatRetrievalErrorKind {
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

impl fmt::Display for ChatRetrievalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ChatRetrievalError {}

/// The future [`ChatRetrievalTransport::authorize`] returns.
pub type ChatRetrievalAuthorization<'a> =
    Pin<Box<dyn Future<Output = Result<HttpRequest, ChatRetrievalError>> + Send + 'a>>;

/// The route's half of a side request.
pub trait ChatRetrievalTransport: Send + Sync {
    /// Revalidates admission and the credential, then builds the exact
    /// authorized request carrying `body`. Called before every physical
    /// request, retries included.
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ChatRetrievalAuthorization<'a>;
}

/// Exchange bounds of one side request.
#[derive(Clone, Copy, Debug)]
pub struct ChatRetrievalLimits {
    /// Bound of one physical exchange up to its response head and of its
    /// body.
    pub exchange_timeout: Duration,
    /// The backoff sample; `None` draws one per wait.
    pub jitter: Option<Jitter>,
}

impl Default for ChatRetrievalLimits {
    fn default() -> Self {
        Self {
            exchange_timeout: Duration::from_secs(120),
            jitter: None,
        }
    }
}

/// The most physical requests one side request can send: one exchange,
/// retried within [`RetryPolicy::TURN`]'s physical budget. A connect failure
/// counts, since its authorization was spent.
pub const MAX_CHAT_RETRIEVAL_PHYSICAL_REQUESTS: u32 = RetryPolicy::TURN.max_physical();

/// Runs one side request to its answer under the bounded retry of
/// [`RetryPolicy::TURN`].
///
/// # Errors
///
/// Refuses a body outside the allowlist before dispatch; otherwise returns
/// the final failure after retries.
pub async fn run_chat_retrieval(
    client: &Client,
    transport: &dyn ChatRetrievalTransport,
    request: &ChatRetrievalRequest,
    cancellation: &CancellationToken,
    limits: ChatRetrievalLimits,
) -> Result<ChatRetrievalOutcome, ChatRetrievalError> {
    let body = request.body()?;
    if classify_chat_retrieval_body(&body) != ReplaySafety::CostOnly {
        return Err(ChatRetrievalError::invalid_request(
            "side request body is outside the hosted retrieval allowlist",
        ));
    }
    let mut physical = 0_u32;
    let mut waited = Duration::ZERO;
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        physical = physical.saturating_add(1);
        let (point, retry_after, error) =
            match exchange_once(client, transport, &body, cancellation, limits).await {
                Ok(Attempted::Done(outcome)) => {
                    return Ok(ChatRetrievalOutcome {
                        physical_requests: physical,
                        ..outcome
                    });
                }
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

enum Attempted {
    Done(ChatRetrievalOutcome),
    Retry {
        point: FailurePoint,
        retry_after: Option<Duration>,
        error: ChatRetrievalError,
    },
}

fn cancelled() -> ChatRetrievalError {
    ChatRetrievalError::new(
        ChatRetrievalErrorKind::Cancelled,
        "the side request was cancelled",
    )
}

async fn exchange_once(
    client: &Client,
    transport: &dyn ChatRetrievalTransport,
    body: &[u8],
    cancellation: &CancellationToken,
    limits: ChatRetrievalLimits,
) -> Result<Attempted, ChatRetrievalError> {
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
    let streamed = response
        .headers()
        .get(&plakous_http::HeaderName::from_static("content-type"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/event-stream"));
    if !streamed {
        let body = read_bounded_body(
            response.into_body(),
            cancellation,
            &http_cancellation,
            MAX_JSON_BODY_BYTES,
        )
        .await;
        return whole(body, cancellation);
    }
    read_stream(response.into_body(), cancellation, &http_cancellation).await
}

/// A streamed 200 body through the side-request grammar.
async fn read_stream(
    mut body: plakous_http::ResponseBody,
    cancellation: &CancellationToken,
    http_cancellation: &HttpCancellationToken,
) -> Result<Attempted, ChatRetrievalError> {
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
        Ok(outcome) => Ok(Attempted::Done(outcome)),
        Err(Broken::Transient(error)) => Ok(Attempted::Retry {
            point: FailurePoint::MidStream,
            retry_after: None,
            error,
        }),
        Err(Broken::Fatal(error)) => Err(error),
    }
}

/// A non-streamed 200 body, read whole.
fn whole(
    body: Result<Vec<u8>, HttpFailure>,
    cancellation: &CancellationToken,
) -> Result<Attempted, ChatRetrievalError> {
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    match body {
        Ok(body) if body.len() <= MAX_JSON_BODY_BYTES => completed(&body).map(Attempted::Done),
        Ok(_) => Err(ChatRetrievalError::invalid_response(
            "side request body exceeded its byte bound",
        )),
        Err(failure) => Ok(Attempted::Retry {
            point: FailurePoint::MidStream,
            retry_after: None,
            error: unavailable(failure),
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

fn unavailable(failure: HttpFailure) -> ChatRetrievalError {
    let phase = match failure.phase() {
        Phase::Resolution => "DNS resolution",
        Phase::Connect => "connect",
        Phase::Tls => "TLS",
        Phase::RequestWrite | Phase::RequestWriteOrResponseHead => "request write",
        Phase::ResponseHead => "response head",
        _ => "response stream",
    };
    ChatRetrievalError::new(
        ChatRetrievalErrorKind::Unavailable,
        format!("the provider was unreachable ({phase} failed)"),
    )
}

/// One bounded line from a provider error body: its `error.message`, never
/// the raw body.
fn status_error(status: u16, body: &[u8]) -> ChatRetrievalError {
    let detail = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(|message| bounded_line(message, 300))
        })
        .filter(|detail| !detail.is_empty());
    let kind = match status {
        401 | 403 => ChatRetrievalErrorKind::Unauthorized,
        429 | 500..=599 => ChatRetrievalErrorKind::Unavailable,
        _ => ChatRetrievalErrorKind::Rejected,
    };
    let message = match detail {
        Some(detail) => format!("the provider refused the side request (HTTP {status}, {detail})"),
        None => format!("the provider refused the side request (HTTP {status})"),
    };
    ChatRetrievalError::new(kind, message)
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
    /// A cut stream or a transient provider failure: the retry decides.
    Transient(ChatRetrievalError),
    Fatal(ChatRetrievalError),
}

impl From<ChatRetrievalError> for Broken {
    fn from(error: ChatRetrievalError) -> Self {
        Self::Fatal(error)
    }
}

/// The fields one side-request message or delta may carry. Anything else,
/// tool calls included, was not asked for.
const MESSAGE_FIELDS: [&str; 7] = [
    "role",
    "content",
    "annotations",
    "refusal",
    "reasoning",
    "reasoning_content",
    "reasoning_details",
];

/// The answer and sources accumulated over one response.
#[derive(Default)]
struct Answer {
    text: String,
    sources: Vec<ChatRetrievalSource>,
    seen: BTreeSet<String>,
    usage: ChatRetrievalUsage,
    finish: Option<String>,
}

impl Answer {
    /// Folds one choice's message or delta into the answer.
    fn message(&mut self, message: &Map<String, Value>) -> Result<(), ChatRetrievalError> {
        if let Some(key) = message
            .keys()
            .find(|key| !MESSAGE_FIELDS.contains(&key.as_str()))
        {
            return Err(ChatRetrievalError::invalid_response(format!(
                "side request carried unexpected message field {:?}",
                bounded_line(key, 64)
            )));
        }
        match message.get("content") {
            None | Some(Value::Null) => {}
            Some(Value::String(text)) => {
                if self.text.len().saturating_add(text.len()) > MAX_ANSWER_BYTES {
                    return Err(ChatRetrievalError::invalid_response(
                        "side request answer exceeded its byte bound",
                    ));
                }
                self.text.push_str(text);
            }
            Some(_) => {
                return Err(ChatRetrievalError::invalid_response(
                    "side request content was not text",
                ));
            }
        }
        match message.get("annotations") {
            None | Some(Value::Null) => Ok(()),
            Some(Value::Array(annotations)) => annotations
                .iter()
                .try_for_each(|annotation| self.annotation(annotation)),
            Some(_) => Err(ChatRetrievalError::invalid_response(
                "side request annotations were not a list",
            )),
        }
    }

    fn annotation(&mut self, annotation: &Value) -> Result<(), ChatRetrievalError> {
        if annotation.get("type").and_then(Value::as_str) != Some("url_citation") {
            return Err(ChatRetrievalError::invalid_response(
                "side request carried an annotation other than url_citation",
            ));
        }
        let Some(url) = annotation
            .pointer("/url_citation/url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
        else {
            return Err(ChatRetrievalError::invalid_response(
                "side request url_citation had no URL",
            ));
        };
        if !self.seen.insert(url.to_owned()) {
            return Ok(());
        }
        if self.sources.len() >= MAX_SOURCES {
            return Err(ChatRetrievalError::invalid_response(
                "side request cited more sources than its bound",
            ));
        }
        self.sources.push(ChatRetrievalSource {
            url: url.to_owned(),
            title: annotation
                .pointer("/url_citation/title")
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty())
                .map(str::to_owned),
        });
        Ok(())
    }

    fn usage(&mut self, usage: &Value) {
        self.usage.input_tokens = u64_at(usage, "/prompt_tokens");
        self.usage.output_tokens = u64_at(usage, "/completion_tokens");
        if let Some(searches) = usage
            .pointer("/server_tool_use/web_search_requests")
            .and_then(Value::as_u64)
        {
            self.usage.hosted_requests = searches;
        }
    }

    /// One response object: a streamed chunk or the whole non-streamed body.
    fn object(&mut self, value: &Value, streamed: bool) -> Result<(), Broken> {
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            return Err(mid_stream_error(error));
        }
        let choices = match value.get("choices") {
            None | Some(Value::Null) => &[][..],
            Some(Value::Array(choices)) => choices.as_slice(),
            Some(_) => {
                return Err(ChatRetrievalError::invalid_response(
                    "side request choices were not a list",
                )
                .into());
            }
        };
        for choice in choices {
            if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                return Err(ChatRetrievalError::invalid_response(
                    "side request returned more than one choice",
                )
                .into());
            }
            let field = if streamed { "delta" } else { "message" };
            match choice.get(field) {
                None | Some(Value::Null) => {}
                Some(Value::Object(message)) => self.message(message)?,
                Some(_) => {
                    return Err(ChatRetrievalError::invalid_response(format!(
                        "side request {field} was not an object"
                    ))
                    .into());
                }
            }
            if self.finish.is_none()
                && let Some(finish) = choice.get("finish_reason").and_then(Value::as_str)
            {
                self.finish = Some(finish.to_owned());
            }
        }
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            self.usage(usage);
        }
        Ok(())
    }

    fn outcome(mut self) -> Result<ChatRetrievalOutcome, Broken> {
        match self.finish.as_deref() {
            Some("stop" | "length") => {}
            None => {
                return Err(Broken::Transient(ChatRetrievalError::new(
                    ChatRetrievalErrorKind::Unavailable,
                    "the side request ended before its finish reason",
                )));
            }
            Some("content_filter") => {
                return Err(ChatRetrievalError::new(
                    ChatRetrievalErrorKind::Rejected,
                    "the provider refused the side request",
                )
                .into());
            }
            Some(other) => {
                return Err(ChatRetrievalError::invalid_response(format!(
                    "side request ended with unexpected finish reason {:?}",
                    bounded_line(other, 64)
                ))
                .into());
            }
        }
        if self.usage.hosted_requests == 0 {
            self.usage.hosted_requests = 1;
        }
        Ok(ChatRetrievalOutcome {
            text: self.text,
            sources: self.sources,
            usage: self.usage,
            physical_requests: 0,
        })
    }
}

/// A provider error object in a 200 response: transient when its code says
/// so, otherwise a refusal.
fn mid_stream_error(error: &Value) -> Broken {
    let code = error.get("code").and_then(Value::as_u64);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(|message| bounded_line(message, 300))
        .unwrap_or_default();
    let text = format!("the provider failed the side request mid-stream ({message})");
    if matches!(code, Some(429 | 500..=599)) {
        Broken::Transient(ChatRetrievalError::new(
            ChatRetrievalErrorKind::Unavailable,
            text,
        ))
    } else {
        Broken::Fatal(ChatRetrievalError::new(
            ChatRetrievalErrorKind::Rejected,
            text,
        ))
    }
}

fn completed(body: &[u8]) -> Result<ChatRetrievalOutcome, ChatRetrievalError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| ChatRetrievalError::invalid_response("side request body was not JSON"))?;
    let mut answer = Answer::default();
    let finished = answer.object(&value, false).and_then(|()| answer.outcome());
    finished.map_err(|broken| match broken {
        Broken::Transient(error) | Broken::Fatal(error) => error,
    })
}

/// The side-request grammar over one streamed response's SSE events.
#[derive(Default)]
struct Collector {
    sse: Option<SseDecoder>,
    answer: Answer,
    done: bool,
}

impl Collector {
    fn push(&mut self, bytes: &[u8]) -> Result<(), Broken> {
        let messages = self
            .sse
            .get_or_insert_with(|| SseDecoder::new(SSE_LIMITS))
            .push(bytes)
            .map_err(|_| ChatRetrievalError::invalid_response("side request SSE was malformed"))?;
        for message in messages {
            self.event(message.data())?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<ChatRetrievalOutcome, Broken> {
        if let Some(sse) = self.sse.as_mut() {
            let messages = sse.finish().map_err(|_| {
                Broken::Transient(ChatRetrievalError::new(
                    ChatRetrievalErrorKind::Unavailable,
                    "the side request stream was truncated",
                ))
            })?;
            for message in messages {
                self.event(message.data())?;
            }
        }
        self.answer.outcome()
    }

    fn event(&mut self, data: &str) -> Result<(), Broken> {
        if self.done {
            return Err(
                ChatRetrievalError::invalid_response("side request event after [DONE]").into(),
            );
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| ChatRetrievalError::invalid_response("side request event was not JSON"))?;
        self.answer.object(&value, true)
    }
}

fn u64_at(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests;
