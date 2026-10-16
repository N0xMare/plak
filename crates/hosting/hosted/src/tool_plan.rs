//! The optional tools a template names, and the deterministic plan they
//! resolve to at session bind.
//!
//! A template's `tools` list is parsed once into a [`TemplateToolSelection`].
//! Binding resolves it against the route and the MCP listings into a
//! [`ToolPlan`], or refuses it with a [`ToolPlanRefusal`].

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use plakous::model::{
    HostedRetrievalSupport, HostedRetrievalTable, HostedRetrievalTool, HostedRetrievalUnsupported,
    ToolName,
};
use plakous::subscriptions::NativeProvider;
use plakous::tools::web::{RetrievalPort, WebTool};
use plakous::tools::{CodingPresentation, McpServerName};

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

    /// Whether the route's provider executes the retrieval (inside a side
    /// request a local wrapper issues) rather than Plak itself.
    #[must_use]
    pub const fn is_hosted_retrieval(self) -> bool {
        self.hosted_retrieval().is_some()
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

/// The optional tools a session appends after its core menu, in the one
/// order every bind of the same selection and listings produces.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ToolPlan {
    tools: Vec<PlannedTool>,
    retrieval: Option<RetrievalBinding>,
}

/// The vocabulary the hosted-retrieval wrappers wear and the route's port
/// that serves their side requests.
#[derive(Clone)]
pub(crate) struct RetrievalBinding {
    presentation: CodingPresentation,
    port: Arc<dyn RetrievalPort>,
}

impl RetrievalBinding {
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
        }
    }

    /// The planned tools in plan order.
    pub(crate) fn tools(&self) -> &[PlannedTool] {
        &self.tools
    }

    /// The port and vocabulary of the planned hosted-retrieval wrappers.
    pub(crate) const fn retrieval(&self) -> Option<&RetrievalBinding> {
        self.retrieval.as_ref()
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
    const fn hosted_retrieval(self) -> HostedRetrievalTable {
        match self {
            Self::Codex => plakous::openai::CODEX_HOSTED_RETRIEVAL,
            Self::OpenAi => plakous::openai::OPENAI_API_HOSTED_RETRIEVAL,
            Self::Grok => plakous::xai::SUPERGROK_HOSTED_RETRIEVAL,
            Self::XaiApi => plakous::xai::XAI_API_HOSTED_RETRIEVAL,
            Self::Claude => plakous::anthropic::CLAUDE_OAUTH_HOSTED_RETRIEVAL,
            Self::Anthropic => plakous::anthropic::ANTHROPIC_API_HOSTED_RETRIEVAL,
            Self::OpenRouter => plakous::openrouter::OPENROUTER_HOSTED_RETRIEVAL,
        }
    }
}

/// Resolves a template's selection on the route a session binds.
///
/// Every selected tool must be served, or binding refuses; nothing is
/// silently left out. A hosted retrieval tool binds its local wrapper only
/// where the route's release-owned table qualifies it and the route supplies
/// a retrieval port serving it; this build supplies no port, so the table
/// refuses it or it is refused as not built in. Every Plak-run and MCP tool
/// is refused as not built in. Only the empty selection binds.
///
/// # Errors
///
/// Refuses the first selected tool, in list order, the route cannot serve.
pub(crate) fn bind_tool_plan(
    selection: &TemplateToolSelection,
    route: ToolRoute,
    route_label: &str,
) -> Result<ToolPlan, ToolPlanRefusal> {
    bind_tool_plan_with(
        selection,
        route.hosted_retrieval(),
        route.presentation(),
        route_label,
        None,
    )
}

/// [`bind_tool_plan`] over an explicit support table and retrieval port.
pub(crate) fn bind_tool_plan_with(
    selection: &TemplateToolSelection,
    table: HostedRetrievalTable,
    presentation: CodingPresentation,
    route_label: &str,
    port: Option<Arc<dyn RetrievalPort>>,
) -> Result<ToolPlan, ToolPlanRefusal> {
    let mut wrappers = false;
    for entry in selection.entries() {
        let served = match entry {
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
            SelectedTool::McpServer(_) | SelectedTool::McpTool { .. } => false,
        };
        if !served {
            return Err(refusal(entry, table, route_label));
        }
        wrappers = true;
    }
    let mut plan = ToolPlan::new(selection, []);
    if wrappers {
        plan.retrieval = port.map(|port| RetrievalBinding { presentation, port });
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
}

#[cfg(test)]
impl ToolPlanRefusal {
    pub(crate) const fn tool(&self) -> &SelectedTool {
        &self.tool
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
        }
    }
}

impl std::error::Error for ToolPlanRefusal {}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn server(id: &str) -> TestResult<McpServerName> {
        Ok(McpServerName::new(id)?)
    }

    #[test]
    fn selection_parse_table() -> TestResult {
        let accepted: [(&str, SelectedTool); 8] = [
            (
                "web_search",
                SelectedTool::Optional(OptionalTool::WebSearch),
            ),
            ("web_fetch", SelectedTool::Optional(OptionalTool::WebFetch)),
            ("x_search", SelectedTool::Optional(OptionalTool::XSearch)),
            ("fetch_url", SelectedTool::Optional(OptionalTool::FetchUrl)),
            ("image_gen", SelectedTool::Optional(OptionalTool::ImageGen)),
            ("mcp:github", SelectedTool::McpServer(server("github")?)),
            (
                "mcp:linear/list_issues",
                SelectedTool::McpTool {
                    server: server("linear")?,
                    tool: "list_issues".to_owned(),
                },
            ),
            (
                "mcp:my_srv/a/b.c-d",
                SelectedTool::McpTool {
                    server: server("my_srv")?,
                    tool: "a/b.c-d".to_owned(),
                },
            ),
        ];
        for (name, expected) in accepted {
            let parsed = SelectedTool::parse(name)?;
            assert_eq!(parsed, expected, "{name}");
            assert_eq!(parsed.to_string(), name, "round trip {name}");
        }

        let unknown = |name: &str| ToolSelectionError::Unknown {
            name: name.to_owned(),
        };
        let bad_server = |name: &str| ToolSelectionError::InvalidMcpServer {
            name: name.to_owned(),
        };
        let bad_tool = |name: &str| ToolSelectionError::InvalidMcpTool {
            name: name.to_owned(),
        };
        let long_tool = format!("mcp:github/{}", "t".repeat(MAX_MCP_TOOL_NAME_BYTES + 1));
        let refused = [
            ("", unknown("")),
            ("WEB_SEARCH", unknown("WEB_SEARCH")),
            ("websearch", unknown("websearch")),
            ("code_execution", unknown("code_execution")),
            ("mcp", unknown("mcp")),
            ("mcp:", bad_server("mcp:")),
            ("mcp:GitHub", bad_server("mcp:GitHub")),
            ("mcp:git__hub", bad_server("mcp:git__hub")),
            ("mcp:git-hub", bad_server("mcp:git-hub")),
            ("mcp:/tool", bad_server("mcp:/tool")),
            ("mcp:github/", bad_tool("mcp:github/")),
            ("mcp:github/a b", bad_tool("mcp:github/a b")),
            ("mcp:github/a\nb", bad_tool("mcp:github/a\nb")),
            (long_tool.as_str(), bad_tool(&long_tool)),
        ];
        for (name, expected) in refused {
            assert_eq!(SelectedTool::parse(name), Err(expected), "{name:?}");
        }
        let at_limit = format!("mcp:github/{}", "t".repeat(MAX_MCP_TOOL_NAME_BYTES));
        assert!(SelectedTool::parse(&at_limit).is_ok());
        Ok(())
    }

    #[test]
    fn selection_refuses_duplicates_and_covered_tools() -> TestResult {
        let parsed = TemplateToolSelection::parse([
            "mcp:linear/list_issues",
            "web_search",
            "mcp:github",
            "mcp:linear/create_issue",
        ])?;
        assert_eq!(parsed.entries().len(), 4);
        assert_eq!(
            parsed
                .mcp_servers()
                .map(McpServerName::as_str)
                .collect::<Vec<_>>(),
            ["github", "linear"]
        );
        assert!(TemplateToolSelection::parse(Vec::<String>::new())?.is_empty());

        assert_eq!(
            TemplateToolSelection::parse(["web_search", "fetch_url", "web_search"]),
            Err(ToolSelectionError::Duplicate {
                name: "web_search".to_owned()
            })
        );
        assert_eq!(
            TemplateToolSelection::parse(["mcp:github/a", "mcp:github/a"]),
            Err(ToolSelectionError::Duplicate {
                name: "mcp:github/a".to_owned()
            })
        );
        for names in [
            ["mcp:github/create_issue", "mcp:github"],
            ["mcp:github", "mcp:github/create_issue"],
        ] {
            assert_eq!(
                TemplateToolSelection::parse(names),
                Err(ToolSelectionError::CoveredByServer {
                    name: "mcp:github/create_issue".to_owned(),
                    server: server("github")?,
                })
            );
        }
        Ok(())
    }

    #[test]
    fn selection_error_text_pins() -> TestResult {
        let text = |names: &[&str]| {
            TemplateToolSelection::parse(names)
                .err()
                .map(|error| error.to_string())
        };
        assert_eq!(
            text(&["web-search"]).as_deref(),
            Some(
                "unknown tool \"web-search\"; expected one of web_search, web_fetch, x_search, \
                 fetch_url, image_gen, mcp:<id> or mcp:<id>/<tool>"
            )
        );
        assert_eq!(
            text(&["x_search", "x_search"]).as_deref(),
            Some("tool \"x_search\" is listed twice")
        );
        assert_eq!(
            text(&["mcp:github", "mcp:github/create_issue"]).as_deref(),
            Some(
                "tool \"mcp:github/create_issue\" is already included by mcp:github; \
                 list one or the other"
            )
        );
        assert_eq!(
            text(&["mcp:Git"]).as_deref(),
            Some(
                "tool \"mcp:Git\" names an invalid MCP server id; an id matches \
                 [a-z][a-z0-9]{0,15}(_[a-z0-9]+)* within 32 bytes"
            )
        );
        assert_eq!(
            text(&["mcp:git/"]).as_deref(),
            Some(
                "tool \"mcp:git/\" names an invalid MCP tool; the tool part is 1 to 128 bytes \
                 without whitespace"
            )
        );
        Ok(())
    }

    #[test]
    fn plan_order_is_fixed_and_independent_of_list_and_listing_order() -> TestResult {
        let exposed = |name: &str| ToolName::new(name);
        let listing = vec![
            (server("linear")?, exposed("mcp__linear__list_issues")?),
            (
                server("github")?,
                exposed("mcp__github__get_file_contents")?,
            ),
            (server("github")?, exposed("mcp__github__create_issue")?),
        ];
        let forward = ToolPlan::new(
            &TemplateToolSelection::parse([
                "image_gen",
                "mcp:linear",
                "x_search",
                "fetch_url",
                "mcp:github",
                "web_fetch",
                "web_search",
            ])?,
            listing.clone(),
        );
        let reverse = ToolPlan::new(
            &TemplateToolSelection::parse([
                "web_search",
                "mcp:github",
                "web_fetch",
                "fetch_url",
                "mcp:linear",
                "x_search",
                "image_gen",
            ])?,
            listing.into_iter().rev(),
        );
        assert_eq!(forward, reverse);
        assert_eq!(
            forward.tools(),
            [
                PlannedTool::Optional(OptionalTool::WebSearch),
                PlannedTool::Optional(OptionalTool::WebFetch),
                PlannedTool::Optional(OptionalTool::XSearch),
                PlannedTool::Optional(OptionalTool::FetchUrl),
                PlannedTool::Optional(OptionalTool::ImageGen),
                PlannedTool::Mcp {
                    server: server("github")?,
                    exposed: exposed("mcp__github__create_issue")?,
                },
                PlannedTool::Mcp {
                    server: server("github")?,
                    exposed: exposed("mcp__github__get_file_contents")?,
                },
                PlannedTool::Mcp {
                    server: server("linear")?,
                    exposed: exposed("mcp__linear__list_issues")?,
                },
            ]
        );
        assert!(
            OptionalTool::ALL
                .iter()
                .take(3)
                .all(|tool| tool.is_hosted_retrieval())
                && !OptionalTool::FetchUrl.is_hosted_retrieval()
                && !OptionalTool::ImageGen.is_hosted_retrieval()
        );
        Ok(())
    }

    fn route_label(route: ToolRoute) -> &'static str {
        match route {
            ToolRoute::Codex => "codex",
            ToolRoute::Grok => "grok",
            ToolRoute::Claude => "claude",
            ToolRoute::OpenAi => "openai",
            ToolRoute::Anthropic => "anthropic",
            ToolRoute::XaiApi => "xai-api",
            ToolRoute::OpenRouter => "openrouter",
        }
    }

    fn refusal_text(selection: &[&str], route: ToolRoute) -> TestResult<String> {
        let selection = TemplateToolSelection::parse(selection)?;
        match bind_tool_plan(&selection, route, route_label(route)) {
            Ok(plan) => Err(format!("{route:?} bound {plan:?}").into()),
            Err(refusal) => Ok(refusal.to_string()),
        }
    }

    #[test]
    fn every_hosted_retrieval_row_starts_unsupported() {
        for route in ToolRoute::ALL {
            for tool in OptionalTool::ALL
                .iter()
                .filter_map(|tool| tool.hosted_retrieval())
            {
                assert_ne!(
                    route.hosted_retrieval().support(tool),
                    HostedRetrievalSupport::Supported,
                    "{route:?} {tool:?} moves to Supported only with a live qualification"
                );
            }
        }
    }

    #[test]
    fn hosted_retrieval_refusal_text_per_route() -> TestResult {
        let offered = |tool: &str, route: &str| {
            format!(
                "{tool} is not yet qualified on {route}; remedy: remove {tool} from the \
                 template's tools until a release qualifies it there"
            )
        };
        let web_fetch_elsewhere = |route: &str| {
            format!(
                "web_fetch is not available on {route}; \
                 remedy: use fetch_url (Plak-run) or a claude/anthropic template"
            )
        };
        let x_search_elsewhere = |route: &str| {
            format!(
                "x_search is not available on {route}; \
                 remedy: put x_search on a grok or xai-api template and spawn it"
            )
        };
        let expected = [
            (ToolRoute::Codex, "web_fetch", web_fetch_elsewhere("codex")),
            (ToolRoute::Codex, "x_search", x_search_elsewhere("codex")),
            (ToolRoute::Grok, "web_fetch", web_fetch_elsewhere("grok")),
            (ToolRoute::Grok, "x_search", offered("x_search", "grok")),
            (
                ToolRoute::Claude,
                "web_fetch",
                offered("web_fetch", "claude"),
            ),
            (ToolRoute::Claude, "x_search", x_search_elsewhere("claude")),
            (
                ToolRoute::OpenAi,
                "web_fetch",
                web_fetch_elsewhere("openai"),
            ),
            (ToolRoute::OpenAi, "x_search", x_search_elsewhere("openai")),
            (
                ToolRoute::Anthropic,
                "web_fetch",
                offered("web_fetch", "anthropic"),
            ),
            (
                ToolRoute::Anthropic,
                "x_search",
                x_search_elsewhere("anthropic"),
            ),
            (
                ToolRoute::XaiApi,
                "web_fetch",
                web_fetch_elsewhere("xai-api"),
            ),
            (
                ToolRoute::XaiApi,
                "x_search",
                offered("x_search", "xai-api"),
            ),
            (
                ToolRoute::OpenRouter,
                "web_fetch",
                web_fetch_elsewhere("openrouter"),
            ),
            (
                ToolRoute::OpenRouter,
                "x_search",
                x_search_elsewhere("openrouter"),
            ),
        ];
        for route in ToolRoute::ALL {
            assert_eq!(
                refusal_text(&["web_search"], route)?,
                offered("web_search", route_label(route)),
                "{route:?}"
            );
        }
        for (route, tool, text) in expected {
            assert_eq!(refusal_text(&[tool], route)?, text, "{route:?} {tool}");
        }
        Ok(())
    }

    #[test]
    fn plak_run_and_mcp_tools_are_refused_as_not_built_in() -> TestResult {
        for route in ToolRoute::ALL {
            for tool in [
                "fetch_url",
                "image_gen",
                "mcp:github",
                "mcp:linear/list_issues",
            ] {
                assert_eq!(
                    refusal_text(&[tool], route)?,
                    format!(
                        "{tool} is not available in this build; remedy: remove {tool} from \
                         the template's tools"
                    )
                );
            }
        }
        let qualified = HostedRetrievalTable::new(
            HostedRetrievalSupport::Supported,
            HostedRetrievalSupport::Supported,
            HostedRetrievalSupport::Supported,
        );
        assert_eq!(
            refusal(&SelectedTool::parse("web_search")?, qualified, "grok").to_string(),
            "web_search is not available in this build; remedy: remove web_search from \
             the template's tools"
        );
        Ok(())
    }

    struct FakePort(Vec<plakous::tools::web::RetrievalKind>);

    impl RetrievalPort for FakePort {
        fn kinds(&self) -> &[plakous::tools::web::RetrievalKind] {
            &self.0
        }

        fn retrieve(
            &self,
            _: plakous::tools::web::RetrievalRequest,
            _: plakous::tools::ExecutionOperationControl,
        ) -> plakous::tools::web::RetrievalFuture {
            Box::pin(async { Ok(plakous::tools::web::RetrievalOutcome::default()) })
        }
    }

    fn search_and_fetch_port() -> Arc<dyn RetrievalPort> {
        Arc::new(FakePort(vec![
            plakous::tools::web::RetrievalKind::WebSearch,
            plakous::tools::web::RetrievalKind::PageFetch,
        ]))
    }

    #[test]
    fn a_qualified_row_with_a_port_binds_the_wrapper_pack() -> TestResult {
        let qualified = HostedRetrievalTable::new(
            HostedRetrievalSupport::Supported,
            HostedRetrievalSupport::Supported,
            HostedRetrievalSupport::Unsupported {
                reason: HostedRetrievalUnsupported::NotOffered,
            },
        );
        let selection = TemplateToolSelection::parse(["web_fetch", "web_search"])?;
        let port = search_and_fetch_port();
        let plan = bind_tool_plan_with(
            &selection,
            qualified,
            CodingPresentation::Claude,
            "claude",
            Some(Arc::clone(&port)),
        )?;
        assert_eq!(
            plan.tools(),
            [
                PlannedTool::Optional(OptionalTool::WebSearch),
                PlannedTool::Optional(OptionalTool::WebFetch)
            ]
        );
        let extensions = crate::tools::ProductToolExtensions::empty()
            .with_plan(&plan)
            .map_err(|error| error.to_string())?;
        assert_eq!(
            extensions.names().map(ToolName::as_str).collect::<Vec<_>>(),
            ["WebSearch", "WebFetch"]
        );

        // The same port on an unqualified row, or a qualified row without a
        // port, refuses with the route text.
        let unqualified = bind_tool_plan_with(
            &TemplateToolSelection::parse(["x_search"])?,
            qualified,
            CodingPresentation::Grok,
            "grok",
            Some(port),
        )
        .err()
        .ok_or("an unoffered tool bound")?;
        assert_eq!(
            unqualified.to_string(),
            "x_search is not available on grok; remedy: put x_search on a grok or xai-api \
             template and spawn it"
        );
        let portless = bind_tool_plan_with(
            &selection,
            qualified,
            CodingPresentation::Claude,
            "claude",
            None,
        )
        .err()
        .ok_or("a portless tool bound")?;
        assert_eq!(
            portless.to_string(),
            "web_fetch is not available in this build; remedy: remove web_fetch from the \
             template's tools"
        );
        Ok(())
    }

    #[test]
    fn a_port_that_does_not_serve_the_kind_refuses() -> TestResult {
        let qualified = HostedRetrievalTable::new(
            HostedRetrievalSupport::Supported,
            HostedRetrievalSupport::Supported,
            HostedRetrievalSupport::Supported,
        );
        let refused = bind_tool_plan_with(
            &TemplateToolSelection::parse(["x_search"])?,
            qualified,
            CodingPresentation::Grok,
            "xai-api",
            Some(search_and_fetch_port()),
        );
        assert!(refused.is_err());
        Ok(())
    }

    #[test]
    fn binding_refuses_the_first_entry_and_binds_only_the_empty_selection() -> TestResult {
        let refused = bind_tool_plan(
            &TemplateToolSelection::parse(["mcp:github", "web_search"])?,
            ToolRoute::Codex,
            "codex",
        )
        .err()
        .ok_or("a non-empty selection bound")?;
        assert_eq!(refused.tool(), &SelectedTool::McpServer(server("github")?));
        for route in ToolRoute::ALL {
            let plan = bind_tool_plan(&TemplateToolSelection::default(), route, "any")?;
            assert!(plan.tools().is_empty(), "{route:?}");
        }
        Ok(())
    }

    #[test]
    fn routes_map_from_every_native_provider_and_api_target() {
        use crate::binding::ResolvedProtocol;
        assert_eq!(
            ToolRoute::native(NativeProvider::Codex),
            Some(ToolRoute::Codex)
        );
        assert_eq!(
            ToolRoute::native(NativeProvider::Xai),
            Some(ToolRoute::Grok)
        );
        assert_eq!(
            ToolRoute::native(NativeProvider::Claude),
            Some(ToolRoute::Claude)
        );
        assert_eq!(
            ToolRoute::api(ApiTarget::OpenRouter {
                wire: ResolvedProtocol::Chat
            }),
            ToolRoute::OpenRouter
        );
        for (provider, route) in [
            (PlatformProvider::OpenAi, ToolRoute::OpenAi),
            (PlatformProvider::Anthropic, ToolRoute::Anthropic),
            (PlatformProvider::Xai, ToolRoute::XaiApi),
        ] {
            assert_eq!(ToolRoute::api(ApiTarget::Platform { provider }), route);
        }
    }
}
