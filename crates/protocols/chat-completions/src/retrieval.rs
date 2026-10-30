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

use plakous_attempt::ReplaySafety;
use plakous_http::{Client, Failure as HttpFailure, SseDecoder, SseLimits};
use plakous_model::CancellationToken;
use serde::Serialize;
use serde_json::{Map, Value};

/// The gateway plugin a web search side request enables.
pub const CHAT_WEB_PLUGIN_ID: &str = "web";
/// The search engine the `web` plugin is pinned to. Left unset, the gateway
/// picks one per model and, for some models, runs no search at all.
pub const CHAT_WEB_PLUGIN_ENGINE: &str = "exa";
/// Results the `web` plugin may add to one side request.
pub const CHAT_WEB_PLUGIN_MAX_RESULTS: u32 = 5;
/// Output cap of one side request.
pub const CHAT_RETRIEVAL_MAX_TOKENS: u32 = 4096;

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

use plakous_exchange::side_request::{
    self, BodyCollector, Broken, Document, ResponseReader, bounded_line,
};
pub use plakous_exchange::side_request::{
    RetrievalAuthorization as ChatRetrievalAuthorization, RetrievalError as ChatRetrievalError,
    RetrievalErrorKind as ChatRetrievalErrorKind, RetrievalLimits as ChatRetrievalLimits,
    RetrievalOutcome as ChatRetrievalOutcome, RetrievalSource as ChatRetrievalSource,
    RetrievalTransport as ChatRetrievalTransport, RetrievalUsage as ChatRetrievalUsage,
};

/// Runs one side request to its answer under the bounded retry of
/// [`plakous_attempt::RetryPolicy::TURN`].
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
    side_request::exchange_with_retry(client, transport, &body, ChatReader, cancellation, limits)
        .await
}

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

struct ChatReader;
enum ChatBody {
    Stream(Box<Collector>),
    Document(Document),
}
impl ResponseReader for ChatReader {
    type Collector = ChatBody;
    fn start(&self, response: &plakous_http::Response) -> ChatBody {
        let streamed = response
            .headers()
            .get(&plakous_http::HeaderName::from_static("content-type"))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/event-stream"));
        if streamed {
            ChatBody::Stream(Box::default())
        } else {
            ChatBody::Document(Document::new(
                MAX_JSON_BODY_BYTES,
                "side request body exceeded its byte bound",
            ))
        }
    }
    fn status_error(&self, status: u16, body: &[u8]) -> ChatRetrievalError {
        status_error(status, body)
    }
}
impl BodyCollector for ChatBody {
    type Output = ChatRetrievalOutcome;
    fn push(&mut self, bytes: &[u8]) -> Result<(), Broken> {
        match self {
            Self::Stream(collector) => collector.push(bytes),
            Self::Document(document) => document.push(bytes),
        }
    }
    fn finish(self) -> Result<Self::Output, Broken> {
        match self {
            Self::Stream(collector) => (*collector).finish(),
            Self::Document(document) => completed(&document.finish()?).map_err(Broken::Fatal),
        }
    }
    fn transport_failure(self, failure: HttpFailure) -> Result<Self::Output, Broken> {
        match self {
            Self::Document(document) if failure.kind() != plakous_http::ErrorKind::Internal => {
                completed(&document.finish()?).map_err(Broken::Fatal)
            }
            _ => Err(Broken::Transient(side_request::unavailable(failure))),
        }
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
            exchanges: 1,
            text: self.text,
            sources: self.sources,
            usage: self.usage,
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
