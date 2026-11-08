//! Canonical tool rows; callers supply their authoritative text and lineage.

use crate::runtime::events::{
    completed_tool_detail, completed_tool_state, projected_tool_subject, wait_settled, wait_started,
};
use crate::sessions::ToolItem;
use plakous_host_protocol::views::transcript::WaitFacts;

pub(super) struct CanonicalToolText {
    pub(super) subject: Option<String>,
    pub(super) detail: Option<String>,
    pub(super) wait: Option<WaitFacts>,
}

pub(super) fn canonical_tool_row(
    call_id: &str,
    canonical: &str,
    exposed: &str,
    text: CanonicalToolText,
) -> ToolItem {
    ToolItem {
        wire_operation: None,
        call_id: call_id.to_owned(),
        name: canonical.to_owned(),
        exposed_name: (exposed != canonical).then(|| exposed.to_owned()),
        kind: crate::activity::tool_activity_kind(canonical),
        state: "running".to_owned(),
        subject: text.subject,
        detail: text.detail,
        started_at_ms: None,
        finished_at_ms: None,
        parent_call_id: None,
        cell_id: None,
        wait: text.wait,
    }
}

pub(crate) fn projected_tool_row(
    call_id: &str,
    canonical: &str,
    exposed: &str,
    arguments: Option<&str>,
) -> ToolItem {
    let args =
        arguments.and_then(|arguments| serde_json::from_str::<serde_json::Value>(arguments).ok());
    canonical_tool_row(
        call_id,
        canonical,
        exposed,
        CanonicalToolText {
            subject: projected_tool_subject(canonical, arguments),
            detail: crate::activity::tool_detail_text_for(
                canonical,
                Some(exposed),
                arguments,
                None,
            ),
            wait: wait_started(canonical, None, args.as_ref()),
        },
    )
}

pub(crate) fn settle_tool_row(
    tool: &mut ToolItem,
    succeeded: bool,
    preview: &str,
    wait_ids: Option<&[String]>,
) {
    tool.state = completed_tool_state(&tool.name, Some((succeeded, preview)));
    if let Some(detail) = completed_tool_detail(
        &tool.name,
        tool.exposed_name.as_deref(),
        tool.detail.as_deref(),
        Some(preview),
    ) {
        tool.detail = Some(detail);
    }
    let value = serde_json::from_str::<serde_json::Value>(preview).ok();
    tool.wait = wait_settled(
        &tool.name,
        tool.wait.take(),
        value.as_ref(),
        succeeded,
        tool.duration_ms(),
        wait_ids,
    );
}
