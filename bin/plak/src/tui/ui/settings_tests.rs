//! Settings tab and theme tests.

use super::App;
use super::terminal::dispatch_action;
use super::test_support::{
    key, launch_basic, left_click, left_mouse, serialize_frame, settings_fixture, settings_row,
};
use crate::tui::blocks::RenderPrefs;
use crate::tui::theme::ThemeId;
use crate::tui::{HostEvent, SettingsSnapshot, UserAction, user_action_channel};
use crate::tui::{SurfaceIntent, SurfaceObservation};
use crossterm::event::{KeyCode, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

/// Spec 02 §5.5: a click focuses a row; a double-click activates it. Clicking
/// the value column does both in one click.
#[test]
fn settings_rows_focus_on_click_and_activate_on_double_click() {
    let mut app = App::new(launch_basic(), true);
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    let motion = settings_row(crate::tui::settings_registry::REDUCED_MOTION);
    let hit = app
        .settings_hits
        .iter()
        .find(|hit| hit.index == motion)
        .copied()
        .expect("reduced motion row is painted");
    assert_ne!(
        app.settings_menu.as_ref().unwrap().selected,
        motion,
        "the row starts unfocused"
    );

    // First click on the label only focuses.
    app.handle_mouse(left_click(hit.rect.x + 2, hit.rect.y));
    assert_eq!(app.settings_menu.as_ref().unwrap().selected, motion);
    assert_eq!(
        app.settings_menu
            .as_ref()
            .unwrap()
            .bool_value(crate::tui::settings_registry::REDUCED_MOTION),
        Some(false),
        "reading a description must not change anything"
    );

    // The second click of a double-click activates.
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let hit = app
        .settings_hits
        .iter()
        .find(|hit| hit.index == motion)
        .copied()
        .expect("row stays painted");
    app.handle_mouse(left_click(hit.rect.x + 2, hit.rect.y));
    assert_eq!(
        app.settings_menu
            .as_ref()
            .unwrap()
            .bool_value(crate::tui::settings_registry::REDUCED_MOTION),
        Some(true)
    );

    // The value column focuses and activates in one click.
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let thinking = settings_row(crate::tui::settings_registry::SHOW_THINKING);
    let hit = app
        .settings_hits
        .iter()
        .find(|hit| hit.index == thinking)
        .copied()
        .expect("show thinking row is painted");
    app.handle_mouse(left_click(hit.value.x + 1, hit.value.y));
    assert_eq!(
        app.settings_menu
            .as_ref()
            .unwrap()
            .bool_value(crate::tui::settings_registry::SHOW_THINKING),
        Some(false),
        "the value column is a one-click activation"
    );
}

/// Spec 02 §5.5 mouse parity: a click on a theme choice focuses and previews
/// it; a double-click applies it exactly as Enter does.
#[test]
fn a_theme_choice_previews_on_click_and_applies_on_double_click() {
    let mut app = App::new(launch_basic(), true);
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    app.settings_menu.as_mut().unwrap().selected =
        settings_row(crate::tui::settings_registry::THEME);
    app.handle_key(key(KeyCode::Enter));
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let choosing = |app: &App| {
        matches!(
            app.settings_menu.as_ref().unwrap().pane,
            super::SettingsPane::Choosing(_)
        )
    };
    assert!(choosing(&app));
    let second = crate::tui::SettingsTheme::ALL[1];
    let hit = app
        .settings_hits
        .iter()
        .find(|hit| hit.index == 1)
        .copied()
        .expect("every choice is clickable");

    assert_eq!(
        app.handle_mouse(left_click(hit.rect.x + 4, hit.rect.y)),
        None
    );
    assert!(choosing(&app), "a click only focuses");
    assert_eq!(app.settings_menu.as_ref().unwrap().draft.theme, second);
    assert_eq!(app.theme.id, super::theme_for(second).id, "and previews");

    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(matches!(
        app.handle_mouse(left_click(hit.rect.x + 4, hit.rect.y)),
        Some(UserAction::Extension(SurfaceIntent::SaveSettings(_)))
    ));
    assert!(!choosing(&app), "a double-click applies like Enter");
    assert_eq!(app.settings_menu.as_ref().unwrap().draft.theme, second);
    assert_eq!(app.theme.id, super::theme_for(second).id);
}

/// Pointer motion over a chooser's choices never sets the browse list's
/// hover, so closing the chooser highlights no stray row.
#[test]
fn hovering_a_chooser_choice_leaves_no_browse_hover_behind() {
    let mut app = App::new(launch_basic(), true);
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    app.settings_menu.as_mut().unwrap().selected =
        settings_row(crate::tui::settings_registry::THEME);
    app.handle_key(key(KeyCode::Enter));
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let hit = app
        .settings_hits
        .iter()
        .find(|hit| hit.index == 3)
        .copied()
        .unwrap();
    app.handle_mouse(left_mouse(
        MouseEventKind::Moved,
        hit.rect.x + 2,
        hit.rect.y,
    ));
    assert_eq!(app.settings_hover, None);
    app.handle_key(key(KeyCode::Esc));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.settings_hover, None);
}

/// A single click on the already focused row no longer activates it: only
/// a double-click does (spec 02 §5.5).
#[test]
fn a_single_click_on_the_focused_settings_row_only_keeps_focus() {
    let mut app = App::new(launch_basic(), true);
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    let motion = settings_row(crate::tui::settings_registry::REDUCED_MOTION);
    app.settings_menu.as_mut().unwrap().selected = motion;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let hit = app
        .settings_hits
        .iter()
        .find(|hit| hit.index == motion)
        .copied()
        .unwrap();
    assert_eq!(
        app.handle_mouse(left_click(hit.rect.x + 2, hit.rect.y)),
        None
    );
    assert_eq!(
        app.settings_menu
            .as_ref()
            .unwrap()
            .bool_value(crate::tui::settings_registry::REDUCED_MOTION),
        Some(false)
    );
}

/// Spec 02 §5.5: the theme chooser previews live on every move, `Esc` reverts
/// to the value that was committed, `Enter` commits.
#[test]
fn theme_preview_and_revert_cover_all_six_themes() {
    let mut app = App::new(launch_basic(), true);
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    let theme_row = settings_row(crate::tui::settings_registry::THEME);
    app.settings_menu.as_mut().unwrap().selected = theme_row;
    assert!(
        app.handle_key(key(KeyCode::Enter)).is_none(),
        "opening the chooser is not itself a change"
    );

    for expected in crate::tui::SettingsTheme::ALL.into_iter().skip(1) {
        app.handle_key(key(KeyCode::Down));
        assert_eq!(
            app.settings_menu.as_ref().unwrap().draft.theme,
            expected,
            "the chooser previews {expected:?} on move"
        );
        assert_eq!(
            app.theme.id,
            super::theme_for(expected).id,
            "the live theme follows the preview"
        );
    }
    // Esc reverts to the committed value, not merely to the previous one.
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(
        app.settings_menu.as_ref().unwrap().draft.theme,
        crate::tui::SettingsTheme::DefaultDark
    );
    assert_eq!(app.theme, crate::tui::Theme::dark());
    assert_eq!(app.control_tab, Some(super::ControlTab::Settings));

    // Enter commits the focused choice.
    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.settings_menu.as_ref().unwrap().draft.theme,
        crate::tui::SettingsTheme::DefaultLight
    );
    assert_eq!(app.theme, crate::tui::Theme::light());
}

#[test]
fn settings_failure_replaces_loading_and_keeps_control_center_open() {
    let mut app = App::new(launch_basic(), true);
    app.control_tab = Some(super::ControlTab::Settings);
    app.apply_host_event(HostEvent::Extension(
        SurfaceObservation::SettingsLoadFailed(
            "settings could not be loaded: invalid config".into(),
        ),
    ));
    assert_eq!(app.control_tab, Some(super::ControlTab::Settings));
    assert!(app.settings_menu.is_none());
    assert!(
        app.settings_error
            .as_deref()
            .is_some_and(|message| { message.contains("invalid config") })
    );
}

#[test]
fn launch_settings_are_immediately_visible_while_host_refresh_is_pending() {
    let mut launch = launch_basic();
    launch.settings = Some(SettingsSnapshot {
        default_account: Some("codex".into()),
        default_model: Some("gpt-exact".into()),
        default_effort: Some("medium".into()),
        default_extra: None,
        ..SettingsSnapshot::default()
    });
    let mut app = App::new(launch, true);
    assert!(app.settings_snapshot.is_some());
    assert!(app.settings_menu.is_none());
    assert!(app.control_tab.is_none());
    assert!(app.handle_key(key(KeyCode::Char('h'))).is_none());
    assert_eq!(app.input, "h");
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Prompt("h".into()))
    );
    let (sender, receiver) = user_action_channel();
    dispatch_action(
        &mut app,
        &UserAction::Extension(SurfaceIntent::OpenSettings),
        Some(&sender),
    );
    assert_eq!(
        *receiver.try_recv().unwrap(),
        UserAction::Extension(SurfaceIntent::OpenSettings)
    );
    assert_eq!(app.control_tab, Some(super::ControlTab::Settings));
    assert!(app.settings_menu.is_some());
    assert!(app.settings_error.is_none());
}

#[test]
fn empty_composer_is_four_rows_and_bottom_anchored_on_every_theme_and_size() {
    for theme_id in ThemeId::ALL {
        for (width, height) in [(80, 40), (40, 8), (10, 4)] {
            let mut app = App::new(launch_basic(), true);
            app.theme = crate::tui::Theme::for_id(theme_id);
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();

            let inner = app.composer_inner.expect("empty composer");
            assert_eq!(inner.height, 2, "{theme_id:?} at {width}x{height}");
            assert_eq!(
                inner.bottom(),
                height.saturating_sub(if height > 4 { 2 } else { 1 }),
                "{theme_id:?} at {width}x{height}"
            );
            assert_eq!(
                terminal.backend().buffer()[(0, height - if height > 4 { 2 } else { 1 })].symbol(),
                "╰",
                "{theme_id:?} at {width}x{height} must anchor the border to the bottom"
            );
        }
    }
}

#[test]
fn composer_uses_app_background_across_catppuccin_themes_and_sizes() {
    let catppuccin = [
        ThemeId::CatppuccinLatte,
        ThemeId::CatppuccinFrappe,
        ThemeId::CatppuccinMacchiato,
        ThemeId::CatppuccinMocha,
    ];
    for theme_id in catppuccin {
        for (width, height) in [(80, 24), (24, 8)] {
            let mut app = App::new(launch_basic(), true);
            app.theme = crate::tui::Theme::for_id(theme_id);
            assert_ne!(app.theme.app_bg, app.theme.menu_bg);
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();

            let inner = app.composer_inner.expect("composer");
            let outer = Rect::new(
                inner.x.saturating_sub(1),
                inner.y.saturating_sub(1),
                inner.width.saturating_add(2),
                inner.height.saturating_add(2),
            );
            let buffer = terminal.backend().buffer();
            for y in outer.y..outer.bottom() {
                for x in outer.x..outer.right() {
                    assert_eq!(
                        buffer[(x, y)].bg,
                        app.theme.app_bg,
                        "{theme_id:?} at {width}x{height}, composer cell ({x}, {y})"
                    );
                }
            }
        }
    }
}

#[test]
fn cancelled_theme_preview_restores_persisted_theme() {
    let mut app = App::new(launch_basic(), true);
    // The operator asked for the control centre; the snapshot is its reply.
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot {
            default_account: None,
            default_model: None,
            default_effort: None,
            default_extra: None,
            ..SettingsSnapshot::default()
        },
    )));
    app.settings_menu.as_mut().expect("settings").selected =
        settings_row(crate::tui::settings_registry::THEME);
    // Enter opens the chooser; Down previews the next theme live.
    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.theme, crate::tui::Theme::light());
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.theme, crate::tui::Theme::dark());
    app.handle_key(key(KeyCode::Esc));
    assert!(app.control_tab.is_none());
}

/// The §5.5 rendering rows reach the renderer, on the persisted
/// snapshot and on live preview, and closing reverts the preview.
#[test]
fn settings_rendering_rows_drive_the_renderer() {
    let mut app = App::new(launch_basic(), true);
    assert_eq!(
        RenderPrefs::default(),
        RenderPrefs::from(&SettingsSnapshot::default())
    );

    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot {
            show_thinking: false,
            group_tool_calls: false,
            reduced_motion: true,
            ..SettingsSnapshot::default()
        },
    )));
    assert!(!app.render_prefs.show_thinking);
    assert!(!app.render_prefs.group_tool_calls);
    assert!(app.render_prefs.reduced_motion);
    assert_eq!(app.status_spinner(), "⠿");

    let row = settings_row(crate::tui::settings_registry::REDUCED_MOTION);
    app.settings_menu.as_mut().expect("settings").selected = row;
    app.handle_settings_key(key(KeyCode::Char(' ')));
    assert!(!app.render_prefs.reduced_motion, "preview applies");
    app.close_control_center();
    assert!(app.render_prefs.reduced_motion, "close reverts the preview");
}

/// Opening an Enum chooser or an Int stepper must paint
/// on the key that opened it. It did not, so the operator typed into a
/// modal that was not on screen and that key was eaten by it.
#[test]
fn opening_a_settings_sub_pane_paints_on_the_same_frame() {
    for (key_name, expect) in [
        (crate::tui::settings_registry::THEME, "Catppuccin Mocha"),
        (crate::tui::settings_registry::SCROLL_LINES, "\u{2039}"),
    ] {
        let mut app = App::new(launch_basic(), true);
        app.pending_open_settings = true;
        app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
            SettingsSnapshot::default(),
        )));
        app.settings_menu.as_mut().expect("settings").selected = settings_row(key_name);

        app.dirty = false;
        assert!(
            app.handle_key(key(KeyCode::Enter)).is_none(),
            "{key_name}: opening a sub-pane is not itself a change"
        );
        assert!(
            app.dirty,
            "{key_name}: opening the sub-pane owes the frame a repaint"
        );
        assert!(
            !matches!(
                app.settings_menu.as_ref().expect("settings").pane,
                crate::tui::SettingsPane::Browse
            ),
            "{key_name}: the sub-pane is open"
        );

        // The very next paint, with no further key, shows the sub-pane.
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let painted: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            painted.contains(expect),
            "{key_name}: the open sub-pane is painted without a second key"
        );
    }
}

/// Spec 02 §5.5: hovering a row highlights it and nothing else. The list is
/// the same height and the same rows before and after the pointer moves.
#[test]
fn hovering_a_settings_row_only_highlights_it() {
    let mut app = settings_fixture();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let before = serialize_frame(terminal.backend().buffer());
    let hit = *app.settings_hits.first().expect("a settings row was drawn");
    app.handle_mouse(left_mouse(
        MouseEventKind::Moved,
        hit.rect.x + 2,
        hit.rect.y,
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let after = serialize_frame(terminal.backend().buffer());
    let text = |frame: &str| {
        frame
            .lines()
            .filter(|line| line.starts_with('|'))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        text(&before),
        text(&after),
        "hover changed the glyphs, not just the styling"
    );
    assert_ne!(before, after, "hover must still highlight the row");
}

/// Spec 02 §5.5: every remaining row applies on change. A `Space` on a switch,
/// an `Enter` on a chooser or a stepper, and a `d`-then-`Enter` reset each
/// persist immediately; nothing waits for a Save row, because there is
/// none.
#[test]
fn every_settings_row_applies_on_change_and_persists_immediately() {
    let mut app = App::new(launch_basic(), true);
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();

    // A switch row.
    app.settings_menu.as_mut().unwrap().selected =
        settings_row(crate::tui::settings_registry::MOUSE);
    let emitted = app.handle_key(key(KeyCode::Char(' ')));
    let Some(UserAction::Extension(SurfaceIntent::SaveSettings(update))) = emitted else {
        panic!("a switch must persist on change: {emitted:?}");
    };
    assert!(!update.mouse);

    // An enum row: the chooser previews live and commits on Enter.
    app.settings_menu.as_mut().unwrap().selected =
        settings_row(crate::tui::settings_registry::THEME);
    app.settings_menu.as_mut().unwrap().pane = crate::tui::SettingsPane::Browse;
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        None,
        "the chooser opens"
    );
    assert_eq!(
        app.handle_key(key(KeyCode::Down)),
        None,
        "a preview is not a save"
    );
    let emitted = app.handle_key(key(KeyCode::Enter));
    let Some(UserAction::Extension(SurfaceIntent::SaveSettings(update))) = emitted else {
        panic!("a committed choice must persist: {emitted:?}");
    };
    assert_ne!(update.theme, crate::tui::SettingsTheme::DefaultDark);

    // An int row: the stepper commits on Enter.
    app.settings_menu.as_mut().unwrap().selected =
        settings_row(crate::tui::settings_registry::SCROLL_LINES);
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        None,
        "the stepper opens"
    );
    app.handle_key(key(KeyCode::Up));
    let emitted = app.handle_key(key(KeyCode::Enter));
    let Some(UserAction::Extension(SurfaceIntent::SaveSettings(update))) = emitted else {
        panic!("a committed step must persist: {emitted:?}");
    };
    assert_eq!(update.scroll_lines, 4);

    // `d` then `Enter` confirms the reset at once. There is no settle.
    app.handle_key(key(KeyCode::Char('d')));
    let emitted = app.handle_key(key(KeyCode::Enter));
    let Some(UserAction::Extension(SurfaceIntent::SaveSettings(update))) = emitted else {
        panic!("a reset must persist: {emitted:?}");
    };
    assert_eq!(update.scroll_lines, 3);

    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        app.control_tab,
        Some(super::ControlTab::Settings),
        "applying on change never closes the tab"
    );
}

/// Spec 02 §5.5: the host is the only authority that can refuse a change, and
/// its refusal is painted inline over a tab that stays open.
#[test]
fn a_refused_settings_change_is_stated_inline_and_keeps_the_tab_open() {
    let mut app = App::new(launch_basic(), true);
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    app.apply_host_event(HostEvent::Extension(
        SurfaceObservation::SettingsSaveFailed("config.toml is read-only".into()),
    ));
    assert_eq!(app.control_tab, Some(super::ControlTab::Settings));
    assert_eq!(
        app.settings_error.as_deref(),
        Some("config.toml is read-only")
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    // A confirmed write leaves the operator exactly where they were.
    let applied = SettingsSnapshot {
        scroll_lines: 6,
        ..SettingsSnapshot::default()
    };
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsApplied(
        applied,
    )));
    assert_eq!(app.control_tab, Some(super::ControlTab::Settings));
    assert_eq!(
        app.settings_menu
            .as_ref()
            .expect("settings")
            .draft
            .scroll_lines,
        6
    );
}

#[test]
fn late_settings_acknowledgement_stays_inert_after_control_center_closes() {
    let mut app = App::new(launch_basic(), true);
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    app.close_control_center();
    let applied = SettingsSnapshot {
        reduced_motion: true,
        ..SettingsSnapshot::default()
    };
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsApplied(
        applied,
    )));
    assert!(app.settings_menu.is_none(), "closed Settings stays inert");
    assert!(
        app.render_prefs.reduced_motion,
        "authoritative value still applies"
    );

    let newer = SettingsSnapshot {
        reduced_motion: false,
        ..SettingsSnapshot::default()
    };
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        newer,
    )));
    assert!(
        app.settings_menu.is_none(),
        "a late refresh stays inert too"
    );
    assert!(!app.render_prefs.reduced_motion);

    let action = app.open_control_tab(super::ControlTab::Settings);
    assert_eq!(
        action,
        Some(UserAction::Extension(SurfaceIntent::OpenSettings))
    );
    assert!(
        !app.settings_menu
            .as_ref()
            .expect("reopened settings")
            .draft
            .reduced_motion,
        "reopen starts from the newest authoritative snapshot"
    );
}

/// Spec 02 §5.5: Settings carries no Save row, no Composition row, and no
/// `OpenRouter` protocol row.
#[test]
fn settings_carries_no_save_composition_or_protocol_row() {
    let labels: Vec<&str> = crate::tui::settings_registry::ROWS
        .iter()
        .map(|row| row.label)
        .collect();
    assert_eq!(
        labels,
        vec![
            "Theme",
            "Mouse capture",
            "Scroll lines",
            "Show thinking blocks",
            "Group tool calls",
            "Auto compaction",
            "Reduced motion",
            "Edit raw config",
        ]
    );
}

/// A settings echo that arrives while Reset is open is kept. Confirm
/// restores only the bound row on that live draft.
#[test]
fn settings_reset_is_taken_from_the_live_draft_at_confirm() {
    for applied in [true, false] {
        let mut app = settings_fixture();
        let row = settings_row(crate::tui::settings_registry::SCROLL_LINES);
        app.settings_menu.as_mut().unwrap().selected = row;
        app.settings_menu.as_mut().unwrap().draft.scroll_lines = 8;
        assert!(app.handle_key(key(KeyCode::Char('d'))).is_none());
        let changed = SettingsSnapshot {
            compaction: !SettingsSnapshot::default().compaction,
            scroll_lines: 9,
            ..SettingsSnapshot::default()
        };
        let observation = if applied {
            SurfaceObservation::SettingsApplied(changed.clone())
        } else {
            SurfaceObservation::SettingsSnapshot(changed.clone())
        };
        app.apply_host_event(HostEvent::Extension(observation));
        assert!(
            app.destructive_confirm.is_some(),
            "the pop-up survives the echo"
        );
        let sent = app.handle_key(key(KeyCode::Enter));
        let Some(UserAction::Extension(SurfaceIntent::SaveSettings(update))) = sent else {
            panic!("{sent:?}");
        };
        assert_eq!(update.scroll_lines, 3, "the bound row resets");
        assert_eq!(
            update.compaction, changed.compaction,
            "applied={applied}: the echo was reverted"
        );
    }
}
