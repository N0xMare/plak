use super::*;

fn activity(id: &str, label: &str, correlation_id: Option<&str>) -> ActivityItem {
    ActivityItem {
        parent_call_id: None,
        cell_id: None,
        id: id.to_owned(),
        correlation_id: correlation_id.map(str::to_owned),
        kind: ActivityKind::Tool,
        label: label.to_owned(),
        subject: Some("$ cargo test".to_owned()),
        detail: None,
        state: ActivityState::Running,
        started_at_ms: Some(100),
        finished_at_ms: None,
        duration_ms: None,
    }
}

#[test]
fn durable_transcript_payload_ceiling_has_presentation_identity_headroom() {
    let mut snapshot = PresentationSnapshot::empty(1);
    snapshot.items.push(TranscriptItem {
        key: super::super::transcript::RowKey::new(1, super::super::transcript::Lane::Runtime, 0),
        row: TranscriptRow::assistant("x".repeat(16 * 1024 * 1024)),
    });
    assert_eq!(snapshot.validate(), Ok(()));
    snapshot.items.push(TranscriptItem {
        key: super::super::transcript::RowKey::new(2, super::super::transcript::Lane::Runtime, 0),
        row: TranscriptRow::assistant("x".repeat(3 * 1024 * 1024)),
    });
    assert_eq!(snapshot.validate(), Err(PresentationError::ByteLimit));
}

#[test]
fn binding_updates_distinguish_rebind_and_model_switch() {
    let a = BindingView {
        session_id: Some("session-a".into()),
        model: Some("grok-4.6".into()),
        ..BindingView::default()
    };
    let b = BindingView {
        session_id: Some("session-b".into()),
        model: Some("gpt-5.6-sol".into()),
        ..BindingView::default()
    };
    let rebound = BindingUpdate::rebound(a.clone(), b.clone());
    assert!(matches!(
        rebound.transition,
        Some(BindingTransition::Rebound { .. })
    ));
    let switched = BindingUpdate::model_switched(a.model.clone(), b);
    assert!(matches!(
        switched.transition,
        Some(BindingTransition::ModelSwitched { .. })
    ));
}

#[test]
fn activity_labels_name_web_and_mcp_calls() {
    for (name, label) in [
        ("web_search", "searching"),
        ("WebSearch", "searching"),
        ("x_search", "searching"),
        ("web_fetch", "reading"),
        ("WebFetch", "reading"),
        ("fetch", "reading"),
        ("fetch_url", "reading"),
        ("web__run", "reading"),
        ("mcp__docs__search", "calling mcp:docs"),
        ("mcp__git_hub__list_issues", "calling mcp:git_hub"),
        ("mcp__", "working"),
        ("mcp__Docs__x", "working"),
        ("mcp__docs__", "working"),
    ] {
        assert_eq!(activity_label(name, ActivityKind::Tool), label, "{name}");
    }
    assert_eq!(
        activity_label("mcp__docs__search", ActivityKind::Compose),
        "working"
    );
}

#[test]
fn mcp_calls_split_their_exposed_name() {
    assert_eq!(
        McpCall::parse("mcp__git_hub__list__issues"),
        Some(McpCall {
            server: "git_hub",
            tool: "list__issues"
        })
    );
    assert_eq!(McpCall::parse("search_repository"), None);
}

#[test]
fn activity_derives_duration() {
    let mut row = activity("tool-1", "search_repository", None);
    row.state = ActivityState::Succeeded;
    row.finished_at_ms = Some(145);
    assert_eq!(row.with_derived_duration().duration_ms, Some(45));
}
