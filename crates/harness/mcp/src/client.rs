//! The rmcp client handler: one workspace root, progress routing, and
//! ignored tool-list changes.

#![allow(
    deprecated,
    reason = "roots are deprecated by SEP-2577 yet remain how a server learns the workspace root"
)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use plakous_tools::{McpServerName, ProgressSink};
use rmcp::model::{
    ClientCapabilities, ClientConfig, Implementation, ListRootsResult, ProgressNotificationParam,
    ProgressToken, ResourceUpdatedNotificationParam, Root, RootsCapabilities,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::{ClientHandler, ErrorData, RoleClient};

/// Shared state of one connection's handler.
pub(crate) struct HandlerState {
    server: McpServerName,
    root_uri: String,
    progress: Mutex<HashMap<ProgressToken, ProgressSink>>,
}

impl HandlerState {
    pub(crate) fn new(server: McpServerName, workspace_root: &Path) -> Arc<Self> {
        Arc::new(Self {
            server,
            root_uri: file_uri(workspace_root),
            progress: Mutex::new(HashMap::new()),
        })
    }

    /// Routes progress for one in-flight call until the guard drops.
    pub(crate) fn route_progress(
        self: &Arc<Self>,
        token: ProgressToken,
        sink: ProgressSink,
    ) -> ProgressRoute {
        self.progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(token.clone(), sink);
        ProgressRoute {
            state: Arc::clone(self),
            token,
        }
    }

    fn report(&self, params: &ProgressNotificationParam) {
        let progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(sink) = progress.get(&params.progress_token) else {
            return;
        };
        let (message, completed, total) = progress_report(params);
        // Progress is a transient observation; a rejected update is dropped.
        let _ = sink.report(message, completed, total);
    }
}

/// Unregisters a call's progress route on drop.
pub(crate) struct ProgressRoute {
    state: Arc<HandlerState>,
    token: ProgressToken,
}

impl Drop for ProgressRoute {
    fn drop(&mut self) {
        self.state
            .progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.token);
    }
}

/// The client side of one MCP connection.
pub(crate) struct Handler(pub(crate) Arc<HandlerState>);

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        let mut roots = RootsCapabilities::default();
        roots.list_changed = Some(false);
        let mut capabilities = ClientCapabilities::default();
        capabilities.roots = Some(roots);
        ClientConfig::new(
            capabilities,
            Implementation::new("plakous", env!("CARGO_PKG_VERSION")),
        )
    }

    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ListRootsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListRootsResult::new(vec![Root::new(
            self.0.root_uri.clone(),
        )])))
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.0.report(&params);
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        tracing::info!(server = %self.0.server, "mcp server reported a resource list change");
    }

    async fn on_resource_updated(
        &self,
        _params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        // Nothing subscribes; an unsolicited update is logged only.
        tracing::info!(server = %self.0.server, "mcp server reported a resource update");
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        tracing::info!(server = %self.0.server, "mcp server reported a prompt list change");
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        // Running sessions keep their frozen tools for prompt-cache stability.
        tracing::info!(
            server = %self.0.server,
            "mcp server reported a tool list change; running sessions keep their frozen tools"
        );
    }
}

/// `file://` URI of an absolute path, percent-encoding every byte outside
/// the unreserved set and `/`.
pub(crate) fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(*byte));
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

/// Bounded progress text and integral units for one MCP progress update.
fn progress_report(params: &ProgressNotificationParam) -> (String, Option<u64>, Option<u64>) {
    let units = |value: f64| {
        (0.0..=9_007_199_254_740_991.0)
            .contains(&value)
            .then(|| format!("{value:.0}").parse::<u64>().ok())
            .flatten()
    };
    let total = params.total.and_then(units);
    let completed = units(params.progress).map(|done| total.map_or(done, |all| done.min(all)));
    let percent = match (completed, total) {
        (Some(done), Some(all)) if all > 0 => Some(done.saturating_mul(100) / all),
        _ => None,
    };
    let text = params.message.as_deref().unwrap_or("").trim();
    let mut message = match percent {
        Some(percent) if text.is_empty() => format!("{percent}%"),
        Some(percent) => format!("{percent}% {text}"),
        None => text.to_owned(),
    };
    let mut end = message.len().min(256);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    (message, completed, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_roots_are_percent_encoded_file_uris() {
        assert_eq!(file_uri(Path::new("/w/a b/c%")), "file:///w/a%20b/c%25");
        assert_eq!(file_uri(Path::new("/w/caf\u{e9}")), "file:///w/caf%C3%A9");
    }

    #[test]
    fn progress_is_a_percentage_with_the_server_message() {
        let mut params = ProgressNotificationParam::new(
            ProgressToken(rmcp::model::NumberOrString::Number(1)),
            42.0,
        );
        params.total = Some(100.0);
        params.message = Some("indexing".into());
        assert_eq!(
            progress_report(&params),
            ("42% indexing".into(), Some(42), Some(100))
        );
        params.progress = 150.0;
        params.message = None;
        assert_eq!(
            progress_report(&params),
            ("100%".into(), Some(100), Some(100))
        );
        params.total = None;
        params.progress = f64::NAN;
        params.message = Some("x".repeat(300));
        let (message, completed, total) = progress_report(&params);
        assert_eq!((message.len(), completed, total), (256, None, None));
    }
}
