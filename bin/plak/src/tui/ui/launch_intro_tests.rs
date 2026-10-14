//! Session banner at the top of the transcript; composer stays live.

use super::App;
use super::test_support::{key, launch_basic, screen_text};
use crate::tui::blocks::ScrollBlock;
use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn paint(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    screen_text(&terminal)
}

/// The pixel mark: a run of ink cells plus a double-line shadow stroke.
fn looks_like_banner(text: &str) -> bool {
    text.contains("powered by plakous") && text.contains("██████████") && text.contains('╝')
}

#[test]
fn a_new_app_does_not_cover_the_shell_with_the_banner() {
    let mut app = App::new(launch_basic(), true);
    assert!(!app.showing_launch_intro());
    let text = paint(&mut app, 80, 24);
    assert!(text.contains("context"), "composer: {text:?}");
}

#[test]
fn the_empty_transcript_shows_the_banner_and_the_composer() {
    let mut launch = launch_basic();
    launch.version = "9.8.7-test";
    let mut app = App::new(launch, true);
    app.arm_launch_intro_cells();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let text = screen_text(&terminal);
    if let Some(path) = std::env::var_os("PLAK_TUI_BUILD_FRAME") {
        std::fs::write(
            path,
            super::test_support::serialize_frame(terminal.backend().buffer()),
        )
        .unwrap();
    }
    assert!(app.showing_launch_intro());
    assert!(looks_like_banner(&text), "banner: {text:?}");
    assert!(text.contains("version  9.8.7-test"), "version: {text:?}");
    assert!(text.contains("binary   "), "binary identity: {text:?}");
    let flavor = if cfg!(feature = "maintainer") {
        if cfg!(debug_assertions) {
            "maintainer debug"
        } else {
            "maintainer release"
        }
    } else if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    assert!(
        text.contains(&format!("build    {flavor}")),
        "build: {text:?}"
    );
    assert_eq!(
        text.contains("hooks    test-hooks"),
        cfg!(feature = "test-hooks")
    );
    assert!(text.contains("context"), "composer stays: {text:?}");
}

#[test]
fn typing_does_not_dismiss_the_banner() {
    let mut app = App::new(launch_basic(), true);
    app.arm_launch_intro_cells();
    assert!(app.handle_key(key(KeyCode::Char('x'))).is_none());
    assert!(app.showing_launch_intro());
    assert_eq!(app.input, "x");
    let text = paint(&mut app, 80, 24);
    assert!(looks_like_banner(&text), "still banner: {text:?}");
}

#[test]
fn a_user_turn_starts_below_the_banner() {
    let mut app = App::new(launch_basic(), true);
    app.arm_launch_intro_cells();
    app.push_block(ScrollBlock::user("hello from the composer".into()));
    assert!(app.showing_launch_intro());
    let text = paint(&mut app, 80, 24);
    assert!(looks_like_banner(&text), "banner stays: {text:?}");
    assert!(
        text.contains("hello from the composer"),
        "turn below: {text:?}"
    );
    let banner = text.find("powered by plakous").expect("banner");
    let hello = text.find("hello from the composer").expect("turn");
    assert!(banner < hello, "banner above the turn: {text:?}");
}

#[test]
fn a_full_transcript_scrolls_the_banner_off() {
    let mut app = App::new(launch_basic(), true);
    app.arm_launch_intro_cells();
    for index in 0..40 {
        app.push_block(ScrollBlock::System(format!("row-{index:02}")));
    }
    let filled = paint(&mut app, 80, 24);
    assert!(
        !looks_like_banner(&filled),
        "follow-live hides the banner: {filled:?}"
    );
    assert!(
        filled.contains("row-39"),
        "latest row stays on screen: {filled:?}"
    );
    app.pane.scroll_from_bottom = usize::MAX;
    let top = paint(&mut app, 80, 24);
    assert!(
        looks_like_banner(&top),
        "scrolling to the top brings the banner back: {top:?}"
    );
}

#[test]
fn a_slash_command_does_not_dismiss_the_banner() {
    let mut app = App::new(launch_basic(), true);
    app.arm_launch_intro_cells();
    app.input = "/help".into();
    let _ = app.handle_key(key(KeyCode::Enter));
    assert!(app.showing_launch_intro());
}

/// The wordmark has no setting behind it any more: an ordinary launch arms it,
/// and only a pane too small to hold it declines to paint it. Reduced motion is
/// a different bit and does not touch the banner.
#[test]
fn an_ordinary_launch_always_arms_the_wordmark() {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.arm_launch_intro();
    assert!(app.showing_launch_intro());
    let text = paint(&mut app, 100, 30);
    assert!(
        looks_like_banner(&text),
        "reduced motion keeps it: {text:?}"
    );
}

#[test]
fn resume_picker_launch_skips_the_banner() {
    let mut launch = launch_basic();
    launch.open_resume_picker = true;
    let mut app = App::new(launch, true);
    app.arm_launch_intro();
    assert!(!app.showing_launch_intro());
}

#[test]
fn a_tiny_screen_keeps_the_composer_without_a_smear() {
    let mut app = App::new(launch_basic(), true);
    app.arm_launch_intro_cells();
    let text = paint(&mut app, 20, 8);
    assert!(text.contains("con") || text.contains('❯'), "tiny: {text:?}");
    assert!(!looks_like_banner(&text), "too small for banner: {text:?}");
}
