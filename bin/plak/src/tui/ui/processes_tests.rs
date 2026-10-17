//! Processes operator journeys and reviewable TestBackend stories.

use super::pilot::PilotAction;
use super::processes::ActivityTab;
use super::test_support::{
    blocks_contain, key, launch_basic, left_click, render_to_string, serialize_frame,
};
use super::{App, HostEvent, KeyCode, UserAction, theme_for};
use crate::tui::{SettingsTheme, slash::SlashEffect};
use plakous_host_protocol::views::{ProcessStatus, ProcessView};
use ratatui::{Terminal, backend::TestBackend};
use std::sync::Arc;

fn fixture() -> App {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.apply_process_snapshot(Arc::new(vec![
        ProcessView {
            id: "root:p1".into(),
            command: "printf progress; sleep 30".into(),
            owner: "root".into(),
            started_at: 10,
            elapsed_ms: 12000,
            status: ProcessStatus::Running,
            output: (0..40).map(|n| format!("line {n}: progress\n")).collect(),
        },
        ProcessView {
            id: "c3:p1".into(),
            command: "cargo check".into(),
            owner: "c3".into(),
            started_at: 11,
            elapsed_ms: 8000,
            status: ProcessStatus::Exited(Some(7)),
            output: "build failed\n".into(),
        },
        ProcessView {
            id: "root:p2".into(),
            command: "printf done".into(),
            owner: "root".into(),
            started_at: 12,
            elapsed_ms: 20,
            status: ProcessStatus::Exited(Some(0)),
            output: "done\n".into(),
        },
    ]));
    app
}

/// Spec 02 §5.7: an idle root with one live descendant shows `1 agent` and,
/// from the host snapshot, `1 process`, separated by ` · `. The motion glyph
/// sits in that separator. Ending the child's process drops the count; the
/// row keeps the child owner, and Processes can still act on it.
#[test]
fn idle_hud_counts_a_child_owned_running_process() {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.tree_live_descendants = 1;
    let running = ProcessView {
        id: "child-session:child-branch:p1".into(),
        command: "python3 hold.py 25 token".into(),
        owner: "c1".into(),
        started_at: 10,
        elapsed_ms: 2000,
        status: ProcessStatus::Running,
        output: String::new(),
    };
    app.apply_process_snapshot(Arc::new(vec![running.clone()]));
    assert_eq!(app.background_processes, 1);
    let text = render_to_string(&mut app, 120, 24);
    let agent = text.find("1 agent").expect(&text);
    let process = text.find("1 process").expect(&text);
    assert!(agent < process, "{text}");
    assert!(text[agent..process].contains(" · "), "{text}");
    assert!(!text.contains("1 processes"), "{text}");
    app.open_processes();
    let open = render_to_string(&mut app, 120, 24);
    assert!(open.contains("c1"), "{open}");
    assert!(open.contains("hold.py"), "{open}");
    app.handle_key(key(KeyCode::Esc));
    let mut ended = running;
    ended.status = ProcessStatus::Stopped;
    app.apply_process_snapshot(Arc::new(vec![ended]));
    assert_eq!(app.background_processes, 0);
    let idle = render_to_string(&mut app, 120, 24);
    assert!(idle.contains("1 agent"), "{idle}");
    assert!(!idle.contains("1 process"), "{idle}");
    app.open_processes();
    let kept = render_to_string(&mut app, 120, 24);
    assert!(kept.contains("c1"), "{kept}");
}

#[test]
fn hud_gap_and_singular_process_in_all_chrome() {
    let mut app = fixture();
    let text = render_to_string(&mut app, 80, 24);
    assert!(text.contains("1 process ─"), "{text}");
    assert!(!text.contains("1 processes"));
    app.confirm_turn_stop();
    assert!(render_to_string(&mut app, 80, 24).contains("1 background process keeps running"));
    app.destructive_confirm = None;
    app.request_quit();
    assert!(render_to_string(&mut app, 80, 24).contains("1 background process will be stopped"));
}

#[test]
fn activity_tabs_share_geometry_and_only_transcripts_have_tracks() {
    let theme = SettingsTheme::DefaultDark;
    let mut app = fixture();
    app.persisted_theme = theme;
    app.theme = theme_for(theme);
    app.open_tree_navigator();
    render_to_string(&mut app, 80, 24);
    let popup = app.tree_popup;
    let tabs = (app.activity_agents_hit, app.activity_processes_hit);
    app.open_processes();
    let text = render_to_string(&mut app, 80, 24);
    assert_eq!(app.tree_popup, popup);
    assert_eq!((app.activity_agents_hit, app.activity_processes_hit), tabs);
    assert!(text.contains(" Activity "), "{text}");
    assert!(!text.contains("Activity ·"), "{text}");
    app.handle_key(key(KeyCode::Esc));
    app.open_control_tab(super::ControlTab::Usage);
    render_to_string(&mut app, 80, 24);
}

#[test]
fn activity_agents_unavailable_stays_in_the_body_and_can_switch_or_close() {
    let mut app = fixture();
    app.open_tree_navigator();
    app.tree_navigator = None;
    app.agents_unavailable = Some("no topology hub is attached".into());
    assert!(
        render_to_string(&mut app, 80, 24)
            .contains("Sub-agents are unavailable: no topology hub is attached")
    );
    assert_eq!(app.pane.blocks, [] as [crate::tui::blocks::ScrollBlock; 0]);
    assert_eq!(app.semantic_snapshot().owner, "canvas-loading");
    app.handle_paste("must not reach the composer");
    assert_eq!(app.input, "");
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.activity_tab, Some(ActivityTab::Processes));
    app.handle_key(key(KeyCode::BackTab));
    app.tree_navigator = None;
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.activity_tab, None);
}

#[test]
fn processes_body_hints_fit_narrow_shared_chrome() {
    let mut app = fixture();
    app.open_processes();
    assert!(render_to_string(&mut app, 48, 24).contains("S stop all"));
    app.handle_key(key(KeyCode::Char('s')));
    let text = render_to_string(&mut app, 48, 24);
    assert!(text.contains("Stop this process?"), "{text}");
    assert!(text.contains("[ Cancel ]"), "{text}");
    app.handle_key(key(KeyCode::Esc));
    assert!(app.destructive_confirm.is_none());
    app.handle_key(key(KeyCode::Enter));
    let text = render_to_string(&mut app, 48, 24);
    assert!(text.contains("Esc back"));
    assert!(!text.contains("Esc close"), "{text}");
}

#[test]
fn super_editing_never_changes_the_hidden_activity_composer() {
    use super::{KeyEvent, KeyModifiers};
    for processes in [false, true] {
        for chord in ['a', 'x', 'z'] {
            let mut app = fixture();
            app.set_input("draft".into());
            app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SUPER));
            if chord == 'a' {
                app.composer_selection.clear();
            }
            if processes {
                app.open_processes();
            } else {
                app.open_tree_navigator();
                app.tree_navigator = None;
                app.agents_unavailable = Some("no topology".into());
            }
            let selection = app.composer_selection.range(&app.input);
            app.handle_key(KeyEvent::new(KeyCode::Char(chord), KeyModifiers::SUPER));
            assert_eq!(app.input, "draft", "processes={processes}, Cmd+{chord}");
            assert_eq!(app.composer_selection.range(&app.input), selection);
        }
    }
}

#[test]
fn activity_close_glyph_brightens_under_the_pointer() {
    use super::{KeyModifiers, MouseEvent, MouseEventKind};
    for processes in [false, true] {
        let mut app = fixture();
        if processes {
            app.open_processes();
        } else {
            app.open_tree_navigator();
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let hit = app.activity_close_hit.unwrap();
        let before = terminal.backend().buffer()[(hit.x, hit.y)].clone();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: hit.x,
            row: hit.y,
            modifiers: KeyModifiers::NONE,
        });
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_ne!(terminal.backend().buffer()[(hit.x, hit.y)], before);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(terminal.backend().buffer()[(hit.x, hit.y)], before);
    }
}

#[test]
fn agents_body_owns_its_escape_hint() {
    let mut app = fixture();
    super::test_support::open_tree(
        &mut app,
        super::test_support::tree_snapshot(vec![super::test_support::tree_node("root", None, 0)]),
    );
    let text = render_to_string(&mut app, 80, 24);
    assert!(text.contains("Esc back"), "{text}");
    assert!(!text.contains("Esc close"), "{text}");
    app.handle_key(key(KeyCode::Char('s')));
    let text = render_to_string(&mut app, 80, 24);
    assert!(text.contains("Stop all agents?"), "{text}");
    assert!(!text.contains("Esc close"), "{text}");
}

/// Spec 02 §5.7: Activity takes the whole screen; nothing
/// of the composer or its hint line is drawn or hit-tested beneath it, and
/// the close glyph returns to the main view.
#[test]
fn activity_takes_the_whole_screen_and_its_close_glyph_returns() {
    for (width, height) in [(48, 24), (80, 12), (120, 40)] {
        for processes in [false, true] {
            let mut app = fixture();
            app.set_input("draft".into());
            if processes {
                app.open_processes();
            } else {
                app.open_tree_navigator();
            }
            let text = render_to_string(&mut app, width, height);
            // Full height; the shared modal chrome keeps a one-cell side
            // margin on narrow terminals.
            let popup = app.tree_popup.expect("Activity painted");
            assert_eq!((popup.y, popup.height), (0, height), "{width}x{height}");
            assert!(popup.width + 2 >= width, "{width}x{height}: {popup:?}");
            assert!(app.composer_inner.is_none(), "{text}");
            assert!(app.status_processes_hit.is_none(), "{text}");
            assert!(!text.contains("draft"), "{text}");
            assert!(!text.contains("/ commands"), "{text}");
            let close = app.activity_close_hit.expect("close glyph");
            app.handle_mouse(left_click(close.x, close.y));
            assert_eq!(app.activity_tab, None);
            let root = render_to_string(&mut app, width, height);
            assert!(root.contains("draft"), "{root}");
        }
    }
}

#[test]
fn processes_pilot_stop_one_stop_all_and_esc_p_require_confirmations() {
    let mut app = fixture();
    app.open_processes();
    for (action, id) in [
        ("process-stop", Some("root:p1".into())),
        ("process-stop-all", None),
    ] {
        assert_eq!(
            app.dispatch_pilot_action(PilotAction::Invoke(action.into()))
                .unwrap(),
            None
        );
        assert!(app.destructive_confirm.is_some());
        assert_eq!(
            app.dispatch_pilot_action(PilotAction::Invoke("confirm".into()))
                .unwrap(),
            Some(UserAction::StopProcesses {
                id,
                interrupt: false
            })
        );
    }
    app.activity_tab = None;
    app.apply_host_event(HostEvent::SetBusy(true));
    app.confirm_turn_stop();
    // The turn stop's second action stops the processes too. Focus moves at once.
    app.handle_key(key(KeyCode::Right));
    app.handle_key(key(KeyCode::Right));
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::StopProcesses {
            id: None,
            interrupt: true
        })
    );
    assert!(app.destructive_confirm.is_none());
    assert!(app.cancel_pending);
}

#[test]
fn processes_selection_survives_snapshots_and_output_scrolls() {
    let mut app = fixture();
    app.open_processes();
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.process_selected.as_deref(), Some("c3:p1"));
    let mut rows = (*app.processes).clone();
    rows.swap(0, 1);
    app.apply_process_snapshot(Arc::new(rows));
    assert_eq!(app.process_selected.as_deref(), Some("c3:p1"));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    let before = render_to_string(&mut app, 80, 24);
    app.handle_key(key(KeyCode::PageDown));
    let after = render_to_string(&mut app, 80, 24);
    assert_ne!(before, after);
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.activity_tab, Some(ActivityTab::Processes));
    assert!(!app.process_output);
    app.handle_key(key(KeyCode::Esc));
    assert_ne!(app.activity_tab, Some(ActivityTab::Processes));
}

/// Spec 02 Activity > Processes: a row is the status glyph, the owner and the
/// elapsed time in aligned columns, then the command cut with `…`; the full
/// command is in the process view.
#[test]
fn process_rows_align_owner_and_elapsed_and_cut_the_command() {
    let mut app = fixture();
    app.open_processes();
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let rows = app
        .process_hits
        .iter()
        .map(|(_, rect)| {
            (rect.x..rect.right())
                .map(|x| buffer[(x, rect.y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(
        rows[0]
            .chars()
            .skip(1)
            .collect::<String>()
            .starts_with("   root  12s printf progress"),
        "{rows:?}"
    );
    assert!(rows[1].starts_with("✗ 7 c3   8.0s cargo check"), "{rows:?}");
    assert!(rows[2].starts_with("✓   root 0.0s printf done"), "{rows:?}");

    let mut app = fixture();
    let mut long = (*app.processes).clone();
    long[0].command = format!("cargo nextest run {}", "--flag ".repeat(20));
    app.apply_process_snapshot(Arc::new(long));
    app.open_processes();
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let rect = app.process_hits[0].1;
    let row = (rect.x..rect.right())
        .map(|x| terminal.backend().buffer()[(x, rect.y)].symbol())
        .collect::<String>();
    assert!(row.trim_end().ends_with('…'), "{row}");
    assert!(!row.contains(" · "), "{row}");
}

/// Spec 02 §5.5 mouse parity: a click selects a process row; a double-click
/// opens its full view exactly as Enter does.
#[test]
fn a_process_row_selects_on_click_and_opens_on_double_click() {
    let mut app = fixture();
    app.open_processes();
    render_to_string(&mut app, 100, 24);
    let (id, rect) = app.process_hits[1].clone();
    assert_eq!(app.handle_mouse(left_click(rect.x + 2, rect.y)), None);
    assert_eq!(app.process_selected.as_deref(), Some(id.as_str()));
    assert!(!app.process_output, "a click only selects");
    render_to_string(&mut app, 100, 24);
    assert_eq!(app.handle_mouse(left_click(rect.x + 2, rect.y)), None);
    assert!(app.process_output, "a double-click opens the full view");
    assert_eq!(app.process_selected.as_deref(), Some(id.as_str()));
}

#[test]
fn processes_hud_click_hover_last_tab_and_slash_routes() {
    let mut app = fixture();
    let text = render_to_string(&mut app, 80, 24);
    assert!(text.contains("⠴ 1 process"));
    let hit = app.status_processes_hit.unwrap();
    app.handle_mouse(left_click(hit.x, hit.y));
    assert_eq!(app.activity_tab, Some(ActivityTab::Processes));
    app.handle_key(key(KeyCode::Esc));
    app.handle_ctrl_key(KeyCode::Char('t'), false);
    assert_eq!(app.activity_tab, Some(ActivityTab::Processes));
    app.apply_slash_effect(SlashEffect::Action(UserAction::OpenTopologyNavigator));
    assert_ne!(app.activity_tab, Some(ActivityTab::Processes));
    assert!(app.tree_navigator.is_some());
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.activity_tab, Some(ActivityTab::Processes));
    assert_eq!(
        crate::tui::slash::resolve(
            &crate::tui::slash::parse_slash("/ps").unwrap(),
            &crate::tui::slash::SlashContext::default()
        ),
        SlashEffect::OpenProcesses
    );
}

#[test]
fn reload_rechecks_running_processes_after_an_idle_terminal_snapshot() {
    let mut app = fixture();
    app.apply_slash_effect(SlashEffect::Reload);
    assert!(!app.pending_reload);
    assert!(blocks_contain(&app, "background processes are running"));

    let mut rows = (*app.processes).clone();
    rows[0].status = ProcessStatus::Exited(Some(0));
    app.apply_process_snapshot(Arc::new(rows));
    assert_eq!(app.background_processes, 0);
    app.pane.blocks.clear();
    app.apply_slash_effect(SlashEffect::Reload);
    assert!(!blocks_contain(&app, "background processes are running"));
    assert!(app.pending_reload || blocks_contain(&app, "no installed plak"));
}

#[test]
fn processes_testbackend_stories_match_reviewable_goldens() {
    let mut actual = String::new();
    let theme = SettingsTheme::DefaultDark;
    for width in [48, 80, 120] {
        for story in [
            "hud",
            "agents",
            "processes",
            "output",
            "process-confirmation",
            "process-confirmation-action",
            "stop",
            "quit",
        ] {
            let mut app = fixture();
            app.persisted_theme = theme;
            app.theme = theme_for(theme);
            match story {
                "agents" => {
                    super::test_support::open_tree(
                        &mut app,
                        super::test_support::tree_snapshot(vec![super::test_support::tree_node(
                            "root", None, 0,
                        )]),
                    );
                }
                "processes" => app.open_processes(),
                "output" => {
                    app.open_processes();
                    app.handle_key(key(KeyCode::Enter));
                }
                "process-confirmation" => {
                    app.open_processes();
                    app.handle_key(key(KeyCode::Char('s')));
                }
                "process-confirmation-action" => {
                    app.open_processes();
                    app.handle_key(key(KeyCode::Char('S')));
                    app.handle_key(key(KeyCode::Right));
                }
                "stop" => app.confirm_turn_stop(),
                "quit" => {
                    app.request_quit();
                }
                _ => {}
            }
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            actual.push_str(&format!("{theme:?} {width} {story}\n"));
            actual.push_str(&serialize_frame(terminal.backend().buffer()));
        }
    }
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::write("tests/goldens/activity-processes.frame", &actual).unwrap();
    } else {
        assert_eq!(
            actual,
            include_str!("../../../tests/goldens/activity-processes.frame")
        );
    }
}

/// Below the modal floor a previous process row cannot take a click.
#[test]
fn a_too_small_activity_frame_drops_process_hits() {
    let mut app = fixture();
    app.open_processes();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(!app.process_hits.is_empty(), "the list painted rows");
    let stale = app.process_hits[0].1;
    let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        app.process_hits,
        [] as [(String, ratatui::prelude::Rect); 0]
    );
    assert!(app.activity_close_hit.is_none());
    assert!(app.activity_agents_hit.is_none());
    assert!(app.tree_popup.is_none());
    let selected = app.process_selected.clone();
    app.handle_mouse(left_click(stale.x, stale.y));
    assert_eq!(app.process_selected, selected);
}
