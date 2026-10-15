//! The optional tools a template names, and the deterministic plan they
//! resolve to at session bind.
//!
//! A template's `tools` list is parsed once into a [`TemplateToolSelection`].
//! Binding resolves it against the route and the MCP listings into a
//! [`ToolPlan`], or refuses it with a [`ToolPlanRefusal`].

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the plan and its refusal are built by the session-bind step"
    )
)]

use std::collections::BTreeSet;
use std::fmt;

use plakous::model::ToolName;
use plakous::tools::McpServerName;

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
        matches!(self, Self::WebSearch | Self::WebFetch | Self::XSearch)
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
}

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
        Self { tools }
    }

    /// The planned tools in plan order.
    pub(crate) fn tools(&self) -> &[PlannedTool] {
        &self.tools
    }
}

/// A selected tool the bound route cannot serve; it fails session creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolPlanRefusal {
    tool: SelectedTool,
    route: String,
    remedy: String,
}

impl ToolPlanRefusal {
    /// `route` is the route's display label and `remedy` one concrete fix;
    /// both are one line.
    pub(crate) fn unavailable_on_route(
        tool: SelectedTool,
        route: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Self {
            tool,
            route: route.into(),
            remedy: remedy.into(),
        }
    }

    pub(crate) const fn tool(&self) -> &SelectedTool {
        &self.tool
    }
}

impl fmt::Display for ToolPlanRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} is not available on {}; remedy: {}",
            self.tool, self.route, self.remedy
        )
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

    #[test]
    fn refusal_text_pins() -> TestResult {
        let x_search = ToolPlanRefusal::unavailable_on_route(
            SelectedTool::parse("x_search")?,
            "codex (ChatGPT subscription)",
            "put x_search on a grok or xai-api template and spawn it",
        );
        assert_eq!(
            x_search.tool(),
            &SelectedTool::Optional(OptionalTool::XSearch)
        );
        assert_eq!(
            x_search.to_string(),
            "x_search is not available on codex (ChatGPT subscription); \
             remedy: put x_search on a grok or xai-api template and spawn it"
        );
        assert_eq!(
            ToolPlanRefusal::unavailable_on_route(
                SelectedTool::parse("web_fetch")?,
                "openrouter",
                "use fetch_url (Plak-run) or a claude/anthropic template",
            )
            .to_string(),
            "web_fetch is not available on openrouter; \
             remedy: use fetch_url (Plak-run) or a claude/anthropic template"
        );
        Ok(())
    }
}
