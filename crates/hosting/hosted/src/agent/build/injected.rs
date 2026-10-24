//! Session construction over an injected runtime.

use super::session::{
    AttachedCodingSession, CodingSessionOpenFailure, RuntimeWording, SeededHistory, SessionSource,
    build_or_open,
};
use super::tools::bind_node_tool_plan;
use crate::agent::ProductStateRoots;
use crate::tools::ProductToolExtensions;
use plakous::sessions::{
    AgentSessions, AgentSessionsError, PortableSessionHistory, SessionLocator, SessionOperationId,
    SessionRuntimeBinding,
};
use plakous::subscriptions::NativeProvider;
use std::sync::Arc;

/// The registrations a root session's frozen optional tools bind to, for an
/// injected offline runtime that compiles its own registry over them: the
/// same plan a native Codex runtime binds, MCP tools rebuilt from their
/// frozen records when the record has them.
///
/// # Errors
///
/// Returns the bind refusal's one line.
pub fn injected_root_tool_registrations(
    frozen: &crate::frozen_tools::FrozenRootTools,
    session: &crate::sessions::SessionRecord,
    state_roots: &ProductStateRoots,
) -> Result<Vec<plakous::tools::ToolRegistration>, String> {
    let tools = frozen.selection();
    let plan = bind_node_tool_plan(
        &tools,
        crate::tool_plan::ToolRoute::native(NativeProvider::Codex),
        state_roots
            .roster_source()
            .account_name(NativeProvider::Codex),
        &session.workspace_root,
        state_roots,
    )
    .map_err(|failure| failure.message().to_owned())?;
    let extensions = ProductToolExtensions::empty()
        .with_plan(&plan)
        .map_err(|failure| failure.message().to_owned())?;
    Ok(extensions.registrations().to_vec())
}

pub async fn build_or_open_bound_session(
    sessions: Arc<AgentSessions>,
    locator: Option<SessionLocator>,
    seed: Option<PortableSessionHistory>,
    operation_id: SessionOperationId,
    runtime: SessionRuntimeBinding,
) -> Result<AttachedCodingSession, CodingSessionOpenFailure> {
    Box::pin(build_or_open(
        SessionSource::Runtime {
            sessions,
            runtime: Box::new(runtime),
            topology_owner: None,
            output_bound_rederived: None,
            wording: &INJECTED_WORDING,
        },
        locator,
        seed.map(SeededHistory::new),
        operation_id,
    ))
    .await
}

/// An injected runtime is not checked against the product's tool
/// projection, and its refusals name the injection.
const INJECTED_WORDING: RuntimeWording = RuntimeWording {
    checked: false,
    reopen_failed: injected_reopen_failed,
    reopen_needs_recovery: "injected exact reopen requires recovery",
    reopen_unavailable: "injected exact reopen was unavailable",
    create_failed: "injected session create failed",
    create_unavailable: "injected session create was unavailable",
};

fn injected_reopen_failed(error: &AgentSessionsError) -> String {
    format!("injected exact reopen failed: {error}")
}
