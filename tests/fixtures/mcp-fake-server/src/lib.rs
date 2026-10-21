//! A scripted MCP server for the MCP client's offline tests.
//!
//! One handler serves both lifecycle styles: the `initialize` handshake
//! (protocol 2025-11-25) and the no-initialize discovery lifecycle
//! (2026-07-28). The binary serves it over stdio or streamable HTTP; tests
//! may also serve it in process over any byte stream or, through [`http`],
//! on a loopback HTTP port behind a credential gate.

#![forbid(unsafe_code)]

// `plakous-mcp`'s unit tests include this file by path, without the HTTP
// server dependencies; they never compile the HTTP fixture.
#[cfg(not(test))]
pub mod http;
#[cfg(not(test))]
pub mod oauth;

use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, DiscoverResult,
    ErrorCode, GetPromptRequestParams, GetPromptResponse, GetPromptResult, InputRequiredResult,
    ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, ProgressNotificationParam, Prompt, PromptArgument, PromptMessage,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, Role, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value, json};

/// Which MCP lifecycle the server speaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Style {
    /// `initialize` handshake only (protocol 2025-11-25).
    Initialize,
    /// Discovery without `initialize` only (protocol 2026-07-28).
    Discover,
}

impl Style {
    /// The single protocol version the server accepts.
    #[must_use]
    pub fn version(self) -> ProtocolVersion {
        match self {
            Self::Initialize => ProtocolVersion::V_2025_11_25,
            Self::Discover => ProtocolVersion::V_2026_07_28,
        }
    }
}

/// What the scripted server observed, shared with the test.
#[derive(Debug, Default)]
pub struct Observed {
    calls: Mutex<Vec<String>>,
    cancelled: AtomicUsize,
}

impl Observed {
    /// Tool names called, in arrival order.
    #[must_use]
    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Calls the client cancelled.
    #[must_use]
    pub fn cancelled(&self) -> usize {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// The scripted server.
#[derive(Clone)]
pub struct FakeServer {
    style: Style,
    tools: Vec<Tool>,
    observed: Arc<Observed>,
    on_exit: Arc<dyn Fn() + Send + Sync>,
    call_log: Option<std::path::PathBuf>,
    list_log: Option<std::path::PathBuf>,
    resources: bool,
}

impl FakeServer {
    /// The standard tool set under `style`; the `exit` tool runs `on_exit`.
    #[must_use]
    pub fn new(style: Style, on_exit: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            style,
            tools: standard_tools(),
            observed: Arc::default(),
            on_exit,
            call_log: None,
            list_log: None,
            resources: false,
        }
    }

    /// Advertises the resources capability and serves the scripted
    /// resources ([`scripted_resources`]).
    #[must_use]
    pub const fn with_resources(mut self) -> Self {
        self.resources = true;
        self
    }

    /// Appends each called tool's name as one line to `path` before the call
    /// runs, so a test in another process can count dispatches.
    #[must_use]
    #[allow(
        dead_code,
        reason = "only the binary logs calls; plakous-mcp compiles this file in process"
    )]
    pub fn with_call_log(mut self, path: std::path::PathBuf) -> Self {
        self.call_log = Some(path);
        self
    }

    /// Appends one `list` line to `path` per `tools/list` request, so a test
    /// in another process can count listings.
    #[must_use]
    #[allow(
        dead_code,
        reason = "only the binary logs listings; plakous-mcp compiles this file in process"
    )]
    pub fn with_list_log(mut self, path: std::path::PathBuf) -> Self {
        self.list_log = Some(path);
        self
    }

    /// Replaces the listed tools.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = tools;
        self
    }

    /// Returns the shared observations.
    #[must_use]
    pub fn observed(&self) -> Arc<Observed> {
        Arc::clone(&self.observed)
    }
}

/// One listed tool over an inline JSON schema.
#[must_use]
pub fn tool(name: &str, description: &str, schema: Value) -> Tool {
    let schema = match schema {
        Value::Object(schema) => schema,
        _ => Map::new(),
    };
    Tool::new(name.to_owned(), description.to_owned(), Arc::new(schema))
}

/// The scripted tools every style lists.
#[must_use]
pub fn standard_tools() -> Vec<Tool> {
    let empty = json!({"type": "object"});
    vec![
        tool(
            "echo",
            "Echoes text.",
            json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
        ),
        tool("image", "Returns a small image.", empty.clone()),
        tool(
            "structured",
            "Returns structured content only.",
            empty.clone(),
        ),
        tool("fail", "Returns an error result.", empty.clone()),
        tool("rpc_error", "Returns a JSON-RPC error.", empty.clone()),
        tool(
            "escaped_result",
            "Returns text exceeding encoded outcome limits.",
            empty.clone(),
        ),
        tool(
            "slow",
            "Reports progress, then finishes.",
            json!({"type": "object", "properties": {"steps": {"anyOf": [{"type": "integer"}, {"type": "null"}]}}}),
        ),
        tool("hang", "Never finishes until cancelled.", empty.clone()),
        tool("exit", "Exits the server mid-call.", empty.clone()),
        tool("big", "Returns 1 MiB of text.", empty.clone()),
        tool("input", "Asks for interactive input.", empty.clone()),
        tool("roots", "Lists the client's roots.", empty),
    ]
}

fn append_line(path: &std::path::Path, line: &str) {
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn text(value: impl Into<String>) -> CallToolResponse {
    CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::text(value)]))
}

/// Eight zero bytes, base64: a tiny PNG-typed payload.
const TINY_PNG: &str = "AAAAAAAAAAA=";

/// The resources a server with resources enabled lists.
#[must_use]
pub fn scripted_resources() -> Vec<Resource> {
    vec![
        Resource::new("fake://readme", "readme").with_mime_type("text/markdown"),
        Resource::new("fake://logo.png", "logo").with_mime_type("image/png"),
        Resource::new("fake://data.bin", "data").with_mime_type("application/octet-stream"),
    ]
}

impl FakeServer {
    fn record(&self, entry: String) {
        self.observed
            .calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
    }

    fn prompt(&self, request: GetPromptRequestParams) -> Result<GetPromptResponse, ErrorData> {
        self.record(format!("prompts/get {}", request.name));
        let arguments = request.arguments.unwrap_or_default();
        let argument = |name: &str| {
            arguments
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_owned()
        };
        let messages = match request.name.as_str() {
            "review" => vec![
                PromptMessage::new_text(Role::User, format!("Review {}.", argument("target"))),
                PromptMessage::new_text(
                    Role::Assistant,
                    format!("I will focus on {}.", argument("focus")),
                ),
                PromptMessage::new(Role::User, ContentBlock::image(TINY_PNG, "image/png")),
            ],
            "big" => vec![PromptMessage::new_text(Role::User, "p".repeat(300 * 1024))],
            "input" => {
                return Ok(GetPromptResponse::InputRequired(InputRequiredResult::new(
                    None,
                    Some(String::from("state")),
                )));
            }
            other => {
                return Err(ErrorData::invalid_params(
                    format!("no prompt {other}"),
                    None,
                ));
            }
        };
        Ok(GetPromptResponse::Complete(GetPromptResult::new(messages)))
    }
}

impl ServerHandler for FakeServer {
    fn get_info(&self) -> ServerConfig {
        let capabilities = if self.resources {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build()
        } else {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build()
        };
        ServerConfig::new(capabilities)
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(
            ListResourcesResult::with_all_items(scripted_resources()),
        ))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("fake://item/{id}", "item"),
        ])))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        self.record(format!("resources/read {}", request.uri));
        let blob = |mime: &str, data: &str| ResourceContents::BlobResourceContents {
            uri: request.uri.clone(),
            mime_type: Some(mime.to_owned()),
            blob: data.to_owned(),
            meta: None,
        };
        let text = |mime: &str, text: String| ResourceContents::TextResourceContents {
            uri: request.uri.clone(),
            mime_type: Some(mime.to_owned()),
            text,
            meta: None,
        };
        let contents = match request.uri.as_str() {
            "fake://readme" => vec![text("text/markdown", String::from("# hello resource"))],
            "fake://logo.png" => vec![blob("image/png", TINY_PNG)],
            "fake://data.bin" => vec![blob("application/octet-stream", "AAAA")],
            "fake://big" => vec![text("text/plain", "r".repeat(1024 * 1024))],
            "fake://hang" => {
                context.ct.cancelled().await;
                self.observed.cancelled.fetch_add(1, Ordering::AcqRel);
                return Err(ErrorData::internal_error("cancelled", None));
            }
            "fake://input" => {
                return Ok(ReadResourceResponse::InputRequired(
                    InputRequiredResult::new(None, Some(String::from("state"))),
                ));
            }
            other => {
                return Err(ErrorData::invalid_params(
                    format!("resource not found: {other}"),
                    None,
                ));
            }
        };
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            contents,
        )))
    }

    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListPromptsResult::with_all_items(vec![
            Prompt::new(
                "review",
                Some("Reviews a target."),
                Some(vec![
                    PromptArgument::new("target").with_required(true),
                    PromptArgument::new("focus"),
                ]),
            ),
            Prompt::new("big", Some("Expands to 300 KiB."), None),
            Prompt::new("input", Some("Asks for interactive input."), None),
        ])))
    }

    fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<GetPromptResponse, ErrorData>> + Send + '_ {
        std::future::ready(self.prompt(request))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Owned(vec![self.style.version()])
    }

    /// A legacy server predates `server/discover` and answers it as an
    /// unknown method, which sends the client to `initialize`.
    fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<DiscoverResult, ErrorData>> + Send + '_ {
        std::future::ready(match self.style {
            Style::Initialize => Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "server/discover",
                None,
            )),
            Style::Discover => Ok(DiscoverResult::from_server_info(
                vec![self.style.version()],
                self.get_info(),
            )),
        })
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        if let Some(path) = &self.list_log {
            append_line(path, "list");
        }
        std::future::ready(Ok(ListToolsResult::with_all_items(self.tools.clone())))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.observed
            .calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.name.to_string());
        if let Some(path) = &self.call_log {
            append_line(path, request.name.as_ref());
        }
        let arguments = request.arguments.unwrap_or_default();
        Ok(match request.name.as_ref() {
            "echo" => text(
                arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            "image" => {
                CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::image(
                    TINY_PNG,
                    "image/png",
                )]))
            }
            "structured" => {
                let mut result = CallToolResult::structured(json!({"ok": true, "n": 2}));
                result.content.clear();
                CallToolResponse::Complete(result)
            }
            "fail" => CallToolResponse::Complete(CallToolResult::error(vec![ContentBlock::text(
                "scripted failure",
            )])),
            "rpc_error" => {
                return Err(ErrorData::new(
                    ErrorCode(-32021),
                    "scripted RPC refusal",
                    None,
                ));
            }
            "escaped_result" => text("\0".repeat(200_000)),
            "slow" => {
                let steps = arguments
                    .get("steps")
                    .and_then(Value::as_u64)
                    .and_then(|steps| u32::try_from(steps).ok())
                    .unwrap_or(2);
                if let Some(token) = context.meta.get_progress_token() {
                    for step in 1..=steps {
                        let mut progress =
                            ProgressNotificationParam::new(token.clone(), f64::from(step));
                        progress.total = Some(f64::from(steps));
                        progress.message = Some(String::from("indexing"));
                        let _ = context.peer.notify_progress(progress).await;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
                text("slow done")
            }
            "hang" => {
                context.ct.cancelled().await;
                self.observed.cancelled.fetch_add(1, Ordering::AcqRel);
                return Err(ErrorData::internal_error("cancelled", None));
            }
            "exit" => {
                (self.on_exit)();
                std::future::pending::<()>().await;
                text("unreachable")
            }
            "big" => text("z".repeat(1024 * 1024)),
            "input" => CallToolResponse::InputRequired(InputRequiredResult::new(
                None,
                Some(String::from("state")),
            )),
            #[allow(
                deprecated,
                reason = "the client still advertises roots; the fixture reads them back"
            )]
            "roots" => {
                let roots = context
                    .peer
                    .list_roots()
                    .await
                    .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
                text(
                    roots
                        .roots
                        .iter()
                        .map(|root| root.uri.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
            other => text(format!("called {other}")),
        })
    }
}
