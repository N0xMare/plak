use super::super::test_support::{confirm_action, key, launch_basic};
use super::super::{App, ControlTab};
use crate::tui::{
    HostEvent, ProfileOutcome, ProfileRole, ProfileRow, ProfilesView, SurfaceIntent,
    SurfaceObservation, UserAction,
};
use crossterm::event::KeyCode;

fn role(label: &str, account: &str, model: &str) -> ProfileRole {
    ProfileRole {
        label: label.to_owned(),
        account: account.to_owned(),
        model: model.to_owned(),
        effort: "medium".to_owned(),
        fast: false,
        depth: 0,
    }
}

fn view() -> ProfilesView {
    let row = |id: &str, aliases: &[&str], account: &str, model: &str| ProfileRow {
        id: id.to_owned(),
        differences: Vec::new(),
        aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
        root: role("root", account, model),
        cards: vec![role("worker", account, model)],
    };
    ProfilesView {
        rows: vec![
            row("codex-default", &["chatgpt"], "codex", "gpt-6-astra"),
            row("claude-default", &["claude"], "claude", "claude-sonnet-5"),
            row("grok-default", &["grok"], "grok", "grok-4.7"),
            // A user profile named like a built-in alias owns that name.
            row("codex", &[], "codex", "gpt-5.6-luna"),
        ],
        current: Some("grok-default".to_owned()),
        diagnostics: vec!["profiles/bad.toml is ignored: it needs `root`".to_owned()],
    }
}

/// An app on the Agents tab with the product's list delivered.
fn agents_app() -> App {
    let mut app = App::new(launch_basic(), true);
    let action = app.activate_control_tab(ControlTab::Models);
    assert!(matches!(
        action,
        Some(UserAction::Extension(SurfaceIntent::ListProfiles))
    ));
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view())));
    app
}

fn apply(action: Option<UserAction>) -> Option<(String, bool)> {
    match action {
        Some(UserAction::Extension(SurfaceIntent::ApplyProfile { name, fork_now })) => {
            Some((name, fork_now))
        }
        _ => None,
    }
}

/// The profile the open pop-up applies, and whether it offers Fork now.
fn asking(app: &App) -> Option<(String, bool)> {
    let confirm = app.destructive_confirm.as_ref()?;
    assert_eq!(confirm.kind(), "profile");
    apply(confirm.first_action().cloned())
}

/// `→` `n` times from the primary action, then `Enter`.
fn press_button(app: &mut App, n: usize) -> Option<UserAction> {
    for _ in 0..n {
        app.handle_key(key(KeyCode::Right));
    }
    app.handle_key(key(KeyCode::Enter))
}

#[test]
fn the_list_opens_on_the_current_profile_and_enter_asks_before_applying() {
    let mut app = agents_app();
    assert_eq!(
        app.profiles.selected().map(|row| row.id.as_str()),
        Some("grok-default")
    );
    assert!(app.handle_key(key(KeyCode::Enter)).is_none());
    assert_eq!(asking(&app), Some(("grok-default".to_owned(), true)));
    // Esc cancels; Enter on the primary action (Fork now) applies at once.
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert!(app.destructive_confirm.is_none());
    assert_eq!(app.control_tab, Some(ControlTab::Models));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        apply(confirm_action(&mut app)),
        Some(("grok-default".to_owned(), true))
    );
    assert!(app.control_tab.is_none());
    assert!(app.destructive_confirm.is_none());
}

/// Spec 02 §5.5 mouse parity: a click moves the cursor; a double-click
/// opens the apply dialog exactly as Enter does.
#[test]
fn a_profile_row_selects_on_click_and_asks_on_double_click() {
    let mut app = agents_app();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let (index, rect) = *app.profiles.hits.last().expect("rows are clickable");
    let click = super::super::test_support::left_click(rect.x + 2, rect.y);
    assert_eq!(app.handle_mouse(click), None);
    assert_eq!(app.profiles.cursor, index);
    assert!(app.destructive_confirm.is_none(), "a click only selects");
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.handle_mouse(click), None);
    assert!(asking(&app).is_some(), "a double-click asks like Enter");
}

#[test]
fn next_session_keeps_the_agents_tab_open() {
    let mut app = agents_app();
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        apply(press_button(&mut app, 1)),
        Some(("grok-default".to_owned(), false))
    );
    assert_eq!(app.control_tab, Some(ControlTab::Models));
}

#[test]
fn typing_filters_and_enter_on_no_match_applies_nothing() {
    let mut app = agents_app();
    app.handle_key(key(KeyCode::Char('c')));
    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(
        app.profiles.selected().map(|row| row.id.as_str()),
        Some("claude-default")
    );
    for ch in ['z', 'z', 'z'] {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    assert!(app.profiles.selected().is_none());
    assert!(app.handle_key(key(KeyCode::Enter)).is_none());
    assert!(
        app.destructive_confirm.is_none(),
        "a hidden row must not be offered"
    );
    let frame = super::super::test_support::render_to_string(&mut app, 100, 30);
    assert!(frame.contains("No matching profiles"), "{frame}");
    assert!(
        !frame.contains("Root agent"),
        "no detail for a hidden row: {frame}"
    );
}

#[test]
fn slash_is_an_ordinary_filter_character_and_the_hint_does_not_offer_it() {
    let mut app = agents_app();
    let frame = super::super::test_support::render_to_string(&mut app, 100, 30);
    assert!(frame.contains("type to filter"), "{frame}");
    assert!(!frame.contains("/ search"), "{frame}");
}

#[test]
fn product_diagnostics_are_listed_under_the_profiles() {
    let mut app = agents_app();
    let frame = super::super::test_support::render_to_string(&mut app, 100, 30);
    assert!(frame.contains("profiles/bad.toml is ignored"), "{frame}");
    assert!(frame.contains("codex-default"), "{frame}");
}

#[test]
fn a_failed_config_is_one_diagnostic_not_no_matching_profiles() {
    let mut app = App::new(launch_basic(), true);
    app.activate_control_tab(ControlTab::Models);
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(
        ProfilesView {
            diagnostics: vec![
                "templates could not be loaded because config.toml failed: unknown config key: not_a_key"
                    .into(),
            ],
            ..ProfilesView::default()
        },
    )));
    let frame = super::super::test_support::render_to_string(&mut app, 100, 30);
    assert!(frame.contains("templates could not be loaded"), "{frame}");
    assert!(!frame.contains("No matching profiles"), "{frame}");
    assert!(!frame.contains("does not exist"), "{frame}");
    assert!(!frame.contains("custom"), "{frame}");
}

#[test]
fn agents_name_resolves_like_the_list() {
    let mut app = App::new(launch_basic(), true);
    let action = app.open_profile_confirm("codex");
    assert!(matches!(
        action,
        Some(UserAction::Extension(SurfaceIntent::ListProfiles))
    ));
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view())));
    assert_eq!(asking(&app), Some(("codex".to_owned(), true)));
    app.destructive_confirm = None;
    app.open_profile_confirm("claude");
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view())));
    assert_eq!(asking(&app), Some(("claude-default".to_owned(), true)));
    app.destructive_confirm = None;
    app.open_profile_confirm("nope");
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view())));
    assert!(app.destructive_confirm.is_none());
}

#[test]
fn the_default_mark_follows_only_the_products_default_outcome() {
    let mut app = agents_app();
    app.handle_key(key(KeyCode::Up));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        apply(press_button(&mut app, 1)),
        Some(("claude-default".to_owned(), false))
    );
    // Sent, not yet answered: the mark has not moved.
    assert_eq!(
        app.profiles
            .view
            .as_ref()
            .and_then(|view| view.current.as_deref()),
        Some("grok-default")
    );
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::ProfileRefused {
        name: "claude-default".to_owned(),
        reason: "claude is not ready".to_owned(),
    }));
    assert_eq!(
        app.profiles
            .view
            .as_ref()
            .and_then(|view| view.current.as_deref()),
        Some("grok-default")
    );
    // A fork changes only that session: the default mark stays.
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::ProfileApplied {
        name: "claude-default".to_owned(),
        outcome: ProfileOutcome::Forked,
    }));
    assert_eq!(
        app.profiles
            .view
            .as_ref()
            .and_then(|view| view.current.as_deref()),
        Some("grok-default")
    );
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::ProfileApplied {
        name: "claude-default".to_owned(),
        outcome: ProfileOutcome::Saved,
    }));
    assert_eq!(
        app.profiles
            .view
            .as_ref()
            .and_then(|view| view.current.as_deref()),
        Some("claude-default")
    );
}

#[test]
fn an_unbound_session_offers_use_now() {
    let mut launch = launch_basic();
    launch.model = None;
    let mut app = App::new(launch, true);
    app.activate_control_tab(ControlTab::Models);
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(view())));
    app.handle_key(key(KeyCode::Enter));
    let frame = super::super::test_support::render_to_string(&mut app, 100, 30);
    assert!(frame.contains("[ Use now ]"), "{frame}");
    assert_eq!(
        apply(confirm_action(&mut app)),
        Some(("grok-default".to_owned(), true))
    );
}
