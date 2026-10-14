//! Snapshot-and-tail observation through the curated component.

use plakous_agent::{
    Agent, AgentEvent, AgentEventKind, EventCursor, EventSubscription, ToolCallerClass,
    ToolCallerLineageView, ViewAttachment, ViewError, ViewRequest,
};

pub(crate) fn surface_is_nameable() {
    let _ = std::any::type_name::<AgentEvent>();
    let _ = std::any::type_name::<EventCursor>();
    let _ = std::any::type_name::<EventSubscription>();
    let _ = std::any::type_name::<ToolCallerClass>();
    let _ = std::any::type_name::<ToolCallerLineageView>();
    assert_eq!(
        ToolCallerLineageView::direct().class(),
        ToolCallerClass::Direct
    );
    assert!(ToolCallerLineageView::direct().parent_call_id().is_none());
}

/// Correlates one nested tool observation to the marker call that owns its cell.
///
/// Tool lifecycle events carry caller lineage at every visibility, so an
/// external consumer groups a programmatic cell without parsing display text.
#[allow(dead_code)]
pub(crate) fn owning_marker_call(event: &AgentEvent) -> Option<String> {
    let lineage = event.kind().tool_caller_lineage()?;
    match lineage.class() {
        ToolCallerClass::Direct => None,
        _ => lineage
            .parent_call_id()
            .map(|call_id| call_id.as_uuid().to_string()),
    }
}

/// Names the tool lifecycle variants that carry lineage.
#[allow(dead_code)]
pub(crate) fn is_tool_lifecycle(kind: &AgentEventKind) -> bool {
    matches!(
        kind,
        AgentEventKind::ToolAuthorized { .. }
            | AgentEventKind::ToolStarted { .. }
            | AgentEventKind::ToolProgress { .. }
            | AgentEventKind::ToolPreparationRejected { .. }
            | AgentEventKind::ToolCompleted { .. }
    )
}

#[allow(dead_code)]
pub(crate) fn prepared_request_tool_count(kind: &AgentEventKind) -> Option<usize> {
    match kind {
        AgentEventKind::RequestPrepared {
            input_parts,
            occupied_tokens,
            prepared_input_upper_bound,
            tool_names,
        } => {
            let _ = (input_parts, occupied_tokens, prepared_input_upper_bound);
            Some(tool_names.len())
        }
        _ => None,
    }
}

#[allow(dead_code)]
pub(crate) async fn snapshot(
    agent: &Agent,
    request: ViewRequest,
) -> Result<ViewAttachment, ViewError> {
    agent.attach(request).await
}
