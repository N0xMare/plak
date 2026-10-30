//! The optional tools a template names, and the deterministic plan they
//! resolve to at session bind.
//!
//! A template's `tools` list is parsed once into a [`TemplateToolSelection`].
//! Binding resolves it against the route and the MCP listings into a
//! crate-private `ToolPlan`, or refuses it with a `ToolPlanRefusal`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, OnceLock};

use plakous::model::{
    HostedRetrievalSupport, HostedRetrievalTable, HostedRetrievalTool, HostedRetrievalUnsupported,
    ToolName,
};
use plakous::subscriptions::NativeProvider;
use plakous::tools::web::{
    DEFAULT_WEB_SEARCH_MAX_USES, FetchUrlLimits, ImageGenFuture, ImageGenPort, ImageGenRequest,
    RetrievalError, RetrievalErrorKind, RetrievalFuture, RetrievalKind, RetrievalOptions,
    RetrievalPort, RetrievalRequest, WebTool,
};
use plakous::tools::{CodingPresentation, ExecutionOperationControl, McpServerName};

use crate::binding::{ApiTarget, PlatformProvider};

/// Maximum byte length of the MCP tool part of `mcp:<id>/<tool>`.
pub const MAX_MCP_TOOL_NAME_BYTES: usize = 128;

/// The closed set of optional tools a template enables by name.
///
/// Declaration order is plan order: the hosted-retrieval wrappers
/// `web_search, web_fetch, x_search`, then the Plak-run tools
/// `fetch_url, image_gen`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OptionalTool {
    /// Hosted web search through the route's provider.
    WebSearch,
    /// Hosted page fetch through the route's provider.
    WebFetch,
    /// Hosted X search through the route's provider.
    XSearch,
    /// Plak-run HTTP fetch.
    FetchUrl,
    /// Plak-run image generation.
    ImageGen,
}

impl OptionalTool {
    /// Every optional tool, in plan order.
    pub const ALL: [Self; 5] = [
        Self::WebSearch,
        Self::WebFetch,
        Self::XSearch,
        Self::FetchUrl,
        Self::ImageGen,
    ];

    /// The name a template lists.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
            Self::WebFetch => "web_fetch",
            Self::XSearch => "x_search",
            Self::FetchUrl => "fetch_url",
            Self::ImageGen => "image_gen",
        }
    }

    /// The local wrapper of the hosted retrieval, when this tool is one.
    #[must_use]
    pub const fn web_tool(self) -> Option<WebTool> {
        match self {
            Self::WebSearch => Some(WebTool::WebSearch),
            Self::WebFetch => Some(WebTool::WebFetch),
            Self::XSearch => Some(WebTool::XSearch),
            Self::FetchUrl | Self::ImageGen => None,
        }
    }

    /// The provider-hosted retrieval this tool wraps, when it wraps one.
    #[must_use]
    pub const fn hosted_retrieval(self) -> Option<HostedRetrievalTool> {
        match self {
            Self::WebSearch => Some(HostedRetrievalTool::WebSearch),
            Self::WebFetch => Some(HostedRetrievalTool::WebFetch),
            Self::XSearch => Some(HostedRetrievalTool::XSearch),
            Self::FetchUrl | Self::ImageGen => None,
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }
}

/// One validated entry of a template's `tools` list.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SelectedTool {
    /// A closed optional tool.
    Optional(OptionalTool),
    /// `mcp:<id>`: every allowlisted tool of that server.
    McpServer(McpServerName),
    /// `mcp:<id>/<tool>`: one tool, by its MCP name before sanitization.
    McpTool {
        /// The configured server id.
        server: McpServerName,
        /// The tool's MCP name.
        tool: String,
    },
}

impl SelectedTool {
    /// Parses one list entry.
    ///
    /// # Errors
    ///
    /// Refuses a name outside the closed set and a malformed `mcp:` name.
    pub fn parse(name: &str) -> Result<Self, ToolSelectionError> {
        if let Some(tool) = OptionalTool::parse(name) {
            return Ok(Self::Optional(tool));
        }
        let Some(mcp) = name.strip_prefix("mcp:") else {
            return Err(ToolSelectionError::Unknown {
                name: name.to_owned(),
            });
        };
        let (server, tool) = match mcp.split_once('/') {
            Some((server, tool)) => (server, Some(tool)),
            None => (mcp, None),
        };
        let server =
            McpServerName::new(server).map_err(|_| ToolSelectionError::InvalidMcpServer {
                name: name.to_owned(),
            })?;
        let Some(tool) = tool else {
            return Ok(Self::McpServer(server));
        };
        if tool.is_empty()
            || tool.len() > MAX_MCP_TOOL_NAME_BYTES
            || tool
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(ToolSelectionError::InvalidMcpTool {
                name: name.to_owned(),
            });
        }
        Ok(Self::McpTool {
            server,
            tool: tool.to_owned(),
        })
    }

    fn server(&self) -> Option<&McpServerName> {
        match self {
            Self::Optional(_) => None,
            Self::McpServer(server) | Self::McpTool { server, .. } => Some(server),
        }
    }
}

impl fmt::Display for SelectedTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Optional(tool) => formatter.write_str(tool.name()),
            Self::McpServer(server) => write!(formatter, "mcp:{server}"),
            Self::McpTool { server, tool } => write!(formatter, "mcp:{server}/{tool}"),
        }
    }
}

/// Why a template's `tools` list is refused at load.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolSelectionError {
    /// The name is neither a closed optional tool nor an `mcp:` name.
    Unknown {
        /// The name as written.
        name: String,
    },
    /// The same entry is listed twice.
    Duplicate {
        /// The name as written.
        name: String,
    },
    /// `mcp:<id>/<tool>` is listed beside `mcp:<id>`, which already
    /// includes it.
    CoveredByServer {
        /// The single-tool name as written.
        name: String,
        /// The server listed whole.
        server: McpServerName,
    },
    /// The server id breaks `[a-z][a-z0-9]{0,15}(_[a-z0-9]+)*`.
    InvalidMcpServer {
        /// The name as written.
        name: String,
    },
    /// The MCP tool part is empty, over 128 bytes, or has whitespace or
    /// control characters.
    InvalidMcpTool {
        /// The name as written.
        name: String,
    },
}

impl fmt::Display for ToolSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { name } => write!(
                formatter,
                "unknown tool {name:?}; expected one of web_search, web_fetch, x_search, \
                 fetch_url, image_gen, mcp:<id> or mcp:<id>/<tool>"
            ),
            Self::Duplicate { name } => write!(formatter, "tool {name:?} is listed twice"),
            Self::CoveredByServer { name, server } => write!(
                formatter,
                "tool {name:?} is already included by mcp:{server}; list one or the other"
            ),
            Self::InvalidMcpServer { name } => write!(
                formatter,
                "tool {name:?} names an invalid MCP server id; an id matches \
                 [a-z][a-z0-9]{{0,15}}(_[a-z0-9]+)* within 32 bytes"
            ),
            Self::InvalidMcpTool { name } => write!(
                formatter,
                "tool {name:?} names an invalid MCP tool; the tool part is 1 to 128 bytes \
                 without whitespace"
            ),
        }
    }
}

impl std::error::Error for ToolSelectionError {}

/// A template's parsed and validated `tools` list, in the order written.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TemplateToolSelection {
    entries: Vec<SelectedTool>,
}

impl TemplateToolSelection {
    /// Parses a template's `tools` list.
    ///
    /// # Errors
    ///
    /// Refuses the first unknown, malformed, duplicate or covered entry.
    pub fn parse<I>(names: I) -> Result<Self, ToolSelectionError>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        for name in names {
            let name = name.as_ref();
            let entry = SelectedTool::parse(name)?;
            if !seen.insert(entry.clone()) {
                return Err(ToolSelectionError::Duplicate {
                    name: name.to_owned(),
                });
            }
            entries.push(entry);
        }
        for entry in &entries {
            if let SelectedTool::McpTool { server, .. } = entry
                && seen.contains(&SelectedTool::McpServer(server.clone()))
            {
                return Err(ToolSelectionError::CoveredByServer {
                    name: entry.to_string(),
                    server: server.clone(),
                });
            }
        }
        Ok(Self { entries })
    }

    /// The entries in the order written.
    #[must_use]
    pub fn entries(&self) -> &[SelectedTool] {
        &self.entries
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The distinct MCP servers the list reaches, in id order.
    pub fn mcp_servers(&self) -> impl Iterator<Item = &McpServerName> {
        self.entries
            .iter()
            .filter_map(SelectedTool::server)
            .collect::<BTreeSet<_>>()
            .into_iter()
    }
}

/// One tool a bound plan appends after the core menu.
///
/// Variant then field order is plan order: optional tools in
/// [`OptionalTool`] order, then MCP tools by server id and exposed name.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum PlannedTool {
    /// A closed optional tool.
    Optional(OptionalTool),
    /// One MCP tool under its exposed `mcp__<server>__<tool>` name.
    Mcp {
        server: McpServerName,
        exposed: ToolName,
    },
}

/// A node's template selection with the canonical `[tools.*]` /
/// `[mcp_servers.*]` tables it reaches, frozen where the node's card is: the
/// epoch snapshot for a child, the session's bind for a root.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NodeToolSelection {
    /// The template's `tools` list.
    pub selection: TemplateToolSelection,
    /// Canonical tables keyed `tools.<name>` / `mcp_servers.<id>`.
    pub tool_sections: BTreeMap<String, String>,
    /// The MCP tools frozen with the selection, rebuilt without listing the
    /// server again; `None` lists each server at bind.
    pub mcp_tools: Option<Vec<plakous::mcp::McpToolRecord>>,
}

impl NodeToolSelection {
    /// Refuses an SDK selection before optional tools or MCP are bound.
    ///
    /// # Errors
    /// Returns the configured extension names and the key to change.
    pub fn admit_composition(
        &self,
        composition: crate::binding::CompositionSelection,
    ) -> crate::CliResult<()> {
        if composition == crate::binding::CompositionSelection::PySdk {
            crate::tools::admit_sdk_extensions(
                self.selection
                    .entries()
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            )?;
        }
        Ok(())
    }

    /// A selection that reaches no table.
    #[must_use]
    pub fn new(selection: TemplateToolSelection) -> Self {
        Self {
            selection,
            tool_sections: BTreeMap::new(),
            mcp_tools: None,
        }
    }
}

/// The optional tools a session appends after its core menu, in the one
/// order every bind of the same selection and listings produces.
///
/// Two seams fill it: hosted-retrieval wrappers through the route's
/// [`RetrievalBinding`] (registered by `WebToolPack` in `with_plan`), and MCP
/// tools through the registrations [`ToolPlan::with_mcp`] carries.
#[derive(Clone, Default)]
pub(crate) struct ToolPlan {
    tools: Vec<PlannedTool>,
    retrieval: Option<RetrievalBinding>,
    image_gen: Option<ImageGenBinding>,
    mcp_registrations: Vec<plakous::tools::ToolRegistration>,
    notices: Vec<String>,
    fetch_url_limits: FetchUrlLimits,
    retrieval_options: RetrievalOptions,
}

impl fmt::Debug for ToolPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolPlan")
            .field("tools", &self.tools)
            .field("retrieval", &self.retrieval)
            .field("image_gen", &self.image_gen.is_some())
            .field("notices", &self.notices)
            .field("fetch_url_limits", &self.fetch_url_limits)
            .field("retrieval_options", &self.retrieval_options)
            .finish_non_exhaustive()
    }
}

/// The vocabulary the hosted-retrieval wrappers wear and the route's port
/// that serves their side requests.
#[derive(Clone)]
pub(crate) struct RetrievalBinding {
    presentation: CodingPresentation,
    port: Arc<dyn RetrievalPort>,
    slot: Option<Arc<DeferredRetrievalPort>>,
}

/// A route's retrieval port known by its kinds at bind and connected once
/// the session's executor exists: binding precedes model construction on
/// the API routes. A call before the connection is an ordinary tool error.
pub(crate) struct DeferredRetrievalPort {
    kinds: Vec<RetrievalKind>,
    port: OnceLock<Arc<dyn RetrievalPort>>,
}

impl DeferredRetrievalPort {
    pub(crate) fn new(kinds: &[RetrievalKind]) -> Self {
        Self {
            kinds: kinds.to_vec(),
            port: OnceLock::new(),
        }
    }

    /// Connects the session's executor; it must serve every bound kind, and
    /// a slot connects once.
    pub(crate) fn connect(&self, port: Arc<dyn RetrievalPort>) -> Result<(), &'static str> {
        if !self.kinds.iter().all(|kind| port.kinds().contains(kind)) {
            return Err("the session's retrieval port does not serve the bound tools");
        }
        self.port
            .set(port)
            .map_err(|_| "the session's retrieval port is already connected")
    }
}

impl RetrievalPort for DeferredRetrievalPort {
    fn kinds(&self) -> &[RetrievalKind] {
        &self.kinds
    }

    fn retrieve(
        &self,
        request: RetrievalRequest,
        control: ExecutionOperationControl,
    ) -> RetrievalFuture {
        match self.port.get() {
            Some(port) => port.retrieve(request, control),
            None => Box::pin(async {
                Err(RetrievalError::new(
                    RetrievalErrorKind::Unavailable,
                    "hosted retrieval is not connected to this session's route",
                ))
            }),
        }
    }
}

/// Whether a route serves `image_gen`. Rows are release-owned and move to
/// [`ImageGenRow::Supported`] only with a live receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImageGenRow {
    Supported,
    /// The route's provider has no images endpoint.
    NotOffered,
}

/// The port `image_gen` calls and, when it was bound before the session's
/// executor existed, the deferred slot that executor connects.
#[derive(Clone)]
pub(crate) struct ImageGenBinding {
    port: Arc<dyn ImageGenPort>,
    slot: Option<Arc<DeferredImagePort>>,
}

impl ImageGenBinding {
    pub(crate) fn port(&self) -> Arc<dyn ImageGenPort> {
        Arc::clone(&self.port)
    }

    pub(crate) fn slot(&self) -> Option<Arc<DeferredImagePort>> {
        self.slot.clone()
    }
}

/// A route's image port bound at plan time and connected once the
/// session's executor exists. A call before the connection is an ordinary
/// tool error.
#[derive(Default)]
pub(crate) struct DeferredImagePort {
    port: OnceLock<Arc<dyn ImageGenPort>>,
}

impl DeferredImagePort {
    /// Connects the session's executor; a slot connects once.
    pub(crate) fn connect(&self, port: Arc<dyn ImageGenPort>) -> Result<(), &'static str> {
        self.port
            .set(port)
            .map_err(|_| "the session's image port is already connected")
    }
}

impl ImageGenPort for DeferredImagePort {
    fn max_edit_images(&self) -> usize {
        self.port.get().map_or(0, |port| port.max_edit_images())
    }

    fn takes_size(&self) -> bool {
        self.port.get().is_some_and(|port| port.takes_size())
    }

    fn generate(
        &self,
        request: ImageGenRequest,
        control: ExecutionOperationControl,
    ) -> ImageGenFuture {
        match self.port.get() {
            Some(port) => port.generate(request, control),
            None => Box::pin(async {
                Err(RetrievalError::new(
                    RetrievalErrorKind::Unavailable,
                    "image generation is not connected to this session's route",
                ))
            }),
        }
    }
}

impl RetrievalBinding {
    /// The deferred slot the session's executor connects to, when the port
    /// was bound before the executor existed.
    pub(crate) fn slot(&self) -> Option<Arc<DeferredRetrievalPort>> {
        self.slot.clone()
    }

    pub(crate) const fn presentation(&self) -> CodingPresentation {
        self.presentation
    }

    pub(crate) fn port(&self) -> Arc<dyn RetrievalPort> {
        Arc::clone(&self.port)
    }
}

impl fmt::Debug for RetrievalBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetrievalBinding")
            .field("presentation", &self.presentation)
            .field("kinds", &self.port.kinds())
            .finish_non_exhaustive()
    }
}

/// Two bindings are equal when they share one port.
impl PartialEq for RetrievalBinding {
    fn eq(&self, other: &Self) -> bool {
        self.presentation == other.presentation && Arc::ptr_eq(&self.port, &other.port)
    }
}

impl Eq for RetrievalBinding {}

impl ToolPlan {
    /// Orders the selection's optional tools and the MCP tools binding
    /// resolved for it, independent of list and listing order.
    pub(crate) fn new(
        selection: &TemplateToolSelection,
        mcp_tools: impl IntoIterator<Item = (McpServerName, ToolName)>,
    ) -> Self {
        let tools = selection
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                SelectedTool::Optional(tool) => Some(PlannedTool::Optional(*tool)),
                SelectedTool::McpServer(_) | SelectedTool::McpTool { .. } => None,
            })
            .chain(
                mcp_tools
                    .into_iter()
                    .map(|(server, exposed)| PlannedTool::Mcp { server, exposed }),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Self {
            tools,
            retrieval: None,
            image_gen: None,
            mcp_registrations: Vec::new(),
            notices: Vec::new(),
            fetch_url_limits: FetchUrlLimits::default(),
            retrieval_options: RetrievalOptions::default(),
        }
    }

    /// This plan with `selection`'s bound MCP tools added in plan order.
    pub(crate) fn with_mcp(
        self,
        selection: &TemplateToolSelection,
        mcp: crate::mcp_binding::BoundMcpTools,
    ) -> Self {
        Self {
            retrieval: self.retrieval,
            image_gen: self.image_gen,
            mcp_registrations: mcp.registrations,
            notices: mcp.notices,
            fetch_url_limits: self.fetch_url_limits,
            retrieval_options: self.retrieval_options,
            ..Self::new(selection, mcp.names)
        }
    }

    /// The registration of one planned MCP tool.
    pub(crate) fn mcp_registration(
        &self,
        exposed: &ToolName,
    ) -> Option<&plakous::tools::ToolRegistration> {
        self.mcp_registrations
            .iter()
            .find(|registration| registration.name() == exposed)
    }

    /// The planned tools in plan order.
    pub(crate) fn tools(&self) -> &[PlannedTool] {
        &self.tools
    }

    /// The port and vocabulary of the planned hosted-retrieval wrappers.
    pub(crate) const fn retrieval(&self) -> Option<&RetrievalBinding> {
        self.retrieval.as_ref()
    }

    /// The port a planned `image_gen` calls.
    pub(crate) const fn image_gen(&self) -> Option<&ImageGenBinding> {
        self.image_gen.as_ref()
    }

    /// The bounds a planned `fetch_url` runs under.
    pub(crate) const fn fetch_url_limits(&self) -> FetchUrlLimits {
        self.fetch_url_limits
    }

    /// This plan with `fetch_url` bound under `limits`, the node's frozen
    /// `[tools.fetch_url]` table.
    pub(crate) fn with_fetch_url_limits(mut self, limits: FetchUrlLimits) -> Self {
        self.fetch_url_limits = limits;
        self
    }

    /// The options the planned retrieval wrappers fix on every call.
    pub(crate) const fn retrieval_options(&self) -> &RetrievalOptions {
        &self.retrieval_options
    }

    /// This plan with its retrieval wrappers under `options`, the node's
    /// frozen `[tools.web_search]` and `[tools.x_search]` tables.
    ///
    /// # Errors
    ///
    /// Refuses an option a bound wrapper's route has no provider parameter
    /// for, as [`check_retrieval_options`] does.
    pub(crate) fn with_retrieval_options(
        mut self,
        selection: &TemplateToolSelection,
        route: ToolRoute,
        route_label: &str,
        options: RetrievalOptions,
    ) -> Result<Self, ToolPlanRefusal> {
        check_retrieval_options(selection, route, route_label, &options)?;
        self.retrieval_options = options;
        Ok(self)
    }
}

/// Refuses a `[tools.web_search]` option the route's provider has no
/// parameter for, rather than dropping it. Every route filters allowed
/// domains; excluded domains need the provider's own exclusion list; a
/// search count other than the default needs the provider's own limit, or a
/// side request that runs exactly one search (Codex, OpenRouter).
pub(crate) fn check_retrieval_options(
    selection: &TemplateToolSelection,
    route: ToolRoute,
    route_label: &str,
    options: &RetrievalOptions,
) -> Result<(), ToolPlanRefusal> {
    let tool = SelectedTool::Optional(OptionalTool::WebSearch);
    if !selection.entries().contains(&tool) {
        return Ok(());
    }
    let unserved = match route {
        ToolRoute::Codex | ToolRoute::OpenAi if !options.blocked_domains.is_empty() => Some((
            "blocked_domains",
            "use allowed_domains instead of blocked_domains in [tools.web_search]",
        )),
        ToolRoute::OpenAi | ToolRoute::Grok | ToolRoute::XaiApi
            if options.web_search_max_uses != DEFAULT_WEB_SEARCH_MAX_USES =>
        {
            Some(("max_uses", "remove max_uses from [tools.web_search]"))
        }
        _ => None,
    };
    match unserved {
        Some((option, remedy)) => Err(ToolPlanRefusal {
            tool,
            reason: RefusalReason::Option {
                route: route_label.to_owned(),
                option,
                remedy,
            },
        }),
        None => Ok(()),
    }
}

/// The route a session binds, as far as the tool plan asks about it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolRoute {
    Codex,
    Grok,
    Claude,
    OpenAi,
    Anthropic,
    XaiApi,
    OpenRouter,
}

impl ToolRoute {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 7] = [
        Self::Codex,
        Self::Grok,
        Self::Claude,
        Self::OpenAi,
        Self::Anthropic,
        Self::XaiApi,
        Self::OpenRouter,
    ];

    pub(crate) fn native(provider: NativeProvider) -> Option<Self> {
        match provider {
            NativeProvider::Codex => Some(Self::Codex),
            NativeProvider::Xai => Some(Self::Grok),
            NativeProvider::Claude => Some(Self::Claude),
            _ => None,
        }
    }

    pub(crate) const fn api(target: ApiTarget) -> Self {
        match target {
            ApiTarget::OpenRouter { .. } => Self::OpenRouter,
            ApiTarget::Platform { provider } => match provider {
                PlatformProvider::OpenAi => Self::OpenAi,
                PlatformProvider::Anthropic => Self::Anthropic,
                PlatformProvider::Xai => Self::XaiApi,
            },
        }
    }

    /// The retrievals this build's port for the route serves.
    pub(crate) const fn retrieval_kinds(self) -> &'static [RetrievalKind] {
        match self {
            Self::Claude | Self::Anthropic => &[RetrievalKind::WebSearch, RetrievalKind::PageFetch],
            Self::Codex | Self::OpenRouter | Self::OpenAi => &[RetrievalKind::WebSearch],
            Self::Grok | Self::XaiApi => &[RetrievalKind::WebSearch, RetrievalKind::PostSearch],
        }
    }

    /// Whether the route serves `image_gen`.
    pub(crate) const fn image_gen(self) -> ImageGenRow {
        match self {
            Self::Codex | Self::Grok | Self::OpenAi | Self::XaiApi => ImageGenRow::Supported,
            Self::Claude | Self::Anthropic | Self::OpenRouter => ImageGenRow::NotOffered,
        }
    }

    /// Whether the route can carry an image port: the API routes' executors
    /// and the subscription routes' resolved models. A route whose port is
    /// not built yet connects none, so its row must stay unqualified.
    pub(crate) const fn has_image_port(self) -> bool {
        matches!(self, Self::Codex | Self::Grok | Self::OpenAi | Self::XaiApi)
    }

    /// The tool vocabulary the route's coding presentation uses.
    const fn presentation(self) -> CodingPresentation {
        match self {
            Self::Claude | Self::Anthropic => CodingPresentation::Claude,
            Self::Codex | Self::OpenAi => CodingPresentation::Codex,
            Self::Grok | Self::XaiApi => CodingPresentation::Grok,
            Self::OpenRouter => CodingPresentation::Pi,
        }
    }

    /// The provider crate's release-owned table for this route.
    fn hosted_retrieval(self) -> HostedRetrievalTable {
        let route = match self {
            Self::Codex => &crate::capabilities::CODEX,
            Self::OpenAi => &crate::capabilities::OPENAI,
            Self::Grok => &crate::capabilities::GROK,
            Self::XaiApi => &crate::capabilities::XAI,
            Self::Claude => &crate::capabilities::CLAUDE,
            Self::Anthropic => &crate::capabilities::ANTHROPIC,
            Self::OpenRouter => &crate::capabilities::OPENROUTER,
        };
        (route.hosted_retrieval)().unwrap_or(HostedRetrievalTable::new(
            HostedRetrievalSupport::NOT_OFFERED,
            HostedRetrievalSupport::NOT_OFFERED,
            HostedRetrievalSupport::NOT_OFFERED,
        ))
    }
}

/// Resolves a template's selection on the route a session binds.
///
/// Every selected tool must be served, or binding refuses; nothing is
/// silently left out. A hosted retrieval tool binds its local wrapper only
/// where the route's release-owned table qualifies it and the route has a
/// retrieval port serving it; that port is a deferred slot the session's
/// executor connects once it exists. `fetch_url` runs in Plak and binds on
/// every route. `image_gen` binds where the route's row qualifies it and
/// the route has an image port, on a deferred slot as well. MCP entries
/// pass through: `crate::mcp_binding::bind_mcp_tools` binds them.
///
/// # Errors
///
/// Refuses the first selected tool, in list order, the route cannot serve.
pub(crate) fn bind_tool_plan(
    selection: &TemplateToolSelection,
    route: ToolRoute,
    route_label: &str,
) -> Result<ToolPlan, ToolPlanRefusal> {
    bind_tool_plan_on(
        selection,
        route,
        route.hosted_retrieval(),
        route.image_gen(),
        route_label,
    )
}

/// [`bind_tool_plan`] over an explicit support table, with the route's
/// deferred port.
pub(crate) fn bind_tool_plan_on(
    selection: &TemplateToolSelection,
    route: ToolRoute,
    table: HostedRetrievalTable,
    image_gen: ImageGenRow,
    route_label: &str,
) -> Result<ToolPlan, ToolPlanRefusal> {
    let kinds = route.retrieval_kinds();
    let slot = (!kinds.is_empty()).then(|| Arc::new(DeferredRetrievalPort::new(kinds)));
    let image_slot = route
        .has_image_port()
        .then(|| Arc::new(DeferredImagePort::default()));
    let mut plan = bind_tool_plan_with(
        selection,
        table,
        route.presentation(),
        route_label,
        slot.clone().map(|slot| slot as Arc<dyn RetrievalPort>),
        (
            image_gen,
            image_slot.clone().map(|slot| slot as Arc<dyn ImageGenPort>),
        ),
    )?;
    if let Some(binding) = plan.retrieval.as_mut() {
        binding.slot = slot;
    }
    if let Some(binding) = plan.image_gen.as_mut() {
        binding.slot = image_slot;
    }
    Ok(plan)
}

/// A route without `image_gen`, for binds that only ask about other tools.
#[cfg(test)]
pub(crate) const NO_IMAGE_GEN: (ImageGenRow, Option<Arc<dyn ImageGenPort>>) =
    (ImageGenRow::NotOffered, None);

/// [`bind_tool_plan`] over an explicit support table and retrieval port.
pub(crate) fn bind_tool_plan_with(
    selection: &TemplateToolSelection,
    table: HostedRetrievalTable,
    presentation: CodingPresentation,
    route_label: &str,
    port: Option<Arc<dyn RetrievalPort>>,
    (image_row, image_port): (ImageGenRow, Option<Arc<dyn ImageGenPort>>),
) -> Result<ToolPlan, ToolPlanRefusal> {
    let mut wrappers = false;
    let mut image_gen = None;
    for entry in selection.entries() {
        let served = match entry {
            SelectedTool::Optional(OptionalTool::FetchUrl) => continue,
            SelectedTool::Optional(OptionalTool::ImageGen) => {
                let reason = match (image_row, &image_port) {
                    (ImageGenRow::Supported, Some(port)) => {
                        image_gen = Some(ImageGenBinding {
                            port: Arc::clone(port),
                            slot: None,
                        });
                        continue;
                    }
                    (ImageGenRow::Supported, None) => RefusalReason::Unbuilt,
                    (ImageGenRow::NotOffered, _) => RefusalReason::Unoffered {
                        route: route_label.to_owned(),
                    },
                };
                return Err(ToolPlanRefusal {
                    tool: entry.clone(),
                    reason,
                });
            }
            SelectedTool::Optional(tool) => match (tool.hosted_retrieval(), tool.web_tool()) {
                (Some(hosted), Some(web))
                    if table.support(hosted) == HostedRetrievalSupport::Supported =>
                {
                    port.as_ref().is_some_and(|port| {
                        port.kinds().contains(&web.kind())
                            && web.presented_name(presentation).is_some()
                    })
                }
                _ => false,
            },
            // MCP entries bind through `crate::mcp_binding`.
            SelectedTool::McpServer(_) | SelectedTool::McpTool { .. } => continue,
        };
        if !served {
            return Err(refusal(entry, table, route_label));
        }
        wrappers = true;
    }
    let mut plan = ToolPlan::new(selection, []);
    plan.image_gen = image_gen;
    if wrappers {
        plan.retrieval = port.map(|port| RetrievalBinding {
            presentation,
            port,
            slot: None,
        });
    }
    Ok(plan)
}

fn refusal(
    entry: &SelectedTool,
    table: HostedRetrievalTable,
    route_label: &str,
) -> ToolPlanRefusal {
    let reason = match entry {
        SelectedTool::Optional(tool) => {
            match tool.hosted_retrieval().map(|tool| table.support(tool)) {
                Some(HostedRetrievalSupport::Unsupported {
                    reason: HostedRetrievalUnsupported::NotOffered,
                }) => RefusalReason::Unoffered {
                    route: route_label.to_owned(),
                },
                Some(HostedRetrievalSupport::Unsupported {
                    reason: HostedRetrievalUnsupported::NotQualified,
                }) => RefusalReason::Unqualified {
                    route: route_label.to_owned(),
                },
                // A qualified route still needs a port that issues the side
                // request in this vocabulary.
                Some(HostedRetrievalSupport::Supported) | None => RefusalReason::Unbuilt,
            }
        }
        SelectedTool::McpServer(_) | SelectedTool::McpTool { .. } => RefusalReason::Unbuilt,
    };
    ToolPlanRefusal {
        tool: entry.clone(),
        reason,
    }
}

/// A selected tool the bound route cannot serve; it fails session creation
/// and the spawn of that template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolPlanRefusal {
    tool: SelectedTool,
    reason: RefusalReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RefusalReason {
    /// The route's provider offers no such tool.
    Unoffered { route: String },
    /// The provider offers it; no release has qualified it on this route.
    Unqualified { route: String },
    /// This build has no implementation of the tool.
    Unbuilt,
    /// The route's provider has no parameter for one frozen option.
    Option {
        route: String,
        option: &'static str,
        remedy: &'static str,
    },
}

#[cfg(test)]
impl ToolPlanRefusal {
    pub(crate) const fn tool(&self) -> &SelectedTool {
        &self.tool
    }
}

impl ToolPlanRefusal {
    /// The line a rebind refuses with: the session keeps its tools across a
    /// rebind, so the remedy is a new session rather than a template edit.
    pub(crate) fn rebind_line(&self) -> String {
        let tool = &self.tool;
        let reason = match &self.reason {
            RefusalReason::Unoffered { route } => format!("{tool} is not available on {route}"),
            RefusalReason::Unqualified { route } => {
                format!("{tool} is not yet qualified on {route}")
            }
            RefusalReason::Unbuilt => format!("{tool} is not available in this build"),
            RefusalReason::Option { route, option, .. } => {
                format!("[tools.{tool}] {option} is not available on {route}")
            }
        };
        format!(
            "this session keeps its tools across a model change, and {reason}; start a fresh conversation on that model"
        )
    }
}

/// Where a tool the route does not offer can run instead.
fn not_offered_remedy(tool: &SelectedTool) -> &'static str {
    match tool {
        SelectedTool::Optional(OptionalTool::WebFetch) => {
            "use fetch_url (Plak-run) or a claude/anthropic template"
        }
        SelectedTool::Optional(OptionalTool::XSearch) => {
            "put x_search on a grok or xai-api template and spawn it"
        }
        SelectedTool::Optional(OptionalTool::ImageGen) => {
            "put image_gen on a codex, grok, openai or xai-api template and spawn it"
        }
        _ => "put it on a template whose route offers it and spawn it",
    }
}

impl fmt::Display for ToolPlanRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tool = &self.tool;
        match &self.reason {
            RefusalReason::Unoffered { route } => write!(
                formatter,
                "{tool} is not available on {route}; remedy: {}",
                not_offered_remedy(tool)
            ),
            RefusalReason::Unqualified { route } => write!(
                formatter,
                "{tool} is not yet qualified on {route}; remedy: remove {tool} from the \
                 template's tools until a release qualifies it there"
            ),
            RefusalReason::Unbuilt => write!(
                formatter,
                "{tool} is not available in this build; remedy: remove {tool} from the \
                 template's tools"
            ),
            RefusalReason::Option {
                route,
                option,
                remedy,
            } => write!(
                formatter,
                "[tools.{tool}] {option} is not available on {route}; remedy: {remedy}, or put \
                 {tool} on a template whose route serves it"
            ),
        }
    }
}

impl std::error::Error for ToolPlanRefusal {}

#[cfg(test)]
mod tests;
