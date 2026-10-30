//! Terminal plan bullets, rendered from neutral status labels.

pub(super) fn plan_status_glyph(status: &str) -> &'static str {
    match status {
        "completed" => "✓",
        "in_progress" => "▶",
        "cancelled" | "canceled" => "×",
        _ => "○",
    }
}

pub(super) fn plan_line(line: &str) -> String {
    let Some((status, text)) = line
        .trim()
        .strip_prefix("- [")
        .and_then(|s| s.split_once(']'))
    else {
        return line.to_owned();
    };
    format!("{} {}", plan_status_glyph(status), text.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_plan_statuses_keep_the_terminal_bullets() {
        for (status, glyph) in [
            ("completed", "✓"),
            ("in_progress", "▶"),
            ("pending", "○"),
            ("cancelled", "×"),
            ("canceled", "×"),
        ] {
            assert_eq!(
                plan_line(&format!("- [{status}] task")),
                format!("{glyph} task")
            );
        }
        assert_eq!(plan_line("ordinary output"), "ordinary output");
    }
}
