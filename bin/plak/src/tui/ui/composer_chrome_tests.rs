//! Composer-region stories: cell geometry, themes and operator gestures.

use super::pilot::PilotAction;
use super::test_support::{
    builtin_profiles_view, confirm_action, key, launch_basic, left_click, left_mouse, pin_clock,
    queue_item, render_to_string, tool_line, tree_node, tree_snapshot,
};
use super::{App, HostEvent, KeyCode, QueueDrainState, UserAction, theme_for};
use crate::tui::SettingsTheme;
use crate::tui::SurfaceObservation;
use crate::tui::presentation::ActivityKind;
use crossterm::event::{KeyEvent, KeyModifiers, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};
use std::time::{Duration, Instant};

#[test]
fn processes_hud_and_stop_popup_show_live_jobs() {
    let mut app = App::new(launch_basic(), true);
    app.background_processes = 2;
    let text = render_to_string(&mut app, 120, 24);
    assert!(text.contains("2 processes"), "{text}");
    app.confirm_turn_stop();
    let text = render_to_string(&mut app, 120, 24);
    assert!(
        text.contains("2 background processes keep running"),
        "{text}"
    );
    assert!(text.contains("[ Stop turn + processes ]"), "{text}");
}

#[test]
fn quit_with_background_processes_requires_confirmation() {
    let mut app = App::new(launch_basic(), true);
    app.background_processes = 2;
    let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert!(app.handle_key(quit).is_none());
    assert!(!app.should_quit);
    let text = render_to_string(&mut app, 120, 24);
    assert!(
        text.contains("2 background processes will be stopped"),
        "{text}"
    );
    // Focus starts on Quit. Esc cancels; Enter confirms.
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert!(!app.should_quit);
    assert!(app.destructive_confirm.is_none());
    assert!(app.handle_key(quit).is_none());
    assert_eq!(confirm_action(&mut app), Some(UserAction::Quit));
    assert!(app.should_quit);
}

#[test]
fn config_profiles_never_invent_a_resumed_sessions_identity() {
    let mut app = App::new(launch_basic(), true);
    app.account = Some("claude".into());
    app.model = Some("claude-opus-5-5".into());
    app.effort = Some("high".into());
    let mut view = builtin_profiles_view();
    view.current = Some("claude-default".into());
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view)));
    assert!(app.composer_chrome.profile.is_none());
}

#[test]
fn idle_pending_indicator_names_each_source_and_hides_while_a_turn_runs() {
    let mut app = App::new(launch_basic(), true);
    let cases = [
        (
            plakous_host_protocol::PendingContinuation {
                results: 2,
                ..Default::default()
            },
            "2 results pending",
        ),
        (
            plakous_host_protocol::PendingContinuation {
                results: 1,
                ..Default::default()
            },
            "1 result pending",
        ),
        (
            plakous_host_protocol::PendingContinuation {
                questions: 1,
                ..Default::default()
            },
            "1 question pending",
        ),
        (
            plakous_host_protocol::PendingContinuation {
                process_exits: 1,
                ..Default::default()
            },
            "1 process exited",
        ),
        (
            plakous_host_protocol::PendingContinuation {
                process_exits: 3,
                ..Default::default()
            },
            "3 processes exited",
        ),
        (
            plakous_host_protocol::PendingContinuation {
                auto_continue_paused: true,
                ..Default::default()
            },
            "auto-continue paused",
        ),
        (
            plakous_host_protocol::PendingContinuation {
                tree_paused: true,
                ..Default::default()
            },
            "agents paused · /tree resume",
        ),
    ];
    for (pending, needle) in cases {
        app.apply_host_event(HostEvent::PendingContinuation {
            results: pending.results,
            questions: pending.questions,
            process_exits: pending.process_exits,
            auto_continue_paused: pending.auto_continue_paused,
            tree_paused: pending.tree_paused,
        });
        let text = render_to_string(&mut app, 160, 24);
        assert!(
            text.contains(&format!("◆ {needle}")),
            "missing {needle}: {text}"
        );
        app.apply_host_event(HostEvent::PendingContinuation {
            results: 0,
            questions: 0,
            process_exits: 0,
            auto_continue_paused: false,
            tree_paused: false,
        });
        let cleared = render_to_string(&mut app, 160, 24);
        assert!(
            !cleared.contains(needle),
            "still showing {needle}: {cleared}"
        );
    }
    app.apply_host_event(HostEvent::PendingContinuation {
        results: 2,
        questions: 0,
        process_exits: 0,
        auto_continue_paused: true,
        tree_paused: true,
    });
    app.apply_host_event(HostEvent::SetBusy(true));
    let busy = render_to_string(&mut app, 160, 24);
    assert!(
        !busy.contains("results pending") && !busy.contains("auto-continue paused"),
        "pending indicator stayed up during a turn: {busy}"
    );
}

#[test]
fn waiting_clock_starts_at_each_request_not_at_the_last_tool_phase() {
    let mut app = App::new(launch_basic(), true);
    pin_clock(&mut app);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.phase_started = app.now().checked_sub(Duration::from_secs(90));
    let text = render_to_string(&mut app, 120, 24);
    assert!(!text.contains("waiting for model 90s"));
    app.apply_host_event(HostEvent::ModelRequestStarted);
    app.model_request_started = app.now().checked_sub(Duration::from_secs(12));
    let text = render_to_string(&mut app, 120, 24);
    assert!(text.contains("waiting for model 12s"), "{text}");
    app.apply_host_event(HostEvent::ModelRequestStarted);
    let text = render_to_string(&mut app, 120, 24);
    assert!(text.contains("waiting for model 0.0s"), "{text}");
}

#[test]
fn pointer_motion_only_dirties_when_a_painted_hover_target_changes() {
    let mut app = App::new(launch_basic(), true);
    render_to_string(&mut app, 120, 24);
    app.dirty = false;
    app.handle_mouse(left_mouse(MouseEventKind::Moved, 0, 0));
    assert!(!app.dirty);
    let hit = app.composer_chrome.profile_hit.unwrap();
    app.handle_mouse(left_mouse(MouseEventKind::Moved, hit.x, hit.y));
    assert!(app.dirty);
    app.dirty = false;
    app.handle_mouse(left_mouse(MouseEventKind::Moved, hit.x + 1, hit.y));
    assert!(!app.dirty);
    app.handle_mouse(left_mouse(MouseEventKind::Moved, 0, 0));
    assert!(app.dirty);
}

#[test]
fn clipped_hint_never_starts_with_a_separator() {
    for width in 14..24 {
        let mut app = App::new(launch_basic(), true);
        app.apply_host_event(HostEvent::SetBusy(true));
        let text = render_to_string(&mut app, width, 24);
        let hint = text.lines().last().unwrap().trim();
        assert!(!hint.starts_with('·'), "{width}: {hint}");
    }
}

#[test]
fn installed_build_hint_keeps_the_brighter_chrome_style() {
    let mut app = App::new(launch_basic(), true);
    app.newer_build_installed = true;
    let mut terminal = Terminal::new(TestBackend::new(160, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let hint = &terminal.backend().buffer()[(1, 23)];
    assert_eq!(hint.fg, app.theme.chrome.fg.unwrap());
    assert_ne!(hint.fg, app.theme.chrome_dim.fg.unwrap());
}

#[test]
fn steers_are_not_queued_prompts_and_offer_no_queue_controls() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SteerAck {
        state: "queued".into(),
        position: Some(1),
    });
    let text = render_to_string(&mut app, 120, 24);
    assert!(text.contains("sent · waiting for the current reply"));
    assert!(!text.contains("1 queued"));
    assert!(!text.contains("Tab focus"));
    assert!(!text.contains("Ctrl+S sends now"));
    assert!(!text.contains("Enter send now"));
}

#[test]
fn stop_confirmation_has_no_zero_agents_and_keeps_short_text_clear() {
    for height in [4, 5, 6, 7, 8, 24] {
        let mut app = App::new(launch_basic(), true);
        app.confirm_turn_stop();
        let text = render_to_string(&mut app, 80, height);
        assert!(!text.contains("0 running agents"));
        assert!(text.contains("root turn"), "{height}: {text}");
        assert!(!text.contains("root["), "{height}: {text}");
    }
}

#[test]
fn hud_counters_are_separated_and_use_singular_agent_and_leave_the_queue_to_its_panel() {
    let mut app = App::new(launch_basic(), true);
    app.tree_live_descendants = 1;
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q", 1, "next")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    let text = render_to_string(&mut app, 120, 24);
    assert!(text.contains("1 agent "), "{text}");
    assert!(!text.contains("1 agents"));
    // The queue panel above the composer already shows it; the HUD does not
    // repeat it.
    assert!(!text.contains("1 queued"), "{text}");
}

#[test]
fn composer_hint_is_below_a_plain_body_colour_border_at_every_width() {
    let theme = SettingsTheme::DefaultDark;
    for width in [20, 32, 48, 64, 80, 120, 160] {
        let mut app = App::new(launch_basic(), true);
        app.theme = theme_for(theme);
        app.apply_host_event(HostEvent::SetBusy(true));
        let text = render_to_string(&mut app, width, 24);
        let rows = text.lines().collect::<Vec<_>>();
        let bottom = usize::from(app.composer_inner.unwrap().bottom());
        assert!(rows[bottom].contains('─'));
        assert!(!rows[bottom].contains("commands"));
        assert!(rows[bottom + 1].contains("Esc stop"), "{width}: {text}");
        assert!(!text.contains("[stop]"));
        if width >= 80 {
            assert!(rows[bottom + 1].contains("/ commands · Ctrl+X menu · ? keybinds"));
        }
    }
}

#[test]
fn empty_question_mark_opens_keybinds_but_a_draft_keeps_it_literal() {
    let mut app = App::new(launch_basic(), true);
    app.handle_key(key(KeyCode::Char('?')));
    assert_eq!(app.control_tab, Some(super::ControlTab::Keybinds));
    assert_eq!(app.input, "");
    app.close_control_center();
    app.set_input("draft".into());
    app.handle_key(key(KeyCode::Char('?')));
    assert_eq!(app.input, "draft?");
    assert!(app.control_tab.is_none());
}

#[test]
fn slash_only_opens_the_palette_from_an_empty_composer() {
    let mut app = App::new(launch_basic(), true);
    app.handle_key(key(KeyCode::Char('/')));
    assert!(app.slash_open);
    app.handle_key(key(KeyCode::Char('h')));
    assert!(app.slash_open);
    app.set_input("draft".into());
    app.input_cursor = 0;
    app.handle_key(key(KeyCode::Char('/')));
    assert_eq!(app.input, "/draft");
    assert!(!app.slash_open);
}

#[test]
fn stop_buttons_hover_and_click_match_keyboard_confirm_without_stealing_a_draft() {
    for theme in SettingsTheme::ALL {
        let mut app = App::new(launch_basic(), true);
        app.theme = theme_for(theme);
        app.apply_host_event(HostEvent::SetBusy(true));
        app.tree_live_descendants = 3;
        app.set_input("next draft".into());
        app.handle_key(key(KeyCode::Esc));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let confirm = app
            .destructive_confirm
            .as_ref()
            .unwrap()
            .action_hit()
            .unwrap();
        let cancel = app
            .destructive_confirm
            .as_ref()
            .unwrap()
            .cancel_hit()
            .unwrap();
        app.handle_mouse(left_mouse(MouseEventKind::Moved, confirm.x, confirm.y));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        // The primary action is already focused, so hover keeps the selected
        // style. An unfocused button brightens to the title colour.
        assert_eq!(
            terminal.backend().buffer()[(confirm.x, confirm.y)].fg,
            app.theme.slash_selected.fg.unwrap_or_default()
        );
        app.handle_mouse(left_mouse(MouseEventKind::Moved, cancel.x, cancel.y));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(cancel.x, cancel.y)].fg,
            app.theme.title.fg.unwrap_or_default()
        );
        assert_eq!(app.handle_mouse(left_click(cancel.x, cancel.y)), None);
        assert!(!app.cancel_pending);
        assert_eq!(app.input, "next draft");
        app.dispatch_pilot_action(PilotAction::Invoke("cancel".into()))
            .unwrap();
        assert!(app.destructive_confirm.is_some());
        let text = render_to_string(&mut app, 80, 24);
        assert!(
            text.contains("3 running agents are interrupted, resumable"),
            "{text}"
        );
        assert_eq!(
            app.handle_mouse(left_click(confirm.x, confirm.y)),
            Some(UserAction::Interrupt)
        );
        assert_eq!(app.input, "next draft");
    }
}

#[test]
fn stop_confirmation_pilot_requires_confirm_and_allows_dismissal() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert_eq!(app.semantic_snapshot().owner, "destructive-confirmation");
    let text = render_to_string(&mut app, 80, 24);
    assert!(text.contains("root turn"), "{text}");
    assert_eq!(
        app.dispatch_pilot_action(PilotAction::CloseFocusedOwner)
            .unwrap(),
        None
    );
    assert!(!app.cancel_pending);
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(
        app.dispatch_pilot_action(PilotAction::Invoke("confirm".into()))
            .unwrap(),
        Some(UserAction::Interrupt)
    );
    assert!(app.cancel_pending);
}

#[test]
fn admitted_steer_stays_visible_until_applied_without_a_duplicate_transcript_row() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::SteerAck {
        state: "queued".into(),
        position: Some(1),
    });
    assert!(render_to_string(&mut app, 100, 24).contains("sent · waiting for the current reply"));
    assert_eq!(app.pane.blocks, [] as [crate::tui::blocks::ScrollBlock; 0]);
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q1", 1, "next")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    app.apply_host_event(HostEvent::SteerAck {
        state: "applied".into(),
        position: None,
    });
    let text = render_to_string(&mut app, 100, 24);
    assert!(!text.contains("sent · waiting"));
    assert!(text.contains("next"));
}

#[test]
fn hud_priority_and_verbs_never_expose_the_tool_subject() {
    for theme in SettingsTheme::ALL {
        let mut app = App::new(launch_basic(), true);
        app.theme = theme_for(theme);
        app.apply_host_event(HostEvent::SetBusy(true));
        assert_eq!(
            app.turn_status_label().as_deref(),
            Some("waiting for model")
        );
        assert_hud_width_classes(&mut app);
        app.apply_host_event(HostEvent::AssistantDelta("answer".into()));
        assert_eq!(app.turn_status_label().as_deref(), Some("responding"));
        assert_hud_width_classes(&mut app);
        app.apply_host_event(HostEvent::ReasoningDelta {
            location_key: "r".into(),
            text: "thought".into(),
        });
        assert_eq!(app.turn_status_label().as_deref(), Some("thinking"));
        assert_hud_width_classes(&mut app);
        for (name, kind, label) in [
            ("exec_command", ActivityKind::Process, "running command"),
            ("read_file", ActivityKind::Tool, "reading"),
            ("apply_patch", ActivityKind::Tool, "editing"),
            ("search_repository", ActivityKind::Tool, "searching"),
            ("wait_agent", ActivityKind::Agent, "waiting on agents"),
            ("wait", ActivityKind::Compose, "waiting"),
            ("exec", ActivityKind::Compose, "working"),
        ] {
            app.apply_host_event(tool_line(
                name,
                None,
                None,
                kind,
                name,
                "running",
                Some("DO_NOT_PAINT_SUBJECT"),
            ));
            assert_eq!(app.turn_status_label().as_deref(), Some(label));
            assert_hud_width_classes(&mut app);
            let text = render_to_string(&mut app, 120, 24);
            let top = usize::from(app.composer_inner.unwrap().y - 1);
            assert!(text.lines().nth(top).unwrap().contains(label));
            assert!(
                !text
                    .lines()
                    .nth(top)
                    .unwrap()
                    .contains("DO_NOT_PAINT_SUBJECT")
            );
        }
        app.apply_host_event(HostEvent::ModelRetry(
            plakous_host_protocol::ModelRetryNotice {
                next_attempt: 2,
                max_attempts: 4,
                delay_ms: 60_000,
                cause_kind: "busy".into(),
                cause: "cause stays out of HUD".into(),
                output_discarded: false,
            },
        ));
        let label = app.turn_status_label().unwrap();
        assert!(label.starts_with("retrying ") && label.ends_with("(2/4)"));
        assert!(!label.contains("cause"));
        app.phase = "compacting".into();
        assert_eq!(app.turn_status_label().as_deref(), Some("compacting"));
        assert_hud_width_classes(&mut app);
        app.cancel_pending = true;
        assert_eq!(app.turn_status_label().as_deref(), Some("cancelling…"));
        assert_hud_width_classes(&mut app);
        app.busy = false;
        app.tree_restored_paused = true;
        assert_eq!(
            app.turn_status_label().as_deref(),
            Some(super::TREE_PAUSED_STATUS)
        );
        assert_hud_width_classes(&mut app);
        app.apply_host_event(HostEvent::ApprovalRequired {
            approval_id: "a".into(),
            call_id: None,
            tool_name: "shell".into(),
            subject: None,
            prompt: None,
            expires_at_ms: 0,
        });
        assert_eq!(app.turn_status_label().as_deref(), Some("approval needed"));
        assert_hud_width_classes(&mut app);
        app.session_open_started = Some(Instant::now());
        assert_eq!(app.turn_status_label().as_deref(), Some("starting…"));
        assert_hud_width_classes(&mut app);
        app.quitting = true;
        assert_eq!(app.turn_status_label().as_deref(), Some("quitting…"));
        assert_hud_width_classes(&mut app);
    }
}

#[test]
fn nested_activity_wins_over_a_computing_cell_and_problem_labels_replace_work() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(tool_line(
        "cell",
        None,
        Some("cell"),
        ActivityKind::Compose,
        "exec",
        "running",
        None,
    ));
    app.apply_host_event(tool_line(
        "read",
        Some("cell"),
        Some("cell"),
        ActivityKind::Tool,
        "read_file",
        "running",
        None,
    ));
    assert_eq!(app.turn_status_label().as_deref(), Some("reading"));
    app.apply_host_event(tool_line(
        "read",
        Some("cell"),
        Some("cell"),
        ActivityKind::Tool,
        "read_file",
        "done",
        None,
    ));
    assert_eq!(app.turn_status_label().as_deref(), Some("working"));
    for (phase, label) in [
        ("error", "error: disk full"),
        ("interrupted", "interrupted"),
        ("rebind", "switching model…"),
    ] {
        app.apply_host_event(HostEvent::Status {
            phase: phase.into(),
            queue_len: 0,
            detail: Some("disk full".into()),
        });
        assert_eq!(app.turn_status_label().as_deref(), Some(label));
    }
}

#[test]
fn hint_width_budget_drops_labels_and_keeps_the_bare_timer_alone_on_the_right() {
    for theme in SettingsTheme::ALL {
        for (width, labels) in [
            (160, true),
            (80, true),
            (48, false),
            (32, false),
            (20, false),
        ] {
            let mut app = App::new(launch_basic(), true);
            pin_clock(&mut app);
            app.theme = theme_for(theme);
            app.workspace_chrome = "~/workspace-with-a-long-path · feature".into();
            app.apply_host_event(HostEvent::SetBusy(true));
            app.turn_started = app.now().checked_sub(Duration::from_secs(252));
            let text = render_to_string(&mut app, width, 24);
            let hint = text
                .lines()
                .nth(usize::from(app.composer_inner.unwrap().bottom()) + 1)
                .unwrap();
            assert!(
                !hint.contains("feature") && !hint.contains('~'),
                "{width}: {hint}"
            );
            assert!(hint.trim_end().ends_with("4m12s"), "{width}: {hint}");
            assert_eq!(hint.contains("commands"), labels, "{width}: {hint}");
            assert!(
                hint.contains("Esc stop") && hint.contains("4m12s"),
                "{width}: {hint}"
            );
            app.apply_host_event(HostEvent::SetBusy(false));
            let idle = render_to_string(&mut app, width, 24);
            assert!(!idle.contains("4m12s") && !idle.contains("Esc stop"));
        }
    }
}

/// Spec 02 §5.4: `~/path · branch` sits right-aligned in the composer's
/// bottom border in the profile id's style; a narrow border shortens the path
/// to its tail, then drops it, and clips the branch last.
#[test]
fn workspace_facts_sit_in_the_composer_bottom_border_and_narrow_path_first() {
    for theme in SettingsTheme::ALL {
        for (width, expected) in [
            (160, Some(" ~/workspace-with-a-long-path · feature ")),
            (40, Some(" …rkspace-with-a-long-path · feature ")),
            (20, Some(" …path · feature ")),
            (16, Some(" feature ")),
            (12, Some(" feature ")),
            (8, Some(" fe… ")),
            (6, None),
        ] {
            let mut app = App::new(launch_basic(), true);
            app.theme = theme_for(theme);
            app.workspace_chrome = "~/workspace-with-a-long-path · feature".into();
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let inner = app.composer_inner.unwrap();
            let buffer = terminal.backend().buffer();
            let border = (0..width)
                .map(|x| buffer[(x, inner.bottom())].symbol().to_owned())
                .collect::<String>();
            assert!(
                border.starts_with('╰') && border.ends_with('╯'),
                "{width}: {border}"
            );
            match expected {
                Some(text) => {
                    let body = border.trim_start_matches(['╰', '─']);
                    assert_eq!(body, format!("{text}╯"), "{width}: {border}");
                    if let Some(profile) = app.composer_chrome.profile_hit {
                        let x = width - 1 - u16::try_from(text.chars().count()).unwrap();
                        assert_eq!(
                            buffer[(x + 1, inner.bottom())].style(),
                            buffer[(profile.x + 1, profile.y)].style(),
                            "{width}: styled like the profile id"
                        );
                    }
                }
                None => assert!(
                    border
                        .trim_start_matches('╰')
                        .trim_end_matches('╯')
                        .chars()
                        .all(|c| c == '─'),
                    "{width}: {border}"
                ),
            }
        }
    }
}

#[test]
fn a_workspace_without_a_branch_keeps_its_path_tail_until_it_drops() {
    use super::composer_chrome::workspace_border_text;
    assert_eq!(workspace_border_text("~/ws", 20), " ~/ws ");
    assert_eq!(workspace_border_text("~/a/long/path", 10), " …ng/path ");
    assert_eq!(workspace_border_text("~/a/long/path", 6), "");
}

/// The HUD's `N agents` / `N processes` buttons: the hover highlight and the
/// click target are the count text, never its separator or motion glyph.
#[test]
fn hud_counter_buttons_cover_only_their_count_text() {
    let (name, theme) = ("dark", SettingsTheme::DefaultDark);
    let mut app = App::new(launch_basic(), true);
    app.theme = theme_for(theme);
    app.render_prefs.reduced_motion = true;
    app.tree_live_descendants = 2;
    app.background_processes = 3;
    let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let agents = app.status_agents_hit.unwrap();
    let processes = app.status_processes_hit.unwrap();
    for (hit, text, glyph) in [(agents, "2 agents", "⠿"), (processes, "3 processes", "⠴")] {
        let buffer = terminal.backend().buffer();
        let painted = (hit.x..hit.right())
            .map(|x| buffer[(x, hit.y)].symbol())
            .collect::<String>();
        assert_eq!(painted, text, "{name}");
        let glyph_x = hit.x - 2;
        assert_eq!(buffer[(glyph_x, hit.y)].symbol(), glyph, "{name}");
        let resting = buffer[(glyph_x, hit.y)].style();
        app.handle_mouse(left_mouse(MouseEventKind::Moved, glyph_x, hit.y));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(glyph_x, hit.y)].style(),
            resting,
            "{name}: the glyph never highlights"
        );
        assert_eq!(app.handle_mouse(left_click(glyph_x, hit.y)), None);
        assert!(app.activity_tab.is_none() && app.tree_navigator.is_none());
    }
    app.handle_mouse(left_mouse(MouseEventKind::Moved, agents.x, agents.y));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let actual = super::test_support::serialize_frame(terminal.backend().buffer());
    let path = format!("tests/goldens/hud-counter-hover-{name}-120x12.frame");
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::write(&path, actual).unwrap();
    } else {
        assert_eq!(
            actual,
            std::fs::read_to_string(&path).expect("hud hover golden"),
            "{path}"
        );
    }
}

/// Spec 02 §5.5 mouse parity: a click on a queued prompt selects it; a
/// double-click sends it now while busy, exactly as Enter on the focused
/// queue does.
#[test]
fn a_queued_prompt_selects_on_click_and_sends_now_on_double_click() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q1", 1, "first"), queue_item("q2", 3, "second")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let area = app.queue_area.expect("queue panel drawn");
    let click = left_click(area.x + 4, area.y + 2);
    assert_eq!(app.handle_mouse(click), None);
    assert!(app.queue_focused);
    assert_eq!(app.queue_selected, 1, "a click only selects");
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        app.handle_mouse(click),
        Some(UserAction::SendQueuedNow {
            id: "q2".into(),
            revision: 3
        })
    );
}

/// Spec 02 §5.4: while the root is idle the hint line's right slot says how
/// long ago the operator last sent the root a prompt; a running turn shows
/// its timer instead, and a session switch forgets the clock.
#[test]
fn the_idle_hint_shows_when_the_last_prompt_was_sent() {
    let hint = |app: &mut App, width: u16| {
        let text = render_to_string(app, width, 24);
        text.lines()
            .nth(usize::from(app.composer_inner.unwrap().bottom()) + 1)
            .unwrap()
            .trim_end()
            .to_owned()
    };
    let mut app = App::new(launch_basic(), true);
    assert!(!hint(&mut app, 120).contains("ago"), "no prompt yet");

    app.set_input("hello".into());
    app.handle_key(key(KeyCode::Enter));
    assert!(
        app.last_prompt_at.is_some(),
        "a sent prompt starts the clock"
    );
    app.last_prompt_at = app.now().checked_sub(Duration::from_secs(25 * 60 + 7));
    assert!(
        hint(&mut app, 120).ends_with("last user prompt · 25m ago"),
        "{}",
        hint(&mut app, 120)
    );
    assert!(
        hint(&mut app, 32).ends_with(" 25m ago"),
        "{}",
        hint(&mut app, 32)
    );
    assert!(hint(&mut app, 32).contains("/ · Ctrl+X · ?"));

    app.apply_host_event(HostEvent::SetBusy(true));
    app.turn_started = Instant::now().checked_sub(Duration::from_secs(12));
    let busy = hint(&mut app, 120);
    assert!(busy.ends_with("12s") && !busy.contains("ago"), "{busy}");

    app.apply_host_event(HostEvent::SetBusy(false));
    app.session_id = Some("s1".into());
    app.apply_host_event(HostEvent::Binding {
        session_id: Some("s2".into()),
        account: None,
        model: None,
        effort: None,
        extra: None,
        composition: crate::tui::CompositionSelection::FollowCatalog,
        protocol: crate::tui::ProtocolSelection::Auto,
        effective_presentation: "direct".to_owned(),
    });
    assert!(
        app.last_prompt_at.is_none(),
        "another session has its own clock"
    );
}

/// After an attach the host's stored prompt time seeds the clock, so it
/// survives /reload and resume; a prompt sent here since stays the newer.
#[test]
fn the_idle_clock_is_seeded_from_the_stored_prompt_time_on_attach() {
    let hint = |app: &mut App| {
        let text = render_to_string(app, 120, 24);
        text.lines()
            .nth(usize::from(app.composer_inner.unwrap().bottom()) + 1)
            .unwrap()
            .trim_end()
            .to_owned()
    };
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::LastUserPromptAt(None));
    assert!(
        app.last_prompt_at.is_none(),
        "nothing stored, nothing shown"
    );

    let stored = app.unix_now_ms() - (25 * 60 + 7) * 1_000;
    app.apply_host_event(HostEvent::LastUserPromptAt(Some(stored)));
    assert!(
        hint(&mut app).ends_with("last user prompt · 25m ago"),
        "{}",
        hint(&mut app)
    );
    app.apply_host_event(HostEvent::LastUserPromptAt(None));
    assert!(
        hint(&mut app).ends_with("25m ago"),
        "a later empty read keeps it"
    );

    app.set_input("hello".into());
    app.handle_key(key(KeyCode::Enter));
    let local = app.last_prompt_at;
    app.apply_host_event(HostEvent::LastUserPromptAt(Some(stored)));
    assert_eq!(app.last_prompt_at, local, "the local prompt is newer");

    let now = Instant::now();
    let mut seeded = App::new(launch_basic(), true);
    seeded.seed_last_prompt_at(Some(10_000), now, 4_000);
    assert_eq!(
        seeded.last_prompt_at,
        Some(now),
        "a stored time ahead of this clock reads as just sent"
    );
}

#[test]
fn the_idle_clock_is_coarse_and_repaints_only_when_its_text_changes() {
    use super::composer_chrome::prompt_age;
    assert_eq!(prompt_age(Duration::from_secs(9)), "9s ago");
    assert_eq!(prompt_age(Duration::from_secs(25 * 60 + 59)), "25m ago");
    assert_eq!(
        prompt_age(Duration::from_secs(3 * 3600 + 5 * 60)),
        "3h05m ago"
    );

    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.last_prompt_at = app.now().checked_sub(Duration::from_secs(5 * 60));
    render_to_string(&mut app, 120, 24);
    let later = app.status_spinner_at + Duration::from_secs(2);
    assert!(
        !app.advance_status_spinner(later),
        "the same minute paints nothing"
    );
    let next_minute = app.status_spinner_at + Duration::from_secs(61);
    assert!(app.advance_status_spinner(next_minute));
}

#[test]
fn agents_and_queue_chips_highlight_on_hover_without_moving_keyboard_focus() {
    for theme in SettingsTheme::ALL {
        for width in [32, 48, 64, 80, 120, 160] {
            let mut app = App::new(launch_basic(), true);
            app.theme = theme_for(theme);
            app.render_prefs.reduced_motion = true;
            app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
                tree_node("root", None, 0),
                tree_node("child", Some("root"), 1),
            ])));
            app.apply_host_event(HostEvent::QueueSnapshot {
                items: vec![queue_item(
                    "q",
                    1,
                    "a long draft which must leave room for buttons",
                )],
                drain: QueueDrainState::LiveAutoDrain,
            });
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let agents = app.status_agents_hit.unwrap();
            let (now, drop) = (app.queue_button_hits[0].now, app.queue_button_hits[0].drop);
            for hit in [agents, now, drop] {
                let before = terminal.backend().buffer()[(hit.x, hit.y)].style();
                app.handle_mouse(left_mouse(MouseEventKind::Moved, hit.x, hit.y));
                terminal.draw(|frame| app.draw(frame)).unwrap();
                let after = terminal.backend().buffer()[(hit.x, hit.y)].style();
                assert_ne!(before, after, "{theme:?} {width} {hit:?}");
                assert!(!app.queue_focused);
                assert!(hit.right() <= width);
            }
            assert_eq!(
                app.handle_mouse(left_click(agents.x, agents.y)),
                Some(UserAction::OpenTopologyNavigator)
            );
            app.tree_navigator = None;
            app.activity_tab = None;
            assert_eq!(
                app.dispatch_pilot_action(PilotAction::Invoke("focus-queue".into()))
                    .unwrap(),
                None
            );
            // A chip fires once it has held its place for CHIP_SETTLE.
            super::test_support::pass_time(&mut app, 300);
            assert_eq!(
                app.handle_mouse(left_click(now.x, now.y)),
                Some(UserAction::SendQueuedNow {
                    id: "q".into(),
                    revision: 1
                })
            );
            assert_eq!(
                app.handle_mouse(left_click(drop.x, drop.y)),
                Some(UserAction::DeleteQueued {
                    id: "q".into(),
                    revision: 1
                })
            );
        }
    }
}

#[test]
fn profile_is_a_hover_only_role_popup_and_border_uses_body_colour() {
    for theme in SettingsTheme::ALL {
        let mut app = App::new(launch_basic(), true);
        app.theme = theme_for(theme);
        app.model = Some("claude-opus-5-5".into());
        app.account = Some("claude".into());
        app.effort = Some("high".into());
        let mut view = builtin_profiles_view();
        view.current = Some("claude-default".into());
        let profile = view
            .rows
            .iter()
            .find(|row| row.id == "claude-default")
            .unwrap()
            .clone();
        app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view)));
        app.apply_host_event(HostEvent::Extension(SurfaceObservation::SessionProfile(
            profile,
        )));
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let inner = app.composer_inner.unwrap();
        let border = &terminal.backend().buffer()[(inner.x, inner.bottom())];
        assert_eq!(border.fg, app.theme.assistant.fg.unwrap());
        let hit = app.composer_chrome.profile_hit.unwrap();
        assert_eq!(app.handle_mouse(left_click(hit.x, hit.y)), None);
        app.handle_mouse(left_mouse(MouseEventKind::Moved, hit.x, hit.y));
        let text = render_to_string(&mut app, 120, 24);
        for role in [
            "root:",
            "worker:",
            "reviewer:",
            "researcher:",
            "Menu → Agents",
        ] {
            assert!(text.contains(role), "{text}");
        }
        assert!(app.control_tab.is_none());
        app.handle_mouse(left_mouse(MouseEventKind::Moved, 0, 0));
        assert!(!render_to_string(&mut app, 120, 24).contains("Menu → Agents"));
    }
}

#[test]
fn pending_steer_has_a_visible_row_even_with_a_full_queue() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: (0..8)
            .map(|i| queue_item(&format!("q{i}"), 1, "next"))
            .collect(),
        drain: QueueDrainState::LiveAutoDrain,
    });
    app.apply_host_event(HostEvent::SteerAck {
        state: "queued".into(),
        position: Some(1),
    });
    assert!(render_to_string(&mut app, 120, 24).contains("sent · waiting for the current reply"));
}

#[test]
fn turn_end_requests_fresh_product_workspace_facts() {
    let mut app = App::new(launch_basic(), true);
    let (sender, receiver) = crate::tui::user_action_channel();
    app.maybe_refresh_composer_chrome(Some(&sender));
    assert_eq!(
        receiver.try_recv().unwrap(),
        UserAction::Extension(crate::tui::SurfaceIntent::RefreshComposerChrome)
    );
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::SetBusy(false));
    app.maybe_refresh_composer_chrome(Some(&sender));
    assert_eq!(
        receiver.try_recv().unwrap(),
        UserAction::Extension(crate::tui::SurfaceIntent::RefreshComposerChrome)
    );
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::WorkspaceChrome(
        "~/ws · new-branch".into(),
    )));
    assert!(render_to_string(&mut app, 160, 24).contains("~/ws · new-branch"));
}

#[test]
fn resting_or_interrupted_descendants_do_not_masquerade_as_running_agents() {
    use plakous_host_protocol::topology::NodeStatus;
    let mut app = App::new(launch_basic(), true);
    let mut interrupted = tree_node("interrupted", Some("root"), 1);
    interrupted.status = NodeStatus::Interrupted;
    let mut paused = tree_node("paused", Some("root"), 1);
    paused.status = NodeStatus::Paused;
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        tree_node("running", Some("root"), 1),
        interrupted,
        paused,
    ])));
    assert_eq!(app.tree_live_descendants, 1);
}

fn assert_hud_width_classes(app: &mut App) {
    for width in [20, 32, 48, 64, 80, 120, 160] {
        let text = render_to_string(app, width, 24);
        let row = text
            .lines()
            .nth(usize::from(app.composer_inner.unwrap().y - 1))
            .unwrap();
        assert!(row.starts_with('╭') && row.ends_with('╮'), "{width}: {row}");
        assert!(!row.contains("DO_NOT_PAINT_SUBJECT"));
        assert!(!row.contains("[stop]") && !row.contains("⇣"));
        if width >= 120
            && let Some(label) = app.turn_status_label()
        {
            assert!(row.contains(&label), "{width}: {row}");
        }
    }
}

#[test]
fn process_activity_hud_uses_the_projected_operation() {
    for (name, subject, expected) in [
        (
            "exec_command",
            "Waiting on process 7 · cargo test",
            "waiting",
        ),
        (
            "process_session_poll",
            "Read output of process 7 · cargo test",
            "reading",
        ),
        (
            "write_stdin",
            "Sent input to process 7 · 4 bytes",
            "sending input",
        ),
        (
            "process_session_write",
            "Sent interrupt to process 7 · cargo test",
            "sending interrupt",
        ),
    ] {
        let mut app = App::new(launch_basic(), true);
        app.apply_host_event(HostEvent::SetBusy(true));
        app.apply_host_event(tool_line(
            "process",
            None,
            None,
            ActivityKind::Process,
            name,
            "running",
            Some(subject),
        ));
        assert_eq!(app.turn_status_label().as_deref(), Some(expected), "{name}");
    }
}

/// The HUD names a web search, a fetch and an MCP call by what it is doing.
#[test]
fn hud_labels_web_and_mcp_calls() {
    for (name, label) in [
        ("web_search", "searching"),
        ("WebSearch", "searching"),
        ("x_search", "searching"),
        ("web_fetch", "reading"),
        ("WebFetch", "reading"),
        ("fetch_url", "reading"),
        ("web__run", "reading"),
        ("mcp__docs__search", "calling mcp:docs"),
    ] {
        assert_eq!(
            super::composer_chrome::activity_label(name, ActivityKind::Tool, None),
            label,
            "{name}"
        );
    }
}

#[test]
fn workflow_escape_offers_operation_stop_not_turn_loss() {
    for phase in ["fork", "rebind"] {
        let mut app = App::new(launch_basic(), true);
        app.busy = true;
        app.phase = phase.into();
        assert!(app.handle_key(key(KeyCode::Esc)).is_none());
        let text = render_to_string(&mut app, 120, 24);
        assert!(text.contains("Stop this operation?"), "{text}");
        assert!(!text.contains("work in flight is lost"), "{text}");
        assert_eq!(confirm_action(&mut app), Some(UserAction::Interrupt));
    }
}
