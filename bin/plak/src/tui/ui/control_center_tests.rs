//! Menu tab tests.

use super::terminal::{commit_sent_action, dispatch_action, osc52_sequence, send_user_action};
use super::test_support::{
    blocks_contain, key, key_ctrl, launch_basic, left_click, left_mouse, lines_text, pin_clock,
    queue_item, screen_text, serialize_frame, settings_fixture, tree_node, tree_snapshot, typed,
    usage_body_text, usage_fixture, usage_offline_fixture,
};
use super::{App, format_turn_usage};
use crate::tui::blocks::ScrollBlock;
use crate::tui::presentation::{ContextUsage, PreparedInputUsage};
use crate::tui::{HostEvent, QueueDrainState, SettingsSnapshot, UserAction, user_action_channel};
use crate::tui::{SurfaceIntent, SurfaceObservation};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use plakous_host_protocol::topology::AttemptStatus;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn menu_reopens_last_tab_and_keybinds_start_while_typing() {
    let mut app = App::new(launch_basic(), true);
    app.activate_control_tab(super::ControlTab::Session);
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key_ctrl(KeyCode::Char('x')));
    assert_eq!(app.control_tab, Some(super::ControlTab::Session));
    app.open_control_tab(super::ControlTab::Keybinds);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let short = screen_text(&terminal);
    assert!(
        short.contains("Menu") && short.contains("While typing"),
        "{short}"
    );
    assert!(!short.contains("any printable key") && !short.contains("drag"));
    app.handle_key(key(KeyCode::Enter));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let full = screen_text(&terminal);
    assert!(full.contains("All keybinds"), "{full}");
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.control_tab, Some(super::ControlTab::Keybinds));
}

#[test]
fn keybinds_pages_cover_width_classes_and_all_theme_pairs() {
    for theme in crate::tui::SettingsTheme::ALL {
        for width in [40, 60, 80, 120] {
            let mut app = App::new(launch_basic(), true);
            app.persisted_theme = theme;
            app.theme = super::theme_for(theme);
            app.open_control_tab(super::ControlTab::Keybinds);
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert!(
                screen_text(&terminal).contains("While typing"),
                "{theme:?} {width}"
            );
            assert!(!screen_text(&terminal).contains("any printable key"));
            app.handle_key(key(KeyCode::Enter));
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert!(
                screen_text(&terminal).contains("All keybinds"),
                "{theme:?} {width}"
            );
            app.handle_key(key(KeyCode::Esc));
            assert!(!app.keybinds_full);
            assert_eq!(app.control_tab, Some(super::ControlTab::Keybinds));
        }
    }
}

#[test]
fn usage_limit_rows_fit_the_menu_body_without_word_clipping() {
    let app = usage_fixture();
    for width in [42, 64, 102] {
        let lines = app.usage_tab_lines(width);
        assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
        let text = lines_text(&lines);
        for word in ["resets", "23:44", "16:00"] {
            assert!(text.contains(word), "{text}");
        }
    }
}

#[test]
fn info_overlay_wheel_scrolls_without_a_track() {
    let mut app = App::new(launch_basic(), true);
    app.info_overlay = Some((
        "Info".into(),
        (0..40).map(|n| format!("row {n}\n")).collect(),
    ));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.handle_mouse(left_mouse(MouseEventKind::ScrollDown, 20, 10));
    assert_eq!(app.info_overlay_scroll.offset(), 1);
    app.handle_mouse(left_mouse(MouseEventKind::ScrollUp, 20, 10));
    assert_eq!(app.info_overlay_scroll.offset(), 0);
}

#[test]
fn long_keybinds_overlay_scrolls_without_a_track() {
    let mut app = App::new(launch_basic(), true);
    app.open_control_tab(super::ControlTab::Keybinds);
    app.handle_key(key(KeyCode::Enter));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.handle_key(key(KeyCode::PageDown));
    assert!(app.control_scroll[super::ControlTab::Keybinds.index()].offset() > 0);
    app.handle_key(key(KeyCode::Home));
    let popup = app.control_popup.unwrap();
    app.handle_mouse(left_mouse(
        MouseEventKind::ScrollDown,
        popup.x + 2,
        popup.y + 4,
    ));
    assert!(app.control_scroll[super::ControlTab::Keybinds.index()].offset() > 0);
    app.handle_key(key(KeyCode::End));
    let scroll = app.control_scroll[super::ControlTab::Keybinds.index()];
    assert_eq!(scroll.offset(), scroll.max_offset());
    assert!(!app.selection.has_range());
}

/// Spec 02 §5.5: "an unscrollable pane is a defect regardless of content
/// length" — PageDown/End/Home and the wheel move every tab.
#[test]
fn every_control_center_tab_scrolls() {
    for tab in super::ControlTab::ALL {
        let mut app = App::new(launch_basic(), true);
        app.pending_open_settings = true;
        app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
            SettingsSnapshot::default(),
        )));
        app.control_tab = Some(tab);
        app.templates = (0..12)
            .map(|index| crate::tui::TemplateRow {
                name: format!("template-{index}"),
                account: "codex".into(),
                model: "gpt-exact".into(),
                effort: Some("medium".into()),
                composition: "plakous-default".into(),
                tier: "standard".into(),
                unready_reason: None,
            })
            .collect();
        // A short viewport guarantees the content overflows on every tab.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();

        app.handle_key(key(KeyCode::End));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let bottom = app.control_scroll[tab.index()].offset();
        app.handle_key(key(KeyCode::Home));
        // Tabs that own a row cursor answer Home by moving the cursor; the
        // offset follows on the next frame, when the cursor is revealed.
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.control_scroll[tab.index()].offset(), 0, "{tab:?}");
        assert!(
            bottom > 0 || app.control_scroll[tab.index()].max_offset() == 0,
            "{tab:?} must either scroll or genuinely fit"
        );

        // The wheel reaches every tab too.
        app.handle_mouse(left_mouse(MouseEventKind::ScrollDown, 40, 12));
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::PageUp));
        assert!(
            app.control_tab.is_some(),
            "{tab:?} stayed open while scrolling"
        );
    }
}

#[test]
fn terminal_usage_rendering_preserves_reporting_coverage() {
    assert_eq!(
        format_turn_usage(10, 4, 2, 3, 0, 2, 2),
        "usage · input 10 · output 4 · reasoning 2 · provider-reported cached input 3 · provider-reported cache write 0 · coverage complete (2/2 model operations)"
    );
    assert_eq!(
        format_turn_usage(10, 4, 2, 3, 0, 2, 1),
        "usage · reported subtotal · input 10 · output 4 · reasoning 2 · provider-reported cached input 3 · provider-reported cache write 0 · coverage partial (1/2 model operations)"
    );
    assert_eq!(
        format_turn_usage(0, 0, 0, 0, 0, 2, 0),
        "usage · input unknown · output unknown · reasoning unknown · provider-reported cached input unknown · provider-reported cache write unknown · coverage missing (0/2 model operations)"
    );
    assert_eq!(
        format_turn_usage(0, 0, 0, 0, 0, 0, 0),
        "usage · input 0 · output 0 · reasoning 0 · provider-reported cached input 0 · provider-reported cache write 0 · coverage not applicable (0 model operations)"
    );

    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::TurnUsage {
        input_tokens: 10,
        output_tokens: 4,
        reasoning_tokens: 2,
        cached_input_tokens: 3,
        cache_write_tokens: 0,
        model_operations: Some(2),
        reporting_model_operations: Some(1),
        peak_prepared_input_tokens: None,
        terminal: true,
    });
    assert!(
        app.pane.blocks.is_empty(),
        "usage must not bloat conversation scrollback"
    );
    assert_eq!(
        app.last_turn_usage,
        Some((10, 4, 2, 3, 0, Some(2), Some(1)))
    );
}

#[test]
fn usage_overlay_separates_context_turn_coverage_and_unknown_limits() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::Usage(ContextUsage {
        breakdown: None,
        prepared_input: Some(PreparedInputUsage::UpperBound(1_020)),
        total_window: Some(1_100),
        admitted_ceiling: Some(crate::tui::presentation::AdmittedContextCeiling {
            tokens: 1_000,
            basis: "envelope".into(),
        }),
        gauge_percent: Some(100),
        display_percent: Some(102),
        ceiling_excess: Some(crate::tui::presentation::ContextCeilingExcess::UpperBoundExcess(20)),
        evidence: "reference-client-envelope".into(),
        non_guarantee: Some("not a provider hard cap".into()),
        status: "fits".into(),
    }));
    app.apply_host_event(HostEvent::TurnUsage {
        input_tokens: 10,
        output_tokens: 4,
        reasoning_tokens: 2,
        cached_input_tokens: 3,
        cache_write_tokens: 0,
        model_operations: Some(2),
        reporting_model_operations: Some(1),
        peak_prepared_input_tokens: None,
        terminal: true,
    });
    app.activate_control_tab(super::ControlTab::Usage);
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::OpenUsageOverlay));
    assert_eq!(app.control_tab, Some(super::ControlTab::Usage));
    let context = usage_body_text(&mut app, 120);
    // Occupancy is the header; the admission bound is a labelled fact with `≤`.
    assert!(context.contains("1.0k / 1.1k tokens (92.73%)"), "{context}");
    assert!(context.contains("Prepared input ≤1.0k tokens"), "{context}");
    assert!(
        context.contains("Admitted input 1.0k tokens (91% of window · envelope)"),
        "{context}"
    );
    assert!(context.contains("Qualification: not a provider hard cap"));
    // Without an enrolled trigger the UI reports enablement without inventing one.
    assert!(
        context.contains("Auto compaction is on; trigger not reported for this route."),
        "{context}"
    );

    // Honesty clause 1: the current turn's exact counters and coverage.
    let limits = usage_body_text(&mut app, 120);
    assert!(limits.contains("partial (1/2 operations)"), "{limits}");
    assert!(limits.contains("Loading provider allowance snapshots"));
    assert!(limits.contains("cached input 3"));
}

#[test]
fn the_limits_read_state_is_stated_rather_than_painted_as_connected() {
    let mut app = App::new(launch_basic(), true);
    assert!(usage_body_text(&mut app, 120).contains("Loading provider allowance snapshots"));

    app.apply_host_event(HostEvent::SubscriptionUsage(
        crate::tui::presentation::SubscriptionUsageView::Unavailable {
            reason: "credential is not ready".into(),
        },
    ));
    let unavailable = usage_body_text(&mut app, 120);
    assert!(unavailable.contains("Allowance read unavailable · credential is not ready"));
    // All four verticals are still listed, none painted as connected.
    for label in ["Codex", "SuperGrok", "Claude", "OpenRouter"] {
        assert!(unavailable.contains(label), "{label} must be listed");
    }
    assert!(!unavailable.contains(crate::tui::usage_tab::LIMIT_BAR_CELL));
    assert!(
        app.pane.blocks.is_empty(),
        "usage projections are inspector state only"
    );
}

#[test]
fn usage_overlay_calls_only_exact_excess_overflow() {
    let mut app = App::new(launch_basic(), true);
    app.context_usage = ContextUsage {
        breakdown: None,
        prepared_input: Some(PreparedInputUsage::Exact(1_020)),
        total_window: Some(1_000),
        admitted_ceiling: Some(crate::tui::presentation::AdmittedContextCeiling {
            tokens: 1_000,
            basis: "exact".into(),
        }),
        gauge_percent: Some(100),
        display_percent: Some(102),
        ceiling_excess: Some(crate::tui::presentation::ContextCeilingExcess::ExactOverflow(20)),
        evidence: "release-verified".into(),
        non_guarantee: None,
        status: "would-overflow".into(),
    };
    let body = usage_body_text(&mut app, 120);
    // An exact measurement carries no `≤` qualifier.
    assert!(body.contains("1.0k / 1.0k tokens (102.00%)"));
    assert!(!body.contains("≤"));
}

#[test]
fn shared_control_center_tabs_are_keyboard_and_mouse_modal() {
    let mut app = App::new(launch_basic(), true);
    app.activate_control_tab(super::ControlTab::Usage);
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::OpenUsageOverlay));
    assert_eq!(app.control_tab, Some(super::ControlTab::Usage));
    // Tab switches the outer tab; so do the arrows now that Usage, Context
    // and Session are peer tabs (spec 02 §5.5).
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.control_tab, Some(super::ControlTab::Context));
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.control_tab, Some(super::ControlTab::Session));
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.control_tab, Some(super::ControlTab::Keybinds));
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.control_tab, Some(super::ControlTab::Settings));
    app.handle_key(key(KeyCode::BackTab));
    assert_eq!(app.control_tab, Some(super::ControlTab::Keybinds));
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.control_tab, Some(super::ControlTab::Settings));

    let backend = TestBackend::new(90, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let usage = app
        .control_tab_hits
        .iter()
        .find(|(tab, _)| *tab == super::ControlTab::Usage)
        .map(|(_, rect)| *rect)
        .expect("usage tab hit target");
    assert_eq!(
        app.handle_mouse(left_click(usage.x, usage.y)),
        Some(UserAction::Extension(SurfaceIntent::OpenUsage))
    );
    assert_eq!(app.control_tab, Some(super::ControlTab::Usage));

    let before_cursor = app.input_cursor;
    let popup = app.control_popup.expect("control popup");
    app.handle_mouse(left_click(
        popup.x.saturating_add(1),
        popup.y.saturating_add(2),
    ));
    assert!(app.control_tab.is_some());
    assert_eq!(app.input_cursor, before_cursor);
    let close = app.control_close_hit.expect("close glyph");
    app.handle_mouse(left_click(close.x, close.y));
    assert!(app.control_tab.is_none());
}

#[test]
fn control_overlay_geometry_is_the_whole_terminal_and_floored() {
    // Menu is the full-page frame at every size the review uses.
    for (width, height) in [(80u16, 24u16), (120, 40), (200, 50)] {
        let mut app = App::new(launch_basic(), true);
        app.set_input("draft-under-menu".into());
        app.activate_control_tab(super::ControlTab::Usage);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let popup = app.control_popup.expect("control popup");
        assert_eq!(
            (popup.x, popup.y, popup.width, popup.height),
            (0, 0, width, height)
        );
        assert!(!app.control_tab_hits.is_empty(), "tabs are clickable");
        assert!(app.composer_inner.is_none(), "no composer hit under Menu");
        assert!(app.queue_area.is_none());
        assert!(app.status_agents_hit.is_none());
        assert!(app.status_processes_hit.is_none());
        let screen = serialize_frame(terminal.backend().buffer());
        assert!(
            !screen.contains("draft-under-menu"),
            "the main view is not drawn under Menu: {screen}"
        );
        app.handle_mouse(left_click(4, height.saturating_sub(2)));
        assert_eq!(app.input, "draft-under-menu");
        assert!(app.control_tab.is_some());
    }

    // Below the hard floor the shared too-small line is the frame, and every
    // hit rect is cleared so a stale click cannot fire (spec 02 §5.5).
    let mut app = App::new(launch_basic(), true);
    app.activate_control_tab(super::ControlTab::Usage);
    let backend = TestBackend::new(18, 5);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(app.control_popup.is_none(), "refuses to draw the frame");
    assert!(app.control_tab_hits.is_empty());
    assert!(app.control_close_hit.is_none());
    assert!(app.settings_hits.is_empty());
    let screen = serialize_frame(terminal.backend().buffer());
    assert!(screen.contains("terminal·too·small"), "{screen}");
}

#[test]
fn menu_and_activity_too_small_lines_are_goldens() {
    fn frame(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        serialize_frame(terminal.backend().buffer())
    }
    let mut menu = App::new(launch_basic(), true);
    menu.activate_control_tab(super::ControlTab::Settings);
    let menu_frame = frame(&mut menu, 40, 5);
    assert!(menu_frame.contains("Menu·"), "{menu_frame}");
    assert!(menu_frame.contains("terminal·too·small"), "{menu_frame}");
    let menu_path = "tests/goldens/menu-too-small-40x5.frame";
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::write(menu_path, &menu_frame).unwrap();
    } else {
        assert_eq!(
            menu_frame,
            std::fs::read_to_string(menu_path).expect("menu too-small golden"),
            "{menu_path}"
        );
    }

    let mut activity = App::new(launch_basic(), true);
    activity.activity_tab = Some(super::processes::ActivityTab::Agents);
    let activity_frame = frame(&mut activity, 40, 5);
    assert!(activity_frame.contains("Activity·"), "{activity_frame}");
    assert!(
        activity_frame.contains("terminal·too·small"),
        "{activity_frame}"
    );
    let activity_path = "tests/goldens/activity-too-small-40x5.frame";
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::write(activity_path, &activity_frame).unwrap();
    } else {
        assert_eq!(
            activity_frame,
            std::fs::read_to_string(activity_path).expect("activity too-small golden"),
            "{activity_path}"
        );
    }
}

#[test]
fn durable_draft_disposition_controls_restoration() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.set_input("preserve until durable".into());
    let action = app.submit_line(false).expect("prompt action");
    dispatch_action(&mut app, &action, Some(&sender));
    assert!(app.input.is_empty());
    assert_eq!(app.pending_drafts.len(), 1);
    assert!(
        app.pane.blocks.is_empty(),
        "mailbox acceptance must not echo"
    );

    app.apply_host_event(HostEvent::DraftRejected(action.clone()));
    assert_eq!(app.input, "preserve until durable");
    assert!(app.pending_drafts.is_empty());
    assert!(
        app.pane.blocks.is_empty(),
        "refused draft must not leave a false echo"
    );

    app.clear_input();
    assert!(send_user_action(&mut app, Some(&sender), action.clone()));
    app.apply_host_event(HostEvent::DraftAccepted(action));
    assert!(app.input.is_empty());
    assert!(app.pending_drafts.is_empty());
}

#[test]
fn host_disconnect_restores_unacknowledged_drafts_and_remains_quittable() {
    let (sender, receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    let action = UserAction::Prompt("recover after crash".into());
    assert!(send_user_action(&mut app, Some(&sender), action));
    drop(receiver);
    app.note_host_disconnected();
    assert_eq!(app.input, "recover after crash");
    assert!(app.host_disconnected);
    dispatch_action(&mut app, &UserAction::Quit, Some(&sender));
    assert!(app.should_quit);
}

/// P2: inject host events + keys in loop order (no real TTY).
#[test]
fn injected_event_loop_control_plane_sequence() {
    let (tx, rx) = mpsc::channel::<UserAction>();
    let (ev_tx, ev_rx) = mpsc::channel::<HostEvent>();
    let mut app = App::new(launch_basic(), true);

    let drain = |app: &mut App| {
        while let Ok(ev) = ev_rx.try_recv() {
            app.apply_host_event(ev);
        }
    };

    for ch in "first".chars() {
        assert!(app.handle_key(key(KeyCode::Char(ch))).is_none());
    }
    let action = app.handle_key(key(KeyCode::Enter));
    assert_eq!(action, Some(UserAction::Prompt("first".into())));
    tx.send(action.unwrap()).unwrap();

    ev_tx.send(HostEvent::SetBusy(true)).unwrap();
    ev_tx
        .send(HostEvent::Status {
            phase: "running".into(),
            queue_len: 0,
            detail: Some("turn started".into()),
        })
        .unwrap();
    ev_tx
        .send(HostEvent::QueueSnapshot {
            items: vec![queue_item("q-second", 1, "second")],
            drain: QueueDrainState::LiveAutoDrain,
        })
        .unwrap();
    drain(&mut app);
    assert!(app.busy);
    assert_eq!(app.phase, "running");

    for ch in "second".chars() {
        assert!(app.handle_key(key(KeyCode::Char(ch))).is_none());
    }
    let action = app.handle_key(key(KeyCode::Enter));
    assert_eq!(action, Some(UserAction::FollowUp("second".into())));
    tx.send(action.unwrap()).unwrap();

    ev_tx
        .send(HostEvent::Status {
            phase: "running".into(),
            queue_len: 1,
            detail: Some("queued follow-up".into()),
        })
        .unwrap();
    drain(&mut app);
    assert_eq!(app.queue_len, 1);

    for ch in "now".chars() {
        assert!(app.handle_key(key(KeyCode::Char(ch))).is_none());
    }
    let action = app.handle_key(key_ctrl(KeyCode::Enter));
    assert_eq!(action, Some(UserAction::Steer("now".into())));
    tx.send(action.unwrap()).unwrap();

    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    let action = super::test_support::confirm_action(&mut app);
    assert_eq!(action, Some(UserAction::Interrupt));
    assert!(!app.should_quit);
    tx.send(action.unwrap()).unwrap();

    ev_tx.send(HostEvent::SetBusy(false)).unwrap();
    ev_tx
        .send(HostEvent::Row(
            plakous_host_protocol::views::transcript::TranscriptRow::Interrupted,
        ))
        .unwrap();
    drain(&mut app);
    assert!(!app.busy);
    assert!(blocks_contain(&app, "⊘ interrupted"));

    // `Ctrl+D` drops from the focused queue: `Tab` onto it, `↓` to the last.
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key(KeyCode::Down));
    let action = app.handle_key(key_ctrl(KeyCode::Char('d')));
    assert_eq!(
        action,
        Some(UserAction::DeleteQueued {
            id: "q-second".into(),
            revision: 1,
        })
    );
    tx.send(action.unwrap()).unwrap();

    // Esc never quits; idle empty-draft Ctrl+C arms, then Ctrl+Q still quits (§6.1).
    let action = app.handle_key(key(KeyCode::Esc));
    assert!(action.is_none());
    assert!(!app.should_quit);
    assert!(app.handle_key(key_ctrl(KeyCode::Char('c'))).is_none());
    assert!(app.quit_arm.is_some());
    assert!(!app.should_quit);
    let action = app.handle_key(key_ctrl(KeyCode::Char('q')));
    assert_eq!(action, Some(UserAction::Quit));
    assert!(app.should_quit);

    let seen: Vec<UserAction> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(matches!(seen[0], UserAction::Prompt(_)));
    assert!(matches!(seen[1], UserAction::FollowUp(_)));
    assert!(matches!(seen[2], UserAction::Steer(_)));
    assert_eq!(seen[3], UserAction::Interrupt);
    assert_eq!(
        seen[4],
        UserAction::DeleteQueued {
            id: "q-second".into(),
            revision: 1,
        }
    );
}

#[test]
fn clear_slash_produces_clear_session_action() {
    let mut app = App::new(launch_basic(), true);
    app.pane
        .blocks
        .push(ScrollBlock::user("preserve me".into()));
    app.session_id = Some("source-session".into());
    app.input = "/clear".into();
    let action = app.handle_key(key(KeyCode::Enter));
    assert_eq!(action, Some(UserAction::ClearSession));
    assert!(!blocks_contain(&app, "session cleared"));
    commit_sent_action(&mut app, action.as_ref().unwrap());
    assert!(blocks_contain(&app, "preserve me"));
    assert_eq!(app.session_id.as_deref(), Some("source-session"));
    assert!(app.clear_pending);
    assert_eq!(app.phase, "clear");
    assert!(
        app.status_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("pending"))
    );

    app.apply_host_event(HostEvent::ClearView);
    assert!(app.pane.blocks.is_empty());
    assert!(!app.clear_pending);
}

#[test]
fn slash_tab_completes_without_submitting() {
    let mut app = App::new(launch_basic(), true);
    app.input = "/cle".into();
    app.slash_open = true;
    app.slash_selected = 0;
    assert!(app.handle_key(key(KeyCode::Tab)).is_none());
    assert_eq!(app.input, "/clear");
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::ClearSession)
    );
}

#[test]
fn compactable_capacity_recovery_is_visible_and_clickable() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::ContextCapacityRecovery(Some(
        crate::tui::ContextCapacityRecovery::Compactable,
    )));
    let backend = TestBackend::new(90, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    let rendered = serialize_frame(terminal.backend().buffer());
    assert!(rendered.contains("context·recovery"));
    assert!(rendered.contains("failed·turn·will·not·replay"));
    assert!(rendered.contains("compact·this·checkpoint"));
    let action = app.capacity_recovery_action.expect("compact action hitbox");
    assert_eq!(
        app.handle_mouse(left_click(action.x, action.y)),
        Some(UserAction::Compact)
    );

    app.apply_host_event(HostEvent::ContextCapacityRecovery(None));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(app.capacity_recovery_action.is_none());
}

#[test]
fn capacity_recovery_keeps_draft_and_recalls_saved_prompt_without_dispatch() {
    use crate::tui::ContextCapacityRecovery::{Compactable, Protected, ResendShorter};

    let saved = "safe saved prompt ".repeat(70);
    let saved: String = saved.chars().take(1_184).collect();
    assert_eq!(saved.chars().count(), 1_184);
    for recovery in [Compactable, Protected, ResendShorter] {
        for width in [48, 90] {
            for accept in [KeyCode::Enter, KeyCode::Left] {
                let mut app = App::new(launch_basic(), true);
                let (sender, receiver) = user_action_channel();
                app.set_input("current draft must stay".into());
                app.input_cursor = 7;
                app.apply_host_event(HostEvent::PromptHistorySnapshot(vec![
                    "older prompt".into(),
                    saved.clone(),
                ]));
                app.apply_host_event(HostEvent::ContextCapacityRecovery(Some(recovery)));
                let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
                terminal.draw(|frame| app.draw(frame)).unwrap();
                let text = screen_text(&terminal);
                assert!(text.contains("Prompt saved in history."), "{text}");
                assert!(
                    text.contains("Empty composer: ↑ history; Enter/← loads only."),
                    "{text}"
                );
                assert_eq!(app.input, "current draft must stay");
                assert_eq!(app.input_cursor, 7);
                assert_eq!(app.history_browse, None);
                assert_eq!(
                    app.capacity_recovery_action.is_some(),
                    recovery == Compactable
                );

                // Up on a nonempty draft stays in the editor, never replaces it.
                assert!(app.handle_key(key(KeyCode::Up)).is_none());
                assert_eq!(app.input, "current draft must stay");
                assert_eq!(app.history_browse, None);
                app.clear_input();
                assert!(app.handle_key(key(KeyCode::Up)).is_none());
                assert_eq!(app.history_browse, Some(1));
                assert!(app.input.is_empty());
                terminal.draw(|frame| app.draw(frame)).unwrap();
                assert!(screen_text(&terminal).contains("safe saved prompt"));
                if accept == KeyCode::Enter
                    && let Some(dir) = std::env::var_os("PLAK_TUI_CAPACITY_FRAMES")
                {
                    std::fs::write(
                        std::path::Path::new(&dir).join(format!("{recovery:?}-{width}.frame")),
                        serialize_frame(terminal.backend().buffer()),
                    )
                    .unwrap();
                }
                let action = app.handle_key(key(accept));
                if let Some(action) = &action {
                    dispatch_action(&mut app, action, Some(&sender));
                }
                assert!(action.is_none(), "history acceptance must not submit");
                assert!(receiver.try_recv().is_err(), "no duplicate prompt dispatch");
                assert_eq!(app.input, saved);
                assert_eq!(app.history_browse, None);
                app.apply_host_event(HostEvent::ContextCapacityRecovery(None));
                assert_eq!(app.input, saved);
            }
        }
    }
}

#[test]
fn overflow_restores_an_already_accepted_prompt_for_every_recovery_kind() {
    use crate::tui::ContextCapacityRecovery::{Compactable, Protected, ResendShorter};
    for recovery in [Compactable, Protected, ResendShorter] {
        for draft in ["", "new draft"] {
            let mut app = App::new(launch_basic(), true);
            let prompt = UserAction::Prompt("withheld prompt".into());
            app.pending_drafts.push_back(prompt.clone());
            app.apply_host_event(HostEvent::DraftAccepted(prompt.clone()));
            app.set_input(draft.into());
            app.apply_host_event(HostEvent::DraftRestored("withheld prompt".into()));
            app.apply_host_event(HostEvent::ContextCapacityRecovery(Some(recovery)));
            assert_eq!(
                app.input,
                if draft.is_empty() {
                    "withheld prompt"
                } else {
                    "withheld prompt\n\nnew draft"
                }
            );
            assert!(app.pending_drafts.is_empty());
            assert_eq!(app.history_browse, None);
            let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert!(screen_text(&terminal).contains("withheld prompt"));
        }
    }
}

/// Per-attempt child rows are not a usage fact. The session ledger above
/// them is the session-wide reading; a child belongs on the canvas.
#[test]
fn the_usage_tab_does_not_list_child_attempts() {
    let mut app = usage_fixture();
    let mut measured = tree_node("c1", Some("root"), 1);
    measured.attempt_seq = Some(2);
    measured.attempt_status = Some(AttemptStatus::Running);
    measured.usage = Some(plakous_host_protocol::views::AttemptUsage {
        elapsed_ms: 91_000,
        turns: 1,
        tool_calls: 11,
        input_tokens: 4_200,
        output_tokens: 800,
        compaction: None,
    });
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        measured,
    ])));
    let body = lines_text(&app.usage_tab_lines(120));
    assert!(!body.contains("Agents"), "{body}");
    assert!(!body.contains("c1 · a2"), "{body}");
    assert!(!body.contains("11 tool calls"), "{body}");
}

/// With no delegated children there is no Agents section at all: an empty
/// table is a claim that there is nothing to measure, which is a different
/// statement from "no tree".
#[test]
fn the_usage_tab_draws_no_agents_section_without_children() {
    let mut app = usage_fixture();
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
    ])));
    assert!(!lines_text(&app.usage_tab_lines(120)).contains("Agents"));
}

/// §5.2: a Keybinds row too wide for the modal wraps under its own indent
/// rather than truncating mid-word.
#[test]
fn the_keybinds_sheet_wraps_instead_of_truncating() {
    let sheet = crate::tui::keymap::sheet_text();
    let long = sheet
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    assert!(long > 40, "the sheet has rows worth wrapping");

    let wrapped = App::wrap_sheet_lines_impl(&sheet, 40);
    assert!(
        wrapped.iter().all(|line| line.chars().count() <= 40),
        "every wrapped row fits the width"
    );
    assert!(
        wrapped.len() > sheet.lines().count(),
        "the long rows took extra rows instead of losing their tails"
    );
    // No word is lost: the wrapped rows carry the same words in order.
    let source: Vec<&str> = sheet.split_whitespace().collect();
    let rendered: Vec<String> = wrapped
        .iter()
        .flat_map(|line| line.split_whitespace().map(str::to_owned))
        .collect();
    assert_eq!(source.len(), rendered.len(), "no word is dropped");
}

/// A wrapped keybinds row keeps its aligned description column.
///
/// The wrap tokenised the whole row, which collapsed the padded key column
/// and started the tail at column 2, so exactly the rows that needed help
/// were the ones that lost the alignment their neighbours kept.
#[test]
fn a_wrapped_keybinds_row_keeps_its_description_column() {
    const WIDTH: u16 = 66; // the Menu body at 80x24
    let sheet = crate::tui::keymap::sheet_text();
    let wrapped = App::wrap_sheet_lines_impl(&sheet, WIDTH);
    assert!(
        wrapped
            .iter()
            .all(|line| line.chars().count() <= usize::from(WIDTH)),
        "every row fits the body"
    );

    // Every chord row starts its description at the same column, wrapped
    // or not, and every continuation row sits under that column.
    let column = |line: &str| line.find(|ch: char| !ch.is_whitespace());
    let described: Vec<usize> = wrapped
        .iter()
        .filter(|line| !line.starts_with(char::is_whitespace) && line.contains("  "))
        .filter_map(|line| App::sheet_value_column(line))
        .collect();
    assert!(described.len() > 5, "the sheet has chord rows");
    let first = described[0];
    assert!(
        described.iter().all(|value| *value == first),
        "every chord row shares one description column: {described:?}"
    );

    let long = wrapped
        .iter()
        .position(|line| line.contains("fold everything open"))
        .expect("the longest row is present");
    let tail = wrapped
        .get(long + 1)
        .expect("that row wrapped onto a continuation");
    assert_eq!(
        column(tail),
        Some(first),
        "a continuation sits under the description column, not at column 2: {tail:?}"
    );
}

#[test]
/// Spec 02 §7.2: the same Limits section with nothing connected. Every
/// vertical still appears, each as one plain line with its CLI remedy, and
/// no bar is drawn anywhere on the Usage tab.
fn the_limits_section_matches_reviewable_goldens_with_nothing_connected() {
    for (width, height, size) in [(120_u16, 40_u16, "120x40"), (80, 24, "80x24")] {
        let mut app = usage_offline_fixture();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        app.activate_control_tab(super::ControlTab::Usage);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let actual = serialize_frame(terminal.backend().buffer());
        let path = format!("tests/goldens/usage-limits-offline-{size}.frame");
        if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
            std::fs::write(&path, &actual).expect("write reviewed frame golden");
        } else {
            let expected = std::fs::read_to_string(&path).expect("offline Limits golden exists");
            assert_eq!(actual, expected, "{path}");
        }
    }
}

#[test]
fn the_session_section_copies_one_row_by_click_and_every_row_with_y() {
    let mut app = usage_fixture();
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    // The Session tab is where the clickable rows are painted.
    app.activate_control_tab(super::ControlTab::Session);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        !app.session_row_hits.is_empty(),
        "the painted Session rows are clickable"
    );
    let all = crate::tui::usage_tab::session_copy_text(&app.session_rows);
    assert!(all.contains("Session ID: sid-123abc"));
    assert!(all.contains("API kernel: Responses"));
    assert!(all.contains("Tree epoch: live"));
    assert!(all.contains("Context: 36.7k / 1.0m tokens (3.7%)"), "{all}");
    assert!(osc52_sequence(&all).is_some(), "copy payload stays bounded");

    let &(row, rect) = app
        .session_row_hits
        .iter()
        .find(|(row, _)| app.session_rows[*row].label == "Session ID")
        .expect("the session id row is painted");
    assert!(app.handle_mouse(left_click(rect.x, rect.y)).is_none());
    assert_eq!(
        crate::tui::usage_tab::session_row_copy_text(&app.session_rows[row]),
        "Session ID: sid-123abc"
    );
}

#[test]
fn the_context_grid_keeps_one_hundred_cells_at_both_breakpoints() {
    for (width, rows) in [(120_u16, 5_usize), (44, 10)] {
        let mut app = usage_fixture();
        let body = usage_body_text(&mut app, width);
        let per_row = if width <= crate::tui::usage_tab::NARROW_BREAKPOINT {
            10
        } else {
            20
        };
        let grid: Vec<String> = body
            .lines()
            .filter(|line| crate::tui::usage_tab::is_context_grid_line(line, width))
            .map(|line| line.chars().take(per_row * 2 - 1).collect())
            .collect();
        assert_eq!(grid.len(), rows, "width {width}");
        let cells: usize = grid
            .iter()
            .map(|line| line.chars().filter(|ch| *ch != ' ').count())
            .sum();
        assert_eq!(cells, 100, "width {width}");
        assert!(!body.contains("Stale"));
        assert!(body.contains("◈ Tool definitions"));
    }
}

#[test]
fn every_input_path_rejects_control_and_format_scalars() {
    let mut app = App::new(launch_basic(), true);
    // Bracketed paste.
    assert_eq!(app.handle_paste("safe\u{202e}drowssap\u{0}!"), None);
    assert_eq!(app.input, "safedrowssap!");
    app.clear_input();
    // Individual key presses.
    typed(&mut app, "ok");
    app.handle_key(key(KeyCode::Char('\u{200f}')));
    app.handle_key(key(KeyCode::Char('\u{feff}')));
    assert_eq!(app.input, "ok");
    // A history-shelf load is filtered too.
    app.set_input("from\u{202a}history".into());
    assert_eq!(app.input, "fromhistory");
}

#[test]
fn readline_motions_and_kills_match_the_spec_table() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("alpha beta gamma".into());

    // Ctrl+A / Ctrl+E.
    app.handle_key(key_ctrl(KeyCode::Char('a')));
    assert_eq!(app.input_cursor, 0);
    app.handle_key(key_ctrl(KeyCode::Char('e')));
    assert_eq!(app.input_cursor, 16);

    // Ctrl+W deletes the word before the caret.
    app.handle_key(key_ctrl(KeyCode::Char('w')));
    assert_eq!(app.input, "alpha beta ");
    assert_eq!(app.input_cursor, 11);

    // Alt+B / Alt+F move by word.
    let mut alt_b = key(KeyCode::Char('b'));
    alt_b.modifiers = KeyModifiers::ALT;
    app.handle_key(alt_b);
    assert_eq!(app.input_cursor, 6);
    let mut alt_f = key(KeyCode::Char('f'));
    alt_f.modifiers = KeyModifiers::ALT;
    app.handle_key(alt_f);
    assert_eq!(app.input_cursor, 10);

    // Ctrl+Left / Ctrl+Right move by word too.
    app.handle_key(key_ctrl(KeyCode::Left));
    assert_eq!(app.input_cursor, 6);
    app.handle_key(key_ctrl(KeyCode::Right));
    assert_eq!(app.input_cursor, 10);

    // Ctrl+U kills to line start, Ctrl+K to line end.
    app.set_input("one two three".into());
    app.input_cursor = 8;
    app.composer_selection.set_caret(&app.input, 8);
    app.handle_key(key_ctrl(KeyCode::Char('u')));
    assert_eq!(app.input, "three");
    app.input_cursor = 2;
    app.composer_selection.set_caret(&app.input, 2);
    app.handle_key(key_ctrl(KeyCode::Char('k')));
    assert_eq!(app.input, "th");

    // Alt+Backspace is a word delete, not a single grapheme.
    app.set_input("keep this word".into());
    let mut alt_backspace = key(KeyCode::Backspace);
    alt_backspace.modifiers = KeyModifiers::ALT;
    app.handle_key(alt_backspace);
    assert_eq!(app.input, "keep this ");
}

#[test]
fn ctrl_shift_a_selects_the_whole_draft_and_is_on_the_sheet() {
    fn ctrl_shift(code: KeyCode) -> KeyEvent {
        let mut event = key(code);
        event.modifiers = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        event
    }

    let mut app = App::new(launch_basic(), true);
    app.set_input("first line\nsecond line".into());
    app.input_cursor = 3;

    // Plain Ctrl+A is still the readline line-start motion.
    app.handle_key(key_ctrl(KeyCode::Char('a')));
    assert_eq!(app.input_cursor, 0);
    assert!(app.composer_selection.range(&app.input).is_none());

    // Ctrl+Shift+A selects everything, whichever case the terminal reports.
    for code in [KeyCode::Char('a'), KeyCode::Char('A')] {
        let mut app = App::new(launch_basic(), true);
        app.set_input("first line\nsecond line".into());
        app.input_cursor = 3;
        app.handle_key(ctrl_shift(code));
        assert_eq!(
            app.composer_selection.range(&app.input),
            Some(0..app.input.chars().count()),
            "{code:?} must select the whole draft"
        );
        assert_eq!(app.input_cursor, app.input.chars().count());
    }

    // And the selection is the one Ctrl+X cuts.
    let mut app = App::new(launch_basic(), true);
    app.set_input("cut every last word".into());
    app.handle_key(ctrl_shift(KeyCode::Char('a')));
    app.handle_key(key_ctrl(KeyCode::Char('x')));
    assert!(app.input.is_empty(), "Ctrl+X cuts the whole selection");
    assert!(
        app.control_tab.is_none(),
        "with a selection Ctrl+X cuts, it does not open the Menu"
    );

    // §5.2: a binding cannot exist in the input layer without the sheet.
    assert!(
        crate::tui::keymap::BINDINGS
            .iter()
            .any(|binding| binding.chord == "Ctrl+Shift+A"),
        "the keymap registry must carry the chord"
    );
}

#[test]
fn ctrl_x_cuts_a_selection_and_otherwise_opens_the_menu() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("cut me".into());
    app.composer_selection.begin_drag(&app.input, 0);
    app.composer_selection.drag_to(&app.input, 3);
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('x'))), None);
    assert_eq!(app.input, " me");
    assert_eq!(app.control_tab, None, "a cut must not open a modal");
    // Undoable like every other kill.
    app.handle_key(key_ctrl(KeyCode::Char('z')));
    assert_eq!(app.input, "cut me");

    let mut app = App::new(launch_basic(), true);
    app.handle_key(key_ctrl(KeyCode::Char('x')));
    assert_eq!(app.control_tab, Some(super::ControlTab::Session));
}

#[test]
fn startup_row_ticks_session_open_then_the_ready_row_states_it() {
    let mut app = App::new(launch_basic(), true);
    pin_clock(&mut app);
    app.apply_host_event(HostEvent::SessionOpening);
    app.session_open_started = app.now().checked_sub(Duration::from_millis(5_700));
    assert!(
        app.redraw_demand().spinner_visible,
        "a ticking startup row needs spinner cadence"
    );

    let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(
        screen_text(&terminal).contains("starting…"),
        "startup row must tick while the session opens"
    );
    let opening = screen_text(&terminal);
    assert!(
        !opening.contains("5.7s"),
        "startup HUD has no duplicate elapsed clock: {opening:?}"
    );

    app.apply_host_event(HostEvent::SessionOpened {
        session_open_ms: Some(5_692),
    });
    assert!(app.session_open_started.is_none());
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let ready = screen_text(&terminal);
    assert!(
        !ready.contains("session open"),
        "open latency is not product chrome: {ready:?}"
    );
    assert!(
        ready.contains("awaiting context") || ready.contains('%'),
        "ready idle is the context meter: {ready:?}"
    );

    app.apply_host_event(HostEvent::SetBusy(true));
    assert!(
        app.session_open_ms.is_none(),
        "startup timing retires once the session is used"
    );
}

/// An attach that fails falls back to the not-ready shell, which publishes
/// `SessionOpened` with no measurement. The startup row must retire: it was
/// the only thing holding the 80 ms spinner cadence, so leaving it armed
/// repainted a static screen for the life of the process. No duration may be
/// claimed for a session that never opened.
#[test]
fn an_abandoned_session_open_retires_the_startup_row_without_claiming_a_time() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SessionOpening);
    assert!(app.redraw_demand().spinner_visible);

    app.apply_host_event(HostEvent::SessionOpened {
        session_open_ms: None,
    });

    assert!(app.session_open_started.is_none());
    assert!(app.session_open_ms.is_none());
    assert!(
        !app.redraw_demand().spinner_visible,
        "a not-ready shell must park, not spin"
    );
    assert!(app.redraw_demand().is_deep_idle());

    let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let screen = screen_text(&terminal);
    assert!(
        !screen.contains("starting…") || app.turn_status_label().is_none(),
        "{screen}"
    );
    assert!(!screen.contains("session open"), "{screen}");
}

// --- Control center (spec 02 §5.5) -----------------------------

/// Spec 02 §5.5: every entry point that opens the Menu routes
/// through the real key/slash dispatch and lands on its own tab. A tab
/// nobody can reach is a tab that does not exist.
#[test]
fn every_control_center_entry_point_routes_through_real_dispatch() {
    for (line, expected, action) in [
        (
            "/settings",
            super::ControlTab::Settings,
            Some(UserAction::Extension(SurfaceIntent::OpenSettings)),
        ),
        (
            "/providers",
            super::ControlTab::Providers,
            Some(UserAction::RefreshReadiness),
        ),
        (
            "/usage",
            super::ControlTab::Usage,
            Some(UserAction::Extension(SurfaceIntent::OpenUsage)),
        ),
        (
            "/context",
            super::ControlTab::Context,
            Some(UserAction::Extension(SurfaceIntent::OpenUsage)),
        ),
        (
            "/status",
            super::ControlTab::Session,
            Some(UserAction::RefreshTemplates),
        ),
        ("/keybinds", super::ControlTab::Keybinds, None),
    ] {
        let mut app = App::new(launch_basic(), true);
        app.set_input(line.to_owned());
        let emitted = app.submit_line(false);
        assert_eq!(app.control_tab, Some(expected), "{line}");
        assert_eq!(emitted, action, "{line}");
    }

    // `/agents` opens the Agents tab through its own pane opener.
    let mut app = App::new(launch_basic(), true);
    app.set_input("/agents".to_owned());
    app.submit_line(false);
    assert_eq!(app.control_tab, Some(super::ControlTab::Models));

    // Ctrl+X with an empty composer first opens the Session tab.
    let mut app = App::new(launch_basic(), true);
    app.handle_key(key_ctrl(KeyCode::Char('x')));
    assert_eq!(app.control_tab, Some(super::ControlTab::Session));
}

/// `/reload` hands an idle session to the installed binary and never cuts off
/// a running turn or live sub-agents.
#[test]
fn reload_quits_into_a_handoff_only_when_idle() {
    let mut app = App::new(launch_basic(), true);
    app.busy = true;
    app.set_input("/reload".to_owned());
    assert_eq!(app.submit_line(false), None);
    assert!(!app.should_quit && app.terminal_handoff.is_none());
    assert!(blocks_contain(&app, "reload refused"));

    app.busy = false;
    app.tree_live_descendants = 1;
    app.set_input("/reload".to_owned());
    assert_eq!(app.submit_line(false), None);
    assert!(!app.should_quit && app.terminal_handoff.is_none());

    app.tree_live_descendants = 0;
    app.set_input("/reload".to_owned());
    assert_eq!(app.submit_line(false), None);
    assert!(app.pending_reload && !app.should_quit);
}

fn settle_reload_probe(app: &mut App) -> super::reload::ReloadOutcome {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(outcome) = app.poll_reload_probe() {
            return outcome;
        }
        assert!(std::time::Instant::now() < deadline, "probe never answered");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_reload_probe_runs_off_the_ui_thread_and_hands_off_when_it_answers() {
    let mut app = App::new(launch_basic(), true);
    let (release, gate) = mpsc::channel::<()>();
    app.start_reload_probe(move || {
        let _ = gate.recv();
        None
    });
    // The probe is still blocked: the UI keeps running and says why.
    assert!(blocks_contain(&app, "checking installed build…"));
    assert!(app.poll_reload_probe().is_none());
    assert!(app.redraw_demand().spinner_visible);
    release.send(()).unwrap();
    assert!(matches!(
        settle_reload_probe(&mut app),
        super::reload::ReloadOutcome::HandOff(None)
    ));
    assert!(app.reload_probe.is_none());
}

#[test]
fn a_turn_started_while_the_reload_probe_ran_refuses_the_handoff() {
    let mut app = App::new(launch_basic(), true);
    let (release, gate) = mpsc::channel::<()>();
    app.start_reload_probe(move || {
        let _ = gate.recv();
        None
    });
    app.busy = true;
    release.send(()).unwrap();
    assert!(matches!(
        settle_reload_probe(&mut app),
        super::reload::ReloadOutcome::Refused
    ));
    assert!(blocks_contain(
        &app,
        "reload refused: wait for the turn to finish or cancel it"
    ));
    assert!(!app.should_quit && app.terminal_handoff.is_none());
}

#[test]
fn reload_refuses_running_processes_without_closing_the_tui() {
    let mut app = App::new(launch_basic(), true);
    app.background_processes = 2;
    app.set_input("/reload".to_owned());
    assert_eq!(app.submit_line(false), None);
    assert!(blocks_contain(
        &app,
        "reload refused: background processes are running; stop them first (/ps)"
    ));
    assert!(!app.pending_reload && !app.should_quit && app.terminal_handoff.is_none());
}

/// The `/help` overlay scrolls; it used to clip whatever did not fit.
#[test]
fn the_help_overlay_scrolls_the_generated_sheet() {
    let mut app = App::new(launch_basic(), true);
    let ctx = app.slash_ctx();
    let effect =
        crate::tui::slash::resolve(&crate::tui::slash::parse_slash("/help").unwrap(), &ctx);
    let action = app.apply_slash_effect(effect);
    assert!(action.is_none());
    let (_, body) = app.info_overlay.clone().expect("the overlay opened");
    for binding in crate::tui::keymap::BINDINGS {
        assert!(
            body.contains(binding.chord),
            "/help must document {}",
            binding.chord
        );
    }

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| app.draw_info_overlay(frame, frame.area()))
        .unwrap();
    assert!(
        app.info_overlay_scroll.max_offset() > 0,
        "the sheet is taller than the overlay, so it must be scrollable"
    );
    assert!(app.handle_key(key(KeyCode::Down)).is_none());
    assert_eq!(app.info_overlay_scroll.offset(), 1);
    assert!(app.handle_key(key(KeyCode::End)).is_none());
    assert_eq!(
        app.info_overlay_scroll.offset(),
        app.info_overlay_scroll.max_offset(),
        "End reaches the last row instead of losing it"
    );
    assert!(app.handle_key(key(KeyCode::Char('q'))).is_none());
    assert!(app.info_overlay.is_none(), "q still closes the overlay");
}

/// Spec 02 §5.5: the tab strip is exactly the seven tabs, in order.
#[test]
fn the_tab_strip_is_the_seven_control_center_tabs() {
    let labels: Vec<&str> = super::ControlTab::ALL
        .iter()
        .map(|tab| tab.label())
        .collect();
    assert_eq!(
        labels,
        vec![
            "Settings",
            "Agents",
            "Providers",
            "Usage",
            "Context",
            "Session",
            "Keybinds"
        ]
    );
}

#[test]
fn bounded_draft_restoration_preserves_newer_text_and_undo() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("older".into());
    let length = super::MAX_COMPOSER_BYTES - 8;
    let mut newer = format!("{}\n", "n".repeat(63)).repeat(length / 64);
    newer.push_str(&"n".repeat(length % 64));
    app.set_input(newer.clone());
    app.apply_host_event(HostEvent::PromptHistorySnapshot(vec![
        "withheld prompt".into(),
    ]));
    app.apply_host_event(HostEvent::DraftRestored("withheld prompt".into()));
    assert_eq!(app.input, newer);
    app.composer_undo_step();
    assert_eq!(app.input, "older");
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(screen_text(&terminal).contains("prompt remains in history"));
}

#[test]
fn protected_recovery_panel_offers_no_compact_action() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::ContextCapacityRecovery(Some(
        crate::tui::ContextCapacityRecovery::Protected,
    )));
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let text = screen_text(&terminal);
    assert!(!text.contains("compact this checkpoint"));
    assert!(text.contains("fork"));
    assert!(app.capacity_recovery_action.is_none());
}

fn menu_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect()
}

fn theme_value_gap(width: u16, height: u16) -> usize {
    let mut app = settings_fixture();
    let rows = menu_rows(&mut app, width, height);
    let row = rows
        .iter()
        .find(|row| row.contains("Theme") && row.contains("default-dark"))
        .unwrap_or_else(|| panic!("{width}x{height}: theme row missing\n{rows:#?}"));
    let label = row.find("Theme").unwrap();
    let value = row.find("default-dark").unwrap();
    row[label..value].chars().count()
}

/// A 200-column Menu keeps the full-page frame and a body no wider than the
/// 120-column sheet, so a value stays next to its label.
#[test]
fn menu_body_stays_next_to_its_labels_at_200_columns() {
    let at_120 = theme_value_gap(120, 40);
    let at_200 = theme_value_gap(200, 50);
    assert!(
        at_200.abs_diff(at_120) <= 8,
        "the wide sheet should match a 120-column body (120={at_120}, 200={at_200})"
    );
    assert!(at_200 < 140, "value drifted to the right edge: {at_200}");

    let mut app = App::new(launch_basic(), true);
    app.activate_control_tab(super::ControlTab::Keybinds);
    let rows = menu_rows(&mut app, 200, 50);
    assert_eq!(rows[0].chars().count(), 200, "the frame spans the page");
    assert!(rows[0].contains("Menu"), "{}", rows[0]);
    let tab = rows
        .iter()
        .find_map(|row| row.find("Settings").map(|byte| row[..byte].chars().count()))
        .expect("tab row");
    assert!(
        (30..55).contains(&tab),
        "the tab row centers with the 120-column body: {tab}"
    );
    let close = rows[0]
        .find('\u{00d7}')
        .map(|byte| rows[0][..byte].chars().count())
        .expect("close glyph");
    assert!(
        close > 180,
        "the close glyph stays on the full-width frame: {close}"
    );
    let settings_hit = app
        .control_tab_hits
        .iter()
        .find(|(tab, _)| *tab == super::ControlTab::Settings)
        .map(|(_, rect)| rect.x)
        .expect("settings hit");
    assert!(
        (30..55).contains(&usize::from(settings_hit)),
        "the tab hit follows the centered label: {settings_hit}"
    );

    let mut narrow = App::new(launch_basic(), true);
    narrow.activate_control_tab(super::ControlTab::Keybinds);
    let narrow_rows = menu_rows(&mut narrow, 120, 40);
    let narrow_tab = narrow_rows
        .iter()
        .find_map(|row| row.find("Settings").map(|byte| row[..byte].chars().count()))
        .expect("tab row at 120");
    assert!(
        narrow_tab < 15,
        "at 120 columns the tab row stays at the left edge: {narrow_tab}"
    );
    let heading = rows
        .iter()
        .find_map(|row| {
            row.find("While typing")
                .map(|byte| row[..byte].chars().count())
        })
        .expect("keybinds body");
    assert!(
        (20..80).contains(&heading),
        "keybinds body is not a centered 120-column column: {heading}"
    );
}

/// Activity shares the frame but not Menu's 120-column cap.
#[test]
fn activity_tabs_stay_full_width_above_120_columns() {
    let mut app = App::new(launch_basic(), true);
    app.open_processes();
    let rows = menu_rows(&mut app, 200, 50);
    let tab = rows
        .iter()
        .find_map(|row| {
            row.find("Processes")
                .map(|byte| row[..byte].chars().count())
        })
        .expect("activity tabs");
    assert!(
        tab < 15,
        "activity tabs stay at the left edge above 120 columns: {tab}"
    );
}

#[test]
fn ctrl_x_first_opens_menu_on_the_session_tab() {
    let mut app = App::new(launch_basic(), true);
    app.handle_key(key_ctrl(KeyCode::Char('x')));
    assert_eq!(app.control_tab, Some(super::ControlTab::Session));
}
