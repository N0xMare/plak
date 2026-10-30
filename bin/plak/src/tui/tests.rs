use super::{HostEvent, QueueDrainState, QueueItem, TuiError, TuiLaunch, UserAction, control};
use plakous_hosted::sessions::{NoticeScope, TranscriptRow};
use std::path::PathBuf;

#[test]
fn launch_defaults_are_stable() {
    let launch = TuiLaunch::default();
    assert_eq!(launch.session_id, None);
    assert_eq!(launch.workspace, PathBuf::from("."));
    assert_eq!(launch.account, None);
    assert_eq!(launch.model, None);
    assert_eq!(launch.effort, None);
    assert!(!launch.agent_enabled);
}

#[test]
fn error_display_is_clear() {
    assert!(
        TuiError::NotATty
            .to_string()
            .contains("interactive terminal")
    );
    assert!(TuiError::Io("boom".into()).to_string().contains("boom"));
}

#[test]
fn control_plane_routing_table() {
    assert_eq!(
        control::route_submit(false, false, "p".into()),
        UserAction::Prompt("p".into())
    );
    assert_eq!(
        control::route_submit(true, false, "f".into()),
        UserAction::FollowUp("f".into())
    );
    assert_eq!(
        control::route_submit(true, true, "s".into()),
        UserAction::Steer("s".into())
    );
    assert_eq!(control::route_ctrl_c(true), UserAction::Interrupt);
    assert_eq!(control::route_ctrl_c(false), UserAction::Quit);
    assert!(control::is_quit_command("/quit"));
    assert!(control::is_drop_command("/drop"));
}
#[test]
fn host_event_variants_are_constructible() {
    let _ = HostEvent::Row(TranscriptRow::user("x"));
    let _ = HostEvent::ProviderSnapshot {
        providers: Vec::new(),
    };
    let _ = HostEvent::Activity {
        label: "starting".into(),
        active: true,
    };
    let _ = HostEvent::Status {
        phase: "idle".into(),
        queue_len: 0,
        detail: None,
    };
    let _ = HostEvent::QueueSnapshot {
        items: vec![QueueItem {
            rejected: false,
            id: "q1".into(),
            revision: 2,
            text: "same".into(),
            images: Vec::new(),
        }],
        drain: QueueDrainState::RestoredPaused,
    };
    let _ = HostEvent::SetBusy(true);
    let _ = HostEvent::PromptHistorySnapshot(vec![]);
    let _ = HostEvent::UserMessage {
        text: "u".into(),
        sent_at_ms: None,
    };
    let _ = HostEvent::AssistantDelta("d".into());
    let _ = HostEvent::AssistantDone;
    let _ = HostEvent::ToolLine {
        call_id: None,
        parent_call_id: None,
        cell_id: None,
        kind: crate::tui::presentation::ActivityKind::Tool,
        name: "t".into(),
        state: "start".into(),
        subject: None,
        detail: None,
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    };
    let _ = HostEvent::Row(TranscriptRow::info(NoticeScope::Session, "s"));
    let _ = HostEvent::Row(TranscriptRow::error(NoticeScope::Host, "e"));
    let _ = HostEvent::Binding {
        session_id: None,
        account: None,
        model: None,
        effort: None,
        extra: None,
        composition: crate::tui::CompositionSelection::FollowCatalog,
        protocol: crate::tui::ProtocolSelection::Auto,
        effective_presentation: "direct".into(),
    };
    let _ = HostEvent::ClearView;
    let _ = HostEvent::Usage(crate::tui::presentation::ContextUsage {
        breakdown: None,
        prepared_input: None,
        total_window: None,
        admitted_ceiling: None,
        gauge_percent: None,
        display_percent: None,
        ceiling_excess: None,
        evidence: "unavailable".into(),
        non_guarantee: None,
        status: "unavailable".into(),
    });
    let _ = HostEvent::TurnUsage {
        input_tokens: 10,
        output_tokens: 4,
        reasoning_tokens: 2,
        cached_input_tokens: 3,
        cache_write_tokens: 0,
        model_operations: Some(2),
        reporting_model_operations: Some(1),
        peak_prepared_input_tokens: None,
        terminal: true,
    };
}
