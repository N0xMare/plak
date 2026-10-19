//! The MCP tool pack: one serial-mutating registration per frozen tool.

use std::sync::Arc;

use plakous_model::{
    CancellationToken, ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName,
    ToolOutputContract,
};
use plakous_tools::{
    CodingPresentation, MenuEntry, PassthroughPack, ProgressSink, PromptPin, RegisteredToolId,
    ToolContext, ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture,
    ToolInvocation, ToolOutcome, ToolOwnerClass, ToolPack, ToolRegistration, ToolRuntimeError,
    ToolRuntimeErrorKind,
};
use rmcp::ServiceError;
use rmcp::model::{CallToolRequest, CallToolRequestParams, ClientRequest, ServerResult};
use rmcp::service::PeerRequestOptions;
use serde_json::Value;

use crate::error::{McpError, McpErrorKind};
use crate::http::{self, HttpRefusal};
use crate::listing::{FrozenMcpListing, McpToolRecord};
use crate::pool::{Connection, Entry, McpLease};
use crate::result::map_call_result;

const MCP_TOOL_VERSION: &str = "1";
const OUTPUT_MAX_PARTS: u32 = 128;
const OUTPUT_MAX_BYTES: u32 = 1_048_576;
const TOOL_CHANGED: &str =
    "mcp tool changed since this session started; start a new session to pick up the change";

/// The frozen tools of one MCP server, registered as serial, mutating tools
/// owned by that server.
///
/// Every tool is [`ToolEffect::Mutating`] and [`ToolConcurrency::Serial`]
/// whatever its annotations claim: the server is untrusted, and a call that
/// may have acted is never replayed.
pub struct McpToolPack {
    lease: Arc<McpLease>,
    records: Vec<McpToolRecord>,
}

impl std::fmt::Debug for McpToolPack {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpToolPack")
            .field("server", self.lease.server())
            .field("tools", &self.records.len())
            .finish()
    }
}

impl McpToolPack {
    /// A pack over a leased server and its frozen selection.
    #[must_use]
    pub fn new(lease: McpLease, listing: &FrozenMcpListing) -> Self {
        Self {
            lease: Arc::new(lease),
            records: listing.records().to_vec(),
        }
    }

    /// A pack over a leased server and records frozen earlier, as an exact
    /// reopen rebuilds them: the server is not listed again, and a tool whose
    /// live definition differs fails its call as changed.
    ///
    /// # Errors
    ///
    /// Refuses a record of another server.
    pub fn from_records(lease: McpLease, records: Vec<McpToolRecord>) -> Result<Self, McpError> {
        if let Some(record) = records
            .iter()
            .find(|record| record.server() != lease.server())
        {
            return Err(McpError::new(
                McpErrorKind::Selection,
                format!(
                    "mcp tool record {} belongs to server {}, not {}",
                    record.exposed_name().as_str(),
                    record.server(),
                    lease.server()
                ),
            ));
        }
        Ok(Self {
            lease: Arc::new(lease),
            records,
        })
    }

    fn names(&self) -> Vec<ToolName> {
        self.records
            .iter()
            .map(|record| record.exposed_name().clone())
            .collect()
    }
}

impl ToolPack for McpToolPack {
    fn id(&self) -> &'static str {
        "plakous.mcp.v1"
    }

    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        self.records
            .iter()
            .map(|record| registration(&self.lease, record))
            .collect()
    }

    fn menu(&self, _presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        PassthroughPack::appended_menu(&self.names())
    }

    fn pins(&self) -> &'static [PromptPin] {
        // Server-supplied descriptions are not Plakous-owned text.
        &[]
    }
}

fn registration(
    lease: &Arc<McpLease>,
    record: &McpToolRecord,
) -> Result<ToolRegistration, ToolRuntimeError> {
    let invalid = |error: &(dyn std::error::Error + 'static)| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "mcp tool definition is invalid",
            error,
        )
    };
    let definition = ToolDefinition::new(
        record.exposed_name().clone(),
        MCP_TOOL_VERSION,
        record.description(),
        ToolInputContract::Structured(record.input_schema().clone()),
        ToolOutputContract::model_content(OUTPUT_MAX_PARTS, OUTPUT_MAX_BYTES)
            .map_err(|error| invalid(&error))?,
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )
    .map_err(|error| invalid(&error))?;
    let server = record.server().as_str();
    let exposed = record.exposed_name().as_str();
    let executor_id = ToolExecutorId::new(format!("plakous.mcp.{server}.{exposed}.v1"))?;
    let digest = record.definition_digest();
    Ok(ToolRegistration::new(
        RegisteredToolId::new(format!(
            "plakous.mcp.{server}.{}",
            record.exposed_name().as_str()
        ))?,
        executor_id.clone(),
        ToolExecutorBindingIdentity::new(
            1,
            &[
                b"mcp-tool",
                executor_id.as_str().as_bytes(),
                record.mcp_name().as_bytes(),
                digest.bytes(),
            ],
        )?,
        definition,
        Arc::new(McpToolExecutor {
            lease: Arc::clone(lease),
            record: record.clone(),
        }),
    )
    .with_owner(ToolOwnerClass::Mcp(record.server().clone())))
}

struct McpToolExecutor {
    lease: Arc<McpLease>,
    record: McpToolRecord,
}

impl ToolExecutor for McpToolExecutor {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let arguments = match invocation.arguments().value().as_ref() {
                Value::Object(arguments) => arguments.clone(),
                _ => {
                    return Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::InvalidArguments,
                        "mcp tool arguments must be an object",
                    ));
                }
            };
            let entry = &self.lease.entry;
            let raw = call(
                entry,
                &self.record,
                arguments,
                context.cancellation(),
                context.progress().clone(),
            )
            .await?;
            let outcome = match raw {
                CallResult::Outcome(outcome) => return Ok(outcome),
                CallResult::Complete(result) => result,
            };
            let sessions = context.environment().existing_joined_process_sessions();
            let server = self.record.server().as_str().to_owned();
            let tool = self.record.mcp_name().to_owned();
            tokio::task::spawn_blocking(move || {
                map_call_result(&server, &tool, outcome, &mut |bytes| {
                    sessions
                        .as_ref()
                        .and_then(|sessions| sessions.spill_output(bytes))
                        .map(|file| file.path)
                })
            })
            .await
            .map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::Execution,
                    "mcp result mapping did not finish",
                    &error,
                )
            })
            .and_then(std::convert::identity)
            .or_else(|error| {
                ToolOutcome::outcome_unknown_text(format!(
                    "mcp call completed but its result could not be represented ({error}); it may have acted; nothing was retried"
                ))
            })
        })
    }
}

pub(crate) enum CallResult {
    /// A settled outcome: refused before dispatch, a server error, or unknown.
    Outcome(ToolOutcome),
    /// A completed result to map.
    Complete(rmcp::model::CallToolResult),
}

/// Dispatches one `tools/call` and settles everything but a completed result.
pub(crate) async fn call(
    entry: &Entry,
    record: &McpToolRecord,
    arguments: serde_json::Map<String, Value>,
    cancellation: &CancellationToken,
    progress: ProgressSink,
) -> Result<CallResult, ToolRuntimeError> {
    let server = record.server();
    let tool = record.mcp_name();
    let _permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "mcp call cancelled before dispatch",
            ));
        }
        permit = entry.permits.acquire() => permit.map_err(|_| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, "mcp call admission closed")
        })?,
    };
    let connection = match entry.connection().await {
        Ok(connection) => connection,
        Err(error) => return failure(format!("{error}; the call was not sent")),
    };
    if connection.digests.get(tool) != Some(&record.definition_digest()) {
        return failure(TOOL_CHANGED.to_owned());
    }
    let timeout = entry.spec().call_timeout();
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(
        CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments),
    ));
    let mut options = PeerRequestOptions::no_options();
    options.timeout = Some(timeout);
    options.max_total_timeout = Some(timeout);
    let handle = match connection
        .peer
        .send_cancellable_request(request, options)
        .await
    {
        Ok(handle) => handle,
        Err(error) => {
            return settle_transport_error(entry, &connection, server, tool, &error).await;
        }
    };
    let request_id = handle.id.clone();
    let _route = connection
        .handler
        .route_progress(handle.progress_token.clone(), progress);
    let response = tokio::select! {
        response = handle.await_response() => response,
        () = cancellation.cancelled() => {
            let _ = connection
                .peer
                .notify_cancelled(rmcp::model::CancelledNotificationParam::new(
                    Some(request_id),
                    Some(String::from("the turn was cancelled")),
                ))
                .await;
            return unknown(format!(
                "mcp call {server}/{tool} was cancelled after it was sent; it may have acted; nothing was retried"
            ));
        }
    };
    match response {
        Ok(ServerResult::CallToolResult(result)) => Ok(CallResult::Complete(result)),
        Ok(ServerResult::InputRequiredResult(_)) => failure(String::from(
            "server asked for interactive input; elicitation is not supported",
        )),
        Ok(ServerResult::CreateTaskResult(_)) => unknown(format!(
            "mcp server {server} started {tool} as a background task, which is not supported; it may still act; nothing was retried"
        )),
        Ok(_) => unknown(format!(
            "mcp server {server} answered {tool} with an unexpected result; it may have acted; nothing was retried"
        )),
        Err(ServiceError::McpError(error)) => failure(format!(
            "mcp server {server} refused {tool}: {} ({})",
            error.message, error.code.0
        )),
        Err(ServiceError::Timeout { .. }) => unknown(format!(
            "mcp call {server}/{tool} did not finish within {} s and was cancelled; it may have acted; nothing was retried",
            timeout.as_secs()
        )),
        Err(error) => settle_transport_error(entry, &connection, server, tool, &error).await,
    }
}

async fn settle_transport_error(
    entry: &Entry,
    connection: &Arc<Connection>,
    server: &plakous_tools::McpServerName,
    tool: &str,
    error: &ServiceError,
) -> Result<CallResult, ToolRuntimeError> {
    tracing::warn!(%server, %tool, %error, "mcp call transport failed");
    // An HTTP failure leaves the transport running; a dead transport is
    // found and restarted at the next call.
    match http::classify(error) {
        Some(HttpRefusal::Credentials(status)) => {
            return failure(format!(
                "{}; the call did not run",
                http::credentials_text(server, status)
            ));
        }
        Some(HttpRefusal::NotSent) => {
            return failure(format!(
                "mcp server {server} could not be reached ({error}); the call was not sent"
            ));
        }
        Some(HttpRefusal::Unknown) => {
            return unknown(format!(
                "mcp call {server}/{tool} failed over HTTP ({error}); it may have acted; nothing was retried"
            ));
        }
        None => {}
    }
    entry.crashed(connection).await;
    Ok(
        match ToolOutcome::outcome_unknown_text(format!(
            "mcp server {server} exited during the call; it may have acted; nothing was retried"
        )) {
            Ok(outcome) => CallResult::Outcome(outcome),
            Err(_) => CallResult::Outcome(plakous_tools::internal::runtime_outcome_unknown()),
        },
    )
}

fn failure(text: String) -> Result<CallResult, ToolRuntimeError> {
    ToolOutcome::failure_text(text).map(CallResult::Outcome)
}

fn unknown(text: String) -> Result<CallResult, ToolRuntimeError> {
    ToolOutcome::outcome_unknown_text(text).map(CallResult::Outcome)
}
