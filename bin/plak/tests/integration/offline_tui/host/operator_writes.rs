//! The public socket TUI names CLI remedies instead of offering legacy writes.

use super::*;

#[test]
fn socket_tui_explains_unavailable_settings_default_and_credential_refresh() {
    let fixture = Fixture::with_mode(&tool_then_answer("unused"), Mode::Host);
    let config = std::fs::read(fixture.home().join("config.toml")).unwrap();
    let mut terminal = Terminal::spawn_ready(&fixture);
    terminal.send(b"/settings\r");
    terminal.wait_screen("/ search settings");
    terminal.focus_control_row("Auto compaction");
    terminal.wait_screen("Read-only on a host attachment");
    terminal.send(b" ");
    terminal.wait_screen("now on · default on");
    terminal.send(b"\x1b");
    terminal.wait_ready();
    terminal.wait_screen("plak config set compaction.enabled");
    terminal.send(b"/agents codex\r");
    let frame = terminal.wait_screen("plak config set root.profile");
    assert!(!frame.contains("[ Set as default ]"), "{frame}");
    terminal.send(b"\x1b");
    terminal.wait_where("the profile dialog closed", |frame| {
        frame.contains("Profiles") && !frame.contains("Apply profile codex-default?")
    });
    terminal.send(b"\x1b");
    terminal.wait_ready();
    terminal.send(b"/usage\r");
    let frame = terminal.wait_screen("plak refresh <account>");
    assert!(!frame.contains("[refresh]"), "{frame}");
    assert_eq!(
        std::fs::read(fixture.home().join("config.toml")).unwrap(),
        config
    );
    terminal.quit();
}
