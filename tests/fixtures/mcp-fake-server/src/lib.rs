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

use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, DiscoverResult,
    ErrorCode, InputRequiredResult, ListToolsResult, PaginatedRequestParams,
    ProgressNotificationParam, ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
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
        }
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

fn text(value: impl Into<String>) -> CallToolResponse {
    CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::text(value)]))
}

impl ServerHandler for FakeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
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
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{}", request.name);
            }
        }
        let arguments = request.arguments.unwrap_or_default();
        Ok(match request.name.as_ref() {
            "echo" => text(
                arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            // Eight zero bytes, base64.
            "image" => {
                CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::image(
                    "AAAAAAAAAAA=",
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
