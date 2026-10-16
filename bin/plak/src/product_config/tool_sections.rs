//! `[tools.<name>]` and `[mcp_servers.<id>]` in `config.toml`.
//!
//! The optional tools a template lists by name take their options from these
//! sections. Presence enables nothing: a `[tools.web_search]` table only says
//! how `web_search` behaves where a template lists it, and an
//! `[mcp_servers.<id>]` table only says how to reach a server a template names
//! as `mcp:<id>`. Secrets are never values here: `secret_env`,
//! `bearer_secret` and `secret_headers` name a secret stored with
//! `plak secret set`.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Range, RangeInclusive};

use plakous_hosted::tool_plan::{OptionalTool, SelectedTool, TemplateToolSelection};
use serde::Deserialize;
use toml::Spanned;

use super::toml_file::{Entry, located, table_header};
use super::{toml_key, toml_text};

/// `[tools.fetch_url]` default response-body cap, in bytes.
pub const DEFAULT_FETCH_URL_MAX_BYTES: u32 = 2_097_152;
/// `[tools.fetch_url]` default whole-fetch timeout, in seconds.
pub const DEFAULT_FETCH_URL_TIMEOUT_SECONDS: u16 = 30;
/// `[tools.web_search]` default searches per call.
pub const DEFAULT_WEB_SEARCH_MAX_USES: u8 = 5;
/// `[mcp_servers.<id>]` default `initialize` + `tools/list` budget, in seconds.
pub const DEFAULT_MCP_STARTUP_SECONDS: u16 = 20;
/// `[mcp_servers.<id>]` default hard per-call ceiling, in seconds.
pub const DEFAULT_MCP_CALL_SECONDS: u16 = 120;

const FETCH_URL_MAX_BYTES: RangeInclusive<i64> = 1..=8_388_608;
const FETCH_URL_TIMEOUT_SECONDS: RangeInclusive<i64> = 1..=120;
const WEB_SEARCH_MAX_USES: RangeInclusive<i64> = 1..=20;
const MAX_DOMAINS: usize = 10;
const MAX_X_HANDLES: usize = 20;
const MCP_STARTUP_SECONDS: RangeInclusive<i64> = 1..=120;
const MCP_CALL_SECONDS: RangeInclusive<i64> = 1..=3600;
const MAX_MCP_TOOL_NAME_BYTES: usize = plakous_hosted::tool_plan::MAX_MCP_TOOL_NAME_BYTES;

/// `[tools.fetch_url]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FetchUrlOptions {
    /// Response body cap before conversion, in bytes.
    pub max_bytes: u32,
    /// Whole fetch including redirects, in seconds.
    pub timeout_seconds: u16,
}

impl Default for FetchUrlOptions {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_FETCH_URL_MAX_BYTES,
            timeout_seconds: DEFAULT_FETCH_URL_TIMEOUT_SECONDS,
        }
    }
}

/// `[tools.web_search]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebSearchOptions {
    /// Searches one call may run.
    pub max_uses: u8,
    /// Only these domains; exclusive with `blocked_domains`.
    pub allowed_domains: Vec<String>,
    /// Never these domains; exclusive with `allowed_domains`.
    pub blocked_domains: Vec<String>,
}

impl Default for WebSearchOptions {
    fn default() -> Self {
        Self {
            max_uses: DEFAULT_WEB_SEARCH_MAX_USES,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
        }
    }
}

/// `[tools.x_search]`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct XSearchOptions {
    /// Only posts from these X handles; empty is every handle.
    pub allowed_x_handles: Vec<String>,
}

/// How Plak reaches one MCP server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpTransport {
    /// A child process speaking MCP on stdin/stdout.
    Stdio {
        /// `argv[0]`, resolved on the captured PATH at spawn.
        command: String,
        /// The remaining argv.
        args: Vec<String>,
        /// Literal, non-secret environment.
        env: BTreeMap<String, String>,
        /// Environment variable -> named secret.
        secret_env: BTreeMap<String, String>,
    },
    /// Streamable HTTP.
    Http {
        /// `https://`, or `http://` to a loopback literal.
        url: String,
        /// Named secret sent as `Authorization: Bearer <secret>`.
        bearer_secret: Option<String>,
        /// Literal, non-secret headers.
        headers: BTreeMap<String, String>,
        /// Header -> named secret.
        secret_headers: BTreeMap<String, String>,
    },
}

/// `[mcp_servers.<id>]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerConfig {
    /// How the server is reached.
    pub transport: McpTransport,
    /// Server-level allowlist of MCP tool names; `None` is every listed tool.
    pub tools: Option<Vec<String>>,
    /// Startup budget, in seconds.
    pub startup_seconds: u16,
    /// Hard per-call ceiling, in seconds.
    pub call_seconds: u16,
}

/// Every `[tools.*]` and `[mcp_servers.*]` table one config resolved.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolSections {
    /// `[tools.fetch_url]`, when the file carries it.
    pub fetch_url: Option<FetchUrlOptions>,
    /// `[tools.web_search]`, when the file carries it.
    pub web_search: Option<WebSearchOptions>,
    /// `[tools.x_search]`, when the file carries it.
    pub x_search: Option<XSearchOptions>,
    /// `[mcp_servers.<id>]`, keyed by id.
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

impl ToolSections {
    /// Whether no table was given.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fetch_url.is_none()
            && self.web_search.is_none()
            && self.x_search.is_none()
            && self.mcp_servers.is_empty()
    }

    /// A trusted project's tables replace the home's entry of the same name;
    /// every other home entry stays.
    pub(super) fn overlay(&mut self, project: &Self) {
        if let Some(options) = project.fetch_url {
            self.fetch_url = Some(options);
        }
        if let Some(options) = &project.web_search {
            self.web_search = Some(options.clone());
        }
        if let Some(options) = &project.x_search {
            self.x_search = Some(options.clone());
        }
        for (id, server) in &project.mcp_servers {
            self.mcp_servers.insert(id.clone(), server.clone());
        }
    }

    /// The canonical rendering of every table `selection` reaches, keyed
    /// `tools.<name>` / `mcp_servers.<id>`. A listed tool whose table is
    /// absent renders its defaults, so spelling a default out changes
    /// nothing. Tools without options reach no table.
    #[must_use]
    pub fn reached(&self, selection: &TemplateToolSelection) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for entry in selection.entries() {
            match entry {
                SelectedTool::Optional(tool) => {
                    if let Some(rendered) = self.render_options(*tool) {
                        out.insert(format!("tools.{}", tool.name()), rendered);
                    }
                }
                SelectedTool::McpServer(server) | SelectedTool::McpTool { server, .. } => {
                    if let Some(config) = self.mcp_servers.get(server.as_str()) {
                        out.insert(
                            format!("mcp_servers.{}", server.as_str()),
                            render_mcp_server(server.as_str(), config),
                        );
                    }
                }
            }
        }
        out
    }

    /// Every table this config carries, canonically rendered, for
    /// `plak config show`.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for tool in OptionalTool::ALL {
            let present = match tool {
                OptionalTool::FetchUrl => self.fetch_url.is_some(),
                OptionalTool::WebSearch => self.web_search.is_some(),
                OptionalTool::XSearch => self.x_search.is_some(),
                OptionalTool::WebFetch | OptionalTool::ImageGen => false,
            };
            if let Some(rendered) = present.then(|| self.render_options(tool)).flatten() {
                out.push('\n');
                out.push_str(&rendered);
            }
        }
        for (id, server) in &self.mcp_servers {
            out.push('\n');
            out.push_str(&render_mcp_server(id, server));
        }
        out
    }

    fn render_options(&self, tool: OptionalTool) -> Option<String> {
        let header = format!("[tools.{}]\n", tool.name());
        match tool {
            OptionalTool::FetchUrl => {
                let options = self.fetch_url.unwrap_or_default();
                Some(format!(
                    "{header}max_bytes = {}\ntimeout_seconds = {}\n",
                    options.max_bytes, options.timeout_seconds
                ))
            }
            OptionalTool::WebSearch => {
                let options = self.web_search.clone().unwrap_or_default();
                Some(format!(
                    "{header}max_uses = {}\nallowed_domains = {}\nblocked_domains = {}\n",
                    options.max_uses,
                    render_array(&options.allowed_domains),
                    render_array(&options.blocked_domains)
                ))
            }
            OptionalTool::XSearch => {
                let options = self.x_search.clone().unwrap_or_default();
                Some(format!(
                    "{header}allowed_x_handles = {}\n",
                    render_array(&options.allowed_x_handles)
                ))
            }
            OptionalTool::WebFetch | OptionalTool::ImageGen => None,
        }
    }
}

fn render_mcp_server(id: &str, server: &McpServerConfig) -> String {
    let mut out = format!("[mcp_servers.{id}]\n");
    match &server.transport {
        McpTransport::Stdio {
            command,
            args,
            env,
            secret_env,
        } => {
            out.push_str("transport = \"stdio\"\n");
            out.push_str(&format!("command = {}\n", toml_text(command)));
            out.push_str(&format!("args = {}\n", render_array(args)));
            out.push_str(&format!("env = {}\n", render_table(env)));
            out.push_str(&format!("secret_env = {}\n", render_table(secret_env)));
        }
        McpTransport::Http {
            url,
            bearer_secret,
            headers,
            secret_headers,
        } => {
            out.push_str("transport = \"http\"\n");
            out.push_str(&format!("url = {}\n", toml_text(url)));
            if let Some(secret) = bearer_secret {
                out.push_str(&format!("bearer_secret = {}\n", toml_text(secret)));
            }
            out.push_str(&format!("headers = {}\n", render_table(headers)));
            out.push_str(&format!(
                "secret_headers = {}\n",
                render_table(secret_headers)
            ));
        }
    }
    if let Some(tools) = &server.tools {
        out.push_str(&format!("tools = {}\n", render_array(tools)));
    }
    out.push_str(&format!("startup_seconds = {}\n", server.startup_seconds));
    out.push_str(&format!("call_seconds = {}\n", server.call_seconds));
    out
}

fn render_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|value| toml_text(value)).collect();
    format!("[{}]", items.join(", "))
}

fn render_table(values: &BTreeMap<String, String>) -> String {
    if values.is_empty() {
        return "{}".to_owned();
    }
    let items: Vec<String> = values
        .iter()
        .map(|(key, value)| format!("{} = {}", toml_key(key), toml_text(value)))
        .collect();
    format!("{{ {} }}", items.join(", "))
}

/// `[tools]`: the closed set of tools that take options.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ToolsSection {
    fetch_url: Option<FetchUrlSection>,
    web_search: Option<WebSearchSection>,
    x_search: Option<XSearchSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchUrlSection {
    max_bytes: Entry,
    timeout_seconds: Entry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebSearchSection {
    max_uses: Entry,
    allowed_domains: Entry,
    blocked_domains: Entry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct XSearchSection {
    allowed_x_handles: Entry,
}

/// One `[mcp_servers.<id>]` table, every key of both transports.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct McpServerSection {
    transport: Entry,
    command: Entry,
    args: Entry,
    env: Entry,
    secret_env: Entry,
    url: Entry,
    bearer_secret: Entry,
    headers: Entry,
    secret_headers: Entry,
    tools: Entry,
    startup_seconds: Entry,
    call_seconds: Entry,
}

/// The two section families as a typed, validated value. Every refusal is
/// located at the value (or the table header) it is about.
pub(super) fn parse(
    text: &str,
    tools: Option<&ToolsSection>,
    mcp_servers: Option<&BTreeMap<String, McpServerSection>>,
) -> Result<ToolSections, String> {
    let mut sections = ToolSections::default();
    if let Some(tools) = tools {
        if let Some(section) = &tools.fetch_url {
            let reader = Reader::new(text, "tools.fetch_url");
            let defaults = FetchUrlOptions::default();
            sections.fetch_url = Some(FetchUrlOptions {
                max_bytes: reader
                    .integer("max_bytes", &section.max_bytes, FETCH_URL_MAX_BYTES)?
                    .map_or(Ok(defaults.max_bytes), u32::try_from)
                    .map_err(|_| reader.message("max_bytes is out of range"))?,
                timeout_seconds: reader
                    .integer(
                        "timeout_seconds",
                        &section.timeout_seconds,
                        FETCH_URL_TIMEOUT_SECONDS,
                    )?
                    .map_or(Ok(defaults.timeout_seconds), u16::try_from)
                    .map_err(|_| reader.message("timeout_seconds is out of range"))?,
            });
        }
        if let Some(section) = &tools.web_search {
            let reader = Reader::new(text, "tools.web_search");
            let allowed = reader.names(
                "allowed_domains",
                &section.allowed_domains,
                MAX_DOMAINS,
                "domain names",
                is_domain,
            )?;
            let blocked = reader.names(
                "blocked_domains",
                &section.blocked_domains,
                MAX_DOMAINS,
                "domain names",
                is_domain,
            )?;
            if !allowed.is_empty()
                && !blocked.is_empty()
                && let Some(spanned) = &section.blocked_domains
            {
                return Err(reader.at(
                    spanned,
                    "allowed_domains and blocked_domains cannot both be set; list one or the other",
                ));
            }
            sections.web_search = Some(WebSearchOptions {
                max_uses: reader
                    .integer("max_uses", &section.max_uses, WEB_SEARCH_MAX_USES)?
                    .map_or(Ok(DEFAULT_WEB_SEARCH_MAX_USES), u8::try_from)
                    .map_err(|_| reader.message("max_uses is out of range"))?,
                allowed_domains: allowed,
                blocked_domains: blocked,
            });
        }
        if let Some(section) = &tools.x_search {
            let reader = Reader::new(text, "tools.x_search");
            sections.x_search = Some(XSearchOptions {
                allowed_x_handles: reader.names(
                    "allowed_x_handles",
                    &section.allowed_x_handles,
                    MAX_X_HANDLES,
                    "X handles (1 to 15 letters, digits or _, no @)",
                    is_x_handle,
                )?,
            });
        }
    }
    for (id, section) in mcp_servers.into_iter().flatten() {
        let server = parse_mcp_server(text, id, section)?;
        sections.mcp_servers.insert(id.clone(), server);
    }
    Ok(sections)
}

fn parse_mcp_server(
    text: &str,
    id: &str,
    section: &McpServerSection,
) -> Result<McpServerConfig, String> {
    let name = format!("mcp_servers.{id}");
    let reader = Reader::new(text, &name);
    plakous::tools::McpServerName::new(id).map_err(|_| {
        reader.header(
            "is not a valid MCP server id; an id matches [a-z][a-z0-9]{0,15}(_[a-z0-9]+)* within 32 bytes",
        )
    })?;
    let transport = match &section.transport {
        None => return Err(reader.header("needs transport = \"stdio\" or \"http\"")),
        Some(spanned) => match spanned.as_ref() {
            toml::Value::String(value) if value == "stdio" => "stdio",
            toml::Value::String(value) if value == "http" => "http",
            _ => return Err(reader.at(spanned, "transport must be \"stdio\" or \"http\"")),
        },
    };
    let foreign: [(&str, &Entry); 4] = if transport == "stdio" {
        [
            ("url", &section.url),
            ("bearer_secret", &section.bearer_secret),
            ("headers", &section.headers),
            ("secret_headers", &section.secret_headers),
        ]
    } else {
        [
            ("command", &section.command),
            ("args", &section.args),
            ("env", &section.env),
            ("secret_env", &section.secret_env),
        ]
    };
    for (key, entry) in foreign {
        if let Some(spanned) = entry {
            return Err(reader.at(
                spanned,
                &format!("{key} does not apply to transport = \"{transport}\""),
            ));
        }
    }
    let transport = if transport == "stdio" {
        let Some(command) = reader.string("command", &section.command)? else {
            return Err(reader.header("needs command for transport = \"stdio\""));
        };
        if command.trim().is_empty() || command.contains('\0') {
            return Err(reader.at_entry(&section.command, "command must be a non-empty string"));
        }
        let args = match &section.args {
            None => Vec::new(),
            Some(spanned) => reader.string_array("args", spanned)?,
        };
        let env = reader.string_table("env", &section.env, is_env_name, "environment names")?;
        let secret_env = reader.string_table(
            "secret_env",
            &section.secret_env,
            is_env_name,
            "environment names",
        )?;
        reader.secret_names("secret_env", &section.secret_env, secret_env.values())?;
        if let Some(shared) = env.keys().find(|key| secret_env.contains_key(*key)) {
            return Err(reader.at_entry(
                &section.secret_env,
                &format!("{shared} is in both env and secret_env; keep one"),
            ));
        }
        McpTransport::Stdio {
            command,
            args,
            env,
            secret_env,
        }
    } else {
        let Some(url) = reader.string("url", &section.url)? else {
            return Err(reader.header("needs url for transport = \"http\""));
        };
        if let Err(message) = check_url(&url) {
            return Err(reader.at_entry(&section.url, &message));
        }
        let bearer_secret = reader.string("bearer_secret", &section.bearer_secret)?;
        if let Some(secret) = &bearer_secret {
            reader.secret_names(
                "bearer_secret",
                &section.bearer_secret,
                std::iter::once(secret),
            )?;
        }
        let headers =
            reader.string_table("headers", &section.headers, is_header_name, "header names")?;
        let secret_headers = reader.string_table(
            "secret_headers",
            &section.secret_headers,
            is_header_name,
            "header names",
        )?;
        reader.secret_names(
            "secret_headers",
            &section.secret_headers,
            secret_headers.values(),
        )?;
        if headers
            .keys()
            .any(|header| header.eq_ignore_ascii_case("authorization"))
        {
            return Err(reader.at_entry(
                &section.headers,
                "headers cannot carry Authorization; name a secret with bearer_secret or secret_headers",
            ));
        }
        if let Some((header, _)) = headers
            .iter()
            .find(|(_, value)| value.chars().any(char::is_control))
        {
            return Err(reader.at_entry(
                &section.headers,
                &format!("header {header} has a control character in its value"),
            ));
        }
        let mut seen = BTreeSet::new();
        for header in headers.keys().chain(secret_headers.keys()) {
            if !seen.insert(header.to_ascii_lowercase()) {
                return Err(reader.at_entry(
                    &section.secret_headers,
                    &format!("header {header} is set twice across headers and secret_headers"),
                ));
            }
        }
        if bearer_secret.is_some() && seen.contains("authorization") {
            return Err(reader.at_entry(
                &section.secret_headers,
                "Authorization is set by bearer_secret; remove it from secret_headers",
            ));
        }
        McpTransport::Http {
            url,
            bearer_secret,
            headers,
            secret_headers,
        }
    };
    let tools = match &section.tools {
        None => None,
        Some(spanned) => {
            let tools = reader.string_array("tools", spanned)?;
            let mut seen = BTreeSet::new();
            for tool in &tools {
                if !is_mcp_tool_name(tool) {
                    return Err(reader.at(
                        spanned,
                        &format!(
                            "tools entry {tool:?} is not an MCP tool name (1 to {MAX_MCP_TOOL_NAME_BYTES} bytes, no whitespace)"
                        ),
                    ));
                }
                if !seen.insert(tool.as_str()) {
                    return Err(reader.at(spanned, &format!("tools lists {tool:?} twice")));
                }
            }
            Some(tools)
        }
    };
    let startup_seconds = reader
        .integer(
            "startup_seconds",
            &section.startup_seconds,
            MCP_STARTUP_SECONDS,
        )?
        .map_or(Ok(DEFAULT_MCP_STARTUP_SECONDS), u16::try_from)
        .map_err(|_| reader.message("startup_seconds is out of range"))?;
    let call_seconds = reader
        .integer("call_seconds", &section.call_seconds, MCP_CALL_SECONDS)?
        .map_or(Ok(DEFAULT_MCP_CALL_SECONDS), u16::try_from)
        .map_err(|_| reader.message("call_seconds is out of range"))?;
    Ok(McpServerConfig {
        transport,
        tools,
        startup_seconds,
        call_seconds,
    })
}

/// Reads typed values out of one section, naming the section in every
/// refusal.
struct Reader<'a> {
    text: &'a str,
    section: String,
}

impl<'a> Reader<'a> {
    fn new(text: &'a str, section: &str) -> Self {
        Self {
            text,
            section: section.to_owned(),
        }
    }

    fn message(&self, message: &str) -> String {
        format!("[{}] {message}", self.section)
    }

    fn at(&self, spanned: &Spanned<toml::Value>, message: &str) -> String {
        located(self.text, spanned.span(), &self.message(message))
    }

    fn at_entry(&self, entry: &Entry, message: &str) -> String {
        match entry {
            Some(spanned) => self.at(spanned, message),
            None => self.header(message),
        }
    }

    /// A refusal about the table as a whole, at its `[header]` when the file
    /// spells one.
    fn header(&self, message: &str) -> String {
        match header_span(self.text, &self.section) {
            Some(span) => located(self.text, span, &self.message(message)),
            None => self.message(message),
        }
    }

    fn integer(
        &self,
        key: &str,
        entry: &Entry,
        range: RangeInclusive<i64>,
    ) -> Result<Option<i64>, String> {
        let Some(spanned) = entry else {
            return Ok(None);
        };
        let refusal = || {
            self.at(
                spanned,
                &format!(
                    "{key} must be an integer in {}..={}",
                    range.start(),
                    range.end()
                ),
            )
        };
        match spanned.as_ref() {
            toml::Value::Integer(value) if range.contains(value) => Ok(Some(*value)),
            _ => Err(refusal()),
        }
    }

    fn string(&self, key: &str, entry: &Entry) -> Result<Option<String>, String> {
        let Some(spanned) = entry else {
            return Ok(None);
        };
        match spanned.as_ref() {
            toml::Value::String(value) => Ok(Some(value.clone())),
            _ => Err(self.at(spanned, &format!("{key} must be a string"))),
        }
    }

    fn string_array(
        &self,
        key: &str,
        spanned: &Spanned<toml::Value>,
    ) -> Result<Vec<String>, String> {
        let refusal = || self.at(spanned, &format!("{key} must be an array of strings"));
        let toml::Value::Array(items) = spanned.as_ref() else {
            return Err(refusal());
        };
        items
            .iter()
            .map(|item| match item {
                toml::Value::String(value) => Ok(value.clone()),
                _ => Err(refusal()),
            })
            .collect()
    }

    /// A bounded array of strings that each pass `valid`, none twice.
    fn names(
        &self,
        key: &str,
        entry: &Entry,
        maximum: usize,
        what: &str,
        valid: fn(&str) -> bool,
    ) -> Result<Vec<String>, String> {
        let Some(spanned) = entry else {
            return Ok(Vec::new());
        };
        let refusal = || {
            self.at(
                spanned,
                &format!("{key} must be an array of at most {maximum} {what}"),
            )
        };
        let names = self.string_array(key, spanned).map_err(|_| refusal())?;
        if names.len() > maximum || !names.iter().all(|name| valid(name)) {
            return Err(refusal());
        }
        let mut seen = BTreeSet::new();
        if let Some(twice) = names.iter().find(|name| !seen.insert(name.as_str())) {
            return Err(self.at(spanned, &format!("{key} lists {twice:?} twice")));
        }
        Ok(names)
    }

    /// An inline or standard table of strings whose keys pass `valid`.
    fn string_table(
        &self,
        key: &str,
        entry: &Entry,
        valid: fn(&str) -> bool,
        what: &str,
    ) -> Result<BTreeMap<String, String>, String> {
        let Some(spanned) = entry else {
            return Ok(BTreeMap::new());
        };
        let toml::Value::Table(table) = spanned.as_ref() else {
            return Err(self.at(spanned, &format!("{key} must be a table of strings")));
        };
        let mut out = BTreeMap::new();
        for (name, value) in table {
            if !valid(name) {
                return Err(self.at(
                    spanned,
                    &format!("{key} key {name:?} is not one of the valid {what}"),
                ));
            }
            let toml::Value::String(value) = value else {
                return Err(self.at(spanned, &format!("{key} {name} must be a string")));
            };
            out.insert(name.clone(), value.clone());
        }
        Ok(out)
    }

    /// Secret references are names, never values.
    fn secret_names<'b>(
        &self,
        key: &str,
        entry: &Entry,
        names: impl Iterator<Item = &'b String>,
    ) -> Result<(), String> {
        for name in names {
            if let Err(error) = crate::home::secrets::validate_secret_name(name) {
                return Err(self.at_entry(entry, &format!("{key} names a secret: {error}")));
            }
        }
        Ok(())
    }
}

/// The byte span of `[name]` in the file, when the file spells the table as
/// a header (an inline table has none).
fn header_span(text: &str, name: &str) -> Option<Range<usize>> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if table_header(line).as_deref() == Some(name) {
            let start = offset + (line.len() - line.trim_start().len());
            return Some(start..start + line.trim().len());
        }
        offset += line.len();
    }
    None
}

/// A domain name as a search filter takes it: dot-separated labels of ASCII
/// letters, digits and `-`, at most 253 bytes.
fn is_domain(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// An X handle without its `@`: 1 to 15 ASCII letters, digits or `_`.
fn is_x_handle(value: &str) -> bool {
    (1..=15).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
fn is_env_name(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// An HTTP header field name (RFC 9110 token).
fn is_header_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// The tool part of `mcp:<id>/<tool>`: the template grammar's own rule.
fn is_mcp_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_MCP_TOOL_NAME_BYTES
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

/// `https://<host>[:port][/path]`, or `http://` only to the loopback
/// literals `127.0.0.1` and `[::1]`. Never credentials or a fragment.
fn check_url(url: &str) -> Result<(), String> {
    const RULE: &str =
        "url must be https://, or http:// to a loopback literal (127.0.0.1 or [::1])";
    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(RULE.to_owned());
    };
    if url
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
        || url.contains('#')
    {
        return Err(RULE.to_owned());
    }
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err("url cannot carry credentials; name a secret with bearer_secret".to_owned());
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((host, after)) = bracketed.split_once(']') else {
            return Err(RULE.to_owned());
        };
        (format!("[{host}]"), after.strip_prefix(':'))
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (authority.to_owned(), None),
        }
    };
    if host.is_empty() || host == "[]" {
        return Err(RULE.to_owned());
    }
    if let Some(port) = port
        && !port.parse::<u16>().is_ok_and(|port| port > 0)
    {
        return Err(RULE.to_owned());
    }
    if !secure && host != "127.0.0.1" && host != "[::1]" {
        return Err(RULE.to_owned());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::check_url;

    #[test]
    fn mcp_urls_are_https_or_loopback_http_without_credentials() {
        for good in [
            "https://mcp.linear.app/mcp",
            "https://example.com:8443/a?b=c",
            "http://127.0.0.1:3000/mcp",
            "http://[::1]:3000/mcp",
            "http://127.0.0.1/mcp",
        ] {
            assert_eq!(check_url(good), Ok(()), "{good}");
        }
        for bad in [
            "http://example.com/mcp",
            "http://localhost:3000/mcp",
            "ftp://example.com",
            "https://",
            "https://example.com/a#frag",
            "https://example.com:0/",
            "https://example.com:99999/",
            "https:// example.com",
            "mcp.linear.app",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
        assert_eq!(
            check_url("https://user:pw@example.com/mcp").unwrap_err(),
            "url cannot carry credentials; name a secret with bearer_secret"
        );
    }
}
