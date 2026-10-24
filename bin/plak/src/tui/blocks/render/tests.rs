use super::activity_line;
use crate::tui::theme::Theme;

#[test]
fn expanded_command_rails_are_muted_without_muting_the_command() {
    let theme = Theme::dark();
    let line = activity_line("  └ $ cargo test".into(), &theme, theme.success);
    assert_eq!(line.spans[0].content, "  └ ");
    assert_eq!(line.spans[0].style, theme.tool_label);
    assert_eq!(line.spans[1].content, "$ cargo test");
    assert_eq!(line.spans[1].style, theme.tool);
}
