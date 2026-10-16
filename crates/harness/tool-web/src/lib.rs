//! Local wrapper tools for provider-hosted retrieval.
//!
//! Each wrapper is a local read-only tool. A call validates its arguments and
//! hands one [`RetrievalRequest`] to the session's [`RetrievalPort`], which
//! runs one isolated side request on the session's own route; the provider
//! performs the search or fetch inside that request. Only the rendered tool
//! result enters history.
//!
//! One module per tool family: `search` (`web_search`), `web_fetch`
//! (hosted page fetch) and `x_search`. The Plak-run `fetch_url` is a
//! separate tool and lives in its own module.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact provider tool identifiers"
)]

mod render;
mod search;
mod text;
mod web_fetch;
mod x_search;

pub use render::render_outcome;
pub use text::{
    PLAKOUS_WEB_CLAUDE_FETCH_DESCRIPTION, PLAKOUS_WEB_CLAUDE_SEARCH_DESCRIPTION,
    PLAKOUS_WEB_SEARCH_DESCRIPTION, PLAKOUS_WEB_X_SEARCH_DESCRIPTION,
};

use std::sync::Arc;

use plakous_model::{
    ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
};
use serde_json::Value;

use plakous_tools::{
    CodingPresentation, MenuEntry, PassthroughPack, PromptPin, RegisteredToolId,
    RetrievalErrorKind, RetrievalKind, RetrievalPort, RetrievalRequest, ToolContext, ToolExecutor,
    ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation, ToolOutcome, ToolPack,
    ToolRegistration, ToolRuntimeError, ToolRuntimeErrorKind, operation_control,
};

/// Longest accepted query, in bytes.
pub const MAX_QUERY_BYTES: usize = 512;
/// Longest accepted URL, in bytes.
pub const MAX_URL_BYTES: usize = 2048;
/// Longest accepted fetch prompt, in bytes.
pub const MAX_PROMPT_BYTES: usize = 2048;
/// Most domains or handles one call may name.
pub const MAX_DOMAINS: usize = 20;

const SEMANTIC_VERSION: &str = "plakous.web.v1";
const ISSUER: &[u8] = b"plakous-tool-web";

/// One hosted-retrieval wrapper, in plan order.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WebTool {
    /// Web search.
    WebSearch,
    /// Hosted page fetch.
    WebFetch,
    /// X post search.
    XSearch,
}

impl WebTool {
    /// The retrieval this wrapper asks its port for.
    #[must_use]
    pub const fn kind(self) -> RetrievalKind {
        match self {
            Self::WebSearch => RetrievalKind::WebSearch,
            Self::WebFetch => RetrievalKind::PageFetch,
            Self::XSearch => RetrievalKind::PostSearch,
        }
    }

    /// The tool name a template lists.
    #[must_use]
    pub const fn template_name(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
            Self::WebFetch => "web_fetch",
            Self::XSearch => "x_search",
        }
    }

    /// The model-visible name under `presentation`, or `None` when that
    /// vocabulary has no shape for this wrapper.
    #[must_use]
    pub fn presented_name(self, presentation: CodingPresentation) -> Option<&'static str> {
        self.shape(presentation).map(|shape| shape.name)
    }

    fn shape(self, presentation: CodingPresentation) -> Option<Shape> {
        match self {
            Self::WebSearch => Some(search::shape(presentation)),
            Self::WebFetch => web_fetch::shape(presentation),
            Self::XSearch => x_search::shape(presentation),
        }
    }

    fn request(self, arguments: Value) -> Result<RetrievalRequest, ToolRuntimeError> {
        match self {
            Self::WebSearch => search::request(arguments),
            Self::WebFetch => web_fetch::request(arguments),
            Self::XSearch => x_search::request(arguments),
        }
    }
}

/// One vocabulary's model-visible form of a wrapper.
pub(crate) struct Shape {
    name: &'static str,
    description: &'static str,
    schema: Value,
}

fn registration_error<E>(message: &'static str) -> impl FnOnce(E) -> ToolRuntimeError {
    move |_| ToolRuntimeError::new(ToolRuntimeErrorKind::Registration, message)
}

pub(crate) fn invalid(message: &'static str) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::InvalidArguments, message)
}

pub(crate) fn require_text(
    value: &str,
    max_bytes: usize,
    field: &'static str,
) -> Result<String, ToolRuntimeError> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            format!("{field} is empty"),
        ));
    }
    if value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            format!("{field} exceeds {max_bytes} bytes or has control characters"),
        ));
    }
    Ok(value)
}

/// The wrappers one session binds, presented in one vocabulary and served by
/// one route's port.
#[derive(Clone)]
pub struct WebToolPack {
    presentation: CodingPresentation,
    tools: Vec<WebTool>,
    port: Arc<dyn RetrievalPort>,
}

impl WebToolPack {
    /// Binds `tools`, deduplicated into plan order, to `port`.
    ///
    /// # Errors
    ///
    /// Refuses a tool the vocabulary has no shape for or the port does not
    /// serve.
    pub fn new(
        presentation: CodingPresentation,
        tools: impl IntoIterator<Item = WebTool>,
        port: Arc<dyn RetrievalPort>,
    ) -> Result<Self, ToolRuntimeError> {
        let mut tools = tools.into_iter().collect::<Vec<_>>();
        tools.sort_unstable();
        tools.dedup();
        for tool in &tools {
            if tool.shape(presentation).is_none() || !port.kinds().contains(&tool.kind()) {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::UnsupportedCapability,
                    format!(
                        "{} has no {presentation:?} presentation or no retrieval port",
                        tool.template_name()
                    ),
                ));
            }
        }
        Ok(Self {
            presentation,
            tools,
            port,
        })
    }

    /// The bound wrappers in plan order.
    #[must_use]
    pub fn tools(&self) -> &[WebTool] {
        &self.tools
    }

    fn registration(&self, tool: WebTool) -> Result<ToolRegistration, ToolRuntimeError> {
        let shape = tool.shape(self.presentation).ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "web tool has no shape in this vocabulary",
            )
        })?;
        let definition = ToolDefinition::new(
            ToolName::new(shape.name).map_err(registration_error("web tool name is invalid"))?,
            SEMANTIC_VERSION,
            shape.description,
            ToolInputContract::structured(shape.schema)
                .map_err(registration_error("web tool schema is invalid"))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )
        .map_err(registration_error("web tool definition is invalid"))?;
        let name = tool.template_name();
        Ok(ToolRegistration::new(
            RegisteredToolId::new(format!("plakous.web.{name}.v1"))
                .map_err(registration_error("web tool id is invalid"))?,
            ToolExecutorId::new(format!("plakous.web.{name}.executor.v1"))
                .map_err(registration_error("web executor id is invalid"))?,
            ToolExecutorBindingIdentity::new(1, &[ISSUER, name.as_bytes(), shape.name.as_bytes()])
                .map_err(registration_error("web binding identity is invalid"))?,
            definition,
            Arc::new(RetrievalTool {
                tool,
                presented: shape.name,
                port: Arc::clone(&self.port),
            }),
        )
        .nested_eligible())
    }
}

impl ToolPack for WebToolPack {
    fn id(&self) -> &'static str {
        "plakous.web.v1"
    }

    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        self.tools
            .iter()
            .map(|tool| self.registration(*tool))
            .collect()
    }

    /// Every wrapper appears under its vocabulary's name and description,
    /// appended after the core menu in plan order.
    fn menu(&self, _presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        let names = self
            .registrations()?
            .iter()
            .map(|registration| registration.name().clone())
            .collect::<Vec<_>>();
        PassthroughPack::appended_menu(&names)
    }

    fn pins(&self) -> &'static [PromptPin] {
        WEB_PROMPT_PINS
    }
}

struct RetrievalTool {
    tool: WebTool,
    presented: &'static str,
    port: Arc<dyn RetrievalPort>,
}

impl ToolExecutor for RetrievalTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let request = self
                .tool
                .request(invocation.arguments().value().as_ref().clone())?;
            let subject = subject(&request);
            let _ = context.progress().report(&subject, None, None);
            let outcome = self
                .port
                .retrieve(request, operation_control(&context))
                .await;
            let outcome = call_result(self.presented, outcome)?;
            Ok(outcome.with_presentation_subject(subject))
        })
    }
}

/// The tool result of one finished retrieval. A failure after the port's
/// bounded retries is an ordinary tool error; only cancellation is not.
fn call_result(
    presented: &str,
    outcome: Result<plakous_tools::RetrievalOutcome, plakous_tools::RetrievalError>,
) -> Result<ToolOutcome, ToolRuntimeError> {
    match outcome {
        Ok(outcome) => ToolOutcome::success_text(render_outcome(presented, &outcome)),
        Err(error) if error.kind() == RetrievalErrorKind::Cancelled => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Cancelled,
            error.message(),
        )),
        Err(error) => ToolOutcome::failure_text(format!("{presented} failed: {error}")),
    }
}

/// The host-only label of one call: the query, or the fetched host.
fn subject(request: &RetrievalRequest) -> String {
    match request {
        RetrievalRequest::WebSearch { query, .. } | RetrievalRequest::PostSearch { query, .. } => {
            format!("{query:?}")
        }
        RetrievalRequest::PageFetch { url, .. } => web_fetch::host(url).to_owned(),
    }
}

/// Every model-visible string the web pack owns.
static WEB_PROMPT_PINS: &[PromptPin] = &[
    PromptPin::new(
        "web.claude.web_search",
        PLAKOUS_WEB_CLAUDE_SEARCH_DESCRIPTION,
        "f0363806c67da72b61fbfde1ebf9f27ea3bed68d1bd5c6ab0c6d7260c964db6a",
    ),
    PromptPin::new(
        "web.claude.web_fetch",
        PLAKOUS_WEB_CLAUDE_FETCH_DESCRIPTION,
        "d5334d59c1a0a06abf1dd5d7f96e3e3193c5adff8bf47828526a618dccada4f8",
    ),
    PromptPin::new(
        "web.web_search",
        PLAKOUS_WEB_SEARCH_DESCRIPTION,
        "008d3f8dfc06aaff0670f4de69b6049ffe12d8aa44dcb2ecd2545e6375058dfd",
    ),
    PromptPin::new(
        "web.grok.x_search",
        PLAKOUS_WEB_X_SEARCH_DESCRIPTION,
        "130455172a0ca7b25c1bf7976cdba31df86b498f027143fad0a6cc578730872f",
    ),
    PromptPin::new(
        "web.untrusted_line",
        render::UNTRUSTED_LINE_TEMPLATE,
        "6430f61c5633b0659bab2873965d487bc4e8c7fed5d0013f99ffe969899c240c",
    ),
];

#[cfg(test)]
mod tests;
