use super::App;
use super::test_support::{key, launch_basic};
use crate::tui::{CompositionSelection, HostEvent, ProtocolSelection};
use crossterm::event::KeyCode;

fn binding(session: &str) -> HostEvent {
    HostEvent::Binding {
        session_id: Some(session.into()),
        account: Some("codex".into()),
        model: Some("gpt-test".into()),
        effort: None,
        extra: None,
        composition: CompositionSelection::Direct,
        protocol: ProtocolSelection::Auto,
        effective_presentation: "direct".into(),
    }
}

#[test]
fn session_switch_retires_source_usage_waits_queue_and_tree_but_keeps_preferences() {
    {
        let mut app = App::new(launch_basic(), true);
        app.apply_host_event(binding("source"));
        app.last_turn_usage = Some((1, 2, 3, 4, 5, Some(1), Some(1)));
        app.last_prompt_at = Some(app.now());
        app.context_usage.total_window = Some(1000);
        app.credential_wait = Some("codex".into());
        app.request_slot_wait = Some("slots".into());
        app.cancel_pending = true;
        app.tree_restored_paused = true;
        app.tree_live_descendants = 3;
        app.composer_chrome
            .sent_steers
            .push_back(Some("status update".into()));
        app.composer_chrome.sent_steers.push_back(None);
        app.open_approval = Some("source-approval".into());
        app.phase_started = Some(app.now());
        app.queue_focused = true;
        app.queue_selected = 5;
        app.pending_queue_edit = Some(("source-item".into(), "edit".into()));
        app.prompt_history = vec!["source-prompt".into()];
        app.recovered_drafts = vec!["source-recovered-draft".into()];
        app.history_browse = Some(0);
        app.input = "ordinary unsent draft".into();
        let prefs = app.render_prefs;
        app.capacity_recovery = Some(crate::tui::ContextCapacityRecovery::Compactable);
        app.capacity_recovery_action = Some(ratatui::layout::Rect::new(1, 1, 4, 1));
        app.usage_limits.stale_age_seconds = Some(37);
        app.subscription_usage = crate::tui::presentation::SubscriptionUsageView::Read;
        app.refresh_failure_notes = vec!["account-level refresh failed".into()];
        let limits = app.usage_limits.clone();
        let subscription = app.subscription_usage.clone();
        app.apply_host_event(binding("successor"));
        assert!(app.last_turn_usage.is_none());
        assert!(app.last_prompt_at.is_none());
        assert!(app.context_usage.total_window.is_none());
        assert!(app.credential_wait.is_none());
        assert!(app.request_slot_wait.is_none());
        assert!(!app.cancel_pending);
        assert!(!app.tree_restored_paused);
        assert_eq!(app.tree_live_descendants, 0);
        assert!(app.composer_chrome.sent_steers.is_empty());
        assert!(app.open_approval.is_none());
        assert!(app.phase_started.is_none());
        assert!(!app.queue_focused);
        assert_eq!(app.queue_selected, 0);
        assert!(app.pending_queue_edit.is_none());
        assert_eq!(app.prompt_history, [] as [String; 0]);
        assert_eq!(app.recovered_drafts, [] as [String; 0]);
        assert!(app.history_browse.is_none());
        assert_eq!(app.input, "ordinary unsent draft");
        assert_eq!(app.render_prefs, prefs);
        assert!(app.capacity_recovery.is_none());
        assert!(app.capacity_recovery_action.is_none());
        assert_eq!(app.usage_limits, limits);
        assert_eq!(app.subscription_usage, subscription);
        assert_eq!(app.refresh_failure_notes, ["account-level refresh failed"]);
    }
}

#[test]
fn same_session_binding_and_failed_open_keep_source_observations() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(binding("source"));
    app.last_turn_usage = Some((1, 2, 3, 4, 5, Some(1), Some(1)));
    app.apply_host_event(HostEvent::SessionOpening);
    app.apply_host_event(binding("source"));
    assert!(app.last_turn_usage.is_some());
}

#[test]
fn blocked_usage_io_does_not_block_typing_or_publish_after_a_session_switch() {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut app = App::new(launch_basic(), true);
    app.usage_decoration = Some(crate::config::UsageDecorationReader::with_reader(
        move |view| {
            if view.stale_age_seconds == Some(1) {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            vec![view.stale_age_seconds.unwrap().to_string()]
        },
        || {},
    ));
    let mut limits = crate::tui::providers::unread_usage_limits();
    limits.stale_age_seconds = Some(1);
    app.apply_host_event(HostEvent::UsageLimits(limits.clone()));
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    // The reader cannot finish until this UI work has finished: no timing assertion.
    app.handle_key(key(KeyCode::Char('x')));
    assert_eq!(app.input, "x");
    app.apply_host_event(binding("successor"));
    limits.stale_age_seconds = Some(2);
    app.apply_host_event(HostEvent::UsageLimits(limits));
    release_tx.send(()).unwrap();
    app.usage_decoration.as_ref().unwrap().wait();
    app.poll_usage_decoration();
    assert_eq!(app.refresh_failure_notes, ["2"]);
}

#[test]
fn session_switch_invalidates_a_blocked_read_without_a_successor_usage_request() {
    let (entered, seen) = std::sync::mpsc::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let mut launch = launch_basic();
    launch.usage_decoration = Some(crate::config::UsageDecorationReader::with_reader(
        move |_| {
            entered.send(()).unwrap();
            blocked.recv().unwrap();
            vec!["source account decoration".into()]
        },
        || {},
    ));
    let mut app = App::new(launch, true);
    app.apply_host_event(binding("source"));
    app.apply_host_event(HostEvent::UsageLimits(
        crate::tui::providers::unread_usage_limits(),
    ));
    seen.recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let mut successor = binding("successor");
    if let HostEvent::Binding { account, .. } = &mut successor {
        *account = Some("claude".into());
    }
    app.apply_host_event(successor);
    assert_eq!(app.account.as_deref(), Some("claude"));
    // No successor UsageLimits request may mask a missing boundary fence.
    release.send(()).unwrap();
    let reader = app.usage_decoration.as_mut().unwrap();
    reader.wait_idle();
    assert_eq!(reader.poll(), None);
}

#[test]
fn same_session_clear_view_preserves_published_queue_and_history() {
    let mut app = App::new(launch_basic(), true);
    app.prompt_history = vec!["target history".into()];
    app.queue_drain = crate::tui::QueueDrainState::RestoredPaused;
    app.local_queue = vec![super::test_support::queue_item(
        "target-id",
        1,
        "target queued",
    )];
    let queue = app.local_queue.clone();
    app.apply_host_event(HostEvent::ClearView);
    assert_eq!(app.local_queue, queue);
    assert_eq!(app.queue_drain, crate::tui::QueueDrainState::RestoredPaused);
    assert_eq!(app.prompt_history, ["target history"]);
}
