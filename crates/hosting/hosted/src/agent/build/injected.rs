//! Session construction over an injected runtime.

use crate::CliFailure;

use super::session::{
    AttachedCodingSession, CodingSessionOpenFailure, SeededHistory, create_request, reopen_refusal,
};
use super::tools::bind_node_tool_plan;
use crate::agent::ProductStateRoots;
use crate::tools::ProductToolExtensions;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, OpenSessionOutcome, OpenSessionRequest,
    PortableSessionHistory, SessionLocator, SessionOperationId, SessionRuntimeBinding,
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
    let reachability_probe = plakous::sessions::internal::reachability_probe(&runtime);
    if let Some(locator) = locator {
        if seed.is_some() {
            return Err(CliFailure::new("a carried history seeds only a new session").into());
        }
        let opened = sessions
            .open(OpenSessionRequest::new(operation_id, locator, runtime))
            .await
            .map_err(|error| CodingSessionOpenFailure {
                failure: CliFailure::new(format!("injected exact reopen failed: {error}")),
                current_locator: error.current_locator(),
            })?;
        return match opened {
            OpenSessionOutcome::Opened { session, .. } => {
                let locator = session.locator();
                Ok(AttachedCodingSession {
                    output_bound_rederived: None,
                    reachability_probe: reachability_probe.clone(),
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: None,
                    locator,
                    topology_owner: None,
                })
            }
            OpenSessionOutcome::RestoredWorking { session, turn, .. } => {
                let locator = session.locator();
                Ok(AttachedCodingSession {
                    output_bound_rederived: None,
                    reachability_probe: reachability_probe.clone(),
                    sessions: Arc::clone(&sessions),
                    opened: Some(session),
                    recovered_turn: Some(turn),
                    locator,
                    topology_owner: None,
                })
            }
            OpenSessionOutcome::RecoveryRequired {
                retry_after_seconds,
                ..
            } => Err(reopen_refusal(
                "injected exact reopen requires recovery",
                retry_after_seconds,
            )
            .into()),
            _ => Err(CliFailure::new("injected exact reopen was unavailable").into()),
        };
    }
    match sessions
        .create(create_request(
            operation_id,
            runtime,
            seed.map(SeededHistory::new),
        ))
        .await
        .map_err(|error| {
            CliFailure::new(format!(
                "injected session create failed ({:?}): {error}",
                error.kind()
            ))
        })? {
        CreateSessionOutcome::Created { session, .. } => {
            let locator = session.locator();
            Ok(AttachedCodingSession {
                output_bound_rederived: None,
                reachability_probe: reachability_probe.clone(),
                sessions: Arc::clone(&sessions),
                opened: Some(session),
                recovered_turn: None,
                locator,
                topology_owner: None,
            })
        }
        _ => Err(CliFailure::new("injected session create was unavailable").into()),
    }
}
