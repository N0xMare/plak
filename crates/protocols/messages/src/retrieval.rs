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

use plakous_attempt::ReplaySafety;
use plakous_http::{Client, SseDecoder, SseLimits};
use plakous_model::CancellationToken;
use serde::Serialize;
use serde_json::{Map, Value, json};

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

const ALLOWED_CALLERS: [&str; 1] = ["direct"];
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

use plakous_exchange::side_request::{self, BodyCollector, Broken, ResponseReader, bounded_line};
pub use plakous_exchange::side_request::{
    RetrievalAuthorization as MessagesRetrievalAuthorization,
    RetrievalError as MessagesRetrievalError, RetrievalErrorKind as MessagesRetrievalErrorKind,
    RetrievalLimits as MessagesRetrievalLimits, RetrievalOutcome as MessagesRetrievalOutcome,
    RetrievalSource as MessagesRetrievalSource, RetrievalTransport as MessagesRetrievalTransport,
    RetrievalUsage as MessagesRetrievalUsage,
};

/// Runs one side request to its answer: the bounded retry of
/// [`plakous_attempt::RetryPolicy::TURN`] inside each exchange, and up to
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
    for exchange in 0..=MAX_MESSAGES_RETRIEVAL_CONTINUATIONS {
        let body = request.body(&content)?;
        if classify_messages_retrieval_body(&body) != ReplaySafety::CostOnly {
            return Err(MessagesRetrievalError::invalid_request(
                "side request body is outside the hosted retrieval allowlist",
            ));
        }
        let thinking = request.effort.is_some();
        let finished = side_request::exchange_with_retry(
            client,
            transport,
            &body,
            MessagesReader { thinking },
            cancellation,
            limits,
        )
        .await?;
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
                return assemble(&content, usage, exchange + 1);
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

struct MessagesReader {
    thinking: bool,
}
impl ResponseReader for MessagesReader {
    type Collector = Collector;
    fn start(&self, _: &plakous_http::Response) -> Collector {
        Collector::new(self.thinking)
    }
    fn status_error(&self, status: u16, body: &[u8]) -> MessagesRetrievalError {
        status_error(status, body)
    }
}
impl BodyCollector for Collector {
    type Output = Finished;
    fn push(&mut self, bytes: &[u8]) -> Result<(), Broken> {
        Collector::push(self, bytes)
    }
    fn finish(self) -> Result<Finished, Broken> {
        Collector::finish(self).map_err(Broken::Transient)
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
                        Broken::Transient(error) | Broken::Fatal(error) => error,
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
                Broken::Transient(error)
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
